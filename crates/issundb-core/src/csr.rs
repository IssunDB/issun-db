use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use ahash::AHashMap;
use arc_swap::ArcSwap;

use crate::{
    array::Array,
    error::Error,
    schema::{AdjEntry, EdgeId, EdgeRecord, NodeId, TypeId},
    storage::{Storage, props},
};

/// Minimum number of writes between two successive background rebuilds.
pub const REBUILD_THRESHOLD: u64 = 1_000;

/// Compressed Sparse Row snapshot of the adjacency (outgoing and incoming).
///
/// Every array is an [`Array`]: owned when the snapshot was built or patched in
/// this process, a view into the mapped cache file when it was loaded. The
/// kernels index them as slices either way, so a loaded snapshot costs heap for
/// nothing but `id_to_dense`, and only the pages a query touches become
/// resident.
pub struct CsrSnapshot {
    /// `row_ptr[i]..row_ptr[i+1]` is the range of the i-th node's edges.
    pub row_ptr: Array<usize>,
    pub col_idx: Array<u32>,
    pub edge_type: Array<TypeId>,
    pub edge_id: Array<EdgeId>,
    /// Per-entry edge weight, parallel to `col_idx`, present only on a snapshot
    /// built by [`CsrSnapshot::build_weighted`].
    ///
    /// `None` on every other snapshot, and that is the common case: the only
    /// consumer is Dijkstra. Loading it costs a full `edges` scan that decodes each
    /// record and its property blob to look for one key, and holds eight bytes per
    /// edge for the life of the snapshot, so a workload that never asks a weighted
    /// question must not pay it. [`CsrCache::request_weights`] is how a consumer
    /// asks.
    pub edge_weight: Option<Array<f64>>,
    /// Whether any entry of `edge_weight` is negative, decided once at build time.
    ///
    /// Dijkstra's heap relaxation needs non-negative weights and falls back to a
    /// label-correcting pass when it cannot have them, so it has to ask this on every
    /// call; asking the array directly would scan all E weights per query. Always
    /// false on an unweighted snapshot, which has no weights to be negative.
    pub has_negative_weight: bool,
    /// Transpose of the outgoing CSR: `in_row_ptr[i]..in_row_ptr[i+1]` ranges
    /// over the i-th node's incoming edges, `in_col_idx` holds source dense
    /// indices, and entries within a row are ordered by ascending source.
    pub in_row_ptr: Array<usize>,
    pub in_col_idx: Array<u32>,
    /// For each incoming entry, the position of the same edge in the outgoing
    /// arrays, so its type and id are `edge_type[in_pos[k]]` (see
    /// [`CsrSnapshot::in_edge_type`]) and `edge_id[in_pos[k]]`. Four bytes per
    /// edge in place of the twelve
    /// a duplicated type and id cost; the indirection is one random read per
    /// incoming entry whose type or id a kernel asks for, and the kernels that
    /// scan the incoming view for its structure alone never pay it.
    pub in_pos: Array<u32>,
    pub dense_to_id: Array<NodeId>,
    pub id_to_dense: AHashMap<NodeId, u32>,
}

/// The largest number of edges a snapshot can hold: `in_pos` addresses the
/// outgoing arrays with 32 bits.
pub const MAX_SNAPSHOT_EDGES: usize = u32::MAX as usize;

impl CsrSnapshot {
    /// An empty snapshot: no nodes, no edges. Used as the placeholder a graph
    /// opens with before any consumer has asked for a built snapshot, and by
    /// tests that need a snapshot without a storage environment.
    pub fn empty() -> Self {
        Self {
            row_ptr: vec![0].into(),
            col_idx: Array::default(),
            edge_type: Array::default(),
            edge_id: Array::default(),
            edge_weight: None,
            has_negative_weight: false,
            in_row_ptr: vec![0].into(),
            in_col_idx: Array::default(),
            in_pos: Array::default(),
            dense_to_id: Array::default(),
            id_to_dense: AHashMap::new(),
        }
    }

    /// The type id of the k-th incoming entry, read through `in_pos`.
    #[inline]
    pub fn in_edge_type(&self, k: usize) -> TypeId {
        self.edge_type[self.in_pos[k] as usize]
    }

    /// The error for an edge count `in_pos` cannot address.
    fn too_many_edges(total: usize) -> Error {
        Error::InvalidArgument(format!(
            "the adjacency holds {total} edges, more than the {MAX_SNAPSHOT_EDGES} a snapshot can index"
        ))
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
            let entry = AdjEntry::decode_value(bytes)?;
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
        if total > MAX_SNAPSHOT_EDGES {
            return Err(Self::too_many_edges(total));
        }

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
        let has_negative_weight = edge_weight
            .as_ref()
            .is_some_and(|weights| weights.iter().any(|w| *w < 0.0));

        let (in_row_ptr, in_col_idx, in_pos) = Self::transpose(n, &row_ptr, &col_idx);

        Ok(Self {
            row_ptr: row_ptr.into(),
            col_idx: col_idx.into(),
            edge_type: edge_type.into(),
            edge_id: edge_id.into(),
            edge_weight: edge_weight.map(Array::from),
            has_negative_weight,
            in_row_ptr: in_row_ptr.into(),
            in_col_idx: in_col_idx.into(),
            in_pos: in_pos.into(),
            dense_to_id: dense_to_id.into(),
            id_to_dense,
        })
    }

