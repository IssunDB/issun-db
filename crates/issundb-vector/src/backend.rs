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
//! Without it the backend is an exact scan, which is slower per *query* by a factor of
//! the vector count and does not honor `quantization` (it keeps the raw `f32` it was
//! given). It is not a stub: the results are the true nearest neighbors under the
//! same distance conventions, so the crate's own tests hold for both.
//!
//! Only the query is linear. Insertion and removal are `O(1)`, which matters because
//! those are what a *rebuild* does, once per stored vector, and the index is rebuilt
//! from the persisted embeddings on every `Graph::open`; see `ExactBackend::slots`.
//! `exact_backend_cost` is the harness for deciding whether a linear query is still
//! acceptable at a given vector count, which is the question that would justify
//! replacing this with a pure-Rust approximate index.

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
    use std::collections::HashMap;

    use super::*;
    use crate::index::exact_distance;

    pub(crate) struct ExactBackend {
        dims: usize,
        metric: VectorMetric,
        /// The vectors, in no particular order: a removal swaps the last entry into the
        /// hole it leaves. That is safe because the search sorts by `(distance, node)`,
        /// a total order, so no result depends on where an entry sits here.
        vectors: Vec<(NodeId, Vec<f32>)>,
        /// Each node's slot in `vectors`.
        ///
        /// The point of the map is that `upsert` and `remove` are the operations a
        /// *rebuild* performs, once per stored vector, and the index is rebuilt from the
        /// persisted embeddings on every `Graph::open`. Finding the slot by scanning made
        /// each one `O(n)` and so the rebuild `O(n^2)`: about 5x10^9 comparisons for
        /// 100 k vectors, spent before the first query could run.
        slots: HashMap<NodeId, usize>,
    }

    impl ExactBackend {
        pub(crate) fn new(dims: usize, opts: &VectorIndexOptions) -> Self {
            Self {
                dims,
                metric: opts.metric,
                vectors: Vec::new(),
                slots: HashMap::new(),
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
            // `total_cmp` rather than `partial_cmp`, because a NaN distance makes the
            // latter's `unwrap_or(Equal)` non-transitive (a NaN compares equal to two
            // values that differ), and `sort_by` panics on a comparator that is not a
            // total order. A NaN is reachable from a Cosine norm that overflows.
            // The node id breaks a distance tie, so equal distances rank deterministically
            // rather than by whatever order insertion left.
            let order =
                |a: &Hit, b: &Hit| a.distance.total_cmp(&b.distance).then(a.node.cmp(&b.node));
            // Selecting the k smallest first keeps this O(n + k log k) rather than sorting
            // every stored vector to return k of them.
            if hits.len() > k {
                hits.select_nth_unstable_by(k - 1, order);
                hits.truncate(k);
            }
            hits.sort_unstable_by(order);
            Ok(hits)
        }
    }

    impl VectorBackend for ExactBackend {
        fn upsert(&mut self, node: NodeId, v: &[f32]) -> Result<(), VectorError> {
            // The HNSW backend's `add` rejects a wrong-length vector, so this has to as
            // well, or the two backends disagree on an error case and the shared suite stops
            // proving anything about it. Accepting one is worse than an error: `exact_distance`
            // zips the two slices, so a short vector would score over its prefix and rank
            // against full-length ones.
            if v.len() != self.dims {
                return Err(VectorError::DimensionMismatch {
                    expected: self.dims,
                    got: v.len(),
                });
            }
            match self.slots.get(&node) {
                Some(&slot) => {
                    // Reuse the allocation rather than replacing the `Vec`, so a
                    // re-upsert of the same node does not churn the heap.
                    let stored = &mut self.vectors[slot].1;
                    stored.clear();
                    stored.extend_from_slice(v);
                }
                None => {
                    self.slots.insert(node, self.vectors.len());
                    self.vectors.push((node, v.to_vec()));
                }
            }
            Ok(())
        }

        fn remove(&mut self, node: NodeId) -> Result<(), VectorError> {
            let Some(slot) = self.slots.remove(&node) else {
                return Ok(());
            };
            let last = self.vectors.len() - 1;
            self.vectors.swap_remove(slot);
            // `swap_remove` moved the final entry into the vacated slot, so that entry's
            // recorded position is now wrong. Missing this is the one way this
            // bookkeeping can go bad, and it goes bad silently: the map would point at
            // another node's vector, and a search would answer with it.
            if slot != last {
                let moved = self.vectors[slot].0;
                self.slots.insert(moved, slot);
            }
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

    /// Both backends must refuse a wrong-length vector, or the shared suite proves nothing
    /// about the case. Accepting one is not a lesser failure: `exact_distance` zips the two
    /// slices, so a short vector would be scored over its prefix and ranked against
    /// full-length ones.
    #[test]
    fn upsert_refuses_a_vector_of_the_wrong_length() {
        let mut b = backend(VectorMetric::L2, &[(1, vec![1.0, 2.0])]);
        for wrong in [vec![1.0], vec![1.0, 2.0, 3.0]] {
            let got = wrong.len();
            match b.upsert(2, &wrong) {
                Err(VectorError::DimensionMismatch { expected, got: g }) => {
                    assert_eq!((expected, g), (2, got));
                }
                other => panic!("expected a dimension mismatch for {got} dims, got {other:?}"),
            }
        }
        assert_eq!(b.len(), 1, "a refused upsert must not be stored");
    }

    /// The tie-break has to survive the top-k selection, not just a full sort: `k` smaller
    /// than the number of equidistant candidates is what decides which of them is dropped.
    #[test]
    fn equal_distances_rank_by_node_id_when_k_truncates_the_tie() {
        let entries: Vec<(NodeId, Vec<f32>)> =
            [9u64, 4, 7, 1, 6].iter().map(|n| (*n, vec![1.0])).collect();
        let b = backend(VectorMetric::L2, &entries);
        for k in 1..=entries.len() {
            let hits = b.search(&[0.0], k).unwrap();
            let expected: Vec<NodeId> = vec![1, 4, 6, 7, 9].into_iter().take(k).collect();
            assert_eq!(
                hits.iter().map(|h| h.node).collect::<Vec<_>>(),
                expected,
                "k = {k}"
            );
        }
    }

    /// A NaN distance makes `partial_cmp(..).unwrap_or(Equal)` non-transitive, which the
    /// sort detects and panics on. A stored NaN is rejected at the boundary now, so this
    /// reaches the comparator the only way still open to it: a Cosine norm that overflows
    /// to infinity, leaving `inf / inf`.
    #[test]
    fn a_non_finite_distance_does_not_panic_the_ranking() {
        let entries: Vec<(NodeId, Vec<f32>)> = (0u64..8)
            .map(|i| (i, vec![if i % 3 == 0 { 1e30 } else { i as f32 + 1.0 }]))
            .collect();
        let b = backend(VectorMetric::Cosine, &entries);
        let hits = b.search(&[1e30], 4).unwrap();
        assert_eq!(hits.len(), 4);
    }

    /// Removing from the middle moves the last entry into the vacated slot, so the
    /// slot map has to be repaired for the entry that moved. If it is not, the map
    /// points at another node's vector and a search answers with it silently, with no
    /// length change and no error. This interleaves removals with re-upserts and then
    /// asks, per surviving node, whether its own vector comes back.
    #[test]
    fn removals_from_the_middle_keep_every_node_pointing_at_its_own_vector() {
        // Node `i` gets the one-dimensional vector `[i]`, so a search at `[i]` must
        // return `i` and a distance of zero.
        let entries: Vec<(NodeId, Vec<f32>)> = (0u64..12).map(|i| (i, vec![i as f32])).collect();
        let mut b = backend(VectorMetric::L2, &entries);

        // Remove from the middle, from the front, and the (then) last entry, so the
        // swap-into-the-hole case fires both ways round.
        for node in [5u64, 0, 11, 6] {
            b.remove(node).unwrap();
        }
        // Re-add two of them, which appends into slots the removals shuffled.
        b.upsert(5, &[5.0]).unwrap();
        b.upsert(11, &[11.0]).unwrap();
        // Overwrite one that has been moved by a swap, to catch a stale slot on upsert.
        b.upsert(9, &[9.0]).unwrap();

        let expected: Vec<u64> = vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11];
        assert_eq!(b.len(), expected.len());
        for node in expected {
            let hits = b.search(&[node as f32], 1).unwrap();
            assert_eq!(hits[0].node, node, "node {node} resolved to another vector");
            assert!(
                hits[0].distance.abs() < 1e-6,
                "node {node} holds the wrong vector: {:?}",
                hits[0]
            );
        }
        // A removed node must be gone rather than aliased to a survivor.
        for gone in [0u64, 6] {
            assert!(
                b.search(&[gone as f32], 12)
                    .unwrap()
                    .iter()
                    .all(|h| h.node != gone),
                "removed node {gone} still present"
            );
        }
    }

    /// Removing a node that was never added is a no-op, not a panic: `remove` reads a
    /// slot out of the map and would index the vector list with whatever it found.
    #[test]
    fn removing_an_absent_node_is_a_no_op() {
        let mut b = backend(VectorMetric::L2, &[(1, vec![1.0])]);
        b.remove(42).unwrap();
        assert_eq!(b.len(), 1);
        b.remove(1).unwrap();
        b.remove(1).unwrap();
        assert_eq!(b.len(), 0);
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

    /// The two backends must report the same distance convention, and `vector_search_with`
    /// is why: a rescored hit whose stored bytes are missing keeps its backend-reported
    /// distance while its neighbors get `exact_distance` values, and the two are then sorted
    /// into one list. If usearch ever reported cosine similarity where `exact_distance`
    /// reports `1 - cos`, or plain L2 where it reports the square, that mixed list would
    /// sort wrongly with nothing to catch it. This mirrors the assertions above against the
    /// backend actually built when the feature is on.
    #[cfg(feature = "hnsw")]
    #[test]
    fn the_hnsw_backend_reports_the_same_convention_as_exact_distance() {
        fn hnsw(metric: VectorMetric, node: NodeId, v: &[f32]) -> f32 {
            let mut b = super::hnsw::HnswBackend::new(v.len(), &opts(metric))
                .expect("the hnsw backend builds");
            b.upsert(node, v).expect("upsert");
            b.search(v, 1).expect("search")[0].distance
        }
        for (metric, stored) in [
            (VectorMetric::Cosine, vec![1.0f32, 0.0]),
            (VectorMetric::Dot, vec![2.0f32, 0.0]),
            (VectorMetric::L2, vec![3.0f32, 4.0]),
        ] {
            // Queried at the stored vector itself, so both sides compute over identical
            // inputs and any difference is the convention rather than approximation.
            let approximate = hnsw(metric, 1, &stored);
            let exact = crate::index::exact_distance(&stored, &stored, metric);
            assert!(
                (approximate - exact).abs() < 1e-5,
                "{metric:?}: hnsw reported {approximate}, exact_distance {exact}"
            );
        }
    }

    /// Measurement, not an assertion: rebuild and query cost of the exact backend.
    ///
    /// Run with
    /// `cargo test --release -p issundb-vector --lib exact_backend_cost -- --ignored --nocapture`.
    ///
    /// It exists to answer the one question that decides whether this backend needs
    /// replacing with an approximate index on a target that cannot have `usearch`: the
    /// vector count at which a linear scan stops being acceptable. A query is one
    /// distance per stored vector, so the answer depends on the dimension count and the
    /// latency budget, not on anything here.
    ///
    /// The recorded figures live in `crates/issundb-vector/AGENTS.md` beside the other
    /// rebuild costs, because nothing runs this by default and numbers kept here would rot
    /// silently. Read the query figure against the dimension count it uses, four, where a
    /// real embedding is 384 or 768.
    #[test]
    #[ignore = "measurement: prints exact-backend rebuild and query timings"]
    fn exact_backend_cost() {
        const DIMS: usize = 4;
        for n in [10_000u64, 40_000, 160_000] {
            let mut b = super::exact::ExactBackend::new(
                DIMS,
                &VectorIndexOptions {
                    metric: VectorMetric::L2,
                    ..Default::default()
                },
            );
            let build = std::time::Instant::now();
            for i in 0..n {
                b.upsert(i, &[i as f32, 1.0, 2.0, 3.0]).unwrap();
            }
            let build_ms = build.elapsed().as_secs_f64() * 1000.0;

            let query = std::time::Instant::now();
            const QUERIES: usize = 100;
            for q in 0..QUERIES {
                let _ = b.search(&[q as f32, 1.0, 2.0, 3.0], 10).unwrap();
            }
            let per_query_us = query.elapsed().as_secs_f64() * 1_000_000.0 / QUERIES as f64;

            println!(
                "n={n:>7}  rebuild {build_ms:>8.1} ms  query {per_query_us:>9.1} us ({DIMS} dims)"
            );
        }
    }
}
