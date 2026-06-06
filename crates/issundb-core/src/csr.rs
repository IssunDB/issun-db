use std::path::Path;
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

// ---------------------------------------------------------------------------
// Binary layout for the persisted CSR file
// ---------------------------------------------------------------------------
//
//   [0..8]           magic    u64 LE  = 0x49535355_4E435352  ("ISSUNCSR")
//   [8..16]          n_nodes  u64 LE
//   [16..24]         n_edges  u64 LE
//   [24 .. 24 + (n+1)*8]     row_ptr  u64 LE  (usize stored as u64)
//   [.. + n_e*4]             col_idx  u32 LE
//   [.. + n_e*4]             edge_type u32 LE
//   [.. + n_e*8]             edge_id  u64 LE
//   [.. + n_e*8]             edge_weight f64 LE
//   [.. + n*8]               dense_to_id u64 LE

const MAGIC: u64 = 0x4953_5355_4E43_5352;

/// Compressed Sparse Row snapshot of the outgoing adjacency.
pub struct CsrSnapshot {
    /// `row_ptr[i]..row_ptr[i+1]` is the range of the i-th node's edges.
    pub row_ptr: Vec<usize>,
    pub col_idx: Vec<u32>,
    pub edge_type: Vec<TypeId>,
    pub edge_id: Vec<EdgeId>,
    pub edge_weight: Vec<f64>,
    pub dense_to_id: Vec<NodeId>,
    pub id_to_dense: AHashMap<NodeId, u32>,
}

impl CsrSnapshot {
    #[cfg(test)]
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

    /// Scan `nodes` and `edges` sub-databases and build a fresh in-RAM snapshot.
    pub fn build(storage: &Storage) -> Result<Self, Error> {
        let rtxn = storage.env.read_txn()?;

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

    /// Build a snapshot and serialize it to `path` in the binary layout
    /// documented above, so external tooling can inspect a snapshot without
    /// opening LMDB. The returned snapshot always owns its arrays in RAM; a
    /// failed write is ignored and the snapshot is returned unchanged.
    pub fn build_persisted(storage: &Storage, path: &Path) -> Result<Self, Error> {
        let snap = Self::build(storage)?;
        let _ = snap.persist(path);
        Ok(snap)
    }

    /// Serialize the snapshot arrays to `path` through a buffered writer.
    fn persist(&self, path: &Path) -> Result<(), Error> {
        use std::io::Write;

        let n = self.dense_to_id.len();
        let n_edges = self.col_idx.len();

        let file = std::fs::File::create(path).map_err(Error::from)?;
        let mut out = std::io::BufWriter::new(file);

        // Header.
        out.write_all(&MAGIC.to_le_bytes()).map_err(Error::from)?;
        out.write_all(&(n as u64).to_le_bytes())
            .map_err(Error::from)?;
        out.write_all(&(n_edges as u64).to_le_bytes())
            .map_err(Error::from)?;

        // row_ptr: (n+1) u64 values.
        for &v in &self.row_ptr {
            out.write_all(&(v as u64).to_le_bytes())
                .map_err(Error::from)?;
        }
        // col_idx: n_edges u32.
        for &v in &self.col_idx {
            out.write_all(&v.to_le_bytes()).map_err(Error::from)?;
        }
        // edge_type: n_edges u32.
        for &v in &self.edge_type {
            out.write_all(&v.to_le_bytes()).map_err(Error::from)?;
        }
        // edge_id: n_edges u64.
        for &v in &self.edge_id {
            out.write_all(&v.to_le_bytes()).map_err(Error::from)?;
        }
        // edge_weight: n_edges f64.
        for &v in &self.edge_weight {
            out.write_all(&v.to_le_bytes()).map_err(Error::from)?;
        }
        // dense_to_id: n u64.
        for &v in &self.dense_to_id {
            out.write_all(&v.to_le_bytes()).map_err(Error::from)?;
        }
        out.flush().map_err(Error::from)?;
        Ok(())
    }
}

/// Mutations accumulated since the matrices were last refreshed, sufficient to
/// update the GraphBLAS matrices incrementally instead of rebuilding them from a
/// full LMDB scan. Recorded post-commit on the write path (so an aborted
/// transaction never pollutes it) and drained by the matrix-refresh path.
///
/// `added_edges` and `removed_edges` carry the source and destination node ids;
/// the combined adjacency matrices are a boolean union, so the matrix-refresh
/// path resolves parallel edges against LMDB before deciding to clear a bit.
/// Node deletion reshuffles the sorted dense-index mapping, so it sets
/// `force_full` to fall back to a full rebuild rather than an incremental patch.
#[derive(Default)]
pub struct GraphDelta {
    pub added_nodes: Vec<NodeId>,
    pub added_edges: Vec<(NodeId, NodeId)>,
    pub removed_edges: Vec<(NodeId, NodeId)>,
    pub force_full: bool,
}

impl GraphDelta {
    /// True when there is nothing to apply: no structural change and no forced
    /// full rebuild pending.
    pub fn is_empty(&self) -> bool {
        !self.force_full
            && self.added_nodes.is_empty()
            && self.added_edges.is_empty()
            && self.removed_edges.is_empty()
    }
}

/// Thread-safe handle around a `CsrSnapshot` that supports atomic swaps and
/// background rebuilds triggered by a dirty-write threshold.
pub struct CsrCache {
    pub snapshot: ArcSwap<CsrSnapshot>,
    dirty: AtomicU64,
    rebuilding: AtomicBool,
    /// The dirty count captured when the in-flight rebuild was claimed. On
    /// install this much is subtracted from `dirty` rather than zeroing it, so
    /// writes that committed while the rebuild ran are not lost.
    claimed: AtomicU64,
    /// Structural mutations accumulated since the last matrix refresh. Writers
    /// serialize on the `Graph` write lock, so contention here is only between a
    /// writer recording a mutation and the refresh path draining it.
    pending: parking_lot::Mutex<GraphDelta>,
    /// Monotonic count of committed structural writes. Bumped on every write,
    /// independent of the `pending` delta (which the incremental matrix-refresh
    /// path drains). The CSR snapshot records the value it was built at in
    /// `snapshot_gen`; a mismatch means the snapshot lags committed writes.
    write_gen: AtomicU64,
    /// The `write_gen` value the currently installed snapshot reflects.
    snapshot_gen: AtomicU64,
}

impl CsrCache {
    pub fn new(initial: CsrSnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(initial),
            dirty: AtomicU64::new(0),
            rebuilding: AtomicBool::new(false),
            claimed: AtomicU64::new(0),
            pending: parking_lot::Mutex::new(GraphDelta::default()),
            write_gen: AtomicU64::new(0),
            snapshot_gen: AtomicU64::new(0),
        }
    }

