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

pub mod algo;
pub mod edge;
pub mod fts_mod;
pub mod graphblas;
pub mod index;
pub mod node;
pub mod txn;
pub mod vector;

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
pub(super) fn composite_key(prefix: u32, id: u64) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&prefix.to_be_bytes());
    key[4..].copy_from_slice(&id.to_be_bytes());
    key
}

/// Encodes a JSON property value into a sortable byte representation for the index.
pub(super) fn encode_property_value(val: &serde_json::Value) -> Option<Vec<u8>> {
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
pub(super) fn decode_property_value(bytes: &[u8]) -> Option<serde_json::Value> {
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
pub(super) fn node_prop_index_key(
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
pub(super) fn edge_prop_index_key(
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
pub(super) fn fts_postings_key(label_id: LabelId, prop_key_id: PropKeyId, term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + term.len());
    key.extend_from_slice(&label_id.to_be_bytes());
    key.extend_from_slice(&prop_key_id.to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key
}

/// Builds a 12-byte FTS posting value `(node_id, frequency)`.
pub(super) fn fts_posting_val(node_id: NodeId, frequency: u32) -> [u8; 12] {
    let mut val = [0u8; 12];
    val[0..8].copy_from_slice(&node_id.to_be_bytes());
    val[8..12].copy_from_slice(&frequency.to_be_bytes());
    val
}

/// Parses a 12-byte FTS posting value into `(node_id, frequency)`.
pub(super) fn parse_fts_posting_val(bytes: &[u8]) -> Result<(NodeId, u32), Error> {
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
pub(super) fn fts_doc_key(label_id: LabelId, prop_key_id: PropKeyId, node_id: NodeId) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..4].copy_from_slice(&label_id.to_be_bytes());
    key[4..8].copy_from_slice(&prop_key_id.to_be_bytes());
    key[8..16].copy_from_slice(&node_id.to_be_bytes());
    key
}

/// Parses a 4-byte doc length value.
pub(super) fn parse_fts_doc_val(bytes: &[u8]) -> Result<u32, Error> {
    if bytes.len() != 4 {
        return Err(Error::Corrupt("fts doc val must be 4 bytes"));
    }
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
        Error::Corrupt("fts doc val: slice wrong size")
    })?))
}

pub(super) fn fts_stats_n_key(label_id: LabelId, prop_key_id: PropKeyId) -> String {
    format!("fts_stats:node:l:{label_id}:p:{prop_key_id}:N")
}

pub(super) fn fts_stats_sum_dl_key(label_id: LabelId, prop_key_id: PropKeyId) -> String {
    format!("fts_stats:node:l:{label_id}:p:{prop_key_id}:sum_dl")
}

/// The graph database handle. Cheap to clone: all state is behind `Arc`.
#[derive(Clone)]
pub struct Graph {
    pub(super) storage: Arc<Storage>,
    pub(super) _write_lock: Arc<ReentrantMutex<()>>,
    pub(super) csr_cache: Arc<CsrCache>,
    pub(super) matrices: Arc<parking_lot::RwLock<Option<MatrixSet>>>,
    /// Type-erased extension cache. Higher-level crates use this to attach
    /// caches (e.g. HNSW vector index) to a Graph without creating a circular
    /// dependency. Keys are `std::any::TypeId`; values are `Arc<dyn Any + Send + Sync>`.
    pub extensions: Arc<parking_lot::Mutex<AHashMap<StdTypeId, Box<dyn Any + Send + Sync>>>>,
}

/// A read-only transaction on the graph.
pub struct ReadTxn<'a> {
    pub(super) graph: &'a Graph,
    pub(super) rtxn: heed::RoTxn<'a, heed::WithTls>,
}

/// A read-write transaction on the graph.
pub struct WriteTxn<'a> {
    pub(super) graph: &'a Graph,
    pub(super) wtxn: heed::RwTxn<'a>,
    pub(super) mutations_count: usize,
}

impl Graph {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        let csr_file = path.join("csr_snapshot.bin");
        let initial = CsrSnapshot::build_mapped(&storage, &csr_file)?;
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
    ///
    /// The snapshot is serialized to `<db_dir>/csr_snapshot.bin` and then
    /// memory-mapped, enabling out-of-core traversal for graphs that exceed
    /// available RAM.  Falls back to an in-RAM snapshot if the file cannot be
    /// written.
    #[instrument(skip(self))]
    pub fn rebuild_csr(&self) -> Result<(), Error> {
        // Derive the CSR file path from the LMDB env path.
        let csr_file = self.storage.env.path().join("csr_snapshot.bin");
        let snap = CsrSnapshot::build_mapped(&self.storage, &csr_file)?;
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
}
