#![allow(clippy::duplicated_attributes)]

use std::sync::Arc;

use issundb_graphblas::{Context, Matrix, Reducer};

use ahash::AHashMap;

use crate::{csr::CsrSnapshot, error::Error, schema::NodeId};

/// How much of a [`MatrixSet`] a consumer needs materialized.
///
/// Three tiers rather than two, because the two extra matrices have different
/// prerequisites and exactly one consumer each. `page_rank_matrix` is derived from
/// the CSR out-degrees alone, so it needs no weights; `weight_matrix` needs a
/// per-edge weight, which is a second full scan of `edges` decoding every record
/// and its property blob. Collapsing the two into one tier made PageRank pay for
/// that scan and for a 111 MB matrix it never reads.
///
/// The tiers form a ladder, so a consumer's requirement is a `>=` test and two
/// requirements combine with `max`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MatrixTier {
    /// `adjacency` and `adjacency_t` only. What traversal, the path searches, the
    /// centralities, and the components algorithms read.
    Adjacency,
    /// Adds `page_rank_matrix`, which only `page_rank` reads. Built from the CSR
    /// row boundaries, so an unweighted snapshot is enough.
    PageRank,
    /// Adds `weight_matrix`, which only `shortest_path_dijkstra` reads. Requires a
    /// snapshot built by [`CsrSnapshot::build_weighted`].
    Weighted,
}

/// Set of materialized adjacency matrices for all edge types.
///
/// Owns the GraphBLAS context and, by [`MatrixTier`]:
/// - A combined integer adjacency matrix and its transpose, for BFS and SSSP SpMV.
/// - From [`MatrixTier::PageRank`] up, a column-stochastic float matrix for PageRank SpMV.
/// - At [`MatrixTier::Weighted`], a weighted adjacency matrix for Dijkstra.
pub struct MatrixSet {
    pub context: Arc<Context>,
    /// Combined outgoing adjacency: `A[i][j] = 1` for any edge i→j.
    pub adjacency: Matrix<i32>,
    /// Combined transpose adjacency: `A^T[i][j] = 1` if edge j→i exists.
    pub adjacency_t: Matrix<i32>,
    /// Column-stochastic matrix: `M[j][i] = 1 / out_degree(i)` for each edge i→j.
    /// `None` below [`MatrixTier::PageRank`].
    pub page_rank_matrix: Option<Matrix<f32>>,
    /// Weighted adjacency: `W[i][j] = weight` for each edge i→j. `None` below
    /// [`MatrixTier::Weighted`].
    pub weight_matrix: Option<Matrix<f64>>,
    /// The tier this set was materialized at, recorded rather than inferred from
    /// which matrices are present. Inferring it made the tier a function of one of
    /// the two `Option`s it governs, so a set with a weight matrix but no PageRank
    /// matrix would have reported `Weighted` and then failed when PageRank read it.
    tier: MatrixTier,
    pub n_nodes: usize,
    /// Dense-index → node id, mirroring the CSR snapshot the matrices were built
    /// from. Owned here so the matrix view is self-contained and can be extended
    /// incrementally (see `apply_delta`) without rebuilding the CSR arrays.
    pub dense_to_id: Vec<NodeId>,
    /// Node id → dense index, the inverse of `dense_to_id`.
    pub id_to_dense: AHashMap<NodeId, u32>,
}

impl MatrixSet {
    /// Materialize the sparse matrices of `tier` from the CSR snapshot.
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
        tier: MatrixTier,
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
        let ones = vec![1i32; nnz];
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

        let page_rank_matrix = if tier >= MatrixTier::PageRank {
            // M[col][i] = 1/out_deg(i) so that M * r gives incoming rank. Plus sums
            // the contributions of parallel edges i→j, which is what the transition
            // probability of the pair is. Note this reads only the row boundaries,
            // which is why it does not belong to the weighted tier.
            let mut pr_vals: Vec<f32> = Vec::with_capacity(nnz);
            for i in 0..n_nodes {
                let (start, end) = (csr.row_ptr[i], csr.row_ptr[i + 1]);
                let out_deg = (end - start) as f32;
                for _ in start..end {
                    pr_vals.push(1.0f32 / out_deg);
                }
            }
            let m = Matrix::<f32>::from_arrays(
                context.clone(),
                n_nodes,
                n_nodes,
                &cols,
                &rows,
                &pr_vals,
                Reducer::Plus,
            )
            .map_err(gb)?;
            Some(m)
        } else {
            None
        };

