use super::*;

use super::traversal::{UNREACHED, expand_frontier};

/// Below this L2 norm the eigenvector iteration has collapsed and the next rescale
/// would divide by roughly zero, so the kernel reports the uniform distribution.
const EIGENVECTOR_MIN_NORM: f64 = 1e-10;

/// Hard cap on Louvain coarsening levels.
///
/// Each level strictly reduces the node count or the pass stops, so a graph cannot
/// need more than `log2(n)` of them and this is unreachable in practice. It exists so
/// that a defect in the merge condition cannot turn into a non-terminating query.
const LOUVAIN_MAX_LEVELS: usize = 32;

/// Holds one level of the Louvain hierarchy as an undirected weighted graph in CSR form.
///
/// Every edge appears in both endpoints' rows, so a scan over all rows visits each
/// edge twice. Self-loops are held apart from the rows so the neighbor scan never has
/// to test for them, and because modularity counts a self-loop twice in a node's
/// degree but never as a link to another community.
struct LouvainLevel {
    row_ptr: Vec<usize>,
    col: Vec<u32>,
    weight: Vec<f64>,
    self_loop: Vec<f64>,
    /// Sum of incident edge weights per node, with a self-loop counted twice.
    degree: Vec<f64>,
    /// `2m`, the sum of every node's degree. Constant across levels, since
    /// coarsening moves weight around without creating or destroying it.
    total: f64,
}

impl LouvainLevel {
    fn len(&self) -> usize {
        self.degree.len()
    }

