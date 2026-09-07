use std::path::Path;

use byteorder::BE;
use heed::{
    Database, DatabaseFlags, Env, EnvOpenOptions,
    types::{Bytes, Str, U64, Unit},
};

use crate::error::Error;

/// All LMDB sub-databases for IssunDB.
///
/// `out_adj` and `in_adj` use `DUPSORT + DUPFIXED`, so each duplicate value is
/// one raw `AdjEntry` (20 bytes). A single `put` adds one entry in O(log n);
/// no read-modify-write of a blob is needed.
///
/// `label_idx` uses composite keys `(u32 BE, u64 BE)` = 12 bytes
/// for prefix-range scans by label.
pub struct Storage {
    pub env: Env,

    // Core records
    pub nodes: Database<U64<BE>, Bytes>, // node_id → msgpack NodeRecord
    pub edges: Database<U64<BE>, Bytes>, // edge_id → msgpack EdgeRecord

    // Adjacency: DUPSORT+DUPFIXED, one AdjEntry (20 B) per duplicate value
    pub out_adj: Database<U64<BE>, Bytes>, // node_id → [AdjEntry...]
    pub in_adj: Database<U64<BE>, Bytes>,  // node_id → [AdjEntry...]

    // Secondary indexes: composite key (u32 BE, u64 BE) → ()
    pub label_idx: Database<Bytes, Unit>, // (LabelId, NodeId) → ()

    // Property indexes
    pub node_prop_idx: Database<Bytes, Unit>,
    pub edge_prop_idx: Database<Bytes, Unit>,

    // Full-text search databases
    pub fts_postings: Database<Bytes, Bytes>, // composite key (LabelId, PropKeyId, term) → DUPSORT [NodeId BE, frequency BE]
    pub fts_docs: Database<Bytes, Bytes>,     // (LabelId, PropKeyId, NodeId) → doc_len u32 BE

    // Vector embeddings (usearch HNSW, added later)
    pub vectors: Database<U64<BE>, Bytes>, // node_id → raw f32 bytes

    // Metadata + counters
    pub meta: Database<Str, Bytes>, // string key → bytes

    /// Random 128-bit database identity, created once on first open and
    /// persisted in `meta`. The cache files record it, so a file built beside
    /// one database is refused by any other, even at a matching commit
    /// generation: the generation is a per-database counter, and two databases
    /// with equal commit counts are indistinguishable by it alone.
    pub db_id: [u8; 16],
}

impl Storage {
    /// Page-level size and entry count of every table, in declaration order.
    pub fn table_stats(&self, rtxn: &RoTxn) -> Result<Vec<crate::schema::TableStat>, Error> {
        fn stat<K: 'static, V: 'static>(
            name: &'static str,
            db: &Database<K, V>,
            rtxn: &RoTxn,
        ) -> Result<crate::schema::TableStat, Error> {
            let st = db.stat(rtxn)?;
            let pages = (st.branch_pages + st.leaf_pages + st.overflow_pages) as u64;
            Ok(crate::schema::TableStat {
                name,
                entries: st.entries as u64,
                bytes: pages * st.page_size as u64,
                pages: Some(pages),
            })
        }
        Ok(vec![
            stat("nodes", &self.nodes, rtxn)?,
            stat("edges", &self.edges, rtxn)?,
            stat("out_adj", &self.out_adj, rtxn)?,
            stat("in_adj", &self.in_adj, rtxn)?,
            stat("label_idx", &self.label_idx, rtxn)?,
            stat("node_prop_idx", &self.node_prop_idx, rtxn)?,
            stat("edge_prop_idx", &self.edge_prop_idx, rtxn)?,
            stat("fts_postings", &self.fts_postings, rtxn)?,
            stat("fts_docs", &self.fts_docs, rtxn)?,
            stat("vectors", &self.vectors, rtxn)?,
            stat("meta", &self.meta, rtxn)?,
        ])
    }
}

/// A read transaction as a *parameter*: what a function that only reads accepts.
///
/// Aliased here rather than named at each of the ~90 use sites, so the engine is
/// nameable in exactly one module. This is deliberately the thread-local-agnostic
/// flavour, because both concrete read transactions and a write transaction deref to
/// it, which is what lets a write path hand its `RwTxn` to a read helper. Naming a
/// specific TLS flavour here instead would break every one of those calls.
pub type RoTxn<'a> = heed::RoTxn<'a>;

/// A read transaction as an *owned value*, held across calls by `ReadTxn`.
///
/// Distinct from [`RoTxn`] because an owned transaction has to name whether it is
/// thread-local; `env.read_txn()` returns this flavour, and it derefs to `RoTxn`.
pub type OwnedRoTxn<'a> = heed::RoTxn<'a, heed::WithTls>;

/// A write transaction on the storage engine.
pub type RwTxn<'a> = heed::RwTxn<'a>;

/// The error type [`crate::error::Error::Storage`] carries for this backend.
pub type StorageError = heed::Error;

