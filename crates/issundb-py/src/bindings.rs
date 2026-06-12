//! Python `IssunDB` class. The module is gated behind the `extension-module`
//! feature by `lib.rs`, so this file carries no feature attribute of its own.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use issundb::{
    FusionStrategy, Graph, GraphQueryExt, HybridRetrieveOptions, Language, TextGraphExt,
    TextIndexExt, TextSearchOptions, VectorGraphExt, VectorIndexOptions, VectorMetric,
    VectorQuantization, VectorSearchOptions, retrieve_hybrid,
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

/// Parse a JSON-string argument into a `serde_json::Value`, raising `ValueError`
/// on malformed input.
fn parse_json(s: &str) -> PyResult<serde_json::Value> {
    serde_json::from_str(s).map_err(val)
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
    fn new(path: &str, map_size_gb: Option<usize>) -> PyResult<Self> {
        let size = map_size_gb.unwrap_or(1);
        let graph = Graph::open(Path::new(path), size)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { graph })
    }

    /// Insert a node with one or more labels and JSON-encoded `props`. Returns the
    /// new node ID. `labels` accepts either a single label string or a list of
    /// label strings (multi-label node).
    fn add_node(&self, labels: &Bound<'_, PyAny>, props: &str) -> PyResult<u64> {
        let value = parse_json(props)?;
        if let Ok(single) = labels.extract::<String>() {
            self.graph.add_node(&single, &value).map_err(rt)
        } else {
            let multi: Vec<String> = labels
                .extract()
                .map_err(|_| val("labels must be a string or a list of strings"))?;
            let refs: Vec<&str> = multi.iter().map(String::as_str).collect();
            self.graph.add_node_multi(&refs, &value).map_err(rt)
        }
    }

    /// Return the JSON-encoded properties of node `id`, or `None` if the node does not exist.
    fn get_node(&self, id: u64) -> PyResult<Option<String>> {
        let record = self
            .graph
            .get_node(id)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        match record {
            None => Ok(None),
            Some(r) => {
                let value: serde_json::Value = rmp_serde::from_slice(&r.props)
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
                let json = serde_json::to_string(&value)
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
                Ok(Some(json))
            }
        }
    }

    /// Replace the properties of node `id` with JSON-encoded `props`.
    fn update_node(&self, id: u64, props: &str) -> PyResult<()> {
        let value: serde_json::Value = serde_json::from_str(props)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.graph
            .update_node(id, &value)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Delete node `id` and all of its incident edges.
    fn delete_node(&self, id: u64) -> PyResult<()> {
        self.graph
            .delete_node(id)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Add a label to node `id`. No-op if it already has it.
    fn add_label(&self, id: u64, label: &str) -> PyResult<()> {
        self.graph
            .add_label(id, label)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Remove a label from node `id`. No-op if missing.
    fn remove_label(&self, id: u64, label: &str) -> PyResult<()> {
        self.graph
            .remove_label(id, label)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Insert a directed edge from `src` to `dst` with edge type `etype` and JSON-encoded `props`.
    /// Returns the new edge ID.
    fn add_edge(&self, src: u64, dst: u64, etype: &str, props: &str) -> PyResult<u64> {
        let value = parse_json(props)?;
        self.graph.add_edge(src, dst, etype, &value).map_err(rt)
    }

    /// Return edge `id` as a JSON string `{"src", "dst", "type", "props"}`, or
    /// `None` if the edge does not exist.
    fn get_edge(&self, id: u64) -> PyResult<Option<String>> {
        match self.graph.get_edge(id).map_err(rt)? {
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
        }
    }

    /// Replace the properties of edge `id` with JSON-encoded `props`.
    fn update_edge(&self, id: u64, props: &str) -> PyResult<()> {
        let value: serde_json::Value = serde_json::from_str(props)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.graph
            .update_edge(id, &value)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Delete edge `id`.
    fn delete_edge(&self, id: u64) -> PyResult<()> {
        self.graph.delete_edge(id).map_err(rt)
    }

    /// Execute a Cypher query with optional JSON-encoded parameter bindings and return the result as a JSON string.
    ///
    /// The returned object has the shape `{"columns": [...], "records": [[...]]}`.
    #[pyo3(signature = (cypher, params=None))]
    fn query(&self, cypher: &str, params: Option<String>) -> PyResult<String> {
        let result = match params {
            None => self.graph.query(cypher).map_err(rt)?,
            Some(s) => {
                let map: HashMap<String, serde_json::Value> = serde_json::from_str(&s)
                    .map_err(|e| val(format!("parameters must be a JSON object: {e}")))?;
                self.graph.query_with_params(cypher, &map).map_err(rt)?
            }
        };
        serde_json::to_string(&result).map_err(rt)
    }

    /// Compile `cypher`, optimize the physical plan, and return it as a human-readable tree.
    fn explain(&self, cypher: &str) -> PyResult<String> {
        self.graph
            .explain(cypher)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Index or update the float32 embedding for node `id`.
    fn upsert_vector(&self, id: u64, vec: Vec<f32>) -> PyResult<()> {
        self.graph.upsert_vector(id, &vec).map_err(rt)
    }

    /// Return the `k` nearest neighbors to `vec` as a JSON array of
    /// `{"node": u64, "distance": f32}`.
    ///
    /// `label` restricts results to nodes carrying that label. `properties` is a
    /// JSON object string of key-value filters; only nodes matching every filter
    /// are returned. Both filters are applied during index traversal.
    #[pyo3(signature = (vec, k, label=None, properties=None, rescore_factor=None))]
    fn vector_search(
        &self,
        vec: Vec<f32>,
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
        let hits = self.graph.vector_search_with(&vec, &opts).map_err(rt)?;
        let json_hits: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| serde_json::json!({ "node": h.node, "distance": h.distance }))
            .collect();
        serde_json::to_string(&json_hits).map_err(rt)
    }

    /// Configure or rebuild the vector index metric and quantization.
    ///
    /// `metric` is one of 'cosine', 'l2', or 'dot' (alias 'ip'); `quantization`
    /// is one of 'float32', 'float16', or 'int8'. Set `reindex=True` to rebuild
    /// from existing stored vectors under the new configuration.
    #[pyo3(signature = (metric, quantization="float32", reindex=false))]
    fn configure_vector_index(
        &self,
        metric: &str,
        quantization: &str,
        reindex: bool,
    ) -> PyResult<()> {
        let opts = VectorIndexOptions {
            metric: VectorMetric::from_str(metric).map_err(val)?,
            quantization: VectorQuantization::from_str(quantization).map_err(val)?,
        };
        if reindex {
            self.graph.reindex_vector_index(opts).map_err(rt)
        } else {
            self.graph.configure_vector_index(opts).map_err(rt)
        }
    }

    /// Full-text search over indexed node properties.
    ///
    /// `label` and `property` narrow the search to a specific index. `limit` caps the result count.
    /// Returns a JSON array of `{"node": u64, "score": f32}`.
    #[pyo3(signature = (query, label=None, property=None, limit=10))]
    fn text_search(
        &self,
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
        let hits = self
            .graph
            .text_search(query, &opts)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        let json_hits: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "node": h.node,
                    "score": h.score,
                })
            })
            .collect();
        serde_json::to_string(&json_hits)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Create a full-text index on `property` for nodes with `label`.
    ///
    /// `language` selects the stemming language (one of 'english', 'spanish',
    /// 'french', 'german', 'italian', or 'portuguese'); it defaults to English.
    #[pyo3(signature = (label, property, language=None))]
    fn create_text_index(
        &self,
        label: &str,
        property: &str,
        language: Option<String>,
    ) -> PyResult<()> {
        let lang = Language::from_str(language.as_deref().unwrap_or("english")).map_err(val)?;
        self.graph
            .create_text_index_with_language(label, property, lang)
            .map_err(rt)
    }

    /// Drop the full-text index on `property` for nodes with `label`.
    fn drop_text_index(&self, label: &str, property: &str) -> PyResult<()> {
        self.graph.drop_text_index(label, property).map_err(rt)
    }

    /// List all active full-text indexes as a JSON array of
    /// `{"label", "property", "language"}`.
    fn list_text_indexes(&self) -> PyResult<String> {
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
    }

    /// Set the GraphBLAS thread count (0 restores default behavior).
    fn set_thread_count(&self, count: i32) -> PyResult<()> {
        self.graph.set_thread_count(count).map_err(rt)
    }

    /// Execute a hybrid retrieval (GraphRAG) query combining vector search, text
    /// search, and relationship expansion. Returns a JSON object
    /// `{"nodes", "edges", "scores"}`.
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
        let subgraph = retrieve_hybrid(&self.graph, &vector, &text_query, &opts).map_err(rt)?;
        let scores: HashMap<String, f32> = subgraph
            .scores
            .into_iter()
            .map(|(node, score)| (node.to_string(), score))
            .collect();
        let value = serde_json::json!({
            "nodes": subgraph.nodes,
            "edges": subgraph.edges,
            "scores": scores,
        });
        serde_json::to_string(&value).map_err(rt)
    }

    /// Write a hot backup of the database to `path`.
    fn backup(&self, path: &str) -> PyResult<()> {
        self.graph
            .backup(Path::new(path))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Write a compacted hot backup of the database to `path`.
    fn backup_compact(&self, path: &str) -> PyResult<()> {
        self.graph
            .backup_compact(Path::new(path))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Remove the indexed vector for node `id`.
    fn remove_vector(&self, id: u64) -> PyResult<()> {
        self.graph.remove_vector(id).map_err(rt)
    }

    /// Check if a full-text index exists on `property` for nodes with `label`.
    fn has_text_index(&self, label: &str, property: &str) -> PyResult<bool> {
        self.graph.has_text_index(label, property).map_err(rt)
    }

    /// Restore a snapshot file at `snapshot` into a new database directory at `dst`.
    ///
    /// After restoration, open the database with `IssunDB(dst)`.
    #[staticmethod]
    fn restore(snapshot: &str, dst: &str) -> PyResult<()> {
        Graph::restore(Path::new(snapshot), Path::new(dst))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }
}
