#![allow(clippy::duplicated_attributes)]

use std::collections::HashMap;
use std::os::raw::c_int;
use std::sync::Arc;

use graphblas_sparse_linear_algebra::collections::sparse_matrix::operations::FromMatrixElementList;
use graphblas_sparse_linear_algebra::collections::sparse_matrix::{
    MatrixElementList, Size, SparseMatrix,
};
use graphblas_sparse_linear_algebra::context::Context;
use graphblas_sparse_linear_algebra::operators::binary_operator::{First, Plus};
use suitesparse_graphblas_sys::{GxB_Global_Option_set, GxB_NTHREADS};

use ahash::AHashMap;

use crate::{csr::CsrSnapshot, error::Error, schema::NodeId};

/// Set of materialized adjacency matrices for all edge types.
///
/// Owns the GraphBLAS context and:
/// - A boolean sparse matrix per edge type (for typed pattern matching).
/// - A combined integer adjacency matrix for BFS and SSSP SpMV.
/// - A column-stochastic float matrix for PageRank SpMV.
pub struct MatrixSet {
    pub context: Arc<Context>,
    /// Combined outgoing adjacency: `A[i][j] = 1` for any edge i→j.
    pub adjacency: SparseMatrix<i32>,
    /// Combined transpose adjacency: `A^T[i][j] = 1` if edge j→i exists.
    pub adjacency_t: SparseMatrix<i32>,
    /// Column-stochastic matrix: `M[j][i] = 1 / out_degree(i)` for each edge i→j.
    pub page_rank_matrix: SparseMatrix<f32>,
    /// Weighted adjacency: `W[i][j] = weight` for each edge i→j.
    pub weight_matrix: SparseMatrix<f64>,
    pub n_nodes: usize,
    /// Dense-index → node id, mirroring the CSR snapshot the matrices were built
    /// from. Owned here so the matrix view is self-contained and can be extended
    /// incrementally (see `apply_delta`) without rebuilding the CSR arrays.
    pub dense_to_id: Vec<NodeId>,
    /// Node id → dense index, the inverse of `dense_to_id`.
    pub id_to_dense: AHashMap<NodeId, u32>,
}

