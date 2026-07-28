use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use ahash::AHashMap;
use arc_swap::ArcSwap;
use zerocopy::FromBytes;

use crate::{
    error::Error,
    schema::{AdjEntry, EdgeId, EdgeRecord, NodeId, TypeId},
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
    /// Per-entry edge weight, parallel to `col_idx`, present only on a snapshot
    /// built by [`CsrSnapshot::build_weighted`].
    ///
    /// `None` on every other snapshot, and that is the common case: the only
    /// consumer is the weighted adjacency matrix, which in turn is read only by
    /// Dijkstra. Loading it costs a full `edges` scan that decodes each record and
    /// its property blob to look for one key, and holds eight bytes per edge for
    /// the life of the snapshot, so a workload that never asks a weighted question
    /// must not pay it. [`crate::matrices::MatrixTier`] is how a consumer asks.
    pub edge_weight: Option<Vec<f64>>,
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
    /// An empty snapshot: no nodes, no edges. Used as the placeholder a graph
    /// opens with before any consumer has asked for a built snapshot, and by
    /// tests that need a snapshot without a storage environment.
    pub fn empty() -> Self {
        Self {
            row_ptr: vec![0],
            col_idx: vec![],
            edge_type: vec![],
            edge_id: vec![],
            edge_weight: None,
            in_row_ptr: vec![0],
            in_col_idx: vec![],
            in_edge_type: vec![],
            in_edge_id: vec![],
            dense_to_id: vec![],
            id_to_dense: AHashMap::new(),
        }
    }

    /// Build a fresh in-RAM snapshot of the adjacency, without edge weights.
    ///
    /// This is what every consumer but Dijkstra wants. See
    /// [`CsrSnapshot::build_weighted`] for the other one, and the `edge_weight`
    /// field for why they are separate.
    pub fn build(storage: &Storage) -> Result<Self, Error> {
        Self::build_inner(storage, false)
    }

    /// Build a snapshot that also carries a per-entry edge weight, for the
    /// weighted adjacency matrix.
    ///
    /// The weights cost a full scan of `edges` on top of the adjacency scan, since
    /// a weight lives in the edge's property blob and nowhere else, so only a
    /// consumer that reads them asks for this.
    pub fn build_weighted(storage: &Storage) -> Result<Self, Error> {
        Self::build_inner(storage, true)
    }

    /// Body of both builders.
    ///
    /// The adjacency comes from `out_adj`, which stores one 20-byte `AdjEntry` per
    /// edge under its source node id: the destination, the type id, and the edge id
    /// are every field the arrays hold, already grouped by source and in ascending
    /// key order. Reading them from there rather than from `edges` avoids decoding
    /// one `EdgeRecord` per edge, which also copies that edge's whole encoded
    /// property blob, and it is what lets the entries go straight into the flat
    /// arrays.
    ///
    /// Filling the arrays directly is the point. The previous builder accumulated
    /// one `Vec` per node first and copied that into the arrays afterwards, so a
    /// graph with a million nodes made a million small allocations, held them
    /// alongside the finished arrays at the peak, and then returned them to the
    /// allocator's bins as a million holes that could not be handed back to the
    /// operating system. On a 1 M-node, 13.9 M-edge graph that left 3.3 GB resident
    /// for 620 MB of live arrays. Do not reintroduce a per-node buffer here.
    fn build_inner(storage: &Storage, weighted: bool) -> Result<Self, Error> {
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

        // One pass over `out_adj`. Keys ascend by node id and the dense index is
        // the rank in that same order, so the entries arrive grouped by ascending
        // dense index: pushing them in arrival order puts each one inside its own
        // row, and counting per row at the same time yields the boundaries. The
        // capacity is a hint (the total number of duplicate values); nothing here
        // depends on it being exact.
        let hint = storage.out_adj.len(&rtxn)? as usize;
        let mut row_ptr = vec![0usize; n + 1];
        let mut col_idx: Vec<u32> = Vec::with_capacity(hint);
        let mut edge_type: Vec<TypeId> = Vec::with_capacity(hint);
        let mut edge_id: Vec<EdgeId> = Vec::with_capacity(hint);

        let mut cached_src: Option<NodeId> = None;
        let mut src_dense: Option<u32> = None;
        for result in storage.out_adj.iter(&rtxn)? {
            let (src, bytes) = result?;
            // Resolved once per key rather than once per entry, since the entries
            // of one node arrive consecutively.
            if cached_src != Some(src) {
                cached_src = Some(src);
                src_dense = id_to_dense.get(&src).copied();
            }
            // An endpoint with no record in `nodes` has no dense index, so its
            // entries are skipped, exactly as the previous builder skipped an edge
            // whose endpoints it could not map. The skip must happen identically in
            // the count and in the push, which is why both live in this one loop.
            let Some(src_d) = src_dense else { continue };
            let entry = AdjEntry::read_from_bytes(bytes)
                .ok()
                .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
            // Copied out before use: `AdjEntry` is `repr(packed)`, so a field
            // cannot be borrowed.
            let dst = entry.other;
            let Some(&dst_d) = id_to_dense.get(&dst) else {
                continue;
            };
            row_ptr[src_d as usize + 1] += 1;
            col_idx.push(dst_d);
            edge_type.push(entry.edge_type);
            edge_id.push(entry.edge_id);
        }
        for i in 0..n {
            row_ptr[i + 1] += row_ptr[i];
        }
        let total = row_ptr[n];
        debug_assert_eq!(total, col_idx.len());

        // Restore the by-edge-id order inside each row.
        //
        // This pass exists because of the `AdjEntry` byte layout, not because of
        // anything here: the struct is `repr(C, packed)` with native little-endian
        // fields and `edge_type` first, so LMDB's `memcmp` ordering of duplicates
        // starts from the low byte of the type id. Almost every row with two or more
        // entries therefore arrives unordered and gets sorted, which is O(E log d) per
        // rebuild. Ordering `edge_id` first in big-endian, or installing a `DUPSORT`
        // comparator, would make `out_adj` iterate in edge-id order natively and
        // delete this pass; both change the stored format and the order
        // `out_neighbors` returns, so they belong with the other on-disk work rather
        // than here.
        //
        // The previous builder read `edges` in ascending edge-id order, so that is the
        // order every consumer has seen, and an expansion emits its neighbors in it.
        // `DUPSORT` instead
        // orders duplicates by their raw bytes, and `AdjEntry` holds native
        // little-endian integers, so its order is neither edge-id nor destination
        // order. The scratch buffer is reused across rows, so this costs one
        // allocation of the largest row rather than one per row, and
        // `load_weights` then relies on the ordering to find an edge's slot.
        let mut scratch: Vec<(EdgeId, u32, TypeId)> = Vec::new();
        for i in 0..n {
            let (start, end) = (row_ptr[i], row_ptr[i + 1]);
            if end - start < 2 || edge_id[start..end].is_sorted() {
                continue;
            }
            scratch.clear();
            scratch.extend((start..end).map(|k| (edge_id[k], col_idx[k], edge_type[k])));
            // Edge ids are unique, so the order is total and the sort is stable
            // regardless.
            scratch.sort_unstable_by_key(|&(eid, _, _)| eid);
            for (slot, &(eid, col, ty)) in (start..end).zip(scratch.iter()) {
                edge_id[slot] = eid;
                col_idx[slot] = col;
                edge_type[slot] = ty;
            }
        }

        let edge_weight = if weighted {
            Some(Self::load_weights(
                storage,
                &rtxn,
                &row_ptr,
                &edge_id,
                &id_to_dense,
            )?)
        } else {
            None
        };

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
                in_edge_id[slot] = edge_id[k];
            }
        }

        Ok(Self {
            row_ptr,
            col_idx,
            edge_type,
            edge_id,
            edge_weight,
            in_row_ptr,
            in_col_idx,
            in_edge_type,
            in_edge_id,
            dense_to_id,
            id_to_dense,
        })
    }

    /// Read one weight per outgoing entry, in `col_idx` order.
    ///
    /// A weight is the first present of the `weight`, `cost`, `capacity`, or `cap`
    /// property, defaulting to `1.0`, so the array starts filled with the default
    /// and a scan of `edges` overwrites the entries it finds a value for. Each edge
    /// is placed by looking its id up in its source's row, which is sorted by edge
    /// id, so an edge whose adjacency entry is missing simply finds no slot and an
    /// entry no edge claims keeps the default: neither can shift another entry's
    /// weight, which a cursor-per-row fill could.
    fn load_weights(
        storage: &Storage,
        rtxn: &heed::RoTxn,
        row_ptr: &[usize],
        edge_id: &[EdgeId],
        id_to_dense: &AHashMap<NodeId, u32>,
    ) -> Result<Vec<f64>, Error> {
        let mut weights = vec![1.0f64; edge_id.len()];
        for result in storage.edges.iter(rtxn)? {
            let (id, bytes) = result?;
            let rec: EdgeRecord = props::decode(bytes)?;
            // No properties means no weight property, so the default already holds
            // and the blob decode below is skipped.
            if rec.props.is_empty() {
                continue;
            }
            let Some(&src_d) = id_to_dense.get(&rec.src) else {
                continue;
            };
            let (start, end) = (row_ptr[src_d as usize], row_ptr[src_d as usize + 1]);
            let Ok(offset) = edge_id[start..end].binary_search(&id) else {
                continue;
            };
            let val: serde_json::Value =
                props::decode(&rec.props).unwrap_or(serde_json::Value::Null);
            if let Some(w) = val
                .get("weight")
                .or_else(|| val.get("cost"))
                .or_else(|| val.get("capacity"))
                .or_else(|| val.get("cap"))
                .and_then(|v| v.as_f64())
            {
                weights[start + offset] = w;
            }
        }
        Ok(weights)
    }
}

