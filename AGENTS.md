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
  (`issundb-cli`, `issundb-rest`, `issundb-mcp`, `issundb-py`) use only the `issundb` facade. Do not import across those boundaries in the wrong
  direction; see Dependency Boundaries.
- Keep all mutable state inside `Graph` and `Storage`; do not introduce module-level `static mut` or `lazy_static` globals for runtime state.
- Writes are serialized via the `parking_lot::ReentrantMutex<()>` write lock on `Graph`; LMDB enforces the same constraint at the storage level. Do
  not bypass either.
- Add comments only when they clarify a non-obvious storage invariant, an LMDB lifetime constraint, or a GraphBLAS semiring choice.
- Maintain the permissive license boundary of the workspace (MIT or Apache-2.0). Do not add dependencies or statically link libraries with copyleft,
  weak copyleft, or source-available licenses (such as GPL, MPL, or SSPL). Keep comparison or benchmarking harnesses that link to such external
  engines excluded from the root Cargo workspace.
- Format with `rustfmt` (`make format`) and lint with Clippy (`make lint`) before declaring a change done.

Quick examples:

- Good: add a `Graph::bfs` method in `crates/issundb-core/src/graph/algo.rs` with unit tests using a temp LMDB directory.
- Good: add a Cypher parser test in `crates/issundb-cypher/src/` against the openCypher TCK subset.
- Bad: import `heed` directly in `crates/issundb/src/lib.rs` instead of going through `issundb-core`.
- Bad: store a node cache in a `static` `HashMap` outside `Graph`.
- Bad: add a cargo dependency to a workspace crate that pulls in a copyleft or source-available library.

## Writing Style

- Use Oxford commas in inline lists: "a, b, and c" not "a, b, c".
- Do not use em dashes. Restructure the sentence, or use a colon or semicolon instead.
- Avoid colorful adjectives and adverbs. Write "adjacency query" not "blazing adjacency query".
- Prefer noun phrases for checklist items over imperative verbs. Write "temp directory teardown" not "tear down the temp directory".
- Headings in Markdown files must be in title case: "Build from Source" not "Build from source". Minor words (a, an, the, and, but, or, for, in, on,
  at, to, by, of) stay lowercase unless they are the first word.
- Do not bold the lead-in of a list item. Write "Vector and set similarity: ..." not "**Vector and set similarity**: ...".
- Use sentence case for the lead-in of a list item. Write "Seed selection: ..." not "Seed Selection: ...". Proper nouns keep their capitals.
- Capitalize only the first part of a hyphenated compound: "Full-text Search" in a heading, "Breadth-first" at the start of a sentence, and
  "breadth-first search" elsewhere. Never write "Breadth-First".
- Start each sentence with a capital letter, capitalize proper nouns (Rust, Cypher, LMDB, GraphBLAS), and leave common nouns lowercase in the middle
  of a sentence.
