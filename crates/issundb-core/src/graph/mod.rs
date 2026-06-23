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
    /// Property that must be non-null on the counted endpoint for an edge to
    /// count; `None` counts every qualifying edge.
    pub counted_nonnull_prop: Option<&'a str>,
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

impl Graph {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        let storage = Storage::open(path, map_size_gb)?;
        // Older versions persisted the CSR snapshot next to the LMDB files but
        // never read it back; remove the stale artifact if one is present.
        let _ = std::fs::remove_file(path.join("csr_snapshot.bin"));
        let initial = CsrSnapshot::build(&storage)?;
        let storage = Arc::new(storage);
        let csr_cache = Arc::new(CsrCache::new(initial));
        let matrices = {
            let initial_snap = csr_cache.snapshot.load();
            let m = MatrixSet::materialize(&initial_snap, 0)?;
            Arc::new(parking_lot::RwLock::new(Some(m)))
        };
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
    /// `ISSUNDB_NUM_THREADS` environment variable. Set to 0 to restore the default behavior.
    pub fn set_thread_count(&self, n: i32) -> Result<(), Error> {
        self.n_threads
            .store(n, std::sync::atomic::Ordering::Release);
        issundb_graphblas::set_global_threads(n).map_err(|e| Error::GraphBLAS(e.to_string()))?;
        Ok(())
    }

    /// Read one property of a node through the in-memory property columns,
    /// as the `serde_json::Value` that decoding the stored record would give.
    /// Returns `None` for a nonexistent node and `Some(Value::Null)` for a
    /// missing property. Builds or refreshes the columns on first use after a
    /// write, so the result always reflects committed state.
    pub fn node_prop_json(
        &self,
        id: NodeId,
        prop: &str,
    ) -> Result<Option<serde_json::Value>, Error> {
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
        self.prop_columns
            .with_fresh(&self.storage, |cols| cols.prop_column(ids, prop))?
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
    /// `None` when the property has no typed column or no non-null values.
    pub fn node_prop_min_max(
        &self,
        prop: &str,
    ) -> Result<Option<(serde_json::Value, serde_json::Value)>, Error> {
        self.prop_columns.with_fresh_mut(&self.storage, |cols| {
            cols.prop_stats(prop)
                .map(|s| (s.min.clone(), s.max.clone()))
        })
    }

    /// Estimated fraction of non-null values of `prop` inside the given
    /// bounds (either bound optional), from the property's equi-depth
    /// histogram. `None` when no statistics exist for the property.
    pub fn estimate_range_selectivity(
        &self,
        prop: &str,
        lower: Option<&serde_json::Value>,
        upper: Option<&serde_json::Value>,
    ) -> Result<Option<f64>, Error> {
        self.prop_columns.with_fresh_mut(&self.storage, |cols| {
            cols.prop_stats(prop)
                .map(|s| s.histogram.estimate_range_selectivity(lower, upper))
        })
    }

    /// Estimated fraction of non-null values of `prop` equal to `val`: exact
    /// for the property's most common values, histogram-estimated otherwise.
    /// `None` when no statistics exist for the property.
    pub fn estimate_equality_selectivity(
        &self,
        prop: &str,
        val: &serde_json::Value,
    ) -> Result<Option<f64>, Error> {
        self.prop_columns.with_fresh_mut(&self.storage, |cols| {
            cols.prop_stats(prop).map(|s| s.equality_selectivity(val))
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
                if delta.force_full {
                    self.prop_columns.record_force_full();
                } else {
                    self.prop_columns.record_touched_many(&delta.added_nodes);
                    self.prop_columns.record_touched_many(&delta.updated_nodes);
                }
                // Edge columns: an edge removal (or a node deletion that may
                // cascade to edges) reshuffles the dense edge mapping, so fall
                // back to a full rebuild; otherwise patch the added edges in.
                if delta.force_full || !delta.removed_edges.is_empty() {
                    self.edge_columns.record_force_full();
                } else {
                    self.edge_columns.record_touched_many(&delta.added_edge_ids);
                }
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
    #[instrument(skip(self))]
    pub fn rebuild_csr(&self) -> Result<(), Error> {
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
