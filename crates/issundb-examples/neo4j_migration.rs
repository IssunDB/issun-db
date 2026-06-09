//! A demo that shows how to migrate data from Neo4j into IssunDB.
//!
//! In a real migration, replace the sample data below with output from
//! `neo4j-admin dump` or the Neo4j Cypher export procedure. The sample
//! data here simulates the JSON structure that a Neo4j export produces
//! so you can drop in real data with minimal changes.
//!
//! Steps demonstrated:
//!   1. Define simulated Neo4j export structs (NeoNode, NeoEdge).
//!   2. Open an IssunDB graph.
//!   3. Import nodes and edges via `add_node` and `add_edge`.
//!   4. Check the import with a Cypher query.
//!   5. Print the result table.

use std::collections::HashMap;

use issundb::{Graph, GraphQueryExt};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Simulated Neo4j export types
// ---------------------------------------------------------------------------

/// A node row from a Neo4j Cypher export.
struct NeoNode {
    /// Unique ID in the source Neo4j database.
    neo_id: u64,
    /// Node label (first label in Neo4j's multi-label model).
    label: &'static str,
    /// Node properties as a JSON object.
    props: Value,
}

/// A relationship row from a Neo4j Cypher export.
struct NeoEdge {
    /// Neo4j ID of the source node.
    src_neo_id: u64,
    /// Neo4j ID of the destination node.
    dst_neo_id: u64,
    /// Relationship type.
    rel_type: &'static str,
    /// Relationship properties as a JSON object.
    props: Value,
}

fn sample_nodes() -> Vec<NeoNode> {
    vec![
        NeoNode {
            neo_id: 1,
            label: "Person",
            props: json!({"name": "Alice",   "age": 30, "city": "London"}),
        },
        NeoNode {
            neo_id: 2,
            label: "Person",
            props: json!({"name": "Bob",     "age": 25, "city": "Paris"}),
        },
        NeoNode {
            neo_id: 3,
            label: "Person",
            props: json!({"name": "Carol",   "age": 35, "city": "Berlin"}),
        },
        NeoNode {
            neo_id: 4,
            label: "Person",
            props: json!({"name": "David",   "age": 28, "city": "London"}),
        },
        NeoNode {
            neo_id: 5,
            label: "Person",
            props: json!({"name": "Eva",     "age": 32, "city": "Madrid"}),
        },
        NeoNode {
            neo_id: 6,
            label: "Person",
            props: json!({"name": "Frank",   "age": 40, "city": "Rome"}),
        },
        NeoNode {
            neo_id: 7,
            label: "Person",
            props: json!({"name": "Grace",   "age": 27, "city": "Amsterdam"}),
        },
        NeoNode {
            neo_id: 8,
            label: "Person",
            props: json!({"name": "Henry",   "age": 45, "city": "Brussels"}),
        },
        NeoNode {
            neo_id: 9,
            label: "Person",
            props: json!({"name": "Iris",    "age": 22, "city": "Vienna"}),
        },
        NeoNode {
            neo_id: 10,
            label: "Person",
            props: json!({"name": "Jack",    "age": 38, "city": "Warsaw"}),
        },
    ]
}

fn sample_edges() -> Vec<NeoEdge> {
    vec![
        NeoEdge {
            src_neo_id: 1,
            dst_neo_id: 2,
            rel_type: "KNOWS",
            props: json!({"since": 2018}),
        },
        NeoEdge {
            src_neo_id: 1,
            dst_neo_id: 3,
            rel_type: "KNOWS",
            props: json!({"since": 2020}),
        },
        NeoEdge {
            src_neo_id: 2,
            dst_neo_id: 4,
            rel_type: "KNOWS",
            props: json!({"since": 2019}),
        },
        NeoEdge {
            src_neo_id: 3,
            dst_neo_id: 5,
            rel_type: "KNOWS",
            props: json!({"since": 2021}),
        },
        NeoEdge {
            src_neo_id: 4,
            dst_neo_id: 6,
            rel_type: "KNOWS",
            props: json!({"since": 2017}),
        },
        NeoEdge {
            src_neo_id: 5,
            dst_neo_id: 7,
            rel_type: "KNOWS",
            props: json!({"since": 2022}),
        },
        NeoEdge {
            src_neo_id: 7,
            dst_neo_id: 8,
            rel_type: "KNOWS",
            props: json!({"since": 2016}),
        },
        NeoEdge {
            src_neo_id: 8,
            dst_neo_id: 9,
            rel_type: "KNOWS",
            props: json!({"since": 2023}),
        },
    ]
}

// ---------------------------------------------------------------------------
// Migration entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Neo4j to IssunDB Migration Demo");
    println!("================================\n");

    // ---- 2. Open an IssunDB graph ------------------------------------------
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    // ---- 3. Import nodes ---------------------------------------------------
    // Map from Neo4j node ID to IssunDB NodeId so we can wire up edges.
    let mut id_map: HashMap<u64, issundb::NodeId> = HashMap::new();

    let nodes = sample_nodes();
    println!("Importing {} Person nodes...", nodes.len());
    for neo_node in nodes {
        let issun_id = graph.add_node(neo_node.label, &neo_node.props)?;
        id_map.insert(neo_node.neo_id, issun_id);
        let name = neo_node
            .props
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("  Imported {:?}: {}", issun_id, name);
    }

    // ---- 3. Import edges ---------------------------------------------------
    let edges = sample_edges();
    println!("\nImporting {} KNOWS edges...", edges.len());
    for neo_edge in &edges {
        let src = *id_map
            .get(&neo_edge.src_neo_id)
            .expect("source node not found in id_map");
        let dst = *id_map
            .get(&neo_edge.dst_neo_id)
            .expect("destination node not found in id_map");
        let edge_id = graph.add_edge(src, dst, neo_edge.rel_type, &neo_edge.props)?;
        println!("  Imported edge {:?}: {:?} -> {:?}", edge_id, src, dst);
    }

    // Optional: rebuild CSR snapshot manually after bulk writes
    graph.rebuild_csr()?;

    // ---- 4. Verify the import by running a Cypher query --------------------------
    println!(
        "\n--- Cypher verification: MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name ---"
    );
    let result = graph.query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")?;

    // ---- 5. Print the result table -----------------------------------------
    println!("{:<20} {:<20}", "from", "to");
    println!("{}", "-".repeat(42));
    for record in &result.records {
        let from = record
            .values
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let to = record.values.get(1).and_then(|v| v.as_str()).unwrap_or("?");
        println!("{:<20} {:<20}", from, to);
    }

    println!("\nImported {} relationships total.", result.records.len());
    println!("Migration complete.");

    Ok(())
}
