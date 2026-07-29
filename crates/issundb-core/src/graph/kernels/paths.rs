use super::*;

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use super::traversal::{UNREACHED, expand_frontier};

/// A cost ordered as a total order, so it can key a [`BinaryHeap`].
///
/// A weight comes from a JSON property, so a NaN is reachable from data rather
/// than from a bug here; it compares equal to everything instead of panicking,
/// which keeps a malformed property from taking down a query.
#[derive(Debug, PartialEq)]
struct TotalF64(f64);

impl Eq for TotalF64 {}

impl PartialOrd for TotalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TotalF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
    }
}

/// A heap entry ordered so the cheapest cost pops first.
#[derive(Debug, PartialEq, Eq)]
struct Step<T: Eq> {
    cost: Reverse<TotalF64>,
    node: T,
}

impl<T: Eq> Ord for Step<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cost.cmp(&other.cost)
    }
}

impl<T: Eq> PartialOrd for Step<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Breadth-first hop distance from `src` to every reachable dense index.
///
/// Stops as soon as `stop` is reached, one whole level at a time, so every node at
/// a distance below `stop`'s is recorded and nothing beyond that level is. A
/// backward walk needs exactly that much, and stopping early is what keeps a
/// point-to-point query off the rest of the graph.
fn hop_distances(snap: &CsrSnapshot, src: u32, stop: Option<u32>) -> Vec<u32> {
    let mut levels = vec![UNREACHED; snap.dense_to_id.len()];
    levels[src as usize] = 0;
    let mut frontier = vec![src];
    let mut next = Vec::new();
    for hop in 1.. {
        expand_frontier(snap, &frontier, hop, &mut levels, &mut next);
        if next.is_empty() {
            break;
        }
        std::mem::swap(&mut frontier, &mut next);
        if stop.is_some_and(|dst| levels[dst as usize] != UNREACHED) {
            break;
        }
    }
    levels
}

