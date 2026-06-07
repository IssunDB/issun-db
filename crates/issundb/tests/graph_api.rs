use issundb::{Graph, GraphQueryExt, NodeId, PropValue};
use serde_json::json;
use tempfile::TempDir;

fn open_tmp() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    (dir, g)
}

// ---------------------------------------------------------------------------
// Node CRUD
// ---------------------------------------------------------------------------

#[test]
fn node_insert_and_fetch() {
    let (_dir, g) = open_tmp();
    let id = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
    let record = g.get_node(id).unwrap().expect("node must exist");
    let props: serde_json::Value = rmp_serde::from_slice(&record.props).unwrap();
    assert_eq!(props["name"], "Alice");
}

#[test]
fn get_node_returns_none_for_missing_id() {
    let (_dir, g) = open_tmp();
    assert!(g.get_node(9999).unwrap().is_none());
}

#[test]
fn node_label_is_stored() {
    let (_dir, g) = open_tmp();
    let id = g.add_node("Company", &json!({})).unwrap();
    let record = g.get_node(id).unwrap().unwrap();
    // Each node carries one label here; different label names get different IDs.
    let id2 = g.add_node("Person", &json!({})).unwrap();
    let record2 = g.get_node(id2).unwrap().unwrap();
    assert_ne!(record.primary_label(), record2.primary_label());
}

// ---------------------------------------------------------------------------
// Edge CRUD
// ---------------------------------------------------------------------------

#[test]
fn edge_insert_and_fetch() {
    let (_dir, g) = open_tmp();
    let alice = g.add_node("Person", &json!({})).unwrap();
    let bob = g.add_node("Person", &json!({})).unwrap();
    let eid = g
        .add_edge(alice, bob, "KNOWS", &json!({ "since": 2021 }))
        .unwrap();
    let edge = g.get_edge(eid).unwrap().expect("edge must exist");
    assert_eq!(edge.src, alice);
    assert_eq!(edge.dst, bob);
    let props: serde_json::Value = rmp_serde::from_slice(&edge.props).unwrap();
    assert_eq!(props["since"], 2021);
}

#[test]
fn get_edge_returns_none_for_missing_id() {
    let (_dir, g) = open_tmp();
    assert!(g.get_edge(9999).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Adjacency
// ---------------------------------------------------------------------------

#[test]
fn out_neighbors_reflects_inserted_edge() {
    let (_dir, g) = open_tmp();
    let a = g.add_node("N", &json!({})).unwrap();
    let b = g.add_node("N", &json!({})).unwrap();
    let eid = g.add_edge(a, b, "REL", &json!({})).unwrap();

    let out = g.out_neighbors(a).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, b);
    assert_eq!(out[0].edge, eid);
}

#[test]
fn in_neighbors_reflects_inserted_edge() {
    let (_dir, g) = open_tmp();
    let a = g.add_node("N", &json!({})).unwrap();
    let b = g.add_node("N", &json!({})).unwrap();
    let eid = g.add_edge(a, b, "REL", &json!({})).unwrap();

    let inc = g.in_neighbors(b).unwrap();
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0].node, a);
    assert_eq!(inc[0].edge, eid);
}

#[test]
fn node_with_no_edges_has_empty_adjacency() {
    let (_dir, g) = open_tmp();
    let id = g.add_node("N", &json!({})).unwrap();
    assert!(g.out_neighbors(id).unwrap().is_empty());
    assert!(g.in_neighbors(id).unwrap().is_empty());
}

#[test]
fn multiple_out_edges_are_all_returned() {
    let (_dir, g) = open_tmp();
    let src = g.add_node("N", &json!({})).unwrap();
    let targets: Vec<NodeId> = (0..5)
        .map(|_| g.add_node("N", &json!({})).unwrap())
        .collect();
    for &dst in &targets {
        g.add_edge(src, dst, "E", &json!({})).unwrap();
    }
    let out = g.out_neighbors(src).unwrap();
    assert_eq!(out.len(), 5);
    let mut got: Vec<NodeId> = out.into_iter().map(|ne| ne.node).collect();
    got.sort_unstable();
    let mut expected = targets.clone();
    expected.sort_unstable();
    assert_eq!(got, expected);
}

