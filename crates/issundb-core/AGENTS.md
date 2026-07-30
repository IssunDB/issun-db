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

## Storage Backends

The engine is selected at compile time from the `lmdb` feature: on by default it is LMDB (`storage/lmdb.rs`), and with `--no-default-features` it is the
in-memory backend (`storage/memory.rs`). The contract both owe is documented on `storage/mod.rs`; read that before changing either.

- `heed` is named in exactly two places, both inside `storage/`. Everything else names the aliases `storage::{RoTxn, OwnedRoTxn, RwTxn}` and the twelve tables on
  `Storage`. Do not reintroduce a `heed::` path elsewhere, and do not "simplify" the aliases away: they are what let a second backend exist at all.
- `RoTxn` is the *parameter* alias and is deliberately the thread-local-agnostic flavour, because a write transaction derefs to it. That is what lets a write
  path hand its `RwTxn` to a read helper, which ~90 signatures rely on. `OwnedRoTxn` is the owned flavour, used only by `ReadTxn`'s field.
- Do not turn this into a trait. The tables hang off `Storage` which hangs off `Graph`, so a trait makes `Graph` generic over its backend and pushes that
  parameter through every crate and the public API.
- Three of the contract's guarantees are load-bearing rather than incidental LMDB behaviour, and a new backend that misses one breaks callers silently: key
  order is byte order (a `u64` key is stored big-endian so byte and numeric order agree, which the CSR build relies on when it treats `out_adj` as grouped by
  ascending node id); duplicate order is byte order (which the CSR row-reordering pass relies on); and an uncommitted transaction leaves nothing behind.
- A read transaction opened *while* a write transaction is live must work, and must see committed state. A write statement such as `MATCH ... CREATE` does
  exactly that. A single reader-writer lock deadlocks on it, which is why the in-memory backend is copy-on-write.
- Anything that is a file operation belongs on `Storage`, not on `Graph`: `copy_to_file` and `restore_from_file` are there so a backend without files refuses
  both halves rather than one. `Graph::backup` and `Graph::restore` are thin delegations.
- The in-memory backend does not persist, so a reopen sees an empty graph. Tests whose premise is reopen or backup are gated on `#[cfg(feature = "lmdb")]` and
  say so; a new test in that class needs the same gate.

## LMDB Lifetime Rules

These apply to the LMDB backend, which is the default and the only persistent one.

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

## Thread Count

Every parallel consumer resolves its budget through `threads::resolve` (`threads.rs`). Precedence, first positive value winning: the programmatic
override `Graph::set_thread_count` stored, then `ISSUNDB_NUM_THREADS`, then `OMP_NUM_THREADS`, then the machine's available parallelism, clamped to
`MAX_THREADS`. There is no graph-size heuristic.

