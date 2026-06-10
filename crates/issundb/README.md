## IssunDB

An embedded graph database with vector and full-text search, written in Rust.

This is the main crate and the public facade of the IssunDB workspace.
It re-exports the stable API: the `Graph` type, Cypher query execution, vector search, full-text search, and hybrid retrieval.
Applications should depend on this crate rather than on the internal workspace crates.

See the [repository](https://github.com/IssunDB/issun-db) for documentation and examples.

### License

MIT or Apache-2.0.
