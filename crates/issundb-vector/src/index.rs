use std::cell::RefCell;
use std::sync::Arc;

use crate::backend::{VectorBackend, new_backend};
use parking_lot::RwLock;
use tracing::instrument;

use crate::error::VectorError;
use issundb_core::{Graph, NodeId};

/// A single result from vector search.
#[derive(Debug)]
pub struct Hit {
    pub node: NodeId,
    pub distance: f32,
}

/// Options for `vector_search_with`.
#[derive(Debug, Clone)]
pub struct VectorSearchOptions {
    /// Maximum number of results to return.
    pub k: usize,
    /// If set, only nodes carrying this exact label are included in results.
    pub label: Option<String>,
    /// Optional property key-value filters. Only nodes matching all filters are returned.
    pub properties: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Rescore factor. When greater than 1, search fetches `k * rescore_factor`
    /// candidates from the index and re-ranks them by exact distance against
    /// the full-precision vectors stored in LMDB. Defaults to 2 on a quantized
    /// index and 1 (no rescore) on a Float32 index. Without the `hnsw` feature
    /// the default is always 1, because that backend keeps the raw `f32` and so
    /// has no precision to recover whatever the persisted tag says. The default applies to
    /// filtered searches too, where the over-fetch means the traversal must
    /// find `k * rescore_factor` predicate-matching candidates; pass
    /// `Some(1)` to disable rescoring for a selective filter.
    pub rescore_factor: Option<usize>,
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self {
            k: 10,
            label: None,
            properties: None,
            rescore_factor: None,
        }
    }
}

/// Distance metric for the vector index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VectorMetric {
    /// Cosine similarity (default).
    #[default]
    Cosine,
    /// Euclidean (L2) distance.
    L2,
    /// Inner product / dot product.
    Dot,
}

/// Quantization format for in-memory vector storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VectorQuantization {
    /// Float32 quantization (default, full accuracy).
    #[default]
    Float32,
    /// Float16 quantization (half memory footprint).
    Float16,
    /// Int8 quantization (quarter memory footprint).
    Int8,
}

impl std::str::FromStr for VectorMetric {
    type Err = VectorError;

    /// Parse a metric name. Case-insensitive. Accepts `cosine`, `l2`, and `dot`
    /// (with the alias `ip` for inner product). This is the one canonical
    /// mapping every binding (CLI, REST, MCP, and Python) parses through.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cosine" => Ok(Self::Cosine),
            "l2" => Ok(Self::L2),
            "dot" | "ip" => Ok(Self::Dot),
            other => Err(VectorError::InvalidConfig(format!(
                "unknown metric '{other}' (expected 'cosine', 'l2', or 'dot')"
            ))),
        }
    }
}

impl std::str::FromStr for VectorQuantization {
    type Err = VectorError;

    /// Parse a quantization name. Case-insensitive. Accepts `float32`,
    /// `float16`, and `int8`. The one canonical mapping shared by every binding.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "float32" => Ok(Self::Float32),
            "float16" => Ok(Self::Float16),
            "int8" => Ok(Self::Int8),
            other => Err(VectorError::InvalidConfig(format!(
                "unknown quantization '{other}' (expected 'float32', 'float16', or 'int8')"
            ))),
        }
    }
}

/// Construction options for `VectorIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorIndexOptions {
    pub metric: VectorMetric,
    pub quantization: VectorQuantization,
}

enum Inner {
    Empty,
    Ready {
        index: Box<dyn VectorBackend>,
        dims: usize,
    },
}

/// An in-memory vector index over whichever backend this crate was compiled with
/// (see [`crate::backend`]): an approximate HNSW index by default, an exact scan
/// without the `hnsw` feature.
///
/// Internal building block for the `VectorGraphExt` implementation on `Graph`.
/// It holds no persistence of its own, so it is not part of the public surface;
/// callers use the graph-backed `VectorGraphExt` methods instead.
pub(crate) struct VectorIndex {
    opts: VectorIndexOptions,
    inner: RwLock<Inner>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndex {
    /// Construct a new empty vector index with default Cosine and Float32 options.
    pub fn new() -> Self {
        Self::new_with_options(VectorIndexOptions::default())
    }

    /// Construct a new empty vector index with custom metric and quantization.
    pub fn new_with_options(opts: VectorIndexOptions) -> Self {
        Self {
            opts,
            inner: RwLock::new(Inner::Empty),
        }
    }

    /// Insert or replace the embedding for `node`.
    ///
    /// On the first call, the index is initialised with `v.len()` dimensions
    /// using the metric and quantization from the construction options. Subsequent
    /// calls with a different dimension count return `VectorError::DimensionMismatch`.
    pub fn upsert(&self, node: NodeId, v: &[f32]) -> Result<(), VectorError> {
        let dims = v.len();
        if dims == 0 {
            return Err(VectorError::IndexFault(
                "embedding must not be empty".into(),
            ));
        }
        let mut guard = self.inner.write();
        match &mut *guard {
            Inner::Empty => {
                let mut index = new_backend(dims, &self.opts)?;
                index.upsert(node, v)?;
                *guard = Inner::Ready { index, dims };
            }
            Inner::Ready { index, dims: d } => {
                if dims != *d {
                    return Err(VectorError::DimensionMismatch {
                        expected: *d,
                        got: dims,
                    });
                }
                index.upsert(node, v)?;
            }
        }
        Ok(())
    }

    /// True when the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        match &*self.inner.read() {
            Inner::Empty => true,
            Inner::Ready { index, .. } => index.len() == 0,
        }
    }

    /// Remove the embedding for `node` from the index.
    pub fn remove(&self, node: NodeId) -> Result<(), VectorError> {
        let mut guard = self.inner.write();
        if let Inner::Ready { index, .. } = &mut *guard {
            index.remove(node)?;
        }
        Ok(())
    }

    /// Return the `k` nearest neighbors to `q` under this graph's configured metric
    /// (default Cosine). Approximate with `hnsw`, exact without it.
    ///
    /// Returns an empty slice when the index has no vectors or `k == 0`.
    /// `k` is silently clamped to the number of indexed vectors.
    pub fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        let guard = self.inner.read();
        match &*guard {
            Inner::Empty => Ok(vec![]),
            Inner::Ready { index, .. } => index.search(q, k),
        }
    }

    /// Return up to `k` nearest neighbors to `q` that satisfy `predicate`.
    ///
    /// The predicate is evaluated during the traversal, so the search keeps
    /// expanding until it has `k` matching neighbors or exhausts the reachable
    /// graph. Unlike post-filtering a fixed over-fetch, this does not silently
    /// truncate the result set when the filter is selective.
    pub fn search_filtered<F>(
        &self,
        q: &[f32],
        k: usize,
        predicate: F,
    ) -> Result<Vec<Hit>, VectorError>
    where
        F: Fn(NodeId) -> bool,
    {
        let guard = self.inner.read();
        match &*guard {
            Inner::Empty => Ok(vec![]),
            Inner::Ready { index, .. } => index.search_filtered(q, k, &predicate),
        }
    }
}

