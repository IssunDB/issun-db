use super::*;

/// Largest source-set size for which typed expansion over a stale snapshot
/// stays on per-source LMDB point reads instead of paying the O(nodes + edges)
/// snapshot refresh. Point reads are a few microseconds each, so below this
/// size they are cheaper than any refresh; above it the refreshed CSR wins and
/// also amortizes over subsequent expansions.
pub(crate) const STALE_POINT_EXPAND_MAX: usize = 64;

/// Hop level standing for a dense index the search has not reached.
pub(super) const UNREACHED: u32 = u32::MAX;

/// Advance one level of a breadth-first search over the outgoing rows.
///
/// Marks every unreached neighbor of `frontier` at `hop` in `levels` and leaves
/// them in `next`, in ascending dense order. The ordering is not incidental: it is
/// what the readouts below and the `max_nodes` cap observe, so the nodes a capped
/// search keeps are the lowest-numbered ones rather than whichever the traversal
/// happened to reach first.
///
/// Taking the whole visited set as input, as an SpMV formulation does, finds the
/// same nodes: every unvisited neighbor of an already-visited node was itself
/// discovered when that node's level was expanded, so the unvisited neighbors of
/// the visited set are exactly the unvisited neighbors of the last frontier.
pub(super) fn expand_frontier(
    snap: &CsrSnapshot,
    frontier: &[u32],
    hop: u32,
    levels: &mut [u32],
    next: &mut Vec<u32>,
) {
    next.clear();
    for &u in frontier {
        for k in snap.row_ptr[u as usize]..snap.row_ptr[u as usize + 1] {
            let v = snap.col_idx[k];
            if levels[v as usize] == UNREACHED {
                levels[v as usize] = hop;
                next.push(v);
            }
        }
    }
    next.sort_unstable();
}

/// The node ids a search reached, in ascending dense (so ascending id) order.
fn reached_in_order(snap: &CsrSnapshot, levels: &[u32]) -> Vec<NodeId> {
    levels
        .iter()
        .enumerate()
        .filter(|(_, level)| **level != UNREACHED)
        .map(|(dense, _)| snap.dense_to_id[dense])
        .collect()
}

