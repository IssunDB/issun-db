## Comparison with LadybugDB

This directory contains a benchmark harness for comparing the performance of LadybugDB and IssunDB databases.
The harness runs an identical Cypher workload against IssunDB and LadybugDB (the Kùzu successor; via the `lbug` crate).
It reports the median wall time and and also checks that the results are identical.

### Running the Harness

```bash
make bench-ladybugdb
# Or directly (from this directory, so the local toolchain pin applies):
cd benchmarks/ladybugdb-compare && cargo run --release
```

The runs can be configured with these environment variables:

- `LADYBUGDB_COMPARE_NODES`: Person node count (default: 10000)
- `LADYBUGDB_COMPARE_EDGES`: KNOWS edge count (default: 50000)
- `LADYBUGDB_COMPARE_REPS`: timed repetitions per query, median reported (default: 10)
- `LADYBUGDB_COMPARE_WARMUPS`: untimed warmup runs per query (default: 3)
- `LADYBUGDB_COMPARE_SKEW`: `uniform` (default) or `zipf` for a power-law degree distribution with hub nodes; the skewed graph contains far more
  two-paths and triangles, so join-heavy queries get a lot slower
- `LADYBUGDB_COMPARE_SWEEP`: set to `1` to run the workload at base/5, base, and base*5 sizes and print per-query scaling ratios between consecutive
  sizes; ratios above the 5x dataset growth indicate superlinear behavior
- `LADYBUGDB_COMPARE_BUDGET_SECS`: time budget per query configuration (default: 30s); repetitions stop early when the budget is spent, and
  a trailing `*` in the table shows the median taken from fewer than the requested repetitions

### Data and Workload

The graph used in the benchmarks is a social network (Person nodes with id, name, age, and city; distinct KNOWS edges, no self-loops).
It is synthetically generated with a fixed random seed so runs are reproducible.
Edge endpoints are sampled uniformly by default or from a Zipf distribution (with exponent 0.8) with `LADYBUGDB_COMPARE_SKEW=zipf`,
which produces hub nodes as in real social graphs and help stress-test the joins (with sckewed degree distributions).

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

> [!NOTE]
> Currently, the benchmarks only include read-only queries, which are more directly comparable across the two databses.
> Things like mutation throughput, concurrent clients, and direct graph-algorithm APIs are deliberately excluded.
> They need separate setup and transaction semantics that would make it hard to maintain a clean comparison in a single harness.

To make the measurements more comparable, LadybugDB query runs are measured twice per query.
Once using LadybugDB's default thread count and once with the number of thread pinned to one, since IssunDB currently executes
a query in a single thread.

### Fairness Notes

- Loading data differ structurally for the two databases. LadybugDB bulk-loads via `COPY FROM` CSV; IssunDB inserts per record through `add_node` and
  `add_edge`. Both are timed and reported, but they measure different ingestion models.
- LadybugDB defaults to WALK semantics for variable-length patterns (a relationship may repeat within a path); the harness pins
  `recursive_pattern_semantic = 'TRAIL'` so both databases use the openCypher path semantics on identical query strings.
- `rebuild_csr` runs once after the IssunDB load so queries start from a fresh snapshot, matching steady-state operation.
