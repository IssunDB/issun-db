//! In-memory storage backend.
//!
//! The second implementor of the contract in [`crate::storage`], selected when the
//! `lmdb` feature is off. It exists for two reasons: it is what makes the storage
//! seam a real boundary rather than a rename (a contract with one implementor is
//! only a guess at a contract), and it is the only backend that compiles for a
//! target with no libc, since LMDB is C and is a memory-mapped file.
//!
//! It is deliberately faithful to the properties the engine above it depends on,
//! not merely to LMDB's function signatures. Three of those properties are load
//! bearing and easy to get wrong:
//!
//! - Key order is byte order: LMDB's default comparator is `memcmp`, so every
//!   table here is a `BTreeMap<Vec<u8>, _>` and keys are stored already encoded. A
//!   `u64` key encodes big-endian precisely so that byte order and numeric order
//!   agree, which is what lets `CsrSnapshot::build` assume `out_adj` arrives grouped
//!   by ascending node id.
//! - Duplicate order is byte order too: `DUPSORT` sorts the values under one key
//!   by `memcmp`, which is why the duplicates live in a `BTreeSet<Vec<u8>>`. The CSR
//!   builder depends on this exact ordering: it reorders each row by edge id
//!   *because* `AdjEntry`'s byte layout puts `edge_type` first, and that reasoning is
//!   only correct if duplicates arrive in raw-byte order here as well.
//! - An aborted write leaves nothing behind: a dropped `RwTxn` must roll back,
//!   because the engine relies on that for consistency: a failed mutation aborts
//!   mid-transaction and expects storage untouched. A writer therefore mutates a
//!   private copy and publishes it only at `commit`, so an abort is the absence of a
//!   publish rather than an undo log to keep correct. See [`Env`] for why the
//!   copy-on-write shape is also what makes a read transaction opened during a write
//!   legal, which the engine requires.
//!
//! What it does not do is persist. `open` ignores its path and starts empty, so a
//! reopen sees an empty graph. That is honest for its purpose rather than a stub:
//! every read and write path, index, and algorithm works against it, which is what
//! lets the whole test suite run on this backend and hold the seam to something.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::{Mutex, MutexGuard};

use crate::error::Error;

/// Failure modes of the in-memory backend.
///
/// Deliberately small: most of what can go wrong with a real engine (I/O, a full
/// map, a corrupt page) cannot happen here, and inventing variants for those would
/// suggest this backend can report conditions it never encounters.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// A stored key could not be decoded to its declared type, which means a writer
    /// and a reader disagree about a table's key type.
    #[error("in-memory storage: malformed {0} key")]
    MalformedKey(&'static str),
    /// An operation the in-memory backend cannot offer at all.
    #[error("in-memory storage: {0} is not supported without a persistent backend")]
    Unsupported(&'static str),
}

/// The error type [`crate::error::Error::Storage`] carries for this backend.
pub type StorageError = MemoryError;

/// One table: encoded key to the set of encoded values stored under it.
///
/// A non-duplicate table keeps at most one value per key; the shared shape avoids a
/// second code path whose ordering could drift from the duplicate one.
type TableData = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

/// How many tables a `Storage` holds; see the index constants below.
const TABLE_COUNT: usize = 12;

// Table indices. A `Table` handle is just one of these plus its type parameters, so
// it stays `Copy` exactly as a `heed::Database` handle is.
const T_NODES: usize = 0;
const T_EDGES: usize = 1;
const T_OUT_ADJ: usize = 2;
const T_IN_ADJ: usize = 3;
const T_LABEL_IDX: usize = 4;
const T_TYPE_IDX: usize = 5;
const T_NODE_PROP_IDX: usize = 6;
const T_EDGE_PROP_IDX: usize = 7;
const T_FTS_POSTINGS: usize = 8;
const T_FTS_DOCS: usize = 9;
const T_VECTORS: usize = 10;
const T_META: usize = 11;

/// A key type a table can be declared over.
///
/// `encode` returns `Cow` because a `u64` key has to materialize its big-endian
/// bytes while a byte-slice key is already in its stored form.
pub trait Key {
    /// What iteration and lookup hand back for this key type.
    type Out<'a>;
    fn encode(&self) -> Cow<'_, [u8]>;
    fn decode(bytes: &[u8]) -> Result<Self::Out<'_>, MemoryError>;
}

impl Key for u64 {
    type Out<'a> = u64;

    fn encode(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.to_be_bytes().to_vec())
    }

    fn decode(bytes: &[u8]) -> Result<u64, MemoryError> {
        let array: [u8; 8] = bytes
            .try_into()
            .map_err(|_| MemoryError::MalformedKey("u64"))?;
        Ok(u64::from_be_bytes(array))
    }
}

