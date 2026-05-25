## Project Roadmap

This document outlines the features implemented in IssunDB and the future goals for the project.

> [!IMPORTANT]
> This roadmap is a work in progress and is subject to change.

---

## Core Database Engine and Storage

High-performance, transactional, and schema-flexible embedded storage foundation.

- [x] On-disk key-value storage engine using Lightning Memory-Mapped Database (LMDB)
- [x] Zero-copy binary adjacency serialization with memory-mapped layouts
- [x] Monotonic identifier allocation and label/type registries
- [x] Flexible property storage with messagepack serialization
- [x] Thread-safe write serialization and lock-free concurrent reads
- [x] Unique and required property constraints on labels or types
- [x] Multi-step database transactions with atomic commits and rollbacks
- [ ] Native full-text index database storage for terms, postings, and tokenizer configurations

---

## Unified GraphBLAS Analytics

High-performance graph analysis executing mathematical operations directly on sparse adjacency matrices.

- [x] Thread-safe in-memory Compressed Sparse Row (CSR) snapshot cache
- [x] Dynamic, zero-overhead GraphBLAS matrix materialization triggered by database writes
- [x] SuiteSparse:GraphBLAS algorithm suite executing via sparse matrix-vector multiplication (SpMV) kernels:
  - [x] Breadth-first search (BFS) and multi-source BFS
  - [x] Directed PageRank power iterations
  - [x] Weighted shortest path using Dijkstra on a MinPlus semiring
  - [x] Weakly connected components (WCC) label propagation
  - [x] Strongly connected components (Kosaraju's algorithm)
  - [x] Degree, betweenness, and harmonic centrality measures
  - [x] Label-propagation community detection (CDLP)
  - [x] Minimum and maximum spanning forests
  - [x] Edmonds-Karp maximum flow
  - [x] Yen's top-k path search
  - [x] Longest path, cycle detection, and general DFS/all-paths traversals

---

## Advanced Retrieval and Vector Search

Unified hybrid retrieval combining exact graph patterns, dense vector spaces, and ranked full-text indexing.

- [x] Hierarchical Navigable Small World (HNSW) vector index integration using `usearch`
- [x] Vector database APIs for dense embedding search and dynamic index rebuilds
- [x] High-speed full-text indexing with ranked matches, BM25 scoring, and multi-language stemming
- [ ] Vector deletion API and persisted dimension/metric metadata
- [ ] Property-filtered vector search constraints
- [ ] Hybrid retrieval combining vector search, full-text search, and GraphBLAS graph expansions
- [ ] Retrieval score fusion, attribution scoring, and result limiters

---

## Cypher Query Language and Planner

Declarative graph querying with compile-time optimization and vector-matrix execution.

- [x] Hand-written recursive-descent Cypher parser for read, write, and schema manipulation patterns
- [x] Parameter binding and projection support
- [x] Cost-based logical query planner with label scanning, expansion, and filtering
- [x] Physical planner and optimization engine featuring filter pushdown and operator reordering
- [x] Unconditional GraphBLAS-accelerated Cypher pattern matching using vector-matrix multiplication
- [x] Variable-length path patterns, collection unwinding, and projection barriers
- [x] Result shaping with order, skip, limit, and aggregation functions
- [ ] Idempotent writes using the `MERGE` clause
- [ ] Cypher DDL for administrative index and constraint creation
- [ ] Query plan visualization for logical, physical, and optimized query paths
- [ ] Broad openCypher TCK conformance validation suite

---

## Ecosystem and Tooling

Developer experience, integrations, and operational tools.

- [x] Interactive Command Line Interface (CLI) REPL
- [x] Workspace benchmarking suite measuring throughput and load scaling
- [x] Property-based invariant testing and end-to-end integration verification
- [ ] High-performance language bindings for Python using PyO3
- [ ] High-performance language bindings for Node.js using NAPI-RS
- [ ] Batch data import utilities for JSONL and CSV formats
- [ ] Online backup, restore, and snapshot tools
