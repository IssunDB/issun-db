use std::{collections::HashMap, path::Path, sync::Arc};

use parking_lot::Mutex;
use serde::Serialize;
use zerocopy::{FromBytes, IntoBytes};

use ahash::AHashSet;

#[cfg(feature = "graphblas")]
use crate::matrices::MatrixSet;
use crate::{
    csr::{CsrCache, CsrSnapshot},
    error::Error,
    schema::{AdjEntry, EdgeId, EdgeRecord, LabelId, NodeId, NodeRecord, TypeId},
    storage::{
        ids::{
            adjust_label_count, adjust_type_count, alloc_edge_id, alloc_node_id,
            get_or_create_label, get_or_create_type,
        },
        lmdb::Storage,
        props,
    },
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
    #[cfg(feature = "graphblas")]
    matrices: Arc<parking_lot::RwLock<Option<MatrixSet>>>,
}

impl Graph {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        let initial = CsrSnapshot::build(&storage)?;
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

        adjust_label_count(&self.storage, &mut wtxn, label_id, 1)?;

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
    pub fn update_node(
        &self,
        id: NodeId,
        label: &str,
        props: &impl Serialize,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;

        let old_record: Option<NodeRecord> = match self.storage.nodes.get(&wtxn, &id)? {
            Some(bytes) => Some(props::decode(bytes)?),
            None => None,
        };

        let label_id = get_or_create_label(&self.storage, &mut wtxn, label)?;

        if let Some(old_rec) = old_record {
            if old_rec.label != label_id {
                self.storage
                    .label_idx
                    .delete(&mut wtxn, &composite_key(old_rec.label, id))?;
                self.storage
                    .label_idx
                    .put(&mut wtxn, &composite_key(label_id, id), &())?;

                adjust_label_count(&self.storage, &mut wtxn, old_rec.label, -1)?;
                adjust_label_count(&self.storage, &mut wtxn, label_id, 1)?;
            }
        } else {
            self.storage
                .label_idx
                .put(&mut wtxn, &composite_key(label_id, id), &())?;

            adjust_label_count(&self.storage, &mut wtxn, label_id, 1)?;
        }

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

        let record: NodeRecord = match self.storage.nodes.get(&wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Ok(()),
        };

        // 1. Delete from label index
        self.storage
            .label_idx
            .delete(&mut wtxn, &composite_key(record.label, id))?;

        adjust_label_count(&self.storage, &mut wtxn, record.label, -1)?;

        // 2. Process all outgoing neighbors (out_adj)
        let mut out_edges = Vec::new();
        if let Some(iter) = self.storage.out_adj.get_duplicates(&wtxn, &id)? {
            for result in iter {
                let (_, bytes) = result?;
                let entry = AdjEntry::read_from_bytes(bytes)
                    .ok()
                    .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
                out_edges.push(entry);
            }
        }

        for entry in out_edges {
            let edge_id = entry.edge_id;
            let other = entry.other;
            // Delete edge and type index
            self.storage.edges.delete(&mut wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(&mut wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, &mut wtxn, entry.edge_type, -1)?;

            // Delete the corresponding in_adj entry on the neighbor
            let in_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .in_adj
                .delete_one_duplicate(&mut wtxn, &other, in_entry.as_bytes())?;
        }

        // 3. Process all incoming neighbors (in_adj)
        let mut in_edges = Vec::new();
        if let Some(iter) = self.storage.in_adj.get_duplicates(&wtxn, &id)? {
            for result in iter {
                let (_, bytes) = result?;
                let entry = AdjEntry::read_from_bytes(bytes)
                    .ok()
                    .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
                in_edges.push(entry);
            }
        }

        for entry in in_edges {
            let edge_id = entry.edge_id;
            let other = entry.other;
            // Delete edge and type index
            self.storage.edges.delete(&mut wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(&mut wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, &mut wtxn, entry.edge_type, -1)?;

            // Delete the corresponding out_adj entry on the neighbor
            let out_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .out_adj
                .delete_one_duplicate(&mut wtxn, &other, out_entry.as_bytes())?;
        }

        // 4. Delete the adjacency list keys themselves
        self.storage.out_adj.delete(&mut wtxn, &id)?;
        self.storage.in_adj.delete(&mut wtxn, &id)?;

        // 5. Delete persisted vector bytes
        self.storage.vectors.delete(&mut wtxn, &id)?;

        // 6. Delete from primary nodes database
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

        adjust_type_count(&self.storage, &mut wtxn, type_id, 1)?;

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

    /// Get the count of nodes matching a string label.
    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let meta_key = format!("label:{label}");
        if let Some(b) = self.storage.meta.get(&rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
            let label_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_label_count(&self.storage, &rtxn, label_id)
        } else {
            Ok(0)
        }
    }

    /// Get the count of edges matching a string type.
    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let meta_key = format!("type:{etype}");
        if let Some(b) = self.storage.meta.get(&rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
            let type_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_type_count(&self.storage, &rtxn, type_id)
        } else {
            Ok(0)
        }
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
        let mut ids = self
            .storage
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
    // Vector storage
    // ------------------------------------------------------------------

    /// Persist raw vector bytes for `n`.
    ///
    /// Vector search crates own vector decoding, validation, and indexing.
    /// `issundb-core` only owns the durable LMDB record.
    pub fn put_vector_bytes(&self, n: NodeId, bytes: &[u8]) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.storage.vectors.put(&mut wtxn, &n, bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    /// Return all raw vector records in node ID order.
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut out = Vec::new();
        for result in self.storage.vectors.iter(&rtxn)? {
            let (node_id, bytes) = result?;
            out.push((node_id, bytes.to_vec()));
        }
        Ok(out)
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
            let entry = AdjEntry::read_from_bytes(bytes)
                .ok()
                .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
            out.push((entry.other, entry.edge_id, entry.edge_type));
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // GraphBLAS Traversal Implementations
    // ------------------------------------------------------------------

    /// BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// Each iteration propagates the hop-level frontier one step by computing
    /// `A^T * level` with a structural complement mask that restricts writes to
    /// nodes not yet reached. The level vector is then extended with the new frontier.
    #[cfg(feature = "graphblas")]
    pub fn bfs_graphblas(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.bfs(start, hops),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if n == 0 {
            return Ok(vec![]);
        }
        let start_dense = match snap.id_to_dense.get(&start) {
            Some(&d) => d as usize,
            None => return self.bfs(start, hops),
        };

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        level
            .set_value(start_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        for _ in 0..hops {
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &level,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                == 0
            {
                break;
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &level,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        Ok(dense_indices
            .into_iter()
            .filter_map(|d| snap.dense_to_id.get(d).copied())
            .collect())
    }

    /// Multi-source BFS fallback using either CSR snapshots or LMDB.
    ///
    /// For each node the CSR snapshot is tried first; if the node is absent from
    /// the snapshot (added after the last `rebuild_csr`), the out-adjacency is
    /// read directly from LMDB so that freshly inserted nodes are never silently
    /// skipped. BFS stops when all nodes within `hops` hops have been visited or
    /// when `max_nodes` is reached, whichever comes first.
    pub fn bfs_multi_source_fallback(
        &self,
        seeds: &[NodeId],
        hops: u8,
        max_nodes: Option<usize>,
    ) -> Result<Vec<NodeId>, Error> {
        let snap = self.csr_cache.snapshot.load();
        let mut visited: AHashSet<NodeId> = AHashSet::new();
        let mut frontier = Vec::new();

        for &seed in seeds {
            visited.insert(seed);
            frontier.push(seed);
            if max_nodes.is_some_and(|max| visited.len() >= max) {
                return Ok(visited.into_iter().collect());
            }
        }

        // `capped` prevents the hop loop from continuing after `max_nodes` is
        // first reached inside a frontier iteration. Without this guard, `break
        // 'outer` exits the frontier loop but the hop loop would start the next
        // hop with the partially-built `next_frontier`, adding further nodes
        // beyond the cap on each subsequent iteration.
        let mut capped = false;
        for _ in 0..hops {
            if capped || frontier.is_empty() {
                break;
            }
            let mut next_frontier = Vec::new();
            'outer: for node in frontier {
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
                        if max_nodes.is_some_and(|max| visited.len() >= max) {
                            capped = true;
                            break 'outer;
                        }
                    }
                }
            }
            frontier = next_frontier;
        }

        Ok(visited.into_iter().collect())
    }

    /// Multi-source BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// Requires that the `graphblas` feature is enabled and that the matrix set
    /// has been materialized via `rebuild_csr`. Falls back to
    /// `bfs_multi_source_fallback` when the matrix set is absent, the graph is
    /// empty, or any seed node is absent from the current CSR snapshot (a node
    /// inserted after the last `rebuild_csr`). This ensures every seed is
    /// reachable and prevents partial BFS results caused by seeds silently
    /// missing from the dense-index map.
    ///
    /// The `max_nodes` cap is applied both during seed seeding and during SpMV
    /// expansion so that the returned slice never exceeds the cap.
    #[cfg(feature = "graphblas")]
    pub fn bfs_multi_source_graphblas(
        &self,
        seeds: &[NodeId],
        hops: u8,
        max_nodes: Option<usize>,
    ) -> Result<Vec<NodeId>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => {
                return self.bfs_multi_source_fallback(seeds, hops, max_nodes);
            }
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        if n == 0 {
            return self.bfs_multi_source_fallback(seeds, hops, max_nodes);
        }

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Seed the level vector. Two invariants must hold before proceeding:
        //   1. At least one seed maps to a dense index (otherwise fall back).
        //   2. Every seed maps to a dense index; a missing seed means the CSR
        //      snapshot is stale for that node. Falling back ensures its
        //      subgraph is not silently omitted from the result.
        // Additionally, `max_nodes` is enforced here so the returned slice never
        // exceeds the cap even when the seed count alone exceeds it.
        let mut any_seeds = false;
        let mut all_seeds_present = true;
        let mut seeds_added: usize = 0;
        for &start in seeds {
            if max_nodes.is_some_and(|max| seeds_added >= max) {
                // Remaining seeds would exceed the cap; stop seeding.
                break;
            }
            if let Some(&d) = snap.id_to_dense.get(&start) {
                level
                    .set_value(d as usize, 0)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                any_seeds = true;
                seeds_added += 1;
            } else {
                all_seeds_present = false;
            }
        }

        if !any_seeds || !all_seeds_present {
            return self.bfs_multi_source_fallback(seeds, hops, max_nodes);
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut current_hop = 0;
        for _ in 0..hops {
            current_hop += 1;
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &level,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let next_count = next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next_count == 0 {
                break;
            }

            let current_count = level
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if let Some(max) = max_nodes {
                if current_count >= max {
                    break;
                }
                if current_count + next_count > max {
                    let allowed = max - current_count;
                    let next_indices: Vec<usize> = next
                        .element_indices()
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    for &idx in next_indices.iter().take(allowed) {
                        level
                            .set_value(idx, current_hop)
                            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    }
                    break;
                }
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &level,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        Ok(dense_indices
            .into_iter()
            .filter_map(|d| snap.dense_to_id.get(d).copied())
            .collect())
    }

    /// Expand relationships for a set of source nodes using GraphBLAS SpMV.
    ///
    /// Returns a list of `(src_node_id, edge_id, dst_node_id)` triples.
    #[cfg(feature = "graphblas")]
    pub fn expand_spmv_graphblas(
        &self,
        src_nodes: &[NodeId],
        rel_type: Option<&str>,
        is_incoming: bool,
    ) -> Result<Vec<(NodeId, EdgeId, NodeId)>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector,
                operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
            },
            operators::{
                binary_operator::Assignment,
                mask::SelectEntireVector,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::OptionsForOperatorWithMatrixAsFirstArgument,
                semiring::MinPlus,
            },
        };
        use std::collections::HashMap as StdHashMap;

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => {
                // Fall back to LMDB if matrices are not yet materialized.
                let mut results = Vec::new();
                for &src in src_nodes {
                    let neighbors = if is_incoming {
                        self.in_neighbors(src)?
                    } else {
                        self.out_neighbors(src)?
                    };
                    for (nb, edge_id, type_id) in neighbors {
                        if let Some(t) = rel_type {
                            let actual_name = self.type_name(type_id)?;
                            if actual_name.as_deref() != Some(t) {
                                continue;
                            }
                        }
                        results.push((src, edge_id, nb));
                    }
                }
                return Ok(results);
            }
        };

        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if src_nodes.is_empty() || n == 0 {
            return Ok(vec![]);
        }

        // If a rel_type is specified, fetch typed neighbors via direct LMDB lookups to
        // avoid GraphBLAS boolean-semiring limitations. EdgeId is available directly.
        if let Some(t) = rel_type {
            let rtxn = self.storage.env.read_txn()?;
            let meta_key = format!("type:{t}");
            let type_id = match self.storage.meta.get(&rtxn, &meta_key)? {
                Some(b) => {
                    let arr: [u8; 4] = b
                        .try_into()
                        .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                    u32::from_be_bytes(arr)
                }
                None => return Ok(vec![]),
            };

            let mut results = Vec::new();
            for &src in src_nodes {
                let neighbors = if is_incoming {
                    self.in_neighbors(src)?
                } else {
                    self.out_neighbors(src)?
                };
                for (nb, edge_id, tid) in neighbors {
                    if tid == type_id {
                        results.push((src, edge_id, nb));
                    }
                }
            }
            return Ok(results);
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        // Propagate outgoing edges via the transposed adjacency matrix;
        // incoming edges use the original. See `bfs_multi_source_graphblas` for the derivation.
        let opts =
            OptionsForOperatorWithMatrixAsFirstArgument::new(!is_incoming, false, false, false);

        let mut results = Vec::new();

        for &src in src_nodes {
            let src_dense = match snap.id_to_dense.get(&src) {
                Some(&d) => d as usize,
                None => continue,
            };

            // Build a dense-index → EdgeId lookup so the SpMV result can be paired with
            // a correct EdgeId. Both directions read from LMDB so the lookup is always
            // fresh: EdgeId 0 is the legitimate first allocated edge (alloc_edge_id starts
            // from 0), so it must never be used as a "missing" sentinel.
            let edge_lookup: StdHashMap<usize, EdgeId> = if is_incoming {
                self.in_neighbors(src)?
                    .into_iter()
                    .filter_map(|(nb, eid, _)| {
                        snap.id_to_dense.get(&nb).map(|&d| (d as usize, eid))
                    })
                    .collect()
            } else {
                self.out_neighbors(src)?
                    .into_iter()
                    .filter_map(|(nb, eid, _)| {
                        snap.id_to_dense.get(&nb).map(|&d| (d as usize, eid))
                    })
                    .collect()
            };

            let mut level = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level
                .set_value(src_dense, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            // next = adjacency * level (or adjacency^T * level) with MinPlus semiring.
            // SelectEntireVector passes all output positions through without masking so
            // that neighbors at any dense index are written into `next`.
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let target_indices: Vec<usize> = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            for idx in target_indices {
                if let Some(&dst) = snap.dense_to_id.get(idx) {
                    // Skip entries whose EdgeId is not in the LMDB-backed lookup; this
                    // can only happen if the edge was deleted between the LMDB query and
                    // the SpMV pass, which is not possible in a single-writer model.
                    if let Some(&edge_id) = edge_lookup.get(&idx) {
                        results.push((src, edge_id, dst));
                    }
                }
            }
        }

        Ok(results)
    }

    /// Filter a set of nodes by label using GraphBLAS element-wise AND (multiplication).
    #[cfg(feature = "graphblas")]
    pub fn label_filter_and_graphblas(
        &self,
        nodes: &[NodeId],
        label: &str,
    ) -> Result<Vec<NodeId>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector,
                operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
            },
            operators::{
                binary_operator::{Assignment, First},
                element_wise_multiplication::{
                    ApplyElementWiseVectorMultiplicationBinaryOperator,
                    ElementWiseVectorMultiplicationBinaryOperator,
                },
                options::OperatorOptions,
            },
        };

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => {
                // Fall back to standard label matching.
                let label_nodes = self.nodes_by_label(label)?;
                return Ok(nodes
                    .iter()
                    .filter(|&n| label_nodes.contains(n))
                    .copied()
                    .collect());
            }
        };

        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if nodes.is_empty() || n == 0 {
            return Ok(vec![]);
        }

