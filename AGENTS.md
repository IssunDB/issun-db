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
  the public facade, and the binding crates (`issundb-server`, `issundb-mcp`, `issundb-gui`, `issundb-node`, `issundb-py`) consume only the
  `issundb` facade and its extension crates. Do not import across those boundaries in the wrong direction.
- Keep all mutable state inside `Graph` and `Storage`; do not introduce module-level `static mut` or `lazy_static` globals for runtime state.
- Writes are serialized via the `parking_lot::Mutex<()>` write lock on `Graph`; LMDB enforces the same constraint at the storage level. Do not bypass
  either.
- Add comments only when they clarify a non-obvious storage invariant, an LMDB lifetime constraint, or a GraphBLAS semiring choice.
- Format with `rustfmt` (`make format`) and lint with Clippy (`make lint`) before declaring a change done.

Quick examples:

- Good: add a `Graph::bfs` method in `crates/issundb-core/src/graph/algo.rs` with unit tests using a temp LMDB directory.
- Good: add a Cypher parser test in `crates/issundb-cypher/src/` against the openCypher TCK subset.
- Bad: import `heed` directly in `crates/issundb/src/lib.rs` instead of going through `issundb-core`.
- Bad: store a node cache in a `static` `HashMap` outside `Graph`.

## Writing Style

- Use Oxford commas in inline lists: "a, b, and c" not "a, b, c".
- Do not use em dashes. Restructure the sentence, or use a colon or semicolon instead.
- Avoid colorful adjectives and adverbs. Write "adjacency query" not "blazing adjacency query".
- Use noun phrases for checklist items, not imperative verbs. Write "temp directory teardown" not "tear down the temp directory".
- Headings in Markdown files must be in title case: "Build from Source" not "Build from source". Minor words (a, an, the, and, but, or, for, in, on,
  at, to, by, of) stay lowercase unless they are the first word.

## Repository Layout

The current tree includes storage, CSR snapshots, vector search, hybrid retrieval primitives, Cypher planning, the CLI, an HTTP server, a desktop
GUI, language bindings, and shared test utilities.
This layout describes the current structure and target decoupled crate boundaries.
Do not invent modules that do not yet exist when answering questions, but do place new modules according to this map.

- `crates/issundb-core/`: storage engine. Public surface is `Graph` and the schema types.
    - `src/schema.rs`: `NodeId`, `EdgeId`, `LabelId`, `TypeId`, `AdjEntry`, `NodeRecord`, and `EdgeRecord`.
    - `src/storage/lmdb.rs`: `Storage` struct; opens and owns all LMDB sub-databases.
    - `src/storage/ids.rs`: monotonic ID allocation and string-to-integer registries for labels and edge types, persisted in the `meta` sub-database.
    - `src/storage/props.rs`: msgpack encode and decode helpers via `rmp-serde`.
    - `src/storage/fts.rs`: full-text index storage primitives (postings and document tables) inside the LMDB environment.
    - `src/graph/mod.rs`: `Graph`, `ReadTxn`, `WriteTxn` struct definitions and lifecycle methods (`open`, `view`, `update`, `backup`, `restore`, `rebuild_csr`).
    - `src/graph/node.rs`: node CRUD (`add_node`, `get_node`, `update_node`, `delete_node`).
    - `src/graph/edge.rs`: edge CRUD and adjacency (`add_edge`, `get_edge`, `delete_edge`, `out_neighbors`, `in_neighbors`).
    - `src/graph/index.rs`: label and type indexes, property indexes, constraints, and property scan methods.
    - `src/graph/fts_mod.rs`: full-text search index lifecycle and FTS storage primitives.
    - `src/graph/vector.rs`: vector byte storage helpers.
    - `src/graph/algo.rs`: public algorithm dispatch methods and internal traversal helpers.
    - `src/graph/graphblas/`: GraphBLAS algorithm implementations split by family: `traversal.rs`, `analytics.rs`, `paths.rs`, and `flow.rs`.
    - `src/graph/txn.rs`: `ReadTxn` and `WriteTxn` delegation impls and transaction tests.
    - `src/csr.rs`: in-memory CSR snapshot, rebuilt in the background and swapped via `arc-swap`.
    - `src/matrices.rs`: GraphBLAS matrix materialization from the CSR snapshot.
    - `src/metrics.rs`: lightweight runtime counters for storage operations.
    - `src/error.rs`: `Error` enum; all storage and serialization errors unify here.
