<div align="center">
  <picture>
    <img alt="IssunDB Logo" src="logo.svg" height="25%" width="25%">
  </picture>
<br>

<h2>IssunDB</h2>

[![Tests](https://img.shields.io/github/actions/workflow/status/IssunDB/issun-db/tests.yml?label=tests&style=flat&labelColor=282c34&logo=github)](https://github.com/IssunDB/issun-db/actions/workflows/tests.yml)
[![Code Coverage](https://img.shields.io/codecov/c/github/IssunDB/issun-db?label=coverage&style=flat&labelColor=282c34&logo=codecov)](https://codecov.io/gh/IssunDB/issun-db)
[![Crates.io](https://img.shields.io/crates/v/issun-db.svg?label=crates.io&style=flat&labelColor=282c34&color=fc8d62&logo=rust)](https://crates.io/crates/issun-db)
[![Docs.rs](https://img.shields.io/badge/docs-issun-db-66c2a5?style=flat&labelColor=282c34&logo=docs.rs)](https://docs.rs/issun-db)
[![MSRV](https://img.shields.io/badge/msrv-1.85.0-informational?style=flat&labelColor=282c34&logo=rust)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-007ec6?style=flat&labelColor=282c34&logo=open-source-initiative)](https://github.com/IssunDB/issun-db)

A fast embedded graph database in Rust

</div>

---

An embedded graph database with property graph data model, Cypher support, vector and full-text search, written in Rust.

### Key Features

* **Embedded Graph Storage**: Native Rust graph engine built on LMDB with transactional guarantees, monotonic ID allocation, and adjacency lists stored using `DUPSORT`.
* **GraphBLAS Algorithms**: High-performance graph traversal and analytics operating on an in-memory Compressed Sparse Row (CSR) snapshot, supporting PageRank, Dijkstra, and betweenness centrality.
* **Vector Search**: Integrated vector index using `usearch` for fast similarity searches and nearest-neighbor retrieval.
* **Full-Text Search**: Inverted index storage with BM25 and TF-IDF ranking, custom tokenization, and multi-language support.
* **Hybrid Retrieval**: Unified queries combining graph traversal, vector similarity, and text relevance scores.
* **Cypher Query Language**: Planner and executor for an openCypher query subset supporting `MATCH`, `WHERE`, `RETURN`, `CREATE`, `SET`, and `DELETE`.

---

### Quickstart

To use IssunDB in your Rust project, add the dependency to your `Cargo.toml`:

```toml
[dependencies]
issundb = { path = "path/to/issundb" }
serde_json = "1.0"
```

Here is a basic example showing how to open a database, insert nodes, establish relationships, and query the graph using Cypher:

```rust
use std::path::Path;
use issundb::{Graph, GraphQueryExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open a graph database environment with a 1 GB map size limit
    let graph = Graph::open(Path::new("./issundb-data"), 1)?;

    // Add nodes with properties
    let alice_props = serde_json::json!({ "name": "Alice", "age": 30 });
    let alice_id = graph.add_node("Person", &alice_props)?;

    let bob_props = serde_json::json!({ "name": "Bob", "age": 28 });
    let bob_id = graph.add_node("Person", &bob_props)?;

    // Create an edge connecting the nodes
    let edge_props = serde_json::json!({ "since": 2021 });
    graph.add_edge(alice_id, bob_id, "KNOWS", &edge_props)?;

    // Rebuild the in-memory CSR snapshot for physical plan synchronization
    graph.rebuild_csr()?;

    // Execute a Cypher query
    let result = graph.query(
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since"
    )?;

    for record in result.records {
        println!(
            "Match: {} knows {} since {}",
            record.values[0],
            record.values[1],
            record.values[2]
        );
    }

    Ok(())
}
```

---

### Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for details on how to make a contribution.

### License

IssunDB is available under either of these licenses:

* MIT License ([LICENSE-MIT](LICENSE-MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

### Acknowledgements

* The logo is from [SVG Repo](https://www.svgrepo.com/svg/451006/knowledge-graph) with some modifications.
