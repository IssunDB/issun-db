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
- Add comments only when they clarify a non-obvious storage invariant, an LMDB lifetime constraint, or an algorithm kernel's ordering or duplicate-handling rule.
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
- Headings in Markdown files must be in title case: "Build from Source" not "Build from source". Minor words stay lowercase unless they are the first
  word: the articles (a, an, the), the coordinating conjunctions (and, but, or, nor, so, yet, for), and the short prepositions (in, on, at, to, by, of,
  up, as, from, with, into, over). The example above is why the prepositions are named: "from" has to be lowercase for "Build from Source" to be
  correct, and an earlier version of this rule listed only through "of", which made its own example a violation.
- Do not bold the lead-in of a list item. Write "Vector and set similarity: ..." not "**Vector and set similarity**: ...".
- Use sentence case for the lead-in of a list item. Write "Seed selection: ..." not "Seed Selection: ...". Proper nouns keep their capitals.
- Capitalize only the first part of a hyphenated compound: "Full-text Search" in a heading, "Breadth-first" at the start of a sentence, and
  "breadth-first search" elsewhere. Never write "Breadth-First".
- Start each sentence with a capital letter, capitalize proper nouns (Rust, Cypher, LMDB), and leave common nouns lowercase in the middle of a sentence.
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
    - `src/graph/kernels/`: graph algorithm implementations over the CSR snapshot, split by family: `traversal.rs`, `analytics.rs`, `paths.rs`, and
      `flow.rs`. Almost every kernel reads the snapshot and nothing else, so one gate (`Graph::with_snapshot`) covers them; a kernel needing a per-edge
      property the snapshot does not carry reads that property from storage per call. `label_propagation` is the one exception, walking
      `all_neighbors` per node per iteration instead, which is why it needs no gate and is far more expensive than its siblings. Where a result's sequence is observable the kernel fixes it deliberately:
      a traversal reports reached nodes in ascending dense (so ascending node id) order, each frontier is sorted, and Brandes accumulates over sources
      and predecessors in that order, so a betweenness total is reproducible rather than merely close. No depth-first kernel recurses over graph structure,
      because a stack overflow aborts the process instead of returning an error; `dfs` is the sole exception, bounded by its `u8` hop count.
    - `src/graph/txn.rs`: `ReadTxn` and `WriteTxn` delegation impls and transaction tests.
    - `src/csr.rs`: in-memory CSR snapshot (outgoing arrays plus a transposed incoming view with per-edge type and edge ids), rebuilt in the
      background and swapped via `arc-swap`. Also owns the `GraphDelta` buffer captured on the write path (whose only consumers are the property
      column caches) and the `write_gen`/`snapshot_gen` generation counters that drive on-demand CSR refresh. The adjacency is read from `out_adj`, whose 20-byte
      `AdjEntry` carries every field the arrays hold, already grouped by source in ascending key order, so the build decodes no `EdgeRecord` and copies
      no property blob. Entries go straight into the flat arrays, counting each row as it goes: a per-node `Vec` staged first, as this once did, cost
      one allocation per node and left 3.3 GB resident for 620 MB of live arrays on a 1 M-node, 13.9 M-edge graph, because a million freed small chunks
      are holes the allocator cannot return. Because `DUPSORT` orders duplicates by their raw little-endian bytes, each row is then reordered by
      ascending edge id, which is the order every consumer has always seen and which `load_weights` binary-searches. The per-entry `edge_weight` is
      `Option`, built only by `build_weighted`, since a weight lives in the edge's property blob and only Dijkstra reads one.
    - `src/columns.rs`: in-memory property columns for the read path. One typed column (`Int`, `Float`, `Bool`, dictionary-encoded `Str`, or the
      exact-semantics `Json` fallback) per node property, built lazily from one full node scan and kept fresh by a post-commit delta (node deletion
      forces a rebuild). Read through `Graph::node_prop_json`. Also owns the lazily computed per-property statistics (`PropStats`: bounds, an
      equi-depth histogram, and the most common values) that back the selectivity estimates, invalidated by the post-commit patch. Which readers may
      cause the build is deliberate, because the build is one full scan: a gather larger than `SMALL_GATHER_MAX` does, a smaller one is served straight
      from storage (`should_serve_directly`), and the advisory statistics never do (`with_existing_mut` rather than `with_fresh`).
    - `src/histogram.rs`: equi-depth histogram over property values with equality and range selectivity estimates; backs `PropStats`. Nothing here is
      persisted.
    - `src/threads.rs`: the one resolution of the thread budget every parallel consumer shares (`threads::resolve`). Precedence is the programmatic
      override from `set_thread_count`, then `ISSUNDB_NUM_THREADS`, then `OMP_NUM_THREADS`, then the machine's parallelism, clamped to `MAX_THREADS`.
      Both the counting kernels' scoped threads and the analytics passes that split over nodes or sources resolve through it (`Graph::kernel_threads`),
      so the one knob has one meaning and two overlapping passes cannot each claim the whole machine. `OMP_NUM_THREADS` is honored because setting it is
      how a caller caps parallelism process-wide, including this repository's own `test` and `coverage` targets.
    - `src/storage/memory.rs`: the in-memory storage backend, second implementor of the contract in `storage/mod.rs`. Byte-ordered `BTreeMap` tables with
      `BTreeSet` duplicate values, copy-on-write transactions over `ArcSwap`, and a single writer lock. It is what a target with no libc compiles, and it is
      what holds the storage seam to something: the whole suite runs against it (893 tests across core, vector, text, retrieval, and cypher).
    - `src/error.rs`: `Error` enum; all storage and serialization errors unify here. `Error::Storage` carries `storage::StorageError`, which is the selected
      backend's error type, so the variant is `heed::Error` on a default build and unchanged from before the backend split.
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
- `crates/issundb-vector/`: vector index abstraction, vector metadata, vector storage integration, and vector search APIs. The index itself sits behind
  `backend.rs`, which selects one at compile time from the `hnsw` feature: on by default it is `usearch`, the workspace's only C++ dependency, and with
  `--no-default-features` it is an exact scan in pure Rust. The fallback is not a stub. It returns the true nearest neighbors under the same distance
  conventions (`exact_distance`, shared with the rescore pass), so the crate's whole suite passes either way; what it gives up is the sublinear query and
  `quantization`, which it ignores because it keeps the raw `f32`. The feature is forwarded by every crate that reaches this one (`issundb-cypher`,
  `issundb-retrieval`, and the `issundb` facade), and the workspace declarations of those three carry `default-features = false` so that
  `--no-default-features` on the facade actually reaches the bottom of the graph rather than being re-enabled by a sibling. Verify a change to this
  plumbing with `cargo tree -p issundb --no-default-features | grep usearch`, which must print nothing.
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
- `crates/issundb-wasm/`: browser bindings, exposing one `Playground` type that owns a single `Graph`. Depends only on `issundb`, and is the only crate
  built for `wasm32-unknown-unknown`. It is what proves the storage-backend seam and the pure-Rust kernels actually hold: the module is built
  `--no-default-features`, so storage is the in-memory backend and the vector index is the exact scan, and a regression that reintroduces an LMDB or C++
  dependency below the facade breaks this build rather than going unnoticed. Do not add `--features hnsw` to that build, which reads like it selects the
  index and in fact selects `usearch`: the wasm build then fails compiling `cxx`. `make playground-check` is where that was caught, so keep the flags in
  the `WASM_BUILD` variable rather than repeating them per target. Every method returns a JSON string, so the boundary carries one
  type in both directions instead of a second serialization contract. The methods are split into a private logic layer returning `Result<_, String>` and a
  thin exported layer that converts to `JsError`, because constructing a `JsError` calls a wasm-bindgen import that panics off-target, and without the
  split none of it could be covered by `cargo test`. Reading all of a node's properties decodes the stored msgpack blob directly, as the REST node route
  does, since every read-path method on `Graph` takes the property names to fetch and an inspector cannot know them.
