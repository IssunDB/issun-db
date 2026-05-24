pub mod csr;
pub mod error;
pub mod graph;

#[cfg(feature = "graphblas")]
pub mod matrices;
pub mod schema;
pub mod storage;

pub use error::Error;
pub use graph::Graph;
#[cfg(feature = "graphblas")]
pub use matrices::MatrixSet;
pub use schema::{AdjEntry, EdgeId, EdgeRecord, LabelId, NodeId, NodeRecord, TypeId};

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn insert_and_read_node() {
        let (_dir, g) = open_tmp();

        let id = g
            .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
            .unwrap();
        let record = g.get_node(id).unwrap().expect("node should exist");

        // Deserialize props back and assert
        let props: serde_json::Value = rmp_serde::from_slice(&record.props).unwrap();
        assert_eq!(props["name"], "Alice");
        assert_eq!(props["age"], 30);
    }

    #[test]
    fn insert_and_read_edge() {
        let (_dir, g) = open_tmp();

        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        let eid = g
            .add_edge(alice, bob, "KNOWS", &json!({ "since": 2020 }))
            .unwrap();

        let edge = g.get_edge(eid).unwrap().expect("edge should exist");
        assert_eq!(edge.src, alice);
        assert_eq!(edge.dst, bob);

        let neighbors = g.out_neighbors(alice).unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].0, bob);
        assert_eq!(neighbors[0].1, eid);
    }

    #[test]
    fn multiple_nodes_get_unique_ids() {
        let (_dir, g) = open_tmp();
        let ids: Vec<_> = (0..10)
            .map(|i| g.add_node("Node", &json!({ "i": i })).unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 10);
    }

    #[test]
    fn nodes_by_label_returns_correct_set() {
        let (_dir, g) = open_tmp();

        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let _c = g.add_node("Company", &json!({})).unwrap();

        let mut persons = g.nodes_by_label("Person").unwrap();
        persons.sort_unstable();
        assert_eq!(persons, vec![a, b]);

        let companies = g.nodes_by_label("Company").unwrap();
        assert_eq!(companies.len(), 1);

        let missing = g.nodes_by_label("Robot").unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn edges_by_type_returns_correct_set() {
        let (_dir, g) = open_tmp();

        let alice = g.add_node("Person", &json!({})).unwrap();
        let bob = g.add_node("Person", &json!({})).unwrap();
        let corp = g.add_node("Company", &json!({})).unwrap();

        let e1 = g.add_edge(alice, bob, "KNOWS", &json!({})).unwrap();
        let e2 = g.add_edge(alice, corp, "WORKS_AT", &json!({})).unwrap();
        let e3 = g.add_edge(bob, corp, "WORKS_AT", &json!({})).unwrap();

        let knows = g.edges_by_type("KNOWS").unwrap();
        assert_eq!(knows, vec![e1]);

        let mut works = g.edges_by_type("WORKS_AT").unwrap();
        works.sort_unstable();
        assert_eq!(works, vec![e2, e3]);

        let missing = g.edges_by_type("FOLLOWS").unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn label_idx_consistent_across_reopen() {
        let dir = TempDir::new().unwrap();
        let id = {
            let g = Graph::open(dir.path(), 1).unwrap();
            g.add_node("Person", &json!({})).unwrap()
        };
        let g2 = Graph::open(dir.path(), 1).unwrap();
        let persons = g2.nodes_by_label("Person").unwrap();
        assert_eq!(persons, vec![id]);
    }

    #[test]
    fn csr_hot_path_returns_correct_neighbors() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let e1 = g.add_edge(a, b, "E", &json!({})).unwrap();
        let e2 = g.add_edge(a, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let mut out = g.out_neighbors(a).unwrap();
        out.sort_unstable_by_key(|&(n, _, _)| n);
        assert_eq!(out.len(), 2);
        let edge_ids: Vec<_> = out.iter().map(|&(_, eid, _)| eid).collect();
        assert!(edge_ids.contains(&e1));
        assert!(edge_ids.contains(&e2));
    }

    #[test]
    fn csr_fallback_to_lmdb_for_new_nodes() {
        let (_dir, g) = open_tmp();
        // Snapshot is built empty on open; nodes added after open are not in it yet.
        // rebuild_csr is NOT called here, so out_neighbors must fall back to LMDB.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let eid = g.add_edge(a, b, "E", &json!({})).unwrap();

        let out = g.out_neighbors(a).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, eid);
    }

    #[test]
    fn csr_snapshot_rebuilds_correctly_on_reopen() {
        let dir = TempDir::new().unwrap();
        let (a, b, eid) = {
            let g = Graph::open(dir.path(), 1).unwrap();
            let a = g.add_node("N", &json!({})).unwrap();
            let b = g.add_node("N", &json!({})).unwrap();
            let eid = g.add_edge(a, b, "E", &json!({})).unwrap();
            (a, b, eid)
        };
        let g2 = Graph::open(dir.path(), 1).unwrap();
        let out = g2.out_neighbors(a).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, b);
        assert_eq!(out[0].1, eid);
    }

    // ------------------------------------------------------------------
    // BFS
    // ------------------------------------------------------------------

    #[test]
    fn bfs_hops_zero_returns_start_only() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();

        let result = g.bfs(a, 0).unwrap();
        assert_eq!(result, vec![a]);
    }

    #[test]
    fn bfs_linear_chain_respects_hop_limit() {
        let (_dir, g) = open_tmp();
        // Build a → b → c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut hop1 = g.bfs(a, 1).unwrap();
        hop1.sort_unstable();
        assert_eq!(hop1, vec![a, b]);

        let mut hop2 = g.bfs(a, 2).unwrap();
        hop2.sort_unstable();
        assert_eq!(hop2, vec![a, b, c]);

        let mut hop3 = g.bfs(a, 3).unwrap();
        hop3.sort_unstable();
        assert_eq!(hop3, vec![a, b, c, d]);
    }

    #[test]
    fn bfs_does_not_revisit_nodes_in_a_cycle() {
        let (_dir, g) = open_tmp();
        // Build a → b → c → a  (cycle)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut result = g.bfs(a, 10).unwrap();
        result.sort_unstable();
        assert_eq!(result, vec![a, b, c]);
    }

    #[test]
    fn bfs_isolated_node_returns_only_itself() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let _b = g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs(a, 5).unwrap();
        assert_eq!(result, vec![a]);
    }

    #[test]
    fn bfs_works_via_lmdb_fallback_without_rebuild() {
        let (_dir, g) = open_tmp();
        // No rebuild_csr: nodes are not in the CSR snapshot.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        let mut result = g.bfs(a, 2).unwrap();
        result.sort_unstable();
        assert_eq!(result, vec![a, b, c]);
    }

    // ------------------------------------------------------------------
    // shortest_path
    // ------------------------------------------------------------------

    #[test]
    fn shortest_path_same_node() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let path = g.shortest_path(a, a).unwrap();
        assert_eq!(path, Some(vec![a]));
    }

    #[test]
    fn shortest_path_direct_edge() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let path = g.shortest_path(a, b).unwrap().unwrap();
        assert_eq!(path, vec![a, b]);
    }

    #[test]
    fn shortest_path_multi_hop() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let path = g.shortest_path(a, c).unwrap().unwrap();
        assert_eq!(path, vec![a, b, c]);
    }

    #[test]
    fn shortest_path_returns_shortest_not_any_path() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        // Two paths: a→b→c (length 2) and a→c (length 1).
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let path = g.shortest_path(a, c).unwrap().unwrap();
        assert_eq!(path.len(), 2); // [a, c]
    }

    #[test]
    fn shortest_path_unreachable_returns_none() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        assert!(g.shortest_path(a, b).unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // page_rank
    // ------------------------------------------------------------------

    #[test]
    fn page_rank_all_nodes_present() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let pr = g.page_rank(20, 0.85).unwrap();
        assert_eq!(pr.len(), 3);
        assert!(pr.contains_key(&a));
        // In a balanced cycle, all ranks converge to 1/n.
        for &score in pr.values() {
            assert!((score - 1.0 / 3.0).abs() < 1e-3, "rank = {score}");
        }
    }

    #[test]
    fn page_rank_empty_graph_returns_empty() {
        let (_dir, g) = open_tmp();
        g.rebuild_csr().unwrap();
        assert!(g.page_rank(10, 0.85).unwrap().is_empty());
    }

    // ------------------------------------------------------------------
    // connected_components
    // ------------------------------------------------------------------

    #[test]
    fn connected_components_single_component() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        let cc = g.connected_components().unwrap();
        assert_eq!(cc.len(), 3);
        assert_eq!(cc[&a], cc[&b]);
        assert_eq!(cc[&b], cc[&c]);
    }

    #[test]
    fn connected_components_two_components() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();

        let cc = g.connected_components().unwrap();
        assert_eq!(cc.len(), 4);
        assert_eq!(cc[&a], cc[&b]);
        assert_eq!(cc[&c], cc[&d]);
        assert_ne!(cc[&a], cc[&c]);
    }

    #[test]
    fn connected_components_weakly_connected_via_reverse_edge() {
        let (_dir, g) = open_tmp();
        // a → b and b → c: all three are weakly connected.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        // Only edge from a to b; c is reachable from b, not from a → c.
        g.add_edge(c, b, "E", &json!({})).unwrap();

        let cc = g.connected_components().unwrap();
        assert_eq!(cc[&a], cc[&b]);
        assert_eq!(cc[&b], cc[&c]);
    }

    #[test]
    fn label_name_roundtrip() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({})).unwrap();
        let rec = g.get_node(id).unwrap().unwrap();
        assert_eq!(g.label_name(rec.label).unwrap().as_deref(), Some("Person"));
    }

    #[test]
    fn type_name_roundtrip() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let eid = g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        let rec = g.get_edge(eid).unwrap().unwrap();
        assert_eq!(
            g.type_name(rec.edge_type).unwrap().as_deref(),
            Some("KNOWS")
        );
    }

    #[test]
    fn label_name_unknown_id_returns_none() {
        let (_dir, g) = open_tmp();
        assert!(g.label_name(9999).unwrap().is_none());
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_bfs_page_rank_sssp() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        // BFS from a with 1 hop should reach a and b only.
        let bfs1 = g.bfs_graphblas(a, 1).unwrap();
        assert!(bfs1.contains(&a));
        assert!(bfs1.contains(&b));
        assert!(!bfs1.contains(&c));

        // BFS from a with 2 hops should reach all three.
        let bfs2 = g.bfs_graphblas(a, 2).unwrap();
        assert!(bfs2.contains(&a));
        assert!(bfs2.contains(&b));
        assert!(bfs2.contains(&c));

        // PageRank returns one entry per node.
        let pr = g.page_rank_graphblas(10, 0.85).unwrap();
        assert_eq!(pr.len(), 3);
        for &id in &[a, b, c] {
            assert!(pr.contains_key(&id));
            assert!(*pr.get(&id).unwrap() > 0.0);
        }

        // SSSP a→c should return the two-hop path [a, b, c].
        let path = g
            .shortest_path_graphblas(a, c)
            .unwrap()
            .expect("path a→c must exist");
        assert_eq!(path, vec![a, b, c]);

        // SSSP a→a is a trivial path.
        let trivial = g.shortest_path_graphblas(a, a).unwrap().unwrap();
        assert_eq!(trivial, vec![a]);

        // SSSP in reverse direction (no edge) returns None.
        assert!(g.shortest_path_graphblas(c, a).unwrap().is_none());
    }
}

