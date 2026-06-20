# AGENTS.md

This file provides guidance to coding agents collaborating on this repository.

## Mission

IssunDB is an embedded graph database with vector and full-text search, written in Rust.
Priorities, in order:

1. Correct storage behavior: ACID transactions, adjacency consistency, and ID uniqueness.
2. Clear boundaries between the storage engine, query layer, vector and text indexes, and public facade.
3. Reproducible, benchmark-backed performance; no premature optimization before correctness is covered.
4. Idiomatic Rust: ownership, zero-cost abstractions, and `unsafe` only where necessary and documented.

## Core Rules

- Use English for code, comments, docs, and tests.
- Prefer small, focused changes over broad rewrites.
- Keep the workspace modular: `issundb-core` owns graph storage, `issundb-vector` owns vector search, `issundb-text` owns full-text search,
  `issundb-retrieval` owns hybrid retrieval, `issundb-cypher` owns the query layer, `issundb` is the public facade, `issundb-cli` uses only
  the public facade, and the binding crates (`issundb-rest`, `issundb-mcp`, `issundb-py`) consume only the
  `issundb` facade and its extension crates. Do not import across those boundaries in the wrong direction.
- Keep all mutable state inside `Graph` and `Storage`; do not introduce module-level `static mut` or `lazy_static` globals for runtime state.
- Writes are serialized via the `parking_lot::ReentrantMutex<()>` write lock on `Graph`; LMDB enforces the same constraint at the storage level. Do not bypass
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

The current tree includes storage, CSR snapshots, vector search, hybrid retrieval primitives, Cypher, the CLI, a REST API, language bindings,
and shared test utilities.
This layout describes the current structure and target decoupled crate boundaries.
Do not invent modules that do not yet exist when answering questions, but do place new modules according to this map.

- `crates/issundb-core/`: storage engine. Public surface is `Graph` and the schema types.
    - `src/bin/gen_testdata.rs`: the `gen_testdata` binary that regenerates the versioned LMDB storage-format snapshot (works with `make testdata`).
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
      exact-semantics `Json` fallback) per node property, built lazily from one full node scan and kept fresh by a post-commit delta (node
      deletion forces a rebuild). Read through `Graph::node_prop_json`. Also owns the lazily computed per-property statistics (`PropStats`: bounds,
      an equi-depth histogram, and the most common values) that back the selectivity estimates, invalidated by the post-commit patch.
    - `src/histogram.rs`: equi-depth histogram over property values with equality and range selectivity estimates; backs `PropStats`. Nothing
      here is persisted.
    - `src/matrices.rs`: GraphBLAS matrix materialization from the CSR snapshot, plus `MatrixSet::apply_delta` for incremental in-place maintenance
      (resize plus per-element set and drop) and the self-contained `dense_to_id`/`id_to_dense` mapping the matrix-view consumers read.
    - `src/error.rs`: `Error` enum; all storage and serialization errors unify here.
