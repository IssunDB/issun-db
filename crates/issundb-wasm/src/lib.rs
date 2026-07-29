//! Browser bindings for IssunDB, backing the playground in `web/`.
//!
//! It owns one `Graph`, forwards Cypher to it, and exposes the two capabilities Cypher
//! cannot reach on its own (vector upsert and search, and full-text index creation and
//! search), which are Rust extension traits rather than query-language features.
//!
//! Every method returns a JSON string, which keeps the whole boundary to one type in both
//! directions rather than a second serialization contract to keep in agreement with the
//! JavaScript.
//!
//! Built with `--no-default-features`, so storage is the in-memory backend and the vector
//! index is the exact scan. Enabling `hnsw` here does not work: it selects usearch, which is
//! C++, and the wasm build fails in `cxx`. Nothing persists across a reload, and
//! `backup`/`restore` are unavailable.
//!
//! The engine runs a query inline up to a cost of `SMALL_STACK_EXEC_BUDGET_KB` (about
//! 1 MB), which is exactly wasm's default stack, and past that wants a large-stack thread a
//! browser module cannot spawn. The playground therefore links with a 16 MB stack (see
//! `.cargo/config.toml`); without it a moderately nested query overflows rather than
//! failing cleanly.

use issundb::{
    DegreeDirection, Graph, GraphQueryExt, Language, TextGraphExt, TextIndexExt, TextSearchOptions,
    VectorGraphExt,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

/// The page draws a force simulation, which stops being readable long before it stops being
/// computable, so this cap is about legibility rather than cost.
const MAX_GRAPH_NODES: usize = 300;

/// One IssunDB instance, owned by the page.
#[wasm_bindgen]
pub struct Playground {
    graph: Graph,
}

fn js_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// The logic layer, separate from the exported layer below because constructing a `JsError`
/// calls a wasm-bindgen import that panics on a non-wasm target, so a binding building one
/// directly could not be tested by `cargo test`. These return `String` and are covered by
/// the tests at the bottom of this file.
impl Playground {
    /// The path is ignored by the in-memory backend but still required by `Graph::open`,
    /// which is the same constructor a native embedding calls.
    fn new_inner() -> Result<Playground, String> {
        let graph = Graph::open(std::path::Path::new("/issundb-playground"), 1).map_err(js_err)?;
        Ok(Playground { graph })
    }

    /// Returns `{columns, rows, statement_count, elapsed_ms}` with row-major rows. For a
    /// semicolon-separated script only the final statement's result is returned, and
    /// `statement_count` is how the page can say so rather than appearing to ignore the
    /// earlier ones.
    fn query_inner(&self, cypher: &str) -> Result<String, String> {
        let started = js_sys_now();
        let result = self.graph.query(cypher).map_err(js_err)?;
        let elapsed = js_sys_now() - started;
        let rows: Vec<Value> = result
            .records
            .into_iter()
            .map(|record| Value::Array(record.values))
            .collect();
        Ok(json!({
            "columns": result.columns,
            "rows": rows,
            "statement_count": result.statement_count,
            "elapsed_ms": elapsed,
        })
        .to_string())
    }

    fn explain_inner(&self, cypher: &str) -> Result<String, String> {
        self.graph.explain(cypher).map_err(js_err)
    }

    fn stats_inner(&self) -> Result<String, String> {
        let nodes = self.graph.all_nodes().map_err(js_err)?;
        let mut labels: Vec<String> = Vec::new();
        let mut label_counts = serde_json::Map::new();
        for node in &nodes {
            for name in self.graph.node_labels(*node).map_err(js_err)? {
                if !labels.contains(&name) {
                    labels.push(name.clone());
                }
                let entry = label_counts.entry(name).or_insert(json!(0));
                if let Some(n) = entry.as_u64() {
                    *entry = json!(n + 1);
                }
            }
        }
        let mut edges = 0u64;
        let mut type_counts = serde_json::Map::new();
        for node in &nodes {
            for neighbor in self.graph.out_neighbors(*node).map_err(js_err)? {
                edges += 1;
                let name = self
                    .graph
                    .type_name(neighbor.edge_type)
                    .map_err(js_err)?
                    .unwrap_or_else(|| "?".to_string());
                let entry = type_counts.entry(name).or_insert(json!(0));
                if let Some(n) = entry.as_u64() {
                    *entry = json!(n + 1);
                }
            }
        }
        labels.sort();
        Ok(json!({
            "nodes": nodes.len(),
            "edges": edges,
            "labels": labels,
            "label_counts": label_counts,
            "type_counts": type_counts,
        })
        .to_string())
    }

    /// The whole graph as `{nodes, edges, truncated}` for the force-directed view. Each node
    /// carries its labels and properties so the page can label and inspect a vertex without a
    /// second query, and `truncated` keeps a partial picture from being presented as a whole
    /// one.
    fn graph_snapshot_inner(&self) -> Result<String, String> {
        let all = self.graph.all_nodes().map_err(js_err)?;
        let truncated = all.len() > MAX_GRAPH_NODES;
        let kept: Vec<_> = all.iter().copied().take(MAX_GRAPH_NODES).collect();
        let included: std::collections::HashSet<_> = kept.iter().copied().collect();

        let mut nodes = Vec::new();
        for id in &kept {
            nodes.push(json!({
                "id": id,
                "labels": self.graph.node_labels(*id).map_err(js_err)?,
                "props": self.node_props(*id)?,
                "degree": self.graph.out_neighbors(*id).map_err(js_err)?.len(),
            }));
        }

        let mut edges = Vec::new();
        for id in &kept {
            for neighbor in self.graph.out_neighbors(*id).map_err(js_err)? {
                // An edge to a node the cap excluded would draw as a dangling line.
                if !included.contains(&neighbor.node) {
                    continue;
                }
                edges.push(json!({
                    "id": neighbor.edge,
                    "source": id,
                    "target": neighbor.node,
                    "type": self.graph.type_name(neighbor.edge_type).map_err(js_err)?,
                }));
            }
        }
        Ok(json!({ "nodes": nodes, "edges": edges, "truncated": truncated }).to_string())
    }

    /// Every property of one node. The read-path methods on `Graph` all take the property
    /// names to fetch, which an inspector cannot know in advance, so this decodes the stored
    /// msgpack blob the way the REST node route does.
    fn node_props(&self, id: u64) -> Result<Value, String> {
        match self.graph.get_node(id).map_err(js_err)? {
            Some(record) => rmp_serde::from_slice(&record.props).map_err(js_err),
            None => Ok(json!({})),
        }
    }

    fn create_text_index_inner(&self, label: &str, property: &str) -> Result<(), String> {
        self.graph
            .create_text_index_with_language(label, property, Language::English)
            .map_err(js_err)
    }

    /// Returns `{node, score, label, property}` per hit, so the page can show which field
    /// matched.
    fn text_search_inner(&self, query: &str, k: usize) -> Result<String, String> {
        let hits = self
            .graph
            .text_search(
                query,
                &TextSearchOptions {
                    limit: k,
                    ..Default::default()
                },
            )
            .map_err(js_err)?;
        let hits: Vec<Value> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "node": h.node,
                    "score": h.score,
                    "label": h.label,
                    "property": h.property,
                })
            })
            .collect();
        Ok(json!({ "hits": hits }).to_string())
    }

    fn upsert_vector_inner(&self, node: u64, vector: Vec<f32>) -> Result<(), String> {
        self.graph.upsert_vector(node, &vector).map_err(js_err)
    }

    /// Nearest neighbors as `{node, distance}`, ordered nearest first.
    fn vector_search_inner(&self, vector: Vec<f32>, k: usize) -> Result<String, String> {
        let hits = self.graph.vector_search(&vector, k).map_err(js_err)?;
        let hits: Vec<Value> = hits
            .into_iter()
            .map(|h| json!({ "node": h.node, "distance": h.distance }))
            .collect();
        Ok(json!({ "hits": hits }).to_string())
    }

    /// Exposed directly as well as through `CALL issundb.degree`, because the page sizes
    /// vertices by degree on every redraw.
    fn degrees_inner(&self) -> Result<String, String> {
        let degrees: HashMap<u64, u64> = self
            .graph
            .degree_centrality(DegreeDirection::Both)
            .map_err(js_err)?;
        let mapped: serde_json::Map<String, Value> = degrees
            .into_iter()
            .map(|(node, degree)| (node.to_string(), json!(degree)))
            .collect();
        Ok(Value::Object(mapped).to_string())
    }

    fn version_inner() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// So the page can say whether data survives a reload rather than leaving a visitor to
    /// discover it.
    fn is_persistent_inner() -> bool {
        cfg!(feature = "lmdb")
    }
}

