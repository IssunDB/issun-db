use super::*;

use super::traversal::{UNREACHED, expand_frontier};

/// Out-degree of every dense index, counting parallel edges separately.
///
/// This is the row length, so an edge added twice between the same pair counts
/// twice. PageRank divides by it, which is what spreads a source's rank over its
/// edges rather than over its distinct neighbors.
fn out_degrees(snap: &CsrSnapshot) -> Vec<u32> {
    (0..snap.dense_to_id.len())
        .map(|i| (snap.row_ptr[i + 1] - snap.row_ptr[i]) as u32)
        .collect()
}

/// Run `body` over `n` dense indices, split across `threads` disjoint output
/// chunks, and return the filled output.
///
/// `body(lo, out)` fills `out[i]` for the index `lo + i`. Every worker writes its
/// own chunk, so there is no synchronization and the result does not depend on how
/// the range was split.
fn map_dense_range<T: Copy + Send + Default>(
    n: usize,
    threads: usize,
    body: impl Fn(usize, &mut [T]) + Send + Sync,
) -> Vec<T> {
    let mut out = vec![T::default(); n];
    fill_dense_range(&mut out, threads, body);
    out
}

/// [`map_dense_range`] into a buffer the caller owns.
///
/// The split is for an iterative pass: PageRank alternates two buffers and swaps them,
/// so it allocates twice rather than once per iteration. On a 1 M-node graph the
/// allocating form churned 4 MB per iteration inside the hot loop.
fn fill_dense_range<T: Copy + Send>(
    out: &mut [T],
    threads: usize,
    body: impl Fn(usize, &mut [T]) + Send + Sync,
) {
    let n = out.len();
    if threads <= 1 || n == 0 {
        body(0, out);
        return;
    }
    let chunk = n.div_ceil(threads);
    std::thread::scope(|scope| {
        let workers: Vec<_> = out
            .chunks_mut(chunk)
            .enumerate()
            .map(|(t, slice)| {
                let body = &body;
                scope.spawn(move || body(t * chunk, slice))
            })
            .collect();
        for worker in workers {
            // A worker only reads the snapshot and writes its own chunk, so a panic is
            // a bug in this kernel, not a data condition. Resume the unwind rather
            // than converting it: `Error::Corrupt` tells an operator to restore a
            // backup, and it would also throw away the payload and the backtrace.
            if let Err(payload) = worker.join() {
                std::panic::resume_unwind(payload);
            }
        }
    });
}

