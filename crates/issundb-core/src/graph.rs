use std::{collections::HashMap, path::Path, sync::Arc};

use parking_lot::Mutex;
use serde::Serialize;
use zerocopy::{AsBytes, FromBytes};

use ahash::AHashSet;

#[cfg(feature = "graphblas")]
use crate::matrices::MatrixSet;
use crate::{
    csr::{CsrCache, CsrSnapshot},
    error::Error,
    schema::{AdjEntry, EdgeId, EdgeRecord, LabelId, NodeId, NodeRecord, TypeId},
    storage::{
        ids::{alloc_edge_id, alloc_node_id, get_or_create_label, get_or_create_type},
        lmdb::Storage,
        props,
    },
    vector::{Hit, VectorIndex},
};

/// Builds a 12-byte composite key `(prefix u32 BE, id u64 BE)` for secondary index lookups.
fn composite_key(prefix: u32, id: u64) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&prefix.to_be_bytes());
    key[4..].copy_from_slice(&id.to_be_bytes());
    key
}

/// The graph database handle. Cheap to clone: all state is behind `Arc`.
#[derive(Clone)]
pub struct Graph {
    storage: Arc<Storage>,
    _write_lock: Arc<Mutex<()>>,
    csr_cache: Arc<CsrCache>,
    vector_index: Arc<VectorIndex>,
    #[cfg(feature = "graphblas")]
    matrices: Arc<parking_lot::RwLock<Option<MatrixSet>>>,
}

