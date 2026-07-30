## issundb-core

The storage engine and core data structures of [IssunDB](https://github.com/IssunDB/issun-db).

This crate owns graph storage (including node and edge records, adjacency, transactions, indexes, CSR snapshots, and the graph algorithms).
The storage engine is selected at compile time, LMDB by default and an in-memory backend with `--no-default-features`, which is what lets the stack build for targets with no filesystem.
It is an internal crate; applications should depend on the [`issundb`](https://crates.io/crates/issundb) crate instead of directly depending on this crate.

### License

MIT or Apache-2.0.