- `web/`: the playground page that loads that module: `index.html`, `app.js`, `demos.js`, and `style.css`, with the generated module in the gitignored
  `web/pkg/`. Vanilla ES modules with no build step, and no library is fetched from a network, so the Cypher highlighter and the force-directed layout are
  written in `app.js` rather than pulled from one. The page's only external request is the Google Fonts link for Inter and JetBrains Mono, which is the same
  request `theme.font` in `mkdocs.yml` already makes for the same two families; the size scale is the reference playground's, in rem against a 1rem body. It is served under the MkDocs site and styled to match it: the custom properties at the top of
  `style.css` are Material for MkDocs' own tokens, copied from the built `palette.*.min.css` for this site's palette, and the scheme is carried on
  `data-md-color-scheme` with Material's `default` and `slate` values. A `theme.palette` change in `mkdocs.yml` means updating that block from a fresh
  `make docs` build rather than from the Material Design palette, since MkDocs derives its primary from the named color instead of using it directly. `demos.js` holds the example catalog, the
  Setup panel's six sample graphs, and the sidebar's procedure reference, all of which are Cypher inside a JavaScript file and therefore invisible to every
  Rust test; `make playground-check` runs all three through the compiled module and fails on an error, which is how a wrong procedure signature is caught.
  A sample graph is the only place a dataset lives: every example queries whatever is loaded rather than creating its own, each category names the graph it
  queries through `sample` and the label that proves it is loaded through `requiresLabel`, and the check seeds that graph before running the category. The one
  exception is the Cypher basics lesson on `CREATE`, which writes two nodes.
  Selecting an example or a sample loads it into the editor without running it, since running a `CREATE` on click wrote to the database before the statement
  had been read and a second click silently duplicated its data; the full-text and vector examples keep their post-statement step by holding the selected
  example until the run. The sample graphs carry no comments, being data rather than documentation. `app.js` also holds a Cypher formatter, whose casing rule
  is narrower than the highlighter's keyword set on purpose: uppercasing every word in that set rewrote `issundb.shortestPath` and the case-sensitive yield
  fields `index` and `count`. It must not be able to change what a query means, which is checked by running every string in `demos.js` before and after
  formatting and comparing the rows. The procedure reference is written out by hand
  because the engine cannot enumerate its own procedures, so that check is the only thing keeping it from drifting; it treats `ProcedureNotFound` as a failure
  even for the two retrieval entries whose empty-index error it tolerates, since a rename is exactly what that error reports.
  `docs/hooks/playground_links.py` is the MkDocs hook putting a "Run in the playground" link under a Cypher block in `docs/` marked `<!-- playground -->`,
  carrying the block as `q` and the page's earlier marked blocks as `s`. The marker is opt-in because most documented Cypher cannot run in the playground
  (a query parameter, a CLI script, or embeddings the seeded graph lacks), so marking a block asserts that it runs, and `make playground-check` executes
  every marked block and fails one returning no rows.
  See `web/README.md` for the three build targets and what the browser configuration gives up (no persistence, one thread,
  no `backup`/`restore`, and a 16 MB stack set by a link argument in `.cargo/config.toml` because the 1 MB default is also the engine's inline-execution
  budget).
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
- `Cargo.toml`: workspace root with shared `[workspace.dependencies]`, 39 entries. A shared dependency belongs here and a crate should reach it with
  `workspace = true`. The manifests have drifted from that: `serde_json` is declared independently in six crates, and `anyhow`, `rmp-serde`, `serde`,
  and `tracing` in two each, all of them despite a root entry that those declarations bypass. A further four are pinned per crate with no root entry
  at all (`clap` in three crates, and `axum`, `tokio`, and `tracing-subscriber` in `issundb-rest` and `issundb-mcp`), and the two `axum` pins disagree
  on their version string. Consolidating is a manifest change nobody has made yet, so do not read the current layout as the intended one.
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
- The CSR snapshot backs the graph algorithms, pattern matching, and multi-source expansion. It is kept fresh on demand rather than by a periodic
  rebuild, through one gate: `Graph::ensure_snapshot_fresh`, reached by `Graph::with_snapshot`. Every algorithm kernel reads the snapshot and nothing
  else, so there is one freshness condition, the installed `snapshot_gen` against the committed `write_gen`.
    - `Graph::open` builds nothing: it installs an empty snapshot through `CsrCache::new_unbuilt`, so the gate does the first build when a consumer that
      needs one runs. A workload of point lookups, property reads, or small typed expansions never builds it, because those paths read LMDB directly.
      The unbuilt cache starts `write_gen` at 1 with `snapshot_gen` at 0 so it reports stale; a placeholder that claimed to be current would make typed
      expansion read zero rows out of the empty snapshot. Do not reintroduce an eager build in `open`: it costs a full edge scan on every open and is
      repaid on every reopen.
    - `shortest_path_dijkstra` is the one consumer needing more than the adjacency, and it goes through `Graph::with_weighted_snapshot`. Per-edge weights
      cost a second full scan of `edges`, so what the snapshot carries is a separate condition from its generation: an unweighted snapshot is current at
      its generation and still has no weights. The request is sticky (`CsrCache::request_weights`), or a workload alternating Dijkstra with anything else
      would rebuild twice per write; the cost is eight bytes per edge held once anything asks a weighted question, which is also why `Graph::rebuild_csr`
      does not ask: every bulk load calls it, so asking there would pin the weights in every process that loads data.
    - Typed bulk expansion goes through the same gate; for a small source set over a stale snapshot it skips the gate entirely and reads per-source LMDB
      adjacency (`STALE_POINT_EXPAND_MAX`), so an interleaved write-then-expand workload never pays a rebuild. The background rebuild after
      `REBUILD_THRESHOLD` writes is a compaction safety net, not the freshness path; callers needing a guaranteed fresh CSR view still call
      `rebuild_csr`. Point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`) read the `out_adj` and `in_adj` stores directly through
      the transaction, never the snapshot, so they always reflect committed and in-transaction writes.
- `Storage::open` is the only entry point for the storage engine, and the engine is selected at compile time from the `lmdb` feature: on by default it is LMDB
  (`storage/lmdb.rs`), and with `--no-default-features` it is the in-memory backend (`storage/memory.rs`). `heed` is named in exactly two places, both inside
  `storage/`, so nothing above the storage module knows which engine it is talking to; everything else names the aliases `storage::{RoTxn, OwnedRoTxn, RwTxn}`
  and the twelve tables on `Storage`. Do not reintroduce a `heed::` path outside `storage/`, and do not turn this into a trait: the tables hang off `Storage`
  which hangs off `Graph`, so a trait would make `Graph` generic over its backend and push that parameter through every crate and the public API.
  The contract a backend owes is documented on `storage/mod.rs`, and three of its guarantees are load-bearing rather than incidental: key order is byte order
  (a `u64` key is stored big-endian so byte order and numeric order agree, which is what lets the CSR build assume `out_adj` arrives grouped by ascending node
  id), duplicate order is byte order (which the CSR row-reordering pass depends on), and an uncommitted transaction publishes nothing. The in-memory backend is
  copy-on-write for the third: a reader loads the published table set and a writer publishes its own copy at commit, which is also what makes a read
  transaction opened *while* a write transaction is live legal. That is not a nicety, it is required: a `MATCH ... CREATE` statement runs its match against
  committed state through a separate read transaction while its own write transaction is still open, and a single reader-writer lock deadlocks on exactly that.
  The in-memory backend does not persist, so a reopen sees an empty graph; the handful of tests whose premise is reopen or backup are gated on the `lmdb`
  feature and say so.
- The `lmdb` feature is forwarded by every crate between the facade and core, and each of their workspace declarations carries `default-features = false`, or a
  sibling silently re-enables LMDB for the whole graph. Verify a change to that plumbing with `cargo tree -p issundb --no-default-features | grep lmdb`, which
  must print nothing. Note that a whole-workspace `--no-default-features` build does *not* select the in-memory backend, because `issundb-cli` and the other
  consumer crates depend on the facade with its defaults and cargo unifies features; test the backend per crate (`cargo test -p issundb-core --no-default-features`).
- Heavy dependencies are tracked in the workspace `Cargo.toml`. `chumsky` is an active, non-optional dependency; `usearch` is the workspace's only
  C++ dependency and sits behind the default-on `hnsw` feature. The graph algorithms are pure Rust over the CSR snapshot, so the build needs no CMake,
  no Clang, no bindgen, and no OpenMP runtime.
- Async is not used in the core engine. LMDB is synchronous. `tokio` is an optional dependency for server mode only; do not add `.await` inside
  `issundb-core`.
- Parallelism has two consumers, and both resolve their thread count through `threads::resolve` (see the module map): the scoped-thread reductions in
  the counting kernels, and the analytics passes that split over nodes (PageRank) or over sources (betweenness and harmonic centrality). Both split a
  pass only above `MIN_PARALLEL_WORK` items, so a small pass and a unit test stay serial and deterministic. The budget is then capped by regime, which is
  why there are two resolvers: `Graph::kernel_threads` caps at `MAX_SCAN_THREADS` because a pass streaming the adjacency arrays (a counting kernel, or
  PageRank) saturates memory bandwidth before compute and gets *slower* past that point, while `Graph::parallel_threads` leaves the budget uncapped for
  the all-pairs passes, whose cost is arithmetic per source out of per-worker buffers. PageRank and harmonic centrality write disjoint output chunks and
  are therefore split-invariant; betweenness sums per-worker partials, so the last bits of a total depend on the worker count. Writes are never parallel:
  they serialize on the `ReentrantMutex` write lock and on LMDB's single writer.

## Dependency Boundaries

Target dependency direction:

1. `issundb-core` sits at the bottom. It must not depend on the vector, text, retrieval, Cypher, bindings, server, or CLI crates.
2. `issundb-vector` may depend on `issundb-core`, but not on text, retrieval, Cypher, bindings, server, or CLI crates.
3. `issundb-text` may depend on `issundb-core`, but not on vector, retrieval, Cypher, bindings, server, or CLI crates.
4. `issundb-retrieval` may depend on `issundb-core`, `issundb-vector`, and `issundb-text`.
5. `issundb-cypher` may depend on public APIs from core, vector, text, and retrieval crates, but not storage internals.
6. `issundb` composes and re-exports the stable public API.
7. `issundb-cli` uses only the `issundb` facade.
8. `issundb-rest`, `issundb-mcp`, `issundb-py`, and `issundb-wasm` must depend only on `issundb`; they must not import `issundb-core`,
   `issundb-vector`, `issundb-text`, `issundb-retrieval`, or `issundb-cypher` directly.

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
- `set_thread_count(n: i32) -> Result<(), Error>`: sets the thread count for the parallel read passes, overriding the `ISSUNDB_NUM_THREADS`
  environment variable (0 restores default behavior, resolved by `threads::resolve`). There is no pool to configure: each pass resolves the budget when
  it starts and spawns scoped threads for its own duration, so the call stores the value, takes effect on the next pass, and cannot fail.

Graph algorithms have self-describing signatures over `NodeId` and `EdgeId`: `bfs`, `bfs_multi_source`, `expand_bulk`, `dfs`, `shortest_path`, `all_paths`, `all_shortest_paths`,
`longest_path`, `shortest_path_top_k`, `page_rank`, `connected_components`, `strongly_connected_components`, `detect_cycle`, `label_propagation`,
`degree_centrality`, `betweenness_centrality`, `harmonic_centrality`, `spanning_forest`, `maximum_flow`, and `all_neighbors`. Several carry behavior
worth pinning:

- `shortest_path_dijkstra(src, dst) -> Result<Option<WeightedPath>, Error>`: edge weight is the first present of the `weight`, `cost`, `capacity`, or
  `cap` property, default `1.0`; the source is fixed, so unlike `shortest_path_top_k` and `spanning_forest` this method takes no weight-property
  argument. Relaxation is Dijkstra's over a binary heap, which needs non-negative weights; a weight comes from a property, so a negative one is a data
  condition rather than a bug and the pass falls back to a bounded label-correcting relaxation when the snapshot reports any (`has_negative_weight`,
  decided once at build time so a point query does not scan every weight). A reachable negative *cycle* has no shortest path and is reported as
  `Error::InvalidArgument`. Parallel edges need no special handling, since relaxing each keeps the cheapest.
- `connected_components() -> Result<HashMap<NodeId, u64>, Error>`: the component id is the smallest *node id* in the component. Only the induced
  partition is contractual, so compare membership rather than depending on the numbering.
- `betweenness_centrality() -> Result<HashMap<NodeId, f64>, Error>`: unnormalized and directed, and counts distinct pairs: two parallel edges are one
  shortest path, so crediting both would inflate the path counts and every dependency downstream of them.
- `degree_centrality(direction) -> Result<HashMap<NodeId, u64>, Error>`: the number of *distinct* neighbors in that direction. Parallel edges between the
  same pair count once, `Both` is the distinct out-neighbors plus the distinct in-neighbors, and a self-loop counts in each direction. This is the
  boolean-adjacency semantics of the SpMV formulation it replaced, kept deliberately: a plain row length would count parallel edges separately and
  silently change the score on a multigraph.
- `page_rank(iterations, damping) -> Result<HashMap<NodeId, f32>, Error>`: power iteration where a source spreads its rank over its *edges*, so parallel
  edges do each carry mass, which is the opposite of the distinct-neighbor rule above and is why the two are pinned separately. Dangling-node mass is not
  redistributed, so ranks do not sum to 1; `tests/oracle.rs` compares against NetworkX over a corpus restricted to graphs with no dangling nodes for
  exactly that reason. The accumulation reads the incoming rows, so each output entry is a sum over one node's in-edges, which is what makes the pass
  parallel over disjoint output chunks and independent of the worker count.
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

The executor resolves patterns through the physical plan. Both typed and untyped expansion read the CSR snapshot in bulk behind
`ensure_snapshot_fresh` (`Graph::expand_bulk`), falling back to per-source LMDB point reads when the snapshot is stale and the source set is small. Key optimizer behaviors,
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

REST exposes the data plane and retrieval only. Index administration (vector index configuration, text index create/drop/list), thread
control, and backup/restore are intentionally absent: provisioning and host operations are done through the CLI or the Python surface, not over HTTP.

Startup spawns `Graph::materialize_edge_statistics` on a detached thread, so the optimizer's expand-ratio estimates and exact type-inference pruning become
available without gating readiness: nothing builds that table as a side effect of a query, and an HTTP caller has no way to ask for it. It is deliberately
*not* synchronous. The scan costs seconds on a large graph (3.4 s on a 1 M-node, 13.9 M-edge graph, measured through the release wheel), while the plans it
sharpens measured a few percent on a workload of ordinary aggregations, so delaying readiness to buy that is the wrong trade. Backgrounding it is only safe
because `materialize_edge_statistics` scans *without* holding the statistics lock and installs the finished table at the end; concurrent requests keep
planning on the bounded probe and the global average throughout, verified at a 1.5 ms median and an 18.6 ms maximum for point queries issued during the
scan. Do not move the build back under that lock, and do not make this call synchronous again. A failure is logged and ignored, since every reader works
without the table. `--no-warm-statistics` (or `ISSUNDB_NO_WARM_STATISTICS`) skips the scan entirely. The property columns are deliberately not warmed here:
that build is a full node scan and holds every scalar property in memory, which is a footprint decision an operator should make, not a startup default.

The API is self-describing: the OpenAPI 3.1 document is generated from the handler annotations (`#[utoipa::path]`) and the request and response
`ToSchema` derives, served as JSON at `GET /v1/openapi.json` with a Scalar UI at `GET /v1/docs`. The generator crates are `utoipa` and
`utoipa-scalar` (both MIT or Apache-2.0), on the Axum 0.8 line alongside `axum` itself, which is why the route patterns use `{id}` rather than the
`:id` form Axum 0.7 took. Because the handlers build their JSON bodies inline with `json!`, the
documentation-only response structs (`NodeResponse`, `EdgeResponse`, `IdResponse`, `QueryResponse`, `ExplainResponse`, `RetrieveResponse`,
`HealthResponse`, and `ErrorResponse`) describe the response shapes and must be kept in sync with those literals. The Cypher result is documented as
columns plus row-major records of arbitrary JSON.

