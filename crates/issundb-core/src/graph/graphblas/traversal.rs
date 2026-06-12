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
        use issundb_graphblas::{Descriptor, Monoid, Semiring, Vector, ewise_add, mxv};

        self.ensure_matrix_view()?;
        let guard = self.matrices.read();
        let m = guard
            .as_ref()
            .ok_or(Error::Corrupt("matrices not initialized"))?;
        let n = m.n_nodes;
        if n == 0 {
            return Ok(vec![]);
        }
        let start_dense = match m.id_to_dense.get(&start) {
            Some(&d) => d as usize,
            None => return Ok(vec![]),
        };

        // level[d] = BFS hop count to dense node d; absent = not yet reached.
        let mut level = Vector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        level
            .set(start_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = Descriptor::new(false, true, true, true);

        for _ in 0..hops {
            let mut next = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut next,
                Some(&level),
                Semiring::MinPlus,
                &m.adjacency,
                &level,
                opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next.nvals().map_err(|e| Error::GraphBLAS(e.to_string()))? == 0 {
                break;
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add(
                &mut merged,
                None,
                Monoid::Plus,
                &level,
                &next,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .indices()
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
        use issundb_graphblas::{Descriptor, Monoid, Semiring, Vector, ewise_add, mxv};

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
        let mut level = Vector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // Seed the level vector.
        let mut seeds_added: usize = 0;
        for &start in seeds {
            if max_nodes.is_some_and(|max| seeds_added >= max) {
                break;
            }
            if let Some(&d) = m.id_to_dense.get(&start) {
                level
                    .set(d as usize, 0)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                seeds_added += 1;
            }
        }

        if seeds_added == 0 {
            return Ok(vec![]);
        }

        // Transpose A so product[j] = min_i(A[i][j] + level[i]) = min incoming hop + 1.
        // Structural complement mask restricts writes to unvisited nodes only.
        let opts_next = Descriptor::new(false, true, true, true);

        let mut current_hop = 0;
        for _ in 0..hops {
            current_hop += 1;
            let mut next = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut next,
                Some(&level),
                Semiring::MinPlus,
                &m.adjacency,
                &level,
                opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let next_count = next.nvals().map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next_count == 0 {
                break;
            }

            let current_count = level.nvals().map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if let Some(max) = max_nodes {
                if current_count >= max {
                    break;
                }
                if current_count + next_count > max {
                    let allowed = max - current_count;
                    let next_indices: Vec<usize> = next
                        .indices()
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    for &idx in next_indices.iter().take(allowed) {
                        level
                            .set(idx, current_hop)
                            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    }
                    break;
                }
            }

            // Union next into level (disjoint due to complement mask).
            let mut merged = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add(
                &mut merged,
                None,
                Monoid::Plus,
                &level,
                &next,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            level = merged;
        }

        let dense_indices: Vec<usize> = level
            .indices()
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
        let type_id = if let Some(t) = rel_type {
            let rtxn = self.storage.env.read_txn()?;
            match get_type(&self.storage, &rtxn, t)? {
                Some(id) => Some(id),
                None => return Ok(vec![]),
            }
        } else {
            None
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
                    if let Some(tid) = type_id {
                        if ne.edge_type == tid {
                            results.push((src, ne.edge, ne.node));
                        }
                    } else {
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
                if let Some(tid) = type_id {
                    if edge_type[k] == tid {
                        results.push((src, edge_id[k], snap.dense_to_id[col_idx[k] as usize]));
                    }
                } else {
                    results.push((src, edge_id[k], snap.dense_to_id[col_idx[k] as usize]));
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

    #[test]
    fn untyped_expand_preserves_parallel_edges_and_multiple_types() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        let e_ab = g.add_edge(a, b, "knows", &()).unwrap();
        let e_ab2 = g.add_edge(a, b, "knows", &()).unwrap();
        let e_likes = g.add_edge(a, b, "likes", &()).unwrap();
        g.rebuild_csr().unwrap();

        let out = g.expand_spmv_graphblas(&[a], None, false).unwrap();
        assert_eq!(out, vec![(a, e_ab, b), (a, e_ab2, b), (a, e_likes, b)]);
    }

    #[test]
    fn bfs_graphblas_unknown_start_is_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        g.add_edge(a, b, "knows", &()).unwrap();
        g.rebuild_csr().unwrap();

        let out = g.bfs_graphblas(999_999, 2).unwrap();
        assert!(out.is_empty());
    }
}
