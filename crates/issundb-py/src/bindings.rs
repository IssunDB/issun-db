//! Python `IssunDB` class. The module is gated behind the `extension-module`
//! feature by `lib.rs`, so this file carries no feature attribute of its own.
//!
//! Every method releases the GIL around the native engine call
//! (`Python::detach`), so a long-running query, backup, or reindex does not
//! stall other Python threads and a pending `KeyboardInterrupt` is delivered
//! as soon as the call returns. Arguments are extracted to owned Rust values
//! before the release and results are serialized to JSON strings inside it,
//! so the released section never touches a Python object.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use issundb::{
    FusionStrategy, Graph, GraphQueryExt, HybridRetrieveOptions, Language, RetrievalError,
    TextGraphExt, TextIndexExt, TextSearchOptions, VectorGraphExt, VectorIndexOptions,
    VectorMetric, VectorQuantization, VectorSearchOptions, retrieve_hybrid,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAny;

/// Map any displayable error into a Python `RuntimeError`.
fn rt(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Map any displayable error into a Python `ValueError` (for bad arguments).
fn val(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// Parse a property-map argument, raising `ValueError` on malformed JSON or on
/// JSON that is not an object.
///
/// The shape check matters as much as the syntax check. A property bag is a map,
/// and the engine stores whatever it is given: passing `[1,2,3]` or `5` created a
/// node that took an id and counted in a label scan but had no readable properties
/// and could never match a property predicate, with no error at any layer. The
/// binding already required an object for `query` parameters and for
/// `vector_search` filters; properties were the inconsistent case.
fn parse_props(s: &str) -> PyResult<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(s).map_err(val)?;
    if !value.is_object() {
        let kind = match &value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "a boolean",
            serde_json::Value::Number(_) => "a number",
            serde_json::Value::String(_) => "a string",
            serde_json::Value::Array(_) => "an array",
            serde_json::Value::Object(_) => unreachable!(),
        };
        return Err(val(format!(
            "props must be a JSON object, got {kind}; a non-object would be stored \
             but no property read or predicate could ever see it"
        )));
    }
    Ok(value)
}

/// Python-facing handle for an IssunDB graph database.
#[pyclass(name = "IssunDB")]
pub struct PyGraph {
    graph: Graph,
}

#[pymethods]
impl PyGraph {
    /// Open or create an IssunDB graph at `path`, specifying optional LMDB map size in GB.
    #[new]
    #[pyo3(signature = (path, map_size_gb=None))]
    fn new(py: Python<'_>, path: &str, map_size_gb: Option<usize>) -> PyResult<Self> {
        let size = map_size_gb.unwrap_or(1);
        let graph = py
            .detach(|| Graph::open(Path::new(path), size))
            .map_err(rt)?;
        Ok(Self { graph })
    }

    /// Insert a node with one or more labels and JSON-encoded `props`. Returns the
    /// new node ID. `labels` accepts either a single label string or a list of
    /// label strings (multi-label node).
    fn add_node(&self, py: Python<'_>, labels: &Bound<'_, PyAny>, props: &str) -> PyResult<u64> {
        let value = parse_props(props)?;
        if let Ok(single) = labels.extract::<String>() {
            py.detach(|| self.graph.add_node(&single, &value))
                .map_err(rt)
        } else {
            let multi: Vec<String> = labels
                .extract()
                .map_err(|_| val("labels must be a string or a list of strings"))?;
            py.detach(|| {
                let refs: Vec<&str> = multi.iter().map(String::as_str).collect();
                self.graph.add_node_multi(&refs, &value)
            })
            .map_err(rt)
        }
    }