    /// Counting-sort transpose of the outgoing arrays into the incoming view.
    /// Walking the outgoing rows in ascending source order keeps each incoming
    /// row ordered by ascending source dense index. The third array is the
    /// outgoing position of each incoming entry, which is where its type and
    /// id live.
    fn transpose(n: usize, row_ptr: &[usize], col_idx: &[u32]) -> (Vec<usize>, Vec<u32>, Vec<u32>) {
        let total = col_idx.len();
        let mut in_row_ptr = vec![0usize; n + 1];
        for &dst_d in col_idx {
            in_row_ptr[dst_d as usize + 1] += 1;
        }
        for i in 0..n {
            in_row_ptr[i + 1] += in_row_ptr[i];
        }
        let mut in_col_idx = vec![0u32; total];
        let mut in_pos = vec![0u32; total];
        let mut cursor = in_row_ptr.clone();
        for src_d in 0..n {
            for k in row_ptr[src_d]..row_ptr[src_d + 1] {
                let slot = cursor[col_idx[k] as usize];
                cursor[col_idx[k] as usize] += 1;
                in_col_idx[slot] = src_d as u32;
                in_pos[slot] = k as u32;
            }
        }
        (in_row_ptr, in_col_idx, in_pos)
    }

    /// This snapshot plus the nodes and edges of `change`, without reading the
    /// adjacency: the incremental refresh. `None` when the change cannot be
    /// applied and the caller must build from storage: a deletion (`full`), an
    /// edge property update on a snapshot carrying weights (a weight may have
    /// moved), or a node id out of allocation order (dense indices are the rank
    /// of ascending node ids, so a new node must sort after every existing one).
    ///
    /// The cost is one pass over the arrays plus the transpose, with no LMDB
    /// iteration, no per-entry decode, and no hash map construction beyond the
    /// new nodes. Every row keeps its ascending edge-id order, so the result is
    /// exactly what a full build would produce; the proptest in this module
    /// pins that. `weight_of` supplies the weight of an added edge when the
    /// snapshot carries weights.
    pub fn with_additions(
        &self,
        change: &CsrChange,
        mut weight_of: impl FnMut(EdgeId) -> Result<f64, Error>,
    ) -> Result<Option<Self>, Error> {
        if change.full || (change.edges_updated && self.edge_weight.is_some()) {
            return Ok(None);
        }
        let mut added_nodes: Vec<NodeId> = change
            .added_nodes
            .iter()
            .copied()
            .filter(|id| !self.id_to_dense.contains_key(id))
            .collect();
        added_nodes.sort_unstable();
        added_nodes.dedup();
        if let (Some(&last), Some(&first_new)) = (self.dense_to_id.last(), added_nodes.first()) {
            if first_new <= last {
                return Ok(None);
            }
        }
        let old_n = self.dense_to_id.len();
        let mut dense_to_id = self.dense_to_id.to_vec();
        dense_to_id.extend(added_nodes.iter().copied());
        let mut id_to_dense = self.id_to_dense.clone();
        for (i, &id) in added_nodes.iter().enumerate() {
            id_to_dense.insert(id, (old_n + i) as u32);
        }
        let n = dense_to_id.len();

        // Added entries keyed by (source dense index, edge id): sorted once, they
        // arrive row by row in the order each row keeps.
        let mut adds: Vec<(u32, EdgeId, u32, TypeId)> = change
            .added_edges
            .iter()
            .filter_map(|e| {
                let src_d = *id_to_dense.get(&e.src)?;
                let dst_d = *id_to_dense.get(&e.dst)?;
                Some((src_d, e.edge_id, dst_d, e.edge_type))
            })
            .collect();
        adds.sort_unstable();
        adds.dedup();

        let total = self.col_idx.len() + adds.len();
        if total > MAX_SNAPSHOT_EDGES {
            return Ok(None);
        }
        let mut row_ptr = vec![0usize; n + 1];
        let mut col_idx: Vec<u32> = Vec::with_capacity(total);
        let mut edge_type: Vec<TypeId> = Vec::with_capacity(total);
        let mut edge_id: Vec<EdgeId> = Vec::with_capacity(total);
        let mut edge_weight: Option<Vec<f64>> =
            self.edge_weight.as_ref().map(|_| Vec::with_capacity(total));
        let mut has_negative_weight = self.has_negative_weight;

        let mut next_add = 0usize;
        let mut scratch: Vec<(EdgeId, u32, TypeId, f64)> = Vec::new();
        for i in 0..n {
            let row_start = col_idx.len();
            if i < old_n {
                let (start, end) = (self.row_ptr[i], self.row_ptr[i + 1]);
                col_idx.extend_from_slice(&self.col_idx[start..end]);
                edge_type.extend_from_slice(&self.edge_type[start..end]);
                edge_id.extend_from_slice(&self.edge_id[start..end]);
                if let (Some(w), Some(old_w)) = (edge_weight.as_mut(), self.edge_weight.as_ref()) {
                    w.extend_from_slice(&old_w[start..end]);
                }
            }
            let old_last = edge_id.last().copied();
            let adds_start = next_add;
            while next_add < adds.len() && adds[next_add].0 as usize == i {
                let (_, eid, dst_d, ty) = adds[next_add];
                col_idx.push(dst_d);
                edge_type.push(ty);
                edge_id.push(eid);
                if let Some(w) = edge_weight.as_mut() {
                    let weight = weight_of(eid)?;
                    has_negative_weight |= weight < 0.0;
                    w.push(weight);
                }
                next_add += 1;
            }
            // Edge ids are allocated monotonically, so the appended entries
            // normally follow the row's last existing id; restore the order
            // otherwise, exactly as the full build does.
            let needs_sort =
                adds_start < next_add && old_last.is_some_and(|last| last > adds[adds_start].1);
            if needs_sort {
                let end = col_idx.len();
                scratch.clear();
                scratch.extend((row_start..end).map(|k| {
                    (
                        edge_id[k],
                        col_idx[k],
                        edge_type[k],
                        edge_weight.as_ref().map(|w| w[k]).unwrap_or(1.0),
                    )
                }));
                scratch.sort_unstable_by_key(|&(eid, _, _, _)| eid);
                for (slot, &(eid, col, ty, w)) in (row_start..end).zip(scratch.iter()) {
                    edge_id[slot] = eid;
                    col_idx[slot] = col;
                    edge_type[slot] = ty;
                    if let Some(weights) = edge_weight.as_mut() {
                        weights[slot] = w;
                    }
                }
            }
            row_ptr[i + 1] = col_idx.len();
        }
        debug_assert_eq!(next_add, adds.len());

        let (in_row_ptr, in_col_idx, in_pos) = Self::transpose(n, &row_ptr, &col_idx);

        Ok(Some(Self {
            row_ptr: row_ptr.into(),
            col_idx: col_idx.into(),
            edge_type: edge_type.into(),
            edge_id: edge_id.into(),
            edge_weight: edge_weight.map(Array::from),
            has_negative_weight,
            in_row_ptr: in_row_ptr.into(),
            in_col_idx: in_col_idx.into(),
            in_pos: in_pos.into(),
            dense_to_id: dense_to_id.into(),
            id_to_dense,
        }))
    }

