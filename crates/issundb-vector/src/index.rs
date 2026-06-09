use std::cell::RefCell;
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::instrument;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::error::VectorError;
use issundb_core::{Graph, NodeId};

/// A single result from vector search.
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
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self {
            k: 10,
            label: None,
            properties: None,
        }
    }
}

/// Distance metric for the HNSW index.
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

/// Construction options for `VectorIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorIndexOptions {
    pub metric: VectorMetric,
    pub quantization: VectorQuantization,
}

enum Inner {
    Empty,
    Ready { index: Index, dims: usize },
}

/// An in-memory HNSW vector index using the `usearch` library.
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
                let opts = IndexOptions {
                    dimensions: dims,
                    metric: metric_to_usearch(self.opts.metric),
                    quantization: quantization_to_usearch(self.opts.quantization),
                    ..Default::default()
                };
                let index =
                    Index::new(&opts).map_err(|e| VectorError::IndexFault(e.to_string()))?;
                index
                    .reserve(64)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                index
                    .add(node, v)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                *guard = Inner::Ready { index, dims };
            }
            Inner::Ready { index, dims: d } => {
                if dims != *d {
                    return Err(VectorError::DimensionMismatch {
                        expected: *d,
                        got: dims,
                    });
                }
                if index.contains(node) {
                    index
                        .remove(node)
                        .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                }
                if index.size() >= index.capacity() {
                    let new_cap = (index.capacity() * 2).max(64);
                    index
                        .reserve(new_cap)
                        .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                }
                index
                    .add(node, v)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Remove the embedding for `node` from the index.
    pub fn remove(&self, node: NodeId) -> Result<(), VectorError> {
        let mut guard = self.inner.write();
        if let Inner::Ready { index, .. } = &mut *guard {
            if index.contains(node) {
                index
                    .remove(node)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    ///
    /// Returns an empty slice when the index has no vectors or `k == 0`.
    /// `k` is silently clamped to the number of indexed vectors.
    pub fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        let guard = self.inner.read();
        match &*guard {
            Inner::Empty => Ok(vec![]),
            Inner::Ready { index, dims } => {
                if q.len() != *dims {
                    return Err(VectorError::DimensionMismatch {
                        expected: *dims,
                        got: q.len(),
                    });
                }
                if k == 0 || index.size() == 0 {
                    return Ok(vec![]);
                }
                let actual_k = k.min(index.size());
                let matches = index
                    .search::<f32>(q, actual_k)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                Ok(matches
                    .keys
                    .iter()
                    .zip(matches.distances.iter())
                    .map(|(&node, &distance)| Hit { node, distance })
                    .collect())
            }
        }
    }

    /// Return up to `k` approximate nearest neighbors to `q` that satisfy
    /// `predicate`.
    ///
    /// The predicate is evaluated during the HNSW traversal, so the search keeps
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
            Inner::Ready { index, dims } => {
                if q.len() != *dims {
                    return Err(VectorError::DimensionMismatch {
                        expected: *dims,
                        got: q.len(),
                    });
                }
                if k == 0 || index.size() == 0 {
                    return Ok(vec![]);
                }
                let actual_k = k.min(index.size());
                let matches = index
                    .filtered_search::<f32, _>(q, actual_k, predicate)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
                Ok(matches
                    .keys
                    .iter()
                    .zip(matches.distances.iter())
                    .map(|(&node, &distance)| Hit { node, distance })
                    .collect())
            }
        }
    }
}