    fn neighbors(&self, u: usize) -> impl Iterator<Item = (u32, f64)> + '_ {
        (self.row_ptr[u]..self.row_ptr[u + 1]).map(|k| (self.col[k], self.weight[k]))
    }

    /// Project the directed multigraph in `snap` onto an undirected weighted graph.
    ///
    /// The weight between two nodes is the number of edges joining them in either
    /// direction, so parallel edges strengthen a connection rather than collapsing.
    /// That is the reading modularity wants, and unlike the clustering coefficient
    /// there is no bound for it to violate: a pair joined by five edges genuinely is
    /// more strongly tied than a pair joined by one.
    ///
    /// A node's out-row and in-row together list every edge incident to it exactly
    /// once, except a self-loop, which appears in both. Self-loops are therefore
    /// counted from the out-row alone.
    fn from_snapshot(snap: &CsrSnapshot) -> Self {
        let n = snap.dense_to_id.len();
        let mut row_ptr = Vec::with_capacity(n + 1);
        let mut col: Vec<u32> = Vec::new();
        let mut weight: Vec<f64> = Vec::new();
        let mut self_loop = vec![0.0f64; n];
        let mut degree = vec![0.0f64; n];

        // `stamp[v] == u + 1` marks v as already collected for u, and `acc[v]` holds
        // the running multiplicity, so each row is built in one pass without a map.
        let mut stamp = vec![0usize; n];
        let mut acc = vec![0.0f64; n];
        let mut seen: Vec<u32> = Vec::new();

        row_ptr.push(0);
        for u in 0..n {
            seen.clear();
            for k in snap.row_ptr[u]..snap.row_ptr[u + 1] {
                let v = snap.col_idx[k];
                if v as usize == u {
                    self_loop[u] += 1.0;
                    continue;
                }
                if stamp[v as usize] != u + 1 {
                    stamp[v as usize] = u + 1;
                    acc[v as usize] = 0.0;
                    seen.push(v);
                }
                acc[v as usize] += 1.0;
            }
            for k in snap.in_row_ptr[u]..snap.in_row_ptr[u + 1] {
                let v = snap.in_col_idx[k];
                if v as usize == u {
                    // Already counted from the out-row.
                    continue;
                }
                if stamp[v as usize] != u + 1 {
                    stamp[v as usize] = u + 1;
                    acc[v as usize] = 0.0;
                    seen.push(v);
                }
                acc[v as usize] += 1.0;
            }

            let mut incident = 0.0f64;
            // Sorted so a row's order depends on the graph and not on scan order,
            // which keeps the floating-point accumulation below reproducible.
            seen.sort_unstable();
            for &v in &seen {
                col.push(v);
                weight.push(acc[v as usize]);
                incident += acc[v as usize];
            }
            degree[u] = incident + 2.0 * self_loop[u];
            row_ptr.push(col.len());
        }

        let total = degree.iter().sum();
        Self {
            row_ptr,
            col,
            weight,
            self_loop,
            degree,
            total,
        }
    }

    /// Runs Louvain's first phase, repeatedly moving each node into the neighboring
    /// community that most increases modularity, until a full sweep moves nobody.
    ///
    /// Returns the community index of every node, not yet renumbered.
    ///
    /// The move gain for node `i` joining community `c` is
    /// `w(i, c) - sigma_tot(c) * k_i / 2m`, dropping the terms that are equal for
    /// every candidate. `i` is removed from its own community first, so staying put
    /// is evaluated on the same footing as leaving.
    ///
    /// This phase is deliberately serial. The outcome depends on the order nodes are
    /// visited, so parallelizing it would make the result depend on the worker count,
    /// which every other kernel here avoids. Ascending dense order is ascending node
    /// id, so the partition is reproducible run to run.
    fn local_moving(&self) -> Vec<u32> {
        let n = self.len();
        let mut community: Vec<u32> = (0..n as u32).collect();
        let mut sigma_tot = self.degree.clone();
        if self.total <= 0.0 {
            return community;
        }

        // `stamp`/`link` accumulate the weight from the current node into each
        // neighboring community without clearing an n-sized buffer per node.
        let mut stamp = vec![usize::MAX; n];
        let mut link = vec![0.0f64; n];
        let mut candidates: Vec<u32> = Vec::new();

        for _ in 0..LOUVAIN_MAX_LEVELS {
            let mut moved = false;
            for u in 0..n {
                let origin = community[u];
                let k_u = self.degree[u];

                candidates.clear();
                for (v, w) in self.neighbors(u) {
                    let c = community[v as usize] as usize;
                    if stamp[c] != u {
                        stamp[c] = u;
                        link[c] = 0.0;
                        candidates.push(c as u32);
                    }
                    link[c] += w;
                }

                // Detach `u` before scoring, so its own degree does not appear in the
                // penalty term of the community it is sitting in.
                sigma_tot[origin as usize] -= k_u;

                let gain_of = |c: u32| -> f64 {
                    let to_c = if stamp[c as usize] == u {
                        link[c as usize]
                    } else {
                        0.0
                    };
                    to_c - sigma_tot[c as usize] * k_u / self.total
                };

                let mut best = origin;
                let mut best_gain = gain_of(origin);
                for &c in &candidates {
                    let gain = gain_of(c);
                    // The tie-break on the smaller index is what makes the result
                    // independent of the order candidates were discovered in, which
                    // follows CSR row order rather than anything meaningful.
                    if gain > best_gain || (gain == best_gain && c < best) {
                        best = c;
                        best_gain = gain;
                    }
                }

                sigma_tot[best as usize] += k_u;
                community[u] = best;
                if best != origin {
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }

        community
    }

    /// Runs Louvain's second phase, contracting each community into a single node.
    ///
    /// Communities are renumbered by the ascending dense index of their smallest
    /// member, so the coarse graph's node order is a deterministic function of the
    /// fine one. Returns the coarse level and the renumbered assignment.
    fn coarsen(&self, community: &[u32]) -> (LouvainLevel, Vec<u32>) {
        let n = self.len();
        let mut renumbered = vec![u32::MAX; n];
        let mut next_id = 0u32;
        let mut assignment = vec![0u32; n];
        for u in 0..n {
            let c = community[u] as usize;
            if renumbered[c] == u32::MAX {
                renumbered[c] = next_id;
                next_id += 1;
            }
            assignment[u] = renumbered[c];
        }

        let groups = next_id as usize;
        let mut self_loop = vec![0.0f64; groups];
        let mut degree = vec![0.0f64; groups];
        // Degree is carried over rather than recomputed. A community's incident weight
        // is exactly the sum of its members' degrees, and summing them avoids any
        // chance of the coarse graph disagreeing with the fine one about `2m`.
        for (u, &group) in assignment.iter().enumerate() {
            let c = group as usize;
            degree[c] += self.degree[u];
            self_loop[c] += self.self_loop[u];
        }

        // Group members by community up front. Rescanning every node once per
        // community instead would make coarsening quadratic, which on a graph that
        // resolves into many small communities is the whole cost of the algorithm.
        let mut member_ptr = vec![0usize; groups + 1];
        for &c in &assignment {
            member_ptr[c as usize + 1] += 1;
        }
        for c in 0..groups {
            member_ptr[c + 1] += member_ptr[c];
        }
        let mut members = vec![0u32; n];
        let mut cursor = member_ptr.clone();
        for (u, &c) in assignment.iter().enumerate() {
            members[cursor[c as usize]] = u as u32;
            cursor[c as usize] += 1;
        }

        let mut stamp = vec![usize::MAX; groups];
        let mut acc = vec![0.0f64; groups];
        let mut seen: Vec<u32> = Vec::new();
        let mut row_ptr = Vec::with_capacity(groups + 1);
        let mut col: Vec<u32> = Vec::new();
        let mut weight: Vec<f64> = Vec::new();

        row_ptr.push(0);
        for c in 0..groups {
            seen.clear();
            for &u in &members[member_ptr[c]..member_ptr[c + 1]] {
                for (v, w) in self.neighbors(u as usize) {
                    let d = assignment[v as usize] as usize;
                    if d == c {
                        // Each intra-community edge is walked from both endpoints, so
                        // halving turns the doubled total into the self-loop weight.
                        self_loop[c] += w / 2.0;
                        continue;
                    }
                    if stamp[d] != c {
                        stamp[d] = c;
                        acc[d] = 0.0;
                        seen.push(d as u32);
                    }
                    acc[d] += w;
                }
            }
            seen.sort_unstable();
            for &d in &seen {
                col.push(d);
                weight.push(acc[d as usize]);
            }
            row_ptr.push(col.len());
        }

        let total = self.total;
        (
            LouvainLevel {
                row_ptr,
                col,
                weight,
                self_loop,
                degree,
                total,
            },
            assignment,
        )
    }
}

/// Every neighbor of `u` in both directions, parallel edges and self-loops included.
///
/// The caller dedups, because the two rows overlap whenever a pair is joined in both
/// directions and either row alone can hold a pair twice.
fn undirected_neighbors(snap: &CsrSnapshot, u: usize) -> impl Iterator<Item = u32> + '_ {
    snap.col_idx[snap.row_ptr[u]..snap.row_ptr[u + 1]]
        .iter()
        .chain(snap.in_col_idx[snap.in_row_ptr[u]..snap.in_row_ptr[u + 1]].iter())
        .copied()
}

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
    /// Iterative, for the reason given on [`Graph::detect_cycle_kernel`]. Tarjan is a
    /// depth-first search, so recursion put one call frame per node on the current path
    /// and a long chain aborted the process instead of returning.
    ///
    /// The translation keeps the recursive algorithm exactly, including the order
    /// components are emitted, because that order is what assigns the component ids: a
    /// component is emitted when its root finishes, so ids still ascend in
    /// root-completion order. What was the recursion's implicit per-frame state is now
    /// explicit (the node, and how far through its row the search has gone) and the
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

    /// Sums, for each node, the reciprocals of its shortest-path distances to every
    /// node it can reach.
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

    /// Counts the *distinct* neighbors of each node in the requested direction.
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

    /// Closeness centrality in the Wasserman-Faust form: for each node,
    /// `(reachable / total_distance) * (reachable / (n - 1))`, where `reachable` is
    /// the number of other nodes it can reach and `total_distance` is the sum of the
    /// hop distances to them.
    ///
    /// Distance is hop count over outgoing edges rather than a weighted path length,
    /// the same convention [`Graph::harmonic_centrality_kernel`] uses, whose
    /// per-source breadth-first pass this shares. The two differ only in what they
    /// accumulate, since harmonic sums `1 / hop` and so needs no reachability correction,
    /// while closeness sums the distances and does.
    ///
    /// The Wasserman-Faust factor is what makes the score usable on a disconnected
    /// graph. Plain `reachable / total_distance` is a reciprocal mean distance, so a
    /// node in a two-node component scores the maximum while a well-connected node in
    /// a large component scores less; scaling by the fraction of the graph it reaches
    /// removes that inversion. On a connected graph the factor is 1 and the score
    /// reduces to `(n - 1) / total_distance`. A node that reaches nothing scores zero.
    ///
    /// Parallel edges cannot change the result, since a second edge between the same
    /// pair does not shorten a hop distance.
    pub(in crate::graph) fn closeness_centrality_kernel(
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
                let mut total_distance = 0.0f64;
                let mut reachable = 0usize;
                for hop in 1.. {
                    expand_frontier(snap, &frontier, hop, &mut levels, &mut next);
                    if next.is_empty() {
                        break;
                    }
                    total_distance += next.len() as f64 * f64::from(hop);
                    reachable += next.len();
                    std::mem::swap(&mut frontier, &mut next);
                }
                *value = if reachable > 0 && n > 1 {
                    (reachable as f64 / total_distance) * (reachable as f64 / (n as f64 - 1.0))
                } else {
                    0.0
                };
            }
        });

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, centrality[d]))
            .collect())
    }

    /// Eigenvector centrality by power iteration over the CSR snapshot.
    ///
    /// Each iteration computes `x[j] = sum over edges i -> j of x[i]` and rescales to
    /// unit L2 norm, so a node is important when the nodes pointing at it are. Like
    /// [`Graph::page_rank`] the accumulation reads the *incoming* rows, which is what
    /// keeps the pass parallel over disjoint output chunks and independent of the
    /// worker count.
    ///
    /// Parallel edges each contribute, matching PageRank rather than the
    /// distinct-neighbor rule of [`Graph::degree_centrality`]. Two edges from `i` to
    /// `j` pass `i`'s score to `j` twice, which is the multigraph reading of "how much
    /// influence flows along this connection".
    ///
    /// Iteration is bounded and does not fail. It stops early once the L2 change falls
    /// below `tolerance`, and otherwise returns the estimate after `iterations`
    /// rounds. That follows `page_rank`, which also runs a fixed budget, and it is the
    /// right choice for a database procedure: refusing to answer because a graph
    /// converges slowly is worse than answering approximately and saying so. A
    /// degenerate operator, meaning no edges at all or a vector that collapses to
    /// zero, yields the uniform `1 / n` distribution rather than a division by zero.
    ///
    /// Scores are reported as magnitudes scaled to sum to `n`. An eigenvector's sign
    /// and length are arbitrary, so only the ratios between nodes carry meaning.
    pub(in crate::graph) fn eigenvector_centrality_kernel(
        &self,
        snap: &CsrSnapshot,
        iterations: u32,
        tolerance: f64,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let uniform = |value: f64| -> HashMap<NodeId, f64> {
            snap.dense_to_id.iter().map(|&id| (id, value)).collect()
        };
        if snap.col_idx.is_empty() {
            return Ok(uniform(1.0 / n as f64));
        }

        let threads = self.kernel_threads(n.saturating_add(snap.col_idx.len()));
        let mut x = vec![1.0 / (n as f64).sqrt(); n];
        let mut next = vec![0.0f64; n];

        for _ in 0..iterations {
            {
                let previous = &x;
                fill_dense_range(&mut next, threads, move |lo, slice| {
                    for (offset, value) in slice.iter_mut().enumerate() {
                        let j = lo + offset;
                        let mut sum = 0.0f64;
                        for k in snap.in_row_ptr[j]..snap.in_row_ptr[j + 1] {
                            sum += previous[snap.in_col_idx[k] as usize];
                        }
                        *value = sum;
                    }
                });
            }

            let norm = next.iter().map(|v| v * v).sum::<f64>().sqrt();
            if norm < EIGENVECTOR_MIN_NORM {
                return Ok(uniform(1.0 / n as f64));
            }
            let mut delta = 0.0f64;
            for (current, raw) in x.iter_mut().zip(next.iter()) {
                let normalized = raw / norm;
                let step = normalized - *current;
                delta += step * step;
                *current = normalized;
            }
            if delta.sqrt() < tolerance {
                break;
            }
        }

        // The orientation of an eigenvector is arbitrary, so report magnitudes, and
        // rescale to sum to `n` so the numbers do not shrink as the graph grows.
        let total: f64 = x.iter().map(|v| v.abs()).sum();
        if total > 0.0 {
            for value in x.iter_mut() {
                *value = value.abs() * n as f64 / total;
            }
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, x[d]))
            .collect())
    }

    /// Katz centrality by the fixed-point iteration `x = alpha * A^T x + beta`.
    ///
    /// A node's score is a sum over all walks that reach it, with a walk of length `k`
    /// attenuated by `alpha^k`, plus the constant `beta` every node receives for
    /// existing. Compared with eigenvector centrality this gives a node with no
    /// incoming edges a non-zero score, which is why it behaves better on the directed
    /// acyclic shapes where eigenvector centrality collapses.
    ///
    /// `alpha` must be below the reciprocal of the largest eigenvalue for the series
    /// to converge, and that bound is a property of the data rather than something
    /// this method can check cheaply. A value above it diverges; the bounded iteration
    /// then returns a large but finite estimate instead of looping, and the scores are
    /// meaningless. A safe default is well under `1 / max_degree`.
    ///
    /// Parallel edges each contribute, matching [`Graph::page_rank`] and eigenvector
    /// centrality, since every edge is a distinct walk. Iteration is bounded and does
    /// not fail, on the same reasoning as eigenvector centrality.
    pub(in crate::graph) fn katz_centrality_kernel(
        &self,
        snap: &CsrSnapshot,
        alpha: f64,
        beta: f64,
        iterations: u32,
        tolerance: f64,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let threads = self.kernel_threads(n.saturating_add(snap.col_idx.len()));
        let mut x = vec![beta; n];
        let mut next = vec![0.0f64; n];

        for _ in 0..iterations {
            {
                let previous = &x;
                fill_dense_range(&mut next, threads, move |lo, slice| {
                    for (offset, value) in slice.iter_mut().enumerate() {
                        let j = lo + offset;
                        let mut sum = 0.0f64;
                        for k in snap.in_row_ptr[j]..snap.in_row_ptr[j + 1] {
                            sum += previous[snap.in_col_idx[k] as usize];
                        }
                        *value = alpha * sum + beta;
                    }
                });
            }

            let mut delta = 0.0f64;
            for (current, raw) in x.iter_mut().zip(next.iter()) {
                let step = raw - *current;
                delta += step * step;
                *current = *raw;
            }
            if delta.sqrt() < tolerance {
                break;
            }
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, x[d]))
            .collect())
    }

    /// Score how likely two nodes are to become connected, by one of the classic
    /// neighborhood heuristics.
    ///
    /// The neighborhood is undirected and distinct, matching
    /// [`Graph::clustering_coefficient`]: a pair joined by three edges is one
    /// neighbor, direction is ignored, and a node is never its own neighbor. Every
    /// metric here is a statement about *who* two nodes both know, so multiplicity
    /// would double-count one relationship as evidence of several.
    ///
    /// A node absent from the snapshot scores zero rather than erroring, which is the
    /// same choice [`Graph::typed_neighbor_counts`] makes: these are per-row scoring
    /// functions, and failing a whole query because one row names a node that has
    /// since been deleted is worse than scoring it zero.
    ///
    /// Cost is the two nodes' degrees, plus the degrees of their shared neighbors for
    /// the two weighted metrics.
    pub(in crate::graph) fn link_prediction_kernel(
        &self,
        snap: &CsrSnapshot,
        a: NodeId,
        b: NodeId,
        metric: LinkPredictionMetric,
    ) -> f64 {
        let (Some(&da), Some(&db)) = (snap.id_to_dense.get(&a), snap.id_to_dense.get(&b)) else {
            return 0.0;
        };

        let neighborhood = |u: usize| -> Vec<u32> {
            let mut set: Vec<u32> = undirected_neighbors(snap, u)
                .filter(|&v| v as usize != u)
                .collect();
            set.sort_unstable();
            set.dedup();
            set
        };
        let na = neighborhood(da as usize);
        let nb = neighborhood(db as usize);

        if metric == LinkPredictionMetric::PreferentialAttachment {
            return na.len() as f64 * nb.len() as f64;
        }

        // Both sides are sorted, so the intersection is a merge rather than a hash
        // probe per element.
        let mut shared: Vec<u32> = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < na.len() && j < nb.len() {
            match na[i].cmp(&nb[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    shared.push(na[i]);
                    i += 1;
                    j += 1;
                }
            }
        }

        let degree_of = |w: u32| -> usize {
            let mut set: Vec<u32> = undirected_neighbors(snap, w as usize)
                .filter(|&v| v != w)
                .collect();
            set.sort_unstable();
            set.dedup();
            set.len()
        };

        match metric {
            LinkPredictionMetric::CommonNeighbors => shared.len() as f64,
            LinkPredictionMetric::Jaccard => {
                let union = na.len() + nb.len() - shared.len();
                if union == 0 {
                    0.0
                } else {
                    shared.len() as f64 / union as f64
                }
            }
            // Both sums fold from `0.0` rather than using `Iterator::sum`, whose
            // identity for a float is `-0.0`. That identity is deliberate in the
            // standard library, since `-0.0 + x` preserves the sign of `x`, but it
            // surfaces here: a pair with no common neighbor has an empty
            // intersection, so the sum is the identity itself and the score reaches
            // a caller as `-0.0` while every other metric on the same row reads `0.0`.
            LinkPredictionMetric::AdamicAdar => shared
                .iter()
                .filter_map(|&w| {
                    let degree = degree_of(w);
                    // `ln(1)` is zero, so a neighbor of degree one has no defined
                    // weight; contributing nothing is the conventional reading.
                    (degree > 1).then(|| 1.0 / (degree as f64).ln())
                })
                .fold(0.0, |acc, term| acc + term),
            LinkPredictionMetric::ResourceAllocation => shared
                .iter()
                .map(|&w| {
                    let degree = degree_of(w);
                    if degree == 0 {
                        0.0
                    } else {
                        1.0 / degree as f64
                    }
                })
                .fold(0.0, |acc, term| acc + term),
            // Handled above, before the intersection is built.
            LinkPredictionMetric::PreferentialAttachment => unreachable!(),
        }
    }

    /// Community detection by the Louvain method.
    ///
    /// Two phases alternate until neither changes anything. The first moves each node
    /// into whichever neighboring community most increases modularity; the second
    /// contracts every community into one node and repeats on the smaller graph, which
    /// is what lets the method find communities larger than a single neighborhood.
    ///
    /// The graph is read as undirected and weighted by edge multiplicity, so a pair
    /// joined by three edges is three times as strongly tied as a pair joined by one.
    /// That is the opposite of the distinct-neighbor rule
    /// [`Graph::clustering_coefficient`] needs, and it is right for the same underlying
    /// reason: modularity compares observed against expected edge weight, so weight is
    /// the quantity it is defined over, and there is no bound for multiplicity to
    /// break. Self-loops contribute to a node's degree, as modularity requires, but
    /// never pull it toward another community.
    ///
    /// The community id is the smallest *node id* in the community, matching
    /// [`Graph::connected_components`]. Only the induced partition is contractual, so
    /// compare membership rather than depending on the numbering.
    ///
    /// Unlike the other analytics passes this one is serial. Local moving is
    /// order-dependent by construction, so splitting it over workers would make the
    /// partition depend on the worker count; visiting nodes in ascending id order
    /// instead makes the result reproducible.
    pub(in crate::graph) fn louvain_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mut level = LouvainLevel::from_snapshot(snap);
        // Maps every original node to its community in the current level.
        let mut membership: Vec<u32> = (0..n as u32).collect();

        for _ in 0..LOUVAIN_MAX_LEVELS {
            let community = level.local_moving();
            let (coarse, assignment) = level.coarsen(&community);
            // Nothing merged, so further levels would repeat this one unchanged.
            if coarse.len() == level.len() {
                break;
            }
            for slot in membership.iter_mut() {
                *slot = assignment[*slot as usize];
            }
            level = coarse;
            if level.len() == 1 {
                break;
            }
        }

        // Name each community after the smallest node id it contains. Dense indices
        // ascend with node id, so the first member encountered is the smallest.
        let mut label = vec![None; level.len()];
        for (dense, &group) in membership.iter().enumerate() {
            label[group as usize].get_or_insert(snap.dense_to_id[dense]);
        }

        Ok(membership
            .iter()
            .enumerate()
            .map(|(dense, &group)| {
                let id = snap.dense_to_id[dense];
                (id, label[group as usize].unwrap_or(id))
            })
            .collect())
    }

    /// Local clustering coefficient: for each node, the fraction of its neighbor pairs
    /// that are themselves connected.
    ///
    /// The graph is read as undirected here, so a node's neighborhood is the union of
    /// its out- and in-neighbors, and a neighbor pair counts as connected when an edge
    /// runs between them in either direction. Directed clustering has several
    /// competing definitions and no default worth guessing at; the undirected
    /// coefficient is the one every other tool means by the name.
    ///
    /// Neighbors are *distinct*, following [`Graph::degree_centrality`] rather than
    /// PageRank. That is not a preference: the coefficient is a ratio bounded by 1, and
    /// counting a parallel edge twice inflates the numerator past its denominator and
    /// produces scores above 1. Self-loops are excluded for the same reason.
    ///
    /// A node with fewer than two distinct neighbors scores zero, since it has no pair
    /// that could be connected.
    ///
    /// Cost is the sum over nodes of the degrees of their neighbors, so a hub makes
    /// this expensive in a way the linear passes are not.
    pub(in crate::graph) fn clustering_coefficient_kernel(
        &self,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let per_node = snap.col_idx.len().saturating_mul(2) / n.max(1);
        let threads = self.parallel_threads(n.saturating_mul(per_node.max(1)));
        let coefficients = map_dense_range(n, threads, |lo, slice| {
            // `member[v] == token` marks v as a neighbor of the node being scored, and
            // `seen[v] == token` dedups one neighbor's own adjacency. Stamping avoids
            // clearing an n-sized buffer per node; both counters only ever increase,
            // so a stale stamp can never be mistaken for a current one.
            let mut member = vec![0u64; n];
            let mut seen = vec![0u64; n];
            let mut node_token = 0u64;
            let mut pair_token = 0u64;
            let mut neighbors: Vec<u32> = Vec::new();

            for (offset, value) in slice.iter_mut().enumerate() {
                let u = lo + offset;
                node_token += 1;
                neighbors.clear();
                for v in undirected_neighbors(snap, u) {
                    if v as usize != u && member[v as usize] != node_token {
                        member[v as usize] = node_token;
                        neighbors.push(v);
                    }
                }

                let k = neighbors.len();
                if k < 2 {
                    *value = 0.0;
                    continue;
                }

                // Each connected pair inside the neighborhood is counted once from
                // each of its two endpoints, so this ordered total is exactly twice
                // the number of pairs. That cancels the 2 in the usual
                // `2 * pairs / (k * (k - 1))`, leaving the plain ratio below.
                let mut ordered_links = 0u64;
                for &a in &neighbors {
                    pair_token += 1;
                    for b in undirected_neighbors(snap, a as usize) {
                        let bi = b as usize;
                        if b != a && member[bi] == node_token && seen[bi] != pair_token {
                            seen[bi] = pair_token;
                            ordered_links += 1;
                        }
                    }
                }

                *value = ordered_links as f64 / (k as f64 * (k as f64 - 1.0));
            }
        });

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, coefficients[d]))
            .collect())
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

    /// A pair with no common neighbor scores a positive zero, like every other
    /// metric on the same pair.
    ///
    /// `Iterator::sum` folds a float from `-0.0`, so the empty intersection used to
    /// hand back a negative zero: two nodes with nothing in common reported
    /// `-0.0` for Adamic-Adar and resource allocation while Jaccard and common
    /// neighbors on the same pair reported `0.0`, which reads as a defect to anyone
    /// looking at the row.
    #[test]
    fn a_pair_with_nothing_in_common_scores_positive_zero() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        // One neighbor each, and not shared, so the intersection is empty while the
        // neighborhoods are not.
        let na = g.add_node("N", &()).unwrap();
        let nb = g.add_node("N", &()).unwrap();
        g.add_edge(a, na, "E", &()).unwrap();
        g.add_edge(b, nb, "E", &()).unwrap();

        for metric in [
            LinkPredictionMetric::AdamicAdar,
            LinkPredictionMetric::ResourceAllocation,
            LinkPredictionMetric::Jaccard,
            LinkPredictionMetric::CommonNeighbors,
        ] {
            let score = g.link_prediction_score(a, b, metric).unwrap();
            assert_eq!(score, 0.0, "{metric:?} must be zero");
            assert!(
                score.is_sign_positive(),
                "{metric:?} returned a negative zero",
            );
        }
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

    /// On the directed path a -> b -> c the Wasserman-Faust score is computable by
    /// hand: a reaches two nodes at total distance 3 for `(2/3) * (2/2)`, b reaches
    /// one at distance 1 for `(1/1) * (1/2)`, and c reaches nothing.
    ///
    /// The middle value is the one that pins the reachability factor. Without it b
    /// would score 1.0, beating a, purely because its single reachable node is
    /// adjacent.
    #[test]
    fn closeness_scales_by_the_fraction_of_the_graph_reached() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[2], "E", &()).unwrap();

        let scores = g.closeness_centrality().unwrap();
        assert!((scores[&nodes[0]] - 2.0 / 3.0).abs() < 1e-12);
        assert!((scores[&nodes[1]] - 0.5).abs() < 1e-12);
        assert_eq!(scores[&nodes[2]], 0.0);
    }

    /// A second edge between the same pair cannot shorten a hop, so it must not move
    /// a closeness score.
    #[test]
    fn closeness_ignores_parallel_edges() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[2], "E", &()).unwrap();
        let before = g.closeness_centrality().unwrap();

        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        assert_eq!(g.closeness_centrality().unwrap(), before);
    }

    /// Every node of a directed cycle is structurally identical, so eigenvector
    /// centrality must give them equal scores, and the sum-to-n normalization makes
    /// each exactly 1.
    #[test]
    fn eigenvector_is_uniform_on_a_directed_cycle() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
        for i in 0..4 {
            g.add_edge(nodes[i], nodes[(i + 1) % 4], "E", &()).unwrap();
        }

        let scores = g.eigenvector_centrality(100, 1e-10).unwrap();
        for node in &nodes {
            assert!((scores[node] - 1.0).abs() < 1e-9, "{}", scores[node]);
        }
    }

    /// With no edges the operator is degenerate and the kernel reports the uniform
    /// distribution rather than dividing by a zero norm.
    #[test]
    fn eigenvector_on_an_edgeless_graph_is_uniform() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();

        let scores = g.eigenvector_centrality(100, 1e-10).unwrap();
        for node in &nodes {
            assert!((scores[node] - 1.0 / 3.0).abs() < 1e-12);
        }
    }

    /// Unlike eigenvector centrality, Katz gives a source with no incoming edges a
    /// non-zero score, because every node collects `beta` for existing. On the path
    /// a -> b -> c with alpha 0.1 and beta 1 the fixed point is a = 1, b = 1.1, and
    /// c = 1.11.
    #[test]
    fn katz_gives_every_node_the_beta_floor() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();
        g.add_edge(nodes[1], nodes[2], "E", &()).unwrap();

        let scores = g.katz_centrality(0.1, 1.0, 200, 1e-12).unwrap();
        assert!((scores[&nodes[0]] - 1.0).abs() < 1e-9);
        assert!((scores[&nodes[1]] - 1.1).abs() < 1e-9);
        assert!((scores[&nodes[2]] - 1.11).abs() < 1e-9);
    }

    /// In a triangle every node's two neighbors are joined, so the coefficient is 1;
    /// in a star the center's neighbors are joined to nothing, so it is 0. Direction
    /// must not matter, which the triangle's one-way edges check.
    #[test]
    fn clustering_coefficient_reads_the_graph_as_undirected() {
        let (_dir, g) = open_tmp();
        let tri: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(tri[0], tri[1], "E", &()).unwrap();
        g.add_edge(tri[1], tri[2], "E", &()).unwrap();
        g.add_edge(tri[2], tri[0], "E", &()).unwrap();

        let scores = g.clustering_coefficient().unwrap();
        for node in &tri {
            assert!((scores[node] - 1.0).abs() < 1e-12, "{}", scores[node]);
        }

        let (_dir2, star) = open_tmp();
        let hub = star.add_node("N", &()).unwrap();
        let spokes: Vec<NodeId> = (0..3).map(|_| star.add_node("N", &()).unwrap()).collect();
        for spoke in &spokes {
            star.add_edge(hub, *spoke, "E", &()).unwrap();
        }
        assert_eq!(star.clustering_coefficient().unwrap()[&hub], 0.0);
    }

    /// The coefficient is a ratio bounded by 1, so the neighborhood must be counted
    /// over *distinct* neighbors. Counting a parallel edge twice inflates the
    /// numerator past the denominator and yields a score above 1, which is the whole
    /// reason this kernel does not follow PageRank's parallel-edge rule.
    #[test]
    fn clustering_coefficient_stays_bounded_under_parallel_edges() {
        let (_dir, g) = open_tmp();
        let tri: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(tri[0], tri[1], "E", &()).unwrap();
        g.add_edge(tri[1], tri[2], "E", &()).unwrap();
        g.add_edge(tri[2], tri[0], "E", &()).unwrap();
        // Duplicate every edge, and add a reversed one, so each pair is multiply and
        // bidirectionally connected.
        g.add_edge(tri[0], tri[1], "E", &()).unwrap();
        g.add_edge(tri[1], tri[0], "E", &()).unwrap();
        g.add_edge(tri[1], tri[2], "E", &()).unwrap();

        for (node, score) in g.clustering_coefficient().unwrap() {
            assert!((score - 1.0).abs() < 1e-12, "node {node} scored {score}");
        }
    }

    /// Three cliques joined by one edge each is the standard shape Louvain must get
    /// right and label propagation often does not: the bridges are too weak to justify
    /// merging, so the partition has to be the three cliques.
    #[test]
    fn louvain_separates_cliques_joined_by_single_edges() {
        let (_dir, g) = open_tmp();
        let groups: Vec<Vec<NodeId>> = (0..3)
            .map(|_| (0..5).map(|_| g.add_node("N", &()).unwrap()).collect())
            .collect();
        for group in &groups {
            add_clique(&g, group);
        }
        g.add_edge(groups[0][0], groups[1][0], "E", &()).unwrap();
        g.add_edge(groups[1][0], groups[2][0], "E", &()).unwrap();

        let mut expected: Vec<Vec<NodeId>> = groups
            .iter()
            .map(|group| {
                let mut sorted = group.clone();
                sorted.sort_unstable();
                sorted
            })
            .collect();
        expected.sort();
        assert_eq!(partition(&g.louvain().unwrap()), expected);
    }

    /// Nodes in different weakly connected components can never be in one community,
    /// since no sequence of moves could ever increase modularity by joining them.
    /// This is the invariant that holds on every graph, so it is the one to check on
    /// a shape with no obvious hand-computable answer.
    #[test]
    fn louvain_never_merges_disconnected_components() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..12).map(|_| g.add_node("N", &()).unwrap()).collect();
        // Two components with an irregular internal shape.
        for (a, b) in [(0, 1), (1, 2), (2, 3), (3, 0), (0, 2), (4, 5)] {
            g.add_edge(nodes[a], nodes[b], "E", &()).unwrap();
        }
        for (a, b) in [(6, 7), (7, 8), (8, 9), (9, 10), (10, 11), (11, 6)] {
            g.add_edge(nodes[a], nodes[b], "E", &()).unwrap();
        }

        let communities = g.louvain().unwrap();
        let components = g.connected_components().unwrap();
        for (a, b) in communities.keys().flat_map(|&a| {
            communities
                .keys()
                .filter(move |&&b| b > a)
                .map(move |&b| (a, b))
        }) {
            if communities[&a] == communities[&b] {
                assert_eq!(
                    components[&a], components[&b],
                    "{a} and {b} share a community across components"
                );
            }
        }
    }

    /// The community id is the smallest node id in the community, matching the
    /// connected-components convention, and every node must be assigned exactly once.
    #[test]
    fn louvain_names_a_community_after_its_smallest_member() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..6).map(|_| g.add_node("N", &()).unwrap()).collect();
        add_clique(&g, &nodes[..3]);
        add_clique(&g, &nodes[3..]);

        let communities = g.louvain().unwrap();
        assert_eq!(communities.len(), nodes.len());
        for part in partition(&communities) {
            let smallest = *part.iter().min().unwrap();
            for node in &part {
                assert_eq!(communities[node], smallest);
            }
        }
    }

    /// The partition must not depend on how many times the pass is run, which is the
    /// determinism the serial local-moving phase exists to provide.
    #[test]
    fn louvain_is_reproducible() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..10).map(|_| g.add_node("N", &()).unwrap()).collect();
        add_clique(&g, &nodes[..4]);
        add_clique(&g, &nodes[4..]);
        g.add_edge(nodes[0], nodes[4], "E", &()).unwrap();

        let first = g.louvain().unwrap();
        for _ in 0..4 {
            assert_eq!(g.louvain().unwrap(), first);
        }
    }

    /// An edgeless graph has no moves available, so every node stays alone.
    #[test]
    fn louvain_leaves_isolated_nodes_alone() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();

        let communities = g.louvain().unwrap();
        for node in &nodes {
            assert_eq!(communities[node], *node);
        }
    }

    /// A self-loop is not a neighbor pair and must not enter the neighborhood, or a
    /// node with one real neighbor plus a loop would look like it had two.
    #[test]
    fn clustering_coefficient_excludes_self_loops() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..2).map(|_| g.add_node("N", &()).unwrap()).collect();
        g.add_edge(nodes[0], nodes[0], "E", &()).unwrap();
        g.add_edge(nodes[0], nodes[1], "E", &()).unwrap();

        assert_eq!(g.clustering_coefficient().unwrap()[&nodes[0]], 0.0);
    }
}
