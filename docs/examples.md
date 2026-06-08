# Code Examples

This page provides examples demonstrating vector search, full-text search, and Cypher query execution.

## Vector Search Example

You can insert vector embeddings for nodes and perform k-nearest-neighbor search:

```rust
use issundb::{Graph, VectorGraphExt};

fn run_vector_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    let doc_node = graph.add_node("Document", &serde_json::json!({ "title": "Rust Guide" }))?;

    // Upsert a 3-dimensional vector embedding for the node
    graph.upsert_vector(doc_node, &[0.1, 0.9, 0.4])?;

    // Perform vector similarity search
    let query_vector = vec![0.15, 0.85, 0.35];
    let hits = graph.vector_search(&query_vector, 5)?;

    for hit in hits {
        println!("Node ID: {:?}, Distance: {}", hit.node_id, hit.distance);
    }
    Ok(())
}
```

## Full-Text Search Example

Create a text index on specific node properties to support unstructured text queries:

```rust
use issundb::{Graph, TextIndexExt, TextGraphExt, TextSearchOptions};

fn run_text_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Create full-text search index
    graph.create_text_index("Book", "summary")?;

    // Add nodes with indexed properties
    graph.add_node("Book", &serde_json::json!({
        "title": "Programming in Rust",
        "summary": "An introduction to Rust, systems programming, and memory safety."
    }))?;

    // Query the full-text search index
    let opts = TextSearchOptions::default();
    let hits = graph.text_search("memory safety", &opts)?;

    for hit in hits {
        println!("Match found on Node: {:?} with score: {}", hit.node_id, hit.score);
    }
    Ok(())
}
```

## Cypher Query Execution Example

Execute Cypher queries against your graph to create, match, and filter nodes and relationships:

```rust
use std::collections::HashMap;
use issundb::{Graph, GraphQueryExt};

fn run_cypher(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Execute a read-write transaction to populate nodes and edges
    let cypher = "
        CREATE (p1:Person {name: 'Alice', age: 30})
        CREATE (p2:Person {name: 'Bob', age: 25})
        CREATE (p1)-[:FRIEND]->(p2)
    ";
    graph.query(cypher)?;

    // Execute a read-only parameterized query
    let query = "
        MATCH (a:Person)-[:FRIEND]->(b:Person)
        WHERE a.age > $min_age
        RETURN a.name, b.name
    ";
    let mut params = HashMap::new();
    params.insert("min_age".to_string(), serde_json::Value::from(20));

    let result = graph.query_with_params(query, &params)?;
    for record in result.records {
        println!("Matched friendship: {} knows {}", record.values[0], record.values[1]);
    }
    Ok(())
}
```
