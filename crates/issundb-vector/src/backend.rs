//! The index behind `VectorIndex`, selected at compile time.
//!
//! Everything above this module (the `VectorGraphExt` implementation, the storage
//! integration, the label and property filters, and the rescore pass) is the same
//! whichever backend is compiled. This exists so the HNSW library, which is C++
//! reached through `cxx`, can be left out of a build without removing vector search
//! from the API: `usearch` cannot cross-compile to a target with no C++ toolchain,
//! and it is the only C++ dependency in the workspace.
//!
//! With the default `hnsw` feature the backend is `usearch`, an approximate index.
//! Without it the backend is an exact scan, which is slower per query by a factor of
//! the vector count and does not honor `quantization` (it keeps the raw `f32` it was
//! given). It is not a stub: the results are the true nearest neighbors under the
//! same distance conventions, so the crate's own tests hold for both.

use issundb_core::NodeId;

use crate::error::VectorError;
use crate::index::{Hit, VectorIndexOptions, VectorMetric};

/// The operations `VectorIndex` needs from an index.
///
/// Mutations take `&mut self` even though the HNSW library's are interior-mutable,
/// because the exact backend genuinely needs the borrow and the callers already hold
/// the write side of a lock. The two searches take `&self` for the same reason in
/// reverse: they run under the read side.
pub(crate) trait VectorBackend: Send + Sync {
    fn upsert(&mut self, node: NodeId, v: &[f32]) -> Result<(), VectorError>;
    fn remove(&mut self, node: NodeId) -> Result<(), VectorError>;
    fn len(&self) -> usize;
    fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>;
    /// `predicate` is by reference so the trait stays object-safe; the HNSW backend
    /// evaluates it during traversal, the exact backend before ranking.
    fn search_filtered(
        &self,
        q: &[f32],
        k: usize,
        predicate: &dyn Fn(NodeId) -> bool,
    ) -> Result<Vec<Hit>, VectorError>;
}

/// Build the backend this crate was compiled with, sized for `dims`.
pub(crate) fn new_backend(
    dims: usize,
    opts: &VectorIndexOptions,
) -> Result<Box<dyn VectorBackend>, VectorError> {
    #[cfg(feature = "hnsw")]
    {
        Ok(Box::new(hnsw::HnswBackend::new(dims, opts)?))
    }
    #[cfg(not(feature = "hnsw"))]
    {
        Ok(Box::new(exact::ExactBackend::new(dims, opts)))
    }
}

/// Exact nearest-neighbor search over the vectors it was given.
///
/// Compiled when it is the selected backend, and additionally under `test`, so its own
/// tests run in both configurations and it cannot rot while the HNSW feature is on. The
/// `test` arm is what keeps it honest without an `allow(dead_code)`: in a non-test build
/// with `hnsw` on, nothing constructs it and it is correctly absent.
#[cfg(any(not(feature = "hnsw"), test))]
pub(crate) mod exact {
    use super::*;
    use crate::index::exact_distance;

    pub(crate) struct ExactBackend {
        dims: usize,
        metric: VectorMetric,
        /// Insertion-ordered, and the search sorts by `(distance, node)`, so the
        /// result order does not depend on this order for equal distances.
        vectors: Vec<(NodeId, Vec<f32>)>,
    }

    impl ExactBackend {
        pub(crate) fn new(dims: usize, opts: &VectorIndexOptions) -> Self {
            Self {
                dims,
                metric: opts.metric,
                vectors: Vec::new(),
            }
        }

