<div align="center">
  <picture>
    <img alt="IssunDB Logo" src="docs/assets/logo.svg" height="25%" width="25%">
  </picture>
<br>

<h2>IssunDB</h2>

[![Tests](https://img.shields.io/github/actions/workflow/status/IssunDB/issun-db/tests.yml?label=tests&style=flat&labelColor=282c34&logo=github)](https://github.com/IssunDB/issun-db/actions/workflows/tests.yml)
[![Code Coverage](https://img.shields.io/codecov/c/github/IssunDB/issun-db?label=coverage&style=flat&labelColor=282c34&logo=codecov)](https://codecov.io/gh/IssunDB/issun-db)
[![Crates.io](https://img.shields.io/crates/v/issun-db.svg?label=crates.io&style=flat&labelColor=282c34&color=fc8d62&logo=rust)](https://crates.io/crates/issun-db)
[![Docs.rs](https://img.shields.io/badge/docs-issun-db-66c2a5?style=flat&labelColor=282c34&logo=docs.rs)](https://docs.rs/issun-db)
[![Docs](https://img.shields.io/badge/docs-read-007ec6?label=docs&style=flat&labelColor=282c34&logo=readthedocs)](https://IssunDB.github.io/issun-db/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-007ec6?style=flat&labelColor=282c34&logo=open-source-initiative)](https://github.com/IssunDB/issun-db)

A fast embedded graph database for AI applications and graph analytics

</div>

---

IssunDB is a fast, embedded graph database writen in Rust.
It can be embedded in Rust applications without the need for a server, cand can be used for wide range of applications like building building
GraphRAG pipleines and querying knowledge graphs.

### Features

* Rust graph engine built on [LMDB](https://www.symas.com/mdb) with ACID, property graph model, and Cypher query language support
* Fast graph traversal and analytics using [GraphBLAS](https://graphblas.org)-based sparse matrix operations
* Buit-in vector, text, and hybrid search and retrieval support
* Can be used via a wide range of APIs, including native Rust, CLI, GUI, HTTP, and MCP
* Fully cross-platform; supports Linux, macOS, and Windows
* Bindings for Python and JavaScript (other languages coming soon)

See [ROADMAP.md](ROADMAP.md) for the full list of implemented and planned features.

> [!IMPORTANT]
> This project is still in early development, so bugs and breaking changes are expected.
> Please use the [issue page](https://github.com/IssunDB/issun-db/issues) to report bugs or request features.

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
    // Open a graph database with a 1 GB memory map size limit
    let graph = Graph::open(Path::new("./issundb-data"), 1)?;

    // Add nodes with properties
    let alice_props = serde_json::json!({ "name": "Alice", "age": 30 });
    let alice_id = graph.add_node("Person", &alice_props)?;

    let bob_props = serde_json::json!({ "name": "Bob", "age": 28 });
    let bob_id = graph.add_node("Person", &bob_props)?;

    // Create an edge connecting the nodes
    let edge_props = serde_json::json!({ "since": 2021 });
    graph.add_edge(alice_id, bob_id, "KNOWS", &edge_props)?;

    // Rebuild the in-memory CSR snapshot
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

### Documentation

The project documentation is available [here](https://IssunDB.github.io/issun-db/).

---

### Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for details on how to make a contribution.

### License

IssunDB is available under either of these licenses:

* MIT License ([LICENSE-MIT](LICENSE-MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

### Acknowledgements

* The logo is from [SVG Repo](https://www.svgrepo.com/svg/451006/knowledge-graph) with some modifications.