#[test]
fn adjacency_type_id_matches_edge_record() {
    let (_dir, g) = open_tmp();
    let a = g.add_node("N", &json!({})).unwrap();
    let b = g.add_node("N", &json!({})).unwrap();
    let eid = g.add_edge(a, b, "TYPED", &json!({})).unwrap();
    let edge = g.get_edge(eid).unwrap().unwrap();
    let out = g.out_neighbors(a).unwrap();
    assert_eq!(out[0].edge_type, edge.edge_type);
}

// ---------------------------------------------------------------------------
// Secondary indexes
// ---------------------------------------------------------------------------

#[test]
fn nodes_by_label_across_mixed_inserts() {
    let (_dir, g) = open_tmp();
    let p1 = g.add_node("Person", &json!({})).unwrap();
    let _c = g.add_node("Company", &json!({})).unwrap();
    let p2 = g.add_node("Person", &json!({})).unwrap();

    let mut persons = g.nodes_by_label("Person").unwrap();
    persons.sort_unstable();
    assert_eq!(persons, vec![p1, p2]);
    assert_eq!(g.nodes_by_label("Company").unwrap().len(), 1);
    assert!(g.nodes_by_label("Robot").unwrap().is_empty());
}

#[test]
fn edges_by_type_across_mixed_inserts() {
    let (_dir, g) = open_tmp();
    let a = g.add_node("N", &json!({})).unwrap();
    let b = g.add_node("N", &json!({})).unwrap();
    let e1 = g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
    let _e2 = g.add_edge(a, b, "LIKES", &json!({})).unwrap();
    let e3 = g.add_edge(b, a, "KNOWS", &json!({})).unwrap();

    let mut knows = g.edges_by_type("KNOWS").unwrap();
    knows.sort_unstable();
    assert_eq!(knows, vec![e1, e3]);
    assert_eq!(g.edges_by_type("LIKES").unwrap().len(), 1);
    assert!(g.edges_by_type("FOLLOWS").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Persistence across reopens
// ---------------------------------------------------------------------------

#[test]
fn data_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let (node_id, edge_id) = {
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("Person", &json!({ "x": 1 })).unwrap();
        let b = g.add_node("Person", &json!({ "x": 2 })).unwrap();
        let eid = g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        (a, eid)
    };

    let g2 = Graph::open(dir.path(), 1).unwrap();
    assert!(g2.get_node(node_id).unwrap().is_some());
    assert!(g2.get_edge(edge_id).unwrap().is_some());
    assert_eq!(g2.nodes_by_label("Person").unwrap().len(), 2);
    assert_eq!(g2.edges_by_type("KNOWS").unwrap().len(), 1);
}

#[test]
fn ids_continue_from_last_value_after_reopen() {
    let dir = TempDir::new().unwrap();
    let last_node = {
        let g = Graph::open(dir.path(), 1).unwrap();
        g.add_node("N", &json!({})).unwrap();
        g.add_node("N", &json!({})).unwrap()
    };
    let g2 = Graph::open(dir.path(), 1).unwrap();
    let next_node = g2.add_node("N", &json!({})).unwrap();
    assert!(next_node > last_node);
}

#[test]
fn test_cypher_index_scan_integration() {
    use issundb::GraphQueryExt;

    let (_dir, g) = open_tmp();

    // 1. Create nodes
    g.add_node("Person", &json!({"name": "Alice", "age": 30}))
        .unwrap();
    g.add_node("Person", &json!({"name": "Bob", "age": 25}))
        .unwrap();
    g.add_node("Person", &json!({"name": "Charlie", "age": 30}))
        .unwrap();

    // 2. Create index on Person(age)
    g.create_node_property_index("Person", "age").unwrap();

    // 3. Rebuild CSR
    g.rebuild_csr().unwrap();

    // 4. Run Cypher query filtering on age
    let q = "MATCH (p:Person) WHERE p.age = 30 RETURN p.name AS name";
    let res = g.query(q).unwrap();

    // 5. Assert result
    assert_eq!(res.columns, vec!["name".to_string()]);
    let mut names: Vec<String> = res
        .records
        .into_iter()
        .map(|r| r.values[0].as_str().unwrap().to_string())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Alice".to_string(), "Charlie".to_string()]);
}

#[test]
fn test_facade_full_text_search_integration() {
    use issundb::{TextGraphExt, TextIndexExt, TextSearchOptions};

    let (_dir, g) = open_tmp();

    // Create a node property text index
    g.create_text_index("Movie", "synopsis").unwrap();

    // Insert some nodes
    let m1 = g
        .add_node(
            "Movie",
            &json!({
                "title": "Inception",
                "synopsis": "A dream thief thief enters the dreams of targets to steal secrets"
            }),
        )
        .unwrap();
    let m2 = g.add_node("Movie", &json!({
        "title": "Interstellar",
        "synopsis": "An astronaut astronaut traverses a wormhole in search of a new home in space"
    })).unwrap();

    // Perform full-text search via the facade
    let opts = TextSearchOptions {
        label: Some("Movie".to_string()),
        property: Some("synopsis".to_string()),
        limit: 10,
        ..Default::default()
    };
    let hits = g.text_search("astronaut space", &opts).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node, m2);

    let hits_thief = g.text_search("thief", &opts).unwrap();
    assert_eq!(hits_thief.len(), 1);
    assert_eq!(hits_thief[0].node, m1);
}