/// Mutations staged during one write transaction, flushed to the caches only on
/// commit so an aborted transaction never pollutes them.
///
/// The staged fields serve two consumers whose buffers have different lifetimes,
/// so the delta is split at commit rather than shared whole. `Graph::update`
/// drains the entity-level fields (`updated_nodes`, `added_edge_ids`, and
/// `updated_edges`) straight into the property-column caches, which absorb them
/// on the spot; only the structural remainder goes on to accumulate in
/// [`CsrCache::pending`], where it waits for the next matrix refresh. Keeping the
/// column-only fields out of that buffer is what stops a property-update workload
/// that never touches the matrices from growing it without bound.
///
/// `updated_nodes` records property updates on existing nodes. The matrix
/// refresh ignores it (adjacency is unchanged); the property-column cache
/// drains it to re-read those records.
#[derive(Default)]
pub struct GraphDelta {
    pub added_nodes: Vec<NodeId>,
    pub updated_nodes: Vec<NodeId>,
    pub added_edges: Vec<(NodeId, NodeId)>,
    /// Edge ids of the edges added in this transaction, parallel in spirit to
    /// `added_edges`. The edge property column cache drains this to patch the
    /// new edges in without a full rebuild.
    pub added_edge_ids: Vec<crate::schema::EdgeId>,
    /// Edge ids updated (not added) in this transaction, so the edge property
    /// column cache can refresh them once, at commit, instead of per-call.
    pub updated_edges: Vec<crate::schema::EdgeId>,
    pub removed_edges: Vec<(NodeId, NodeId)>,
    pub force_full: bool,
}

