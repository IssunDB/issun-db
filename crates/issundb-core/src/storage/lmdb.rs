use std::path::Path;

use byteorder::BE;
use heed::{
    Database, DatabaseFlags, Env, EnvOpenOptions,
    types::{Bytes, Str, U64, Unit},
};

use crate::error::Error;

/// All LMDB sub-databases for IssunDB.
///
/// `out_adj` and `in_adj` use `DUPSORT + DUPFIXED`: each duplicate value is
/// one raw `AdjEntry` (20 bytes). A single `put` adds one entry in O(log n);
/// no read-modify-write of a blob is needed.
///
/// `label_idx` and `type_idx` use composite keys `(u32 BE, u64 BE)` = 12 bytes
/// for prefix-range scans by label or edge type.
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
    pub type_idx: Database<Bytes, Unit>,  // (TypeId, EdgeId) → ()

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
}

impl Storage {
    pub fn open(path: &Path, map_size_gb: usize) -> Result<Self, Error> {
        std::fs::create_dir_all(path)?;

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size_gb * 1024 * 1024 * 1024)
                .max_dbs(12)
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
        let type_idx = env.create_database(&mut wtxn, Some("type_idx"))?;
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
        let meta = env.create_database(&mut wtxn, Some("meta"))?;

        wtxn.commit()?;

        Ok(Self {
            env,
            nodes,
            edges,
            out_adj,
            in_adj,
            label_idx,
            type_idx,
            node_prop_idx,
            edge_prop_idx,
            fts_postings,
            fts_docs,
            vectors,
            meta,
        })
    }
}
