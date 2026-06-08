//! Integration tests for the REST router.
//!
//! Each test builds a router over a fresh `TempDir`-backed `Graph` and drives it
//! with `tower::ServiceExt::oneshot`, so no socket is bound and the assertions are
//! deterministic. The router is consumed by `oneshot`, so a new one is built per
//! request via `app()`.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use issundb::{Graph, VectorGraphExt};
use issundb_rest::routes::build_router;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// Open a fresh graph in a temp directory. The `TempDir` is returned so the
/// caller keeps it alive for the duration of the test.
fn fresh_graph() -> (Arc<Graph>, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let graph = Graph::open(dir.path(), 1).expect("open graph");
    (Arc::new(graph), dir)
}

/// Send one request through a fresh router and return the status and parsed JSON
/// body (or `Value::Null` for an empty body).
async fn send(graph: &Arc<Graph>, req: Request<Body>) -> (StatusCode, Value) {
    let app = build_router(graph.clone());
    let resp = app.oneshot(req).await.expect("router response");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn put(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Create a node and return its id.
async fn create_node(graph: &Arc<Graph>, label: &str, props: Value) -> u64 {
    let (status, body) = send(
        graph,
        post("/v1/nodes", json!({ "label": label, "props": props })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_node body: {body}");
    body["id"].as_u64().expect("node id")
}

#[tokio::test]
async fn health_reports_status_and_api_version() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(&graph, get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["api"], "v1");
    assert!(body["version"].is_string());
}

#[tokio::test]
async fn create_then_get_node_round_trip() {
    let (graph, _dir) = fresh_graph();
    let id = create_node(&graph, "Person", json!({ "name": "Ada" })).await;

    let (status, body) = send(&graph, get(&format!("/v1/nodes/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_u64(), Some(id));
    assert_eq!(body["label"], "Person");
    assert_eq!(body["props"]["name"], "Ada");
}

#[tokio::test]
async fn create_multi_label_node_round_trip() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(
        &graph,
        post(
            "/v1/nodes",
            json!({ "labels": ["Person", "Admin"], "props": { "name": "Ada" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let id = body["id"].as_u64().expect("node id");

    let (status, body) = send(&graph, get(&format!("/v1/nodes/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    let labels = body["labels"].as_array().expect("labels array");
    assert!(labels.contains(&json!("Person")));
    assert!(labels.contains(&json!("Admin")));
    // `label` is the primary (first) label.
    assert_eq!(body["label"], body["labels"][0]);
}

#[tokio::test]
async fn create_node_without_label_is_bad_request() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(&graph, post("/v1/nodes", json!({ "props": {} }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn get_missing_node_is_not_found() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(&graph, get("/v1/nodes/999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn update_node_persists_props() {
    let (graph, _dir) = fresh_graph();
    let id = create_node(&graph, "Person", json!({ "name": "Ada" })).await;

    let (status, _) = send(
        &graph,
        put(
            &format!("/v1/nodes/{id}"),
            json!({ "props": { "name": "Grace" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = send(&graph, get(&format!("/v1/nodes/{id}"))).await;
    assert_eq!(body["props"]["name"], "Grace");
}

#[tokio::test]
async fn update_missing_node_is_not_found() {
    let (graph, _dir) = fresh_graph();
    let (status, _) = send(&graph, put("/v1/nodes/999", json!({ "props": {} }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_node_removes_it() {
    let (graph, _dir) = fresh_graph();
    let id = create_node(&graph, "Person", json!({})).await;

    let (status, _) = send(&graph, delete(&format!("/v1/nodes/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&graph, get(&format!("/v1/nodes/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_missing_node_is_idempotent() {
    // `Graph::delete_node` returns `Ok(())` for a nonexistent id rather than
    // `NodeNotFound`, so the handler's 404 arm is unreachable and the route is
    // idempotent: deleting a missing node reports 204.
    let (graph, _dir) = fresh_graph();
    let (status, _) = send(&graph, delete("/v1/nodes/999")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn create_then_get_edge_round_trip() {
    let (graph, _dir) = fresh_graph();
    let src = create_node(&graph, "Person", json!({})).await;
    let dst = create_node(&graph, "Person", json!({})).await;

    let (status, body) = send(
        &graph,
        post(
            "/v1/edges",
            json!({ "src": src, "dst": dst, "type": "KNOWS", "props": { "since": 2020 } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_edge body: {body}");
    let edge_id = body["id"].as_u64().expect("edge id");

    let (status, body) = send(&graph, get(&format!("/v1/edges/{edge_id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["src"].as_u64(), Some(src));
    assert_eq!(body["dst"].as_u64(), Some(dst));
    assert_eq!(body["type"], "KNOWS");
    assert_eq!(body["props"]["since"], 2020);
}

#[tokio::test]
async fn create_edge_with_missing_endpoint_currently_succeeds() {
    // NOTE: `Graph::add_edge` does not validate that its endpoints exist, so the
    // handler's `NodeNotFound` -> 400 arm is unreachable and a dangling edge is
    // created. This test pins the current behavior; if endpoint validation is
    // added to the core, switch this assertion to `BAD_REQUEST`.
    let (graph, _dir) = fresh_graph();
    let src = create_node(&graph, "Person", json!({})).await;

    let (status, body) = send(
        &graph,
        post(
            "/v1/edges",
            json!({ "src": src, "dst": 999, "type": "KNOWS" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_edge body: {body}");
    assert!(body["id"].as_u64().is_some());
}

#[tokio::test]
async fn get_missing_edge_is_not_found() {
    let (graph, _dir) = fresh_graph();
    let (status, _) = send(&graph, get("/v1/edges/999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_edge_removes_it() {
    let (graph, _dir) = fresh_graph();
    let src = create_node(&graph, "Person", json!({})).await;
    let dst = create_node(&graph, "Person", json!({})).await;
    let (_, body) = send(
        &graph,
        post(
            "/v1/edges",
            json!({ "src": src, "dst": dst, "type": "KNOWS" }),
        ),
    )
    .await;
    let edge_id = body["id"].as_u64().unwrap();

    let (status, _) = send(&graph, delete(&format!("/v1/edges/{edge_id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&graph, get(&format!("/v1/edges/{edge_id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_missing_edge_is_idempotent() {
    // As with node deletion, `Graph::delete_edge` returns `Ok(())` for a missing
    // id, so the route is idempotent and reports 204 rather than 404.
    let (graph, _dir) = fresh_graph();
    let (status, _) = send(&graph, delete("/v1/edges/999")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn cypher_query_returns_columns_and_records() {
    let (graph, _dir) = fresh_graph();
    create_node(&graph, "Person", json!({ "name": "Ada" })).await;
    create_node(&graph, "Person", json!({ "name": "Grace" })).await;

    let (status, body) = send(
        &graph,
        post(
            "/v1/query",
            json!({ "query": "MATCH (n:Person) RETURN n.name AS name ORDER BY name" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query body: {body}");
    assert_eq!(body["columns"], json!(["name"]));
    assert_eq!(body["records"], json!([["Ada"], ["Grace"]]));
}

#[tokio::test]
async fn cypher_query_with_params() {
    let (graph, _dir) = fresh_graph();
    create_node(&graph, "Person", json!({ "name": "Ada" })).await;
    create_node(&graph, "Person", json!({ "name": "Grace" })).await;

    let (status, body) = send(
        &graph,
        post(
            "/v1/query",
            json!({
                "query": "MATCH (n:Person) WHERE n.name = $name RETURN n.name AS name",
                "params": { "name": "Ada" }
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query body: {body}");
    assert_eq!(body["records"], json!([["Ada"]]));
}

#[tokio::test]
async fn invalid_cypher_query_is_bad_request() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(
        &graph,
        post("/v1/query", json!({ "query": "NOT VALID CYPHER" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn explain_returns_a_plan() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(
        &graph,
        post(
            "/v1/explain",
            json!({ "query": "MATCH (n:Person) RETURN n" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "explain body: {body}");
    assert!(body["plan"].is_string() || body["plan"].is_array() || body["plan"].is_object());
}

#[tokio::test]
async fn invalid_explain_query_is_bad_request() {
    let (graph, _dir) = fresh_graph();
    let (status, _) = send(&graph, post("/v1/explain", json!({ "query": "NOT VALID" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vector_search_with_empty_vector_is_bad_request() {
    let (graph, _dir) = fresh_graph();
    let (status, body) = send(&graph, post("/v1/search/vector", json!({ "vector": [] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn vector_search_returns_nearest_node() {
    let (graph, _dir) = fresh_graph();
    let a = create_node(&graph, "Doc", json!({})).await;
    let b = create_node(&graph, "Doc", json!({})).await;
    graph.upsert_vector(a, &[1.0, 0.0, 0.0]).expect("upsert a");
    graph.upsert_vector(b, &[0.0, 1.0, 0.0]).expect("upsert b");

    let (status, body) = send(
        &graph,
        post(
            "/v1/search/vector",
            json!({ "vector": [1.0, 0.0, 0.0], "k": 1 }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vector body: {body}");
    let hits = body.as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["node"].as_u64(), Some(a));
}