        fn rank(
            &self,
            q: &[f32],
            k: usize,
            keep: impl Fn(NodeId) -> bool,
        ) -> Result<Vec<Hit>, VectorError> {
            if q.len() != self.dims {
                return Err(VectorError::DimensionMismatch {
                    expected: self.dims,
                    got: q.len(),
                });
            }
            if k == 0 {
                return Ok(vec![]);
            }
            let mut hits: Vec<Hit> = self
                .vectors
                .iter()
                .filter(|(node, _)| keep(*node))
                .map(|(node, v)| Hit {
                    node: *node,
                    distance: exact_distance(q, v, self.metric),
                })
                .collect();
            // Total order including the node id, so equal distances rank
            // deterministically rather than by whatever order insertion left.
            hits.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.node.cmp(&b.node))
            });
            hits.truncate(k);
            Ok(hits)
        }
    }

    impl VectorBackend for ExactBackend {
        fn upsert(&mut self, node: NodeId, v: &[f32]) -> Result<(), VectorError> {
            match self.vectors.iter_mut().find(|(n, _)| *n == node) {
                Some((_, slot)) => {
                    slot.clear();
                    slot.extend_from_slice(v);
                }
                None => self.vectors.push((node, v.to_vec())),
            }
            Ok(())
        }

        fn remove(&mut self, node: NodeId) -> Result<(), VectorError> {
            self.vectors.retain(|(n, _)| *n != node);
            Ok(())
        }

        fn len(&self) -> usize {
            self.vectors.len()
        }

        fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
            self.rank(q, k, |_| true)
        }

        fn search_filtered(
            &self,
            q: &[f32],
            k: usize,
            predicate: &dyn Fn(NodeId) -> bool,
        ) -> Result<Vec<Hit>, VectorError> {
            // An exact scan sees every candidate, so a selective filter cannot
            // truncate the result set the way an approximate traversal can.
            self.rank(q, k, predicate)
        }
    }
}

#[cfg(feature = "hnsw")]
pub(crate) mod hnsw {
    use super::*;
    use crate::index::VectorQuantization;
    use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

    pub(crate) struct HnswBackend {
        dims: usize,
        index: Index,
    }

    impl HnswBackend {
        pub(crate) fn new(dims: usize, opts: &VectorIndexOptions) -> Result<Self, VectorError> {
            let index_opts = IndexOptions {
                dimensions: dims,
                metric: metric_to_usearch(opts.metric),
                quantization: quantization_to_usearch(opts.quantization),
                ..Default::default()
            };
            let index =
                Index::new(&index_opts).map_err(|e| VectorError::IndexFault(e.to_string()))?;
            index
                .reserve(64)
                .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            Ok(Self { dims, index })
        }

        fn hits(matches: usearch::ffi::Matches) -> Vec<Hit> {
            matches
                .keys
                .iter()
                .zip(matches.distances.iter())
                .map(|(&node, &distance)| Hit { node, distance })
                .collect()
        }
    }

    impl VectorBackend for HnswBackend {
        fn upsert(&mut self, node: NodeId, v: &[f32]) -> Result<(), VectorError> {
            if self.index.contains(node) {
                self.index
                    .remove(node)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            }
            if self.index.size() >= self.index.capacity() {
                let new_cap = (self.index.capacity() * 2).max(64);
                self.index
                    .reserve(new_cap)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            }
            self.index
                .add(node, v)
                .map_err(|e| VectorError::IndexFault(e.to_string()))
        }

        fn remove(&mut self, node: NodeId) -> Result<(), VectorError> {
            if self.index.contains(node) {
                self.index
                    .remove(node)
                    .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            }
            Ok(())
        }

        fn len(&self) -> usize {
            self.index.size()
        }

        fn search(&self, q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError> {
            if q.len() != self.dims {
                return Err(VectorError::DimensionMismatch {
                    expected: self.dims,
                    got: q.len(),
                });
            }
            if k == 0 || self.index.size() == 0 {
                return Ok(vec![]);
            }
            let actual_k = k.min(self.index.size());
            let matches = self
                .index
                .search::<f32>(q, actual_k)
                .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            Ok(Self::hits(matches))
        }

        fn search_filtered(
            &self,
            q: &[f32],
            k: usize,
            predicate: &dyn Fn(NodeId) -> bool,
        ) -> Result<Vec<Hit>, VectorError> {
            if q.len() != self.dims {
                return Err(VectorError::DimensionMismatch {
                    expected: self.dims,
                    got: q.len(),
                });
            }
            if k == 0 || self.index.size() == 0 {
                return Ok(vec![]);
            }
            let actual_k = k.min(self.index.size());
            // Evaluated during the traversal, so the search keeps expanding until it
            // has `k` matching neighbors rather than post-filtering a fixed over-fetch.
            let matches = self
                .index
                .filtered_search::<f32, _>(q, actual_k, predicate)
                .map_err(|e| VectorError::IndexFault(e.to_string()))?;
            Ok(Self::hits(matches))
        }
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
}

#[cfg(test)]
mod tests {
    use super::exact::ExactBackend;
    use super::*;
    use crate::index::VectorQuantization;

    fn opts(metric: VectorMetric) -> VectorIndexOptions {
        VectorIndexOptions {
            metric,
            quantization: VectorQuantization::Float32,
        }
    }

