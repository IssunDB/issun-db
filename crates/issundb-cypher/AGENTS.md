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
   tree. Current rewrites include predicate push-down and filter reordering.
   The optimizer takes ownership of the physical tree and returns a new tree;
   it never mutates in place.
5. **Execute** (`src/exec.rs`): `execute` drives the physical tree against a
   `Graph` reference, producing a `QueryResult`.

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

## Adding a New Cypher Clause

Follow this checklist in order:

1. **AST variant**: add the new clause type to `src/ast.rs`. Derive `Clone`
   and `PartialEq`.
2. **Parser rule**: add a parsing function in `src/parser.rs`. Wire it into
   the dispatch table at the top of `parse`.
3. **Logical planner arm**: add a match arm in `LogicalPlanner::plan_statement`
   or `plan_query` in `src/plan/logical.rs`.
4. **Physical planner arm**: add a match arm in `PhysicalPlanner::plan` in
   `src/plan/physical.rs`.
5. **Optimizer arm** (if applicable): if the new clause benefits from
   rewriting, add a rewrite rule in `src/plan/optimize.rs`. Skip this step if
   no rewrite applies; do not add a pass-through arm unless the optimizer
   explicitly needs to descend into the clause.
6. **Executor arm**: add a match arm in the physical operator dispatch loop
   in `src/exec.rs`.
7. **Conformance test**: add at least one TCK scenario in
   `crates/issundb/tests/conformance/` gated on `ISSUNDB_CONFORMANCE=1`.

All seven steps are required for the change to be considered complete.

## `FilterExpr` vs. `Expr`

- `Expr` (defined in `src/ast.rs`) is the general expression type produced by
  the parser. It covers literals, property accesses, function calls,
  arithmetic, comparisons, and boolean combinators.
- `FilterExpr` (defined in `src/plan/logical.rs`) is the typed predicate
  representation used inside `LogicalOperator::Filter` and
  `PhysicalOperator::Filter`. It has explicit variants for common binary
  comparisons (`Eq`, `Ne`, `Lt`, `Gt`, `Le`, `Ge`), a `HasLabel` variant for
  label checks, and an `Expr` catch-all for predicates that do not fit a named
  variant (IS NULL, quantifiers, compound boolean expressions).
- Conversion from `Expr` to `FilterExpr` happens in `LogicalPlanner`. Do not
  perform this conversion in the parser or executor.

## `WhereClause` Variants

`WhereClause` (in `src/ast.rs`) covers the three forms that appear in Cypher
WHERE positions:

- `Simple(FilterExpr)`: a single predicate, e.g. `WHERE n.age > 30`.
- `And(Vec<FilterExpr>)` / `Or(Vec<FilterExpr>)`: compound predicates joined
  with AND or OR.
- `Expr(Expr)`: an arbitrary expression used for IS NULL checks, quantifier
  expressions (`ANY`, `ALL`, `NONE`, `SINGLE`), and nested boolean
  sub-expressions that do not reduce to a `FilterExpr` variant.

When lowering a `WhereClause` to a plan node, prefer `FilterExpr` variants for
simple comparisons so the optimizer can inspect and reorder them. Fall back to
`FilterExpr::Expr` only when no named variant applies.

## Executor Mutation Safety

CREATE, SET, DELETE, and MERGE all mutate the graph:

- Acquire the graph write lock before opening a `RwTxn` and hold it for the
  entire duration of the mutation. Do not interleave reads and writes in the
  same transaction.
- All graph mutations go through `Graph` public methods (`add_node`,
  `add_edge`, `update_node`, `delete_node`, `delete_edge`). Do not call
  `Storage` directly from `exec.rs`.
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
