# Cypher Agent Guidance

Read the root `AGENTS.md` first.

## Parser and Planner Rules

- Keep graph access out of parsing and logical and physical plan construction. Resolve runtime data in execution.
- Add operators to the Pratt binding-power table and test precedence against neighboring operators. Use combinators
  and `recursive` rather than shared mutable lookahead state.
- Keep parse decisions dependent only on query text so cached results and errors remain valid. Keep `parse` uncached
  so parser benchmarks measure parsing. Do not cache oversized queries without a memory bound.
- Preserve stack-budget handling for deep queries rather than adding arbitrary grammar recursion limits.
- Keep AST and plan nodes immutable. Rewrite by constructing replacements; do not add `Cell` or `RefCell` to them.
- Convert expressions to `FilterExpr` in the logical planner. Prefer inspectable comparison variants over the
  general expression fallback when a comparison variant applies.
- Read `optimize_impl` before adding a pass: rewrite order matters. Do not duplicate its pass list in this guide.
- Add gated conformance coverage for new clauses. Keep conformance scenarios out of inline crate unit tests.

## Parse Diagnostics

- Render errors through `render_rich`, never chumsky's `Debug` representation.
- Distinguish query-relative positions from file positions. Use columns for single-line queries and explicitly
  say "query line" for multiline queries. CLI and import callers may add their own file locations, but must not
  renumber query spans: statement trimming and splitting mean they are not faithful file slices.
- Count characters, not bytes, and keep error rendering safe for invalid or mid-character offsets.
- Drop an oversized expected-token list instead of truncating sorted alternatives to a misleading prefix.

## Optimizer and Executor Invariants

- Preserve row multiplicity and relationship uniqueness, including parallel edges and self-loops. Reachability or
  distinct endpoint pairs cannot replace per-edge rows unless multiplicity is provably irrelevant.
- Keep materialized and streaming cyclic joins on the shared `multiway_join_rows` helper. Optimizer recursion must
  visit `MultiwayJoin` children even though the physical planner never emits that operator.
- Evaluate a factorized filter once per source only when it references neither the new relationship nor destination.
  Keep label filters on the bulk label path.
- Declining any fast path must preserve results. Widening the columnar recognizer requires a differential test
  against the row pipeline, including byte-identical columns and records.
- Do not admit repeated relationship types into multi-hop columnar chains without enforcing relationship uniqueness.
- A speculative predicate over a superset must decline on errors rather than raise an error on an unreachable row.
- Group bare node and edge variables by element ID and preserve entity bindings. Serializing whole property bags
  per row changes identity semantics and adds work proportional to the property count.
- Keep count-window boundary ties and canonical group ordering consistent with the full result.
- Preserve short-circuit error semantics: a decisive boolean operand suppresses an error in the other operand,
  but a successfully evaluated non-boolean operand must still raise.
- Never cache rows from a resolved `CALL` in a query plan. Plan validity must account for graph identity, data and
  schema generations, execution mode, and resolved SKIP and LIMIT values.

## Row-pipeline-only Switch

- Keep every exact shortcut that can change rows under `ISSUNDB_ROW_PIPELINE_ONLY`, including type-inference
  pruning. Leaving a pruning pass enabled would hide its false negatives from differential tests.
- Read `exec_mode.rs` before adding a fast path. Tests requiring a lowered operator and the fast half of a
  differential comparison must pin `fast_paths_required`. Approximate vector search is outside this oracle.

## Mutation Safety

- Use the active `WriteTxn`, never auto-committing `Graph` mutations, inside statement execution. A nested write
  transaction can deadlock. Do not call storage internals from the executor.
- Keep a statement's writes and following projections in one transaction. Data-statement pipelines share that
  transaction and its pending-write overlay; errors must roll back all preceding writes.
- Reads after writes must see pending property and structural changes. Keep these statements on the row pipeline,
  since columnar and kernel paths read committed snapshots. Build a writing join side before a dependent read side.
- Validate computed MERGE properties on creation too. Validate whole-map SET values as strictly as single-property SET.
- Do not rebuild caches manually from the executor; publish committed changes through core's freshness mechanisms.
- Keep imports streaming into one transaction. Do not restore a collect-then-write pass that retains every row.
  Both import paths must finish with the explicit CSR and property-column builders so reopen avoids full scans.

## Statement Clock

The scoped statement clock is an intentional exception to the root runtime-global rule. Nested execution must
restore the outer instant, and calls within a statement must see the same instant. Do not widen the thread-local
clock into general engine state or use it to justify interior mutability on AST nodes.

## Export Compatibility

- Reject properties whose names collide with export metadata; accepting them would lose properties or structure.
- Preserve quoted CSV records with embedded newlines. Keep legacy inference for files without `json_properties`.
- Preserve text-index languages through `text_indexes.json`, with `index.cypher` as the fallback for older exports.
- Quote schema identifiers with doubled backticks and parse schema files as complete pipelines so embedded
  semicolons and newlines remain part of identifiers.