fn encode_vector(v: &[f32]) -> Result<Vec<u8>, VectorError> {
    if v.is_empty() {
        return Err(VectorError::IndexFault(
            "embedding must not be empty".into(),
        ));
    }
    // A NaN or infinity would be stored and then produce a NaN distance at every search,
    // which no ranking can order meaningfully, so it is rejected at the boundary rather
    // than silently poisoning every later query.
    if let Some(position) = v.iter().position(|f| !f.is_finite()) {
        return Err(VectorError::IndexFault(format!(
            "embedding component {position} is not finite ({})",
            v[position]
        )));
    }
    Ok(v.iter().flat_map(|f| f.to_le_bytes()).collect())
}

fn decode_vector(bytes: &[u8]) -> Result<Vec<f32>, VectorError> {
    if bytes.len() % 4 != 0 {
        return Err(VectorError::IndexFault(format!(
            "stored embedding byte length must be divisible by 4, got {}",
            bytes.len()
        )));
    }
    let vector = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok(vector)
}

/// Vector search operations for `Graph`.
pub trait VectorGraphExt {
    /// Set the metric and quantization for this graph's vector index.
    ///
    /// The choice is persisted, so reopening the graph rebuilds the index with
    /// the same configuration. Call this before upserting the first vector. The
    /// HNSW graph is built per-metric, so the configuration cannot change once
    /// vectors exist: a call that would change the persisted metric or
    /// quantization while embeddings are present returns
    /// `VectorError::AlreadyConfigured`. Re-applying the identical configuration
    /// is a no-op. When no graph configuration is set, the index defaults to
    /// `Cosine` and `Float32`.
    ///
    /// A build without `hnsw` accepts a non-`Float32` quantization and persists it, but
    /// cannot honor it: that backend keeps the raw `f32`, so the memory reduction the
    /// quantization names does not happen. The tag is still recorded rather than rejected,
    /// because it is honored by any later build that does have `hnsw` opening the same
    /// directory.
    fn configure_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError>;

    /// Change the metric and quantization and rebuild the index from the
    /// persisted embeddings under the new configuration.
    ///
    /// Unlike `configure_vector_index`, this accepts a change after vectors
    /// exist. The raw f32 embeddings are stored in LMDB independently of the
    /// metric, so they are re-indexed under `opts`; switching back to `Float32`
    /// recovers full precision from storage. This rebuilds the entire in-memory
    /// index, so it is O(n) in the number of stored vectors and is intended
    /// as an administrative operation, not a concurrent one: running it while
    /// other threads upsert may drop an in-flight write from the snapshot, which
    /// the next `Graph::open` rebuild reconciles.
    fn reindex_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError>;

    /// Persist `v` under `n`.
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError>;

    /// Remove the embedding for `n` from the index and from persistent storage.
    fn remove_vector(&self, n: NodeId) -> Result<(), VectorError>;

    /// Return the `k` nearest neighbors to `q` under this graph's configured metric
    /// (default Cosine). Approximate with `hnsw`, exact without it.
    ///
    /// Returns `VectorError::EmptyIndex` when the graph holds no embeddings at
    /// all, so a caller can distinguish "no semantic matches" from "there is
    /// nothing to search".
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>;

    /// Return the `opts.k` nearest neighbors that satisfy the label and property
    /// filters in `opts`.
    ///
    /// When neither `opts.label` nor `opts.properties` is set the call is
    /// identical to `vector_search(q, opts.k)`. When a filter is set, it is
    /// applied during the traversal through a predicate, so the search
    /// keeps expanding until it has `opts.k` matching neighbors rather than
    /// post-filtering a fixed over-fetch (which silently under-returns for
    /// selective filters). A node matches when it carries `opts.label` (if set)
    /// and every entry in `opts.properties` (if set) equals the node's value for
    /// that property. Fewer than `opts.k` results are returned only when the
    /// index genuinely contains fewer matching nodes.
    fn vector_search_with(
        &self,
        q: &[f32],
        opts: &VectorSearchOptions,
    ) -> Result<Vec<Hit>, VectorError>;

    /// Return the full-precision embedding stored for `n`, or `None` when the
    /// node has no embedding. This is a point lookup against LMDB and does not
    /// build or consult the in-memory index.
    fn node_vector(&self, n: NodeId) -> Result<Option<Vec<f32>>, VectorError>;

    /// Distance between two vectors under this graph's configured metric
    /// (default Cosine). The convention matches `vector_search`: squared L2 for
    /// `L2` and `1 - dot` for `Dot`. Returns `DimensionMismatch` when the two
    /// vectors differ in length.
    fn vector_distance(&self, a: &[f32], b: &[f32]) -> Result<f32, VectorError>;
}

/// Key type used to store the persistent HNSW cache in `Graph::extensions`.
struct VectorIndexCache(VectorIndex);

