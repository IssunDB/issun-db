# AGENTS.md

This file provides guidance to coding agents collaborating on this repository.

## Mission

IssunDB is an embedded graph database with vector and full-text search, written in Rust.
Priorities, in order:

1. Correct storage behavior: ACID transactions, adjacency consistency, and ID uniqueness.
2. Clear boundaries between the storage engine, query layer, vector index, and public facade.
3. Reproducible, benchmark-backed performance; no premature optimization before correctness is covered.
4. Idiomatic Rust: ownership, zero-cost abstractions, and `unsafe` only where necessary and documented.

## Core Rules

- Use English for code, comments, docs, and tests.
- Prefer small, focused changes over broad rewrites.
- Keep the workspace modular: `issundb-core` owns graph storage, `issundb-vector` owns vector search, `issundb-text` owns full-text search,
  `issundb-retrieval` owns hybrid retrieval, `issundb-cypher` owns the query layer, `issundb` is the public facade, `issundb-cli` uses only
  the public facade, and the binding crates (`issundb-rest`, `issundb-mcp`, `issundb-node`, `issundb-py`) consume only the
  `issundb` facade and its extension crates. Do not import across those boundaries in the wrong direction.
- Keep all mutable state inside `Graph` and `Storage`; do not introduce module-level `static mut` or `lazy_static` globals for runtime state.
- Writes are serialized via the `parking_lot::Mutex<()>` write lock on `Graph`; LMDB enforces the same constraint at the storage level. Do not bypass
  either.
- Add comments only when they clarify a non-obvious storage invariant, an LMDB lifetime constraint, or a GraphBLAS semiring choice.
- Maintain the permissive license boundary of the workspace (MIT or Apache-2.0). Do not add dependencies or statically link libraries with copyleft,
  weak copyleft, or source-available licenses (such as GPL, MPL, or SSPL). Keep any comparison or benchmarking harnesses that link to such external
  engines excluded from the root Cargo workspace.
- Format with `rustfmt` (`make format`) and lint with Clippy (`make lint`) before declaring a change done.

Quick examples:

- Good: add a `Graph::bfs` method in `crates/issundb-core/src/graph/algo.rs` with unit tests using a temp LMDB directory.
- Good: add a Cypher parser test in `crates/issundb-cypher/src/` against the openCypher TCK subset.
- Bad: import `heed` directly in `crates/issundb/src/lib.rs` instead of going through `issundb-core`.
- Bad: store a node cache in a `static` `HashMap` outside `Graph`.
- Bad: add a cargo dependency to a workspace crate that pulls in a copyleft or source-available library (such as an MPL-licensed or SSPL-licensed
  library).

## Writing Style

- Use Oxford commas in inline lists: "a, b, and c" not "a, b, c".
- Do not use em dashes. Restructure the sentence, or use a colon or semicolon instead.
- Avoid colorful adjectives and adverbs. Write "adjacency query" not "blazing adjacency query".
- Use noun phrases for checklist items, not imperative verbs. Write "temp directory teardown" not "tear down the temp directory".
- Headings in Markdown files must be in title case: "Build from Source" not "Build from source". Minor words (a, an, the, and, but, or, for, in, on,
  at, to, by, of) stay lowercase unless they are the first word.

## Repository Layout

The current tree includes storage, CSR snapshots, vector search, hybrid retrieval primitives, Cypher, the CLI, an REST API, language bindings,
and shared test utilities.
This layout describes the current structure and target decoupled crate boundaries.
Do not invent modules that do not yet exist when answering questions, but do place new modules according to this map.

