# Code Examples

This page provides code examples for performing vector search, full-text keyword search, Cypher queries, running script files via the CLI, and executing GraphBLAS-backed graph algorithms in Rust and Cypher.

## Vector Search Example

The following Rust example demonstrates inserting vector embeddings for nodes and performing $k$-nearest-neighbor similarity searches:

```rust
use issundb::{Graph, VectorGraphExt};
use serde_json::json;

fn run_vector_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    let doc_node = graph.add_node("Document", &json!({ "title": "Rust Guide" }))?;

    // Upsert a 3-dimensional vector embedding for the node
    graph.upsert_vector(doc_node, &[0.1, 0.9, 0.4])?;

    // Perform a vector similarity search to find matching nodes
    let query_vector = vec![0.15, 0.85, 0.35];
    let hits = graph.vector_search(&query_vector, 5)?;

    for hit in hits {
        println!("Node ID: {:?}, Distance: {}", hit.node, hit.distance);
    }
    Ok(())
}
```

## Vector Search in Cypher

Vector similarity searches can be performed directly inside Cypher queries using the `vector_dist` function. This allows nearest-neighbor ranking to be expressed declaratively alongside graph patterns. The first argument is either a node variable (whose stored embedding is resolved) or a numeric vector, and the second is the query vector. The distance is computed using the metric configured for the graph:

```cypher
MATCH (p:Document)
WHERE p.language = 'English'
RETURN p.title
ORDER BY vector_dist(p, $query_vector)
LIMIT 10
```

When a query uses an ascending `ORDER BY vector_dist(node, query)` with a `LIMIT` over a labeled scan, the query planner uses a single HNSW index search instead of calculating distances for every node. It also pushes equality `WHERE` predicates (such as `p.language = 'English'`) into the index traversal as a pre-filter. Other query forms (such as descending order or a non-constant query vector) fall back to exact evaluation over the row pipeline.

## Full-Text Search Example

The following Rust example demonstrates configuring and querying a full-text search index on specific node properties:

```rust
use issundb::{Graph, TextIndexExt, TextGraphExt, TextSearchOptions};
use serde_json::json;

fn run_text_search(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Create a full-text search index on the 'summary' property of 'Book' nodes
    graph.create_text_index("Book", "summary")?;

    // Add a node with the indexed property
    graph.add_node("Book", &json!({
        "title": "Programming in Rust",
        "summary": "An introduction to Rust, systems programming, and memory safety."
    }))?;

    // Query the full-text index
    let opts = TextSearchOptions::default();
    let hits = graph.text_search("memory safety", &opts)?;

    for hit in hits {
        println!("Match found on Node: {:?} with score: {}", hit.node, hit.score);
    }
    Ok(())
}
```

## Cypher Query Execution Example

Cypher queries can be executed against the graph to create, match, and filter nodes and relationships. The following example demonstrates performing a read-write transaction to populate the graph, followed by a parameterized read-only query:

```rust
use std::collections::HashMap;
use issundb::{Graph, GraphQueryExt};

fn run_cypher(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Run a query to populate nodes and edges in the graph
    let cypher = "
        CREATE (p1:Person {name: 'Alice', age: 30})
        CREATE (p2:Person {name: 'Bob', age: 25})
        CREATE (p1)-[:FRIEND]->(p2)
    ";
    graph.query(cypher)?;

    // Run a parameterized query to retrieve friendship details
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

## Running a Cypher Script with the CLI

The CLI supports running script files containing meta commands, comments, and Cypher statements. A script can be executed inside the REPL using the `:run` command, or at launch using the `--script` (or `-f`) flag. For example, write the following to `setup.cypher`:

```cypher
// setup.cypher: open a database, seed it, and query it
:open ./issundb-data

CREATE (a:Person {name: 'Alice', age: 40})
CREATE (b:Person {name: 'Bob', age: 25})
CREATE (a)-[:FRIEND]->(b);

MATCH (p:Person)
WHERE p.age > 30
RETURN p.name;
```

Now the script can be executed using one of the following methods:

```bash
# Method 1: Inside the interactive REPL
issundb> :run ./setup.cypher

# Method 2: Batch mode from the terminal (exits with a non-zero status on failure)
issundb-cli --script ./setup.cypher
```

When writing scripts, remember that a semicolon inside a string literal or comment is not treated as a statement terminator, so values like
`{name: 'a;b'}` are safe. If two Cypher statements are written with no semicolon and no blank line between them, the CLI will read them as a single
statement, so it is a good idea to separate distinct statements with a semicolon. The `query`, `cypher`, and `:explain` forms always stay single-line.

## GraphBLAS Algorithms Example

The following example demonstrates running GraphBLAS-backed pathfinding and centrality algorithms. These algorithms run on the in-memory CSR snapshot and automatically refresh the snapshot on demand, removing the need to call `rebuild_csr()` manually after inserting data:

```rust
use issundb::{Graph, NodeId};
use serde_json::json;