impl Graph {
    /// PageRank by power iteration over the CSR snapshot.
    ///
    /// Each iteration accumulates `raw[j] = sum over edges i -> j of
    /// rank[i] / out_degree(i)`, then applies the damping formula
    /// `rank[j] = d * raw[j] + (1 - d) / n`. A node with no incoming edges receives
    /// only the teleportation term. The rank mass of dangling nodes (no outgoing
    /// edges) is not redistributed, so ranks do not sum to 1; that is a deliberate
    /// simplification, and `tests/oracle.rs` compares against NetworkX over a
    /// corpus restricted to graphs with no dangling nodes for exactly that reason.
    ///
    /// The accumulation reads the *incoming* rows, so each output entry is a sum
    /// over one node's in-edges. That is what makes the pass parallel over disjoint
    /// output chunks and what makes the result independent of the worker count: a
    /// push formulation over outgoing rows would have several workers accumulating
    /// into the same entry, needing either a lock or per-worker buffers, and would
    /// leave the summation order dependent on the split.
    pub fn page_rank(&self, iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error> {
        self.with_snapshot(|snap| {
            let n = snap.dense_to_id.len();
            if n == 0 {
                return Ok(HashMap::new());
            }

            let out_deg = out_degrees(snap);
            // Streaming, like a counting pass: one pass over the incoming rows per
            // iteration, so the memory-bandwidth cap applies rather than the
            // compute-bound budget the all-pairs passes take.
            let threads = self.kernel_threads(n.saturating_add(snap.col_idx.len()));
            let base = (1.0 - damping) / n as f32;
            let mut rank = vec![1.0f32 / n as f32; n];
            // Two buffers for the whole run rather than one allocation per iteration.
            let mut next = vec![0.0f32; n];

            for _ in 0..iterations {
                {
                    let previous = &rank;
                    let out_deg = &out_deg;
                    fill_dense_range(&mut next, threads, move |lo, slice| {
                        for (offset, value) in slice.iter_mut().enumerate() {
                            let j = lo + offset;
                            let mut sum = 0.0f32;
                            for k in snap.in_row_ptr[j]..snap.in_row_ptr[j + 1] {
                                let i = snap.in_col_idx[k] as usize;
                                // A node with no outgoing edges cannot be the tail
                                // of an incoming edge, so the degree is never zero.
                                sum += previous[i] / out_deg[i] as f32;
                            }
                            *value = damping * sum + base;
                        }
                    });
                }
                std::mem::swap(&mut rank, &mut next);
            }

            Ok(snap
                .dense_to_id
                .iter()
                .enumerate()
                .map(|(dense, &id)| (id, rank[dense]))
                .collect())
        })
    }

    /// Weakly connected components by union-find over the adjacency, treating every
    /// edge as undirected.
    ///
    /// The component id is the smallest *node id* in the component. Unioning toward
    /// the smaller root finds the smallest dense index, which is then mapped back
    /// through `dense_to_id`. The mapping step is not redundant: a dense index is a
    /// node's rank in the sorted id list, so it equals the id only while ids run
    /// `0..n` with nothing deleted, and reporting the raw index on a graph that has
    /// had a node deleted would name a component after an id no live node holds.
    /// Only the induced partition is guaranteed by the public contract.
    pub(in crate::graph) fn connected_components_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mut parent: Vec<u32> = (0..n as u32).collect();

        fn find(parent: &mut [u32], mut node: u32) -> u32 {
            while parent[node as usize] != node {
                // Path halving: point the node at its grandparent as the walk
                // climbs, so a long chain is flattened without a second pass.
                let grandparent = parent[parent[node as usize] as usize];
                parent[node as usize] = grandparent;
                node = grandparent;
            }
            node
        }

        for u in 0..n {
            for k in snap.row_ptr[u]..snap.row_ptr[u + 1] {
                let (a, b) = (
                    find(&mut parent, u as u32),
                    find(&mut parent, snap.col_idx[k]),
                );
                if a != b {
                    // Toward the smaller root, so a component's representative is
                    // its smallest dense index.
                    let (root, child) = if a < b { (a, b) } else { (b, a) };
                    parent[child as usize] = root;
                }
            }
        }

        Ok((0..n)
            .map(|d| {
                let root = find(&mut parent, d as u32);
                (snap.dense_to_id[d], snap.dense_to_id[root as usize])
            })
            .collect())
    }

    /// Strongly connected components (Tarjan) over the contiguous CSR arrays.
    ///
    /// Iterative, for the reason given on [`Graph::detect_cycle_kernel`]: Tarjan is a
    /// depth-first search, so recursion put one call frame per node on the current path
    /// and a long chain aborted the process instead of returning.
    ///
    /// The translation keeps the recursive algorithm exactly, including the order
    /// components are emitted, because that order is what assigns the component ids: a
    /// component is emitted when its root finishes, so ids still ascend in
    /// root-completion order. What was the recursion's implicit per-frame state is now
    /// explicit — the node, and how far through its row the search has gone — and the
    /// `lowlink` propagation that happened on return from a call now happens when a
    /// frame is popped, against the frame beneath it.
    pub(in crate::graph) fn strongly_connected_components_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mut index = 0usize;
        let mut indices: Vec<Option<usize>> = vec![None; n];
        let mut lowlinks = vec![0usize; n];
        let mut on_stack = vec![false; n];
        // Tarjan's own stack of nodes awaiting assignment, distinct from the search
        // stack below.
        let mut pending: Vec<usize> = Vec::with_capacity(n);
        let mut components: HashMap<NodeId, u64> = HashMap::with_capacity(n);
        let mut next_comp_id = 0u64;
        let mut search: Vec<(usize, usize)> = Vec::new();