/// The structural part of a [`GraphDelta`]: exactly what the GraphBLAS matrices
/// need to be patched in place instead of rebuilt from a full LMDB scan. This is
/// what accumulates in [`CsrCache::pending`] between matrix refreshes.
///
/// `added_edges` and `removed_edges` carry the source and destination node ids;
/// the combined adjacency matrices are a boolean union, so the matrix-refresh
/// path resolves parallel edges against LMDB before deciding to clear a bit.
/// Node deletion reshuffles the sorted dense-index mapping, so it sets
/// `force_full` to fall back to a full rebuild rather than an incremental patch.
#[derive(Default)]
pub struct StructuralDelta {
    pub added_nodes: Vec<NodeId>,
    pub added_edges: Vec<(NodeId, NodeId)>,
    pub removed_edges: Vec<(NodeId, NodeId)>,
    pub force_full: bool,
}

impl StructuralDelta {
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
    pending: parking_lot::Mutex<StructuralDelta>,
    /// Serializes every cache-maintenance operation (incremental delta apply,
    /// snapshot-only refresh, and full rebuild, foreground or background) against
    /// each other. Writers do not take it (they only record the delta and bump
    /// `write_gen`), and idle reads skip it via a lock-free pre-check, so it is
    /// contended only when maintenance is actually needed. Holding it across a
    /// full rebuild is what keeps an incremental drain from racing a background
    /// rebuild that would otherwise discard the drained write, and keeps two
    /// rebuilds from running concurrently.
    pub(crate) maintenance: parking_lot::Mutex<()>,
    /// Monotonic count of committed structural writes. Bumped on every write,
    /// independent of the `pending` delta (which the incremental matrix-refresh
    /// path drains). The CSR snapshot records the value it was built at in
    /// `snapshot_gen`; a mismatch means the snapshot lags committed writes.
    write_gen: AtomicU64,
    /// The `write_gen` value the currently installed snapshot reflects.
    snapshot_gen: AtomicU64,
    /// The `write_gen` value the currently materialized `MatrixSet` reflects.
    ///
    /// The weight and PageRank matrices have no incremental maintenance: a
    /// snapshot-only refresh (`install_snapshot`) and an in-place adjacency delta
    /// (`MatrixSet::apply_delta`) both leave them untouched. So a fresh
    /// `snapshot_gen` does not imply fresh matrices. Only a full rebuild
    /// (`install`/`install_full`, which re-run `MatrixSet::materialize`) advances
    /// this counter; weighted consumers gate on it via `matrices_stale`. The
    /// invariant `matrices_gen <= snapshot_gen <= write_gen` always holds, so
    /// `matrices_stale` subsumes `snapshot_is_stale`.
    matrices_gen: AtomicU64,
}