/// Each method converts the logic layer's message into a JS exception and does nothing else,
/// so there is no behavior here to test separately.
#[wasm_bindgen]
impl Playground {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<Playground, JsError> {
        Self::new_inner().map_err(|e| JsError::new(&e))
    }

    pub fn query(&self, cypher: &str) -> Result<String, JsError> {
        self.query_inner(cypher).map_err(|e| JsError::new(&e))
    }

    pub fn explain(&self, cypher: &str) -> Result<String, JsError> {
        self.explain_inner(cypher).map_err(|e| JsError::new(&e))
    }

    pub fn stats(&self) -> Result<String, JsError> {
        self.stats_inner().map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = graphSnapshot)]
    pub fn graph_snapshot(&self) -> Result<String, JsError> {
        self.graph_snapshot_inner().map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = createTextIndex)]
    pub fn create_text_index(&self, label: &str, property: &str) -> Result<(), JsError> {
        self.create_text_index_inner(label, property)
            .map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = textSearch)]
    pub fn text_search(&self, query: &str, k: usize) -> Result<String, JsError> {
        self.text_search_inner(query, k)
            .map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = upsertVector)]
    pub fn upsert_vector(&self, node: u64, vector: Vec<f32>) -> Result<(), JsError> {
        self.upsert_vector_inner(node, vector)
            .map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = vectorSearch)]
    pub fn vector_search(&self, vector: Vec<f32>, k: usize) -> Result<String, JsError> {
        self.vector_search_inner(vector, k)
            .map_err(|e| JsError::new(&e))
    }

    pub fn degrees(&self) -> Result<String, JsError> {
        self.degrees_inner().map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(js_name = version)]
    pub fn version() -> String {
        Self::version_inner()
    }

    #[wasm_bindgen(js_name = isPersistent)]
    pub fn is_persistent() -> bool {
        Self::is_persistent_inner()
    }
}

