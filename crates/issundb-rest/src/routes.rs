use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRequest, FromRequestParts, Path, Request, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use issundb::{
    CypherError, FusionStrategy, Graph, GraphQueryExt, HybridRetrieveOptions, TextGraphExt,
    TextSearchOptions, VectorGraphExt, VectorSearchOptions, retrieve_hybrid,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use utoipa::{OpenApi, ToSchema};
use utoipa_scalar::{Scalar, Servable};

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

/// Map a Cypher error to an HTTP status: query-shape faults (parse, plan, type,
/// unbound variable, and math) are client errors, while execution and storage
/// faults on an otherwise-valid query are server errors.
fn cypher_status(e: &CypherError) -> StatusCode {
    match e {
        CypherError::Parse(_)
        | CypherError::Plan(_)
        | CypherError::TypeMismatch(_)
        | CypherError::VariableNotBound(_)
        | CypherError::Math(_) => StatusCode::BAD_REQUEST,
        CypherError::Execution(_) | CypherError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Await a blocking graph task, mapping a join failure (panic or cancellation)
/// to a 500. The synchronous `Graph` calls run on a blocking thread so they do
/// not stall the async worker pool.
async fn join(handle: tokio::task::JoinHandle<Response>) -> Response {
    handle.await.unwrap_or_else(|e| internal(e).into_response())
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// `Json` body extractor that renders a deserialization or content-type
/// rejection with the same `{"error": ...}` envelope the handlers use, instead
/// of axum's default plain-text rejection body.
pub struct JsonBody<T>(pub T);

#[axum::async_trait]
impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rej) => Err(err_json(rej.body_text(), rej.status()).into_response()),
        }
    }
}

/// `Path<u64>` extractor that renders an unparseable id with the JSON error
/// envelope rather than axum's plain-text rejection body.
pub struct PathU64(pub u64);

