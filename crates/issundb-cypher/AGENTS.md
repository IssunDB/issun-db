# issundb-cypher Agent Guide

This file covers crate-specific guidance for contributors working inside
`crates/issundb-cypher`. Read the root `AGENTS.md` first; the rules there
apply everywhere and are not repeated here.

## The Query Execution Pipeline

Every Cypher string passes through five stages, each owned by a distinct
source file:

1. **Parse** (`src/parser.rs`): recursive-descent parser produces a `Statement`
   AST from the raw query string.
2. **Logical plan** (`src/plan/logical.rs`): `LogicalPlanner` walks the AST
   and emits a tree of `LogicalOperator` nodes. All variable bindings and
   label/type resolutions are established here.
3. **Physical plan** (`src/plan/physical.rs`): `PhysicalPlanner` converts each
   `LogicalOperator` into a `PhysicalOperator`, choosing access paths (label
   scan, index seek, adjacency expansion).
4. **Optimize** (`src/plan/optimize.rs`): `Optimizer` rewrites the physical
   tree. Passes run in this order: extract filters, eliminate statically-true
   filters, reorder operators, select the cheapest scan node (reversing linear
   expand chains), push-down filters, optimize index scans and id seeks, rewrite
   closing expands into `MultiwayJoin`, then reduce counts over a bare labeled
   scan to a constant. The optimizer takes ownership of the physical tree and
   returns a new tree; it never mutates in place.
5. **Execute** (`src/exec/read.rs`): `execute_physical` drives the physical
   tree against a `Graph` reference, producing a `Vec<PathMap>`. The
   `Filter { input: Expand }` pattern uses a factorized fast path implemented
   in `execute_filter_over_expand`.

Keep each concern in its own file. Do not call `Graph` methods from
`parser.rs`, `logical.rs`, or `physical.rs`.

## Parser Recursion Rules

The parser is hand-written and must avoid left-recursion. Expression parsing
uses a descent chain ordered by precedence (lowest to highest):

```
parse_expr
  └─ parse_expr_or
       └─ parse_expr_and
            └─ parse_expr_cmp
                 └─ parse_expr_isnull
                      └─ parse_expr_unary
                           └─ parse_expr_call
                                └─ parse_expr_atom
```

Each level handles only its own operators and calls the next level for
sub-expressions. Never call a higher-precedence function from a
lower-precedence one except through this chain.

String tokenization uses a single forward pass without backtracking. If a
lookahead of more than one token is needed, use a local peek variable rather
than a global parser state.

## AST Immutability Policy

- All AST node types derive `Clone` and `PartialEq`. They are produced once by
  the parser and treated as read-only thereafter.
- The optimizer must not mutate existing AST or physical plan nodes in place.
  Produce new nodes from rewrite rules and replace subtrees by constructing
  new parent nodes.
- Do not add `Cell`, `RefCell`, or interior mutability to any AST or plan type.

## Optimizer Correctness Invariants

- Preserve row multiplicity. IssunDB is a multigraph: the LMDB `DUPSORT`
  adjacency stores one entry per edge, so two parallel edges between the same
  pair produce two distinct expansion results and two result rows. A rewrite
  that collapses one-row-per-edge into one-row-per-(src, dst), or that uses a
  boolean reachability product in place of per-edge traversal, will silently
  drop rows for parallel edges. Such rewrites (for example a `reduce_expand_into`
  that drops edge emission, or matrix-product chain fusion) are only valid when
  multiplicity is provably irrelevant; default to rejecting them.
- A rewrite must produce the same result set as the unoptimized plan for every
  graph, not just the common single-edge case. When in doubt, add an end-to-end
  test with parallel edges through the `issundb` facade.

## Adding a New Cypher Clause

Follow this checklist in order:

1. **AST variant**: add the new clause type to `src/ast.rs`. Derive `Clone` and `PartialEq`.
2. **Parser rule**: add a parsing function in `src/parser.rs`. Wire it into the dispatch table at the top of `parse`.
3. **Logical planner arm**: add a match arm in `LogicalPlanner::plan` in `src/plan/logical.rs`.
4. **Physical planner arm**: add a match arm in `PhysicalPlanner::plan` in `src/plan/physical.rs`.
5. **Optimizer arm** (if applicable): if the new clause benefits from rewriting, add a rewrite rule in `src/plan/optimize.rs`. Skip this step if no
   rewrite applies; do not add a pass-through arm unless the optimizer
   explicitly needs to descend into the clause.
6. **Executor arm**: add a match arm in the physical operator dispatch loop in `execute_physical` (`src/exec/read.rs`).
7. **Conformance test**: add at least one TCK scenario in `crates/issundb/tests/conformance/` gated on `ISSUNDB_CONFORMANCE=1`.

All seven steps are required for the change to be considered complete.

## `FilterExpr` vs. `Expr`

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

The executor (`exec/read.rs`) handles `MultiwayJoin` by:

1. Collecting unique `closing_src_var` node IDs from all input rows.
2. Bulk-expanding from those nodes once via `expand_multi_type`.
3. Building a `(src_node, dst_node) → EdgeId` hash map.
4. For each input row, doing an O(1) lookup to check the closing edge and bind `closing_rel_var`.

`MultiwayJoin` is optimizer-generated only: `PhysicalPlanner::plan` never emits it. Every match arm in `optimize.rs` that recurses into operator
children must handle the `MultiwayJoin` variant.

## Factorized Execution

`exec/factorize.rs` defines `FactorizedRecordGroup`: a shared `Arc<PathMap>` prefix (bindings from ancestor hops) plus a `Vec` of per-row extensions
`(rel_var, rel_binding, dst_var, dst_binding)` for the current hop. Using `Arc` avoids O(shared_vars) HashMap clone cost for every destination; only
the two new bindings are paid per output row.

`execute_filter_over_expand` (in `exec/read.rs`) handles the `Filter { input: Expand(single-hop, directed) }` pattern:

- **Factorized fast path**: when the filter expression does not reference `rel_var` or `dst_var`, it is evaluated once per source path. Sources that
  fail skip all their destinations, costing zero PathMap clones for rejected sources.
- **Per-row fallback**: when the filter touches the expansion variables, the full path is materialized before evaluation.
- **`HasLabel` filters**: always route through the existing bulk-GraphBLAS path; `execute_filter_over_expand` is not called for `HasLabel`
  expressions.

## Executor Mutation Safety

CREATE, SET, DELETE, and MERGE all mutate the graph:

- Acquire the graph write lock before opening a `RwTxn` and hold it for the
  entire duration of the mutation. Do not interleave reads and writes in the
  same transaction.
- All graph mutations go through `Graph` public methods (`add_node`,
  `add_edge`, `update_node`, `delete_node`, `delete_edge`). Do not call
  `Storage` directly from the `exec` module.
- After a mutation, the caller is responsible for deciding whether to rebuild
  the CSR snapshot. The executor does not rebuild it automatically.

## Conformance Test Gating

Cypher conformance tests live in `crates/issundb/tests/conformance/` and are
gated on the `ISSUNDB_CONFORMANCE=1` environment variable:

```rust
#[test]
fn scenario_name() {
    if std::env::var("ISSUNDB_CONFORMANCE").is_err() {
        return;
    }
    // ... TCK scenario body ...
}
```

Run them with `make test-conformance`. Do not add new conformance scenarios as
inline unit tests inside `issundb-cypher`; always place them in the
conformance directory so they stay out of the default `make test` run.
