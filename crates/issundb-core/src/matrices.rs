#![allow(clippy::duplicated_attributes)]

use std::sync::Arc;

use issundb_graphblas::{Context, Matrix, Reducer};

use ahash::AHashMap;

use crate::{csr::CsrSnapshot, error::Error, schema::NodeId};

/// Which of a [`MatrixSet`]'s optional matrices a consumer needs materialized.
///
/// A set rather than a ladder, because the two optional matrices are independent:
/// each has exactly one consumer, and neither implies the other. `page_rank_matrix`
/// is derived from the CSR row boundaries and is read only by `page_rank`;
/// `weight_matrix` needs a per-edge weight, which costs a second full scan of
/// `edges` decoding every record and its property blob, and is read only by
/// `shortest_path_dijkstra`. An ordering would make each of them imply the cheaper
/// one, so asking for either would build both, which is the coupling this exists to
/// avoid: it cost PageRank a scan it does not need, and Dijkstra a 167 MB matrix it
/// never reads.
///
/// The two boolean adjacency matrices are not optional and so are not represented
/// here; [`MatrixKinds::ADJACENCY`] is the empty set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MatrixKinds {
    page_rank: bool,
    weight: bool,
}

impl MatrixKinds {
    /// `adjacency` and `adjacency_t` only. What traversal, the path searches, the
    /// centralities, and the components algorithms read.
    pub const ADJACENCY: Self = Self {
        page_rank: false,
        weight: false,
    };
    /// Adds `page_rank_matrix`. An unweighted snapshot serves it.
    pub const PAGE_RANK: Self = Self {
        page_rank: true,
        weight: false,
    };
    /// Adds `weight_matrix`. Requires a snapshot built by
    /// [`CsrSnapshot::build_weighted`].
    pub const WEIGHTED: Self = Self {
        page_rank: false,
        weight: true,
    };

    /// Whether this set covers everything `needed` asks for.
    pub fn contains(self, needed: Self) -> bool {
        (self.page_rank || !needed.page_rank) && (self.weight || !needed.weight)
    }

    /// The smallest set covering both, used to keep a rebuild from stripping a
    /// matrix some other consumer already asked for.
    pub fn union(self, other: Self) -> Self {
        Self {
            page_rank: self.page_rank || other.page_rank,
            weight: self.weight || other.weight,
        }
    }

    /// Whether a snapshot for this set must carry per-edge weights.
    pub fn needs_weights(self) -> bool {
        self.weight
    }
}

/// Set of materialized adjacency matrices for all edge types.
///
/// Owns the GraphBLAS context and, by [`MatrixKinds`]:
/// - A combined integer adjacency matrix and its transpose, for BFS and SSSP SpMV.
/// - With [`MatrixKinds::PAGE_RANK`], a column-stochastic float matrix for PageRank SpMV.
/// - With [`MatrixKinds::WEIGHTED`], a weighted adjacency matrix for Dijkstra.
pub struct MatrixSet {
    pub context: Arc<Context>,
    /// Combined outgoing adjacency: `A[i][j] = 1` for any edge i→j.
    pub adjacency: Matrix<i32>,
    /// Combined transpose adjacency: `A^T[i][j] = 1` if edge j→i exists.
    pub adjacency_t: Matrix<i32>,
    /// Column-stochastic matrix: `M[j][i] = 1 / out_degree(i)` for each edge i→j.
    /// `None` without [`MatrixKinds::PAGE_RANK`].
    pub page_rank_matrix: Option<Matrix<f32>>,
    /// Weighted adjacency: `W[i][j] = weight` for each edge i→j. `None` without
    /// [`MatrixKinds::WEIGHTED`].
    pub weight_matrix: Option<Matrix<f64>>,
    /// Which optional matrices this set was materialized with, recorded rather than
    /// inferred from which are present. Inferring it made the answer a function of one
    /// of the two `Option`s it governs, so a set carrying a weight matrix but no
    /// PageRank matrix would have claimed to cover both and then failed at the read.
    kinds: MatrixKinds,
    pub n_nodes: usize,
    /// Dense-index → node id, mirroring the CSR snapshot the matrices were built
    /// from. Owned here so the matrix view is self-contained and can be extended
    /// incrementally (see `apply_delta`) without rebuilding the CSR arrays.
    pub dense_to_id: Vec<NodeId>,
    /// Node id → dense index, the inverse of `dense_to_id`.
    pub id_to_dense: AHashMap<NodeId, u32>,
}

