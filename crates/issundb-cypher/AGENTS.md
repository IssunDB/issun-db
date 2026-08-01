# `issundb-cypher` Agent Guide

This file covers crate-specific guidance for contributors working inside `crates/issundb-cypher`. Read the root `AGENTS.md` first; the rules there
apply everywhere and are not repeated here.

## The Query Execution Pipeline

Every Cypher string passes through five stages, each owned by a distinct source file:

1. Parse (`src/parser.rs`): a `chumsky` combinator parser produces a `Statement` AST from the raw query string.
2. Logical plan (`src/plan/logical.rs`): `LogicalPlanner` walks the AST and emits a tree of `LogicalOperator` nodes. All variable bindings and
   label/type resolutions are established here.
3. Physical plan (`src/plan/physical.rs`): `PhysicalPlanner` converts each `LogicalOperator` into a `PhysicalOperator`, choosing access paths (
   label scan, index seek, adjacency expansion).
4. Optimize (`src/plan/optimize.rs`): `Optimizer` rewrites the physical tree. It takes ownership and returns a new tree; it never mutates in place.
   Order matters between several of these, so read `optimize_impl` rather than this summary before inserting a pass: collect label constraints, extract
   filters, split top-level conjunctions, drop statically-true predicates, reorder operators, choose the cheapest scan node (reversing linear expand
   chains), push filters down, lower a `vector_dist` top-k sort to `VectorTopK`, optimize index scans and id seeks, rewrite a correlated equality into
   `CorrelatedIndexSeek`, linearize a re-scanning `HashJoin` into an expand chain, rewrite closing expands into `MultiwayJoin`, reduce a count over a
   bare labeled scan to a constant, lower grouping-free and grouped counts to the `TriangleCount`, `PathCount`, and `GroupedDegree` kernels, push an
   `ORDER BY <count> LIMIT n` into the grouped kernel as a window, prune provably-empty typed hops, and fuse a closing hop into `ExpandIntersect`.
   Everything from the count reduction onward is skipped under the row-pipeline-only switch (see "Row-pipeline-only Switch" below).
5. Execute (`src/exec/read.rs`): `execute_physical` drives the physical tree against a `Graph` reference. The `Filter { input: Expand }` pattern uses a
   factorized fast path in `filter_over_expand_batch`.

Keep each concern in its own file. Do not call `Graph` methods from `parser.rs`, `logical.rs`, or `physical.rs`.

## Parser Structure Rules

The grammar covers multi-label node patterns such as `(n:A:B)` and inline relationship property maps.

The parser is built with the `chumsky` parser-combinator library, in two phases. Phase 1 lexes the query text into a token stream. Phase 2 builds the
combinator graph, with operator precedence expressed through chumsky's Pratt parser (`chumsky::pratt::{infix, left, postfix, prefix}`) rather than a
hand-written descent chain.

- Add a new operator by giving it a binding power in the Pratt rule table, not by inserting a new precedence level function. Getting the binding power
  wrong is the common failure, so cover every new operator against its neighbors in a precedence test.
- Combinators compose bottom-up: a sub-parser must be defined before the parser that uses it, and recursive positions go through `recursive`. Do not
  reach for a shared mutable parser state to carry a lookahead; express it with the combinators.
- Building the combinator graph costs more than consuming the tokens, and for a small query more than executing it. `parse_with_exec_depth` therefore
  serves repeated query text from a bounded thread-local cache of `Arc<Statement>`. Parsing reads no graph state, no parameters, and no clock, so a
  cached outcome (including a parse error) is always valid and the cache needs no invalidation. Keep it that way: never make a parse decision depend
  on anything outside the query text. `parse` is the uncached entry point and must stay uncached so the `parse` benchmarks keep measuring the parser.
- Deep nesting is handled by stack budget, not by recursion limits inside the grammar. An iterative token-stream scan (`scan_nesting`) rejects
  genuinely pathological input before any AST is built, a deep parse runs on a dedicated large-stack thread, and a query whose nesting exceeds
  `SMALL_STACK_EXEC_BUDGET_KB` has its execution dispatched to a large-stack thread by `execute_with_procedures`.