impl VectorGraphExt for Graph {
    fn configure_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError> {
        // Compare against the EFFECTIVE config: when nothing is persisted the
        // active configuration is the lazily built default, so re-applying that
        // default (or any already-active config) is a no-op, as documented, not
        // an `AlreadyConfigured` error.
        let effective = load_config(self)?.unwrap_or_default();
        if effective == opts {
            return Ok(());
        }
        // The HNSW graph is built per-metric. Changing the metric or
        // quantization once embeddings exist would silently reinterpret them on
        // the next cold-start rebuild, so refuse it while vectors are present.
        if !self.vector_bytes()?.is_empty() {
            return Err(VectorError::AlreadyConfigured {
                existing: format!("{effective:?}"),
                requested: format!("{opts:?}"),
            });
        }
        self.put_vector_config(&encode_config(opts))?;
        // Replace any lazily built default cache so later upserts use the new
        // configuration. Safe because no vectors exist yet.
        self.set_extension(Arc::new(VectorIndexCache(VectorIndex::new_with_options(
            opts,
        ))));
        Ok(())
    }

    fn reindex_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError> {
        // Rebuild the index from the stored raw embeddings FIRST, then persist
        // the new configuration and swap the cache. Building before persisting
        // means a mid-rebuild failure leaves BOTH the previous cache and the
        // previous persisted config in place, so the next cold-start rebuild does
        // not silently reinterpret embeddings under a metric the failed operation
        // never finished applying. `build_index` takes `opts` explicitly, so it
        // does not depend on the persisted config.
        let rebuilt = build_index(self, opts)?;
        self.put_vector_config(&encode_config(opts))?;
        self.set_extension(Arc::new(VectorIndexCache(rebuilt)));
        Ok(())
    }

    #[instrument(skip(self, v), fields(node = %n, dims = v.len()))]
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError> {
        // Reject an embedding for an id no node holds. Node ids are handed out monotonically, so a
        // vector written ahead of its node is not inert: the next node allocated that id inherits
        // it and answers a search at distance zero, having never been embedded. Nothing downstream
        // could detect that, because a stored vector carries no evidence of who it was meant for.
        //
        // The cost is one key probe per upsert, inside a call that already opens a write
        // transaction and rebuilds an index entry. `remove_vector` stays permissive on purpose, so
        // a database that already holds such a vector can still be cleaned up.
        if !self.node_exists(n)? {
            return Err(VectorError::NodeNotFound(n));
        }
        let bytes = encode_vector(v)?;
        // Validate against (and update) the in-memory index BEFORE persisting to
        // LMDB. `upsert` rejects empty or dimension-mismatched embeddings, so
        // doing it first guarantees a rejected vector never reaches durable
        // storage. If it did, the cold-start rebuild on the next `Graph::open`
        // would hit the mismatch and fail to build the index, bricking every
        // subsequent search. The reverse failure (index updated, LMDB write
        // fails) only drops an in-memory entry that the next reopen rebuilds
        // consistently, so it is the safe ordering.
        let arc = get_or_init_cache(self)?;
        arc.0.upsert(n, v)?;
        self.put_vector_bytes(n, &bytes)?;
        Ok(())
    }

    fn remove_vector(&self, n: NodeId) -> Result<(), VectorError> {
        self.delete_vector_bytes(n)?;
        if let Some(arc) = self.get_extension::<VectorIndexCache>() {
            arc.0.remove(n)?;
        }
        Ok(())
    }