impl MatrixSet {
    /// Materialize the boolean adjacency matrices plus whatever `kinds` asks for.
    ///
    /// Every matrix is built over the same coordinates, one entry per edge, so the
    /// two index arrays are built once and handed to each build with their roles
    /// swapped for the transposed ones. Only the value array differs per matrix,
    /// and each is dropped before the next is allocated. That is the whole reason
    /// for [`Matrix::from_arrays`]: staging four triple buffers and two coordinate
    /// hash maps instead, as this did, cost 2.7 GB above the finished matrices on a
    /// 13.9 M-edge graph, since `GrB_Matrix_build` wants three arrays and a triple
    /// buffer has to be split into them anyway. Do not reintroduce a per-matrix
    /// triple buffer or a deduplicating map: the duplicate handling belongs to the
    /// build's reducer.
    pub fn materialize(
        csr: &CsrSnapshot,
        kinds: MatrixKinds,
        programmatic_threads: i32,
    ) -> Result<Self, Error> {
        let context = Context::init_default().map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // One shared resolution of the thread budget, so this pool and the
        // counting kernels' scoped threads read the same knob the same way and
        // cannot oversubscribe each other. See `crate::threads`.
        let n_threads = crate::threads::resolve(programmatic_threads) as i32;
        issundb_graphblas::set_global_threads(n_threads)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let n_nodes = csr.dense_to_id.len();
        let nnz = csr.col_idx.len();

        let mut rows: Vec<u64> = Vec::with_capacity(nnz);
        let mut cols: Vec<u64> = Vec::with_capacity(nnz);
        for i in 0..n_nodes {
            for k in csr.row_ptr[i]..csr.row_ptr[i + 1] {
                rows.push(i as u64);
                cols.push(csr.col_idx[k] as u64);
            }
        }

        let gb = |e: issundb_graphblas::GraphblasError| Error::GraphBLAS(e.to_string());

        // First-wins union for the boolean adjacency matrices: parallel edges
        // between one pair collapse to a single bit, so the reducer never has to
        // combine anything meaningful.
        //
        // `ones` is the one per-edge buffer here that carries no information: four
        // bytes per edge, 56 MB on a 13.9 M-edge graph, held across both boolean
        // builds. `GxB_Matrix_build_Scalar` builds from a single scalar and would
        // remove it, at the cost of a `GrB_Scalar` wrapper this crate does not have
        // yet. Left as a known remaining cost rather than an oversight.
        let ones = vec![1i32; rows.len()];
        let adjacency = Matrix::<i32>::from_arrays(
            context.clone(),
            n_nodes,
            n_nodes,
            &rows,
            &cols,
            &ones,
            Reducer::First,
        )
        .map_err(gb)?;
        let adjacency_t = Matrix::<i32>::from_arrays(
            context.clone(),
            n_nodes,
            n_nodes,
            &cols,
            &rows,
            &ones,
            Reducer::First,
        )
        .map_err(gb)?;
        drop(ones);

        let page_rank_matrix = if kinds.contains(MatrixKinds::PAGE_RANK) {
            // M[col][i] = 1/out_deg(i) so that M * r gives incoming rank. Note this
            // reads only the row boundaries, which is why it is independent of the
            // weight matrix.
            //
            // Parallel edges i→j contribute once each, so the pair's transition
            // probability is their count over the out-degree. That count is taken here
            // rather than left to `Reducer::Plus`, because `GrB_Matrix_build` may combine
            // duplicates in any order and `f32` addition is not associative, which would
            // make the ranks depend on the thread count. One multiply per distinct pair
            // is exact and order-independent, and it emits fewer entries besides.
            let mut pr_rows: Vec<u64> = Vec::new();
            let mut pr_cols: Vec<u64> = Vec::new();
            let mut pr_vals: Vec<f32> = Vec::new();
            let mut row_targets: Vec<u32> = Vec::new();
            for i in 0..n_nodes {
                let (start, end) = (csr.row_ptr[i], csr.row_ptr[i + 1]);
                if start == end {
                    continue;
                }
                let out_deg = (end - start) as f32;
                // Sorted so parallel edges to one destination sit together; the CSR row
                // is ordered by edge id, not by destination.
                row_targets.clear();
                row_targets.extend_from_slice(&csr.col_idx[start..end]);
                row_targets.sort_unstable();
                let mut k = 0;
                while k < row_targets.len() {
                    let col = row_targets[k];
                    let mut run = 1;
                    while k + run < row_targets.len() && row_targets[k + run] == col {
                        run += 1;
                    }
                    pr_rows.push(col as u64);
                    pr_cols.push(i as u64);
                    pr_vals.push(run as f32 / out_deg);
                    k += run;
                }
            }
            let m = Matrix::<f32>::from_arrays(
                context.clone(),
                n_nodes,
                n_nodes,
                &pr_rows,
                &pr_cols,
                &pr_vals,
                Reducer::First,
            )
            .map_err(gb)?;
            Some(m)
        } else {
            None
        };

        let weight_matrix = if kinds.contains(MatrixKinds::WEIGHTED) {
            let weights = csr.edge_weight.as_deref().ok_or_else(|| {
                Error::InvalidArgument(
                    "the weighted matrix tier needs a snapshot built by \
                     CsrSnapshot::build_weighted"
                        .to_string(),
                )
            })?;
            if weights.len() != nnz {
                return Err(Error::InvalidArgument(format!(
                    "snapshot carries {} weights for {} adjacency entries",
                    weights.len(),
                    nnz
                )));
            }
            // Min over parallel edges: the weight matrix models the cheapest
            // connection, so summing them would invent a weight no real edge has,
            // inflating shortest-path costs and breaking path reconstruction (which
            // looks for a real edge matching the matrix weight).
            let m = Matrix::<f64>::from_arrays(
                context.clone(),
                n_nodes,
                n_nodes,
                &rows,
                &cols,
                weights,
                Reducer::Min,
            )
            .map_err(gb)?;
            Some(m)
        } else {
            None
        };

        Ok(Self {
            context,
            adjacency,
            adjacency_t,
            page_rank_matrix,
            weight_matrix,
            kinds,
            n_nodes,
            dense_to_id: csr.dense_to_id.clone(),
            id_to_dense: csr.id_to_dense.clone(),
        })
    }

