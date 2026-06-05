## LadybugDB Comparison Harness

Runs an identical Cypher workload against IssunDB and LadybugDB (the Kùzu successor, via the `lbug` crate), reporting median wall time per engine and
asserting row-set equality, so every timing run doubles as a differential correctness check.

This crate is deliberately detached from the root workspace: `lbug` links the LadybugDB C++ library (a precompiled static archive by default, a CMake
source build as a fallback), and that dependency must never become part of `make build` or `make test`.

### Running

```bash
make bench-ladybug
# or directly (from this directory, so the local toolchain pin applies):
cd benchmarks/ladybug-compare && cargo run --release
```

The crate carries its own `rust-toolchain.toml`: `lbug`'s build dependencies need Rust 1.88 or newer, while the repo root pins the workspace MSRV (
1.85.0).

Knobs, all environment variables:

- `LADYBUG_COMPARE_NODES`: Person node count (default 10000)
- `LADYBUG_COMPARE_EDGES`: KNOWS edge count (default 50000)
- `LADYBUG_COMPARE_REPS`: timed repetitions per query, median reported (default 10)
- `LADYBUG_COMPARE_WARMUPS`: untimed warmup runs per query (default 3)

### Workload

The synthetic graph is a deterministic LCG-generated social network (Person nodes with id, name, age, and city; distinct KNOWS edges, no self-loops),
so runs are reproducible and both engines always see the same data.

Current queries, each sent verbatim to both engines:

- Point lookup by indexed property (IssunDB property index versus LadybugDB primary key)
- Two-hop typed expansion from a fixed seed with an aggregate
- Cyclic triangle count (exercises the IssunDB MultiwayJoin closing hop)
- Aggregation over a one-hop traversal grouped by city

LadybugDB is measured twice per query: at its default thread count and pinned to one thread, since IssunDB executes a query single-threaded.

### Fairness Notes

- Load paths differ structurally: LadybugDB bulk-loads via `COPY FROM` CSV; IssunDB inserts per record through `add_node` and `add_edge`. Both are
  timed and reported, but they measure different ingestion models.
- The differential check compares normalized string rows; the workload avoids float projections so no formatting reconciliation is needed.
- `rebuild_csr` runs once after the IssunDB load so queries start from a fresh snapshot, matching steady-state operation.
