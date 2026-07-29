//! Browser bindings for IssunDB, backing the playground in `web/`.
//!
//! This is the thinnest layer that lets a page drive a real engine: it owns one
//! `Graph`, forwards Cypher to it, and exposes the two capabilities Cypher cannot
//! reach on its own (vector upsert and search, and full-text index creation and
//! search), which are Rust extension traits rather than query-language features.
//!
//! # Why every method returns a JSON string
//!
//! The alternative is `serde-wasm-bindgen` or hand-built `JsValue` trees. A JSON
//! string keeps the whole boundary to one type in both directions, so there is no
//! second serialization contract to keep in agreement with the JavaScript, and the
//! page already parses JSON for everything else. The cost is one extra
//! serialize/parse per call, which is irrelevant beside executing the query.
//!
//! # What the browser configuration means
//!
//! Built with `--no-default-features`, so storage is the in-memory backend and the
//! vector index is the exact scan. Two consequences the page has to present honestly:
//! nothing persists across a reload, and `backup`/`restore` are unavailable. The
//! samples exist so a fresh page is never empty.
//!
//! # Stack size
//!
//! Cypher execution recurses over the query's own structure, and the engine allows a
//! query to run inline up to a cost of `SMALL_STACK_EXEC_BUDGET_KB` (about 1 MB) —
//! which is exactly wasm's default stack. Past that it wants a large-stack thread,
//! which a browser wasm module has no way to spawn, so it reports an error instead.
//! The playground therefore links with a larger stack (see `.cargo/config.toml`);
//! without it a moderately nested query overflows rather than failing cleanly.

