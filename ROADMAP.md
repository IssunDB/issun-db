## Project Roadmap

This document outlines the features implemented in IssunDB and the future goals for the project.

> [!IMPORTANT]
> This roadmap is a work in progress and is subject to change.

### Product Scope

IssunDB is an embedded graph database with vector and full-text search, written in Rust.
The roadmap prioritizes local storage correctness, graph traversal, vector retrieval, full-text retrieval, and native embedded APIs before
server-oriented features.

### Target Crate Architecture

- [x] Workspace with `issundb-core`, `issundb-vector`, `issundb-text`, `issundb-retrieval`, `issundb-cypher`, `issundb-rag`, `issundb`, and `issundb-cli`
- [x] `issundb-vector` crate for vector index abstractions, vector metadata, vector storage integration, and vector search APIs
- [x] `issundb-text` crate scaffold for tokenization, inverted index storage, ranked full-text search, and text search APIs
- [x] `issundb-retrieval` crate for hybrid retrieval over graph traversal, vector hits, text hits, property filters, score fusion, and subgraph
  materialization
- [x] Transitional `issundb-rag` compatibility shim that re-exports `issundb-retrieval` APIs
- [x] `issundb-cypher` integration through public core APIs only
- [x] `issundb-cli` use of only the `issundb` public facade

### Storage Engine

- [x] Shared `[workspace.dependencies]` with all version pins in the root `Cargo.toml`
- [x] Schema types: `NodeId`, `EdgeId`, `LabelId`, `TypeId`, `AdjEntry`, `NodeRecord`, and `EdgeRecord`
- [x] `AdjEntry` as a fixed 20-byte `#[repr(C, packed)]` struct with `zerocopy` derives
- [x] LMDB environment via `heed`; eight sub-databases: `nodes`, `edges`, `out_adj`, `in_adj`, `label_idx`, `type_idx`, `vectors`, and `meta`
- [x] Monotonic node and edge ID allocation persisted in the `meta` sub-database
- [x] String-to-integer label and edge-type registries persisted in the `meta` sub-database
- [x] msgpack property encoding and decoding via `rmp-serde`
- [x] Write serialization via `parking_lot::Mutex`
- [x] `out_adj` and `in_adj` as LMDB `DUPSORT + DUPFIXED` databases: one raw `AdjEntry` per duplicate value, O(log n) per edge insert
- [x] Label secondary index: composite key `(LabelId, NodeId)` for prefix-range scans
- [x] Edge-type secondary index: composite key `(TypeId, EdgeId)` for prefix-range scans
- [x] Criterion benchmark suite: `node_insert`, `edge_insert`, and `out_neighbors` throughput groups; `load_test` group (1 million nodes, 5 million
  edges) with `sample_size(10)`, run via `ISSUNDB_LOAD_TEST=1 cargo bench -p issundb-core -- load_test`
- [ ] User-facing transaction API for multi-step writes with atomic commit and rollback
- [ ] Full-text index storage for terms, postings, document metadata, and tokenizer configuration

### Indexing and Constraints

- [ ] Property secondary indexes for node and edge properties
- [ ] Planner integration for property index scans
- [ ] Unique-property constraints for labels and edge types
- [ ] Required-property constraints for labels and edge types

### Graph API

- [x] `Graph::add_node`, `get_node`, `update_node`, `delete_node`, `add_edge`, `get_edge`, `out_neighbors`, and `in_neighbors`
- [x] `Graph::nodes_by_label(label: &str) -> Result<Vec<NodeId>, Error>`
- [x] `Graph::edges_by_type(etype: &str) -> Result<Vec<EdgeId>, Error>`
- [x] `Graph::label_name(id: LabelId) -> Result<Option<String>, Error>`
- [x] `Graph::type_name(id: TypeId) -> Result<Option<String>, Error>`
- [x] `Graph::bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- [x] `Graph::shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- [x] `Graph::page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`
- [x] `Graph::connected_components() -> Result<HashMap<NodeId, u64>, Error>`
- [x] `Graph::all_nodes() -> Result<Vec<NodeId>, Error>`
- [ ] DFS traversal
- [ ] All-neighbors traversal helper
- [ ] All paths between two nodes
- [ ] All shortest paths between two nodes
- [ ] Weighted shortest path with Dijkstra
- [ ] Single-pair weighted path search
- [ ] Single-source weighted path search
- [ ] Top-k path search
- [ ] Cycle detection
- [ ] Longest path search
- [ ] Strongly connected components
- [ ] Betweenness centrality
- [ ] Harmonic closeness centrality
- [ ] Degree centrality
- [ ] Label-propagation community detection
- [ ] Minimum and maximum spanning forest
- [ ] Maximum flow

### CSR Cache and GraphBLAS

- [x] `CsrSnapshot`: `row_ptr`, `col_idx`, `edge_type`, `edge_id`, `dense_to_id`, and `id_to_dense` fields
- [x] `CsrCache` backed by `arc-swap`; rebuilt from LMDB in a background thread when the dirty count exceeds a threshold
- [x] `Graph::out_neighbors` hot path reading from the CSR snapshot with LMDB fallback for nodes absent from the current snapshot
- [x] Optional GraphBLAS context and per-type boolean matrix materialization from the CSR snapshot; combined integer adjacency and column-stochastic
  float
  matrix also materialized in `MatrixSet`
- [x] Optional GraphBLAS-backed BFS (`bfs_graphblas`), PageRank (`page_rank_graphblas`), and SSSP (`shortest_path_graphblas`) via MinPlus and
  PlusTimes
  SpMV behind the `graphblas` feature
