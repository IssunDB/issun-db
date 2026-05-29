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
- [x] Semi-columnar auto-indexing: all scalar node properties are automatically written to `node_prop_idx` on every insert and update, enabling `NodeIndexScan` for any equality predicate without a prior `CREATE INDEX`

---

### Unified GraphBLAS Analytics

- [x] Thread-safe in-memory Compressed Sparse Row (CSR) snapshot cache
- [x] Dynamic, zero-overhead GraphBLAS matrix materialization triggered by database writes
- [x] Threshold-gated OpenMP multi-threading: graphs with more than 100k edges use all available CPU cores for SpMV; smaller graphs run single-threaded to avoid scheduling overhead
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

- [x] Hierarchical Navigable Small World (HNSW) vector index integration
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
- [x] Worst-case optimal join (`MultiwayJoin`) for closing hops in cyclic patterns (triangles, cliques): optimizer detects already-bound `dst_var` and rewrites to O(1) hash-map lookup per row
- [x] Factorized Filter-over-Expand execution: source-predicate filters are evaluated once per source node; destinations of rejected sources are skipped with zero PathMap clones
- [x] Scan-node selection: the optimizer reverses a linear single-hop Expand chain to start traversal from its lowest-cardinality or index-backed endpoint, flipping each hop's direction so no executor change is needed
- [x] Count reduction: `count(*)` or `count(n)` over a bare labeled scan is replaced with a constant read from label metadata, avoiding a full scan
- [x] Primary-key seek: `WHERE id(n) = <const>` over a node scan is rewritten to a `NodeByIdSeek` that fetches one node directly instead of scanning the label
- [x] Fused linear expand chains: a contiguous run of single-hop directed expands is executed as one operation that bulk-expands each hop level and clones the base path once per output row, generalizing the former two-hop fast path to any length
- [ ] Full openCypher TCK conformance: as of 2026-05-29, about 75% of executed scenarios pass (roughly 2,480 of 3,300; a further 597 scenarios are skipped as intentional exclusions, such as negative-test tags and node or relationship display-literal representational mismatches). Notable remaining capability gaps:
    - [ ] Temporal timezone resolution for named and historical zones, and duration-between computation
    - [ ] `CALL` and procedure invocation
    - [ ] Aggregation expressions inside `ORDER BY`
    - [ ] Three-valued null comparison logic

---

### Ecosystem and Tooling

- [x] An interactive REPL
- [x] An HTTP REST API server with node, edge, query, vector search, and full-text search routes
- [x] An MCP server over stdio or Streamable HTTP, exposing node and edge CRUD, query, explanation, full-text search, and vector search as tools
- [x] A desktop GUI with a Cypher console and interactive graph visualization
- [x] A benchmarking suite that measures throughput and load scaling
- [x] Property-based and integration tests
- [x] Shared test fixture library (`issundb-testing`) with graph builders and assertion helpers
- [x] Language bindings for Python
- [x] Language bindings for Node.js
- [x] Batch data import utilities for JSONL and CSV formats
- [x] Online backup, restore, and snapshot tools
