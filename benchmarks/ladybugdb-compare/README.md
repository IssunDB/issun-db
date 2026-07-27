## Comparison with LadybugDB

This directory contains a harness for comparing LadybugDB and IssunDB (the Kùzu successor; via the `lbug` crate).
It does two separate jobs: a differential correctness pass over row-returning queries, and a timing comparison.
LadybugDB is a genuinely independent implementation, which makes it the one oracle that can catch a mistake IssunDB makes consistently across all of
its own execution paths.
The in-tree `ISSUNDB_ROW_PIPELINE_ONLY` sweep cannot: it compares IssunDB's fast paths against IssunDB's own row pipeline, so a shared
misunderstanding of Cypher looks like agreement.

### Running the Harness

```bash
make test-ladybugdb       # differential correctness only, no timing
make test-ladybugdb-zipf  # the same under a skewed degree distribution
make bench-ladybugdb      # timing comparison (runs the curated differential pass first)
# Or directly (from this directory, so the local toolchain pin applies):
cd benchmarks/ladybugdb-compare && cargo run --release
```

`make test-ladybugdb` runs the differential passes twice, once normally and once with `ISSUNDB_ROW_PIPELINE_ONLY=1`, so both IssunDB's fast paths and its
general row pipeline are checked. Size and breadth come from `LADYBUGDB_DIFF_NODES` and `LADYBUGDB_DIFF_GENERATED` in the root Makefile; raise the latter
for a longer hunt.

The runs can be configured with these environment variables:

- `LADYBUGDB_COMPARE_NODES`: Person node count (default: 50000)
- `LADYBUGDB_COMPARE_EDGES`: KNOWS edge count (default: five per node, so the density stays fixed when only the node count is overridden). Setting
  this without also setting the node count changes the average degree, which moves the comparison more than the dataset size does
- `LADYBUGDB_COMPARE_REPS`: timed repetitions per query, median reported (default: 10)
- `LADYBUGDB_COMPARE_WARMUPS`: untimed warmup runs per query (default: 3)
- `LADYBUGDB_COMPARE_SKEW`: `uniform` (default) or `zipf` for a power-law degree distribution with hub nodes; the skewed graph contains far more
  two-paths and triangles, so join-heavy queries get a lot slower
- `LADYBUGDB_COMPARE_SWEEP`: set to `1` to run the workload at base/5, base, and base*5 sizes and print per-query scaling ratios between consecutive
  sizes; ratios above the 5x dataset growth indicate superlinear behavior
- `LADYBUGDB_COMPARE_BUDGET_SECS`: time budget per query configuration (default: 30s); repetitions stop early when the budget is spent, and
  a trailing `*` in the table shows the median taken from fewer than the requested repetitions
- `LADYBUGDB_COMPARE_DIFF_ONLY`: set to `1` to run the differential passes and skip timing entirely
- `LADYBUGDB_COMPARE_GENERATED`: generated differential queries per dataset size (default: 0, so a timing run keeps its length)
- `LADYBUGDB_COMPARE_SEED`: seed for the generator, printed with any finding so it replays

### Differential Pass

Before anything is timed, a corpus of row-returning queries runs on both databases and their sorted row sets must match exactly.
Any mismatch fails the run and prints the first differing row.

This corpus is deliberately separate from the timed workload. That workload is shaped for measurement, so most of its queries return a single
`count(...)`, and a scalar count is a weak differential signal: it cannot see wrong row content, wrong column names, wrong row multiplicity, or two
errors that cancel. The differential corpus returns the rows themselves, and covers projections over a bounded slice of the label scan, range and
string predicates, disjunction and negation, one hop in both directions, two fixed hops with and without `DISTINCT`, expand-into,
and grouped aggregation (which emits one row per group, so a wrong group key or per-group tally is visible where a single total would hide it).

Two invariants keep it cheap to extend, and a unit test pins both:

- No pattern can bind one edge to two relationship slots, which means at most two hops, both in the same direction, and no closing hop. That is
  narrower than "fixed-length" and the difference matters: relationship uniqueness applies to any pattern with two or more slots, so
  `(a)-[:KNOWS]->(b)<-[:KNOWS]-(c)` diverges when `c` is `a`, and `(a)<-[:KNOWS]-(b)-[:KNOWS]->(a)` always does. The pinned LadybugDB build permits
  that reuse where openCypher forbids it, and it does not honor the `TRAIL` setting. Two same-direction hops are safe only because the generator emits
  no self-loops. Within this rule no adjudication is needed, so any divergence is a real defect in one of the two databases; shapes outside it belong in
  the generated corpus below, where a reference evaluator adjudicates them.