/// `std::time::Instant` is unimplemented on `wasm32-unknown-unknown`, so timing a query
/// needs the host's clock.
fn js_sys_now() -> f64 {
    #[cfg(target_family = "wasm")]
    {
        js_sys_date_now()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen(inline_js = "export function js_sys_date_now() { return Date.now(); }")]
extern "C" {
    fn js_sys_date_now() -> f64;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `TempDir` is returned rather than dropped: a native test run uses the LMDB
    /// backend, so each test needs its own directory, and tests in one binary run
    /// concurrently. Deriving the path from the process id instead gave every test the same
    /// environment, and four of five failed on the shared lock.
    fn playground() -> (tempfile::TempDir, Playground) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let graph = Graph::open(dir.path(), 1).expect("open");
        (dir, Playground { graph })
    }

    #[test]
    fn query_returns_columns_and_row_major_rows() {
        let (_dir, p) = playground();
        p.query_inner("CREATE (:Person {name: 'Ada'}), (:Person {name: 'Grace'})")
            .unwrap();
        let out: Value = serde_json::from_str(
            &p.query_inner("MATCH (n:Person) RETURN n.name AS name")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["columns"], json!(["name"]));
        let mut names: Vec<String> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Ada", "Grace"]);
        assert_eq!(out["statement_count"], json!(1));
    }

    #[test]
    fn stats_counts_nodes_edges_and_registries() {
        let (_dir, p) = playground();
        p.query_inner("CREATE (a:Person {name:'a'})-[:KNOWS]->(b:Person {name:'b'})")
            .unwrap();
        let s: Value = serde_json::from_str(&p.stats_inner().unwrap()).unwrap();
        assert_eq!(s["nodes"], json!(2));
        assert_eq!(s["edges"], json!(1));
        assert_eq!(s["labels"], json!(["Person"]));
        assert_eq!(s["type_counts"]["KNOWS"], json!(1));
    }

    #[test]
    fn graph_snapshot_drops_edges_to_excluded_nodes() {
        let (_dir, p) = playground();
        let snap: Value = serde_json::from_str(&p.graph_snapshot_inner().unwrap()).unwrap();
        assert_eq!(snap["nodes"].as_array().unwrap().len(), 0);
        assert_eq!(snap["truncated"], json!(false));

        p.query_inner("CREATE (a:N)-[:R]->(b:N)").unwrap();
        let snap: Value = serde_json::from_str(&p.graph_snapshot_inner().unwrap()).unwrap();
        assert_eq!(snap["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(snap["edges"].as_array().unwrap().len(), 1);
        let ids: Vec<u64> = snap["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_u64().unwrap())
            .collect();
        let edge = &snap["edges"][0];
        assert!(ids.contains(&edge["source"].as_u64().unwrap()));
        assert!(ids.contains(&edge["target"].as_u64().unwrap()));
    }

    /// Asking `node_prop_json` for a property named `"*"` returned null for every node, which
    /// drew an unlabeled graph rather than failing.
    #[test]
    fn graph_snapshot_carries_every_property_of_a_node() {
        let (_dir, p) = playground();
        p.query_inner("CREATE (:Person {name: 'Ada', city: 'London', age: 36})")
            .unwrap();
        let snap: Value = serde_json::from_str(&p.graph_snapshot_inner().unwrap()).unwrap();
        let props = &snap["nodes"][0]["props"];
        assert_eq!(props["name"], json!("Ada"));
        assert_eq!(props["city"], json!("London"));
        assert_eq!(props["age"], json!(36));
    }

    #[test]
    fn explain_reports_a_plan() {
        let (_dir, p) = playground();
        let plan = p.explain_inner("MATCH (n:Person) RETURN n").unwrap();
        assert!(!plan.trim().is_empty(), "a plan must not be blank");
    }

    #[test]
    fn a_query_error_becomes_a_js_exception_rather_than_a_panic() {
        let (_dir, p) = playground();
        assert!(p.query_inner("MATCH ( RETURN").is_err());
    }
}