impl Key for [u8] {
    type Out<'a> = &'a [u8];

    fn encode(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self)
    }

    fn decode(bytes: &[u8]) -> Result<&[u8], MemoryError> {
        Ok(bytes)
    }
}

impl Key for str {
    type Out<'a> = &'a str;

    fn encode(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<&str, MemoryError> {
        std::str::from_utf8(bytes).map_err(|_| MemoryError::MalformedKey("str"))
    }
}

/// Marks a handle as owning neither its key nor its value type, and as invariant in
/// both, so a `Table<u64, _>` can never be coerced into a `Table<str, _>`.
type TypeMarker<K, V> = PhantomData<(fn() -> K, fn() -> V)>;

/// A value type a table can be declared over.
pub trait Val {
    /// What iteration and lookup hand back for this value type.
    type Out<'a>;
    fn encode(&self) -> &[u8];
    fn decode(bytes: &[u8]) -> Self::Out<'_>;
}

impl Val for [u8] {
    type Out<'a> = &'a [u8];

    fn encode(&self) -> &[u8] {
        self
    }

    fn decode(bytes: &[u8]) -> &[u8] {
        bytes
    }
}

impl Val for () {
    type Out<'a> = ();

    fn encode(&self) -> &[u8] {
        &[]
    }

    fn decode(_: &[u8]) {}
}

/// The storage environment: the published tables, plus the lock that admits one
/// writer at a time.
///
/// The concurrency model is copy-on-write, chosen to match the two properties the
/// engine above actually relies on rather than to be the simplest thing that stores
/// bytes:
///
/// - A reader is never blocked and never sees a partial write: `read_txn` loads
///   the published table set and holds it; a writer builds its own copy and publishes
///   it atomically at commit. That is what makes a read transaction opened *while* a
///   write transaction is live legal, and the engine depends on it: a write statement
///   such as `MATCH ... CREATE` runs its match against committed state through a
///   separate read transaction while its own write transaction is still open. A
///   single reader-writer lock deadlocks on exactly that, which is how this design
///   was arrived at.
/// - One writer at a time, as LMDB enforces, via `writer`.
///
/// Rollback falls out for free: an uncommitted transaction simply never publishes its
/// copy, so there is no undo log to keep correct.
///
/// The cost is honest to state: the first mutation of a table within a transaction
/// clones that table, so a single-row write to a large table is `O(table)` rather than
/// `O(log n)`. That is the wrong trade for a persistent engine and an acceptable one
/// here, where the point is portability and testability rather than throughput.
#[derive(Clone)]
pub struct Env {
    published: Arc<ArcSwap<Tables>>,
    writer: Arc<Mutex<()>>,
}

/// One table set. Each table is shared behind an `Arc` so a writer clones only the
/// tables it touches.
type Tables = Vec<Arc<TableData>>;

impl Env {
    fn new() -> Self {
        Self {
            published: Arc::new(ArcSwap::from_pointee(
                (0..TABLE_COUNT)
                    .map(|_| Arc::new(TableData::new()))
                    .collect(),
            )),
            writer: Arc::new(Mutex::new(())),
        }
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_>, Error> {
        Ok(RoTxn {
            tables: self.published.load_full(),
            marker: PhantomData,
        })
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, Error> {
        let guard = self.writer.lock();
        // The working copy: twelve `Arc` clones, so nothing is deep-copied until a
        // table is actually mutated.
        let working = Arc::new((*self.published.load_full()).clone());
        Ok(RwTxn {
            inner: RoTxn {
                tables: working,
                marker: PhantomData,
            },
            published: Arc::clone(&self.published),
            _guard: guard,
        })
    }
}

/// A read transaction: one published table set, held for the transaction's life.
///
/// Also the read *view* of a write transaction: [`RwTxn`] contains one of these and
/// derefs to it, so every read method below accepts either, which is what lets a write
/// path pass its own transaction to a read helper and see its uncommitted writes. The
/// LMDB backend gets that property from heed's deref chain, and the ~90 signatures
/// written against `&RoTxn` rely on it under both backends.
pub struct RoTxn<'e> {
    tables: Arc<Tables>,
    /// Ties the transaction to its environment's lifetime, as the LMDB backend's does,
    /// so the two backends' signatures agree.
    marker: PhantomData<&'e ()>,
}

impl RoTxn<'_> {
    fn table_data(&self, index: usize) -> &TableData {
        &self.tables[index]
    }
}

/// A read transaction held as an owned value; see the LMDB backend's alias of the
/// same name. This backend needs no distinct type for it.
pub type OwnedRoTxn<'e> = RoTxn<'e>;

/// A write transaction: a private working copy plus the writer lock.
pub struct RwTxn<'e> {
    inner: RoTxn<'e>,
    published: Arc<ArcSwap<Tables>>,
    /// Held for the transaction's life so only one writer runs at a time.
    _guard: MutexGuard<'e, ()>,
}

