use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Graph algorithms
    // ------------------------------------------------------------------

    /// Depth-first search outward from `start` up to `hops` levels deep.
    pub fn dfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.dfs_graphblas(m, &snap, start, hops)
    }

    /// Counts variable assignments of the directed triangle pattern
    /// `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` under `spec`'s per-hop relationship
    /// types and per-variable labels.
    ///
    /// The count follows Cypher MATCH semantics: each distinct assignment of
    /// `(a, b, c, e1, e2, e3)` is one match, so a single 3-cycle of distinct
    /// nodes counts once per rotation of `a` (three when all hops share one
    /// type), parallel edges multiply, and the three relationships must be
    /// pairwise distinct (relationship uniqueness), which only constrains
    /// self-loop assignments where `a == b == c`.
    pub fn count_triangle_cycles(&self, spec: &TriangleCountSpec) -> Result<u64, Error> {
        self.ensure_csr_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(0);
        }

        // A named but unregistered relationship type matches nothing.
        let mut type_ids: [Option<TypeId>; 3] = [None; 3];
        {
            let rtxn = self.storage.env.read_txn()?;
            for (i, name) in spec.rel_types.iter().enumerate() {
                if let Some(name) = name {
                    match get_type(&self.storage, &rtxn, name)? {
                        Some(tid) => type_ids[i] = Some(tid),
                        None => return Ok(0),
                    }
                }
            }
        }

        // Dense-index masks for the per-variable labels; `None` means
        // unconstrained. An unknown label yields an all-false mask, which
        // counts zero without a special case.
        let mut masks: [Option<Vec<bool>>; 3] = [None, None, None];
        for (i, label) in spec.labels.iter().enumerate() {
            if let Some(name) = label {
                let mut mask = vec![false; n];
                for id in self.nodes_by_label(name)? {
                    if let Some(&d) = snap.id_to_dense.get(&id) {
                        mask[d as usize] = true;
                    }
                }
                masks[i] = Some(mask);
            }
        }
        let label_ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);

        // Sorted typed adjacency for each hop: hop 1 and hop 2 read forward
        // rows, hop 3 reads the transpose (edges into `a`). Hop 2 reuses the
        // hop-1 view when the types coincide.
        let out1 = typed_out_sorted(&snap, type_ids[0]);
        let out2_built = if type_ids[1] == type_ids[0] {
            None
        } else {
            Some(typed_out_sorted(&snap, type_ids[1]))
        };
        let out2 = out2_built.as_ref().unwrap_or(&out1);
        let in3 = typed_in_sorted(&snap, type_ids[2]);

        let mut total: u64 = 0;
        for a in 0..n {
            if !label_ok(&masks[0], a) {
                continue;
            }
            let in3_row = in3.row(a);
            if in3_row.is_empty() {
                continue;
            }
            let out1_row = out1.row(a);

            let mut i = 0;
            while i < out1_row.len() {
                let b = out1_row[i].0 as usize;
                let run1_start = i;
                while i < out1_row.len() && out1_row[i].0 as usize == b {
                    i += 1;
                }
                if !label_ok(&masks[1], b) {
                    continue;
                }
                let m1 = (i - run1_start) as u64;
                let out2_row = out2.row(b);

                // Sorted merge of the hop-2 candidates from `b` against the
                // hop-3 sources into `a`; equal runs give parallel-edge
                // multiplicities.
                let (mut j, mut k) = (0, 0);
                let mut pair_count: u64 = 0;
                while j < out2_row.len() && k < in3_row.len() {
                    let c2 = out2_row[j].0;
                    let c3 = in3_row[k].0;
                    match c2.cmp(&c3) {
                        std::cmp::Ordering::Less => j += 1,
                        std::cmp::Ordering::Greater => k += 1,
                        std::cmp::Ordering::Equal => {
                            let c = c2 as usize;
                            let j0 = j;
                            while j < out2_row.len() && out2_row[j].0 as usize == c {
                                j += 1;
                            }
                            let k0 = k;
                            while k < in3_row.len() && in3_row[k].0 as usize == c {
                                k += 1;
                            }
                            if !label_ok(&masks[2], c) {
                                continue;
                            }
                            if a == b && c == a {
                                // Every hop is a self-loop at `a`, the one shape
                                // where two hops can bind the same relationship.
                                // Enumerate ordered triples of pairwise-distinct
                                // edge IDs explicitly; this term replaces the
                                // multiplicity product for this cell, so it is
                                // not scaled by `m1`.
                                for &(_, e1) in &out1_row[run1_start..run1_start + m1 as usize] {
                                    for &(_, e2) in &out2_row[j0..j] {
                                        if e2 == e1 {
                                            continue;
                                        }
                                        for &(_, e3) in &in3_row[k0..k] {
                                            if e3 != e1 && e3 != e2 {
                                                total += 1;
                                            }
                                        }
                                    }
                                }
                            } else {
                                pair_count += ((j - j0) * (k - k0)) as u64;
                            }
                        }
                    }
                }
                total += m1 * pair_count;
            }
        }
        Ok(total)
    }

    /// Counts variable assignments of an open directed path of one or two hops
    /// under `spec`'s per-hop relationship types and per-variable labels, with
    /// no materialization of the matched rows.
    ///
    /// The count follows Cypher MATCH semantics: each distinct assignment of
    /// the node and relationship variables is one match, nodes may repeat,
    /// parallel edges multiply, and for the two-hop pattern the two
    /// relationships must be distinct (relationship uniqueness). That
    /// uniqueness only removes assignments where a single edge could fill both
    /// hops, which requires a self-loop shared by both hops.
    ///
    /// The Cypher optimizer lowers a grouping-free `count` over a one-hop or
    /// two-hop directed expansion to this kernel via the `PathCount` physical operator.
    pub fn count_linear_paths(&self, spec: &PathCountSpec) -> Result<u64, Error> {
        let hops = spec.rel_types.len();
        debug_assert!(hops == 1 || hops == 2, "count_linear_paths: 1 or 2 hops");
        debug_assert_eq!(spec.labels.len(), hops + 1, "labels must be hops + 1");

        self.ensure_csr_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(0);
        }

        // A named but unregistered relationship type matches nothing.
        let mut type_ids: Vec<Option<TypeId>> = vec![None; hops];
        {
            let rtxn = self.storage.env.read_txn()?;
            for (i, name) in spec.rel_types.iter().enumerate() {
                if let Some(name) = name {
                    match get_type(&self.storage, &rtxn, name)? {
                        Some(tid) => type_ids[i] = Some(tid),
                        None => return Ok(0),
                    }
                }
            }
        }

        // Dense-index masks for the per-variable labels; `None` is
        // unconstrained. An unknown label yields an all-false mask, counting
        // zero without a special case.
        let mut masks: Vec<Option<Vec<bool>>> = vec![None; hops + 1];
        for (i, label) in spec.labels.iter().enumerate() {
            if let Some(name) = label {
                let mut mask = vec![false; n];
                for id in self.nodes_by_label(name)? {
                    if let Some(&d) = snap.id_to_dense.get(&id) {
                        mask[d as usize] = true;
                    }
                }
                masks[i] = Some(mask);
            }
        }
        // Per-variable allow-sets from pushed-down property predicates. A
        // present set intersects with the label mask (a node passes only when it
        // is in both); a node id absent from the snapshot maps to no dense index
        // and is simply dropped, counting zero without a special case. An empty
        // `vertex_allow` (the default) leaves every mask as the label mask, so an
        // unfiltered path count is unchanged.
        for (i, allow) in spec.vertex_allow.iter().enumerate() {
            let Some(ids) = allow else { continue };
            let mut amask = vec![false; n];
            for &id in ids {
                if let Some(&d) = snap.id_to_dense.get(&id) {
                    amask[d as usize] = true;
                }
            }
            match &mut masks[i] {
                Some(m) => {
                    for (slot, &keep) in m.iter_mut().zip(amask.iter()) {
                        *slot = *slot && keep;
                    }
                }
                None => masks[i] = Some(amask),
            }
        }
        let label_ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);

        if hops == 1 {
            // Count typed edges `v0 -> v1` with `v0` and `v1` inside their masks.
            let out1 = typed_out_sorted(&snap, type_ids[0]);
            let mut total: u64 = 0;
            for v0 in 0..n {
                if !label_ok(&masks[0], v0) {
                    continue;
                }
                for &(dst, _e) in out1.row(v0) {
                    if label_ok(&masks[1], dst as usize) {
                        total += 1;
                    }
                }
            }
            return Ok(total);
        }

        // Two hops `(v0:m0)-[t1]->(v1:m1)-[t2]->(v2:m2)`. The path count
        // factors through the middle node: for each `v1`, the number of
        // matches is the count of qualifying hop-1 in-edges times the count of
        // qualifying hop-2 out-edges. Relationship uniqueness then removes the
        // assignments where hop 1 and hop 2 bind the same edge, which is only
        // possible for a self-loop at `v1` that satisfies both hops.
        let in1 = typed_in_sorted(&snap, type_ids[0]); // edges into v1, type t1
        let out2 = typed_out_sorted(&snap, type_ids[1]); // edges out of v1, type t2
        let mut total: u64 = 0;
        for b in 0..n {
            if !label_ok(&masks[1], b) {
                continue;
            }
            let in_row = in1.row(b);
            let indeg = in_row
                .iter()
                .filter(|&&(src, _)| label_ok(&masks[0], src as usize))
                .count() as u64;
            if indeg == 0 {
                continue;
            }
            let out_row = out2.row(b);
            let outdeg = out_row
                .iter()
                .filter(|&&(dst, _)| label_ok(&masks[2], dst as usize))
                .count() as u64;
            total += indeg * outdeg;

            // Relationship-uniqueness correction. A single edge can fill both
            // hops only when it is a self-loop at `b` and `b` satisfies the
            // first and last masks. Such an edge appears in both rows with
            // neighbor `b`; intersect those self-loop entries by edge id. Rows
            // are sorted by `(neighbor, edge id)`, so the self-loop entries for
            // each row are a contiguous, edge-id-ascending run.
            if label_ok(&masks[0], b) && label_ok(&masks[2], b) {
                let in_self: Vec<EdgeId> = in_row
                    .iter()
                    .filter(|&&(src, _)| src as usize == b)
                    .map(|&(_, e)| e)
                    .collect();
                if !in_self.is_empty() {
                    let shared = out_row
                        .iter()
                        .filter(|&&(dst, e)| dst as usize == b && in_self.binary_search(&e).is_ok())
                        .count() as u64;
                    total = total.saturating_sub(shared);
                }
            }
        }
        Ok(total)
    }

    /// Counts typed edges grouped by one endpoint, returning `(group node id, count)`
    /// for every group node with a non-zero count. See [`GroupedDegreeSpec`]
    /// for the grouping and filtering semantics.
    ///
    /// This scans the CSR snapshot's outgoing adjacency once, incrementing a
    /// per-node counter, so it is `O(nodes + edges)` with no per-edge row
    /// materialization. It is the kernel the Cypher optimizer lowers a
    /// `count` aggregation grouped by one endpoint of a single directed hop
    /// to (the `GroupedDegree` physical operator), turning what would be a
    /// full expansion-and-fold into an integer pass over adjacency.
    pub fn grouped_edge_counts(
        &self,
        spec: &GroupedDegreeSpec,
    ) -> Result<Vec<(NodeId, u64)>, Error> {
        self.ensure_csr_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(Vec::new());
        }

        // A named but unregistered relationship type matches nothing.
        let type_id = match spec.rel_type {
            Some(name) => {
                let rtxn = self.storage.env.read_txn()?;
                match get_type(&self.storage, &rtxn, name)? {
                    Some(tid) => Some(tid),
                    None => return Ok(Vec::new()),
                }
            }
            None => None,
        };

        // Dense label masks; an unknown label yields an all-false mask, which
        // counts zero without a special case.
        let label_mask = |label: Option<&str>| -> Result<Option<Vec<bool>>, Error> {
            match label {
                Some(name) => {
                    let mut mask = vec![false; n];
                    for id in self.nodes_by_label(name)? {
                        if let Some(&d) = snap.id_to_dense.get(&id) {
                            mask[d as usize] = true;
                        }
                    }
                    Ok(Some(mask))
                }
                None => Ok(None),
            }
        };
        let group_mask = label_mask(spec.group_label)?;
        // The endpoints usually carry the same label (e.g. `(:Person)->(:Person)`);
        // reuse the mask instead of scanning that label a second time.
        let counted_mask = if spec.counted_label == spec.group_label {
            group_mask.clone()
        } else {
            label_mask(spec.counted_label)?
        };

        // Non-null mask for the counted endpoint's property: dense index `d` is
        // true when the property is present on `dense_to_id[d]`. The property
        // columns carry their own dense mapping, so resolve each CSR node id
        // through it. A missing column (no such property anywhere) leaves the
        // mask all-false, so `count(v.prop)` over an absent property counts
        // zero, matching the row pipeline.
        let nonnull_mask: Option<Vec<bool>> = match spec.counted_nonnull_prop {
            Some(prop) => Some(self.prop_columns.with_fresh(&self.storage, |cols| {
                let mut mask = vec![false; n];
                if let Some(col) = cols.cols.get(prop) {
                    for (d, id) in snap.dense_to_id.iter().enumerate() {
                        if let Some(&cd) = cols.id_to_dense.get(id) {
                            mask[d] = col.is_present(cd as usize);
                        }
                    }
                }
                mask
            })?),
            None => None,
        };

        let ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);

        // `present` marks a group node with at least one label-qualifying edge,
        // so it produces a MATCH row and therefore a group. `counts` is the
        // number of those edges whose counted endpoint also passes the non-null
        // filter. The two differ for `count(v.prop)`: a group can exist (an edge
        // reaches it) while its count is zero (every counted source has a null
        // property), and that group must still appear with count zero, exactly
        // as the row pipeline emits it.
        let mut counts = vec![0u64; n];
        let mut present = vec![false; n];
        for v0 in 0..n {
            for k in snap.row_ptr[v0]..snap.row_ptr[v0 + 1] {
                if let Some(tid) = type_id {
                    if snap.edge_type[k] != tid {
                        continue;
                    }
                }
                let v1 = snap.col_idx[k] as usize;
                // Map the stored edge `v0 -> v1` to the group and counted
                // endpoints per the grouping direction.
                let (group_d, counted_d) = if spec.group_is_dst {
                    (v1, v0)
                } else {
                    (v0, v1)
                };
                // Label constraints decide which edges match (existence); the
                // non-null property filter only narrows the count within them.
                if !ok(&group_mask, group_d) || !ok(&counted_mask, counted_d) {
                    continue;
                }
                present[group_d] = true;
                if ok(&nonnull_mask, counted_d) {
                    counts[group_d] += 1;
                }
            }
        }

        let mut out = Vec::new();
        for (d, &p) in present.iter().enumerate() {
            if p {
                out.push((snap.dense_to_id[d], counts[d]));
            }
        }
        Ok(out)
    }

    /// Detects if there is at least one directed cycle in the graph.
    pub fn detect_cycle(&self) -> Result<bool, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.detect_cycle_graphblas(m, &snap)
    }

    /// Returns directed neighbor entries for all outgoing and incoming edges of `node`.
    pub fn all_neighbors(&self, node: NodeId) -> Result<Vec<DirectedNeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut neighbors = Vec::new();
        for ne in self.out_neighbors_impl(&rtxn, node)? {
            neighbors.push(DirectedNeighborEntry {
                node: ne.node,
                edge: ne.edge,
                edge_type: ne.edge_type,
                outgoing: true,
            });
        }
        for ne in self.in_neighbors_impl(&rtxn, node)? {
            neighbors.push(DirectedNeighborEntry {
                node: ne.node,
                edge: ne.edge,
                edge_type: ne.edge_type,
                outgoing: false,
            });
        }
        Ok(neighbors)
    }

    /// Returns all simple paths (no repeated nodes) between `src` and `dst`.
    pub fn all_paths(&self, src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.all_paths_graphblas(m, &snap, src, dst)
    }

    /// Returns all unweighted shortest paths between `src` and `dst`.
    pub fn all_shortest_paths(&self, src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.all_shortest_paths_graphblas(m, &snap, src, dst)
    }

    /// Returns the longest simple path (no repeated nodes) between `src` and `dst`.
    pub fn longest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.longest_path_graphblas(m, &snap, src, dst)
    }

    /// Computes the weighted shortest path between `src` and `dst` using Dijkstra's algorithm.
    ///
    /// Edge weights come from the materialized CSR snapshot, which reads the
    /// first present of the `weight`, `cost`, `capacity`, or `cap` edge
    /// properties, defaulting to `1.0`. The weight source is fixed: unlike
    /// `shortest_path_top_k` and `spanning_forest`, this method does not take a
    /// weight-property argument.
    pub fn shortest_path_dijkstra(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<WeightedPath>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.shortest_path_dijkstra_graphblas(m, &snap, src, dst)
    }

    /// Computes the Minimum or Maximum Spanning Forest (MSF) of the graph.
    pub fn spanning_forest(
        &self,
        weight_property: &str,
        maximum: bool,
    ) -> Result<Vec<EdgeId>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.spanning_forest_graphblas(m, &snap, weight_property, maximum)
    }

    /// Computes community detection on the graph using the Label Propagation Algorithm (LPA / CDLP).
    pub fn label_propagation(&self, max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.label_propagation_graphblas(m, &snap, max_iterations)
    }

    /// Computes the harmonic closeness centrality for all nodes in the graph.
    pub fn harmonic_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.harmonic_centrality_graphblas(m, &snap)
    }

    /// Computes the betweenness centrality for all nodes in the graph.
    pub fn betweenness_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.betweenness_centrality_graphblas(m, &snap)
    }

    /// Computes the strongly connected components (SCC) of the graph using Tarjan's algorithm.
    pub fn strongly_connected_components(&self) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.strongly_connected_components_graphblas(m, &snap)
    }

    /// Computes the degree centrality for all nodes in the graph based on the specified direction.
    pub fn degree_centrality(
        &self,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_matrix_view()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        self.degree_centrality_graphblas(m, direction)
    }

    /// Computes the maximum flow from a source node to a sink node.
    pub fn maximum_flow(
        &self,
        source: NodeId,
        sink: NodeId,
        capacity_property: &str,
    ) -> Result<f64, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.maximum_flow_graphblas(m, &snap, source, sink, capacity_property)
    }

    /// Computes the K shortest paths from a source node to a destination node using Yen's algorithm.
    pub fn shortest_path_top_k(
        &self,
        src: NodeId,
        dst: NodeId,
        k: usize,
        weight_property: &str,
    ) -> Result<Vec<WeightedPath>, Error> {
        self.ensure_csr_fresh()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        let paths = self.shortest_path_top_k_graphblas(m, &snap, src, dst, k, weight_property)?;
        Ok(paths
            .into_iter()
            .map(|(nodes, total_weight)| WeightedPath {
                nodes,
                total_weight,
            })
            .collect())
    }

    /// Breadth-first search outward from `start` up to `hops` levels deep.
    pub fn bfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        self.ensure_matrix_view()?;
        self.bfs_graphblas(start, hops)
    }

    /// Unweighted shortest path from `src` to `dst` by BFS.
    pub fn shortest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        self.ensure_csr_fresh()?;
        self.shortest_path_graphblas(src, dst)
    }

    /// Iterative PageRank over the current CSR snapshot.
    pub fn page_rank(&self, iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error> {
        self.ensure_csr_fresh()?;
        self.page_rank_graphblas(iterations, damping)
    }

    /// Freshness gate for consumers that read the CSR snapshot: the native-CSR
    /// algorithms (`dfs`, `strongly_connected_components`, `maximum_flow`,
    /// `spanning_forest`, `shortest_path_top_k`, `all_paths`, `longest_path`,
    /// `detect_cycle`) and the hybrid SpMV-plus-path-reconstruction algorithms
    /// (`shortest_path_dijkstra`, `betweenness_centrality`, `harmonic_centrality`,
    /// `all_shortest_paths`, `page_rank`). A full rebuild refreshes both the
    /// snapshot and all matrices. Gated by the write generation, so it catches
    /// edge-only drift, not just node-count changes.
    pub(crate) fn ensure_csr_fresh(&self) -> Result<(), Error> {
        if self.matrices.read().is_none() || self.csr_cache.snapshot_is_stale() {
            self.rebuild_csr()?;
        } else {
            // A snapshot-only refresh (`ensure_snapshot_fresh`) leaves the
            // structural delta pending, so a fresh snapshot generation does
            // not imply fresh matrices; drain the delta into them.
            self.ensure_matrix_view()?;
        }
        Ok(())
    }

    /// Freshness gate for consumers that read only the CSR snapshot (typed
    /// expansion). Rebuilds the snapshot alone when it lags committed writes,
    /// skipping GraphBLAS matrix materialization; the pending structural delta
    /// stays in place for `ensure_matrix_view` to drain later.
    pub(crate) fn ensure_snapshot_fresh(&self) -> Result<(), Error> {
        if self.csr_cache.snapshot_is_stale() {
            let built_gen = self.csr_cache.current_gen();
            let snap = CsrSnapshot::build(&self.storage)?;
            self.csr_cache.install_snapshot(snap, built_gen);
        }
        Ok(())
    }

    /// Freshness gate for the pure-adjacency consumers (`bfs`,
    /// `bfs_multi_source`, untyped `expand`, `degree_centrality`,
    /// `connected_components`), which read only `adjacency`/`adjacency_t` and the
    /// dense mapping carried on `MatrixSet`. Applies the pending structural delta
    /// to the cached matrices in place (resize plus per-element set/drop) in
    /// O(delta), falling back to a full rebuild when a node was deleted (the
    /// dense-index mapping is reshuffled) or the matrices are not yet
    /// materialized. The take-and-apply runs under the matrices write lock, so a
    /// reader's subsequent `matrices.read()` never observes a partial apply.
    pub(crate) fn ensure_matrix_view(&self) -> Result<(), Error> {
        // A node deletion or an unmaterialized matrix set needs a full rebuild,
        // which refreshes the snapshot and all matrices from LMDB.
        if self.matrices.read().is_none() || self.csr_cache.pending_force_full() {
            return self.rebuild_csr();
        }
        // Cheap pre-check: skip the exclusive lock when nothing is pending.
        if !self.csr_cache.has_pending() {
            return Ok(());
        }

        let mut guard = self.matrices.write();
        let delta = self.csr_cache.take_delta();
        if delta.force_full {
            // A node deletion raced in after the peek above. Drop the guard
            // (rebuild_csr re-acquires the write lock) and rebuild from LMDB; the
            // taken delta is superseded.
            drop(guard);
            return self.rebuild_csr();
        }
        if delta.is_empty() {
            return Ok(());
        }

        // A removed edge clears the boolean adjacency bit only when no parallel
        // edge between the same endpoints remains. LMDB is the fresh truth.
        let mut clear_edges = Vec::new();
        {
            let rtxn = self.storage.env.read_txn()?;
            for &(src, dst) in &delta.removed_edges {
                let still_connected = self
                    .out_neighbors_impl(&rtxn, src)?
                    .into_iter()
                    .any(|ne| ne.node == dst);
                if !still_connected {
                    clear_edges.push((src, dst));
                }
            }
        }

        if let Some(m) = guard.as_mut() {
            m.apply_delta(&delta.added_nodes, &delta.added_edges, &clear_edges)?;
        }
        Ok(())
    }

    /// Returns all node IDs in the graph in ascending order.
    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.all_nodes_impl(&rtxn)
    }

    pub(super) fn all_nodes_impl(&self, rtxn: &heed::RoTxn) -> Result<Vec<NodeId>, Error> {
        let mut ids = self
            .storage
            .nodes
            .iter(rtxn)?
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        Ok(ids)
    }

    /// Weakly connected components via BFS treating all edges as undirected.
    ///
    /// Returns a map from each node ID to a component ID. Component IDs are
    /// assigned in ascending order of first discovery and have no guaranteed
    /// relationship to node IDs.
    pub fn connected_components(&self) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_matrix_view()?;
        {
            let guard = self.matrices.read();
            if let Some(m) = guard.as_ref() {
                if m.n_nodes > 0 {
                    return self.connected_components_graphblas(m);
                }
            }
        }
        let nodes: Vec<NodeId> = {
            let rtxn = self.storage.env.read_txn()?;
            self.storage
                .nodes
                .iter(&rtxn)?
                .map(|r| r.map(|(k, _)| k))
                .collect::<Result<Vec<_>, _>>()?
        };

        let mut component: HashMap<NodeId, u64> = HashMap::with_capacity(nodes.len());
        let mut next_id: u64 = 0;

        for &start in &nodes {
            if component.contains_key(&start) {
                continue;
            }
            let comp_id = next_id;
            next_id += 1;
            component.insert(start, comp_id);
            let mut queue = vec![start];
            while let Some(node) = queue.pop() {
                for ne in self.out_neighbors(node)? {
                    if component.insert(ne.node, comp_id).is_none() {
                        queue.push(ne.node);
                    }
                }
                for ne in self.in_neighbors(node)? {
                    if component.insert(ne.node, comp_id).is_none() {
                        queue.push(ne.node);
                    }
                }
            }
        }

        Ok(component)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Increment the dirty counter and, if the threshold is crossed and no
    /// rebuild is already running, spawn a background thread to rebuild the
    /// CSR snapshot from LMDB.
    pub(super) fn maybe_spawn_rebuild(&self) {
        self.maybe_spawn_rebuild_n(1);
    }

    pub(super) fn maybe_spawn_rebuild_n(&self, count: usize) {
        if self.csr_cache.mark_dirty_n(count as u64) {
            let cache = Arc::clone(&self.csr_cache);
            let storage = Arc::clone(&self.storage);
            let matrices = Arc::clone(&self.matrices);
            let thread_count = Arc::clone(&self.n_threads);
            std::thread::spawn(move || {
                // Rebuild until the dirty count drops below the threshold: writes
                // that commit while a rebuild runs keep the count above zero, and
                // `install` retains the claim and asks for another pass so the
                // snapshot does not silently lag behind LMDB.
                loop {
                    // Capture the generation before reading LMDB; writes that
                    // commit during the build leave the snapshot stale until the
                    // next pass, which the dirty-count loop already drives.
                    let built_gen = cache.current_gen();
                    // Clear before reading LMDB so writes during the build are
                    // retained in the emptied delta for a later incremental apply.
                    cache.clear_delta();
                    match CsrSnapshot::build(&storage) {
                        Ok(snap) => {
                            if let Ok(m) = MatrixSet::materialize(
                                &snap,
                                thread_count.load(std::sync::atomic::Ordering::Acquire),
                            ) {
                                *matrices.write() = Some(m);
                            }
                            if !cache.install(snap, built_gen) {
                                break;
                            }
                        }
                        Err(_) => {
                            cache.cancel_rebuild();
                            break;
                        }
                    }
                }
            });
        }
    }

    /// Append one `AdjEntry` as a new LMDB duplicate value: O(log n), no blob read.
    pub(super) fn append_adj(
        &self,
        wtxn: &mut heed::RwTxn,
        node: NodeId,
        other: NodeId,
        edge_type: u32,
        edge_id: EdgeId,
        outgoing: bool,
    ) -> Result<(), Error> {
        let entry = AdjEntry {
            edge_type,
            other,
            edge_id,
        };
        let db = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };
        db.put(wtxn, &node, entry.as_bytes())?;
        Ok(())
    }

    /// Iterate all duplicate `AdjEntry` values for `node` via LMDB cursor.
    pub(super) fn adj_entries(
        &self,
        node: NodeId,
        outgoing: bool,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.adj_entries_impl(&rtxn, node, outgoing)
    }

    pub(super) fn adj_entries_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
        outgoing: bool,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let db = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };

        let iter = match db.get_duplicates(rtxn, &node)? {
            Some(iter) => iter,
            None => return Ok(vec![]),
        };

        let mut out = Vec::new();
        for result in iter {
            let (_, bytes) = result?;
            let entry = AdjEntry::read_from_bytes(bytes)
                .ok()
                .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
            out.push(NeighborEntry {
                node: entry.other,
                edge: entry.edge_id,
                edge_type: entry.edge_type,
            });
        }
        Ok(out)
    }
}

