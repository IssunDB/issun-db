use super::*;

/// Largest source-set size for which typed expansion over a stale snapshot
/// stays on per-source LMDB point reads instead of paying the O(nodes + edges)
/// snapshot refresh. Point reads are a few microseconds each, so below this
/// size they are cheaper than any refresh; above it the refreshed CSR wins and
/// also amortizes over subsequent expansions.
const STALE_POINT_EXPAND_MAX: usize = 64;

impl Graph {
    /// BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// Each iteration propagates the hop-level frontier one step by computing
    /// `A^T * level` with a structural complement mask that restricts writes to
    /// nodes not yet reached. The level vector is then extended with the new frontier.
    #[doc(hidden)]
    pub fn bfs_graphblas(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.bfs(start, hops),
        };
        let n = m.n_nodes;
        if n == 0 {
            return Ok(vec![]);
        }
        let start_dense = match m.id_to_dense.get(&start) {
            Some(&d) => d as usize,
            None => return self.bfs(start, hops),
        };

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        level
            .set_value(start_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        for _ in 0..hops {
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &level,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                == 0
            {
                break;
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &level,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        Ok(dense_indices
            .into_iter()
            .filter_map(|d| m.dense_to_id.get(d).copied())
            .collect())
    }

    /// Multi-source BFS via repeated SpMV over the combined adjacency using the MinPlus semiring.
    ///
    /// The `max_nodes` cap is applied both during seed seeding and during SpMV
    /// expansion so that the returned slice never exceeds the cap.
    #[doc(hidden)]
    pub fn bfs_multi_source_graphblas(
        &self,
        seeds: &[NodeId],
        hops: u8,
        max_nodes: Option<usize>,
    ) -> Result<Vec<NodeId>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        self.ensure_matrix_view()?;

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return Ok(vec![]),
        };
        let n = m.n_nodes;
        if seeds.is_empty() || n == 0 {
            return Ok(vec![]);
        }

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Seed the level vector.
        let mut seeds_added: usize = 0;
        for &start in seeds {
            if max_nodes.is_some_and(|max| seeds_added >= max) {
                break;
            }
            if let Some(&d) = m.id_to_dense.get(&start) {
                level
                    .set_value(d as usize, 0)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                seeds_added += 1;
            }
        }

        if seeds_added == 0 {
            return Ok(vec![]);
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut current_hop = 0;
        for _ in 0..hops {
            current_hop += 1;
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &level,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let next_count = next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next_count == 0 {
                break;
            }

            let current_count = level
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if let Some(max) = max_nodes {
                if current_count >= max {
                    break;
                }
                if current_count + next_count > max {
                    let allowed = max - current_count;
                    let next_indices: Vec<usize> = next
                        .element_indices()
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    for &idx in next_indices.iter().take(allowed) {
                        level
                            .set_value(idx, current_hop)
                            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    }
                    break;
                }
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &level,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        Ok(dense_indices
            .into_iter()
            .filter_map(|d| m.dense_to_id.get(d).copied())
            .collect())
    }

    /// Expand relationships for a set of source nodes using GraphBLAS SpMV.
    ///
    /// Returns a list of `(src_node_id, edge_id, dst_node_id)` triples.
    #[doc(hidden)]
    pub fn expand_spmv_graphblas(
        &self,
        src_nodes: &[NodeId],
        rel_type: Option<&str>,
        is_incoming: bool,
    ) -> Result<Vec<(NodeId, EdgeId, NodeId)>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector,
                operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
            },
            operators::{
                binary_operator::Assignment,
                mask::SelectEntireVector,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::OptionsForOperatorWithMatrixAsFirstArgument,
                semiring::MinPlus,
            },
        };
        use std::collections::HashMap as StdHashMap;

        // The typed path reads the CSR snapshot per source row: a hash lookup
        // plus a contiguous slice scan, with edge ids and type ids carried on
        // the snapshot arrays. It must run before any matrices access (the
        // cached matrix dimension can be stale) and never needs GraphBLAS, so
        // freshness is the snapshot-only gate.
        if let Some(t) = rel_type {
            let type_id = {
                let rtxn = self.storage.env.read_txn()?;
                match get_type(&self.storage, &rtxn, t)? {
                    Some(id) => id,
                    None => return Ok(vec![]),
                }
            };

            // A stale snapshot needs an O(nodes + edges) refresh before the
            // CSR is readable; for a small source set the per-source LMDB
            // point reads (always fresh) are cheaper, so an interleaved
            // write-then-expand workload never pays a rebuild.
            if self.csr_cache.snapshot_is_stale() && src_nodes.len() <= STALE_POINT_EXPAND_MAX {
                let mut results = Vec::new();
                for &src in src_nodes {
                    let neighbors = if is_incoming {
                        self.in_neighbors(src)?
                    } else {
                        self.out_neighbors(src)?
                    };
                    for ne in neighbors {
                        if ne.edge_type == type_id {
                            results.push((src, ne.edge, ne.node));
                        }
                    }
                }
                return Ok(results);
            }

            self.ensure_snapshot_fresh()?;
            let snap = self.csr_cache.snapshot.load();
            let (row_ptr, col_idx, edge_type, edge_id) = if is_incoming {
                (
                    &snap.in_row_ptr,
                    &snap.in_col_idx,
                    &snap.in_edge_type,
                    &snap.in_edge_id,
                )
            } else {
                (&snap.row_ptr, &snap.col_idx, &snap.edge_type, &snap.edge_id)
            };
            let mut results = Vec::new();
            for &src in src_nodes {
                let d = match snap.id_to_dense.get(&src) {
                    Some(&d) => d as usize,
                    None => continue,
                };
                for k in row_ptr[d]..row_ptr[d + 1] {
                    if edge_type[k] == type_id {
                        results.push((src, edge_id[k], snap.dense_to_id[col_idx[k] as usize]));
                    }
                }
            }
            return Ok(results);
        }

        // The untyped path reaches reachability through the adjacency matrix,
        // so it needs the incremental matrix view fresh.
        self.ensure_matrix_view()?;

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => {
                // Fall back to LMDB if matrices are not yet materialized.
                let mut results = Vec::new();
                for &src in src_nodes {
                    let neighbors = if is_incoming {
                        self.in_neighbors(src)?
                    } else {
                        self.out_neighbors(src)?
                    };
                    for ne in neighbors {
                        results.push((src, ne.edge, ne.node));
                    }
                }
                return Ok(results);
            }
        };