- `crates/issundb-cypher/`: Cypher parser, AST, logical planner, physical planner, optimizer, and executor.
    - `src/parser.rs`: hand-written recursive-descent parser for MATCH (including inline relationship property maps), WHERE, RETURN, CREATE, SET, and
      DELETE.
    - `src/ast.rs`: AST node types.
    - `src/plan/`: logical planner, physical planner, optimizer, and statistics helpers.
    - `src/exec/mod.rs`: public entry points (`execute`, `explain`), shared type definitions, and tests.
    - `src/exec/read.rs`: `execute_physical` and read-path helpers (`evaluate_where`, `evaluate_sort_key`, `json_to_prop_value`, `execute_filter_over_expand`).
    - `src/exec/factorize.rs`: `FactorizedRecordGroup` (shared `Arc<PathMap>` prefix plus per-row extensions) and `filter_refs_in_expr`.
    - `src/exec/expr.rs`: expression evaluation (`evaluate_expr`, `eval_binary_op`, `eval_arithmetic`, `eval_function_call`).
    - `src/exec/write.rs`: mutation execution (`execute_create`, `execute_set`, `execute_delete`, `execute_merge`).
    - `src/exec/ddl.rs`: DDL execution (`execute_create_index`, `execute_drop_index`).
- `crates/issundb-vector/`: vector index abstraction, vector metadata, vector storage integration, and vector search APIs.
- `crates/issundb-text/`: tokenization, full-text index storage, text search APIs, and ranking.
- `crates/issundb-retrieval/`: hybrid retrieval over graph traversal, vector hits, text hits, property filters, score fusion, and subgraph
  materialization.
- `crates/issundb/`: public facade. Re-exports the deliberate public surface from `issundb-core`, `issundb-vector`, `issundb-text`,
  `issundb-retrieval`, and `issundb-cypher`. Do not re-export internal storage types like `Storage`.
- `crates/issundb-cli/`: interactive REPL binary. Uses only the `issundb` public facade for manual exploration and demos.
- `crates/issundb-server/`: Axum-based HTTP REST API server. Exposes node and edge CRUD, Cypher query execution, query plan explanation, vector
  search, and full-text search over HTTP. Uses `tokio` as its async runtime; depends only on `issundb`.
- `crates/issundb-mcp/`: Model Context Protocol server built on the `rmcp` SDK, serving over either stdio or MCP's Streamable HTTP transport.
  Exposes node and edge CRUD, Cypher query execution, query plan explanation, full-text search, and vector search as MCP tools. Uses `tokio` as
  its async runtime; depends only on `issundb`.
- `crates/issundb-gui/`: egui desktop application for interactive graph exploration. Provides a Cypher query console, a graph visualization canvas
  via `egui_graphs`, and a node or edge inspector. Depends only on `issundb`.
- `crates/issundb-node/`: Node.js bindings via NAPI-RS. Exposes the `IssunDB` class with node, edge, query, vector search, text search, and
  backup methods. Depends only on `issundb`.
- `crates/issundb-py/`: Python bindings via PyO3. Exposes the `IssunDB` class with the same surface as the Node.js bindings. Depends only on
  `issundb`.
- `crates/issundb-testing/`: shared test fixtures and graph builders (`open_tmp`, `chain`, `clique`, `diamond`, `two_triangles`) used across
  unit and integration tests. Depends on `issundb-core`; must not be imported by production crates.
- `crates/issundb-examples/`: standalone example programs (`hybrid_retrieval_quickstart.rs`, `neo4j_migration.rs`, `load_ldbc.rs`).
- `crates/issundb-core/benches/`: Criterion storage benchmarks.
- `crates/issundb/tests/conformance/`: openCypher TCK subset integration tests.
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
  Prefix-range scans via `prefix_iter` enumerate all nodes or edges for a given label or type in ascending ID order.