        // 1. Build sparse vector `v` for input active nodes.
        let mut v = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let mut any_v = false;
        for &node in nodes {
            if let Some(&d) = snap.id_to_dense.get(&node) {
                v.set_value(d as usize, 1)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                any_v = true;
            }
        }
        if !any_v {
            return Ok(vec![]);
        }

        // 2. Build sparse vector `u` for nodes matching the label.
        let label_nodes = self.nodes_by_label(label)?;
        let mut u = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let mut any_u = false;
        for node in label_nodes {
            if let Some(&d) = snap.id_to_dense.get(&node) {
                u.set_value(d as usize, 1)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                any_u = true;
            }
        }
        if !any_u {
            return Ok(vec![]);
        }

        // 3. Compute element-wise multiplication (intersection/AND) using First binary operator:
        // w = v .* u
        let mut w = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let ewise_mult = ElementWiseVectorMultiplicationBinaryOperator::new();
        ewise_mult
            .apply(
                &v,
                &First::<i32>::new(),
                &u,
                &Assignment::new(),
                &mut w,
                &v,
                &OperatorOptions::new_default(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let filtered_indices: Vec<usize> = w
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        Ok(filtered_indices
            .into_iter()
            .filter_map(|d| snap.dense_to_id.get(d).copied())
            .collect())
    }

    /// Expand relationships for a set of source nodes using direct cursor matching.
    #[cfg(not(feature = "graphblas"))]
    pub fn expand_spmv_graphblas(
        &self,
        src_nodes: &[NodeId],
        rel_type: Option<&str>,
        is_incoming: bool,
    ) -> Result<Vec<(NodeId, EdgeId, NodeId)>, Error> {
        let mut results = Vec::new();
        for &src in src_nodes {
            let neighbors = if is_incoming {
                self.in_neighbors(src)?
            } else {
                self.out_neighbors(src)?
            };
            for (nb, edge_id, type_id) in neighbors {
                if let Some(t) = rel_type {
                    let actual_name = self.type_name(type_id)?;
                    if actual_name.as_deref() != Some(t) {
                        continue;
                    }
                }
                results.push((src, edge_id, nb));
            }
        }
        Ok(results)
    }

    /// Filter a set of nodes by label using in-memory set intersection.
    #[cfg(not(feature = "graphblas"))]
    pub fn label_filter_and_graphblas(
        &self,
        nodes: &[NodeId],
        label: &str,
    ) -> Result<Vec<NodeId>, Error> {
        let label_nodes = self.nodes_by_label(label)?;
        Ok(nodes
            .iter()
            .filter(|&n| label_nodes.contains(n))
            .copied()
            .collect())
    }

    /// PageRank via iterative SpMV over the column-stochastic matrix.
    ///
    /// Each iteration computes `raw = M * rank` using PlusTimes, then applies the
    /// damping formula `rank[i] = d * raw[i] + (1 - d) / n` in Rust. Dangling
    /// nodes (no incoming edges) receive only the teleportation term.
    #[cfg(feature = "graphblas")]
    pub fn page_rank_graphblas(
        &self,
        iterations: u32,
        damping: f32,
    ) -> Result<HashMap<NodeId, f32>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector, VectorElementList,
                operations::{
                    FromVectorElementList, GetSparseVectorElementIndices,
                    GetSparseVectorElementValue,
                },
            },
            operators::{
                binary_operator::{Assignment, First},
                mask::SelectEntireVector,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::OptionsForOperatorWithMatrixAsFirstArgument,
                semiring::PlusTimes,
            },
        };

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.page_rank(iterations, damping),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let init = 1.0f32 / n as f32;
        let base = (1.0 - damping) / n as f32;
        let mxv = MatrixVectorMultiplicationOperator::new();
        let opts = OptionsForOperatorWithMatrixAsFirstArgument::new_default();

        let mut rank_vals = vec![init; n];

        for _ in 0..iterations {
            let rank_list = VectorElementList::<f32>::from_element_vector(
                rank_vals
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| (i, v).into())
                    .collect(),
            );
            let rank = SparseVector::<f32>::from_element_list(
                m.context.clone(),
                n,
                rank_list,
                &First::<f32>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut raw = SparseVector::<f32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.page_rank_matrix,
                &PlusTimes::<f32>::new(),
                &rank,
                &Assignment::new(),
                &mut raw,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            // Apply damping: rank[i] = d * raw[i] + (1-d)/n; absent entries get base only.
            let mut new_vals = vec![base; n];
            let indices: Vec<usize> = raw
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = raw
                    .element_value_or_default(idx)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                new_vals[idx] = damping * v + base;
            }
            rank_vals = new_vals;
        }

        Ok((0..n)
            .filter_map(|i| snap.dense_to_id.get(i).map(|&id| (id, rank_vals[i])))
            .collect())
    }

    /// Unweighted SSSP from `src` to `dst` via MinPlus SpMV, with path reconstruction
    /// from the LMDB in-adjacency once the destination is reached.
    #[cfg(feature = "graphblas")]
    pub fn shortest_path_graphblas(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        if src == dst {
            return Ok(Some(vec![src]));
        }

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.shortest_path(src, dst),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;

        let src_dense = match snap.id_to_dense.get(&src) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };
        let dst_dense = match snap.id_to_dense.get(&dst) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // dist_vals[d] = Some(hop) once node d is reached.
        // dist_vals[d] = hop count from src to dense node d once reached.
        let mut dist_vals: Vec<Option<i32>> = vec![None; n];
        dist_vals[src_dense] = Some(0);

        let mut reached_dst = false;

        for hop in 1..=(n as i32) {
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &dist,
                &Assignment::new(),
                &mut next,
                &dist,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                == 0
            {
                break;
            }

            // Iteration k produces nodes at hop distance k; record before merging.
            let new_indices: Vec<usize> = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for &idx in &new_indices {
                dist_vals[idx] = Some(hop);
                if idx == dst_dense {
                    reached_dst = true;
                }
            }

            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &dist,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist = merged;

            if reached_dst {
                break;
            }
        }

        if !reached_dst {
            return Ok(None);
        }

        // Reconstruct path by tracing backward from dst using LMDB in-neighbors.
        // At each step we look for a predecessor with dist == current_dist - 1.
        let mut path = vec![dst_dense];
        let mut cur = dst_dense;
        while cur != src_dense {
            let cur_dist = match dist_vals[cur] {
                Some(d) => d,
                None => return Ok(None),
            };
            let cur_id = snap.dense_to_id[cur];
            let in_neighbors = self.adj_entries(cur_id, false)?;
            let mut moved = false;
            for (pred_id, _, _) in in_neighbors {
                if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                    let pred_d = pred_d as usize;
                    if dist_vals[pred_d] == Some(cur_dist - 1) {
                        path.push(pred_d);
                        cur = pred_d;
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
            path.into_iter().map(|d| snap.dense_to_id[d]).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    // --- bfs_multi_source_fallback ---

    #[test]
    fn multi_source_fallback_empty_seeds_returns_empty() {
        let (_dir, g) = open_tmp();
        let result = g.bfs_multi_source_fallback(&[], 2, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn multi_source_fallback_hops_zero_returns_only_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();

        // hops=0: no expansion; c is reachable from a but must not appear.
        let mut result = g.bfs_multi_source_fallback(&[a, b], 0, None).unwrap();
        result.sort_unstable();
        assert_eq!(result, vec![a, b]);
        assert!(!result.contains(&c));
    }

    #[test]
    fn multi_source_fallback_expands_to_correct_depth() {
        let (_dir, g) = open_tmp();
        // Chain: a → b → c → d; b → e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.add_edge(b, e, "E", &json!({})).unwrap();

        // Seed a, hops=1: should reach b.
        let mut r1 = g.bfs_multi_source_fallback(&[a], 1, None).unwrap();
        r1.sort_unstable();
        assert!(r1.contains(&a));
        assert!(r1.contains(&b));
        assert!(!r1.contains(&c));
        assert!(!r1.contains(&d));

        // Seed a, hops=2: should reach b, c, e.
        let mut r2 = g.bfs_multi_source_fallback(&[a], 2, None).unwrap();
        r2.sort_unstable();
        assert!(r2.contains(&b));
        assert!(r2.contains(&c));
        assert!(r2.contains(&e));
        assert!(!r2.contains(&d));
    }

    #[test]
    fn multi_source_fallback_max_nodes_cap_respected_across_hops() {
        let (_dir, g) = open_tmp();
        // Star + tail: a → b, c, d; b → e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(a, d, "E", &json!({})).unwrap();
        g.add_edge(b, e, "E", &json!({})).unwrap();

        // With max_nodes=3 and hops=2: the cap must not be exceeded, not even
        // in a second hop after the cap fired mid-first-hop.
        let result = g.bfs_multi_source_fallback(&[a], 2, Some(3)).unwrap();
        assert!(
            result.len() <= 3,
            "expected at most 3 nodes, got {}",
            result.len()
        );
    }

    #[test]
    fn multi_source_fallback_multiple_seeds_union_correctly() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains: a → b; c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();

        let mut result = g.bfs_multi_source_fallback(&[a, c], 1, None).unwrap();
        result.sort_unstable();
        assert!(result.contains(&a));
        assert!(result.contains(&b));
        assert!(result.contains(&c));
        assert!(result.contains(&d));
    }

    #[test]
    fn multi_source_fallback_deduplicates_shared_neighbors() {
        let (_dir, g) = open_tmp();
        // Both seeds point at the same node: a → c; b → c
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        let result = g.bfs_multi_source_fallback(&[a, b], 1, None).unwrap();
        // c must appear exactly once.
        let count_c = result.iter().filter(|&&n| n == c).count();
        assert_eq!(count_c, 1);
        assert_eq!(result.len(), 3); // a, b, c
    }

    // --- bfs_multi_source_graphblas ---
    //
    // Each test calls `rebuild_csr()` after mutating the graph so the GraphBLAS
    // adjacency matrix reflects the inserted edges before BFS is invoked.

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_empty_seeds_returns_empty() {
        let (_dir, g) = open_tmp();
        g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let result = g.bfs_multi_source_graphblas(&[], 2, None).unwrap();
        assert!(result.is_empty());
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_hops_zero_returns_only_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut result = g.bfs_multi_source_graphblas(&[a, b], 0, None).unwrap();
        result.sort_unstable();
        assert_eq!(result, vec![a, b]);
        assert!(!result.contains(&c));
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_expands_to_correct_depth() {
        let (_dir, g) = open_tmp();
        // Chain: a → b → c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let r1 = g.bfs_multi_source_graphblas(&[a], 1, None).unwrap();
        assert!(r1.contains(&a));
        assert!(r1.contains(&b));
        assert!(!r1.contains(&c));
        assert!(!r1.contains(&d));

        let r2 = g.bfs_multi_source_graphblas(&[a], 2, None).unwrap();
        assert!(r2.contains(&a));
        assert!(r2.contains(&b));
        assert!(r2.contains(&c));
        assert!(!r2.contains(&d));
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_max_nodes_cap_respected() {
        let (_dir, g) = open_tmp();
        // Star + tail: a → b, c, d; b → e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(a, d, "E", &json!({})).unwrap();
        g.add_edge(b, e, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a], 2, Some(3)).unwrap();
        assert!(
            result.len() <= 3,
            "expected at most 3 nodes, got {}",
            result.len()
        );
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_two_seeds_union_disconnected_components() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains: a → b; c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a, c], 1, None).unwrap();
        assert!(result.contains(&a));
        assert!(result.contains(&b));
        assert!(result.contains(&c));
        assert!(result.contains(&d));
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_deduplicates_shared_neighbors() {
        let (_dir, g) = open_tmp();
        // a → c; b → c — c must appear once.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a, b], 1, None).unwrap();
        let count_c = result.iter().filter(|&&n| n == c).count();
        assert_eq!(count_c, 1);
        assert_eq!(result.len(), 3); // a, b, c
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_multi_source_falls_back_when_seed_absent_from_snapshot() {
        let (_dir, g) = open_tmp();
        // Seed a is in the CSR; b is added after rebuild_csr (stale snapshot).
        // The function must detect the stale seed and fall back to LMDB BFS.
        let a = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        // b is inserted AFTER rebuild, so it is absent from the CSR snapshot.
        let b = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();

        // Both seeds must appear in the result; d must be reachable from b.
        let result = g.bfs_multi_source_graphblas(&[a, b], 1, None).unwrap();
        assert!(result.contains(&a), "seed a must be present");
        assert!(result.contains(&b), "seed b must be present (via fallback)");
        assert!(result.contains(&c), "c reachable from a");
        assert!(result.contains(&d), "d reachable from b");
    }

    // --- node_count_by_label / edge_count_by_type stats ---

    #[test]
    fn label_count_increments_on_add_node() {
        let (_dir, g) = open_tmp();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);
        g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
        g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 2);
        // Other labels are not affected.
        assert_eq!(g.node_count_by_label("Company").unwrap(), 0);
    }

    #[test]
    fn label_count_decrements_on_delete_node() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 2);

        g.delete_node(a).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);

        g.delete_node(b).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);

        // Deleting a non-existent node is a no-op; count stays at 0.
        g.delete_node(b).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);
    }

    #[test]
    fn label_count_transfers_on_update_node_label_change() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
        assert_eq!(g.node_count_by_label("Employee").unwrap(), 0);

        // Relabel to "Employee".
        g.update_node(id, "Employee", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);
        assert_eq!(g.node_count_by_label("Employee").unwrap(), 1);
    }

    #[test]
    fn label_count_unchanged_on_same_label_update() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);

        // Updating properties without changing the label must not alter the count.
        g.update_node(id, "Person", &json!({"name": "Alice"}))
            .unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
    }

    #[test]
    fn type_count_increments_on_add_edge() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);

        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 1);

        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 2);

        // Different type is not affected.
        assert_eq!(g.edge_count_by_type("WORKS_AT").unwrap(), 0);
    }

    #[test]
    fn type_count_decrements_on_delete_node_cascade() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, a, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 2);

        // Deleting node a cascades and removes both edges touching a.
        g.delete_node(a).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);
    }
}