Query text longer than `PARSE_CACHE_MAX_QUERY_LEN` is parsed but not cached, so many large unique statements cannot grow the parse cache by their
length.

### Parse Diagnostics

Both phases report through `render_rich`, which turns chumsky's `Rich` errors into a compiler-style diagnostic: a summary naming what was found, the
position, the offending source line, and a caret under the span. Neither phase may format a `Rich` with `Debug`, which is what produced the
`found 'Integer(4)' at 797..798 expected something else` this replaced.

A position never reads as a file position, because the parser is handed a query and not a file, and the callers that hold both print their own number
beside this one. A single-line query is located by column alone and its snippet gutter is blank, so nothing on the line can be mistaken for a file
line; a query genuinely spanning lines says "query line N", naming what it counts. Do not reintroduce a bare "line N". The callers that depend on this
are `issundb-cli`, which prints `path:N` for the file line a script statement starts on, and `execute_import_db`, which prints `copy.cypher line N`.

Neither caller may renumber a diagnostic onto its file, however tempting: the CLI's statement text is not a faithful slice, since `segment_script`
trims continuation lines (which moves columns) and gives every statement split from one buffer the same start line (which moves lines). A renumbered
position would be confidently wrong, which is worse than one that is honestly relative.

- `Tok` carries a `Display` writing each token as it was spelled in the source, so a diagnostic quotes the author's text rather than the parser's
  representation of it. A new token variant needs an arm, and the compiler requires one.
- `Tok::noun` supplies the word in front of the quote ("number", "string", "keyword"), and `None` for punctuation, where the symbol alone reads
  better. Keywords are told from identifiers by `is_clause_keyword`, since the lexer keeps both in `Tok::Ident`; a non-clause keyword therefore reads
  as an identifier.
- An expected list is printed only when it names at most `MAX_EXPECTED_LISTED` alternatives, and is otherwise dropped rather than truncated. The
  alternatives are sorted for determinism, so a truncation would keep whichever sort first, and at an unlexable character that prefix is every
  punctuation mark the grammar can open a token with.
- Positions count characters, not bytes, and `locate` walks back off a mid-character offset instead of panicking. Rendering runs on the error path, so
  a panic there turns a syntax error into a crash; `rendering_never_panics` covers that with a proptest.
- The expected set chumsky reports is empty at most positions, because the grammar labels almost nothing. `expr_parser` is labelled, which is what
  supplies "expected an expression" after a bare `WHERE`. Labelling more of the grammar is the way to improve these messages further, one construct at
  a time, since a label replaces the expected set for every failure inside the parser it is attached to.

## AST Immutability Policy

- All AST node types derive `Clone` and `PartialEq`. They are produced once by the parser and treated as read-only thereafter.
- The optimizer must not mutate existing AST or physical plan nodes in place. Produce new nodes from rewrite rules and replace subtrees by
  constructing new parent nodes.
- Do not add `Cell`, `RefCell`, or interior mutability to any AST or plan type.

## Optimizer Correctness Invariants

- Preserve row multiplicity. IssunDB is a multigraph: the LMDB `DUPSORT` adjacency stores one entry per edge, so two parallel edges between the same
  pair produce two distinct expansion results and two result rows. A rewrite that collapses one-row-per-edge into one-row-per-(src, dst), or that uses
  a boolean reachability product in place of per-edge traversal, will silently drop rows for parallel edges. Such rewrites (for example a
  `reduce_expand_into` that drops edge emission, or matrix-product chain fusion) are only valid when multiplicity is provably irrelevant; default to
  rejecting them.
- A rewrite must produce the same result set as the unoptimized plan for every graph, not just the common single-edge case. When in doubt, add an
  end-to-end test with parallel edges through the `issundb` facade.

## Adding a New Cypher Clause

Follow this checklist in order:

1. AST variant: add the new clause type to `src/ast.rs`. Derive `Clone` and `PartialEq`.
2. Parser rule: add a combinator for the clause in `src/parser.rs` and wire it into the statement-level `choice(...)` alternation.
3. Logical planner arm: add a match arm in `LogicalPlanner::plan` in `src/plan/logical.rs`.
4. Physical planner arm: add a match arm in `PhysicalPlanner::plan` in `src/plan/physical.rs`.
5. Optimizer arm (if applicable): if the new clause benefits from rewriting, add a rewrite rule in `src/plan/optimize.rs`. Skip this step if no
   rewrite applies; do not add a pass-through arm unless the optimizer explicitly needs to descend into the clause.
6. Executor arm: add a match arm in the physical operator dispatch loop in `execute_physical` (`src/exec/read.rs`).
7. Conformance test: add at least one TCK scenario in `crates/issundb/tests/conformance/` gated on `ISSUNDB_CONFORMANCE=1`.

All seven steps are required for the change to be considered complete.

## `FilterExpr` Vs. `Expr`

- `Expr` (defined in `src/ast.rs`) is the general expression type produced by the parser. It covers literals, property accesses, function calls,
  arithmetic, comparisons, and boolean combinators.
- `FilterExpr` (defined in `src/plan/logical.rs`) is the typed predicate representation used inside `LogicalOperator::Filter` and
  `PhysicalOperator::Filter`. It has explicit variants for common binary comparisons (`Eq`, `Ne`, `Lt`, `Gt`, `Le`, `Ge`), a `HasLabel` variant for
  label checks, and an `Expr` catch-all for predicates that do not fit a named variant (IS NULL, quantifiers, compound boolean expressions).
- Conversion from `Expr` to `FilterExpr` happens in `LogicalPlanner`. Do not perform this conversion in the parser or executor.

## `WhereClause` Variants

`WhereClause` (in `src/ast.rs`) covers the forms that appear in Cypher WHERE positions:

- `Eq`, `Ne`, `Lt`, `Gt`, `Le`, `Ge`: a single binary comparison between two `Expr` operands, e.g. `WHERE n.age > 30`.
- `Expr(Expr)`: an arbitrary boolean expression used for IS NULL checks, quantifier expressions (`ANY`, `ALL`, `NONE`, `SINGLE`), and compound boolean
  sub-expressions (AND, OR, NOT) that do not reduce to a single comparison variant.

`LogicalPlanner` lowers each `WhereClause` variant to the matching `FilterExpr` variant (`Eq` to `FilterExpr::Eq`, and so on, with `Expr` to
`FilterExpr::Expr`).
`FilterExpr` additionally has a `HasLabel` variant for label checks. Prefer the named comparison variants over `Expr` so the optimizer can inspect and
reorder them; fall back to `Expr` only when no comparison variant applies.

## `MultiwayJoin` and Cyclic Pattern Execution

`PhysicalOperator::MultiwayJoin` is emitted by the `rewrite_closing_expands` pass (in `optimize.rs`) when a single-hop directed `Expand` node's
`dst_var` is already bound by an earlier operator in the same plan tree. This is the "closing hop" of a cyclic pattern (triangles, cliques, etc.).

The executor (`exec/read.rs`) closes it in `multiway_join_rows`, over one batch of child rows:

1. Collect the distinct `closing_src_var` node ids in the batch.
2. Bulk-expand from those nodes once.
3. Index the transitions.
4. Emit a row per matching `(closing_src, closing_dst)` pair, binding `closing_rel_var`.

There are two callers of that one implementation: the materializing operator arm, and the streaming `RowStream::MultiwayJoin`. Streaming runs the bulk
expansion once per batch rather than once over every row, which for a typed relationship is a cheap per-source adjacency loop and lets a `LIMIT` bound
the number of batches. Keep both on the shared helper so they cannot diverge; `streaming_directed_multiway_join_matches_materialized` pins that they
agree.

`MultiwayJoin` is optimizer-generated only: `PhysicalPlanner::plan` never emits it. Every match arm in `optimize.rs` that recurses into operator
children must handle the `MultiwayJoin` variant.

## Factorized Execution

`exec/factorize.rs` defines `FactorizedRecordGroup`: a shared `Arc<PathMap>` prefix (bindings from ancestor hops) plus a `Vec` of per-row extensions
`(rel_var, rel_binding, dst_var, dst_binding)` for the current hop. Using `Arc` avoids O(shared_vars) HashMap clone cost for every destination; only
the two new bindings are paid per output row.