- `crates/issundb-core/`: storage engine. Public surface is `Graph` and the schema types.
    - `src/schema.rs`: `NodeId`, `EdgeId`, `LabelId`, `TypeId`, `AdjEntry`, `NodeRecord`, and `EdgeRecord`. `NodeRecord` holds `labels: Vec<LabelId>`
      (a node carries zero or more labels); use `primary_label` and `has_label` to inspect them.
    - `src/storage/lmdb.rs`: `Storage` struct; opens and owns all LMDB sub-databases.
    - `src/storage/ids.rs`: monotonic ID allocation and string-to-integer registries for labels and edge types, persisted in the `meta` sub-database.
    - `src/storage/props.rs`: msgpack encode and decode helpers via `rmp-serde`.
    - `src/storage/fts.rs`: full-text index storage primitives (postings and document tables) inside the LMDB environment.
    - `src/graph/mod.rs`: `Graph`, `ReadTxn`, `WriteTxn` struct definitions and lifecycle methods (`open`, `view`, `update`, `backup`, `restore`,
      `rebuild_csr`).
    - `src/graph/node.rs`: node CRUD (`add_node`, `get_node`, `update_node`, `delete_node`).
    - `src/graph/edge.rs`: edge CRUD and adjacency (`add_edge`, `get_edge`, `delete_edge`, `out_neighbors`, `in_neighbors`, `node_has_relationships`).
    - `src/graph/index.rs`: label and type indexes, property indexes, constraints, and property scan methods.
    - `src/graph/fts_mod.rs`: full-text search index lifecycle and FTS storage primitives.
    - `src/graph/vector.rs`: vector byte storage helpers.
    - `src/graph/algo.rs`: public algorithm dispatch methods and internal traversal helpers.
    - `src/graph/graphblas/`: GraphBLAS algorithm implementations split by family: `traversal.rs`, `analytics.rs`, `paths.rs`, and `flow.rs`.
    - `src/graph/txn.rs`: `ReadTxn` and `WriteTxn` delegation impls and transaction tests.
    - `src/csr.rs`: in-memory CSR snapshot (outgoing arrays plus a transposed incoming view with per-edge type and edge ids), rebuilt in the
      background and swapped via `arc-swap`. Also owns the `GraphDelta` buffer captured on the write path and the `write_gen`/`snapshot_gen`
      generation counters that drive incremental matrix maintenance and on-demand CSR refresh.
    - `src/columns.rs`: in-memory property columns for the read path. One typed column (`Int`, `Float`, `Bool`, dictionary-encoded `Str`, or the
      exact-semantics `Json` fallback) per node property name over a self-contained dense node mapping, built lazily from one full node scan and
      kept fresh by a post-commit delta (added and updated nodes are re-read individually; node deletion forces a rebuild). Read through
      `Graph::node_prop_json`.
    - `src/matrices.rs`: GraphBLAS matrix materialization from the CSR snapshot, plus `MatrixSet::apply_delta` for incremental in-place maintenance
      (resize plus per-element set and drop) and the self-contained `dense_to_id`/`id_to_dense` mapping the matrix-view consumers read.
    - `src/error.rs`: `Error` enum; all storage and serialization errors unify here.
- `crates/issundb-cypher/`: Cypher parser, AST, logical planner, physical planner, optimizer, and executor.
    - `src/parser.rs`: hand-written recursive-descent parser for MATCH (including inline relationship property maps and multi-label node patterns
      such as `(n:A:B)`), WHERE, RETURN, CREATE, SET (property and label assignment), REMOVE (label and property), and DELETE/DETACH DELETE over
      arbitrary expression targets.
    - `src/ast.rs`: AST node types.
    - `src/plan/`: logical planner, physical planner, optimizer, and statistics helpers.
    - `src/exec/mod.rs`: public entry points (`execute`, `explain`), shared type definitions, and tests.
    - `src/exec/read.rs`: `execute_physical` and read-path helpers (`evaluate_where`, `evaluate_sort_key`, `json_to_prop_value`,
      `execute_filter_over_expand`).
    - `src/exec/vectorized.rs`: columnar fast path for the final projection or aggregation over a single-hop expansion. A structural
      recognizer matches `[Sort]? [Distinct]? Project [Aggregate]? Filter(HasLabel)* Expand(one directed hop) LabelScan` with single-property
      expressions, then executes column-at-a-time (bulk expansion, bulk label membership, bulk property gather via
      `Graph::node_props_json_table`, and group-by-code aggregation via `Graph::node_prop_group_codes`), building the result records
      directly. The recognizer sees through a `Distinct` operator because the caller deduplicates the built records. Any unrecognized
      shape falls back to the row pipeline, so correctness never depends on the recognizer.
    - `src/exec/factorize.rs`: `FactorizedRecordGroup` (shared `Arc<PathMap>` prefix plus per-row extensions) and `filter_refs_in_expr`.
    - `src/exec/expr.rs`: expression evaluation (`evaluate_expr`, `eval_binary_op`, `eval_arithmetic`, `eval_function_call`).
    - `src/exec/write.rs`: mutation execution (`execute_create`, `execute_set`, `execute_delete`, `execute_merge`).
    - `src/exec/ddl.rs`: DDL execution (`execute_create_index`, `execute_drop_index`).
