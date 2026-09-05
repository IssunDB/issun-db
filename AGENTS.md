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
  `issundb-retrieval` owns hybrid retrieval, `issundb-cypher` owns the query layer, `issundb` is the public facade, and the consumer crates
  (`issundb-cli`, `issundb-rest`, `issundb-mcp`, `issundb-py`, `issundb-wasm`) use only the `issundb` facade. See Dependency Boundaries.
- Keep all mutable state inside `Graph` and `Storage`; do not introduce module-level `static mut` or `lazy_static` globals for runtime state.
- Writes are serialized via the `parking_lot::ReentrantMutex<()>` write lock on `Graph`; LMDB enforces the same constraint at the storage level. Do
  not bypass either.
- Add comments only when they clarify a non-obvious storage invariant, an LMDB lifetime constraint, or an algorithm kernel's ordering or
  duplicate-handling rule.
- Maintain the permissive license boundary of the workspace (MIT or Apache-2.0). Do not add dependencies or statically link libraries with copyleft,
  weak copyleft, or source-available licenses (such as GPL, MPL, or SSPL). Keep comparison or benchmarking harnesses that link to such external
  engines excluded from the root Cargo workspace.
- Format with `rustfmt` (`make format`) and lint with Clippy (`make lint`) before declaring a change done.

## Writing Style

- Write in simple, plain English. Use short sentences and everyday words.
- Use Oxford commas in inline lists: "a, b, and c" not "a, b, c".
- Do not use em dashes. Restructure the sentence, or use a colon or semicolon instead.
- Avoid colorful adjectives and adverbs. Write "adjacency query" not "blazing adjacency query".
- Prefer noun phrases for checklist items over imperative verbs. Write "temp directory teardown" not "tear down the temp directory".
- Headings in Markdown files must be in title case: "Build from Source" not "Build from source". Minor words stay lowercase unless they are the first
  word: the articles (a, an, the), the coordinating conjunctions (and, but, or, nor, so, yet, for), and the short prepositions (in, on, at, to, by, of,
  up, as, from, with, into, over).
- Do not bold the lead-in of a list item. Write "Vector and set similarity: ..." not "**Vector and set similarity**: ...".
- Use sentence case for the lead-in of a list item. Write "Seed selection: ..." not "Seed Selection: ...". Proper nouns keep their capitals.
- Capitalize only the first part of a hyphenated compound: "Full-text Search" in a heading, "Breadth-first" at the start of a sentence, and
  "breadth-first search" elsewhere. Never write "Breadth-First".
- Start each sentence with a capital letter, capitalize proper nouns (Rust, Cypher, LMDB), and leave common nouns lowercase in the middle of a sentence.
- Write correct and complete sentences. Avoid made-up words.
- Do not use a colon in place of a verb. A colon may join two clauses inside a complete sentence, introduce the gloss of a list item, or introduce an
  enumeration. It must not turn a sentence into a label and a definition: write "Merges vector search seeds with text search seeds, then expands via
  BFS" rather than "Hybrid retrieval: merges vector search seeds with text search seeds".
- Use participial phrases and abbreviations scarcely.

## Repository Layout

An entry says what a module owns and where a new thing belongs. How a module works lives in the crate's own `AGENTS.md`, named at the end of this
section, and a public method's contract lives under Component APIs. Do not invent modules that do not yet exist; place new modules according to this map.

- `crates/issundb-core/`: storage engine. Public surface is `Graph` and the schema types.
    - `src/bin/gen_testdata.rs`: the `gen_testdata` binary that regenerates the versioned LMDB storage-format snapshot (`make testdata`).
    - `src/schema.rs`: `NodeId`, `EdgeId`, `LabelId`, `TypeId`, `AdjEntry`, `NodeRecord`, `EdgeRecord`, and `TableStat`. A node carries zero or more
      labels; use `primary_label` and `has_label` to inspect them.
    - `src/storage/lmdb.rs`: the LMDB `Storage`; `src/storage/memory.rs`: the in-memory backend, second implementor of the contract in
      `storage/mod.rs`, which the whole suite also runs against. `src/storage/ids.rs`: monotonic ID allocation and the label and type registries in
      `meta`. `src/storage/props.rs`: msgpack helpers. `src/storage/fts.rs`: full-text postings and document tables.
    - `src/graph/mod.rs`: `Graph`, `ReadTxn`, `WriteTxn` definitions and lifecycle methods (`open`, `view`, `update`, `backup`, `restore`,
      `rebuild_csr`). `src/graph/node.rs` and `src/graph/edge.rs`: node and edge CRUD and adjacency. `src/graph/index.rs`: the label index, property
      indexes, constraints, and property scans. `src/graph/txn.rs`: `ReadTxn` and `WriteTxn` delegation and transaction tests.
    - `src/graph/stats.rs`: cardinality statistics and the data-graph schema for the optimizer (crate guide, "Schema Statistics").
    - `src/graph/fts_mod.rs`: full-text index lifecycle. `src/graph/vector.rs`: vector byte storage.
    - `src/graph/algo.rs`: public algorithm dispatch and the snapshot freshness gate. `src/graph/kernels/`: the algorithm implementations over the
      CSR snapshot, split into `traversal.rs`, `analytics.rs`, `paths.rs`, and `flow.rs` (crate guide, "Algorithm Kernels").
    - `src/csr.rs`: the CSR snapshot, its incoming view, the `GraphDelta` write buffer, the `CsrChange` records the incremental refresh applies, and
      the generation counters (crate guide, "CSR Snapshot Vs. LMDB Adjacency").
    - `src/columns.rs`: in-memory typed property columns and per-property statistics (crate guide, "In-memory Property Columns"). `src/histogram.rs`:
      the equi-depth histogram behind the selectivity estimates; nothing here is persisted.
    - `src/threads.rs`: the one resolution of the thread budget every parallel consumer shares (crate guide, "Thread Count").
    - `src/cache_file.rs`: the on-disk cache files for the CSR snapshot and the property columns (`lmdb` feature only), keyed by database identity and
      commit generation and refused on any mismatch. The only save sites are `Graph::rebuild_csr` and the `materialize_*_columns` methods; no lazy
      build writes a file as a side effect of a query.
    - `src/error.rs`: the `Error` enum; `Error::Storage` carries the selected backend's error type.