// ---------------------------------------------------------------------------
// Optimizer: scan-node selection and count reduction (end-to-end correctness)
// ---------------------------------------------------------------------------

/// Build a graph where Person is common and City is rare, so the optimizer
/// reverses `(:Person)-[:KNOWS]->(:City)` to start from City and walk the edge
/// incoming. The query results must be identical regardless of traversal order.
fn knows_city_graph() -> (TempDir, Graph) {
    let (dir, g) = open_tmp();
    let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
    let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
    let carol = g.add_node("Person", &json!({ "name": "Carol" })).unwrap();
    let paris = g.add_node("City", &json!({ "name": "Paris" })).unwrap();
    let lyon = g.add_node("City", &json!({ "name": "Lyon" })).unwrap();
    g.add_edge(alice, paris, "KNOWS", &json!({})).unwrap();
    g.add_edge(bob, paris, "KNOWS", &json!({})).unwrap();
    g.add_edge(carol, lyon, "KNOWS", &json!({})).unwrap();
    g.rebuild_csr().unwrap();
    (dir, g)
}

fn pairs(result: &issundb::QueryResult) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = result
        .records
        .iter()
        .map(|r| {
            (
                r.values[0].as_str().unwrap().to_string(),
                r.values[1].as_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn scan_selection_preserves_results_when_chain_reversed() {
    let (_dir, g) = knows_city_graph();
    let result = g
        .query("MATCH (a:Person)-[:KNOWS]->(b:City) RETURN a.name AS person, b.name AS city")
        .unwrap();
    assert_eq!(
        pairs(&result),
        vec![
            ("Alice".to_string(), "Paris".to_string()),
            ("Bob".to_string(), "Paris".to_string()),
            ("Carol".to_string(), "Lyon".to_string()),
        ]
    );
}

#[test]
fn scan_selection_multi_hop_preserves_results() {
    let (_dir, g) = open_tmp();
    let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
    let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
    let acme = g.add_node("Company", &json!({ "name": "Acme" })).unwrap();
    let paris = g.add_node("City", &json!({ "name": "Paris" })).unwrap();
    g.add_edge(alice, acme, "WORKS_AT", &json!({})).unwrap();
    g.add_edge(bob, acme, "WORKS_AT", &json!({})).unwrap();
    g.add_edge(acme, paris, "LOCATED_IN", &json!({})).unwrap();
    g.rebuild_csr().unwrap();
    let result = g
        .query(
            "MATCH (a:Person)-[:WORKS_AT]->(c:Company)-[:LOCATED_IN]->(city:City) \
             RETURN a.name AS person, city.name AS city",
        )
        .unwrap();
    assert_eq!(
        pairs(&result),
        vec![
            ("Alice".to_string(), "Paris".to_string()),
            ("Bob".to_string(), "Paris".to_string()),
        ]
    );
}

#[test]
fn reduce_count_returns_label_node_count() {
    let (_dir, g) = knows_city_graph();
    let result = g.query("MATCH (n:Person) RETURN count(*) AS c").unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].values[0].as_i64(), Some(3));
}

#[test]
fn reduce_count_bare_variable_matches_row_count() {
    let (_dir, g) = knows_city_graph();
    let result = g.query("MATCH (n:City) RETURN count(n) AS c").unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].values[0].as_i64(), Some(2));
}