- `crates/issundb-graphblas-sys/`: raw FFI bindings to the Apache-2.0 SuiteSparse:GraphBLAS C library, vendored as the `external/GraphBLAS`
  git submodule (pinned to v10.3.1) and built from source by `build.rs` as a position-independent static library with a dynamically linked
  OpenMP runtime (`libgomp`), so the objects link into the binding `cdylib`s. Bindings are generated by `bindgen`. Replaces the non-permissive
  `suitesparse_graphblas_sys`.
- `crates/issundb-graphblas/`: minimal safe wrapper over the GraphBLAS operations the engine uses (typed `Matrix`/`Vector` over `i32`/`f32`/`f64`,
  build from triples, `mxv` over predefined semirings, `ewise_add` over predefined monoids, and the descriptor flags). Depends only on
  `issundb-graphblas-sys`. `issundb-core` reaches GraphBLAS exclusively through this crate. Replaces the non-permissive `graphblas-sparse-linear-algebra`.
- `crates/issundb-vector/`: vector index abstraction, vector metadata, vector storage integration, and vector search APIs.
- `crates/issundb-text/`: tokenization, full-text index storage, text search APIs, and ranking.
- `crates/issundb-retrieval/`: hybrid retrieval over graph traversal, vector hits, text hits, property filters, score fusion, and subgraph
  materialization.
- `crates/issundb/`: public facade. Re-exports the deliberate public surface from `issundb-core`, `issundb-vector`, `issundb-text`,
  `issundb-retrieval`, and `issundb-cypher`. Do not re-export internal storage types like `Storage`.
- `crates/issundb-cli/`: interactive REPL binary. Uses only the `issundb` public facade for manual exploration and demos.
- `crates/issundb-rest/`: Axum-based HTTP REST API server. Exposes node and edge CRUD, Cypher query execution, query plan explanation, vector
  search, and full-text search over HTTP. Uses `tokio` as its async runtime; depends only on `issundb`.
- `crates/issundb-mcp/`: Model Context Protocol server built on the `rmcp` SDK, serving over either stdio or MCP's Streamable HTTP transport.
  Exposes node and edge CRUD, Cypher query execution, query plan explanation, full-text search, and vector search as MCP tools. Uses `tokio` as
  its async runtime; depends only on `issundb`.
- `crates/issundb-node/`: Node.js bindings via NAPI-RS. Exposes the `IssunDB` class with node, edge, query, vector search, text search, and
  backup methods. Depends only on `issundb`.
- `crates/issundb-py/`: Python bindings via PyO3. Exposes the `IssunDB` class with the same surface as the Node.js bindings. Depends only on
  `issundb`.
- `crates/issundb-examples/`: standalone example programs (`quickstart.rs`, `hybrid_retrieval_quickstart.rs`, `neo4j_migration.rs`, and
  `load_ldbc.rs`), the `gen_testdata` binary that regenerates the versioned LMDB storage-format snapshot (driven by `make testdata`), and two
  profiling drivers that load a persistent graph once and rerun a query so a profiler observes query execution without load noise:
  `profile_triangle` (Zipf-skewed graph, cyclic triangle-count query) and `profile_query` (uniform graph with the comparison harness's
  Person/KNOWS schema, arbitrary query via `PROFILE_QUERY`). Depends only on `issundb`.
- `crates/issundb-core/benches/`: Criterion storage, Pokec dataset, Wikipedia PageRank, and write throughput benchmarks.
- `crates/issundb-cypher/benches/`: Criterion Cypher parsing, execution, LSQB Q1–Q9 queries, and OLTP transactional read benchmarks.
- `crates/issundb-vector/benches/`: Criterion vector search benchmarks.
- `crates/issundb-text/benches/`: Criterion full-text search benchmarks.
- `crates/issundb-retrieval/benches/`: Criterion hybrid retrieval and GraphRAG local/global query benchmarks.
- `crates/issundb/tests/conformance/`: openCypher TCK subset integration tests.
- `benchmarks/ladybugdb-compare/`: differential comparison harness against LadybugDB. Deliberately excluded from the workspace (own `[workspace]`
  stanza, root `exclude`, and own `rust-toolchain.toml`) because the `lbug` crate links the LadybugDB C++ library and needs a newer Rust than the
  workspace MSRV; it must never become part of `make build` or `make test`. Run via `make bench-ladybugdb`, which `cd`s into the directory so the
  local toolchain pin applies. Cross-engine harnesses belong here, not in crate-local `benches/`, which is reserved for Criterion targets.
  The differential row-set check runs before timing, and a divergent query is reported without being timed. Traversal queries anchor at
  deterministic degree-percentile probes (cold, median, and hub) derived from the generated graph. The trail-sensitive queries carry an
  openCypher trail reference computed from the dataset, because LadybugDB matches walks (its `recursive_pattern_semantic` setting registers
  but is inert in the pinned `lbug` build, and fixed-length chains never enforce relationship uniqueness; `tests/lbug_trail_semantics.rs`
  pins this). A `DIVERGENT` verdict is an attributed LadybugDB walk-semantics overcount and does not fail the run; a `MISMATCH` does.
