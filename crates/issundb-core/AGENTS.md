# `issundb-core` Agent Guide

This file covers crate-specific guidance for contributors working inside `crates/issundb-core`.
Read the root `AGENTS.md` first; the rules there apply everywhere and are not repeated here.

## Storage Invariants

These invariants must hold after every successful write transaction:

1. Adjacency consistency. For every edge `(src → dst)` stored in `out_adj` under key `src`, a matching `AdjEntry` must exist in `in_adj` under key
   `dst`, and vice versa. Both entries encode the same `EdgeId`, `TypeId`, and the other node. Never write one side without writing the other in the
   same`RwTxn`.

2. ID monotonicity. `NodeId` and `EdgeId` are allocated by `alloc_node_id` and `alloc_edge_id` in `storage/ids.rs`, which increment a `u64`
   counter stored in the `meta` sub-database. These counters must only ever increase. Never reset, reuse, or manually write a counter key outside
   `ids.rs`.

3. Label and type registry persistence. String-to-integer mappings for labels (`LabelId`) and edge types (`TypeId`) are stored as `"label:<name>"`
   and `"type:<name>"` keys in `meta`. Every node or edge write must call `get_or_create_label` or `get_or_create_type` inside the same `RwTxn` that
   writes the record. Do not cache integer IDs in memory between transactions and then use them in a later transaction without verifying they exist.

4. Secondary index consistency. `label_idx` and `type_idx` use composite keys `(u32 BE, u64 BE)` with `Unit` values. Every `add_node` must insert
   its `(LabelId, NodeId)` entry, and every `delete_node` must remove it. Same rule applies to `type_idx` for edges.

5. Property index consistency. Every `add_node` must write a `node_prop_idx` entry for each non-null scalar property in `props_json`. Every
   `update_node` must delete old entries and write new ones for all changed scalar properties. Every `delete_node` must remove all `node_prop_idx`
   entries for the deleted node. Failing to maintain this invariant causes `has_node_property_index` to return stale results and the Cypher optimizer
   to emit incorrect `NodeIndexScan` plans.

## LMDB Lifetime Rules

- Transactions must not escape the function that opened them. Open a `RoTxn` or `RwTxn`, use it, then commit (write) or drop (read) before returning.
- `RoTxn` is cheap to create; open one per read call rather than storing it across calls.
- `RwTxn` must be committed with `txn.commit()?` for changes to persist. A dropped `RwTxn` silently aborts; this is safe, but do not rely on implicit
  abort as a rollback strategy. Explicit abort is `drop(txn)`.
- Do not hold a `RwTxn` open while calling any method that might open another `RwTxn`; LMDB on Linux does not support nested write transactions.
- Do not store transactions, cursors, or database handles with lifetimes tied to the transaction in `struct` fields or `Arc`.

## Write-lock Contract

All mutations to the graph go through the `Graph` API. Inside `Graph`:

- A `parking_lot::ReentrantMutex<()>` serializes writes at the Rust level.
- The LMDB environment enforces the same constraint at the storage level.
- The `RwTxn` must be opened **inside** the lock scope, not before acquiring it. Pattern:

  ```rust
  let _guard = self._write_lock.lock();
  let mut wtxn = self.storage.env.write_txn()?;
  // ... mutations ...
  wtxn.commit()?;
  ```

- Do not bypass either lock. Do not open a `RwTxn` directly from outside `Graph` methods.

## OpenMP Thread Count

`MatrixSet::materialize` (in `matrices.rs`) sets the thread count immediately after creating the SuiteSparse:GraphBLAS context, through
`issundb_graphblas::set_global_threads(n)`. It does not decide the count itself: every parallel consumer resolves through `threads::resolve`
(`threads.rs`), which is the single resolution the GraphBLAS pool and the counting kernels share. Precedence, first positive value winning: the
programmatic override `Graph::set_thread_count` stored, then `ISSUNDB_NUM_THREADS`, then `OMP_NUM_THREADS`, then the machine's available parallelism,
clamped to `MAX_THREADS`. There is no graph-size heuristic.