- The CSR snapshot is the hot read path for outgoing traversal. GraphBLAS operates on the CSR snapshot for all graph algorithms,
  pattern matching, and multi-source expansion.
- `Storage::open` is the only entry point for LMDB. Do not call `heed::EnvOpenOptions` from outside `crates/issundb-core/src/storage/lmdb.rs`.
- Heavy dependencies are tracked in the workspace `Cargo.toml`. `usearch`, `chumsky`, and GraphBLAS (`graphblas-sparse-linear-algebra`) are active,
  non-optional dependencies.
- Async is not used in the core engine. LMDB and GraphBLAS are synchronous. `tokio` is an optional dependency for server mode only; do not add
  `.await` inside `issundb-core`.

## Dependency Boundaries

Target dependency direction:

1. `issundb-core` has no dependencies on vector, text, retrieval, Cypher, bindings, server, GUI, or CLI crates.
2. `issundb-vector` may depend on `issundb-core`, but not on text, retrieval, Cypher, bindings, server, GUI, or CLI crates.
3. `issundb-text` may depend on `issundb-core`, but not on vector, retrieval, Cypher, bindings, server, GUI, or CLI crates.
4. `issundb-retrieval` may depend on `issundb-core`, `issundb-vector`, and `issundb-text`.
5. `issundb-cypher` may depend on public APIs from core, vector, text, and retrieval crates, but not storage internals.
6. `issundb` composes and re-exports the stable public API.
7. `issundb-cli` uses only the `issundb` facade.
8. `issundb-server`, `issundb-mcp`, `issundb-gui`, `issundb-node`, and `issundb-py` must depend only on `issundb`; they must not import
   `issundb-core`, `issundb-vector`, `issundb-text`, `issundb-retrieval`, or `issundb-cypher` directly.
9. `issundb-testing` may depend on `issundb-core` for fixture setup; it must not be imported by any production crate.

Lower-level crates must not know about higher-level crates.

## Component APIs

### `issundb_core::Graph`