impl Storage {
    /// Copy the whole database to a single file, optionally compacting as it goes.
    ///
    /// Lives here so the compaction flag, which is an engine concept, is not named
    /// by the facade method that offers it.
    pub fn copy_to_file(&self, destination: &Path, compact: bool) -> Result<(), Error> {
        let option = if compact {
            heed::CompactionOption::Enabled
        } else {
            heed::CompactionOption::Disabled
        };
        self.env
            .copy_to_path(destination, option)
            .map(|_| ())
            .map_err(Error::Storage)
    }

    /// Copy a snapshot file into `dst_dir` as a database this backend can open.
    ///
    /// An associated function rather than a method: a restore happens before there is
    /// anything open to restore into.
    pub fn restore_from_file(snapshot_file: &Path, dst_dir: &Path) -> Result<(), Error> {
        let dst_file = dst_dir.join("data.mdb");
        // Refuse a destination that already holds a database. The copy below
        // truncates, so without this the call silently destroys whatever was there
        // and still reports success, including the caller's own open database.
        // The check lives here rather than in any one front end because every
        // caller reaches the same `fs::copy`: the CLI, the Python binding's
        // `restore`, and any library consumer of the facade.
        if dst_file.exists() {
            return Err(Error::InvalidArgument(format!(
                "{} already contains a database (data.mdb); restore into a new or \
                 empty directory rather than overwriting it",
                dst_dir.display()
            )));
        }
        std::fs::create_dir_all(dst_dir)?;
        // Leftover cache files describe whatever database used to live here,
        // not the one being restored. The database identity they carry already
        // gets them refused on load; removing them as well keeps the restored
        // directory from holding files that can never serve it.
        for entry in std::fs::read_dir(dst_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "cache") {
                std::fs::remove_file(&path)?;
            }
        }
        std::fs::copy(snapshot_file, &dst_file)?;
        Ok(())
    }

    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        std::fs::create_dir_all(path)?;

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size_gb * 1024 * 1024 * 1024)
                .max_dbs(11)
                .open(path)?
        };

        let mut wtxn = env.write_txn()?;

        let nodes = env.create_database(&mut wtxn, Some("nodes"))?;
        let edges = env.create_database(&mut wtxn, Some("edges"))?;

        let out_adj = env
            .database_options()
            .types::<U64<BE>, Bytes>()
            .name("out_adj")
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED)
            .create(&mut wtxn)?;

        let in_adj = env
            .database_options()
            .types::<U64<BE>, Bytes>()
            .name("in_adj")
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED)
            .create(&mut wtxn)?;

        let label_idx = env.create_database(&mut wtxn, Some("label_idx"))?;
        let node_prop_idx = env.create_database(&mut wtxn, Some("node_prop_idx"))?;
        let edge_prop_idx = env.create_database(&mut wtxn, Some("edge_prop_idx"))?;

        let fts_postings = env
            .database_options()
            .types::<Bytes, Bytes>()
            .name("fts_postings")
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED)
            .create(&mut wtxn)?;

        let fts_docs = env.create_database(&mut wtxn, Some("fts_docs"))?;

        let vectors = env.create_database(&mut wtxn, Some("vectors"))?;
        let meta: Database<Str, Bytes> = env.create_database(&mut wtxn, Some("meta"))?;

        let db_id = match meta.get(&wtxn, DB_ID_KEY)? {
            Some(bytes) => bytes
                .try_into()
                .map_err(|_| Error::Corrupt("db_id must be 16 bytes"))?,
            None => {
                let id = generate_db_id();
                meta.put(&mut wtxn, DB_ID_KEY, &id)?;
                id
            }
        };

        wtxn.commit()?;

        Ok(Self {
            env,
            nodes,
            edges,
            out_adj,
            in_adj,
            label_idx,
            node_prop_idx,
            edge_prop_idx,
            fts_postings,
            fts_docs,
            vectors,
            meta,
            db_id,
        })
    }
}

const DB_ID_KEY: &str = "db_id";

/// A fresh 128-bit database identity. It needs uniqueness across databases,
/// not cryptographic strength, so it mixes the process's randomly seeded
/// `RandomState` hashers with the clock instead of pulling in a randomness
/// dependency.
fn generate_db_id() -> [u8; 16] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut id = [0u8; 16];
    for (i, chunk) in id.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_usize(i);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    id
}

/// Write `entries`, sorted by key and then by value bytes, into a duplicate
/// table through one cursor. LMDB positions a cursor with a full descent from
/// the root on every `put`, but an already positioned cursor whose next key
/// lands on the same leaf skips the descent, so a sorted run through one
/// cursor touches each leaf page once. The bulk adjacency flush is the caller.
pub(crate) fn put_sorted_duplicates(
    db: &Database<U64<BE>, Bytes>,
    txn: &mut RwTxn<'_>,
    entries: &[(u64, &[u8])],
) -> Result<(), Error> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut cursor = db.iter_mut(txn)?;
    for (key, value) in entries {
        // SAFETY: no value borrowed from this database is held across the
        // call; `key` and `value` are the caller's own buffers.
        unsafe {
            cursor.put_current_with_options::<Bytes>(heed::PutFlags::empty(), key, value)?;
        }
    }
    Ok(())
}