        let n = m.n_nodes;
        if src_nodes.is_empty() || n == 0 {
            return Ok(vec![]);
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        // Propagate outgoing edges via the transposed adjacency matrix;
        // incoming edges use the original. See `bfs_multi_source_graphblas` for the derivation.
        let opts =
            OptionsForOperatorWithMatrixAsFirstArgument::new(true, false, false, !is_incoming);

        let mut results = Vec::new();

        for &src in src_nodes {
            let src_dense = match m.id_to_dense.get(&src) {
                Some(&d) => d as usize,
                None => continue,
            };

            // Build a dense-index → EdgeId lookup so the SpMV result can be paired with
            // a correct EdgeId. Both directions read from LMDB so the lookup is always
            // fresh: EdgeId 0 is the legitimate first allocated edge (alloc_edge_id starts
            // from 0), so it must never be used as a "missing" sentinel.
            let edge_lookup: StdHashMap<usize, EdgeId> = if is_incoming {
                self.in_neighbors(src)?
                    .into_iter()
                    .filter_map(|ne| m.id_to_dense.get(&ne.node).map(|&d| (d as usize, ne.edge)))
                    .collect()
            } else {
                self.out_neighbors(src)?
                    .into_iter()
                    .filter_map(|ne| m.id_to_dense.get(&ne.node).map(|&d| (d as usize, ne.edge)))
                    .collect()
            };

            let mut level = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level
                .set_value(src_dense, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            // next = adjacency * level (or adjacency^T * level) with MinPlus semiring.
            // SelectEntireVector passes all output positions through without masking so
            // that neighbors at any dense index are written into `next`.
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &level,
                &Assignment::new(),
                &mut next,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let target_indices: Vec<usize> = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            for idx in target_indices {
                if let Some(&dst) = m.dense_to_id.get(idx) {
                    // Skip entries whose EdgeId is not in the LMDB-backed lookup; this
                    // can only happen if the edge was deleted between the LMDB query and
                    // the SpMV pass, which is not possible in a single-writer model.
                    if let Some(&edge_id) = edge_lookup.get(&idx) {
                        results.push((src, edge_id, dst));
                    }
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn typed_expand_sees_writes_without_matrix_refresh() {
        // Regression: the typed branch reads LMDB directly, so it must not be
        // short-circuited by a stale (here: empty) matrix dimension before any
        // matrix view refresh has run.
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        let e = g.add_edge(a, b, "knows", &()).unwrap();

        let out = g.expand_spmv_graphblas(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e, b)]);

        let incoming = g.expand_spmv_graphblas(&[b], Some("knows"), true).unwrap();
        assert_eq!(incoming, vec![(b, e, a)]);
    }

    #[test]
    fn typed_expand_unknown_type_is_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        g.add_edge(a, b, "knows", &()).unwrap();

        let out = g.expand_spmv_graphblas(&[a], Some("likes"), false).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn typed_expand_reads_the_csr_when_fresh() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        let e_ab = g.add_edge(a, b, "knows", &()).unwrap();
        let e_ab2 = g.add_edge(a, b, "knows", &()).unwrap();
        let e_aa = g.add_edge(a, a, "knows", &()).unwrap();
        g.add_edge(a, b, "likes", &()).unwrap();
        g.rebuild_csr().unwrap();

        // Per-source results follow the snapshot's edge order (ascending edge
        // id, so the self-loop comes last), and the type filter drops the
        // `likes` edge.
        let out = g.expand_spmv_graphblas(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e_ab, b), (a, e_ab2, b), (a, e_aa, a)]);

        // The incoming direction reads the transposed arrays.
        let incoming = g.expand_spmv_graphblas(&[b], Some("knows"), true).unwrap();
        assert_eq!(incoming, vec![(b, e_ab, a), (b, e_ab2, a)]);
    }

