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

/// Per-`(label, type)` edge frequencies in both directions, tagged with the
/// committed-write generation they were built at.
pub(crate) struct EdgeFanout {
    /// The `csr_cache` write generation this table reflects.
    generation: u64,
    /// Count of edges of a type whose source node carries a label.
    out_by_src_label: AHashMap<(LabelId, TypeId), u64>,
    /// Count of edges of a type whose target node carries a label.
    in_by_dst_label: AHashMap<(LabelId, TypeId), u64>,
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
        for result in storage.edges.iter(&rtxn)? {
            let (_edge_id, bytes) = result?;
            let rec: EdgeRecord = props::decode(bytes)?;
            if let Some(labels) = node_labels.get(&rec.src) {
                for &label in labels {
                    *out_by_src_label.entry((label, rec.edge_type)).or_insert(0) += 1;
                }
            }
            if let Some(labels) = node_labels.get(&rec.dst) {
                for &label in labels {
                    *in_by_dst_label.entry((label, rec.edge_type)).or_insert(0) += 1;
                }
            }
        }

        Ok(Self {
            generation,
            out_by_src_label,
            in_by_dst_label,
        })
    }
}

impl Graph {
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
        let (label_id, type_id) = {
            let rtxn = self.storage.env.read_txn()?;
            let label_id = match get_label(&self.storage, &rtxn, src_label)? {
                Some(id) => id,
                None => return Ok(None),
            };
            let type_id = match get_type(&self.storage, &rtxn, rel_type)? {
                Some(id) => id,
                None => return Ok(None),
            };
            (label_id, type_id)
        };

        let node_count = self.node_count_by_label(src_label)?;
        if node_count == 0 {
            return Ok(None);
        }

        let generation = self.csr_cache.current_gen();
        let mut guard = self.edge_fanout.lock();
        let stale = guard
            .as_ref()
            .map(|f| f.generation != generation)
            .unwrap_or(true);
        if stale {
            *guard = Some(EdgeFanout::build(&self.storage, generation)?);
        }
        let table = guard.as_ref().expect("fanout table was just populated");
        let map = if incoming {
            &table.in_by_dst_label
        } else {
            &table.out_by_src_label
        };
        match map.get(&(label_id, type_id)).copied() {
            Some(edges) if edges > 0 => Ok(Some(edges as f64 / node_count as f64)),
            _ => Ok(None),
        }
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
}