impl<'e> std::ops::Deref for RwTxn<'e> {
    type Target = RoTxn<'e>;

    fn deref(&self) -> &RoTxn<'e> {
        &self.inner
    }
}

impl RwTxn<'_> {
    /// Publish the working copy, making every write in this transaction visible at
    /// once. A reader either sees all of them or none.
    pub fn commit(self) -> Result<(), Error> {
        self.published.store(Arc::clone(&self.inner.tables));
        Ok(())
    }

    /// Discard the transaction's writes.
    ///
    /// Dropping the working copy *is* the rollback: nothing was published, so storage
    /// still holds what it held before. An explicit abort and a plain drop therefore
    /// cannot diverge.
    pub fn abort(self) {}

    /// The working copy of one table, cloned on first touch in this transaction.
    fn table_mut(&mut self, index: usize) -> &mut TableData {
        // The outer `Arc` is unique to this transaction, so this hands back the
        // working vector without copying it.
        let tables = Arc::make_mut(&mut self.inner.tables);
        // The inner `Arc` is still shared with the published set on first touch, so
        // this is where the copy-on-write clone happens; later mutations of the same
        // table in this transaction find it unique and clone nothing.
        Arc::make_mut(&mut tables[index])
    }
}

/// A handle to one table, typed by its key and value.
///
/// `Copy` and free of borrows, like a `heed::Database`, so `Storage` can hand the
/// same handle to every caller.
pub struct Table<K: ?Sized, V: ?Sized> {
    index: usize,
    /// Whether several values may live under one key. Only the adjacency tables and
    /// the FTS postings are duplicate tables; for the rest a `put` replaces.
    duplicates: bool,
    marker: TypeMarker<K, V>,
}

impl<K: ?Sized, V: ?Sized> Clone for Table<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: ?Sized, V: ?Sized> Copy for Table<K, V> {}

impl<K: ?Sized, V: ?Sized> Table<K, V> {
    const fn new(index: usize, duplicates: bool) -> Self {
        Self {
            index,
            duplicates,
            marker: PhantomData,
        }
    }
}

impl<K: Key + ?Sized, V: Val + ?Sized> Table<K, V> {
    /// The value stored under `key`, or the first of them in a duplicate table.
    pub fn get<'t>(&self, txn: &'t RoTxn<'_>, key: &K) -> Result<Option<V::Out<'t>>, Error> {
        let encoded = key.encode();
        Ok(txn
            .table_data(self.index)
            .get(encoded.as_ref())
            .and_then(|values| values.first())
            .map(|value| V::decode(value)))
    }

    /// Number of entries, counting each duplicate separately as LMDB does.
    pub fn len(&self, txn: &RoTxn<'_>) -> Result<u64, Error> {
        Ok(txn
            .table_data(self.index)
            .values()
            .map(|values| values.len() as u64)
            .sum())
    }

    /// Every entry in key order, then value order within a key.
    pub fn iter<'t>(&self, txn: &'t RoTxn<'_>) -> Result<TableIter<'t, K, V>, Error> {
        Ok(TableIter::new(
            txn.table_data(self.index)
                .iter()
                .flat_map(|(k, values)| values.iter().map(move |v| (k.as_slice(), v.as_slice())))
                .collect(),
        ))
    }