impl CsrCache {
    pub fn new(initial: CsrSnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(initial),
            dirty: AtomicU64::new(0),
            rebuilding: AtomicBool::new(false),
            claimed: AtomicU64::new(0),
            pending: parking_lot::Mutex::new(StructuralDelta::default()),
            maintenance: parking_lot::Mutex::new(()),
            write_gen: AtomicU64::new(0),
            snapshot_gen: AtomicU64::new(0),
            matrices_gen: AtomicU64::new(0),
        }
    }

    /// Cache for a graph opened without building anything: the snapshot is an
    /// empty placeholder and the caller leaves the matrices unmaterialized.
    ///
    /// `write_gen` starts at 1 while both installed generations stay at 0, so
    /// `snapshot_is_stale` and `matrices_are_stale` report true until the first
    /// gated consumer installs a snapshot built from storage. Only equality of
    /// these counters is ever tested, so the offset start is harmless, and the
    /// documented invariant `matrices_gen <= snapshot_gen <= write_gen` holds.
    /// Without the offset the empty placeholder would claim to be current and a
    /// typed-expansion consumer would read zero rows out of it.
    pub fn new_unbuilt() -> Self {
        let cache = Self::new(CsrSnapshot::empty());
        cache.write_gen.store(1, Ordering::Release);
        cache
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

    /// True when the materialized `MatrixSet` lags committed writes, so a
    /// consumer of the weight or PageRank matrices must rebuild before reading
    /// them. Strictly weaker than fresh matrices being current: it also catches
    /// the case where a snapshot-only refresh or an adjacency-only delta advanced
    /// the snapshot while leaving the weighted matrices behind.
    pub fn matrices_are_stale(&self) -> bool {
        self.write_gen.load(Ordering::Acquire) != self.matrices_gen.load(Ordering::Acquire)
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

    /// Merge the structural part of one transaction's staged mutations into the
    /// pending buffer. Called once, post-commit, so an aborted transaction
    /// contributes nothing.
    ///
    /// Destructured exhaustively on purpose: a new [`GraphDelta`] field cannot be
    /// added without deciding here whether the matrix refresh needs it. The
    /// previous version copied a hand-written subset and silently dropped the
    /// rest, which was correct only for as long as nobody made the matrix path
    /// read one of the dropped ones.
    ///
    /// The column-only fields are deliberately not accumulated. `Graph::update`
    /// drains those into the property columns on the spot, and nothing here would
    /// ever consume them, so buffering them would grow this buffer without bound
    /// on a property-update workload that never touches the matrices.
    ///
    /// Takes the delta by reference rather than by value so the caller can record
    /// it immediately after publishing the commit and still hand the same delta to
    /// the column bookkeeping afterwards. Extending from slices copies the same
    /// elements a moved `Vec` would, since the target buffer is not empty.
    pub fn record_batch(&self, batch: &GraphDelta) {
        let GraphDelta {
            added_nodes,
            updated_nodes: _,
            added_edges,
            added_edge_ids: _,
            updated_edges: _,
            removed_edges,
            force_full,
        } = batch;
        // Nothing structural to apply: a property-only update must not queue work
        // that no consumer drains, and must not make `has_pending` report true.
        if !*force_full
            && added_nodes.is_empty()
            && added_edges.is_empty()
            && removed_edges.is_empty()
        {
            return;
        }
        let mut pending = self.pending.lock();
        pending.force_full |= *force_full;
        pending.added_nodes.extend_from_slice(added_nodes);
        pending.added_edges.extend_from_slice(added_edges);
        pending.removed_edges.extend_from_slice(removed_edges);
    }

    /// Take the accumulated delta, leaving the buffer empty.
    pub fn take_delta(&self) -> StructuralDelta {
        std::mem::take(&mut *self.pending.lock())
    }

    /// Clear the accumulated delta. A full rebuild calls this *before* reading
    /// LMDB (after capturing the generation), so writes that commit during the
    /// build land in the freshly-emptied delta and are re-applied incrementally
    /// later rather than lost.
    pub fn clear_delta(&self) {
        *self.pending.lock() = StructuralDelta::default();
    }

    /// Advance the committed-write generation by `count`, which is what marks the
    /// snapshot and the matrices stale. Every committed write advances it, so a
    /// CSR consumer can tell its snapshot lags even when the matrix-refresh path
    /// has drained the structural delta.
    ///
    /// Call this immediately after `wtxn.commit()` returns, ahead of every other
    /// piece of post-commit bookkeeping. LMDB's commit is what makes a write
    /// visible to readers, and this counter is what tells a reader the caches no
    /// longer reflect storage; every instruction between the two is a window in
    /// which a cache claims to be current while storage has already moved on, and
    /// a reader landing inside it reads pre-write data as though it were fresh.
    /// The bookkeeping that used to run first (the property-column patches and
    /// the structural delta record, both of them mutex acquisitions whose cost
    /// scales with the batch) stretched that window to the width of the
    /// transaction. Publishing first narrows it to one atomic increment.
    ///
    /// The window cannot be closed outright this way, because LMDB's commit and
    /// this increment are not one atomic step. Closing it needs either
    /// statement-level snapshot isolation on the read path, or a second counter
    /// bumped before the commit, and the latter trades the window for a snapshot
    /// rebuild on every read that overlaps a write. See
    /// `Graph::ensure_snapshot_fresh`.
    pub fn advance_write_gen(&self, count: u64) {
        if count == 0 {
            return;
        }
        self.write_gen.fetch_add(count, Ordering::AcqRel);
    }

    /// Increment the dirty counter by `count`. Returns `true` if this call crosses
    /// the rebuild threshold and no rebuild is already running; the caller must
    /// then perform the rebuild. The committed-write generation is advanced
    /// separately, at commit time, by [`CsrCache::advance_write_gen`].
    pub fn note_dirty_n(&self, count: u64) -> bool {
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
        // pass. This is a full rebuild (the caller re-ran `MatrixSet::materialize`
        // before installing), so the weighted matrices are fresh at `built_gen`
        // too.
        self.snapshot_gen.store(built_gen, Ordering::Release);
        self.matrices_gen.store(built_gen, Ordering::Release);
        self.settle_rebuild_claim()
    }

    /// Settle a claimed rebuild pass: subtract the claimed dirty count, then either
    /// retain the claim (returning `true`, meaning build again because that much
    /// landed while this pass ran) or release it.
    ///
    /// Split out of [`CsrCache::install`] so a pass that refreshes the snapshot
    /// alone can settle its claim without also advancing `matrices_gen`, which would
    /// assert that matrices nobody materialized are fresh. Such a pass must settle
    /// rather than [`CsrCache::cancel_rebuild`]: cancelling releases the claim but
    /// leaves `dirty` untouched, so the counter stays above `REBUILD_THRESHOLD` and
    /// the next commit spawns the pass again, and every commit after that does too.
    #[must_use]
    pub fn settle_rebuild_claim(&self) -> bool {
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
        // A full synchronous rebuild also re-materialized the `MatrixSet`, so the
        // weighted matrices are fresh at `built_gen`.
        self.matrices_gen.store(built_gen, Ordering::Release);
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
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use tempfile::TempDir;

    use super::*;
    use crate::Graph;

    /// Every array exactly as the builder produced it before it read `out_adj`: one
    /// `Vec` per node, filled by a scan of `edges` in ascending edge-id order, plus
    /// the counting-sort transpose derived from those rows.
    ///
    /// This is the reference the current builder is checked against. The entry order
    /// inside a row is observable (an expansion emits its neighbors in it) and the
    /// rewrite had to preserve it while changing where the entries are read from, so
    /// a comparison that only counted entries or compared them as sets would not have
    /// held the rewrite to anything.
    struct Reference {
        row_ptr: Vec<usize>,
        col_idx: Vec<u32>,
        edge_type: Vec<TypeId>,
        edge_id: Vec<EdgeId>,
        edge_weight: Vec<f64>,
        in_row_ptr: Vec<usize>,
        in_col_idx: Vec<u32>,
        in_edge_type: Vec<TypeId>,
        in_edge_id: Vec<EdgeId>,
    }

    fn reference_arrays(storage: &Storage) -> Reference {
        let rtxn = storage.env.read_txn().unwrap();
        let mut dense_to_id: Vec<NodeId> = storage
            .nodes
            .iter(&rtxn)
            .unwrap()
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        dense_to_id.sort_unstable();
        let n = dense_to_id.len();
        let id_to_dense: AHashMap<NodeId, u32> = dense_to_id
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as u32))
            .collect();

        let mut adj: Vec<Vec<(u32, TypeId, EdgeId, f64)>> = vec![vec![]; n];
        for result in storage.edges.iter(&rtxn).unwrap() {
            let (edge_id, bytes) = result.unwrap();
            let rec: EdgeRecord = props::decode(bytes).unwrap();
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
        let mut col_idx = Vec::new();
        let mut edge_type = Vec::new();
        let mut edge_id = Vec::new();
        let mut edge_weight = Vec::new();
        for neighbors in adj.iter() {
            for &(dst_d, etype, eid, weight) in neighbors {
                col_idx.push(dst_d);
                edge_type.push(etype);
                edge_id.push(eid);
                edge_weight.push(weight);
            }
        }

        // The transpose, by the same counting sort the builder uses. Derived from the
        // rows above, so comparing it catches a permutation that the outgoing
        // comparison alone would miss only if the two were built independently, and
        // it pins the incoming order an in-direction expansion emits rows in.
        let total = row_ptr[n];
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
                in_edge_id[slot] = edge_id[k];
            }
        }

        Reference {
            row_ptr,
            col_idx,
            edge_type,
            edge_id,
            edge_weight,
            in_row_ptr,
            in_col_idx,
            in_edge_type,
            in_edge_id,
        }
    }

    /// After any random write history, every array must equal what the previous
    /// builder produced: same row boundaries, same entries, the same order inside
    /// each row, the same transpose, and the same weights.
    ///
    /// The rewrite reads `out_adj` instead of `edges`, and LMDB orders duplicate
    /// values by their raw little-endian bytes, which is not edge-id order. So the
    /// builder restores the order per row, and this is what pins that: without it a
    /// row's entries come out permuted, which is invisible to a count and to a set
    /// comparison but changes the order an expansion emits its neighbors in.
    ///
    /// Each case generates its whole history up front and replays it on a fresh
    /// graph. Mutating one shared graph across cases instead would make every case
    /// depend on the ones before it, so proptest's shrinker would replay candidate
    /// histories against a graph that had moved on and could report a minimal
    /// counterexample that does not reproduce on its own.
    #[test]
    fn build_matches_the_previous_builder_over_a_random_write_history() {
        /// One step of a generated history: connect two of the six nodes with one of
        /// three types, or delete the edge added `nth` steps ago.
        #[derive(Debug, Clone)]
        enum Op {
            Add { src: usize, dst: usize, ty: usize },
            Delete { nth: usize },
        }

        let op = prop_oneof![
            8 => (0usize..6, 0usize..6, 0usize..3).prop_map(|(src, dst, ty)| Op::Add { src, dst, ty }),
            2 => (0usize..8).prop_map(|nth| Op::Delete { nth }),
        ];

        let config = ProptestConfig {
            fork: false,
            cases: 48,
            ..Default::default()
        };
        proptest!(config, |(ops in proptest::collection::vec(op, 1..24))| {
            let dir = TempDir::new().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let g = Graph::open(dir.path(), 1).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let nodes: Vec<NodeId> = (0..6)
                .map(|_| g.add_node("N", &()).unwrap())
                .collect();
            let mut live: Vec<EdgeId> = Vec::new();

            for op in &ops {
                match *op {
                    // Parallel edges and self-loops arise naturally from picking both
                    // endpoints at random, and both are cases where row order matters.
                    // A weight on every third edge exercises the weighted build too.
                    Op::Add { src, dst, ty } => {
                        let props = if (src + dst) % 3 == 0 {
                            serde_json::json!({ "weight": (src + dst + 1) as f64 })
                        } else {
                            serde_json::Value::Null
                        };
                        let id = g
                            .add_edge(nodes[src], nodes[dst], ["t", "u", "v"][ty], &props)
                            .map_err(|e| TestCaseError::fail(e.to_string()))?;
                        live.push(id);
                    }
                    // Deleting makes rows shrink as well as grow and leaves the
                    // surviving edge ids non-contiguous, which is what the per-row
                    // ordering step has to cope with.
                    Op::Delete { nth } => {
                        if !live.is_empty() {
                            let victim = live.remove(nth % live.len());
                            g.delete_edge(victim)
                                .map_err(|e| TestCaseError::fail(e.to_string()))?;
                        }
                    }
                }
            }

            let want = reference_arrays(&g.storage);
            let snap = CsrSnapshot::build_weighted(&g.storage)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(&snap.row_ptr, &want.row_ptr);
            prop_assert_eq!(&snap.col_idx, &want.col_idx);
            prop_assert_eq!(&snap.edge_type, &want.edge_type);
            prop_assert_eq!(&snap.edge_id, &want.edge_id);
            prop_assert_eq!(snap.edge_weight.as_ref(), Some(&want.edge_weight));
            prop_assert_eq!(&snap.in_row_ptr, &want.in_row_ptr);
            prop_assert_eq!(&snap.in_col_idx, &want.in_col_idx);
            prop_assert_eq!(&snap.in_edge_type, &want.in_edge_type);
            prop_assert_eq!(&snap.in_edge_id, &want.in_edge_id);

            // The unweighted build must agree on everything except the weights it
            // deliberately does not load.
            let plain = CsrSnapshot::build(&g.storage)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(plain.edge_weight.is_none());
            prop_assert_eq!(&plain.col_idx, &want.col_idx);
            prop_assert_eq!(&plain.edge_id, &want.edge_id);
        });
    }

    /// `build` must carry no weights and `build_weighted` must carry one per entry,
    /// aligned with the entry it belongs to.
    ///
    /// Alignment is the part worth pinning: the weights are placed by looking each
    /// edge id up in its source's row rather than by the order `edges` is scanned
    /// in, so a mis-sorted row would attach a weight to the wrong edge. The graph
    /// below gives one node several outgoing edges with distinct weights, which is
    /// where such a mix-up would show.
    #[test]
    fn build_omits_weights_and_build_weighted_aligns_them_with_their_entries() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        let c = g.add_node("n", &()).unwrap();
        // Distinct property names, since a weight is the first present of four, plus
        // one edge with no weight property at all to exercise the 1.0 default.
        let e_ab = g
            .add_edge(a, b, "t", &serde_json::json!({"weight": 2.5}))
            .unwrap();
        let e_ac = g
            .add_edge(a, c, "t", &serde_json::json!({"cost": 4.0}))
            .unwrap();
        let e_ba = g
            .add_edge(b, a, "t", &serde_json::json!({"capacity": 8.0}))
            .unwrap();
        let e_bc = g
            .add_edge(b, c, "t", &serde_json::json!({"cap": 16.0}))
            .unwrap();
        let e_ca = g.add_edge(c, a, "t", &()).unwrap();

        let plain = CsrSnapshot::build(&g.storage).unwrap();
        assert!(
            plain.edge_weight.is_none(),
            "an unweighted build must not pay for the weights"
        );

        let snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let weights = snap
            .edge_weight
            .as_ref()
            .expect("weighted build carries them");
        assert_eq!(weights.len(), snap.col_idx.len());
        let by_edge: AHashMap<EdgeId, f64> = snap
            .edge_id
            .iter()
            .zip(weights.iter())
            .map(|(&e, &w)| (e, w))
            .collect();
        assert_eq!(by_edge[&e_ab], 2.5);
        assert_eq!(by_edge[&e_ac], 4.0);
        assert_eq!(by_edge[&e_ba], 8.0);
        assert_eq!(by_edge[&e_bc], 16.0);
        assert_eq!(by_edge[&e_ca], 1.0, "no weight property means 1.0");
    }

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
            cache.note_dirty_n(REBUILD_THRESHOLD),
            "crossing claims a rebuild"
        );
        // Five more writes land while the rebuild runs; the claim is already held.
        assert!(!cache.note_dirty_n(5));
        // Install subtracts only the claimed THRESHOLD, leaving 5 dirty. That is
        // below the threshold, so no follow-up rebuild is requested.
        assert!(!cache.install(CsrSnapshot::empty(), 0));
        // The residual 5 is retained: THRESHOLD - 5 more writes re-trigger.
        assert!(cache.note_dirty_n(REBUILD_THRESHOLD - 5));
    }

    /// When a full threshold of writes lands during a rebuild, install must keep
    /// the claim and ask for another pass so the snapshot catches up.
    #[test]
    fn install_requests_followup_when_still_dirty() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(cache.note_dirty_n(REBUILD_THRESHOLD));
        assert!(!cache.note_dirty_n(REBUILD_THRESHOLD));
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
        cache.advance_write_gen(1);
        assert!(!cache.note_dirty_n(1));
        cache.record_added_edge(1, 2);
        assert!(cache.snapshot_is_stale());

        cache.install_snapshot(CsrSnapshot::empty(), cache.current_gen());

        assert!(!cache.snapshot_is_stale());
        assert!(
            cache.has_pending(),
            "the delta is drained by ensure_matrix_view, not by a snapshot install"
        );
    }

    /// `record_batch` must carry every structural field into the pending buffer.
    /// The previous version copied a hand-written subset of a wider struct and
    /// dropped the rest, so this pins that each field the matrix refresh reads
    /// survives the hand-off.
    #[test]
    fn record_batch_carries_every_structural_field() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        cache.record_batch(&GraphDelta {
            added_nodes: vec![1, 2],
            added_edges: vec![(1, 2)],
            removed_edges: vec![(3, 4)],
            force_full: true,
            ..GraphDelta::default()
        });

        let drained = cache.take_delta();
        assert_eq!(drained.added_nodes, vec![1, 2]);
        assert_eq!(drained.added_edges, vec![(1, 2)]);
        assert_eq!(drained.removed_edges, vec![(3, 4)]);
        assert!(drained.force_full);
        assert!(
            cache.take_delta().is_empty(),
            "the buffer must be empty once drained"
        );
    }

    /// The column-only fields of a `GraphDelta` must not reach the cache's
    /// pending buffer: `Graph::update` has already drained them into the property
    /// columns, and nothing here would ever consume them, so accumulating them
    /// would grow the buffer without bound on a property-update workload that
    /// never touches the matrices. A property-only update must leave the buffer
    /// empty, so `has_pending` keeps reporting false and the idle-read fast path
    /// in `ensure_matrix_view` stays lock-free.
    #[test]
    fn record_batch_ignores_the_column_only_fields() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        cache.record_batch(&GraphDelta {
            updated_nodes: vec![2],
            added_edge_ids: vec![10],
            updated_edges: vec![11],
            ..GraphDelta::default()
        });
        assert!(
            !cache.has_pending(),
            "a property-only update must queue no structural work"
        );

        cache.record_batch(&GraphDelta {
            added_nodes: vec![1],
            updated_nodes: vec![2],
            added_edge_ids: vec![10],
            updated_edges: vec![11],
            ..GraphDelta::default()
        });
        let drained = cache.take_delta();
        assert_eq!(
            drained.added_nodes,
            vec![1],
            "the structural field survives"
        );
        assert!(drained.added_edges.is_empty());
        assert!(!drained.force_full);
    }

    /// A snapshot-only background pass must settle its claim, not cancel it.
    ///
    /// Cancelling releases the claim while leaving `dirty` above the threshold, so
    /// the next write re-triggers the pass, and every write after that does too: a
    /// continuous background full scan on any write workload with no GraphBLAS
    /// consumer, which is exactly the workload the lazy open serves.
    #[test]
    fn a_snapshot_only_pass_settles_its_claim() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(
            cache.note_dirty_n(REBUILD_THRESHOLD),
            "crossing claims a pass"
        );

        cache.install_snapshot(CsrSnapshot::empty(), cache.current_gen());
        assert!(
            !cache.settle_rebuild_claim(),
            "nothing landed during the pass"
        );

        assert!(
            !cache.note_dirty_n(1),
            "one more write must not re-trigger; the claimed count was subtracted"
        );

        // Cancelling instead would have left the counter armed, which is the bug.
        let leaky = CsrCache::new(CsrSnapshot::empty());
        assert!(leaky.note_dirty_n(REBUILD_THRESHOLD));
        leaky.cancel_rebuild();
        assert!(
            leaky.note_dirty_n(1),
            "cancel leaves the dirty count armed, so this documents why settle exists"
        );
    }

    /// A full synchronous rebuild clears the counter and any outstanding claim.
    #[test]
    fn install_full_clears_dirty_and_claim() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(cache.note_dirty_n(REBUILD_THRESHOLD));
        cache.install_full(CsrSnapshot::empty(), 0);
        assert!(
            !cache.note_dirty_n(1),
            "counter was reset by the full rebuild"
        );
    }
}
