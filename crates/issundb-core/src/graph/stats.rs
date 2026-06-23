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
//! types), recomputed by one full scan and cached against the committed-write
//! generation, so it is refreshed only when writes advance past the cached
//! value. Estimates only drive plan weights, so a stale table never affects
//! correctness.

use ahash::AHashMap;

use crate::{
    error::Error,
    schema::{EdgeRecord, LabelId, NodeId, NodeRecord, TypeId},
    storage::{
        ids::{get_label, get_type},
        lmdb::Storage,
        props,
    },
};

use super::Graph;

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
    /// Build the frequency table from one pass over the node labels and one over
    /// the edges. A node with multiple labels contributes to each of its labels,
    /// matching the label-index semantics where such a node appears in every
    /// matching label scan.
    fn build(storage: &Storage, generation: u64) -> Result<Self, Error> {
        let rtxn = storage.env.read_txn()?;

        let mut node_labels: AHashMap<NodeId, Vec<LabelId>> = AHashMap::new();
        for result in storage.nodes.iter(&rtxn)? {
            let (id, bytes) = result?;
            let rec: NodeRecord = props::decode(bytes)?;
            if !rec.labels.is_empty() {
                node_labels.insert(id, rec.labels);
            }
        }

        let mut out_by_src_label: AHashMap<(LabelId, TypeId), u64> = AHashMap::new();
        let mut in_by_dst_label: AHashMap<(LabelId, TypeId), u64> = AHashMap::new();
        let mut triples: AHashMap<(LabelId, TypeId, LabelId), u64> = AHashMap::new();
        for result in storage.edges.iter(&rtxn)? {
            let (_edge_id, bytes) = result?;
            let rec: EdgeRecord = props::decode(bytes)?;
            let src_labels = node_labels.get(&rec.src);
            let dst_labels = node_labels.get(&rec.dst);
            if let Some(labels) = src_labels {
                for &label in labels {
                    *out_by_src_label.entry((label, rec.edge_type)).or_insert(0) += 1;
                }
            }
            if let Some(labels) = dst_labels {
                for &label in labels {
                    *in_by_dst_label.entry((label, rec.edge_type)).or_insert(0) += 1;
                }
            }
            if let (Some(srcs), Some(dsts)) = (src_labels, dst_labels) {
                for &s in srcs {
                    for &d in dsts {
                        *triples.entry((s, rec.edge_type, d)).or_insert(0) += 1;
                    }
                }
            }
        }

        Ok(Self {
            generation,
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
        let label_id = match get_label(&self.storage, &rtxn, label)? {
            Some(id) => id,
            None => return Ok(None),
        };
        let type_id = match get_type(&self.storage, &rtxn, rel_type)? {
            Some(id) => id,
            None => return Ok(None),
        };
        Ok(Some((label_id, type_id)))
    }

    /// Run `f` against the cached schema table, rebuilding it first when
    /// committed writes have advanced past the cached generation.
    fn with_fanout<T>(&self, f: impl FnOnce(&EdgeFanout) -> T) -> Result<T, Error> {
        let generation = self.csr_cache.current_gen();
        let mut guard = self.edge_fanout.lock();
        let fresh = guard.as_ref().is_some_and(|t| t.generation == generation);
        if fresh {
            if let Some(table) = guard.as_ref() {
                return Ok(f(table));
            }
        }
        // Stale or absent: rebuild, run the closure against the new table, then
        // cache it. Computing the result before storing keeps the helper
        // panic-free (no `expect` on the just-populated guard).
        let table = EdgeFanout::build(&self.storage, generation)?;
        let result = f(&table);
        *guard = Some(table);
        Ok(result)
    }

    /// Estimated average fan-out for expanding edges of `rel_type` from a node
    /// carrying `src_label`: the per-source-label typed out-degree, or the typed
    /// in-degree when `incoming` is true.
    ///
    /// Returns the count of qualifying edges divided by the count of
    /// `src_label` nodes. Returns `None` when the label or type is unknown, the
    /// label has no nodes, or no such edges exist, so the caller can fall back
    /// to the global average fan-out. The underlying frequency table is
    /// recomputed lazily when committed writes advance past the cached
    /// generation; because the result only weights plan choices, a stale or
    /// absent estimate never affects query correctness.
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
        self.with_fanout(|table| {
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
        self.with_fanout(|table| match table.triples.get(&key).copied() {
            Some(edges) if edges > 0 => Some(edges as f64 / node_count as f64),
            _ => None,
        })
    }

    /// Whether the data schema contains any directed edge `src_label --rel_type-->
    /// dst_label`. Returns `Some(false)` when the labels and type are all known
    /// but no such edge exists (the directed pattern is unsatisfiable), and
    /// `None` when any of the three names is unknown to the registry, so the
    /// caller cannot decide.
    ///
    /// The underlying schema table reflects all committed writes (it is rebuilt
    /// when the write generation advances), so a `Some(false)` is authoritative
    /// for committed state. Callers that prune work on this answer must guard
    /// against uncommitted same-statement writes, which the table cannot see.
    pub fn schema_has_edge(
        &self,
        src_label: &str,
        rel_type: &str,
        dst_label: &str,
    ) -> Result<Option<bool>, Error> {
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
        self.with_fanout(|table| Some(table.triples.contains_key(&(src_id, type_id, dst_id))))
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

    #[test]
    fn expand_fanout_refreshes_after_writes() {
        let (_dir, graph) = open_graph();
        let p0 = graph.add_node("Person", &json!({})).unwrap();
        let p1 = graph.add_node("Person", &json!({})).unwrap();
        graph.add_edge(p0, p1, "KNOWS", &json!({})).unwrap();

        // One KNOWS edge over two Person nodes.
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(0.5)
        );

        // Adding another KNOWS edge advances the write generation, so the cached
        // table is rebuilt on the next query.
        graph.add_edge(p1, p0, "KNOWS", &json!({})).unwrap();
        assert_eq!(
            graph
                .estimate_expand_fanout("Person", "KNOWS", false)
                .unwrap(),
            Some(1.0)
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
        // A schema-absent destination falls back rather than reporting zero.
        let p2 = graph.add_node("Robot", &json!({})).unwrap();
        let _ = p2;
        assert_eq!(
            graph
                .estimate_expand_fanout_to("Person", "KNOWS", "Robot", false)
                .unwrap(),
            None
        );
    }
}