fn encode_vector(v: &[f32]) -> Result<Vec<u8>, VectorError> {
    if v.is_empty() {
        return Err(VectorError::IndexFault(
            "embedding must not be empty".into(),
        ));
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
    fn configure_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError>;

    /// Change the metric and quantization and rebuild the index from the
    /// persisted embeddings under the new configuration.
    ///
    /// Unlike `configure_vector_index`, this accepts a change after vectors
    /// exist. The raw f32 embeddings are stored in LMDB independently of the
    /// metric, so they are re-indexed under `opts`; switching back to `Float32`
    /// recovers full precision from storage. This rebuilds the entire in-memory
    /// HNSW index, so it is O(n) in the number of stored vectors and is intended
    /// as an administrative operation, not a concurrent one: running it while
    /// other threads upsert may drop an in-flight write from the snapshot, which
    /// the next `Graph::open` rebuild reconciles.
    fn reindex_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError>;

    /// Persist `v` under `n`.
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError>;

    /// Remove the embedding for `n` from the index and from persistent storage.
    fn remove_vector(&self, n: NodeId) -> Result<(), VectorError>;

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>;

    /// Return the `opts.k` approximate nearest neighbors that satisfy the label
    /// and property filters in `opts`.
    ///
    /// When neither `opts.label` nor `opts.properties` is set the call is
    /// identical to `vector_search(q, opts.k)`. When a filter is set, it is
    /// applied during the HNSW traversal through a predicate, so the search
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
}

/// Key type used to store the persistent HNSW cache in `Graph::extensions`.
struct VectorIndexCache(VectorIndex);

impl VectorGraphExt for Graph {
    fn configure_vector_index(&self, opts: VectorIndexOptions) -> Result<(), VectorError> {
        let current = load_config(self)?;
        if current == Some(opts) {
            return Ok(());
        }
        // The HNSW graph is built per-metric. Changing the metric or
        // quantization once embeddings exist would silently reinterpret them on
        // the next cold-start rebuild, so refuse it while vectors are present.
        if !self.vector_bytes()?.is_empty() {
            return Err(VectorError::AlreadyConfigured {
                existing: format!("{:?}", current.unwrap_or_default()),
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
        // Persist the new configuration, rebuild the index from the stored raw
        // embeddings, then swap the cache atomically. The build runs before the
        // swap so a mid-rebuild failure leaves the previous cache in place.
        self.put_vector_config(&encode_config(opts))?;
        let rebuilt = build_index(self, opts)?;
        self.set_extension(Arc::new(VectorIndexCache(rebuilt)));
        Ok(())
    }

    #[instrument(skip(self, v), fields(node = %n, dims = v.len()))]
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError> {
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
        // Remove from in-memory HNSW index if the cache has been built.
        if let Some(arc) = self.get_extension::<VectorIndexCache>() {
            arc.0.remove(n)?;
        }
        Ok(())
    }

    #[instrument(skip(self, q), fields(k = %k, dims = q.len()))]
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        let arc = get_or_init_cache(self)?;

        arc.0.search(q, k)
    }

    #[instrument(skip(self, q), fields(k = %opts.k, label = ?opts.label, dims = q.len()))]
    fn vector_search_with(
        &self,
        q: &[f32],
        opts: &VectorSearchOptions,
    ) -> Result<Vec<Hit>, VectorError> {
        if opts.label.is_none() && opts.properties.is_none() {
            return self.vector_search(q, opts.k);
        }

        let arc = get_or_init_cache(self)?;

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

        let hits = arc.0.search_filtered(q, opts.k, predicate)?;
        if let Some(e) = pred_err.into_inner() {
            return Err(e);
        }
        Ok(hits)
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

fn metric_to_usearch(m: VectorMetric) -> MetricKind {
    match m {
        VectorMetric::Cosine => MetricKind::Cos,
        VectorMetric::L2 => MetricKind::L2sq,
        VectorMetric::Dot => MetricKind::IP,
    }
}

fn quantization_to_usearch(q: VectorQuantization) -> ScalarKind {
    match q {
        VectorQuantization::Float32 => ScalarKind::F32,
        VectorQuantization::Float16 => ScalarKind::F16,
        VectorQuantization::Int8 => ScalarKind::I8,
    }
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
    fn vector_search_empty_index_returns_empty() {
        let (_dir, graph) = open_tmp();
        let hits = graph.vector_search(&[1.0f32, 0.0, 0.0], 5).unwrap();
        assert!(hits.is_empty());
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
        };
        let hits = graph
            .vector_search_with(&[1.0f32, 0.0, 0.0], &opts)
            .unwrap();

        assert_eq!(hits.len(), 2, "both blue matches must be returned");
        assert!(hits.iter().any(|h| h.node == blue1));
        assert!(hits.iter().any(|h| h.node == blue2));
    }

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
        };
        let hits = graph.vector_search_with(&[1.0, 0.0, 0.0], &opts).unwrap();

        // `near` is closer but is role=ic, so only `far` satisfies both filters.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, far);
    }
}
