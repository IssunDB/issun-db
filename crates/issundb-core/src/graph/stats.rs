//! High-order cardinality statistics for the query optimizer.
//!
//! The optimizer estimates the cost of an `Expand` from the average fan-out per
//! input row: the number of edges of the expanded type divided by the number of
//! candidate source nodes. The simplest model divides the global typed edge
//! count by the total node count, which assumes every node type expands at the
//! same rate. Real schemas are skewed: a `Person` may have dozens of `KNOWS`
//! edges while a `City` has none, yet both inflate the global denominator.
//!
//! This module precomputes the per-source-label typed out-degree (and the
//! symmetric per-destination-label typed in-degree): for each `(label, type)`
//! pair, the count of edges of that type incident to a node carrying that label
//! in the given direction. Dividing by the label's node count yields the
//! per-label expand ratio, the "expand ratio" of high-order statistics. The
//! table is a schema-level aggregate (bounded by distinct labels times distinct
//! types), computed by one full scan and cached against the committed-write
//! generation.
//!
//! Nothing builds it as a side effect of a query. The three readers do not share
//! one freshness policy, because they do not share one consequence.
//!
//! The two fan-out estimates only weight a plan choice, and nothing builds the table
//! on their behalf: one full scan to sharpen an estimate made the first query
//! mentioning a relationship pattern pay for the whole graph.
//! [`Graph::materialize_edge_statistics`] is how a caller asks for them. They do
//! accept a table the write generation has moved past, because their alternative is
//! not a fresher estimate but no estimate, and the global average they fall back to
//! is cruder than a slightly dated per-label ratio. The tolerance is bounded by how
//! much the relationship type in question has grown since the build, not by how long
//! ago it was.
//!
//! [`Graph::schema_has_edge`] is not advisory, because the optimizer drops rows on
//! a negative. It therefore neither builds the table nor tolerates a stale one: with
//! no current table it is answered directly from the label index and the adjacency
//! under a bounded probe budget, and from the table when one is current. Leaving it
//! to the table alone would make its pass dormant on every graph nobody had
//! materialized, and letting it read a stale one would have it deny a triple a
//! committed write just realized. Because the pass that asks runs on every execution,
//! a decided probe verdict is remembered for its generation.
//!
//! So the generation check gates *use*, not refresh, and what "usable" means differs
//! per reader. That split is the whole design: freshness requirements follow from
//! consequences, and these three readers do not share a consequence.

use ahash::AHashMap;
use zerocopy::FromBytes;

use crate::{
    error::Error,
    schema::{AdjEntry, LabelId, NodeId, TypeId},
    storage::{
        ids::{get_label, get_label_count, get_type, get_type_count},
        lmdb::Storage,
    },
};

use super::{Graph, composite_key};

/// Storage probes one [`Graph::schema_has_edge`] question may spend before it
/// reports the question undecidable.
///
/// Proving a triple *absent* means exhausting every candidate edge, which on a
/// large graph is the same scan the table build costs, so the probe has to stop
/// somewhere. Stopping costs the caller an optimization; not stopping would cost
/// it the build it was avoiding.
///
/// The budget is only ever fully spent on a hop that is unsatisfiable at scale or
/// undecidable, because a realized triple short-circuits on the first matching
/// edge. That is the trade: at most this many probes to possibly prove a whole
/// expansion empty. A caller wanting the exact answer on a graph too large to
/// probe calls [`Graph::materialize_edge_statistics`].
const SCHEMA_PROBE_BUDGET: u64 = 32_768;

/// How far one relationship type may grow past a stale fan-out table before the table
/// is refused rather than served to the advisory readers.
///
/// The estimate is a ratio whose denominator is read live (`node_count_by_label`), so
/// only its numerator ages. A type that has doubled its edges has roughly doubled the
/// fan-out of the labels it leaves, which is the same order of error the global average
/// already carries on a skewed schema. An order of magnitude is not: past some point
/// the live global average is better information than the snapshot, and an estimate
/// that understates fan-out by 100x invites the planner to treat an expensive expansion
/// as nearly free.
///
/// Growth is the right thing to bound, rather than elapsed generations, because a
/// generation is one commit and one commit may be a single edge or a bulk import. It is
/// compared per type rather than over the whole graph because a global count cannot see
/// skew: half a million new edges of one type on a graph of a million sits inside any
/// global factor while moving that type's fan-out by orders of magnitude.
const STALE_FANOUT_GROWTH_FACTOR: u64 = 2;

/// Split a `label_idx` key into the `(label, node)` pair it encodes, rejecting a
/// key that is not exactly the 12-byte composite.
fn split_label_key(key: &[u8]) -> Result<(LabelId, NodeId), Error> {
    let label: [u8; 4] = key
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::Corrupt("label_idx key has wrong length"))?;
    let node: [u8; 8] = key
        .get(4..)
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::Corrupt("label_idx key has wrong length"))?;
    Ok((u32::from_be_bytes(label), u64::from_be_bytes(node)))
}

/// The data graph schema as edge frequencies, tagged with the committed-write
/// generation the table reflects.
///
/// `out_by_src_label` and `in_by_dst_label` are the per-source-label and
/// per-destination-label typed edge counts (the marginals) that back the
/// expand-ratio cardinality estimate. `triples` is the realized schema graph:
/// for each directed `(src_label, type, dst_label)` actually present in the
/// data, the count of edges matching it. The set of `triples` keys is the
/// schema connectivity that drives type inference; the counts refine the
/// cardinality estimate when both endpoint labels are known.
pub(crate) struct EdgeFanout {
    /// The `csr_cache` write generation this table reflects.
    generation: u64,
    /// Edges counted per relationship type, once each regardless of how many labels
    /// its endpoints carry. Compared against the live `stats:t:` counter to decide
    /// whether a table the generation has moved past still describes this type; see
    /// [`STALE_FANOUT_GROWTH_FACTOR`].
    edges_by_type: AHashMap<TypeId, u64>,
    /// Count of edges of a type whose source node carries a label.
    out_by_src_label: AHashMap<(LabelId, TypeId), u64>,
    /// Count of edges of a type whose target node carries a label.
    in_by_dst_label: AHashMap<(LabelId, TypeId), u64>,
    /// Count of edges matching a realized `(src_label, type, dst_label)` schema
    /// triple. A multi-label endpoint contributes one triple per label it
    /// carries, so an edge between an `m`-label source and an `n`-label target
    /// contributes to `m * n` triples.
    triples: AHashMap<(LabelId, TypeId, LabelId), u64>,
}