impl Graph {
    /// Unweighted shortest path from `src` to `dst`, by hop count.
    ///
    /// The search runs over the CSR snapshot; the path is then traced backwards from
    /// `dst` through the LMDB in-adjacency, taking the first predecessor one hop
    /// closer to the source. Which of several equally short paths that yields is
    /// determined by the adjacency order (ascending edge id), so the answer is
    /// stable for a given graph.
    pub fn shortest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        if src == dst {
            return Ok(Some(vec![src]));
        }
        self.with_snapshot(|snap| {
            let (Some(&src_dense), Some(&dst_dense)) =
                (snap.id_to_dense.get(&src), snap.id_to_dense.get(&dst))
            else {
                return Ok(None);
            };

            let levels = hop_distances(snap, src_dense, Some(dst_dense));
            if levels[dst_dense as usize] == UNREACHED {
                return Ok(None);
            }

            let mut path = vec![dst_dense];
            let mut cur = dst_dense;
            while cur != src_dense {
                let cur_level = levels[cur as usize];
                let mut moved = false;
                for ne in self.adj_entries(snap.dense_to_id[cur as usize], false)? {
                    if let Some(&pred) = snap.id_to_dense.get(&ne.node) {
                        if levels[pred as usize] == cur_level - 1 {
                            path.push(pred);
                            cur = pred;
                            moved = true;
                            break;
                        }
                    }
                }
                if !moved {
                    return Ok(None);
                }
            }

            path.reverse();
            Ok(Some(
                path.into_iter()
                    .map(|d| snap.dense_to_id[d as usize])
                    .collect(),
            ))
        })
    }

    /// Weighted shortest path from `src` to `dst` over the snapshot's per-edge
    /// weights.
    ///
    /// Relaxation is Dijkstra's, over a binary heap, which requires non-negative
    /// weights. A weight here is whatever an edge property holds, so a negative one
    /// is a data condition rather than a bug, and the pass falls back to a bounded
    /// label-correcting relaxation when the snapshot reports any: that handles
    /// negative weights, and is what the SpMV formulation this replaced did in every
    /// case. A reachable negative *cycle* has no shortest path, and is reported as
    /// `Error::InvalidArgument` rather than answered with the last round's distance.
    ///
    /// Parallel edges need no special handling: relaxing each one keeps the
    /// cheapest, which is the `Min` duplicate rule of the weight matrix this
    /// replaced.
    pub(in crate::graph) fn shortest_path_dijkstra_kernel(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<WeightedPath>, Error> {
        if src == dst {
            return Ok(Some(WeightedPath {
                nodes: vec![src],
                total_weight: 0.0,
            }));
        }

        // Present because the caller gated on a weighted snapshot. Its absence is a
        // gating mistake rather than a data condition, which is why it is reported
        // and not worked around.
        let weights = snap.edge_weight.as_ref().ok_or_else(|| {
            Error::InvalidArgument(
                "Dijkstra needs per-edge weights; the snapshot was built without them".to_string(),
            )
        })?;

        let n = snap.dense_to_id.len();
        let (Some(&src_dense), Some(&dst_dense)) =
            (snap.id_to_dense.get(&src), snap.id_to_dense.get(&dst))
        else {
            return Ok(None);
        };

        let mut dist = vec![f64::INFINITY; n];
        dist[src_dense as usize] = 0.0;

        if snap.has_negative_weight {
            // Label-correcting: one round per node, stopping as soon as a round
            // changes nothing. `n` rounds settle every distance in a graph with no
            // negative cycle, so a round still improving something after that proves
            // one is reachable and no shortest path exists. Report that rather than
            // returning whatever the last round happened to leave behind, which is a
            // confident wrong distance.
            let mut settled = false;
            for _ in 0..n {
                let mut changed = false;
                for u in 0..n {
                    if !dist[u].is_finite() {
                        continue;
                    }
                    let row = snap.row_ptr[u]..snap.row_ptr[u + 1];
                    for (&neighbor, &weight) in snap.col_idx[row.clone()].iter().zip(&weights[row])
                    {
                        let v = neighbor as usize;
                        let candidate = dist[u] + weight;
                        if candidate < dist[v] {
                            dist[v] = candidate;
                            changed = true;
                        }
                    }
                }
                if !changed {
                    settled = true;
                    break;
                }
            }
            if !settled {
                return Err(Error::InvalidArgument(
                    "a negative-weight cycle is reachable from the source, so no shortest path is \
                     defined"
                        .to_string(),
                ));
            }
        } else {
            let mut heap = BinaryHeap::new();
            heap.push(Step {
                cost: Reverse(TotalF64(0.0)),
                node: src_dense,
            });
            while let Some(Step {
                cost: Reverse(TotalF64(cost)),
                node,
            }) = heap.pop()
            {
                // A node can be queued more than once; the first pop is the settled
                // distance and any later one is stale.
                if cost > dist[node as usize] {
                    continue;
                }
                let row = snap.row_ptr[node as usize]..snap.row_ptr[node as usize + 1];
                for (&v, &weight) in snap.col_idx[row.clone()].iter().zip(&weights[row]) {
                    let candidate = cost + weight;
                    if candidate < dist[v as usize] {
                        dist[v as usize] = candidate;
                        heap.push(Step {
                            cost: Reverse(TotalF64(candidate)),
                            node: v,
                        });
                    }
                }
            }
        }

        let total_cost = dist[dst_dense as usize];
        if !total_cost.is_finite() {
            return Ok(None);
        }

        // Trace back through the in-adjacency, taking the first predecessor whose
        // settled distance plus the connecting edge's weight lands on this node. The
        // weight comes from the same `weights` array `dist` was computed from, found
        // by binary-searching the predecessor's outgoing row, which the builder sorts
        // by edge id. Re-reading the property from storage instead would open a
        // transaction per candidate edge and, worse, could read a weight a concurrent
        // `update_edge` has changed since the search: no predecessor would then
        // satisfy the equation and a real path would be reported as none.
        let mut path = vec![dst_dense];
        let mut cur = dst_dense;
        // A shortest path visits at most every node once. Without this bound a
        // zero-weight cycle off the source makes the walk ping-pong between two nodes
        // forever, since both satisfy the equation for the other.
        while cur != src_dense {
            if path.len() > n {
                return Ok(None);
            }
            let cur_dist = dist[cur as usize];
            let mut moved = false;
            for ne in self.adj_entries(snap.dense_to_id[cur as usize], false)? {
                let Some(&pred) = snap.id_to_dense.get(&ne.node) else {
                    continue;
                };
                let pred_dist = dist[pred as usize];
                if !pred_dist.is_finite() {
                    continue;
                }
                let row = snap.row_ptr[pred as usize]..snap.row_ptr[pred as usize + 1];
                let Ok(offset) = snap.edge_id[row.clone()].binary_search(&ne.edge) else {
                    continue;
                };
                let weight = weights[row.start + offset];
                // Relative tolerance: `cur_dist` is an accumulated total, so with
                // weights of any magnitude its own ULP can exceed a fixed epsilon,
                // and an absolute test would match no predecessor at all.
                let scale = cur_dist.abs().max(1.0);
                if (pred_dist + weight - cur_dist).abs() < 1e-9 * scale {
                    path.push(pred);
                    cur = pred;
                    moved = true;
                    break;
                }
            }
            if !moved {
                return Ok(None);
            }
        }

        path.reverse();
        Ok(Some(WeightedPath {
            nodes: path
                .into_iter()
                .map(|d| snap.dense_to_id[d as usize])
                .collect(),
            total_weight: total_cost,
        }))
    }

    /// Depth-first search over the contiguous CSR snapshot arrays.
    ///
    /// The one depth-first kernel here that is still recursive, and deliberately so:
    /// `hops` is a `u8`, so the recursion is bounded at 255 frames whatever the graph
    /// looks like. The others had no such bound and are iterative for the reason on
    /// [`Graph::detect_cycle_kernel`]. Widening `hops` would remove the bound and this
    /// would need converting too.
    pub(in crate::graph) fn dfs_kernel(
        &self,
        snap: &CsrSnapshot,
        start: NodeId,
        hops: u8,
    ) -> Result<Vec<NodeId>, Error> {
        // Track the shallowest depth at which each node has been reached. A
        // plain visited set would under-report: a node first discovered via a
        // longer branch gets pruned, so nodes that are within `hops` along a
        // shorter path (and their deeper neighbors) would be missed. Re-expand
        // whenever a node is reached at a strictly shallower depth so the result
        // is every node within `hops`, in DFS discovery order.
        let mut best_depth: AHashMap<NodeId, u8> = AHashMap::new();
        let mut order: Vec<NodeId> = Vec::new();

        fn dfs_recurse(
            snap: &CsrSnapshot,
            node: NodeId,
            depth: u8,
            max_depth: u8,
            best_depth: &mut AHashMap<NodeId, u8>,
            order: &mut Vec<NodeId>,
        ) {
            match best_depth.get(&node) {
                Some(&d) if d <= depth => return,
                Some(_) => {}
                None => order.push(node),
            }
            best_depth.insert(node, depth);

            if depth < max_depth {
                if let Some(dense) = snap.id_to_dense.get(&node) {
                    let start_idx = snap.row_ptr[*dense as usize];
                    let end_idx = snap.row_ptr[*dense as usize + 1];
                    for k in start_idx..end_idx {
                        let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                        dfs_recurse(snap, neighbor, depth + 1, max_depth, best_depth, order);
                    }
                }
            }
        }

        dfs_recurse(snap, start, 0, hops, &mut best_depth, &mut order);
        Ok(order)
    }

    /// Directed cycle detection by three-color DFS over the CSR snapshot arrays.
    ///
    /// Iterative, with the search stack on the heap. The depth of a DFS is the length
    /// of the current path, so a recursive version needs one call frame per node on a
    /// chain and aborts the *process* on a stack overflow rather than returning an
    /// error — a query cannot be allowed to do that. Every kernel here that walks
    /// depth-first is written this way for that reason.
    pub(in crate::graph) fn detect_cycle_kernel(&self, snap: &CsrSnapshot) -> Result<bool, Error> {
        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;

        let n = snap.dense_to_id.len();
        let mut state = vec![WHITE; n];
        // Each frame is a node plus how far through its row the search has gone, which
        // is what the recursive form kept implicitly in the loop counter of each frame.
        let mut stack: Vec<(usize, usize)> = Vec::new();

        for root in 0..n {
            if state[root] != WHITE {
                continue;
            }
            state[root] = GRAY;
            stack.push((root, snap.row_ptr[root]));

            while let Some(&mut (u, ref mut cursor)) = stack.last_mut() {
                if *cursor < snap.row_ptr[u + 1] {
                    let v = snap.col_idx[*cursor] as usize;
                    *cursor += 1;
                    match state[v] {
                        // An edge back to a node still on the current path closes a
                        // cycle. Black is a finished node, which is reachable again
                        // without forming one.
                        GRAY => return Ok(true),
                        WHITE => {
                            state[v] = GRAY;
                            stack.push((v, snap.row_ptr[v]));
                        }
                        _ => {}
                    }
                } else {
                    state[u] = BLACK;
                    stack.pop();
                }
            }
        }

        Ok(false)
    }

    /// All simple paths between `src` and `dst` over the CSR snapshot arrays.
    ///
    /// Iterative for the reason on [`Graph::detect_cycle_kernel`]: the search depth is
    /// the path length, so recursion put one frame per node on the stack. The number
    /// of paths can still be exponential in the graph — that is the question's shape,
    /// not this implementation's — but the *stack* is now bounded by the longest path
    /// and lives on the heap.
    pub(in crate::graph) fn all_paths_kernel(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
        let mut paths = Vec::new();
        self.walk_simple_paths(snap, src, dst, |path| paths.push(path.to_vec()));
        Ok(paths)
    }

    /// Enumerate every simple path from `src` to `dst`, calling `emit` with each.
    ///
    /// Shared by `all_paths` and `longest_path`, which differ only in what they keep.
    /// Neighbors are walked in row order and a path is emitted the moment `dst` is
    /// reached, so the sequence matches the depth-first recursion this replaced.
    fn walk_simple_paths(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
        mut emit: impl FnMut(&[NodeId]),
    ) {
        // The source counts as reached before any edge is walked, which is what makes
        // `all_paths(a, a)` one empty-length path rather than none.
        if src == dst {
            emit(&[src]);
            return;
        }
        let Some(&src_dense) = snap.id_to_dense.get(&src) else {
            return;
        };

        let mut current_path = vec![src];
        let mut visited = AHashSet::from_iter([src]);
        // Frame: the node being expanded and how far through its row we are. The dense
        // index is carried so a step costs no hash lookup.
        let mut stack: Vec<(u32, usize)> = vec![(src_dense, snap.row_ptr[src_dense as usize])];

        while let Some(&mut (u_dense, ref mut cursor)) = stack.last_mut() {
            if *cursor >= snap.row_ptr[u_dense as usize + 1] {
                stack.pop();
                // The root's entry belongs to the caller's `current_path`, so it is
                // never unwound; every deeper node is.
                if !stack.is_empty() {
                    if let Some(left) = current_path.pop() {
                        visited.remove(&left);
                    }
                }
                continue;
            }
            let neighbor_dense = snap.col_idx[*cursor];
            *cursor += 1;
            let neighbor = snap.dense_to_id[neighbor_dense as usize];
            if visited.contains(&neighbor) {
                continue;
            }
            if neighbor == dst {
                // Reaching the destination ends this path without extending past it,
                // matching the recursion's check on entry.
                current_path.push(neighbor);
                emit(&current_path);
                current_path.pop();
                continue;
            }
            visited.insert(neighbor);
            current_path.push(neighbor);
            stack.push((neighbor_dense, snap.row_ptr[neighbor_dense as usize]));
        }
    }

    /// Every unweighted shortest path between `src` and `dst`.
    pub(in crate::graph) fn all_shortest_paths_kernel(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
        if src == dst {
            return Ok(vec![vec![src]]);
        }

        let (Some(&src_dense), Some(&dst_dense)) =
            (snap.id_to_dense.get(&src), snap.id_to_dense.get(&dst))
        else {
            return Ok(vec![]);
        };

        let levels = hop_distances(snap, src_dense, Some(dst_dense));
        if levels[dst_dense as usize] == UNREACHED {
            return Ok(vec![]);
        }

        // Walk back from `dst` over the shortest-path DAG, iteratively: the depth is the
        // path length, so recursion needed a frame per hop (see
        // [`Graph::detect_cycle_kernel`]).
        //
        // A frame holds its node's admissible predecessors rather than a cursor into
        // storage, because the predecessors come from an LMDB read per node and are
        // filtered before use. Two filters apply:
        //
        // - one hop closer to the source, which is what makes the walk follow shortest
        //   paths rather than any path;
        // - distinct *predecessors*, not incoming edges. `adj_entries` yields one entry
        //   per edge, so two edges from the same predecessor would emit the same node
        //   sequence twice. A path is a sequence of nodes, so that is one path; the
        //   betweenness kernel counts distinct pairs for the same reason and the two must
        //   agree about what a shortest path is. Deduplication needs a set rather than a
        //   check against the previous entry, since `in_adj` is `DUPSORT` over `AdjEntry`
        //   whose byte layout puts `edge_type` ahead of `other`: one predecessor reached
        //   by two relationship types is separated by every entry whose type sorts
        //   between them.
        let admissible = |node: NodeId| -> Result<Vec<NodeId>, Error> {
            let Some(&dense) = snap.id_to_dense.get(&node) else {
                return Ok(Vec::new());
            };
            let cur_level = levels[dense as usize];
            if cur_level == UNREACHED {
                return Ok(Vec::new());
            }
            let mut seen: AHashSet<NodeId> = AHashSet::new();
            let mut preds = Vec::new();
            for ne in self.adj_entries(node, false)? {
                if let Some(&pred) = snap.id_to_dense.get(&ne.node) {
                    if levels[pred as usize] == cur_level - 1 && seen.insert(ne.node) {
                        preds.push(ne.node);
                    }
                }
            }
            Ok(preds)
        };

        let mut paths = Vec::new();
        // `current_path` runs destination-first and is reversed on emit, as the
        // recursion did.
        let mut current_path = vec![dst];
        let mut stack: Vec<(Vec<NodeId>, usize)> = vec![(admissible(dst)?, 0)];

        while let Some((preds, cursor)) = stack.last_mut() {
            if *cursor >= preds.len() {
                stack.pop();
                if !stack.is_empty() {
                    current_path.pop();
                }
                continue;
            }
            let pred = preds[*cursor];
            *cursor += 1;
            current_path.push(pred);
            if pred == src {
                let mut found = current_path.clone();
                found.reverse();
                paths.push(found);
                current_path.pop();
                continue;
            }
            stack.push((admissible(pred)?, 0));
        }

        Ok(paths)
    }

    /// Yen's k shortest loopless paths over the CSR snapshot arrays.
    pub(in crate::graph) fn shortest_path_top_k_kernel(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
        k: usize,
        weight_property: &str,
    ) -> Result<Vec<(Vec<NodeId>, f64)>, Error> {
        if k == 0 {
            return Ok(vec![]);
        }

        let rtxn = self.storage.env.read_txn()?;

        let find_shortest_path = |s: NodeId,
                                  t: NodeId,
                                  blocked_nodes: &AHashSet<NodeId>,
                                  blocked_edges: &AHashSet<(NodeId, NodeId)>|
         -> Result<Option<(Vec<NodeId>, f64)>, Error> {
            if s == t {
                return Ok(Some((vec![s], 0.0)));
            }

            let mut dist: HashMap<NodeId, f64> = HashMap::new();
            let mut pred: HashMap<NodeId, NodeId> = HashMap::new();
            let mut heap = BinaryHeap::new();

            dist.insert(s, 0.0);
            heap.push(Step {
                cost: Reverse(TotalF64(0.0)),
                node: s,
            });

            while let Some(Step {
                cost: Reverse(TotalF64(cost)),
                node,
            }) = heap.pop()
            {
                if node == t {
                    let mut path = vec![t];
                    let mut cur = t;
                    while cur != s {
                        cur = pred[&cur];
                        path.push(cur);
                    }
                    path.reverse();
                    return Ok(Some((path, cost)));
                }

                if cost > *dist.get(&node).unwrap_or(&f64::INFINITY) {
                    continue;
                }

                if let Some(&node_dense) = snap.id_to_dense.get(&node) {
                    let start = snap.row_ptr[node_dense as usize];
                    let end = snap.row_ptr[node_dense as usize + 1];
                    for k in start..end {
                        let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                        let edge_id = snap.edge_id[k];
                        if blocked_nodes.contains(&neighbor) {
                            continue;
                        }
                        if blocked_edges.contains(&(node, neighbor)) {
                            continue;
                        }

                        let weight = if let Some(edge_record) =
                            self.get_edge_impl(&rtxn, edge_id)?
                        {
                            let props_json: serde_json::Value = props::decode(&edge_record.props)?;
                            if let Some(val) = props_json.get(weight_property) {
                                val.as_f64().unwrap_or(1.0)
                            } else {
                                1.0
                            }
                        } else {
                            1.0
                        };

                        let next_cost = cost + weight;
                        let current_best = *dist.get(&neighbor).unwrap_or(&f64::INFINITY);

                        if next_cost < current_best {
                            dist.insert(neighbor, next_cost);
                            pred.insert(neighbor, node);
                            heap.push(Step {
                                cost: Reverse(TotalF64(next_cost)),
                                node: neighbor,
                            });
                        }
                    }
                }
            }

            Ok(None)
        };

        let first_path_opt = find_shortest_path(src, dst, &AHashSet::new(), &AHashSet::new())?;
        let mut paths = Vec::new();
        if let Some((first_path, first_cost)) = first_path_opt {
            paths.push((first_path, first_cost));
        } else {
            return Ok(vec![]);
        }

        let mut candidates: Vec<(Vec<NodeId>, f64)> = Vec::new();

        for i in 1..k {
            let prev_path = &paths[i - 1].0;

            for j in 0..prev_path.len() - 1 {
                let spur_node = prev_path[j];
                let root_path = &prev_path[0..=j];

                let mut blocked_edges = AHashSet::new();
                let mut blocked_nodes = AHashSet::new();

                for (p, _) in &paths {
                    if p.len() > j && &p[0..=j] == root_path {
                        blocked_edges.insert((p[j], p[j + 1]));
                    }
                }

                for &node in root_path {
                    if node != spur_node {
                        blocked_nodes.insert(node);
                    }
                }

                let spur_path_opt =
                    find_shortest_path(spur_node, dst, &blocked_nodes, &blocked_edges)?;
                if let Some((spur_path, spur_cost)) = spur_path_opt {
                    let mut total_path = root_path.to_vec();
                    total_path.extend_from_slice(&spur_path[1..]);

                    let mut root_cost = 0.0;
                    for m_idx in 0..root_path.len() - 1 {
                        let u = root_path[m_idx];
                        let v = root_path[m_idx + 1];
                        let mut min_w = f64::INFINITY;
                        if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                            let start = snap.row_ptr[u_dense as usize];
                            let end = snap.row_ptr[u_dense as usize + 1];
                            for k_idx in start..end {
                                let neighbor = snap.dense_to_id[snap.col_idx[k_idx] as usize];
                                let edge_id = snap.edge_id[k_idx];
                                if neighbor == v {
                                    let weight = if let Some(edge_record) =
                                        self.get_edge_impl(&rtxn, edge_id)?
                                    {
                                        let props_json: serde_json::Value =
                                            props::decode(&edge_record.props)?;
                                        if let Some(val) = props_json.get(weight_property) {
                                            val.as_f64().unwrap_or(1.0)
                                        } else {
                                            1.0
                                        }
                                    } else {
                                        1.0
                                    };
                                    if weight < min_w {
                                        min_w = weight;
                                    }
                                }
                            }
                        }
                        if min_w == f64::INFINITY {
                            root_cost += 1.0;
                        } else {
                            root_cost += min_w;
                        }
                    }

                    let total_cost = root_cost + spur_cost;
                    if !candidates.iter().any(|(p, _)| p == &total_path) {
                        candidates.push((total_path, total_cost));
                    }
                }
            }

            if candidates.is_empty() {
                break;
            }

            candidates.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| b.0.cmp(&a.0))
            });

            if let Some(best_cand) = candidates.pop() {
                paths.push(best_cand);
            } else {
                break;
            }
        }

        Ok(paths)
    }

    /// Longest simple path between `src` and `dst` over the CSR snapshot arrays.
    pub(in crate::graph) fn longest_path_kernel(
        &self,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        // Same enumeration as `all_paths`, keeping only the longest. Strictly greater,
        // so the first path of a given length wins and the choice among equal-length
        // paths stays the depth-first one.
        let mut max_path: Option<Vec<NodeId>> = None;
        self.walk_simple_paths(snap, src, dst, |path| {
            if max_path.as_ref().is_none_or(|best| path.len() > best.len()) {
                max_path = Some(path.to_vec());
            }
        });
        Ok(max_path)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::{Error, Graph};

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// Dijkstra takes the cheapest of several parallel edges, which is the `Min`
    /// duplicate rule of the weight matrix this replaced.
    #[test]
    fn dijkstra_takes_the_cheapest_parallel_edge() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &serde_json::json!({ "weight": 7.0 }))
            .unwrap();
        g.add_edge(a, b, "E", &serde_json::json!({ "weight": 2.0 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        let path = g.shortest_path_dijkstra(a, b).unwrap().unwrap();
        assert_eq!(path.nodes, vec![a, b]);
        assert_eq!(path.total_weight, 2.0);
    }

    /// A negative weight comes from data, not from a bug, so the pass falls back to
    /// a label-correcting relaxation rather than reporting the wrong distance a
    /// heap-ordered search would settle on.
    #[test]
    fn dijkstra_handles_a_negative_weight() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let c = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &serde_json::json!({ "weight": 5.0 }))
            .unwrap();
        g.add_edge(b, c, "E", &serde_json::json!({ "weight": -4.0 }))
            .unwrap();
        g.add_edge(a, c, "E", &serde_json::json!({ "weight": 2.0 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        // Through b costs 1, the direct edge costs 2.
        let path = g.shortest_path_dijkstra(a, c).unwrap().unwrap();
        assert_eq!(path.nodes, vec![a, b, c]);
        assert_eq!(path.total_weight, 1.0);
    }

    /// A reachable negative-weight cycle means no shortest path exists, so the call
    /// must say so instead of returning the distance the last relaxation round left
    /// behind.
    #[test]
    fn dijkstra_reports_a_negative_cycle() {
        let (_dir, g) = open_tmp();
        let s = g.add_node("N", &()).unwrap();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.add_edge(s, a, "E", &serde_json::json!({ "weight": 1.0 }))
            .unwrap();
        // a -> b -> a sums to -1, so each lap makes any distance through it smaller.
        g.add_edge(a, b, "E", &serde_json::json!({ "weight": 1.0 }))
            .unwrap();
        g.add_edge(b, a, "E", &serde_json::json!({ "weight": -2.0 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        let err = g.shortest_path_dijkstra(s, b).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument(ref m) if m.contains("negative-weight cycle")),
            "expected a negative-cycle report, got {err:?}"
        );
    }

    /// A zero-weight cycle makes two nodes each look like the other's predecessor, so
    /// the backward walk has to be bounded. Without the bound it ping-pongs forever,
    /// growing the path until memory runs out.
    #[test]
    fn dijkstra_terminates_on_a_zero_weight_cycle() {
        let (_dir, g) = open_tmp();
        // `a` and `b` are created before `s` on purpose. The backward walk takes the
        // first in-neighbour that fits, and `in_adj` orders duplicates by the raw
        // bytes of the neighbour id, so `b` is only considered before `s` when it has
        // the lower id. Created the other way round the walk escapes to `s`
        // immediately and the cycle is never entered.
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let s = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &serde_json::json!({ "weight": 0.0 }))
            .unwrap();
        g.add_edge(b, a, "E", &serde_json::json!({ "weight": 0.0 }))
            .unwrap();
        g.add_edge(s, a, "E", &serde_json::json!({ "weight": 1.0 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        // Whatever it settles on, it must return rather than hang.
        let found = g.shortest_path_dijkstra(s, b).unwrap();
        if let Some(path) = found {
            assert_eq!(path.total_weight, 1.0);
            assert_eq!(path.nodes.first(), Some(&s));
            assert_eq!(path.nodes.last(), Some(&b));
        }
    }

    /// Large fractional weights must still reconstruct.
    ///
    /// The tolerance in the backward walk is relative rather than absolute, which is
    /// defensive rather than load-bearing: `dist[v]` was computed as `dist[u] + w` by
    /// the same addition the walk re-checks, so for the predecessor the search
    /// actually settled on the difference is exactly zero whatever the magnitude. The
    /// tolerance only decides how a *tie* through some other predecessor is treated.
    #[test]
    fn dijkstra_reconstructs_with_large_weights() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<_> = (0..6).map(|_| g.add_node("N", &()).unwrap()).collect();
        for w in nodes.windows(2) {
            g.add_edge(
                w[0],
                w[1],
                "E",
                &serde_json::json!({ "weight": 1_300_000_000.1f64 }),
            )
            .unwrap();
        }
        g.rebuild_csr().unwrap();

        let path = g
            .shortest_path_dijkstra(nodes[0], nodes[5])
            .unwrap()
            .expect("a finite distance must reconstruct to a path");
        assert_eq!(path.nodes, nodes);
    }

    /// The weight is the first present of the four accepted property names, and an
    /// edge carrying none of them weighs 1.
    #[test]
    fn dijkstra_defaults_an_unweighted_edge_to_one() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let c = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &()).unwrap();
        g.add_edge(b, c, "E", &serde_json::json!({ "cost": 3.0 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        let path = g.shortest_path_dijkstra(a, c).unwrap().unwrap();
        assert_eq!(path.total_weight, 4.0);
    }
}

#[cfg(test)]
mod multigraph_tests {
    use tempfile::TempDir;

    use crate::Graph;

    /// A shortest path is a sequence of nodes, so a second edge between an already
    /// joined pair is not a second path. The same rule is pinned for betweenness in
    /// `kernels::analytics`, which counts distinct predecessor pairs for exactly this
    /// reason; the two kernels must agree about what a shortest path is.
    #[test]
    fn all_shortest_paths_ignores_parallel_edges() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let nodes: Vec<_> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
        // Diamond a->b->d and a->c->d, with b->d doubled.
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[3], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[3], "E", &()).unwrap();
        g.add_edge(nodes[0], nodes[2], "E", &()).unwrap();
        g.add_edge(nodes[2], nodes[3], "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let paths = g.all_shortest_paths(nodes[0], nodes[3]).unwrap();
        assert_eq!(
            paths.len(),
            2,
            "two distinct shortest paths, not one per edge: {paths:?}"
        );
    }

    /// The duplicate predecessor need not arrive consecutively. `in_adj` is `DUPSORT`
    /// over `AdjEntry`, whose byte layout puts `edge_type` before `other`, so the same
    /// predecessor reached by two *different* relationship types is separated by any
    /// entry whose type sorts between them. A check against only the previous entry
    /// misses that, which is why the dedup is a set.
    #[test]
    fn all_shortest_paths_dedups_a_predecessor_split_across_types() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let nodes: Vec<_> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[0], nodes[2], "E", &()).unwrap();
        // Into d: b via type A, c via type B, then b again via type C. Ordered by type,
        // b's two entries sit either side of c's.
        g.add_edge(nodes[1], nodes[3], "A", &()).unwrap();
        g.add_edge(nodes[2], nodes[3], "B", &()).unwrap();
        g.add_edge(nodes[1], nodes[3], "C", &()).unwrap();
        g.rebuild_csr().unwrap();

        let paths = g.all_shortest_paths(nodes[0], nodes[3]).unwrap();
        assert_eq!(paths.len(), 2, "one path per distinct route: {paths:?}");
    }
}

#[cfg(test)]
mod deep_graph_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{DegreeDirection, Graph};

    /// Every depth-first kernel must survive a graph deeper than the call stack.
    ///
    /// These were recursive, one frame per node on the current path, so a long chain
    /// overflowed the stack — and a Rust stack overflow aborts the *process*, so a
    /// single query could take down a server rather than returning an error. This runs
    /// them on a thread with a 1 MiB stack, which is `wasm32-unknown-unknown`'s default
    /// and about an eighth of a native main thread, over a chain far longer than that
    /// many frames would fit. A regression here does not fail politely: it kills the
    /// test binary, which is the same bargain `near_budget_queries_run_on_a_small_stack`
    /// makes in `issundb-cypher`.
    #[test]
    fn depth_first_kernels_survive_a_chain_deeper_than_the_stack() {
        const CHAIN: usize = 20_000;

        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let dir = TempDir::new().unwrap();
                let g = Graph::open(dir.path(), 1).unwrap();
                let ids: Vec<_> = g
                    .update(|txn| {
                        let ids: Vec<_> = (0..CHAIN)
                            .map(|_| txn.add_node("N", &json!({})).unwrap())
                            .collect();
                        for w in ids.windows(2) {
                            txn.add_edge(w[0], w[1], "R", &json!({})).unwrap();
                        }
                        Ok(ids)
                    })
                    .unwrap();
                g.rebuild_csr().unwrap();

                let first = ids[0];
                let last = ids[CHAIN - 1];

                // Tarjan: every node is its own component on a chain.
                assert_eq!(g.strongly_connected_components().unwrap().len(), CHAIN);
                // Three-color DFS over the whole graph.
                assert!(!g.detect_cycle().unwrap(), "a chain is acyclic");
                // Simple-path enumeration, and the backward shortest-path walk: on a
                // chain there is exactly one route end to end, of full length.
                assert_eq!(g.all_paths(first, last).unwrap().len(), 1);
                assert_eq!(g.longest_path(first, last).unwrap().unwrap().len(), CHAIN);
                let shortest = g.all_shortest_paths(first, last).unwrap();
                assert_eq!(shortest.len(), 1);
                assert_eq!(shortest[0].len(), CHAIN);

                // A closing edge makes it cyclic, which the same walk must now report.
                g.add_edge(last, first, "R", &json!({})).unwrap();
                g.rebuild_csr().unwrap();
                assert!(g.detect_cycle().unwrap(), "the closed chain is a cycle");
                assert_eq!(
                    g.strongly_connected_components().unwrap().len(),
                    CHAIN,
                    "still one entry per node, now all in one component"
                );

                // A sanity check that the graph is the shape intended, so the
                // assertions above cannot pass over an empty snapshot.
                assert_eq!(
                    g.degree_centrality(DegreeDirection::Out).unwrap()[&first],
                    1
                );
            })
            .expect("spawn a small-stack thread");

        handle.join().expect("no kernel may overflow the stack");
    }
}