    /// Which optional matrices this set carries.
    pub fn kinds(&self) -> MatrixKinds {
        self.kinds
    }

    /// Apply a structural delta to the cached matrices in place, instead of
    /// rebuilding them from a full LMDB scan.
    ///
    /// `added_nodes` extend the dense-index mapping: node ids are monotonic, so
    /// they append to the sorted order without shifting existing indices, and the
    /// matrices are resized to fit. `set_edges` set the adjacency bit for each
    /// `(src, dst)`; `clear_edges` drop it. Because the combined adjacency is a
    /// boolean union, the caller resolves parallel edges against LMDB so a bit is
    /// cleared only when no edge between the pair remains. Indexing is by node id;
    /// endpoints absent from the mapping are skipped.
    ///
    /// Only `adjacency` and `adjacency_t` carry edge updates; `weight_matrix` and
    /// `page_rank_matrix`, when the set carries them at all, are resized for
    /// dimensional consistency but their incremental edge maintenance is deferred,
    /// which is why a weighted consumer gates on the matrices generation rather
    /// than on the pending delta.
    pub fn apply_delta(
        &mut self,
        added_nodes: &[NodeId],
        set_edges: &[(NodeId, NodeId)],
        clear_edges: &[(NodeId, NodeId)],
    ) -> Result<(), Error> {
        let gb = |e: issundb_graphblas::GraphblasError| Error::GraphBLAS(e.to_string());

        // Extend the dense-index mapping with the new nodes. Monotonic ids append
        // in sorted order, so existing dense indices stay valid.
        for &node in added_nodes {
            if self.id_to_dense.contains_key(&node) {
                continue;
            }
            let idx = self.dense_to_id.len() as u32;
            self.dense_to_id.push(node);
            self.id_to_dense.insert(node, idx);
        }
        let new_n = self.dense_to_id.len();
        if new_n > self.n_nodes {
            self.adjacency.resize(new_n, new_n).map_err(gb)?;
            self.adjacency_t.resize(new_n, new_n).map_err(gb)?;
            if let Some(m) = self.page_rank_matrix.as_mut() {
                m.resize(new_n, new_n).map_err(gb)?;
            }
            if let Some(m) = self.weight_matrix.as_mut() {
                m.resize(new_n, new_n).map_err(gb)?;
            }
            self.n_nodes = new_n;
        }

        for &(src, dst) in set_edges {
            let (Some(&s), Some(&d)) = (self.id_to_dense.get(&src), self.id_to_dense.get(&dst))
            else {
                continue;
            };
            self.adjacency.set(s as usize, d as usize, 1).map_err(gb)?;
            self.adjacency_t
                .set(d as usize, s as usize, 1)
                .map_err(gb)?;
        }
        for &(src, dst) in clear_edges {
            let (Some(&s), Some(&d)) = (self.id_to_dense.get(&src), self.id_to_dense.get(&dst))
            else {
                continue;
            };
            self.adjacency
                .drop_element(s as usize, d as usize)
                .map_err(gb)?;
            self.adjacency_t
                .drop_element(d as usize, s as usize)
                .map_err(gb)?;
        }

        // `set` and `drop_element` are lazy in non-blocking mode: they leave
        // pending tuples and zombies that the first read would otherwise flush,
        // mutating the matrix's internal representation. `apply_delta` runs under
        // the matrices write lock, but the read-path consumers (`bfs`, untyped
        // expansion, `connected_components`, ...) only take the shared read lock
        // and then run `mxv` concurrently. Materialize now, while exclusive, so no
        // concurrent reader triggers lazy completion on a shared `&Matrix`.
        // `page_rank_matrix` and `weight_matrix` receive only a resize here (no
        // pending element ops), and their incremental edge maintenance is
        // deferred, so they are not read in this state.
        self.adjacency.wait().map_err(gb)?;
        self.adjacency_t.wait().map_err(gb)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_num_threads_env_override() {
        // Test default execution (should default to 1 thread)
        let csr = CsrSnapshot::empty();
        let ms_default = MatrixSet::materialize(&csr, MatrixKinds::ADJACENCY, 0).unwrap();
        assert_eq!(ms_default.n_nodes, 0);

        // Test explicit override via environment variable
        unsafe {
            std::env::set_var("ISSUNDB_NUM_THREADS", "2");
        }
        let ms_override = MatrixSet::materialize(&csr, MatrixKinds::ADJACENCY, 0).unwrap();
        unsafe {
            std::env::remove_var("ISSUNDB_NUM_THREADS");
        }
        assert_eq!(ms_override.n_nodes, 0);

        // Test explicit override via programmatic parameter (higher precedence)
        unsafe {
            std::env::set_var("ISSUNDB_NUM_THREADS", "2");
        }
        let ms_prog = MatrixSet::materialize(&csr, MatrixKinds::ADJACENCY, 4).unwrap();
        unsafe {
            std::env::remove_var("ISSUNDB_NUM_THREADS");
        }
        assert_eq!(ms_prog.n_nodes, 0);
    }

    /// Each request must build exactly what it asked for and nothing else. What a
    /// request declines to build is the whole saving, so a set that quietly carried all
    /// four matrices would be indistinguishable from a correct one except in memory.
    ///
    /// Both directions are pinned here, because an ordering between the two optional
    /// matrices would silently satisfy one of them. `PAGE_RANK` must build from an
    /// *unweighted* snapshot, since that matrix comes from the row boundaries alone, and
    /// `WEIGHTED` must not drag the PageRank matrix along: 167 MB on a 13.9 M-edge graph
    /// that Dijkstra never reads.
    #[test]
    fn each_request_builds_exactly_the_matrices_it_asked_for() {
        let dir = tempfile::TempDir::new().unwrap();
        let g = crate::Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        g.add_edge(a, b, "t", &()).unwrap();

        let plain = CsrSnapshot::build(&g.storage).unwrap();
        let adjacency = MatrixSet::materialize(&plain, MatrixKinds::ADJACENCY, 1).unwrap();
        assert_eq!(adjacency.kinds(), MatrixKinds::ADJACENCY);
        assert!(adjacency.page_rank_matrix.is_none());
        assert!(adjacency.weight_matrix.is_none());
        assert_eq!(adjacency.adjacency.nvals().unwrap(), 1);
        assert_eq!(adjacency.adjacency_t.nvals().unwrap(), 1);

        let pr = MatrixSet::materialize(&plain, MatrixKinds::PAGE_RANK, 1).unwrap();
        assert_eq!(pr.kinds(), MatrixKinds::PAGE_RANK);
        assert_eq!(
            pr.page_rank_matrix.as_ref().unwrap().nvals().unwrap(),
            1,
            "the PageRank matrix needs no weights"
        );
        assert!(
            pr.weight_matrix.is_none(),
            "a PageRank request must not build the weight matrix"
        );

        let weighted_snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let weighted = MatrixSet::materialize(&weighted_snap, MatrixKinds::WEIGHTED, 1).unwrap();
        assert_eq!(weighted.kinds(), MatrixKinds::WEIGHTED);
        assert!(
            weighted.page_rank_matrix.is_none(),
            "a weighted request must not build the PageRank matrix"
        );
        assert_eq!(weighted.weight_matrix.unwrap().nvals().unwrap(), 1);

        // And a consumer that wants both gets both from one materialization.
        let both = MatrixSet::materialize(
            &weighted_snap,
            MatrixKinds::PAGE_RANK.union(MatrixKinds::WEIGHTED),
            1,
        )
        .unwrap();
        assert!(both.page_rank_matrix.is_some());
        assert!(both.weight_matrix.is_some());
        assert!(both.kinds().contains(MatrixKinds::PAGE_RANK));
        assert!(both.kinds().contains(MatrixKinds::WEIGHTED));
    }

    /// The weight matrix needs weights, so asking for it with a snapshot that
    /// carries none is a gating mistake and must be reported rather than silently
    /// producing a matrix of default weights, which would make every path cost 1.
    #[test]
    fn weighted_tier_rejects_a_snapshot_without_weights() {
        let csr = CsrSnapshot::empty();
        let Err(err) = MatrixSet::materialize(&csr, MatrixKinds::WEIGHTED, 1) else {
            panic!("no weights, no weight matrix");
        };
        // `InvalidArgument`, not `Corrupt`: this is a caller passing a tier the
        // snapshot cannot serve, and `Corrupt` tells an operator to restore a backup.
        assert!(
            matches!(&err, Error::InvalidArgument(msg) if msg.contains("build_weighted")),
            "unexpected error: {err}"
        );
    }

    /// Parallel edges between one pair must collapse to the cheapest weight. The
    /// deduplication moved from a coordinate hash map into the build's reducer, so
    /// this pins that the reducer is `Min` and not `First` or `Plus`.
    #[test]
    fn parallel_edges_collapse_to_the_minimum_weight() {
        let dir = tempfile::TempDir::new().unwrap();
        let g = crate::Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        g.add_edge(a, b, "t", &serde_json::json!({"weight": 7.5}))
            .unwrap();
        g.add_edge(a, b, "t", &serde_json::json!({"weight": 2.5}))
            .unwrap();
        g.add_edge(a, b, "t", &serde_json::json!({"weight": 4.0}))
            .unwrap();

        let snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let m = MatrixSet::materialize(&snap, MatrixKinds::WEIGHTED, 1).unwrap();
        let weights = m.weight_matrix.unwrap().triples().unwrap();
        assert_eq!(
            weights.len(),
            1,
            "one coordinate for the three parallel edges"
        );
        assert_eq!(weights[0].2, 2.5);
    }
}