    /// Current committed-write generation. Capture this before building a
    /// snapshot and pass it to `install`/`install_full`; writes that land during
    /// the build leave the snapshot conservatively stale.
    pub fn current_gen(&self) -> u64 {
        self.write_gen.load(Ordering::Acquire)
    }

    /// True when the installed snapshot lags committed writes, so a CSR-array or
    /// hybrid consumer must rebuild before reading it.
    pub fn snapshot_is_stale(&self) -> bool {
        self.write_gen.load(Ordering::Acquire) != self.snapshot_gen.load(Ordering::Acquire)
    }

    /// Record a newly inserted node. Called post-commit under the write lock.
    pub fn record_added_node(&self, node: NodeId) {
        self.pending.lock().added_nodes.push(node);
    }

    /// Record a newly inserted edge by its endpoints. Called post-commit under
    /// the write lock.
    pub fn record_added_edge(&self, src: NodeId, dst: NodeId) {
        self.pending.lock().added_edges.push((src, dst));
    }

    /// Record a removed edge by its endpoints. Called post-commit under the
    /// write lock.
    pub fn record_removed_edge(&self, src: NodeId, dst: NodeId) {
        self.pending.lock().removed_edges.push((src, dst));
    }

    /// Mark that the next refresh must do a full rebuild (a node was deleted, so
    /// the sorted dense-index mapping is reshuffled).
    pub fn mark_force_full(&self) {
        self.pending.lock().force_full = true;
    }

    /// True when a structural mutation is pending. A cheap pre-check so the
    /// incremental refresh avoids the exclusive matrices lock when idle.
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// True when a node deletion is pending, requiring a full rebuild.
    pub fn pending_force_full(&self) -> bool {
        self.pending.lock().force_full
    }

    /// Merge a batch of mutations recorded during a multi-write transaction.
    /// Called once, post-commit, so an aborted transaction contributes nothing.
    pub fn record_batch(&self, batch: GraphDelta) {
        if batch.is_empty() {
            return;
        }
        let mut pending = self.pending.lock();
        pending.force_full |= batch.force_full;
        pending.added_nodes.extend(batch.added_nodes);
        pending.added_edges.extend(batch.added_edges);
        pending.removed_edges.extend(batch.removed_edges);
    }

    /// Take the accumulated delta, leaving the buffer empty.
    pub fn take_delta(&self) -> GraphDelta {
        std::mem::take(&mut *self.pending.lock())
    }

    /// Clear the accumulated delta. A full rebuild calls this *before* reading
    /// LMDB (after capturing the generation), so writes that commit during the
    /// build land in the freshly-emptied delta and are re-applied incrementally
    /// later rather than lost.
    pub fn clear_delta(&self) {
        *self.pending.lock() = GraphDelta::default();
    }