- `Cargo.toml`: workspace root with shared `[workspace.dependencies]`. All version pins live here.
- `Makefile`: developer workflow entry points.

## Testing Layout Rules

- Unit tests for `issundb-core` belong in `#[cfg(test)]` blocks inside the relevant source file. Each test that touches LMDB must open a fresh
  `tempfile::TempDir` and must not share state with other tests.
- Integration tests that exercise multiple crates belong in `tests/` at the workspace root or in `crates/issundb/tests/`.
- Cypher conformance tests belong in `crates/issundb/tests/conformance/` and are gated on the `ISSUNDB_CONFORMANCE=1` environment variable so the
  default
  `make test` stays fast (run them via `make test-conformance`).
- Property-based tests (via `proptest`) belong alongside the unit tests for the module whose invariants they exercise.
- Do not reach into `issundb-core` internals from integration tests; drive behavior through the `issundb` public facade or the `Graph` API.
- If you move code across modules, move or rewrite the unit tests with it.
- Benchmark targets live in crate-local `benches/` directories; do not add `#[bench]` to source files.

## Architecture Constraints

- Adjacency is stored as LMDB `DUPSORT + DUPFIXED`: each duplicate value under a node key is one raw `AdjEntry` (20 bytes). A single `db.put` appends
  one entry in O(log n); there is no read-modify-write of a blob.
- Secondary indexes (`label_idx`, `type_idx`) use 12-byte composite keys `(u32 BE, u64 BE)` stored in plain LMDB databases with `Unit` values.
  Prefix-range scans via `prefix_iter` enumerate all nodes or edges for a given label or type in ascending ID order. A multi-label node has one
  `label_idx` entry per label it carries, so it appears in every matching label scan.