- `crates/issundb-cypher/`: Cypher parser, AST, logical planner, physical planner, optimizer, and executor.
    - `src/parser.rs`: Cypher parser built with the `chumsky` parser-combinator library (with a Pratt parser for operator-precedence expressions) for
      MATCH (including inline relationship property maps and multi-label node patterns
      such as `(n:A:B)`), WHERE, RETURN, CREATE, SET (property and label assignment), REMOVE (label and property), and DELETE/DETACH DELETE over
      arbitrary expression targets.
    - `src/ast.rs`: AST node types.
    - `src/plan/`: logical planner, physical planner, optimizer, and statistics helpers.
    - `src/exec/mod.rs`: public entry points (`execute`, `explain`), shared type definitions, and tests.
    - `src/exec/read.rs`: `execute_physical` and read-path helpers (`evaluate_where`, `evaluate_sort_key`, `json_to_prop_value`,
      `execute_filter_over_expand`).
    - `src/exec/vectorized.rs`: columnar fast path for the final projection or aggregation over a linear chain of up to `MAX_VEC_HOPS`
      directed single hops. A structural recognizer matches `[Limit]? [Sort]? [Distinct]? Project [Aggregate]? Stage* (Expand(directed single
      hop) Stage*){0,MAX_VEC_HOPS} Leaf` with single-property expressions, modeling the chain as one id column per node variable (the leaf
      plus each hop's destination). It executes column-at-a-time (per-hop bulk expansion with the fan-out preserving the row pipeline's
      depth-first order, bulk label membership, bulk property gather via `Graph::node_props_json_table`, and group-by-code aggregation via
      `Graph::node_prop_group_codes`), building the result records directly. A multi-hop chain is recognized only when every hop carries a
      distinct relationship type, so no single edge can fill two hops and relationship uniqueness is vacuous (the column fan-out tracks no
      edge identity); a repeated type, or a chain longer than `MAX_VEC_HOPS`, falls back. When the single aggregate is a non-distinct `count`
      over the chain's terminal variable and that variable feeds no group key, the executor collapses the final hop: instead of materializing
      every terminal row it counts each source's qualifying neighbors once (`execute_collapsed_count`), so a `count` of upstream-grouped
      neighbors stays proportional to the edges scanned rather than the rows produced. The recognizer sees through a `Distinct` operator
      because the caller deduplicates the built records. Any unrecognized shape falls back to the row pipeline, so correctness never depends
      on the recognizer.
    - `src/exec/factorize.rs`: `FactorizedRecordGroup` (shared `Arc<PathMap>` prefix plus per-row extensions) and `filter_refs_in_expr`.
    - `src/exec/expr.rs`: expression evaluation (`evaluate_expr`, `eval_binary_op`, `eval_arithmetic`, `eval_function_call`).
    - `src/exec/write.rs`: mutation execution (`execute_create`, `execute_set`, `execute_delete`, `execute_merge`).
    - `src/exec/ddl.rs`: DDL execution (`execute_create_index`, `execute_drop_index`).
- `crates/issundb-graphblas-sys/`: raw FFI bindings to the Apache-2.0 SuiteSparse:GraphBLAS C library, vendored as the `external/GraphBLAS`
  git submodule (pinned to v10.3.1) and built from source by `build.rs` as a position-independent static library with a dynamically linked
  OpenMP runtime (`libgomp`), so the objects link into the binding `cdylib`s. Bindings are generated by `bindgen`. Replaces the non-permissive
  `suitesparse_graphblas_sys`. `cargo package` never descends into submodules, so a crate consumed from crates.io carries no submodule;
  `build.rs` resolves the source in priority order (the `ISSUNDB_GRAPHBLAS_SRC` override, then the submodule, then the pinned tarball downloaded
  into `OUT_DIR` and checksum-verified) so the in-repo build uses the submodule with no network while a crates.io build fetches the pinned source.
- `crates/issundb-graphblas/`: minimal safe wrapper over the GraphBLAS operations the engine uses (typed `Matrix`/`Vector` over `i32`/`f32`/`f64`,
  build from triples, `mxv` over predefined semirings, `ewise_add` over predefined monoids, and the descriptor flags). Depends only on
  `issundb-graphblas-sys`. `issundb-core` reaches GraphBLAS exclusively through this crate. Replaces the non-permissive
  `graphblas-sparse-linear-algebra`.
- `crates/issundb-vector/`: vector index abstraction, vector metadata, vector storage integration, and vector search APIs.
- `crates/issundb-text/`: tokenization, full-text index storage, text search APIs, and ranking.
- `crates/issundb-retrieval/`: hybrid retrieval over graph traversal, vector hits, text hits, property filters, score fusion, and subgraph
  materialization.