#[cfg(test)]
mod prop_tests {
    use proptest::prelude::*;
    use tempfile::TempDir;

    use super::*;

    // Each test opens one LMDB environment for all iterations so that the
    // process does not exhaust lock-file descriptors across hundreds of
    // open/close cycles.  The graph accumulates state across iterations; all
    // assertions are stated in terms of the incremental deltas observed within
    // each iteration, not absolute counts.

    /// Node IDs returned by successive `add_node` calls are strictly increasing.
    #[test]
    fn node_ids_are_monotonically_increasing() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(label in "[A-Z]{1,4}")| {
            let a = g.add_node(&label, &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let b = g.add_node(&label, &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(a < b);
        });
    }

    /// Edge IDs returned by successive `add_edge` calls are strictly increasing.
    #[test]
    fn edge_ids_are_monotonically_increasing() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let src = g.add_node("N", &()).unwrap();
        let dst = g.add_node("N", &()).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(_dummy in 0u8..=255)| {
            let a = g.add_edge(src, dst, "E", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let b = g.add_edge(src, dst, "E", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(a < b);
        });
    }

    /// Every pair of `add_node` calls produces two distinct IDs.
    #[test]
    fn node_ids_are_unique() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(_dummy in 0u8..=255)| {
            let a = g.add_node("N", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let b = g.add_node("N", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_ne!(a, b);
        });
    }

