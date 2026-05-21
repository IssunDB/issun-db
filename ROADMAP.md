# Roadmap

This document tracks what is complete and what remains for IssunDB.

> [!IMPORTANT]
> This roadmap reflects the current state of the project and is updated as work lands.

## Storage Engine

- [x] Cargo workspace with five crates: `issundb-core`, `issundb-cypher`, `issundb-rag`, `issundb`, and `issundb-cli`
- [x] Shared `[workspace.dependencies]` with all version pins in the root `Cargo.toml`
- [x] Schema types: `NodeId`, `EdgeId`, `LabelId`, `TypeId`, `AdjEntry`, `NodeRecord`, and `EdgeRecord`
- [x] `AdjEntry` as a fixed 20-byte `#[repr(C, packed)]` struct with `zerocopy` derives
- [x] LMDB environment via `heed`; six sub-databases: `nodes`, `edges`, `out_adj`, `in_adj`, `vectors`, and `meta`
- [x] Monotonic node and edge ID allocation persisted in the `meta` sub-database
- [x] String-to-integer label and edge-type registries persisted in the `meta` sub-database
- [x] msgpack property encoding and decoding via `rmp-serde`
- [x] Write serialization via `parking_lot::Mutex`
- [x] `out_adj` and `in_adj` as LMDB `DUPSORT + DUPFIXED` databases: one raw `AdjEntry` per duplicate value, O(log n) per edge insert
- [x] Label secondary index: composite key `(LabelId, NodeId)` for prefix-range scans
- [x] Edge-type secondary index: composite key `(TypeId, EdgeId)` for prefix-range scans
- [x] Criterion benchmark suite: `node_insert`, `edge_insert`, and `out_neighbors` throughput groups; `load_test` group (1 million nodes, 5 million edges) with `sample_size(10)`, run via `ISSUNDB_LOAD_TEST=1 cargo bench -p issundb-core -- load_test`

## Graph API

- [x] `Graph::add_node`, `get_node`, `add_edge`, `get_edge`, `out_neighbors`, and `in_neighbors`
- [x] `Graph::nodes_by_label(label: &str) -> Result<Vec<NodeId>, Error>`
- [x] `Graph::edges_by_type(etype: &str) -> Result<Vec<EdgeId>, Error>`
- [x] `Graph::bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- [x] `Graph::shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- [x] `Graph::page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`
- [x] `Graph::connected_components() -> Result<HashMap<NodeId, u64>, Error>`

## CSR Cache and GraphBLAS

- [x] `CsrSnapshot`: `row_ptr`, `col_idx`, `edge_type`, `edge_id`, `dense_to_id`, and `id_to_dense` fields
- [x] `CsrCache` backed by `arc-swap`; rebuilt from LMDB in a background thread when the dirty count exceeds a threshold
- [x] `Graph::neighbors` hot path reading from the CSR snapshot
- [ ] GraphBLAS context and per-type boolean matrix materialization from the CSR snapshot
- [ ] GraphBLAS-backed BFS, PageRank, and SSSP via `graphblas_sparse_linear_algebra`
- [x] Microbenchmark: CSR BFS vs raw LMDB cursor

## Vector Index

- [x] `usearch` HNSW index wrapper in `crates/issundb-core/src/vector.rs`
- [x] `Graph::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), Error>`
- [x] `Graph::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, Error>`
- [x] Index rebuild from the `vectors` sub-database on `Graph::open`

## GraphRAG

- [x] `Subgraph` type: `nodes`, `edges`, and `scores` fields
- [x] `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, Error>`
- [x] `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, Error>`
- [ ] k-hop BFS expansion using GraphBLAS SpMV over all edge types
- [x] Subgraph materialization from LMDB

## Cypher Query Language

- [ ] AST node types in `crates/issundb-cypher/src/ast.rs`
- [ ] `chumsky`-based parser for node patterns, relationship patterns, WHERE predicates, and RETURN projections
- [ ] Parameter binding (`$param` syntax)
- [ ] Logical planner: `LabelScan`, `Expand`, `LabelFilter`, and `Project` operators
- [ ] Physical planner and optimizer: filter pushdown and operator reordering
- [ ] `Expand` compiled to GraphBLAS SpMV; `LabelFilter` compiled to element-wise AND
- [ ] CREATE, SET, and DELETE execution
- [ ] Variable-length path patterns (`[:REL*1..3]`)
- [ ] `WITH` and `UNWIND` clauses
- [ ] Selectivity estimates from LMDB sub-database statistics
- [ ] openCypher TCK subset in `tests/conformance/`, gated on `ISSUNDB_CONFORMANCE=1`

## Language Bindings

- [ ] `pyo3` bindings in `crates/issundb-py`: `Db`, `Graph`, `Subgraph`, and `Hit` classes
- [ ] `maturin` wheel build and CI publishing to TestPyPI
- [ ] Cross-platform wheel matrix via `cibuildwheel` (Linux x86-64, macOS arm64, and Windows x86-64)
- [ ] `napi-rs` bindings in `crates/issundb-node`: `Db` and `Graph` classes
- [ ] Auto-generated TypeScript type definitions
- [ ] Prebuilt Node.js binary matrix (Linux x86-64, macOS arm64, and Windows x86-64)

## Testing and Tooling

- [x] Unit tests: node insert and read, edge insert and read, adjacency consistency, and ID uniqueness
- [x] `proptest` property tests for ID allocation invariants and adjacency round-trips
- [x] Integration tests driving the `issundb` public facade
- [x] Criterion benchmark suite against the LDBC SNB subset
- [ ] Benchmark against Cozo on the LDBC SNB subset
- [x] `issundb-cli` interactive REPL

## Distribution

- [ ] `README.md` with quickstart, API overview, and build instructions
- [ ] Rustdoc on the `issundb` public facade published to docs.rs
- [ ] Examples: `graphrag_quickstart.rs`, `neo4j_migration.rs`, and `load_ldbc.rs`
- [ ] `issundb`, `issundb-core`, `issundb-cypher`, and `issundb-rag` published to crates.io
- [ ] GitHub release with changelog and prebuilt CLI binaries