- `crates/issundb/`: public facade. Re-exports the deliberate public surface from `issundb-core`, `issundb-vector`, `issundb-text`,
  `issundb-retrieval`, and `issundb-cypher`. Do not re-export internal storage types like `Storage`.
    - `benches/`: Criterion query optimizer benchmark, and two profiling drivers that load a persistent graph once and rerun a query so a profiler
      observes query execution without load noise: `profile_triangle` (Zipf-skewed graph, cyclic triangle-count query) and `profile_query` (uniform
      graph with the comparison harness's Person/KNOWS schema, arbitrary query via `PROFILE_QUERY`).
- `crates/issundb-cli/`: interactive REPL binary. Uses only the `issundb` public facade for manual exploration and demos.
- `crates/issundb-rest/`: Axum-based HTTP REST API server. Exposes the data plane and retrieval over HTTP: node and edge CRUD, Cypher query
  execution, query plan explanation, vector upsert and search, full-text search, and hybrid retrieval. Index administration and host operations
  (backup, restore, thread control) are intentionally not exposed over HTTP. Serves a generated OpenAPI 3.1 document at `/v1/openapi.json` and a
  Scalar UI at `/v1/docs`. Uses `tokio` as its async runtime; depends only on `issundb`.
- `crates/issundb-mcp/`: Model Context Protocol server built on the `rmcp` SDK, serving over either stdio or MCP's Streamable HTTP transport.
  Exposes a curated read, query, and retrieval surface for LLM agents: node and edge reads, Cypher query execution (the mutation path), query
  plan explanation, full-text search, vector search, and hybrid retrieval. Index administration, vector loading, and host operations are
  intentionally excluded. Uses `tokio` as its async runtime; depends only on `issundb`.
- `crates/issundb-py/`: Python bindings via PyO3. Exposes the `IssunDB` class with node and edge CRUD, Cypher query and explain, vector upsert
  and search, vector index configuration, full-text search and index administration, hybrid retrieval, GraphBLAS thread-count control, and
  backup and restore methods. Depends only on `issundb`.
- `crates/issundb-examples/`: standalone example programs (`quickstart.rs`, `hybrid_retrieval_quickstart.rs`, `neo4j_migration.rs`, and
  `load_ldbc.rs`). Depends only on `issundb`.
- `crates/issundb-core/benches/`: Criterion storage, Pokec dataset, Wikipedia PageRank, and write throughput benchmarks.
- `crates/issundb-cypher/benches/`: Criterion Cypher parsing, execution, LSQB Q1–Q9 queries, and OLTP transactional read benchmarks.
- `crates/issundb-vector/benches/`: Criterion vector search benchmarks.
- `crates/issundb-text/benches/`: Criterion full-text search benchmarks.
- `crates/issundb-retrieval/benches/`: Criterion hybrid retrieval and GraphRAG local/global query benchmarks.
- `crates/issundb/tests/conformance/`: openCypher TCK subset integration tests.
- `benchmarks/ladybugdb-compare/`: differential comparison harness against LadybugDB. Deliberately excluded from the workspace (own `[workspace]`
  stanza, root `exclude`, and own `rust-toolchain.toml`) because the `lbug` crate links the LadybugDB C++ library and needs a newer Rust than the
  workspace MSRV; it must never become part of `make build` or `make test`. Run via `make bench-ladybugdb`, which `cd`s into the directory so the
  local toolchain pin applies. Cross-engine harnesses belong here, not in crate-local `benches/`, which is reserved for Criterion targets. The
  differential row-set check runs before timing; a `DIVERGENT` verdict is an attributed LadybugDB walk-semantics overcount and does not fail the
  run, but a `MISMATCH` does (`tests/lbug_trail_semantics.rs` pins the walk-versus-trail divergence).
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
- Property indexes (`node_prop_idx`, `edge_prop_idx`) embed the encoded property value inside the LMDB key, so an indexable value is bounded by
  LMDB's 511-byte key limit. `encode_property_value` declines a string longer than `MAX_INDEXED_STRING_LEN` (480 bytes, conservative), leaving that
  value out of the index; the property is still stored, and equality lookups (`nodes_by_property`, `edges_by_property`) fall back to a label or type
  scan that compares the stored value directly, so results stay correct. Long text belongs in a full-text index, not a property index.
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
8. `issundb-rest`, `issundb-mcp`, and `issundb-py` must depend only on `issundb`; they must not import
   `issundb-core`, `issundb-vector`, `issundb-text`, `issundb-retrieval`, or `issundb-cypher` directly.

Lower-level crates must not know about higher-level crates.

## Component APIs

### `issundb_core::Graph`

The central coordination type.
All graph operations go through `Graph`; do not call `Storage` directly from outside `issundb-core`.

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>` is the only constructor.

Node and edge CRUD, accessors, and registry lookups have predictable signatures; read them from the source rather than this file. Methods:
`add_node`, `add_node_multi`, `get_node`, `update_node`, `delete_node`, `add_label`, `remove_label`, `node_labels`, `add_edge`, `get_edge`,
`update_edge`, `delete_edge`, `out_neighbors`, `in_neighbors`, `node_has_relationships`, `nodes_by_label`, `edges_by_type`, `all_nodes`,
`label_name`, `type_name`, `list_node_indexes_and_constraints`, `list_edge_indexes_and_constraints`, `node_count_by_label`,
`edge_count_by_type`, `put_vector_bytes`, `vector_bytes`, and `rebuild_csr`.

The read-path and statistics methods carry non-obvious semantics:

- `node_prop_json(id: NodeId, prop: &str) -> Result<Option<serde_json::Value>, Error>` (single-property read through the in-memory property
  columns; `None` for a nonexistent node, `Some(Value::Null)` for a missing property)
- `node_props_json_table(ids: &[NodeId], props: &[&str]) -> Result<Vec<Vec<serde_json::Value>>, Error>` (bulk row-major property gather
  through the property columns; one columns refresh and one dense-index resolution per id, `Value::Null` for a missing property, and
  `Error::NodeNotFound` for a nonexistent node)
- `node_prop_json_column(ids: &[NodeId], prop: &str) -> Result<Vec<serde_json::Value>, Error>` (single-property column form of the table
  gather: one flat vector with no per-row vector allocation; same null and missing-node semantics)
- `node_prop_group_codes(ids: &[NodeId], prop: &str) -> Result<(Vec<u32>, Vec<serde_json::Value>), Error>` (dense group codes under exact
  value identity of one property, plus one representative value per code; null and missing values share one `Value::Null` code; on a typed
  column no per-row value is materialized)
- `node_prop_min_max(prop: &str) -> Result<Option<(serde_json::Value, serde_json::Value)>, Error>` (bounds of one property's non-null values
  from the lazily computed column statistics; `None` for a `Json` fallback column or no non-null values; backs the vectorized executor's
  zone-map filter pruning)
- `estimate_range_selectivity(prop: &str, lower: Option<&serde_json::Value>, upper: Option<&serde_json::Value>) -> Result<Option<f64>, Error>`
  (estimated fraction of non-null values inside the bounds, from the property's equi-depth histogram)
- `estimate_equality_selectivity(prop: &str, val: &serde_json::Value) -> Result<Option<f64>, Error>` (estimated fraction of non-null values
  equal to `val`: exact for the property's most common values, histogram-estimated otherwise; both estimates feed the optimizer's
  selectivity-aware `Filter` plan weight)
- `label_filter(nodes: &[NodeId], label: &str) -> Result<Vec<NodeId>, Error>` (subset of `nodes` carrying `label`, via one `label_idx` point
  lookup per candidate)
- `set_thread_count(n: i32) -> Result<(), Error>`: sets the thread count for GraphBLAS matrix computations, overriding the `ISSUNDB_NUM_THREADS`
  environment variable (set to 0 to restore default behavior).

Graph algorithms have self-describing signatures over `NodeId` and `EdgeId`: `bfs`, `dfs`, `shortest_path`, `all_paths`, `all_shortest_paths`,
`longest_path`, `shortest_path_top_k`, `page_rank`, `connected_components`, `strongly_connected_components`, `detect_cycle`, `label_propagation`,
`degree_centrality`, `betweenness_centrality`, `harmonic_centrality`, `spanning_forest`, `maximum_flow`, and `all_neighbors`. Two carry behavior
worth pinning:

- `shortest_path_dijkstra(src: NodeId, dst: NodeId) -> Result<Option<WeightedPath>, Error>` (edge weight is the first present of the `weight`, `cost`,
  `capacity`, or `cap` property, default `1.0`; the source is fixed, so unlike `shortest_path_top_k` and `spanning_forest` this method takes no
  weight-property argument)
- `count_triangle_cycles(spec: &TriangleCountSpec) -> Result<u64, Error>` (assignment count of the directed triangle pattern
  `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` with optional per-hop relationship types and per-variable labels, following Cypher MATCH row
  semantics including relationship uniqueness; the Cypher optimizer lowers grouping-free `count` aggregates over that pattern to this
  kernel via the `TriangleCount` physical operator)

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
- `VectorGraphExt::remove_vector(n: NodeId) -> Result<(), VectorError>`: removes the embedding for a node from both memory and storage.
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>`
- `VectorGraphExt::vector_search_with(q: &[f32], opts: &VectorSearchOptions) -> Result<Vec<Hit>, VectorError>`: adds an exact-label filter,
  property equality filters (both evaluated during the HNSW traversal), and `rescore_factor`. On a quantized index the search defaults to
  fetching `2k` candidates and re-ranking them by exact distance against the raw f32 vectors persisted in LMDB, which recovers most of the
  recall lost to quantization; `Some(1)` disables the rescore and a `Float32` index never rescores by default.

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
- `retrieve_hybrid(graph: &Graph, q: &[f32], text_query: &str, opts: &HybridRetrieveOptions) -> Result<Subgraph, RetrievalError>`: fuses vector and text search seed relevance scores before running expansion.
- `Subgraph`: `nodes: Vec<NodeId>`, `edges: Vec<EdgeId>`, `scores: HashMap<NodeId, f32>`
- `RetrieveOptions`: `k`, `hops`, `max_distance`, `max_nodes`
- `HybridRetrieveOptions`: `vector_k`, `text_k`, `text_label`, `text_property`, `hops`, `max_distance`, `max_nodes`, `vector_label`, `fusion`
- `FusionStrategy`: reciprocal rank fusion (`Rrf { k }`) or linear combination (`WeightedSum { vector_weight, text_weight }`)

### `issundb_cypher`

Cypher query execution. Exposed through the `issundb` facade via the `GraphQueryExt` trait; do not call `issundb_cypher::execute` directly from
outside `issundb`.

- `query(cypher: &str) -> Result<QueryResult, CypherError>`,
  `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, CypherError>`,
  `query_with_procedures(cypher: &str, params: &HashMap<String, serde_json::Value>, registry: &ProcedureRegistry) -> Result<QueryResult, CypherError>`, and
  `explain(cypher: &str) -> Result<String, CypherError>`
- `QueryResult`: `columns: Vec<String>`, `records: Vec<Record>`
- `Record`: `values: Vec<serde_json::Value>`

The executor resolves patterns through the physical plan.
Untyped expansion uses GraphBLAS SpMV; typed expansion reads the CSR snapshot in bulk behind a snapshot-only freshness gate
(`ensure_snapshot_fresh`, which skips GraphBLAS matrix materialization), falling back to per-source LMDB point reads when the snapshot is stale
and the source set is small so a write-then-expand workload never pays a rebuild. The optimizer splits top-level `AND` conjunctions in WHERE so
each conjunct pushes down to its own lowest binder, and rewrites an equality or range filter over a labeled scan into `NodeIndexScan` or
`NodeRangeScan` when the property has a declared index; the rewrite recurses through every single-input operator (including `Aggregate`, `Sort`,
`Limit`, and `Distinct`) and treats a split conjunct's expression form like the structured comparison forms. A natural inner `HashJoin` whose one
side merely re-scans a variable the other already binds (the shape a multi-`MATCH` sharing a pivot produces) is rewritten into a linear
"expand into" chain (`rewrite_join_to_expand`), grafting the redundant-scan side's `Filter`/`Expand` chain onto the driver so the full re-scan is
eliminated and the columnar path and closing-join rewrite can both exploit the chain; it fires only when the two sides share exactly the one rooted
variable and never across an `OptionalMatch`. Bulk label filtering uses `label_idx` point
lookups (`Graph::label_filter`), and single-property node reads go through the in-memory property columns (`Graph::node_prop_json`).
A final projection or aggregation over a linear chain of up to `MAX_VEC_HOPS` directed hops executes column-at-a-time through `exec/vectorized.rs`
(`Graph::node_props_json_table` and `Graph::node_prop_group_codes`); every other shape runs the row pipeline.
A grouping-free `count` over a one-hop or two-hop directed expansion lowers instead to the `PathCount` kernel
(`Graph::count_linear_paths`); per-vertex `prop CMP literal` predicates on the path's labeled variables push down into the kernel as
index-resolved node-id allow-sets (`PathCountSpec::vertex_allow`), so a filtered path count stays a kernel call rather than materializing rows.
`RETURN DISTINCT` plans a `Distinct` operator between the final `Project` and `Sort`, keyed on the projected columns, so deduplication
happens before `ORDER BY` and `SKIP`/`LIMIT`; `WITH DISTINCT` keeps full-row deduplication behind its barrier project, and only
`RETURN DISTINCT *` deduplicates records after projection in the executor.

### `issundb_rest`

HTTP REST API server built on Axum and Tokio.
Depends only on `issundb`; must not import lower-level crates directly. All handlers share a single `Arc<Graph>` instance.

Data and query routes are versioned under a `/v1` prefix.
`GET /health` stays unversioned so infrastructure probes do not track the API version; its body reports the crate `version` and the current `api`
version.

REST exposes the data plane and retrieval only. Index administration (vector index configuration, text index create/drop/list), GraphBLAS
thread control, and backup/restore are intentionally absent: provisioning and host operations are done through the CLI or the Python surface, not
over HTTP. This keeps the network surface to data and queries and avoids exposing host-filesystem operations to network callers.

The API is self-describing: the OpenAPI 3.1 document is generated from the handler annotations (`#[utoipa::path]`) and the request and response
`ToSchema` derives, so it cannot drift from the routes. It is served as JSON at `GET /v1/openapi.json`, with an interactive Scalar UI at
`GET /v1/docs`. The generator crates are `utoipa` and `utoipa-scalar` (both MIT or Apache-2.0), pinned to the axum 0.7 line. Because the
handlers build their JSON bodies inline with `json!`, the documentation-only response structs (`NodeResponse`, `EdgeResponse`, `IdResponse`,
`QueryResponse`, `ExplainResponse`, `RetrieveResponse`, `HealthResponse`, and `ErrorResponse`) describe the response shapes and must be kept in
sync with those literals. The Cypher result is documented as columns plus row-major records of arbitrary JSON, because the per-query value types
are not statically known.

Routes:

- `POST /v1/nodes`, `GET /v1/nodes/:id`, `PUT /v1/nodes/:id`, `DELETE /v1/nodes/:id`
- `POST /v1/edges`, `GET /v1/edges/:id`, `DELETE /v1/edges/:id`
- `POST /v1/query` (Cypher execution), `POST /v1/explain` (query plan)
- `POST /v1/search/text`, `POST /v1/search/vector`
- `POST /v1/vectors` (upsert embedding)
- `POST /v1/retrieve` (hybrid retrieval)
- `GET /v1/openapi.json` (OpenAPI 3.1 document), `GET /v1/docs` (Scalar UI)
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

The tool surface is deliberately curated for an LLM agent: reads, queries, and retrieval only. Tools: `get_node`, `get_edge`, `cypher_query`,
`explain`, `text_search`, `vector_search`, and `retrieve_hybrid`. There are no typed mutation tools: graph mutations are expressed as Cypher
(`CREATE`, `SET`, `REMOVE`, `DELETE`, `MERGE`) through `cypher_query`. There are also no index-administration, vector-loading, thread-control, or
backup/restore tools; those are operator concerns driven through the CLI or the Python and REST surfaces, not through an agent. Keep this surface
minimal: every additional tool dilutes the agent's tool selection, so new agent-facing capability should clear that bar before being added here.

### `issundb_py`

Python bindings via PyO3. Exposes a single `IssunDB` class. The `extension-module` feature must be enabled for the Python extension to compile.
Depends only on `issundb`.

Methods: `add_node` (accepts a single label string or a list of label strings), `get_node`, `update_node`, `delete_node`, `add_edge`,
`get_edge`, `delete_edge`, `query`, `explain`, `upsert_vector`, `vector_search` (with optional `label` and JSON-object `properties` filters),
`configure_vector_index`, `text_search`, `create_text_index` (with optional `language`), `drop_text_index`, `list_text_indexes`,
`retrieve_hybrid`, `set_thread_count`, `backup`, `backup_compact`, and `restore`.

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
- `make bench-search-data` to download the Stack Exchange datasets (into the gitignored `data/` path) that back the text, vector, and hybrid
  retrieval benchmarks. Those benches are gated on `ISSUNDB_BENCH_SEARCH_DIR` and skip cleanly when it is unset, so they never block `make bench`.

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