        for root in 0..n {
            if indices[root].is_some() {
                continue;
            }
            indices[root] = Some(index);
            lowlinks[root] = index;
            index += 1;
            pending.push(root);
            on_stack[root] = true;
            search.push((root, snap.row_ptr[root]));

            while let Some(&mut (u, ref mut cursor)) = search.last_mut() {
                if *cursor < snap.row_ptr[u + 1] {
                    let v = snap.col_idx[*cursor] as usize;
                    *cursor += 1;
                    match indices[v] {
                        None => {
                            // Descend. The `lowlinks[u] = min(lowlinks[u], lowlinks[v])`
                            // the recursion ran after the call happens when this frame
                            // is popped.
                            indices[v] = Some(index);
                            lowlinks[v] = index;
                            index += 1;
                            pending.push(v);
                            on_stack[v] = true;
                            search.push((v, snap.row_ptr[v]));
                        }
                        Some(iv) if on_stack[v] => {
                            lowlinks[u] = lowlinks[u].min(iv);
                        }
                        // Already assigned to a finished component: not part of this one.
                        Some(_) => {}
                    }
                    continue;
                }

                // `u` is finished.
                search.pop();
                if let Some(&(parent, _)) = search.last() {
                    lowlinks[parent] = lowlinks[parent].min(lowlinks[u]);
                }
                if Some(lowlinks[u]) == indices[u] {
                    let comp_id = next_comp_id;
                    next_comp_id += 1;
                    while let Some(w) = pending.pop() {
                        on_stack[w] = false;
                        if let Some(&node_id) = snap.dense_to_id.get(w) {
                            components.insert(node_id, comp_id);
                        }
                        if w == u {
                            break;
                        }
                    }
                }
            }
        }

