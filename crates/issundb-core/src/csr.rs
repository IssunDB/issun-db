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

/// Compressed Sparse Row snapshot of the adjacency (outgoing and incoming).
pub struct CsrSnapshot {
    /// `row_ptr[i]..row_ptr[i+1]` is the range of the i-th node's edges.
    pub row_ptr: Vec<usize>,
    pub col_idx: Vec<u32>,
    pub edge_type: Vec<TypeId>,
    pub edge_id: Vec<EdgeId>,
    pub edge_weight: Vec<f64>,
    /// Transpose of the outgoing CSR: `in_row_ptr[i]..in_row_ptr[i+1]` ranges
    /// over the i-th node's incoming edges, `in_col_idx` holds source dense
    /// indices, and entries within a row are ordered by ascending source.
    pub in_row_ptr: Vec<usize>,
    pub in_col_idx: Vec<u32>,
    pub in_edge_type: Vec<TypeId>,
    pub in_edge_id: Vec<EdgeId>,
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
            in_row_ptr: vec![0],
            in_col_idx: vec![],
            in_edge_type: vec![],
            in_edge_id: vec![],
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

        // Counting-sort transpose for the incoming view. Walking the outgoing
        // rows in ascending source order keeps each incoming row ordered by
        // ascending source dense index.
        let mut in_row_ptr = vec![0usize; n + 1];
        for &dst_d in &col_idx {
            in_row_ptr[dst_d as usize + 1] += 1;
        }
        for i in 0..n {
            in_row_ptr[i + 1] += in_row_ptr[i];
        }
        let mut in_col_idx = vec![0u32; total];
        let mut in_edge_type = vec![0u32; total];
        let mut in_edge_id = vec![0u64; total];
        let mut cursor = in_row_ptr.clone();
        for src_d in 0..n {
            for k in row_ptr[src_d]..row_ptr[src_d + 1] {
                let slot = cursor[col_idx[k] as usize];
                cursor[col_idx[k] as usize] += 1;
                in_col_idx[slot] = src_d as u32;
                in_edge_type[slot] = edge_type[k];
                in_edge_id[slot] = edge_id_arr[k];
            }
        }

        Ok(Self {
            row_ptr,
            col_idx,
            edge_type,
            edge_id: edge_id_arr,
            edge_weight: edge_weight_arr,
            in_row_ptr,
            in_col_idx,
            in_edge_type,
            in_edge_id,
            dense_to_id,
            id_to_dense,
        })
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
///
/// `updated_nodes` records property updates on existing nodes. The matrix
/// refresh ignores it (adjacency is unchanged); the property-column cache
/// drains it to re-read those records.
#[derive(Default)]
pub struct GraphDelta {
    pub added_nodes: Vec<NodeId>,
    pub updated_nodes: Vec<NodeId>,
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
            && self.updated_nodes.is_empty()
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

    /// Install a snapshot-only refresh: store the snapshot and the generation
    /// it was built at, leaving the dirty counter, any rebuild claim, and the
    /// pending structural delta untouched. The delta still belongs to the
    /// incremental matrix path (`ensure_matrix_view`); clearing it here would
    /// strand the matrices stale behind a fresh snapshot.
    pub fn install_snapshot(&self, snap: CsrSnapshot, built_gen: u64) {
        self.snapshot.store(Arc::new(snap));
        self.snapshot_gen.store(built_gen, Ordering::Release);
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
mod snapshot_tests {
    use tempfile::TempDir;

    use super::*;
    use crate::Graph;

    /// The incoming arrays must be an exact transpose of the outgoing CSR:
    /// every outgoing entry appears exactly once under its destination row,
    /// rows are ordered by ascending source dense index, and each entry keeps
    /// its edge id and type id.
    #[test]
    fn build_transposes_incoming_adjacency() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        let c = g.add_node("n", &()).unwrap();
        let e_ab = g.add_edge(a, b, "t", &()).unwrap();
        let e_cb = g.add_edge(c, b, "u", &()).unwrap();
        // A parallel edge and a self-loop exercise duplicate destination rows
        // and a row that is both source and destination.
        let e_ab2 = g.add_edge(a, b, "t", &()).unwrap();
        let e_aa = g.add_edge(a, a, "t", &()).unwrap();

        let snap = CsrSnapshot::build(&g.storage).unwrap();
        let da = snap.id_to_dense[&a] as usize;
        let db = snap.id_to_dense[&b] as usize;
        let dc = snap.id_to_dense[&c] as usize;

        assert_eq!(snap.in_row_ptr.len(), snap.dense_to_id.len() + 1);
        assert_eq!(snap.in_col_idx.len(), snap.col_idx.len());
        assert_eq!(snap.in_edge_id.len(), snap.col_idx.len());
        assert_eq!(snap.in_edge_type.len(), snap.col_idx.len());

        let in_row = |d: usize| -> Vec<(u32, EdgeId)> {
            (snap.in_row_ptr[d]..snap.in_row_ptr[d + 1])
                .map(|k| (snap.in_col_idx[k], snap.in_edge_id[k]))
                .collect()
        };
        assert_eq!(in_row(da), vec![(da as u32, e_aa)]);
        assert_eq!(
            in_row(db),
            vec![(da as u32, e_ab), (da as u32, e_ab2), (dc as u32, e_cb)]
        );
        assert_eq!(in_row(dc), vec![]);

        // Each transposed entry carries the same type id as the outgoing entry for the same edge.
        let out_type: AHashMap<EdgeId, TypeId> = snap
            .edge_id
            .iter()
            .zip(snap.edge_type.iter())
            .map(|(&e, &t)| (e, t))
            .collect();
        for k in 0..snap.in_edge_id.len() {
            assert_eq!(snap.in_edge_type[k], out_type[&snap.in_edge_id[k]]);
        }
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

    /// A snapshot-only install must refresh the generation while leaving the
    /// pending structural delta in place: the delta still belongs to the
    /// incremental matrix path, which drains it later.
    #[test]
    fn install_snapshot_leaves_the_matrix_delta() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(!cache.mark_dirty_n(1));
        cache.record_added_edge(1, 2);
        assert!(cache.snapshot_is_stale());

        cache.install_snapshot(CsrSnapshot::empty(), cache.current_gen());

        assert!(!cache.snapshot_is_stale());
        assert!(
            cache.has_pending(),
            "the delta is drained by ensure_matrix_view, not by a snapshot install"
        );
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
