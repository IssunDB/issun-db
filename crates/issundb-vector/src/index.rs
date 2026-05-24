use parking_lot::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use issundb_core::{Error, Graph, NodeId};

/// A single result from vector search.
pub struct Hit {
    pub node: NodeId,
    pub distance: f32,
}

enum Inner {
    Empty,
    Ready { index: Index, dims: usize },
}

/// In-memory HNSW vector index backed by usearch.
///
/// Dimensions are inferred from the first call to `upsert`. All subsequent
/// calls must supply the same number of dimensions or an error is returned.
/// Thread safety is provided by an internal `Mutex`; the graph write lock
/// serializes upserts, while searches may run concurrently.
pub struct VectorIndex {
    inner: Mutex<Inner>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndex {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::Empty),
        }
    }

    /// Insert or replace the embedding for `node`.
    ///
    /// On the first call, the index is initialised with `v.len()` dimensions
    /// using cosine distance. Subsequent calls with a different dimension count
    /// return `Error::Vector`.
    pub fn upsert(&self, node: NodeId, v: &[f32]) -> Result<(), Error> {
        let dims = v.len();
        if dims == 0 {
            return Err(Error::Vector("embedding must not be empty".into()));
        }
        let mut guard = self.inner.lock();
        match &mut *guard {
            Inner::Empty => {
                let opts = IndexOptions {
                    dimensions: dims,
                    metric: MetricKind::Cos,
                    quantization: ScalarKind::F32,
                    ..Default::default()
                };
                let index = Index::new(&opts).map_err(|e| Error::Vector(e.to_string()))?;
                index
                    .reserve(64)
                    .map_err(|e| Error::Vector(e.to_string()))?;
                index
                    .add(node, v)
                    .map_err(|e| Error::Vector(e.to_string()))?;
                *guard = Inner::Ready { index, dims };
            }
            Inner::Ready { index, dims: d } => {
                if dims != *d {
                    return Err(Error::Vector(format!(
                        "expected {d}-dimensional embedding, got {dims}"
                    )));
                }
                if index.contains(node) {
                    index
                        .remove(node)
                        .map_err(|e| Error::Vector(e.to_string()))?;
                }
                if index.size() >= index.capacity() {
                    let new_cap = (index.capacity() * 2).max(64);
                    index
                        .reserve(new_cap)
                        .map_err(|e| Error::Vector(e.to_string()))?;
                }
                index
                    .add(node, v)
                    .map_err(|e| Error::Vector(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Remove the embedding for `node` from the index.
    pub fn remove(&self, node: NodeId) -> Result<(), Error> {
        let mut guard = self.inner.lock();
        if let Inner::Ready { index, .. } = &mut *guard {
            if index.contains(node) {
                index
                    .remove(node)
                    .map_err(|e| Error::Vector(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    ///
    /// Returns an empty slice when the index has no vectors or `k == 0`.
    /// `k` is silently clamped to the number of indexed vectors.
    pub fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, Error> {
        let guard = self.inner.lock();
        match &*guard {
            Inner::Empty => Ok(vec![]),
            Inner::Ready { index, dims } => {
                if q.len() != *dims {
                    return Err(Error::Vector(format!(
                        "expected {dims}-dimensional query, got {}",
                        q.len()
                    )));
                }
                if k == 0 || index.size() == 0 {
                    return Ok(vec![]);
                }
                let actual_k = k.min(index.size());
                let matches = index
                    .search::<f32>(q, actual_k)
                    .map_err(|e| Error::Vector(e.to_string()))?;
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

fn encode_vector(v: &[f32]) -> Result<Vec<u8>, Error> {
    if v.is_empty() {
        return Err(Error::Vector("embedding must not be empty".into()));
    }
    Ok(v.iter().flat_map(|f| f.to_le_bytes()).collect())
}

fn decode_vector(bytes: &[u8]) -> Result<Vec<f32>, Error> {
    if bytes.len() % 4 != 0 {
        return Err(Error::Vector(format!(
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
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), Error>;

    /// Return the `k` approximate nearest neighbors to `q` by cosine distance.
    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, Error>;
}

impl VectorGraphExt for Graph {
    fn upsert_vector(&self, n: NodeId, v: &[f32]) -> Result<(), Error> {
        let bytes = encode_vector(v)?;
        self.put_vector_bytes(n, &bytes)
    }

    fn vector_search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, Error> {
        let index = VectorIndex::new();
        for (node_id, bytes) in self.vector_bytes()? {
            let vector = decode_vector(&bytes)?;
            index.upsert(node_id, &vector)?;
        }
        index.search(q, k)
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
}
