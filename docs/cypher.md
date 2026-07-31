# Cypher Support

IssunDB implements a substantial subset of openCypher, executed through the `query` interfaces on every binding (Rust, Python, CLI, REST, and MCP).
This page lists the supported clauses, patterns, expressions, and functions, followed by the constructs that are not supported and the known behavioral deviations.
Conformance is tracked against a subset of the openCypher TCK (Technology Compatibility Kit).

## Clauses

| Clause | Notes |
|--------|-------|
| `MATCH` | Comma-separated patterns; each `MATCH` may carry a `WHERE`. |
| `OPTIONAL MATCH` | Unmatched rows are null-filled. |
| `WHERE` | Full boolean expressions on `MATCH`, `OPTIONAL MATCH`, and `WITH`. |
| `RETURN` | Supports `DISTINCT`, `*`, and `AS` aliases; an unaliased item's column name is its source text. |
| `WITH` | Supports `DISTINCT`, `*`, `WHERE`, `ORDER BY`, `SKIP`, and `LIMIT`. |
| `UNWIND` | `UNWIND expr AS var`. |
| `CREATE` | Comma-separated patterns; a created relationship must be directed and carry exactly one type. |
| `MERGE` | Supports `ON CREATE SET` and `ON MATCH SET`; relationship uniqueness applies to the match. |
| `SET` | Property assignment (`SET n.prop = expr`) and label assignment (`SET n:Label1:Label2`). Setting a property to null removes it. |
| `REMOVE` | Removes properties (`REMOVE n.prop`) and labels (`REMOVE n:Label`). |
| `DELETE` / `DETACH DELETE` | Targets are arbitrary expressions evaluating to nodes, relationships, or lists of them; deletion applies after the whole result is computed. |
| `ORDER BY` | One or more sort keys, each `ASC` or `DESC`. |
| `SKIP` / `LIMIT` | Accept literals, parameters, and constant expressions (any expression that does not depend on variables). |
| `UNION` / `UNION ALL` | Combines queries with matching column names. |
| `FOREACH` | Applies a body of write clauses to each element of a list expression. |
| `CALL ... YIELD` | Invokes built-in `issundb.*` procedures or a custom procedure registry; supports `YIELD field AS alias`, `YIELD *`, and the no-parentheses form that reads arguments from query parameters. |

Queries compose as pipelines: a sequence of clauses such as `MATCH ... WITH ... UNWIND ... CREATE ... RETURN ...` executes in order, and standalone write statements may follow one another in a single query string.

## Patterns

- Node patterns support a variable, multiple labels, and an inline property map: `(n:Person:Admin {name: 'Alice'})`.
- Relationship patterns support a variable, type alternation (`[:KNOWS|LIKES]`), and an inline property map.
- Directions: outgoing `-[..]->`, incoming `<-[..]-`, and undirected `-[..]-`. The both-arrows form `<-[..]->` matches either direction, exactly like an undirected pattern; `CREATE` and `MERGE` reject it because a created relationship must be directed. Bare arrows (`-->`, `<--`, `--`) also parse.
- Variable-length relationships support every range form: `*`, `*n`, `*n..m`, `*n..`, `*..m`, and `*..`. A variable on a variable-length relationship always binds a list of relationships, including for `*1`.
- Relationship uniqueness follows openCypher: within one pattern match, a node may repeat but a relationship may not.
- Named paths (`p = (a)-[:KNOWS]->(b)`) bind path values; `nodes(p)`, `relationships(p)` (alias `rels(p)`), and `length(p)` operate on them.
- Because a variable-length relationship variable is a list rather than a path, its hop count is `size(r)`. `length()` applies to a named path, so
  `length(r)` on a relationship list is a type error.

## Expressions

- Literals: integers (decimal, hex, and octal), floats, single- or double-quoted strings, booleans, `null`, lists, and maps. String escapes cover `\'`, `\"`, `\\`, `\n`, `\t`, `\r`, `\b`, `\f`, and `\uXXXX`.
- Parameters (`$name`) are accepted anywhere an expression is, including `SKIP`, `LIMIT`, and inline property maps.
- Property access (`n.prop`), subscripting (`expr[key]`, `expr[i]`), and list slicing (`expr[a..b]`, `expr[a..]`, `expr[..b]`).
- Arithmetic: `+`, `-`, `*`, `/`, `%`, `^`, and unary minus. Integer overflow raises an error rather than silently widening to float.
- String predicates: `STARTS WITH`, `ENDS WITH`, `CONTAINS`, and regular expression matching with `=~`, each with a `NOT` form.
- Comparisons: `=`, `<>`, `<`, `>`, `<=`, `>=`, including chained comparisons (`1 < x < 10`).
- `IS NULL` and `IS NOT NULL`, three-valued `AND`, `OR`, `XOR`, and `NOT`, and membership with `IN`. When one `AND`/`OR` operand alone determines
  the result (`false AND x`, `true OR x`), a runtime evaluation error in the other operand is suppressed, so a guard clause such as
  `x <> 0 AND y / x > 1` protects against a division error. A successfully evaluated non-boolean operand still raises a type error even on the
  determined side, as openCypher requires (`false AND 123` raises; `false AND (1 / 0)` is `false`).
- `CASE` in both the simple (`CASE x WHEN ...`) and searched (`CASE WHEN ...`) forms, with an optional `ELSE`.
- List comprehensions (`[x IN list WHERE pred | expr]`), pattern comprehensions (`[p = (a)-[r]->(b) WHERE pred | expr]`), the quantifiers `all`, `any`, `none`, and `single`, and `reduce(acc = init, x IN list | expr)`.
- Label predicates in expression position: `n:Label` and `n:A:B` evaluate to booleans.