fn run_algorithms(graph: &Graph) -> Result<(), Box<dyn std::error::Error>> {
    // Build a sample path to query
    let n1 = graph.add_node("Station", &json!({ "name": "Station A" }))?;
    let n2 = graph.add_node("Station", &json!({ "name": "Station B" }))?;
    let n3 = graph.add_node("Station", &json!({ "name": "Station C" }))?;

    // Add weighted edges where the weight property is called 'cost'
    graph.add_edge(n1, n2, "CONNECTS", &json!({ "cost": 5 }))?;
    graph.add_edge(n2, n3, "CONNECTS", &json!({ "cost": 10 }))?;
    graph.add_edge(n1, n3, "CONNECTS", &json!({ "cost": 20 }))?;

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

## Graph Data Science in Cypher

We can also invoke these analytics, pathfinding, and retrieval algorithms directly inside Cypher queries! This allows feeding algorithm results
directly into `MATCH`, `WHERE`, and `RETURN` clauses. There are two surfaces: built-in `CALL issundb.*` procedures and the `issundb.distance.*` and
`issundb.similarity.*` scalar functions. We can find a complete, runnable tour in the `gds_cypher.rs` example program (
`cargo run -p issundb-examples --example gds_cypher`).

### Built-in Procedures

Every procedure runs the algorithm against the live graph and yields one row per result. A procedure's `YIELD` columns are bound for the rest of the
query, so `nodeId` joins back to the matched nodes through `id()`.

| Procedure                                                   | Optional configuration                                                                                                                | Yields                       |
|-------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------|------------------------------|
| `issundb.pageRank`                                          | `{iterations, damping}`                                                                                                               | `nodeId, score`              |
| `issundb.betweenness`                                       | none                                                                                                                                  | `nodeId, score`              |
| `issundb.harmonic`                                          | none                                                                                                                                  | `nodeId, score`              |
| `issundb.degree`                                            | `{direction: 'IN'                                                                                                                     | 'OUT'                        |'BOTH'}` | `nodeId, score` |
| `issundb.connectedComponents` (alias `issundb.wcc`)         | none                                                                                                                                  | `nodeId, componentId`        |
| `issundb.stronglyConnectedComponents` (alias `issundb.scc`) | none                                                                                                                                  | `nodeId, componentId`        |
| `issundb.labelPropagation`                                  | `{maxIterations}`                                                                                                                     | `nodeId, communityId`        |
| `issundb.communities`                                       | `{maxIterations, topPerCommunity}`                                                                                                    | `communityId, nodeId, rank`  |
| `issundb.shortestPath`                                      | requires `(srcId, dstId)`                                                                                                             | `index, nodeId`              |
| `issundb.dijkstra`                                          | requires `(srcId, dstId)`                                                                                                             | `index, nodeId, totalWeight` |
| `issundb.triangleCount`                                     | `{relTypes, labels}`                                                                                                                  | `count`                      |
| `issundb.retrieve.vector`                                   | requires `queryVector`, then `{k, hops, maxDistance, maxNodes}`                                                                       | `nodeId, distance`           |
| `issundb.retrieve.hybrid`                                   | requires `queryVector, queryText`, then `{vectorK, textK, hops, maxDistance, maxNodes, textLabel, textProperty, vectorLabel, fusion}` | `nodeId, score`              |

The following Cypher query calculates PageRank scores and returns node names:

```cypher
CALL issundb.pageRank({iterations: 20, damping: 0.85}) YIELD nodeId, score
MATCH (p:Person) WHERE id(p) = nodeId
RETURN p.name AS name, score
ORDER BY score DESC
```

Vector and text hits can be fused before expanding the graph neighborhood during GraphRAG retrieval:

```cypher
CALL issundb.retrieve.hybrid([0.20, 0.85], 'machine learning',
  {vectorK: 2, textK: 2, textLabel: 'Person', textProperty: 'bio', hops: 1})
  YIELD nodeId, score
MATCH (p:Person) WHERE id(p) = nodeId
RETURN p.name AS name, score
ORDER BY score IS NULL, score DESC
```

### Comparison Functions

Four scalar functions compare two values pairwise. Vector measures are distances (lower is more similar), and set measures are similarities (higher is
more similar). A vector argument is either a numeric list or a node, in which case its stored embedding is resolved.

| Function                           | Operates on  | Returns                              |
|------------------------------------|--------------|--------------------------------------|
| `issundb.distance.cosine(a, b)`    | vectors      | cosine distance, in `[0, 2]`         |
| `issundb.distance.euclidean(a, b)` | vectors      | Euclidean (L2) distance, in `[0, ∞)` |
| `issundb.similarity.jaccard(a, b)` | sets (lists) | Jaccard similarity, in `[0, 1]`      |
| `issundb.similarity.overlap(a, b)` | sets (lists) | overlap coefficient, in `[0, 1]`     |

Each measure has a single canonical form, so the opposite direction is a short inline expression rather than a separate function: cosine similarity is
`1 - issundb.distance.cosine(a, b)`, Euclidean similarity is `1.0 / (1.0 + issundb.distance.euclidean(a, b))`, and a set distance is
`1 - issundb.similarity.jaccard(a, b)`. A null operand, or a vector length mismatch, yields null.

```cypher
MATCH (a:Person {name: 'Alice'}), (e:Person {name: 'Erin'})
RETURN issundb.distance.cosine(a, e) AS cosineDistance,
       1 - issundb.distance.cosine(a, e) AS cosineSimilarity,
       issundb.similarity.jaccard(a.skills, e.skills) AS skillJaccard
```