use issundb::{
    DegreeDirection, Graph, GraphQueryExt, Language, TextGraphExt, TextIndexExt, TextSearchOptions,
    VectorGraphExt,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

/// How many nodes a visualization request will return at most.
///
/// The page draws a force simulation, which stops being readable long before it stops
/// being computable, so the cap is about legibility rather than cost.
const MAX_GRAPH_NODES: usize = 300;

/// One IssunDB instance, owned by the page.
#[wasm_bindgen]
pub struct Playground {
    graph: Graph,
}

/// Render any engine error as the message the page will display. The engine's errors
/// are written for a person to read, so nothing is added to them.
fn js_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// The logic layer.
///
/// Separate from the exported layer below because constructing a `JsError` calls a
/// wasm-bindgen import, which panics on a non-wasm target — so a binding that built
/// one directly could not be tested by `cargo test`. These return `String` and are
/// covered by the tests at the bottom of this file; the exported methods are one-line
/// adapters that only convert the error.
impl Playground {
    /// Open an empty in-memory database.
    ///
    /// The path is ignored by the in-memory backend but still required by
    /// `Graph::open`, which is the same constructor a native embedding calls; keeping
    /// one constructor is why the browser needs no special-casing inside the engine.
    fn new_inner() -> Result<Playground, String> {
        let graph = Graph::open(std::path::Path::new("/issundb-playground"), 1).map_err(js_err)?;
        Ok(Playground { graph })
    }

    /// Run one or more Cypher statements.
    ///
    /// Returns `{columns, rows, statement_count, elapsed_ms}`. `rows` is row-major, so
    /// the page can render a table without knowing anything about the schema. For a
    /// semicolon-separated script only the final statement's result is returned, and
    /// `statement_count` is how the page can say so rather than appearing to ignore
    /// the earlier ones.
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

    /// The physical plan for a query, as the engine's own `EXPLAIN` renders it.
    fn explain_inner(&self, cypher: &str) -> Result<String, String> {
        self.graph.explain(cypher).map_err(js_err)
    }

    /// Counts and registries, for the page's status line: how much data is loaded and
    /// which labels and relationship types exist.
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
        // Edges are counted through the adjacency rather than a scan, so this stays
        // cheap enough to call after every statement.
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

    /// The whole graph as `{nodes, edges, truncated}` for the force-directed view.
    ///
    /// Each node carries its labels and its properties so the page can label and
    /// inspect a vertex without a second query. `truncated` is true when the cap cut
    /// the graph short, so a partial picture is never presented as the whole one.
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

    /// Every property of one node, as a JSON object.
    ///
    /// The read-path methods on `Graph` all take the property names to fetch, which an
    /// inspector cannot know in advance, so this decodes the stored msgpack blob the way
    /// the REST node route does. An empty object stands in for a node that no longer
    /// exists, since the caller is drawing a snapshot rather than resolving a lookup.
    fn node_props(&self, id: u64) -> Result<Value, String> {
        match self.graph.get_node(id).map_err(js_err)? {
            Some(record) => rmp_serde::from_slice(&record.props).map_err(js_err),
            None => Ok(json!({})),
        }
    }

    /// Provision a full-text index over one label's property.
    ///
    /// Separate from `query` because full-text indexing is an engine capability rather
    /// than a Cypher clause; the page's text demo calls this first.
    fn create_text_index_inner(&self, label: &str, property: &str) -> Result<(), String> {
        self.graph
            .create_text_index_with_language(label, property, Language::English)
            .map_err(js_err)
    }

    /// BM25-ranked full-text search, returning `{node, score, label, property}` per
    /// hit so the page can show which field matched.
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

    /// Attach an embedding to a node.
    fn upsert_vector_inner(&self, node: u64, vector: Vec<f32>) -> Result<(), String> {
        self.graph.upsert_vector(node, &vector).map_err(js_err)
    }

    /// Nearest neighbors of `vector`, as `{node, distance}` ordered nearest first.
    fn vector_search_inner(&self, vector: Vec<f32>, k: usize) -> Result<String, String> {
        let hits = self.graph.vector_search(&vector, k).map_err(js_err)?;
        let hits: Vec<Value> = hits
            .into_iter()
            .map(|h| json!({ "node": h.node, "distance": h.distance }))
            .collect();
        Ok(json!({ "hits": hits }).to_string())
    }

    /// Degree centrality for every node, as `{node: degree}`.
    ///
    /// Exposed directly as well as through `CALL issundb.degree` because the page
    /// sizes graph vertices by it on every redraw, and a Cypher round trip per redraw
    /// would be wasteful.
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

    /// The engine version, for the page footer.
    fn version_inner() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Whether this build persists to disk, so the page can say which it is rather
    /// than leaving a user to discover it on reload.
    fn is_persistent_inner() -> bool {
        cfg!(feature = "lmdb")
    }
}

/// The exported surface. Each method converts the logic layer's message into a JS
/// exception and does nothing else, so there is no behavior here to test separately.
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

    /// The engine version, for the page footer.
    #[wasm_bindgen(js_name = version)]
    pub fn version() -> String {
        Self::version_inner()
    }

    /// Whether this build persists to disk, so the page can say which it is rather
    /// than leaving a user to discover it on reload.
    #[wasm_bindgen(js_name = isPersistent)]
    pub fn is_persistent() -> bool {
        Self::is_persistent_inner()
    }
}

/// Milliseconds from the host clock.
///
/// `std::time::Instant` is unimplemented on `wasm32-unknown-unknown`, so timing a
/// query needs the host's clock. `chrono` already reaches it (the engine's temporal
/// functions depend on that), so this borrows the same route rather than adding a
/// `js-sys` dependency of its own.
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

    /// The bindings are exercised natively, so their behavior is covered by the normal
    /// test run rather than only by loading a page.
    ///
    /// The `TempDir` is returned rather than dropped: a native test run uses the LMDB
    /// backend, so each test needs its own directory, and tests in one binary run
    /// concurrently. Deriving the path from the process id instead gave every test the
    /// same environment and four of five failed on the shared lock.
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

    /// The snapshot must not emit an edge whose endpoint the node cap excluded, or the
    /// page would draw a line to nothing.
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

    /// The graph view labels each vertex from its properties, so the snapshot has to
    /// carry them. Asking `node_prop_json` for a property named `"*"` returned null for
    /// every node, which drew an unlabeled graph rather than failing.
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