/// Per-row adjacency restricted to one relationship type, with each row
/// sorted by `(neighbor, edge id)` so intersections run as sorted merges and
/// parallel edges form contiguous runs.
struct TypedSortedAdj {
    ptr: Vec<usize>,
    adj: Vec<(u32, EdgeId)>,
}

impl TypedSortedAdj {
    fn row(&self, d: usize) -> &[(u32, EdgeId)] {
        &self.adj[self.ptr[d]..self.ptr[d + 1]]
    }
}

/// Forward adjacency from the CSR snapshot filtered to `type_id` (`None`
/// keeps every edge), rows sorted by `(dst, edge id)`.
fn typed_out_sorted(snap: &CsrSnapshot, type_id: Option<TypeId>) -> TypedSortedAdj {
    let n = snap.dense_to_id.len();
    let keep = |idx: usize| type_id.is_none_or(|t| snap.edge_type[idx] == t);

    let mut ptr = vec![0usize; n + 1];
    for row in 0..n {
        let mut count = 0;
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                count += 1;
            }
        }
        ptr[row + 1] = ptr[row] + count;
    }

    let mut adj = vec![(0u32, 0u64); ptr[n]];
    for row in 0..n {
        let mut at = ptr[row];
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                adj[at] = (snap.col_idx[idx], snap.edge_id[idx]);
                at += 1;
            }
        }
        adj[ptr[row]..at].sort_unstable();
    }
    TypedSortedAdj { ptr, adj }
}