- The GraphBLAS matrices (`MatrixSet`) and the CSR snapshot are the basis for the GraphBLAS algorithms, pattern matching, and multi-source
  expansion. They are kept fresh through three gates rather than a single periodic rebuild. The write path records a structural delta (added nodes,
  added edges, and removed edges, plus a `force_full` flag set on any node deletion). The pure-adjacency consumers (`bfs`, `bfs_multi_source`,
  untyped expansion, `degree_centrality`, and `connected_components`) call `ensure_matrix_view`, which applies the delta in place in time
  proportional to the change (resize plus per-element set and drop on `adjacency` and `adjacency_t`), falling back to a full `rebuild_csr` only when
  a node was deleted. The CSR-array and hybrid consumers (everything else, including `dfs`, the path searches, the weighted and flow algorithms,
  `page_rank`, and the remaining centralities) call `ensure_csr_fresh`, which rebuilds on demand gated by a committed-write generation counter
  (`write_gen` versus `snapshot_gen`); when the snapshot is already fresh it still drains the pending delta into the matrices, because a
  snapshot-only refresh leaves them lagging. Typed bulk expansion calls `ensure_snapshot_fresh`, which rebuilds only the snapshot (no GraphBLAS
  materialization) and leaves the delta for the matrix path; for a small source set over a stale snapshot it skips the gate entirely and reads
  per-source LMDB adjacency. The background rebuild after `REBUILD_THRESHOLD` writes is a compaction safety net, not the freshness path.
  Callers needing a guaranteed fresh CSR view still call `rebuild_csr`. Point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`)
  read the `out_adj` and `in_adj` stores directly through the transaction, never the snapshot, so they always reflect committed and in-transaction
  writes.
- `Storage::open` is the only entry point for LMDB. Do not call `heed::EnvOpenOptions` from outside `crates/issundb-core/src/storage/lmdb.rs`.
- Heavy dependencies are tracked in the workspace `Cargo.toml`. `usearch` and `chumsky` are active, non-optional dependencies. GraphBLAS is
  reached through the in-house permissive crates `issundb-graphblas` (safe wrapper) and `issundb-graphblas-sys` (raw FFI), which build the
  Apache-2.0 SuiteSparse:GraphBLAS C library from the `external/GraphBLAS` submodule; the non-permissive `graphblas-sparse-linear-algebra` and
  `suitesparse_graphblas_sys` crates are no longer used. Building requires the submodule (`git submodule update --init external/GraphBLAS`) plus
  cmake and clang.
- Async is not used in the core engine. LMDB and GraphBLAS are synchronous. `tokio` is an optional dependency for server mode only; do not add
  `.await` inside `issundb-core`.
- GraphBLAS initializes a process-global context and OpenMP thread pool on first use (`GrB_init`), and the crate never finalizes it. Under
  `cargo nextest` (process-per-test, used by `make coverage`) every test process pays this cost independently, so on small CI runners the thread
  pools oversubscribe and a GraphBLAS call can fail intermittently. The coverage job pins `OMP_NUM_THREADS=1` and sets `NEXTEST_RETRIES=2` to
  compensate.

## Dependency Boundaries

Target dependency direction:

0. `issundb-graphblas-sys` (raw GraphBLAS FFI) sits at the bottom; `issundb-graphblas` (safe wrapper) depends only on it. Neither depends on any
   other workspace crate. `issundb-core` reaches GraphBLAS only through `issundb-graphblas`.
1. `issundb-core` may depend on `issundb-graphblas`, but not on vector, text, retrieval, Cypher, bindings, server, or CLI crates.
2. `issundb-vector` may depend on `issundb-core`, but not on text, retrieval, Cypher, bindings, server, or CLI crates.
3. `issundb-text` may depend on `issundb-core`, but not on vector, retrieval, Cypher, bindings, server, or CLI crates.
4. `issundb-retrieval` may depend on `issundb-core`, `issundb-vector`, and `issundb-text`.
5. `issundb-cypher` may depend on public APIs from core, vector, text, and retrieval crates, but not storage internals.
6. `issundb` composes and re-exports the stable public API.
7. `issundb-cli` uses only the `issundb` facade.
8. `issundb-rest`, `issundb-mcp`, `issundb-node`, and `issundb-py` must depend only on `issundb`; they must not import
   `issundb-core`, `issundb-vector`, `issundb-text`, `issundb-retrieval`, or `issundb-cypher` directly.

Lower-level crates must not know about higher-level crates.

## Component APIs

### `issundb_core::Graph`

The central coordination type.
All graph operations go through `Graph`; do not call `Storage` directly from outside `issundb-core`.

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>`
- `add_node(label: &str, props: &impl Serialize) -> Result<NodeId, Error>`
- `add_node_multi(labels: &[&str], props: &impl Serialize) -> Result<NodeId, Error>`
- `get_node(id: NodeId) -> Result<Option<NodeRecord>, Error>`
- `node_prop_json(id: NodeId, prop: &str) -> Result<Option<serde_json::Value>, Error>` (single-property read through the in-memory property
  columns; `None` for a nonexistent node, `Some(Value::Null)` for a missing property)
- `node_props_json_table(ids: &[NodeId], props: &[&str]) -> Result<Vec<Vec<serde_json::Value>>, Error>` (bulk row-major property gather
  through the property columns; one columns refresh and one dense-index resolution per id, `Value::Null` for a missing property, and
  `Error::NodeNotFound` for a nonexistent node)
- `node_prop_group_codes(ids: &[NodeId], prop: &str) -> Result<(Vec<u32>, Vec<serde_json::Value>), Error>` (dense group codes under exact
  value identity of one property, plus one representative value per code; null and missing values share one `Value::Null` code; on a typed
  column no per-row value is materialized)
- `update_node(id: NodeId, props: &impl Serialize) -> Result<(), Error>`
- `delete_node(id: NodeId) -> Result<(), Error>`
- `add_label(id: NodeId, label: &str) -> Result<(), Error>`
- `remove_label(id: NodeId, label: &str) -> Result<(), Error>`
- `node_labels(id: NodeId) -> Result<Vec<String>, Error>`
- `add_edge(src: NodeId, dst: NodeId, etype: &str, props: &impl Serialize) -> Result<EdgeId, Error>`
- `get_edge(id: EdgeId) -> Result<Option<EdgeRecord>, Error>`
- `out_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`
- `in_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`
- `node_has_relationships(node: NodeId) -> Result<bool, Error>`
- `nodes_by_label(label: &str) -> Result<Vec<NodeId>, Error>`
- `label_filter(nodes: &[NodeId], label: &str) -> Result<Vec<NodeId>, Error>` (subset of `nodes` carrying `label`, via one `label_idx` point
  lookup per candidate)