    /// Every entry whose key starts with `prefix`, in the same order as `iter`.
    pub fn prefix_iter<'t>(
        &self,
        txn: &'t RoTxn<'_>,
        prefix: &K,
    ) -> Result<TableIter<'t, K, V>, Error> {
        let encoded = prefix.encode().into_owned();
        Ok(TableIter::new(
            txn.table_data(self.index)
                .range(encoded.clone()..)
                .take_while(move |(k, _)| k.starts_with(&encoded))
                .flat_map(|(k, values)| values.iter().map(move |v| (k.as_slice(), v.as_slice())))
                .collect(),
        ))
    }

    /// Every value stored under `key`, or `None` when the key is absent.
    ///
    /// `None` rather than an empty iterator, because the engine distinguishes the
    /// two: a node with no adjacency entries has no key at all.
    pub fn get_duplicates<'t>(
        &self,
        txn: &'t RoTxn<'_>,
        key: &K,
    ) -> Result<Option<TableIter<'t, K, V>>, Error> {
        let encoded = key.encode();
        // `get_key_value` rather than `get`, so the key is re-borrowed from the map:
        // the yielded pairs then live as long as the transaction rather than as long
        // as the caller's key argument, which is what lets a caller hold the iterator.
        let Some((stored_key, values)) = txn.table_data(self.index).get_key_value(encoded.as_ref())
        else {
            return Ok(None);
        };
        if values.is_empty() {
            return Ok(None);
        }
        Ok(Some(TableIter::new(
            values
                .iter()
                .map(|v| (stored_key.as_slice(), v.as_slice()))
                .collect(),
        )))
    }

    /// Store `value` under `key`, replacing in a normal table and adding in a
    /// duplicate one.
    pub fn put(&self, txn: &mut RwTxn<'_>, key: &K, value: &V) -> Result<(), Error> {
        let encoded = key.encode();
        let entry = txn
            .table_mut(self.index)
            .entry(encoded.into_owned())
            .or_default();
        if !self.duplicates {
            entry.clear();
        }
        entry.insert(value.encode().to_vec());
        Ok(())
    }

    /// Remove `key` and every value under it. True when something was removed.
    pub fn delete(&self, txn: &mut RwTxn<'_>, key: &K) -> Result<bool, Error> {
        let encoded = key.encode();
        Ok(txn.table_mut(self.index).remove(encoded.as_ref()).is_some())
    }

    /// Remove one specific value from a duplicate key, leaving its siblings. The key
    /// itself goes when its last value does, so an empty key never lingers to make
    /// `get_duplicates` report a node that has no entries.
    pub fn delete_one_duplicate(
        &self,
        txn: &mut RwTxn<'_>,
        key: &K,
        value: &V,
    ) -> Result<bool, Error> {
        let encoded = key.encode();
        let table = txn.table_mut(self.index);
        let Some(values) = table.get_mut(encoded.as_ref()) else {
            return Ok(false);
        };
        let removed = values.remove(value.encode());
        if values.is_empty() {
            table.remove(encoded.as_ref());
        }
        Ok(removed)
    }
}

/// Iterator over a table's entries, decoding each pair on demand.
///
/// The pairs are collected as borrowed slices up front. That is a real cost against
/// LMDB's cursor, and it is what keeps the iterator independent of the map it came
/// from so a caller can hold it across other reads, which several call sites do.
pub struct TableIter<'t, K: ?Sized, V: ?Sized> {
    pairs: std::vec::IntoIter<(&'t [u8], &'t [u8])>,
    marker: TypeMarker<K, V>,
}

impl<'t, K: ?Sized, V: ?Sized> TableIter<'t, K, V> {
    fn new(pairs: Vec<(&'t [u8], &'t [u8])>) -> Self {
        Self {
            pairs: pairs.into_iter(),
            marker: PhantomData,
        }
    }
}

impl<'t, K: Key + ?Sized, V: Val + ?Sized> Iterator for TableIter<'t, K, V> {
    type Item = Result<(K::Out<'t>, V::Out<'t>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let (key, value) = self.pairs.next()?;
        Some(match K::decode(key) {
            Ok(decoded) => Ok((decoded, V::decode(value))),
            Err(err) => Err(Error::Storage(err)),
        })
    }
}