    /// Increment the dirty counter by `count`. Returns `true` if this call crosses
    /// the rebuild threshold and no rebuild is already running; the caller must
    /// then perform the rebuild.
    pub fn mark_dirty_n(&self, count: u64) -> bool {
        // Every committed write advances the generation, so a CSR consumer can
        // tell its snapshot lags even when the matrix-refresh path has drained
        // the structural delta.
        self.write_gen.fetch_add(count, Ordering::AcqRel);
        let prev = self.dirty.fetch_add(count, Ordering::Relaxed);
        let total = prev + count;
        if total >= REBUILD_THRESHOLD && !self.rebuilding.swap(true, Ordering::AcqRel) {
            self.claimed.store(total, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Install a snapshot produced by a claimed background rebuild. Subtracts the
    /// claimed dirty count instead of zeroing it, so writes that landed during
    /// the rebuild remain counted. Returns `true` if the residual dirty count
    /// still meets the threshold, in which case the rebuild claim is retained
    /// and the caller must build again; otherwise the claim is released.
    #[must_use]
    pub fn install(&self, snap: CsrSnapshot, built_gen: u64) -> bool {
        self.snapshot.store(Arc::new(snap));
        // `built_gen` was captured before the build, so the snapshot reflects at
        // least that generation. Writes that landed during the build keep
        // `write_gen` ahead, leaving the snapshot correctly stale until the next
        // pass.
        self.snapshot_gen.store(built_gen, Ordering::Release);
        let claimed = self.claimed.swap(0, Ordering::AcqRel);
        let prev = self.dirty.fetch_sub(claimed, Ordering::AcqRel);
        let remaining = prev.saturating_sub(claimed);
        if remaining >= REBUILD_THRESHOLD {
            self.claimed.store(remaining, Ordering::Release);
            true
        } else {
            self.rebuilding.store(false, Ordering::Release);
            false
        }
    }

    /// Install a snapshot from a full synchronous rebuild that captured all
    /// committed state. Clears the dirty counter and any outstanding rebuild
    /// claim, since the new snapshot already reflects every prior write.
    pub fn install_full(&self, snap: CsrSnapshot, built_gen: u64) {
        self.snapshot.store(Arc::new(snap));
        self.snapshot_gen.store(built_gen, Ordering::Release);
        self.dirty.store(0, Ordering::Release);
        self.claimed.store(0, Ordering::Release);
        self.rebuilding.store(false, Ordering::Release);
        // Note: the delta is cleared by `clear_delta` *before* the build, not
        // here, so writes that committed during the build are retained in the
        // freshly-emptied delta for a later incremental apply.
    }

    /// Release the rebuild claim without installing a snapshot; used when the
    /// build step fails so a future write can retry.
    pub fn cancel_rebuild(&self) {
        self.claimed.store(0, Ordering::Release);
        self.rebuilding.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Writes that arrive while a rebuild is in flight must not be discarded by
    /// the install that follows; only the claimed count is subtracted.
    #[test]
    fn install_retains_writes_during_rebuild() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(
            cache.mark_dirty_n(REBUILD_THRESHOLD),
            "crossing claims a rebuild"
        );
        // Five more writes land while the rebuild runs; the claim is already held.
        assert!(!cache.mark_dirty_n(5));
        // Install subtracts only the claimed THRESHOLD, leaving 5 dirty. That is
        // below the threshold, so no follow-up rebuild is requested.
        assert!(!cache.install(CsrSnapshot::empty(), 0));
        // The residual 5 is retained: THRESHOLD - 5 more writes re-trigger.
        assert!(cache.mark_dirty_n(REBUILD_THRESHOLD - 5));
    }

    /// When a full threshold of writes lands during a rebuild, install must keep
    /// the claim and ask for another pass so the snapshot catches up.
    #[test]
    fn install_requests_followup_when_still_dirty() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(cache.mark_dirty_n(REBUILD_THRESHOLD));
        assert!(!cache.mark_dirty_n(REBUILD_THRESHOLD));
        assert!(
            cache.install(CsrSnapshot::empty(), 0),
            "still dirty: rebuild again"
        );
        assert!(!cache.install(CsrSnapshot::empty(), 0), "now caught up");
    }

    /// A full synchronous rebuild clears the counter and any outstanding claim.
    #[test]
    fn install_full_clears_dirty_and_claim() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(cache.mark_dirty_n(REBUILD_THRESHOLD));
        cache.install_full(CsrSnapshot::empty(), 0);
        assert!(
            !cache.mark_dirty_n(1),
            "counter was reset by the full rebuild"
        );
    }
}