        let weight_matrix = if tier >= MatrixTier::Weighted {
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
            tier,
            n_nodes,
            dense_to_id: csr.dense_to_id.clone(),
            id_to_dense: csr.id_to_dense.clone(),
        })
    }

    /// The tier this set was materialized at.
    pub fn tier(&self) -> MatrixTier {
        self.tier
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
        let ms_default = MatrixSet::materialize(&csr, MatrixTier::Adjacency, 0).unwrap();
        assert_eq!(ms_default.n_nodes, 0);

        // Test explicit override via environment variable
        unsafe {
            std::env::set_var("ISSUNDB_NUM_THREADS", "2");
        }
        let ms_override = MatrixSet::materialize(&csr, MatrixTier::Adjacency, 0).unwrap();
        unsafe {
            std::env::remove_var("ISSUNDB_NUM_THREADS");
        }
        assert_eq!(ms_override.n_nodes, 0);

        // Test explicit override via programmatic parameter (higher precedence)
        unsafe {
            std::env::set_var("ISSUNDB_NUM_THREADS", "2");
        }
        let ms_prog = MatrixSet::materialize(&csr, MatrixTier::Adjacency, 4).unwrap();
        unsafe {
            std::env::remove_var("ISSUNDB_NUM_THREADS");
        }
        assert_eq!(ms_prog.n_nodes, 0);
    }

    /// Each tier must build exactly its own matrices. What a tier declines to build
    /// is the whole saving, so a set that quietly carried all four would be
    /// indistinguishable from a correct one except in memory.
    ///
    /// The PageRank tier is the case worth pinning: it must build its matrix from an
    /// *unweighted* snapshot, because that matrix is derived from the row boundaries
    /// alone. If it ever required weights again, PageRank would be paying for a
    /// second full scan of `edges` and for a weight matrix it never reads.
    #[test]
    fn each_tier_builds_exactly_its_own_matrices() {
        let dir = tempfile::TempDir::new().unwrap();
        let g = crate::Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        g.add_edge(a, b, "t", &()).unwrap();

        let plain = CsrSnapshot::build(&g.storage).unwrap();
        let adjacency = MatrixSet::materialize(&plain, MatrixTier::Adjacency, 1).unwrap();
        assert_eq!(adjacency.tier(), MatrixTier::Adjacency);
        assert!(adjacency.page_rank_matrix.is_none());
        assert!(adjacency.weight_matrix.is_none());

        let pr = MatrixSet::materialize(&plain, MatrixTier::PageRank, 1).unwrap();
        assert_eq!(pr.tier(), MatrixTier::PageRank);
        assert_eq!(
            pr.page_rank_matrix.as_ref().unwrap().nvals().unwrap(),
            1,
            "the PageRank matrix needs no weights"
        );
        assert!(
            pr.weight_matrix.is_none(),
            "the PageRank tier must not build the weight matrix"
        );

        let weighted_snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let weighted = MatrixSet::materialize(&weighted_snap, MatrixTier::Weighted, 1).unwrap();
        assert_eq!(weighted.tier(), MatrixTier::Weighted);
        assert!(weighted.page_rank_matrix.is_some());
        assert_eq!(weighted.weight_matrix.unwrap().nvals().unwrap(), 1);
    }

    /// One graph, both ends of the ladder: the adjacency tier must build the two
    /// boolean matrices and nothing else, and the weighted tier must add the other
    /// two.
    #[test]
    fn adjacency_tier_builds_two_matrices_and_weighted_builds_four() {
        let dir = tempfile::TempDir::new().unwrap();
        let g = crate::Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        g.add_edge(a, b, "t", &()).unwrap();

        let plain = CsrSnapshot::build(&g.storage).unwrap();
        let adjacency = MatrixSet::materialize(&plain, MatrixTier::Adjacency, 1).unwrap();
        assert_eq!(adjacency.tier(), MatrixTier::Adjacency);
        assert!(adjacency.page_rank_matrix.is_none());
        assert!(adjacency.weight_matrix.is_none());
        assert_eq!(adjacency.adjacency.nvals().unwrap(), 1);
        assert_eq!(adjacency.adjacency_t.nvals().unwrap(), 1);

        let weighted_snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let weighted = MatrixSet::materialize(&weighted_snap, MatrixTier::Weighted, 1).unwrap();
        assert_eq!(weighted.tier(), MatrixTier::Weighted);
        assert_eq!(weighted.page_rank_matrix.unwrap().nvals().unwrap(), 1);
        assert_eq!(weighted.weight_matrix.unwrap().nvals().unwrap(), 1);
    }

    /// The weighted tier needs weights, so asking for it with a snapshot that
    /// carries none is a gating mistake and must be reported rather than silently
    /// producing a matrix of default weights, which would make every path cost 1.
    #[test]
    fn weighted_tier_rejects_a_snapshot_without_weights() {
        let csr = CsrSnapshot::empty();
        let Err(err) = MatrixSet::materialize(&csr, MatrixTier::Weighted, 1) else {
            panic!("no weights, no weighted tier");
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
        let m = MatrixSet::materialize(&snap, MatrixTier::Weighted, 1).unwrap();
        let weights = m.weight_matrix.unwrap().triples().unwrap();
        assert_eq!(
            weights.len(),
            1,
            "one coordinate for the three parallel edges"
        );
        assert_eq!(weights[0].2, 2.5);
    }
}
