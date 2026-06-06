//! Tests against the TinkerPop Modern graph, the 6-node, 6-edge reference dataset whose traversal
//! results are documented by the Apache TinkerPop project. Expected values below come from that
//! documentation, so these tests check the engine against externally fixed answers rather than
//! against its own output.
//!
//! Vertices: marko (person, 29), vadas (person, 27), josh (person, 32),
//! peter (person, 35), lop (software, java), ripple (software, java).
//! Edges: marko-knows->vadas (0.5), marko-knows->josh (1.0),
//! marko-created->lop (0.4), josh-created->ripple (1.0),
//! josh-created->lop (0.4), peter-created->lop (0.2).

use std::collections::HashMap;

use issundb::{DegreeDirection, Graph, GraphQueryExt, NodeId};
use serde_json::json;
use tempfile::TempDir;

fn load_modern() -> (TempDir, Graph, HashMap<&'static str, NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();

    let mut ids = HashMap::new();
    for (name, age) in [("marko", 29), ("vadas", 27), ("josh", 32), ("peter", 35)] {
        let id = g
            .add_node("person", &json!({ "name": name, "age": age }))
            .unwrap();
        ids.insert(name, id);
    }
    for name in ["lop", "ripple"] {
        let id = g
            .add_node("software", &json!({ "name": name, "lang": "java" }))
            .unwrap();
        ids.insert(name, id);
    }

    for (src, dst, etype, weight) in [
        ("marko", "vadas", "knows", 0.5),
        ("marko", "josh", "knows", 1.0),
        ("marko", "lop", "created", 0.4),
        ("josh", "ripple", "created", 1.0),
        ("josh", "lop", "created", 0.4),
        ("peter", "lop", "created", 0.2),
    ] {
        g.add_edge(ids[src], ids[dst], etype, &json!({ "weight": weight }))
            .unwrap();
    }

    (dir, g, ids)
}

fn node_name(g: &Graph, id: NodeId) -> String {
    let record = g.get_node(id).unwrap().unwrap();
    let props: serde_json::Value = rmp_serde::from_slice(&record.props).unwrap();
    props["name"].as_str().unwrap().to_string()
}

