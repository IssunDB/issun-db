# Docs

Internal architecture and format documentation for IssunDB contributors.

| Document | Contents |
|---|---|
| [architecture.md](architecture.md) | Crate dependency graph, layer descriptions, data-flow diagrams for read and write paths |
| [storage-format.md](storage-format.md) | LMDB sub-database schema, key/value encodings, ID allocation, msgpack layout |
| [query-execution.md](query-execution.md) | Cypher pipeline: parsing, logical plan, optimization, physical plan, execution |
| [vector-search.md](vector-search.md) | HNSW lifecycle, `VectorIndexOptions`, metric and quantization trade-offs, persistence |
| [full-text-search.md](full-text-search.md) | Tokenization pipeline, BM25/TF-IDF scoring, WAND algorithm, boolean pre-filtering |

These documents are aimed at contributors and maintainers. For user-facing
documentation see the project `README.md` and the runnable `examples/`.
