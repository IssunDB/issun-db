## issundb-core

The storage engine and core data structures of [IssunDB](https://github.com/IssunDB/issun-db).

This crate owns graph storage on LMDB.
Things like node and edge records, adjacency, transactions, indexes, CSR snapshots, and the GraphBLAS-backed graph algorithms.
It is an internal crate; applications should depend on the [`issundb`](https://crates.io/crates/issundb) instead of directly depending on this crate.

### License

MIT or Apache-2.0.
