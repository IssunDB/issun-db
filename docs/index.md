# IssunDB Overview

IssunDB is an embedded graph database with vector and full-text search, written in Rust. It is designed to prioritize correct storage behavior,
high-performance graph traversal, hybrid retrieval, and clear boundaries between modular components.

## Core Features

- **ACID Transactions**: Fully transactional database storage engine built on top of Lightning Memory-Mapped Database (LMDB).
- **Adjacency Consistency**: Adjacency list storage utilizing LMDB DUPSORT keys to guarantee that outgoing and incoming traversal operations remain
  consistent, performant, and safe.
- **Hybrid Retrieval Primitives**: Combines graph traversal, vector index hits, full-text search hits, and property filters into fused query
  pipelines.
- **GraphBLAS Integration**: Employs high-performance sparse matrix and vector operations for structural graph algorithms, pattern matching, and
  traversal execution.