- `edges_by_type(etype: &str) -> Result<Vec<EdgeId>, Error>`
- `rebuild_csr() -> Result<(), Error>`
- `all_nodes() -> Result<Vec<NodeId>, Error>`
- `label_name(id: LabelId) -> Result<Option<String>, Error>`
- `type_name(id: TypeId) -> Result<Option<String>, Error>`
- `list_node_indexes_and_constraints() -> Result<Vec<(String, String, u8)>, Error>`
- `list_edge_indexes_and_constraints() -> Result<Vec<(String, String, u8)>, Error>`
- `node_count_by_label(label: &str) -> Result<u64, Error>`
- `edge_count_by_type(etype: &str) -> Result<u64, Error>`
- `put_vector_bytes(n: NodeId, bytes: &[u8]) -> Result<(), Error>`
- `vector_bytes() -> Result<Vec<(NodeId, Vec<u8>)>, Error>`
- `bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- `dfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- `shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- `shortest_path_dijkstra(src: NodeId, dst: NodeId) -> Result<Option<WeightedPath>, Error>` (edge weight is the first present of the `weight`, `cost`,
  `capacity`, or `cap` property, default `1.0`; the source is fixed, so unlike `shortest_path_top_k` and `spanning_forest` this method takes no
  weight-property argument)
- `all_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`
- `all_shortest_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`
- `longest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- `shortest_path_top_k(src: NodeId, dst: NodeId, k: usize, weight_property: &str) -> Result<Vec<WeightedPath>, Error>`
- `page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`
- `connected_components() -> Result<HashMap<NodeId, u64>, Error>`
- `strongly_connected_components() -> Result<HashMap<NodeId, u64>, Error>`
- `detect_cycle() -> Result<bool, Error>`
- `count_triangle_cycles(spec: &TriangleCountSpec) -> Result<u64, Error>` (assignment count of the directed triangle pattern
  `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` with optional per-hop relationship types and per-variable labels, following Cypher MATCH row
  semantics including relationship uniqueness; the Cypher optimizer lowers grouping-free `count` aggregates over that pattern to this
  kernel via the `TriangleCount` physical operator)
- `label_propagation(max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error>`
- `degree_centrality(direction: DegreeDirection) -> Result<HashMap<NodeId, u64>, Error>`
- `betweenness_centrality() -> Result<HashMap<NodeId, f64>, Error>`
- `harmonic_centrality() -> Result<HashMap<NodeId, f64>, Error>`
- `spanning_forest(weight_property: &str, maximum: bool) -> Result<Vec<EdgeId>, Error>`
- `maximum_flow(source: NodeId, sink: NodeId, capacity_property: &str) -> Result<f64, Error>`
- `all_neighbors(node: NodeId) -> Result<Vec<DirectedNeighborEntry>, Error>`

### `issundb_vector`

Vector search crate. Owns vector index abstractions, vector metadata, vector storage integration, and vector search APIs.
It may depend on `issundb-core`; it must not depend on `issundb-text`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `VectorGraphExt::configure_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`: sets the per-graph metric and
  quantization. The choice persists in the `meta` sub-database, so reopen rebuilds with the same configuration. Call it before the first
  upsert; changing the metric or quantization once vectors exist returns `VectorError::AlreadyConfigured`. Defaults to Cosine and Float32.
- `VectorGraphExt::reindex_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`: changes the metric or quantization on a
  populated graph and rebuilds the index from the persisted embeddings under the new configuration. The stored vectors are raw, metric-agnostic
  f32, so they re-index under any metric; this is O(n) in the stored vector count and is an administrative operation, not a concurrent one.