The central coordination type.
All graph operations go through `Graph`; do not call `Storage` directly from outside `issundb-core`.

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>`
- `add_node(label: &str, props: &impl Serialize) -> Result<NodeId, Error>`
- `get_node(id: NodeId) -> Result<Option<NodeRecord>, Error>`
- `update_node(id: NodeId, props: &impl Serialize) -> Result<(), Error>`
- `delete_node(id: NodeId) -> Result<(), Error>`
- `add_edge(src: NodeId, dst: NodeId, etype: &str, props: &impl Serialize) -> Result<EdgeId, Error>`
- `get_edge(id: EdgeId) -> Result<Option<EdgeRecord>, Error>`
- `out_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`
- `in_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`
- `nodes_by_label(label: &str) -> Result<Vec<NodeId>, Error>`
- `edges_by_type(etype: &str) -> Result<Vec<EdgeId>, Error>`
- `rebuild_csr() -> Result<(), Error>`
- `all_nodes() -> Result<Vec<NodeId>, Error>`
- `label_name(id: LabelId) -> Result<Option<String>, Error>`
- `type_name(id: TypeId) -> Result<Option<String>, Error>`
- `node_count_by_label(label: &str) -> Result<u64, Error>`
- `edge_count_by_type(etype: &str) -> Result<u64, Error>`
- `put_vector_bytes(n: NodeId, bytes: &[u8]) -> Result<(), Error>`
- `vector_bytes() -> Result<Vec<(NodeId, Vec<u8>)>, Error>`
- `bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- `dfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`
- `shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- `shortest_path_dijkstra(src: NodeId, dst: NodeId, weight_property: &str) -> Result<Option<WeightedPath>, Error>`
- `all_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`
- `all_shortest_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`
- `longest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`
- `shortest_path_top_k(src: NodeId, dst: NodeId, k: usize, weight_property: &str) -> Result<Vec<WeightedPath>, Error>`
- `page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`
- `connected_components() -> Result<HashMap<NodeId, u64>, Error>`
- `strongly_connected_components() -> Result<HashMap<NodeId, u64>, Error>`
- `detect_cycle() -> Result<bool, Error>`
- `label_propagation(max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error>`
- `degree_centrality(direction: DegreeDirection) -> Result<HashMap<NodeId, u64>, Error>`
- `betweenness_centrality() -> Result<HashMap<NodeId, f64>, Error>`
- `harmonic_centrality() -> Result<HashMap<NodeId, f64>, Error>`
- `spanning_forest(weight_property: &str, maximum: bool) -> Result<Vec<EdgeId>, Error>`
- `maximum_flow(source: NodeId, sink: NodeId, capacity_property: &str) -> Result<f64, Error>`
- `all_neighbors(node: NodeId) -> Result<Vec<DirectedNeighborEntry>, Error>`

### `issundb_vector`

Vector search crate. Owns vector index abstractions, vector metadata, vector storage integration, and vector search APIs. It may depend on
`issundb-core`; it must not depend on `issundb-text`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), Error>`
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, Error>`

### `issundb_text`

Full-text search crate. Owns tokenization, inverted index storage, ranking, and text search APIs. It may depend on `issundb-core`; it must not
depend on `issundb-vector`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `TextGraphExt::text_search(query: &str, opts: &TextSearchOptions) -> Result<Vec<TextHit>, TextError>`
- `TextIndexExt::create_text_index(label: &str, property: &str) -> Result<(), Error>`
- `TextIndexExt::create_text_index_with_language(label: &str, property: &str, lang: Language) -> Result<(), Error>`
- `TextIndexExt::drop_text_index(label: &str, property: &str) -> Result<(), Error>`
- `TextIndexExt::has_text_index(label: &str, property: &str) -> Result<bool, Error>`
- `TextIndexExt::list_text_indexes() -> Result<Vec<(String, String, Language)>, Error>`

### `issundb_retrieval`

Hybrid retrieval crate. May depend on `issundb-core`, `issundb-vector`, and `issundb-text`; must not be imported by those lower-level crates. All
retrieve functions are free functions, not methods on `Graph`, to preserve the crate boundary.

- `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, Error>`
- `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, Error>`
- `Subgraph`: `nodes: Vec<NodeId>`, `edges: Vec<EdgeId>`, `scores: HashMap<NodeId, f32>`
- `RetrieveOptions`: `k`, `hops`, `max_distance`, `max_nodes`

### `issundb_cypher`

Cypher query execution. Exposed through the `issundb` facade via the `GraphQueryExt` trait; do not call `issundb_cypher::execute` directly from
outside `issundb`.

- `query(cypher: &str) -> Result<QueryResult, String>` and
  `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, String>`
- `QueryResult`: `columns: Vec<String>`, `records: Vec<Record>`
- `Record`: `values: Vec<serde_json::Value>`

The executor resolves patterns through the physical plan.
Expansion and label filtering use GraphBLAS SpMV and element-wise matrix operators unconditionally.

### `issundb_server`

HTTP REST API server built on Axum and Tokio.
Depends only on `issundb`; must not import lower-level crates directly. All handlers share a single `Arc<Graph>` instance.

Data and query routes are versioned under a `/v1` prefix.
`GET /health` stays unversioned so infrastructure probes do not track the API version; its body reports the crate `version` and the current `api` version.

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
`issundb-server`, which is a plain REST API; the HTTP transport here still speaks MCP JSON-RPC. The `rmcp` dependency is pinned to `0.11` because
`0.12` and later require `darling` `0.23`, which exceeds the workspace MSRV (`1.85`).

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

### `issundb_gui`

Desktop application built with `eframe` and `egui`. Depends only on `issundb`.
Provides a Cypher query console, an interactive graph visualization canvas via `egui_graphs`, and a node or edge inspector panel.
Not part of the library surface; changes here do not affect the public API.

### `issundb_testing`

Shared test fixtures for unit and integration tests. Depends on `issundb-core`. Must not be imported by production crates.

- `open_tmp() -> (Graph, TempDir)`: opens a fresh LMDB environment in a temporary directory.
- `open_at(dir: &Path) -> Graph`: opens an environment at a known path.
- `chain(graph, n)`, `clique(graph, n)`, `diamond(graph)`, `two_triangles(graph)`: graph fixture builders.
- `assert_nodes_eq(...)`, `assert_valid_path(...)`: targeted assertion helpers.

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