- `crates/issundb-cypher/`: Cypher parser, AST, logical planner, physical planner, optimizer, and executor.
    - `src/parser.rs`: the `chumsky` parser with a Pratt expression parser, the parse cache, and compiler-style diagnostics (crate guide, "Parser
      Structure Rules" and "Parse Diagnostics"). `src/ast.rs`: AST types. `src/plan/`: planners, optimizer, and statistics helpers.
    - `src/procedure.rs`: the `ProcedureRegistry` for `query_with_procedures`. `src/builtin_procs.rs`: the built-in `issundb.*` procedures, resolved
      against a `CALL` clause before planning; path algorithms other than `shortestPath` and `dijkstra` are deliberately excluded.
    - `src/exec/mod.rs`: entry points (`execute`, `explain`) and shared types. `src/exec/read.rs`: `execute_physical`, the read-path helpers, and the
      plan cache. `src/exec/vectorized.rs`: the columnar fast path (crate guide, "Vectorized Aggregate and Columnar Fast Path").
      `src/exec/factorize.rs`: `FactorizedRecordGroup`. `src/exec/expr.rs`: expression evaluation. `src/exec/write.rs`: mutation execution.
      `src/exec/row.rs`: `SlotRow` and `SlotSchema`.
    - `src/exec/ddl.rs`: DDL execution. A node `CREATE INDEX` provisions the full-text index, because node property lookups are served by the
      always-on auto-index; a relationship `CREATE INDEX` provisions the property index.
    - `src/exec/copy.rs`: `COPY ... FROM`, `EXPORT DATABASE`, and `IMPORT DATABASE`. An import streams rows into one transaction as they decode; do
      not reintroduce a collect-then-write pass, which held every row on the heap at bulk-load scale. Both import entry points end with `rebuild_csr`
      and `materialize_property_columns`, so the imported database reopens without either scan.
- `crates/issundb-vector/`: the vector index behind `backend.rs`, selected at compile time from the default-on `hnsw` feature: `usearch` (the
  workspace's only C++ dependency) or a pure-Rust exact scan. The fallback is exact rather than a stub, so one suite proves both (crate guide, "The
  Backend Seam").
- `crates/issundb-text/`: text query APIs and ranking (the `Scorer` trait, BM25, `TextGraphExt`, `TextIndexExt`). Tokenization and the inverted-index
  storage live in `issundb-core`, because the postings are written inside the same transaction as the node record and a tokenizer here could not be
  reached from there without inverting the dependency. Queries tokenize through core's `tokenize_text` so indexing and querying cannot disagree.
- `crates/issundb-retrieval/`: hybrid retrieval over graph traversal, vector hits, text hits, score fusion, and subgraph materialization.
- `crates/issundb/`: public facade. Re-exports the deliberate public surface; do not re-export internal storage types. `benches/` holds the
  Criterion optimizer benchmarks (`query_optimizer`, `skewed_schema`, `cyclic_enumeration`) and two profiling drivers (`profile_triangle`,
  `profile_query`).
- `crates/issundb-cli/`, `crates/issundb-rest/`, `crates/issundb-mcp/`, `crates/issundb-py/`, `crates/issundb-wasm/`, and `crates/issundb-examples/`:
  consumers that depend only on `issundb`. `issundb-wasm` is the only crate built for `wasm32-unknown-unknown`, with `--no-default-features`, which is
  what proves the storage-backend seam and the pure-Rust kernels hold. Do not add `--features hnsw` to that build: it selects `usearch` and fails
  compiling `cxx`. Binding conventions and build flags are in `web/README.md`.
- `web/`: the playground page that loads that module, vanilla ES modules with no build step. `demos.js` holds the example catalog and the procedure and
  function references, which are Cypher inside JavaScript and therefore invisible to every Rust test; `make playground-check` runs them and is the only
  thing keeping them from drifting. Everything else about the page is in `web/README.md`.
- `crates/*/benches/`: crate-local Criterion targets. `crates/issundb/tests/conformance/`: the openCypher TCK subset.
- `benchmarks/ladybugdb-compare/`: the differential and timing harness against LadybugDB, excluded from the workspace (own `[workspace]`, root
  `exclude`, own `rust-toolchain.toml`) because `lbug` links a C++ library and needs a newer Rust than the MSRV; it must never join `make build` or
  `make test`. Run `make test-ladybugdb` for correctness and `make bench-ladybugdb` for timing. Cross-engine harnesses belong here, not in `benches/`.
  Its rules: the curated `differential_workload` binds no edge to two relationship slots (the pinned
  LadybugDB build permits the reuse openCypher forbids); the generated corpus (`LADYBUGDB_COMPARE_GENERATED`) is adjudicated by a brute-force
  `reference_rows` oracle, so a divergence names the database at fault; the timed `workload` treats a `DIVERGENT` verdict as an attributed LadybugDB
  walk-semantics overcount and a `MISMATCH` as a failure. Add a correctness query to the differential corpus, not to the workload.
- `Cargo.toml`: workspace root with shared `[workspace.dependencies]`. A shared dependency belongs there and a crate reaches it with `workspace = true`;
  a dependency used by one crate only stays on that crate. A new declaration that bypasses a root entry is a regression.
- `Makefile`: developer workflow entry points.
- Directory-scoped guides: `crates/issundb-core/AGENTS.md`, `crates/issundb-cypher/AGENTS.md`, `crates/issundb-text/AGENTS.md`, and
  `crates/issundb-vector/AGENTS.md` carry the crate-specific rules this file does not repeat. Read the one covering the crate being changed, and update
  it in the same patch when its subject changes. `web/README.md` plays the same role for the playground page.

## Testing Layout Rules

- Unit tests for `issundb-core` belong in `#[cfg(test)]` blocks inside the relevant source file. Each test that touches LMDB must open a fresh
  `tempfile::TempDir` and must not share state with other tests.
- Integration tests that exercise multiple crates belong in `tests/` at the workspace root or in `crates/issundb/tests/`.
- Cypher conformance tests belong in `crates/issundb/tests/conformance/` and are gated on `ISSUNDB_CONFORMANCE=1` so the default `make test` stays
  fast (`make test-conformance`).
- Property-based tests (via `proptest`) belong alongside the unit tests for the module whose invariants they exercise.
- The row pipeline is the differential oracle for every shape-specific fast path. `ISSUNDB_ROW_PIPELINE_ONLY=1` keeps the columnar executor, the
  `PathCount`, `GroupedDegree`, and `TriangleCount` kernels, the fused `ExpandIntersect` hop, the metadata count shortcut, and the type-inference pruning
  pass out of the answer, so any suite can be swept through the general path and compared. Both `cargo test` and `ISSUNDB_CONFORMANCE=1` runs must pass
  identically with and without it; a divergence is a fast-path defect. A test whose premise is that a particular operator lowers, and the fast half of
  any differential comparison, must pin the setting with `exec_mode::fast_paths_required`. The corpus lives in
  `crates/issundb-cypher/src/exec/differential.rs`. `VectorTopK` is deliberately outside the switch, because an HNSW search is approximate.
- Do not reach into `issundb-core` internals from integration tests; drive behavior through the `issundb` facade or the `Graph` API.
- If you move code across modules, move or rewrite the unit tests with it.
- Benchmark targets live in crate-local `benches/` directories; do not add `#[bench]` to source files.

## Architecture Constraints

- Adjacency is stored as LMDB `DUPSORT + DUPFIXED`: each duplicate value under a node key is one raw `AdjEntry` (20 bytes). A single `db.put` appends
  one entry in O(log n); there is no read-modify-write of a blob.
- The label index (`label_idx`) uses 12-byte composite keys `(u32 BE, u64 BE)` with `Unit` values, so a prefix scan enumerates a label's nodes in
  ascending ID order. A multi-label node has one entry per label. There is no edge type index: `edges_by_type` is one filtered pass over `edges`, which
  iterates in ascending edge id, per-type counts come from the `stats:t:` counters, and the counting kernels and the Cypher executor read the CSR
  snapshot.
- Property indexes (`node_prop_idx`, `edge_prop_idx`) embed the encoded value in the LMDB key, so an indexable value is bounded by LMDB's 511-byte
  key limit. `encode_property_value` declines a string longer than `MAX_INDEXED_STRING_LEN`, leaving that value out of the index; the property is still
  stored, and equality lookups fall back to a scan that compares the stored value, so results stay correct. Long text belongs in a full-text index.
- The CSR snapshot backs the graph algorithms, pattern matching, and multi-source expansion. It is kept fresh on demand through one gate,
  `Graph::ensure_snapshot_fresh`, reached by `Graph::with_snapshot`; the one freshness condition is the installed `snapshot_gen` against the committed
  `write_gen`. A refresh after additions patches the installed snapshot from the `CsrChange` each commit recorded; a removal, an edge update against a
  weighted snapshot, or a pending list past `INCREMENTAL_MAX_EDGES` builds from storage. The rules are in the crate guide under "CSR Snapshot Vs. LMDB
  Adjacency".
    - `Graph::open` builds nothing: it installs an unbuilt placeholder that reports stale, so the gate does the first build when a consumer needs one.
      Point lookups, property reads, and small typed expansions read LMDB directly and never build it. Do not reintroduce an eager build in `open`.
    - The first build in a process serves from the CSR cache file when its persisted commit generation matches storage; any mismatch falls through to
      the build.
    - `shortest_path_dijkstra` is the one consumer needing per-edge weights and goes through `Graph::with_weighted_snapshot`. The weight request is
      sticky (`CsrCache::request_weights`), or a workload alternating Dijkstra with anything else would rebuild twice per write; `Graph::rebuild_csr`
      does not ask, because every bulk load calls it.
    - Typed bulk expansion over at most `STALE_POINT_EXPAND_MAX` sources skips a stale snapshot and reads per-source LMDB adjacency, so an interleaved
      write-then-expand workload never pays a rebuild. The background rebuild after `REBUILD_THRESHOLD` writes is a compaction safety net, not the
      freshness path. Point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`) read the adjacency stores through the transaction,
      never the snapshot, so they always reflect committed and in-transaction writes.
- `Storage::open` is the only entry point for the storage engine, selected at compile time from the `lmdb` feature: LMDB by default, the in-memory
  backend with `--no-default-features`. `heed` is named in exactly two places, both inside `storage/`; everything else names the aliases
  `storage::{RoTxn, OwnedRoTxn, RwTxn}` and the eleven tables on `Storage`. Do not reintroduce a `heed::` path outside `storage/`, and do not turn this
  into a trait: a trait would make `Graph` generic over its backend and push that parameter through every crate and the public API. Three guarantees of
  the contract on `storage/mod.rs` are load-bearing: key order is byte order (a `u64` key is big-endian, which lets the CSR build assume `out_adj`
  arrives grouped by ascending node id), duplicate order is byte order, and an uncommitted transaction publishes nothing. The in-memory backend is
  copy-on-write for the third, which is what makes a read transaction opened while a write transaction is live legal; `MATCH ... CREATE` depends on
  exactly that, and a single reader-writer lock deadlocks on it. The in-memory backend does not persist; tests whose premise is reopen or backup are
  gated on the `lmdb` feature.
- `Graph::update` is the transaction and the only unit of atomicity. An `Err` out of the closure rolls back everything the closure wrote, LMDB
  supplies the isolation, and the environment is opened with nothing but `map_size` and `max_dbs`: no `MDB_NOSYNC`, `MDB_NOMETASYNC`, or `MDB_WRITEMAP`,
  so a commit fsyncs and an acknowledged write survives a crash. That is why a single-record insert costs one commit's latency and why the batch forms
  of the binding APIs exist. Do not buy write throughput by relaxing a sync flag: durability is claimed at every documented surface, so weakening it is
  an API change. Four boundaries qualify the guarantee.
    - The transaction covers the node and edge records, both adjacency stores, `label_idx`, both property indexes, the constraints those indexes
      enforce, and the full-text postings. Every other secondary structure is a cache.
    - The transaction does not cover the vector index. `VectorGraphExt::upsert_vector` updates the in-memory HNSW and then opens its own write
      transaction for the stored bytes, so an embedding can neither join a caller's `Graph::update` nor roll back with it. Index before storage is
      deliberate: a crash between the two loses an in-memory entry the next reopen rebuilds, where the reverse would make a rejected vector durable. A
      caller needing the bytes to land atomically with graph writes stages them through `WriteTxn::put_vector_bytes`.
    - Atomicity is per statement, not per script. The Cypher grammar has no `BEGIN` or `COMMIT`, so a semicolon-separated pipeline runs each statement
      in its own `Graph::update`, and only a Rust caller can group operations. Do not paper over this with a retry or a compensating write in a
      consumer crate; multi-statement transactions belong in the query layer as a language feature.
    - Durability is what the in-memory backend gives up; atomicity, consistency, and isolation hold there.
- `issundb-cli`, `issundb-rest`, and `issundb-mcp` relay `lmdb` and `hnsw` to the facade rather than naming them on the dependency, so a binary can be
  built without the C vector index (`release.yml` needs this for `aarch64-pc-windows-msvc`, where `usearch` does not compile). Do not put
  `features = ["lmdb", "hnsw"]` back on those dependencies; it makes the feature unselectable from the command line.
- Both features are forwarded by every crate between the facade and core, each declaration carrying `default-features = false`, or a sibling silently
  re-enables the default for the whole graph. Verify a change to that plumbing with `cargo tree -p issundb --no-default-features | grep -iE "usearch|heed"`,
  which must print nothing. A whole-workspace `--no-default-features` build does not select the in-memory backend, because the consumer crates depend
  on the facade with its defaults and cargo unifies features; test the backend per crate (`cargo test -p issundb-core --no-default-features`).
- `chumsky` is an active, non-optional dependency; `usearch` is the workspace's only C++ dependency. The graph algorithms are pure Rust over the CSR
  snapshot, so the build needs no CMake, Clang, bindgen, or OpenMP runtime.
- Async is not used in the core engine. `tokio` is for server mode only; do not add `.await` inside `issundb-core`.
- Parallelism has two consumers, and both resolve their thread count through `threads::resolve`: the scoped-thread reductions in the counting kernels
  and the analytics passes that split over nodes or sources. Both split only above `MIN_PARALLEL_WORK` items, so a small pass and a unit test stay
  serial. `Graph::kernel_threads` caps at `MAX_SCAN_THREADS` because a pass streaming the adjacency arrays saturates memory bandwidth and gets slower
  past that point; `Graph::parallel_threads` leaves the all-pairs passes uncapped. PageRank and harmonic centrality are split-invariant; betweenness
  sums per-worker partials, so the last bits of a total depend on the worker count. Writes are never parallel.

## Dependency Boundaries

Target dependency direction:

1. `issundb-core` sits at the bottom. It must not depend on the vector, text, retrieval, Cypher, bindings, server, or CLI crates.
2. `issundb-vector` may depend on `issundb-core`, but not on text, retrieval, Cypher, bindings, server, or CLI crates.
3. `issundb-text` may depend on `issundb-core`, but not on vector, retrieval, Cypher, bindings, server, or CLI crates.
4. `issundb-retrieval` may depend on `issundb-core`, `issundb-vector`, and `issundb-text`.
5. `issundb-cypher` may depend on public APIs from core, vector, text, and retrieval crates, but not storage internals.
6. `issundb` composes and re-exports the stable public API.
7. `issundb-cli`, `issundb-rest`, `issundb-mcp`, `issundb-py`, and `issundb-wasm` depend only on `issundb`.

Lower-level crates must not know about higher-level crates.

## Component APIs

### `issundb_core::Graph`

The central coordination type. All graph operations go through `Graph`; do not call `Storage` directly from outside `issundb-core`.
`Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>` is the only constructor.

Node and edge CRUD, accessors, and registry lookups have self-describing signatures; read them from the source. Only the methods below carry
behavior the signature does not show.

Read-path and statistics methods:

- `node_prop_json(id, prop)`, `node_props_json_table(ids, props)`, and `node_prop_json_column(ids, prop)`: `None` (or `Error::NodeNotFound` for the
  bulk forms) for a nonexistent node, `Value::Null` for a missing property. They read through the in-memory property columns once those exist, but a
  request of up to `SMALL_GATHER_MAX` ids is served as LMDB point reads instead (`should_serve_directly`), because building every column is one full
  node scan and a query touching a handful of nodes must not pay it. The size test is on the request, not the method. Sustained direct reads amortize
  the build after `DIRECT_READ_BUILD_THRESHOLD` of them. `node_prop_group_codes` follows the same size test.
- `materialize_property_columns()` and `materialize_edge_property_columns()`: build the columns now. Nothing builds them as a side effect of a small
  workload, so this is the deliberate way to make selectivity estimates and zone-map pruning available on a cold graph. Each is also its cache file's
  save site. The one caller that materializes without being asked is a bulk load (`COPY ... FROM`, `IMPORT DATABASE`).
- `node_prop_group_codes(ids, prop)`: dense group codes under exact value identity plus one representative value per code; null and missing share one
  code. `node_prop_group_codes_by_id(prop)`: the id-indexed whole-graph form (`ID_GROUP_ABSENT` where no such node exists), cached per write
  generation; a small request wants the per-request form, which never builds whole-graph state.
- `node_prop_min_max(prop)`, `estimate_range_selectivity(prop, lower, upper)`, and `estimate_equality_selectivity(prop, val)`: advisory readers that
  never build the property columns and return `None` when the columns do not exist, leaving the caller on its default plan weight. A caller that needs
  statistics on a cold graph must call `materialize_property_columns` first.
- `estimate_expand_fanout(src_label, rel_type, incoming)` and `estimate_expand_fanout_to(src_label, rel_type, dst_label, incoming)`: the per-label
  "expand ratio" that sharpens the optimizer's `Expand` weight; `None` when a name is unknown, no such edges exist, or no usable statistics table
  exists. Neither builds the table; `materialize_edge_statistics` is how a caller asks. They accept a table the write generation has moved past,
  because the alternative is no estimate at all, bounded per relationship type: a stale table is refused for a type once its live `stats:t:` count
  exceeds `STALE_FANOUT_GROWTH_FACTOR` times the count at build time. That catches a type that grew (a stale estimate understates its fan-out) and not
  one that shrank (the planner is merely conservative). An estimate weights a plan and never changes an answer.
- `materialize_edge_statistics()`: build the schema statistics table now, one pass over `label_idx` and one over `out_adj`, cached until a committed
  write advances the generation. It makes the expand-ratio estimates available and upgrades `schema_has_edge` from a budgeted probe to an exact lookup.
- `schema_has_edge(src_label, rel_type, dst_label) -> Result<Option<bool>, Error>`: whether the committed data contains any directed edge
  `src_label --rel_type--> dst_label`. `Some(false)` means the pattern is provably unsatisfiable; `None` when a name is unknown or the question could
  not be settled within `SCHEMA_PROBE_BUDGET`. Not advisory, since a negative drops rows, so it never depends on the statistics table: with no current
  table it walks the smaller endpoint population through `label_idx`, charging every storage operation against the budget. Only the choice of side
  reads a stored counter; the emptiness shortcut asks `label_idx`, so a prune never rests on a counter being exact. Verdicts are memoized per write
  generation.
- `storage_table_stats() -> Result<Vec<TableStat>, Error>`: size and entry count of each storage table, in declaration order. On LMDB `bytes` is the
  table's pages times the page size, so the entries sum to the live data in the file without free-page slack; the in-memory backend reports summed key
  and value lengths and no page count. The CLI's `stats` command prints it.
- `plan_generation() -> (u64, u64, u64)`: what a cached query plan is valid for: a per-open identity nonce, the committed write generation, and a
  schema generation that index and constraint DDL and the `materialize_*` builders advance. Those changes alter plans without being data writes, and
  they do not advance the write generation because that would mark the CSR snapshot stale for nothing.
- `label_filter(nodes, label)`: the subset of `nodes` carrying `label`, one `label_idx` point lookup per candidate.
- `nodes_by_label_arc(label)`: `nodes_by_label` without the copy, served from a per-generation cache that any committed write discards;
  transaction-scoped label reads bypass it, because an open write transaction must see its own uncommitted labels.
- `nodes_prop_cmp_mask(ids, prop, op, rhs) -> Result<Option<Vec<bool>>, Error>`: the per-id outcome of `prop <op> rhs` against the typed column, with
  Cypher filter semantics (null and missing fail every operator, mixed int and float compare through `f64`, a kind-mismatched non-null value passes
  only `Ne`). `Ok(None)` declines, and the caller falls back to the boxed comparison.
- `set_thread_count(n: i32)`: sets the thread count for the parallel read passes, overriding `ISSUNDB_NUM_THREADS`; `0` restores the default. There
  is no pool; each pass resolves the budget when it starts, so the call cannot fail.

Graph algorithms are the public methods of `graph/algo.rs`. Several carry behavior a signature cannot show:

- `shortest_path_dijkstra(src, dst)`: the edge weight is the first present of `weight`, `cost`, `capacity`, or `cap`, default `1.0`, so unlike
  `shortest_path_top_k` and `spanning_forest` it takes no weight-property argument. A negative weight is a data condition: the pass falls back to a
  bounded label-correcting relaxation when the snapshot reports any (`has_negative_weight`, decided at build time), and a reachable negative cycle is
  `Error::InvalidArgument`.
- `connected_components()` and `louvain()`: the component id is the smallest node id in the component. Only the induced partition is contractual;
  compare membership, not numbering. `louvain` is deliberately serial, because local moving is order-dependent and splitting it over workers would
  make the partition depend on the worker count.
- Parallel edges follow three rules, chosen by what each score means. `betweenness_centrality`, `degree_centrality`, `clustering_coefficient`, and
  `link_prediction_score` count distinct neighbors (a self-loop counts in each direction for degree). `page_rank`, `eigenvector_centrality`, and
  `katz_centrality` count every edge, since each edge is a path for influence. `louvain` weights an edge by its multiplicity. Do not change one without
  its test. `betweenness_centrality` is unnormalized and directed. `page_rank` does not redistribute dangling-node mass, so ranks do not sum to 1;
  `tests/oracle.rs` compares against NetworkX over graphs with no dangling nodes for that reason.
- `closeness_centrality()`: Wasserman-Faust closeness, `(reachable / total_distance) * (reachable / (n - 1))`, which stays usable on a disconnected
  graph.
- `eigenvector_centrality(iterations, tolerance)` and `katz_centrality(alpha, beta, iterations, tolerance)`: bounded rather than fallible; each stops
  early on convergence and otherwise returns the estimate after the budget. Katz needs `alpha` below the reciprocal of the largest eigenvalue.
- `count_triangle_cycles(spec)`: assignment count of the directed triangle pattern with optional per-hop types and per-variable labels, following
  Cypher row semantics including relationship uniqueness. `count_linear_paths(spec)`, `grouped_edge_counts(spec)`, and
  `typed_neighbor_counts(sources, spec)`: the other counting kernels the Cypher optimizer lowers to; a source absent from the snapshot counts zero.
- `prefers_point_expansion(sources) -> bool` and `adjacency_span(sources, incoming)`: advisory sizing calls the Cypher executor uses to choose an
  evaluation route. Both routes return the same rows, and `adjacency_span` deliberately does not refresh the snapshot, so it under-reports after a
  write; a caller treating a low span as cheap declines an optimization rather than computing a wrong answer.

### `issundb_vector`

- `VectorGraphExt::configure_vector_index(opts)`: sets the per-graph metric and quantization, persisted in `meta`. Call it before the first upsert;
  changing either once vectors exist returns `VectorError::AlreadyConfigured`, and `reindex_vector_index` is the explicit, O(n) way to change them
  afterward.
- `VectorGraphExt::upsert_vector(n, v)`: rejects a node that does not exist with `VectorError::NodeNotFound`, because node ids are monotonic and a
  vector accepted ahead of its node would be inherited by the next node allocated that id. `remove_vector` stays permissive. It is not transactional
  with graph writes; see the Architecture Constraints entry on `Graph::update`.
- Searching a graph with no stored embeddings returns `VectorError::EmptyIndex` rather than an empty hit list; the Cypher `VectorTopK` operator maps
  that to zero rows.
- `vector_search_with(q, opts)`: adds an exact-label filter and property equality filters, both evaluated during the traversal, and `rescore_factor`.
  A quantized index fetches `2k` candidates and re-ranks them by exact distance by default; `Some(1)` disables that, and a `Float32` index never
  rescores by default.

### `issundb_text`

- `text_search` errors instead of returning a silent empty list when the request cannot match anything: an empty query (`EmptyQuery`), a filter naming
  no active index (`LabelNotIndexed`, `PropertyNotIndexed`, `IndexNotFound`), or a graph with no text indexes (`NoIndexes`).
- `TextHit` carries `node`, `score`, and the `label` and `property` of the index that contributed the largest partial score.

### `issundb_retrieval`

All retrieve functions are free functions, not methods on `Graph`, to preserve the crate boundary. `retrieve_hybrid` returns
`RetrievalError::NoQuery` when neither modality would run (both inputs empty or both k values zero). `Subgraph::truncated` is true when the
`max_nodes` cap cut off seeds or expansion, so a capped result is distinguishable from a complete one.

### `issundb_cypher`

Exposed through the `issundb` facade via the `GraphQueryExt` trait; do not call `issundb_cypher::execute` directly from outside `issundb`.

- `QueryResult::statement_count` is how a caller notices a semicolon-separated query, whose `columns` and `records` reflect only the last statement.
- When one `AND`/`OR` operand alone determines the result, a runtime error in the other operand is suppressed, but a successfully evaluated
  non-boolean operand still raises, as the TCK requires (`false AND 123` raises, `false AND (1 / 0)` is `false`).
- A single statement's write clauses and the `RETURN`/`WITH` that follows them share one `Graph::update`: an error anywhere rolls back every write of
  the statement. A projection reading a variable bound by an earlier write in the same statement sees the fresh value through the pending-writes
  overlay in `exec/expr.rs`. A `MATCH` after a write clause does not see that write's structural effect until the statement commits, because label
  scans and the snapshot read committed state only.
- Atomicity stops at the statement. Each top-level statement of a pipeline gets its own `Graph::update` in `execute_pipeline`; the binding
  environment does cross statements, so a surviving variable is no evidence that a transaction did.
- Plans are cached per thread (`PlanCache` in `exec/read.rs`, 256 entries), keyed on the query's address, `Graph::plan_generation`, and the
  execution mode, and served only when the stored query equals the incoming one after SKIP and LIMIT parameter resolution. A plan holding a resolved
  `CALL` is never cached, because it embeds the procedure's rows; the `EXISTS` subquery body uses the uncached entry point. Parsing is cached
  separately in the parser.

Key optimizer behaviors, each navigable by the named symbol:

- Top-level `AND` conjunctions in WHERE split so each conjunct pushes down to its own lowest binder.
- An equality or range filter over a labeled scan rewrites into `NodeIndexScan` or `NodeRangeScan`; the rewrite recurses through every single-input
  operator.
- A correlated equality whose key is bound at runtime rewrites into a `CorrelatedIndexSeek` (`rewrite_correlated_seek`), one seek per outer row, when
  one join side is a bare `LabelScan` for the seek variable.
- A natural inner `HashJoin` whose one side merely re-scans a variable the other binds rewrites into an "expand into" chain (`rewrite_join_to_expand`),
  never across an `OptionalMatch`.
- A final projection or aggregation over a linear chain of up to `MAX_VEC_HOPS` directed hops executes column-at-a-time (`exec/vectorized.rs`); every
  other shape runs the row pipeline.
- A grouping-free `count` over a one-hop or two-hop directed expansion lowers to `PathCount`, with per-vertex `prop CMP literal` predicates pushed down
  as index-resolved allow-sets. A `count` grouped by one endpoint of a single hop lowers to `GroupedDegree`, whose groups are folded through dense
  integer codes and emitted in the row pipeline's canonical key order, so both paths agree without an `ORDER BY`.
- An `ORDER BY <count> LIMIT n` above a grouped count pushes a `CountWindow` into the group producer. The window keeps every group reaching the `n`-th
  best count, ties included, in the order the full set would have had. It declines when the leading sort key is not the count, when a `Distinct` sits
  between the sort and the projection, or when any projected item is more than a variable or property read.
- `RETURN DISTINCT` deduplicates before `ORDER BY` and `SKIP`/`LIMIT`; `WITH DISTINCT` deduplicates full rows behind its barrier; only
  `RETURN DISTINCT *` deduplicates after projection in the executor.
- The type-inference pass (`prune_unsatisfiable`) wraps an `Expand` that `Graph::schema_has_edge` proves empty in a `Limit` of zero. It runs only on
  read-only plans and prunes only on a definitive negative.
- The plan-weight cost model applies the statistics at every hop (`collect_label_constraints`, `plan_weight`, `label_of_var`); a cyclic pattern
  closed by a `MultiwayJoin` is weighted by its closing edge's per-pair probability (`closing_selectivity`).

### `issundb_rest`

Axum and Tokio server, depending only on `issundb`, all handlers sharing one `Arc<Graph>`. Routes are versioned under `/v1`; `GET /health` stays
unversioned so infrastructure probes do not track the API version. The route list and request shapes are in `docs/integrations.md`.

- REST exposes the data plane and retrieval only. Index administration, thread control, and backup and restore are intentionally absent; those are
  done through the CLI or the Python surface.
- Startup spawns `Graph::materialize_edge_statistics` on a detached thread, deliberately not synchronous: the scan costs seconds on a large graph and
  the plans it sharpens gain a few percent, so readiness must not wait on it. Backgrounding is safe only because the build scans without holding the
  statistics lock and installs the table at the end; do not move it back under the lock. `--no-warm-statistics` (or `ISSUNDB_NO_WARM_STATISTICS`)
  skips it. The property columns are not warmed: that build holds every scalar property in memory, which is an operator's decision. The engine logs
  a warning whenever it builds the columns for want of a current cache file, and both servers default their `EnvFilter` to `info` so the warning is
  seen.
- The OpenAPI 3.1 document is generated from `#[utoipa::path]` annotations and `ToSchema` derives (`utoipa`, `utoipa-scalar`, both permissively
  licensed), served at `GET /v1/openapi.json` with a Scalar UI at `GET /v1/docs`. The handlers build JSON with `json!`, so the documentation-only
  response structs must be kept in sync with those literals.

### `issundb_mcp`

`rmcp` server, depending only on `issundb`, serving over stdio (default) or MCP's Streamable HTTP transport. Diagnostics go to `stderr` because the
stdio transport owns `stdout`. `rmcp` is pinned to `0.11` because later versions exceed the workspace MSRV; since that version does not validate the
`Host` header (GHSA-89vp-x53w-74fx), the HTTP arm wraps the router in a `Host` allowlist middleware (loopback names plus the `--bind` host, extended
with `--allowed-host`), answering 403 otherwise.

- The tool surface is curated for an LLM agent: `get_node`, `get_edge`, `cypher_query`, `explain`, `text_search`, `vector_search`, and
  `retrieve_hybrid`. There are no typed mutation tools; mutations are Cypher through `cypher_query`. Keep it minimal: every additional tool dilutes the
  agent's tool selection. Responses are bounded (`max_property_chars`, value excerpts) and self-describing (`truncated`, matched label and property).
- `get_node` and `get_edge` take the internal engine id, the value Cypher's `id()` returns, never a domain property such as `Id`; the two numbering
  spaces can collide, so a domain id passed straight in silently returns the wrong entity. The tool descriptions say so, and `expect_label` and
  `expect_type` reject a mismatched entity with an error.
- Startup warms the edge statistics on a detached thread as REST does, and for a stronger reason: a stdio client launches one subprocess per
  session, and a synchronous scan would land on the initialize handshake. `issundb-cli` warms synchronously on every open and takes the same flag; a
  visible pause before an interactive prompt is honest. `materialize-columns` is the CLI's only way to build and persist the property columns, since
  `:import-nodes` does not warm them the way `COPY ... FROM` does. `issundb-py` does not warm on construction and exposes the `materialize_*` methods
  instead; a comparison measured through the Python binding without calling them measures the planner with its statistics unavailable, which is worth
  stating.

### `issundb_py`

PyO3 bindings exposing one `IssunDB` class, depending only on `issundb`; the `extension-module` feature must be enabled. The method list is in
`docs/python.md`.

- `add_nodes` and `add_edges` write a whole batch under one `Graph::update` and are all-or-nothing; a Python loop over `add_node` is bound by commit
  latency.
- Every method releases the GIL around the native call. Keep that invariant when adding one: extract arguments to owned Rust values first, run the
  engine call and JSON serialization inside `Python::detach`, and never touch a Python object in the released section.

### `issundb_wasm`

Browser bindings exposing one `Playground` owning a single `Graph`, depending only on `issundb`. Every method returns a JSON string, so the boundary
carries one type in both directions. The surface is curated like the MCP one: reads, queries, and the two capabilities Cypher cannot reach (full-text
index creation and search, vector upsert and search); mutations are Cypher through `query`; `backup` and `restore` are absent on a target with no
filesystem. `memoryBytes` comes from a counting `GlobalAlloc` this crate alone installs, so no other crate pays for it. `buildRef` is compiled in from
`ISSUNDB_BUILD_REF` so it cannot disagree with the module it describes; `build.rs` exists only to declare `rerun-if-env-changed` for it.

Two structural rules. Methods split into a private logic layer returning `Result<_, String>` and a thin exported layer converting to `JsError`,
because constructing a `JsError` panics on a non-wasm target and the logic layer must stay testable under `cargo test`. And the wasm-bindgen CLI must
be the exact version of the wasm-bindgen crate, which `make playground-build` checks before running.

### `issundb_core::Storage`

Internal to `issundb-core`. Owns the environment and eleven tables: `nodes`, `edges`, `out_adj`, `in_adj`, `label_idx`, `node_prop_idx`,
`edge_prop_idx`, `fts_postings`, `fts_docs`, `vectors`, and `meta`. Do not expose `Storage` through the `issundb` facade.

### `issundb_core::error::Error`

All `issundb-core` errors unify here: storage, encoding, decoding, and domain errors (`NodeNotFound`, `EdgeNotFound`). Do not leak `heed` error types
through the public facade.

### Encapsulation Rule

`Storage` and the `storage` module are `pub(crate)` inside `issundb-core`. The `issundb` facade re-exports only `Graph`, `Error`, `Hit`, the hybrid
retrieval types and functions, the Cypher result types, the schema ID and record types, and the counting-kernel spec types, which are re-exported
because a `Graph` method whose argument type cannot be named is not callable. Do not add a "just for now" re-export anywhere else; add a deliberate
testing helper in `issundb-core` if a test needs internal access.

## Workflow

Before coding:

1. Identification of whether this is a storage, query, vector, hybrid retrieval, bindings, or docs change.
2. Reading of the touched module, its crate guide, and nearby tests.

Implementation using red-green TDD:

1. A failing `#[test]` that describes the expected behavior (red). For invariants, prefer a `proptest` property.
2. Verification that the test fails for the right reason (red).
3. The smallest implementation that makes the test pass (green).
4. Refactor while keeping tests green.
5. Narrowest relevant test while iterating, then `make test` and `make lint` before declaring done.
6. `make format` before every commit.
7. Update of `README.md` or `docs/` if behavior or workflow changed.

Clippy is pinned to the MSRV in `lints.yml`, and a current clippy reports lints the pinned one does not. A lint step belongs in `lints.yml` beside
`make lint`, never in `tests.yml`, whose jobs run on stable. A clean local `make lint` says nothing about a newer clippy, so run `cargo +stable clippy`
before adding a lint gate.

Additional validation when relevant:

- `make test-backends` and `make lint-backends` for a storage or vector backend change; the default run selects LMDB and usearch, so nothing else
  exercises the in-memory backend or the exact vector index as the selected backend.
- `make bench` for performance-sensitive storage changes.
- `make test-conformance` for Cypher conformance coverage.
- `make test-ladybugdb` for differential correctness against LadybugDB, and `make bench-ladybugdb` for cross-engine timing.
- `make bench-search-data` to download the datasets behind the text, vector, and hybrid retrieval benchmarks; those benches skip cleanly when
  `ISSUNDB_BENCH_SEARCH_DIR` is unset.

## Testing Expectations

- No storage behavior change is complete without tests.
- Node insertion, edge insertion, adjacency consistency, ID uniqueness, and label or type registry correctness all need explicit coverage.
- Prefer targeted assertions (one field, one count, one round-trip) over broad snapshot tests.
- Keep tests deterministic. Each test opens its own `TempDir`; do not share LMDB environments across tests.
- When uncertain about storage correctness, add or refine tests first.

## Documentation Expectations

- Public API docs are generated from `rustdoc` on `crates/issundb/src/lib.rs`. Keep that module focused on the deliberate public surface.
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