    #[instrument(skip(self, q), fields(k = %k, dims = q.len()))]
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        let opts = VectorSearchOptions {
            k,
            ..Default::default()
        };
        self.vector_search_with(q, &opts)
    }

    #[instrument(skip(self, q), fields(k = %opts.k, label = ?opts.label, dims = q.len()))]
    fn vector_search_with(
        &self,
        q: &[f32],
        opts: &VectorSearchOptions,
    ) -> Result<Vec<Hit>, VectorError> {
        let arc = get_or_init_cache(self)?;

        // An empty index is an error, not an empty result: for a caller (and
        // especially an agent surface like MCP) an empty hit list claims
        // "nothing matched", which is wrong when there was nothing to search.
        if arc.0.is_empty() {
            return Err(VectorError::EmptyIndex);
        }

        let index_quantization = arc.0.opts.quantization;
        // Rescoring re-reads and re-decodes `2k` stored vectors to recompute distances at
        // full precision, which is only worth it against a backend that lost precision.
        // The exact backend keeps the raw `f32` and already ranks through `exact_distance`,
        // so a persisted quantization tag there would buy bit-identical distances for a
        // second pass over storage.
        let backend_quantizes =
            cfg!(feature = "hnsw") && index_quantization != VectorQuantization::Float32;
        let rescore_factor = opts
            .rescore_factor
            .unwrap_or(if backend_quantizes { 2 } else { 1 });

        let fetch_k = if rescore_factor > 1 {
            opts.k.saturating_mul(rescore_factor)
        } else {
            opts.k
        };

        // An empty property map with no label is a vacuous filter set: the
        // predicate below would accept every candidate, including nodes
        // deleted through the core API that linger in the HNSW index (the
        // filtered path relies on its label and property lookups to reject
        // those ghosts). Route it to the unfiltered path, which drops ghosts
        // via its liveness backfill.
        let has_filters =
            opts.label.is_some() || opts.properties.as_ref().is_some_and(|m| !m.is_empty());
        let hits = if has_filters {
            // Evaluate the label and property filters during the HNSW traversal via
            // a predicate, so the search keeps expanding until it has `opts.k`
            // matching neighbors instead of post-filtering a fixed over-fetch, which
            // silently under-returns when the filter is selective. The predicate
            // reads through the core accessors (`label_filter` point lookup and the
            // in-memory property columns via `node_prop_json`) rather than decoding
            // raw node records, to respect the crate boundary. A storage error
            // cannot travel through the `Fn(NodeId) -> bool` callback, so it is
            // captured and surfaced after the search; once set, the predicate
            // rejects every remaining candidate to end the traversal promptly.
            let pred_err: RefCell<Option<VectorError>> = RefCell::new(None);
            let matches_filters = |node: NodeId| -> Result<bool, VectorError> {
                if let Some(label) = &opts.label {
                    if self.label_filter(&[node], label)?.is_empty() {
                        return Ok(false);
                    }
                }
                if let Some(filters) = &opts.properties {
                    for (key, want) in filters {
                        match self.node_prop_json(node, key)? {
                            Some(got) if &got == want => {}
                            _ => return Ok(false),
                        }
                    }
                }
                Ok(true)
            };
            let predicate = |node: NodeId| -> bool {
                if pred_err.borrow().is_some() {
                    return false;
                }
                match matches_filters(node) {
                    Ok(keep) => keep,
                    Err(e) => {
                        *pred_err.borrow_mut() = Some(e);
                        false
                    }
                }
            };

            let results = arc.0.search_filtered(q, fetch_k, predicate)?;
            if let Some(e) = pred_err.into_inner() {
                return Err(e);
            }
            // The label and property predicate fails for a node deleted through
            // the core API (it is gone from `label_idx` and the property columns),
            // so `search_filtered` has already skipped those "ghost" hits and kept
            // expanding, returning live matches only.
            results
        } else {
            // Unfiltered: a node deleted through the core graph API stays in the
            // in-memory HNSW index, because core deletion cannot reach this
            // vector-crate extension. Over-fetch and drop those "ghost" hits,
            // growing the fetch window until `fetch_k` live hits remain (or the
            // index is exhausted). A fixed `fetch_k` would truncate the result
            // below `opts.k` when enough top-ranked nodes had been deleted.
            let mut want = fetch_k.max(1);
            loop {
                let raw = arc.0.search(q, want)?;
                let raw_len = raw.len();
                let live: Vec<Hit> = self.view(|txn| {
                    let mut live = Vec::with_capacity(raw_len);
                    for hit in raw {
                        if txn.get_node(hit.node)?.is_some() {
                            live.push(hit);
                        }
                    }
                    Ok(live)
                })?;
                if live.len() >= fetch_k || raw_len < want {
                    break live;
                }
                want = want.saturating_mul(2);
            }
        };

        let mut final_hits = if rescore_factor > 1 && !hits.is_empty() {
            // One read transaction covers every stored-vector lookup. A hit
            // whose stored bytes are absent keeps its approximate distance,
            // so a vacuous index entry degrades the estimate, not the call.
            let byte_rows: Vec<(Hit, Option<Vec<u8>>)> = self.view(|txn| {
                hits.into_iter()
                    .map(|hit| {
                        let bytes = txn.get_vector_bytes(hit.node)?;
                        Ok((hit, bytes))
                    })
                    .collect()
            })?;
            let mut rescored = Vec::with_capacity(byte_rows.len());
            for (hit, bytes) in byte_rows {
                rescored.push(match bytes {
                    Some(b) => Hit {
                        node: hit.node,
                        distance: exact_distance(q, &decode_vector(&b)?, arc.0.opts.metric),
                    },
                    None => hit,
                });
            }
            // Node id included, because `truncate` below decides which of two equidistant
            // hits survives at the k-th position and sorting on distance alone left that to
            // whatever order the sort happened to leave. This makes the rescored path
            // deterministic; it does not make the whole surface so, since a `Float32` HNSW
            // index does not rescore and usearch breaks a distance tie by its own traversal
            // order.
            rescored.sort_unstable_by(|a, b| {
                a.distance.total_cmp(&b.distance).then(a.node.cmp(&b.node))
            });
            rescored
        } else {
            hits
        };

        final_hits.truncate(opts.k);
        Ok(final_hits)
    }

    fn node_vector(&self, n: NodeId) -> Result<Option<Vec<f32>>, VectorError> {
        let bytes = self.view(|txn| txn.get_vector_bytes(n))?;
        match bytes {
            Some(b) => Ok(Some(decode_vector(&b)?)),
            None => Ok(None),
        }
    }

    fn vector_distance(&self, a: &[f32], b: &[f32]) -> Result<f32, VectorError> {
        if a.len() != b.len() {
            return Err(VectorError::DimensionMismatch {
                expected: a.len(),
                got: b.len(),
            });
        }
        let metric = load_config(self)?.unwrap_or_default().metric;
        Ok(exact_distance(a, b, metric))
    }
}

/// Full-precision distance between `q` and a stored vector. Both backends report this
/// same convention for a given metric (squared L2, and `1 - dot` for inner product), which
/// is what lets a rescored distance and an approximate one be sorted into one list;
/// `the_hnsw_backend_reports_the_same_convention_as_exact_distance` pins it.
pub(crate) fn exact_distance(q: &[f32], v: &[f32], metric: VectorMetric) -> f32 {
    match metric {
        VectorMetric::Cosine => {
            let mut dot = 0.0;
            let mut norm_q = 0.0;
            let mut norm_v = 0.0;
            for (&qi, &vi) in q.iter().zip(v.iter()) {
                dot += qi * vi;
                norm_q += qi * qi;
                norm_v += vi * vi;
            }
            if norm_q > 0.0 && norm_v > 0.0 {
                // Clamped at zero: rounding can push the ratio past 1.
                (1.0 - (dot / (norm_q.sqrt() * norm_v.sqrt()))).max(0.0)
            } else {
                1.0
            }
        }
        VectorMetric::L2 => {
            let mut sum = 0.0;
            for (&qi, &vi) in q.iter().zip(v.iter()) {
                let diff = qi - vi;
                sum += diff * diff;
            }
            sum
        }
        VectorMetric::Dot => {
            let mut dot = 0.0;
            for (&qi, &vi) in q.iter().zip(v.iter()) {
                dot += qi * vi;
            }
            1.0 - dot
        }
    }
}

/// Return the cached `VectorIndexCache` for this Graph, building it from LMDB
/// if it has not been initialised yet.
fn get_or_init_cache(graph: &Graph) -> Result<Arc<VectorIndexCache>, VectorError> {
    // Cold start: load all vectors from LMDB into a fresh HNSW index, built with
    // the graph's persisted metric and quantization (default Cosine and Float32
    // when never configured). The initializer runs without the extensions lock
    // held, so reading from storage here cannot deadlock against it.
    graph.get_or_init_extension_with(|| {
        let opts = load_config(graph)?.unwrap_or_default();
        Ok(Arc::new(VectorIndexCache(build_index(graph, opts)?)))
    })
}