/// Transposed adjacency (edges grouped by destination) filtered to
/// `type_id`, rows sorted by `(src, edge id)`.
fn typed_in_sorted(snap: &CsrSnapshot, type_id: Option<TypeId>) -> TypedSortedAdj {
    let n = snap.dense_to_id.len();
    let keep = |idx: usize| type_id.is_none_or(|t| snap.edge_type[idx] == t);

    let mut ptr = vec![0usize; n + 1];
    for idx in 0..snap.col_idx.len() {
        if keep(idx) {
            ptr[snap.col_idx[idx] as usize + 1] += 1;
        }
    }
    for d in 0..n {
        ptr[d + 1] += ptr[d];
    }

    let mut at = ptr.clone();
    let mut adj = vec![(0u32, 0u64); ptr[n]];
    for row in 0..n {
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                let dst = snap.col_idx[idx] as usize;
                adj[at[dst]] = (row as u32, snap.edge_id[idx]);
                at[dst] += 1;
            }
        }
    }
    for d in 0..n {
        adj[ptr[d]..ptr[d + 1]].sort_unstable();
    }
    TypedSortedAdj { ptr, adj }
}

#[cfg(test)]
mod incremental_matrix_tests {
    use issundb_graphblas::Matrix;
    use serde_json::json;
    use tempfile::TempDir;