- Row sets stay small and skew-independent, either anchored at a non-hub probe or bounded by an `id` predicate, so the pass costs the same at every
  size in a sweep. It runs at every size on purpose, because the engine switches internal strategies at size thresholds and the same corpus at 10k and
  250k nodes is therefore not the same test.

Projections avoid floats and nulls, which is a display-form difference rather than a semantic one. The float case is measured: a whole-valued weight
comes back as `0.0` from IssunDB and `0` from LadybugDB, while fractional weights agree. Reconciling numeric display in `issundb_rows` and
`ladybugdb_rows` would let floats join the corpus.

Running the harness under `ISSUNDB_ROW_PIPELINE_ONLY=1` compares LadybugDB against IssunDB's row pipeline instead of its fast paths, which composes
the two oracles: agreement in both configurations means the fast paths and the general path both match an independent implementation.

### Generated Queries

`LADYBUGDB_COMPARE_GENERATED=n` adds `n` generated queries per dataset size, drawn from a small grammar: fixed-length `:KNOWS` hop chains in either
direction with an optional closing hop, `WHERE` over id, age, and city, and either a property projection or one grouped aggregate. Generation is seeded
and the seed is printed with every finding, so a report replays exactly. Multi-source anchors are bounded by measuring real out-degrees rather than by
guessing, because the skewed generator concentrates edges on low ids: under Zipf a plain `id < 100` selects precisely the hubs.

These are adjudicated by a third oracle rather than by comparing the two databases against each other. `reference_rows` evaluates each generated query
directly over the dataset by brute force under openCypher semantics, so every divergence names the database at fault:

- both databases match the reference: agreement,
- IssunDB matches and LadybugDB does not: a LadybugDB walk-semantics divergence, counted and not a failure,
- IssunDB does not match: an IssunDB defect, which fails the run and is reported at its smallest reproducing shape,
- the reference disagrees with both databases while they agree with each other: the reference is the suspect, since two independent implementations
  agreeing is stronger evidence than one brute-force evaluator, and the run fails as a harness defect.

The adjudicator is what makes the generated corpus usable at all. Without it roughly one generated query in ten diverges, because any pattern that can
bind one edge to two relationship slots hits the walk-against-trail difference, and each would need a human to attribute. Findings are shrunk before
being reported: pieces are dropped while the verdict holds, which turns a three-hop query carrying four predicates into the smallest shape that still
reproduces.

### Data and Workload

The graph used in the benchmarks is a social network (Person nodes with id, name, age, and city; distinct KNOWS edges, no self-loops).
It is synthetically generated with a fixed random seed so runs are reproducible.
Edge endpoints are sampled uniformly by default or from a Zipf distribution (with exponent 0.8) with `LADYBUGDB_COMPARE_SKEW=zipf`,
which produces hub nodes as in real social graphs and helps stress-test the joins under a skewed degree distribution.

Currently, these queries are used in the benchmarks:

- Node and relationship counts
- Point lookup by indexed property (IssunDB property index versus LadybugDB primary key)
- Property range filtering
- One-, two-, three-, and four-hop typed expansion from a fixed seed
- Combined one-or-two-hop neighborhood counting with duplicate-node elimination
- Two-hop typed expansion from node 0, the hottest node under Zipf skew
- Selective property filtering after a one-hop expansion
- Two-hop expansion with both source and destination fixed
- Variable-length expansion (`*2..3`) from a fixed seed
- `ORDER BY ... LIMIT` over node properties
- `DISTINCT ... LIMIT` over duplicate-heavy traversal results
- Full-scan projection of three node properties per row
- Cyclic triangle count
- Aggregation over a one-hop traversal grouped by city

The differential corpus is listed in `differential_workload`; the timed one in `workload`.

> [!NOTE]
> Currently, the benchmarks only include read-only queries, which are more directly comparable across the two databases.
> Things like mutation throughput, concurrent clients, and direct graph-algorithm APIs are deliberately excluded.
> They need separate setup and transaction semantics that would make it hard to maintain a clean comparison in a single harness.

To make the measurements more comparable, LadybugDB query runs are measured twice per query.
Once using LadybugDB's default thread count and once with the number of threads pinned to one, since IssunDB currently executes a query in a single
thread.

### Fairness Notes

- Loading data differs structurally for the two databases. LadybugDB bulk-loads via `COPY FROM` CSV; IssunDB inserts per record through `add_node` and
  `add_edge`. Both are timed and reported, but they measure different ingestion models.
- LadybugDB defaults to WALK semantics for variable-length patterns (a relationship may repeat within a path); the harness pins
  `recursive_pattern_semantic = 'TRAIL'` so both databases use the openCypher path semantics on identical query strings.
- `rebuild_csr` runs once after the IssunDB load so queries start from a fresh snapshot, matching steady-state operation.