- Write correct and complete sentences.
- Avoid made-up words.
- Do not use a colon in place of a verb. Three uses are fine: joining two clauses inside a complete sentence (the replacement the em-dash rule above
  calls for), introducing the gloss of a list item, and introducing an enumeration, whether as a list or inline ("Methods: `add_node`, `add_nodes`,
  ..."). What a colon must not do is turn a sentence into a label and a definition: write "Merges vector search seeds with text search seeds, then
  expands via BFS" rather than "Hybrid retrieval: merges vector search seeds with text search seeds". That shape belongs to a list item, and carrying it
  into prose (a doc comment summary, a paragraph) leaves a fragment where a sentence was required.
- Use participial phrases and abbreviations scarcely.

## Repository Layout

This map describes the current structure and the target decoupled crate boundaries. Do not invent modules that do not yet exist, but do place new
modules according to this map.

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
    - `src/graph/stats.rs`: high-order cardinality statistics and the data-graph schema for the optimizer. Owns the `(label, type)` edge-frequency
      table behind `estimate_expand_fanout` and the realized `(src_label, type, dst_label)` triples behind `estimate_expand_fanout_to` and
      `schema_has_edge`, built by one pass over `label_idx` and one over `out_adj` (neither decodes a record, since a `NodeRecord` or `EdgeRecord`
      decode also copies a property blob this table never reads) and cached against the committed-write generation. Nothing builds it as a side effect
      of a query, and the generation check gates use rather than refresh: a table from an earlier generation is ignored, never trusted. The two fan-out
      estimates are advisory and fall back to the global average without it, so only `Graph::materialize_edge_statistics` builds it. `schema_has_edge`
      is not advisory (the optimizer drops rows on a negative), so it never depends on the table existing: with no current table it probes `label_idx`
      and the adjacency directly under `SCHEMA_PROBE_BUDGET`, settling on the first matching edge and reporting `None` when the budget runs out.
    - `src/graph/fts_mod.rs`: full-text search index lifecycle and FTS storage primitives.
    - `src/graph/vector.rs`: vector byte storage helpers.
    - `src/graph/algo.rs`: public algorithm dispatch methods and internal traversal helpers.
    - `src/graph/graphblas/`: GraphBLAS algorithm implementations split by family: `traversal.rs`, `analytics.rs`, `paths.rs`, and `flow.rs`.
    - `src/graph/txn.rs`: `ReadTxn` and `WriteTxn` delegation impls and transaction tests.
    - `src/csr.rs`: in-memory CSR snapshot (outgoing arrays plus a transposed incoming view with per-edge type and edge ids), rebuilt in the
      background and swapped via `arc-swap`. Also owns the `GraphDelta` buffer captured on the write path and the `write_gen`/`snapshot_gen`
      generation counters that drive incremental matrix maintenance and on-demand CSR refresh.
    - `src/columns.rs`: in-memory property columns for the read path. One typed column (`Int`, `Float`, `Bool`, dictionary-encoded `Str`, or the
      exact-semantics `Json` fallback) per node property, built lazily from one full node scan and kept fresh by a post-commit delta (node deletion
      forces a rebuild). Read through `Graph::node_prop_json`. Also owns the lazily computed per-property statistics (`PropStats`: bounds, an
      equi-depth histogram, and the most common values) that back the selectivity estimates, invalidated by the post-commit patch. Which readers may
      cause the build is deliberate, because the build is one full scan: a gather larger than `SMALL_GATHER_MAX` does, a smaller one is served straight
      from storage (`should_serve_directly`), and the advisory statistics never do (`with_existing_mut` rather than `with_fresh`).
    - `src/histogram.rs`: equi-depth histogram over property values with equality and range selectivity estimates; backs `PropStats`. Nothing here is
      persisted.
    - `src/matrices.rs`: GraphBLAS matrix materialization from the CSR snapshot, plus `MatrixSet::apply_delta` for incremental in-place maintenance
      (resize plus per-element set and drop) and the self-contained `dense_to_id`/`id_to_dense` mapping the matrix-view consumers read.
    - `src/threads.rs`: the one resolution of the thread budget every parallel consumer shares (`threads::resolve`). Precedence is the programmatic
      override from `set_thread_count`, then `ISSUNDB_NUM_THREADS`, then `OMP_NUM_THREADS`, then the machine's parallelism, clamped to `MAX_THREADS`.
      Both the GraphBLAS pool (`MatrixSet::materialize`) and the counting kernels' scoped threads (`Graph::kernel_threads`) resolve through it, so the one
      knob has one meaning; resolving it in two places previously made an unset configuration mean one thread for the matrices and the whole machine for a
      kernel pass, letting the two pools oversubscribe each other. `OMP_NUM_THREADS` is honored because the GraphBLAS pool is an OpenMP pool and capping
      it is how this repository's own `test` and `coverage` targets keep the pools in check.
    - `src/error.rs`: `Error` enum; all storage and serialization errors unify here.
- `crates/issundb-cypher/`: Cypher parser, AST, logical planner, physical planner, optimizer, and executor.
    - `src/parser.rs`: Cypher parser built with the `chumsky` parser-combinator library (with a Pratt parser for operator-precedence expressions),
      covering MATCH (including inline relationship property maps and multi-label node patterns such as `(n:A:B)`), WHERE, RETURN, CREATE, SET
      (property and label assignment), REMOVE (label and property), and DELETE/DETACH DELETE over arbitrary expression targets. An iterative
      token-stream scan (`scan_nesting`) rejects genuinely pathological input (thousands of levels) with a parse error before any AST is built.
      Realistic deep input is kept safe by running on large stacks: a deep parse runs on a dedicated large-stack thread, and a query whose nesting
      exceeds `SMALL_STACK_EXEC_BUDGET_KB` has its execution dispatched to a large-stack thread by `execute_with_procedures`. Shallow queries, the
      common case, parse and execute inline on the caller stack.
      Building the combinator graph costs more than consuming the tokens, and for a small query more than executing it, so the executor's entry point
      (`parse_with_exec_depth`) serves repeated query text from a bounded thread-local cache of `Arc<Statement>` and returns the same allocation.
      Parsing reads no graph state, no parameters, and no clock, so the cached outcome (including a parse error, and including the nesting-depth
      rejection) is always valid and the cache needs no invalidation. `parse` is the uncached entry point: it always does the work, which keeps the
      `parse` benchmarks a regression guard on the parser itself. Query text over `PARSE_CACHE_MAX_QUERY_LEN` is parsed but not stored, so many large
      unique statements cannot grow the cache by their length.
    - `src/ast.rs`: AST node types.
    - `src/plan/`: logical planner, physical planner, optimizer, and statistics helpers.
    - `src/procedure.rs`: the `ProcedureRegistry` a caller passes to `query_with_procedures`, plus the argument and yield types a procedure sees.
    - `src/builtin_procs.rs`: the built-in `issundb.*` analytics, pathfinding, and retrieval procedures, resolved against a `CALL` clause before
      planning. Path algorithms other than `shortestPath` and `dijkstra` are deliberately excluded.
    - `src/exec/mod.rs`: public entry points (`execute`, `explain`), shared type definitions, and tests.
    - `src/exec/read.rs`: `execute_physical` and read-path helpers (`evaluate_where`, `evaluate_sort_key`, `json_to_prop_value`,
      `filter_over_expand_batch`, and `multiway_join_rows`, the last shared by the materializing and streaming `MultiwayJoin` paths).
    - `src/exec/vectorized.rs`: columnar fast path for the final projection or aggregation over a linear chain of up to `MAX_VEC_HOPS` directed
      single hops. A structural recognizer matches `[Limit]? [Sort]? [Distinct]? Project [Aggregate]? Stage* (Expand(directed single hop)
      Stage*){0,MAX_VEC_HOPS} Leaf` with single-property expressions, executing column-at-a-time (bulk expansion via `Graph::node_props_json_table`
      and group-by-code aggregation via `Graph::node_prop_group_codes`). A multi-hop chain is recognized only when every hop carries a distinct
      relationship type, so relationship uniqueness is vacuous; a repeated type or a chain longer than `MAX_VEC_HOPS` falls back. A non-distinct
      `count` over the terminal variable that feeds no group key collapses the final hop (`execute_collapsed_count`). The collapse counts each source's
      qualifying neighbors through `Graph::typed_neighbor_counts`, so the final hop costs no triple per traversed edge and no hash lookup per edge: a
      terminal filter that is a label test goes straight into the spec, and a terminal property comparison is resolved into a `neighbor_allow` set by
      running those exact stages over the label's whole node set (`resolve_terminal_allow`). That resolution is gated on the sources' `adjacency_span`
      reaching half the label count, so a selective hop over a large label keeps the expansion fallback instead of paying for a full label pass, and it
      is speculative: it evaluates predicates over a superset of the real neighbors, so a stage that errors there declines to the fallback rather than
      raising. Two shapes route to the fallback regardless: a multi-type hop, because `Expand::rel_type` carries the raw pattern text (`"F|G"`) and the
      kernel resolves one registered type; and a stale snapshot with at most `STALE_POINT_EXPAND_MAX` sources (`Graph::prefers_point_expansion`), because
      the kernel would rebuild the whole snapshot where the fallback serves those sources from per-source adjacency. The recognizer sees through a `Distinct` because the caller deduplicates. Any unrecognized shape falls back to the row pipeline, so
      correctness never depends on the recognizer.
    - `src/exec/factorize.rs`: `FactorizedRecordGroup` (shared `Arc<PathMap>` prefix plus per-row extensions) and `filter_refs_in_expr`.
    - `src/exec/expr.rs`: expression evaluation (`evaluate_expr`, `eval_binary_op`, `eval_arithmetic`, `eval_function_call`).
    - `src/exec/write.rs`: mutation execution (`execute_create`, `execute_set`, `execute_delete`, `execute_merge`).
    - `src/exec/ddl.rs`: DDL execution (`execute_create_index`, `execute_drop_index`). A node `CREATE INDEX` provisions the full-text index, because
      node property lookups are already served by the always-on auto-index; a relationship `CREATE INDEX` provisions the property index.
    - `src/exec/copy.rs`: bulk data administration execution (`COPY ... FROM`, `EXPORT DATABASE`, and `IMPORT DATABASE`).
    - `src/exec/row.rs`: the positional row representation (`SlotRow` and `SlotSchema`) the row pipeline binds variables through.
- `crates/issundb-graphblas-sys/`: raw FFI bindings to the Apache-2.0 SuiteSparse:GraphBLAS C library, vendored as the `external/GraphBLAS` git
  submodule (pinned to v10.3.1) and built from source by `build.rs` as a position-independent static library with a dynamically linked OpenMP runtime
  (`libgomp`). Bindings are generated by `bindgen`. `cargo package` never descends into submodules, so `build.rs` resolves the source in priority
  order (the `ISSUNDB_GRAPHBLAS_SRC` override, then the submodule, then the pinned tarball downloaded into `OUT_DIR` and checksum-verified): the
  in-repo build uses the submodule with no network, while a crates.io build fetches the pinned source.
- `crates/issundb-graphblas/`: minimal safe wrapper over the GraphBLAS operations the engine uses (typed `Matrix`/`Vector` over `i32`/`f32`/`f64`,
  build from triples, `mxv` over predefined semirings, `ewise_add` over predefined monoids, and the descriptor flags). Depends only on
  `issundb-graphblas-sys`. `issundb-core` reaches GraphBLAS exclusively through this crate.
- `crates/issundb-vector/`: vector index abstraction, vector metadata, vector storage integration, and vector search APIs.
- `crates/issundb-text/`: text query APIs and ranking. Tokenization and the inverted-index storage are *not* here: they live in
  `issundb-core` (`graph/fts_mod.rs` and `storage/fts.rs`), because the write path is in core and the FTS postings are maintained inside the same
  write transaction as the node record (`index_node_for_label` on insert and update, `delete_node_fts` on delete). A tokenizer in this crate could not
  be reached from there without inverting the dependency, so the full-text index is the one secondary structure that is transactional rather than an
  eventually-consistent cache. This crate owns the `Scorer` trait (BM25), query evaluation, and the `TextGraphExt`/`TextIndexExt` surface, and it
  tokenizes queries through core's `tokenize_text` so indexing and querying cannot disagree.
- `crates/issundb-retrieval/`: hybrid retrieval over graph traversal, vector hits, text hits, property filters, score fusion, and subgraph
  materialization.
- `crates/issundb/`: public facade. Re-exports the deliberate public surface from `issundb-core`, `issundb-vector`, `issundb-text`,
  `issundb-retrieval`, and `issundb-cypher`. Do not re-export internal storage types like `Storage`.
    - `benches/`: Criterion query optimizer benchmarks (`query_optimizer`, `skewed_schema`, `cyclic_enumeration`) and two profiling drivers (`profile_triangle`,
      `profile_query`) that load a persistent graph once and rerun a query so a profiler observes execution without load noise. `skewed_schema`
      exercises the schema-aware passes: provably-empty typed hops pruned to a zero-row plan, plus join-ordering and chaining sensitivity.
      `cyclic_enumeration` sizes the intermediate wedge blowup on cyclic patterns, so it guards the fused `ExpandIntersect` closing hop against a
      regression back to materializing every wedge.
- `crates/issundb-cli/`: interactive REPL binary. Uses only the `issundb` public facade for manual exploration and demos.
- `crates/issundb-rest/`: Axum-based HTTP REST API server. Exposes the data plane and retrieval over HTTP. Depends only on `issundb`; uses `tokio`.
  See its Component APIs entry for routes and intentional exclusions.
- `crates/issundb-mcp/`: Model Context Protocol server built on the `rmcp` SDK, serving over stdio or MCP's Streamable HTTP transport. Depends only on
  `issundb`; uses `tokio`. See its Component APIs entry for the tool surface and the Host-header allowlist.
- `crates/issundb-py/`: Python bindings via PyO3. Exposes the `IssunDB` class. Depends only on `issundb`.
- `crates/issundb-examples/`: standalone example programs. These depend only on `issundb`.
- `crates/*/benches/`: crate-local Criterion benchmark targets (storage and write throughput, Cypher parsing and execution plus LSQB Q1-Q9 and OLTP
  reads, vector search, full-text search, and hybrid retrieval plus GraphRAG).
- `crates/issundb/tests/conformance/`: openCypher TCK subset integration tests.
- `benchmarks/ladybugdb-compare/`: differential comparison harness against LadybugDB. Deliberately excluded from the workspace (own `[workspace]`
  stanza, root `exclude`, and own `rust-toolchain.toml`) because the `lbug` crate links the LadybugDB C++ library and needs a newer Rust than the
  workspace MSRV; it must never become part of `make build` or `make test`. Run via `make test-ladybugdb` for correctness and `make bench-ladybugdb`
  for timing. Cross-engine harnesses belong here, not
  in crate-local `benches/`, which is reserved for Criterion targets. It runs two separate passes.
    - `differential_workload`: curated row-returning queries whose sorted row sets must match exactly, run before anything is timed. Together with the
      generated corpus below this is the only oracle that can catch a mistake IssunDB makes consistently across all of its own execution paths, which is
      exactly what `ISSUNDB_ROW_PIPELINE_ONLY` cannot see. No pattern here may bind one edge to two relationship slots, which means at most two
      same-direction hops and no closing hop: walk-versus-trail is *not* only a variable-length question, because relationship uniqueness applies to any
      pattern with two or more slots, and the pinned LadybugDB build permits the reuse openCypher forbids. Row sets are bounded so the pass costs the
      same at every size in a sweep. `differential_corpus_is_fixed_length_and_row_returning` pins both rules. Projections avoid floats and nulls because
      the two databases' display forms differ there (a whole-valued float is `0.0` against `0`), which is not a semantic divergence.
    - `generate_queries` plus `reference_rows`: the generated corpus, enabled by `LADYBUGDB_COMPARE_GENERATED` and run by `make test-ladybugdb`. Shapes
      outside the curated corpus's rule belong here, because here they are adjudicated rather than merely compared: `reference_rows` evaluates each
      generated query over the dataset by brute force under openCypher semantics, so a divergence names the database at fault instead of leaving a human to
      decide. IssunDB disagreeing with the reference fails the run; LadybugDB disagreeing is counted as a walk-semantics divergence and does not; the
      reference disagreeing with both databases while they agree fails as a harness defect. Findings are shrunk to their smallest reproducing shape.
      `make test-ladybugdb` runs the pass twice, once with the fast paths and once under `ISSUNDB_ROW_PIPELINE_ONLY`, which composes the two oracles.
    - `workload`: the timed comparison. Its queries are shaped for measurement, so most return a single `count(...)`; a `DIVERGENT` verdict there is an
      attributed LadybugDB walk-semantics overcount and does not fail the run, but a `MISMATCH` does (`tests/lbug_trail_semantics.rs` pins the
      walk-versus-trail divergence). Add a correctness query to the differential corpus, not here.
- `Cargo.toml`: workspace root with shared `[workspace.dependencies]`. All version pins live here.
- `Makefile`: developer workflow entry points.
- Directory-scoped guides: `crates/issundb-core/AGENTS.md`, `crates/issundb-cypher/AGENTS.md`, `crates/issundb-text/AGENTS.md`, and
  `crates/issundb-vector/AGENTS.md` carry crate-specific rules that this file does not repeat (LMDB lifetime rules, the query pipeline stages, the
  tokenization order, the HNSW lock ordering). Read the one covering the crate being changed, and update it in the same patch when its subject changes:
  being unreferenced from here is what let several of them drift behind the code.

## Testing Layout Rules

- Unit tests for `issundb-core` belong in `#[cfg(test)]` blocks inside the relevant source file. Each test that touches LMDB must open a fresh
  `tempfile::TempDir` and must not share state with other tests.
- Integration tests that exercise multiple crates belong in `tests/` at the workspace root or in `crates/issundb/tests/`.
- Cypher conformance tests belong in `crates/issundb/tests/conformance/` and are gated on the `ISSUNDB_CONFORMANCE=1` environment variable so the
  default `make test` stays fast (run them via `make test-conformance`).
- Property-based tests (via `proptest`) belong alongside the unit tests for the module whose invariants they exercise.
- The row pipeline is the differential oracle for every shape-specific fast path. `ISSUNDB_ROW_PIPELINE_ONLY=1` keeps the columnar executor, the
  `PathCount`, `GroupedDegree`, and `TriangleCount` kernels, the fused `ExpandIntersect` hop, the metadata count shortcut, and the type-inference pruning
  pass out of the answer, so any suite can be swept through the general path and compared. Pruning is in the switch because it is the one pass that drops
  rows rather than reorganizing them, so leaving it outside made a wrong `schema_has_edge` negative invisible to the comparison. Both `cargo test` and `ISSUNDB_CONFORMANCE=1` runs must pass identically with and without
  it; a divergence is a fast-path defect, not a configuration difference. A test whose premise is that a particular operator lowers, and the fast half
  of any differential comparison, must pin the setting with `exec_mode::fast_paths_required` rather than inherit it, or the sweep makes it either fail
  on its own premise or pass vacuously. The corpus lives in `crates/issundb-cypher/src/exec/differential.rs`. `VectorTopK` is deliberately outside the
  switch, because an HNSW search is approximate and is entitled to differ from the exact sort it replaces.
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
- The GraphBLAS matrices (`MatrixSet`) and the CSR snapshot back the GraphBLAS algorithms, pattern matching, and multi-source expansion. They are kept
  fresh through three gates rather than a single periodic rebuild. The write path records a structural delta (added nodes, added edges, and removed
  edges, plus a `force_full` flag set on any node deletion).
    - `Graph::open` builds neither: it installs an empty snapshot through `CsrCache::new_unbuilt` and leaves `matrices` as `None`, so the gates below
      do the first build when a consumer that needs one runs. A workload of point lookups, property reads, or small typed expansions never builds
      either structure, because those paths read LMDB directly. The unbuilt cache starts `write_gen` at 1 with both installed generations at 0 so it
      reports stale; a placeholder that claimed to be current would make typed expansion read zero rows out of the empty snapshot. Do not reintroduce
      an eager build in `open`: it costs one full edge scan plus a full matrix materialization on every open (roughly 26 seconds for a 1 M-node,
      14 M-edge graph) and is repaid on every reopen.
    - Pure-adjacency consumers (`bfs`, `bfs_multi_source`, untyped expansion, `degree_centrality`, and `connected_components`) call
      `ensure_matrix_view`, which applies the delta in place, falling back to a full `rebuild_csr` only when a node was deleted.
    - CSR-array and hybrid consumers (everything else, including `dfs`, the path searches, the weighted and flow algorithms, `page_rank`, and the
      remaining centralities) call `ensure_csr_fresh`, which rebuilds on demand gated by the `write_gen` versus `snapshot_gen` counter; when the
      snapshot is already fresh it still drains the pending delta into the matrices.
    - Typed bulk expansion calls `ensure_snapshot_fresh`, which rebuilds only the snapshot (no GraphBLAS materialization); for a small source set over
      a stale snapshot it skips the gate and reads per-source LMDB adjacency.
      The background rebuild after `REBUILD_THRESHOLD` writes is a compaction safety net, not the freshness path; callers needing a guaranteed fresh
      CSR view still call `rebuild_csr`. Point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`) read the `out_adj` and `in_adj`
      stores directly through the transaction, never the snapshot, so they always reflect committed and in-transaction writes.
- `Storage::open` is the only entry point for LMDB. Do not call `heed::EnvOpenOptions` from outside `crates/issundb-core/src/storage/lmdb.rs`.
- Heavy dependencies are tracked in the workspace `Cargo.toml`. `usearch` and `chumsky` are active, non-optional dependencies. GraphBLAS is reached
  through the in-house permissive crates `issundb-graphblas` and `issundb-graphblas-sys`. Building requires the submodule
  (`git submodule update --init external/GraphBLAS`) plus CMake and Clang.
- Async is not used in the core engine. LMDB and GraphBLAS are synchronous. `tokio` is an optional dependency for server mode only; do not add
  `.await` inside `issundb-core`.
- Parallelism has exactly two consumers, and both resolve their thread count through `threads::resolve` (see the module map): the GraphBLAS OpenMP pool,
  and the scoped-thread reductions in the counting kernels, which split a pass only above `MIN_PARALLEL_WORK` items so a small pass and a unit test stay
  serial and deterministic. Writes are never parallel: they serialize on the `ReentrantMutex` write lock and on LMDB's single writer.
- GraphBLAS initializes a process-global context and OpenMP thread pool on first use (`GrB_init`) and never finalizes it. Under `cargo nextest`
  (process-per-test, used by `make coverage`) every process pays this cost, so on small CI runners the thread pools oversubscribe and a GraphBLAS call
  can fail intermittently. The coverage job pins `OMP_NUM_THREADS=1` and sets `NEXTEST_RETRIES=2` to compensate.

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
8. `issundb-rest`, `issundb-mcp`, and `issundb-py` must depend only on `issundb`; they must not import `issundb-core`, `issundb-vector`,
   `issundb-text`, `issundb-retrieval`, or `issundb-cypher` directly.

Lower-level crates must not know about higher-level crates.

## Component APIs

### `issundb_core::Graph`

The central coordination type. All graph operations go through `Graph`; do not call `Storage` directly from outside `issundb-core`.
`Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>` is the only constructor.

Node and edge CRUD, accessors, and registry lookups have self-describing signatures; read them from the source rather than this file. Methods:
`add_node`, `add_node_multi`, `get_node`, `update_node`, `delete_node`, `add_label`, `remove_label`, `node_labels`, `add_edge`, `get_edge`,
`update_edge`, `delete_edge`, `out_neighbors`, `in_neighbors`, `node_has_relationships`, `nodes_by_label`, `edges_by_type`, `all_nodes`, `label_name`,
`type_name`, `list_node_indexes_and_constraints`, `list_edge_indexes_and_constraints`, `node_count_by_label`, `edge_count_by_type`,
`put_vector_bytes`, `vector_bytes`, and `rebuild_csr`.

The read-path and statistics methods carry non-obvious semantics:

- `node_prop_json(id, prop) -> Result<Option<Value>, Error>`: single-property read; `None` for a nonexistent node, `Some(Value::Null)` for a missing
  property.
- `node_props_json_table(ids, props) -> Result<Vec<Vec<Value>>, Error>`: bulk row-major property gather; `Value::Null` for a missing property and
  `Error::NodeNotFound` for a nonexistent node.
- `node_prop_json_column(ids, prop) -> Result<Vec<Value>, Error>`: single-property column form of the table gather, one flat vector with no per-row
  allocation; same null and missing-node semantics.
- Those three read through the in-memory property columns once those exist, but a small request does not build them (`should_serve_directly`): up to
  `SMALL_GATHER_MAX` ids are served as LMDB point reads instead, because building every column costs one full node scan and a query touching a handful
  of nodes must not pay it. The size test is on the request, not the method, because the vectorized executor gathers even a one-row projection through
  the bulk API; keying on the method instead left a cold point query paying roughly 1.2 seconds on an 800 K-node graph. Sustained direct reads amortize
  the build after `DIRECT_READ_BUILD_THRESHOLD` of them, so a row pipeline reading one property per row still ends up on the columns.
  `node_prop_group_codes` follows the same size test: a large request builds the columns, while a small one is grouped over an ephemeral column set
  built from just those entities. Grouping is a bulk read only when the id set is bulk, and both go through the same `PropColumns::from_items` and
  `group_codes`, so the narrower population is the same grouping code rather than a second implementation of it.
- `materialize_property_columns() -> Result<(), Error>`: build the in-memory property columns now. Nothing builds them as a side effect of a
  small workload, so this is the deliberate way to make the optimizer's selectivity estimates and zone-map pruning available on a cold graph, or to pay
  the one full scan up front rather than in a later bulk read.
- `node_prop_group_codes(ids, prop) -> Result<(Vec<u32>, Vec<Value>), Error>`: dense group codes under exact value identity of one property, plus one
  representative value per code; null and missing values share one `Value::Null` code.
- `node_prop_min_max(prop) -> Result<Option<(Value, Value)>, Error>`: bounds of one property's non-null values from the column statistics; `None` for
  a `Json` fallback column or no non-null values; backs the vectorized executor's zone-map filter pruning.
- `estimate_range_selectivity(prop, lower, upper) -> Result<Option<f64>, Error>`: estimated fraction of non-null values inside the bounds, from the
  property's equi-depth histogram.
- `estimate_equality_selectivity(prop, val) -> Result<Option<f64>, Error>`: estimated fraction of non-null values equal to `val`, exact for the most
  common values and histogram-estimated otherwise; both feed the optimizer's selectivity-aware `Filter` plan weight.
- Those three readers are advisory, and none of them builds the property columns: each also returns `None` when the columns do not exist yet, leaving
  the caller on its default plan weight or declining to prune. Forcing a build for them made the first query mentioning any property pay one full node
  scan (measured at roughly 1.3 seconds on an 800 K-node graph), which was the dominant cold-start latency, and the answer only weights a choice.
  A caller that needs statistics on a cold graph must materialize the columns first, and `Graph::materialize_property_columns` is how: no small read
  builds them as a side effect any more, so a gather of more than `SMALL_GATHER_MAX` ids is the only other thing that will. Calling a reader and
  discarding the result is no longer an idiom for warming them.
- `estimate_expand_fanout(src_label, rel_type, incoming) -> Result<Option<f64>, Error>`: per-source-label typed degree (the count of `rel_type` edges
  incident to `src_label` nodes in the given direction, divided by the `src_label` node count), the "expand ratio" that sharpens the optimizer's
  `Expand` plan weight over the global average on a skewed schema. `None` when a label or type is unknown or no such edges exist. The estimate only
  weights plan choices, so a stale or absent value never affects correctness.
- `estimate_expand_fanout_to(src_label, rel_type, dst_label, incoming) -> Result<Option<f64>, Error>`: destination-label-aware refinement, the average
  number of `dst_label` neighbors per `src_label` node; sharpens the plan weight of a `HasLabel` filter over an `Expand`.
- Both fan-out estimates are advisory in the same sense the property-column statistics are, and neither builds the schema statistics table: each returns
  `None` when no usable table exists, leaving the caller on the global average fan-out. Building it to sharpen a plan weight made the first query
  mentioning a relationship pattern pay for the whole graph (measured at roughly 5 seconds on a 1 M-node, 13.9 M-edge graph, which was the cold
  `one_hop_neighbors` latency). `Graph::materialize_edge_statistics` is how a caller asks for them.
- They do accept a table the write generation has moved past, unlike `schema_has_edge`, because their alternative is not a fresher estimate but no
  estimate, and the global average is cruder than a dated per-label ratio. Refusing every stale table meant a process that materialized lost its
  statistics on the first write and never recovered them, so a long-lived server that ingests anything spent the rest of its life on default plan
  weights. The tolerance is bounded by growth rather than age, and *per relationship type*: a stale table is refused for a given type once that type's
  live `stats:t:` count exceeds `STALE_FANOUT_GROWTH_FACTOR` times the count recorded at build time. Growth is the right bound because a generation is
  one commit and a commit may be a single edge or a bulk import; per type is necessary because a global edge count cannot see skew, and a skewed ingest
  that adds half a million edges of one type to a graph of a million stays inside any global factor while moving that type's fan-out by orders of
  magnitude. Because the counter never decreases, the refusal catches a type that grew (where a stale estimate understates fan-out and invites the
  planner to treat an expensive expansion as free) and not one that shrank (where it overstates and the planner is merely conservative). What it still
  cannot see is redistribution *within* a type, which would need the live per-`(label, type)` counter that is the table itself; that residual weights a
  plan and never changes an answer.
- `materialize_edge_statistics() -> Result<(), Error>`: build the schema statistics table now, the counterpart of `materialize_property_columns` for the
  edge-level statistics. One pass over `label_idx` and one over `out_adj`, cached until a committed write advances the generation. It is what makes the
  expand-ratio estimates available at all, and it upgrades `schema_has_edge` from a budgeted probe to an exact lookup that also decides the questions the
  probe gives up on. A caller wanting the optimizer at full strength on a cold graph wants this and `materialize_property_columns`.
- `schema_has_edge(src_label, rel_type, dst_label) -> Result<Option<bool>, Error>`: whether the committed data schema contains any directed edge
  `src_label --rel_type--> dst_label`. `Some(false)` means the directed pattern is provably unsatisfiable; `None` when any name is unknown or the
  question could not be settled within `SCHEMA_PROBE_BUDGET`. Backs the optimizer's type-inference pass. Unlike the fan-out estimates this one is not
  advisory, since a negative drops rows, so it never depends on the statistics table existing: with no current table it walks the smaller endpoint
  population through `label_idx` and tests each of its `rel_type` edges for the opposite label, settling on the first match and otherwise exhausting that
  population. Every storage operation the walk performs is charged against the budget, visiting a node included, or a population whose nodes have no
  adjacency in the direction under test would walk to the end of the label for free. Only the choice of which side to walk reads the stored per-label
  counter; the emptiness shortcut asks `label_idx` instead, so a prune never rests on a counter being exact. A decided verdict is memoized against the
  write generation, because the pass that asks runs on every execution (there is no plan cache) and would otherwise re-walk the graph per query.
- `label_filter(nodes, label) -> Result<Vec<NodeId>, Error>`: subset of `nodes` carrying `label`, via one `label_idx` point lookup per candidate.
- `set_thread_count(n: i32) -> Result<(), Error>`: sets the GraphBLAS thread count, overriding the `ISSUNDB_NUM_THREADS` environment variable (0
  restores default behavior, resolved by `threads::resolve`). The count is stored and applied by `MatrixSet::materialize`, which is also what initializes the GraphBLAS context, so a
  call made before the matrices exist takes effect at the next materialization rather than reaching GraphBLAS immediately. Since `Graph::open` no longer
  materializes eagerly, that is the normal case for a caller configuring threads up front; setting a global option on an uninitialized context would
  fail.

Graph algorithms have self-describing signatures over `NodeId` and `EdgeId`: `bfs`, `dfs`, `shortest_path`, `all_paths`, `all_shortest_paths`,
`longest_path`, `shortest_path_top_k`, `page_rank`, `connected_components`, `strongly_connected_components`, `detect_cycle`, `label_propagation`,
`degree_centrality`, `betweenness_centrality`, `harmonic_centrality`, `spanning_forest`, `maximum_flow`, and `all_neighbors`. Five carry behavior worth
pinning:

- `shortest_path_dijkstra(src, dst) -> Result<Option<WeightedPath>, Error>`: edge weight is the first present of the `weight`, `cost`, `capacity`, or
  `cap` property, default `1.0`; the source is fixed, so unlike `shortest_path_top_k` and `spanning_forest` this method takes no weight-property
  argument.
- `count_triangle_cycles(spec: &TriangleCountSpec) -> Result<u64, Error>`: assignment count of the directed triangle pattern
  `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` with optional per-hop relationship types and per-variable labels, following Cypher MATCH row semantics including
  relationship uniqueness; the Cypher optimizer lowers grouping-free `count` aggregates over that pattern to this kernel via the `TriangleCount`
  physical operator.
- `typed_neighbor_counts(sources, spec: &NeighborCountSpec) -> Result<Vec<(u64, u64)>, Error>`: per-source `(qualifying, counted)` neighbor counts
  across one typed hop, in input order. It reads only the sources' own CSR rows, so it costs the sum of their degrees rather than a full scan, and it
  tallies into integers instead of materializing one entry per traversed edge. A neighbor qualifies when it carries every label in `neighbor_labels`
  and belongs to `neighbor_allow` when that is present; the two totals differ only for `neighbor_nonnull_prop`, where a neighbor can qualify (so the
  source produces rows) without adding to the count. `neighbor_allow` is the counterpart of `PathCountSpec::vertex_allow`: the caller resolves a
  per-neighbor property predicate itself and hands the kernel the surviving ids, so a filtered count stays a kernel call. This is the kernel behind the
  Cypher executor's terminal count-collapse; a source absent from the snapshot counts zero rather than erroring.
- `prefers_point_expansion(sources) -> bool`: whether a typed expansion over `sources` many source nodes should read per-source LMDB adjacency instead of
  refreshing the CSR snapshot, true only when the snapshot is stale and the source set is at most `STALE_POINT_EXPAND_MAX`. Advisory: both routes return
  the same rows, so a caller ignoring it is correct but may rebuild a whole snapshot to serve a handful of sources. It is public because the Cypher
  executor's collapse decision needs it from another crate.
- `adjacency_span(sources, incoming) -> Result<u64, Error>`: total length of `sources`' adjacency rows in one direction, measured over the *installed*
  CSR snapshot. It reads two array elements per source and no edge, and deliberately does not refresh: it exists so a caller can size an expansion before
  choosing how to evaluate it, and refreshing would make the sizing call perform the work it is meant to help skip. The value is therefore advisory and
  snapshot-relative, not a guaranteed bound on what a subsequent `typed_neighbor_counts` visits: after a write, or on a graph whose snapshot is not built,
  it under-reports, and a caller that treats a low span as "cheap" simply declines an optimization rather than computing a wrong answer.

### `issundb_vector`

Vector search crate. Owns vector index abstractions, vector metadata, vector storage integration, and vector search APIs. May depend on
`issundb-core`; must not depend on `issundb-text`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `VectorGraphExt::configure_vector_index(opts) -> Result<(), VectorError>`: sets the per-graph metric and quantization, persisted in the `meta`
  sub-database so reopen rebuilds with the same configuration. Call it before the first upsert; changing the metric or quantization once vectors exist
  returns `VectorError::AlreadyConfigured`. Defaults to Cosine and Float32.
- `VectorGraphExt::reindex_vector_index(opts) -> Result<(), VectorError>`: changes the metric or quantization on a populated graph and rebuilds the
  index from the persisted embeddings. The stored vectors are raw, metric-agnostic f32, so they re-index under any metric; this is O(n) and is an
  administrative operation, not a concurrent one.
- `VectorGraphExt::upsert_vector(n, v) -> Result<(), VectorError>`
- Searching a graph with no stored embeddings returns `VectorError::EmptyIndex` rather than an empty hit list, so a caller can distinguish "no
  semantic matches" from "there is nothing to search". The Cypher `VectorTopK` operator maps that error to zero rows, keeping MATCH semantics.
- `VectorGraphExt::remove_vector(n) -> Result<(), VectorError>`: removes the embedding from both memory and storage.
- `VectorGraphExt::vector_search(q, k) -> Result<Vec<Hit>, VectorError>`
- `VectorGraphExt::vector_search_with(q, opts) -> Result<Vec<Hit>, VectorError>`: adds an exact-label filter and property equality filters (both
  evaluated during the HNSW traversal) and `rescore_factor`. On a quantized index the search defaults to fetching `2k` candidates and re-ranking them
  by exact distance against the raw f32 vectors in LMDB; `Some(1)` disables the rescore, and a `Float32` index never rescores by default.

### `issundb_text`

Full-text search crate. Owns ranking and the text query APIs. Tokenization and the inverted-index storage live in `issundb-core` so the postings can
be written inside the same transaction as the node record; see the Repository Layout entry. May depend on `issundb-core`; must not depend on
`issundb-vector`, `issundb-retrieval`, `issundb-cypher`, bindings, or CLI crates.

- `TextGraphExt::text_search(query, opts) -> Result<Vec<TextHit>, TextError>`
- `TextHit` carries `node`, `score`, and the `label` and `property` of the text index that contributed the hit's largest partial score, so a caller
  can read the matched field without a follow-up lookup.
- `text_search` errors instead of returning a silent empty list when the request cannot match anything: an empty query (`EmptyQuery`), a label or
  property filter naming no active index (`LabelNotIndexed`, `PropertyNotIndexed`, or `IndexNotFound` for a pair), or a graph with no text indexes at
  all (`NoIndexes`).
- `TextIndexExt::create_text_index(label, property) -> Result<(), TextError>`
- `TextIndexExt::create_text_index_with_language(label, property, lang) -> Result<(), TextError>`
- `TextIndexExt::drop_text_index(label, property) -> Result<(), TextError>`
- `TextIndexExt::has_text_index(label, property) -> Result<bool, TextError>`
- `TextIndexExt::list_text_indexes() -> Result<Vec<(String, String, Language)>, TextError>`

### `issundb_retrieval`

Hybrid retrieval crate. May depend on `issundb-core`, `issundb-vector`, and `issundb-text`; must not be imported by those lower-level crates. All
retrieve functions are free functions, not methods on `Graph`, to preserve the crate boundary.

- `retrieve(graph, q, k, hops) -> Result<Subgraph, RetrievalError>`
- `retrieve_with(graph, q, opts) -> Result<Subgraph, RetrievalError>`
- `retrieve_hybrid(graph, q, text_query, opts) -> Result<Subgraph, RetrievalError>`: fuses vector and text search seed relevance scores before running
  expansion. When neither modality would run (both inputs empty or both k values zero) it returns `RetrievalError::NoQuery` instead of a silently
  empty subgraph.
- `Subgraph`: `nodes: Vec<NodeId>`, `edges: Vec<EdgeId>`, `scores: HashMap<NodeId, f32>`, and `truncated: bool` (true when the `max_nodes` cap cut
  off seeds or expansion, so a capped result is distinguishable from a complete one)
- `RetrieveOptions`: `k`, `hops`, `max_distance`, `max_nodes`
- `HybridRetrieveOptions`: `vector_k`, `text_k`, `text_label`, `text_property`, `hops`, `max_distance`, `max_nodes`, `vector_label`, `fusion`
- `FusionStrategy`: reciprocal rank fusion (`Rrf { k }`) or linear combination (`WeightedSum { vector_weight, text_weight }`)

### `issundb_cypher`

Cypher query execution. Exposed through the `issundb` facade via the `GraphQueryExt` trait; do not call `issundb_cypher::execute` directly from
outside `issundb`.

- `query(cypher) -> Result<QueryResult, CypherError>`, `query_with_params(cypher, params) -> ...`,
  `query_with_procedures(cypher, params, registry) -> ...` (resolves `CALL` clauses against a custom procedure registry), and
  `explain(cypher) -> Result<String, CypherError>`
- `QueryResult`: `columns: Vec<String>`, `records: Vec<Record>`, `statement_count: usize`; `Record`: `values: Vec<serde_json::Value>`. A
  semicolon-separated query (`Statement::Pipeline`) runs every top-level statement, but `columns`/`records` reflect only the last one;
  `statement_count` (always 1 otherwise) lets a caller notice a multi-statement query instead of silently reading the final statement's result as if
  it were the whole query. When one `AND`/`OR` operand alone determines the result (`false AND x`/`true OR x`), a runtime evaluation error in the
  other operand is suppressed, so a guard clause protects against a division error; a successfully evaluated non-boolean operand still raises a
  type error even on the determined side, as the openCypher TCK requires (`false AND 123` raises, `false AND (1 / 0)` is `false`).
- A single statement's write clauses (`CREATE`/`MERGE`/`SET`/`DELETE`/`REMOVE`, in any combination, and the `RETURN`/`WITH` projection that follows
  them) share one `Graph::update` transaction: an error anywhere rolls back every write the statement already made, not just the one that failed.
  This covers a `RETURN`/`WITH` clause reading a property of a variable bound by an earlier write in the same statement (`CREATE (n {a:1}) RETURN
  n.a` and its `WITH`-chained form both see the fresh value via a thread-local pending-writes overlay in `exec/expr.rs`, since the in-memory property
  columns `node_prop_json` reads only refresh after commit) and a `MERGE`'s match-or-create decision sharing a transaction with its `ON CREATE
  SET`/`ON MATCH SET`. It does not cover a `MATCH`/label-scan/index-scan *after* a write clause in the same statement observing that write's
  structural effect (a brand-new node becoming visible to a label scan): those read through the committed-only `label_idx`/CSR snapshot, not the
  still-open transaction, so `CREATE (a:Foo) WITH a MATCH (m:Foo) RETURN count(m)` does not count `a` until the statement commits.

The executor resolves patterns through the physical plan. Untyped expansion uses GraphBLAS SpMV; typed expansion reads the CSR snapshot in bulk behind
`ensure_snapshot_fresh`, falling back to per-source LMDB point reads when the snapshot is stale and the source set is small. Key optimizer behaviors,
each navigable by the named symbol:

- Top-level `AND` conjunctions in WHERE split so each conjunct pushes down to its own lowest binder.
- An equality or range filter over a labeled scan rewrites into `NodeIndexScan` or `NodeRangeScan` when the property has a declared index; the rewrite
  recurses through every single-input operator (`Aggregate`, `Sort`, `Limit`, `Distinct`).
- A correlated equality whose key is bound at runtime (the `UNWIND`- or parameter-driven shape the literal-only rewrite cannot lower) rewrites from a
  `Filter` over a `HashJoin` into a `CorrelatedIndexSeek` (`rewrite_correlated_seek`), one index seek per outer row. It fires only when one join side
  is a bare `LabelScan` for the seek variable and the key references only the other side's variables.
- A natural inner `HashJoin` whose one side merely re-scans a variable the other already binds (the multi-`MATCH`-shared-pivot shape) rewrites into a
  linear "expand into" chain (`rewrite_join_to_expand`); it fires only when the two sides share exactly the one rooted variable and never across an
  `OptionalMatch`.
- Bulk label filtering uses `label_idx` point lookups (`Graph::label_filter`); single-property node reads go through the property columns
  (`Graph::node_prop_json`).
- A final projection or aggregation over a linear chain of up to `MAX_VEC_HOPS` directed hops executes column-at-a-time through `exec/vectorized.rs`;
  every other shape runs the row pipeline.
- A grouping-free `count` over a one-hop or two-hop directed expansion lowers to the `PathCount` kernel (`Graph::count_linear_paths`); per-vertex
  `prop CMP literal` predicates push down into the kernel as index-resolved node-id allow-sets (`PathCountSpec::vertex_allow`).
- A `count` grouped by one endpoint of a single directed hop lowers to the `GroupedDegree` kernel (`Graph::grouped_edge_counts`), which emits one entry
  per group node; the executor folds those into groups through dense integer codes (`combine_group_codes`) rather than a key string per group node, and
  emits them in the canonical-key order the row pipeline's `BTreeMap` fold uses, so both paths agree without an `ORDER BY`.
- An `ORDER BY <count> LIMIT n` above a grouped count pushes a `CountWindow` into the operator that produces the groups (`set_count_window` for
  `GroupedDegree`, `leading_count_sort` for the vectorized collapse), so a grouped count over a whole label stops building a row per group to keep `n`
  of them. The window keeps every group reaching the `n`-th best count, boundary ties included, and emits survivors in the order it would have emitted
  the full group set, so the enclosing `Sort` and `Limit` pick exactly what they would have over every group. It declines when the leading sort key is
  not the count, when a `Distinct` sits between the sort and the projection, and when any projected item is more than a variable or property read (a
  pruned group is never projected, so an expression that can raise must not be skipped).
- `RETURN DISTINCT` plans a `Distinct` between the final `Project` and `Sort`, so deduplication happens before `ORDER BY` and `SKIP`/`LIMIT`;
  `WITH DISTINCT` keeps full-row deduplication behind its barrier project; only `RETURN DISTINCT *` deduplicates after projection in the executor.
- A type-inference pass (`prune_unsatisfiable`) consults `Graph::schema_has_edge`: a typed hop between two labeled endpoints with no realized triple
  is
  provably empty, so that `Expand` is wrapped in a `Limit` with `count` zero. It runs only on read-only plans and prunes only on a definitive
  negative, so it never drops rows the query should return.
- The plan-weight cost model applies the high-order statistics at every hop. It collects each variable's label constraints from the pre-strip tree
  (`collect_label_constraints`) and threads them through `plan_weight`; `label_of_var` recovers a stripped intermediate hop endpoint's label so a
  multi-hop chain applies the per-source-label expand ratio at each hop. A cyclic pattern closed by a `MultiwayJoin` is weighted by its closing edge's
  per-pair probability (`closing_selectivity`, `triples / (N_src * N_dst)`).

### `issundb_rest`

HTTP REST API server built on Axum and Tokio. Depends only on `issundb`; must not import lower-level crates directly. All handlers share a single
`Arc<Graph>`.

Data and query routes are versioned under a `/v1` prefix. `GET /health` stays unversioned so infrastructure probes do not track the API version; its
body reports the crate `version` and the current `api` version.

REST exposes the data plane and retrieval only. Index administration (vector index configuration, text index create/drop/list), GraphBLAS thread
control, and backup/restore are intentionally absent: provisioning and host operations are done through the CLI or the Python surface, not over HTTP.

Startup calls `Graph::materialize_edge_statistics` before binding the listener, so the optimizer's expand-ratio estimates and type-inference pruning are
available to every request rather than to none: nothing builds that table as a side effect of a query, and an HTTP caller has no way to ask for it. The
warm-up is synchronous, because a process that is not ready is better than one serving on default plan weights, and a failure is logged and ignored since
every reader of the table works without it. `--no-warm-statistics` (or `ISSUNDB_NO_WARM_STATISTICS`) skips it for a graph large enough that readiness
matters more. The property columns are deliberately *not* warmed here: that build is a full node scan and holds every scalar property in memory, which is
a footprint decision an operator should make, not a startup default.

The API is self-describing: the OpenAPI 3.1 document is generated from the handler annotations (`#[utoipa::path]`) and the request and response
`ToSchema` derives, served as JSON at `GET /v1/openapi.json` with a Scalar UI at `GET /v1/docs`. The generator crates are `utoipa` and
`utoipa-scalar` (both MIT or Apache-2.0), pinned to the Axum 0.7 line. Because the handlers build their JSON bodies inline with `json!`, the
documentation-only response structs (`NodeResponse`, `EdgeResponse`, `IdResponse`, `QueryResponse`, `ExplainResponse`, `RetrieveResponse`,
`HealthResponse`, and `ErrorResponse`) describe the response shapes and must be kept in sync with those literals. The Cypher result is documented as
columns plus row-major records of arbitrary JSON.

Routes:

- `POST /v1/nodes`, `GET /v1/nodes/:id`, `PUT /v1/nodes/:id`, `DELETE /v1/nodes/:id`
- `POST /v1/nodes/:id/labels/:label`, `DELETE /v1/nodes/:id/labels/:label` (label add and remove; both return 204, and removal is idempotent)
- `POST /v1/nodes/batch`, `POST /v1/edges/batch` (many records in one transaction; a single-record insert costs one durable LMDB commit, so
  per-record requests are bound by commit latency, and a batch is all-or-nothing)
- `POST /v1/edges`, `GET /v1/edges/:id`, `PUT /v1/edges/:id`, `DELETE /v1/edges/:id`
- `POST /v1/query` (Cypher execution), `POST /v1/explain` (query plan)
- `POST /v1/search/text`, `POST /v1/search/vector`
- `POST /v1/vectors` (upsert embedding), `DELETE /v1/vectors/:id` (remove embedding from the index and storage)
- `POST /v1/retrieve` (hybrid retrieval)
- `GET /v1/openapi.json` (OpenAPI 3.1 document), `GET /v1/docs` (Scalar UI)
- `GET /health` (unversioned)

### `issundb_mcp`

Model Context Protocol server built on the `rmcp` SDK. Depends only on `issundb`; must not import lower-level crates directly. Holds a single
`Arc<Graph>` and serves the tool surface over one of two transports, selected with `--transport`: `stdio` (default; for clients that launch the server
as a subprocess) or `http` (MCP's Streamable HTTP transport, mounted on an Axum router at `--http-path`, default `/mcp`, bound to `--bind`, default
`127.0.0.1:8000`). Diagnostics always go to `stderr` because the stdio transport owns `stdout`. The HTTP transport still speaks MCP JSON-RPC, distinct
from `issundb-rest`. The `rmcp` dependency is pinned to `0.11` because `0.12` and later require `darling` `0.23`, which exceeds the workspace MSRV
(`1.85`). Because the `rmcp` `0.11` Streamable HTTP transport does not validate the `Host` header (DNS rebinding, GHSA-89vp-x53w-74fx, fixed upstream
only in `rmcp` `1.4.0`), the HTTP arm wraps the router in a `Host` header allowlist middleware: it defaults to the loopback names (`localhost`,
`127.0.0.1`, `::1`) plus the `--bind` host, repeat `--allowed-host` to add proxy-forwarded hostnames, and a missing or non-allowlisted `Host` gets
HTTP 403.

The tool surface is deliberately curated for an LLM agent: reads, queries, and retrieval only. Tools: `get_node`, `get_edge`, `cypher_query`,
`explain`, `text_search`, `vector_search`, and `retrieve_hybrid`. There are no typed mutation tools: graph mutations are expressed as Cypher (
`CREATE`,
`SET`, `REMOVE`, `DELETE`, `MERGE`) through `cypher_query`. The responses are bounded and self-describing for an agent consumer: `get_node` and
`get_edge` truncate string property values at `max_property_chars` (default 2000) with an explicit marker and accept a `properties` selection list,
`text_search` hits carry the matched label, property, and a bounded value excerpt, `vector_search` hits carry the node's labels, and
`retrieve_hybrid` reports `truncated` when the `max_nodes` cap cut off expansion. Index administration, vector loading, thread control, and backup/restore are operator
concerns driven through the CLI or the Python and REST surfaces. Keep this surface minimal: every additional tool dilutes the agent's tool selection.
`get_node` and `get_edge` take the internal engine id, the same value Cypher's `id(n)`/`id(r)` returns, never a domain property such as `Id`: the two
live in separate numbering spaces and can collide, so passing a domain identifier straight to `get_node` silently returns the wrong,
differently-labeled entity instead of erroring. The tool descriptions, argument docs, and server instructions all say this; resolve a domain
identifier first with `MATCH (n:Label) WHERE n.Id = x RETURN id(n)`, or pass `expect_label` (`get_node`) or `expect_type` (`get_edge`) to reject a
mismatched entity with an error instead.

Startup calls `Graph::materialize_edge_statistics` before either transport serves, for the same reason REST does: an agent issuing Cypher through
`cypher_query` cannot ask for the optimizer's statistics, and a session outlives any one tool call. `--no-warm-statistics` (or `ISSUNDB_NO_WARM_STATISTICS`)
skips it. `issundb-cli` warms on every open, at launch and on `:open`, and takes no flag: a slow open there is visible and interactive rather than a
readiness contract. `issundb-py` deliberately does not warm on construction, because a short script should not pay a scan it may never use; it exposes
`materialize_edge_statistics` and `materialize_property_columns` so a long-lived Python process asks for itself. A caller that measures IssunDB through the
Python binding and does not call them is measuring the planner with its statistics unavailable, which is worth stating in any comparison, the same way an
index-creation step is.

### `issundb_py`

Python bindings via PyO3. Exposes a single `IssunDB` class. The `extension-module` feature must be enabled for the Python extension to compile.
Depends only on `issundb`.

Methods: `add_node` (accepts a single label string or a list of label strings), `add_nodes`, `get_node`, `update_node`, `delete_node`, `add_label`,
`remove_label`, `add_edge`, `add_edges`, `get_edge`, `update_edge`, `delete_edge`, `query`, `explain`, `upsert_vector`, `remove_vector`,
`vector_search` (with optional `label` and JSON-object `properties` filters), `configure_vector_index`, `text_search`, `create_text_index` (with
optional `language`), `drop_text_index`, `list_text_indexes`, `has_text_index`, `retrieve_hybrid`, `set_thread_count`,
`materialize_edge_statistics`, `materialize_property_columns`, `backup`, `backup_compact`, and `restore`.

`add_nodes` and `add_edges` take an iterable of `(labels, props_json)` pairs and `(src, dst, type, props_json)` tuples respectively, and write the
whole batch under one `Graph::update` transaction. A single-record insert costs one durable LMDB commit, so a Python loop over `add_node` is bound by
commit latency rather than by the work; the batch form is the ingestion path. Both are all-or-nothing: any failure rolls back the whole batch.

`materialize_edge_statistics` and `materialize_property_columns` are the two deliberate warm-ups. Nothing builds either structure as a side effect of a
query, so a Python process that never calls them plans every relationship pattern on the global average fan-out and gets no selectivity estimates; they are
exposed because a Python caller otherwise has no way to ask. They are not equal in cost: the edge statistics are one pass over the label index and the
adjacency (measured at 226 ms over 300 K nodes), while the property columns are a full node scan whose result holds every scalar node property in memory
for the life of the object (1355 ms over the same graph). Call the first freely in a long-lived process; treat the second as a memory commitment.

Every method releases the GIL around the native engine call, so a long-running query, backup, reindex, or warm-up does not stall other Python threads.
Keep that invariant when adding a method: extract arguments to owned Rust values first, run the engine call and JSON serialization inside
`Python::detach`, and never touch a Python object in the released section. The two warm-ups are the longest-running calls on this surface, so they are
where the invariant matters most.

### `issundb_core::Storage`

Internal to `issundb-core`. Owns the LMDB environment and twelve sub-databases: `nodes`, `edges`, `out_adj`, `in_adj`, `label_idx`, `type_idx`,
`node_prop_idx`, `edge_prop_idx`, `fts_postings`, `fts_docs`, `vectors`, and `meta`. Do not expose `Storage` through the `issundb` facade.

### `issundb_core::error::Error`

All `issundb-core` errors unify here. Variants cover storage (`heed::Error`), encoding (`rmp_serde::encode::Error`), decoding
(`rmp_serde::decode::Error`), and domain errors (`NodeNotFound`, `EdgeNotFound`). Callers outside `issundb-core` match on this type; do not leak
`heed`
error types through the public facade.

### Encapsulation Rule

`Storage` and the `storage` module are `pub(crate)` inside `issundb-core` and are not reachable from any other crate. The `issundb` facade re-exports
only `Graph`, `Error`, `Hit`, hybrid retrieval types and functions, Cypher result types, the schema ID and record types, and the counting-kernel spec
types (`TriangleCountSpec`, `PathCountSpec`, `GroupedDegreeSpec`, and `NeighborCountSpec`), which are re-exported because the `Graph` methods taking them
are part of the documented public surface and a method whose argument type cannot be named is not callable. Do not add a "just for now" re-export
anywhere else; add a deliberate testing helper in `issundb-core` if a test needs internal access.

## Workflow

Before coding:

1. Identification of whether this is a storage, query, vector, hybrid retrieval, bindings, or docs change.
2. Reading of the touched module and nearby tests.

Implementation using red-green TDD:

1. A failing `#[test]` that describes the expected behavior (red). For invariants, prefer a `proptest` property.
2. Verification that the test fails for the right reason, running `make test` or `cargo test -p issundb-core -- <test_name>` (red).
3. The smallest implementation that makes the test pass (green).
4. Refactor while keeping tests green.
5. Narrowest relevant test while iterating, then `make test` and `make lint` before declaring done.
6. `make format` before every commit.
7. Update of `README.md` or `docs/` if behavior or workflow changed.

Additional validation when relevant:

- `make bench` for performance-sensitive storage changes.
- `make test-conformance` for Cypher conformance coverage.
- `make bench-ladybugdb` for cross-engine performance comparison and differential correctness checks on the Cypher execution path.
- `make bench-search-data` to download the Stack Exchange datasets (into the gitignored `data/` path) that back the text, vector, and hybrid retrieval
  benchmarks. Those benches are gated on `ISSUNDB_BENCH_SEARCH_DIR` and skip cleanly when it is unset, so they never block `make bench`.

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