    use std::collections::{BTreeMap, HashMap};

    use crate::Graph;
    use crate::graph::DegreeDirection;
    use crate::schema::NodeId;

    /// Adjacency coordinates, transpose coordinates, and the dense-index mapping:
    /// the matrix-view state the incremental path maintains.
    type MatrixView = (Vec<(usize, usize)>, Vec<(usize, usize)>, Vec<NodeId>);

    /// Canonicalize a component map to its underlying partition (each node mapped
    /// to the smallest node id in its component), so two results compare equal
    /// regardless of the arbitrary component-id numbering.
    fn canonical_partition(cc: &HashMap<NodeId, u64>) -> BTreeMap<NodeId, NodeId> {
        let mut groups: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for (&node, &comp) in cc {
            groups.entry(comp).or_default().push(node);
        }
        let mut out = BTreeMap::new();
        for members in groups.into_values() {
            let rep = *members.iter().min().unwrap();
            for n in members {
                out.insert(n, rep);
            }
        }
        out
    }

    /// Sorted, deduplicated `(row, col)` coordinates of a boolean adjacency
    /// matrix, for set comparison independent of internal storage order.
    fn matrix_coords(m: &Matrix<i32>) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = m
            .triples()
            .expect("triples")
            .into_iter()
            .map(|(r, c, _)| (r, c))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Snapshot the matrix-view state that the incremental path maintains:
    /// adjacency coordinates, transpose coordinates, and the dense-index mapping.
    fn extract(graph: &Graph) -> MatrixView {
        let guard = graph.matrices.read();
        let m = guard.as_ref().expect("matrices materialized");
        (
            matrix_coords(&m.adjacency),
            matrix_coords(&m.adjacency_t),
            m.dense_to_id.clone(),
        )
    }

