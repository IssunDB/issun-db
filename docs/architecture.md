# Architecture

IssunDB is an embedded graph database with vector and full-text search, written
in Rust. It is structured as a workspace of focused crates with strict dependency
boundaries.

## Crate Dependency Graph

```
issundb-cli  issundb-server  issundb-py  issundb-node  issundb-gui
     └──────────────────────────┬──────────────────────────┘
                                │
                          issundb  (public facade)
                                │
          ┌─────────────────────┼────────────────────┐
          │                     │                     │
   issundb-cypher       issundb-retrieval      issundb-vector
          │                 /       \                  │
          │    issundb-text           issundb-vector   │
          │         │                                  │
          └─────────┴──────── issundb-core ────────────┘
```

Dependency direction is strictly downward. Lower-level crates must not import
from higher-level crates.

## Layer Descriptions

### `issundb-core`: Storage Engine

All graph state lives here. The public surface is `Graph` and the schema types.

- **`Storage`** (`src/storage/lmdb.rs`): owns the LMDB environment and all twelve
  sub-databases. It is the only place where `heed::EnvOpenOptions` is called.
- **`Graph`** (`src/graph.rs`): the coordination type. All node, edge, adjacency,
  and FTS operations go through `Graph`; callers never touch `Storage` directly.
- **CSR snapshot** (`src/csr.rs`): an in-memory Compressed Sparse Row snapshot
  rebuilt in the background and swapped atomically via `arc-swap`. This is the
  hot read path for graph traversal.
- **GraphBLAS matrices** (`src/matrices.rs`): materialized from the CSR snapshot
  for graph algorithms (BFS, PageRank, SSSP).

Write serialization: a `parking_lot::Mutex<()>` write lock on `Graph` plus the
LMDB single-writer constraint together ensure that no two writes overlap.

### `issundb-cypher`: Query Layer

Parses, plans, optimizes, and executes openCypher queries against a `Graph`.

Pipeline: query string → `parser.rs` (AST) → `plan/logical.rs` (logical plan)
→ `plan/optimize.rs` (rewrites) → `plan/physical.rs` (physical plan) → `exec.rs`
(execution against the graph).

### `issundb-vector`: Vector Search

Owns the HNSW vector index (backed by usearch), vector metadata, and
persistence. The index is held in memory and rebuilt from LMDB on cold start.

### `issundb-text`: Full-Text Search

Owns tokenization (ASCII folding, Unicode segmentation, stop words, Snowball
stemming), BM25/TF-IDF scoring, WAND top-k retrieval, and boolean pre-filtering
via `RoaringTreemap`.

### `issundb-retrieval`: Hybrid Retrieval

Fuses graph traversal, vector nearest-neighbor hits, and full-text hits into a
ranked subgraph using configurable score fusion strategies.

### `issundb`: Public Facade

Re-exports the deliberate public surface from the layers above. Nothing internal
(`Storage`, LMDB types, planner internals) leaks through this boundary.

## Data Flow: Write Path

```
caller
  │  add_node / add_edge / upsert_vector / text_index
  ▼
Graph::with_write_lock
  │
  ├─ Storage::put_node / put_edge / put_adj_entry  (LMDB RwTxn)
  ├─ Storage::update_label_idx / type_idx           (LMDB secondary index)
  ├─ Storage::put_fts_postings                      (LMDB fts_postings)
  └─ VectorIndex::upsert                            (in-memory HNSW)
```

## Data Flow: Read Path

```
caller
  │  out_neighbors / bfs / vector_search / text_search
  ▼
Graph (no lock held for reads)
  │
  ├─ CSR snapshot (hot path for adjacency)
  ├─ LMDB RoTxn  (fallback or for non-adjacency reads)
  ├─ VectorIndex::search (in-memory HNSW)
  └─ fts_postings + WAND scoring (LMDB DUPSORT cursors)
```
