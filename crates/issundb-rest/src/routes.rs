use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use issundb::{
    Graph, GraphQueryExt, TextGraphExt, TextSearchOptions, VectorGraphExt, VectorSearchOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub type AppState = Arc<Graph>;

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn err_json(msg: impl std::fmt::Display, status: StatusCode) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.to_string() })))
}

fn internal(msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err_json(msg, StatusCode::INTERNAL_SERVER_ERROR)
}

fn not_found(msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err_json(msg, StatusCode::NOT_FOUND)
}

fn bad_request(msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err_json(msg, StatusCode::BAD_REQUEST)
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateNodeBody {
    pub label: String,
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize)]
pub struct UpdateNodeBody {
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize)]
pub struct CreateEdgeBody {
    pub src: u64,
    pub dst: u64,
    #[serde(rename = "type")]
    pub edge_type: String,
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize)]
pub struct CypherQueryBody {
    pub query: String,
    #[serde(default)]
    pub params: HashMap<String, Value>,
}

#[derive(Deserialize)]
pub struct ExplainBody {
    pub query: String,
}

#[derive(Deserialize)]
pub struct TextSearchBody {
    pub query: String,
    pub label: Option<String>,
    pub property: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Deserialize)]
pub struct VectorSearchBody {
    pub vector: Vec<f32>,
    #[serde(default = "default_k")]
    pub k: usize,
    pub label: Option<String>,
}

fn default_limit() -> usize {
    10
}

fn default_k() -> usize {
    5
}

// ---------------------------------------------------------------------------
// Node handlers
// ---------------------------------------------------------------------------

pub async fn create_node(
    State(graph): State<AppState>,
    Json(body): Json<CreateNodeBody>,
) -> impl IntoResponse {
    match graph.add_node(&body.label, &body.props) {
        Ok(id) => (StatusCode::OK, Json(json!({ "id": id }))).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

pub async fn get_node(State(graph): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    match graph.get_node(id) {
        Ok(Some(record)) => {
            let label = match graph.node_labels(id) {
                Ok(labels) => labels.into_iter().next().unwrap_or_default(),
                Err(e) => return internal(e).into_response(),
            };
            let props: Value = match rmp_serde::from_slice(&record.props) {
                Ok(v) => v,
                Err(e) => return internal(e).into_response(),
            };
            (
                StatusCode::OK,
                Json(json!({ "id": id, "label": label, "props": props })),
            )
                .into_response()
        }
        Ok(None) => not_found(format!("node {id} not found")).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

pub async fn update_node(
    State(graph): State<AppState>,
    Path(id): Path<u64>,
    Json(body): Json<UpdateNodeBody>,
) -> impl IntoResponse {
    match graph.update_node(id, &body.props) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(issundb::Error::NodeNotFound(_)) => {
            not_found(format!("node {id} not found")).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

pub async fn delete_node(State(graph): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    match graph.delete_node(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(issundb::Error::NodeNotFound(_)) => {
            not_found(format!("node {id} not found")).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Edge handlers
// ---------------------------------------------------------------------------

pub async fn create_edge(
    State(graph): State<AppState>,
    Json(body): Json<CreateEdgeBody>,
) -> impl IntoResponse {
    match graph.add_edge(body.src, body.dst, &body.edge_type, &body.props) {
        Ok(id) => (StatusCode::OK, Json(json!({ "id": id }))).into_response(),
        Err(issundb::Error::NodeNotFound(n)) => {
            bad_request(format!("node {n} not found")).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

pub async fn get_edge(State(graph): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    match graph.get_edge(id) {
        Ok(Some(record)) => {
            let edge_type = match graph.type_name(record.edge_type) {
                Ok(Some(t)) => t,
                Ok(None) => String::new(),
                Err(e) => return internal(e).into_response(),
            };
            let props: Value = match rmp_serde::from_slice(&record.props) {
                Ok(v) => v,
                Err(e) => return internal(e).into_response(),
            };
            (
                StatusCode::OK,
                Json(json!({
                    "id": id,
                    "src": record.src,
                    "dst": record.dst,
                    "type": edge_type,
                    "props": props
                })),
            )
                .into_response()
        }
        Ok(None) => not_found(format!("edge {id} not found")).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

pub async fn delete_edge(State(graph): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    match graph.delete_edge(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(issundb::Error::EdgeNotFound(_)) => {
            not_found(format!("edge {id} not found")).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Cypher handlers
// ---------------------------------------------------------------------------

pub async fn execute_query(
    State(graph): State<AppState>,
    Json(body): Json<CypherQueryBody>,
) -> impl IntoResponse {
    match graph.query_with_params(&body.query, &body.params) {
        Ok(result) => {
            let records: Vec<Vec<Value>> =
                result.records.iter().map(|r| r.values.clone()).collect();
            (
                StatusCode::OK,
                Json(json!({
                    "columns": result.columns,
                    "records": records
                })),
            )
                .into_response()
        }
        Err(e) => bad_request(e).into_response(),
    }
}

pub async fn explain_query(
    State(graph): State<AppState>,
    Json(body): Json<ExplainBody>,
) -> impl IntoResponse {
    match graph.explain(&body.query) {
        Ok(plan) => (StatusCode::OK, Json(json!({ "plan": plan }))).into_response(),
        Err(e) => bad_request(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Search handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TextHitResponse {
    node: u64,
    score: f32,
}

pub async fn search_text(
    State(graph): State<AppState>,
    Json(body): Json<TextSearchBody>,
) -> impl IntoResponse {
    let opts = TextSearchOptions {
        label: body.label,
        property: body.property,
        limit: body.limit,
        ..Default::default()
    };
    match graph.text_search(&body.query, &opts) {
        Ok(hits) => {
            let response: Vec<TextHitResponse> = hits
                .iter()
                .map(|h| TextHitResponse {
                    node: h.node,
                    score: h.score,
                })
                .collect();
            (StatusCode::OK, Json(json!(response))).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Serialize)]
struct VectorHitResponse {
    node: u64,
    distance: f32,
}

pub async fn search_vector(
    State(graph): State<AppState>,
    Json(body): Json<VectorSearchBody>,
) -> impl IntoResponse {
    if body.vector.is_empty() {
        return bad_request("vector must not be empty").into_response();
    }
    let opts = VectorSearchOptions {
        k: body.k,
        label: body.label,
    };
    match graph.vector_search_with(&body.vector, &opts) {
        Ok(hits) => {
            let response: Vec<VectorHitResponse> = hits
                .iter()
                .map(|h| VectorHitResponse {
                    node: h.node,
                    distance: h.distance,
                })
                .collect();
            (StatusCode::OK, Json(json!(response))).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Health handler
// ---------------------------------------------------------------------------

pub async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "api": "v1",
        })),
    )
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

pub fn build_router(graph: Arc<Graph>) -> Router {
    // Versioned data and query routes. `/health` stays unversioned so
    // infrastructure probes do not need to track the API version.
    let v1 = Router::new()
        .route("/nodes", post(create_node))
        .route("/nodes/:id", get(get_node))
        .route("/nodes/:id", put(update_node))
        .route("/nodes/:id", delete(delete_node))
        .route("/edges", post(create_edge))
        .route("/edges/:id", get(get_edge))
        .route("/edges/:id", delete(delete_edge))
        .route("/query", post(execute_query))
        .route("/explain", post(explain_query))
        .route("/search/text", post(search_text))
        .route("/search/vector", post(search_vector))
        .with_state(graph);

    Router::new().route("/health", get(health)).nest("/v1", v1)
}
