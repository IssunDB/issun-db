# Vector Search

IssunDB embeds an HNSW (Hierarchical Navigable Small World) vector index backed
by [usearch](https://github.com/unum-cloud/usearch).

## Index Lifecycle

```
Graph::open
  │
  ▼  cold start: load all (NodeId, bytes) pairs from LMDB `vectors` database
VectorIndex::upsert(node, v)  ← dimensions fixed on first call
  │
  ▼  in-memory HNSW (usearch Index)
VectorIndex::search(query, k)  → Vec<Hit { node, distance }>
```

On restart, all stored vectors are loaded from LMDB into a fresh in-memory HNSW
index before the first search. This cold start is O(n) in the number of vectors;
for very large indexes consider pre-warming at startup.

## Configuration: `VectorIndexOptions`

| Field | Type | Default | Notes |
|---|---|---|---|
| `metric` | `VectorMetric` | `Cosine` | Distance function |
| `quantization` | `VectorQuantization` | `F32` | Scalar precision |

### Distance Metrics (`VectorMetric`)

| Variant | usearch Kind | Use Case |
|---|---|---|
| `Cosine` | `MetricKind::Cos` | Normalized embeddings (text, images) |
| `L2` | `MetricKind::L2sq` | Geometric distance (squared L2 in usearch) |
| `Dot` | `MetricKind::IP` | Inner product / recommendation embeddings |
| `Hamming` | `MetricKind::Hamming` | Binary vectors |

### Scalar Quantization (`VectorQuantization`)

| Variant | Bytes per dim | Memory vs F32 | Recall impact |
|---|---|---|---|
| `F32` | 4 | 1× (baseline) | None |
| `F16` | 2 | 0.5× | Minimal (<1 % recall loss) |
| `I8` | 1 | 0.25× | Small (1–3 % recall loss) |
| `B1` | 1/8 | 0.03× | Significant; binary vectors only |

Dimensions are fixed when the index is first constructed (first `upsert` call).
All subsequent upserts must provide the same number of dimensions; mismatches
are rejected with `Error::Vector`.

## Persistence

Vectors are stored in LMDB as raw little-endian `f32` bytes under the `vectors`
sub-database. The in-memory HNSW index is derived from LMDB and is not
separately persisted; it is rebuilt on each `Graph::open` call.

## Label Filtering

`vector_search_with(query, opts)` supports filtering results by node label:
1. Over-fetch `k * 4 + 64` candidates from the HNSW index.
2. Discard candidates whose stored label does not match `opts.label`.
3. Return the first `k` survivors.

This is approximate: if fewer than `k` labeled nodes exist near the query, fewer
than `k` results are returned.

## Thread Safety

`VectorIndex` is protected by an internal `parking_lot::Mutex`. Searches may run
concurrently with other searches; upserts are serialized by the `Graph` write
lock, which must be held before calling `upsert_vector`.
