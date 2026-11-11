# Core Agent Guidance

Read the root `AGENTS.md` first.

## Storage and Write Invariants

- Write both adjacency directions and their record in the same transaction. Never publish one without the other.
- Never reset or reuse node or edge IDs, or write allocation counters outside the allocation helpers. Do not reuse
  registry IDs across transactions without verifying that the mappings committed.
- Maintain label and property indexes in the record's transaction. Respect auto-index opt-outs while preserving
  declared indexes and constraints. An omitted index entry must not make a stored value unqueryable.
- Preserve persisted numeric key encoding, including signed zero. Equality and uniqueness must probe both zero keys.
  Verify index candidates against stored values where unsigned keys can collide above `i64::MAX`; range ties and
  constraint backfills must preserve exact numeric identity, including equal integer and float values.
- Read `storage/mod.rs` before changing a backend. Preserve byte ordering of keys and duplicates, rollback, and
  reads of committed state while a write is open. A single reader-writer lock would deadlock `MATCH ... CREATE`.
- Keep `heed` paths inside `storage/` and retain the transaction aliases. The read parameter alias must accept a
  dereferenced write transaction; do not replace it with the owned read-transaction flavor.
- Keep file operations on `Storage` so a backend without files refuses both backup and restore.
- Acquire the graph write lock before opening a write transaction. Never open another write transaction while one
  is live. Do not retain borrowed transactions or cursors beyond their lifetime.
- Route every fallible `WriteTxn` mutation through `mutate`. A caught mutation error must still poison the transaction
  and prevent commit; a read error must not.
- Use `WriteBatchCache` for all allocations and count changes inside `WriteTxn`. Mixing direct metadata writes with
  cached counters can allocate an ID twice or clamp counts against stale values.
- Bulk adjacency buffering must affect cost only: reads merge buffered entries in store order, and deletions flush
  before walking the stores. Keep sorted cursor writes rather than random per-edge puts.
- Advance vector generations only after commit. Aborts must leave them unchanged, including raw vector writes and
  node deletions that remove embeddings.

## CSR Snapshot Vs. LMDB Adjacency

- Storage is authoritative. Point adjacency reads and reads inside a write transaction must use storage truth,
  including pending writes, rather than a committed snapshot.
- Keep `Graph::open` lazy. Small point workloads must not pay for a full snapshot or property-column build.
- Gate snapshot freshness before acquiring a live snapshot guard. Reentering the maintenance gate while holding
  a guard can deadlock. Use the weighted gate when reading weights, and use the snapshot that gate validated
  rather than reloading a pointer a concurrent refresh could replace.
- Keep weight requests sticky across refreshes to avoid alternating weighted and unweighted rebuilds.
  Do not make every bulk load request weights. Property-name-based algorithms must read their requested property.
- Build directly into flat arrays from adjacency, not decoded edge records or a temporary vector per node. The
  latter retains property blobs or leaves allocator fragmentation proportional to the graph size.
- Preserve ascending edge-ID order in every row, including incremental patches. Raw duplicate order differs from
  that observable expansion order, and weight loading binary-searches it. Changing stored ordering requires a
  format-compatibility decision.
- Retain the incoming type array: indirect type reads through outgoing positions have regressed typed traversal.
- Record incremental changes before advancing the generation. An install must discard only changes already in its
  captured generation. Decline incremental patches when node ordering would invalidate dense indices.
- Mutate mapped arrays only through `Array::with_mut`; never write through a mapped cache file.
- Accept cache files only after identity, generation, bounds, and checksum validation. Restore must not reuse
  another database's cache at a coincidentally matching generation.
- Capture generations before building or saving so a concurrent write leaves results conservatively stale.
- Keep cache saves explicit: `rebuild_csr` and the property-column materializers are the save sites. Query-time
  refreshes must not write cache files. Explicit CSR rebuilds must read storage so a bad file can be repaired.
- Preserve safe replacement of live mappings, including Windows rename restrictions and old-file cleanup.

## In-memory Property Columns

- Stream decoded records into column builders and discard each record before reading the next. Do not collect
  every property map before building columns.
- Size cold-read decisions on the request, not the API method: even bulk APIs can receive one entity.
- Advisory readers must not build absent columns. Use `with_existing_mut`; use `with_fresh` for authoritative reads.
- Record post-commit deltas for every affected node or edge column cache. If absorbing a delta fails, queue a full
  rebuild rather than losing the delta and serving stale values.
- Prefer bulk gathers over repeated per-property refreshes. Do not introduce a separate storage scan per property.
- Keep typed comparison masks consistent with boxed Cypher comparisons, including nulls and mixed numeric types.

## Schema Statistics

- Build from label indexes and adjacency rather than decoding records with property blobs.
- Advisory fan-out estimates may use stale statistics only within the per-type growth bound. Do not replace that
  bound with global edge growth, remove it, or require strict freshness after every write.
- Build outside the statistics lock, then install the result. Holding the lock during the scan blocks query planning.
- Read the generation after taking the lock; reading it first can accept a stale negative after a concurrent commit.
- `schema_has_edge` is authoritative because a negative drops rows. Never use stale statistics for it. Keep its
  budgeted storage fallback, charge every storage operation, and return unknown when the budget is exhausted.
- Never prove emptiness from counters. Counters may choose a probe direction; the label index must prove emptiness.
- Key memoized probe verdicts by write generation so a write cannot leave a false negative cached.

## Algorithms and Parallelism

- Document the duplicate rule for new kernels and retain tests for parallel edges. Do not substitute row lengths
  for distinct degrees or reachability for per-edge influence.
- Do not recurse over unbounded graph depth: stack overflow aborts the process. Keep search stacks on the heap.
  The bounded `dfs(hops: u8)` exception must become iterative if that bound is widened.
- Preserve deterministic traversal, frontier, and accumulation order. A result cap must not select arbitrary nodes.
- Keep Louvain serial: local moving is order-dependent, so splitting it across workers changes the partition.
- Resolve thread budgets through `threads::resolve`, never by reading environment variables independently.
  Use `kernel_threads` for bandwidth-bound scans and `parallel_threads` for all-pairs work; the latter must not
  silently discard the caller's requested budget through the scan cap.
- Prefer disjoint output partitions when possible. Betweenness partial sums can vary in their last bits with
  worker count; do not promise bitwise equality across counts.
- Resume worker panics rather than reporting storage corruption, which could prompt an unnecessary restore.

## Instrumentation and Tests

- Derive `DeepSizeOf` only for measured heap-owning types. Packed, heap-free records need a manual zero-child-size
  implementation rather than a derive that takes unaligned references.
- Use property tests for ID uniqueness and adjacency round trips. Mutation tests should verify deletion removes
  the record and its index entries as well as checking insertion.
