# Vector Agent Guidance

Read the root `AGENTS.md` first.

## Index and Cache Invariants

- Reject empty vectors and dimension mismatches. Never pad, truncate, or change dimensions after initialization.
- Do not change metric or quantization on an index with embeddings except through the explicit reindex operation.
- Acquire the graph vector-write lock before the index lock, and never while holding the extensions mutex.
  Recheck freshness after acquiring the write lock to avoid rebuilding twice.
- Never build an index while holding the extensions mutex: building calls back into `Graph`.
- Coordinate cache rebuilds with committed raw vector writes and embedded-node deletions through vector generations.
  Invalidate the cached generation before mutating the index so a failed storage write cannot leave it serving
  an uncommitted change.
- Reject vectors for missing nodes: accepting one could attach it to a future node allocated the same ID.

## The Backend Seam

- Keep `usearch` references inside `backend.rs`. Keep the backend trait small; capacity management belongs behind
  `upsert`, not in callers or extra trait methods.
- Keep the exact backend compiled and tested even with HNSW enabled. Do not cfg it out merely to silence warnings.
- Preserve constant-time exact-backend insertion and removal. A scan per insertion makes reload quadratic.
  Slot reordering is safe only because query ties are ordered by node ID rather than storage position.
- Reserve HNSW capacity before adding, remove an existing entry before replacing it, and clamp search requests to
  the current index size. The external index does not supply those behaviors automatically.
- Apply label and property filters during traversal. Fixed over-fetch followed by filtering can return too few hits
  even when enough matching nodes exist. Rescoring is a separate mechanism, not a filtering substitute.

## Testing Rules

- Test HNSW with LMDB, exact search with LMDB, and exact search with in-memory storage. Turning off all defaults
  changes both the vector and storage backends, so that configuration alone does not test persistent exact search.
- Gate approximate or quantization-specific tests on `hnsw`, and reopen tests on `lmdb`.
- Cover persist and reload, dimension mismatches, and an empty index. Preserve `EmptyIndex` rather than silently
  returning an empty hit list; query callers can map that error to zero rows where their semantics require it.
- Give each test its own `TempDir` and graph instance.