Routes:

- `POST /v1/nodes`, `GET /v1/nodes/{id}`, `PUT /v1/nodes/{id}`, `DELETE /v1/nodes/{id}`
- `POST /v1/nodes/{id}/labels/{label}`, `DELETE /v1/nodes/{id}/labels/{label}` (label add and remove; both return 204, and removal is idempotent)
- `POST /v1/nodes/batch`, `POST /v1/edges/batch` (many records in one transaction; a single-record insert costs one durable LMDB commit, so
  per-record requests are bound by commit latency, and a batch is all-or-nothing)
- `POST /v1/edges`, `GET /v1/edges/{id}`, `PUT /v1/edges/{id}`, `DELETE /v1/edges/{id}`
- `POST /v1/query` (Cypher execution), `POST /v1/explain` (query plan)
- `POST /v1/search/text`, `POST /v1/search/vector`
- `POST /v1/vectors` (upsert embedding), `DELETE /v1/vectors/{id}` (remove embedding from the index and storage)
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

Startup spawns `Graph::materialize_edge_statistics` on a detached thread, as REST does and for the same reason: an agent issuing Cypher through
`cypher_query` cannot ask for the optimizer's statistics, and a session outlives any one tool call. Backgrounding matters more here than in REST, because a
stdio client launches one subprocess per session, so a synchronous scan would be repeated every session and would land on the initialize handshake where a
client with a startup timeout can abandon it. `--no-warm-statistics` (or `ISSUNDB_NO_WARM_STATISTICS`) skips it. `issundb-cli` warms *synchronously* on
every open, at launch and on `:open`, and takes the same flag: a visible pause before an interactive prompt is honest, and there is no readiness contract
to break. That pause is worth knowing the size of: measured at 3.7 s on a 1 M-node, 13.9 M-edge graph, against a 4 ms open with the flag, so a `--script`
run of a few statements should pass it. Those two numbers come from wall-clocking the process, since `:timer` covers Cypher statements only and neither the
warm-up nor the open is one. `:timer` (or `--timer`) is how a *query* is timed: it measures execution alone, not the row formatting, so it is comparable with
a timing taken around the same query in another surface. `issundb-py` deliberately does not warm on construction, because a short script should not pay a scan it may never use; it exposes
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

