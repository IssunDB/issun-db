pub(crate) mod columns;
pub(crate) mod csr;
mod error;
mod graph;
pub(crate) mod histogram;
pub(crate) mod matrices;
mod schema;
pub(crate) mod storage;

pub use error::Error;
pub use graph::{DegreeDirection, Graph, ReadTxn, TriangleCountSpec, WriteTxn};
pub use schema::{
    DirectedNeighborEntry, EdgeId, EdgeRecord, LabelId, Language, NeighborEntry, NodeId,
    NodeRecord, PropValue, TypeId, WeightedPath,
};

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
        assert_eq!(neighbors[0].node, bob);
        assert_eq!(neighbors[0].edge, eid);
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
        out.sort_unstable_by_key(|ne| ne.node);
        assert_eq!(out.len(), 2);
        let edge_ids: Vec<_> = out.iter().map(|ne| ne.edge).collect();
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
        assert_eq!(out[0].edge, eid);
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
        assert_eq!(out[0].node, b);
        assert_eq!(out[0].edge, eid);
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
    fn bfs_works_via_dynamic_matrix_materialization_without_manual_rebuild() {
        let (_dir, g) = open_tmp();
        // Dynamic materialization automatically loads the newly added nodes into CSR snapshot and MatrixSet.
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
    // DFS and Traversal Algorithms
    // ------------------------------------------------------------------

    #[test]
    fn dfs_hops_zero_returns_start_only() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();

        let result = g.dfs(a, 0).unwrap();
        assert_eq!(result, vec![a]);
    }

    #[test]
    fn dfs_linear_chain_pre_order_and_limit() {
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

        let hop1 = g.dfs(a, 1).unwrap();
        assert_eq!(hop1, vec![a, b]);

        let hop2 = g.dfs(a, 2).unwrap();
        assert_eq!(hop2, vec![a, b, c]);

        let hop3 = g.dfs(a, 3).unwrap();
        assert_eq!(hop3, vec![a, b, c, d]);
    }

    #[test]
    fn dfs_does_not_loop_on_cycle() {
        let (_dir, g) = open_tmp();
        // Build a → b → c → a  (cycle)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.dfs(a, 10).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], a);
        assert_eq!(result[1], b);
        assert_eq!(result[2], c);
    }

    #[test]
    fn cycle_detection() {
        let (_dir, g) = open_tmp();

        // Empty graph is acyclic
        assert!(!g.detect_cycle().unwrap());

        // Acyclic linear graph
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        assert!(!g.detect_cycle().unwrap());

        // Self loop
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(d, d, "E", &json!({})).unwrap();
        assert!(g.detect_cycle().unwrap());
    }

    #[test]
    fn cycle_detection_multi_hop() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        assert!(g.detect_cycle().unwrap());
    }

    #[test]
    fn all_neighbors_retrieval() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        let e1 = g.add_edge(a, b, "OUT", &json!({})).unwrap();
        let e2 = g.add_edge(c, a, "IN", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let mut neighbors = g.all_neighbors(a).unwrap();
        neighbors.sort_by_key(|ne| ne.node);

        assert_eq!(neighbors.len(), 2);

        // Neighbor b: outgoing edge
        assert_eq!(neighbors[0].node, b);
        assert_eq!(neighbors[0].edge, e1);
        assert!(neighbors[0].outgoing); // is_outgoing == true

        // Neighbor c: incoming edge
        assert_eq!(neighbors[1].node, c);
        assert_eq!(neighbors[1].edge, e2);
        assert!(!neighbors[1].outgoing); // is_outgoing == false
    }

    // ------------------------------------------------------------------
    // Phase 2 Pathing Algorithms
    // ------------------------------------------------------------------

    #[test]
    fn all_paths_linear_and_multiple() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        // Single linear path: a → b → c
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let paths = g.all_paths(a, c).unwrap();
        assert_eq!(paths, vec![vec![a, b, c]]);

        // Add a second, parallel path: a → c
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut paths = g.all_paths(a, c).unwrap();
        paths.sort_by_key(|p| p.len());
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], vec![a, c]);
        assert_eq!(paths[1], vec![a, b, c]);
    }

    #[test]
    fn all_paths_cyclic_avoids_infinite_loop() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        // Path: a → b → c, plus cycle b → a
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(b, a, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let paths = g.all_paths(a, c).unwrap();
        // Should only return the simple path vec![a, b, c]
        assert_eq!(paths, vec![vec![a, b, c]]);
    }

    #[test]
    fn all_shortest_paths_multiple() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        // Two paths of length 2: a → b → d and a → c → d
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut paths = g.all_shortest_paths(a, d).unwrap();
        paths.sort();

        let mut expected = vec![vec![a, b, d], vec![a, c, d]];
        expected.sort();

        assert_eq!(paths, expected);
    }

    #[test]
    fn all_shortest_paths_unreachable_returns_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        assert!(g.all_shortest_paths(a, b).unwrap().is_empty());
    }

    #[test]
    fn longest_path_selection() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        // Paths: a → b → c (length 2) and a → c (length 1)
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let path = g.longest_path(a, c).unwrap().unwrap();
        assert_eq!(path, vec![a, b, c]);
    }

    #[test]
    fn longest_path_unreachable_returns_none() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        assert!(g.longest_path(a, b).unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // Phase 3 Dijkstra Algorithm
    // ------------------------------------------------------------------

    #[test]
    fn dijkstra_shortest_path_same_node() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let wp = g.shortest_path_dijkstra(a, a).unwrap().unwrap();
        assert_eq!(wp.nodes, vec![a]);
        assert_eq!(wp.total_weight, 0.0);
    }

    #[test]
    fn dijkstra_shortest_path_linear_and_weighted_decision() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        // Path 1: a → b → c (2 hops) with weights 1.5 and 2.0 (total cost = 3.5)
        g.add_edge(a, b, "E", &json!({ "cost": 1.5 })).unwrap();
        g.add_edge(b, c, "E", &json!({ "cost": 2.0 })).unwrap();

        // Path 2: a → c (1 hop) with weight 10.0 (total cost = 10.0)
        g.add_edge(a, c, "E", &json!({ "cost": 10.0 })).unwrap();

        g.rebuild_csr().unwrap();

        // Dijkstra must select the longer hop path (a → b → c) because its total cost (3.5) is smaller than 10.0
        let wp = g.shortest_path_dijkstra(a, c).unwrap().unwrap();
        assert_eq!(wp.nodes, vec![a, b, c]);
        assert_eq!(wp.total_weight, 3.5);
    }

    #[test]
    fn dijkstra_shortest_path_defaults_to_one() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        // Path: a → b (no cost property, defaults to 1.0)
        g.add_edge(a, b, "E", &json!({})).unwrap();
        // Path: b → c (non-numeric property, defaults to 1.0)
        g.add_edge(b, c, "E", &json!({ "cost": "invalid" }))
            .unwrap();

        g.rebuild_csr().unwrap();

        let wp = g.shortest_path_dijkstra(a, c).unwrap().unwrap();
        assert_eq!(wp.nodes, vec![a, b, c]);
        assert_eq!(wp.total_weight, 2.0);
    }

    #[test]
    fn dijkstra_shortest_path_unreachable_returns_none() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        assert!(g.shortest_path_dijkstra(a, b).unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // Phase 4 Spanning Forest Algorithm
    // ------------------------------------------------------------------

    #[test]
    fn spanning_forest_empty() {
        let (_dir, g) = open_tmp();
        let forest = g.spanning_forest("weight", false).unwrap();
        assert!(forest.is_empty());
    }

    #[test]
    fn spanning_forest_min_max_cyclic() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        // Cyclic triangle graph:
        // a-b weight 1.0
        // b-c weight 2.0
        // a-c weight 3.0
        let e_ab = g.add_edge(a, b, "E", &json!({ "cost": 1.0 })).unwrap();
        let e_bc = g.add_edge(b, c, "E", &json!({ "cost": 2.0 })).unwrap();
        let e_ac = g.add_edge(a, c, "E", &json!({ "cost": 3.0 })).unwrap();

        // Minimum Spanning Forest: should pick e_ab (1.0) and e_bc (2.0)
        let mut min_forest = g.spanning_forest("cost", false).unwrap();
        min_forest.sort();
        let mut expected_min = vec![e_ab, e_bc];
        expected_min.sort();
        assert_eq!(min_forest, expected_min);

        // Maximum Spanning Forest: should pick e_ac (3.0) and e_bc (2.0)
        let mut max_forest = g.spanning_forest("cost", true).unwrap();
        max_forest.sort();
        let mut expected_max = vec![e_bc, e_ac];
        expected_max.sort();
        assert_eq!(max_forest, expected_max);
    }

    #[test]
    fn spanning_forest_disconnected() {
        let (_dir, g) = open_tmp();

        // Component 1: a-b (cost 5.0)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let e_ab = g.add_edge(a, b, "E", &json!({ "cost": 5.0 })).unwrap();

        // Component 2: c-d (cost 10.0)
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e_cd = g.add_edge(c, d, "E", &json!({ "cost": 10.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let mut forest = g.spanning_forest("cost", false).unwrap();
        forest.sort();

        let mut expected = vec![e_ab, e_cd];
        expected.sort();

        assert_eq!(forest, expected);
    }

    // ------------------------------------------------------------------
    // Phase 5 Label Propagation Algorithm
    // ------------------------------------------------------------------

    #[test]
    fn label_propagation_empty() {
        let (_dir, g) = open_tmp();
        let labels = g.label_propagation(10).unwrap();
        assert!(labels.is_empty());
    }

    #[test]
    fn label_propagation_singletons() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();

        let labels = g.label_propagation(10).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[&a], a);
        assert_eq!(labels[&b], b);
    }

    #[test]
    fn label_propagation_cliques() {
        let (_dir, g) = open_tmp();

        // Clique 1: nodes a, b, c (fully connected triangle)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();

        // Clique 2: nodes d, e, f (fully connected triangle)
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        let f = g.add_node("N", &json!({})).unwrap();
        g.add_edge(d, e, "E", &json!({})).unwrap();
        g.add_edge(e, f, "E", &json!({})).unwrap();
        g.add_edge(f, d, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let labels = g.label_propagation(100).unwrap();
        assert_eq!(labels.len(), 6);

        // Within Clique 1, they must all share the same community label
        let comm1 = labels[&a];
        assert_eq!(labels[&b], comm1);
        assert_eq!(labels[&c], comm1);

        // Within Clique 2, they must all share the same community label
        let comm2 = labels[&d];
        assert_eq!(labels[&e], comm2);
        assert_eq!(labels[&f], comm2);

        // Clique 1 and Clique 2 should have different community labels since they are disconnected
        assert_ne!(comm1, comm2);
    }

    // ------------------------------------------------------------------
    // Phase 6 Harmonic closeness centrality
    // ------------------------------------------------------------------

    #[test]
    fn harmonic_centrality_empty() {
        let (_dir, g) = open_tmp();
        let scores = g.harmonic_centrality().unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn harmonic_centrality_singletons() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();

        let scores = g.harmonic_centrality().unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[&a], 0.0);
        assert_eq!(scores[&b], 0.0);
    }

    #[test]
    fn harmonic_centrality_linear_chain() {
        let (_dir, g) = open_tmp();
        // A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.harmonic_centrality().unwrap();
        assert_eq!(scores.len(), 3);

        // A can reach B (dist 1) and C (dist 2): centrality = 1/1 + 1/2 = 1.5
        assert!((scores[&a] - 1.5).abs() < 1e-6);
        // B can reach C (dist 1): centrality = 1/1 = 1.0
        assert!((scores[&b] - 1.0).abs() < 1e-6);
        // C can reach no one: centrality = 0.0
        assert!((scores[&c] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn harmonic_centrality_triangle_clique() {
        let (_dir, g) = open_tmp();
        // A -> B -> C -> A
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.harmonic_centrality().unwrap();
        assert_eq!(scores.len(), 3);

        // Each node can reach one node at distance 1, and the other at distance 2
        // Centrality for each should be 1/1 + 1/2 = 1.5
        for &score in scores.values() {
            assert!((score - 1.5).abs() < 1e-6);
        }
    }

    #[test]
    fn harmonic_centrality_disconnected() {
        let (_dir, g) = open_tmp();
        // Component 1: A -> B
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();

        // Component 2: C
        let c = g.add_node("N", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.harmonic_centrality().unwrap();
        assert_eq!(scores.len(), 3);
        assert!((scores[&a] - 1.0).abs() < 1e-6);
        assert!((scores[&b] - 0.0).abs() < 1e-6);
        assert!((scores[&c] - 0.0).abs() < 1e-6);
    }

    // ------------------------------------------------------------------
    // Phase 7 Betweenness centrality
    // ------------------------------------------------------------------

    #[test]
    fn betweenness_centrality_empty() {
        let (_dir, g) = open_tmp();
        let scores = g.betweenness_centrality().unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn betweenness_centrality_singletons() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();

        let scores = g.betweenness_centrality().unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[&a], 0.0);
        assert_eq!(scores[&b], 0.0);
    }

    #[test]
    fn betweenness_centrality_linear_chain() {
        let (_dir, g) = open_tmp();
        // A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.betweenness_centrality().unwrap();
        assert_eq!(scores.len(), 3);
        assert_eq!(scores[&a], 0.0);
        assert_eq!(scores[&b], 1.0);
        assert_eq!(scores[&c], 0.0);
    }

    #[test]
    fn betweenness_centrality_diamond_graph() {
        let (_dir, g) = open_tmp();
        //     B
        //   /   \
        // A       D
        //   \   /
        //     C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.betweenness_centrality().unwrap();
        assert_eq!(scores.len(), 4);
        assert_eq!(scores[&a], 0.0);
        assert_eq!(scores[&d], 0.0);
        assert!((scores[&b] - 0.5).abs() < 1e-6);
        assert!((scores[&c] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn betweenness_centrality_disconnected() {
        let (_dir, g) = open_tmp();
        // Component 1: A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        // Component 2: D (isolated)
        let d = g.add_node("N", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let scores = g.betweenness_centrality().unwrap();
        assert_eq!(scores.len(), 4);
        assert_eq!(scores[&a], 0.0);
        assert_eq!(scores[&b], 1.0);
        assert_eq!(scores[&c], 0.0);
        assert_eq!(scores[&d], 0.0);
    }

    // ------------------------------------------------------------------
    // Phase 8 Strongly connected components
    // ------------------------------------------------------------------

    #[test]
    fn strongly_connected_components_empty() {
        let (_dir, g) = open_tmp();
        let comps = g.strongly_connected_components().unwrap();
        assert!(comps.is_empty());
    }

    #[test]
    fn strongly_connected_components_singletons() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();

        let comps = g.strongly_connected_components().unwrap();
        assert_eq!(comps.len(), 2);
        assert_ne!(comps[&a], comps[&b]);
    }

    #[test]
    fn strongly_connected_components_linear_chain() {
        let (_dir, g) = open_tmp();
        // A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let comps = g.strongly_connected_components().unwrap();
        assert_eq!(comps.len(), 3);
        assert_ne!(comps[&a], comps[&b]);
        assert_ne!(comps[&b], comps[&c]);
        assert_ne!(comps[&a], comps[&c]);
    }

    #[test]
    fn strongly_connected_components_loop() {
        let (_dir, g) = open_tmp();
        // A -> B -> C -> A
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, a, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let comps = g.strongly_connected_components().unwrap();
        assert_eq!(comps.len(), 3);
        assert_eq!(comps[&a], comps[&b]);
        assert_eq!(comps[&b], comps[&c]);
    }

    #[test]
    fn strongly_connected_components_disconnected_clusters() {
        let (_dir, g) = open_tmp();
        // Loop 1: A -> B -> A
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, a, "E", &json!({})).unwrap();

        // Loop 2: C -> D -> C
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.add_edge(d, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let comps = g.strongly_connected_components().unwrap();
        assert_eq!(comps.len(), 4);
        assert_eq!(comps[&a], comps[&b]);
        assert_eq!(comps[&c], comps[&d]);
        assert_ne!(comps[&a], comps[&c]);
    }

    // ------------------------------------------------------------------
    // Phase 9 Degree centrality
    // ------------------------------------------------------------------

    #[test]
    fn degree_centrality_empty() {
        let (_dir, g) = open_tmp();
        let scores = g.degree_centrality(DegreeDirection::Both).unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn degree_centrality_singletons() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();

        for dir in &[
            DegreeDirection::In,
            DegreeDirection::Out,
            DegreeDirection::Both,
        ] {
            let scores = g.degree_centrality(*dir).unwrap();
            assert_eq!(scores.len(), 2);
            assert_eq!(scores[&a], 0);
            assert_eq!(scores[&b], 0);
        }
    }

    #[test]
    fn degree_centrality_linear_chain() {
        let (_dir, g) = open_tmp();
        // A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        // Direction::Out
        let out_scores = g.degree_centrality(DegreeDirection::Out).unwrap();
        assert_eq!(out_scores[&a], 1);
        assert_eq!(out_scores[&b], 1);
        assert_eq!(out_scores[&c], 0);

        // Direction::In
        let in_scores = g.degree_centrality(DegreeDirection::In).unwrap();
        assert_eq!(in_scores[&a], 0);
        assert_eq!(in_scores[&b], 1);
        assert_eq!(in_scores[&c], 1);

        // Direction::Both
        let both_scores = g.degree_centrality(DegreeDirection::Both).unwrap();
        assert_eq!(both_scores[&a], 1);
        assert_eq!(both_scores[&b], 2);
        assert_eq!(both_scores[&c], 1);
    }

    #[test]
    fn degree_centrality_disconnected() {
        let (_dir, g) = open_tmp();
        // Component 1: A -> B
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();

        // Component 2: C (isolated)
        let c = g.add_node("N", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let both_scores = g.degree_centrality(DegreeDirection::Both).unwrap();
        assert_eq!(both_scores[&a], 1);
        assert_eq!(both_scores[&b], 1);
        assert_eq!(both_scores[&c], 0);
    }

    // ------------------------------------------------------------------
    // Phase 10 Maximum flow
    // ------------------------------------------------------------------

    #[test]
    fn maximum_flow_trivial() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let flow = g.maximum_flow(a, a, "cap").unwrap();
        assert_eq!(flow, 0.0);
    }

    #[test]
    fn maximum_flow_single_path_bottleneck() {
        let (_dir, g) = open_tmp();
        // A -(10.0)-> B -(5.0)-> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "capacity": 10.0 })).unwrap();
        g.add_edge(b, c, "E", &json!({ "capacity": 5.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let flow = g.maximum_flow(a, c, "capacity").unwrap();
        assert!((flow - 5.0).abs() < 1e-6);
    }

    #[test]
    fn maximum_flow_diamond_parallel() {
        let (_dir, g) = open_tmp();
        //      B (10.0)
        //    /   \
        // A        D
        //    \   /
        //      C (5.0)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "cap": 10.0 })).unwrap();
        g.add_edge(b, d, "E", &json!({ "cap": 10.0 })).unwrap();
        g.add_edge(a, c, "E", &json!({ "cap": 5.0 })).unwrap();
        g.add_edge(c, d, "E", &json!({ "cap": 5.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let flow = g.maximum_flow(a, d, "cap").unwrap();
        assert!((flow - 15.0).abs() < 1e-6);
    }

    #[test]
    fn maximum_flow_redirection() {
        let (_dir, g) = open_tmp();
        // Standard flow network requiring redirection:
        // A -> B (3.0), A -> C (2.0)
        // B -> C (1.0), B -> D (2.0)
        // C -> D (3.0)
        // Augmenting path 1: A -> B -> C -> D (flows: 1.0)
        // Augmenting path 2: A -> B -> D (flows: 2.0)
        // Augmenting path 3: A -> C -> D (flows: 2.0 - but A -> C is only 2.0 and B -> C redirect is used)
        // Let's verify max flow is 5.0.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "cap": 3.0 })).unwrap();
        g.add_edge(a, c, "E", &json!({ "cap": 2.0 })).unwrap();
        g.add_edge(b, c, "E", &json!({ "cap": 1.0 })).unwrap();
        g.add_edge(b, d, "E", &json!({ "cap": 2.0 })).unwrap();
        g.add_edge(c, d, "E", &json!({ "cap": 3.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let flow = g.maximum_flow(a, d, "cap").unwrap();
        assert!((flow - 5.0).abs() < 1e-6);
    }

    #[test]
    fn maximum_flow_disconnected() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let flow = g.maximum_flow(a, b, "cap").unwrap();
        assert_eq!(flow, 0.0);
    }

    // ------------------------------------------------------------------
    // Phase 11 Top-k path search
    // ------------------------------------------------------------------

    #[test]
    fn shortest_path_top_k_trivial() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let paths = g.shortest_path_top_k(a, a, 3, "weight").unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].nodes, vec![a]);
        assert_eq!(paths[0].total_weight, 0.0);
    }

    #[test]
    fn shortest_path_top_k_linear_chain() {
        let (_dir, g) = open_tmp();
        // A -> B -> C
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "cost": 2.0 })).unwrap();
        g.add_edge(b, c, "E", &json!({ "cost": 3.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let paths = g.shortest_path_top_k(a, c, 3, "cost").unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].nodes, vec![a, b, c]);
        assert!((paths[0].total_weight - 5.0).abs() < 1e-6);
    }

    #[test]
    fn shortest_path_top_k_diamond() {
        let (_dir, g) = open_tmp();
        //      B (cost 1.0, 1.0)
        //    /   \
        // A        D
        //    \   /
        //      C (cost 2.0, 2.0)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "cost": 1.0 })).unwrap();
        g.add_edge(b, d, "E", &json!({ "cost": 1.0 })).unwrap();
        g.add_edge(a, c, "E", &json!({ "cost": 2.0 })).unwrap();
        g.add_edge(c, d, "E", &json!({ "cost": 2.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let paths = g.shortest_path_top_k(a, d, 3, "cost").unwrap();
        assert_eq!(paths.len(), 2);

        // Path 1: A -> B -> D (cost 2.0)
        assert_eq!(paths[0].nodes, vec![a, b, d]);
        assert!((paths[0].total_weight - 2.0).abs() < 1e-6);

        // Path 2: A -> C -> D (cost 4.0)
        assert_eq!(paths[1].nodes, vec![a, c, d]);
        assert!((paths[1].total_weight - 4.0).abs() < 1e-6);
    }

    #[test]
    fn shortest_path_top_k_cyclic() {
        let (_dir, g) = open_tmp();
        // A -> B (cost 1.0)
        // B -> C (cost 1.0)
        // C -> D (cost 1.0)
        // A -> C (cost 3.0)
        // B -> A (cost 1.0 - cycle)
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();

        g.add_edge(a, b, "E", &json!({ "cost": 1.0 })).unwrap();
        g.add_edge(b, c, "E", &json!({ "cost": 1.0 })).unwrap();
        g.add_edge(c, d, "E", &json!({ "cost": 1.0 })).unwrap();
        g.add_edge(a, c, "E", &json!({ "cost": 3.0 })).unwrap();
        g.add_edge(b, a, "E", &json!({ "cost": 1.0 })).unwrap();

        g.rebuild_csr().unwrap();

        let paths = g.shortest_path_top_k(a, d, 4, "cost").unwrap();
        assert_eq!(paths.len(), 2);

        // Path 1: A -> B -> C -> D (cost 3.0)
        assert_eq!(paths[0].nodes, vec![a, b, c, d]);
        assert!((paths[0].total_weight - 3.0).abs() < 1e-6);

        // Path 2: A -> C -> D (cost 4.0)
        assert_eq!(paths[1].nodes, vec![a, c, d]);
        assert!((paths[1].total_weight - 4.0).abs() < 1e-6);
    }

    #[test]
    fn shortest_path_top_k_disconnected() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let paths = g.shortest_path_top_k(a, b, 3, "cost").unwrap();
        assert!(paths.is_empty());
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
    fn connected_components_graphblas_path_node_zero_connected() {
        // Regression: after rebuild_csr the GraphBLAS path runs. Node index 0
        // must join its component rather than being stranded as a singleton by a
        // label value colliding with the semiring's implicit zero.
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let cc = g.connected_components().unwrap();
        let distinct: std::collections::HashSet<u64> = cc.values().copied().collect();
        assert_eq!(distinct.len(), 1, "all three nodes form one component");
        assert_eq!(cc[&a], cc[&b]);
        assert_eq!(cc[&b], cc[&c]);
    }

    #[test]
    fn connected_components_graphblas_path_keeps_components_separate() {
        // Regression: the GraphBLAS WCC propagation must reduce over neighbor
        // labels, not the adjacency matrix value. Two disjoint edges A->B and
        // C->D form two components; a MinFirst semiring collapses every
        // edge-touching node into one component, so this guards MinSecond.
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let cc = g.connected_components().unwrap();
        assert_eq!(cc.len(), 4);
        assert_eq!(cc[&a], cc[&b]);
        assert_eq!(cc[&c], cc[&d]);
        assert_ne!(cc[&a], cc[&c], "disjoint edges must be separate components");
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
        assert_eq!(
            g.label_name(rec.primary_label().unwrap())
                .unwrap()
                .as_deref(),
            Some("Person")
        );
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
            prop_assert!(out.iter().any(|ne| ne.edge == eid));
            prop_assert!(inc.iter().any(|ne| ne.edge == eid));
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

#[cfg(test)]
mod differential_tests {
    use std::collections::HashMap;

    use proptest::prelude::*;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// Union-find reference for weakly connected components: treat every edge
    /// as undirected and union its endpoints. This is the textbook definition
    /// the GraphBLAS min-label propagation in `connected_components` computes.
    fn reference_wcc(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }
        let mut parent: Vec<usize> = (0..n).collect();
        for &(a, b) in edges {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
        (0..n).map(|i| find(&mut parent, i)).collect()
    }

    /// Two component labelings agree iff they induce the same partition: nodes
    /// share a label under one exactly when they share a label under the other.
    /// This is convention-independent, so it does not depend on which integer
    /// each side picked as a component representative.
    fn same_partition<A: Eq, B: Eq>(xs: &[A], ys: &[B]) -> bool {
        let n = xs.len();
        (0..n).all(|i| (0..n).all(|j| (xs[i] == xs[j]) == (ys[i] == ys[j])))
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        #[test]
        fn connected_components_matches_union_find(
            n in 1usize..12,
            edges in prop::collection::vec((0usize..12, 0usize..12), 0..24),
        ) {
            // A fresh graph per case: whole-graph algorithms cannot share state.
            let edges: Vec<(usize, usize)> =
                edges.into_iter().filter(|&(a, b)| a < n && b < n).collect();
            let (_dir, g) = open_tmp();
            let ids: Vec<NodeId> = (0..n)
                .map(|_| g.add_node("N", &json!({})).unwrap())
                .collect();
            for &(a, b) in &edges {
                g.add_edge(ids[a], ids[b], "E", &json!({})).unwrap();
            }
            g.rebuild_csr().unwrap();

            let got: HashMap<NodeId, u64> = g.connected_components().unwrap();
            let impl_labels: Vec<u64> = ids.iter().map(|id| got[id]).collect();
            let ref_labels = reference_wcc(n, &edges);

            prop_assert!(
                same_partition(&impl_labels, &ref_labels),
                "WCC partition mismatch: impl={:?} ref={:?} edges={:?}",
                impl_labels, ref_labels, edges
            );
        }
    }
}
