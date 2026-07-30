## issundb-text

Full-text indexing and search for [IssunDB](https://github.com/IssunDB/issun-db).

This crate provides BM25 ranking, query evaluation, and the text search APIs.
Tokenization and the inverted index itself live in `issundb-core`, because the postings are maintained inside the same write transaction as the node
record; this crate tokenizes queries through core so that indexing and querying cannot disagree.
It is an internal crate; applications should use the [`issundb`](https://crates.io/crates/issundb) crate instead.

### License

MIT or Apache-2.0.