### `issundb_wasm`

Browser bindings, exposing one `Playground` that owns a single `Graph`. Depends only on `issundb`. Every method returns a JSON string, so the boundary
carries one type in both directions rather than a second serialization contract to keep in agreement with the page.

Methods: `query`, `explain`, `stats`, `graphSnapshot`, `createTextIndex`, `textSearch`, `upsertVector`, `vectorSearch`, and the four statics `version`,
`isPersistent`, `buildRef`, and `memoryBytes`. `memoryBytes` is live allocated bytes, from a counting `GlobalAlloc` wrapper this crate installs, which the page's
footer reports beside the WebAssembly heap size the browser exposes. It is live rather than reserved because the wasm heap only grows, so the browser's figure
says what was once needed; and it is module-wide rather than per-instance, because an allocator cannot attribute an allocation to a `Playground`. No other crate
in the workspace sets a global allocator, so nothing an application links against pays the two relaxed atomics per allocation. `buildRef` is the `branch@commit` the page names in its footer, read from `ISSUNDB_BUILD_REF` at compile time through
`option_env!` and empty without it. It is compiled in rather than fetched as a sidecar file so it cannot disagree with the module it describes, and the
crate's `build.rs` exists only to declare `rerun-if-env-changed` for that variable, without which a cached artifact would keep reporting an earlier
build's commit. `make playground-build` supplies it from `git`, and `docs.yml` sets it from the workflow's own refs, since
`actions/checkout` leaves a detached HEAD where `rev-parse --abbrev-ref` answers `HEAD`. `query` returns `{columns, rows, statement_count, elapsed_ms}` with row-major rows, so the page renders a table knowing
nothing about the schema, and `statement_count` is how it can say a semicolon-separated script ran more statements than the one result shown.
`graphSnapshot` returns `{nodes, edges, truncated}` capped at `MAX_GRAPH_NODES` for legibility rather than cost, and drops an edge whose endpoint the cap
excluded so the page never draws a line to nothing.

