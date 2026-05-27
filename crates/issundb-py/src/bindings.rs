#![cfg(feature = "extension-module")]

use std::path::Path;

use issundb::{
    Graph, GraphQueryExt, TextGraphExt, TextIndexExt, TextSearchOptions, VectorGraphExt,
};
use pyo3::prelude::*;

/// Python-facing handle for an IssunDB graph database.
#[pyclass(name = "IssunDB")]
pub struct PyGraph {
    graph: Graph,
}

#[pymethods]
impl PyGraph {
    /// Open or create an IssunDB graph at `path`.
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        let graph = Graph::open(Path::new(path), 1)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { graph })
    }

    /// Insert a node with `label` and JSON-encoded `props`. Returns the new node ID.
    fn add_node(&self, label: &str, props: &str) -> PyResult<u64> {
        let value: serde_json::Value = serde_json::from_str(props)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.graph
            .add_node(label, &value)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
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

    /// Insert a directed edge from `src` to `dst` with edge type `etype` and JSON-encoded `props`.
    /// Returns the new edge ID.
    fn add_edge(&self, src: u64, dst: u64, etype: &str, props: &str) -> PyResult<u64> {
        let value: serde_json::Value = serde_json::from_str(props)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.graph
            .add_edge(src, dst, etype, &value)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Execute a Cypher query and return the result as a JSON string.
    ///
    /// The returned object has the shape `{"columns": [...], "records": [[...]]}`.
    fn query(&self, cypher: &str) -> PyResult<String> {
        let result = self
            .graph
            .query(cypher)
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
        serde_json::to_string(&result)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Compile `cypher`, optimize the physical plan, and return it as a human-readable tree.
    fn explain(&self, cypher: &str) -> PyResult<String> {
        self.graph
            .explain(cypher)
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }

    /// Index or update the float32 embedding for node `id`.
    fn upsert_vector(&self, id: u64, vec: Vec<f32>) -> PyResult<()> {
        self.graph
            .upsert_vector(id, &vec)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Return the `k` nearest neighbors to `vec` as a JSON array of `{"node": u64, "distance": f32}`.
    fn vector_search(&self, vec: Vec<f32>, k: usize) -> PyResult<String> {
        let hits = self
            .graph
            .vector_search(&vec, k)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        let json_hits: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "node": h.node,
                    "distance": h.distance,
                })
            })
            .collect();
        serde_json::to_string(&json_hits)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
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
    fn create_text_index(&self, label: &str, property: &str) -> PyResult<()> {
        self.graph
            .create_text_index(label, property)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Drop the full-text index on `property` for nodes with `label`.
    fn drop_text_index(&self, label: &str, property: &str) -> PyResult<()> {
        self.graph
            .drop_text_index(label, property)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
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

    /// Restore a snapshot file at `snapshot` into a new database directory at `dst`.
    ///
    /// After restoration, open the database with `IssunDB(dst)`.
    #[staticmethod]
    fn restore(snapshot: &str, dst: &str) -> PyResult<()> {
        Graph::restore(Path::new(snapshot), Path::new(dst))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }
}