Resolve through that one function rather than reading the environment here. Resolving it in two places is what previously made an unset configuration
mean one thread for the matrices and the whole machine for a kernel pass, so the two pools oversubscribed each other. `OMP_NUM_THREADS` is honored
because the GraphBLAS pool is an OpenMP pool and capping it is how a caller (including this repository's coverage job) caps that pool.

The setting is global to the SuiteSparse runtime for the lifetime of the process. `GxB_Global_Option_set(GxB_NTHREADS, n)` is called in exactly one
place, `issundb_graphblas::set_global_threads`; do not call the raw FFI from `issundb-core` or from anywhere else.

## CSR Snapshot Vs. LMDB Adjacency

`CsrSnapshot` (in `csr.rs`) is a read-only in-memory Compressed Sparse Row view of the adjacency: the outgoing arrays plus a transposed incoming view
carrying per-edge type and edge ids, and optionally a per-edge weight. It is swapped atomically via `arc_swap::ArcSwap`. `MatrixSet` (in `matrices.rs`)
holds the GraphBLAS sparse matrices derived from it.

Both are built at the smallest size their consumer reads, and both builds are memory-shaped in ways that are easy to undo by accident:

- The snapshot is built from `out_adj`, not from `edges`. The 20-byte `AdjEntry` holds the destination, the type id, and the edge id, which is every
  field the arrays carry, and the entries arrive grouped by source in ascending key order. Reading them from `edges` instead means decoding one
  `EdgeRecord` per edge, which also copies that edge's whole property blob.
- Entries go straight into the flat arrays while the pass counts each row. Do not stage a `Vec` per node first: that is one allocation per node, held
  alongside the finished arrays at the peak and then returned to the allocator as one hole per node, which `malloc_trim` cannot hand back. On a 1 M-node,
  13.9 M-edge graph it left 3.3 GB resident for 620 MB of live arrays.
- Each row is reordered by ascending edge id after the fill, because `DUPSORT` orders duplicates by the raw little-endian bytes of an `AdjEntry`, whose
  first field is `edge_type`. That order is observable (an expansion emits its neighbors in it) and `load_weights` binary-searches it, so `csr.rs` has a
  proptest pinning every array, incoming included, against the builder this replaced. The pass is a consequence of the stored layout: putting `edge_id`
  first in big-endian, or installing a `DUPSORT` comparator, would make the iteration order right natively and delete it, but both change the format and
  the order `out_neighbors` returns.
- `edge_weight` is `Option` and only `build_weighted` fills it, at the cost of a second full scan of `edges`, since a weight lives in a property blob.
  Only the weight matrix reads it, and only Dijkstra reads that.
- `MatrixTier` decides how many matrices `MatrixSet::materialize` builds, and the three rungs exist because the two upper matrices have different
  prerequisites: `page_rank_matrix` needs only the row boundaries, while `weight_matrix` needs a snapshot from `build_weighted`. Keep them apart. The
  tier is stored on the set, not inferred from which `Option` is populated, so the two cannot desynchronize into a set that claims a tier it cannot
  serve. Requesting `Weighted` with an unweighted snapshot is `Error::InvalidArgument`, not `Error::Corrupt`: it is a caller mistake about gating, and
  `Corrupt` is what tells an operator to restore a backup.
- The public `page_rank_graphblas` and `shortest_path_graphblas` gate themselves, and do it *before* taking the matrices read guard. They used to
  recurse into their gated wrapper from inside a `match` on a live guard, which deadlocks the calling thread against itself as soon as the gate reaches a
  rebuild, since `parking_lot::RwLock` is not reentrant. Any new public entry point here must follow the same order: gate, then read.
- The materialization builds one row array and one column array for the whole set and swaps their roles for a transpose, with one value array alive at a
  time. Do not go back to a triple buffer per matrix or a coordinate hash map for deduplication: `GrB_Matrix_build` wants three arrays and takes a
  duplicate-combining operator, so both were pure overhead, worth 2.7 GB above the finished matrices on that same graph.

Rebuilds happen on demand through the freshness gates below; the background rebuild after `REBUILD_THRESHOLD` writes is a compaction safety net, not
the freshness path.

- Always write to LMDB first. The CSR snapshot is derived from LMDB, not the other way around.
- Use LMDB adjacency databases (`out_adj`, `in_adj`) for correctness-critical reads: single-node neighbor lookups, existence checks, and anything
  inside a transaction.
- The point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`, and `node_has_relationships`) all read `out_adj` and `in_adj`
  directly through the transaction and never consult the snapshot, so they always reflect committed and in-transaction writes. A write-time
  consistency check (such as the DELETE connected-node guard) depends on that: keep any new point lookup on storage truth rather than routing it
  through the snapshot for speed.
- Use the CSR snapshot as the hot read path for graph algorithms (BFS, DFS, PageRank, SCC). Callers do not have to refresh it by hand: the algorithm
  entry points go through `ensure_matrix_view`, `ensure_csr_fresh`, `ensure_weighted_matrices`, or `ensure_snapshot_fresh` (see the freshness gates in the
  root `AGENTS.md`). A new algorithm picks its gate by what it reads, and reading a weighted matrix behind the wrong one is an error rather than a wrong
  answer. `Graph::rebuild_csr` remains available for forcing a full weighted-tier rebuild before a burst of algorithm calls.
- `MatrixSet` is derived from the CSR snapshot. A full rebuild goes through `MatrixSet::materialize`; incremental maintenance goes through
  `MatrixSet::apply_delta`, which patches the matrices in place from the write path's `GraphDelta` and falls back to a full rebuild when a node was
  deleted. Either way the CSR and the matrix set advance together; do not update one without the other. `apply_delta` maintains only the boolean
  adjacency, so the weighted matrices go stale behind it, which is why their consumers gate on the matrices generation rather than on the pending delta.

## In-memory Property Columns

`columns.rs` holds a typed, in-memory columnar view of scalar properties used as the hot read path for property gathers and aggregations.
It is derived from LMDB, like the CSR snapshot, and follows the same write-LMDB-first rule.

- `PropColumns<S: ColumnSource>` stores one typed column per property (Int, Float, Bool, dict-encoded Str, or a JSON fallback) over a dense
  `id -> index` map. `NodeSource` and `EdgeSource` implement `ColumnSource`, so nodes and edges share one generic store; `Graph` holds
  `prop_columns: ColumnsCache<NodeSource>` and `edge_columns: ColumnsCache<EdgeSource>`.
- `ColumnsCache<S>` builds lazily from one full `scan_all`, but a read does not necessarily cause that build, and the distinction is deliberate. A
  request for at most `SMALL_GATHER_MAX` entities is served as point reads straight from storage while the columns are absent
  (`should_serve_directly`), because building every column is one full scan and that is the wrong answer to a request for a handful of entities. Those
  direct reads amortize a build after `DIRECT_READ_BUILD_THRESHOLD` of them, so a row pipeline reading one property per row still ends up on the
  columns. The advisory statistics readers never build (see `with_existing_mut` below), and a grouped read follows the same size test by building an
  ephemeral column set over just the requested entities. Size the test on the request, not on the method: the Cypher vectorized executor gathers even a
  one-row projection through the bulk API. Nothing builds the shared columns as a side effect of a small workload, so a caller that wants them warm
  calls `Graph::materialize_property_columns`.
- Once built, the columns are kept fresh by post-commit deltas: writers call `record_touched`/`record_force_full`, and `with_fresh` patches the touched
  ids (one read transaction for the whole batch, via `fetch_many`) or rebuilds on `force_full` before serving a read. A failed absorb queues a full
  rebuild rather than dropping the taken delta, so a transient storage error cannot leave the columns quietly serving pre-write values.
- Read the columns through `with_fresh`, or through `with_existing_mut` when the caller is advisory. `with_existing_mut` never builds an absent cache;
  it answers `None` instead. That is what keeps the optimizer's selectivity estimates and the zone-map prune from making the first query that mentions
  any property pay a full node scan, so do not "fix" an advisory reader by switching it to `with_fresh`.
- Prefer the bulk forms (`Graph::node_props_json_table`, `node_prop_json_column`, `node_prop_group_codes`, and the `edge_*` equivalents): they refresh
  once and gather a whole column, versus `node_prop_json`, which refreshes per call. The Cypher vectorized aggregate path depends on the bulk forms
  (see `issundb-cypher/AGENTS.md`).
- This store is a cache, never the source of truth. Any new write path that changes a scalar property must record a delta against both `prop_columns`
  and `edge_columns` as applicable, the same way it updates `node_prop_idx`.

## Schema Statistics

`graph/stats.rs` holds the schema-level edge statistics: the `(label, type)` fan-out marginals and the realized `(src_label, type, dst_label)` triples.
Like the property columns it is a derived cache, and it follows the same rule that an advisory reader never builds one. It differs in one way that
matters, so keep the two readers apart when changing it.

- The build is one pass over `label_idx` and one over `out_adj`. Do not "simplify" it back to scanning `nodes` and `edges`: decoding a `NodeRecord` to
  read its `labels` also copies that node's whole property blob, and decoding an `EdgeRecord` to read its endpoints copies the edge's, which is most of
  the cost and none of the answer. Reading the labels from `label_idx` also means the statistics describe exactly the population a label scan enumerates.
- The generation check gates use, not refresh. A table from an earlier generation is ignored, never served: `schema_has_edge` reads the same table, and a
  stale negative there would deny a triple a committed write just realized.
- `estimate_expand_fanout` and `estimate_expand_fanout_to` are advisory, and read through `with_possibly_stale_fanout`. Nothing builds the table for them;
  `Graph::materialize_edge_statistics` is the deliberate warm-up: `issundb-cli` calls it synchronously on open, and `issundb-rest` and `issundb-mcp` spawn
  it in the background so readiness is not gated on a scan that costs seconds (3.4 s on a 1 M-node, 13.9 M-edge graph). They accept a table the generation
  has moved past, bounded per relationship type by `STALE_FANOUT_GROWTH_FACTOR` against that type's live `stats:t:` counter. Do not "fix" this to a strict generation check: that is what made a warmed process lose its statistics on the first write and never
  recover them. Do not weaken the bound to a global edge count either, which cannot see a skewed ingest that quadruples one type inside a graph that grew
  by a third, and do not remove it, or a process that warms at startup and then ingests plans forever against the startup snapshot.
- The whole build runs outside the `edge_fanout` lock, with the result installed at the end. Holding the lock across it was tolerable while this was an
  internal lazy helper; it is not now that consumers call it on a live graph, where it would block every concurrent query's planning for the scan's
  duration.
- Both accessors read `csr_cache.current_gen()` *after* taking the lock. Reading it first leaves a window in which a write commits before the lock is
  acquired, so a table predating that write matches the captured value and passes as current: for `schema_has_edge` that is a stale negative, and the
  optimizer drops rows on it.
- `schema_has_edge` is not advisory, because the optimizer prunes rows on a negative. It must keep answering with no table, so it falls back to a bounded
  probe of `label_idx` plus the adjacency (`SCHEMA_PROBE_BUDGET`), settling on the first matching edge and reporting `None` when the budget runs out.
  Do not make this reader table-only: that leaves `prune_unsatisfiable` dormant on every graph nobody has materialized, which is a silent loss of the
  pass rather than a visible failure. Its one test is `type_inference_prunes_unsatisfiable_pattern` in `issundb-cypher`.
- Charge the probe budget for every storage operation, including visiting a node before its adjacency is read. Charging only adjacency entries left the
  walk unbounded for exactly the population most likely to be chosen: a small label whose nodes have no edges in the direction under test, where every
  node took the "no adjacency" path for free.
- Probe verdicts are memoized in `schema_probes` against the write generation. The pass that asks runs on every execution because there is no plan cache,
  so without the memo an unsatisfiable hop re-walks the graph once per query rather than once per generation. The memo is keyed on the generation, never
  cleared lazily, so it cannot answer for a graph a write has changed.
- A `Some(false)` from either path must never rest on a stored counter. The probe reads the per-label counter only to choose which endpoint population to
  walk, where a wrong answer costs at most an undecided verdict; emptiness is asked of `label_idx` directly.

## GraphBLAS Semiring Choices

Use the correct GraphBLAS semiring for each algorithm:

| Algorithm                      | Semiring                              | Notes                                                                           |
|--------------------------------|---------------------------------------|---------------------------------------------------------------------------------|
| BFS / reachability             | Boolean (`any + land` / `lor + land`) | Frontier is a boolean vector; multiplication is logical AND.                    |
| PageRank                       | FP32 / FP64 (`plus × times`)          | Column-stochastic matrix `M` times rank vector; accumulate with addition.       |
| SSSP (Dijkstra / Bellman-Ford) | Min-plus tropical (`min + plus`)      | Relax edge weights; `min` replaces addition and `plus` replaces multiplication. |
| Typed pattern matching         | Boolean element-wise                  | Per-type boolean matrix; element-wise `land` between type matrices.             |

When adding a new graph algorithm, document the semiring choice in a comment above the operation.

## The 12 LMDB Sub-databases

All sub-databases are opened once by `Storage::open` in `storage/lmdb.rs`:

| Name            | Key                                                        | Value                                 | Notes                                                                                                                                                                                                  |
|-----------------|------------------------------------------------------------|---------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `nodes`         | `u64 BE` (NodeId)                                          | msgpack `NodeRecord`                  | Primary node store.                                                                                                                                                                                    |
| `edges`         | `u64 BE` (EdgeId)                                          | msgpack `EdgeRecord`                  | Primary edge store.                                                                                                                                                                                    |
| `out_adj`       | `u64 BE` (NodeId)                                          | `AdjEntry` (20 B, DUPSORT + DUPFIXED) | Outgoing adjacency; one duplicate per edge.                                                                                                                                                            |
| `in_adj`        | `u64 BE` (NodeId)                                          | `AdjEntry` (20 B, DUPSORT + DUPFIXED) | Incoming adjacency; mirror of `out_adj`.                                                                                                                                                               |
| `label_idx`     | `(u32 BE, u64 BE)` = 12 B composite                        | `Unit`                                | Secondary index: `(LabelId, NodeId)`.                                                                                                                                                                  |
| `type_idx`      | `(u32 BE, u64 BE)` = 12 B composite                        | `Unit`                                | Secondary index: `(TypeId, EdgeId)`.                                                                                                                                                                   |
| `node_prop_idx` | `(LabelId, PropKeyId, encoded_val, NodeId)` variable       | `Unit`                                | Property range index for nodes. Auto-populated for every scalar property on every `add_node` and `update_node` (semi-columnar auto-index); also used for user-created unique and required constraints. |
| `edge_prop_idx` | `(TypeId, PropKeyId, encoded_val, EdgeId)` variable        | `Unit`                                | Property range index for edges.                                                                                                                                                                        |
| `fts_postings`  | `(LabelId, PropKeyId, term)` variable (DUPSORT + DUPFIXED) | 12 B `(NodeId BE, frequency BE)`      | Inverted posting lists for full-text search.                                                                                                                                                           |
| `fts_docs`      | 16 B `(LabelId, PropKeyId, NodeId BE)`                     | 4 B `u32 BE` doc length               | Per-document term count for BM25.                                                                                                                                                                      |
| `vectors`       | `u64 BE` (NodeId)                                          | raw `f32` bytes (little-endian)       | Persistent vector embeddings.                                                                                                                                                                          |
| `meta`          | `Str` key                                                  | `Bytes` value                         | Counters, label/type registries, FTS stats.                                                                                                                                                            |

`DUPSORT + DUPFIXED` databases require all duplicate values under a key to be the same byte length; `AdjEntry` is 20 bytes and FTS posting values are
12 bytes.

## `deepsize::DeepSizeOf` Usage

`deepsize` is used to track heap allocation of record types for memory instrumentation:

- Derive `#[derive(DeepSizeOf)]` for types that own heap-allocated fields (`Vec<u8>`, `String`, nested structs with allocations). Examples:
  `NodeRecord`, `EdgeRecord`.
- Implement manually for `#[repr(C, packed)]` or zero-copy structs that contain no heap allocations. Override `deep_size_of_children` to return
  `0`. Example: `AdjEntry`.
- Do not derive `DeepSizeOf` for types that are never measured; implement it only where the size is actually read at runtime.

## Testing Rules

- Every test that touches LMDB must open a fresh `tempfile::TempDir`:

  ```rust
  let dir = TempDir::new().unwrap();
  let graph = Graph::open(dir.path(), 1).unwrap();
  ```

- Never share a `Graph`, `Storage`, or `TempDir` across tests. Each test is responsible for its own directory.
- Use `proptest` for ID uniqueness and adjacency round-trip invariants. A good property: for any sequence of `add_node` / `add_edge` / `delete_node`
  calls, every surviving edge must have matching entries in both `out_adj` and `in_adj`.
- Prefer targeted, single-assertion tests over broad snapshot comparisons. Test one round-trip, one count, or one invariant per test function.
- After any mutation test, verify the inverse: delete what was added and check that the record and all index entries are gone.
