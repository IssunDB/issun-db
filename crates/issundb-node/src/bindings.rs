#![cfg(feature = "napi-module")]

use std::path::Path;

use issundb::{
    Graph, GraphQueryExt, TextGraphExt, TextIndexExt, TextSearchOptions, VectorGraphExt,
};
use napi::bindgen_prelude::BigInt;
use napi_derive::napi;

/// Node.js handle for an IssunDB graph database.
///
/// Node IDs and edge IDs are the core `u64` values, exposed to JavaScript as
/// `BigInt`. Methods return IDs as `BigInt` and accept them as `BigInt`, so the
/// full identifier range round-trips without truncation.
#[napi]
pub struct IssunDB {
    graph: Graph,
}

/// Convert a JavaScript `BigInt` node or edge ID into the core `u64` value.
///
/// The conversion is rejected when the value is negative or does not fit in a
/// `u64`, so an out-of-range ID surfaces as a JavaScript error rather than a
/// silently truncated lookup.
fn id_from_bigint(value: BigInt) -> napi::Result<u64> {
    let (_, id, lossless) = value.get_u64();
    if !lossless {
        return Err(napi::Error::from_reason(
            "node and edge IDs must be non-negative integers that fit in u64",
        ));
    }
    Ok(id)
}

#[napi]
impl IssunDB {
    /// Open or create an IssunDB graph database at `path`.
    #[napi(constructor)]
    pub fn new(path: String) -> napi::Result<Self> {
        let graph = Graph::open(Path::new(&path), 1)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self { graph })
    }

    /// Insert a node with `label` and JSON-encoded `props`. Returns the new node ID as u64.
    #[napi]
    pub fn add_node(&self, label: String, props_json: String) -> napi::Result<u64> {
        let value: serde_json::Value = serde_json::from_str(&props_json)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        self.graph
            .add_node(&label, &value)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Return the JSON-encoded properties of node `id`, or `null` if the node does not exist.
    #[napi]
    pub fn get_node(&self, id: BigInt) -> napi::Result<Option<String>> {
        let id = id_from_bigint(id)?;
        let record = self
            .graph
            .get_node(id)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        match record {
            None => Ok(None),
            Some(r) => {
                let value: serde_json::Value = rmp_serde::from_slice(&r.props)
                    .map_err(|e| napi::Error::from_reason(e.to_string()))?;
                let json = serde_json::to_string(&value)
                    .map_err(|e| napi::Error::from_reason(e.to_string()))?;
                Ok(Some(json))
            }
        }
    }

    /// Replace the properties of node `id` with JSON-encoded `props`.
    #[napi]
    pub fn update_node(&self, id: BigInt, props_json: String) -> napi::Result<()> {
        let id = id_from_bigint(id)?;
        let value: serde_json::Value = serde_json::from_str(&props_json)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        self.graph
            .update_node(id, &value)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Delete node `id` and all of its incident edges.
    #[napi]
    pub fn delete_node(&self, id: BigInt) -> napi::Result<()> {
        let id = id_from_bigint(id)?;
        self.graph
            .delete_node(id)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Insert a directed edge from `src` to `dst` with edge type `etype` and JSON-encoded `props`.
    /// Returns the new edge ID as u64.
    #[napi]
    pub fn add_edge(
        &self,
        src: BigInt,
        dst: BigInt,
        etype: String,
        props_json: String,
    ) -> napi::Result<u64> {
        let src = id_from_bigint(src)?;
        let dst = id_from_bigint(dst)?;
        let value: serde_json::Value = serde_json::from_str(&props_json)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        self.graph
            .add_edge(src, dst, &etype, &value)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Execute a Cypher query and return the result as a JSON string.
    ///
    /// The returned object has the shape `{"columns": [...], "records": [[...]]}`.
    #[napi]
    pub fn query(&self, cypher: String) -> napi::Result<String> {
        let result = self
            .graph
            .query(&cypher)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        // QueryResult does not derive Serialize, so we construct the JSON manually.
        let records: Vec<serde_json::Value> = result
            .records
            .iter()
            .map(|r| serde_json::Value::Array(r.values.clone()))
            .collect();
        let json = serde_json::json!({
            "columns": result.columns,
            "records": records,
        });
        serde_json::to_string(&json).map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Compile `cypher`, optimize the physical plan, and return it as a human-readable tree.
    #[napi]
    pub fn explain(&self, cypher: String) -> napi::Result<String> {
        self.graph
            .explain(&cypher)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Index or update the float32 embedding for node `id`.
    ///
    /// `vec` is accepted as `f64` (the natural JavaScript number type) and
    /// converted to `f32` internally. Values outside the `f32` range are
    /// clamped silently by the cast.
    #[napi]
    pub fn upsert_vector(&self, id: BigInt, vec: Vec<f64>) -> napi::Result<()> {
        let id = id_from_bigint(id)?;
        let floats: Vec<f32> = vec.iter().map(|&v| v as f32).collect();
        self.graph
            .upsert_vector(id, &floats)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Return the `k` nearest neighbors to `vec` as a JSON array of `{"node": number, "distance": number}`.
    ///
    /// `vec` is accepted as `f64` and converted to `f32` internally.
    #[napi]
    pub fn vector_search(&self, vec: Vec<f64>, k: u32) -> napi::Result<String> {
        let floats: Vec<f32> = vec.iter().map(|&v| v as f32).collect();
        let hits = self
            .graph
            .vector_search(&floats, k as usize)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let json_hits: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "node": h.node,
                    "distance": h.distance,
                })
            })
            .collect();
        serde_json::to_string(&json_hits).map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Full-text search over indexed node properties.
    ///
    /// `label` and `property` narrow the search to a specific index. `limit` caps the result count.
    /// Returns a JSON array of `{"node": number, "score": number}`.
    #[napi]
    pub fn text_search(
        &self,
        query: String,
        label: Option<String>,
        property: Option<String>,
        limit: u32,
    ) -> napi::Result<String> {
        let opts = TextSearchOptions {
            label,
            property,
            limit: limit as usize,
            ..Default::default()
        };
        let hits = self
            .graph
            .text_search(&query, &opts)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let json_hits: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "node": h.node,
                    "score": h.score,
                })
            })
            .collect();
        serde_json::to_string(&json_hits).map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Create a full-text index on `property` for nodes with `label`.
    #[napi]
    pub fn create_text_index(&self, label: String, property: String) -> napi::Result<()> {
        self.graph
            .create_text_index(&label, &property)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Drop the full-text index on `property` for nodes with `label`.
    #[napi]
    pub fn drop_text_index(&self, label: String, property: String) -> napi::Result<()> {
        self.graph
            .drop_text_index(&label, &property)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Write a hot backup of the database to `path`.
    #[napi]
    pub fn backup(&self, path: String) -> napi::Result<()> {
        self.graph
            .backup(Path::new(&path))
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Write a compacted hot backup of the database to `path`.
    #[napi]
    pub fn backup_compact(&self, path: String) -> napi::Result<()> {
        self.graph
            .backup_compact(Path::new(&path))
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Restore a snapshot file at `snapshot` into a new database directory at `dst`.
    ///
    /// After restoration, open the database with `new IssunDB(dst)`.
    #[napi]
    pub fn restore(snapshot: String, dst: String) -> napi::Result<()> {
        Graph::restore(Path::new(&snapshot), Path::new(&dst))
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}