`filter_over_expand_batch` (in `exec/read.rs`) handles the `Filter { input: Expand(single-hop, directed) }` pattern over one batch of child rows:

- Factorized fast path: when the filter expression references neither `rel_var` nor `dst_var` (decided by `filter_refs_in_expr`), it is evaluated once
  per source row. Sources that fail skip all their destinations, costing zero row clones for rejected sources.
- Per-row fallback: when the filter touches the expansion variables, the full row is materialized before evaluation.
- `HasLabel` filters: always route through the existing bulk path; this function is not called for `HasLabel` expressions.

## Vectorized Aggregate and Columnar Fast Path

`exec/vectorized.rs` is an alternative executor for read queries whose optimized physical plan is a linear chain (a scan, then zero or more single-hop
expands) topped by a projection, an aggregation, or an aggregation feeding projections and an optional sort. `recognize` inspects the optimized
`PhysicalOperator` tree and returns a `VecPipeline` when the shape qualifies; `execute` runs it, and any unrecognized plan falls through to the row
pipeline in `exec/read.rs`. The fast path is a performance choice only: declining it must never change results.

The `VecRoot` variants escalate in generality:

- `Project`: the root is a RETURN of single-property reads.
- `Aggregate`: every group key and aggregate input is a single-property read on a chain node variable, folded through dense group codes and bulk
  column gathers.
- `AggregateGeneral`: group keys or aggregate inputs are general scalar expressions (CASE, arithmetic, comparisons, IS NULL, function calls) over
  chain node and relationship variables, including edge properties. It binds each row's node and edge ids and folds through the shared `evaluate_expr`
  and `AggState`, so its semantics match the row pipeline exactly. `agg_expr_eligible` gates which expressions qualify; anything it declines stays on
  the row pipeline, so correctness never depends on the gate.

Both aggregate roots read properties from the in-memory columnar store (see "In-memory Property Columns" in `issundb-core/AGENTS.md`): one bulk gather
of every referenced `(variable, property)` column per query rather than a point read per row. Every vectorized shape must be covered by a differential
test that asserts byte-identical columns and records against the row pipeline (`assert_matches_row_path` and the `*_matches_row_path` tests in
`exec/vectorized.rs`); add one whenever you widen `recognize`.

### What `recognize` Accepts

The structural pattern is `[Limit]? [Sort]? [Distinct]? Project [Aggregate]? Stage* (Expand(directed single hop) Stage*){0,MAX_VEC_HOPS} Leaf` with
single-property expressions, executed column-at-a-time: bulk expansion through `Graph::node_props_json_table` and group-by-code aggregation through
`Graph::node_prop_group_codes`. A multi-hop chain qualifies only when every hop carries a distinct relationship type, which makes relationship uniqueness
vacuous; a repeated type, or a chain longer than `MAX_VEC_HOPS`, falls back. The recognizer sees through a `Distinct` because the caller deduplicates.

A non-distinct `count` over the terminal variable that feeds no group key collapses the final hop (`execute_collapsed_count`), counting each source's
qualifying neighbors through `Graph::typed_neighbor_counts` so the last hop costs no triple per traversed edge and no hash lookup per edge. A terminal
filter that is a label test goes into the spec directly; a terminal property comparison is resolved into a `neighbor_allow` set by running those exact
stages over the label's whole node set (`resolve_terminal_allow`). That resolution is gated on the sources' `adjacency_span` reaching half the label
count, so a selective hop over a large label keeps the expansion fallback rather than paying for a full label pass, and it is speculative: it evaluates
predicates over a superset of the real neighbors, so a stage that errors there declines to the fallback rather than raising.

Two shapes route to the fallback whatever else holds. A multi-type hop does, because `Expand::rel_type` carries the raw pattern text (`"F|G"`) while
the kernel resolves one registered type. And a stale snapshot with at most `STALE_POINT_EXPAND_MAX` sources does
(`Graph::prefers_point_expansion`), because the kernel would rebuild the whole snapshot where the fallback serves those sources from per-source
adjacency.