Resolve through that one function rather than reading the environment here, or the same value comes to mean two things and two overlapping passes each
claim the whole machine. `OMP_NUM_THREADS` is honored because setting it is how a caller (including this repository's coverage job) caps parallelism
process-wide, and a caller that set it deliberately should not have to learn a second variable.

There is no thread pool. Each pass resolves the budget when it starts and spawns scoped threads for its own duration, so `Graph::set_thread_count`
stores a value, takes effect on the next pass, and cannot fail. Both stay serial below `MIN_PARALLEL_WORK`, so a unit test is deterministic.

Pick the resolver by regime, not by habit. `Graph::kernel_threads` applies `MAX_SCAN_THREADS`, because a pass that streams the adjacency arrays (a
counting kernel, or a PageRank iteration) saturates memory bandwidth long before compute and measurably slows down past that cap.
`Graph::parallel_threads` is the same resolution without the cap, for the all-pairs passes (betweenness, harmonic centrality) whose cost is arithmetic
per source out of per-worker buffers and which keep scaling. Capping those too silently discards most of the budget a caller asked for.

PageRank and harmonic centrality write disjoint output chunks and so are split-invariant; betweenness sums per-worker partials, so a total's last bits
depend on the worker count. Prefer the first shape where the algorithm allows it. A worker panic is a bug in the kernel, so it is resumed into the
caller's thread with `resume_unwind` rather than converted to an `Error`: `Corrupt` would tell an operator to restore a backup over a code defect.

## CSR Snapshot Vs. LMDB Adjacency

`CsrSnapshot` (in `csr.rs`) is a read-only in-memory Compressed Sparse Row view of the adjacency: the outgoing arrays plus a transposed incoming view
carrying per-edge type and edge ids, and optionally a per-edge weight. It is swapped atomically via `arc_swap::ArcSwap`. It is the only in-memory
adjacency structure, and every algorithm kernel in `graph/kernels/` reads it, bar `label_propagation`, which walks storage per node and is noted as the
exception in that module's header.

It is built at the smallest size its consumers read, and the build is memory-shaped in ways that are easy to undo by accident:

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
  `shortest_path_dijkstra` is its only reader. Asking for it is sticky (`CsrCache::request_weights`), so a later unweighted refresh does not strip it out
  from under an alternating workload; the cost is eight bytes per edge held once anything asks a weighted question. Requesting Dijkstra against a
  snapshot without weights is `Error::InvalidArgument`, not `Error::Corrupt`: it is a caller mistake about gating, and `Corrupt` is what tells an operator
  to restore a backup.
- `Graph::with_weighted_snapshot` returns the snapshot it validated instead of letting the caller reload the pointer. A concurrent unweighted refresh can
  replace it between a gate and a load, and the caller would then find no weights on a snapshot the gate had just vouched for.
- The public entry points that gate themselves (`page_rank`, `shortest_path`, `bfs`, `expand_bulk`) must gate *before* reading, never from inside a
  live guard. An entry point that recursed into its own gate while holding one deadlocked the calling thread against itself as soon as the gate reached a
  rebuild, since the maintenance mutex is not reentrant. Any new public entry point here follows the same order: gate, then read.

Rebuilds happen on demand through the freshness gates below; the background rebuild after `REBUILD_THRESHOLD` writes is a compaction safety net, not
the freshness path.

- Always write to LMDB first. The CSR snapshot is derived from LMDB, not the other way around.
- Use LMDB adjacency databases (`out_adj`, `in_adj`) for correctness-critical reads: single-node neighbor lookups, existence checks, and anything
  inside a transaction.
- The point adjacency lookups (`out_neighbors`, `in_neighbors`, `all_neighbors`, and `node_has_relationships`) all read `out_adj` and `in_adj`
  directly through the transaction and never consult the snapshot, so they always reflect committed and in-transaction writes. A write-time
  consistency check (such as the DELETE connected-node guard) depends on that: keep any new point lookup on storage truth rather than routing it
  through the snapshot for speed.
- Use the CSR snapshot as the hot read path for graph algorithms (BFS, DFS, PageRank, SCC). Callers do not have to refresh it by hand: every algorithm
  entry point goes through `Graph::with_snapshot`, or `Graph::with_weighted_snapshot` for the one that reads per-edge weights. A new algorithm picks
  between those two by what it reads, and reading `edge_weight` behind the unweighted one is an error rather than a wrong answer. `Graph::rebuild_csr`
  does not *ask* for weights (though it keeps loading them once something else has), so it is not a way to warm them before a burst of weighted calls;
  Dijkstra's own gate does that on first use.
- A kernel that needs a per-edge property the snapshot does not carry reads it from storage per call. That is deliberate for the weight-*property*
  algorithms (`spanning_forest`, `shortest_path_top_k`, `maximum_flow`), which take the property name as an argument: there is no fixed key to preload.

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

## Algorithm Kernels

The kernels live in `graph/kernels/`, split by family: `traversal.rs` (BFS and bulk expansion), `analytics.rs` (PageRank, components, the centralities,
label propagation), `paths.rs` (the shortest-path family and the simple-path searches), and `flow.rs` (spanning forest and maximum flow). Each is plain
Rust over the CSR arrays.

Two rules matter when adding or changing one, because both have been silently violated before:

- Duplicate handling is per algorithm, and the conventions disagree. `degree_centrality` counts *distinct* neighbors, so parallel edges collapse;
  `page_rank` spreads a source's rank over its *edges*, so parallel edges each carry mass; `betweenness_centrality` counts distinct pairs, because two
  parallel edges are one shortest path and crediting both inflates `sigma` and every dependency downstream of it; Dijkstra takes the cheapest. These were
  the `First`, `Plus`, `break`-after-first, and `Min` duplicate rules of the matrix formulation that preceded this code, and each has a test pinning it.
  A row length is not a degree and a row scan is not a transition probability: decide which convention a new kernel wants and say so in a comment. Note
  that the NetworkX oracle cannot catch a mistake here, because its corpus is simple graphs.
- No depth-first kernel may recurse over graph structure. A DFS's depth is the length of the current path, so recursion needs one call frame per node on a chain,
  and a Rust stack overflow aborts the *process* rather than returning an `Error`: a single query would take down a server. `detect_cycle`,
  `strongly_connected_components`, `all_paths`, `longest_path`, and the `all_shortest_paths` backward walk all carry their search stack on the heap for this
  reason, with the node plus a cursor into its row where the recursion kept a loop counter. `dfs` is the one exception and only because `hops: u8` bounds it at
  255 frames; widening that argument means converting it too. `deep_graph_tests` pins all of this by running the kernels on a thread with a 1 MiB stack
  (`wasm32-unknown-unknown`'s default) over a 20 000-node chain, and a regression there aborts the test binary rather than failing politely.
- Where a result's sequence is observable, fix it deliberately. A traversal reports reached nodes in ascending dense (so ascending node id) order, each
  frontier is sorted before it is consumed, so a `max_nodes` cap keeps the lowest-numbered nodes rather than whichever the traversal happened to reach
  first. Brandes accumulates over sources and predecessors in that same order, which is what makes a betweenness total reproducible run to run rather
  than merely close.

## The 12 Sub-databases

All twelve are opened once by `Storage::open`, in `storage/lmdb.rs` for the default backend, and mirrored field for field by `storage/memory.rs`. The layout
below is the LMDB one; a second backend has to reproduce its key encoding and ordering, not just its field names:

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