- `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), VectorError>`
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>`

### `issundb_text`

Full-text search crate. Owns tokenization, inverted index storage, ranking, and text search APIs.
It may depend on `issundb-core`; it must not depend on `issundb-vector`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `TextGraphExt::text_search(query: &str, opts: &TextSearchOptions) -> Result<Vec<TextHit>, TextError>`
- `TextIndexExt::create_text_index(label: &str, property: &str) -> Result<(), TextError>`
- `TextIndexExt::create_text_index_with_language(label: &str, property: &str, lang: Language) -> Result<(), TextError>`
- `TextIndexExt::drop_text_index(label: &str, property: &str) -> Result<(), TextError>`
- `TextIndexExt::has_text_index(label: &str, property: &str) -> Result<bool, TextError>`
- `TextIndexExt::list_text_indexes() -> Result<Vec<(String, String, Language)>, TextError>`

### `issundb_retrieval`

Hybrid retrieval crate. May depend on `issundb-core`, `issundb-vector`, and `issundb-text`; must not be imported by those lower-level crates. All
retrieve functions are free functions, not methods on `Graph`, to preserve the crate boundary.

- `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, RetrievalError>`
- `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, RetrievalError>`
- `Subgraph`: `nodes: Vec<NodeId>`, `edges: Vec<EdgeId>`, `scores: HashMap<NodeId, f32>`
- `RetrieveOptions`: `k`, `hops`, `max_distance`, `max_nodes`

### `issundb_cypher`

Cypher query execution. Exposed through the `issundb` facade via the `GraphQueryExt` trait; do not call `issundb_cypher::execute` directly from
outside `issundb`.

- `query(cypher: &str) -> Result<QueryResult, CypherError>` and
  `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, CypherError>`
- `QueryResult`: `columns: Vec<String>`, `records: Vec<Record>`
- `Record`: `values: Vec<serde_json::Value>`

The executor resolves patterns through the physical plan.
Untyped expansion uses GraphBLAS SpMV; typed expansion reads the CSR snapshot in bulk behind a snapshot-only freshness gate
(`ensure_snapshot_fresh`, which skips GraphBLAS matrix materialization), falling back to per-source LMDB point reads when the snapshot is stale
and the source set is small so a write-then-expand workload never pays a rebuild. The optimizer splits top-level `AND` conjunctions in WHERE so
each conjunct pushes down to its own lowest binder. Bulk label filtering uses `label_idx` point
lookups (`Graph::label_filter`), and single-property node reads go through the in-memory property columns (`Graph::node_prop_json`).
A final projection or aggregation over a single-hop expansion executes column-at-a-time through `exec/vectorized.rs`
(`Graph::node_props_json_table` and `Graph::node_prop_group_codes`); every other shape runs the row pipeline.
`RETURN DISTINCT` plans a `Distinct` operator between the final `Project` and `Sort`, keyed on the projected columns, so deduplication
happens before `ORDER BY` and `SKIP`/`LIMIT`; `WITH DISTINCT` keeps full-row deduplication behind its barrier project, and only
`RETURN DISTINCT *` deduplicates records after projection in the executor.

### `issundb_rest`

HTTP REST API server built on Axum and Tokio.
Depends only on `issundb`; must not import lower-level crates directly. All handlers share a single `Arc<Graph>` instance.

Data and query routes are versioned under a `/v1` prefix.
`GET /health` stays unversioned so infrastructure probes do not track the API version; its body reports the crate `version` and the current `api`
version.

Routes:

- `POST /v1/nodes`, `GET /v1/nodes/:id`, `PUT /v1/nodes/:id`, `DELETE /v1/nodes/:id`
- `POST /v1/edges`, `GET /v1/edges/:id`, `DELETE /v1/edges/:id`
- `POST /v1/query` (Cypher execution), `POST /v1/explain` (query plan)
- `POST /v1/search/text`, `POST /v1/search/vector`
- `GET /health` (unversioned)

### `issundb_mcp`

Model Context Protocol server built on the `rmcp` SDK. Depends only on `issundb`; must not import lower-level crates directly. Holds a single
`Arc<Graph>` and serves the same tool surface over one of two transports, selected with `--transport`: `stdio` (default; for clients that launch
the server as a subprocess) or `http` (MCP's Streamable HTTP transport, mounted on an Axum router at `--http-path`, default `/mcp`, bound to
`--bind`, default `127.0.0.1:8000`). Diagnostics always go to `stderr` because the stdio transport owns `stdout`. This is distinct from
`issundb-rest`, which is a plain REST API; the HTTP transport here still speaks MCP JSON-RPC. The `rmcp` dependency is pinned to `0.11` because
`0.12` and later require `darling` `0.23`, which exceeds the workspace MSRV (`1.85`). Because the `rmcp` `0.11` Streamable HTTP transport does not
validate the `Host` header (DNS rebinding, GHSA-89vp-x53w-74fx, fixed upstream only in `rmcp` `1.4.0`), the HTTP arm wraps the router in a `Host`
header allowlist middleware. The allowlist defaults to the loopback names (`localhost`, `127.0.0.1`, `::1`) plus the `--bind` host; repeat
`--allowed-host` to add the public hostnames a reverse proxy forwards under. Requests with a missing or non-allowlisted `Host` header get HTTP 403.

Tools: `add_node`, `get_node`, `update_node`, `delete_node`, `add_edge`, `get_edge`, `delete_edge`, `cypher_query`, `explain`, `text_search`, and
`vector_search`.

### `issundb_node`

Node.js bindings via NAPI-RS. Exposes a single `IssunDB` class. Depends only on `issundb`; the `napi-module` feature must be enabled for the
NAPI entry point to compile.

Methods: `add_node`, `get_node`, `update_node`, `delete_node`, `add_edge`, `query`, `explain`, `upsert_vector`, `vector_search`,
`text_search`, `create_text_index`, `drop_text_index`, `backup`, `backup_compact`, `restore`.

### `issundb_py`

Python bindings via PyO3. Exposes a single `IssunDB` class with the same method surface as `issundb_node`. The `extension-module` feature must
be enabled for the Python extension to compile. Depends only on `issundb`.

### `issundb_core::Storage`

Internal to `issundb-core`. Owns the LMDB environment and twelve sub-databases: `nodes`, `edges`, `out_adj`, `in_adj`, `label_idx`, `type_idx`,
`node_prop_idx`, `edge_prop_idx`, `fts_postings`, `fts_docs`, `vectors`, and `meta`. Do not expose `Storage` through the `issundb` facade.

### `issundb_core::error::Error`

All `issundb-core` errors unify here. Variants cover storage (`heed::Error`), encoding (`rmp_serde::encode::Error`), decoding (
`rmp_serde::decode::Error`), and domain errors (`NodeNotFound`, `EdgeNotFound`). Callers outside `issundb-core` match on this type; do not leak `heed`
error types through the public facade.

### Encapsulation Rule

`Storage` and the `storage` module are `pub(crate)` inside `issundb-core` and are not reachable from any other crate. The `issundb`
facade re-exports only `Graph`, `Error`, `Hit`, hybrid retrieval types and functions, Cypher result types, and the schema ID and record types.
Do not add a "just for now" re-export anywhere else; add a deliberate testing helper in `issundb-core` if a test needs internal access.

## Workflow

Before coding:

1. Identification of whether this is a storage, query, vector, hybrid retrieval, bindings, or docs change.
2. Reading of the touched module and nearby tests.

Implementation using red-green TDD:

1. A failing `#[test]` that describes the expected behavior (red). For invariants, prefer a `proptest` property.
2. Verification that the test fails for the right reason: running `make test` or `cargo test -p issundb-core -- <test_name>` (red).
3. The smallest implementation that makes the test pass (green).
4. Refactor while keeping tests green.
5. Narrowest relevant test while iterating, then `make test` and `make lint` before declaring done.
6. `make format` before every commit.
7. Update of `README.md` or `docs/` if behavior or workflow changed.