impl EdgeFanout {
    /// Build the frequency table from one pass over the label index and one over
    /// the outgoing adjacency. A node with multiple labels contributes to each of
    /// its labels, matching the label-index semantics where such a node appears in
    /// every matching label scan.
    ///
    /// Neither pass decodes a record, because neither needs one. Reading `labels`
    /// out of a `NodeRecord` also copies that node's whole encoded property blob,
    /// and reading the endpoints out of an `EdgeRecord` copies the edge's, so a
    /// scan of `nodes` and `edges` spent most of its time on properties this table
    /// never looks at. `label_idx` carries the `(label, node)` pair in twelve raw
    /// bytes and `out_adj` carries one 20-byte `AdjEntry` per edge under its
    /// source, which is every field counted here. Sourcing the labels from
    /// `label_idx` also means the statistics describe exactly the population a
    /// label scan enumerates.
    fn build(storage: &Storage, generation: u64) -> Result<Self, Error> {
        let rtxn = storage.env.read_txn()?;

        // Keyed by `(label, node)`, so a multi-label node's labels arrive in
        // label-id order rather than the insertion order `NodeRecord` keeps. Only
        // counting reads them, so the order does not matter. An unlabeled node has
        // no key here and so no entry, which is what the counting below skips over.
        //
        // Sized from the label index up front: most nodes carry one label, so the
        // entry count is close to the node count and growing the map from empty would
        // rehash it repeatedly on a large graph.
        let mut node_labels: AHashMap<NodeId, Vec<LabelId>> =
            AHashMap::with_capacity(storage.label_idx.len(&rtxn)? as usize);
        for result in storage.label_idx.iter(&rtxn)? {
            let (key, _) = result?;
            let (label, node) = split_label_key(key)?;
            node_labels.entry(node).or_default().push(label);
        }

        let mut edges_by_type: AHashMap<TypeId, u64> = AHashMap::new();
        let mut out_by_src_label: AHashMap<(LabelId, TypeId), u64> = AHashMap::new();
        let mut in_by_dst_label: AHashMap<(LabelId, TypeId), u64> = AHashMap::new();
        let mut triples: AHashMap<(LabelId, TypeId, LabelId), u64> = AHashMap::new();
        // One duplicate value per outgoing edge, grouped by source node, so every
        // edge is seen exactly once with both endpoints and its type. The grouping
        // lets a source's labels be looked up once per node instead of once per
        // edge; the lookup is keyed on the node the entry came from, so ungrouped
        // iteration would still count correctly, just more slowly.
        let mut cached_src: Option<NodeId> = None;
        let mut src_labels: &[LabelId] = &[];
        for result in storage.out_adj.iter(&rtxn)? {
            let (src, bytes) = result?;
            if cached_src != Some(src) {
                cached_src = Some(src);
                src_labels = node_labels.get(&src).map(Vec::as_slice).unwrap_or_default();
            }
            let entry = AdjEntry::read_from_bytes(bytes)
                .ok()
                .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
            // Copied out before use: `AdjEntry` is `repr(packed)`, so borrowing a
            // field to pass it by reference is not allowed.
            let edge_type = entry.edge_type;
            let dst = entry.other;
            let dst_labels = node_labels.get(&dst).map(Vec::as_slice).unwrap_or_default();
            // Once per edge, not once per label pair, so it is comparable with the
            // live `stats:t:` counter the staleness check reads.
            *edges_by_type.entry(edge_type).or_insert(0) += 1;
            for &label in src_labels {
                *out_by_src_label.entry((label, edge_type)).or_insert(0) += 1;
            }
            for &label in dst_labels {
                *in_by_dst_label.entry((label, edge_type)).or_insert(0) += 1;
            }
            for &s in src_labels {
                for &d in dst_labels {
                    *triples.entry((s, edge_type, d)).or_insert(0) += 1;
                }
            }
        }

        Ok(Self {
            generation,
            edges_by_type,
            out_by_src_label,
            in_by_dst_label,
            triples,
        })
    }
}