/// Build a fresh in-memory HNSW index from every embedding persisted in LMDB,
/// using `opts` for the metric and quantization. The stored vectors are raw
/// f32 and metric-agnostic, so this re-indexes them correctly under any metric.
fn build_index(graph: &Graph, opts: VectorIndexOptions) -> Result<VectorIndex, VectorError> {
    let idx = VectorIndex::new_with_options(opts);
    for (node_id, bytes) in graph.vector_bytes()? {
        let v = decode_vector(&bytes)?;
        idx.upsert(node_id, &v)?;
    }
    Ok(idx)
}

/// Load and decode this graph's persisted vector index configuration, or
/// `None` when the graph has never been configured.
fn load_config(graph: &Graph) -> Result<Option<VectorIndexOptions>, VectorError> {
    match graph.get_vector_config()? {
        Some(bytes) => Ok(Some(decode_config(&bytes)?)),
        None => Ok(None),
    }
}

/// Encode the index configuration as two stable tag bytes: `[metric, quantization]`.
fn encode_config(opts: VectorIndexOptions) -> [u8; 2] {
    let metric = match opts.metric {
        VectorMetric::Cosine => 0,
        VectorMetric::L2 => 1,
        VectorMetric::Dot => 2,
    };
    let quant = match opts.quantization {
        VectorQuantization::Float32 => 0,
        VectorQuantization::Float16 => 1,
        VectorQuantization::Int8 => 2,
    };
    [metric, quant]
}

