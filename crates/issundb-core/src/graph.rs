use std::{
    any::{Any, TypeId as StdTypeId},
    collections::HashMap,
    path::Path,
    sync::Arc,
};

use parking_lot::ReentrantMutex;
use serde::Serialize;
use tracing::instrument;
use zerocopy::{FromBytes, IntoBytes};

use ahash::{AHashMap, AHashSet};

use crate::matrices::MatrixSet;
use crate::{
    csr::{CsrCache, CsrSnapshot},
    error::Error,
    schema::{
        AdjEntry, DirectedNeighborEntry, EdgeId, EdgeRecord, LabelId, Language, NeighborEntry,
        NodeId, NodeRecord, PropKeyId, PropValue, TypeId, WeightedPath,
    },
    storage::{
        fts,
        ids::{
            adjust_label_count, adjust_type_count, alloc_edge_id, alloc_node_id, get_label,
            get_or_create_label, get_or_create_prop_key, get_or_create_type, get_prop_key,
            get_prop_key_name,
        },
        lmdb::Storage,
        props,
    },
};

/// The direction of edges to count for degree centrality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DegreeDirection {
    /// Count incoming edges only.
    In,
    /// Count outgoing edges only.
    Out,
    /// Count both incoming and outgoing edges.
    Both,
}

/// Builds a 12-byte composite key `(prefix u32 BE, id u64 BE)` for secondary index lookups.
fn composite_key(prefix: u32, id: u64) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&prefix.to_be_bytes());
    key[4..].copy_from_slice(&id.to_be_bytes());
    key
}

/// Encodes a JSON property value into a sortable byte representation for the index.
fn encode_property_value(val: &serde_json::Value) -> Option<Vec<u8>> {
    match val {
        serde_json::Value::Null => Some(vec![0x00]),
        serde_json::Value::Bool(false) => Some(vec![0x01]),
        serde_json::Value::Bool(true) => Some(vec![0x02]),
        serde_json::Value::Number(num) => {
            let float_val = num.as_f64()?;
            let bits = float_val.to_bits();
            let masked = if (bits & 0x8000_0000_0000_0000) != 0 {
                !bits
            } else {
                bits ^ 0x8000_0000_0000_0000
            };
            let mut buf = Vec::with_capacity(9);
            buf.push(0x03);
            buf.extend_from_slice(&masked.to_be_bytes());
            Some(buf)
        }
        serde_json::Value::String(s) => {
            let mut buf = Vec::with_capacity(1 + s.len() + 1);
            buf.push(0x04);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0x00);
            Some(buf)
        }
        _ => None, // Skip arrays and objects
    }
}

/// Decodes a sortable byte representation back into a JSON property value.
#[allow(dead_code)]
fn decode_property_value(bytes: &[u8]) -> Option<serde_json::Value> {
    if bytes.is_empty() {
        return None;
    }
    match bytes[0] {
        0x00 => Some(serde_json::Value::Null),
        0x01 => Some(serde_json::Value::Bool(false)),
        0x02 => Some(serde_json::Value::Bool(true)),
        0x03 => {
            if bytes.len() < 9 {
                return None;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&bytes[1..9]);
            let masked = u64::from_be_bytes(arr);
            let bits = if (masked & 0x8000_0000_0000_0000) == 0 {
                !masked
            } else {
                masked ^ 0x8000_0000_0000_0000
            };
            let float_val = f64::from_bits(bits);
            serde_json::Number::from_f64(float_val).map(serde_json::Value::Number)
        }
        0x04 => {
            let str_bytes = if bytes.ends_with(&[0x00]) {
                &bytes[1..bytes.len() - 1]
            } else {
                &bytes[1..]
            };
            String::from_utf8(str_bytes.to_vec())
                .ok()
                .map(serde_json::Value::String)
        }
        _ => None,
    }
}

/// Builds a composite key `(label_id, prop_key_id, encoded_val, node_id)` for node property index.
fn node_prop_index_key(
    label_id: LabelId,
    prop_key_id: PropKeyId,
    encoded_val: &[u8],
    node_id: NodeId,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 4 + encoded_val.len() + 8);
    key.extend_from_slice(&label_id.to_be_bytes());
    key.extend_from_slice(&prop_key_id.to_be_bytes());
    key.extend_from_slice(encoded_val);
    key.extend_from_slice(&node_id.to_be_bytes());
    key
}

/// Builds a composite key `(type_id, prop_key_id, encoded_val, edge_id)` for edge property index.
fn edge_prop_index_key(
    type_id: TypeId,
    prop_key_id: PropKeyId,
    encoded_val: &[u8],
    edge_id: EdgeId,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 4 + encoded_val.len() + 8);
    key.extend_from_slice(&type_id.to_be_bytes());
    key.extend_from_slice(&prop_key_id.to_be_bytes());
    key.extend_from_slice(encoded_val);
    key.extend_from_slice(&edge_id.to_be_bytes());
    key
}

/// Builds a composite key `(label_id, prop_key_id, term)` for FTS postings.
fn fts_postings_key(label_id: LabelId, prop_key_id: PropKeyId, term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + term.len());
    key.extend_from_slice(&label_id.to_be_bytes());
    key.extend_from_slice(&prop_key_id.to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key
}

/// Builds a 12-byte FTS posting value `(node_id, frequency)`.
fn fts_posting_val(node_id: NodeId, frequency: u32) -> [u8; 12] {
    let mut val = [0u8; 12];
    val[0..8].copy_from_slice(&node_id.to_be_bytes());
    val[8..12].copy_from_slice(&frequency.to_be_bytes());
    val
}

/// Parses a 12-byte FTS posting value into `(node_id, frequency)`.
fn parse_fts_posting_val(bytes: &[u8]) -> Result<(NodeId, u32), Error> {
    if bytes.len() != 12 {
        return Err(Error::Corrupt("fts posting value must be 12 bytes"));
    }
    let node_id = NodeId::from_be_bytes(
        bytes[0..8]
            .try_into()
            .map_err(|_| Error::Corrupt("fts posting: node_id slice wrong size"))?,
    );
    let frequency = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| Error::Corrupt("fts posting: frequency slice wrong size"))?,
    );
    Ok((node_id, frequency))
}

/// Builds a 16-byte FTS doc key `(label_id, prop_key_id, node_id)`.
fn fts_doc_key(label_id: LabelId, prop_key_id: PropKeyId, node_id: NodeId) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..4].copy_from_slice(&label_id.to_be_bytes());
    key[4..8].copy_from_slice(&prop_key_id.to_be_bytes());
    key[8..16].copy_from_slice(&node_id.to_be_bytes());
    key
}

/// Parses a 4-byte doc length value.
fn parse_fts_doc_val(bytes: &[u8]) -> Result<u32, Error> {
    if bytes.len() != 4 {
        return Err(Error::Corrupt("fts doc val must be 4 bytes"));
    }
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
        Error::Corrupt("fts doc val: slice wrong size")
    })?))
}

fn fts_stats_n_key(label_id: LabelId, prop_key_id: PropKeyId) -> String {
    format!("fts_stats:node:l:{label_id}:p:{prop_key_id}:N")
}

fn fts_stats_sum_dl_key(label_id: LabelId, prop_key_id: PropKeyId) -> String {
    format!("fts_stats:node:l:{label_id}:p:{prop_key_id}:sum_dl")
}

/// The graph database handle. Cheap to clone: all state is behind `Arc`.
#[derive(Clone)]
pub struct Graph {
    storage: Arc<Storage>,
    _write_lock: Arc<ReentrantMutex<()>>,
    csr_cache: Arc<CsrCache>,
    matrices: Arc<parking_lot::RwLock<Option<MatrixSet>>>,
    /// Type-erased extension cache. Higher-level crates use this to attach
    /// caches (e.g. HNSW vector index) to a Graph without creating a circular
    /// dependency. Keys are `std::any::TypeId`; values are `Arc<dyn Any + Send + Sync>`.
    pub extensions: Arc<parking_lot::Mutex<AHashMap<StdTypeId, Box<dyn Any + Send + Sync>>>>,
}

/// A read-only transaction on the graph.
pub struct ReadTxn<'a> {
    graph: &'a Graph,
    rtxn: heed::RoTxn<'a, heed::WithTls>,
}

/// A read-write transaction on the graph.
pub struct WriteTxn<'a> {
    graph: &'a Graph,
    wtxn: heed::RwTxn<'a>,
    mutations_count: usize,
}