/// All tables for IssunDB, held in memory.
///
/// Mirrors the field names and types of the LMDB backend's `Storage`, because the
/// rest of the crate is written against those names and is compiled against exactly
/// one of the two.
pub struct Storage {
    pub env: Env,

    pub nodes: Table<u64, [u8]>,
    pub edges: Table<u64, [u8]>,

    pub out_adj: Table<u64, [u8]>,
    pub in_adj: Table<u64, [u8]>,

    pub label_idx: Table<[u8], ()>,
    pub type_idx: Table<[u8], ()>,

    pub node_prop_idx: Table<[u8], ()>,
    pub edge_prop_idx: Table<[u8], ()>,

    pub fts_postings: Table<[u8], [u8]>,
    pub fts_docs: Table<[u8], [u8]>,

    pub vectors: Table<u64, [u8]>,

    pub meta: Table<str, [u8]>,
}

impl Storage {
    /// Open a fresh in-memory database.
    ///
    /// `path` and `map_size_gb` are accepted so the signature matches the persistent
    /// backend, and ignored: there is no file to create and no map to size. A caller
    /// therefore gets an empty graph every time, which is the one behavioural
    /// difference that matters and is stated on the module.
    pub fn open(_path: &Path, _map_size_gb: usize) -> Result<Self, Error> {
        Ok(Self {
            env: Env::new(),
            nodes: Table::new(T_NODES, false),
            edges: Table::new(T_EDGES, false),
            out_adj: Table::new(T_OUT_ADJ, true),
            in_adj: Table::new(T_IN_ADJ, true),
            label_idx: Table::new(T_LABEL_IDX, false),
            type_idx: Table::new(T_TYPE_IDX, false),
            node_prop_idx: Table::new(T_NODE_PROP_IDX, false),
            edge_prop_idx: Table::new(T_EDGE_PROP_IDX, false),
            fts_postings: Table::new(T_FTS_POSTINGS, true),
            fts_docs: Table::new(T_FTS_DOCS, false),
            vectors: Table::new(T_VECTORS, false),
            meta: Table::new(T_META, false),
        })
    }

    /// Unsupported: a backup is a copy of a file, and this backend has none.
    pub fn copy_to_file(&self, _destination: &Path, _compact: bool) -> Result<(), Error> {
        Err(Error::Storage(MemoryError::Unsupported("backup")))
    }

