//! Regenerate the versioned LMDB snapshot used by storage-format compatibility
//! tests. Run via `make testdata`, which writes into `test_data/v<version>/db`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use issundb::Graph;
use serde_json::json;

fn main() {
    let path_str = std::env::args()
        .nth(1)
        .expect("usage: gen_testdata <output_dir>");
    let path = std::path::Path::new(&path_str);
    std::fs::create_dir_all(path).expect("create output dir");

    let g = Graph::open(path, 1).expect("open graph");

    // Five Person nodes with deterministic properties.
    let people = [
        json!({ "name": "Alice", "age": 30, "bio": "Alice enjoys distributed systems and graph databases." }),
        json!({ "name": "Bob",   "age": 25, "bio": "Bob is a software engineer interested in search and retrieval." }),
        json!({ "name": "Carol", "age": 35, "bio": "Carol researches machine learning and vector embeddings." }),
        json!({ "name": "Dave",  "age": 28, "bio": "Dave works on full-text indexing and information retrieval." }),
        json!({ "name": "Eve",   "age": 31, "bio": "Eve specializes in graph algorithms and network analysis." }),
    ];

    let mut person_ids = Vec::with_capacity(5);
    for props in &people {
        let id = g.add_node("Person", props).expect("add Person node");
        person_ids.push(id);
    }

    let [alice, bob, carol, dave, eve] = person_ids[..] else {
        panic!("expected exactly 5 Person nodes");
    };

    // One Company node.
    let company_id = g
        .add_node(
            "Company",
            &json!({ "name": "Acme Corp", "industry": "Technology" }),
        )
        .expect("add Company node");

    let no_props = json!({});

    // Six KNOWS edges forming a small social graph (not fully connected).
    let knows_edges = [
        (alice, bob),
        (bob, carol),
        (carol, dave),
        (dave, eve),
        (alice, carol),
        (eve, bob),
    ];
    let mut edge_ids = Vec::with_capacity(8);
    for (src, dst) in knows_edges {
        let eid = g
            .add_edge(src, dst, "KNOWS", &no_props)
            .expect("add KNOWS edge");
        edge_ids.push(eid);
    }

    // Two WORKS_AT edges: Alice and Bob work at Acme Corp.
    for person in [alice, bob] {
        let eid = g
            .add_edge(person, company_id, "WORKS_AT", &no_props)
            .expect("add WORKS_AT edge");
        edge_ids.push(eid);
    }

    let n_nodes = 6; // 5 Person + 1 Company
    let n_edges = edge_ids.len();

    println!("Generated test data at {:?}", path);
    println!("  Nodes: {n_nodes}");
    println!("  Edges: {n_edges}");
}
