# `issundb-vector` Agent Guide

This file covers crate-specific guidance for contributors working inside `crates/issundb-vector`.
Read the root `AGENTS.md` first; the rules there apply everywhere and are not repeated here.

## `VectorIndex` Lifecycle

`VectorIndex` starts in the `Inner::Empty` state and is lazily initialized on the first call to `upsert`:

1. Empty: no backend exists yet; the dimension count is unknown.
2. Ready: a backend is live with a fixed dimension count; `upsert` and `search` both operate against it.

State transitions are guarded by an internal `RwLock<Inner>`; a read-only search takes the read guard, and `upsert` and `remove` take the write guard.
Initialization happens inside the mutex: call `backend::new_backend(dims, opts)`, then insert the first vector.
Once `Ready`, the dimension count is immutable for the lifetime of the index.

## Dimension Contract

All vectors added to a given `VectorIndex` must have the same number of dimensions.
This is enforced at the API boundary:

- In `upsert`, if `v.len() != dims` for a `Ready` index, return `Err(VectorError::DimensionMismatch { expected, got })` immediately. Never silently
  truncate or pad the vector.
- In `search`, if `q.len() != dims`, return the same `VectorError::DimensionMismatch`.
- An empty vector (`v.len() == 0`) is rejected by `upsert` before the state check.

Do not add any path that changes `dims` after initialization.

## `VectorIndexOptions` Fields

`VectorIndexOptions` (in `src/index.rs`) controls index construction:

- `metric: VectorMetric` (default: `Cosine`): the distance function used for all ANN queries on this index. Options:
    - `Cosine`: angular similarity; suitable for normalized text embeddings.
    - `L2`: Euclidean distance; suitable for spatial or non-normalized vectors.
    - `Dot`: inner product; use when vectors are already normalized to unit length and maximum dot product is the goal.
- `quantization: VectorQuantization` (default: `Float32`): scalar precision for stored vectors. Trade-offs:
    - `Float32`: full precision, no recall loss.
    - `Float16`: 2x memory reduction, minor recall loss (typically < 1 %).
    - `Int8`: 4x memory reduction, moderate recall loss; suitable for large corpora where approximate results are acceptable.

The metric and quantization are fixed at index construction time. `configure_vector_index` therefore returns `VectorError::AlreadyConfigured` when it
would change either one on a graph that already holds embeddings. `reindex_vector_index` is the sanctioned way to change them afterward: the stored
vectors are raw, metric-agnostic f32, so it rebuilds the whole in-memory index from LMDB under the new configuration. That rebuild is O(n) and is an
administrative operation, not a concurrent one.

## The Backend Seam

`backend.rs` owns the index and is the only module that may name `usearch`. It selects one implementation at compile time from the `hnsw` feature, which is
on by default:

- With `hnsw`: `HnswBackend`, wrapping `usearch`. This is the workspace's only C++ dependency, reached through `cxx`, and it is why a build without the
  feature exists at all: `usearch` cannot cross-compile to a target with no C++ toolchain.
- Only `ExactBackend`'s *query* is linear. Insertion and removal are `O(1)` through a `node -> slot` map, because those are the operations a rebuild performs and the
  index rebuilds from stored embeddings on every `Graph::open`; a scan there made the rebuild quadratic (roughly 80x slower by 40 k vectors). A removal
  `swap_remove`s and repairs the moved entry's slot, which is safe only because the search sorts by `(distance, node)` and so never depends on vector order.
  `exact_backend_cost` (ignored by default) is the harness for the exact-versus-approximate decision. Its recorded figures, on a 12-thread x86-64 machine at
  four dimensions in a release build: rebuild 0.9 ms at 10 k vectors, 4.0 ms at 40 k, and 13.5 ms at 160 k, with a query of 24, 95, and 360 us across the same
  sizes, both linear. Before the slot map the same rebuild measured 21.4 ms at 10 k, 80.7 ms at 20 k, and 334.6 ms at 40 k. Scale the query figure by the real
  dimension count, since 384 or 768 rather than four is what a caller will have.
- Without `hnsw`: `ExactBackend`, a pure-Rust scan. It is exact rather than approximate, ranks through the same `exact_distance` the rescore pass uses, and
  breaks distance ties by node id so a top-k is deterministic. It ignores `quantization`, keeping the raw `f32`, and its query cost is linear in the vector
  count.

Rules for changing this:

- Keep the trait small; it is five methods, and every one of them is a promise a future backend has to keep. Push anything a backend can do for itself, such
  as the capacity dance below, behind `upsert` rather than into the trait.
- `ExactBackend` compiles in both configurations and has its own tests, so it cannot rot while the feature is on. Do not `cfg` it out to silence a
  dead-code warning; construct it in a test instead.
- The whole suite must pass in both configurations. Note that plain `--no-default-features` no longer isolates this crate's own dimension: `lmdb` is a default
  feature too now, so it also swaps the storage backend for the non-persistent one. The three commands that matter are `cargo test -p issundb-vector` (HNSW,
  LMDB), `cargo test -p issundb-vector --no-default-features --features lmdb` (exact index with LMDB, the one that covers this crate's storage integration), and
  `cargo test -p issundb-vector --no-default-features` (exact index, in-memory store, which is the wasm configuration). A test that genuinely depends on
  approximate behavior or on quantization belongs behind `#[cfg(feature = "hnsw")]`; one that reopens the graph belongs behind `#[cfg(feature = "lmdb")]`, and
  the four reopen tests here already are.

### `usearch` API Notes

These constraints are the reason `HnswBackend::upsert` looks the way it does. The usearch `Index` does not auto-grow its internal capacity:

- Call `index.reserve(n)` before calling `index.add`. The initial reservation on construction is `64`.
- Before each subsequent add, check `index.size() >= index.capacity()`. If true, call `index.reserve((index.capacity() * 2).max(64))` first.
- `index.add(node_id, vector)` does not replace an existing entry; call `index.remove(node_id)` first when `index.contains(node_id)`.
- usearch `search` returns at most `min(k, index.size())` results. Clamp `k` to `index.size()` before searching to avoid requesting more results than
  the index holds.

## The Cold-start Pattern in `get_or_init_cache`

`get_or_init_cache` builds the in-memory index from storage on first use, and it is one call:
`graph.get_or_init_extension_with(..)`, whose initializer loads the persisted configuration and then every embedding. Do not hand-roll the locking
around it. The helper owns the double-checked insert, and the initializer deliberately runs *without* the `extensions` lock held, which is what makes
reading from storage there safe.

That ordering is the rule to preserve: never call `graph.vector_bytes()`, or any other `Graph` method, while holding the `extensions` mutex.

## `VectorSearchOptions` Filters

When `opts.label` or `opts.properties` are set, the filter is evaluated **during** the traversal, not after it. `vector_search_with` builds a predicate
over `NodeId` and hands it to `VectorIndex::search_filtered`, which reaches the backend's own filtered search: usearch's `filtered_search` with `hnsw`,
and the scan's `keep` predicate without it. A node matches when it carries `opts.label` (if set) and every entry in `opts.properties` (if set) equals the
node's value for that property.

Do not replace this with a post-filter over a fixed over-fetch. Over-fetching a constant multiple of `k` and then discarding non-matching hits
silently under-returns for a selective filter: the traversal stops after `k * factor` candidates whether or not any of them match, so a filter that
excludes most of the index yields fewer than `opts.k` results even when far more matching nodes exist. Filtering inside the traversal keeps expanding
until it has `opts.k` matching neighbors. Fewer than `opts.k` results then means the index genuinely holds fewer matching nodes; do not error in that
case.

`opts.rescore_factor` is a separate mechanism and is not a filtering over-fetch. On a quantized index the search fetches `k * rescore_factor`
candidates (default 2) and re-ranks them by exact distance against the full-precision vectors in LMDB. It defaults to 1 on a `Float32` index, and
`Some(1)` disables it.

## Testing Rules

Every test that touches vector behavior must cover all three of the following scenarios, each in its own test function:

1. Persist and reload: `upsert → search` in one `Graph` instance; then reopen the same path and `search` again. The same nearest neighbor must
   appear after reload.
2. Dimension mismatch: after the first `upsert` fixes dimensions, a second `upsert` with a different dimension count must return
   `Err(VectorError::DimensionMismatch { .. })`.
3. Empty index: `vector_search` on a graph with no vectors must return `Err(VectorError::EmptyIndex)`, not an empty `Vec`, so a caller can tell
   "no semantic matches" apart from "there is nothing to search". Mapping that error back to zero rows is the caller's job where MATCH semantics
   demand it, as the Cypher `VectorTopK` operator does.

Each test must open its own `TempDir` and must not share a `Graph` instance with other tests.
