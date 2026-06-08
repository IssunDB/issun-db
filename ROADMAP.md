## Project Roadmap

This document outlines the features implemented in IssunDB and the future goals for the project.

> [!IMPORTANT]
> This roadmap is a work in progress and is subject to change.

---

### Core Database Engine

- [x] On-disk key-value storage engine using LMDB
- [x] Zero-copy binary adjacency serialization with memory-mapped layouts
- [x] Monotonic identifier allocation and label and type registries
- [x] Flexible property storage with MessagePack serialization
- [x] Thread-safe write serialization and lock-free concurrent reads
- [x] Unique and required property constraints on labels or types
- [x] Multi-step database transactions with atomic commits and rollbacks
- [x] Native full-text index database storage for terms, postings, and tokenizer configurations
- [x] Strongly-typed and structured error handling for all crates
- [x] Semi-columnar auto-indexing

---

### GraphBLAS-based Graph Analytics

- [x] Thread-safe in-memory CSR snapshot cache
- [x] Built-in GraphBLAS binding (`issundb-graphblas` safe wrapper over `issundb-graphblas-sys`)
- [x] Dynamic, zero-overhead GraphBLAS matrix materialization triggered by database writes
- [x] Incremental (delta) maintenance of the adjacency matrices
- [x] Configurable OpenMP multi-threading for GraphBLAS operations
- [x] GraphBLAS-accelerated graph algorithms:
    - [x] Single and multi-source BFS
    - [x] PageRank via power iterations
    - [x] Weighted shortest path using Dijkstra's algorithm
    - [x] Weakly connected components
    - [x] Strongly connected components
    - [x] Degree, betweenness, and harmonic centrality measures
    - [x] Label-propagation community detection
    - [x] Minimum and maximum spanning forests
    - [x] Maximum flow
    - [x] Top-k path search
    - [x] Longest path, cycle detection, and general DFS/all-paths traversals

---

### Text and Vector Search

- [x] HNSW vector index integration
- [x] Vector database APIs for dense embedding search and dynamic index rebuilds
- [x] Full-text indexing with ranked matches, BM25 scoring, and multi-language stemming
- [x] Vector deletion API and persisted dimension/metric metadata
- [x] Property-filtered vector search constraints
- [x] Hybrid retrieval that combines vector search, full-text search, and graph queries
- [x] Retrieval score fusion, attribution scoring, and result limiters
- [x] Concurrent multi-threaded vector search queries

---

### Cypher Query Language

- [x] A recursive-descent Cypher parser for read, write, and schema manipulation patterns
- [x] Parameter binding and projection support
- [x] Cost-based logical query planner with label scanning, expansion, and filtering
- [x] Physical planner and optimization engine featuring filter pushdown and operator reordering
- [x] Unconditional (GraphBLAS-accelerated) Cypher pattern matching using vector-matrix multiplication
- [x] Variable-length path patterns, collection unwinding, and projection barriers
- [x] Result shaping with order, skip, limit, and aggregation functions
- [x] Idempotent writes using the `MERGE` clause
- [x] `OPTIONAL MATCH` for outer-join pattern matching
- [x] Multi-label nodes: `CREATE`, `MATCH`, and `SET` over patterns such as `(n:A:B)`
- [x] `DELETE` and `DETACH DELETE` over arbitrary expression targets
- [x] Cypher DDL for administrative index and constraint creation
- [x] Query plan visualization for logical, physical, and optimized query paths
- [x] OpenCypher TCK submodule integration and `make test-conformance` target
- [x] Inline relationship property map filter pushdown: e.g. `-[:KNOWS {since: 2026}]->`
- [x] Worst-case optimal join (`MultiwayJoin`) for closing hops in cyclic patterns (triangles and cliques)
- [x] Factorized filter-over-expand execution
- [x] Scan-node selection
- [x] Count reduction
- [x] Primary-key seek
- [x] Fused linear expand chains
- [x] Static filter elimination
- [x] Lazy named-path materialization
- [ ] Full openCypher TCK conformance: as of 2026-06-07, 3,443 of 3,469 executed scenarios pass (99.3%; a further 428 scenarios are
  skipped as intentional exclusions, such as negative-test tags and node, relationship, or path display-literal representational mismatches).

---

### Ecosystem and Tooling

- [x] An interactive REPL
- [x] An HTTP REST API server
- [x] An MCP server over stdio or Streamable HTTP
- [x] A container image bundling the CLI, REST, and MCP binaries
- [x] A benchmarking suite that measures throughput and load scaling:
    - [x] LSQB query patterns Q1–Q9
    - [x] OLTP transactional read query benchmark modeled after the LDBC Interactive Short query patterns
    - [x] Write throughput benchmark evaluating single and batched node/edge insertion performance
    - [x] GraphRAG hybrid retrieval benchmark
- [x] A differential comparison harness against LadybugDB (`benchmarks/ladybugdb-compare`)
- [x] Property-based and integration tests
- [x] Language bindings for Python
- [x] Batch data import utilities for JSONL, CSV, and Parquet formats
- [x] Database export and import functionality (`EXPORT DATABASE` and `IMPORT DATABASE` Cypher queries) with CSV, JSONL, and Parquet formats
- [x] Online backup, restore, and snapshot tools