#[test]
fn id_seek_returns_the_single_node() {
    let (_dir, g) = knows_city_graph();
    let alice = g
        .nodes_by_label("Person")
        .unwrap()
        .into_iter()
        .min()
        .unwrap();
    let q = format!("MATCH (n:Person) WHERE id(n) = {alice} RETURN n.name AS name");
    let result = g.query(&q).unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].values[0].as_str(), Some("Alice"));
}

#[test]
fn id_seek_with_wrong_label_returns_empty() {
    let (_dir, g) = knows_city_graph();
    // A Person id queried under the City label must yield no rows.
    let alice = g
        .nodes_by_label("Person")
        .unwrap()
        .into_iter()
        .min()
        .unwrap();
    let q = format!("MATCH (n:City) WHERE id(n) = {alice} RETURN n");
    let result = g.query(&q).unwrap();
    assert_eq!(result.records.len(), 0);
}

#[test]
fn id_seek_missing_id_returns_empty() {
    let (_dir, g) = knows_city_graph();
    let result = g
        .query("MATCH (n:Person) WHERE id(n) = 999999 RETURN n")
        .unwrap();
    assert_eq!(result.records.len(), 0);
}

// ---------------------------------------------------------------------------
// Fused multi-hop expand chains (execute_expand_chain_n)
// ---------------------------------------------------------------------------

#[test]
fn fused_three_hop_chain_preserves_results() {
    let (_dir, g) = open_tmp();
    let a = g.add_node("A", &json!({ "name": "a" })).unwrap();
    let b = g.add_node("B", &json!({ "name": "b" })).unwrap();
    let c = g.add_node("C", &json!({ "name": "c" })).unwrap();
    let d = g.add_node("D", &json!({ "name": "d" })).unwrap();
    g.add_edge(a, b, "R1", &json!({})).unwrap();
    g.add_edge(b, c, "R2", &json!({})).unwrap();
    g.add_edge(c, d, "R3", &json!({})).unwrap();
    g.rebuild_csr().unwrap();
    let result = g
        .query(
            "MATCH (a:A)-[:R1]->(x)-[:R2]->(y)-[:R3]->(z:D) \
             RETURN a.name AS s, z.name AS e",
        )
        .unwrap();
    assert_eq!(pairs(&result), vec![("a".to_string(), "d".to_string())]);
}