impl Graph {
    /// Breadth-first search outward from `start`, up to `hops` levels.
    ///
    /// Returns every node within `hops` outgoing steps, `start` included, in
    /// ascending node id order. An unknown `start` yields no nodes rather than an
    /// error, since a traversal from a node that is not there has reached nothing.
    pub fn bfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        self.with_snapshot(|snap| {
            let Some(&start_dense) = snap.id_to_dense.get(&start) else {
                return Ok(vec![]);
            };
            let mut levels = vec![UNREACHED; snap.dense_to_id.len()];
            levels[start_dense as usize] = 0;
            let mut frontier = vec![start_dense];
            let mut next = Vec::new();
            for hop in 1..=u32::from(hops) {
                expand_frontier(snap, &frontier, hop, &mut levels, &mut next);
                if next.is_empty() {
                    break;
                }
                std::mem::swap(&mut frontier, &mut next);
            }
            Ok(reached_in_order(snap, &levels))
        })
    }

    /// Breadth-first search outward from several seeds at once.
    ///
    /// The `max_nodes` cap applies both while seeding and while expanding, so the
    /// returned slice never exceeds it. The second element of the pair is true when
    /// the cap cut off seeds or frontier nodes that were still reachable, so a
    /// capped result is distinguishable from an exhaustively explored one.
    /// Exhausting `hops` with frontier remaining is the requested depth, not
    /// truncation. Duplicate seeds count once, so both the cap and the flag reflect
    /// distinct nodes.
    pub fn bfs_multi_source(
        &self,
        seeds: &[NodeId],
        hops: u8,
        max_nodes: Option<usize>,
    ) -> Result<(Vec<NodeId>, bool), Error> {
        self.with_snapshot(|snap| {
            let n = snap.dense_to_id.len();
            if seeds.is_empty() || n == 0 {
                return Ok((vec![], false));
            }

            let mut levels = vec![UNREACHED; n];
            let mut frontier: Vec<u32> = Vec::new();
            let mut truncated = false;
            let mut visited = 0usize;
            for &seed in seeds {
                let Some(&dense) = snap.id_to_dense.get(&seed) else {
                    continue;
                };
                if levels[dense as usize] != UNREACHED {
                    continue;
                }
                if max_nodes.is_some_and(|max| visited >= max) {
                    truncated = true;
                    break;
                }
                levels[dense as usize] = 0;
                frontier.push(dense);
                visited += 1;
            }
            if visited == 0 {
                return Ok((vec![], false));
            }
            frontier.sort_unstable();

            let mut next = Vec::new();
            for hop in 1..=u32::from(hops) {
                expand_frontier(snap, &frontier, hop, &mut levels, &mut next);
                if next.is_empty() {
                    break;
                }
                if let Some(max) = max_nodes {
                    // Already at the cap: this frontier is reachable but cannot be
                    // reported, so unmark it and say so.
                    if visited >= max {
                        for &v in &next {
                            levels[v as usize] = UNREACHED;
                        }
                        truncated = true;
                        break;
                    }
                    // Partially admissible: keep the lowest-numbered nodes up to the
                    // cap and unmark the rest, which `expand_frontier` has already
                    // marked.
                    if visited + next.len() > max {
                        truncated = true;
                        let allowed = max - visited;
                        for &v in &next[allowed..] {
                            levels[v as usize] = UNREACHED;
                        }
                        next.truncate(allowed);
                        break;
                    }
                }
                visited += next.len();
                std::mem::swap(&mut frontier, &mut next);
            }

            Ok((reached_in_order(snap, &levels), truncated))
        })
    }

    /// Expand relationships for a set of source nodes in bulk.
    ///
    /// Returns a list of `(src_node_id, edge_id, dst_node_id)` triples, per source
    /// in input order and, within one source, in ascending edge id order.
    pub fn expand_bulk(
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
    fn typed_expand_sees_writes_without_a_snapshot_refresh() {
        // Regression: the small-source branch reads LMDB directly, so it must not
        // be short-circuited by the empty placeholder snapshot a graph opens with.
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        let e = g.add_edge(a, b, "knows", &()).unwrap();

        let out = g.expand_bulk(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e, b)]);

        let incoming = g.expand_bulk(&[b], Some("knows"), true).unwrap();
        assert_eq!(incoming, vec![(b, e, a)]);
    }

    #[test]
    fn typed_expand_unknown_type_is_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        g.add_edge(a, b, "knows", &()).unwrap();

        let out = g.expand_bulk(&[a], Some("likes"), false).unwrap();
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
        let out = g.expand_bulk(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e_ab, b), (a, e_ab2, b), (a, e_aa, a)]);

        // The incoming direction reads the transposed arrays.
        let incoming = g.expand_bulk(&[b], Some("knows"), true).unwrap();
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

        let out = g.expand_bulk(&nodes, Some("knows"), false).unwrap();
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
        let out = g.expand_bulk(&[a], Some("knows"), false).unwrap();
        assert_eq!(out, vec![(a, e, b)]);
        assert!(g.csr_cache.snapshot_is_stale());
    }

    #[test]
    fn a_traversal_after_a_bulk_expansion_refresh_sees_every_write() {
        let (_dir, g) = open_tmp();
        let mut nodes = Vec::new();
        for _ in 0..66 {
            nodes.push(g.add_node("person", &()).unwrap());
        }
        for w in nodes.windows(2) {
            g.add_edge(w[0], w[1], "knows", &()).unwrap();
        }

        // The bulk expansion refreshes the snapshot, and every other kernel reads
        // that same snapshot behind the same gate, so a traversal issued next needs
        // no second refresh to see these writes.
        g.expand_bulk(&nodes, Some("knows"), false).unwrap();
        assert!(!g.csr_cache.snapshot_is_stale());

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

        let out = g.expand_bulk(&[a], None, false).unwrap();
        assert_eq!(out, vec![(a, e_ab, b), (a, e_ab2, b), (a, e_likes, b)]);
    }

    #[test]
    fn bfs_unknown_start_is_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("person", &()).unwrap();
        let b = g.add_node("person", &()).unwrap();
        g.add_edge(a, b, "knows", &()).unwrap();
        g.rebuild_csr().unwrap();

        let out = g.bfs(999_999, 2).unwrap();
        assert!(out.is_empty());
    }

    /// A capped multi-source search keeps the lowest-numbered frontier nodes and
    /// reports the truncation, so a caller can tell a capped subgraph from a
    /// complete one. Node ids ascend with insertion order, so the hub's leaves are
    /// admitted in the order they were added.
    #[test]
    fn multi_source_bfs_caps_the_frontier_and_reports_truncation() {
        let (_dir, g) = open_tmp();
        let hub = g.add_node("N", &()).unwrap();
        let leaves: Vec<_> = (0..4).map(|_| g.add_node("N", &()).unwrap()).collect();
        for &leaf in &leaves {
            g.add_edge(hub, leaf, "E", &()).unwrap();
        }
        g.rebuild_csr().unwrap();

        let (nodes, truncated) = g.bfs_multi_source(&[hub], 1, Some(3)).unwrap();
        assert_eq!(nodes, vec![hub, leaves[0], leaves[1]]);
        assert!(truncated, "two reachable leaves were cut off");

        // Room for everything: no truncation, and exhausting `hops` with frontier
        // still ahead is the requested depth rather than a cut-off.
        let (all, truncated) = g.bfs_multi_source(&[hub], 1, Some(5)).unwrap();
        assert_eq!(all.len(), 5);
        assert!(!truncated);
    }

    /// Duplicate seeds must count once, or the cap and the flag would describe a
    /// node set larger than the one returned.
    #[test]
    fn multi_source_bfs_counts_a_duplicate_seed_once() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let (nodes, truncated) = g.bfs_multi_source(&[a, a, a], 1, Some(2)).unwrap();
        assert_eq!(nodes, vec![a, b]);
        assert!(!truncated);
    }
}
