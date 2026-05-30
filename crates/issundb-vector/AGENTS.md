# issundb-vector Agent Guide

This file covers crate-specific guidance for contributors working inside `crates/issundb-vector`. Read the root `AGENTS.md` first; the rules there
apply everywhere and are not repeated here.

## `VectorIndex` Lifecycle

`VectorIndex` starts in the `Inner::Empty` state and is lazily initialized on the first call to `upsert`:

1. **Empty**: no usearch index exists yet; the dimension count is unknown.
2. **Ready**: the index is live with a fixed dimension count; `upsert` and `search` both operate against it.

State transitions are guarded by an internal `parking_lot::Mutex<Inner>`. Initialization happens inside the mutex: create an `IndexOptions`, call
`Index::new`, call `index.reserve(64)`, then insert the first vector. Once `Ready`, the dimension count is immutable for the lifetime of the index.

## Dimension Contract

All vectors added to a given `VectorIndex` must have the same number of dimensions. This is enforced at the API boundary:

- In `upsert`, if `v.len() != dims` for a `Ready` index, return `Err(Error::Vector(...))` immediately. Never silently truncate or pad the vector.
- In `search`, if `q.len() != dims`, return `Err(Error::Vector(...))`.
- An empty vector (`v.len() == 0`) is rejected by `upsert` before the state check.

Do not add any path that changes `dims` after initialization.

## `VectorIndexOptions` Fields

`VectorIndexOptions` (in `src/index.rs`) controls index construction:

- `metric: VectorMetric` (default: `Cosine`): the distance function used for all ANN queries on this index. Options:
    - `Cosine`: angular similarity; suitable for normalized text embeddings.
    - `L2`: Euclidean distance; suitable for spatial or non-normalized vectors.
    - `Dot`: inner product; use when vectors are already normalized to unit length and maximum dot product is the goal.
    - `Hamming`: bit-level distance for binary vectors; requires `B1` quantization.
- `quantization: VectorQuantization` (default: `F32`): scalar precision for stored vectors. Trade-offs:
    - `F32`: full precision, no recall loss.
    - `F16`: 2x memory reduction, minor recall loss (typically < 1 %).
    - `I8`: 4x memory reduction, moderate recall loss; suitable for large corpora where approximate results are acceptable.
    - `B1`: 32x memory reduction, significant recall loss; use only for binary vectors with `Hamming` metric.

The metric and quantization are fixed at index construction time and cannot be changed without rebuilding the index from scratch.

## usearch API Notes

The usearch `Index` does not auto-grow its internal capacity. Follow these rules:

- Call `index.reserve(n)` before calling `index.add`. The initial reservation on first `upsert` is `64`.
- Before each subsequent `upsert` in the `Ready` branch, check `index.size() >= index.capacity()`. If true, call
  `index.reserve((index.capacity() * 2).max(64))` before adding.
- `index.add(node_id, vector)` does not replace an existing entry; call `index.remove(node_id)` first if the node already exists in the index (
  `index.contains(node_id)`).
- usearch `search` returns at most `min(k, index.size())` results. Clamp `k` to `index.size()` before searching to avoid requesting more results than
  the index holds.

## The Cold-Start Pattern in `get_or_init_cache`

`get_or_init_cache` builds the in-memory HNSW index from LMDB on first use:

1. Call `graph.get_extension::<VectorIndexCache>()` under no lock. If present, return it immediately.
2. Call `graph.vector_bytes()` **before** acquiring the `extensions` lock. This avoids holding both the LMDB read lock and the extensions lock
   simultaneously.
3. Build a fresh `VectorIndex` and populate it from the loaded bytes.
4. Acquire the `extensions` lock and do a second existence check (double-check idiom) before inserting, to prevent overwriting an index that was
   concurrently initialized by another thread.

Never call `graph.vector_bytes()` or any `Graph` method while holding the `extensions` mutex.

## `VectorSearchOptions.label` Filter

When `opts.label` is `Some(label)`:

1. Over-fetch from the index: request `(opts.k * 4).max(opts.k + 64)` candidates.
2. For each candidate, call `graph.get_node(hit.node)` and `graph.label_name(record.label)` to verify the label.
3. Collect the first `opts.k` survivors.

This over-fetch factor compensates for label distribution skew. Fewer than `opts.k` results may be returned when the index contains fewer matching
nodes. Do not error in this case; return whatever survivors were found.

## Testing Rules

Every test that touches vector behavior must cover all three of the following scenarios, each in its own test function:

1. **Persist and reload**: `upsert → search` in one `Graph` instance; then reopen the same path and `search` again. The same nearest neighbor must
   appear after reload.
2. **Dimension mismatch**: after the first `upsert` fixes dimensions, a second `upsert` with a different dimension count must return
   `Err(Error::Vector(...))`.
3. **Empty index**: `vector_search` on a graph with no vectors must return an empty `Vec`, not an error.

Each test must open its own `TempDir` and must not share a `Graph` instance with other tests.
