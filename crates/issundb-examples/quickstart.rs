use issundb::{Graph, GraphQueryExt};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Clean up any existing data directory from a previous run to start from a clean slate
    let db_path = Path::new("./issundb-data-quickstart");
    if db_path.exists() {
        let _ = std::fs::remove_dir_all(db_path);
    }

    // Open a graph database with a 1 GB memory map size limit
    let graph = Graph::open(db_path, 1)?;

    // Add two nodes with properties
    let alice_props = serde_json::json!({ "name": "Alice", "age": 30 });
    let alice_id = graph.add_node("Person", &alice_props)?;

    // Add another node
    let bob_props = serde_json::json!({ "name": "Bob", "age": 28 });
    let bob_id = graph.add_node("Person", &bob_props)?;

    // Create an edge between Alice and Bob with a property
    let edge_props = serde_json::json!({ "since": 2021 });
    graph.add_edge(alice_id, bob_id, "KNOWS", &edge_props)?;

    // Optional: rebuild CSR snapshot manually after bulk writes
    graph.rebuild_csr()?;

    // Execute a Cypher query and print the results
    let result =
        graph.query("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since")?;

    for record in result.records {
        println!(
            "Match: {} knows {} since {}",
            record.values[0], record.values[1], record.values[2]
        );
    }

    // Remove the database directory now that we're done with the example
    if db_path.exists() {
        let _ = std::fs::remove_dir_all(db_path);
    }

    Ok(())
}