#[test]
fn fused_four_hop_chain_with_branching_multiplicity() {
    // a -> b -> {c1, c2} -> d -> e. Two distinct 4-hop paths must both appear.
    let (_dir, g) = open_tmp();
    let a = g.add_node("N", &json!({ "n": "a" })).unwrap();
    let b = g.add_node("N", &json!({ "n": "b" })).unwrap();
    let c1 = g.add_node("N", &json!({ "n": "c1" })).unwrap();
    let c2 = g.add_node("N", &json!({ "n": "c2" })).unwrap();
    let d = g.add_node("N", &json!({ "n": "d" })).unwrap();
    let e = g.add_node("N", &json!({ "n": "e" })).unwrap();
    g.add_edge(a, b, "R", &json!({})).unwrap();
    g.add_edge(b, c1, "R", &json!({})).unwrap();
    g.add_edge(b, c2, "R", &json!({})).unwrap();
    g.add_edge(c1, d, "R", &json!({})).unwrap();
    g.add_edge(c2, d, "R", &json!({})).unwrap();
    g.add_edge(d, e, "R", &json!({})).unwrap();
    g.rebuild_csr().unwrap();
    let result = g
        .query(
            "MATCH (a)-[:R]->(w)-[:R]->(m)-[:R]->(x)-[:R]->(z) \
             WHERE a.n = 'a' RETURN m.n AS mid",
        )
        .unwrap();
    // Two paths a->b->c1->d->e and a->b->c2->d->e: distinct middles c1 and c2.
    let mut mids: Vec<String> = result
        .records
        .iter()
        .map(|r| r.values[0].as_str().unwrap().to_string())
        .collect();
    mids.sort();
    assert_eq!(mids, vec!["c1".to_string(), "c2".to_string()]);
}

#[test]
fn fused_chain_breaks_on_labeled_intermediate() {
    // A label on the intermediate node forces a Filter between hops, so the chain
    // does not fuse; results must still be correct.
    let (_dir, g) = open_tmp();
    let a = g.add_node("A", &json!({ "n": "a" })).unwrap();
    let b = g.add_node("Stop", &json!({ "n": "b" })).unwrap();
    let c = g.add_node("A", &json!({ "n": "c" })).unwrap();
    let other = g.add_node("Other", &json!({ "n": "x" })).unwrap();
    g.add_edge(a, b, "R", &json!({})).unwrap();
    g.add_edge(b, c, "R", &json!({})).unwrap();
    g.add_edge(a, other, "R", &json!({})).unwrap();
    g.rebuild_csr().unwrap();
    let result = g
        .query("MATCH (a:A)-[:R]->(m:Stop)-[:R]->(z:A) RETURN a.n AS s, z.n AS e")
        .unwrap();
    assert_eq!(pairs(&result), vec![("a".to_string(), "c".to_string())]);
}

#[test]
fn test_facade_explain_integration() {
    let (_dir, g) = open_tmp();
    g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
    g.rebuild_csr().unwrap();

    let plan = g
        .explain("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.name")
        .unwrap();
    assert!(!plan.is_empty());
    assert!(plan.contains("Person"));
}

#[test]
fn test_facade_property_indexes_and_constraints_integration() {
    let (_dir, g) = open_tmp();

    // 1. Create property index
    g.create_node_property_index("Person", "age").unwrap();

    let a = g
        .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
        .unwrap();
    let b = g
        .add_node("Person", &json!({ "name": "Bob", "age": 25 }))
        .unwrap();
    g.rebuild_csr().unwrap();

    // 2. nodes_by_property point query
    let p30 = g
        .nodes_by_property("Person", "age", PropValue::Int(30))
        .unwrap();
    assert_eq!(p30, vec![a]);

    // 3. nodes_by_property_range query
    let pr = g
        .nodes_by_property_range(
            "Person",
            "age",
            Some(PropValue::Int(20)),
            true,
            Some(PropValue::Int(35)),
            true,
        )
        .unwrap();
    assert_eq!(pr.len(), 2);
    assert!(pr.contains(&a));
    assert!(pr.contains(&b));

    // 4. Unique constraint
    g.create_node_unique_constraint("User", "email").unwrap();
    g.add_node("User", &json!({ "email": "alice@example.com" }))
        .unwrap();
    let duplicate = g.add_node("User", &json!({ "email": "alice@example.com" }));
    assert!(duplicate.is_err());

    // 5. Required constraint
    g.create_node_required_constraint("Task", "title").unwrap();
    let task_err = g.add_node("Task", &json!({ "done": false }));
    assert!(task_err.is_err());
}
