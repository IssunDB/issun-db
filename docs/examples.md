# Code Examples

This page provides practical examples demonstrating vector search, full-text search, Cypher query execution, and GraphBLAS algorithm execution.

## Vector Search Example

You can insert vector embeddings for nodes and perform $k$-nearest-neighbor search:

```rust
use issundb::{Graph, VectorGraphExt};
use serde_json::json;

fn run_vector_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    let doc_node = graph.add_node("Document", &json!({ "title": "Rust Guide" }))?;

    // Upsert a 3-dimensional vector embedding for the node
    graph.upsert_vector(doc_node, &[0.1, 0.9, 0.4])?;

    // Perform vector similarity search
    let query_vector = vec![0.15, 0.85, 0.35];
    let hits = graph.vector_search(&query_vector, 5)?;

    for hit in hits {
        println!("Node ID: {:?}, Distance: {}", hit.node, hit.distance);
    }
    Ok(())
}
```

## Vector Search in Cypher

Vector distance is also available inside Cypher through the `vector_dist` function, so nearest-neighbor
ranking can be expressed declaratively alongside graph patterns. The first argument is a node (its stored
embedding is resolved) or a numeric vector, and the second is the query vector; the distance uses the
graph's configured metric.

```cypher
MATCH (p:Document)
WHERE p.language = 'English'
RETURN p.title
ORDER BY vector_dist(p, $query_vector)
LIMIT 10
```

When the query is an ascending `ORDER BY vector_dist(node, query)` with a `LIMIT` over a labeled scan, the
planner answers it with a single HNSW index search instead of computing a distance for every node, and it
pushes any equality `WHERE` predicate over the scanned variable (such as `p.language = 'English'`) into the
index traversal as a pre-filter. Any other shape (for example descending order or a non-constant query
vector) falls back to exact evaluation over the row pipeline, so results are always correct.

## Full-Text Search Example

Create a text index on specific node properties to support unstructured text queries:

```rust
use issundb::{Graph, TextIndexExt, TextGraphExt, TextSearchOptions};
use serde_json::json;

fn run_text_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Create full-text search index
    graph.create_text_index("Book", "summary")?;

    // Add nodes with indexed properties
    graph.add_node("Book", &json!({
        "title": "Programming in Rust",
        "summary": "An introduction to Rust, systems programming, and memory safety."
    }))?;

    // Query the full-text search index
    let opts = TextSearchOptions::default();
    let hits = graph.text_search("memory safety", &opts)?;

    for hit in hits {
        println!("Match found on Node: {:?} with score: {}", hit.node, hit.score);
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

## GraphBLAS Algorithms Example

Use GraphBLAS bindings for path-finding and centrality algorithms:

```rust
use issundb::{Graph, NodeId};
use serde_json::json;

fn run_algorithms(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Populate a sample path
    let n1 = graph.add_node("Station", &json!({ "name": "Station A" }))?;
    let n2 = graph.add_node("Station", &json!({ "name": "Station B" }))?;
    let n3 = graph.add_node("Station", &json!({ "name": "Station C" }))?;

    // Add weighted edges for path-finding (weight property is 'cost')
    graph.add_edge(n1, n2, "CONNECTS", &json!({ "cost": 5 }))?;
    graph.add_edge(n2, n3, "CONNECTS", &json!({ "cost": 10 }))?;
    graph.add_edge(n1, n3, "CONNECTS", &json!({ "cost": 20 }))?;

    // The algorithms refresh the in-memory CSR snapshot on demand, so no
    // explicit rebuild call is needed after the writes above.

    // 1. Dijkstra Shortest Path: Finds the cheapest path using the 'cost' property
    let path = graph.shortest_path_top_k(n1, n3, 1, "cost")?;
    if let Some(shortest) = path.first() {
        println!("Cheapest path nodes: {:?}", shortest.nodes); // Should go Station A -> Station B -> Station C
        println!("Total cost: {}", shortest.total_weight);     // Total cost = 15.0
    }

    // 2. PageRank: Run 20 iterations of PageRank centrality with damping 0.85
    let ranks = graph.page_rank(20, 0.85)?;
    for (node_id, rank) in ranks.iter().take(5) {
        println!("Node ID: {:?}, PageRank Score: {}", node_id, rank);
    }

    Ok(())
}
```