    /// The weight of one edge, read from its record: the first present of
    /// the `weight`, `cost`, `capacity`, or `cap` property, default `1.0`, the
    /// rule `load_weights` applies to every edge at once.
    pub(crate) fn weight_of_edge(
        storage: &Storage,
        rtxn: &crate::storage::RoTxn,
        eid: EdgeId,
    ) -> Result<f64, Error> {
        let Some(bytes) = storage.edges.get(rtxn, &eid)? else {
            return Ok(1.0);
        };
        let rec: EdgeRecord = props::decode(bytes)?;
        Ok(weight_in_props(&rec.props))
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
        rtxn: &crate::storage::RoTxn,
        row_ptr: &[usize],
        edge_id: &[EdgeId],
        id_to_dense: &AHashMap<NodeId, u32>,
    ) -> Result<Vec<f64>, Error> {
        let mut weights = vec![1.0f64; edge_id.len()];
        for result in storage.edges.iter(rtxn)? {
            let (id, bytes) = result?;
            let rec: EdgeRecord = props::decode(bytes)?;
            let Some(&src_d) = id_to_dense.get(&rec.src) else {
                continue;
            };
            let (start, end) = (row_ptr[src_d as usize], row_ptr[src_d as usize + 1]);
            let Ok(offset) = edge_id[start..end].binary_search(&id) else {
                continue;
            };
            weights[start + offset] = weight_in_props(&rec.props);
        }
        Ok(weights)
    }
}

/// The weight an encoded property blob carries: the first present of `weight`,
/// `cost`, `capacity`, or `cap`, else `1.0`.
fn weight_in_props(encoded: &[u8]) -> f64 {
    let val: serde_json::Value = props::decode(encoded).unwrap_or(serde_json::Value::Null);
    val.get("weight")
        .or_else(|| val.get("cost"))
        .or_else(|| val.get("capacity"))
        .or_else(|| val.get("cap"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0)
}

/// One edge added by a committed write, with every field the snapshot's arrays
/// hold for it, so the incremental refresh needs no storage read to place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddedEdge {
    pub src: NodeId,
    pub dst: NodeId,
    pub edge_type: TypeId,
    pub edge_id: EdgeId,
}

/// The structural effect of one committed write on the CSR snapshot, recorded
/// by `Graph::commit_and_publish` and consumed by the refresh gate.
///
/// Additions are listed, because a snapshot can absorb them without touching
/// storage. A removal is only flagged: it reshuffles dense indices and row
/// boundaries, and the refresh then builds from storage. `edges_updated` matters
/// only to a snapshot carrying weights, whose per-edge weight may have changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CsrChange {
    pub added_nodes: Vec<NodeId>,
    pub added_edges: Vec<AddedEdge>,
    pub edges_updated: bool,
    pub full: bool,
}

impl CsrChange {
    /// A write with no structural effect (a property or label update).
    pub fn none() -> Self {
        Self::default()
    }

    /// A write the snapshot cannot absorb (a node or edge removal).
    pub fn full() -> Self {
        Self {
            full: true,
            ..Self::default()
        }
    }

    /// A property update on existing edges.
    pub fn edges_updated() -> Self {
        Self {
            edges_updated: true,
            ..Self::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.added_nodes.is_empty()
            && self.added_edges.is_empty()
            && !self.edges_updated
            && !self.full
    }

    fn absorb(&mut self, other: &CsrChange) {
        self.added_nodes.extend_from_slice(&other.added_nodes);
        self.added_edges.extend_from_slice(&other.added_edges);
        self.edges_updated |= other.edges_updated;
        self.full |= other.full;
    }
}

/// Added edges held for the incremental refresh before it gives up and builds
/// from storage. Past this many the pending list costs more to hold and apply
/// than the scan it replaces; a bulk load ends in `rebuild_csr` anyway.
pub const INCREMENTAL_MAX_EDGES: usize = 1 << 20;

/// Committed structural changes the installed snapshot has not absorbed.
///
/// Each batch is tagged with the write generation *before* the commit that
/// produced it advanced the counter, and a writer records its batch before that
/// advance. A snapshot stamped at generation `B` was built from a storage read
/// taken after the counter reached `B`, so it holds every write whose advance
/// preceded that read, which is every batch tagged below `B`; those are dropped
/// at install, and the batches tagged at or above `B` are what the next
/// incremental refresh applies. A batch recorded after the refresh read the
/// counter but before it drained the list is applied one generation early,
/// which is harmless: its commit had already landed.
#[derive(Default)]
struct PendingChanges {
    batches: Vec<(u64, CsrChange)>,
    total_edges: usize,
}

impl PendingChanges {
    fn record(&mut self, tag: u64, change: CsrChange) {
        if change.is_empty() {
            return;
        }
        if change.full || self.total_edges + change.added_edges.len() > INCREMENTAL_MAX_EDGES {
            // One full marker stands for everything: the refresh must build
            // from storage, and the storage read will cover these batches.
            self.batches.clear();
            self.total_edges = 0;
            self.batches.push((tag, CsrChange::full()));
            return;
        }
        self.total_edges += change.added_edges.len();
        self.batches.push((tag, change));
    }