    /// The incrementally-maintained matrices must be byte-identical (as element
    /// sets and dense mapping) to a full rebuild over the same final LMDB state.
    /// Because the incremental matrices equal the freshly-built ones, any
    /// consumer reading them sees every committed mutation: this is the freshness
    /// proof as well as the correctness proof.
    #[test]
    fn incremental_matrices_match_full_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();

        // Base graph: a 20-node ring.
        let ids: Vec<NodeId> = (0..20)
            .map(|i| g.add_node("N", &json!({ "v": i })).unwrap())
            .collect();
        let mut base_edges = Vec::new();
        for i in 0..20 {
            base_edges.push(
                g.add_edge(ids[i], ids[(i + 1) % 20], "R", &json!({}))
                    .unwrap(),
            );
        }
        // Establish the base matrices and clear the pending delta.
        g.rebuild_csr().unwrap();

        // Mutations recorded into the delta:
        // 1. New edges among existing nodes.
        g.add_edge(ids[0], ids[5], "R", &json!({})).unwrap();
        g.add_edge(ids[3], ids[10], "R", &json!({})).unwrap();
        // 2. Parallel edges, then remove one: the adjacency bit must stay set.
        let par_a = g.add_edge(ids[2], ids[4], "R", &json!({})).unwrap();
        let _par_b = g.add_edge(ids[2], ids[4], "R", &json!({})).unwrap();
        // 3. New nodes with edges (matrix must grow).
        let n20 = g.add_node("N", &json!({ "v": 20 })).unwrap();
        let n21 = g.add_node("N", &json!({ "v": 21 })).unwrap();
        g.add_edge(n20, n21, "R", &json!({})).unwrap();
        g.add_edge(ids[1], n20, "R", &json!({})).unwrap();
        // 4. Remove an edge with no parallel: the adjacency bit must clear.
        g.delete_edge(base_edges[7]).unwrap();
        // 5. Remove one of the parallel pair (the other still connects the pair).
        g.delete_edge(par_a).unwrap();