fn sorted_names(g: &Graph, ids: impl IntoIterator<Item = NodeId>) -> Vec<String> {
    let mut names: Vec<String> = ids.into_iter().map(|id| node_name(g, id)).collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

#[test]
fn modern_node_and_edge_counts() {
    let (_dir, g, _ids) = load_modern();
    assert_eq!(g.all_nodes().unwrap().len(), 6);
    assert_eq!(g.node_count_by_label("person").unwrap(), 4);
    assert_eq!(g.node_count_by_label("software").unwrap(), 2);
    assert_eq!(g.edge_count_by_type("knows").unwrap(), 2);
    assert_eq!(g.edge_count_by_type("created").unwrap(), 4);
}

#[test]
fn marko_knows_vadas_and_josh() {
    let (_dir, g, ids) = load_modern();
    let known: Vec<NodeId> = g
        .out_neighbors(ids["marko"])
        .unwrap()
        .into_iter()
        .filter(|n| g.type_name(n.edge_type).unwrap().as_deref() == Some("knows"))
        .map(|n| n.node)
        .collect();
    assert_eq!(sorted_names(&g, known), ["josh", "vadas"]);
}

#[test]
fn lop_created_by_marko_josh_and_peter() {
    let (_dir, g, ids) = load_modern();
    let creators: Vec<NodeId> = g
        .in_neighbors(ids["lop"])
        .unwrap()
        .into_iter()
        .map(|n| n.node)
        .collect();
    assert_eq!(sorted_names(&g, creators), ["josh", "marko", "peter"]);
}

#[test]
fn out_degrees_match_documented_topology() {
    let (_dir, g, ids) = load_modern();
    let out = g.degree_centrality(DegreeDirection::Out).unwrap();
    assert_eq!(out[&ids["marko"]], 3);
    assert_eq!(out[&ids["josh"]], 2);
    assert_eq!(out[&ids["peter"]], 1);
    assert_eq!(out[&ids["vadas"]], 0);
    assert_eq!(out[&ids["lop"]], 0);
    assert_eq!(out[&ids["ripple"]], 0);
}

#[test]
fn modern_is_one_weakly_connected_acyclic_component() {
    let (_dir, g, _ids) = load_modern();
    let components = g.connected_components().unwrap();
    let first = *components.values().next().unwrap();
    assert_eq!(components.len(), 6);
    assert!(components.values().all(|&c| c == first));
    assert!(!g.detect_cycle().unwrap());
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[test]
fn shortest_path_marko_to_ripple_goes_through_josh() {
    let (_dir, g, ids) = load_modern();
    let path = g
        .shortest_path(ids["marko"], ids["ripple"])
        .unwrap()
        .expect("ripple is reachable from marko");
    assert_eq!(path, vec![ids["marko"], ids["josh"], ids["ripple"]]);
}

#[test]
fn weighted_shortest_path_marko_to_ripple_costs_two() {
    let (_dir, g, ids) = load_modern();
    let path = g
        .shortest_path_dijkstra(ids["marko"], ids["ripple"])
        .unwrap()
        .expect("ripple is reachable from marko");
    assert_eq!(path.nodes, vec![ids["marko"], ids["josh"], ids["ripple"]]);
    assert!((path.total_weight - 2.0).abs() < 1e-9);
}

#[test]
fn two_hop_frontier_from_marko_is_lop_and_ripple() {
    // The Gremlin reference traversal g.V().out().out() resolves to lop and
    // ripple: the only two-hop paths run marko-knows->josh-created->{lop,
    // ripple}.
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query("MATCH (a)-->(b)-->(c) RETURN c.name AS name ORDER BY name")
        .unwrap();
    let names: Vec<&str> = result
        .records
        .iter()
        .map(|r| r.values[0].as_str().unwrap())
        .collect();
    assert_eq!(names, ["lop", "ripple"]);
}

// ---------------------------------------------------------------------------
// Cypher reads
// ---------------------------------------------------------------------------

#[test]
fn cypher_marko_knows_over_thirty_is_josh() {
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query(
            "MATCH (m:person {name: 'marko'})-[:knows]->(p) \
             WHERE p.age > 30 RETURN p.name AS name",
        )
        .unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].values[0], json!("josh"));
}

#[test]
fn cypher_co_developers_of_marko_are_josh_and_peter() {
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query(
            "MATCH (m:person {name: 'marko'})-[:created]->(s)<-[:created]-(c) \
             RETURN c.name AS name ORDER BY name",
        )
        .unwrap();
    let names: Vec<&str> = result
        .records
        .iter()
        .map(|r| r.values[0].as_str().unwrap())
        .collect();
    assert_eq!(names, ["josh", "peter"]);
}

#[test]
fn cypher_mean_person_age_is_thirty_point_seven_five() {
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query("MATCH (p:person) RETURN avg(p.age) AS mean_age")
        .unwrap();
    assert_eq!(result.records.len(), 1);
    let mean = result.records[0].values[0].as_f64().unwrap();
    assert!((mean - 30.75).abs() < 1e-9);
}

#[test]
fn cypher_software_creators_grouped_by_project() {
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query(
            "MATCH (p:person)-[:created]->(s:software) \
             RETURN s.name AS software, count(p) AS creators ORDER BY software",
        )
        .unwrap();
    let rows: Vec<(String, i64)> = result
        .records
        .iter()
        .map(|r| {
            (
                r.values[0].as_str().unwrap().to_string(),
                r.values[1].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, [("lop".into(), 3), ("ripple".into(), 1)]);
}

#[test]
fn cypher_knows_edge_weights_match_fixture() {
    let (_dir, g, _ids) = load_modern();
    let result = g
        .query(
            "MATCH (:person {name: 'marko'})-[k:knows]->(p) \
             RETURN p.name AS name, k.weight AS weight ORDER BY weight",
        )
        .unwrap();
    let rows: Vec<(String, f64)> = result
        .records
        .iter()
        .map(|r| {
            (
                r.values[0].as_str().unwrap().to_string(),
                r.values[1].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "vadas");
    assert!((rows[0].1 - 0.5).abs() < 1e-9);
    assert_eq!(rows[1].0, "josh");
    assert!((rows[1].1 - 1.0).abs() < 1e-9);
}