    /// Drop the batches a snapshot stamped `built_gen` is known to contain.
    fn prune_below(&mut self, built_gen: u64) {
        self.batches.retain(|(tag, _)| *tag >= built_gen);
        self.total_edges = self.batches.iter().map(|(_, c)| c.added_edges.len()).sum();
    }

    /// Everything the snapshot stamped `snapshot_gen` still lacks, merged, with
    /// the number of batches it covers; `None` when a full build is required.
    fn applicable(&self, snapshot_gen: u64) -> Option<(CsrChange, usize)> {
        let mut merged = CsrChange::default();
        for (tag, change) in &self.batches {
            if *tag < snapshot_gen {
                continue;
            }
            if change.full {
                return None;
            }
            merged.absorb(change);
        }
        Some((merged, self.batches.len()))
    }

    fn drain_applied(&mut self, applied: usize) {
        self.batches.drain(..applied.min(self.batches.len()));
        self.total_edges = self.batches.iter().map(|(_, c)| c.added_edges.len()).sum();
    }
}

/// Mutations staged during one write transaction, flushed to the caches only on
/// commit so an aborted transaction never pollutes them.
///
/// `Graph::update` drains this into the property-column caches, which absorb it on
/// the spot, and turns it into the [`CsrChange`] the snapshot's incremental
/// refresh applies; the committed-write generation is still what tells a reader
/// that its snapshot lags (see [`CsrCache::advance_write_gen`]).
///
/// `updated_nodes` records property updates on existing nodes, which the column
/// cache drains to re-read those records.
///
/// Every field here has a reader, and that is a size constraint rather than
/// tidiness: one transaction can be a whole bulk load, so a per-edge `Vec` nobody
/// drains costs its element size per edge for the length of the load. An edge
/// removal only has to be *noticed*, so `removed_edge` is a flag and not a list.
#[derive(Default)]
pub struct GraphDelta {
    pub added_nodes: Vec<NodeId>,
    pub updated_nodes: Vec<NodeId>,
    /// The edges added in this transaction, with the fields the CSR snapshot
    /// holds for each. The edge property column cache patches the new edges in
    /// by id, and the snapshot's incremental refresh appends them by endpoint.
    pub added_edges: Vec<AddedEdge>,
    /// Edge ids updated (not added) in this transaction, so the edge property
    /// column cache can refresh them once, at commit, instead of per-call.
    pub updated_edges: Vec<crate::schema::EdgeId>,
    /// Whether any edge was removed. A removal reshuffles the dense edge mapping,
    /// so the edge columns rebuild rather than patch; which edges went is not part
    /// of that decision.
    pub removed_edge: bool,
    pub force_full: bool,
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
    /// Serializes every cache-maintenance operation (a foreground refresh and a
    /// background rebuild) against each other. Writers do not take it (they only
    /// bump `write_gen`), and idle reads skip it via a lock-free pre-check, so it is
    /// contended only when maintenance is actually needed. Holding it across a whole
    /// pass is what keeps two rebuilds from running concurrently and installing over
    /// each other.
    pub(crate) maintenance: parking_lot::Mutex<()>,
    /// Monotonic count of committed structural writes, bumped on every write. The
    /// CSR snapshot records the value it was built at in `snapshot_gen`; a mismatch
    /// means the snapshot lags committed writes.
    write_gen: AtomicU64,
    /// The `write_gen` value the currently installed snapshot reflects.
    snapshot_gen: AtomicU64,
    /// Whether any consumer has asked for per-edge weights, which only Dijkstra
    /// does. Sticky once set, so a later unweighted refresh does not strip them out
    /// from under an alternating workload; see `Graph::weighted_snapshot`.
    weights_requested: AtomicBool,
    /// Committed structural changes not yet absorbed by the installed snapshot,
    /// for the incremental refresh. See [`PendingChanges`] for the tagging rule.
    pending: parking_lot::Mutex<PendingChanges>,
    /// Snapshots built from storage, for the tests that assert a refresh patched
    /// rather than rebuilt.
    #[cfg(test)]
    pub full_builds: AtomicU64,
}

impl CsrCache {
    pub fn new(initial: CsrSnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(initial),
            dirty: AtomicU64::new(0),
            rebuilding: AtomicBool::new(false),
            claimed: AtomicU64::new(0),
            maintenance: parking_lot::Mutex::new(()),
            write_gen: AtomicU64::new(0),
            snapshot_gen: AtomicU64::new(0),
            weights_requested: AtomicBool::new(false),
            pending: parking_lot::Mutex::new(PendingChanges::default()),
            #[cfg(test)]
            full_builds: AtomicU64::new(0),
        }
    }

    /// The generation the installed snapshot reflects; `0` while it is still the
    /// unbuilt placeholder, which no incremental refresh may extend.
    pub fn snapshot_gen(&self) -> u64 {
        self.snapshot_gen.load(Ordering::Acquire)
    }

    /// Record the structural effect of a write that has just committed. Call
    /// this *before* [`CsrCache::advance_write_gen`]: the batch is tagged with
    /// the generation before the advance, and a refresh that reads the counter
    /// must find every batch the counter accounts for already recorded.
    pub fn record_change(&self, change: CsrChange) {
        if change.is_empty() {
            return;
        }
        let tag = self.current_gen();
        self.pending.lock().record(tag, change);
    }

    /// The changes the installed snapshot lacks, merged, plus how many pending
    /// batches they span; `None` when a removal forces a build from storage.
    /// Call under `maintenance`, and pass the same count to
    /// [`CsrCache::install_incremental`] once the patched snapshot is ready.
    pub fn pending_applicable(&self) -> Option<(CsrChange, usize)> {
        self.pending.lock().applicable(self.snapshot_gen())
    }

