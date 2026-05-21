use parking_lot::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::{error::Error, schema::NodeId};

/// A single result from `Graph::vector_search`.
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