## Functions

Function names are matched case-insensitively.

- Graph and scalar: `id`, `type`, `labels`, `properties`, `keys`, `startNode`, `endNode`, `nodes`, `relationships` (alias `rels`), `length`, `size`, `coalesce`, `exists`, `timestamp`.
- String: `substring`, `left`, `right`, `trim`, `lTrim`, `rTrim`, `toUpper`, `toLower`, `replace`, `split`, `reverse`.
- List: `head`, `last`, `tail`, `range`, `size`, `reverse`.
- Numeric: `abs`, `sqrt`, `floor`, `ceil` (alias `ceiling`), `round`, `sign`, `log`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `degrees`, `radians`, `haversin`, `pi`, `e`, `rand`, and the multi-argument scalar forms of `min` and `max`.
- Conversion: `toString`, `toInteger` (alias `toInt`), `toFloat`, `toBoolean`. Converting a non-numeric string with `toInteger` or `toFloat` yields null.
- Vector and set similarity: `vector_dist`, `issundb.distance.cosine`, `issundb.distance.euclidean`, `issundb.similarity.jaccard`, and `issundb.similarity.overlap`; see the [Cypher Functions](api-reference.md#cypher-functions) reference.
- Aggregates: `count(*)`, `count`, `sum`, `avg`, `min`, `max`, `collect`, `stdev` (sample), `stdevP` (population), `percentileDisc`, and `percentileCont`. `DISTINCT` is supported inside `count`, `sum`, `avg`, `min`, `max`, `collect`, `stdev`, and `stdevP`.

## Temporal Values

The temporal constructors `date`, `time`, `localtime`, `datetime`, `localdatetime`, and `duration` are supported, along with `datetime.fromepoch`, `datetime.fromepochmillis`, `duration.between`, `duration.inDays`, `duration.inMonths`, `duration.inSeconds`, and per-type `truncate` functions. The clock forms `.transaction`, `.statement`, and `.realtime` all read the statement clock, so repeated calls inside one query observe the same instant; the zero-argument constructors return the current instant. Temporal values order correctly under `ORDER BY` and compare within their type family.

## Procedures and Schema Statements

- Built-in graph data science procedures run through `CALL issundb.<name>(...)`; the full list is in the [Cypher Built-in Procedures](api-reference.md#cypher-built-in-procedures) reference, and custom procedures can be registered through `query_with_procedures`.
- Index and constraint DDL (`CREATE INDEX`, `DROP INDEX`, `CREATE CONSTRAINT`, and `DROP CONSTRAINT`) is covered by the [Cypher DDL Reference](api-reference.md#cypher-ddl-reference).
- Bulk data administration statements are also available: `COPY <Label> FROM '<file>' [WITH ...]` bulk-imports nodes or relationships from a file, `EXPORT DATABASE '<path>' [WITH ...]` writes the database contents out, and `IMPORT DATABASE '<path>'` loads a previous export. A file classifies as a relationship import when its rows carry the `_from` and `_to` endpoint keys; rows with bare `from` and `to` keys and no node metadata key (`_id` or `_labels`) are rejected with a migration hint rather than silently imported as nodes. Both `COPY` and `IMPORT DATABASE` return one row per imported file with the columns `target`, `kind` (`nodes` or `relationships`), and `count`.

## Unsupported Constructs

- `SET n = {map}` and `SET n += {map}`: assign properties individually with `SET n.prop = value`.
- `CALL { ... }` subqueries and `EXISTS { ... }` subqueries: `CALL` is procedure invocation only, and `exists()` is a scalar null check.
- `shortestPath(...)` and `allShortestPaths(...)` pattern functions: use the `shortest_path` and `all_shortest_paths` methods on the `Graph` API instead.
- Pattern predicates in `WHERE` (`WHERE (a)-[:KNOWS]->(b)`, `WHERE NOT (a)-->(b)`). A pattern is matched, not tested, so express the positive case as
  an additional `MATCH` and the negative case as an anti-join: `OPTIONAL MATCH (a)-[r:KNOWS]->(b) WITH a, b, r WHERE r IS NULL`.
- Map projections (`n{.name, .age}`).
- `MANDATORY MATCH`.

## Known Deviations

- Integer division or modulo by zero raises an arithmetic error, matching Neo4j. Float division by zero follows IEEE 754: `0.0/0.0` is NaN and a
  nonzero numerator over a zero denominator is +/-Infinity. Neither has a JSON number representation, so both are returned as a tagged sentinel
  object (`{"__type__": "__NaN__"}`, `{"__type__": "__Infinity__"}`, or `{"__type__": "__-Infinity__"}`) rather than as a number or as `null`.
- `=~` matches the entire string, per the openCypher spec: a bare pattern with no `^`/`$` (for example `=~ 'chris'`) matches only the exact string
  `"chris"`, not any string that merely contains it.
- Temporal years are limited to roughly the range -262143 to 262143, narrower than the openCypher specification admits.
- The openCypher TCK subset run (`make test-conformance`) tracks the remaining known gaps; failures there are documented deviations rather than silent wrong results.
- A statement's write clauses (`CREATE`, `MERGE`, `SET`, `DELETE`, `REMOVE`, in any combination, plus the `RETURN`/`WITH` projection that follows
  them) are atomic: an error anywhere rolls back every write the statement already made. A `RETURN`/`WITH` clause correctly sees a property of a
  variable an earlier write in the same statement just created or changed. A `MATCH`/label-scan *after* a write clause in the same statement does not
  see that write's structural effect until the whole statement commits (for example `CREATE (a:Foo) WITH a MATCH (m:Foo) RETURN count(m)` does not
  count `a`).