impl Graph {
    /// Resolve label and type names to their ids, returning `None` when either
    /// is unknown to the registry (the caller then cannot decide on the schema).
    fn resolve_label_type(
        &self,
        label: &str,
        rel_type: &str,
    ) -> Result<Option<(LabelId, TypeId)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.resolve_label_type_in(&rtxn, label, rel_type)
    }

    /// [`Self::resolve_label_type`] against a transaction the caller already holds, so
    /// a question that also reads the graph resolves and reads one snapshot.
    fn resolve_label_type_in(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        rel_type: &str,
    ) -> Result<Option<(LabelId, TypeId)>, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(None),
        };
        let type_id = match get_type(&self.storage, rtxn, rel_type)? {
            Some(id) => id,
            None => return Ok(None),
        };
        Ok(Some((label_id, type_id)))
    }

    /// Run `f` against the cached schema table only when it reflects the current
    /// write generation, reporting `None` when it is absent or stale.
    ///
    /// This never builds the table. Building it is one full pass over the label
    /// index and one over the adjacency, and no caller needs it: two weight a plan
    /// choice and fall back to the global average, and [`Graph::schema_has_edge`]
    /// falls back to a bounded direct probe. Paying an `O(nodes + edges)` scan to
    /// sharpen an estimate made the first query mentioning a relationship pattern
    /// the slowest one in the process (measured at roughly 5 seconds on a 1 M-node,
    /// 13.9 M-edge graph), which is the same trade the property columns already
    /// resolved this way. [`Graph::materialize_edge_statistics`] is how a caller
    /// asks for the table deliberately.
    ///
    /// This is the strict reader, for [`Graph::schema_has_edge`] alone: a stale
    /// negative there is not advisory, because it would claim a triple is unrealized
    /// after a write created it and the optimizer prunes rows on that answer. The
    /// advisory readers use [`Self::with_possibly_stale_fanout`] instead.
    fn with_current_fanout<T>(&self, f: impl FnOnce(&EdgeFanout) -> T) -> Result<Option<T>, Error> {
        let guard = self.edge_fanout.lock();
        // Read the generation under the lock. Reading it first leaves a window in
        // which a write commits before the lock is taken, so a table predating that
        // write still matches the captured value and passes as current: exactly the
        // stale verdict this reader exists to refuse.
        let generation = self.csr_cache.current_gen();
        match guard.as_ref() {
            Some(table) if table.generation == generation => Ok(Some(f(table))),
            _ => Ok(None),
        }
    }

    /// Run `f` against the cached schema table, tolerating one the write generation
    /// has moved past as long as it still describes a graph of about this size.
    ///
    /// For the advisory readers only, and it never builds either. Tolerating
    /// staleness matters because the alternative is not a fresher estimate, it is no
    /// estimate: the caller falls back to the global average fan-out, which is
    /// cruder than a slightly dated per-label ratio. Refusing every stale table
    /// meant a process that had materialized lost its statistics on the first write
    /// and never got them back, so a long-lived server that ingests anything spent
    /// the rest of its life on default plan weights. That coupling existed only
    /// because [`Graph::schema_has_edge`] read the same table and could not tolerate
    /// staleness; it now answers without the table, which is what frees this reader.
    ///
    /// Unbounded staleness is not safe, though, and the bound is on growth rather
    /// than age: see [`STALE_FANOUT_GROWTH_FACTOR`]. The comparison is per
    /// relationship type, against the live `stats:t:` counter, because every estimate
    /// this reader serves is about one type. A global edge count cannot do the job: a
    /// skewed ingest that adds half a million edges of one type to a graph of a
    /// million stays well inside any global factor while moving that type's fan-out
    /// by orders of magnitude.
    ///
    /// What it still cannot see is a redistribution *within* a type: if `KNOWS` grows
    /// 100 to 150 but the new edges all leave a label that had one, that label's ratio
    /// is badly stale and this check passes. Catching that would need a live
    /// per-`(label, type)` counter, which is the table itself. The residual error is
    /// bounded in consequence rather than magnitude: it weights a plan, so the answer
    /// stays correct and only the plan may be poor, and a caller that has just
    /// reshaped its graph should re-materialize.
    ///
    /// Because the counter never decreases, this refuses a type that has grown past
    /// the factor and keeps one that has shrunk. That asymmetry is the useful
    /// direction. A stale count on a grown type understates fan-out, which invites the
    /// planner to treat an expensive expansion as free; on a shrunken one it
    /// overstates, and the planner is merely conservative.
    fn with_possibly_stale_fanout<T>(
        &self,
        rel_type: TypeId,
        f: impl FnOnce(&EdgeFanout) -> T,
    ) -> Result<Option<T>, Error> {
        let guard = self.edge_fanout.lock();
        // Under the lock, for the reason given in `with_current_fanout`.
        let generation = self.csr_cache.current_gen();
        let table = match guard.as_ref() {
            Some(table) => table,
            None => return Ok(None),
        };
        if table.generation == generation {
            return Ok(Some(f(table)));
        }
        // Only the stale path pays this read, so the common cases (no table, or a
        // current one) stay pure in-memory work on the planning path.
        let live = {
            let rtxn = self.storage.env.read_txn()?;
            get_type_count(&self.storage, &rtxn, rel_type)?
        };
        // A type with no edges at build time is described by nothing, and multiplying
        // its zero keeps it refused rather than trusted forever.
        let at_build = table.edges_by_type.get(&rel_type).copied().unwrap_or(0);
        if live > at_build.saturating_mul(STALE_FANOUT_GROWTH_FACTOR) {
            return Ok(None);
        }
        Ok(Some(f(table)))
    }

    /// Build the schema statistics table now, if it is not built and current
    /// already.
    ///
    /// Nothing builds it as a side effect (see [`Self::with_current_fanout`]), so
    /// this is the deliberate way to make the optimizer's expand-ratio estimates
    /// available, and to upgrade [`Graph::schema_has_edge`] from a budgeted probe to
    /// an exact lookup that also decides the cases the probe gives up on. It costs
    /// one full pass over the label index and one over the adjacency, and the result
    /// is cached until a committed write advances the generation.
    ///
    /// It is the counterpart of [`Graph::materialize_property_columns`]: that one
    /// warms the per-property statistics, this one warms the schema-level edge
    /// statistics. A caller wanting the optimizer at full strength on a cold graph
    /// wants both.
    ///
    /// The scan runs without the table lock held, so concurrent queries keep planning
    /// (on the probe and the global average) instead of blocking for its duration.
    /// Holding the lock across the build was tolerable while this was an internal
    /// lazy helper; it is not now that a caller is told to invoke it on a live graph.
    pub fn materialize_edge_statistics(&self) -> Result<(), Error> {
        let generation = {
            let guard = self.edge_fanout.lock();
            let generation = self.csr_cache.current_gen();
            if guard.as_ref().is_some_and(|t| t.generation == generation) {
                return Ok(());
            }
            generation
        };
        // Built outside the lock, and tagged with the generation observed before the
        // scan started. A write landing during the build makes the result stale on
        // arrival, which is a state every reader already handles, rather than a table
        // claiming to be newer than it is.
        let table = EdgeFanout::build(&self.storage, generation)?;
        let mut guard = self.edge_fanout.lock();
        // Another thread may have installed a table from a later generation while this
        // one scanned; keep the newer of the two.
        if guard.as_ref().is_some_and(|t| t.generation > generation) {
            return Ok(());
        }
        *guard = Some(table);
        Ok(())
    }

    /// Estimated average fan-out for expanding edges of `rel_type` from a node
    /// carrying `src_label`: the per-source-label typed out-degree, or the typed
    /// in-degree when `incoming` is true.
    ///
    /// Returns the count of qualifying edges divided by the count of
    /// `src_label` nodes. Returns `None` when the label or type is unknown, the
    /// label has no nodes, or no such edges exist, so the caller can fall back
    /// to the global average fan-out.
    ///
    /// Nothing builds the underlying table, so this also returns `None` on a graph
    /// where [`Graph::materialize_edge_statistics`] has not been called; a table the
    /// write generation has moved past is still served while that relationship type's
    /// edge count has not grown past [`STALE_FANOUT_GROWTH_FACTOR`]. Because the
    /// result only weights plan choices, a stale or absent estimate never affects
    /// query correctness.
    pub fn estimate_expand_fanout(
        &self,
        src_label: &str,
        rel_type: &str,
        incoming: bool,
    ) -> Result<Option<f64>, Error> {
        let (label_id, type_id) = match self.resolve_label_type(src_label, rel_type)? {
            Some(ids) => ids,
            None => return Ok(None),
        };
        let node_count = self.node_count_by_label(src_label)?;
        if node_count == 0 {
            return Ok(None);
        }
        self.with_possibly_stale_fanout(type_id, |table| {
            let map = if incoming {
                &table.in_by_dst_label
            } else {
                &table.out_by_src_label
            };
            match map.get(&(label_id, type_id)).copied() {
                Some(edges) if edges > 0 => Some(edges as f64 / node_count as f64),
                _ => None,
            }
        })
        .map(Option::flatten)
    }

    /// Destination-label-aware fan-out: the average number of `dst_label`
    /// neighbors reached by expanding edges of `rel_type` from a node carrying
    /// `src_label` (or the symmetric in-direction when `incoming`).
    ///
    /// This sharpens [`Graph::estimate_expand_fanout`] when the expansion target
    /// also carries a label, dividing the realized `(src_label, type, dst_label)`
    /// triple count by the `src_label` node count instead of the type marginal.
    /// Returns `None` (fall back to the marginal or the global average) when a
    /// label or type is unknown, the source label has no nodes, or no such
    /// triple exists.
    pub fn estimate_expand_fanout_to(
        &self,
        src_label: &str,
        rel_type: &str,
        dst_label: &str,
        incoming: bool,
    ) -> Result<Option<f64>, Error> {
        let (src_id, type_id) = match self.resolve_label_type(src_label, rel_type)? {
            Some(ids) => ids,
            None => return Ok(None),
        };
        let dst_id = {
            let rtxn = self.storage.env.read_txn()?;
            match get_label(&self.storage, &rtxn, dst_label)? {
                Some(id) => id,
                None => return Ok(None),
            }
        };
        let node_count = self.node_count_by_label(src_label)?;
        if node_count == 0 {
            return Ok(None);
        }
        // An outgoing expand traverses `src --type--> dst`; an incoming expand
        // from a `src_label` node reaches a `dst_label` node along the reversed
        // edge `dst --type--> src`, so the triple key swaps its endpoints.
        let key = if incoming {
            (dst_id, type_id, src_id)
        } else {
            (src_id, type_id, dst_id)
        };
        self.with_possibly_stale_fanout(type_id, |table| match table.triples.get(&key).copied() {
            Some(edges) if edges > 0 => Some(edges as f64 / node_count as f64),
            _ => None,
        })
        .map(Option::flatten)
    }

    /// Whether the data schema contains any directed edge `src_label --rel_type-->
    /// dst_label`. Returns `Some(false)` when the labels and type are all known but
    /// no such edge exists (the directed pattern is unsatisfiable), and `None` when
    /// the caller cannot decide: any of the three names is unknown to the registry,
    /// or the question could not be settled within [`SCHEMA_PROBE_BUDGET`].
    ///
    /// Unlike the fan-out estimates, this does not need the statistics table. A
    /// negative here prunes rows rather than weighting a choice, so answering only
    /// when a table happens to exist would leave the pass dormant on every graph
    /// nobody had materialized. When no current table is available the question is
    /// probed directly against the label index and the adjacency: the smaller
    /// endpoint population is walked and each of its `rel_type` edges tested for the
    /// opposite label, which settles on the first match and otherwise exhausts that
    /// population. A current table answers in one lookup instead, and decides the
    /// cases too large to probe.
    ///
    /// A `Some` answer is authoritative for committed state either way. The table is
    /// consulted only when it matches the current write generation, so a table left
    /// behind by an earlier generation is ignored rather than trusted, and the probe
    /// reads committed state directly. Callers that prune work on this answer must
    /// still guard against uncommitted same-statement writes, which neither path can
    /// see.
    pub fn schema_has_edge(
        &self,
        src_label: &str,
        rel_type: &str,
        dst_label: &str,
    ) -> Result<Option<bool>, Error> {
        // One read transaction for the whole question: the name resolution and the
        // probe then read one snapshot, and a two-label-by-two-label hop opens four
        // transactions during planning rather than twelve.
        let rtxn = self.storage.env.read_txn()?;
        let (src_id, type_id) = match self.resolve_label_type_in(&rtxn, src_label, rel_type)? {
            Some(ids) => ids,
            None => return Ok(None),
        };
        let dst_id = match get_label(&self.storage, &rtxn, dst_label)? {
            Some(id) => id,
            None => return Ok(None),
        };
        if let Some(answer) = self
            .with_current_fanout(|table| table.triples.contains_key(&(src_id, type_id, dst_id)))?
        {
            return Ok(Some(answer));
        }
        // A decided verdict for this generation, if one has already been probed. The
        // pass that asks this runs on every execution (there is no plan cache), so
        // without the memo an unsatisfiable hop re-pays the whole walk per query
        // rather than once per generation.
        let key = (src_id, type_id, dst_id);
        if let Some(memo) = self.cached_schema_probe(key) {
            return Ok(memo);
        }
        let verdict =
            self.probe_schema_edge(&rtxn, src_id, type_id, dst_id, SCHEMA_PROBE_BUDGET)?;
        self.memoize_schema_probe(key, verdict);
        Ok(verdict)
    }

    /// The memoized verdict for one triple, when it was decided under the current
    /// write generation. A `Some(None)` means "probed and undecided", which is worth
    /// remembering too: re-probing would spend the whole budget to reach the same
    /// answer, and a table installed since is consulted before this memo.
    fn cached_schema_probe(&self, key: (LabelId, TypeId, LabelId)) -> Option<Option<bool>> {
        let guard = self.schema_probes.lock();
        let generation = self.csr_cache.current_gen();
        if guard.0 != generation {
            return None;
        }
        guard.1.get(&key).copied()
    }

    /// Remember a probe verdict against the generation it was decided under,
    /// discarding everything remembered for an older one.
    fn memoize_schema_probe(&self, key: (LabelId, TypeId, LabelId), verdict: Option<bool>) {
        let mut guard = self.schema_probes.lock();
        let generation = self.csr_cache.current_gen();
        if guard.0 != generation {
            guard.0 = generation;
            guard.1.clear();
        }
        guard.1.insert(key, verdict);
    }

    /// Decide one schema triple by reading the label index and the adjacency,
    /// spending at most `budget` storage probes before reporting `None`.
    ///
    /// `budget` is a parameter rather than the constant so a test can pin the
    /// give-up behavior without building a graph large enough to exhaust the real
    /// one.
    fn probe_schema_edge(
        &self,
        rtxn: &heed::RoTxn,
        src_label: LabelId,
        rel_type: TypeId,
        dst_label: LabelId,
        budget: u64,
    ) -> Result<Option<bool>, Error> {
        // Walk the smaller population, since a negative has to exhaust whichever
        // side is walked. The stored node count is only a proxy for the adjacency
        // volume behind it and can be wrong in both directions (few nodes of high
        // degree, or a drifted counter); the budget is what bounds the work, so a
        // bad guess costs an undecided answer rather than an unbounded scan. This
        // is the only thing the counter is trusted for here.
        let src_count = get_label_count(&self.storage, rtxn, src_label)?;
        let dst_count = get_label_count(&self.storage, rtxn, dst_label)?;
        let (walk_label, other_label, outgoing) = if src_count <= dst_count {
            (src_label, dst_label, true)
        } else {
            (dst_label, src_label, false)
        };
        // An opposite population with no members cannot match any edge, so the
        // question is settled without walking anything. Asked of `label_idx` rather
        // than the counter, because a negative here prunes rows and so must not
        // depend on a counter being exact. The walked side needs no such check: an
        // empty one yields no nodes and falls out of the loop below as `Some(false)`.
        let other_prefix = other_label.to_be_bytes();
        if self
            .storage
            .label_idx
            .prefix_iter(rtxn, &other_prefix)?
            .next()
            .is_none()
        {
            return Ok(Some(false));
        }
        let adj = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };
        let mut budget = budget;
        let prefix = walk_label.to_be_bytes();
        for result in self.storage.label_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = result?;
            let (_, node) = split_label_key(key)?;
            // Charged before the adjacency lookup, because visiting a node costs an
            // index step and a `get_duplicates` whether or not it has any adjacency in
            // this direction. Leaving those uncharged made the walk unbounded for
            // exactly the population most likely to be walked: a small label whose
            // nodes have no edges in the direction under test, where every node hit
            // `continue` without spending anything.
            if budget == 0 {
                return Ok(None);
            }
            budget -= 1;
            let dups = match adj.get_duplicates(rtxn, &node)? {
                Some(iter) => iter,
                None => continue,
            };
            for result in dups {
                let (_, bytes) = result?;
                if budget == 0 {
                    return Ok(None);
                }
                budget -= 1;
                let entry = AdjEntry::read_from_bytes(bytes)
                    .ok()
                    .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
                if entry.edge_type != rel_type {
                    continue;
                }
                // The opposite endpoint's label decides it. Charged to the same
                // budget as the adjacency read, because this lookup is the more
                // expensive of the two and a hop whose every edge carries the type
                // would otherwise spend an uncounted one per edge.
                if budget == 0 {
                    return Ok(None);
                }
                budget -= 1;
                if self
                    .storage
                    .label_idx
                    .get(rtxn, &composite_key(other_label, entry.other))?
                    .is_some()
                {
                    return Ok(Some(true));
                }
            }
        }
        Ok(Some(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_graph() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    #[test]
    fn expand_fanout_is_per_source_label() {
        let (_dir, graph) = open_graph();

        // Three Person nodes and one City node. The global average fan-out would
        // divide by all four nodes; the per-label ratio divides only by the
        // Person count, so the two models disagree.
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let p2 = graph.add_node("Person", &json!({})).unwrap();
        let c0 = graph.add_node("City", &json!({})).unwrap();

        // Two KNOWS edges, both leaving p0; one VISITED edge from p1 to c0.
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        graph.add_edge(p0, p2, "KNOWS", &json!({})).unwrap();
        graph.add_edge(p1, c0, "VISITED", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        // KNOWS out of Person: 2 edges / 3 Person nodes.
        let knows = graph
            .estimate_expand_fanout("Person", "KNOWS", false)
            .unwrap();
        assert_eq!(knows, Some(2.0 / 3.0));

        // VISITED out of Person: 1 edge / 3 Person nodes.
        let visited = graph
            .estimate_expand_fanout("Person", "VISITED", false)
            .unwrap();
        assert_eq!(visited, Some(1.0 / 3.0));

        // VISITED into City: 1 incoming edge / 1 City node.
        let visited_in = graph
            .estimate_expand_fanout("City", "VISITED", true)
            .unwrap();
        assert_eq!(visited_in, Some(1.0));

        // A City has no outgoing KNOWS, so the caller falls back to the global
        // model rather than treating the fan-out as zero.
        let city_knows = graph
            .estimate_expand_fanout("City", "KNOWS", false)
            .unwrap();
        assert_eq!(city_knows, None);

        // Unknown label and unknown type both fall back.
        assert_eq!(
            graph
                .estimate_expand_fanout("Ghost", "KNOWS", false)
                .unwrap(),
            None
        );
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "GHOST", false)
                .unwrap(),
            None
        );
    }

    /// A write never silently rebuilds the table, and the two readers diverge on what
    /// they do with the one it left behind: the advisory estimate keeps serving the
    /// dated ratio, the schema question does not touch it. Materializing again brings
    /// both onto the write.
    ///
    /// The middle step is the point. Serving the stale table to `schema_has_edge`
    /// would let it deny a triple the write just created, and the optimizer drops rows
    /// on that answer; the probe answers it against committed state instead.
    #[test]
    fn a_write_does_not_rebuild_the_table() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        // The City label must already be in the registry, or the schema question
        // below is undecidable for that reason rather than for the table's state.
        let c0 = graph.add_node("City", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        // One KNOWS edge over two Person nodes.
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(0.5)
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(false),
            "a built table decides the unrealized triple"
        );

        // Realizing that triple advances the write generation without rebuilding
        // anything. The advisory estimate keeps the dated ratio (one edge over two
        // Person nodes, not the two edges now present), while the schema question
        // ignores the table the write moved past: it would still deny this triple.
        graph.add_edge(p0, c0, "KNOWS", &json!({})).unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(0.5)
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(true),
            "a stale table must not deny a triple the write just realized"
        );

        // Materializing again picks the write up.
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(1.0)
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(true)
        );
    }

    /// No reader builds the table, not even the one whose answer is not advisory:
    /// on a graph nobody has materialized, the fan-out estimates decline (leaving
    /// the caller on the global model) while the schema questions are still decided,
    /// by the probe rather than by a full scan.
    #[test]
    fn no_reader_builds_the_table() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        // Registers the City label, so the unrealized question below is decidable
        // rather than undecidable for an unknown name.
        let _city = graph.add_node("City", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();

        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            None
        );
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "Person", false)
                .unwrap(),
            None
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Person").unwrap(),
            Some(true),
            "the probe decides a realized triple with no table"
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(false),
            "the probe decides an unrealized triple with no table"
        );
        assert!(
            graph.edge_fanout.lock().is_none(),
            "reading must not have populated the table as a side effect"
        );

        // Materializing changes which path answers, not what it answers.
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Person").unwrap(),
            Some(true)
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(false)
        );
    }

    /// The probe and the table agree on a graph shaped to stress the counting
    /// rules: a multi-label endpoint (which realizes one triple per label it
    /// carries), an unlabeled endpoint (which realizes none), a self-loop, and
    /// parallel edges.
    ///
    /// The two paths read different stores to answer the same question, so this is
    /// what keeps them from drifting apart.
    #[test]
    fn probe_and_table_agree() {
        let (_dir, graph) = open_graph();
        let hybrid = graph
            .add_node_multi(&["Person", "Robot"], &json!({}))
            .unwrap();
        let person = graph.add_node("Person", &json!({})).unwrap();
        let city = graph.add_node("City", &json!({})).unwrap();
        let bare = graph.add_node_multi(&[], &json!({})).unwrap();

        // A self-loop, parallel edges over the same pair, an edge into an unlabeled
        // node, and one realized (Person, LIVES_IN, City).
        graph.add_edge(hybrid, hybrid, "KNOWS", &json!({})).unwrap();
        graph.add_edge(hybrid, person, "KNOWS", &json!({})).unwrap();
        graph.add_edge(hybrid, person, "KNOWS", &json!({})).unwrap();
        graph.add_edge(person, bare, "KNOWS", &json!({})).unwrap();
        graph
            .add_edge(person, city, "LIVES_IN", &json!({}))
            .unwrap();

        let questions = [
            ("Person", "KNOWS", "Person"),
            ("Person", "KNOWS", "Robot"),
            ("Robot", "KNOWS", "Person"),
            ("Robot", "KNOWS", "Robot"),
            ("Person", "KNOWS", "City"),
            ("City", "KNOWS", "Person"),
            ("Person", "LIVES_IN", "City"),
            ("City", "LIVES_IN", "Person"),
            ("Robot", "LIVES_IN", "City"),
        ];

        let probed: Vec<_> = questions
            .iter()
            .map(|&(s, t, d)| graph.schema_has_edge(s, t, d).unwrap())
            .collect();
        assert!(
            graph.edge_fanout.lock().is_none(),
            "the probe must not have built the table"
        );

        graph.materialize_edge_statistics().unwrap();
        let tabled: Vec<_> = questions
            .iter()
            .map(|&(s, t, d)| graph.schema_has_edge(s, t, d).unwrap())
            .collect();

        assert_eq!(probed, tabled, "probe and table disagree on {questions:?}");
        // Pinned outright so the agreement above cannot be two paths sharing one
        // mistake. The multi-label source realizes the triple under both its
        // labels; nothing KNOWS a City; and LIVES_IN runs Person to City only.
        assert_eq!(
            tabled,
            vec![
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            ]
        );
    }

    /// A question the budget cannot settle reports `None` rather than guessing, and
    /// every storage operation the walk performs is charged against it.
    ///
    /// `None` is the sound direction: the caller keeps every row. A budget too small
    /// to reach the one matching edge must not be read as "no such edge", which is
    /// what the optimizer prunes on.
    ///
    /// The three steps below are the three charged operations for one node carrying
    /// one matching edge: visiting the node, reading the adjacency entry, and looking
    /// up the far endpoint's label. Visiting is charged first and separately because
    /// it is what bounds a walk over nodes with *no* adjacency in the direction under
    /// test, which is otherwise free and unbounded.
    #[test]
    fn an_exhausted_probe_budget_declines() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();

        let (person, knows) = graph
            .resolve_label_type("Person", "KNOWS")
            .unwrap()
            .unwrap();
        let probe = |budget: u64| {
            let rtxn = graph.storage.env.read_txn().unwrap();
            graph
                .probe_schema_edge(&rtxn, person, knows, person, budget)
                .unwrap()
        };

        // Zero cannot afford to visit the first node.
        assert_eq!(probe(0), None);
        // One visits it but cannot read its adjacency entry.
        assert_eq!(probe(1), None);
        // Two read the entry but cannot afford the label lookup that confirms the far
        // endpoint.
        assert_eq!(probe(2), None);
        // Three settle it.
        assert_eq!(probe(3), Some(true));
    }

    /// Walking a population whose nodes have no adjacency in the direction under test
    /// spends budget per node, so the give-up is reached instead of the walk running
    /// to the end of the label for free.
    ///
    /// This is the case the accounting missed: every such node hit the "no adjacency"
    /// path, which charged nothing, so an index step and a `get_duplicates` per node
    /// were unbounded on the planning path. A budget of one must therefore decline
    /// here rather than exhaust three nodes and answer.
    #[test]
    fn visiting_edgeless_nodes_spends_the_probe_budget() {
        let (_dir, graph) = open_graph();
        // Three City nodes with no outgoing edges, and a Person population large
        // enough that City is the side chosen for the walk.
        for _ in 0..3 {
            graph.add_node("City", &json!({})).unwrap();
        }
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let p2 = graph.add_node("Person", &json!({})).unwrap();
        let p3 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        let _ = (p2, p3);

        let (city, knows) = graph.resolve_label_type("City", "KNOWS").unwrap().unwrap();
        let person = graph
            .resolve_label_type("Person", "KNOWS")
            .unwrap()
            .unwrap()
            .0;
        let probe = |budget: u64| {
            let rtxn = graph.storage.env.read_txn().unwrap();
            graph
                .probe_schema_edge(&rtxn, city, knows, person, budget)
                .unwrap()
        };

        // Visiting three edgeless nodes costs three, so anything less declines.
        assert_eq!(probe(1), None);
        assert_eq!(probe(2), None);
        // Three exhausts the population and proves the triple absent.
        assert_eq!(probe(3), Some(false));
    }

    /// An endpoint label with no nodes settles the question without spending any
    /// budget at all: no node can carry the label, so no edge can match.
    #[test]
    fn an_empty_endpoint_population_decides_immediately() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        // Registered by creating and deleting, so the name resolves but the
        // population is empty.
        let ghost = graph.add_node("Ghost", &json!({})).unwrap();
        graph.delete_node(ghost).unwrap();

        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Ghost").unwrap(),
            Some(false)
        );
        let (person, knows) = graph
            .resolve_label_type("Person", "KNOWS")
            .unwrap()
            .unwrap();
        let ghost_label = graph
            .resolve_label_type("Ghost", "KNOWS")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(
            graph
                .probe_schema_edge(
                    &graph.storage.env.read_txn().unwrap(),
                    person,
                    knows,
                    ghost_label,
                    0
                )
                .unwrap(),
            Some(false),
            "deciding on the population must not need any budget"
        );
    }

    /// A write past a materialized table leaves the advisory estimate serving the
    /// stale value while `schema_has_edge` answers from committed state.
    ///
    /// This is the split stated as one assertion. The estimate keeps a dated ratio,
    /// because losing it entirely would drop the caller to the global average, which
    /// is worse information. The schema question cannot keep a dated answer, because
    /// the optimizer prunes rows on it.
    #[test]
    fn a_stale_table_still_serves_the_advisory_estimate() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let city = graph.add_node("City", &json!({})).unwrap();
        // Four edges over two Person nodes, so the ratio is 2.0 and the edge-id
        // high-water mark is 4.
        for _ in 0..4 {
            graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        }
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(2.0)
        );

        // One more edge advances the generation without growing the graph past the
        // factor (5 <= 4 * 2), and it realizes a triple the table does not have.
        graph.add_edge(p0, city, "KNOWS", &json!({})).unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(2.0),
            "the stale ratio is served, not recomputed: five edges would give 2.5"
        );
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(true),
            "the schema question does not read the stale table"
        );
    }

    /// Growth past the factor refuses the stale table, so an estimate cannot outlive
    /// the graph it describes.
    ///
    /// Without this, a process that materialized at startup and then ingested would
    /// keep planning against the startup snapshot forever, understating fan-out by
    /// whatever it grew by. That is worse than the global average it falls back to.
    #[test]
    fn growth_past_the_factor_refuses_the_stale_table() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        for _ in 0..2 {
            graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        }
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(1.0)
        );

        // Three more KNOWS edges puts that type at 5, past 2 * the factor.
        for _ in 0..3 {
            graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        }
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            None,
            "a table describing a type this much smaller is refused"
        );
        assert!(
            graph.edge_fanout.lock().is_some(),
            "refusing to serve it must not have dropped or rebuilt it"
        );

        // Materializing again brings it back, now describing five edges.
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(2.5)
        );
    }

    /// The staleness bound is per relationship type, so growth in one type does not
    /// invalidate an estimate about another, and growth *within* a type is caught even
    /// when the graph as a whole barely moved.
    ///
    /// A single global edge count cannot do this: the writes below grow one type
    /// fourfold while the graph grows by a third, so a global factor of two would
    /// serve a KNOWS estimate that is four times too small.
    #[test]
    fn the_staleness_bound_is_per_relationship_type() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let c0 = graph.add_node("City", &json!({})).unwrap();
        // Nine LIVES_IN edges and one KNOWS: ten edges in total.
        for _ in 0..9 {
            graph.add_edge(p0, c0, "LIVES_IN", &json!({})).unwrap();
        }
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        // Three more KNOWS: that type goes 1 -> 4 while the graph goes 10 -> 13.
        for _ in 0..3 {
            graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        }
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            None,
            "the type quadrupled, so its stale estimate is refused"
        );
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "LIVES_IN", false)
                .unwrap(),
            Some(4.5),
            "LIVES_IN did not grow, so its estimate is still served"
        );
    }

    /// A type with no edges at build time is described by nothing, so it is refused
    /// rather than trusted forever by a zero that no factor can grow.
    #[test]
    fn a_type_absent_at_build_time_is_refused_once_it_has_edges() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        // Registers KNOWS with one edge, then materialize, then a second type appears.
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        graph.add_edge(p0, p1, "LIVES_IN", &json!({})).unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "LIVES_IN", false)
                .unwrap(),
            None
        );
    }

    /// A decided probe verdict is remembered for its write generation and forgotten
    /// when a write advances it.
    ///
    /// The pass that asks runs on every execution, so without the memo an
    /// unsatisfiable hop re-walks the graph per query. The memo must not outlive its
    /// generation, or it would answer for a graph a write has changed.
    #[test]
    fn a_probe_verdict_is_memoized_per_generation() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let c0 = graph.add_node("City", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();

        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(false)
        );
        assert_eq!(
            graph.schema_probes.lock().1.len(),
            1,
            "the verdict is remembered"
        );
        // Repeating the question is served from the memo, not a second walk.
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(false)
        );
        assert_eq!(graph.schema_probes.lock().1.len(), 1);

        // Realizing the triple advances the generation, so the remembered negative
        // must not be served.
        graph.add_edge(p0, c0, "KNOWS", &json!({})).unwrap();
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "City").unwrap(),
            Some(true),
            "a memo from an earlier generation must not deny a realized triple"
        );
    }

    /// Materializing decides a question the probe gives up on.
    ///
    /// This is the property that makes the warm-up worth calling on a large graph, and
    /// it is not observable through the public API alone, since the real budget is far
    /// larger than a unit-test graph. Injecting a budget of zero stands in for a
    /// population too large to exhaust.
    #[test]
    fn the_table_decides_what_the_probe_gives_up_on() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        let (person, knows) = graph
            .resolve_label_type("Person", "KNOWS")
            .unwrap()
            .unwrap();

        // With no budget the probe cannot decide.
        {
            let rtxn = graph.storage.env.read_txn().unwrap();
            assert_eq!(
                graph
                    .probe_schema_edge(&rtxn, person, knows, person, 0)
                    .unwrap(),
                None
            );
        }
        // A current table answers the same question outright, without probing.
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Person").unwrap(),
            Some(true)
        );
        assert!(
            graph.schema_probes.lock().1.is_empty(),
            "the table path must not have probed at all"
        );
    }

    /// The label index, not the node record, is what the build reads, so a label
    /// added or removed after a node was created is reflected in the statistics.
    #[test]
    fn build_follows_later_label_changes() {
        let (_dir, graph) = open_graph();
        let a = graph.add_node("Person", &json!({})).unwrap();
        let b = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(a, b, "KNOWS", &json!({})).unwrap();

        graph.add_label(b, "Admin").unwrap();
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Admin").unwrap(),
            Some(true),
            "the added label realizes a new triple"
        );
        assert_eq!(
            graph
                .estimate_expand_fanout("Admin", "KNOWS", true)
                .unwrap(),
            Some(1.0),
            "one incoming KNOWS over one Admin node"
        );

        graph.remove_label(b, "Admin").unwrap();
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Admin").unwrap(),
            Some(false),
            "removing the label unrealizes it"
        );
    }

    #[test]
    fn schema_has_edge_reflects_realized_triples() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let c0 = graph.add_node("City", &json!({})).unwrap();

        // Person KNOWS Person, and Person LIVES_IN City. No City ever has an
        // outgoing KNOWS, and no Person LIVES_IN a Person.
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        graph.add_edge(p0, c0, "LIVES_IN", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        assert_eq!(
            graph.schema_has_edge("Person", "KNOWS", "Person").unwrap(),
            Some(true)
        );
        assert_eq!(
            graph.schema_has_edge("Person", "LIVES_IN", "City").unwrap(),
            Some(true)
        );
        // Realized in neither the data nor the schema: a provably empty pattern.
        assert_eq!(
            graph.schema_has_edge("City", "KNOWS", "Person").unwrap(),
            Some(false)
        );
        assert_eq!(
            graph
                .schema_has_edge("Person", "LIVES_IN", "Person")
                .unwrap(),
            Some(false)
        );
        // Unknown label or type yields an undecidable answer, never a false prune.
        assert_eq!(
            graph.schema_has_edge("Ghost", "KNOWS", "Person").unwrap(),
            None
        );
        assert_eq!(
            graph.schema_has_edge("Person", "GHOST", "Person").unwrap(),
            None
        );
    }

    #[test]
    fn expand_fanout_to_uses_destination_label() {
        let (_dir, graph) = open_graph();
        // p0 KNOWS one Person and two Cities. The marginal KNOWS fan-out mixes
        // both targets; the destination-aware fan-out separates them.
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        let c0 = graph.add_node("City", &json!({})).unwrap();
        let c1 = graph.add_node("City", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();
        graph.add_edge(p0, c0, "KNOWS", &json!({})).unwrap();
        graph.add_edge(p0, c1, "KNOWS", &json!({})).unwrap();
        graph.materialize_edge_statistics().unwrap();

        // Two Person nodes (p0, p1); the marginal KNOWS fan-out is 3 edges / 2.
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(1.5)
        );
        // Of those edges, one targets a Person and two target a City, each over
        // the same two Person sources.
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "Person", false)
                .unwrap(),
            Some(0.5)
        );
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "City", false)
                .unwrap(),
            Some(1.0)
        );
        // A schema-absent destination falls back rather than reporting zero. The
        // rematerialize is load-bearing: adding the node advances the write
        // generation, and without it every reader would decline for that reason
        // instead, making this assertion pass without testing anything.
        let p2 = graph.add_node("Robot", &json!({})).unwrap();
        let _ = p2;
        graph.materialize_edge_statistics().unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "Person", false)
                .unwrap(),
            Some(0.5),
            "the table is current, so a decline below is about the schema"
        );
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "Robot", false)
                .unwrap(),
            None
        );
    }
}
