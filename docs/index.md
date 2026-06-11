# IssunDB

<p align="center">
  <img src="assets/logo.svg" alt="Project Logo" width="250" />
</p>

---

IssunDB is an embedded graph database with vector and full-text search capabilities, written in Rust.

## Core Features

- **ACID Transactions**: Leverages LMDB for fully transaction-safe graph storage, updates, and indexing.
- **Cypher Query Language**: Supports a recursive-descent parser, physical planner, and optimized row-at-a-time or vectorized execution of Cypher queries.
- **Vector Search Integration**: Supports $k$-nearest-neighbor vector search over embeddings using `usearch` index configurations.
- **Full-Text Search**: Configurable tokenization and ranking using BM25-style scoring for node and relationship properties.
- **Hybrid Retrieval**: Combines vector scores, full-text ranks, and multi-source graph expansion to extract local context subgraphs for GraphRAG.
- **Extensible Interfaces**: Exposes public REST endpoints, Python bindings, and Model Context Protocol (MCP) server capabilities.

## Architecture Overview

The database is built as a set of modular crates:

| Crate | Purpose |
|---|---|
| `issundb-core` | Storage engine, schema types, LMDB database configurations, and property columns. |
| `issundb-vector` | Vector embedding storage, search indexing, and quantization configurations. |
| `issundb-text` | Tokenizer implementation, inverted indexes, and BM25 text search scoring. |
| `issundb-retrieval` | Multi-source hybrid retrieval, rank fusion, and GraphBLAS traversal. |
| `issundb-cypher` | Cypher query parser, AST definitions, planners, and executors. |
| `issundb` | Public facade re-exporting the unified database interface. |
| `issundb-cli` | Interactive REPL command line interface. |
| `issundb-rest` | Axum-based HTTP REST API server with live Scalar documentation. |
| `issundb-mcp` | Model Context Protocol implementation supporting stdio and HTTP transports. |
| `issundb-py` | Python bindings for IssunDB using PyO3. |

<p align="center">
  <img src="assets/diagrams/architecture.svg" alt="IssunDB Architecture" />
</p>

## Documentation Sections

- [Getting Started](getting-started.md): Installation, build instructions, basic CLI usage, and embedding in Rust projects.
- [Code Examples](examples.md): Practical code examples for vector search, text search, and Cypher execution.
- [API Reference](api-reference.md): Public Rust API reference, types, and Cypher DDL syntax.
- [Hybrid Retrieval](hybrid-retrieval.md): Concept overview and implementation guide for GraphRAG pipelines.
- [Integrations](integrations.md): Exposing IssunDB over HTTP REST and the Model Context Protocol.