impl Graph {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        let initial = CsrSnapshot::build(&storage)?;
        let storage = Arc::new(storage);
        let csr_cache = Arc::new(CsrCache::new(initial));
        let matrices = {
            let initial_snap = csr_cache.snapshot.load();
            let m = MatrixSet::materialize(&initial_snap)?;
            Arc::new(parking_lot::RwLock::new(Some(m)))
        };
        Ok(Self {
            storage,
            _write_lock: Arc::new(ReentrantMutex::new(())),
            csr_cache,
            matrices,
            extensions: Arc::new(parking_lot::Mutex::new(AHashMap::new())),
        })
    }

    /// Store an extension value (as `Arc`) keyed by its concrete type.
    /// Replaces any existing value of the same type.
    pub fn set_extension<T: Any + Send + Sync>(&self, val: Arc<T>) {
        self.extensions
            .lock()
            .insert(StdTypeId::of::<T>(), Box::new(val));
    }

    /// Retrieve an `Arc` to a previously stored extension value, or `None` if absent.
    pub fn get_extension<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.extensions
            .lock()
            .get(&StdTypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<Arc<T>>())
            .cloned()
    }

    /// Execute a read-only transaction inside a closure.
    pub fn view<F, T>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&ReadTxn) -> Result<T, Error>,
    {
        let rtxn = self.storage.env.read_txn()?;
        let txn = ReadTxn { graph: self, rtxn };
        f(&txn)
    }

    /// Execute a read-write transaction inside a closure.
    pub fn update<F, T>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut WriteTxn) -> Result<T, Error>,
    {
        let _guard = self._write_lock.lock();
        let wtxn = self.storage.env.write_txn()?;
        let mut txn = WriteTxn {
            graph: self,
            wtxn,
            mutations_count: 0,
        };
        match f(&mut txn) {
            Ok(val) => {
                txn.wtxn.commit()?;
                if txn.mutations_count > 0 {
                    self.maybe_spawn_rebuild_n(txn.mutations_count);
                }
                Ok(val)
            }
            Err(err) => {
                txn.wtxn.abort();
                Err(err)
            }
        }
    }

    /// Hold the write lock for the duration of `f`, executing `f` without
    /// starting an LMDB transaction. Use this to make a multi-step read-then-write
    /// sequence (such as MERGE) atomic with respect to other writers.
    pub fn with_write_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = self._write_lock.lock();
        f()
    }

    /// Synchronously rebuild the CSR snapshot from LMDB. Useful after bulk
    /// loads or when tests need a consistent read view before the threshold
    /// has been crossed.
    #[instrument(skip(self))]
    pub fn rebuild_csr(&self) -> Result<(), Error> {
        let snap = CsrSnapshot::build(&self.storage)?;
        let m = MatrixSet::materialize(&snap)?;
        *self.matrices.write() = Some(m);
        self.csr_cache.install(snap);
        Ok(())
    }

    /// Create a hot backup of this database to `destination`.
    ///
    /// `destination` is a **file path** for the backup snapshot (e.g.
    /// `/backups/mydb_2026-05-27.mdb`). The file is a complete, portable
    /// LMDB snapshot. Concurrent reads and writes are not blocked.
    ///
    /// To restore: create an empty directory, copy the snapshot file to
    /// `<dir>/data.mdb`, then call `Graph::open(<dir>, map_size_gb)`.
    pub fn backup(&self, destination: &Path) -> Result<(), Error> {
        self.storage
            .env
            .copy_to_path(destination, heed::CompactionOption::Disabled)
            .map(|_| ())
            .map_err(Error::Storage)
    }

    /// Same as `backup` but compacts the database during the copy.
    ///
    /// The resulting file is smaller than a raw backup but the operation
    /// takes longer because it rewrites every live page.
    pub fn backup_compact(&self, destination: &Path) -> Result<(), Error> {
        self.storage
            .env
            .copy_to_path(destination, heed::CompactionOption::Enabled)
            .map(|_| ())
            .map_err(Error::Storage)
    }

    /// Restore a backup snapshot created by `backup` or `backup_compact` into
    /// a new database directory.
    ///
    /// Creates `dst_dir` if it does not exist, then copies `snapshot_file` into
    /// `dst_dir/data.mdb`. After this call succeeds the caller can open the
    /// restored database with `Graph::open(dst_dir, map_size_gb)`.
    pub fn restore(snapshot_file: &Path, dst_dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(dst_dir)?;
        let dst_file = dst_dir.join("data.mdb");
        std::fs::copy(snapshot_file, &dst_file)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Nodes
    // ------------------------------------------------------------------

    /// Insert a node with a string label and msgpack-serializable properties.
    #[instrument(skip(self, props), fields(label = %label))]
    pub fn add_node(&self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let id = self.add_node_impl(&mut wtxn, label, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(id)
    }

    fn add_node_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        props: &impl Serialize,
    ) -> Result<NodeId, Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let node_id = alloc_node_id(&self.storage, wtxn)?;
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;

        // Validate constraints and populate indexes
        let active_indexes = self.get_active_node_indexes(wtxn, label_id)?;
        if !active_indexes.is_empty() {
            for (prop_key_id, flags) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    let prop_val = props_json.get(&prop_name);

                    // 1. Required constraint check
                    if flags == 0x02
                        && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null))
                    {
                        return Err(Error::RequiredConstraintViolation(
                            label.to_string(),
                            prop_name.to_string(),
                        ));
                    }

                    if let Some(val) = prop_val {
                        if val != &serde_json::Value::Null {
                            if let Some(encoded) = encode_property_value(val) {
                                // 2. Unique constraint check
                                if flags == 0x01 {
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
                                    prefix.extend_from_slice(&label_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded);

                                    for entry in
                                        self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut node_id_bytes = [0u8; 8];
                                            node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_node_id = u64::from_be_bytes(node_id_bytes);
                                            if found_node_id != node_id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    label.to_string(),
                                                    prop_name.to_string(),
                                                    val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }

                                // 3. Write index entry
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                                self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        let record = NodeRecord {
            label: label_id,
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &node_id, &props::encode(&record)?)?;
        self.storage
            .label_idx
            .put(wtxn, &composite_key(label_id, node_id), &())?;

        // FTS indexing hook
        self.index_node_fts(wtxn, node_id, label_id, &props_json)?;

        adjust_label_count(&self.storage, wtxn, label_id, 1)?;

        Ok(node_id)
    }

    /// Fetch a node record by id.
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.get_node_impl(&rtxn, id)
    }

    fn get_node_impl(&self, txn: &heed::RoTxn, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        match self.storage.nodes.get(txn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Update the properties of an existing node. The node's label is unchanged.
    ///
    /// # Deadlock warning
    ///
    /// Do not call this method from inside a [`Graph::update`] closure. Use
    /// [`WriteTxn::update_node`] inside the closure instead.
    pub fn update_node(&self, id: NodeId, props: &impl Serialize) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.update_node_impl(&mut wtxn, id, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    fn update_node_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: NodeId,
        props: &impl Serialize,
    ) -> Result<(), Error> {
        let old_rec: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Err(Error::NodeNotFound(id)),
        };

        let label_id = old_rec.label;
        let label_name = self
            .label_name_impl(wtxn, label_id)?
            .unwrap_or_else(|| label_id.to_string());
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;
        let old_props_json: serde_json::Value = props::decode(&old_rec.props)?;

        // Same label: delete old, validate and write new property index entries.
        let active = self.get_active_node_indexes(wtxn, label_id)?;
        for (prop_key_id, flags) in active {
            if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                let old_val = old_props_json.get(&prop_name);
                let new_val = props_json.get(&prop_name);

                if old_val != new_val {
                    // Property value changed.
                    if flags == 0x02
                        && (new_val.is_none() || new_val == Some(&serde_json::Value::Null))
                    {
                        return Err(Error::RequiredConstraintViolation(
                            label_name.clone(),
                            prop_name.to_string(),
                        ));
                    }

                    // 1. Delete old
                    if let Some(o_val) = old_val {
                        if o_val != &serde_json::Value::Null {
                            if let Some(encoded_old) = encode_property_value(o_val) {
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded_old, id);
                                self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                            }
                        }
                    }

                    // 2. Validate and write new
                    if let Some(n_val) = new_val {
                        if n_val != &serde_json::Value::Null {
                            if let Some(encoded_new) = encode_property_value(n_val) {
                                if flags == 0x01 {
                                    // Unique check
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded_new.len());
                                    prefix.extend_from_slice(&label_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded_new);
                                    for entry in
                                        self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut node_id_bytes = [0u8; 8];
                                            node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_node_id = u64::from_be_bytes(node_id_bytes);
                                            if found_node_id != id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    label_name.clone(),
                                                    prop_name.to_string(),
                                                    n_val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded_new, id);
                                self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        self.update_node_fts(
            wtxn,
            id,
            old_rec.label,
            label_id,
            &old_props_json,
            &props_json,
        )?;

        let record = NodeRecord {
            label: label_id,
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &id, &props::encode(&record)?)?;
        Ok(())
    }

    /// Delete a node.
    #[instrument(skip(self))]
    pub fn delete_node(&self, id: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_node_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    fn delete_node_impl(&self, wtxn: &mut heed::RwTxn, id: NodeId) -> Result<(), Error> {
        let record: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Ok(()),
        };

        let props_json: serde_json::Value = props::decode(&record.props)?;

        // FTS indexing deletion hook
        self.delete_node_fts(wtxn, id, record.label, &props_json)?;

        // 0. Delete from node property index
        let active = self.get_active_node_indexes(wtxn, record.label)?;
        if !active.is_empty() {
            for (prop_key_id, _) in active {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    if let Some(val) = props_json.get(&prop_name) {
                        if let Some(encoded) = encode_property_value(val) {
                            let idx_key =
                                node_prop_index_key(record.label, prop_key_id, &encoded, id);
                            self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                        }
                    }
                }
            }
        }

        // 1. Delete from label index
        self.storage
            .label_idx
            .delete(wtxn, &composite_key(record.label, id))?;

        adjust_label_count(&self.storage, wtxn, record.label, -1)?;

        // 2. Process all outgoing neighbors (out_adj)
        let mut out_edges = Vec::new();
        if let Some(iter) = self.storage.out_adj.get_duplicates(wtxn, &id)? {
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
            if let Some(edge_rec) = self.get_edge_impl(wtxn, edge_id)? {
                self.delete_edge_index_entries(wtxn, edge_id, &edge_rec)?;
            }
            // Delete edge and type index
            self.storage.edges.delete(wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, wtxn, entry.edge_type, -1)?;

            // Delete the corresponding in_adj entry on the neighbor
            let in_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .in_adj
                .delete_one_duplicate(wtxn, &other, in_entry.as_bytes())?;
        }

        // 3. Process all incoming neighbors (in_adj)
        let mut in_edges = Vec::new();
        if let Some(iter) = self.storage.in_adj.get_duplicates(wtxn, &id)? {
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
            if let Some(edge_rec) = self.get_edge_impl(wtxn, edge_id)? {
                self.delete_edge_index_entries(wtxn, edge_id, &edge_rec)?;
            }
            // Delete edge and type index
            self.storage.edges.delete(wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, wtxn, entry.edge_type, -1)?;

            // Delete the corresponding out_adj entry on the neighbor
            let out_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .out_adj
                .delete_one_duplicate(wtxn, &other, out_entry.as_bytes())?;
        }

        // 4. Delete the adjacency list keys themselves
        self.storage.out_adj.delete(wtxn, &id)?;
        self.storage.in_adj.delete(wtxn, &id)?;

        // 5. Delete persisted vector bytes
        self.storage.vectors.delete(wtxn, &id)?;

        // 6. Delete from primary nodes database
        self.storage.nodes.delete(wtxn, &id)?;

        Ok(())
    }

    /// Delete an edge.
    #[instrument(skip(self))]
    pub fn delete_edge(&self, id: EdgeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_edge_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(crate) fn delete_edge_impl(&self, wtxn: &mut heed::RwTxn, id: EdgeId) -> Result<(), Error> {
        let record: EdgeRecord = match self.get_edge_impl(wtxn, id)? {
            Some(rec) => rec,
            None => return Ok(()),
        };

        // 1. Delete from edge property index
        self.delete_edge_index_entries(wtxn, id, &record)?;

        // 2. Delete the edge record itself
        self.storage.edges.delete(wtxn, &id)?;

        // 3. Delete from the type index
        self.storage
            .type_idx
            .delete(wtxn, &composite_key(record.edge_type, id))?;

        // 4. Adjust the type count
        adjust_type_count(&self.storage, wtxn, record.edge_type, -1)?;

        // 5. Delete from out_adj (key is src, other is dst)
        let out_entry = AdjEntry {
            edge_type: record.edge_type,
            other: record.dst,
            edge_id: id,
        };
        self.storage
            .out_adj
            .delete_one_duplicate(wtxn, &record.src, out_entry.as_bytes())?;

        // 6. Delete from in_adj (key is dst, other is src)
        let in_entry = AdjEntry {
            edge_type: record.edge_type,
            other: record.src,
            edge_id: id,
        };
        self.storage
            .in_adj
            .delete_one_duplicate(wtxn, &record.dst, in_entry.as_bytes())?;

        Ok(())
    }

    // ------------------------------------------------------------------
    // Edges
    // ------------------------------------------------------------------

    /// Insert a directed edge `src → dst` with a string type and properties.
    #[instrument(skip(self, props), fields(src = %src, dst = %dst, etype = %etype))]
    pub fn add_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let edge_id = self.add_edge_impl(&mut wtxn, src, dst, etype, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(edge_id)
    }

    fn add_edge_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let edge_id = alloc_edge_id(&self.storage, wtxn)?;
        let encoded_props = props::encode(props)?;

        // Validate constraints and populate indexes
        let active_indexes = self.get_active_edge_indexes(wtxn, type_id)?;
        if !active_indexes.is_empty() {
            let props_json: serde_json::Value = props::decode(&encoded_props)?;
            for (prop_key_id, flags) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    let prop_val = props_json.get(&prop_name);

                    // 1. Required constraint check
                    if flags == 0x02
                        && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null))
                    {
                        return Err(Error::RequiredConstraintViolation(
                            etype.to_string(),
                            prop_name.to_string(),
                        ));
                    }

                    if let Some(val) = prop_val {
                        if val != &serde_json::Value::Null {
                            if let Some(encoded) = encode_property_value(val) {
                                // 2. Unique constraint check
                                if flags == 0x01 {
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
                                    prefix.extend_from_slice(&type_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded);

                                    for entry in
                                        self.storage.edge_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut edge_id_bytes = [0u8; 8];
                                            edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_edge_id = u64::from_be_bytes(edge_id_bytes);
                                            if found_edge_id != edge_id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    etype.to_string(),
                                                    prop_name.to_string(),
                                                    val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }

                                // 3. Write index entry
                                let idx_key =
                                    edge_prop_index_key(type_id, prop_key_id, &encoded, edge_id);
                                self.storage.edge_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        let record = EdgeRecord {
            src,
            dst,
            edge_type: type_id,
            props: encoded_props,
        };
        self.storage
            .edges
            .put(wtxn, &edge_id, &props::encode(&record)?)?;
        self.storage
            .type_idx
            .put(wtxn, &composite_key(type_id, edge_id), &())?;

        self.append_adj(wtxn, src, dst, type_id, edge_id, true)?;
        self.append_adj(wtxn, dst, src, type_id, edge_id, false)?;

        adjust_type_count(&self.storage, wtxn, type_id, 1)?;

        Ok(edge_id)
    }

    /// Fetch an edge record by id.
    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.get_edge_impl(&rtxn, id)
    }

    fn get_edge_impl(&self, txn: &heed::RoTxn, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        match self.storage.edges.get(txn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Traversal
    // ------------------------------------------------------------------

    /// Returns neighbor entries for all outgoing edges of `node`.
    ///
    /// Uses the in-memory CSR snapshot when the node is present in it; falls
    /// back to an LMDB cursor for nodes added since the last rebuild.
    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.out_neighbors_impl(&rtxn, node)
    }

    fn out_neighbors_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let snap = self.csr_cache.snapshot.load();
        if let Some(neighbors) = snap.out_neighbors(node) {
            return Ok(neighbors
                .into_iter()
                .map(|(node, edge, edge_type)| NeighborEntry {
                    node,
                    edge,
                    edge_type,
                })
                .collect());
        }
        self.adj_entries_impl(rtxn, node, true)
    }

    /// Returns neighbor entries for all incoming edges of `node`.
    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.in_neighbors_impl(&rtxn, node)
    }

    fn in_neighbors_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<NeighborEntry>, Error> {
        self.adj_entries_impl(rtxn, node, false)
    }

    // ------------------------------------------------------------------
    // Secondary index queries
    // ------------------------------------------------------------------

    /// Returns all node IDs with the given label, in ascending ID order.
    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_label_impl(&rtxn, label)
    }

    fn nodes_by_label_impl(&self, rtxn: &heed::RoTxn, label: &str) -> Result<Vec<NodeId>, Error> {
        let label_id = {
            let key = format!("label:{label}");
            match self.storage.meta.get(rtxn, &key)? {
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
        let iter = self.storage.label_idx.prefix_iter(rtxn, &prefix)?;
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
        self.edges_by_type_impl(&rtxn, etype)
    }

    fn edges_by_type_impl(&self, rtxn: &heed::RoTxn, etype: &str) -> Result<Vec<EdgeId>, Error> {
        let type_id = {
            let key = format!("type:{etype}");
            match self.storage.meta.get(rtxn, &key)? {
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
        let iter = self.storage.type_idx.prefix_iter(rtxn, &prefix)?;
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
        let rtxn = self.storage.env.read_txn()?;
        self.label_name_impl(&rtxn, id)
    }

    fn label_name_impl(&self, rtxn: &heed::RoTxn, id: LabelId) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "label:", id)
    }

    /// Resolves a `TypeId` back to its string name.
    ///
    /// Scans the `meta` sub-database for the matching `type:{name}` entry.
    /// Returns `None` for ids that are not in the registry.
    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.type_name_impl(&rtxn, id)
    }

    fn type_name_impl(&self, rtxn: &heed::RoTxn, id: TypeId) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "type:", id)
    }

    fn prop_key_name_impl(
        &self,
        rtxn: &heed::RoTxn,
        id: PropKeyId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "prop_key:", id)
    }

    fn delete_edge_index_entries(
        &self,
        wtxn: &mut heed::RwTxn,
        edge_id: EdgeId,
        record: &EdgeRecord,
    ) -> Result<(), Error> {
        let active_indexes = self.get_active_edge_indexes(wtxn, record.edge_type)?;
        if !active_indexes.is_empty() {
            let props_json: serde_json::Value = props::decode(&record.props)?;
            for (prop_key_id, _) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    if let Some(val) = props_json.get(&prop_name) {
                        if let Some(encoded) = encode_property_value(val) {
                            let idx_key = edge_prop_index_key(
                                record.edge_type,
                                prop_key_id,
                                &encoded,
                                edge_id,
                            );
                            self.storage.edge_prop_idx.delete(wtxn, &idx_key)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Get the count of nodes matching a string label.
    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.node_count_by_label_impl(&rtxn, label)
    }

    fn node_count_by_label_impl(&self, rtxn: &heed::RoTxn, label: &str) -> Result<u64, Error> {
        let meta_key = format!("label:{label}");
        if let Some(b) = self.storage.meta.get(rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
            let label_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_label_count(&self.storage, rtxn, label_id)
        } else {
            Ok(0)
        }
    }

    /// Get the count of edges matching a string type.
    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edge_count_by_type_impl(&rtxn, etype)
    }

    fn edge_count_by_type_impl(&self, rtxn: &heed::RoTxn, etype: &str) -> Result<u64, Error> {
        let meta_key = format!("type:{etype}");
        if let Some(b) = self.storage.meta.get(rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
            let type_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_type_count(&self.storage, rtxn, type_id)
        } else {
            Ok(0)
        }
    }

    fn meta_reverse_lookup_impl(
        &self,
        rtxn: &heed::RoTxn,
        prefix: &str,
        id: u32,
    ) -> Result<Option<String>, Error> {
        for entry in self.storage.meta.iter(rtxn)? {
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

    fn get_active_node_indexes(
        &self,
        rtxn: &heed::RoTxn,
        label_id: LabelId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:node:l:{label_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.iter(rtxn)? {
            let (key, val) = entry?;
            if let Some(prop_str) = key.strip_prefix(&prefix) {
                let prop_key_id: PropKeyId = prop_str
                    .parse()
                    .map_err(|_| Error::Corrupt("prop key id in meta must be integer"))?;
                let flags = val.first().copied().unwrap_or(0x00);
                active.push((prop_key_id, flags));
            }
        }
        Ok(active)
    }

    fn get_active_edge_indexes(
        &self,
        rtxn: &heed::RoTxn,
        type_id: TypeId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:edge:t:{type_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.iter(rtxn)? {
            let (key, val) = entry?;
            if let Some(prop_str) = key.strip_prefix(&prefix) {
                let prop_key_id: PropKeyId = prop_str
                    .parse()
                    .map_err(|_| Error::Corrupt("prop key id in meta must be integer"))?;
                let flags = val.first().copied().unwrap_or(0x00);
                active.push((prop_key_id, flags));
            }
        }
        Ok(active)
    }

    pub fn create_node_property_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_node_unique_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_node_required_constraint(
        &self,
        label: &str,
        property: &str,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    fn create_node_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:node:l:{label_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                return Ok(());
            }
        }

        let node_ids = self.nodes_by_label_impl(wtxn, label)?;
        let mut seen_values = ahash::AHashSet::new();

        for node_id in &node_ids {
            let record = self
                .get_node_impl(wtxn, *node_id)?
                .ok_or(Error::NodeNotFound(*node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            let prop_val = props_json.get(property);

            if flags == 0x02 && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null)) {
                return Err(Error::RequiredConstraintViolation(
                    label.to_string(),
                    property.to_string(),
                ));
            }

            if let Some(val) = prop_val {
                if flags == 0x01 && !seen_values.insert(val.clone()) {
                    return Err(Error::UniqueConstraintViolation(
                        label.to_string(),
                        property.to_string(),
                        val.to_string(),
                    ));
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[flags])?;

        for node_id in node_ids {
            let record = self
                .get_node_impl(wtxn, node_id)?
                .ok_or(Error::NodeNotFound(node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(val) = props_json.get(property) {
                if let Some(encoded) = encode_property_value(val) {
                    let idx_key = node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                    self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        Ok(())
    }

    pub fn drop_node_property_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_node_unique_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_node_required_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    fn drop_node_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:node:l:{label_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                self.storage.meta.delete(wtxn, &meta_key)?;

                let mut prefix = Vec::with_capacity(8);
                prefix.extend_from_slice(&label_id.to_be_bytes());
                prefix.extend_from_slice(&prop_key_id.to_be_bytes());

                let mut to_delete = Vec::new();
                for entry in self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)? {
                    let (key, _) = entry?;
                    to_delete.push(key.to_vec());
                }

                for key in to_delete {
                    self.storage.node_prop_idx.delete(wtxn, &key)?;
                }
            }
        }

        Ok(())
    }

    pub fn create_node_text_index(&self, label: &str, property: &str) -> Result<(), Error> {
        self.create_node_text_index_with_language(label, property, Language::English)
    }

    pub fn create_node_text_index_with_language(
        &self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_text_index_impl(&mut wtxn, label, property, lang)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_node_text_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_text_index_impl(&mut wtxn, label, property)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.has_node_text_index_impl(&rtxn, label, property)
    }

    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.active_text_indexes_impl(&rtxn)
    }

    /// Tokenize `text` using the stemmer and stop-word rules for `lang`,
    /// returning a map from term to term-frequency count.
    ///
    /// Exposed on `Graph` so that higher-level crates (`issundb-text`) can
    /// perform BM25 scoring without reaching into `storage::fts` directly.
    pub fn tokenize_text(
        &self,
        text: &str,
        lang: Language,
    ) -> std::collections::HashMap<String, u32> {
        fts::tokenize(text, lang)
    }

    fn active_text_indexes_impl(
        &self,
        rtxn: &heed::RoTxn,
    ) -> Result<Vec<(String, String, Language)>, Error> {
        let prefix = "fts_meta:node:l:";
        let mut results = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, prefix)? {
            let (key, val) = entry?;
            if let Some(suffix) = key.strip_prefix(prefix) {
                let parts: Vec<&str> = suffix.split(":p:").collect();
                if parts.len() == 2 {
                    if let (Ok(label_id), Ok(prop_key_id)) =
                        (parts[0].parse::<LabelId>(), parts[1].parse::<PropKeyId>())
                    {
                        if let (Some(label), Some(property)) = (
                            self.label_name_impl(rtxn, label_id)?,
                            self.prop_key_name_impl(rtxn, prop_key_id)?,
                        ) {
                            let lang = if !val.is_empty() {
                                Language::from_u8(val[0])
                            } else {
                                Language::English
                            };
                            results.push((label, property, lang));
                        }
                    }
                }
            }
        }
        Ok(results)
    }

    fn get_fts_index_language(
        &self,
        rtxn: &heed::RoTxn,
        label_id: LabelId,
        prop_key_id: PropKeyId,
    ) -> Result<Language, Error> {
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");
        if let Some(bytes) = self.storage.meta.get(rtxn, &meta_key)? {
            if !bytes.is_empty() {
                return Ok(Language::from_u8(bytes[0]));
            }
        }
        Ok(Language::English)
    }

    fn has_node_text_index_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<bool, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(false),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(false),
        };
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");
        let active = self.storage.meta.get(rtxn, &meta_key)?.is_some();
        Ok(active)
    }

    fn create_node_text_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");

        if self.storage.meta.get(wtxn, &meta_key)?.is_some() {
            return Ok(());
        }

        let node_ids = self.nodes_by_label_impl(wtxn, label)?;

        let mut fts_n: u64 = 0;
        let mut fts_sum_dl: u64 = 0;

        for &node_id in &node_ids {
            let record = self
                .get_node_impl(wtxn, node_id)?
                .ok_or(Error::NodeNotFound(node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(serde_json::Value::String(text_val)) = props_json.get(property) {
                let terms = fts::tokenize(text_val, lang);
                let doc_len: u32 = terms.values().sum();
                if doc_len > 0 {
                    let d_key = fts_doc_key(label_id, prop_key_id, node_id);
                    self.storage
                        .fts_docs
                        .put(wtxn, &d_key, &doc_len.to_be_bytes())?;

                    for (term, freq) in terms {
                        let p_key = fts_postings_key(label_id, prop_key_id, &term);
                        let p_val = fts_posting_val(node_id, freq);
                        self.storage.fts_postings.put(wtxn, &p_key, &p_val)?;
                    }

                    fts_n += 1;
                    fts_sum_dl += doc_len as u64;
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[lang.to_u8()])?;
        self.storage.meta.put(
            wtxn,
            &fts_stats_n_key(label_id, prop_key_id),
            &fts_n.to_be_bytes(),
        )?;
        self.storage.meta.put(
            wtxn,
            &fts_stats_sum_dl_key(label_id, prop_key_id),
            &fts_sum_dl.to_be_bytes(),
        )?;

        Ok(())
    }

    fn drop_node_text_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");

        if self.storage.meta.get(wtxn, &meta_key)?.is_some() {
            self.storage.meta.delete(wtxn, &meta_key)?;
            self.storage
                .meta
                .delete(wtxn, &fts_stats_n_key(label_id, prop_key_id))?;
            self.storage
                .meta
                .delete(wtxn, &fts_stats_sum_dl_key(label_id, prop_key_id))?;

            // Delete postings prefix
            let mut postings_prefix = Vec::with_capacity(8);
            postings_prefix.extend_from_slice(&label_id.to_be_bytes());
            postings_prefix.extend_from_slice(&prop_key_id.to_be_bytes());

            let mut postings_to_delete = Vec::new();
            for entry in self
                .storage
                .fts_postings
                .prefix_iter(wtxn, &postings_prefix)?
            {
                let (key, _) = entry?;
                postings_to_delete.push(key.to_vec());
            }
            for key in postings_to_delete {
                self.storage.fts_postings.delete(wtxn, &key)?;
            }

            // Delete doc lengths prefix
            let mut docs_prefix = Vec::with_capacity(8);
            docs_prefix.extend_from_slice(&label_id.to_be_bytes());
            docs_prefix.extend_from_slice(&prop_key_id.to_be_bytes());

            let mut docs_to_delete = Vec::new();
            for entry in self.storage.fts_docs.prefix_iter(wtxn, &docs_prefix)? {
                let (key, _) = entry?;
                docs_to_delete.push(key.to_vec());
            }
            for key in docs_to_delete {
                self.storage.fts_docs.delete(wtxn, &key)?;
            }
        }

        Ok(())
    }

    fn active_text_indexes_for_label(
        &self,
        rtxn: &heed::RoTxn,
        label_id: LabelId,
    ) -> Result<Vec<PropKeyId>, Error> {
        let prefix = format!("fts_meta:node:l:{label_id}:p:");
        let mut prop_keys = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if let Some(suffix) = key.strip_prefix(&prefix) {
                if let Ok(prop_key_id) = suffix.parse::<PropKeyId>() {
                    prop_keys.push(prop_key_id);
                }
            }
        }
        Ok(prop_keys)
    }

    fn index_node_fts(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        // Collect read-side data before taking a mutable borrow.
        let adds: Vec<(PropKeyId, String)> = {
            let rtxn: &heed::RoTxn = wtxn;
            let active_prop_keys = self.active_text_indexes_for_label(rtxn, label_id)?;
            let mut out = Vec::new();
            for prop_key_id in active_prop_keys {
                if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                    if let Some(serde_json::Value::String(text_val)) = props_json.get(&prop_name) {
                        out.push((prop_key_id, text_val.clone()));
                    }
                }
            }
            out
        };
        for (prop_key_id, text_val) in adds {
            self.add_node_prop_fts_entry(wtxn, node_id, label_id, prop_key_id, &text_val)?;
        }
        Ok(())
    }

    fn update_node_fts(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        old_label_id: LabelId,
        new_label_id: LabelId,
        old_props: &serde_json::Value,
        new_props: &serde_json::Value,
    ) -> Result<(), Error> {
        // Collect all read-side data before any mutable borrow of wtxn.
        if old_label_id != new_label_id {
            // Label changed: collect entries to remove for old label.
            let removes: Vec<(PropKeyId, String)> = {
                let rtxn: &heed::RoTxn = wtxn;
                let old_active = self.active_text_indexes_for_label(rtxn, old_label_id)?;
                let mut out = Vec::new();
                for prop_key_id in old_active {
                    if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                        if let Some(serde_json::Value::String(old_text)) = old_props.get(&prop_name)
                        {
                            out.push((prop_key_id, old_text.clone()));
                        }
                    }
                }
                out
            };
            for (prop_key_id, old_text) in removes {
                self.remove_node_prop_fts_entry(
                    wtxn,
                    node_id,
                    old_label_id,
                    prop_key_id,
                    &old_text,
                )?;
            }

            // Collect entries to add for new label.
            let adds: Vec<(PropKeyId, String)> = {
                let rtxn: &heed::RoTxn = wtxn;
                let new_active = self.active_text_indexes_for_label(rtxn, new_label_id)?;
                let mut out = Vec::new();
                for prop_key_id in new_active {
                    if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                        if let Some(serde_json::Value::String(new_text)) = new_props.get(&prop_name)
                        {
                            out.push((prop_key_id, new_text.clone()));
                        }
                    }
                }
                out
            };
            for (prop_key_id, new_text) in adds {
                self.add_node_prop_fts_entry(wtxn, node_id, new_label_id, prop_key_id, &new_text)?;
            }
        } else {
            // Same label: collect changed properties.
            #[allow(clippy::type_complexity)]
            let changes: Vec<(PropKeyId, Option<String>, Option<String>)> = {
                let rtxn: &heed::RoTxn = wtxn;
                let active = self.active_text_indexes_for_label(rtxn, new_label_id)?;
                let mut out = Vec::new();
                for prop_key_id in active {
                    if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                        let old_val = old_props.get(&prop_name);
                        let new_val = new_props.get(&prop_name);
                        if old_val != new_val {
                            let old_text = if let Some(serde_json::Value::String(s)) = old_val {
                                Some(s.clone())
                            } else {
                                None
                            };
                            let new_text = if let Some(serde_json::Value::String(s)) = new_val {
                                Some(s.clone())
                            } else {
                                None
                            };
                            out.push((prop_key_id, old_text, new_text));
                        }
                    }
                }
                out
            };
            for (prop_key_id, old_text, new_text) in changes {
                if let Some(ref t) = old_text {
                    self.remove_node_prop_fts_entry(wtxn, node_id, new_label_id, prop_key_id, t)?;
                }
                if let Some(ref t) = new_text {
                    self.add_node_prop_fts_entry(wtxn, node_id, new_label_id, prop_key_id, t)?;
                }
            }
        }

        Ok(())
    }

    fn delete_node_fts(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        // Collect read-side data before taking a mutable borrow.
        let removes: Vec<(PropKeyId, String)> = {
            let rtxn: &heed::RoTxn = wtxn;
            let active_prop_keys = self.active_text_indexes_for_label(rtxn, label_id)?;
            let mut out = Vec::new();
            for prop_key_id in active_prop_keys {
                if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                    if let Some(serde_json::Value::String(text_val)) = props_json.get(&prop_name) {
                        out.push((prop_key_id, text_val.clone()));
                    }
                }
            }
            out
        };
        for (prop_key_id, text_val) in removes {
            self.remove_node_prop_fts_entry(wtxn, node_id, label_id, prop_key_id, &text_val)?;
        }
        Ok(())
    }

    fn add_node_prop_fts_entry(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        prop_key_id: PropKeyId,
        text: &str,
    ) -> Result<(), Error> {
        let lang = {
            let rtxn: &heed::RoTxn = wtxn;
            self.get_fts_index_language(rtxn, label_id, prop_key_id)?
        };
        let terms = fts::tokenize(text, lang);
        let doc_len: u32 = terms.values().sum();
        if doc_len > 0 {
            let d_key = fts_doc_key(label_id, prop_key_id, node_id);
            self.storage
                .fts_docs
                .put(wtxn, &d_key, &doc_len.to_be_bytes())?;

            for (term, freq) in terms {
                let p_key = fts_postings_key(label_id, prop_key_id, &term);
                let p_val = fts_posting_val(node_id, freq);
                self.storage.fts_postings.put(wtxn, &p_key, &p_val)?;
            }

            // Stats
            let n_key = fts_stats_n_key(label_id, prop_key_id);
            let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

            let current_n = self
                .storage
                .meta
                .get(wtxn, &n_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats n: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);
            let current_sum_dl = self
                .storage
                .meta
                .get(wtxn, &sum_dl_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats sum_dl: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);

            self.storage
                .meta
                .put(wtxn, &n_key, &(current_n + 1).to_be_bytes())?;
            self.storage.meta.put(
                wtxn,
                &sum_dl_key,
                &(current_sum_dl + doc_len as u64).to_be_bytes(),
            )?;
        }
        Ok(())
    }

    fn remove_node_prop_fts_entry(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        prop_key_id: PropKeyId,
        text: &str,
    ) -> Result<(), Error> {
        let d_key = fts_doc_key(label_id, prop_key_id, node_id);
        if let Some(bytes) = self.storage.fts_docs.get(wtxn, &d_key)? {
            let doc_len = parse_fts_doc_val(bytes)?;
            self.storage.fts_docs.delete(wtxn, &d_key)?;

            let lang = {
                let rtxn: &heed::RoTxn = wtxn;
                self.get_fts_index_language(rtxn, label_id, prop_key_id)?
            };
            let terms = fts::tokenize(text, lang);
            for (term, freq) in terms {
                let p_key = fts_postings_key(label_id, prop_key_id, &term);
                let p_val = fts_posting_val(node_id, freq);
                self.storage
                    .fts_postings
                    .delete_one_duplicate(wtxn, &p_key, &p_val)?;
            }

            // Stats
            let n_key = fts_stats_n_key(label_id, prop_key_id);
            let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

            let current_n = self
                .storage
                .meta
                .get(wtxn, &n_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats n: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);
            let current_sum_dl = self
                .storage
                .meta
                .get(wtxn, &sum_dl_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats sum_dl: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);

            let next_n = current_n.saturating_sub(1);
            let next_sum_dl = current_sum_dl.saturating_sub(doc_len as u64);

            self.storage.meta.put(wtxn, &n_key, &next_n.to_be_bytes())?;
            self.storage
                .meta
                .put(wtxn, &sum_dl_key, &next_sum_dl.to_be_bytes())?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_stats_impl(&rtxn, label, property)
    }

    fn fts_stats_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<Option<(u64, u64)>, Error> {
        if !self.has_node_text_index_impl(rtxn, label, property)? {
            return Ok(None);
        }
        let label_id = get_label(&self.storage, rtxn, label)?
            .ok_or(Error::Corrupt("fts_stats: label id not found"))?;
        let prop_key_id = get_prop_key(&self.storage, rtxn, property)?
            .ok_or(Error::Corrupt("fts_stats: prop key id not found"))?;

        let n_key = fts_stats_n_key(label_id, prop_key_id);
        let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

        let n = self
            .storage
            .meta
            .get(rtxn, &n_key)?
            .map(|b| -> Result<u64, Error> {
                Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                    Error::Corrupt("fts stats n: wrong byte length")
                })?))
            })
            .transpose()?
            .unwrap_or(0);
        let sum_dl = self
            .storage
            .meta
            .get(rtxn, &sum_dl_key)?
            .map(|b| -> Result<u64, Error> {
                Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                    Error::Corrupt("fts stats sum_dl: wrong byte length")
                })?))
            })
            .transpose()?
            .unwrap_or(0);

        Ok(Some((n, sum_dl)))
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_doc_len_impl(&rtxn, label, property, node_id)
    }

    fn fts_doc_len_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(None),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(None),
        };

        let d_key = fts_doc_key(label_id, prop_key_id, node_id);
        if let Some(bytes) = self.storage.fts_docs.get(rtxn, &d_key)? {
            let doc_len = parse_fts_doc_val(bytes)?;
            Ok(Some(doc_len))
        } else {
            Ok(None)
        }
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_postings_impl(&rtxn, label, property, term)
    }

    fn fts_postings_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };

        let p_key = fts_postings_key(label_id, prop_key_id, term);
        let mut postings = Vec::new();
        if let Some(iter) = self.storage.fts_postings.get_duplicates(rtxn, &p_key)? {
            for result in iter {
                let (_, bytes) = result?;
                let (node_id, freq) = parse_fts_posting_val(bytes)?;
                postings.push((node_id, freq));
            }
        }

        Ok(postings)
    }

    pub fn create_edge_property_index(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_edge_unique_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_edge_required_constraint(
        &self,
        etype: &str,
        property: &str,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    fn create_edge_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        etype: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:edge:t:{type_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                return Ok(());
            }
        }

        let edge_ids = self.edges_by_type_impl(wtxn, etype)?;
        let mut seen_values = ahash::AHashSet::new();

        for edge_id in &edge_ids {
            let record = self
                .get_edge_impl(wtxn, *edge_id)?
                .ok_or(Error::EdgeNotFound(*edge_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            let prop_val = props_json.get(property);

            if flags == 0x02 && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null)) {
                return Err(Error::RequiredConstraintViolation(
                    etype.to_string(),
                    property.to_string(),
                ));
            }

            if let Some(val) = prop_val {
                if flags == 0x01 && !seen_values.insert(val.clone()) {
                    return Err(Error::UniqueConstraintViolation(
                        etype.to_string(),
                        property.to_string(),
                        val.to_string(),
                    ));
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[flags])?;

        for edge_id in edge_ids {
            let record = self
                .get_edge_impl(wtxn, edge_id)?
                .ok_or(Error::EdgeNotFound(edge_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(val) = props_json.get(property) {
                if let Some(encoded) = encode_property_value(val) {
                    let idx_key = edge_prop_index_key(type_id, prop_key_id, &encoded, edge_id);
                    self.storage.edge_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        Ok(())
    }

    pub fn drop_edge_property_index(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_edge_unique_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_edge_required_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    fn drop_edge_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        etype: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:edge:t:{type_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                self.storage.meta.delete(wtxn, &meta_key)?;

                let mut prefix = Vec::with_capacity(8);
                prefix.extend_from_slice(&type_id.to_be_bytes());
                prefix.extend_from_slice(&prop_key_id.to_be_bytes());

                let mut to_delete = Vec::new();
                for entry in self.storage.edge_prop_idx.prefix_iter(wtxn, &prefix)? {
                    let (key, _) = entry?;
                    to_delete.push(key.to_vec());
                }

                for key in to_delete {
                    self.storage.edge_prop_idx.delete(wtxn, &key)?;
                }
            }
        }

        Ok(())
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_property_impl(&rtxn, label, property, val)
    }

    fn nodes_by_property_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        let val = val.into_json();
        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= 8 {
                let mut node_id_bytes = [0u8; 8];
                node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(node_id_bytes));
            }
        }
        Ok(result)
    }

    pub fn nodes_by_property_range(
        &self,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_property_range_impl(&rtxn, label, property, min_val, max_val)
    }

    fn nodes_by_property_range_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<NodeId>, Error> {
        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(8);
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());

        let min_encoded = min_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);
        let max_encoded = max_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

                if let Some(ref min_enc) = min_encoded {
                    if val_bytes < min_enc.as_slice() {
                        continue;
                    }
                }
                if let Some(ref max_enc) = max_encoded {
                    if val_bytes > max_enc.as_slice() {
                        continue;
                    }
                }

                let mut node_id_bytes = [0u8; 8];
                node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(node_id_bytes));
            }
        }
        Ok(result)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.has_node_property_index_impl(&rtxn, label, property)
    }

    fn has_node_property_index_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<bool, Error> {
        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(false),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(false),
        };

        let meta_key = format!("idx_meta:node:l:{label_id}:p:{prop_key_id}");
        Ok(self.storage.meta.get(rtxn, &meta_key)?.is_some())
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_property_impl(&rtxn, etype, property, val)
    }

    fn edges_by_property_impl(
        &self,
        rtxn: &heed::RoTxn,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        let val = val.into_json();
        let type_key = format!("type:{etype}");
        let type_id = match self.storage.meta.get(rtxn, &type_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&type_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= 8 {
                let mut edge_id_bytes = [0u8; 8];
                edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(edge_id_bytes));
            }
        }
        Ok(result)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_property_range_impl(&rtxn, etype, property, min_val, max_val)
    }

    fn edges_by_property_range_impl(
        &self,
        rtxn: &heed::RoTxn,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        let type_key = format!("type:{etype}");
        let type_id = match self.storage.meta.get(rtxn, &type_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(8);
        prefix.extend_from_slice(&type_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());

        let min_encoded = min_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);
        let max_encoded = max_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

                if let Some(ref min_enc) = min_encoded {
                    if val_bytes < min_enc.as_slice() {
                        continue;
                    }
                }
                if let Some(ref max_enc) = max_encoded {
                    if val_bytes > max_enc.as_slice() {
                        continue;
                    }
                }

                let mut edge_id_bytes = [0u8; 8];
                edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(edge_id_bytes));
            }
        }
        Ok(result)
    }

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
    pub fn shortest_path_dijkstra(
        &self,
        src: NodeId,
        dst: NodeId,
        _weight_property: &str,
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

    fn all_nodes_impl(&self, rtxn: &heed::RoTxn) -> Result<Vec<NodeId>, Error> {
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
    // Vector storage
    // ------------------------------------------------------------------

    /// Persist raw vector bytes for `n`.
    ///
    /// Vector search crates own vector decoding, validation, and indexing.
    /// `issundb-core` only owns the durable LMDB record.
    #[doc(hidden)]
    pub fn put_vector_bytes(&self, n: NodeId, bytes: &[u8]) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.put_vector_bytes_impl(&mut wtxn, n, bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    fn put_vector_bytes_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        n: NodeId,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.storage.vectors.put(wtxn, &n, bytes)?;
        Ok(())
    }

    /// Delete the raw vector bytes for `n` from LMDB. No-op if absent.
    #[doc(hidden)]
    pub fn delete_vector_bytes(&self, n: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_vector_bytes_impl(&mut wtxn, n)?;
        wtxn.commit()?;
        Ok(())
    }

    fn delete_vector_bytes_impl(&self, wtxn: &mut heed::RwTxn, n: NodeId) -> Result<(), Error> {
        self.storage.vectors.delete(wtxn, &n)?;
        Ok(())
    }

    /// Return all raw vector records in node ID order.
    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.vector_bytes_impl(&rtxn)
    }

    fn vector_bytes_impl(&self, rtxn: &heed::RoTxn) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        let mut out = Vec::new();
        for result in self.storage.vectors.iter(rtxn)? {
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
        self.maybe_spawn_rebuild_n(1);
    }

    fn maybe_spawn_rebuild_n(&self, count: usize) {
        if self.csr_cache.mark_dirty_n(count as u64) {
            let cache = Arc::clone(&self.csr_cache);
            let storage = Arc::clone(&self.storage);
            let matrices = Arc::clone(&self.matrices);
            std::thread::spawn(move || match CsrSnapshot::build(&storage) {
                Ok(snap) => {
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
    fn adj_entries(&self, node: NodeId, outgoing: bool) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.adj_entries_impl(&rtxn, node, outgoing)
    }

    fn adj_entries_impl(
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

    // ------------------------------------------------------------------
    // GraphBLAS Traversal Implementations
    // ------------------------------------------------------------------

    /// BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// Each iteration propagates the hop-level frontier one step by computing
    /// `A^T * level` with a structural complement mask that restricts writes to
    /// nodes not yet reached. The level vector is then extended with the new frontier.
    #[doc(hidden)]
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

    /// Multi-source BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// The `max_nodes` cap is applied both during seed seeding and during SpMV
    /// expansion so that the returned slice never exceeds the cap.
    #[doc(hidden)]
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

        self.ensure_matrices()?;

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return Ok(vec![]),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if seeds.is_empty() || n == 0 {
            return Ok(vec![]);
        }

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Seed the level vector.
        let mut seeds_added: usize = 0;
        for &start in seeds {
            if max_nodes.is_some_and(|max| seeds_added >= max) {
                break;
            }
            if let Some(&d) = snap.id_to_dense.get(&start) {
                level
                    .set_value(d as usize, 0)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                seeds_added += 1;
            }
        }

        if seeds_added == 0 {
            return Ok(vec![]);
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
    #[doc(hidden)]
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
                    for ne in neighbors {
                        if let Some(t) = rel_type {
                            let actual_name = self.type_name(ne.edge_type)?;
                            if actual_name.as_deref() != Some(t) {
                                continue;
                            }
                        }
                        results.push((src, ne.edge, ne.node));
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
            let type_id = {
                let rtxn = self.storage.env.read_txn()?;
                let meta_key = format!("type:{t}");
                match self.storage.meta.get(&rtxn, &meta_key)? {
                    Some(b) => {
                        let arr: [u8; 4] = b
                            .try_into()
                            .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                        u32::from_be_bytes(arr)
                    }
                    None => return Ok(vec![]),
                }
            };

            let mut results = Vec::new();
            for &src in src_nodes {
                let neighbors = if is_incoming {
                    self.in_neighbors(src)?
                } else {
                    self.out_neighbors(src)?
                };
                for ne in neighbors {
                    if ne.edge_type == type_id {
                        results.push((src, ne.edge, ne.node));
                    }
                }
            }
            return Ok(results);
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        // Propagate outgoing edges via the transposed adjacency matrix;
        // incoming edges use the original. See `bfs_multi_source_graphblas` for the derivation.
        let opts =
            OptionsForOperatorWithMatrixAsFirstArgument::new(true, false, false, !is_incoming);

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
                    .filter_map(|ne| {
                        snap.id_to_dense
                            .get(&ne.node)
                            .map(|&d| (d as usize, ne.edge))
                    })
                    .collect()
            } else {
                self.out_neighbors(src)?
                    .into_iter()
                    .filter_map(|ne| {
                        snap.id_to_dense
                            .get(&ne.node)
                            .map(|&d| (d as usize, ne.edge))
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
    #[doc(hidden)]
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

    /// PageRank via iterative SpMV over the column-stochastic matrix.
    ///
    /// Each iteration computes `raw = M * rank` using PlusTimes, then applies the
    /// damping formula `rank[i] = d * raw[i] + (1 - d) / n` in Rust. Dangling
    /// nodes (no incoming edges) receive only the teleportation term.
    #[doc(hidden)]
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
    #[doc(hidden)]
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
            for ne in in_neighbors {
                let pred_id = ne.node;
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

    /// Connected Components (WCC) using iterative label-propagation via SpMV.
    fn connected_components_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector, VectorElementList,
                operations::{
                    FromVectorElementList, GetSparseVectorElementIndices,
                    GetSparseVectorElementValue,
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Min,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinFirst,
            },
        };

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let init_list = VectorElementList::<i32>::from_element_vector(
            (0..n).map(|i| (i, i as i32).into()).collect(),
        );
        let mut label = SparseVector::<i32>::from_element_list(
            m.context.clone(),
            n,
            init_list,
            &graphblas_sparse_linear_algebra::operators::binary_operator::First::<i32>::new(),
        )
        .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_min = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_fwd = OptionsForOperatorWithMatrixAsFirstArgument::new_default();
        let opts_rev = OptionsForOperatorWithMatrixAsFirstArgument::new_default();
        let opts_merge = OperatorOptions::new_default();

        for _ in 0..n {
            let mut fwd = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinFirst::<i32>::new(),
                &label,
                &Assignment::new(),
                &mut fwd,
                &SelectEntireVector::new(m.context.clone()),
                &opts_fwd,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut rev = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency_t,
                &MinFirst::<i32>::new(),
                &label,
                &Assignment::new(),
                &mut rev,
                &SelectEntireVector::new(m.context.clone()),
                &opts_rev,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &fwd,
                    &Min::<i32>::new(),
                    &rev,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &label,
                    &Min::<i32>::new(),
                    &merged,
                    &Assignment::new(),
                    &mut next,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let new_indices = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let old_indices = label
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let mut changed = new_indices.len() != old_indices.len();
            if !changed {
                for &idx in &new_indices {
                    let new_v = next
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    let old_v = label
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    if new_v != old_v {
                        changed = true;
                        break;
                    }
                }
            }

            label = next;
            if !changed {
                break;
            }
        }

        let indices = label
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let mut result = HashMap::with_capacity(n);
        for idx in indices {
            let comp = label
                .element_value_or_default(idx)
                .map_err(|e| Error::GraphBLAS(e.to_string()))? as u64;
            if let Some(&node_id) = snap.dense_to_id.get(idx) {
                result.insert(node_id, comp);
            }
        }
        Ok(result)
    }

    /// Strongly Connected Components (SCC) using Tarjan's algorithm optimized over contiguous CSR snapshot arrays.
    fn strongly_connected_components_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        struct Env<'a> {
            snap: &'a CsrSnapshot,
            index: usize,
            indices: Vec<Option<usize>>,
            lowlinks: Vec<usize>,
            on_stack: Vec<bool>,
            stack: Vec<usize>,
            components: HashMap<NodeId, u64>,
            next_comp_id: u64,
        }

        let mut env = Env {
            snap,
            index: 0,
            indices: vec![None; n],
            lowlinks: vec![0; n],
            on_stack: vec![false; n],
            stack: Vec::with_capacity(n),
            components: HashMap::with_capacity(n),
            next_comp_id: 0,
        };

        fn strongconnect(u: usize, env: &mut Env) {
            env.indices[u] = Some(env.index);
            env.lowlinks[u] = env.index;
            env.index += 1;
            env.stack.push(u);
            env.on_stack[u] = true;

            let start = env.snap.row_ptr[u];
            let end = env.snap.row_ptr[u + 1];
            for k in start..end {
                let v = env.snap.col_idx[k] as usize;
                if env.indices[v].is_none() {
                    strongconnect(v, env);
                    env.lowlinks[u] = std::cmp::min(env.lowlinks[u], env.lowlinks[v]);
                } else if env.on_stack[v] {
                    if let Some(iv) = env.indices[v] {
                        env.lowlinks[u] = std::cmp::min(env.lowlinks[u], iv);
                    }
                }
            }

            if Some(env.lowlinks[u]) == env.indices[u] {
                let comp_id = env.next_comp_id;
                env.next_comp_id += 1;

                while let Some(w) = env.stack.pop() {
                    env.on_stack[w] = false;
                    if let Some(&node_id) = env.snap.dense_to_id.get(w) {
                        env.components.insert(node_id, comp_id);
                    }
                    if w == u {
                        break;
                    }
                }
            }
        }

        for u in 0..n {
            if env.indices[u].is_none() {
                strongconnect(u, &mut env);
            }
        }

        Ok(env.components)
    }

    /// Betweenness Centrality (Brandes' algorithm) using SpMV BFS frontier exploration.
    fn betweenness_centrality_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
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

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut betweenness = vec![0.0f64; n];

        for s in 0..n {
            let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set_value(s, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut sigma = vec![0u64; n];
            sigma[s] = 1;
            let mut levels: Vec<Vec<usize>> = vec![vec![s]];
            let mut pred: Vec<Vec<usize>> = vec![vec![]; n];
            let mut dist_vals: Vec<Option<i32>> = vec![None; n];
            dist_vals[s] = Some(0);

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

                let new_count = next
                    .number_of_stored_elements()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .element_indices()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;

                let mut level_nodes = Vec::with_capacity(new_indices.len());
                for &w in &new_indices {
                    dist_vals[w] = Some(hop);
                    level_nodes.push(w);
                }

                for &w in &new_indices {
                    if let Some(prev_level) = levels.last() {
                        for &v in prev_level {
                            let start = snap.row_ptr[v];
                            let end = snap.row_ptr[v + 1];
                            for k in start..end {
                                if snap.col_idx[k] as usize == w {
                                    sigma[w] += sigma[v];
                                    pred[w].push(v);
                                    break;
                                }
                            }
                        }
                    }
                }

                levels.push(level_nodes);

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
            }

            let mut delta = vec![0.0f64; n];
            for level in levels.iter().rev() {
                for &w in level {
                    if w == s {
                        continue;
                    }
                    let dw = delta[w];
                    for &v in &pred[w] {
                        if sigma[w] > 0 {
                            delta[v] += sigma[v] as f64 / sigma[w] as f64 * (1.0 + dw);
                        }
                    }
                    betweenness[w] += dw;
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

    /// Harmonic Centrality using all-pairs BFS distances computed via MinPlus SpMV.
    #[allow(clippy::needless_range_loop)]
    fn harmonic_centrality_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
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

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut centrality = vec![0.0f64; n];

        for src_dense in 0..n {
            let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set_value(src_dense, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut local_sum = 0.0f64;

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

                let new_count = next
                    .number_of_stored_elements()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .element_indices()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                for _ in &new_indices {
                    local_sum += 1.0 / hop as f64;
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
            }

            centrality[src_dense] = local_sum;
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, centrality[d]))
            .collect())
    }

    /// Degree Centrality via row/column reduces utilizing SpMV with standard arithmetic semiring.
    fn degree_centrality_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                sparse_matrix::SparseMatrix,
                sparse_vector::{
                    SparseVector, VectorElementList,
                    operations::{
                        FromVectorElementList, GetSparseVectorElementIndices,
                        GetSparseVectorElementValue,
                    },
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

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let ones_list = VectorElementList::<i32>::from_element_vector(
            (0..n).map(|i| (i, 1i32).into()).collect(),
        );
        let ones = SparseVector::<i32>::from_element_list(
            m.context.clone(),
            n,
            ones_list,
            &First::<i32>::new(),
        )
        .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let opts = OptionsForOperatorWithMatrixAsFirstArgument::new_default();

        let compute_degree = |matrix: &SparseMatrix<i32>| -> Result<Vec<u64>, Error> {
            let mut out = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                matrix,
                &PlusTimes::<i32>::new(),
                &ones,
                &Assignment::new(),
                &mut out,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut degrees = vec![0u64; n];
            let indices = out
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = out
                    .element_value_or_default(idx)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))? as u64;
                degrees[idx] = v;
            }
            Ok(degrees)
        };

        let out_degrees = if matches!(direction, DegreeDirection::Out | DegreeDirection::Both) {
            compute_degree(&m.adjacency)?
        } else {
            vec![0; n]
        };

        let in_degrees = if matches!(direction, DegreeDirection::In | DegreeDirection::Both) {
            compute_degree(&m.adjacency_t)?
        } else {
            vec![0; n]
        };

        let mut result = HashMap::with_capacity(n);
        for (dense, &node_id) in snap.dense_to_id.iter().enumerate() {
            let count = match direction {
                DegreeDirection::Out => out_degrees[dense],
                DegreeDirection::In => in_degrees[dense],
                DegreeDirection::Both => out_degrees[dense] + in_degrees[dense],
            };
            result.insert(node_id, count);
        }
        Ok(result)
    }

    /// Community Detection via Label Propagation (CDLP / LPA).
    fn label_propagation_graphblas(
        &self,
        _m: &MatrixSet,
        _snap: &CsrSnapshot,
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

    /// Dijkstra weighted shortest path from `src` to `dst` using MinPlus SpMV on `m.weight_matrix`.
    fn shortest_path_dijkstra_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<WeightedPath>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector,
                operations::{
                    GetSparseVectorElementIndices, GetSparseVectorElementValue,
                    SetSparseVectorElement,
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Min,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        if src == dst {
            return Ok(Some(WeightedPath {
                nodes: vec![src],
                total_weight: 0.0,
            }));
        }

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
        let ewise_min = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, false, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<f64>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0.0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mut dist_vals: Vec<Option<f64>> = vec![None; n];
        dist_vals[src_dense] = Some(0.0);

        for _ in 1..=n {
            let mut next = SparseVector::<f64>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.weight_matrix,
                &MinPlus::<f64>::new(),
                &dist,
                &Assignment::new(),
                &mut next,
                &SelectEntireVector::new(m.context.clone()),
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut merged = SparseVector::<f64>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &dist,
                    &Min::<f64>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let new_indices = merged
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let old_indices = dist
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut changed = new_indices.len() != old_indices.len();
            if !changed {
                for &idx in &new_indices {
                    let new_v = merged
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    let old_v = dist
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    if (new_v - old_v).abs() > 1e-9 {
                        changed = true;
                        break;
                    }
                }
            }

            dist = merged;
            if !changed {
                break;
            }
        }

        let indices = dist
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        for idx in indices {
            let v = dist
                .element_value_or_default(idx)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist_vals[idx] = Some(v);
        }

        let total_cost = match dist_vals[dst_dense] {
            Some(c) => c,
            None => return Ok(None),
        };

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
            for ne in in_neighbors {
                let (pred_id, edge_id) = (ne.node, ne.edge);
                if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                    let pred_d = pred_d as usize;
                    if let Some(pred_dist) = dist_vals[pred_d] {
                        let rtxn = self.storage.env.read_txn()?;
                        let weight = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                            let props_json: serde_json::Value = props::decode(&rec.props)?;
                            props_json
                                .get("weight")
                                .or_else(|| props_json.get("cost"))
                                .or_else(|| props_json.get("capacity"))
                                .or_else(|| props_json.get("cap"))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(1.0)
                        } else {
                            1.0
                        };
                        if (pred_dist + weight - cur_dist).abs() < 1e-9 {
                            path.push(pred_d);
                            cur = pred_d;
                            moved = true;
                            break;
                        }
                    }
                }
            }
            if !moved {
                return Ok(None);
            }
        }

        path.reverse();
        Ok(Some(WeightedPath {
            nodes: path.into_iter().map(|d| snap.dense_to_id[d]).collect(),
            total_weight: total_cost,
        }))
    }

    /// Depth-First Search (DFS) optimized over contiguous CSR snapshot arrays.
    fn dfs_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        start: NodeId,
        hops: u8,
    ) -> Result<Vec<NodeId>, Error> {
        let mut visited: AHashSet<NodeId> = AHashSet::new();
        let mut path: Vec<NodeId> = Vec::new();

        fn dfs_recurse(
            snap: &CsrSnapshot,
            node: NodeId,
            depth: u8,
            max_depth: u8,
            visited: &mut AHashSet<NodeId>,
            path: &mut Vec<NodeId>,
        ) {
            visited.insert(node);
            path.push(node);

            if depth < max_depth {
                if let Some(dense) = snap.id_to_dense.get(&node) {
                    let start_idx = snap.row_ptr[*dense as usize];
                    let end_idx = snap.row_ptr[*dense as usize + 1];
                    for k in start_idx..end_idx {
                        let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                        if !visited.contains(&neighbor) {
                            dfs_recurse(snap, neighbor, depth + 1, max_depth, visited, path);
                        }
                    }
                }
            }
        }

        dfs_recurse(snap, start, 0, hops, &mut visited, &mut path);
        Ok(path)
    }

    /// Directed Cycle Detection using 3-color DFS over contiguous CSR snapshot arrays.
    fn detect_cycle_graphblas(&self, _m: &MatrixSet, snap: &CsrSnapshot) -> Result<bool, Error> {
        let n = snap.dense_to_id.len();
        let mut state = vec![0u8; n]; // 0 = White, 1 = Gray, 2 = Black

        fn has_cycle(snap: &CsrSnapshot, u: usize, state: &mut Vec<u8>) -> bool {
            state[u] = 1; // Gray

            let start = snap.row_ptr[u];
            let end = snap.row_ptr[u + 1];
            for k in start..end {
                let v = snap.col_idx[k] as usize;
                if state[v] == 1 || (state[v] == 0 && has_cycle(snap, v, state)) {
                    return true;
                }
            }

            state[u] = 2; // Black
            false
        }

        for u in 0..n {
            if state[u] == 0 && has_cycle(snap, u, &mut state) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// All simple paths between `src` and `dst` using contiguous CSR snapshot arrays.
    fn all_paths_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
        let mut paths = Vec::new();
        let mut current_path = vec![src];
        let mut visited = AHashSet::new();
        visited.insert(src);

        fn find_paths(
            snap: &CsrSnapshot,
            u: NodeId,
            dst: NodeId,
            visited: &mut AHashSet<NodeId>,
            current_path: &mut Vec<NodeId>,
            paths: &mut Vec<Vec<NodeId>>,
        ) {
            if u == dst {
                paths.push(current_path.clone());
                return;
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let start = snap.row_ptr[u_dense as usize];
                let end = snap.row_ptr[u_dense as usize + 1];
                for k in start..end {
                    let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                    if !visited.contains(&neighbor) {
                        visited.insert(neighbor);
                        current_path.push(neighbor);
                        find_paths(snap, neighbor, dst, visited, current_path, paths);
                        current_path.pop();
                        visited.remove(&neighbor);
                    }
                }
            }
        }

        find_paths(snap, src, dst, &mut visited, &mut current_path, &mut paths);
        Ok(paths)
    }

    /// All unweighted shortest paths between `src` and `dst` using MinPlus SpMV distances.
    fn all_shortest_paths_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
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
            return Ok(vec![vec![src]]);
        }

        let n = m.n_nodes;
        let src_dense = match snap.id_to_dense.get(&src) {
            Some(&d) => d as usize,
            None => return Ok(vec![]),
        };
        let dst_dense = match snap.id_to_dense.get(&dst) {
            Some(&d) => d as usize,
            None => return Ok(vec![]),
        };

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

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
            return Ok(vec![]);
        }

        let mut paths = Vec::new();
        let mut current_path = vec![dst];

        fn reconstruct(
            graph: &Graph,
            snap: &CsrSnapshot,
            u: NodeId,
            src: NodeId,
            dist_vals: &[Option<i32>],
            current_path: &mut Vec<NodeId>,
            paths: &mut Vec<Vec<NodeId>>,
        ) -> Result<(), Error> {
            if u == src {
                let mut p = current_path.clone();
                p.reverse();
                paths.push(p);
                return Ok(());
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let cur_dist = match dist_vals[u_dense as usize] {
                    Some(d) => d,
                    None => return Ok(()),
                };
                let in_neighbors = graph.adj_entries(u, false)?;
                for ne in in_neighbors {
                    let pred_id = ne.node;
                    if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                        if dist_vals[pred_d as usize] == Some(cur_dist - 1) {
                            current_path.push(pred_id);
                            reconstruct(graph, snap, pred_id, src, dist_vals, current_path, paths)?;
                            current_path.pop();
                        }
                    }
                }
            }
            Ok(())
        }

        reconstruct(
            self,
            snap,
            dst,
            src,
            &dist_vals,
            &mut current_path,
            &mut paths,
        )?;
        Ok(paths)
    }

    /// Minimum/Maximum Spanning Forest (MSF) optimized over contiguous CSR snapshot arrays.
    fn spanning_forest_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        weight_property: &str,
        maximum: bool,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut edges = Vec::new();

        let n = snap.dense_to_id.len();
        for i in 0..n {
            let start = snap.row_ptr[i];
            let end = snap.row_ptr[i + 1];
            for k in start..end {
                let edge_id = snap.edge_id[k];
                let col = snap.col_idx[k] as usize;

                let weight = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                    let props_json: serde_json::Value = props::decode(&rec.props)?;
                    if let Some(val) = props_json.get(weight_property) {
                        val.as_f64().unwrap_or(1.0)
                    } else {
                        1.0
                    }
                } else {
                    1.0
                };

                edges.push((edge_id, snap.dense_to_id[i], snap.dense_to_id[col], weight));
            }
        }

        if maximum {
            edges.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            edges.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
        }

        let mut parent: HashMap<NodeId, NodeId> = HashMap::new();

        fn find(u: NodeId, parent: &mut HashMap<NodeId, NodeId>) -> NodeId {
            let mut root = u;
            while let Some(&p) = parent.get(&root) {
                if p == root {
                    break;
                }
                root = p;
            }
            let mut curr = u;
            while let Some(&p) = parent.get(&curr) {
                if p == curr {
                    break;
                }
                parent.insert(curr, root);
                curr = p;
            }
            root
        }

        fn union(u: NodeId, v: NodeId, parent: &mut HashMap<NodeId, NodeId>) -> bool {
            let root_u = find(u, parent);
            let root_v = find(v, parent);
            if root_u != root_v {
                parent.insert(root_u, root_v);
                true
            } else {
                false
            }
        }

        let mut forest = Vec::new();
        for (edge_id, src, dst, _) in edges {
            parent.entry(src).or_insert(src);
            parent.entry(dst).or_insert(dst);

            if union(src, dst, &mut parent) {
                forest.push(edge_id);
            }
        }

        Ok(forest)
    }

    /// Edmonds-Karp Maximum Flow algorithm utilizing contiguous CSR snapshot arrays.
    fn maximum_flow_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        source: NodeId,
        sink: NodeId,
        capacity_property: &str,
    ) -> Result<f64, Error> {
        if source == sink {
            return Ok(0.0);
        }

        let rtxn = self.storage.env.read_txn()?;
        let mut residual: HashMap<(NodeId, NodeId), f64> = HashMap::new();
        let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();

        let n = snap.dense_to_id.len();
        for i in 0..n {
            let start = snap.row_ptr[i];
            let end = snap.row_ptr[i + 1];
            for k in start..end {
                let edge_id = snap.edge_id[k];
                let col = snap.col_idx[k] as usize;

                let capacity = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                    let props_json: serde_json::Value = props::decode(&rec.props)?;
                    if let Some(val) = props_json.get(capacity_property) {
                        val.as_f64().unwrap_or(0.0)
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };

                if capacity > 0.0 {
                    let u = snap.dense_to_id[i];
                    let v = snap.dense_to_id[col];

                    *residual.entry((u, v)).or_insert(0.0) += capacity;
                    residual.entry((v, u)).or_insert(0.0);

                    adj.entry(u).or_default().push(v);
                    adj.entry(v).or_default().push(u);
                }
            }
        }

        for neighbors in adj.values_mut() {
            neighbors.sort_unstable();
            neighbors.dedup();
        }

        if !adj.contains_key(&source) || !adj.contains_key(&sink) {
            return Ok(0.0);
        }

        let mut max_flow = 0.0;

        loop {
            let mut parent = HashMap::new();
            let mut queue = std::collections::VecDeque::new();
            let mut visited = AHashSet::new();

            queue.push_back(source);
            visited.insert(source);

            let mut path_found = false;

            while let Some(curr) = queue.pop_front() {
                if curr == sink {
                    path_found = true;
                    break;
                }

                if let Some(neighbors) = adj.get(&curr) {
                    for &neighbor in neighbors {
                        if !visited.contains(&neighbor) {
                            if let Some(&cap) = residual.get(&(curr, neighbor)) {
                                if cap > 1e-9 {
                                    visited.insert(neighbor);
                                    parent.insert(neighbor, curr);
                                    queue.push_back(neighbor);
                                }
                            }
                        }
                    }
                }
            }

            if !path_found {
                break;
            }

            let mut bottleneck = f64::INFINITY;
            let mut curr = sink;
            while curr != source {
                let p = parent[&curr];
                let cap = residual[&(p, curr)];
                if cap < bottleneck {
                    bottleneck = cap;
                }
                curr = p;
            }

            let mut curr = sink;
            while curr != source {
                let p = parent[&curr];
                if let Some(cap) = residual.get_mut(&(p, curr)) {
                    *cap -= bottleneck;
                }
                if let Some(cap) = residual.get_mut(&(curr, p)) {
                    *cap += bottleneck;
                }
                curr = p;
            }

            max_flow += bottleneck;
        }

        Ok(max_flow)
    }

    /// Yen's K Shortest Paths algorithm optimized over contiguous CSR snapshot arrays.
    fn shortest_path_top_k_graphblas(
        &self,
        _m: &MatrixSet,
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

            use std::cmp::Ordering;

            #[derive(Debug, PartialEq)]
            struct MinNonNan(f64);

            impl Eq for MinNonNan {}

            impl PartialOrd for MinNonNan {
                fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                    Some(self.cmp(other))
                }
            }

            impl Ord for MinNonNan {
                fn cmp(&self, other: &Self) -> Ordering {
                    self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
                }
            }

            #[derive(Debug, PartialEq, Eq)]
            struct State {
                cost: std::cmp::Reverse<MinNonNan>,
                node: NodeId,
            }

            impl Ord for State {
                fn cmp(&self, other: &Self) -> Ordering {
                    self.cost.cmp(&other.cost)
                }
            }

            impl PartialOrd for State {
                fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                    Some(self.cmp(other))
                }
            }

            let mut dist: HashMap<NodeId, f64> = HashMap::new();
            let mut pred: HashMap<NodeId, NodeId> = HashMap::new();
            let mut heap = std::collections::BinaryHeap::new();

            dist.insert(s, 0.0);
            heap.push(State {
                cost: std::cmp::Reverse(MinNonNan(0.0)),
                node: s,
            });

            while let Some(State {
                cost: std::cmp::Reverse(MinNonNan(cost)),
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
                            heap.push(State {
                                cost: std::cmp::Reverse(MinNonNan(next_cost)),
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
                    .unwrap_or(std::cmp::Ordering::Equal)
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

    /// Longest Path between `src` and `dst` using contiguous CSR snapshot arrays.
    fn longest_path_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        let mut max_path: Option<Vec<NodeId>> = None;
        let mut current_path = vec![src];
        let mut visited = AHashSet::new();
        visited.insert(src);

        fn find_longest(
            snap: &CsrSnapshot,
            u: NodeId,
            dst: NodeId,
            visited: &mut AHashSet<NodeId>,
            current_path: &mut Vec<NodeId>,
            max_path: &mut Option<Vec<NodeId>>,
        ) {
            if u == dst {
                if let Some(max) = max_path.as_ref() {
                    if current_path.len() > max.len() {
                        *max_path = Some(current_path.clone());
                    }
                } else {
                    *max_path = Some(current_path.clone());
                }
                return;
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let start = snap.row_ptr[u_dense as usize];
                let end = snap.row_ptr[u_dense as usize + 1];
                for k in start..end {
                    let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                    if !visited.contains(&neighbor) {
                        visited.insert(neighbor);
                        current_path.push(neighbor);
                        find_longest(snap, neighbor, dst, visited, current_path, max_path);
                        current_path.pop();
                        visited.remove(&neighbor);
                    }
                }
            }
        }

        find_longest(
            snap,
            src,
            dst,
            &mut visited,
            &mut current_path,
            &mut max_path,
        );
        Ok(max_path)
    }
}

impl ReadTxn<'_> {
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        self.graph.get_node_impl(&self.rtxn, id)
    }

    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        self.graph.get_edge_impl(&self.rtxn, id)
    }

    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.out_neighbors_impl(&self.rtxn, node)
    }

    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.in_neighbors_impl(&self.rtxn, node)
    }

    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        self.graph.nodes_by_label_impl(&self.rtxn, label)
    }

    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        self.graph.edges_by_type_impl(&self.rtxn, etype)
    }

    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        self.graph.label_name_impl(&self.rtxn, id)
    }

    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        self.graph.type_name_impl(&self.rtxn, id)
    }

    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        self.graph.node_count_by_label_impl(&self.rtxn, label)
    }

    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        self.graph.edge_count_by_type_impl(&self.rtxn, etype)
    }

    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        self.graph.all_nodes_impl(&self.rtxn)
    }

    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        self.graph.vector_bytes_impl(&self.rtxn)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_property_index_impl(&self.rtxn, label, property)
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_impl(&self.rtxn, label, property, val)
    }

    pub fn nodes_by_property_range(
        &self,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_range_impl(&self.rtxn, label, property, min_val, max_val)
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_impl(&self.rtxn, etype, property, val)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_range_impl(&self.rtxn, etype, property, min_val, max_val)
    }

    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_text_index_impl(&self.rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        self.graph.fts_stats_impl(&self.rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        self.graph
            .fts_doc_len_impl(&self.rtxn, label, property, node_id)
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        self.graph
            .fts_postings_impl(&self.rtxn, label, property, term)
    }

    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        self.graph.active_text_indexes_impl(&self.rtxn)
    }
}

