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
            get_prop_key_name, get_type,
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
pub mod stats;
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

/// Pattern description for [`Graph::count_triangle_cycles`]: the directed
/// cycle `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` with an optional relationship
/// type per hop and an optional label per node variable. `None` means
/// unconstrained.
#[derive(Debug, Clone, Default)]
pub struct TriangleCountSpec<'a> {
    /// Relationship types for the hops `a -> b`, `b -> c`, and `c -> a`.
    pub rel_types: [Option<&'a str>; 3],
    /// Labels required on `a`, `b`, and `c`.
    pub labels: [Option<&'a str>; 3],
}

/// Pattern description for [`Graph::count_linear_paths`]: an open directed
/// path of one or two hops, `(v0)-[t1]->(v1)` or
/// `(v0)-[t1]->(v1)-[t2]->(v2)`, with an optional relationship type per hop
/// and an optional label per node variable. `None` means unconstrained.
///
/// `rel_types.len()` is the hop count (1 or 2); `labels.len()` is the node
/// count (hop count plus one). The two-hop count follows Cypher MATCH
/// relationship-uniqueness semantics: the two relationships must be distinct,
/// which only constrains self-loop assignments where one edge could fill both
/// hops.
#[derive(Debug, Clone, Default)]
pub struct PathCountSpec<'a> {
    /// Relationship type per hop, in path order. Length 1 or 2.
    pub rel_types: Vec<Option<&'a str>>,
    /// Label per node variable, in path order. Length is `rel_types.len() + 1`.
    pub labels: Vec<Option<&'a str>>,
    /// Optional explicit allow-set of node ids per variable, in path order. A
    /// `Some(ids)` entry restricts that variable to `ids` (intersected with its
    /// label, if any); `None` leaves it unconstrained beyond the label. The
    /// caller resolves these sets by pushing per-vertex property predicates down
    /// into index lookups, so a filtered path count stays a kernel call instead
    /// of materializing rows. An empty vector (the default) means no variable is
    /// constrained, identical to the unfiltered path count.
    pub vertex_allow: Vec<Option<Vec<NodeId>>>,
}

/// Pattern description for [`Graph::grouped_edge_counts`]: count typed edges
/// grouped by one endpoint. With `group_is_dst`, edges are grouped by their
/// destination and the source is the counted endpoint (in-degree per
/// destination); otherwise edges are grouped by their source and the
/// destination is counted (out-degree per source). `group_label` and
/// `counted_label` optionally constrain each endpoint (`None` is
/// unconstrained). `counted_nonnull_prop` counts an edge only when the counted
/// endpoint's property is non-null (the semantics of `count(v.prop)` over the
/// expansion); `None` counts every qualifying edge (the semantics of
/// `count(*)` or `count(v)`, where a bound node variable is never null).
#[derive(Debug, Clone, Default)]
pub struct GroupedDegreeSpec<'a> {
    /// Relationship type to count, or `None` for any type.
    pub rel_type: Option<&'a str>,
    /// Group by the edge destination (count incoming) when true; by the edge
    /// source (count outgoing) when false.
    pub group_is_dst: bool,
    /// Label required on the group endpoint.
    pub group_label: Option<&'a str>,
    /// Label required on the counted endpoint.
    pub counted_label: Option<&'a str>,
    /// Explicit allow-set the counted endpoint must belong to, intersected with
    /// `counted_label`; `None` leaves it unconstrained beyond the label. The
    /// caller resolves this set by pushing a per-vertex property predicate down
    /// into index lookups, as [`PathCountSpec::vertex_allow`] does, so a filtered
    /// grouped count stays a kernel call. An empty slice counts zero.
    pub counted_allow: Option<&'a [NodeId]>,
    /// Property that must be non-null on the counted endpoint for an edge to
    /// count; `None` counts every qualifying edge.
    pub counted_nonnull_prop: Option<&'a str>,
}

/// Pattern description for [`Graph::typed_neighbor_counts`]: per-source counts
/// of typed neighbors across one hop. `incoming` follows incoming edges instead
/// of outgoing ones. A neighbor qualifies when it carries every label in
/// `neighbor_labels` (an empty slice is unconstrained) and, when
/// `neighbor_allow` is present, is a member of that set; it adds to the counted
/// total only when `neighbor_nonnull_prop` is absent or non-null on it (the
/// semantics of `count(v.prop)` over the expansion, against `count(*)`).
#[derive(Debug, Clone, Default)]
pub struct NeighborCountSpec<'a> {
    /// Relationship type to follow, or `None` for any type.
    pub rel_type: Option<&'a str>,
    /// Follow incoming edges (neighbors are edge sources) instead of outgoing.
    pub incoming: bool,
    /// Labels a neighbor must all carry to qualify.
    pub neighbor_labels: &'a [&'a str],
    /// Explicit allow-set a neighbor must belong to, intersected with the labels
    /// above; `None` leaves the neighbor unconstrained beyond its labels. The
    /// caller resolves this set by evaluating per-neighbor property predicates
    /// itself, so a filtered count stays a kernel call instead of materializing
    /// one entry per traversed edge, exactly as
    /// [`PathCountSpec::vertex_allow`] does for the path count. An empty slice
    /// admits no neighbor and counts zero.
    pub neighbor_allow: Option<&'a [NodeId]>,
    /// Property that must be non-null on a qualifying neighbor for it to add to
    /// the counted total; `None` counts every qualifying neighbor.
    pub neighbor_nonnull_prop: Option<&'a str>,
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

