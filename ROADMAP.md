## Project Roadmap

This document outlines the features implemented in IssunDB and the future goals for the project.

> [!IMPORTANT]
> This roadmap is a work in progress and is subject to change.

---

### Core Database Engine and Storage

- [x] On-disk key-value storage engine using Lightning Memory-Mapped Database (LMDB)
- [x] Zero-copy binary adjacency serialization with memory-mapped layouts
- [x] Monotonic identifier allocation and label/type registries
- [x] Flexible property storage with messagepack serialization
- [x] Thread-safe write serialization and lock-free concurrent reads
- [x] Unique and required property constraints on labels or types
- [x] Multi-step database transactions with atomic commits and rollbacks
- [x] Native full-text index database storage for terms, postings, and tokenizer configurations

---

### Unified GraphBLAS Analytics

- [x] Thread-safe in-memory Compressed Sparse Row (CSR) snapshot cache
- [x] Dynamic, zero-overhead GraphBLAS matrix materialization triggered by database writes
- [x] SuiteSparse:GraphBLAS algorithm suite executing via sparse matrix-vector multiplication (SpMV) kernels:
    - [x] Breadth-first search (BFS) and multi-source BFS
    - [x] Directed PageRank power iterations
    - [x] Weighted shortest path using Dijkstra on a MinPlus semiring
    - [x] Weakly connected components (WCC) label propagation
    - [x] Strongly connected components (Kosaraju's algorithm)
    - [x] Degree, betweenness, and harmonic centrality measures
    - [x] Label-propagation community detection (CDLP)
    - [x] Minimum and maximum spanning forests
    - [x] Edmonds-Karp maximum flow
    - [x] Yen's top-k path search
    - [x] Longest path, cycle detection, and general DFS/all-paths traversals

---

### Advanced Retrieval and Vector Search

- [x] Hierarchical Navigable Small World (HNSW) vector index integration using `usearch`
- [x] Vector database APIs for dense embedding search and dynamic index rebuilds
- [x] High-speed full-text indexing with ranked matches, BM25 scoring, and multi-language stemming
- [x] Vector deletion API and persisted dimension/metric metadata
- [x] Property-filtered vector search constraints
- [x] Hybrid retrieval combining vector search, full-text search, and GraphBLAS graph expansions
- [x] Retrieval score fusion, attribution scoring, and result limiters

---

### Cypher Query Language and Planner

- [x] Hand-written recursive-descent Cypher parser for read, write, and schema manipulation patterns
- [x] Parameter binding and projection support
- [x] Cost-based logical query planner with label scanning, expansion, and filtering
- [x] Physical planner and optimization engine featuring filter pushdown and operator reordering
- [x] Unconditional GraphBLAS-accelerated Cypher pattern matching using vector-matrix multiplication
- [x] Variable-length path patterns, collection unwinding, and projection barriers
- [x] Result shaping with order, skip, limit, and aggregation functions
- [x] Idempotent writes using the `MERGE` clause
- [x] `OPTIONAL MATCH` for outer-join pattern matching
- [x] Cypher DDL for administrative index and constraint creation
- [x] Query plan visualization for logical, physical, and optimized query paths
- [x] openCypher TCK submodule integration and `make test-conformance` target
- [x] Inline relationship property map filter pushdown: e.g. `-[:KNOWS {since: 2026}]->`

---

### Ecosystem and Tooling

- [x] An interactive REPL
- [x] An HTTP REST API server (Axum) with node, edge, query, vector search, and full-text search routes
- [x] A desktop GUI (egui) with a Cypher console and interactive graph visualization
- [x] A benchmarking suite that measures throughput and load scaling
- [x] Property-based and integration tests
- [x] Shared test fixture library (`issundb-testing`) with graph builders and assertion helpers
- [x] Language bindings for Python (using PyO3)
- [x] Language bindings for Node.js (using NAPI-RS)
- [x] Batch data import utilities for JSONL and CSV formats
- [x] Online backup, restore, and snapshot tools