/// Decode the two-byte index configuration written by `encode_config`.
fn decode_config(bytes: &[u8]) -> Result<VectorIndexOptions, VectorError> {
    let [metric, quant] = bytes.try_into().map_err(|_| {
        VectorError::IndexFault(format!(
            "vector config must be 2 bytes, got {}",
            bytes.len()
        ))
    })?;
    let metric = match metric {
        0 => VectorMetric::Cosine,
        1 => VectorMetric::L2,
        2 => VectorMetric::Dot,
        other => {
            return Err(VectorError::IndexFault(format!(
                "unknown vector metric tag {other}"
            )));
        }
    };
    let quantization = match quant {
        0 => VectorQuantization::Float32,
        1 => VectorQuantization::Float16,
        2 => VectorQuantization::Int8,
        other => {
            return Err(VectorError::IndexFault(format!(
                "unknown vector quantization tag {other}"
            )));
        }
    };
    Ok(VectorIndexOptions {
        metric,
        quantization,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    /// An embedding for an id no node holds is refused.
    ///
    /// Regression: it used to be accepted, and because node ids are handed out monotonically, the
    /// next node created with that id inherited it. The node below is never embedded and yet
    /// answered a search at distance zero, with no error at any layer.
    #[test]
    fn a_vector_for_a_node_that_does_not_exist_is_refused() {
        let (_dir, graph) = open_tmp();
        let alice = graph
            .add_node("Person", &json!({ "name": "Alice" }))
            .unwrap();
        graph.upsert_vector(alice, &[1.0, 0.0]).unwrap();

        // The very next id, which no node holds yet.
        let future = alice + 1;
        let err = graph.upsert_vector(future, &[0.0, 1.0]).unwrap_err();
        assert!(
            matches!(err, VectorError::NodeNotFound(id) if id == future),
            "expected NodeNotFound, got {err:?}"
        );

        // Bob takes that id and must own no embedding.
        let bob = graph.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        assert_eq!(bob, future);
        let hits = graph.vector_search(&[0.0, 1.0], 5).unwrap();
        assert!(
            hits.iter().all(|h| h.node != bob),
            "a node that was never embedded must not appear in a vector search: {hits:?}"
        );
    }

    /// The rejection happens before anything is written, so a refused upsert leaves neither an
    /// index entry nor a stored vector behind. Checking after the fact is what a partial write
    /// would defeat.
    #[test]
    fn a_refused_vector_reaches_neither_the_index_nor_storage() {
        let (_dir, graph) = open_tmp();
        let real = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(real, &[1.0, 0.0]).unwrap();

        assert!(graph.upsert_vector(real + 99, &[0.0, 1.0]).is_err());
        let stored = graph.vector_bytes().unwrap();
        assert!(
            stored.iter().all(|(id, _)| *id != real + 99),
            "the refused vector must not be in storage: {:?}",
            stored.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        );
        let hits = graph.vector_search(&[0.0, 1.0], 5).unwrap();
        assert_eq!(hits.len(), 1, "only the one real embedding: {hits:?}");
        assert_eq!(hits[0].node, real);
    }

    /// Removal stays permissive, which is the escape hatch for a database written before the check
    /// existed: a caller has to be able to delete a vector whose node is already gone.
    #[test]
    fn removing_a_vector_for_a_missing_node_is_not_an_error() {
        let (_dir, graph) = open_tmp();
        let node = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(node, &[1.0, 0.0]).unwrap();
        graph.delete_node(node).unwrap();
        graph.remove_vector(node).unwrap();
    }

    #[test]
    fn metric_from_str_is_case_insensitive_with_alias() {
        assert_eq!(
            "cosine".parse::<VectorMetric>().unwrap(),
            VectorMetric::Cosine
        );
        assert_eq!("L2".parse::<VectorMetric>().unwrap(), VectorMetric::L2);
        assert_eq!("Dot".parse::<VectorMetric>().unwrap(), VectorMetric::Dot);
        assert_eq!("ip".parse::<VectorMetric>().unwrap(), VectorMetric::Dot);
        assert!("hamming".parse::<VectorMetric>().is_err());
    }

    #[test]
    fn quantization_from_str_is_case_insensitive() {
        assert_eq!(
            "float32".parse::<VectorQuantization>().unwrap(),
            VectorQuantization::Float32
        );
        assert_eq!(
            "Float16".parse::<VectorQuantization>().unwrap(),
            VectorQuantization::Float16
        );
        assert_eq!(
            "INT8".parse::<VectorQuantization>().unwrap(),
            VectorQuantization::Int8
        );
        assert!("b1".parse::<VectorQuantization>().is_err());
    }

    #[test]
    fn upsert_vector_and_search_finds_nearest() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        let c = graph.add_node("N", &json!({})).unwrap();

        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        graph.upsert_vector(c, &[0.0f32, 0.0, 1.0]).unwrap();

        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, a);
    }

    #[test]
    fn vector_search_empty_index_is_an_error() {
        let (_dir, graph) = open_tmp();
        let err = graph.vector_search(&[1.0f32, 0.0, 0.0], 5).unwrap_err();
        assert!(matches!(err, VectorError::EmptyIndex), "got {err:?}");
    }

    #[test]
    fn vector_search_after_removing_all_vectors_is_an_error() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.remove_vector(a).unwrap();
        let err = graph.vector_search(&[1.0f32, 0.0, 0.0], 5).unwrap_err();
        assert!(matches!(err, VectorError::EmptyIndex), "got {err:?}");
    }

    /// A node deleted through the core graph API lingers in the in-memory HNSW
    /// index, but `vector_search` must not return it as a "ghost" hit.
    #[test]
    fn vector_search_excludes_deleted_nodes() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();

        // Delete the closest node through the core API (not `remove_vector`), so
        // its embedding stays in the HNSW index.
        graph.delete_node(a).unwrap();

        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 5).unwrap();
        assert!(
            hits.iter().all(|h| h.node != a),
            "deleted node must not appear in vector_search results"
        );
        assert!(
            hits.iter().any(|h| h.node == b),
            "the surviving node is still searchable"
        );
    }

    /// A vacuous filter set (no label, empty property map) must not return
    /// core-deleted ghosts either: the predicate-filtered path cannot rely on
    /// its label and property lookups to reject deleted nodes when there are
    /// no lookups to make. Reachable from the REST and MCP surfaces as
    /// `"properties": {}` with no label.
    #[test]
    fn vector_search_with_empty_filters_excludes_deleted_nodes() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        graph.delete_node(a).unwrap();

        let opts = VectorSearchOptions {
            k: 5,
            properties: Some(std::collections::HashMap::new()),
            ..Default::default()
        };
        let hits = graph
            .vector_search_with(&[1.0f32, 0.0, 0.0], &opts)
            .unwrap();
        assert!(
            hits.iter().all(|h| h.node != a),
            "deleted node must not appear under an empty filter set"
        );
        assert!(
            hits.iter().any(|h| h.node == b),
            "the surviving node is still searchable"
        );
    }

    /// When enough top-ranked nodes are deleted (ghosts) on the default
    /// Float32 path, `vector_search` must still return `k` live hits by
    /// over-fetching past the ghosts, not truncate below `k`.
    #[test]
    fn vector_search_returns_k_live_hits_despite_deleted_top_ranked() {
        let (_dir, graph) = open_tmp();
        let close = [
            graph.add_node("N", &json!({})).unwrap(),
            graph.add_node("N", &json!({})).unwrap(),
            graph.add_node("N", &json!({})).unwrap(),
        ];
        graph.upsert_vector(close[0], &[1.0f32, 0.0]).unwrap();
        graph.upsert_vector(close[1], &[1.0f32, 0.1]).unwrap();
        graph.upsert_vector(close[2], &[1.0f32, 0.2]).unwrap();
        let far = [
            graph.add_node("N", &json!({})).unwrap(),
            graph.add_node("N", &json!({})).unwrap(),
            graph.add_node("N", &json!({})).unwrap(),
        ];
        graph.upsert_vector(far[0], &[1.0f32, 1.0]).unwrap();
        graph.upsert_vector(far[1], &[0.5f32, 1.0]).unwrap();
        graph.upsert_vector(far[2], &[0.0f32, 1.0]).unwrap();

        // Delete the three closest through the core API (leaving them as ghosts
        // in the HNSW index), so a fixed `fetch_k == k` would drop all results.
        for n in close {
            graph.delete_node(n).unwrap();
        }

        let hits = graph.vector_search(&[1.0f32, 0.0], 3).unwrap();
        assert_eq!(
            hits.len(),
            3,
            "must backfill past deleted nodes to return k live hits"
        );
        assert!(
            hits.iter().all(|h| far.contains(&h.node)),
            "only the live (far) nodes are returned"
        );
    }

    /// Re-applying the effective default configuration on a graph that has
    /// vectors but no explicitly persisted config is a documented no-op, not an
    /// `AlreadyConfigured` error.
    #[test]
    fn configure_default_after_upsert_is_noop() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        // No prior configure_vector_index call; the active config is the lazily
        // built default. Re-applying that default must succeed.
        assert!(
            graph
                .configure_vector_index(VectorIndexOptions::default())
                .is_ok()
        );
        // Requesting a DIFFERENT config while vectors exist is still refused.
        let other = VectorIndexOptions {
            metric: VectorMetric::L2,
            ..VectorIndexOptions::default()
        };
        assert!(graph.configure_vector_index(other).is_err());
    }

    #[test]
    fn vector_search_k_larger_than_index_returns_all() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0]).unwrap();

        let hits = graph.vector_search(&[1.0f32, 0.0], 100).unwrap();
        assert_eq!(hits.len(), 2);
    }

    /// A stored non-finite component yields a NaN distance at every later search, which no
    /// ranking can order, so it is refused rather than persisted.
    #[test]
    fn upsert_vector_rejects_a_non_finite_component() {
        let (_dir, graph) = open_tmp();
        let n = graph.add_node("N", &json!({})).unwrap();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = graph.upsert_vector(n, &[1.0, bad]).unwrap_err();
            assert!(
                err.to_string().contains("not finite"),
                "expected a non-finite rejection, got {err}"
            );
        }
        graph.upsert_vector(n, &[1.0, 2.0]).unwrap();
    }

    #[test]
    fn upsert_vector_overwrites_existing_embedding() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();

        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        graph.upsert_vector(a, &[0.0f32, 1.0, 0.0]).unwrap();

        let hits = graph.vector_search(&[0.0f32, 1.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            (hits[0].distance).abs() < 1e-5,
            "distance to query should be near zero"
        );
    }

    // Persistence-dependent: reopens the same path and expects the stored embeddings
    // or configuration to still be there. The in-memory storage backend starts empty
    // on every open by design, so this is gated rather than left to fail there.
    #[cfg(feature = "lmdb")]
    #[test]
    fn vector_index_rebuilds_from_lmdb_on_reopen() {
        let dir = TempDir::new().unwrap();
        let a = {
            let graph = Graph::open(dir.path(), 1).unwrap();
            let a = graph.add_node("N", &json!({})).unwrap();
            graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
            a
        };

        let graph = Graph::open(dir.path(), 1).unwrap();
        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, a);
    }

    #[test]
    fn remove_vector_deletes_from_index_and_lmdb() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();

        graph.remove_vector(a).unwrap();

        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 2).unwrap();
        assert!(
            hits.iter().all(|h| h.node != a),
            "removed node must not appear in search results"
        );
    }

    #[test]
    fn vector_search_with_label_filter_excludes_other_labels() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("Article", &json!({})).unwrap();
        let b = graph.add_node("Person", &json!({})).unwrap();
        let c = graph.add_node("Article", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[1.0f32, 0.0, 0.0]).unwrap(); // same direction as a
        graph.upsert_vector(c, &[0.9f32, 0.1, 0.0]).unwrap();

        let opts = VectorSearchOptions {
            k: 3,
            label: Some("Article".into()),
            properties: None,
            rescore_factor: None,
        };
        let hits = graph
            .vector_search_with(&[1.0f32, 0.0, 0.0], &opts)
            .unwrap();
        // Only Article nodes a and c must appear; Person node b must be absent.
        assert!(
            hits.iter().all(|h| h.node != b),
            "Person node must be filtered out"
        );
        assert!(hits.len() <= 2);
        assert!(hits.iter().any(|h| h.node == a));
    }

    #[test]
    fn vector_search_with_selective_property_filter_finds_distant_matches() {
        // Regression guard: a selective property filter must not silently
        // under-return. Many non-matching nodes sit nearest the query, and the
        // matching nodes rank far below them. A post-filter over a fixed
        // over-fetch would discard every candidate and return nothing; the
        // predicate-driven traversal keeps expanding until it finds them.
        let (_dir, graph) = open_tmp();
        // 200 "red" decoys, all nearer the query than any "blue" node.
        for i in 0..200u32 {
            let n = graph.add_node("N", &json!({ "team": "red" })).unwrap();
            let jitter = (i as f32) * 1e-4;
            graph.upsert_vector(n, &[1.0, jitter, 0.0]).unwrap();
        }
        // 2 "blue" matches, farther from the query in cosine distance.
        let blue1 = graph.add_node("N", &json!({ "team": "blue" })).unwrap();
        let blue2 = graph.add_node("N", &json!({ "team": "blue" })).unwrap();
        graph.upsert_vector(blue1, &[0.6, 0.8, 0.0]).unwrap();
        graph.upsert_vector(blue2, &[0.5, 0.85, 0.0]).unwrap();

        let mut filters = std::collections::HashMap::new();
        filters.insert("team".to_string(), json!("blue"));
        let opts = VectorSearchOptions {
            k: 2,
            label: None,
            properties: Some(filters),
            rescore_factor: None,
        };
        let hits = graph
            .vector_search_with(&[1.0f32, 0.0, 0.0], &opts)
            .unwrap();

        assert_eq!(hits.len(), 2, "both blue matches must be returned");
        assert!(hits.iter().any(|h| h.node == blue1));
        assert!(hits.iter().any(|h| h.node == blue2));
    }

    // Persistence-dependent: reopens the same path and expects the stored embeddings
    // or configuration to still be there. The in-memory storage backend starts empty
    // on every open by design, so this is gated rather than left to fail there.
    #[cfg(feature = "lmdb")]
    #[test]
    fn rejected_upsert_does_not_persist_and_brick_reopen() {
        // A dimension-mismatched upsert must not leave bytes in LMDB. If it did,
        // the cold-start rebuild on the next `Graph::open` would fail to decode
        // a consistent index and brick every subsequent search.
        let dir = TempDir::new().unwrap();
        let a = {
            let graph = Graph::open(dir.path(), 1).unwrap();
            let a = graph.add_node("N", &json!({})).unwrap();
            let b = graph.add_node("N", &json!({})).unwrap();
            graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
            // Wrong dimension count: must be rejected and must not persist.
            let bad = graph.upsert_vector(b, &[1.0f32, 0.0]);
            assert!(matches!(bad, Err(VectorError::DimensionMismatch { .. })));
            a
        };

        // Reopen: the rebuild must succeed and search must still work.
        let graph = Graph::open(dir.path(), 1).unwrap();
        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, a);
    }

    // Persistence-dependent: reopens the same path and expects the stored embeddings
    // or configuration to still be there. The in-memory storage backend starts empty
    // on every open by design, so this is gated rather than left to fail there.
    #[cfg(feature = "lmdb")]
    #[test]
    fn configure_vector_index_persists_metric_across_reopen() {
        let dir = TempDir::new().unwrap();
        let a = {
            let graph = Graph::open(dir.path(), 1).unwrap();
            graph
                .configure_vector_index(VectorIndexOptions {
                    metric: VectorMetric::L2,
                    quantization: VectorQuantization::Float32,
                })
                .unwrap();
            let a = graph.add_node("N", &json!({})).unwrap();
            let b = graph.add_node("N", &json!({})).unwrap();
            graph.upsert_vector(a, &[0.0f32, 0.0]).unwrap();
            graph.upsert_vector(b, &[5.0f32, 5.0]).unwrap();
            a
        };

        // Reopen: the persisted L2 metric must be used by the cold-start rebuild.
        let graph = Graph::open(dir.path(), 1).unwrap();
        let hits = graph.vector_search(&[0.1f32, 0.1], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].node, a,
            "nearest under L2 must be the origin vector"
        );
    }

    #[test]
    fn configure_vector_index_idempotent_with_same_options() {
        let (_dir, graph) = open_tmp();
        let opts = VectorIndexOptions {
            metric: VectorMetric::Dot,
            quantization: VectorQuantization::Float16,
        };
        graph.configure_vector_index(opts).unwrap();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        // Re-applying the identical configuration after vectors exist is a no-op.
        graph.configure_vector_index(opts).unwrap();
    }

    #[test]
    fn configure_vector_index_rejects_change_after_vectors_exist() {
        let (_dir, graph) = open_tmp();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::Cosine,
                quantization: VectorQuantization::Float32,
            })
            .unwrap();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0]).unwrap();

        let changed = graph.configure_vector_index(VectorIndexOptions {
            metric: VectorMetric::L2,
            quantization: VectorQuantization::Float32,
        });
        assert!(matches!(
            changed,
            Err(VectorError::AlreadyConfigured { .. })
        ));
    }

    // Persistence-dependent: reopens the same path and expects the stored embeddings
    // or configuration to still be there. The in-memory storage backend starts empty
    // on every open by design, so this is gated rather than left to fail there.
    #[cfg(feature = "lmdb")]
    #[test]
    fn reindex_vector_index_switches_metric_on_populated_graph() {
        let dir = TempDir::new().unwrap();
        let (a, b) = {
            let graph = Graph::open(dir.path(), 1).unwrap();
            // Default Cosine configuration.
            let a = graph.add_node("N", &json!({})).unwrap();
            let b = graph.add_node("N", &json!({})).unwrap();
            graph.upsert_vector(a, &[0.0f32, 0.0]).unwrap();
            graph.upsert_vector(b, &[5.0f32, 5.0]).unwrap();

            // configure must refuse the change while vectors exist.
            let refused = graph.configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::L2,
                quantization: VectorQuantization::Float32,
            });
            assert!(matches!(
                refused,
                Err(VectorError::AlreadyConfigured { .. })
            ));

            // reindex accepts it and rebuilds from the stored embeddings.
            graph
                .reindex_vector_index(VectorIndexOptions {
                    metric: VectorMetric::L2,
                    quantization: VectorQuantization::Float32,
                })
                .unwrap();
            (a, b)
        };

        // The new metric persists, and search reflects L2 geometry after reopen.
        let graph = Graph::open(dir.path(), 1).unwrap();
        let hits = graph.vector_search(&[0.1f32, 0.1], 2).unwrap();
        assert_eq!(hits[0].node, a, "origin is nearest under L2");
        assert!(hits.iter().any(|h| h.node == b));
    }

    #[test]
    fn vector_cache_is_reused_across_searches() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();

        // Both calls should return consistent results; the second uses the cached index.
        let h1 = graph.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
        let h2 = graph.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h2.len(), 1);
        assert_eq!(h1[0].node, h2[0].node);
    }

    #[test]
    fn test_concurrent_vector_searches() {
        let (_dir, graph) = open_tmp();
        let a = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();

        let graph = Arc::new(graph);
        let mut handles = vec![];
        for _ in 0..10 {
            let g = Arc::clone(&graph);
            let target_node = a;
            handles.push(std::thread::spawn(move || {
                let hits = g.vector_search(&[1.0f32, 0.0, 0.0], 1).unwrap();
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].node, target_node);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn vector_search_with_int8_quantization_finds_nearest() {
        // Int8 quantization is wired to usearch's ScalarKind::I8. Precision is
        // reduced, but well-separated vectors must still rank correctly.
        let (_dir, graph) = open_tmp();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::Cosine,
                quantization: VectorQuantization::Int8,
            })
            .unwrap();
        let a = graph.add_node("N", &json!({})).unwrap();
        let b = graph.add_node("N", &json!({})).unwrap();
        let c = graph.add_node("N", &json!({})).unwrap();
        graph.upsert_vector(a, &[1.0, 0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[0.0, 1.0, 0.0]).unwrap();
        graph.upsert_vector(c, &[0.0, 0.0, 1.0]).unwrap();

        let hits = graph.vector_search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, a);
    }

    #[test]
    fn vector_search_with_multiple_property_filters_requires_all() {
        // A property filter with several keys is an AND: only nodes matching
        // every key/value pair qualify. The nearest node matches one key but not
        // the other and must be excluded.
        let (_dir, graph) = open_tmp();
        let near = graph
            .add_node("N", &json!({ "team": "blue", "role": "ic" }))
            .unwrap();
        let far = graph
            .add_node("N", &json!({ "team": "blue", "role": "lead" }))
            .unwrap();
        graph.upsert_vector(near, &[1.0, 0.0, 0.0]).unwrap();
        graph.upsert_vector(far, &[0.9, 0.1, 0.0]).unwrap();

        let mut filters = std::collections::HashMap::new();
        filters.insert("team".to_string(), json!("blue"));
        filters.insert("role".to_string(), json!("lead"));
        let opts = VectorSearchOptions {
            k: 2,
            label: None,
            properties: Some(filters),
            rescore_factor: None,
        };
        let hits = graph.vector_search_with(&[1.0, 0.0, 0.0], &opts).unwrap();

        // `near` is closer but is role=ic, so only `far` satisfies both filters.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, far);
    }

    #[test]
    fn vector_search_quantized_rescore() {
        let (_dir, graph) = open_tmp();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::Cosine,
                quantization: VectorQuantization::Int8,
            })
            .unwrap();

        let n1 = graph.add_node("N", &json!({})).unwrap();
        let n2 = graph.add_node("N", &json!({})).unwrap();

        graph.upsert_vector(n1, &[0.9, 0.1]).unwrap();
        graph.upsert_vector(n2, &[0.95, 0.05]).unwrap();

        let query = &[1.0, 0.0];

        // Search with rescoring active
        let opts = VectorSearchOptions {
            k: 2,
            rescore_factor: Some(2),
            ..Default::default()
        };
        let hits = graph.vector_search_with(query, &opts).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].node, n2);
        assert_eq!(hits[1].node, n1);

        assert!(hits[0].distance < hits[1].distance);
    }
}