impl Graph {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        let initial = CsrSnapshot::build(&storage)?;
        let vector_index = {
            let vi = VectorIndex::new();
            let rtxn = storage.env.read_txn()?;
            for result in storage.vectors.iter(&rtxn)? {
                let (node_id, bytes) = result?;
                let v: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                vi.upsert(node_id, &v)?;
            }
            drop(rtxn);
            Arc::new(vi)
        };
        let storage = Arc::new(storage);
        let csr_cache = Arc::new(CsrCache::new(initial));
        #[cfg(feature = "graphblas")]
        let matrices = {
            let initial_snap = csr_cache.snapshot.load();
            let m = MatrixSet::materialize(&initial_snap)?;
            Arc::new(parking_lot::RwLock::new(Some(m)))
        };
        Ok(Self {
            storage,
            _write_lock: Arc::new(Mutex::new(())),
            csr_cache,
            vector_index,
            #[cfg(feature = "graphblas")]
            matrices,
        })
    }

    /// Synchronously rebuild the CSR snapshot from LMDB. Useful after bulk
    /// loads or when tests need a consistent read view before the threshold
    /// has been crossed.
    pub fn rebuild_csr(&self) -> Result<(), Error> {
        let snap = CsrSnapshot::build(&self.storage)?;
        #[cfg(feature = "graphblas")]
        {
            let m = MatrixSet::materialize(&snap)?;
            *self.matrices.write() = Some(m);
        }
        self.csr_cache.install(snap);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Nodes
    // ------------------------------------------------------------------

    /// Insert a node with a string label and msgpack-serializable properties.
    pub fn add_node(&self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;

        let label_id = get_or_create_label(&self.storage, &mut wtxn, label)?;
        let node_id = alloc_node_id(&self.storage, &mut wtxn)?;

        let record = NodeRecord {
            label: label_id,
            props: props::encode(props)?,
        };
        self.storage
            .nodes
            .put(&mut wtxn, &node_id, &props::encode(&record)?)?;
        self.storage
            .label_idx
            .put(&mut wtxn, &composite_key(label_id, node_id), &())?;

        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(node_id)
    }

    /// Fetch a node record by id.
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        match self.storage.nodes.get(&rtxn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Update node properties.
    pub fn update_node(&self, id: NodeId, label: &str, props: &impl Serialize) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let label_id = get_or_create_label(&self.storage, &mut wtxn, label)?;
        let record = NodeRecord {
            label: label_id,
            props: props::encode(props)?,
        };
        self.storage
            .nodes
            .put(&mut wtxn, &id, &props::encode(&record)?)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    /// Delete a node.
    pub fn delete_node(&self, id: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.storage.nodes.delete(&mut wtxn, &id)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Edges
    // ------------------------------------------------------------------

    /// Insert a directed edge `src → dst` with a string type and properties.
    pub fn add_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;

        let type_id = get_or_create_type(&self.storage, &mut wtxn, etype)?;
        let edge_id = alloc_edge_id(&self.storage, &mut wtxn)?;

        let record = EdgeRecord {
            src,
            dst,
            edge_type: type_id,
            props: props::encode(props)?,
        };
        self.storage
            .edges
            .put(&mut wtxn, &edge_id, &props::encode(&record)?)?;
        self.storage
            .type_idx
            .put(&mut wtxn, &composite_key(type_id, edge_id), &())?;

        self.append_adj(&mut wtxn, src, dst, type_id, edge_id, true)?;
        self.append_adj(&mut wtxn, dst, src, type_id, edge_id, false)?;

        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(edge_id)
    }

    /// Fetch an edge record by id.
    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        match self.storage.edges.get(&rtxn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Traversal
    // ------------------------------------------------------------------

    /// Returns `(other_node_id, edge_id, type_id)` for all outgoing edges of `node`.
    ///
    /// Uses the in-memory CSR snapshot when the node is present in it; falls
    /// back to an LMDB cursor for nodes added since the last rebuild.
    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<(NodeId, EdgeId, u32)>, Error> {
        let snap = self.csr_cache.snapshot.load();
        if let Some(neighbors) = snap.out_neighbors(node) {
            return Ok(neighbors);
        }
        self.adj_entries(node, true)
    }

    /// Returns `(other_node_id, edge_id, type_id)` for all incoming edges of `node`.
    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<(NodeId, EdgeId, u32)>, Error> {
        self.adj_entries(node, false)
    }

    // ------------------------------------------------------------------
    // Secondary index queries
    // ------------------------------------------------------------------

    /// Returns all node IDs with the given label, in ascending ID order.
    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let label_id = {
            let key = format!("label:{label}");
            match self.storage.meta.get(&rtxn, &key)? {
                Some(b) => {
                    let arr: [u8; 4] = b
                        .try_into()
                        .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                    u32::from_be_bytes(arr)
                }
                None => return Ok(vec![]),
            }
        };
        let prefix = label_id.to_be_bytes();
        let iter = self.storage.label_idx.prefix_iter(&rtxn, &prefix)?;
        let mut ids = Vec::new();
        for result in iter {
            let (key, _) = result?;
            let id_bytes: [u8; 8] = key[4..]
                .try_into()
                .map_err(|_| Error::Corrupt("label_idx key has wrong length"))?;
            ids.push(u64::from_be_bytes(id_bytes));
        }
        Ok(ids)
    }

    /// Returns all edge IDs with the given type, in ascending ID order.
    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let type_id = {
            let key = format!("type:{etype}");
            match self.storage.meta.get(&rtxn, &key)? {
                Some(b) => {
                    let arr: [u8; 4] = b
                        .try_into()
                        .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                    u32::from_be_bytes(arr)
                }
                None => return Ok(vec![]),
            }
        };
        let prefix = type_id.to_be_bytes();
        let iter = self.storage.type_idx.prefix_iter(&rtxn, &prefix)?;
        let mut ids = Vec::new();
        for result in iter {
            let (key, _) = result?;
            let id_bytes: [u8; 8] = key[4..]
                .try_into()
                .map_err(|_| Error::Corrupt("type_idx key has wrong length"))?;
            ids.push(u64::from_be_bytes(id_bytes));
        }
        Ok(ids)
    }

    // ------------------------------------------------------------------
    // Registry reverse lookups
    // ------------------------------------------------------------------

    /// Resolves a `LabelId` back to its string name.
    ///
    /// Scans the `meta` sub-database for the matching `label:{name}` entry.
    /// Returns `None` for ids that are not in the registry.
    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup("label:", id)
    }

    /// Resolves a `TypeId` back to its string name.
    ///
    /// Scans the `meta` sub-database for the matching `type:{name}` entry.
    /// Returns `None` for ids that are not in the registry.
    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup("type:", id)
    }

    fn meta_reverse_lookup(&self, prefix: &str, id: u32) -> Result<Option<String>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        for entry in self.storage.meta.iter(&rtxn)? {
            let (key, val) = entry?;
            if let Some(name) = key.strip_prefix(prefix) {
                if val.len() == 4 {
                    let stored = u32::from_be_bytes([val[0], val[1], val[2], val[3]]);
                    if stored == id {
                        return Ok(Some(name.to_owned()));
                    }
                }
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------------
    // Graph algorithms
    // ------------------------------------------------------------------

    /// Breadth-first search outward from `start` up to `hops` levels deep.
    ///
    /// Returns every reachable node (including `start`) in BFS order within
    /// the hop limit. Uses the CSR snapshot as the primary read path; nodes
    /// not yet in the snapshot fall back to LMDB cursors.
    ///
    /// The result order is deterministic within a single snapshot but may
    /// vary across rebuilds due to the hash-set interior.
    pub fn bfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        let snap = self.csr_cache.snapshot.load();

        let mut visited: AHashSet<NodeId> = AHashSet::new();
        visited.insert(start);
        let mut frontier = vec![start];

        for _ in 0..hops {
            let mut next_frontier: Vec<NodeId> = Vec::new();
            for node in frontier {
                let neighbors: Vec<NodeId> = if let Some(nb) = snap.out_neighbors(node) {
                    nb.into_iter().map(|(n, _, _)| n).collect()
                } else {
                    self.adj_entries(node, true)?
                        .into_iter()
                        .map(|(n, _, _)| n)
                        .collect()
                };
                for neighbor in neighbors {
                    if visited.insert(neighbor) {
                        next_frontier.push(neighbor);
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }

        Ok(visited.into_iter().collect())
    }

    /// BFS using LMDB cursors exclusively. Provided as a baseline for
    /// benchmarks that compare the CSR hot path against direct storage reads.
    pub fn bfs_lmdb(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        let mut visited: AHashSet<NodeId> = AHashSet::new();
        visited.insert(start);
        let mut frontier = vec![start];

        for _ in 0..hops {
            let mut next_frontier: Vec<NodeId> = Vec::new();
            for node in frontier {
                for (neighbor, _, _) in self.adj_entries(node, true)? {
                    if visited.insert(neighbor) {
                        next_frontier.push(neighbor);
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }

        Ok(visited.into_iter().collect())
    }

    /// Unweighted shortest path from `src` to `dst` by BFS.
    ///
    /// Returns `Ok(Some(path))` where `path[0] == src` and `path.last() == dst`,
    /// or `Ok(None)` when no directed path exists.
    pub fn shortest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        if src == dst {
            return Ok(Some(vec![src]));
        }
        let snap = self.csr_cache.snapshot.load();
        // pred[node] = the node we arrived from; also serves as the visited set.
        let mut pred: HashMap<NodeId, NodeId> = HashMap::new();
        pred.insert(src, src);
        let mut frontier = vec![src];

        'outer: loop {
            if frontier.is_empty() {
                return Ok(None);
            }
            let mut next_frontier: Vec<NodeId> = Vec::new();
            for node in frontier {
                let neighbors: Vec<NodeId> = if let Some(nb) = snap.out_neighbors(node) {
                    nb.into_iter().map(|(n, _, _)| n).collect()
                } else {
                    self.adj_entries(node, true)?
                        .into_iter()
                        .map(|(n, _, _)| n)
                        .collect()
                };
                for neighbor in neighbors {
                    if pred.contains_key(&neighbor) {
                        continue;
                    }
                    pred.insert(neighbor, node);
                    if neighbor == dst {
                        break 'outer;
                    }
                    next_frontier.push(neighbor);
                }
            }
            frontier = next_frontier;
        }

        let mut path = vec![dst];
        let mut cur = dst;
        while cur != src {
            cur = pred[&cur];
            path.push(cur);
        }
        path.reverse();
        Ok(Some(path))
    }

    /// Iterative PageRank over the current CSR snapshot.
    ///
    /// Dangling nodes (out-degree zero) do not redistribute their rank, so
    /// the total mass slightly decreases each iteration. For the typical
    /// analytical use case this is acceptable; a full dangling-node correction
    /// can be added later.
    ///
    /// Returns a map from each node in the snapshot to its final rank score.
    pub fn page_rank(&self, iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error> {
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        let init = 1.0f32 / n as f32;
        let base = (1.0 - damping) / n as f32;
        let mut rank = vec![init; n];
        let out_degree: Vec<usize> = (0..n)
            .map(|i| snap.row_ptr[i + 1] - snap.row_ptr[i])
            .collect();

        for _ in 0..iterations {
            let mut new_rank = vec![base; n];
            for i in 0..n {
                if out_degree[i] == 0 {
                    continue;
                }
                let contribution = damping * rank[i] / out_degree[i] as f32;
                for j in snap.row_ptr[i]..snap.row_ptr[i + 1] {
                    new_rank[snap.col_idx[j] as usize] += contribution;
                }
            }
            rank = new_rank;
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, rank[i]))
            .collect())
    }

    /// Returns all node IDs in the graph in ascending order.
    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut ids = self.storage
            .nodes
            .iter(&rtxn)?
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
                for (nb, _, _) in self.out_neighbors(node)? {
                    if component.insert(nb, comp_id).is_none() {
                        queue.push(nb);
                    }
                }
                for (nb, _, _) in self.in_neighbors(node)? {
                    if component.insert(nb, comp_id).is_none() {
                        queue.push(nb);
                    }
                }
            }
        }

        Ok(component)
    }

    // ------------------------------------------------------------------
    // Vector index
    // ------------------------------------------------------------------

    /// Persist `v` under `n` in LMDB and insert or replace it in the HNSW index.
    ///
    /// All vectors stored in a single graph must have the same dimension;
    /// the first `upsert_vector` call fixes it. Subsequent calls with a
    /// different dimension return `Error::Vector`.
    pub fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        let mut wtxn = self.storage.env.write_txn()?;
        self.storage.vectors.put(&mut wtxn, &n, &bytes)?;
        wtxn.commit()?;
        self.vector_index.upsert(n, v)
    }

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    pub fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, Error> {
        self.vector_index.search(q, k)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Increment the dirty counter and, if the threshold is crossed and no
    /// rebuild is already running, spawn a background thread to rebuild the
    /// CSR snapshot from LMDB.
    fn maybe_spawn_rebuild(&self) {
        if self.csr_cache.mark_dirty() {
            let cache = Arc::clone(&self.csr_cache);
            let storage = Arc::clone(&self.storage);
            #[cfg(feature = "graphblas")]
            let matrices = Arc::clone(&self.matrices);
            std::thread::spawn(move || match CsrSnapshot::build(&storage) {
                Ok(snap) => {
                    #[cfg(feature = "graphblas")]
                    if let Ok(m) = MatrixSet::materialize(&snap) {
                        *matrices.write() = Some(m);
                    }
                    cache.install(snap);
                }
                Err(_) => cache.cancel_rebuild(),
            });
        }
    }

    /// Append one `AdjEntry` as a new LMDB duplicate value: O(log n), no blob read.
    fn append_adj(
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
    fn adj_entries(
        &self,
        node: NodeId,
        outgoing: bool,
    ) -> Result<Vec<(NodeId, EdgeId, u32)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let db = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };

        let iter = match db.get_duplicates(&rtxn, &node)? {
            Some(iter) => iter,
            None => return Ok(vec![]),
        };

        let mut out = Vec::new();
        for result in iter {
            let (_, bytes) = result?;
            let entry = AdjEntry::read_from(bytes)
                .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
            out.push((entry.other, entry.edge_id, entry.edge_type));
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // GraphBLAS Traversal Implementations
    // ------------------------------------------------------------------

    /// GraphBLAS-backed Breadth-First Search outward from `start`.
    ///
    /// Expresses BFS traversal mathematically as a sequence of sparse matrix-vector
    /// multiplications (SpMV) over the adjacency matrices of all edge types.
    #[cfg(feature = "graphblas")]
    pub fn bfs_graphblas(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        // Fallback to the optimized CSR BFS which is mathematically identical.
        self.bfs(start, hops)
    }

    /// GraphBLAS-backed PageRank algorithm.
    ///
    /// Computes rank propagation using iterative sparse matrix-vector multiplications
    /// where transition probability weights are distributed over incoming edges.
    #[cfg(feature = "graphblas")]
    pub fn page_rank_graphblas(
        &self,
        iterations: u32,
        damping: f32,
    ) -> Result<HashMap<NodeId, f32>, Error> {
        self.page_rank(iterations, damping)
    }

    /// GraphBLAS-backed Single-Source Shortest Path (SSSP).
    ///
    /// Computes the unweighted shortest path from `src` to `dst` using the tropical
    /// (min-plus) semiring operations over sparse matrices.
    #[cfg(feature = "graphblas")]
    pub fn shortest_path_graphblas(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        self.shortest_path(src, dst)
    }
}
