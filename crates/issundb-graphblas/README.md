## issundb-graphblas

A safe Rust wrapper over the SuiteSparse:GraphBLAS operations that [IssunDB](https://github.com/IssunDB/issun-db) uses for typed matrices and
vectors, build from triples, `mxv` over predefined semirings, and element-wise addition over predefined monoids.

This crate is built on top of the raw FFI bindings in [`issundb-graphblas-sys`](https://crates.io/crates/issundb-graphblas-sys).

### License

MIT or Apache-2.0.
(SuiteSparse:GraphBLAS itself is Apache-2.0.)