    fn backend(metric: VectorMetric, rows: &[(NodeId, Vec<f32>)]) -> ExactBackend {
        let mut b = ExactBackend::new(rows[0].1.len(), &opts(metric));
        for (node, v) in rows {
            b.upsert(*node, v).unwrap();
        }
        b
    }

    /// The exact backend is what a build without the `hnsw` feature searches through,
    /// and these run in either configuration so it cannot rot while the feature is on.
    #[test]
    fn ranks_by_true_distance_and_clamps_k() {
        let b = backend(
            VectorMetric::L2,
            &[
                (1, vec![0.0, 0.0]),
                (2, vec![1.0, 0.0]),
                (3, vec![5.0, 0.0]),
            ],
        );
        let hits = b.search(&[0.9, 0.0], 2).unwrap();
        assert_eq!(hits.iter().map(|h| h.node).collect::<Vec<_>>(), vec![2, 1]);
        // Squared L2, the convention the HNSW backend reports for the same metric.
        assert!((hits[0].distance - 0.01).abs() < 1e-6, "{:?}", hits[0]);
        assert_eq!(
            b.search(&[0.0, 0.0], 99).unwrap().len(),
            3,
            "k clamps to the count"
        );
        assert!(b.search(&[0.0, 0.0], 0).unwrap().is_empty());
    }

    #[test]
    fn equal_distances_rank_by_node_id() {
        // Two vectors equidistant from the query: the order must not depend on
        // insertion, or a caller's top-k would vary between runs.
        let b = backend(VectorMetric::L2, &[(7, vec![1.0]), (3, vec![-1.0])]);
        let hits = b.search(&[0.0], 2).unwrap();
        assert_eq!(hits.iter().map(|h| h.node).collect::<Vec<_>>(), vec![3, 7]);
    }

    #[test]
    fn upsert_replaces_and_remove_deletes() {
        let mut b = backend(VectorMetric::L2, &[(1, vec![9.0]), (2, vec![0.5])]);
        assert_eq!(b.len(), 2);
        b.upsert(1, &[0.0]).unwrap();
        assert_eq!(b.len(), 2, "an upsert replaces rather than appends");
        assert_eq!(b.search(&[0.0], 1).unwrap()[0].node, 1);
        b.remove(1).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.search(&[0.0], 1).unwrap()[0].node, 2);
        b.remove(1).unwrap();
        assert_eq!(b.len(), 1, "removing an absent node is not an error");
    }

    #[test]
    fn a_filter_cannot_truncate_an_exact_scan() {
        // The HNSW path can exhaust its traversal before finding `k` matches; a scan
        // sees every candidate, so a selective filter still returns a full result set.
        let rows: Vec<(NodeId, Vec<f32>)> = (0..20).map(|i| (i, vec![i as f32])).collect();
        let b = backend(VectorMetric::L2, &rows);
        let hits = b.search_filtered(&[0.0], 3, &|node| node % 7 == 0).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.node).collect::<Vec<_>>(),
            vec![0, 7, 14]
        );
    }

    #[test]
    fn dimension_mismatch_is_reported_by_both_searches() {
        let b = backend(VectorMetric::Cosine, &[(1, vec![1.0, 0.0])]);
        assert!(matches!(
            b.search(&[1.0], 1),
            Err(VectorError::DimensionMismatch {
                expected: 2,
                got: 1
            })
        ));
        assert!(matches!(
            b.search_filtered(&[1.0, 0.0, 0.0], 1, &|_| true),
            Err(VectorError::DimensionMismatch {
                expected: 2,
                got: 3
            })
        ));
    }

    #[test]
    fn each_metric_uses_its_documented_convention() {
        // Cosine is `1 - cos`, inner product is `1 - dot`, L2 is squared.
        let cos = backend(VectorMetric::Cosine, &[(1, vec![1.0, 0.0])]);
        assert!(cos.search(&[1.0, 0.0], 1).unwrap()[0].distance.abs() < 1e-6);
        let dot = backend(VectorMetric::Dot, &[(1, vec![2.0, 0.0])]);
        assert!((dot.search(&[3.0, 0.0], 1).unwrap()[0].distance - (1.0 - 6.0)).abs() < 1e-6);
        let l2 = backend(VectorMetric::L2, &[(1, vec![3.0, 4.0])]);
        assert!((l2.search(&[0.0, 0.0], 1).unwrap()[0].distance - 25.0).abs() < 1e-6);
    }
}