    /// Unsupported, for the same reason as [`Storage::copy_to_file`]. Reporting
    /// success here would be worse than refusing: a caller would restore a snapshot,
    /// see no error, open the directory, and find an empty graph.
    pub fn restore_from_file(_snapshot_file: &Path, _dst_dir: &Path) -> Result<(), Error> {
        Err(Error::Storage(MemoryError::Unsupported("restore")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::open(Path::new("unused"), 1).expect("in-memory open cannot fail")
    }

    /// Keys must come back in byte order, which for a big-endian `u64` is numeric
    /// order. The CSR builder assumes exactly this when it treats `out_adj` as
    /// already grouped by ascending node id.
    #[test]
    fn u64_keys_iterate_in_numeric_order() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        for id in [300u64, 1, 256, 2] {
            s.nodes.put(&mut wtxn, &id, b"x".as_slice()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        let keys: Vec<u64> = s.nodes.iter(&rtxn).unwrap().map(|r| r.unwrap().0).collect();
        assert_eq!(keys, vec![1, 2, 256, 300]);
    }

    /// A duplicate table keeps every value under one key, ordered by raw bytes, and
    /// counts each separately. The CSR builder's row-reordering pass is only correct
    /// if this ordering is by bytes rather than by insertion.
    #[test]
    fn duplicates_are_kept_and_ordered_by_bytes() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        for value in [b"\x02".as_slice(), b"\x00", b"\x01"] {
            s.out_adj.put(&mut wtxn, &7u64, value).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        let values: Vec<Vec<u8>> = s
            .out_adj
            .get_duplicates(&rtxn, &7u64)
            .unwrap()
            .expect("key present")
            .map(|r| r.unwrap().1.to_vec())
            .collect();
        assert_eq!(values, vec![vec![0], vec![1], vec![2]]);
        assert_eq!(s.out_adj.len(&rtxn).unwrap(), 3, "each duplicate counts");
    }

    /// A non-duplicate table replaces instead of accumulating.
    #[test]
    fn a_normal_table_replaces_on_put() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        s.nodes.put(&mut wtxn, &1u64, b"first".as_slice()).unwrap();
        s.nodes.put(&mut wtxn, &1u64, b"second".as_slice()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        assert_eq!(
            s.nodes.get(&rtxn, &1u64).unwrap(),
            Some(b"second".as_slice())
        );
        assert_eq!(s.nodes.len(&rtxn).unwrap(), 1);
    }

    /// Removing one duplicate leaves its siblings, and removing the last one takes
    /// the key with it so `get_duplicates` stops reporting the node.
    #[test]
    fn deleting_one_duplicate_leaves_siblings_then_the_key() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        s.out_adj.put(&mut wtxn, &1u64, b"a".as_slice()).unwrap();
        s.out_adj.put(&mut wtxn, &1u64, b"b".as_slice()).unwrap();
        assert!(
            s.out_adj
                .delete_one_duplicate(&mut wtxn, &1u64, b"a".as_slice())
                .unwrap()
        );
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        assert_eq!(s.out_adj.len(&rtxn).unwrap(), 1);
        drop(rtxn);

        let mut wtxn = s.env.write_txn().unwrap();
        s.out_adj
            .delete_one_duplicate(&mut wtxn, &1u64, b"b".as_slice())
            .unwrap();
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        assert!(
            s.out_adj.get_duplicates(&rtxn, &1u64).unwrap().is_none(),
            "the key must go with its last value"
        );
    }

    /// A dropped transaction must leave storage exactly as it was. The engine aborts
    /// mid-transaction on any error and depends on this.
    #[test]
    fn dropping_a_write_txn_rolls_everything_back() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        s.nodes.put(&mut wtxn, &1u64, b"kept".as_slice()).unwrap();
        s.out_adj.put(&mut wtxn, &1u64, b"kept".as_slice()).unwrap();
        wtxn.commit().unwrap();

        let mut wtxn = s.env.write_txn().unwrap();
        s.nodes
            .put(&mut wtxn, &1u64, b"clobbered".as_slice())
            .unwrap();
        s.nodes.put(&mut wtxn, &2u64, b"added".as_slice()).unwrap();
        s.nodes.delete(&mut wtxn, &1u64).unwrap();
        s.out_adj
            .put(&mut wtxn, &1u64, b"extra".as_slice())
            .unwrap();
        drop(wtxn);

        let rtxn = s.env.read_txn().unwrap();
        assert_eq!(
            s.nodes.get(&rtxn, &1u64).unwrap(),
            Some(b"kept".as_slice()),
            "a key mutated twice returns to its pre-transaction value"
        );
        assert_eq!(s.nodes.get(&rtxn, &2u64).unwrap(), None);
        assert_eq!(s.out_adj.len(&rtxn).unwrap(), 1);
    }

    /// A prefix scan sees exactly the keys sharing that prefix, which is what the
    /// label and type indexes are built on.
    #[test]
    fn prefix_iter_bounds_the_scan() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        for key in [
            [0u8, 0, 0, 1, 9].as_slice(),
            &[0, 0, 0, 1, 10],
            &[0, 0, 0, 2, 11],
        ] {
            s.label_idx.put(&mut wtxn, key, &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = s.env.read_txn().unwrap();
        let found: Vec<Vec<u8>> = s
            .label_idx
            .prefix_iter(&rtxn, &[0, 0, 0, 1])
            .unwrap()
            .map(|r| r.unwrap().0.to_vec())
            .collect();
        assert_eq!(found, vec![vec![0, 0, 0, 1, 9], vec![0, 0, 0, 1, 10]]);
    }

    /// A write transaction can read its own uncommitted writes, which the engine
    /// relies on throughout: a mutation reads back the record it just wrote.
    #[test]
    fn a_write_txn_reads_its_own_writes() {
        let s = storage();
        let mut wtxn = s.env.write_txn().unwrap();
        s.meta.put(&mut wtxn, "k", b"v".as_slice()).unwrap();
        assert_eq!(s.meta.get(&wtxn, "k").unwrap(), Some(b"v".as_slice()));
        wtxn.commit().unwrap();
    }
}
