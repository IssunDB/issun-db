# IssunDB

IssunDB is a fast embedded graph database, written in Rust.
It can be embedded in Rust applications without the need for a server, and can be used for a wide range of applications such as building
GraphRAG pipelines and querying knowledge graphs.

## Key Features

Key features of IssunDB:

* **ACID Transactions**: A graph engine with full support for transactional safety and declarative Cypher queries.
* **Fast Graph Analytics**: High-performance traversals and graph algorithms using sparse matrix operations.
* **Vectorized Execution**: Multi-core parallel execution and serializable transactions.
* **Multi-Index Retrieval**: Hybrid search combining vector search, full-text keyword ranking, and neighborhood expansion.
* **Rich Developer Tooling**: Native Rust APIs, Python bindings, interactive CLI, HTTP REST server, and MCP.
* **Cross-Platform Support**: Compatibility with Linux, macOS, and Windows.

## Architecture Overview

The database is designed as a set of modular crates, establishing clear boundaries between storage, queries, and indexes:

| Crate               | Purpose                                                                     |
|---------------------|-----------------------------------------------------------------------------|
| `issundb-core`      | Storage engine, schema types, configurations, and property columns.         |
| `issundb-vector`    | Vector embedding storage, search indexing, and quantization configurations. |
| `issundb-text`      | Tokenizer implementation, inverted indexes, and BM25 text search scoring.   |
| `issundb-retrieval` | Multi-source hybrid retrieval, rank fusion, and graph traversal.            |
| `issundb-cypher`    | Cypher query parser, AST definitions, planners, and executors.              |
| `issundb`           | The primary library crate providing a unified public API.                   |
| `issundb-cli`       | An interactive CLI for IssunDB.                                             |
| `issundb-rest`      | An HTTP server that exposes IssunDB's functionalities over REST API.        |
| `issundb-mcp`       | MCP server implementation for IssunDB.                                      |
| `issundb-py`        | Python bindings for IssunDB.                                                |

<p align="center">
  <img src="assets/diagrams/architecture.svg" alt="IssunDB Architecture" />
</p>

## Documentation Sections

- [Getting Started](getting-started.md): Installation, build instructions, basic CLI usage, and usage in Rust projects.
- [Code Examples](examples.md): Practical code examples for vector search, text search, and Cypher query execution.
- [API Reference](api-reference.md): Public Rust API reference, types, and Cypher DDL syntax.
- [Hybrid Retrieval](hybrid-retrieval.md): Concept overview and implementation guide for GraphRAG pipelines.
- [Integrations](integrations.md): Exposing IssunDB over HTTP REST and the MCP.
- [Python Integration](python.md): Working with IssunDB directly from Python.