        // Incremental refresh, then snapshot.
        g.ensure_matrix_view().unwrap();
        let incremental = extract(&g);

        // Full rebuild over the same LMDB state, then snapshot.
        g.rebuild_csr().unwrap();
        let full = extract(&g);

        assert_eq!(incremental.0, full.0, "adjacency element sets differ");
        assert_eq!(incremental.1, full.1, "adjacency_t element sets differ");
        assert_eq!(incremental.2, full.2, "dense-index mapping differs");
    }

    /// A node deletion reshuffles dense indices, so the refresh must fall back to
    /// a full rebuild and still match.
    #[test]
    fn node_deletion_forces_full_rebuild_and_matches() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..10)
            .map(|i| g.add_node("N", &json!({ "v": i })).unwrap())
            .collect();
        for i in 0..10 {
            g.add_edge(ids[i], ids[(i + 1) % 10], "R", &json!({}))
                .unwrap();
        }
        g.rebuild_csr().unwrap();

        // Delete a node (cascades its edges) and add a fresh edge.
        g.delete_node(ids[3]).unwrap();
        g.add_edge(ids[5], ids[7], "R", &json!({})).unwrap();

        g.ensure_matrix_view().unwrap();
        let incremental = extract(&g);
        g.rebuild_csr().unwrap();
        let full = extract(&g);

        assert_eq!(incremental.0, full.0, "adjacency element sets differ");
        assert_eq!(incremental.1, full.1, "adjacency_t element sets differ");
        assert_eq!(incremental.2, full.2, "dense-index mapping differs");
    }

    /// Go/no-go measurement (ignored by default; the build dominates runtime).
    /// Run with:
    /// `cargo test -p issundb-core --release incremental_apply_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement: prints incremental-apply vs full-rebuild timings"]
    fn incremental_apply_cost() {
        use std::time::Instant;

        fn measure(n_nodes: usize, out_degree: usize, k_added: usize) {
            let dir = TempDir::new().unwrap();
            let g = Graph::open(dir.path(), 4).unwrap();
            // Build the base graph in one batched transaction: individual commits
            // would dominate the runtime and swamp the measurement.
            let ids: Vec<NodeId> = g
                .update(|txn| {
                    let ids: Vec<NodeId> = (0..n_nodes)
                        .map(|i| txn.add_node("N", &json!({ "v": i })).unwrap())
                        .collect();
                    for i in 0..n_nodes {
                        for k in 0..out_degree {
                            let off = 1 + k * 7;
                            txn.add_edge(ids[i], ids[(i + off) % n_nodes], "R", &json!({}))
                                .unwrap();
                        }
                    }
                    Ok(ids)
                })
                .unwrap();
            g.rebuild_csr().unwrap();

            // Stage `k_added` new edges among existing nodes, then time the
            // incremental apply of exactly that delta.
            for j in 0..k_added {
                let a = (j * 31) % n_nodes;
                let b = (j * 97 + 5) % n_nodes;
                g.add_edge(ids[a], ids[b], "R", &json!({})).unwrap();
            }
            let t = Instant::now();
            g.ensure_matrix_view().unwrap();
            let incr = t.elapsed();

            // Full rebuild is independent of the delta size: it is the cost the
            // incremental path replaces.
            let mut best_full = std::time::Duration::from_secs(3600);
            for _ in 0..3 {
                let t = Instant::now();
                g.rebuild_csr().unwrap();
                let e = t.elapsed();
                if e < best_full {
                    best_full = e;
                }
            }
            let n_edges = n_nodes * out_degree + k_added;
            println!(
                "{:>7} nodes, {:>9} edges: incremental apply of {} edges = {:>8.3} ms; full rebuild = {:>8.2} ms",
                n_nodes,
                n_edges,
                k_added,
                incr.as_secs_f64() * 1e3,
                best_full.as_secs_f64() * 1e3,
            );
        }

        measure(10_000, 5, 1_000);
        measure(50_000, 5, 1_000);
        measure(100_000, 5, 1_000);
    }

    /// End-to-end differential check: the migrated matrix-view consumers (`bfs`,
    /// `degree_centrality`, `connected_components`) must return identical results
    /// whether refreshed incrementally or via a forced full rebuild, over a
    /// mutation battery including a new node reached through a new edge.
    #[test]
    fn incremental_consumers_match_full_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..15)
            .map(|i| g.add_node("N", &json!({ "v": i })).unwrap())
            .collect();
        for i in 0..15 {
            g.add_edge(ids[i], ids[(i + 1) % 15], "R", &json!({}))
                .unwrap();
        }
        g.rebuild_csr().unwrap();

        // Mutations recorded into the delta, with no rebuild in between.
        g.add_edge(ids[0], ids[7], "R", &json!({})).unwrap();
        let n15 = g.add_node("N", &json!({ "v": 15 })).unwrap();
        g.add_edge(ids[2], n15, "R", &json!({})).unwrap();
        g.add_edge(n15, ids[5], "R", &json!({})).unwrap();

        // Results via the incremental matrix-view path.
        let bfs_incr = {
            let mut v = g.bfs(ids[0], 3).unwrap();
            v.sort_unstable();
            v
        };
        let deg_incr = g.degree_centrality(DegreeDirection::Both).unwrap();
        let cc_incr = canonical_partition(&g.connected_components().unwrap());

        // Results via a forced full rebuild over the same LMDB state.
        g.rebuild_csr().unwrap();
        let bfs_full = {
            let mut v = g.bfs(ids[0], 3).unwrap();
            v.sort_unstable();
            v
        };
        let deg_full = g.degree_centrality(DegreeDirection::Both).unwrap();
        let cc_full = canonical_partition(&g.connected_components().unwrap());

        assert_eq!(bfs_incr, bfs_full, "bfs: incremental vs full rebuild");
        assert_eq!(deg_incr, deg_full, "degree: incremental vs full rebuild");
        assert_eq!(cc_incr, cc_full, "components: incremental vs full rebuild");
    }

    /// Freshness: a matrix-view consumer reflects an edge, and a brand-new node
    /// reached through a new edge, with no explicit `rebuild_csr` between the
    /// write and the read. This is the edge-drift bug the migration closes.
    #[test]
    fn matrix_view_consumers_reflect_writes_without_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        assert!(
            !g.bfs(a, 5).unwrap().contains(&b),
            "b is unreachable before the edge exists"
        );

        // Edge between existing nodes, no rebuild: the incremental view sees it.
        g.add_edge(a, b, "R", &json!({})).unwrap();
        assert!(
            g.bfs(a, 1).unwrap().contains(&b),
            "b reachable from a after the edge, without a rebuild"
        );

        // A brand-new node reached through a new edge, still no rebuild: this
        // exercises the matrix resize plus dense-mapping extension end to end.
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(b, c, "R", &json!({})).unwrap();
        assert!(
            g.bfs(a, 2).unwrap().contains(&c),
            "new node c reachable two hops from a, without a rebuild"
        );
    }

    /// Freshness for the CSR-snapshot consumers: a generation-gated rebuild makes
    /// a native-CSR algorithm (`all_paths`) reflect an edge added with no explicit
    /// `rebuild_csr`.
    #[test]
    fn csr_consumers_reflect_writes_without_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        assert!(
            g.all_paths(a, c).unwrap().is_empty(),
            "no path a..c before the edge exists"
        );

        // Edge b->c, no rebuild: the write-generation gate forces a refresh.
        g.add_edge(b, c, "R", &json!({})).unwrap();
        assert!(
            !g.all_paths(a, c).unwrap().is_empty(),
            "path a->b->c reflected without an explicit rebuild"
        );
    }

    /// After a write, `ensure_matrix_view` applies the delta with
    /// `GrB_Matrix_setElement` (lazy in non-blocking mode), then drops the write
    /// lock. Multiple `bfs` calls then take the shared `matrices.read()` lock and
    /// run `mxv` concurrently. If the pending operations were not materialized
    /// under the write lock, the first `mxv` triggers GraphBLAS lazy completion,
    /// which mutates the shared matrix's internal representation while other
    /// readers race on it: undefined behavior. With the fix (`apply_delta`
    /// materializes the adjacency matrices before releasing the write lock),
    /// every concurrent `bfs` returns the full reachable set deterministically.
    #[test]
    fn concurrent_bfs_after_incremental_write_is_consistent() {
        use std::sync::Barrier;

        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();

        // A chain 0 -> 1 -> ... -> 29: bfs from node 0 reaches all 30 nodes.
        const N: usize = 30;
        let start = g.add_node("N", &json!({ "v": 0 })).unwrap();
        let mut prev = start;
        for i in 1..N {
            let node = g.add_node("N", &json!({ "v": i })).unwrap();
            g.add_edge(prev, node, "R", &json!({})).unwrap();
            prev = node;
        }
        g.rebuild_csr().unwrap();

        const THREADS: usize = 6;
        const ROUNDS: usize = 200;
        let mut expected = N;
        for r in 0..ROUNDS {
            // Attach a fresh node directly to `start`. The edge start -> new is a
            // brand-new matrix coordinate, so `apply_delta` records a pending
            // `setElement` (lazy in non-blocking mode), re-opening the
            // lazy-completion race window. The reachable set from `start` grows by
            // exactly one, keeping the expected count deterministic.
            let leaf = g.add_node("N", &json!({ "leaf": r })).unwrap();
            g.add_edge(start, leaf, "R", &json!({})).unwrap();
            expected += 1;

            let barrier = Barrier::new(THREADS);
            std::thread::scope(|s| {
                for _ in 0..THREADS {
                    let g = &g;
                    let barrier = &barrier;
                    s.spawn(move || {
                        // Synchronize so the threads reach the shared-read `mxv`
                        // together, maximizing the overlap on the pending matrix.
                        barrier.wait();
                        let reached = g.bfs(start, u8::MAX).unwrap();
                        assert_eq!(
                            reached.len(),
                            expected,
                            "concurrent bfs saw a partially materialized matrix"
                        );
                    });
                }
            });
        }
    }
}

