# Query Optimizer

IssunDB compiles a Cypher query into a logical plan, lowers it to a physical
plan, and then runs a fixed, ordered sequence of rule-based rewrite passes. The
optimizer is rule-based, not cost-based: the only statistics consulted are
per-label and per-type counts and node-property-index existence. There is no
join enumeration or plan-cost search.

## Optimization Passes

Passes run in this order (see `crates/issundb-cypher/src/plan/optimize.rs`):

1. **Extract filters.** Pull every `Filter` predicate out of the spine into a
   flat list so later passes see a contiguous scan and expand tree.
2. **Eliminate true filters.** Drop statically-true predicates (`WHERE true`,
   and equality or inequality of two identical-form literals) so they are
   neither pushed down nor evaluated per row. False and unknown predicates are
   preserved.
3. **Reorder operators.** Place the heavier branch of each `HashJoin` on the
   left for a deterministic structure.
4. **Select scan node.** Reverse a linear single-hop `Expand` chain to start
   from its lowest-cardinality or index-backed endpoint, flipping each hop's
   direction. An index-backed equality endpoint wins over raw label
   cardinality, so the reversal never regresses an index scan.
5. **Push down filters.** Place each predicate at the lowest operator whose
   bound variables cover it.
6. **Index and id-seek selection.** Rewrite `Filter` over a `LabelScan` into a
   `NodeIndexScan` or `NodeRangeScan` when a property index exists, and rewrite
   `WHERE id(n) = <const>` into a `NodeByIdSeek` primary-key lookup.
7. **Closing-hop rewrite.** Rewrite a cycle-closing `Expand` (whose target is
   already bound) into a `MultiwayJoin` with an O(1) lookup per row.
8. **Count reduction.** Replace `count(*)` or `count(n)` over a bare labeled
   scan with a constant read from label metadata.

Execution additionally fuses a contiguous chain of single-hop directed Expands
into one pass that clones the base path once per output row, regardless of chain
length (`execute_expand_chain_n`).

## Benchmark Baselines

The benchmarks in `crates/issundb/benches/query_optimizer.rs` contrast each
accelerated query against a semantically-similar query that is not accelerated,
on the same data. Reproduce with:

```bash
cargo bench -p issundb --bench query_optimizer
```

Indicative results on a development machine (50,000 Person nodes for the
scan-comparison groups, 5,000 for the expansion groups). Treat the ratios, not
the absolute times, as the regression signal:

| Group            | Optimized                   | Baseline                            | Ratio |
|------------------|-----------------------------|-------------------------------------|-------|
| `reduce_count`   | ~34 µs (`count(*)`)         | ~32 ms (count over a property scan) | ~940x |
| `id_seek`        | ~113 µs (`WHERE id(n) = k`) | ~27.6 ms (full unindexable scan)    | ~245x |
| `scan_selection` | ~7.1 ms (reversed chain)    | regression guard                    | n/a   |
| `chain_fusion`   | ~10.8 ms (three-hop fused)  | regression guard                    | n/a   |

### Notes on Interpretation

- IssunDB auto-indexes equality predicates, so `WHERE n.seq = k` is already an
  index lookup (~90 µs), not a full scan. The id-seek's value is for `id()`
  predicates, which have no property index to fall back on; the `id_seek` ratio
  above is measured against a genuinely unindexable full scan (a modulo
  predicate the auto-indexer cannot convert).
- For small result sets, query parse, plan, and optimize overhead dominates the
  measured latency rather than storage access. A significant regression in the
  optimized columns above usually points to a broken rewrite rule; a uniform
  slowdown across all groups usually points to compilation overhead.
