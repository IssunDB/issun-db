# issundb-core Agent Guide

This file covers crate-specific guidance for contributors working inside `crates/issundb-core`.
Read the root `AGENTS.md` first; the rules there apply everywhere and are not repeated here.

## Storage Invariants

These invariants must hold after every successful write transaction:

1. **Adjacency consistency.** For every edge `(src → dst)` stored in `out_adj` under key `src`, a matching `AdjEntry` must exist in `in_adj` under key
   `dst`, and vice versa. Both entries encode the same `EdgeId`, `TypeId`, and the other node. Never write one side without writing the other in the
   same`RwTxn`.

2. **ID monotonicity.** `NodeId` and `EdgeId` are allocated by `alloc_node_id` and `alloc_edge_id` in `storage/ids.rs`, which increment a `u64`
   counter stored in the `meta` sub-database. These counters must only ever increase. Never reset, reuse, or manually write a counter key outside
   `ids.rs`.

3. **Label and type registry persistence.** String-to-integer mappings for labels (`LabelId`) and edge types (`TypeId`) are stored as `"label:<name>"`
   and `"type:<name>"` keys in `meta`. Every node or edge write must call `get_or_create_label` or `get_or_create_type` inside the same `RwTxn` that
   writes the record. Do not cache integer IDs in memory between transactions and then use them in a later transaction without verifying they exist.

4. **Secondary index consistency.** `label_idx` and `type_idx` use composite keys `(u32 BE, u64 BE)` with `Unit` values. Every `add_node` must insert
   its `(LabelId, NodeId)` entry, and every `delete_node` must remove it. Same rule applies to `type_idx` for edges.

5. **Property column consistency.** Every `add_node` must write a `node_prop_idx` entry for each non-null scalar property in `props_json`. Every
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

## Write-Lock Contract

All mutations to the graph go through the `Graph` API. Inside `Graph`:

- A `parking_lot::ReentrantMutex<()>` serializes writes at the Rust level.
- The LMDB environment enforces the same constraint at the storage level.
- The `RwTxn` must be opened **inside** the lock scope, not before acquiring it. Pattern:

  ```rust
  let _guard = self.write_lock.lock();
  let mut wtxn = self.storage.env.write_txn()?;
  // ... mutations ...
  wtxn.commit()?;
  ```

- Do not bypass either lock. Do not open a `RwTxn` directly from outside `Graph` methods.

## OpenMP Thread Count

`MatrixSet::materialize` (in `matrices.rs`) calls `GxB_Global_Option_set(GxB_NTHREADS, n)` immediately after creating the SuiteSparse:GraphBLAS
context. The thread count is threshold-gated: graphs with more than 100 000 edges use `std::thread::available_parallelism()` cores; smaller graphs use
1 thread to avoid scheduling overhead on short operations. This setting is global to the SuiteSparse runtime for the lifetime of the process; do not
call `GxB_Global_Option_set` from anywhere else.

## CSR Snapshot vs. LMDB Adjacency

`CsrSnapshot` (in `csr.rs`) is a read-only in-memory Compressed Sparse Row view of outgoing edges, rebuilt in the background and swapped atomically
via `arc_swap::ArcSwap`. `MatrixSet` (in `matrices.rs`) holds the GraphBLAS sparse matrices derived from the CSR snapshot.

- **Always write to LMDB first.** The CSR snapshot is derived from LMDB, not the other way around.
- Use LMDB adjacency databases (`out_adj`, `in_adj`) for correctness-critical reads: single-node neighbor lookups, existence checks, and anything
  inside a transaction.
- Note that `out_neighbors` consults the CSR snapshot first and falls back to `out_adj` only when the snapshot has no entry for the node, so it can
  return stale results until the background rebuild completes. `in_neighbors` reads `in_adj` directly. A write-time consistency check (such as the
  DELETE connected-node guard) must read storage truth: use `node_has_relationships`, which reads both adjacency databases and never consults the
  snapshot.
- Use the CSR snapshot as the hot read path for graph algorithms (BFS, DFS, PageRank, SCC). After a batch of writes, call `Graph::rebuild_csr` to
  refresh it.
- `MatrixSet` is rebuilt from the CSR snapshot by `MatrixSet::materialize`. Rebuild both the CSR and the matrix set together; do not update one
  without the other.

## GraphBLAS Semiring Choices

Use the correct GraphBLAS semiring for each algorithm:

| Algorithm                      | Semiring                              | Notes                                                                           |
|--------------------------------|---------------------------------------|---------------------------------------------------------------------------------|
| BFS / reachability             | Boolean (`any + land` / `lor + land`) | Frontier is a boolean vector; multiplication is logical AND.                    |
| PageRank                       | FP32 / FP64 (`plus × times`)          | Column-stochastic matrix `M` times rank vector; accumulate with addition.       |
| SSSP (Dijkstra / Bellman-Ford) | Min-plus tropical (`min + plus`)      | Relax edge weights; `min` replaces addition and `plus` replaces multiplication. |
| Typed pattern matching         | Boolean element-wise                  | Per-type boolean matrix; element-wise `land` between type matrices.             |

When adding a new graph algorithm, document the semiring choice in a comment above the operation.

## The 12 LMDB Sub-Databases

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

- **Derive** `#[derive(DeepSizeOf)]` for types that own heap-allocated fields (`Vec<u8>`, `String`, nested structs with allocations). Examples:
  `NodeRecord`, `EdgeRecord`.
- **Implement manually** for `#[repr(C, packed)]` or zero-copy structs that contain no heap allocations. Override `deep_size_of_children` to return
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