The surface is curated the way the MCP one is, and for a related reason: this is a demonstration rather than an embedding API. It carries reads, queries,
and the two capabilities Cypher cannot reach (full-text index creation and search, and vector upsert and search), because those are Rust extension traits
rather than query-language features. Mutations are Cypher through `query`. `backup` and `restore` are absent, being file operations on a target with no
filesystem.

Two structural rules hold here. Methods are split into a private logic layer returning `Result<_, String>` and a thin exported layer that converts to
`JsError`, because constructing a `JsError` calls a wasm-bindgen import that panics on a non-wasm target, so a binding building one directly could not be
covered by `cargo test`; the tests at the bottom of `lib.rs` drive the logic layer and run in the ordinary test suite. And the wasm-bindgen CLI must be
the exact version of the wasm-bindgen crate, or the module fails at load with a message that does not name the cause, which is why
`make playground-build` compares the two before running.

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

Clippy is pinned to the MSRV, in `lints.yml`, and that pin is load-bearing: a current clippy reports about fifteen further lints in `issundb-cypher` that
the pinned one does not, so they land all at once whenever the MSRV moves. Two consequences. A lint step belongs in `lints.yml` beside `make lint`, never in
`tests.yml`, whose jobs run on stable and would therefore gate on a different lint set than the one the project chose; `make lint-backends` is there for
exactly that reason. And a clean local `make lint` says nothing about a newer clippy, so run `cargo +stable clippy` before adding a lint gate rather than
after CI does it.

Additional validation when relevant:

- `make test-backends` for a storage or vector backend change, and `make lint-backends` alongside it. The default run always selects LMDB and usearch, so
  neither the in-memory backend nor the exact vector index is otherwise exercised as the selected backend.
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