        Ok(components)
    }

    /// Betweenness centrality (Brandes) over the CSR snapshot, unnormalized and
    /// directed.
    ///
    /// One breadth-first pass per source builds the shortest-path DAG (levels,
    /// path counts, and predecessor lists), then the dependency accumulation walks
    /// the levels back. Predecessors are collected in ascending dense order and
    /// sources are walked in ascending order, so the accumulation sequence is fixed.
    ///
    /// Sources are independent, so the pass splits over them. Each worker keeps its
    /// own totals and the partials are summed in worker order; floating-point
    /// addition is not associative, so the last bits of a total can depend on how
    /// many workers ran. A pass small enough for a test stays on one worker (see
    /// [`Graph::kernel_threads`]), which is what keeps the unit tests exact.
    pub(in crate::graph) fn betweenness_centrality_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        // All-pairs work, so the estimate multiplies one search by the source
        // count. Sizing it as a single pass would leave every realistic graph
        // below the parallel threshold.
        let per_source = n.saturating_add(snap.col_idx.len());
        let threads = self.parallel_threads(n.saturating_mul(per_source));

        let accumulate = |sources: std::ops::Range<usize>| -> Vec<f64> {
            let mut totals = vec![0.0f64; n];
            let mut levels = vec![UNREACHED; n];
            let mut sigma = vec![0u64; n];
            let mut delta = vec![0.0f64; n];
            let mut pred: Vec<Vec<u32>> = vec![Vec::new(); n];
            let mut order: Vec<Vec<u32>> = Vec::new();
            let mut next = Vec::new();

            for s in sources {
                levels.fill(UNREACHED);
                sigma.fill(0);
                delta.fill(0.0);
                for list in pred.iter_mut() {
                    list.clear();
                }
                order.clear();

                levels[s] = 0;
                sigma[s] = 1;
                order.push(vec![s as u32]);

                // `current` indexes the level being expanded, and `hop` is the level
                // being discovered, so the two advance together. `order` always holds
                // `current`: the source level was just pushed, and every iteration
                // pushes the level it discovers before advancing.
                for (current, hop) in (1u32..).enumerate() {
                    expand_frontier(snap, &order[current], hop, &mut levels, &mut next);
                    if next.is_empty() {
                        break;
                    }
                    // Second pass over the same edges, now that this level's members
                    // are marked: an edge from the previous level into one of them is
                    // a shortest-path edge. Walking the frontier in ascending order
                    // leaves each predecessor list ascending.
                    //
                    // A pair counts once however many edges join it. Betweenness is
                    // defined over shortest *paths*, and two parallel edges are one
                    // path through `v`, so crediting per edge would multiply
                    // `sigma[w]` and push `v` twice, inflating every score downstream
                    // on a multigraph. Consecutive duplicates are all this can
                    // produce, since one `v` contributes its whole row before the next
                    // is walked, so the last entry is the whole test.
                    for &v in &order[current] {
                        for &w in
                            &snap.col_idx[snap.row_ptr[v as usize]..snap.row_ptr[v as usize + 1]]
                        {
                            if levels[w as usize] == hop && pred[w as usize].last() != Some(&v) {
                                sigma[w as usize] += sigma[v as usize];
                                pred[w as usize].push(v);
                            }
                        }
                    }
                    order.push(std::mem::take(&mut next));
                }

                // From the deepest level back, skipping the source's own level: the
                // source has no predecessors to credit, and its dependency value is
                // not part of anyone's betweenness.
                for level in order[1..].iter().rev() {
                    for &w in level {
                        let w = w as usize;
                        let dw = delta[w];
                        if sigma[w] > 0 {
                            for &v in &pred[w] {
                                delta[v as usize] +=
                                    sigma[v as usize] as f64 / sigma[w] as f64 * (1.0 + dw);
                            }
                        }
                        totals[w] += dw;
                    }
                }
            }
            totals
        };

        let mut betweenness = vec![0.0f64; n];
        if threads <= 1 {
            betweenness = accumulate(0..n);
        } else {
            let chunk = n.div_ceil(threads);
            let partials = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads)
                    .map(|t| {
                        let lo = (t * chunk).min(n);
                        let hi = lo.saturating_add(chunk).min(n);
                        let accumulate = &accumulate;
                        scope.spawn(move || accumulate(lo..hi))
                    })
                    .collect();
                let mut partials = Vec::with_capacity(workers.len());
                for worker in workers {
                    match worker.join() {
                        Ok(part) => partials.push(part),
                        // As in `map_dense_range`: a worker panic is a bug here, so
                        // let it reach the caller intact.
                        Err(payload) => std::panic::resume_unwind(payload),
                    }
                }
                partials
            });
            // Summed in worker order, which is ascending source order, so the total
            // is reproducible for a given worker count.
            for part in partials {
                for (total, value) in betweenness.iter_mut().zip(part) {
                    *total += value;
                }
            }
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, betweenness[d]))
            .collect())
    }

    /// Harmonic centrality: for each node, the sum of the reciprocals of its
    /// shortest-path distances to every node it can reach.
    ///
    /// One breadth-first pass per source, each contributing `1 / hop` per node
    /// reached at that hop. Sources are independent and each writes only its own
    /// entry, so the split does not affect the result.
    pub(in crate::graph) fn harmonic_centrality_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let per_source = n.saturating_add(snap.col_idx.len());
        let threads = self.parallel_threads(n.saturating_mul(per_source));
        let centrality = map_dense_range(n, threads, |lo, slice| {
            let mut levels = vec![UNREACHED; n];
            let mut next = Vec::new();
            for (offset, value) in slice.iter_mut().enumerate() {
                let src = lo + offset;
                levels.fill(UNREACHED);
                levels[src] = 0;
                let mut frontier = vec![src as u32];
                let mut sum = 0.0f64;
                for hop in 1.. {
                    expand_frontier(snap, &frontier, hop, &mut levels, &mut next);
                    if next.is_empty() {
                        break;
                    }
                    sum += next.len() as f64 / f64::from(hop);
                    std::mem::swap(&mut frontier, &mut next);
                }
                *value = sum;
            }
        });

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, centrality[d]))
            .collect())
    }

    /// Degree centrality: the number of *distinct* neighbors in the requested
    /// direction.
    ///
    /// Parallel edges between the same pair count once, and `Both` is the distinct
    /// out-neighbors plus the distinct in-neighbors, so a node joined to one
    /// neighbor in both directions scores two. This is the boolean-adjacency
    /// semantics of the SpMV formulation this replaced, kept deliberately: a plain
    /// row length would count parallel edges separately and silently change the
    /// score on a multigraph. A self-loop appears in both directions and so counts
    /// in each.
    pub(in crate::graph) fn degree_centrality_kernel(
        &self,
        snap: &CsrSnapshot,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        // `stamp[v] == source + 1` marks v as already counted for this source, so
        // one buffer serves every row without being cleared between them.
        let distinct = |row_ptr: &[usize], col_idx: &[u32]| -> Vec<u64> {
            let mut stamp = vec![0u64; n];
            (0..n)
                .map(|u| {
                    let mut count = 0;
                    for &neighbor in &col_idx[row_ptr[u]..row_ptr[u + 1]] {
                        let v = neighbor as usize;
                        if stamp[v] != u as u64 + 1 {
                            stamp[v] = u as u64 + 1;
                            count += 1;
                        }
                    }
                    count
                })
                .collect()
        };

        let out_degrees = if matches!(direction, DegreeDirection::Out | DegreeDirection::Both) {
            distinct(&snap.row_ptr, &snap.col_idx)
        } else {
            vec![0; n]
        };
        let in_degrees = if matches!(direction, DegreeDirection::In | DegreeDirection::Both) {
            distinct(&snap.in_row_ptr, &snap.in_col_idx)
        } else {
            vec![0; n]
        };

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(dense, &node_id)| {
                let count = match direction {
                    DegreeDirection::Out => out_degrees[dense],
                    DegreeDirection::In => in_degrees[dense],
                    DegreeDirection::Both => out_degrees[dense] + in_degrees[dense],
                };
                (node_id, count)
            })
            .collect())
    }

    /// Community detection by label propagation (CDLP / LPA).
    pub(in crate::graph) fn label_propagation_kernel(
        &self,
        max_iterations: usize,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let nodes = self.all_nodes()?;
        let mut labels: HashMap<NodeId, u64> = nodes.iter().map(|&n| (n, n)).collect();

        for _ in 0..max_iterations {
            let mut next_labels = labels.clone();
            let mut changed = false;

            for &u in &nodes {
                let neighbors = self.all_neighbors(u)?;
                if neighbors.is_empty() {
                    continue;
                }

                let mut counts: HashMap<u64, usize> = HashMap::new();
                for ne in &neighbors {
                    if let Some(&label) = labels.get(&ne.node) {
                        *counts.entry(label).or_insert(0) += 1;
                    }
                }

                let mut max_label = labels[&u];
                let mut max_count = 0;

                for (&label, &count) in &counts {
                    if count > max_count {
                        max_count = count;
                        max_label = label;
                    } else if count == max_count && label < max_label {
                        max_label = label;
                    }
                }

                if max_label != labels[&u] {
                    next_labels.insert(u, max_label);
                    changed = true;
                }
            }

            labels = next_labels;
            if !changed {
                break;
            }
        }

        Ok(labels)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// Connect every unordered pair among `nodes` with a single directed edge.
    /// `all_neighbors` is direction-agnostic, so one edge per pair yields a
    /// symmetric neighbor relation, i.e. an undirected clique.
    fn add_clique(g: &Graph, nodes: &[NodeId]) {
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                g.add_edge(nodes[i], nodes[j], "E", &()).unwrap();
            }
        }
    }

    /// Canonicalize a label map into sorted groups of node IDs. Only the induced
    /// partition is significant, not the label values.
    fn partition(labels: &HashMap<NodeId, u64>) -> Vec<Vec<NodeId>> {
        let mut groups: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for (&id, &label) in labels {
            groups.entry(label).or_default().push(id);
        }
        let mut parts: Vec<Vec<NodeId>> = groups
            .into_values()
            .map(|mut part| {
                part.sort_unstable();
                part
            })
            .collect();
        parts.sort();
        parts
    }

    /// LPA cannot be compared against NetworkX, whose label propagation is
    /// randomized and yields no canonical partition. Instead these tests pin the
    /// invariants the deterministic implementation must satisfy.
    ///
    /// Three disjoint triangles must collapse to exactly three communities, one
    /// per triangle, matching the weakly connected components. A clique of size
    /// three or more contains an odd cycle, so the synchronous update converges;
    /// a two-clique is bipartite and would oscillate, so triangles are the
    /// smallest safe building block.
    #[test]
    fn label_propagation_resolves_disjoint_cliques_to_components() {
        let (_dir, g) = open_tmp();
        let mut triangles = Vec::new();
        for _ in 0..3 {
            let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
            add_clique(&g, &nodes);
            triangles.push(nodes);
        }
        g.rebuild_csr().unwrap();

        let labels = g.label_propagation(100).unwrap();
        assert_eq!(
            partition(&labels),
            partition(&g.connected_components().unwrap()),
            "community partition must match the connected components"
        );

        let distinct: HashSet<u64> = labels.values().copied().collect();
        assert_eq!(distinct.len(), 3, "expected one community per triangle");
        for tri in &triangles {
            let label = labels[&tri[0]];
            assert!(
                tri.iter().all(|n| labels[n] == label),
                "a triangle was split across communities"
            );
        }
    }

    /// The implementation iterates a `HashMap` of neighbor label counts, whose
    /// order is randomized per process, but breaks ties toward the smallest
    /// label. The result must therefore be identical run to run.
    #[test]
    fn label_propagation_is_deterministic() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..6).map(|_| g.add_node("N", &()).unwrap()).collect();
        add_clique(&g, &nodes[0..3]);
        add_clique(&g, &nodes[3..6]);
        g.add_edge(nodes[2], nodes[3], "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let first = g.label_propagation(100).unwrap();
        let second = g.label_propagation(100).unwrap();
        assert_eq!(first, second, "label propagation must be run-to-run stable");
    }

    /// A component's id is its smallest member's node id. The public contract
    /// promises only the partition, so this pins the numbering against an accidental
    /// change rather than making a new promise.
    #[test]
    fn connected_component_ids_are_the_smallest_member() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..5).map(|_| g.add_node("N", &()).unwrap()).collect();
        // Two components: {0, 1, 2} joined in reverse so unioning cannot rely on
        // arriving in ascending order, and {3, 4}.
        g.add_edge(nodes[2], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[0], "E", &()).unwrap();
        g.add_edge(nodes[4], nodes[3], "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let components = g.connected_components().unwrap();
        for node in &nodes[0..3] {
            assert_eq!(components[node], nodes[0], "keyed by its least member");
        }
        for node in &nodes[3..5] {
            assert_eq!(components[node], nodes[3]);
        }
    }

    /// The id has to be a real node id, not the dense index that happens to equal it
    /// on a graph with no deletions. Deleting the lowest node shifts every later
    /// node's rank down by one, so an index-valued id would name a component after a
    /// node that is gone.
    #[test]
    fn connected_component_ids_survive_a_node_deletion() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[1], nodes[2], "E", &()).unwrap();
        g.add_edge(nodes[2], nodes[3], "E", &()).unwrap();
        g.delete_node(nodes[0]).unwrap();
        g.rebuild_csr().unwrap();

        let components = g.connected_components().unwrap();
        assert_eq!(components.len(), 3, "only the deleted node is gone");
        for node in &nodes[1..4] {
            assert_eq!(
                components[node], nodes[1],
                "the surviving component is keyed by node {}, not by dense index 0",
                nodes[1]
            );
        }
    }

    /// Parallel edges collapse for degree centrality, and a node joined to one
    /// neighbor in both directions scores two under `Both`. This is the boolean
    /// adjacency semantics of the SpMV formulation this replaced; a row length
    /// would report three and two respectively.
    #[test]
    fn degree_centrality_counts_distinct_neighbors() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &()).unwrap();
        g.add_edge(a, b, "E", &()).unwrap();
        g.add_edge(a, b, "OTHER", &()).unwrap();
        g.add_edge(b, a, "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let out = g.degree_centrality(DegreeDirection::Out).unwrap();
        assert_eq!(out[&a], 1, "three parallel edges are one distinct neighbor");
        let both = g.degree_centrality(DegreeDirection::Both).unwrap();
        assert_eq!(both[&a], 2, "one distinct neighbor in each direction");
    }

    /// A self-loop is an out-neighbor and an in-neighbor of the same node, so it
    /// counts once in each direction.
    #[test]
    fn degree_centrality_counts_a_self_loop_in_both_directions() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        g.add_edge(a, a, "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        assert_eq!(g.degree_centrality(DegreeDirection::Out).unwrap()[&a], 1);
        assert_eq!(g.degree_centrality(DegreeDirection::In).unwrap()[&a], 1);
        assert_eq!(g.degree_centrality(DegreeDirection::Both).unwrap()[&a], 2);
    }

    /// PageRank spreads a source's rank over its *edges*, so a second parallel
    /// edge sends more mass to its target than a single edge would. This is the
    /// `Plus` duplicate handling of the transition matrix this replaced, and it is
    /// the opposite of the distinct-neighbor rule degree centrality follows, so the
    /// two are pinned separately.
    #[test]
    fn page_rank_weights_parallel_edges_separately() {
        let (_dir, g) = open_tmp();
        let hub = g.add_node("N", &()).unwrap();
        let doubled = g.add_node("N", &()).unwrap();
        let single = g.add_node("N", &()).unwrap();
        g.add_edge(hub, doubled, "E", &()).unwrap();
        g.add_edge(hub, doubled, "E", &()).unwrap();
        g.add_edge(hub, single, "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let ranks = g.page_rank(1, 0.85).unwrap();
        assert!(
            ranks[&doubled] > ranks[&single],
            "two of the hub's three edges lead to `doubled`: {:?}",
            ranks
        );
    }

    /// Betweenness counts shortest *paths*, so a second edge between an already
    /// joined pair is not a second path and must leave every score alone. The
    /// formulation this replaced deduplicated by breaking after the first matching
    /// edge.
    ///
    /// The graph is a diamond, which is what makes the defect observable: two
    /// competing routes `a->b->d` and `a->c->d` split the dependency by the ratio of
    /// their path counts, so doubling `sigma[b]` shifts it from an even 0.5/0.5 to
    /// 2/3 against 1/3. On a single chain the same doubling cancels in the ratio, so
    /// a chain would pass either way.
    #[test]
    fn betweenness_ignores_parallel_edges() {
        // `build` wires the diamond, adding a second `a->b` edge when asked.
        let build = |parallel: bool| {
            let dir = TempDir::new().unwrap();
            let g = Graph::open(dir.path(), 1).unwrap();
            let nodes: Vec<NodeId> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
            g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
            if parallel {
                g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
            }
            g.add_edge(nodes[0], nodes[2], "E", &()).unwrap();
            g.add_edge(nodes[1], nodes[3], "E", &()).unwrap();
            g.add_edge(nodes[2], nodes[3], "E", &()).unwrap();
            g.rebuild_csr().unwrap();
            let scores = g.betweenness_centrality().unwrap();
            (dir, nodes.iter().map(|n| scores[n]).collect::<Vec<f64>>())
        };

        let (_simple_dir, simple) = build(false);
        assert_eq!(
            simple,
            vec![0.0, 0.5, 0.5, 0.0],
            "the two routes split the dependency evenly"
        );

        let (_multi_dir, multi) = build(true);
        assert_eq!(
            multi, simple,
            "a parallel edge is not a second shortest path"
        );
    }

    /// The parallel split must not change an answer. Forcing four workers over a
    /// graph far below the parallel threshold is the only way to exercise the
    /// split, since `kernel_threads` keeps a pass this small serial.
    #[test]
    fn parallel_analytics_agree_with_the_serial_pass() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..12).map(|_| g.add_node("N", &()).unwrap()).collect();
        for w in nodes.windows(2) {
            g.add_edge(w[0], w[1], "E", &()).unwrap();
        }
        g.add_edge(nodes[11], nodes[0], "E", &()).unwrap();
        g.add_edge(nodes[3], nodes[9], "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let serial = (
            g.page_rank(8, 0.85).unwrap(),
            g.harmonic_centrality().unwrap(),
            g.betweenness_centrality().unwrap(),
        );
        let parallel = crate::graph::algo::FORCE_KERNEL_THREADS.with(|f| {
            f.set(4);
            let out = (
                g.page_rank(8, 0.85).unwrap(),
                g.harmonic_centrality().unwrap(),
                g.betweenness_centrality().unwrap(),
            );
            f.set(0);
            out
        });

        assert_eq!(serial.0, parallel.0, "page rank");
        assert_eq!(serial.1, parallel.1, "harmonic centrality");
        // Betweenness sums per-worker partials, so the split can move the last
        // bits; the totals must still agree to well within any interpretation.
        for (node, value) in &serial.2 {
            assert!(
                (value - parallel.2[node]).abs() < 1e-9,
                "betweenness for {node}: {value} vs {}",
                parallel.2[node]
            );
        }
    }
}