#[cfg(test)]
mod linear_path_count_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, PathCountSpec};

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    fn spec(
        rels: &[Option<&'static str>],
        labels: &[Option<&'static str>],
    ) -> PathCountSpec<'static> {
        PathCountSpec {
            rel_types: rels.to_vec(),
            labels: labels.to_vec(),
            vertex_allow: Vec::new(),
        }
    }

    /// A per-variable allow-set intersects with the label, restricting the
    /// counted paths to the supplied node ids exactly as a brute-force count
    /// over the same restriction does.
    #[test]
    fn two_hop_allow_set_restricts_middle_and_dest() {
        let (_dir, g) = open_tmp();
        // Five people; ages drive the allow-sets below.
        let p: Vec<_> = (0..5)
            .map(|i| {
                g.add_node("Person", &json!({ "age": 20 + i * 10 }))
                    .unwrap()
            })
            .collect();
        // A small FOLLOWS web with two-hop paths through several middles.
        let edges = [(0, 1), (0, 2), (1, 2), (1, 3), (2, 3), (2, 4), (3, 4)];
        for &(s, d) in &edges {
            g.add_edge(p[s], p[d], "FOLLOWS", &json!({})).unwrap();
        }

        // Allow middles {p1, p2} and destinations {p3, p4}. Brute-force the
        // count of (a)-[FOLLOWS]->(b)-[FOLLOWS]->(c) with b in the middle set
        // and c in the dest set.
        let mid = [p[1], p[2]];
        let dst = [p[3], p[4]];
        let mut expected = 0u64;
        for &(_s1, d1) in &edges {
            if !mid.contains(&p[d1]) {
                continue;
            }
            for &(s2, d2) in &edges {
                if p[s2] == p[d1] && dst.contains(&p[d2]) {
                    expected += 1;
                }
            }
        }
        assert!(expected > 0, "test graph must have qualifying paths");

        let filtered = PathCountSpec {
            rel_types: vec![Some("FOLLOWS"), Some("FOLLOWS")],
            labels: vec![Some("Person"), Some("Person"), Some("Person")],
            vertex_allow: vec![None, Some(mid.to_vec()), Some(dst.to_vec())],
        };
        assert_eq!(g.count_linear_paths(&filtered).unwrap(), expected);

        // The same pattern with no allow-sets counts every two-hop path, so the
        // restriction strictly reduces the count.
        let unfiltered = g
            .count_linear_paths(&spec(
                &[Some("FOLLOWS"), Some("FOLLOWS")],
                &[Some("Person"); 3],
            ))
            .unwrap();
        assert!(unfiltered > expected);
    }

    /// One hop counts typed edges whose endpoints carry the required labels.
    #[test]
    fn one_hop_counts_typed_edges() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// A one-hop label predicate on the far endpoint excludes mismatched
    /// targets.
    #[test]
    fn one_hop_label_filter_excludes_endpoint() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("City", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Two distinct hops over distinct nodes count once.
    #[test]
    fn two_hop_distinct_nodes_count_once() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Parallel edges on one hop multiply the assignment count.
    #[test]
    fn two_hop_parallel_edges_multiply() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// Relationship uniqueness removes the assignment where one self-loop edge
    /// would fill both hops, while keeping the path that leaves the self-loop.
    #[test]
    fn two_hop_self_loop_respects_relationship_uniqueness() {
        let (_dir, g) = open_tmp();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap(); // self-loop
        g.add_edge(x, y, "KNOWS", &json!({})).unwrap();

        // Without the uniqueness rule the middle-node product would be 2
        // (in-degree 1 times out-degree 2 at x); the shared self-loop edge is
        // the one excluded pair, leaving the single (self-loop, x->y) path.
        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// An unregistered relationship type matches nothing.
    #[test]
    fn unknown_relationship_type_counts_zero() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("LIKES")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[cfg(test)]
mod triangle_cycle_count_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, TriangleCountSpec};

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    fn spec_all<'a>(rel: &'a str, label: &'a str) -> TriangleCountSpec<'a> {
        TriangleCountSpec {
            rel_types: [Some(rel); 3],
            labels: [Some(label); 3],
        }
    }

    /// One directed 3-cycle of distinct nodes matches once per rotation of
    /// `a`: three assignments, exactly what MATCH row semantics produce.
    #[test]
    fn single_cycle_counts_one_per_rotation() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// A non-cycle triangle orientation (two edges out of one node) is not a
    /// directed cycle and must not count.
    #[test]
    fn non_cyclic_orientation_does_not_count() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Parallel edges are distinct relationships; doubling one hop doubles
    /// every assignment that uses it.
    #[test]
    fn parallel_edges_multiply() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 6);
    }

    /// Per-hop types are positional: a cycle whose third edge has a different
    /// type matches only the rotation whose hop order lines up with the spec.
    #[test]
    fn per_hop_types_are_positional() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "LIKES", &json!({})).unwrap();

        let homogeneous = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(homogeneous, 0);

        let mixed = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [Some("KNOWS"), Some("KNOWS"), Some("LIKES")],
                labels: [Some("Person"); 3],
            })
            .unwrap();
        assert_eq!(mixed, 1);
    }

    /// Untyped hops match any relationship type.
    #[test]
    fn untyped_hops_match_any_type() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "LIKES", &json!({})).unwrap();
        g.add_edge(c, a, "FOLLOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [None; 3],
                labels: [Some("Person"); 3],
            })
            .unwrap();
        assert_eq!(n, 3);
    }

    /// A node missing the required label excludes every assignment that
    /// binds it; a multi-label node still qualifies.
    #[test]
    fn label_filter_applies_per_variable() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Robot", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let strict = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(strict, 0);

        // With the label added, the node carries both labels and qualifies.
        g.add_label(c, "Person").unwrap();
        let after = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(after, 3);

        let unlabeled = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [Some("KNOWS"); 3],
                labels: [None; 3],
            })
            .unwrap();
        assert_eq!(unlabeled, 3);
    }

    /// Relationship uniqueness: with `a == b == c` every hop is a self-loop,
    /// so matches are ordered triples of pairwise-distinct self-loop edges.
    /// Three self-loops give 3! = 6; two give none.
    #[test]
    fn self_loop_assignments_respect_relationship_uniqueness() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();

        let two = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(two, 0);

        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();
        let three = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(three, 6);
    }

    /// A self-loop combined with a 2-cycle yields one assignment per choice
    /// of the variable bound to the looped node: a=b, b=c, or c=a.
    #[test]
    fn self_loop_with_two_cycle_counts_each_position() {
        let (_dir, g) = open_tmp();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap();
        g.add_edge(x, y, "KNOWS", &json!({})).unwrap();
        g.add_edge(y, x, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// Unknown relationship types and labels match nothing instead of
    /// erroring: the query layer maps absent registry entries to empty scans.
    #[test]
    fn unknown_type_or_label_counts_zero() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        assert_eq!(
            g.count_triangle_cycles(&spec_all("NOPE", "Person"))
                .unwrap(),
            0
        );
        assert_eq!(
            g.count_triangle_cycles(&spec_all("KNOWS", "Ghost"))
                .unwrap(),
            0
        );
    }

    /// The count must reflect committed writes without an explicit
    /// `rebuild_csr`: the freshness gate covers this consumer.
    #[test]
    fn count_is_fresh_after_writes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let before = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(before, 0);

        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();
        let after = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(after, 3);
    }

    /// An empty graph counts zero without erroring on unmaterialized state.
    #[test]
    fn empty_graph_counts_zero() {
        let (_dir, g) = open_tmp();
        assert_eq!(
            g.count_triangle_cycles(&spec_all("KNOWS", "Person"))
                .unwrap(),
            0
        );
    }
}