impl WriteTxn<'_> {
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        self.graph.get_node_impl(&self.wtxn, id)
    }

    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        self.graph.get_edge_impl(&self.wtxn, id)
    }

    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.out_neighbors_impl(&self.wtxn, node)
    }

    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.in_neighbors_impl(&self.wtxn, node)
    }

    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        self.graph.nodes_by_label_impl(&self.wtxn, label)
    }

    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        self.graph.edges_by_type_impl(&self.wtxn, etype)
    }

    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        self.graph.label_name_impl(&self.wtxn, id)
    }

    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        self.graph.type_name_impl(&self.wtxn, id)
    }

    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        self.graph.node_count_by_label_impl(&self.wtxn, label)
    }

    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        self.graph.edge_count_by_type_impl(&self.wtxn, etype)
    }

    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        self.graph.all_nodes_impl(&self.wtxn)
    }

    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        self.graph.vector_bytes_impl(&self.wtxn)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_property_index_impl(&self.wtxn, label, property)
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_impl(&self.wtxn, label, property, val)
    }

    pub fn nodes_by_property_range(
        &self,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_range_impl(&self.wtxn, label, property, min_val, max_val)
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_impl(&self.wtxn, etype, property, val)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_range_impl(&self.wtxn, etype, property, min_val, max_val)
    }

    pub fn add_node(&mut self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let node_id = self.graph.add_node_impl(&mut self.wtxn, label, props)?;
        self.mutations_count += 1;
        Ok(node_id)
    }

    pub fn update_node(&mut self, id: NodeId, props: &impl Serialize) -> Result<(), Error> {
        self.graph.update_node_impl(&mut self.wtxn, id, props)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn delete_node(&mut self, id: NodeId) -> Result<(), Error> {
        self.graph.delete_node_impl(&mut self.wtxn, id)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn delete_edge(&mut self, id: EdgeId) -> Result<(), Error> {
        self.graph.delete_edge_impl(&mut self.wtxn, id)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn add_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let edge_id = self
            .graph
            .add_edge_impl(&mut self.wtxn, src, dst, etype, props)?;
        self.mutations_count += 1;
        Ok(edge_id)
    }

    #[doc(hidden)]
    pub fn put_vector_bytes(&mut self, n: NodeId, bytes: &[u8]) -> Result<(), Error> {
        self.graph.put_vector_bytes_impl(&mut self.wtxn, n, bytes)?;
        self.mutations_count += 1;
        Ok(())
    }

    /// Delete the raw vector bytes for `n` from LMDB. No-op if absent.
    #[doc(hidden)]
    pub fn delete_vector_bytes(&mut self, n: NodeId) -> Result<(), Error> {
        self.graph.delete_vector_bytes_impl(&mut self.wtxn, n)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn create_node_text_index(&mut self, label: &str, property: &str) -> Result<(), Error> {
        self.graph.create_node_text_index_impl(
            &mut self.wtxn,
            label,
            property,
            Language::English,
        )?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn drop_node_text_index(&mut self, label: &str, property: &str) -> Result<(), Error> {
        self.graph
            .drop_node_text_index_impl(&mut self.wtxn, label, property)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.has_node_text_index_impl(rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_stats_impl(rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_doc_len_impl(rtxn, label, property, node_id)
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_postings_impl(rtxn, label, property, term)
    }

    pub fn create_node_text_index_with_language(
        &mut self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        self.graph
            .create_node_text_index_impl(&mut self.wtxn, label, property, lang)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.active_text_indexes_impl(rtxn)
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

    #[test]
    fn test_transaction_read_only() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({"name": "Alice"})).unwrap();
        let b = g.add_node("Person", &json!({"name": "Bob"})).unwrap();

        g.view(|txn| {
            let node_a = txn.get_node(a).unwrap().unwrap();
            let props_a: serde_json::Value = rmp_serde::from_slice(&node_a.props).unwrap();
            assert_eq!(props_a["name"], "Alice");

            let node_b = txn.get_node(b).unwrap().unwrap();
            let props_b: serde_json::Value = rmp_serde::from_slice(&node_b.props).unwrap();
            assert_eq!(props_b["name"], "Bob");

            let nodes = txn.all_nodes().unwrap();
            assert_eq!(nodes.len(), 2);

            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_transaction_write_commit() {
        let (_dir, g) = open_tmp();

        let (a, b) = g
            .update(|txn| {
                let a = txn.add_node("Person", &json!({"name": "Alice"})).unwrap();
                let b = txn.add_node("Person", &json!({"name": "Bob"})).unwrap();
                txn.add_edge(a, b, "KNOWS", &json!({"since": 2020}))
                    .unwrap();
                Ok((a, b))
            })
            .unwrap();

        // After commit, data should be in the DB
        let node_a = g.get_node(a).unwrap().unwrap();
        let props_a: serde_json::Value = rmp_serde::from_slice(&node_a.props).unwrap();
        assert_eq!(props_a["name"], "Alice");

        let neighbors = g.out_neighbors(a).unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node, b);
    }

    #[test]
    fn test_transaction_write_rollback() {
        let (_dir, g) = open_tmp();

        let res: Result<(), Error> = g.update(|txn| {
            txn.add_node("Person", &json!({"name": "Alice"})).unwrap();
            // Intentionally fail the transaction
            Err(Error::Corrupt("simulated failure"))
        });

        assert!(res.is_err());

        // The node should NOT be present in the database since the transaction rolled back
        let nodes = g.all_nodes().unwrap();
        assert_eq!(nodes.len(), 0);
    }

    // --- bfs_multi_source_graphblas ---
    //
    // Each test calls `rebuild_csr()` after mutating the graph so the GraphBLAS
    // adjacency matrix reflects the inserted edges before BFS is invoked.

    #[test]
    fn graphblas_multi_source_empty_seeds_returns_empty() {
        let (_dir, g) = open_tmp();
        g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let result = g.bfs_multi_source_graphblas(&[], 2, None).unwrap();
        assert!(result.is_empty());
    }

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

    #[test]
    fn graphblas_multi_source_handles_newly_added_seeds_via_dynamic_materialization() {
        let (_dir, g) = open_tmp();
        // Seed a is in the CSR; b is added after rebuild_csr (making snapshot/matrices stale).
        // The function must detect the new nodes, dynamically rebuild the CSR/matrices, and run successfully.
        let a = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        // b is inserted AFTER rebuild, so it makes the existing MatrixSet stale.
        let b = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();

        // Both seeds must appear in the result; d must be reachable from b via the dynamically rematerialized matrices.
        let result = g.bfs_multi_source_graphblas(&[a, b], 1, None).unwrap();
        assert!(result.contains(&a), "seed a must be present");
        assert!(result.contains(&b), "seed b must be present");
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
    fn label_count_unchanged_on_update_node() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);

        // update_node does not change the label; the count must stay at 1.
        g.update_node(id, &json!({"name": "Alice"})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
    }

    #[test]
    fn update_node_returns_not_found_for_missing_node() {
        let (_dir, g) = open_tmp();
        let res = g.update_node(9999, &json!({}));
        assert!(matches!(res, Err(Error::NodeNotFound(9999))));
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

    #[test]
    fn delete_edge_correctness() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let eid = g.add_edge(a, b, "KNOWS", &json!({})).unwrap();

        // 1. Verify exists
        assert!(g.get_edge(eid).unwrap().is_some());
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 1);

        // 2. Verify adjacency lists
        let out_neighs = g.out_neighbors(a).unwrap();
        assert_eq!(out_neighs.len(), 1);
        assert_eq!(out_neighs[0].node, b);
        assert_eq!(out_neighs[0].edge, eid);

        let in_neighs = g.in_neighbors(b).unwrap();
        assert_eq!(in_neighs.len(), 1);
        assert_eq!(in_neighs[0].node, a);
        assert_eq!(in_neighs[0].edge, eid);

        // 3. Delete the edge
        g.delete_edge(eid).unwrap();

        // 4. Verify gone
        assert!(g.get_edge(eid).unwrap().is_none());
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);

        // 5. Verify adjacency lists updated
        assert_eq!(g.out_neighbors(a).unwrap().len(), 0);
        assert_eq!(g.in_neighbors(b).unwrap().len(), 0);

        // 6. Idempotence: delete non-existent edge
        g.delete_edge(eid).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);
    }

    #[test]
    fn test_node_property_secondary_index_and_scans() {
        let (_dir, g) = open_tmp();

        // Add nodes
        let n1 = g
            .add_node("Person", &json!({"name": "Alice", "age": 30}))
            .unwrap();
        let n2 = g
            .add_node("Person", &json!({"name": "Bob", "age": 25}))
            .unwrap();
        let n3 = g
            .add_node("Person", &json!({"name": "Charlie", "age": 30}))
            .unwrap();
        let _n4 = g
            .add_node("Employee", &json!({"name": "Alice", "age": 40}))
            .unwrap();

        // Create index on Person(age)
        g.create_node_property_index("Person", "age").unwrap();

        // Check index exists
        assert!(g.has_node_property_index("Person", "age").unwrap());

        // Point queries
        let p30 = g
            .nodes_by_property("Person", "age", PropValue::Int(30))
            .unwrap();
        assert_eq!(p30.len(), 2);
        assert!(p30.contains(&n1));
        assert!(p30.contains(&n3));

        let p25 = g
            .nodes_by_property("Person", "age", PropValue::Int(25))
            .unwrap();
        assert_eq!(p25.len(), 1);
        assert!(p25.contains(&n2));

        // Range queries (e.g. age between 20 and 28)
        let pr = g
            .nodes_by_property_range(
                "Person",
                "age",
                Some(PropValue::Int(20)),
                Some(PropValue::Int(28)),
            )
            .unwrap();
        assert_eq!(pr.len(), 1);
        assert!(pr.contains(&n2));

        // Let's create an index on Person(name) to test string sorting/prefix
        g.create_node_property_index("Person", "name").unwrap();
        let p_alice = g
            .nodes_by_property("Person", "name", PropValue::Str("Alice".to_string()))
            .unwrap();
        assert_eq!(p_alice.len(), 1);
        assert!(p_alice.contains(&n1));
    }

    #[test]
    fn test_unique_property_constraint() {
        let (_dir, g) = open_tmp();

        // Create unique constraint on User(email)
        g.create_node_unique_constraint("User", "email").unwrap();

        // Add first user
        let _u1 = g
            .add_node(
                "User",
                &json!({"email": "user1@example.com", "name": "User 1"}),
            )
            .unwrap();

        // Add second user with duplicate email - should fail
        let res2 = g.add_node(
            "User",
            &json!({"email": "user1@example.com", "name": "User 2"}),
        );
        assert!(res2.is_err());
        assert!(matches!(
            res2.unwrap_err(),
            Error::UniqueConstraintViolation { .. }
        ));

        // Add second user with unique email - should succeed
        let u2 = g
            .add_node(
                "User",
                &json!({"email": "user2@example.com", "name": "User 2"}),
            )
            .unwrap();

        // Update u2 to have u1's email - should fail
        let update_res =
            g.update_node(u2, &json!({"email": "user1@example.com", "name": "User 2"}));
        assert!(update_res.is_err());
        assert!(matches!(
            update_res.unwrap_err(),
            Error::UniqueConstraintViolation { .. }
        ));
    }

    #[test]
    fn test_required_property_constraint() {
        let (_dir, g) = open_tmp();

        // Create required constraint on Task(title)
        g.create_node_required_constraint("Task", "title").unwrap();

        // Add task with title - should succeed
        let t1 = g
            .add_node("Task", &json!({"title": "Do homework", "done": false}))
            .unwrap();

        // Add task without title - should fail
        let res2 = g.add_node("Task", &json!({"done": false}));
        assert!(res2.is_err());
        assert!(matches!(
            res2.unwrap_err(),
            Error::RequiredConstraintViolation { .. }
        ));

        // Update t1 to remove title - should fail
        let update_res = g.update_node(t1, &json!({"done": true}));
        assert!(update_res.is_err());
        assert!(matches!(
            update_res.unwrap_err(),
            Error::RequiredConstraintViolation { .. }
        ));
    }

    #[test]
    fn test_index_cleanup_on_delete() {
        let (_dir, g) = open_tmp();

        // Create unique index on Account(number)
        g.create_node_unique_constraint("Account", "number")
            .unwrap();

        let a1 = g.add_node("Account", &json!({"number": "12345"})).unwrap();

        // Delete a1
        g.delete_node(a1).unwrap();

        // Now we should be able to reuse the account number because index was cleaned up!
        let a2 = g.add_node("Account", &json!({"number": "12345"}));
        assert!(a2.is_ok());
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        let dir = TempDir::new().unwrap();
        let backup_file = dir.path().join("snapshot.mdb");
        let restore_dir = dir.path().join("restored");

        // Write data.
        let n;
        {
            let g = Graph::open(&dir.path().join("primary"), 1).unwrap();
            n = g
                .add_node("BackupTest", &serde_json::json!({"x": 42}))
                .unwrap();
            g.backup(&backup_file).unwrap();
        }

        // Restore and verify.
        Graph::restore(&backup_file, &restore_dir).unwrap();
        let g2 = Graph::open(&restore_dir, 1).unwrap();
        let rec = g2
            .get_node(n)
            .unwrap()
            .expect("node must exist in restored graph");
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["x"], serde_json::json!(42));
    }
}
