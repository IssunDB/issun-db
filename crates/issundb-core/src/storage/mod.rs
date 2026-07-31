//! Storage backends and the contract they share.
//!
//! The engine is chosen at compile time from the `lmdb` feature, the same way
//! `issundb-vector` chooses its index: on by default it is LMDB, and with
//! `--no-default-features` it is the in-memory backend. A trait would have been the
//! other option and is deliberately not used, because the tables are held by
//! `Storage` and reached through `Graph`, so a trait would make `Graph` generic over
//! its backend and push that parameter through every crate and the public API. Two
//! modules exposing the same names cost nothing at runtime, keep `Graph` concrete,
//! and are held to the contract by the fact that the whole crate compiles and its
//! whole test suite passes against either.
//!
//! What a backend must supply: `Storage` (with the twelve tables as public fields and
//! an `env`), `RoTxn`, `OwnedRoTxn`, `RwTxn`, and `StorageError`. What the tables must
//! supply is `get`, `put`, `delete`, `len`, `iter`, `prefix_iter`, `get_duplicates`,
//! and `delete_one_duplicate`, plus the ordering and rollback guarantees documented
//! on `memory`, which are properties the layers above genuinely depend on rather than
//! incidental behaviour of LMDB.

pub mod fts;
pub mod ids;
pub mod props;

#[cfg(feature = "lmdb")]
pub mod lmdb;
#[cfg(not(feature = "lmdb"))]
pub mod memory;

#[cfg(feature = "lmdb")]
pub(crate) use lmdb::{OwnedRoTxn, RoTxn, RwTxn, Storage, StorageError};
#[cfg(not(feature = "lmdb"))]
pub(crate) use memory::{OwnedRoTxn, RoTxn, RwTxn, Storage, StorageError};