/// Maximum string length (in bytes) that can be auto-indexed. The property
/// index key is `(label_id, prop_key_id, encoded_val, node_id)`, so it carries
/// 16 bytes of fixed fields plus the 2-byte string-encoding frame (`0x04` tag
/// and `0x00` terminator) around the value. LMDB's default maximum key size is
/// 511 bytes; a string longer than this would overflow that limit and cannot be
/// indexed, so `encode_property_value` declines it and the value is left
/// unindexed (equality lookups fall back to a scan, and long text belongs in a
/// full-text index anyway). The bound is conservative to leave headroom.
pub(super) const MAX_INDEXED_STRING_LEN: usize = 480;

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
            // A string too long to fit an LMDB key cannot be indexed; decline it
            // so the property is left unindexed rather than crashing the write.
            if s.len() > MAX_INDEXED_STRING_LEN {
                return None;
            }
            let mut buf = Vec::with_capacity(1 + s.len() + 1);
            buf.push(0x04);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0x00);
            Some(buf)
        }
        _ => None, // Skip arrays and objects
    }
}

/// Comparable-type family of an encoded property value's leading type tag.
/// Booleans span two tags (`0x01` false, `0x02` true) but form one comparable
/// family; every other tag is its own family. Range scans compare only values
/// within the bound's family, because under openCypher a value of one type
/// never satisfies a range bound of another (a string is not comparable to a
/// numeric bound), even though the tagged encoding orders them globally.
pub(super) fn encoded_tag_family(tag: u8) -> u8 {
    match tag {
        0x02 => 0x01,
        t => t,
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

/// Returns the trailing 8-byte id from a property-index key, but only when the
/// key's encoded-value segment equals `encoded` exactly.
///
/// A property-index key is `(prefix u32, prop_key_id u32, encoded_val, id u64)`,
/// so the value segment is `key[8 .. len - 8]`. A prefix scan on
/// `(prefix, prop_key_id, encoded)` also matches keys whose value merely *starts*
/// with `encoded`: for the NUL-terminated string encoding, a stored `"a\0"`
/// (encoded `04 61 00 00`) is matched by a lookup for `"a"` (encoded `04 61 00`),
/// because a small id has leading zero bytes. Requiring the value segment to
/// equal `encoded` exactly rejects those collisions so equality lookups and
/// unique-constraint checks never conflate distinct string values. Fixed-width
/// encodings (numbers, bools, null) are already exact, so this never rejects a
/// genuine match. Returns `None` when the key is too short or the value differs.
pub(super) fn exact_prop_index_id(key: &[u8], encoded: &[u8]) -> Option<NodeId> {
    if key.len() < 8 + 8 {
        return None;
    }
    if &key[8..key.len() - 8] != encoded {
        return None;
    }
    let id_bytes: [u8; 8] = key[key.len() - 8..].try_into().ok()?;
    Some(u64::from_be_bytes(id_bytes))
}

/// Whether a stored string lies within the range `[lo, hi]` (or the open
/// variants), using byte-wise comparison, which is the same order the
/// order-preserving index encoding reproduces (`"a" < "a\0" < "ab"`). A `None`
/// bound is unbounded on that side. Backs the string-range label-scan fallback
/// for values too long to index.
pub(super) fn str_in_range(
    s: &str,
    lo: Option<&str>,
    lo_inclusive: bool,
    hi: Option<&str>,
    hi_inclusive: bool,
) -> bool {
    if let Some(lo) = lo {
        if lo_inclusive {
            if s < lo {
                return false;
            }
        } else if s <= lo {
            return false;
        }
    }
    if let Some(hi) = hi {
        if hi_inclusive {
            if s > hi {
                return false;
            }
        } else if s >= hi {
            return false;
        }
    }
    true
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
    pub(super) prop_columns: Arc<crate::columns::ColumnsCache<crate::columns::NodeSource>>,
    pub(super) edge_columns: Arc<crate::columns::ColumnsCache<crate::columns::EdgeSource>>,
    /// Per-`(label, type)` edge frequencies backing the optimizer's per-source-label
    /// expand-ratio estimate, recomputed lazily when committed writes advance past
    /// the cached generation. See [`crate::graph::stats`].
    pub(super) edge_fanout: Arc<parking_lot::Mutex<Option<crate::graph::stats::EdgeFanout>>>,
    pub(super) n_threads: Arc<std::sync::atomic::AtomicI32>,
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

thread_local! {
    /// Identity of the LMDB environment whose `Graph::update` closure this
    /// thread is currently inside (0 when none). LMDB permits only one active
    /// writer transaction per environment; a stray call to an auto-committing
    /// `Graph` mutation method on the SAME environment (which opens its own
    /// writer transaction) while this is set would block forever on the
    /// writer lock `Graph::update` already holds. Keyed by environment so
    /// mutating a different, independent `Graph` inside the closure (a safe
    /// pattern, e.g. copying between databases) does not trip the assert.
    /// Checked at the top of every auto-committing mutation method and of
    /// `Graph::update` itself, so a missed conversion to the `WriteTxn`-based
    /// method (or a nested `update` on the same graph) becomes an immediate,
    /// precisely located debug-build panic instead of a silent hang.
    static IN_WRITE_TXN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct WriteTxnGuard {
    previous: usize,
}

impl WriteTxnGuard {
    fn enter(env_id: usize) -> Self {
        let previous = IN_WRITE_TXN.with(|f| f.replace(env_id));
        WriteTxnGuard { previous }
    }
}

impl Drop for WriteTxnGuard {
    fn drop(&mut self) {
        IN_WRITE_TXN.with(|f| f.set(self.previous));
    }
}

impl Graph {
    /// A stable per-environment identity for the deadlock tripwire.
    fn write_txn_env_id(&self) -> usize {
        Arc::as_ptr(&self.storage) as usize
    }

    fn debug_assert_not_in_write_txn(&self) {
        debug_assert!(
            IN_WRITE_TXN.with(|f| f.get()) != self.write_txn_env_id(),
            "an auto-committing Graph method (or a nested Graph::update) was called while a \
             WriteTxn from Graph::update was already open on this graph on this thread; call \
             the WriteTxn method instead to avoid a same-thread deadlock on LMDB's \
             single-writer lock"
        );
    }
}

impl Graph {
    /// Open (creating if absent) the database at `path`.
    ///
    /// `map_size_gb` is the size of the LMDB memory map, and it is an upper bound
    /// on how large the database may grow for the lifetime of this handle, not an
    /// allocation: LMDB reserves the address range and commits pages as they are
    /// written, so a large value costs virtual address space rather than disk or
    /// RAM. There is no resize path. Once the data exceeds the bound, every write
    /// fails with the underlying `MDB_MAP_FULL` through [`Error::Storage`] until
    /// the database is reopened with a larger value, which is safe to do and keeps
    /// the existing data. Size it for the eventual database, not the current one.
    ///
    /// Opening builds none of the derived structures; see the comment inside.
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        // Older versions persisted the CSR snapshot next to the LMDB files but
        // never read it back; remove the stale artifact if one is present.
        let _ = std::fs::remove_file(path.join("csr_snapshot.bin"));
        let storage = Arc::new(storage);
        // Opening builds nothing. The CSR snapshot and the GraphBLAS matrices
        // are materialized by the freshness gates (`ensure_snapshot_fresh`,
        // `ensure_matrix_view`, and `ensure_csr_fresh`) when a consumer that
        // needs them first runs, and every such consumer already calls its gate.
        // Building them here instead cost one full edge scan plus a full matrix
        // materialization on every open, which is time a workload of point
        // lookups, property reads, or point adjacency never uses: those paths
        // read LMDB directly. On a large database that eager work dominates the
        // whole session's latency (roughly 26 s to open a 1 M-node, 14 M-edge
        // graph), and it is repaid on every reopen.
        let csr_cache = Arc::new(CsrCache::new_unbuilt());
        let matrices = Arc::new(parking_lot::RwLock::new(None));
        Ok(Self {
            storage,
            _write_lock: Arc::new(ReentrantMutex::new(())),
            csr_cache,
            matrices,
            prop_columns: Arc::new(crate::columns::ColumnsCache::default()),
            edge_columns: Arc::new(crate::columns::ColumnsCache::default()),
            edge_fanout: Arc::new(parking_lot::Mutex::new(None)),
            n_threads: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            extensions: Arc::new(parking_lot::Mutex::new(AHashMap::new())),
        })
    }

    /// Set the thread count for GraphBLAS matrix computations, overriding the
    /// `ISSUNDB_NUM_THREADS` environment variable. Set to 0 to restore the default
    /// behavior, which resolves through `threads::resolve`: `ISSUNDB_NUM_THREADS`,
    /// then `OMP_NUM_THREADS`, then the machine's parallelism. Every parallel
    /// consumer (the GraphBLAS pool and the counting kernels) shares that
    /// resolution, so this one knob has one meaning.
    pub fn set_thread_count(&self, n: i32) -> Result<(), Error> {
        self.n_threads
            .store(n, std::sync::atomic::Ordering::Release);
        // `MatrixSet::materialize` reads this value and applies it when it builds
        // the matrices, which is also the call that initializes the GraphBLAS
        // context. Setting the live thread pool is therefore only possible once
        // that has happened: before the first materialization GraphBLAS is not
        // initialized and setting a global option fails. Since `open` no longer
        // materializes eagerly, a caller that configures threads up front hits
        // exactly that window, so the stored value carries the setting instead.
        if self.matrices.read().is_some() {
            // Resolve rather than forward `n`: zero means "restore the default",
            // which this method documents, and only `threads::resolve` knows what
            // that is. Resolving also clamps, so the live pool cannot diverge from
            // what the next materialization would pick.
            let resolved = crate::threads::resolve(n) as i32;
            issundb_graphblas::set_global_threads(resolved)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        }
        Ok(())
    }

    /// Read one property of a node as the `serde_json::Value` that decoding the
    /// stored record would give. Returns `None` for a nonexistent node and
    /// `Some(Value::Null)` for a missing property. Either way the result
    /// reflects committed state.
    ///
    /// Served through the in-memory property columns once they exist, refreshing
    /// them against pending writes first; while they are absent the read goes
    /// straight to storage instead of building them (see
    /// [`crate::columns::ColumnsCache::should_serve_directly`]).
    pub fn node_prop_json(
        &self,
        id: NodeId,
        prop: &str,
    ) -> Result<Option<serde_json::Value>, Error> {
        // One property of one node is an LMDB point read. Serving it by building
        // every column costs a full node scan, which is the wrong trade for a
        // point query; the read only goes through the columns once they exist,
        // or once enough direct reads have amortized building them.
        if self.prop_columns.should_serve_directly(1) {
            let Some(obj) = self.direct_node_props(id)? else {
                return Ok(None);
            };
            return Ok(Some(
                obj.get(prop).cloned().unwrap_or(serde_json::Value::Null),
            ));
        }
        self.prop_columns.with_fresh(&self.storage, |cols| {
            cols.id_to_dense.get(&id).map(|&d| {
                cols.cols
                    .get(prop)
                    .and_then(|c| c.get_json_opt(d as usize))
                    .unwrap_or(serde_json::Value::Null)
            })
        })
    }

    /// Bulk form of [`Graph::node_prop_json`]: gather `props` for each id in
    /// `ids` through the in-memory property columns, row-major (`out[i][j]` is
    /// `props[j]` on `ids[i]`). One columns refresh covers the whole gather,
    /// and each id resolves to its dense index once. A missing property reads
    /// as `Value::Null`; a nonexistent node is [`Error::NodeNotFound`].
    pub fn node_props_json_table(
        &self,
        ids: &[NodeId],
        props: &[&str],
    ) -> Result<Vec<Vec<serde_json::Value>>, Error> {
        if self.prop_columns.should_serve_directly(ids.len()) {
            // One transaction for the whole gather, so the request is a single
            // point in time and pays one begin/end pair rather than one per id.
            return self
                .direct_node_props_many(ids)?
                .into_iter()
                .zip(ids)
                .map(|(obj, &id)| {
                    let obj = obj.ok_or(Error::NodeNotFound(id))?;
                    Ok(props
                        .iter()
                        .map(|p| obj.get(*p).cloned().unwrap_or(serde_json::Value::Null))
                        .collect())
                })
                .collect();
        }
        self.prop_columns
            .with_fresh(&self.storage, |cols| cols.props_table(ids, props))?
    }

    /// Single-property column form of [`Graph::node_props_json_table`]:
    /// `out[i]` is the value of `prop` on `ids[i]`, as one flat vector, so a
    /// bulk single-property gather does not pay one row vector allocation per
    /// id. A missing property reads as `Value::Null`; a nonexistent node is
    /// [`Error::NodeNotFound`].
    pub fn node_prop_json_column(
        &self,
        ids: &[NodeId],
        prop: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        if self.prop_columns.should_serve_directly(ids.len()) {
            return self
                .direct_node_props_many(ids)?
                .into_iter()
                .zip(ids)
                .map(|(obj, &id)| {
                    let obj = obj.ok_or(Error::NodeNotFound(id))?;
                    Ok(obj.get(prop).cloned().unwrap_or(serde_json::Value::Null))
                })
                .collect();
        }
        self.prop_columns
            .with_fresh(&self.storage, |cols| cols.prop_column(ids, prop))?
    }

    /// One node's user properties decoded from storage, the way the column
    /// build decodes them, so a gather served directly and the same gather
    /// served through the columns cannot disagree. `None` if the node is gone.
    fn direct_node_props(&self, id: NodeId) -> Result<Option<serde_json::Value>, Error> {
        <crate::columns::NodeSource as crate::columns::ColumnSource>::fetch_one(&self.storage, id)
    }

    /// [`Graph::direct_node_props`] for many ids under one transaction, in input
    /// order. `None` for a node that is gone.
    fn direct_node_props_many(
        &self,
        ids: &[NodeId],
    ) -> Result<Vec<Option<serde_json::Value>>, Error> {
        <crate::columns::NodeSource as crate::columns::ColumnSource>::fetch_many(&self.storage, ids)
    }

    /// Whether each of `ids` carries a non-null value for `prop`, in input order.
    ///
    /// A node that is not there reads as absent rather than raising, because the
    /// callers' ids come from the CSR snapshot, which can lag a deletion; treating
    /// the gap as a null value is what the row pipeline would effectively produce
    /// for a row a stale snapshot should no longer have offered.
    ///
    /// Honors the same small-request path the property gathers do, so resolving
    /// presence for a handful of nodes costs a handful of point reads instead of
    /// one full scan to build every column. That is the whole point of it existing
    /// separately: the counting kernels need presence for the neighbors they
    /// actually visit, not a dense mask over the entire graph.
    pub(super) fn nodes_prop_present(
        &self,
        ids: &[NodeId],
        prop: &str,
    ) -> Result<Vec<bool>, Error> {
        if self.prop_columns.should_serve_directly(ids.len()) {
            return Ok(self
                .direct_node_props_many(ids)?
                .into_iter()
                .map(|obj| obj.is_some_and(|o| o.get(prop).is_some_and(|v| !v.is_null())))
                .collect());
        }
        self.prop_columns.with_fresh(&self.storage, |cols| {
            ids.iter()
                .map(|id| match (cols.id_to_dense.get(id), cols.cols.get(prop)) {
                    (Some(&d), Some(col)) => col.is_present(d as usize),
                    // Either the columns never saw this entity or no such property
                    // exists anywhere; both read as null.
                    _ => false,
                })
                .collect()
        })
    }

    /// Group `ids` by the exact value of `prop` through the in-memory
    /// property columns: one dense group code per id, plus one representative
    /// value per code (the first occurrence). Null and missing property
    /// values share one code represented by `Value::Null`; a nonexistent node
    /// is [`Error::NodeNotFound`]. Codes are assigned under value identity,
    /// which for the typed columns needs no per-row value materialization.
    pub fn node_prop_group_codes(
        &self,
        ids: &[NodeId],
        prop: &str,
    ) -> Result<(Vec<u32>, Vec<serde_json::Value>), Error> {
        self.prop_columns
            .with_fresh(&self.storage, |cols| cols.group_codes(ids, prop))?
    }

    // ------------------------------------------------------------------
    // Edge property columns
    //
    // The edge counterparts of the node column readers above, backed by an
    // independent columnar cache over the `edges` sub-database. They let the
    // query layer gather edge (relationship) properties in bulk through a
    // dense-index read instead of an LMDB point lookup plus a msgpack decode
    // per access. Semantics mirror the node methods exactly: a missing
    // property reads as `Value::Null`; a nonexistent edge is
    // [`Error::EdgeNotFound`].
    // ------------------------------------------------------------------

    /// Read one property of an edge through the in-memory edge property
    /// columns. Returns `None` for a nonexistent edge and `Some(Value::Null)`
    /// for a missing property.
    pub fn edge_prop_json(
        &self,
        id: EdgeId,
        prop: &str,
    ) -> Result<Option<serde_json::Value>, Error> {
        self.edge_columns.with_fresh(&self.storage, |cols| {
            cols.id_to_dense.get(&id).map(|&d| {
                cols.cols
                    .get(prop)
                    .and_then(|c| c.get_json_opt(d as usize))
                    .unwrap_or(serde_json::Value::Null)
            })
        })
    }

    /// Bulk row-major gather of `props` for each edge id in `ids`.
    pub fn edge_props_json_table(
        &self,
        ids: &[EdgeId],
        props: &[&str],
    ) -> Result<Vec<Vec<serde_json::Value>>, Error> {
        self.edge_columns
            .with_fresh(&self.storage, |cols| cols.props_table(ids, props))?
    }

    /// Single-property column gather for edges: `out[i]` is `prop` on `ids[i]`.
    pub fn edge_prop_json_column(
        &self,
        ids: &[EdgeId],
        prop: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        self.edge_columns
            .with_fresh(&self.storage, |cols| cols.prop_column(ids, prop))?
    }

    /// Group `ids` by the exact value of edge property `prop`: one dense group
    /// code per id plus one representative value per code.
    pub fn edge_prop_group_codes(
        &self,
        ids: &[EdgeId],
        prop: &str,
    ) -> Result<(Vec<u32>, Vec<serde_json::Value>), Error> {
        self.edge_columns
            .with_fresh(&self.storage, |cols| cols.group_codes(ids, prop))?
    }

    /// The minimum and maximum non-null value of one node property, from the
    /// lazily computed statistics over the in-memory property columns.
    /// `None` when the property has no typed column or no non-null values, and
    /// also when the columns are not built yet: this reader never builds them,
    /// because it is advisory (see [`Graph::estimate_equality_selectivity`]).
    pub fn node_prop_min_max(
        &self,
        prop: &str,
    ) -> Result<Option<(serde_json::Value, serde_json::Value)>, Error> {
        Ok(self
            .prop_columns
            .with_existing_mut(&self.storage, |cols| {
                cols.prop_stats(prop)
                    .map(|s| (s.min.clone(), s.max.clone()))
            })?
            .flatten())
    }

    /// Estimated fraction of non-null values of `prop` inside the given
    /// bounds (either bound optional), from the property's equi-depth
    /// histogram. `None` when no statistics exist for the property or the
    /// columns are not built yet; this reader never builds them.
    pub fn estimate_range_selectivity(
        &self,
        prop: &str,
        lower: Option<&serde_json::Value>,
        upper: Option<&serde_json::Value>,
    ) -> Result<Option<f64>, Error> {
        Ok(self
            .prop_columns
            .with_existing_mut(&self.storage, |cols| {
                cols.prop_stats(prop)
                    .map(|s| s.histogram.estimate_range_selectivity(lower, upper))
            })?
            .flatten())
    }

    /// Estimated fraction of non-null values of `prop` equal to `val`: exact
    /// for the property's most common values, histogram-estimated otherwise.
    ///
    /// `None` when no statistics exist for the property, and also when the
    /// property columns have not been built yet: the estimate only weights plan
    /// choices, so answering is never worth one full node scan on a query that
    /// would not otherwise materialize the columns.
    pub fn estimate_equality_selectivity(
        &self,
        prop: &str,
        val: &serde_json::Value,
    ) -> Result<Option<f64>, Error> {
        Ok(self
            .prop_columns
            .with_existing_mut(&self.storage, |cols| {
                cols.prop_stats(prop).map(|s| s.equality_selectivity(val))
            })?
            .flatten())
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
        self.debug_assert_not_in_write_txn();
        let _guard = self._write_lock.lock();
        let wtxn = self.storage.env.write_txn()?;
        let mut txn = WriteTxn {
            graph: self,
            wtxn,
            mutations_count: 0,
            delta: crate::csr::GraphDelta::default(),
        };
        let _txn_guard = WriteTxnGuard::enter(self.write_txn_env_id());
        match f(&mut txn) {
            Ok(val) => {
                let WriteTxn {
                    wtxn,
                    mutations_count,
                    delta,
                    graph: _,
                } = txn;
                // Publish before any other bookkeeping, so the window in which
                // the caches claim to be current while LMDB already holds this
                // write is one atomic increment wide rather than the width of
                // the batch. See `CsrCache::advance_write_gen`.
                self.commit_and_publish(wtxn, mutations_count)?;
                // Record the structural delta next, before the column bookkeeping.
                // `ensure_matrix_view` gates on the delta alone (gating it on the
                // generation would force a full rebuild after every write, since
                // only a full rebuild advances `matrices_gen`), so the delta being
                // absent is that gate's whole blind spot. Recording it here rather
                // than after the column patches shrinks the blind spot from the
                // width of the batch's column bookkeeping to a few instructions.
                self.csr_cache.record_batch(&delta);
                if delta.force_full {
                    self.prop_columns.record_force_full();
                } else {
                    self.prop_columns.record_touched_many(&delta.added_nodes);
                    self.prop_columns.record_touched_many(&delta.updated_nodes);
                }
                // Edge columns: an edge removal (or a node deletion that may
                // cascade to edges) reshuffles the dense edge mapping, so fall
                // back to a full rebuild; otherwise patch the added and
                // updated edges in.
                if delta.force_full || !delta.removed_edges.is_empty() {
                    self.edge_columns.record_force_full();
                } else {
                    self.edge_columns.record_touched_many(&delta.added_edge_ids);
                    self.edge_columns.record_touched_many(&delta.updated_edges);
                }
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

    /// Commit `wtxn` and publish the write to the caches' freshness counters as
    /// one step, where `count` is the number of mutations the transaction made.
    ///
    /// Every auto-committing mutation method and [`Graph::update`] commit through
    /// here rather than calling `wtxn.commit()` directly. The publish is what
    /// makes every freshness gate notice the write, so a mutation method that
    /// committed without it would leave the caches permanently claiming to be
    /// current; routing both through one call that consumes the transaction makes
    /// that combination unwritable. Ordering inside is deliberate: see
    /// [`crate::csr::CsrCache::advance_write_gen`].
    pub(super) fn commit_and_publish(
        &self,
        wtxn: heed::RwTxn<'_>,
        count: usize,
    ) -> Result<(), Error> {
        wtxn.commit()?;
        self.csr_cache.advance_write_gen(count as u64);
        Ok(())
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
        // Serialize against every other maintenance path (incremental applies,
        // snapshot-only refreshes, and the background rebuild) so no two run
        // concurrently and an incremental drain cannot race this install.
        let _maint = self.csr_cache.maintenance.lock();
        self.rebuild_csr_locked()
    }

    /// Full CSR-snapshot and matrix rebuild from LMDB. The caller must already
    /// hold `csr_cache.maintenance`; the public [`Graph::rebuild_csr`] acquires
    /// it. Kept separate so the freshness gates, which already hold the lock, do
    /// not deadlock on the non-reentrant mutex.
    pub(super) fn rebuild_csr_locked(&self) -> Result<(), Error> {
        // Capture the generation before reading LMDB so writes that land during
        // the build leave the snapshot conservatively stale.
        let built_gen = self.csr_cache.current_gen();
        // Clear the delta before reading LMDB: writes that commit during the
        // build land in the emptied delta and are re-applied incrementally later
        // (idempotently) rather than lost.
        self.csr_cache.clear_delta();
        let snap = CsrSnapshot::build(&self.storage)?;
        let m = MatrixSet::materialize(
            &snap,
            self.n_threads.load(std::sync::atomic::Ordering::Acquire),
        )?;
        // Install the matrices and the snapshot together under the matrices write
        // lock so a reader holding `matrices.read()` never observes a matrix from
        // one generation paired with a snapshot from another (the snapshot store
        // inside `install_full` cannot interleave with a held read guard).
        let mut guard = self.matrices.write();
        *guard = Some(m);
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
mod extension_tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// Extensions are keyed by concrete type: a stored value round-trips, an
    /// absent type returns `None`, and a second `set_extension` replaces the
    /// previous value of the same type.
    #[test]
    fn extension_roundtrip_by_type() {
        let (_dir, g) = open_tmp();
        assert!(g.get_extension::<String>().is_none());

        g.set_extension(Arc::new(String::from("cache")));
        let got = g.get_extension::<String>().expect("extension must exist");
        assert_eq!(*got, "cache");
        assert!(g.get_extension::<u64>().is_none(), "distinct type slot");

        g.set_extension(Arc::new(String::from("replaced")));
        assert_eq!(*g.get_extension::<String>().unwrap(), "replaced");
    }

    /// `get_or_init_extension_with` runs `init` only when the slot is empty;
    /// later callers observe the first stored value.
    #[test]
    fn get_or_init_extension_initializes_once() {
        let (_dir, g) = open_tmp();

        let v1 = g
            .get_or_init_extension_with::<u64, std::convert::Infallible, _>(|| Ok(Arc::new(7)))
            .unwrap();
        assert_eq!(*v1, 7);

        let v2 = g
            .get_or_init_extension_with::<u64, std::convert::Infallible, _>(|| Ok(Arc::new(9)))
            .unwrap();
        assert_eq!(*v2, 7, "second init must not replace the stored value");
    }

    /// An `init` failure stores nothing, so a later successful `init` runs.
    #[test]
    fn get_or_init_extension_propagates_init_error() {
        let (_dir, g) = open_tmp();

        let err = g
            .get_or_init_extension_with::<u64, &str, _>(|| Err("init failed"))
            .unwrap_err();
        assert_eq!(err, "init failed");
        assert!(g.get_extension::<u64>().is_none());

        let v = g
            .get_or_init_extension_with::<u64, &str, _>(|| Ok(Arc::new(7)))
            .unwrap();
        assert_eq!(*v, 7);
    }
}

#[cfg(test)]
mod encode_tests {
    use serde_json::json;

    use super::{MAX_INDEXED_STRING_LEN, decode_property_value, encode_property_value};

    /// A string up to the indexable bound encodes and round-trips; one byte over
    /// the bound is declined so it never overflows the LMDB key size.
    #[test]
    fn over_long_strings_are_not_indexed() {
        let at_limit = json!("a".repeat(MAX_INDEXED_STRING_LEN));
        let encoded = encode_property_value(&at_limit).expect("at-limit string indexes");
        assert_eq!(decode_property_value(&encoded), Some(at_limit));

        let too_long = json!("a".repeat(MAX_INDEXED_STRING_LEN + 1));
        assert_eq!(
            encode_property_value(&too_long),
            None,
            "a string over the bound must not be indexed",
        );
    }

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

#[cfg(test)]
mod publish_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::Graph;

    /// Every committing mutation must publish its write to the freshness
    /// counters, which is what [`Graph::commit_and_publish`] exists to make
    /// unforgettable. A method that committed without publishing would leave
    /// every gate reporting the caches as current, so a typed expansion or a
    /// graph algorithm would read pre-write state indefinitely rather than for
    /// the length of one atomic increment.
    #[test]
    fn every_committing_mutation_publishes_the_write() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("P", &json!({ "n": 1 })).unwrap();
        let b = g.add_node("P", &json!({ "n": 2 })).unwrap();
        let edge = g.add_edge(a, b, "T", &json!({ "weight": 1.0 })).unwrap();
        // Targets for the cases that consume what they touch, created up front so
        // the mutation under test is the only write inside its own window.
        let victim_node = g.add_node("P", &json!({})).unwrap();
        let victim_edge = g.add_edge(a, b, "T", &json!({})).unwrap();
        let label_target = g.add_node("P", &json!({})).unwrap();

        macro_rules! assert_publishes {
            ($name:literal, $body:block) => {{
                g.rebuild_csr().unwrap();
                assert!(
                    !g.csr_cache.snapshot_is_stale(),
                    concat!($name, ": a fresh rebuild must report current")
                );
                $body
                assert!(
                    g.csr_cache.snapshot_is_stale(),
                    concat!($name, " committed without publishing the write generation")
                );
            }};
        }

        assert_publishes!("add_node", {
            g.add_node("P", &json!({})).unwrap();
        });
        assert_publishes!("add_node_multi", {
            g.add_node_multi(&["P", "Q"], &json!({})).unwrap();
        });
        assert_publishes!("add_edge", {
            g.add_edge(a, b, "T", &json!({})).unwrap();
        });
        assert_publishes!("update_node", {
            g.update_node(a, &json!({ "n": 9 })).unwrap();
        });
        assert_publishes!("update_edge", {
            g.update_edge(edge, &json!({ "weight": 2.0 })).unwrap();
        });
        assert_publishes!("add_label", {
            g.add_label(label_target, "R").unwrap();
        });
        assert_publishes!("remove_label", {
            g.remove_label(label_target, "R").unwrap();
        });
        assert_publishes!("delete_edge", {
            g.delete_edge(victim_edge).unwrap();
        });
        assert_publishes!("delete_node", {
            g.delete_node(victim_node).unwrap();
        });
        assert_publishes!("update", {
            g.update(|txn| {
                txn.add_node("P", &json!({}))?;
                Ok(())
            })
            .unwrap();
        });
    }

    /// A `Graph::update` closure that mutates nothing must not advance the
    /// generation, so a read-only use of the write transaction does not force
    /// every cache to rebuild.
    #[test]
    fn a_mutation_free_update_publishes_nothing() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        g.add_node("P", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        g.update(|txn| txn.get_node(1).map(|_| ())).unwrap();

        assert!(
            !g.csr_cache.snapshot_is_stale(),
            "a read-only update must leave the caches current"
        );
    }
}

#[cfg(test)]
mod lazy_open_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::Graph;
    use crate::schema::NodeId;

    /// Populate a graph, force the CSR and matrices to materialize, then close
    /// it. Returns the directory so the caller can reopen the same path.
    fn seeded_dir() -> (TempDir, Vec<NodeId>) {
        let dir = TempDir::new().unwrap();
        let ids = {
            let g = Graph::open(dir.path(), 1).unwrap();
            // 80 nodes in a ring plus a chord, so a typed expansion over more
            // than `STALE_POINT_EXPAND_MAX` (64) sources takes the snapshot
            // path rather than the per-source LMDB path.
            let ids: Vec<_> = (0..80)
                .map(|i| g.add_node("Person", &json!({ "n": i })).unwrap())
                .collect();
            for i in 0..ids.len() {
                g.add_edge(ids[i], ids[(i + 1) % ids.len()], "FOLLOWS", &json!({}))
                    .unwrap();
            }
            g.add_edge(ids[0], ids[40], "LIKES", &json!({})).unwrap();
            // Touch an algorithm so this handle definitely materialized both.
            g.bfs(ids[0], 2).unwrap();
            assert!(g.matrices.read().is_some(), "seed handle must materialize");
            ids
        };
        (dir, ids)
    }

    /// Opening an existing database does no CSR scan and no GraphBLAS
    /// materialization. Both are the freshness gates' job, so a workload that
    /// only reads properties or point adjacency never pays for them.
    #[test]
    fn open_defers_the_csr_and_matrix_build() {
        let (dir, _ids) = seeded_dir();
        let g = Graph::open(dir.path(), 1).unwrap();

        assert!(
            g.matrices.read().is_none(),
            "open must not materialize the GraphBLAS matrices"
        );
        assert_eq!(
            g.csr_cache.snapshot.load().dense_to_id.len(),
            0,
            "open must not build the CSR snapshot"
        );
        assert!(
            g.csr_cache.snapshot_is_stale(),
            "the unbuilt snapshot must report stale so a consumer rebuilds it"
        );
    }

    /// A freshly opened handle serves every consumer class correctly, each
    /// building what it needs through its own gate. This is the guard on the
    /// generation bookkeeping: if the unbuilt snapshot reported itself fresh,
    /// the typed-expansion path would read an empty CSR and silently return no
    /// rows instead of rebuilding.
    #[test]
    fn reopened_graph_serves_every_consumer_class() {
        let (dir, ids) = seeded_dir();

        // Each consumer gets its own handle, scoped so the LMDB environment is
        // closed before the next open, and so every gate is exercised from the
        // unbuilt state rather than riding on an earlier consumer's build.
        let reopen = || Graph::open(dir.path(), 1).unwrap();

        // Typed expansion over more sources than the stale-point-read cutoff,
        // so this goes through `ensure_snapshot_fresh`.
        {
            let g = reopen();
            let wide = g
                .expand_spmv_graphblas(&ids, Some("FOLLOWS"), false)
                .unwrap();
            assert_eq!(wide.len(), 80, "every ring edge must expand");
        }
        // Typed expansion under the cutoff, which reads LMDB point adjacency
        // directly and needs no snapshot at all.
        {
            let g = reopen();
            let narrow = g
                .expand_spmv_graphblas(&ids[..4], Some("FOLLOWS"), false)
                .unwrap();
            assert_eq!(narrow.len(), 4);
        }
        // Matrix-view consumer. Traversal is untyped, so one hop from `ids[0]`
        // reaches both the ring successor and the `LIKES` chord target.
        {
            let g = reopen();
            assert_eq!(
                g.bfs(ids[0], 1).unwrap().len(),
                3,
                "start plus both one-hop neighbors"
            );
        }
        // CSR-array consumer.
        {
            let g = reopen();
            assert_eq!(g.dfs(ids[0], 1).unwrap().len(), 3);
        }
        // Weighted matrix consumer.
        {
            let g = reopen();
            assert_eq!(g.page_rank(5, 0.85).unwrap().len(), 80);
        }
        // Count kernel.
        {
            let g = reopen();
            let spec = crate::PathCountSpec {
                rel_types: vec![Some("FOLLOWS")],
                labels: vec![Some("Person"), Some("Person")],
                vertex_allow: Vec::new(),
            };
            assert_eq!(g.count_linear_paths(&spec).unwrap(), 80);
        }
        // Point adjacency, which never consults the snapshot.
        {
            let g = reopen();
            assert_eq!(g.out_neighbors(ids[0]).unwrap().len(), 2);
        }
    }

    /// The first gated consumer materializes the matrices, so the deferral is
    /// a delay rather than a permanent absence.
    #[test]
    fn first_algorithm_materializes_what_open_skipped() {
        let (dir, ids) = seeded_dir();
        let g = Graph::open(dir.path(), 1).unwrap();
        assert!(g.matrices.read().is_none());

        assert_eq!(g.bfs(ids[0], 1).unwrap().len(), 3);

        assert!(
            g.matrices.read().is_some(),
            "the matrix-view gate must materialize on first use"
        );
        assert!(!g.csr_cache.snapshot_is_stale());
    }

    /// Reopening an empty database is also lazy, and every consumer reports
    /// empty rather than erroring on the absent snapshot.
    #[test]
    fn empty_database_opens_lazily_and_reads_empty() {
        let dir = TempDir::new().unwrap();
        {
            Graph::open(dir.path(), 1).unwrap();
        }
        let g = Graph::open(dir.path(), 1).unwrap();
        assert!(g.matrices.read().is_none());
        assert!(g.all_nodes().unwrap().is_empty());
        assert!(g.connected_components().unwrap().is_empty());
        assert!(g.page_rank(3, 0.85).unwrap().is_empty());
    }
}
