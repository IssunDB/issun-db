# Agent Guidance

## Priorities

1. Correct storage behavior, including ACID transactions, adjacency consistency, and ID uniqueness.
2. Clear boundaries between storage, queries, indexes, retrieval, and the public facade.
3. Reproducible, benchmark-backed performance after correctness is covered.
4. Idiomatic Rust, with `unsafe` only where necessary and documented.

## Working Rules

- Use English for code, comments, docs, and tests. Prefer small, focused changes.
- Run development commands through `nix develop -c <command>`. Use the `msrv` shell for MSRV checks and the
  `stable` shell for coverage. LadybugDB Make targets select their own shell automatically.
- Read the scoped `AGENTS.md` before changing a crate. `web/README.md` provides the playground's scoped guidance.
  Update guidance when its rules change; keep file inventories, API catalogs, and implementation walkthroughs in the code or user docs.
- Keep mutable engine state inside `Graph` and `Storage`. Do not add module-level runtime globals.
- Do not bypass the graph write lock or the storage write lock. Keep async out of the core engine.
- Comment only non-obvious storage invariants, LMDB lifetime constraints, and algorithm ordering or duplicate-handling rules.
- Preserve the MIT or Apache-2.0 license boundary. Do not add or statically link copyleft, weak-copyleft, or
  source-available dependencies. Keep comparison harnesses that link external engines outside the root workspace.
- Use shared workspace dependency declarations when one exists. Keep dependencies used by only one crate local to it.

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

## Dependency Boundaries

- Core must not depend on indexes, retrieval, queries, or consumers.
- Vector and text may depend on core, but not on each other or higher layers.
- Retrieval may depend on core, vector, and text. Cypher may use their public APIs, never storage internals.
- The facade composes the public API. CLI, REST, MCP, Python, wasm, and examples must depend only on the facade.
- Keep `Storage`, backend errors, and internal modules out of facade exports. Do not add temporary re-exports for tests.
- Do not make `Graph` generic over a storage-backend trait; that would push the backend parameter through the public API.
- Keep tokenization in core so postings and node records can be written in the same transaction. Text queries must use
  the same tokenizer rather than introducing a second implementation.

## Transaction and Feature Boundaries

- Preserve durability. Do not enable `MDB_NOSYNC`, `MDB_NOMETASYNC`, or `MDB_WRITEMAP` to improve write throughput.
- Vector index updates cannot join or roll back with `Graph::update`. Keep index-before-storage ordering so a rejected
  vector does not become durable. Use `WriteTxn::put_vector_bytes` when bytes must commit with graph writes.
- Preserve atomicity across data-statement pipelines. Schema and bulk-administration statements commit separately;
  do not imply that a transaction spans requests.
- Forward `lmdb` and `hnsw` with `default-features = false` through the crate chain. Binary consumers must expose
  selectable features rather than forcing them on their facade dependency.
- Test alternative backends per crate. Whole-workspace `--no-default-features` still unifies consumer defaults.
  `cargo tree -p issundb --no-default-features` must contain neither `usearch` nor `heed`.
- Never enable `hnsw` for the browser build: its C++ dependency cannot build for that target.

## Consumer Constraints

- Keep REST focused on data and retrieval. Index administration, thread control, and backup and restore belong on
  the CLI or Python surface. Keep MCP and browser surfaces curated; mutations go through Cypher.
- Keep REST readiness and the MCP initialize handshake independent of statistics warm-up. Do not warm property
  columns automatically: holding every scalar property in memory must remain an operator's choice.
- Keep OpenAPI response schemas in sync with handler JSON literals; those structs do not generate the actual responses.
- MCP diagnostics must use stderr because stdio owns stdout. Preserve the HTTP Host allowlist while the pinned
  `rmcp` version lacks Host validation. Internal engine IDs and domain property IDs are different numbering spaces.
- In Python methods, extract owned Rust arguments before releasing the GIL. Keep engine work and JSON serialization
  inside `Python::detach`, and never touch Python objects there.
- Keep wasm logic testable without constructing `JsError`, which panics on native targets. The wasm-bindgen CLI
  version must match the crate version exactly.

## Testing and Workflow

- Read the touched code and nearby tests first. For behavior changes, write a failing test, verify its failure,
  implement the smallest fix, and refactor while keeping it green.
- Keep core unit tests beside the implementation and cross-crate tests behind the facade or `Graph` API. Do not
  reach into storage internals from integration tests. Move tests with code when moving a module.
- Use fresh `TempDir` instances for LMDB tests. Never share environments across tests. Gate reopen and backup tests
  on `lmdb`, since the in-memory backend does not persist.
- Keep conformance scenarios in the facade's conformance directory, gated on `ISSUNDB_CONFORMANCE=1`.
- The general row pipeline is the oracle for query fast paths. Relevant suites must agree with and without
  `ISSUNDB_ROW_PIPELINE_ONLY=1`. Tests requiring a lowered operator and the fast half of a differential comparison
  must pin `exec_mode::fast_paths_required`, or a sweep can silently skip the behavior being tested.
- Preserve parallel-edge tests when changing graph algorithms. Neighbor counts, influence, shortest paths, and
  Cypher row multiplicity have different duplicate rules; the simple-graph NetworkX corpus cannot catch these mistakes.
- Run targeted tests while iterating, then `make test`, `make format`, and `make lint` for code changes.
  Run backend checks for backend changes, conformance checks for Cypher changes, and benchmarks for performance changes.
- Keep lint gates in `lints.yml` on the MSRV compiler. A newer Clippy can report different lints; test that compiler
  explicitly before adding a gate. Keep rustdoc warnings denied.
- Run `make playground-check` for playground catalog or Cypher changes affecting it. Rust tests do not execute the
  Cypher strings embedded in JavaScript.
- Keep LadybugDB comparisons outside default builds and tests. Add correctness queries to the differential corpus,
  not the timed workload. Curated queries must not reuse an edge across relationship slots: LadybugDB permits reuse
  that openCypher forbids. Generated queries need the independent reference oracle to attribute divergences.
- Keep tests deterministic and assertions targeted. Storage changes need coverage for transaction safety,
  adjacency consistency, ID uniqueness, and affected indexes or registries.
- Update user docs when behavior or workflow changes, and fix stale related docs in the same patch.

## Review Guidelines (P0/P1 Focus)

Report only critical issues. P0 covers data loss, transaction safety, broken builds, and broken test workflows.
P1 covers adjacency inconsistency, incorrect ID allocation, missing write locks, and risky storage changes without tests.
For each finding, give severity, file and line, the issue, why it matters, and the smallest fix direction.
Do not include style-only feedback or broad praise.