Additional validation when relevant:

- `make bench` for performance-sensitive storage changes.
- `make test-conformance` for Cypher conformance coverage.
- `make bench-ladybugdb` for cross-engine performance comparison and differential correctness checks on the Cypher execution path.

## Testing Expectations

- No storage behavior change is complete without tests.
- Node insertion, edge insertion, adjacency consistency, ID uniqueness, and label or type registry correctness all need explicit coverage.
- Prefer targeted assertions (one field, one count, one round-trip) over broad snapshot tests.
- Keep tests deterministic. Each test opens its own `TempDir`; do not share LMDB environments across tests.
- When uncertain about storage correctness, add or refine tests first.

## Documentation Expectations

- Public API docs are generated from `rustdoc` on `crates/issundb/src/lib.rs`. Keep that module focused on the deliberate public surface; do not
  re-export `Storage` or other internals.
- User workflow changes should update `README.md`.
- Phase progress and completeness changes should update `ROADMAP.md`.
- If you detect stale docs while changing related code, fix them in the same patch.

## Review Guidelines (P0/P1 Focus)

Review output should be concise and include only critical issues.

- `P0`: must-fix defects (data loss, transaction safety violation, broken build, or broken test workflow).
- `P1`: high-priority defects (adjacency inconsistency, incorrect ID allocation, missing write-lock acquisition, or a risky storage change without
  tests).

Use this review format:

1. `Severity` (`P0`/`P1`)
2. `File:line`
3. `Issue`
4. `Why it matters`
5. `Minimal fix direction`

Do not include style-only feedback or broad praise.
