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

/// Type tag for a null value in the sortable property encoding.
pub(super) const ENCODED_NULL: u8 = 0x00;

/// Sign bit mask used to make IEEE-754 `f64` bit patterns and two's-complement
/// `i64` values sort in ascending numeric order as big-endian bytes.
const SORT_SIGN_BIT: u64 = 0x8000_0000_0000_0000;

/// Encodes a JSON property value into a sortable byte representation for the index.
///
/// Numbers use a fixed 17-byte encoding: a `0x03` tag, then 8 bytes of the
/// order-preserving `f64` bit pattern (the primary numeric sort key), then 8
/// bytes of an integer disambiguator. The disambiguator makes the encoding
/// lossless for `i64` values: two integers that round to the same `f64` (any
/// pair beyond 2^53) still produce distinct keys, while an integer and a float
/// of the same real value (e.g. `30` and `30.0`) produce identical keys so they
/// continue to compare equal. Keeping every numeric encoding the same length is
/// required because property lookups match by key prefix; a variable-length
/// encoding where one value is a prefix of another would yield false matches.
pub(super) fn encode_property_value(val: &serde_json::Value) -> Option<Vec<u8>> {
    match val {
        serde_json::Value::Null => Some(vec![ENCODED_NULL]),
        serde_json::Value::Bool(false) => Some(vec![0x01]),
        serde_json::Value::Bool(true) => Some(vec![0x02]),
        serde_json::Value::Number(num) => {
            let float_val = num.as_f64()?;
            let bits = float_val.to_bits();
            let masked = if (bits & SORT_SIGN_BIT) != 0 {
                !bits
            } else {
                bits ^ SORT_SIGN_BIT
            };
            // Integer disambiguator: for any number whose exact real value is an
            // integer in `i64` range, store that integer in sign-flipped
            // big-endian order so distinct large integers never collide. All
            // other numbers (non-integers, out-of-range) get a fixed sentinel;
            // they already have a unique `f64` bit pattern in the primary key,
            // so the sentinel value cannot affect ordering or equality.
            let int_disambig: u64 = if let Some(i) = num.as_i64() {
                (i as u64) ^ SORT_SIGN_BIT
            } else if float_val.fract() == 0.0
                && float_val >= i64::MIN as f64
                && float_val <= i64::MAX as f64
            {
                ((float_val as i64) as u64) ^ SORT_SIGN_BIT
            } else {
                0
            };
            let mut buf = Vec::with_capacity(17);
            buf.push(0x03);
            buf.extend_from_slice(&masked.to_be_bytes());
            buf.extend_from_slice(&int_disambig.to_be_bytes());
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
            // Numbers are `tag + 8-byte f64 sort key + 8-byte int disambiguator`.
            if bytes.len() < 17 {
                return None;
            }
            // Prefer the lossless integer disambiguator when it round-trips,
            // so large integers decode exactly rather than through `f64`.
            let mut int_arr = [0u8; 8];
            int_arr.copy_from_slice(&bytes[9..17]);
            let int_val = (u64::from_be_bytes(int_arr) ^ SORT_SIGN_BIT) as i64;

            let mut arr = [0u8; 8];
            arr.copy_from_slice(&bytes[1..9]);
            let masked = u64::from_be_bytes(arr);
            let bits = if (masked & SORT_SIGN_BIT) == 0 {
                !masked
            } else {
                masked ^ SORT_SIGN_BIT
            };
            let float_val = f64::from_bits(bits);

            // If the disambiguator's integer equals the float key, the value was
            // an integer (or integer-valued float): return it losslessly as an
            // integer. Non-integers store a sentinel whose sign-flipped form is
            // `i64::MIN`, which never matches a non-integer float key.
            if (int_val as f64) == float_val {
                Some(serde_json::Value::Number(int_val.into()))
            } else {
                serde_json::Number::from_f64(float_val).map(serde_json::Value::Number)
            }
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
    /// Type-erased extension cache. Higher-level crates attach caches (e.g. the
    /// HNSW vector index) to a Graph without creating a circular dependency,
    /// through the `get_extension`, `set_extension`, and
    /// `get_or_init_extension_with` methods. Keys are `std::any::TypeId`; values
    /// are `Arc<dyn Any + Send + Sync>`.
    pub(crate) extensions: Arc<parking_lot::Mutex<AHashMap<StdTypeId, Box<dyn Any + Send + Sync>>>>,
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
    /// Structural mutations staged during this transaction, flushed to the
    /// `CsrCache` only on commit so an aborted transaction records nothing.
    pub(super) delta: crate::csr::GraphDelta,
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

    /// Return the extension of type `T`, initializing it with `init` if absent.
    ///
    /// `init` runs without the extensions lock held, so it may call back into
    /// the graph (for example, to read from storage) without risking a lock
    /// ordering problem. If two threads initialize concurrently, both may run
    /// `init`, but only the first stored value is kept and every caller observes
    /// that same `Arc`. `init` is fallible; on error nothing is stored and the
    /// error is propagated.
    pub fn get_or_init_extension_with<T, E, F>(&self, init: F) -> Result<Arc<T>, E>
    where
        T: Any + Send + Sync,
        F: FnOnce() -> Result<Arc<T>, E>,
    {
        if let Some(existing) = self.get_extension::<T>() {
            return Ok(existing);
        }
        let value = init()?;
        let mut ext = self.extensions.lock();
        // Another thread may have initialized while we built ours; prefer the
        // already-stored value so all callers share one instance.
        if let Some(existing) = ext
            .get(&StdTypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<Arc<T>>())
        {
            return Ok(existing.clone());
        }
        ext.insert(StdTypeId::of::<T>(), Box::new(value.clone()));
        Ok(value)
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
            delta: crate::csr::GraphDelta::default(),
        };
        match f(&mut txn) {
            Ok(val) => {
                let mutations_count = txn.mutations_count;
                let delta = std::mem::take(&mut txn.delta);
                txn.wtxn.commit()?;
                self.csr_cache.record_batch(delta);
                if mutations_count > 0 {
                    self.maybe_spawn_rebuild_n(mutations_count);
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
        // Capture the generation before reading LMDB so writes that land during
        // the build leave the snapshot conservatively stale.
        let built_gen = self.csr_cache.current_gen();
        // Clear the delta before reading LMDB: writes that commit during the
        // build land in the emptied delta and are re-applied incrementally later
        // (idempotently) rather than lost.
        self.csr_cache.clear_delta();
        let snap = CsrSnapshot::build_mapped(&self.storage, &csr_file)?;
        let m = MatrixSet::materialize(&snap)?;
        *self.matrices.write() = Some(m);
        self.csr_cache.install_full(snap, built_gen);
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

#[cfg(test)]
mod encode_tests {
    use serde_json::json;

    use super::{decode_property_value, encode_property_value};

    /// Distinct integers beyond 2^53 must encode to distinct keys. Encoding
    /// purely through `f64` (the previous behavior) collapsed them, causing
    /// index collisions and wrong `nodes_by_property` matches.
    #[test]
    fn large_integers_do_not_collide() {
        let a = encode_property_value(&json!(9_007_199_254_740_992_i64)).unwrap(); // 2^53
        let b = encode_property_value(&json!(9_007_199_254_740_993_i64)).unwrap(); // 2^53 + 1
        assert_ne!(a, b, "distinct large integers must encode distinctly");
    }

    /// An integer and the float of the same real value must encode identically
    /// so they keep comparing equal in the index (Cypher treats `30 = 30.0`).
    #[test]
    fn integer_and_equal_float_unify() {
        assert_eq!(
            encode_property_value(&json!(30)).unwrap(),
            encode_property_value(&json!(30.0)).unwrap(),
        );
        assert_eq!(
            encode_property_value(&json!(0)).unwrap(),
            encode_property_value(&json!(0.0)).unwrap(),
        );
    }

    /// Every numeric encoding must be the same length: property lookups match by
    /// key prefix, so a value whose encoding prefixes another's would alias.
    #[test]
    fn numeric_encoding_is_fixed_length() {
        for v in [
            json!(1),
            json!(-1),
            json!(0),
            json!(i64::MAX),
            json!(i64::MIN),
            json!(3.5),
            json!(-2.5e10),
        ] {
            assert_eq!(encode_property_value(&v).unwrap().len(), 17, "value {v}");
        }
    }

    /// Byte-lexicographic order of encodings must match numeric order, including
    /// across the 2^53 boundary where the disambiguator orders the tie.
    #[test]
    fn numeric_ordering_preserved() {
        let ascending: Vec<i64> = vec![
            i64::MIN,
            -1_000,
            -1,
            0,
            1,
            1_000,
            1 << 53,
            (1 << 53) + 1,
            i64::MAX,
        ];
        let encoded: Vec<Vec<u8>> = ascending
            .iter()
            .map(|v| encode_property_value(&json!(v)).unwrap())
            .collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(encoded, sorted, "encodings must sort in numeric order");
    }

    /// Large integers must decode back to the exact integer, not a rounded float.
    #[test]
    fn decode_round_trips_large_integer() {
        for v in [
            json!(0),
            json!(-1),
            json!(9_007_199_254_740_993_i64),
            json!(i64::MAX),
        ] {
            let enc = encode_property_value(&v).unwrap();
            assert_eq!(decode_property_value(&enc), Some(v.clone()), "value {v}");
        }
    }
}
