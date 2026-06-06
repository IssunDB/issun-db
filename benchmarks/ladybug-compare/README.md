## LadybugDB Comparison Harness

Runs an identical Cypher workload against IssunDB and LadybugDB (the Kùzu successor, via the `lbug` crate), reporting median wall time per engine and
asserting row-set equality, so every timing run doubles as a differential correctness check.

This crate is deliberately detached from the root workspace: `lbug` links the LadybugDB C++ library (a precompiled static archive by default, a CMake
source build as a fallback), and that dependency must never become part of `make build` or `make test`.

### Running

```bash
make bench-ladybug
# Or directly (from this directory, so the local toolchain pin applies):
cd benchmarks/ladybug-compare && cargo run --release
```

The crate carries its own `rust-toolchain.toml`: `lbug`'s build dependencies need Rust 1.88 or newer, while the repo root pins the workspace MSRV (
1.85.0).

Knobs, all environment variables:

- `LADYBUG_COMPARE_NODES`: Person node count (default 10000)
- `LADYBUG_COMPARE_EDGES`: KNOWS edge count (default 50000)
- `LADYBUG_COMPARE_REPS`: timed repetitions per query, median reported (default 10)
- `LADYBUG_COMPARE_WARMUPS`: untimed warmup runs per query (default 3)
- `LADYBUG_COMPARE_SKEW`: `uniform` (default) or `zipf` for a power-law degree distribution with hub nodes; the skewed graph contains far more
  two-paths and triangles, so join-heavy queries get much slower on both engines
- `LADYBUG_COMPARE_SWEEP`: set to `1` to run the workload at base/5, base, and base*5 sizes and print per-query scaling ratios between consecutive
  sizes; ratios above the 5x dataset growth indicate superlinear behavior
- `LADYBUG_COMPARE_BUDGET_SECS`: time budget per query per engine configuration (default 30); repetitions stop early once the budget is spent, and
  a trailing `*` in the table marks a median taken from fewer than the requested reps

### Workload

The synthetic graph is a deterministic LCG-generated social network (Person nodes with id, name, age, and city; distinct KNOWS edges, no self-loops),
so runs are reproducible and both engines always see the same data. Edge endpoints are sampled uniformly by default or from a Zipf distribution
(exponent 0.8) with `LADYBUG_COMPARE_SKEW=zipf`, which produces hub nodes as in real social graphs and stresses skewed joins.

Current queries, each sent verbatim to both engines:

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
- Cyclic triangle count (exercises the IssunDB MultiwayJoin closing hop)
- Aggregation over a one-hop traversal grouped by city

Mutation throughput, concurrent clients, and direct graph-algorithm APIs are deliberately excluded.
They need separate setup and transaction semantics, and IssunDB does not expose shortest-path operators through the shared Cypher surface used by
this harness.

LadybugDB is measured twice per query: at its default thread count and pinned to one thread, since IssunDB executes a query single-threaded.

### Fairness Notes

- Load paths differ structurally: LadybugDB bulk-loads via `COPY FROM` CSV; IssunDB inserts per record through `add_node` and `add_edge`. Both are
  timed and reported, but they measure different ingestion models.
- The differential check compares normalized string rows; the workload avoids float projections so no formatting reconciliation is needed.
- LadybugDB defaults to WALK semantics for variable-length patterns (a relationship may repeat within a path); the harness pins
  `recursive_pattern_semantic = 'TRAIL'` so both engines use the openCypher path semantics on identical query strings.
- `rebuild_csr` runs once after the IssunDB load so queries start from a fresh snapshot, matching steady-state operation.