#[axum::async_trait]
impl<S> FromRequestParts<S> for PathU64
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<u64>::from_request_parts(parts, state).await {
            Ok(Path(id)) => Ok(PathU64(id)),
            Err(rej) => Err(err_json(rej.body_text(), rej.status()).into_response()),
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct CreateNodeBody {
    /// Single primary label. Either this or `labels` must be present.
    pub label: Option<String>,
    /// Additional labels for a multi-label node. Merged with `label`, which is
    /// placed first when both are given.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Free-form JSON property map for the node.
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateNodeBody {
    /// Replacement JSON property map for the node.
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateEdgeBody {
    pub src: u64,
    pub dst: u64,
    #[serde(rename = "type")]
    pub edge_type: String,
    /// Free-form JSON property map for the edge.
    #[serde(default)]
    pub props: Value,
}

#[derive(Deserialize, ToSchema)]
pub struct CypherQueryBody {
    pub query: String,
    /// Optional named query parameters referenced as `$name` in the query.
    #[serde(default)]
    pub params: HashMap<String, Value>,
}

#[derive(Deserialize, ToSchema)]
pub struct ExplainBody {
    pub query: String,
}

#[derive(Deserialize, ToSchema)]
pub struct TextSearchBody {
    pub query: String,
    pub label: Option<String>,
    pub property: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Deserialize, ToSchema)]
pub struct VectorSearchBody {
    pub vector: Vec<f32>,
    #[serde(default = "default_k")]
    pub k: usize,
    pub label: Option<String>,
    /// Optional exact-match property filter applied to candidate nodes.
    pub properties: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Optional rescore factor for quantized indexes.
    pub rescore_factor: Option<usize>,
}

fn default_limit() -> usize {
    10
}

fn default_k() -> usize {
    5
}

#[derive(Deserialize, ToSchema)]
pub struct UpsertVectorBody {
    pub id: u64,
    pub vector: Vec<f32>,
}

#[derive(Deserialize, ToSchema)]
pub struct HybridRetrieveBody {
    pub vector: Option<Vec<f32>>,
    pub text_query: Option<String>,
    pub vector_k: Option<usize>,
    pub text_k: Option<usize>,
    pub text_label: Option<String>,
    pub text_property: Option<String>,
    pub vector_label: Option<String>,
    pub hops: Option<u8>,
    pub max_distance: Option<f32>,
    pub max_nodes: Option<usize>,
    /// 'rrf' (default) or 'weighted_sum' (alias 'weighted').
    pub fusion_strategy: Option<String>,
    pub rrf_k: Option<u32>,
    pub vector_weight: Option<f32>,
    pub text_weight: Option<f32>,
}

/// Parse the fusion-strategy name and parameters into a [`FusionStrategy`].
/// Shared by the hybrid-retrieve handler so REST and MCP agree on the names.
fn parse_fusion(body: &HybridRetrieveBody) -> Result<FusionStrategy, String> {
    match body
        .fusion_strategy
        .as_deref()
        .unwrap_or("rrf")
        .to_lowercase()
        .as_str()
    {
        "rrf" => Ok(FusionStrategy::Rrf {
            k: body.rrf_k.unwrap_or(60),
        }),
        "weighted_sum" | "weighted" => Ok(FusionStrategy::WeightedSum {
            vector_weight: body.vector_weight.unwrap_or(0.5),
            text_weight: body.text_weight.unwrap_or(0.5),
        }),
        s => Err(format!("invalid fusion strategy: {s}")),
    }
}

// ---------------------------------------------------------------------------
// Response schemas (documentation only)
//
// The handlers build their JSON bodies inline with `json!`, so these structs
// are never constructed. They exist to give the OpenAPI document an accurate
// response shape; keep them in sync with the `json!` literals above.
// ---------------------------------------------------------------------------

/// `{"error": "..."}` envelope returned on every non-success status.
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct ErrorResponse {
    pub error: String,
}

/// `{"id": ...}` envelope returned by create and upsert endpoints.
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct IdResponse {
    pub id: u64,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct NodeResponse {
    pub id: u64,
    /// Primary (first) label, kept for convenience.
    pub label: String,
    /// Full label set, so multi-label nodes round-trip.
    pub labels: Vec<String>,
    pub props: Value,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct EdgeResponse {
    pub id: u64,
    pub src: u64,
    pub dst: u64,
    #[serde(rename = "type")]
    pub edge_type: String,
    pub props: Value,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct QueryResponse {
    pub columns: Vec<String>,
    /// Row-major result records. Each record is a list of arbitrary JSON values
    /// aligned with `columns`; the per-query value types are not statically
    /// known.
    pub records: Vec<Vec<Value>>,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct ExplainResponse {
    pub plan: String,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct RetrieveResponse {
    pub nodes: Vec<u64>,
    pub edges: Vec<u64>,
    /// Fused relevance score per node id.
    pub scores: HashMap<u64, f32>,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub api: String,
}

// ---------------------------------------------------------------------------
// Node handlers
// ---------------------------------------------------------------------------

/// Create a node with one or more labels and a JSON property map.
#[utoipa::path(
    post, path = "/v1/nodes", tag = "nodes",
    request_body = CreateNodeBody,
    responses(
        (status = 200, description = "Node created", body = IdResponse),
        (status = 400, description = "No label given or malformed body", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn create_node(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<CreateNodeBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
        // Merge the singular `label` (placed first) with any `labels`, dropping
        // duplicates while preserving order.
        let mut labels: Vec<String> = Vec::new();
        if let Some(label) = body.label {
            labels.push(label);
        }
        for label in body.labels {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        if labels.is_empty() {
            return bad_request("a node requires at least one label").into_response();
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        match graph.add_node_multi(&refs, &body.props) {
            Ok(id) => (StatusCode::OK, Json(json!({ "id": id }))).into_response(),
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

/// Fetch a node by id, including its full label set and properties.
#[utoipa::path(
    get, path = "/v1/nodes/{id}", tag = "nodes",
    params(("id" = u64, Path, description = "Node id")),
    responses(
        (status = 200, description = "Node found", body = NodeResponse),
        (status = 404, description = "Node not found", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn get_node(State(graph): State<AppState>, PathU64(id): PathU64) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.get_node(id) {
            Ok(Some(record)) => {
                let labels = match graph.node_labels(id) {
                    Ok(labels) => labels,
                    Err(e) => return internal(e).into_response(),
                };
                let props: Value = match rmp_serde::from_slice(&record.props) {
                    Ok(v) => v,
                    Err(e) => return internal(e).into_response(),
                };
                // `label` is the primary (first) label, kept for convenience;
                // `labels` carries the full set so multi-label nodes round-trip.
                let primary = labels.first().cloned().unwrap_or_default();
                (
                    StatusCode::OK,
                    Json(json!({ "id": id, "label": primary, "labels": labels, "props": props })),
                )
                    .into_response()
            }
            Ok(None) => not_found(format!("node {id} not found")).into_response(),
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

/// Replace a node's property map.
#[utoipa::path(
    put, path = "/v1/nodes/{id}", tag = "nodes",
    params(("id" = u64, Path, description = "Node id")),
    request_body = UpdateNodeBody,
    responses(
        (status = 204, description = "Node updated"),
        (status = 404, description = "Node not found", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn update_node(
    State(graph): State<AppState>,
    PathU64(id): PathU64,
    JsonBody(body): JsonBody<UpdateNodeBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.update_node(id, &body.props) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(issundb::Error::NodeNotFound(_)) => {
                not_found(format!("node {id} not found")).into_response()
            }
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

/// Delete a node and its incident edges.
#[utoipa::path(
    delete, path = "/v1/nodes/{id}", tag = "nodes",
    params(("id" = u64, Path, description = "Node id")),
    responses(
        (status = 204, description = "Node deleted"),
        (status = 404, description = "Node not found", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn delete_node(State(graph): State<AppState>, PathU64(id): PathU64) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.delete_node(id) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(issundb::Error::NodeNotFound(_)) => {
                not_found(format!("node {id} not found")).into_response()
            }
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

// ---------------------------------------------------------------------------
// Edge handlers
// ---------------------------------------------------------------------------

/// Create a typed edge between two existing nodes.
#[utoipa::path(
    post, path = "/v1/edges", tag = "edges",
    request_body = CreateEdgeBody,
    responses(
        (status = 200, description = "Edge created", body = IdResponse),
        (status = 400, description = "Endpoint node not found or malformed body", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn create_edge(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<CreateEdgeBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.add_edge(body.src, body.dst, &body.edge_type, &body.props) {
            Ok(id) => (StatusCode::OK, Json(json!({ "id": id }))).into_response(),
            Err(issundb::Error::NodeNotFound(n)) => {
                bad_request(format!("node {n} not found")).into_response()
            }
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

/// Fetch an edge by id, including its endpoints, type, and properties.
#[utoipa::path(
    get, path = "/v1/edges/{id}", tag = "edges",
    params(("id" = u64, Path, description = "Edge id")),
    responses(
        (status = 200, description = "Edge found", body = EdgeResponse),
        (status = 404, description = "Edge not found", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn get_edge(State(graph): State<AppState>, PathU64(id): PathU64) -> Response {
    join(tokio::task::spawn_blocking(move || {
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
    }))
    .await
}

/// Delete an edge by id.
#[utoipa::path(
    delete, path = "/v1/edges/{id}", tag = "edges",
    params(("id" = u64, Path, description = "Edge id")),
    responses(
        (status = 204, description = "Edge deleted"),
        (status = 404, description = "Edge not found", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn delete_edge(State(graph): State<AppState>, PathU64(id): PathU64) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.delete_edge(id) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(issundb::Error::EdgeNotFound(_)) => {
                not_found(format!("edge {id} not found")).into_response()
            }
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

// ---------------------------------------------------------------------------
// Cypher handlers
// ---------------------------------------------------------------------------

/// Execute a Cypher query, including mutations (CREATE, SET, REMOVE, DELETE,
/// MERGE), and return the column names and row-major records.
#[utoipa::path(
    post, path = "/v1/query", tag = "cypher",
    request_body = CypherQueryBody,
    responses(
        (status = 200, description = "Query executed", body = QueryResponse),
        (status = 400, description = "Parse, plan, type, unbound-variable, or math error", body = ErrorResponse),
        (status = 500, description = "Execution or storage error", body = ErrorResponse),
    ),
)]
pub async fn execute_query(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<CypherQueryBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
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
            Err(e) => err_json(&e, cypher_status(&e)).into_response(),
        }
    }))
    .await
}

/// Return the physical query plan for a Cypher query without executing it.
#[utoipa::path(
    post, path = "/v1/explain", tag = "cypher",
    request_body = ExplainBody,
    responses(
        (status = 200, description = "Plan produced", body = ExplainResponse),
        (status = 400, description = "Parse, plan, type, unbound-variable, or math error", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn explain_query(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<ExplainBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
        match graph.explain(&body.query) {
            Ok(plan) => (StatusCode::OK, Json(json!({ "plan": plan }))).into_response(),
            Err(e) => err_json(&e, cypher_status(&e)).into_response(),
        }
    }))
    .await
}

// ---------------------------------------------------------------------------
// Search handlers
// ---------------------------------------------------------------------------

#[derive(Serialize, ToSchema)]
struct TextHitResponse {
    node: u64,
    score: f32,
}

/// Full-text search over indexed node properties, ranked by relevance.
#[utoipa::path(
    post, path = "/v1/search/text", tag = "search",
    request_body = TextSearchBody,
    responses(
        (status = 200, description = "Ranked text hits", body = [TextHitResponse]),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn search_text(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<TextSearchBody>,
) -> Response {
    join(tokio::task::spawn_blocking(move || {
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
    }))
    .await
}

#[derive(Serialize, ToSchema)]
struct VectorHitResponse {
    node: u64,
    distance: f32,
}

/// Nearest-neighbor vector search with optional label and property filters.
#[utoipa::path(
    post, path = "/v1/search/vector", tag = "search",
    request_body = VectorSearchBody,
    responses(
        (status = 200, description = "Nearest neighbors by distance", body = [VectorHitResponse]),
        (status = 400, description = "Empty query vector", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn search_vector(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<VectorSearchBody>,
) -> Response {
    if body.vector.is_empty() {
        return bad_request("vector must not be empty").into_response();
    }
    join(tokio::task::spawn_blocking(move || {
        let opts = VectorSearchOptions {
            k: body.k,
            label: body.label,
            properties: body.properties,
            rescore_factor: body.rescore_factor,
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
    }))
    .await
}

// ---------------------------------------------------------------------------
// Vector upsert handler
// ---------------------------------------------------------------------------

/// Upsert the embedding vector for a node id.
#[utoipa::path(
    post, path = "/v1/vectors", tag = "vectors",
    request_body = UpsertVectorBody,
    responses(
        (status = 200, description = "Vector stored", body = IdResponse),
        (status = 400, description = "Empty vector", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn upsert_vector(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<UpsertVectorBody>,
) -> Response {
    if body.vector.is_empty() {
        return bad_request("vector must not be empty").into_response();
    }
    join(tokio::task::spawn_blocking(move || {
        match graph.upsert_vector(body.id, &body.vector) {
            Ok(()) => (StatusCode::OK, Json(json!({ "id": body.id }))).into_response(),
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

// ---------------------------------------------------------------------------
// Hybrid retrieval handler
// ---------------------------------------------------------------------------

/// Hybrid retrieval fusing vector and text hits, then expanding the graph
/// neighborhood, returning the induced subgraph and per-node scores.
#[utoipa::path(
    post, path = "/v1/retrieve", tag = "retrieve",
    request_body = HybridRetrieveBody,
    responses(
        (status = 200, description = "Induced subgraph with fused scores", body = RetrieveResponse),
        (status = 400, description = "Invalid fusion strategy", body = ErrorResponse),
        (status = 500, description = "Storage error", body = ErrorResponse),
    ),
)]
pub async fn retrieve(
    State(graph): State<AppState>,
    JsonBody(body): JsonBody<HybridRetrieveBody>,
) -> Response {
    let fusion = match parse_fusion(&body) {
        Ok(f) => f,
        Err(e) => return bad_request(e).into_response(),
    };
    join(tokio::task::spawn_blocking(move || {
        let vector = body.vector.unwrap_or_default();
        let text_query = body.text_query.unwrap_or_default();
        let opts = HybridRetrieveOptions {
            vector_k: body.vector_k.unwrap_or(10),
            text_k: body.text_k.unwrap_or(10),
            text_label: body.text_label,
            text_property: body.text_property,
            hops: body.hops.unwrap_or(2),
            max_distance: body.max_distance.unwrap_or(f32::MAX),
            max_nodes: body.max_nodes,
            vector_label: body.vector_label,
            fusion,
        };
        match retrieve_hybrid(&graph, &vector, &text_query, &opts) {
            Ok(subgraph) => (
                StatusCode::OK,
                Json(json!({
                    "nodes": subgraph.nodes,
                    "edges": subgraph.edges,
                    "scores": subgraph.scores,
                })),
            )
                .into_response(),
            Err(e) => internal(e).into_response(),
        }
    }))
    .await
}

// ---------------------------------------------------------------------------
// Health handler
// ---------------------------------------------------------------------------

/// Liveness probe reporting the crate version and API version.
#[utoipa::path(
    get, path = "/health", tag = "health",
    responses((status = 200, description = "Service is up", body = HealthResponse)),
)]
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
// OpenAPI document
// ---------------------------------------------------------------------------

/// OpenAPI document for the REST API, generated from the `#[utoipa::path]`
/// annotations on the handlers and the `ToSchema` derives on the request and
/// response types. Served as JSON at `GET /v1/openapi.json`.
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "IssunDB REST API",
        description = "A REST API server implementation for IssunDB graph database. \
         The server implementation exposes the core functionalies of IssunDB over HTTP.",
    ),
    paths(
        create_node, get_node, update_node, delete_node,
        create_edge, get_edge, delete_edge,
        execute_query, explain_query,
        search_text, search_vector,
        upsert_vector, retrieve, health,
    ),
    components(schemas(
        CreateNodeBody, UpdateNodeBody, CreateEdgeBody, CypherQueryBody, ExplainBody,
        TextSearchBody, VectorSearchBody, UpsertVectorBody, HybridRetrieveBody,
        ErrorResponse, IdResponse, NodeResponse, EdgeResponse, QueryResponse,
        ExplainResponse, TextHitResponse, VectorHitResponse, RetrieveResponse,
        HealthResponse,
    )),
    tags(
        (name = "nodes", description = "Node CRUD"),
        (name = "edges", description = "Edge CRUD"),
        (name = "cypher", description = "Cypher query and plan"),
        (name = "search", description = "Text and vector search"),
        (name = "vectors", description = "Embedding upsert"),
        (name = "retrieve", description = "Hybrid retrieval"),
        (name = "health", description = "Liveness"),
    ),
)]
pub struct ApiDoc;

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
        .route("/vectors", post(upsert_vector))
        .route("/retrieve", post(retrieve))
        .with_state(graph);

    // Serve the generated OpenAPI document and an interactive Scalar UI. The
    // document is generated from the handler annotations, so it cannot drift
    // from the routes above.
    let docs: Router = Scalar::with_url("/v1/docs", ApiDoc::openapi()).into();
    Router::new()
        .route("/health", get(health))
        .route("/v1/openapi.json", get(openapi_json))
        .merge(docs)
        .nest("/v1", v1)
}

/// Return the generated OpenAPI document as JSON.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}
