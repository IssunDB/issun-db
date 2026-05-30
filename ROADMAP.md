## Project Roadmap

This document outlines the features implemented in IssunDB and the future goals for the project.

> [!IMPORTANT]
> This roadmap is a work in progress and is subject to change.

---

### Core Database Engine and Storage

- [x] On-disk key-value storage engine using LMDB
- [x] Zero-copy binary adjacency serialization with memory-mapped layouts
- [x] Monotonic identifier allocation and label/type registries
- [x] Flexible property storage with MessagePack serialization
- [x] Thread-safe write serialization and lock-free concurrent reads
- [x] Unique and required property constraints on labels or types
- [x] Multi-step database transactions with atomic commits and rollbacks
- [x] Native full-text index database storage for terms, postings, and tokenizer configurations
- [x] Semi-columnar auto-indexing
- [x] Strongly-typed, structured error handling enums across all sub-crates (core, cypher, vector, text, and retrieval) to prevent leaky abstractions
  and eliminate untyped string-based errors

---

### Unified GraphBLAS Analytics

- [x] Thread-safe in-memory CSR snapshot cache
- [x] Dynamic, zero-overhead GraphBLAS matrix materialization triggered by database writes
- [x] Threshold-gated OpenMP multi-threading (graphs with more than 100k edges use all available CPU cores)
- [x] SuiteSparse:GraphBLAS algorithm suite executing via sparse matrix-vector multiplication kernels:
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

### Advanced Retrieval and Vector Search

- [x] HNSW vector index integration
- [x] Vector database APIs for dense embedding search and dynamic index rebuilds
- [x] High-speed full-text indexing with ranked matches, BM25 scoring, and multi-language stemming
- [x] Vector deletion API and persisted dimension/metric metadata
- [x] Property-filtered vector search constraints
- [x] Hybrid retrieval that combines vector search, full-text search, and graph queries
- [x] Retrieval score fusion, attribution scoring, and result limiters

---

### Cypher Query Language

- [x] A recursive-descent Cypher parser for read, write, and schema manipulation patterns
- [x] Parameter binding and projection support
- [x] Cost-based logical query planner with label scanning, expansion, and filtering
- [x] Physical planner and optimization engine featuring filter pushdown and operator reordering
- [x] Unconditional (GraphBLAS-accelerated) Cypher pattern matching using vector-matrix multiplication
- [x] Variable-length path patterns, collection unwinding, and projection barriers
- [x] Result shaping with order, skip, limit, and aggregation functions
- [x] Idempotent writes using the `MERGE` clause, including relationship and bound-node binding carried into following clauses, `ON CREATE`
  and `ON MATCH` actions in either order, and fan-out to one row per matched pattern
- [x] `OPTIONAL MATCH` for outer-join pattern matching
- [x] Multi-label nodes: `CREATE`, `MATCH`, and `SET` over patterns such as `(n:A:B)`, with one label-index entry per label
- [x] `SET` and `REMOVE` for node labels in addition to node and relationship properties
- [x] `DELETE` and `DETACH DELETE` over arbitrary expression targets, not just bare variables, evaluated over the whole result so relationships
  are removed before nodes (with a storage-truth connected-node guard) and compile-time rejection of non-graph delete targets
- [x] Cypher DDL for administrative index and constraint creation
- [x] Query plan visualization for logical, physical, and optimized query paths
- [x] OpenCypher TCK submodule integration and `make test-conformance` target
- [x] Inline relationship property map filter pushdown: e.g. `-[:KNOWS {since: 2026}]->`
- [x] Worst-case optimal join (`MultiwayJoin`) for closing hops in cyclic patterns (triangles and cliques)
- [x] Factorized filter-over-expand execution
- [x] Scan-node selection
- [x] Count reduction: `count(*)` or `count(n)` over a bare labeled scan is replaced with a constant read from label metadata, avoiding a full scan
- [x] Primary-key seek: `WHERE id(n) = <const>` over a node scan is rewritten to a `NodeByIdSeek` that fetches one node directly instead of scanning
  the label
- [x] Fused linear expand chains: a contiguous run of single-hop directed expands is executed as one operation that bulk-expands each hop level and
  clones the base path once per output row, generalizing the former two-hop fast path to any length
- [x] Static filter elimination: provably-true predicates (`WHERE true`, equality or inequality of identical-form literals) are dropped before
  pushdown so they are never evaluated per row
- [ ] Full openCypher TCK conformance: as of 2026-05-31, 3,333 of 3,490 executed scenarios pass (95.50%; a further 407 scenarios are
  skipped as intentional exclusions, such as negative-test tags and node or relationship display-literal representational mismatches). Notable
  remaining capability gaps:
    - [x] Temporal expression conformance: timezone resolution for named and historical zones (DST and local-mean-time offsets via the IANA
      database), storage of temporal values as node properties, duration parsing from ISO strings (including the extended date format),
      `datetime.fromepoch` construction, duration component accessors, statement-clock current-time functions, and extreme-year (±999999999)
      date arithmetic via civil day counting
    - [x] Standard list functions: `filter()`, `extract()`, and `reduce()` list functions
    - [x] Path and graph introspection: `nodes()`, `relationships()`, `length()`, `startNode()`, and `endNode()` introspection functions, including
      compile-time type validation of `length()`
    - [x] Standard math and string scalar functions: `exists()`, `left()`, `right()`, `degrees()`, `radians()`, `haversin()`, and `timestamp()` scalar
      functions
    - [ ] `CALL` and procedure invocation
    - [ ] Pattern comprehension and list comprehension subqueries
    - [x] Aggregation expressions inside `ORDER BY`
    - [x] Three-valued null comparison logic
    - [x] Intermediate orderings and path variable bindings in `WITH` / `ORDER BY`

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
- [x] Language bindings for JavaScript (Node.js)
- [x] Batch data import utilities for JSONL and CSV formats
- [x] Online backup, restore, and snapshot tools