impl MatrixSet {
    /// Materialize all sparse matrices from the CSR snapshot.
    pub fn materialize(csr: &CsrSnapshot) -> Result<Self, Error> {
        let context = Context::init_default().map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Threshold-gate OpenMP parallelism: enable multi-threading only when the
        // graph is large enough that the per-thread overhead is amortized. Below
        // 100 000 edges the single-threaded path avoids context-switching noise.
        let n_edges = csr.col_idx.len();
        let n_threads: c_int = if n_edges > 100_000 {
            std::thread::available_parallelism()
                .map(|n| n.get() as c_int)
                .unwrap_or(1)
        } else {
            1
        };
        context
            .call_without_detailed_error_information(|| unsafe {
                GxB_Global_Option_set(GxB_NTHREADS as c_int, n_threads)
            })
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let n_nodes = csr.dense_to_id.len();
        let matrix_size = Size::from((n_nodes, n_nodes));

        let mut adj_elements: Vec<(usize, usize, i32)> = Vec::new();
        let mut adj_t_elements: Vec<(usize, usize, i32)> = Vec::new();
        let mut weight_elements: Vec<(usize, usize, f64)> = Vec::new();
        // Accumulate PageRank weights in a map so that parallel edges i→j
        // sum their contributions rather than keeping only the first.
        let mut pr_map: HashMap<(usize, usize), f32> = HashMap::new();

        for i in 0..n_nodes {
            let start = csr.row_ptr[i];
            let end = csr.row_ptr[i + 1];
            let out_deg = (end - start) as f32;
            for k in start..end {
                let col = csr.col_idx[k] as usize;
                adj_elements.push((i, col, 1i32));
                adj_t_elements.push((col, i, 1i32));
                weight_elements.push((i, col, csr.edge_weight[k]));
                if out_deg > 0.0 {
                    // M[col][i] = 1/out_deg(i) so that M * r gives incoming rank.
                    *pr_map.entry((col, i)).or_insert(0.0) += 1.0f32 / out_deg;
                }
            }
        }

        let pr_elements: Vec<(usize, usize, f32)> =
            pr_map.into_iter().map(|((r, c), v)| (r, c, v)).collect();

        let adjacency = if adj_elements.is_empty() {
            SparseMatrix::<i32>::new(context.clone(), matrix_size)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
        } else {
            let element_list = MatrixElementList::<i32>::from_element_vector(
                adj_elements.into_iter().map(|c| c.into()).collect(),
            );
            SparseMatrix::<i32>::from_element_list(
                context.clone(),
                matrix_size,
                element_list,
                &First::<i32>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?
        };

        let adjacency_t = if adj_t_elements.is_empty() {
            SparseMatrix::<i32>::new(context.clone(), matrix_size)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
        } else {
            let element_list = MatrixElementList::<i32>::from_element_vector(
                adj_t_elements.into_iter().map(|c| c.into()).collect(),
            );
            SparseMatrix::<i32>::from_element_list(
                context.clone(),
                matrix_size,
                element_list,
                &First::<i32>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?
        };

        let page_rank_matrix = if pr_elements.is_empty() {
            SparseMatrix::<f32>::new(context.clone(), matrix_size)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
        } else {
            let element_list = MatrixElementList::<f32>::from_element_vector(
                pr_elements.into_iter().map(|c| c.into()).collect(),
            );
            SparseMatrix::<f32>::from_element_list(
                context.clone(),
                matrix_size,
                element_list,
                &Plus::<f32>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?
        };

        let weight_matrix = if weight_elements.is_empty() {
            SparseMatrix::<f64>::new(context.clone(), matrix_size)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
        } else {
            let element_list = MatrixElementList::<f64>::from_element_vector(
                weight_elements.into_iter().map(|c| c.into()).collect(),
            );
            SparseMatrix::<f64>::from_element_list(
                context.clone(),
                matrix_size,
                element_list,
                &Plus::<f64>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?
        };

        Ok(Self {
            context,
            adjacency,
            adjacency_t,
            page_rank_matrix,
            weight_matrix,
            n_nodes,
            dense_to_id: csr.dense_to_id.clone(),
            id_to_dense: csr.id_to_dense.clone(),
        })
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
    /// Spike scope: only `adjacency` and `adjacency_t` carry edge updates;
    /// `weight_matrix` and `page_rank_matrix` are resized for dimensional
    /// consistency but their incremental edge maintenance is deferred.
    pub fn apply_delta(
        &mut self,
        added_nodes: &[NodeId],
        set_edges: &[(NodeId, NodeId)],
        clear_edges: &[(NodeId, NodeId)],
    ) -> Result<(), Error> {
        use graphblas_sparse_linear_algebra::collections::sparse_matrix::{
            Size,
            operations::{DropSparseMatrixElement, ResizeSparseMatrix, SetSparseMatrixElement},
        };

        let gb = |e: graphblas_sparse_linear_algebra::error::SparseLinearAlgebraError| {
            Error::GraphBLAS(e.to_string())
        };

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
            let size = Size::from((new_n, new_n));
            self.adjacency.resize(size).map_err(gb)?;
            self.adjacency_t.resize(size).map_err(gb)?;
            self.page_rank_matrix.resize(size).map_err(gb)?;
            self.weight_matrix.resize(size).map_err(gb)?;
            self.n_nodes = new_n;
        }

        for &(src, dst) in set_edges {
            let (Some(&s), Some(&d)) = (self.id_to_dense.get(&src), self.id_to_dense.get(&dst))
            else {
                continue;
            };
            self.adjacency
                .set_value(s as usize, d as usize, 1)
                .map_err(gb)?;
            self.adjacency_t
                .set_value(d as usize, s as usize, 1)
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
        Ok(())
    }
}
