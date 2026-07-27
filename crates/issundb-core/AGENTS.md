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
carrying per-edge type and edge ids, and a per-edge weight. It is swapped atomically via `arc_swap::ArcSwap`. `MatrixSet` (in `matrices.rs`) holds the
GraphBLAS sparse matrices derived from it.

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
  entry points go through `ensure_matrix_view`, `ensure_csr_fresh`, or `ensure_snapshot_fresh` (see the freshness gates in the root `AGENTS.md`).
  `Graph::rebuild_csr` remains available for forcing a full rebuild before a burst of algorithm calls.
- `MatrixSet` is derived from the CSR snapshot. A full rebuild goes through `MatrixSet::materialize`; incremental maintenance goes through
  `MatrixSet::apply_delta`, which patches the matrices in place from the write path's `GraphDelta` and falls back to a full rebuild when a node was
  deleted. Either way the CSR and the matrix set advance together; do not update one without the other.

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
