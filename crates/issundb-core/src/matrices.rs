#![allow(clippy::duplicated_attributes)]

use std::collections::HashMap;
use std::sync::Arc;

use graphblas_sparse_linear_algebra::collections::sparse_matrix::operations::FromMatrixElementList;
use graphblas_sparse_linear_algebra::collections::sparse_matrix::{
    MatrixElementList, Size, SparseMatrix,
};
use graphblas_sparse_linear_algebra::context::Context;
use graphblas_sparse_linear_algebra::operators::binary_operator::{First, Plus};

use crate::{csr::CsrSnapshot, error::Error, schema::TypeId};

/// Set of materialized adjacency matrices for all edge types.
///
/// Owns the GraphBLAS context and:
/// - A boolean sparse matrix per edge type (for typed pattern matching).
/// - A combined integer adjacency matrix for BFS and SSSP SpMV.
/// - A column-stochastic float matrix for PageRank SpMV.
pub struct MatrixSet {
    pub context: Arc<Context>,
    /// Per-type boolean matrices: `A[i][j] = true` for each edge i→j of that type.
    pub by_type: HashMap<TypeId, SparseMatrix<bool>>,
    /// Combined outgoing adjacency: `A[i][j] = 1` for any edge i→j.
    pub adjacency: SparseMatrix<i32>,
    /// Combined transpose adjacency: `A^T[i][j] = 1` if edge j→i exists.
    pub adjacency_t: SparseMatrix<i32>,
    /// Column-stochastic matrix: `M[j][i] = 1 / out_degree(i)` for each edge i→j.
    pub page_rank_matrix: SparseMatrix<f32>,
    /// Weighted adjacency: `W[i][j] = weight` for each edge i→j.
    pub weight_matrix: SparseMatrix<f64>,
    pub n_nodes: usize,
}

impl MatrixSet {
    /// Materialize all sparse matrices from the CSR snapshot.
    pub fn materialize(csr: &CsrSnapshot) -> Result<Self, Error> {
        let context = Context::init_default().map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let n_nodes = csr.dense_to_id.len();
        let matrix_size = Size::from((n_nodes, n_nodes));

        let mut elements_by_type: HashMap<TypeId, Vec<(usize, usize, bool)>> = HashMap::new();
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
                let type_id = csr.edge_type[k];
                elements_by_type
                    .entry(type_id)
                    .or_default()
                    .push((i, col, true));
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

        let mut by_type = HashMap::new();
        for (type_id, coords) in elements_by_type {
            let element_list = MatrixElementList::<bool>::from_element_vector(
                coords.into_iter().map(|c| c.into()).collect(),
            );
            let matrix = SparseMatrix::<bool>::from_element_list(
                context.clone(),
                matrix_size,
                element_list,
                &First::<bool>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            by_type.insert(type_id, matrix);
        }

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
            by_type,
            adjacency,
            adjacency_t,
            page_rank_matrix,
            weight_matrix,
            n_nodes,
        })
    }
}