- [x] Microbenchmark: CSR BFS vs raw LMDB cursor

### Vector Index

- [x] `usearch` HNSW index wrapper in `crates/issundb-vector/src/index.rs`
- [x] Vector index ownership moved from `issundb-core` to `issundb-vector`
- [x] `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), Error>`
- [x] `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, Error>`
- [x] Search-time index rebuild from persisted `vectors` records through `issundb-vector`
- [ ] Vector delete API for removed embeddings
- [ ] Persisted vector dimension, metric, and index metadata
- [ ] Filtered vector search by label, type, property predicate, or candidate set
- [ ] Batch vector upsert and rebuild APIs for bulk ingestion

### Full-Text Search

- [x] `issundb-text` crate scaffold and public extension trait
- [ ] Full-text indexes for selected string properties
- [ ] Full-text search API for ranked text matches
- [ ] Cypher integration for full-text predicates or procedures
- [ ] Hybrid full-text and vector candidate fusion

### Hybrid Retrieval

- [x] `Subgraph` type: `nodes`, `edges`, and `scores` fields
- [x] `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, Error>`
- [x] `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, Error>`
- [x] k-hop BFS expansion using `Graph::out_neighbors`; optional GraphBLAS SpMV expansion via `retrieve_graphblas`
- [x] Subgraph materialization from LMDB
- [ ] Hybrid retrieval that combines vector search, full-text search, property filters, and graph expansion
- [x] `issundb-retrieval` crate implementation
- [ ] Public retrieval API names that emphasize vector, full-text, and graph retrieval
- [ ] Score fusion and source attribution for retrieval results
- [ ] Retrieval limits for nodes, edges, distances, and per-source candidates

### Cypher Query Language

- [x] AST node types in `crates/issundb-cypher/src/ast.rs`
- [x] Hand-written recursive-descent parser for node patterns, relationship patterns, WHERE predicates, and RETURN projections
- [x] Parameter binding (`$param` syntax)
- [x] CREATE, SET, and DELETE execution
- [x] `GraphQueryExt` trait on `Graph` exposing `query(cypher)` and `query_with_params(cypher, params) -> Result<QueryResult, String>` via the
  `issundb`
  facade
- [x] Logical planner: `LabelScan`, `Expand`, `LabelFilter`, and `Project` operators
- [x] Physical planner and optimizer: filter pushdown and operator reordering
- [x] Expand and label filtering use GraphBLAS SpMV and element-wise AND when the `graphblas` feature is enabled; default builds use cursor and set
  intersection fallbacks
- [x] Variable-length path patterns (`[:REL*1..3]`)
- [x] `WITH` and `UNWIND` clauses
- [x] Selectivity estimates from LMDB sub-database statistics
- [x] openCypher TCK subset in `crates/issundb/tests/conformance/`, gated on `ISSUNDB_CONFORMANCE=1`
- [ ] `MERGE` support for idempotent node and relationship writes
- [ ] `ORDER BY`, `LIMIT`, and `SKIP` support for result shaping
- [ ] Aggregation functions: `COUNT`, `collect`, `sum`, `avg`, `min`, and `max`
- [ ] Expanded `WHERE` expressions: `AND`, `OR`, `IN`, comparisons, and `IS NULL`
- [ ] Property pattern matching: `MATCH (n:Label {key: $value})`
- [ ] `OPTIONAL MATCH` support
- [ ] Cypher DDL for `CREATE INDEX` and `CREATE CONSTRAINT`
- [ ] Path return values for matched and variable-length relationships
- [ ] `EXPLAIN` output for logical, physical, and optimized query plans
- [ ] Broader openCypher compatibility after the practical embedded database subset is complete

### Language Bindings

- [ ] `pyo3` bindings in `crates/issundb-py`: `Db`, `Graph`, `Subgraph`, and `Hit` classes
- [ ] Python bindings use only the `issundb` public facade
- [ ] `maturin` wheel build and CI publishing to TestPyPI
- [ ] Cross-platform wheel matrix via `cibuildwheel` (Linux x86-64, macOS arm64, and Windows x86-64)
- [ ] `napi-rs` bindings in `crates/issundb-node`: `Db` and `Graph` classes
- [ ] Node.js bindings use only the `issundb` public facade
- [ ] Auto-generated TypeScript type definitions
- [ ] Prebuilt Node.js binary matrix (Linux x86-64, macOS arm64, and Windows x86-64)

### Testing and Tooling

- [x] Unit tests: node insert and read, edge insert and read, adjacency consistency, and ID uniqueness
- [x] `proptest` property tests for ID allocation invariants and adjacency round-trips
- [x] Integration tests driving the `issundb` public facade
- [x] Criterion storage benchmark suite with node insert, edge insert, adjacency read, CSR BFS, LMDB BFS, and gated load-test groups
- [ ] Benchmark against Cozo on the LDBC SNB subset
- [x] `issundb-cli` interactive REPL

### Import, Export, and Operations

- [ ] JSONL import and export for nodes, edges, properties, and vectors
- [ ] CSV import for node and edge batches
- [ ] Backup and restore APIs for LMDB-backed databases

### Distribution

- [ ] `README.md` with quickstart, API overview, and build instructions
- [ ] Rustdoc on the `issundb` public facade published to docs.rs
- [ ] Examples: `hybrid_retrieval_quickstart.rs`, `neo4j_migration.rs`, and `load_ldbc.rs`
- [ ] `issundb`, `issundb-core`, `issundb-vector`, `issundb-text`, `issundb-retrieval`, and `issundb-cypher` published to crates.io
- [ ] GitHub release with changelog and prebuilt CLI binaries
