# IssunDB

IssunDB is an embedded graph database in Rust.

## Key Features

* ACID transactions, property graph model, and Cypher query support
* Fast graph traversal and analytics
* Vectorized query execution
* Vector, full-text, and hybrid search
* APIs for Rust, Python, CLI, HTTP (REST), and MCP
* Support for Linux, macOS, Windows, and WebAssembly

## Architecture Overview

The database is designed as a set of modular crates, establishing clear boundaries between storage, queries, and indexes:

| Crate               | Purpose                                                                     |
|---------------------|-----------------------------------------------------------------------------|
| `issundb-core`      | Storage engine, schema types, configurations, and property columns.         |
| `issundb-vector`    | Vector embedding storage, search indexing, and quantization configurations. |
| `issundb-text`      | BM25 scoring and the text query APIs.                                       |
| `issundb-retrieval` | Multi-source hybrid retrieval, rank fusion, and graph traversal.            |
| `issundb-cypher`    | Cypher query parser, AST definitions, planners, and executors.              |
| `issundb`           | The primary library crate providing a unified public API.                   |
| `issundb-cli`       | An interactive CLI for IssunDB.                                             |
| `issundb-rest`      | An HTTP server that exposes IssunDB over a REST API.                        |
| `issundb-mcp`       | MCP server implementation for IssunDB.                                      |
| `issundb-py`        | Python bindings for IssunDB.                                                |
| `issundb-wasm`      | Browser bindings, and the crate the IssunDB playground app is built from.   |

<p align="center">
  <img src="assets/diagrams/architecture.svg" alt="IssunDB Architecture" />
</p>

## Documentation Sections

- [Getting Started](getting-started.md): Installation, build instructions, basic CLI usage, and usage in Rust projects.
- [Playground](https://issundb.github.io/issun-db/playground/): The whole engine compiled to WebAssembly, running in a browser.
- [Code Examples](examples.md): Practical code examples for vector search, text search, and Cypher query execution.
- [Cypher Support](cypher.md): The supported Cypher clauses, patterns, expressions, and functions, plus known deviations.
- [API Reference](api-reference.md): Public Rust API reference, types, and Cypher DDL syntax.
- [Hybrid Retrieval](hybrid-retrieval.md): Concept overview and implementation guide for GraphRAG pipelines.
- [Integrations](integrations.md): Exposing IssunDB over HTTP (REST) and MCP.
- [Python Integration](python.md): Working with IssunDB directly from Python.
