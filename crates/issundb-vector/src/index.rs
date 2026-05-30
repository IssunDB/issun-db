use std::sync::Arc;

use parking_lot::Mutex;
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
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self { k: 10, label: None }
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
    /// Hamming distance (for binary vectors).
    Hamming,
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
#[derive(Debug, Clone, Copy, Default)]
pub struct VectorIndexOptions {
    pub metric: VectorMetric,
    pub quantization: VectorQuantization,
}

enum Inner {
    Empty,
    Ready { index: Index, dims: usize },
}

/// An in-memory HNSW vector index using the `usearch` library.
pub struct VectorIndex {
    opts: VectorIndexOptions,
    inner: Mutex<Inner>,
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
            inner: Mutex::new(Inner::Empty),
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
        let mut guard = self.inner.lock();
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
        let mut guard = self.inner.lock();
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
        let guard = self.inner.lock();
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
    /// Persist `v` under `n`.
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError>;

    /// Remove the embedding for `n` from the index and from persistent storage.
    fn remove_vector(&self, n: NodeId) -> Result<(), VectorError>;

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>;

    /// Return the `k` approximate nearest neighbors whose label matches `opts.label`.
    ///
    /// When `opts.label` is `None` the call is identical to `vector_search(q, opts.k)`.
    /// When a label filter is set the index is over-fetched (up to `opts.k * 4` candidates)
    /// and candidates whose stored label does not match are discarded. The first `opts.k`
    /// survivors are returned; fewer may be returned when the index contains fewer matching
    /// nodes.
    fn vector_search_with(
        &self,
        q: &[f32],
        opts: &VectorSearchOptions,
    ) -> Result<Vec<Hit>, VectorError>;
}

/// Key type used to store the persistent HNSW cache in `Graph::extensions`.
struct VectorIndexCache(Mutex<VectorIndex>);

impl VectorGraphExt for Graph {
    #[instrument(skip(self, v), fields(node = %n, dims = v.len()))]
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), VectorError> {
        let bytes = encode_vector(v)?;
        self.put_vector_bytes(n, &bytes)?;
        // Update (or cold-start) the cached HNSW index.
        let arc = get_or_init_cache(self)?;
        let result = arc.0.lock().upsert(n, v);
        result
    }

    fn remove_vector(&self, n: NodeId) -> Result<(), VectorError> {
        self.delete_vector_bytes(n)?;
        // Remove from in-memory HNSW index if the cache has been built.
        if let Some(arc) = self.get_extension::<VectorIndexCache>() {
            arc.0.lock().remove(n)?;
        }
        Ok(())
    }

    #[instrument(skip(self, q), fields(k = %k, dims = q.len()))]
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
        let arc = get_or_init_cache(self)?;
        let result = arc.0.lock().search(q, k);
        result
    }

    #[instrument(skip(self, q), fields(k = %opts.k, label = ?opts.label, dims = q.len()))]
    fn vector_search_with(
        &self,
        q: &[f32],
        opts: &VectorSearchOptions,
    ) -> Result<Vec<Hit>, VectorError> {
        let label_filter = match &opts.label {
            None => return self.vector_search(q, opts.k),
            Some(l) => l.clone(),
        };

        // Over-fetch to increase the chance of finding opts.k label-matching results.
        let fetch_k = (opts.k * 4).max(opts.k + 64);
        let candidates = self.vector_search(q, fetch_k)?;

        let mut out = Vec::with_capacity(opts.k);
        for hit in candidates {
            if out.len() >= opts.k {
                break;
            }
            // Keep the hit if any of the node's labels matches the filter.
            if let Ok(labels) = self.node_labels(hit.node) {
                if labels.iter().any(|l| *l == label_filter) {
                    out.push(hit);
                }
            }
        }
        Ok(out)
    }
}

/// Return the cached `VectorIndexCache` for this Graph, building it from LMDB
/// if it has not been initialised yet.
fn get_or_init_cache(graph: &Graph) -> Result<Arc<VectorIndexCache>, VectorError> {
    if let Some(existing) = graph.get_extension::<VectorIndexCache>() {
        return Ok(existing);
    }
    // Cold start: load all vectors from LMDB into a fresh HNSW index.
    // Call vector_bytes() BEFORE acquiring the extensions lock to avoid
    // holding two locks simultaneously.
    let all_bytes = graph.vector_bytes()?;
    let idx = VectorIndex::new();
    for (node_id, bytes) in all_bytes {
        let v = decode_vector(&bytes)?;
        idx.upsert(node_id, &v)?;
    }
    let arc = Arc::new(VectorIndexCache(Mutex::new(idx)));

    // Double check under lock before inserting to prevent overwriting a concurrently initialized index.
    let mut ext = graph.extensions.lock();
    use std::any::TypeId;
    if let Some(existing) = ext
        .get(&TypeId::of::<VectorIndexCache>())
        .and_then(|b| b.downcast_ref::<Arc<VectorIndexCache>>())
    {
        return Ok(existing.clone());
    }
    ext.insert(TypeId::of::<VectorIndexCache>(), Box::new(arc.clone()));
    Ok(arc)
}

fn metric_to_usearch(m: VectorMetric) -> MetricKind {
    match m {
        VectorMetric::Cosine => MetricKind::Cos,
        VectorMetric::L2 => MetricKind::L2sq,
        VectorMetric::Dot => MetricKind::IP,
        VectorMetric::Hamming => MetricKind::Hamming,
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
}