**Group-key identity invariant** (binds both executors): grouping by a bare node or edge variable (`Expr::Prop(var, "")`) keys on the element id, not
its materialized property bag, and the group row keeps the `Node` or `Edge` binding rather than a materialized `Scalar`. The row pipeline's
`aggregate_all` fold and the vectorized aggregate both depend on this. Serializing a whole node to a JSON object per input row to build the group key
is an O(rows x properties) cliff (it regressed RE24 by roughly 5x), and re-materializing the entity as a `Scalar` forces downstream property reads off
the columnar fast path. Do not reduce either fold back to `evaluate_expr(...).to_string()` for a node or edge group key.

## Row-pipeline-only Switch

`exec_mode.rs` holds `ISSUNDB_ROW_PIPELINE_ONLY` and the `RowPipelineOnly` guard, which take the columnar executor, the counting kernels, the fused
`ExpandIntersect` hop, the metadata count shortcut, and the type-inference pruning pass out of the answer so the general row pipeline answers the query.
Pruning is in that set because it is the one pass that drops rows rather than reorganizing them, so leaving it out made a wrong `schema_has_edge`
negative invisible to every differential comparison. That makes the row pipeline
usable as a differential oracle: the four ways of answering a query each reproduce MATCH semantics independently, and nothing in the type system makes
them agree. Read the module doc before adding to it, and note the two test shapes that must pin the setting with `fast_paths_required` rather than
inherit it, or a sweep silently defeats them. The corpus lives in `exec/differential.rs`.

## Executor Mutation Safety

CREATE, SET, DELETE, and MERGE all mutate the graph:

- A single statement's write clauses, and the `RETURN`/`WITH` projection that follows them, share one `Graph::update` transaction, so an error anywhere
  rolls back every write the statement already made. Do not open a second transaction inside a statement.
- Mutate through the `WriteTxn` methods on the open transaction (`txn.add_node`, `txn.add_edge`, `txn.update_node`, `txn.delete_node`,
  `txn.delete_edge`), not through the auto-committing `Graph` methods of the same name: those open their own transaction and would deadlock against the
  one `Graph::update` already holds. A debug assertion catches that mistake at the call site. Never call `Storage` from the `exec` module.
- Do not rebuild the CSR snapshot by hand. `Graph::update` publishes the write to the caches' freshness counters at commit, and each consumer's gate
  rebuilds what it needs on demand (see the freshness gates in the root `AGENTS.md`).
- A `MATCH` or scan *after* a write clause in the same statement does not see that write's structural effect, because it reads the committed-only label
  index and CSR snapshot rather than the open transaction. A `RETURN`/`WITH` reading a property of a variable the statement just wrote does see it,
  through the pending-writes overlay in `exec/expr.rs`.

## Statement Clock

The current-time functions (`date()`, `localtime()`, `time()`, `localdatetime()`, `datetime()`, and their `.transaction`, `.statement`, and
`.realtime` variants) read a single wall-clock instant captured once per query, so two calls within one statement observe the same time and their
difference is exactly zero. The instant lives in a thread-local `Cell` (`STATEMENT_NOW` in `src/exec/expr.rs`), installed by a `StatementClock` guard
at the top of `execute` and restored when the guard drops, so a nested execution keeps its own instant.

This thread-local is a deliberate, scoped exception to the root rule against module-level runtime globals, and to the AST immutability rule against
`Cell`: it is not engine state (it never touches `Graph` or `Storage`), it is set and cleared within one `execute` call, and the `Cell` is a free
function's clock, not interior mutability on an AST or plan node. Do not widen its use beyond the statement clock.

## Conformance Test Gating

Cypher conformance tests live in `crates/issundb/tests/conformance/` and are gated on the `ISSUNDB_CONFORMANCE=1` environment variable:

```rust
#[test]
fn scenario_name() {
    if std::env::var("ISSUNDB_CONFORMANCE").is_err() {
        return;
    }
    // ... TCK scenario body ...
}
```

Run them with `make test-conformance`. Do not add new conformance scenarios as inline unit tests inside `issundb-cypher`; always place them in the
conformance directory so they stay out of the default `make test` run.
