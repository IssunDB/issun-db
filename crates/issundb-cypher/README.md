## issundb-cypher

Cypher query language support for [IssunDB](https://github.com/IssunDB/issun-db), an embedded graph database written in Rust.

This crate owns the Cypher parser, AST, logical and physical planners, optimizer, and executor.
It is an internal crate; applications should depend on the [`issundb`](https://crates.io/crates/issundb) facade instead, which exposes querying
through the `GraphQueryExt` trait.

### License

MIT or Apache-2.0.
