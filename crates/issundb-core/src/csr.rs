use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use ahash::AHashMap;
use arc_swap::ArcSwap;

use crate::{
    error::Error,
    schema::{EdgeId, EdgeRecord, NodeId, TypeId},
    storage::{lmdb::Storage, props},
};

/// Minimum number of writes between two successive background rebuilds.
pub const REBUILD_THRESHOLD: u64 = 1_000;

/// Compressed Sparse Row snapshot of the outgoing adjacency.
///
/// `row_ptr[i]..row_ptr[i+1]` is the slice of `col_idx`, `edge_type`, and
/// `edge_id` for the node at dense index `i`. Built from LMDB in O(N + E) and
/// swapped atomically via `CsrCache`.
pub struct CsrSnapshot {
    pub row_ptr: Vec<usize>,
    pub col_idx: Vec<u32>,
    pub edge_type: Vec<TypeId>,
    pub edge_id: Vec<EdgeId>,
    pub edge_weight: Vec<f64>,
    pub dense_to_id: Vec<NodeId>,
    pub id_to_dense: AHashMap<NodeId, u32>,
}

impl CsrSnapshot {
    pub fn empty() -> Self {
        Self {
            row_ptr: vec![0],
            col_idx: vec![],
            edge_type: vec![],
            edge_id: vec![],
            edge_weight: vec![],
            dense_to_id: vec![],
            id_to_dense: AHashMap::new(),
        }
    }

    /// Scan `nodes` and `edges` sub-databases and build a fresh snapshot.
    pub fn build(storage: &Storage) -> Result<Self, Error> {
        let rtxn = storage.env.read_txn()?;

        // Enumerate all node IDs and assign contiguous dense indices.
        let mut dense_to_id: Vec<NodeId> = storage
            .nodes
            .iter(&rtxn)?
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()?;
        dense_to_id.sort_unstable();

        let n = dense_to_id.len();
        let id_to_dense: AHashMap<NodeId, u32> = dense_to_id
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as u32))
            .collect();

        // Bucket outgoing adjacency by dense source ID.
        let mut adj: Vec<Vec<(u32, TypeId, EdgeId, f64)>> = vec![vec![]; n];
        for result in storage.edges.iter(&rtxn)? {
            let (edge_id, bytes) = result?;
            let rec: EdgeRecord = props::decode(bytes)?;
            if let (Some(&src_d), Some(&dst_d)) =
                (id_to_dense.get(&rec.src), id_to_dense.get(&rec.dst))
            {
                let weight: f64 = {
                    let val: serde_json::Value =
                        props::decode(&rec.props).unwrap_or(serde_json::Value::Null);
                    val.get("weight")
                        .or_else(|| val.get("cost"))
                        .or_else(|| val.get("capacity"))
                        .or_else(|| val.get("cap"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0)
                };
                adj[src_d as usize].push((dst_d, rec.edge_type, edge_id, weight));
            }
        }

        // Compress into CSR arrays.
        let mut row_ptr = vec![0usize; n + 1];
        for (i, neighbors) in adj.iter().enumerate() {
            row_ptr[i + 1] = row_ptr[i] + neighbors.len();
        }
        let total = row_ptr[n];
        let mut col_idx = vec![0u32; total];
        let mut edge_type = vec![0u32; total];
        let mut edge_id_arr = vec![0u64; total];
        let mut edge_weight_arr = vec![0.0f64; total];
        for (i, neighbors) in adj.iter().enumerate() {
            let base = row_ptr[i];
            for (j, &(dst_d, etype, eid, weight)) in neighbors.iter().enumerate() {
                col_idx[base + j] = dst_d;
                edge_type[base + j] = etype;
                edge_id_arr[base + j] = eid;
                edge_weight_arr[base + j] = weight;
            }
        }

        Ok(Self {
            row_ptr,
            col_idx,
            edge_type,
            edge_id: edge_id_arr,
            edge_weight: edge_weight_arr,
            dense_to_id,
            id_to_dense,
        })
    }

    /// Returns `(other_node_id, edge_id, type_id)` for each outgoing edge of
    /// `node`, or `None` if `node` has no entry in this snapshot.
    pub fn out_neighbors(&self, node: NodeId) -> Option<Vec<(NodeId, EdgeId, TypeId)>> {
        let &dense = self.id_to_dense.get(&node)?;
        let start = self.row_ptr[dense as usize];
        let end = self.row_ptr[dense as usize + 1];
        Some(
            (start..end)
                .map(|i| {
                    (
                        self.dense_to_id[self.col_idx[i] as usize],
                        self.edge_id[i],
                        self.edge_type[i],
                    )
                })
                .collect(),
        )
    }
}

/// Thread-safe handle around a `CsrSnapshot` that supports atomic swaps and
/// background rebuilds triggered by a dirty-write threshold.
pub struct CsrCache {
    pub snapshot: ArcSwap<CsrSnapshot>,
    dirty: AtomicU64,
    rebuilding: AtomicBool,
}

impl CsrCache {
    pub fn new(initial: CsrSnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(initial),
            dirty: AtomicU64::new(0),
            rebuilding: AtomicBool::new(false),
        }
    }

    /// Increment the dirty counter. Returns `true` if this call crosses the
    /// rebuild threshold and no rebuild is already running; the caller must
    /// then perform the rebuild.
    pub fn mark_dirty(&self) -> bool {
        self.mark_dirty_n(1)
    }

    /// Increment the dirty counter by `count`. Returns `true` if this call crosses
    /// the rebuild threshold and no rebuild is already running; the caller must
    /// then perform the rebuild.
    pub fn mark_dirty_n(&self, count: u64) -> bool {
        let prev = self.dirty.fetch_add(count, Ordering::Relaxed);
        prev + count >= REBUILD_THRESHOLD && !self.rebuilding.swap(true, Ordering::AcqRel)
    }

    /// Install a freshly-built snapshot and reset the dirty counter and flag.
    pub fn install(&self, snap: CsrSnapshot) {
        self.snapshot.store(Arc::new(snap));
        self.dirty.store(0, Ordering::Release);
        self.rebuilding.store(false, Ordering::Release);
    }

    /// Release the rebuild claim without installing a snapshot; used when the
    /// build step fails so a future write can retry.
    pub fn cancel_rebuild(&self) {
        self.rebuilding.store(false, Ordering::Release);
    }
}