    /// Every `add_edge(src, dst)` adds exactly one entry to `out_neighbors(src)`
    /// and one to `in_neighbors(dst)`.
    #[test]
    fn adjacency_round_trip() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let src = g.add_node("N", &()).unwrap();
        let dst = g.add_node("N", &()).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(_dummy in 0u8..=255)| {
            let before_out = g.out_neighbors(src).unwrap().len();
            let before_in = g.in_neighbors(dst).unwrap().len();
            let eid = g.add_edge(src, dst, "E", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let out: Vec<_> = g.out_neighbors(src).unwrap();
            let inc: Vec<_> = g.in_neighbors(dst).unwrap();
            prop_assert_eq!(out.len(), before_out + 1);
            prop_assert_eq!(inc.len(), before_in + 1);
            prop_assert!(out.iter().any(|&(_, e, _)| e == eid));
            prop_assert!(inc.iter().any(|&(_, e, _)| e == eid));
        });
    }

    /// Each `add_node("Target", ...)` call adds exactly one entry to
    /// `nodes_by_label("Target")`, and nodes of other labels are never included.
    #[test]
    fn label_index_exact_membership() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(insert_other in proptest::bool::ANY)| {
            let before = g.nodes_by_label("Target").unwrap().len();
            let id = g.add_node("Target", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            if insert_other {
                g.add_node("Other", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            let after = g.nodes_by_label("Target").unwrap();
            prop_assert_eq!(after.len(), before + 1);
            prop_assert!(after.contains(&id));
        });
    }

    /// Each `add_edge(..., "Target", ...)` call adds exactly one entry to
    /// `edges_by_type("Target")`, and edges of other types are never included.
    #[test]
    fn type_index_exact_membership() {
        let _dir = TempDir::new().unwrap();
        let g = Graph::open(_dir.path(), 1).unwrap();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let config = ProptestConfig {
            fork: false,
            cases: 200,
            ..Default::default()
        };
        proptest!(config, |(insert_other in proptest::bool::ANY)| {
            let before = g.edges_by_type("Target").unwrap().len();
            let eid = g.add_edge(a, b, "Target", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            if insert_other {
                g.add_edge(a, b, "Other", &()).map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            let after = g.edges_by_type("Target").unwrap();
            prop_assert_eq!(after.len(), before + 1);
            prop_assert!(after.contains(&eid));
        });
    }
}
