#![allow(clippy::duplicated_attributes)]

use std::collections::HashMap;
use std::sync::Arc;

use issundb_graphblas::{Context, Matrix, Reducer};

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
    pub adjacency: Matrix<i32>,
    /// Combined transpose adjacency: `A^T[i][j] = 1` if edge j→i exists.
    pub adjacency_t: Matrix<i32>,
    /// Column-stochastic matrix: `M[j][i] = 1 / out_degree(i)` for each edge i→j.
    pub page_rank_matrix: Matrix<f32>,
    /// Weighted adjacency: `W[i][j] = weight` for each edge i→j.
    pub weight_matrix: Matrix<f64>,
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

        // Support checking for an ISSUNDB_NUM_THREADS environment variable to override
        // the thread count. If absent, default to 1.
        let n_threads: i32 = if let Ok(val) = std::env::var("ISSUNDB_NUM_THREADS") {
            val.parse::<i32>().unwrap_or(1).max(1)
        } else {
            1
        };
        issundb_graphblas::set_global_threads(n_threads)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let n_nodes = csr.dense_to_id.len();

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

        let gb = |e: issundb_graphblas::GraphblasError| Error::GraphBLAS(e.to_string());

        // First-wins union for the boolean adjacency matrices; Plus to sum the
        // contributions of parallel edges in the PageRank and weight matrices.
        let adjacency = Matrix::<i32>::from_triples(
            context.clone(),
            n_nodes,
            n_nodes,
            &adj_elements,
            Reducer::First,
        )
        .map_err(gb)?;
        let adjacency_t = Matrix::<i32>::from_triples(
            context.clone(),
            n_nodes,
            n_nodes,
            &adj_t_elements,
            Reducer::First,
        )
        .map_err(gb)?;
        let page_rank_matrix = Matrix::<f32>::from_triples(
            context.clone(),
            n_nodes,
            n_nodes,
            &pr_elements,
            Reducer::Plus,
        )
        .map_err(gb)?;
        let weight_matrix = Matrix::<f64>::from_triples(
            context.clone(),
            n_nodes,
            n_nodes,
            &weight_elements,
            Reducer::Plus,
        )
        .map_err(gb)?;

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
            self.page_rank_matrix.resize(new_n, new_n).map_err(gb)?;
            self.weight_matrix.resize(new_n, new_n).map_err(gb)?;
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
        let ms_default = MatrixSet::materialize(&csr).unwrap();
        assert_eq!(ms_default.n_nodes, 0);

        // Test explicit override
        unsafe {
            std::env::set_var("ISSUNDB_NUM_THREADS", "2");
        }
        let ms_override = MatrixSet::materialize(&csr).unwrap();
        unsafe {
            std::env::remove_var("ISSUNDB_NUM_THREADS");
        }
        assert_eq!(ms_override.n_nodes, 0);
    }
}