    /// Insert many nodes in one write transaction, returning the new ids in
    /// order. `items` is an iterable of `(labels, props)` pairs, where `labels`
    /// is a label string or a list of label strings and `props` is a JSON string.
    ///
    /// An auto-commit insert costs one durable commit per node, so a Python loop
    /// over `add_node` is bound by commit latency rather than by the work. This
    /// writes the whole batch under one transaction, and is all-or-nothing: any
    /// failure rolls back every node in the batch.
    fn add_nodes(&self, py: Python<'_>, items: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        // Extract every argument to owned Rust values before releasing the GIL.
        let mut parsed: Vec<(Vec<String>, serde_json::Value)> = Vec::new();
        for item in items.try_iter()? {
            let item = item?;
            let (labels, props): (Bound<'_, PyAny>, String) = item
                .extract()
                .map_err(|_| val("each item must be a (labels, props_json) pair"))?;
            let labels = match labels.extract::<String>() {
                Ok(single) => vec![single],
                Err(_) => labels
                    .extract::<Vec<String>>()
                    .map_err(|_| val("labels must be a string or a list of strings"))?,
            };
            parsed.push((labels, parse_props(&props)?));
        }

        py.detach(|| {
            self.graph.update(|txn| {
                let mut ids = Vec::with_capacity(parsed.len());
                for (labels, props) in &parsed {
                    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    ids.push(txn.add_node_multi(&refs, props)?);
                }
                Ok(ids)
            })
        })
        .map_err(rt)
    }

    /// Return the JSON-encoded properties of node `id`, or `None` if the node does not exist.
    fn get_node(&self, py: Python<'_>, id: u64) -> PyResult<Option<String>> {
        py.detach(|| match self.graph.get_node(id).map_err(rt)? {
            None => Ok(None),
            Some(r) => {
                let value: serde_json::Value = rmp_serde::from_slice(&r.props).map_err(rt)?;
                Ok(Some(serde_json::to_string(&value).map_err(rt)?))
            }
        })
    }

    /// Replace the properties of node `id` with JSON-encoded `props`.
    fn update_node(&self, py: Python<'_>, id: u64, props: &str) -> PyResult<()> {
        let value = parse_props(props)?;
        py.detach(|| self.graph.update_node(id, &value)).map_err(rt)
    }

    /// Delete node `id` and all of its incident edges.
    fn delete_node(&self, py: Python<'_>, id: u64) -> PyResult<()> {
        py.detach(|| self.graph.delete_node(id)).map_err(rt)
    }

    /// Return the labels of node `id` in insertion order.
    ///
    /// An unlabeled node and a node that does not exist both read as an empty list,
    /// which is what the underlying accessor reports; use `get_node` to tell them
    /// apart. Without this, `add_label` and `remove_label` had no read counterpart,
    /// so a node's labels could be written but never enumerated.
    fn node_labels(&self, py: Python<'_>, id: u64) -> PyResult<Vec<String>> {
        py.detach(|| self.graph.node_labels(id)).map_err(rt)
    }

    /// Add a label to node `id`. No-op if it already has it.
    fn add_label(&self, py: Python<'_>, id: u64, label: &str) -> PyResult<()> {
        py.detach(|| self.graph.add_label(id, label)).map_err(rt)
    }

    /// Remove a label from node `id`. No-op if missing.
    fn remove_label(&self, py: Python<'_>, id: u64, label: &str) -> PyResult<()> {
        py.detach(|| self.graph.remove_label(id, label)).map_err(rt)
    }

    /// Insert a directed edge from `src` to `dst` with edge type `etype` and JSON-encoded `props`.
    /// Returns the new edge ID.
    fn add_edge(
        &self,
        py: Python<'_>,
        src: u64,
        dst: u64,
        etype: &str,
        props: &str,
    ) -> PyResult<u64> {
        let value = parse_props(props)?;
        py.detach(|| self.graph.add_edge(src, dst, etype, &value))
            .map_err(rt)
    }

    /// Insert many edges in one write transaction, returning the new ids in
    /// order. `items` is an iterable of `(src, dst, type, props)` tuples, where
    /// `props` is a JSON string.
    ///
    /// An auto-commit insert costs one durable commit per edge, so a Python loop
    /// over `add_edge` is bound by commit latency rather than by the work. This
    /// writes the whole batch under one transaction, and is all-or-nothing: any
    /// failure rolls back every edge in the batch.
    fn add_edges(&self, py: Python<'_>, items: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        // Extract every argument to owned Rust values before releasing the GIL.
        let mut parsed: Vec<(u64, u64, String, serde_json::Value)> = Vec::new();
        for item in items.try_iter()? {
            let item = item?;
            let (src, dst, etype, props): (u64, u64, String, String) = item
                .extract()
                .map_err(|_| val("each item must be a (src, dst, type, props_json) tuple"))?;
            parsed.push((src, dst, etype, parse_props(&props)?));
        }

        py.detach(|| {
            self.graph.update(|txn| {
                let mut ids = Vec::with_capacity(parsed.len());
                for (src, dst, etype, props) in &parsed {
                    ids.push(txn.add_edge(*src, *dst, etype, props)?);
                }
                Ok(ids)
            })
        })
        .map_err(rt)
    }

    /// Return edge `id` as a JSON string `{"src", "dst", "type", "props"}`, or
    /// `None` if the edge does not exist.
    fn get_edge(&self, py: Python<'_>, id: u64) -> PyResult<Option<String>> {
        py.detach(|| match self.graph.get_edge(id).map_err(rt)? {
            None => Ok(None),
            Some(record) => {
                let edge_type = self
                    .graph
                    .type_name(record.edge_type)
                    .map_err(rt)?
                    .unwrap_or_default();
                let props: serde_json::Value = rmp_serde::from_slice(&record.props).map_err(rt)?;
                let value = serde_json::json!({
                    "src": record.src,
                    "dst": record.dst,
                    "type": edge_type,
                    "props": props,
                });
                Ok(Some(value.to_string()))
            }
        })
    }

    /// Replace the properties of edge `id` with JSON-encoded `props`.
    fn update_edge(&self, py: Python<'_>, id: u64, props: &str) -> PyResult<()> {
        let value = parse_props(props)?;
        py.detach(|| self.graph.update_edge(id, &value)).map_err(rt)
    }

    /// Delete edge `id`.
    fn delete_edge(&self, py: Python<'_>, id: u64) -> PyResult<()> {
        py.detach(|| self.graph.delete_edge(id)).map_err(rt)
    }

    /// Execute a Cypher query with optional JSON-encoded parameter bindings and return the result as a JSON string.
    ///
    /// The returned object has the shape
    /// `{"columns": [...], "records": [{"values": [...]}], "statement_count": n}`.
    #[pyo3(signature = (cypher, params=None))]
    fn query(&self, py: Python<'_>, cypher: &str, params: Option<String>) -> PyResult<String> {
        let params: Option<HashMap<String, serde_json::Value>> = match params {
            None => None,
            Some(s) => Some(
                serde_json::from_str(&s)
                    .map_err(|e| val(format!("parameters must be a JSON object: {e}")))?,
            ),
        };
        py.detach(|| {
            let result = match &params {
                None => self.graph.query(cypher).map_err(rt)?,
                Some(map) => self.graph.query_with_params(cypher, map).map_err(rt)?,
            };
            serde_json::to_string(&result).map_err(rt)
        })
    }

    /// Compile `cypher`, optimize the physical plan, and return it as a human-readable tree.
    fn explain(&self, py: Python<'_>, cypher: &str) -> PyResult<String> {
        py.detach(|| self.graph.explain(cypher)).map_err(rt)
    }

    /// Index or update the float32 embedding for node `id`.
    fn upsert_vector(&self, py: Python<'_>, id: u64, vector: Vec<f32>) -> PyResult<()> {
        py.detach(|| self.graph.upsert_vector(id, &vector))
            .map_err(rt)
    }

    /// Return the `k` nearest neighbors to `vector` as a JSON array of
    /// `{"node": u64, "distance": f32}`.
    ///
    /// `label` restricts results to nodes carrying that label. `properties` is a
    /// JSON object string of key-value filters; only nodes matching every filter
    /// are returned. Both filters are applied during index traversal.
    #[pyo3(signature = (vector, k, label=None, properties=None, rescore_factor=None))]
    fn vector_search(
        &self,
        py: Python<'_>,
        vector: Vec<f32>,
        k: usize,
        label: Option<String>,
        properties: Option<String>,
        rescore_factor: Option<usize>,
    ) -> PyResult<String> {
        let properties = match properties {
            None => None,
            Some(s) => {
                let map: HashMap<String, serde_json::Value> = serde_json::from_str(&s)
                    .map_err(|e| val(format!("properties must be a JSON object: {e}")))?;
                Some(map)
            }
        };
        let opts = VectorSearchOptions {
            k,
            label,
            properties,
            rescore_factor,
        };
        py.detach(|| {
            let hits = self.graph.vector_search_with(&vector, &opts).map_err(rt)?;
            let json_hits: Vec<serde_json::Value> = hits
                .into_iter()
                .map(|h| serde_json::json!({ "node": h.node, "distance": h.distance }))
                .collect();
            serde_json::to_string(&json_hits).map_err(rt)
        })
    }

    /// Configure or rebuild the vector index metric and quantization.
    ///
    /// `metric` is one of 'cosine', 'l2', or 'dot' (alias 'ip'); `quantization`
    /// is one of 'float32', 'float16', or 'int8'. Set `reindex=True` to rebuild
    /// from existing stored vectors under the new configuration.
    #[pyo3(signature = (metric, quantization="float32", reindex=false))]
    fn configure_vector_index(
        &self,
        py: Python<'_>,
        metric: &str,
        quantization: &str,
        reindex: bool,
    ) -> PyResult<()> {
        let opts = VectorIndexOptions {
            metric: VectorMetric::from_str(metric).map_err(val)?,
            quantization: VectorQuantization::from_str(quantization).map_err(val)?,
        };
        py.detach(|| {
            if reindex {
                self.graph.reindex_vector_index(opts)
            } else {
                self.graph.configure_vector_index(opts)
            }
        })
        .map_err(rt)
    }

    /// Full-text search over indexed node properties.
    ///
    /// `label` and `property` narrow the search to a specific index. `limit` caps the result count.
    /// Returns a JSON array of `{"node": u64, "score": f32}`.
    #[pyo3(signature = (query, label=None, property=None, limit=10))]
    fn text_search(
        &self,
        py: Python<'_>,
        query: &str,
        label: Option<String>,
        property: Option<String>,
        limit: usize,
    ) -> PyResult<String> {
        let opts = TextSearchOptions {
            label,
            property,
            limit,
            ..Default::default()
        };
        py.detach(|| {
            let hits = self.graph.text_search(query, &opts).map_err(rt)?;
            let json_hits: Vec<serde_json::Value> = hits
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "node": h.node,
                        "score": h.score,
                        "label": h.label,
                        "property": h.property,
                    })
                })
                .collect();
            serde_json::to_string(&json_hits).map_err(rt)
        })
    }

    /// Create a full-text index on `property` for nodes with `label`.
    ///
    /// `language` selects the stemming language (one of 'english', 'spanish',
    /// 'french', 'german', 'italian', or 'portuguese'); it defaults to English.
    #[pyo3(signature = (label, property, language=None))]
    fn create_text_index(
        &self,
        py: Python<'_>,
        label: &str,
        property: &str,
        language: Option<String>,
    ) -> PyResult<()> {
        let lang = Language::from_str(language.as_deref().unwrap_or("english")).map_err(val)?;
        py.detach(|| {
            self.graph
                .create_text_index_with_language(label, property, lang)
        })
        .map_err(rt)
    }

    /// Drop the full-text index on `property` for nodes with `label`.
    fn drop_text_index(&self, py: Python<'_>, label: &str, property: &str) -> PyResult<()> {
        py.detach(|| self.graph.drop_text_index(label, property))
            .map_err(rt)
    }

    /// List all active full-text indexes as a JSON array of
    /// `{"label", "property", "language"}`.
    fn list_text_indexes(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| {
            let indexes = self.graph.list_text_indexes().map_err(rt)?;
            let list: Vec<serde_json::Value> = indexes
                .into_iter()
                .map(|(label, property, language)| {
                    serde_json::json!({
                        "label": label,
                        "property": property,
                        "language": format!("{language:?}").to_lowercase(),
                    })
                })
                .collect();
            serde_json::to_string(&list).map_err(rt)
        })
    }

    /// Set the thread count for the parallel read passes (0 restores default behavior).
    fn set_thread_count(&self, py: Python<'_>, n: i32) -> PyResult<()> {
        py.detach(|| self.graph.set_thread_count(n)).map_err(rt)
    }

    /// Build the schema statistics the query optimizer reads, now.
    ///
    /// Nothing builds them as a side effect of a query, so without this call a
    /// process plans every relationship pattern on the global average fan-out
    /// instead of the per-source-label expand ratio, and a provably empty typed hop
    /// is pruned by a bounded probe rather than an exact lookup. One pass over the
    /// label index and the adjacency, cached until the next committed write, and
    /// then reused while the graph has not grown much past its size at build time.
    ///
    /// Worth calling once for a long-lived process (a service, a notebook, a
    /// benchmark harness) and not worth it for a script that runs a handful of
    /// point lookups, which never consult these statistics at all.
    fn materialize_edge_statistics(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.graph.materialize_edge_statistics())
            .map_err(rt)
    }

    /// Build the in-memory property columns, now.
    ///
    /// The counterpart of `materialize_edge_statistics` for property-level
    /// statistics: it is what makes the optimizer's selectivity estimates and its
    /// zone-map filter pruning available, and it warms the columnar read path that
    /// backs bulk property gathers and aggregations. A caller that wants the
    /// optimizer at full strength on a cold graph wants both.
    ///
    /// Note the cost, which is larger than its sibling's in both directions: one
    /// full scan of every node record, and the resulting columns hold every scalar
    /// node property in memory for the life of the graph handle. On a large graph
    /// that is a deliberate memory commitment, which is why nothing makes it for
    /// you.
    fn materialize_property_columns(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.graph.materialize_property_columns())
            .map_err(rt)
    }

    /// Execute a hybrid retrieval (GraphRAG) query combining vector search, text
    /// search, and relationship expansion. Returns a JSON object
    /// `{"nodes", "edges", "scores", "truncated"}`, where `truncated` is true when
    /// the `max_nodes` cap cut off seeds or expansion, so a capped result is
    /// distinguishable from a complete one.
    ///
    /// `fusion_strategy` is 'rrf' (default) or 'weighted_sum' (alias 'weighted').
    #[pyo3(signature = (
        vector=None,
        text_query=None,
        vector_k=10,
        text_k=10,
        text_label=None,
        text_property=None,
        vector_label=None,
        hops=2,
        max_distance=None,
        max_nodes=None,
        fusion_strategy="rrf",
        rrf_k=60,
        vector_weight=0.5,
        text_weight=0.5
    ))]
    #[allow(clippy::too_many_arguments)]
    fn retrieve_hybrid(
        &self,
        py: Python<'_>,
        vector: Option<Vec<f32>>,
        text_query: Option<String>,
        vector_k: usize,
        text_k: usize,
        text_label: Option<String>,
        text_property: Option<String>,
        vector_label: Option<String>,
        hops: u8,
        max_distance: Option<f32>,
        max_nodes: Option<usize>,
        fusion_strategy: &str,
        rrf_k: u32,
        vector_weight: f32,
        text_weight: f32,
    ) -> PyResult<String> {
        let fusion = match fusion_strategy.to_lowercase().as_str() {
            "rrf" => FusionStrategy::Rrf { k: rrf_k },
            "weighted_sum" | "weighted" => FusionStrategy::WeightedSum {
                vector_weight,
                text_weight,
            },
            s => return Err(val(format!("invalid fusion strategy: {s}"))),
        };
        let opts = HybridRetrieveOptions {
            vector_k,
            text_k,
            text_label,
            text_property,
            hops,
            max_distance: max_distance.unwrap_or(f32::MAX),
            max_nodes,
            vector_label,
            fusion,
        };
        let vector = vector.unwrap_or_default();
        let text_query = text_query.unwrap_or_default();
        py.detach(|| {
            let subgraph =
                retrieve_hybrid(&self.graph, &vector, &text_query, &opts).map_err(|e| match e {
                    e @ RetrievalError::NoQuery => val(e),
                    other => rt(other),
                })?;
            let scores: HashMap<String, f32> = subgraph
                .scores
                .into_iter()
                .map(|(node, score)| (node.to_string(), score))
                .collect();
            let value = serde_json::json!({
                "nodes": subgraph.nodes,
                "edges": subgraph.edges,
                "scores": scores,
                "truncated": subgraph.truncated,
            });
            serde_json::to_string(&value).map_err(rt)
        })
    }

    /// Write a hot backup of the database to `path`.
    fn backup(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        py.detach(|| self.graph.backup(Path::new(path))).map_err(rt)
    }

    /// Write a compacted hot backup of the database to `path`.
    fn backup_compact(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        py.detach(|| self.graph.backup_compact(Path::new(path)))
            .map_err(rt)
    }

    /// Remove the indexed vector for node `id`.
    fn remove_vector(&self, py: Python<'_>, id: u64) -> PyResult<()> {
        py.detach(|| self.graph.remove_vector(id)).map_err(rt)
    }

    /// Check if a full-text index exists on `property` for nodes with `label`.
    fn has_text_index(&self, py: Python<'_>, label: &str, property: &str) -> PyResult<bool> {
        py.detach(|| self.graph.has_text_index(label, property))
            .map_err(rt)
    }

    /// Restore a snapshot file at `snapshot_path` into a new database directory
    /// at `destination_path`.
    ///
    /// After restoration, open the database with `IssunDB(destination_path)`.
    #[staticmethod]
    fn restore(py: Python<'_>, snapshot_path: &str, destination_path: &str) -> PyResult<()> {
        py.detach(|| Graph::restore(Path::new(snapshot_path), Path::new(destination_path)))
            .map_err(rt)
    }
}
