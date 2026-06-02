use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Graph algorithms
    // ------------------------------------------------------------------

    /// Depth-first search outward from `start` up to `hops` levels deep.
    pub fn dfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.dfs_graphblas(m, &snap, start, hops)
    }

    /// Detects if there is at least one directed cycle in the graph.
    pub fn detect_cycle(&self) -> Result<bool, Error> {
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.all_paths_graphblas(m, &snap, src, dst)
    }

    /// Returns all unweighted shortest paths between `src` and `dst`.
    pub fn all_shortest_paths(&self, src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error> {
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.all_shortest_paths_graphblas(m, &snap, src, dst)
    }

    /// Returns the longest simple path (no repeated nodes) between `src` and `dst`.
    pub fn longest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.spanning_forest_graphblas(m, &snap, weight_property, maximum)
    }

    /// Computes community detection on the graph using the Label Propagation Algorithm (LPA / CDLP).
    pub fn label_propagation(&self, max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.label_propagation_graphblas(m, &snap, max_iterations)
    }

    /// Computes the harmonic closeness centrality for all nodes in the graph.
    pub fn harmonic_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.harmonic_centrality_graphblas(m, &snap)
    }

    /// Computes the betweenness centrality for all nodes in the graph.
    pub fn betweenness_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.betweenness_centrality_graphblas(m, &snap)
    }

    /// Computes the strongly connected components (SCC) of the graph using Tarjan's algorithm.
    pub fn strongly_connected_components(&self) -> Result<HashMap<NodeId, u64>, Error> {
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let snap = self.csr_cache.snapshot.load();
        self.degree_centrality_graphblas(m, &snap, direction)
    }

    /// Computes the maximum flow from a source node to a sink node.
    pub fn maximum_flow(
        &self,
        source: NodeId,
        sink: NodeId,
        capacity_property: &str,
    ) -> Result<f64, Error> {
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
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
        self.ensure_matrices()?;
        self.bfs_graphblas(start, hops)
    }

    /// Unweighted shortest path from `src` to `dst` by BFS.
    pub fn shortest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        self.ensure_matrices()?;
        self.shortest_path_graphblas(src, dst)
    }

    /// Iterative PageRank over the current CSR snapshot.
    pub fn page_rank(&self, iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error> {
        self.ensure_matrices()?;
        self.page_rank_graphblas(iterations, damping)
    }

    /// Dynamic matrices materialization guard to rebuild snapshot and matrices unconditionally.
    pub(crate) fn ensure_matrices(&self) -> Result<(), Error> {
        let needs_rebuild = {
            let guard = self.matrices.read();
            match guard.as_ref() {
                Some(m) => {
                    let rtxn = self.storage.env.read_txn()?;
                    let db_len = self.storage.nodes.len(&rtxn)?;
                    m.n_nodes != db_len as usize
                }
                None => true,
            }
        };
        if needs_rebuild {
            self.rebuild_csr()?;
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
        {
            let guard = self.matrices.read();
            if let Some(m) = guard.as_ref() {
                if m.n_nodes > 0 {
                    let snap = self.csr_cache.snapshot.load();
                    return self.connected_components_graphblas(m, &snap);
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
            std::thread::spawn(move || {
                // Rebuild until the dirty count drops below the threshold: writes
                // that commit while a rebuild runs keep the count above zero, and
                // `install` retains the claim and asks for another pass so the
                // snapshot does not silently lag behind LMDB.
                loop {
                    match CsrSnapshot::build(&storage) {
                        Ok(snap) => {
                            if let Ok(m) = MatrixSet::materialize(&snap) {
                                *matrices.write() = Some(m);
                            }
                            if !cache.install(snap) {
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