    #[test]
    fn bulk_typed_expand_over_a_stale_snapshot_refreshes_it() {
        let (_dir, g) = open_tmp();
        let mut nodes = Vec::new();
        for _ in 0..66 {
            nodes.push(g.add_node("person", &()).unwrap());
        }
        let mut expected = Vec::new();
        for w in nodes.windows(2) {
            let e = g.add_edge(w[0], w[1], "knows", &()).unwrap();
            expected.push((w[0], e, w[1]));
        }
        assert!(g.csr_cache.snapshot_is_stale());

        let out = g
            .expand_spmv_graphblas(&nodes, Some("knows"), false)
            .unwrap();
        assert_eq!(out, expected);
        assert!(
            !g.csr_cache.snapshot_is_stale(),
            "a bulk typed expansion refreshes the snapshot"
        );
    }

    #[test]
    fn stale_point_expand_skips_the_snapshot_rebuild() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        let e = g.add_edge(a, b, "knows", &()).unwrap();
        assert!(g.csr_cache.snapshot_is_stale());

        // A small source set over a stale snapshot stays on the per-source
        // point reads, so a write-then-expand workload never pays a rebuild.
        let out = g.expand_spmv_graphblas(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e, b)]);
        assert!(g.csr_cache.snapshot_is_stale());
    }

    #[test]
    fn hybrid_consumers_stay_fresh_after_a_snapshot_only_refresh() {
        let (_dir, g) = open_tmp();
        let mut nodes = Vec::new();
        for _ in 0..66 {
            nodes.push(g.add_node("person", &()).unwrap());
        }
        for w in nodes.windows(2) {
            g.add_edge(w[0], w[1], "knows", &()).unwrap();
        }

        // The bulk expansion refreshes the snapshot without touching the
        // matrices, leaving the structural delta pending.
        g.expand_spmv_graphblas(&nodes, Some("knows"), false)
            .unwrap();
        assert!(!g.csr_cache.snapshot_is_stale());

        // A matrix-reading consumer behind `ensure_csr_fresh` must still see
        // the writes: the pending delta has to reach the matrices even though
        // the snapshot generation says fresh.
        let reached = g.dfs(nodes[0], 1).unwrap();
        assert_eq!(reached, vec![nodes[0], nodes[1]]);
    }
}