    /// Install a snapshot the incremental refresh produced: drop exactly the
    /// batches it applied (later ones stay for the next refresh), then stamp it.
    pub fn install_incremental(&self, snap: Arc<CsrSnapshot>, built_gen: u64, applied: usize) {
        {
            let mut pending = self.pending.lock();
            pending.drain_applied(applied);
            pending.prune_below(built_gen);
        }
        self.snapshot.store(snap);
        self.snapshot_gen.store(built_gen, Ordering::Release);
    }

    fn prune_pending(&self, built_gen: u64) {
        self.pending.lock().prune_below(built_gen);
    }

    /// Cache for a graph opened without building anything: the snapshot is an
    /// empty placeholder.
    ///
    /// `write_gen` starts at 1 while `snapshot_gen` stays at 0, so
    /// `snapshot_is_stale` reports true until the first gated consumer installs a
    /// snapshot built from storage. Only equality of these counters is ever tested,
    /// so the offset start is harmless. Without it the empty placeholder would claim
    /// to be current and a typed-expansion consumer would read zero rows out of it.
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

    /// Ask for per-edge weights on every snapshot built from here on.
    ///
    /// Sticky, and deliberately so, because without it an unweighted refresh would strip
    /// the weights and the next weighted query would rebuild from storage again, so
    /// a workload alternating Dijkstra with any other algorithm would rebuild twice
    /// per write. See `Graph::weighted_snapshot` for the memory this trades away.
    pub fn request_weights(&self) {
        self.weights_requested.store(true, Ordering::Release);
    }

    /// Whether a snapshot built now must carry per-edge weights.
    pub fn wants_weights(&self) -> bool {
        self.weights_requested.load(Ordering::Acquire)
    }

