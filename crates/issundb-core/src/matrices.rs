#![cfg(feature = "graphblas")]

use std::collections::HashMap;
use std::sync::Arc;

use graphblas_sparse_linear_algebra::collections::sparse_matrix::operations::FromMatrixElementList;
use graphblas_sparse_linear_algebra::collections::sparse_matrix::{
    MatrixElementList, Size, SparseMatrix,
};
use graphblas_sparse_linear_algebra::context::Context;
use graphblas_sparse_linear_algebra::operators::binary_operator::First;

use crate::{csr::CsrSnapshot, error::Error, schema::TypeId};

/// Set of materialized adjacency matrices for all edge types.
///
/// Owns the GraphBLAS context and a boolean sparse matrix for each edge type.
/// The matrices represent the outgoing adjacency of the graph.
pub struct MatrixSet {
    pub context: Arc<Context>,
    pub by_type: HashMap<TypeId, SparseMatrix<bool>>,
    pub n_nodes: usize,
}

impl MatrixSet {
    /// Materialize all sparse matrices from the CSR snapshot.
    pub fn materialize(csr: &CsrSnapshot) -> Result<Self, Error> {
        // init_default() returns Arc<Context> directly; do not wrap in Arc::new.
        let context = Context::init_default().map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let n_nodes = csr.dense_to_id.len();
        let matrix_size = Size::from((n_nodes, n_nodes));

        let mut elements_by_type: HashMap<TypeId, Vec<(usize, usize, bool)>> = HashMap::new();

        for i in 0..n_nodes {
            let start = csr.row_ptr[i];
            let end = csr.row_ptr[i + 1];
            for j in start..end {
                let col = csr.col_idx[j] as usize;
                let type_id = csr.edge_type[j];
                elements_by_type
                    .entry(type_id)
                    .or_default()
                    .push((i, col, true));
            }
        }

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

        Ok(Self {
            context,
            by_type,
            n_nodes,
        })
    }
}
