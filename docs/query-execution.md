# Query Execution

IssunDB executes openCypher queries through a five-stage pipeline, all
implemented in `crates/issundb-cypher/`.

## Pipeline Overview

```
Query string
     │
     ▼  crates/issundb-cypher/src/parser.rs
   AST  (QueryPart + Expr + Pattern nodes)
     │
     ▼  crates/issundb-cypher/src/plan/logical.rs
 Logical plan  (LogicalOperator tree)
     │
     ▼  crates/issundb-cypher/src/plan/optimize.rs
 Optimized logical plan  (predicate pushdown, index selection)
     │
     ▼  crates/issundb-cypher/src/plan/physical.rs
 Physical plan  (PhysicalOperator tree)
     │
     ▼  crates/issundb-cypher/src/exec.rs
 Result  (QueryResult with columns and records)
```

## Stage 1: Parsing (`parser.rs`)

A hand-written recursive-descent parser. No external parser generator is used.
Expression precedence is enforced by the call chain:

```
parse_expr
  └─ parse_expr_or
       └─ parse_expr_and
            └─ parse_expr_cmp      (=, <>, <, >, <=, >=, IS NULL, IS NOT NULL)
                 └─ parse_expr_isnull
                      └─ parse_expr_unary   (NOT, unary minus)
                           └─ parse_expr_call   (function calls)
                                └─ parse_expr_atom  (literals, variables, parentheses)
```

Supported clauses: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `WITH`, `RETURN`,
`CREATE`, `MERGE`, `SET`, `DELETE`, `DETACH DELETE`, `UNWIND`, `ORDER BY`,
`SKIP`, `LIMIT`.

## Stage 2: Logical Planning (`plan/logical.rs`)

The planner converts each `QueryPart` into a `LogicalOperator` tree bottom-up.
Pattern nodes are expanded into `LabelScan` + `Expand` operators. `WHERE`
predicates become `Filter` operators. `RETURN` / `WITH` projections become
`Project` operators.

`WhereClause` variants:
- `Simple(FilterExpr)`: a single typed predicate (equality, range, label check).
- `And(Vec<FilterExpr>)` / `Or(Vec<FilterExpr>)`: compound predicates.
- `Expr(Expr)`: arbitrary boolean expression for IS NULL, quantifiers, etc.

## Stage 3: Optimization (`plan/optimize.rs`)

Current rewrites:
- **Predicate pushdown**: moves `Filter` operators as close to their `LabelScan`
  source as possible.
- **NodeIndexScan promotion**: replaces a `LabelScan + Filter(prop = value)` pair
  with a `NodeIndexScan` when a property index exists.
- **Dead projection elimination**: removes projections whose output columns are
  not referenced downstream.

## Stage 4: Physical Planning (`plan/physical.rs`)

A direct structural translation of `LogicalOperator` to `PhysicalOperator`.
The only non-trivial choice is `LogicalOperator::Join` → `PhysicalOperator::HashJoin`.

## Stage 5: Execution (`exec.rs`)

The executor is a pull-based interpreter: each `PhysicalOperator` variant
produces rows on demand. Key behaviors:

- **`LabelScan`**: calls `Graph::nodes_by_label` then fetches each node record.
- **`Expand`**: calls `Graph::out_neighbors` or `Graph::in_neighbors` per source
  row; for variable-length paths uses iterative BFS/DFS respecting hop bounds.
- **`HashJoin`**: materializes the right child into a `HashMap`, then probes
  with each left row (cross-product when no join key is shared).
- **`Aggregate`**: accumulates groups in a `HashMap<Vec<Value>, AccState>`.
- **Mutations** (`CREATE`, `SET`, `DELETE`): acquire `Graph::with_write_lock`
  for the duration of the mutation block; never interleave read and write
  transactions.

## EXPLAIN

`Graph::explain(cypher)` parses, plans, and optimizes a query and returns the
physical plan as an indented tree string without executing it. Useful for
understanding index selection and join ordering.

```
Project [n.name AS name, r.since AS since]
  Expand n-[r:KNOWS]->m
    LabelScan n:Person
```
