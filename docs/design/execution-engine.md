# Design Note: Execution Engine Performance

Status: proposal, not yet implemented. This is a scoping pass for the larger performance work that the incremental optimizer rules do not address.

## Motivation

The optimizer-rule work (scan selection, count reduction, id seek, chain fusion) removes asymptotic waste:
queries that would scan a whole label now seek or read metadata.
The `query_optimizer` benchmarks confirm those wins (see [Query Optimizer](../query-optimizer.md)).

The same benchmarks expose a different, structural cost. A trivial query such as
`MATCH (n:Person) RETURN count(*)` resolves to a constant after `reduce_count`,
yet still takes ~34 µs end to end. A primary-key seek that touches one node
takes ~113 µs. Neither figure is dominated by storage access. Two overheads
account for it:

1. **Per-query compilation.** Every `query()` call re-parses the Cypher string,
   rebuilds the logical and physical plans, and reruns every optimizer pass.
2. **Per-row materialization.** Execution is a materialized tree-walk: each
   operator returns a `Vec<PathMap>`, where `PathMap = HashMap<String,
   GraphBinding>` and `GraphBinding::Scalar` wraps `serde_json::Value`. Every
   output row clones a string-keyed hash map; every scalar clone is a
   `serde_json::Value` deep clone.

These are the two levers for the next tier of performance work. They are
independent and can be pursued separately.

## Lever A: Per-Row Execution Cost

### Current state

- Row shape: `HashMap<String, GraphBinding>`. Variable lookup hashes a string;
  row clone copies the map and all string keys.
- Scalars: `serde_json::Value`, which deep-clones strings and arrays per row.

### Proposal

Adopt the representation used by mature engines (FalkorDB's runtime is one
reference):

1. **Numeric variable slots.** A binder pass resolves each variable name to a
   small integer once, at plan time. The runtime row becomes a flat
   `Vec<Binding>` indexed by slot. Variable access is an index, not a hash; row
   clone copies a small vector, not a string-keyed map.
2. **`Arc`-shared, thin values.** Replace `serde_json::Value` in the runtime
   with an internal `Value` enum whose large payloads (strings, lists, maps) are
   `Arc`-wrapped for O(1) clone, with temporals stored as flat integers.
   Conversion to `serde_json::Value` happens once, at the result boundary.

### Why it is large

`PathMap` and `GraphBinding` are threaded through `exec/read.rs`,
`exec/write.rs`, `exec/expr.rs`, and `exec/factorize.rs` (several thousand
lines). The change touches every operator and every expression evaluator. It is
a mechanical but wide refactor, and it is the reason this is a design note
rather than an incremental patch.

### Phasing

- **Phase A1:** Introduce the internal `Value` type and an `Arc`-shared string
  representation behind the existing `GraphBinding::Scalar`, converting at the
  result boundary. Keep `PathMap` string-keyed. Lower risk; isolates the value
  change from the row-shape change. Measure against `chain_fusion` and
  `scan_selection`.
- **Phase A2:** Add the binder and switch `PathMap` to slot-indexed
  `Vec<Binding>`. This is the disruptive phase; gate it behind a full pass of
  the conformance suite (`make test-conformance`).

### Test strategy

Every phase must keep `make test` and `make test-conformance` green. The
`query_optimizer` benchmarks plus the Pokec and Wikipedia benchmarks in
`crates/issundb-core/benches` provide the before-and-after signal. Add a row-heavy
benchmark (a multi-hop expansion returning many rows) before starting, since the
current optimizer benchmarks are deliberately small-result.

## Lever B: Per-Query Compilation Cost

### Current state

`query()` parses, plans, and optimizes on every call. For repeated queries
(parameterized or not) this work is redundant.

### Constraint

A plan cache is the standard fix, but the crate boundaries make the obvious
placement awkward. The optimized plan type (`PhysicalOperator`) lives in
`issundb-cypher`. `issundb-core::Graph` must not depend on `issundb-cypher`, so
the cache cannot simply be a typed field on `Graph`. Project rules also forbid
module-level mutable global state, so a `static` cache is out.

### Options

1. **Cache inside `issundb-cypher`, keyed by query string, owned by a
   per-`Graph` opaque slot.** `Graph` would hold a `Box<dyn Any + Send + Sync>`
   that `issundb-cypher` downcasts. Works, but leaks an opaque slot onto the
   core type and dents the clean-boundary design.
2. **A cached query handle in the facade.** Introduce an `issundb`-level type
   that owns the cache and wraps `query()`. Keeps the boundary clean at the cost
   of a new public surface.
3. **Faster compilation rather than caching.** Profile parse versus plan versus
   optimize; the optimizer reruns several tree walks per query. If planning
   dominates, reducing allocations there may recover much of the cost without a
   cache and without an architectural change.

Option 3 is the lowest-risk starting point and should be measured first; a cache
(option 1 or 2) is only justified if compilation cost remains significant after
profiling.

## Recommendation

Start with **Lever A, Phase A1** (the `Arc`-shared value type): it is the
highest-value change for row-heavy queries, is well isolated, and does not touch
crate boundaries. Defer Phase A2 (slot-indexed rows) until A1 is measured.
Treat Lever B as profiling-driven: measure where compilation time goes before
choosing between faster compilation and a plan cache.