    /// Advance the committed-write generation by `count`, which is what marks the
    /// snapshot stale. Every committed write advances it.
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
        self.prune_pending(built_gen);
        self.snapshot.store(Arc::new(snap));
        // `built_gen` was captured before the build, so the snapshot reflects at
        // least that generation. Writes that landed during the build keep
        // `write_gen` ahead, leaving the snapshot correctly stale until the next
        // pass.
        self.snapshot_gen.store(built_gen, Ordering::Release);
        self.settle_rebuild_claim()
    }

    /// Settle a claimed rebuild pass: subtract the claimed dirty count, then either
    /// retain the claim (returning `true`, meaning build again because that much
    /// landed while this pass ran) or release it.
    ///
    /// A pass that installed something must settle rather than
    /// [`CsrCache::cancel_rebuild`]: cancelling releases the claim but leaves
    /// `dirty` untouched, so the counter stays above `REBUILD_THRESHOLD` and the
    /// next commit spawns the pass again, and every commit after that does too.
    #[must_use]
    fn settle_rebuild_claim(&self) -> bool {
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

    /// Install a foreground refresh: store the snapshot and the generation it was
    /// built at, leaving the dirty counter and any rebuild claim untouched, since
    /// this pass did not claim one.
    #[cfg(test)]
    pub fn install_snapshot(&self, snap: CsrSnapshot, built_gen: u64) {
        self.install_snapshot_shared(Arc::new(snap), built_gen);
    }

    /// [`CsrCache::install_snapshot`] for a caller that already holds the snapshot
    /// behind an `Arc` and needs to keep reading it after the install, which is how
    /// the weighted gate avoids reloading a pointer another refresh may have
    /// replaced in between.
    pub fn install_snapshot_shared(&self, snap: Arc<CsrSnapshot>, built_gen: u64) {
        self.prune_pending(built_gen);
        self.snapshot.store(snap);
        self.snapshot_gen.store(built_gen, Ordering::Release);
    }

    /// Install a snapshot from a full synchronous rebuild that captured all
    /// committed state. Clears the dirty counter and any outstanding rebuild
    /// claim, since the new snapshot already reflects every prior write.
    pub fn install_full(&self, snap: CsrSnapshot, built_gen: u64) {
        self.prune_pending(built_gen);
        self.snapshot.store(Arc::new(snap));
        self.snapshot_gen.store(built_gen, Ordering::Release);
        self.dirty.store(0, Ordering::Release);
        self.claimed.store(0, Ordering::Release);
        self.rebuilding.store(false, Ordering::Release);
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
        in_pos: Vec<u32>,
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
        let mut in_pos = vec![0u32; total];
        let mut cursor = in_row_ptr.clone();
        for src_d in 0..n {
            for k in row_ptr[src_d]..row_ptr[src_d + 1] {
                let slot = cursor[col_idx[k] as usize];
                cursor[col_idx[k] as usize] += 1;
                in_col_idx[slot] = src_d as u32;
                in_pos[slot] = k as u32;
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
            in_pos,
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
    fn assert_same_snapshot(got: &CsrSnapshot, want: &CsrSnapshot) {
        assert_eq!(got.dense_to_id, want.dense_to_id);
        assert_eq!(got.id_to_dense, want.id_to_dense);
        assert_eq!(got.row_ptr, want.row_ptr);
        assert_eq!(got.col_idx, want.col_idx);
        assert_eq!(got.edge_type, want.edge_type);
        assert_eq!(got.edge_id, want.edge_id);
        assert_eq!(got.edge_weight, want.edge_weight);
        assert_eq!(got.has_negative_weight, want.has_negative_weight);
        assert_eq!(got.in_row_ptr, want.in_row_ptr);
        assert_eq!(got.in_col_idx, want.in_col_idx);
        assert_eq!(got.in_pos, want.in_pos);
    }

    fn full_builds(g: &Graph) -> u64 {
        g.csr_cache.full_builds.load(Ordering::Relaxed)
    }

    /// Additions through both write paths are absorbed without a build from
    /// storage, and the result is array-for-array what a build produces; a
    /// removal then forces the build.
    #[test]
    fn refresh_patches_additions_and_rebuilds_on_removal() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("P", &()).unwrap();
        let b = g.add_node("P", &()).unwrap();
        let ab = g.add_edge(a, b, "T", &()).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        let builds = full_builds(&g);

        let c = g.add_node("P", &()).unwrap();
        g.add_edge(b, c, "T", &()).unwrap();
        g.update(|t| {
            let d = t.add_node("Q", &())?;
            t.add_edge(c, d, "U", &())?;
            t.add_edge(a, d, "T", &())?;
            t.add_edge(d, a, "T", &())?;
            Ok(())
        })
        .unwrap();
        g.ensure_snapshot_fresh().unwrap();
        assert_eq!(full_builds(&g), builds, "additions patch the snapshot");
        assert_same_snapshot(
            &g.csr_cache.snapshot.load(),
            &CsrSnapshot::build(&g.storage).unwrap(),
        );
        assert!(!g.csr_cache.snapshot_is_stale());

        g.delete_edge(ab).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        assert_eq!(full_builds(&g), builds + 1, "a removal builds from storage");
        assert_same_snapshot(
            &g.csr_cache.snapshot.load(),
            &CsrSnapshot::build(&g.storage).unwrap(),
        );
    }

    /// A weighted snapshot absorbs an added edge with its weight read from the
    /// record, and an edge property update makes it build from storage, since a
    /// weight may have moved.
    #[test]
    fn weighted_refresh_patches_new_edges_and_rebuilds_on_edge_update() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("P", &()).unwrap();
        let b = g.add_node("P", &()).unwrap();
        let c = g.add_node("P", &()).unwrap();
        let ab = g
            .add_edge(a, b, "T", &serde_json::json!({ "weight": 5.0 }))
            .unwrap();
        g.add_edge(b, c, "T", &serde_json::json!({ "weight": 5.0 }))
            .unwrap();
        assert_eq!(
            g.shortest_path_dijkstra(a, c)
                .unwrap()
                .unwrap()
                .total_weight,
            10.0
        );
        let builds = full_builds(&g);

        g.add_edge(a, c, "T", &serde_json::json!({ "cost": 2.5 }))
            .unwrap();
        assert_eq!(
            g.shortest_path_dijkstra(a, c)
                .unwrap()
                .unwrap()
                .total_weight,
            2.5
        );
        assert_eq!(full_builds(&g), builds, "the new weight was read per edge");
        assert_same_snapshot(
            &g.csr_cache.snapshot.load(),
            &CsrSnapshot::build_weighted(&g.storage).unwrap(),
        );

        g.update_edge(ab, &serde_json::json!({ "weight": 0.5 }))
            .unwrap();
        g.update_edge(
            g.edges_by_type("T").unwrap()[1],
            &serde_json::json!({ "weight": 0.5 }),
        )
        .unwrap();
        assert_eq!(
            g.shortest_path_dijkstra(a, c)
                .unwrap()
                .unwrap()
                .total_weight,
            1.0
        );
        assert!(
            full_builds(&g) > builds,
            "an edge update rebuilds the weights"
        );
    }

    /// A node deleted between refreshes forces a build from storage too, and a
    /// pending list past the cap collapses to one.
    #[test]
    fn refresh_rebuilds_after_a_node_deletion_and_a_pending_overflow() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("P", &()).unwrap();
        let b = g.add_node("P", &()).unwrap();
        g.add_edge(a, b, "T", &()).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        let builds = full_builds(&g);
        g.delete_node(b).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        assert_eq!(full_builds(&g), builds + 1);
        assert_same_snapshot(
            &g.csr_cache.snapshot.load(),
            &CsrSnapshot::build(&g.storage).unwrap(),
        );

        let mut pending = PendingChanges::default();
        let big = CsrChange {
            added_edges: vec![
                AddedEdge {
                    src: 0,
                    dst: 0,
                    edge_type: 0,
                    edge_id: 0
                };
                INCREMENTAL_MAX_EDGES + 1
            ],
            ..CsrChange::default()
        };
        pending.record(3, big);
        pending.record(
            4,
            CsrChange {
                added_nodes: vec![9],
                ..CsrChange::default()
            },
        );
        assert!(
            pending.applicable(0).is_none(),
            "overflow collapses to a full marker"
        );
        // The later addition is still recorded: a build stamped between the two
        // tags drops the marker but must keep the addition.
        assert_eq!(pending.batches.len(), 2);
        pending.prune_below(4);
        assert_eq!(
            pending.applicable(4).map(|(c, _)| c.added_nodes),
            Some(vec![9])
        );
        pending.prune_below(5);
        assert!(pending.batches.is_empty());
    }

    /// Batches tagged below an installed generation are dropped at install, and
    /// only the batches a patch applied are drained, so a batch recorded during
    /// the patch survives for the next refresh.
    #[test]
    fn pending_changes_follow_the_generation_tagging_rule() {
        let mut pending = PendingChanges::default();
        let node = |id: NodeId| CsrChange {
            added_nodes: vec![id],
            ..CsrChange::default()
        };
        pending.record(1, node(1));
        pending.record(2, node(2));
        pending.record(3, node(3));
        pending.prune_below(3);
        let (change, applied) = pending.applicable(3).unwrap();
        assert_eq!(change.added_nodes, vec![3]);
        assert_eq!(applied, 1);
        pending.record(3, node(4));
        pending.drain_applied(applied);
        let (rest, _) = pending.applicable(3).unwrap();
        assert_eq!(rest.added_nodes, vec![4]);
        pending.record(4, CsrChange::edges_updated());
        let (rest, _) = pending.applicable(3).unwrap();
        assert!(rest.edges_updated);
        pending.record(5, CsrChange::full());
        assert!(pending.applicable(3).is_none());
    }

    /// Over a random history of additions, removals, weight updates, and
    /// refreshes, the snapshot the gate installs, patched or built, equals a
    /// build from storage at every refresh, weights included.
    #[test]
    fn incremental_refresh_matches_a_full_build_over_a_random_history() {
        #[derive(Debug, Clone)]
        enum Op {
            AddNode,
            AddEdge {
                src: usize,
                dst: usize,
                ty: usize,
                weighted: bool,
            },
            DeleteEdge {
                nth: usize,
            },
            UpdateEdge {
                nth: usize,
            },
            DeleteNode {
                nth: usize,
            },
            Refresh,
            Batch {
                count: usize,
            },
        }
        let op = prop_oneof![
            3 => Just(Op::AddNode),
            10 => (0usize..12, 0usize..12, 0usize..3, any::<bool>())
                .prop_map(|(src, dst, ty, weighted)| Op::AddEdge { src, dst, ty, weighted }),
            2 => (0usize..16).prop_map(|nth| Op::DeleteEdge { nth }),
            2 => (0usize..16).prop_map(|nth| Op::UpdateEdge { nth }),
            1 => (0usize..8).prop_map(|nth| Op::DeleteNode { nth }),
            6 => Just(Op::Refresh),
            2 => (1usize..5).prop_map(|count| Op::Batch { count }),
        ];
        let config = ProptestConfig {
            fork: false,
            cases: 32,
            ..Default::default()
        };
        proptest!(config, |(ops in proptest::collection::vec(op, 1..40), weighted in any::<bool>())| {
            let dir = TempDir::new().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let g = Graph::open(dir.path(), 1).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let fail = |e: crate::Error| TestCaseError::fail(e.to_string());
            let mut nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
            let mut live: Vec<EdgeId> = Vec::new();
            let refresh = |g: &Graph| -> Result<(), TestCaseError> {
                if weighted {
                    g.with_weighted_snapshot(|_| Ok(())).map_err(fail)?;
                } else {
                    g.ensure_snapshot_fresh().map_err(fail)?;
                }
                let want = if weighted {
                    CsrSnapshot::build_weighted(&g.storage)
                } else {
                    CsrSnapshot::build(&g.storage)
                }
                .map_err(fail)?;
                let got = g.csr_cache.snapshot.load();
                prop_assert_eq!(&got.dense_to_id, &want.dense_to_id);
                prop_assert_eq!(&got.row_ptr, &want.row_ptr);
                prop_assert_eq!(&got.col_idx, &want.col_idx);
                prop_assert_eq!(&got.edge_type, &want.edge_type);
                prop_assert_eq!(&got.edge_id, &want.edge_id);
                prop_assert_eq!(&got.edge_weight, &want.edge_weight);
                prop_assert_eq!(got.has_negative_weight, want.has_negative_weight);
                prop_assert_eq!(&got.in_row_ptr, &want.in_row_ptr);
                prop_assert_eq!(&got.in_col_idx, &want.in_col_idx);
                prop_assert_eq!(&got.in_pos, &want.in_pos);
                Ok(())
            };
            refresh(&g)?;
            let mut i = 0;
            while i < ops.len() {
                match ops[i].clone() {
                    Op::AddNode => nodes.push(g.add_node("N", &()).map_err(fail)?),
                    Op::AddEdge { src, dst, ty, weighted: w } => {
                        let props = if w {
                            serde_json::json!({ "weight": (src as f64) - 3.0 })
                        } else {
                            serde_json::Value::Null
                        };
                        let (s, d) = (nodes[src % nodes.len()], nodes[dst % nodes.len()]);
                        live.push(g.add_edge(s, d, ["t", "u", "v"][ty], &props).map_err(fail)?);
                    }
                    Op::DeleteEdge { nth } => {
                        if !live.is_empty() {
                            let victim = live.remove(nth % live.len());
                            g.delete_edge(victim).map_err(fail)?;
                        }
                    }
                    Op::UpdateEdge { nth } => {
                        if !live.is_empty() {
                            let e = live[nth % live.len()];
                            g.update_edge(e, &serde_json::json!({ "cost": nth as f64 })).map_err(fail)?;
                        }
                    }
                    Op::DeleteNode { nth } => {
                        if nodes.len() > 1 {
                            let victim = nodes.remove(nth % nodes.len());
                            g.delete_node(victim).map_err(fail)?;
                            // Incident edges went with it; drop any stale ids.
                            live.retain(|e| g.get_edge(*e).ok().flatten().is_some());
                        }
                    }
                    Op::Refresh => refresh(&g)?,
                    Op::Batch { count } => {
                        let snapshot_nodes = nodes.clone();
                        let created = g
                            .update(|t| {
                                let mut made = Vec::new();
                                for k in 0..count {
                                    let n = t.add_node("B", &())?;
                                    let from = snapshot_nodes[k % snapshot_nodes.len()];
                                    let e = t.add_edge(from, n, "t", &serde_json::json!({ "weight": 2.0 }))?;
                                    made.push((n, e));
                                }
                                Ok(made)
                            })
                            .map_err(fail)?;
                        for (n, e) in created {
                            nodes.push(n);
                            live.push(e);
                        }
                    }
                }
                i += 1;
            }
            refresh(&g)?;
        });
    }

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

        // 24 cases, not proptest's default 256 and not the 48 this started at. Each case
        // opens its own LMDB environment and commits every operation separately, so a case
        // costs milliseconds rather than microseconds: 48 cases of up to 24 operations
        // measured 3.7 s, most of a suite whose whole point is to stay fast, while 24
        // cases of up to 20 measure 0.31 s. The drop is far more than the halving suggests
        // because cost grows with history length as well as case count. Coverage is
        // unaffected in practice: what this pins is a permuted row, which shows up in
        // almost every case rather than a rare one.
        let config = ProptestConfig {
            fork: false,
            cases: 24,
            ..Default::default()
        };
        proptest!(config, |(ops in proptest::collection::vec(op, 1..20))| {
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
            prop_assert_eq!(snap.edge_weight.as_ref().map(|w| w.to_vec()), Some(want.edge_weight.clone()));
            prop_assert_eq!(&snap.in_row_ptr, &want.in_row_ptr);
            prop_assert_eq!(&snap.in_col_idx, &want.in_col_idx);
            prop_assert_eq!(&snap.in_pos, &want.in_pos);

            // The unweighted build must agree on everything except the weights it
            // deliberately does not load.
            let plain = CsrSnapshot::build(&g.storage)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(plain.edge_weight.is_none());
            prop_assert_eq!(&plain.col_idx, &want.col_idx);
            prop_assert_eq!(&plain.edge_id, &want.edge_id);
        });
    }

    /// An adjacency entry whose destination node does not exist must be skipped, and
    /// skipped consistently in the row boundaries and in the arrays.
    ///
    /// The rewrite moved the source of truth from `edges` to `out_adj`, so which entries
    /// survive an inconsistency between the two changed: this is the direction that used
    /// to be filtered by `id_to_dense` on both endpoints and still must be. The write
    /// path never produces this state (`add_edge` requires both endpoints to exist), so
    /// the entry is written straight to storage, which is also the only way to reach the
    /// skip at all.
    #[test]
    fn build_skips_an_adjacency_entry_whose_endpoint_is_missing() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        let b = g.add_node("n", &()).unwrap();
        let real = g.add_edge(a, b, "t", &()).unwrap();

        // One entry under a live source pointing at a node id that was never allocated,
        // and one under a source that does not exist either.
        let ghost = 999_999u64;
        {
            use zerocopy::IntoBytes;
            // Written as raw duplicate values rather than through the write path, which
            // refuses a nonexistent endpoint and so cannot produce this state.
            let dangling_dst = AdjEntry {
                edge_type: 0,
                other: ghost,
                edge_id: 12_345,
            };
            let dangling_src = AdjEntry {
                edge_type: 0,
                other: b,
                edge_id: 12_346,
            };
            let mut wtxn = g.storage.env.write_txn().unwrap();
            g.storage
                .out_adj
                .put(&mut wtxn, &a, dangling_dst.as_bytes())
                .unwrap();
            g.storage
                .out_adj
                .put(&mut wtxn, &ghost, dangling_src.as_bytes())
                .unwrap();
            wtxn.commit().unwrap();
        }

        let snap = CsrSnapshot::build(&g.storage).unwrap();
        assert_eq!(
            snap.col_idx.len(),
            1,
            "only the edge with two live endpoints belongs in the snapshot"
        );
        assert_eq!(snap.edge_id, vec![real]);
        assert_eq!(
            snap.row_ptr[snap.dense_to_id.len()],
            1,
            "the row boundaries must count exactly what the arrays hold"
        );
        // The transpose is derived from those arrays, so it must agree.
        assert_eq!(snap.in_col_idx.len(), 1);
        assert_eq!(snap.edge_id[snap.in_pos[0] as usize], real);
    }

    /// A duplicate value that is not a whole `AdjEntry` is reported, not silently
    /// reinterpreted. The size in the message is the layout invariant declared on the
    /// struct, which is why the check lives on the type.
    #[test]
    fn build_rejects_a_malformed_adjacency_value() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("n", &()).unwrap();
        {
            let mut wtxn = g.storage.env.write_txn().unwrap();
            g.storage.out_adj.put(&mut wtxn, &a, &[1u8, 2, 3]).unwrap();
            wtxn.commit().unwrap();
        }
        let Err(err) = CsrSnapshot::build(&g.storage) else {
            panic!("a short value must be rejected");
        };
        assert!(
            matches!(err, Error::Corrupt(msg) if msg.contains("20 bytes")),
            "unexpected error: {err}"
        );
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
        assert_eq!(snap.in_pos.len(), snap.col_idx.len());

        let in_row = |d: usize| -> Vec<(u32, EdgeId)> {
            (snap.in_row_ptr[d]..snap.in_row_ptr[d + 1])
                .map(|k| (snap.in_col_idx[k], snap.edge_id[snap.in_pos[k] as usize]))
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
        for k in 0..snap.in_pos.len() {
            assert_eq!(
                snap.in_edge_type(k),
                out_type[&snap.edge_id[snap.in_pos[k] as usize]]
            );
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

    /// A foreground refresh must advance the generation without touching the dirty
    /// counter or an outstanding rebuild claim, which belong to the background pass.
    #[test]
    fn install_snapshot_advances_the_generation_alone() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        cache.advance_write_gen(1);
        assert!(!cache.note_dirty_n(1));
        assert!(cache.snapshot_is_stale());

        cache.install_snapshot(CsrSnapshot::empty(), cache.current_gen());

        assert!(!cache.snapshot_is_stale());
        assert!(
            !cache.note_dirty_n(REBUILD_THRESHOLD - 2),
            "the dirty count kept accumulating across the foreground refresh"
        );
    }

    /// Asking for weights is sticky, so a later build still loads them. Without
    /// that, an unweighted refresh between two weighted queries would make each one
    /// rebuild from storage.
    #[test]
    fn requesting_weights_is_sticky() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(!cache.wants_weights());
        cache.request_weights();
        assert!(cache.wants_weights());
        cache.install_snapshot(CsrSnapshot::empty(), 0);
        assert!(
            cache.wants_weights(),
            "an install must not withdraw the request"
        );
    }

    /// Cancelling a claimed pass leaves the dirty counter armed, so a later commit
    /// retries it. That is deliberate: the failed pass installed nothing, so the
    /// work still needs doing.
    #[test]
    fn cancelling_a_claim_leaves_the_counter_armed() {
        let cache = CsrCache::new(CsrSnapshot::empty());
        assert!(cache.note_dirty_n(REBUILD_THRESHOLD));
        cache.cancel_rebuild();
        assert!(
            cache.note_dirty_n(1),
            "the next write re-triggers the pass the failure abandoned"
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
