use std::{collections::HashMap, sync::Arc};

use issundb::{
    Error, Graph, GraphQueryExt, TextGraphExt, TextSearchOptions, VectorGraphExt,
    VectorSearchOptions,
};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// MCP server over a shared `Graph`. All tools dispatch through the `issundb`
/// public facade; this crate never touches storage internals.
#[derive(Clone)]
pub struct IssunMcp {
    graph: Arc<Graph>,
    tool_router: ToolRouter<IssunMcp>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(
        value.to_string(),
    )]))
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn invalid(e: impl std::fmt::Display) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

// ---------------------------------------------------------------------------
// Tool argument types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddNodeArgs {
    /// Label to attach to the new node.
    pub label: String,
    /// Arbitrary JSON property map for the node.
    #[serde(default)]
    pub props: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NodeIdArgs {
    /// Node identifier.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateNodeArgs {
    /// Identifier of the node to update.
    pub id: u64,
    /// New JSON property map; replaces the existing properties.
    #[serde(default)]
    pub props: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddEdgeArgs {
    /// Source node identifier.
    pub src: u64,
    /// Destination node identifier.
    pub dst: u64,
    /// Edge type name.
    #[serde(rename = "type")]
    pub edge_type: String,
    /// Arbitrary JSON property map for the edge.
    #[serde(default)]
    pub props: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeIdArgs {
    /// Edge identifier.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CypherArgs {
    /// Cypher query text.
    pub query: String,
    /// Optional parameter bindings referenced by `$name` in the query.
    #[serde(default)]
    pub params: HashMap<String, Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainArgs {
    /// Cypher query text to plan.
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchArgs {
    /// Full-text query string.
    pub query: String,
    /// Optional label to restrict the search to.
    pub label: Option<String>,
    /// Optional property to restrict the search to.
    pub property: Option<String>,
    /// Maximum number of hits to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VectorSearchArgs {
    /// Dense query vector.
    pub vector: Vec<f32>,
    /// Number of nearest neighbors to return.
    #[serde(default = "default_k")]
    pub k: usize,
    /// Optional label to restrict the search to.
    pub label: Option<String>,
}

fn default_limit() -> usize {
    10
}

fn default_k() -> usize {
    5
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl IssunMcp {
    pub fn new(graph: Arc<Graph>) -> Self {
        Self {
            graph,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Create a node with a label and JSON properties; returns the new node id."
    )]
    fn add_node(
        &self,
        Parameters(args): Parameters<AddNodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .graph
            .add_node(&args.label, &args.props)
            .map_err(internal)?;
        ok_json(json!({ "id": id }))
    }

    #[tool(description = "Fetch a node by id, returning its label and properties.")]
    fn get_node(
        &self,
        Parameters(args): Parameters<NodeIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.get_node(args.id).map_err(internal)? {
            Some(record) => {
                let label = self
                    .graph
                    .node_labels(args.id)
                    .map_err(internal)?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let props: Value = rmp_serde::from_slice(&record.props).map_err(internal)?;
                ok_json(json!({ "id": args.id, "label": label, "props": props }))
            }
            None => Err(invalid(format!("node {} not found", args.id))),
        }
    }

    #[tool(description = "Replace the properties of an existing node.")]
    fn update_node(
        &self,
        Parameters(args): Parameters<UpdateNodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.update_node(args.id, &args.props) {
            Ok(()) => ok_json(json!({ "id": args.id, "updated": true })),
            Err(Error::NodeNotFound(_)) => Err(invalid(format!("node {} not found", args.id))),
            Err(e) => Err(internal(e)),
        }
    }

    #[tool(description = "Delete a node by id along with its incident edges.")]
    fn delete_node(
        &self,
        Parameters(args): Parameters<NodeIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.delete_node(args.id) {
            Ok(()) => ok_json(json!({ "id": args.id, "deleted": true })),
            Err(Error::NodeNotFound(_)) => Err(invalid(format!("node {} not found", args.id))),
            Err(e) => Err(internal(e)),
        }
    }

    #[tool(description = "Create a directed edge between two nodes; returns the new edge id.")]
    fn add_edge(
        &self,
        Parameters(args): Parameters<AddEdgeArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .graph
            .add_edge(args.src, args.dst, &args.edge_type, &args.props)
        {
            Ok(id) => ok_json(json!({ "id": id })),
            Err(Error::NodeNotFound(n)) => Err(invalid(format!("node {n} not found"))),
            Err(e) => Err(internal(e)),
        }
    }

    #[tool(description = "Fetch an edge by id, returning its endpoints, type, and properties.")]
    fn get_edge(
        &self,
        Parameters(args): Parameters<EdgeIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.get_edge(args.id).map_err(internal)? {
            Some(record) => {
                let edge_type = self
                    .graph
                    .type_name(record.edge_type)
                    .map_err(internal)?
                    .unwrap_or_default();
                let props: Value = rmp_serde::from_slice(&record.props).map_err(internal)?;
                ok_json(json!({
                    "id": args.id,
                    "src": record.src,
                    "dst": record.dst,
                    "type": edge_type,
                    "props": props,
                }))
            }
            None => Err(invalid(format!("edge {} not found", args.id))),
        }
    }

    #[tool(description = "Delete an edge by id.")]
    fn delete_edge(
        &self,
        Parameters(args): Parameters<EdgeIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.delete_edge(args.id) {
            Ok(()) => ok_json(json!({ "id": args.id, "deleted": true })),
            Err(Error::EdgeNotFound(_)) => Err(invalid(format!("edge {} not found", args.id))),
            Err(e) => Err(internal(e)),
        }
    }

    #[tool(
        description = "Execute a Cypher query with optional parameters; returns columns and records."
    )]
    fn cypher_query(
        &self,
        Parameters(args): Parameters<CypherArgs>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .graph
            .query_with_params(&args.query, &args.params)
            .map_err(invalid)?;
        let records: Vec<Vec<Value>> = result.records.iter().map(|r| r.values.clone()).collect();
        ok_json(json!({ "columns": result.columns, "records": records }))
    }

    #[tool(description = "Return the physical query plan for a Cypher query as an indented tree.")]
    fn explain(
        &self,
        Parameters(args): Parameters<ExplainArgs>,
    ) -> Result<CallToolResult, McpError> {
        let plan = self.graph.explain(&args.query).map_err(invalid)?;
        ok_json(json!({ "plan": plan }))
    }

    #[tool(description = "Full-text search over indexed node properties; returns ranked hits.")]
    fn text_search(
        &self,
        Parameters(args): Parameters<TextSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let opts = TextSearchOptions {
            label: args.label,
            property: args.property,
            limit: args.limit,
            ..Default::default()
        };
        let hits = self
            .graph
            .text_search(&args.query, &opts)
            .map_err(internal)?;
        let response: Vec<Value> = hits
            .iter()
            .map(|h| json!({ "node": h.node, "score": h.score }))
            .collect();
        ok_json(json!(response))
    }

    #[tool(
        description = "Nearest-neighbor vector search; returns the k closest nodes by distance."
    )]
    fn vector_search(
        &self,
        Parameters(args): Parameters<VectorSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.vector.is_empty() {
            return Err(invalid("vector must not be empty"));
        }
        let opts = VectorSearchOptions {
            k: args.k,
            label: args.label,
        };
        let hits = self
            .graph
            .vector_search_with(&args.vector, &opts)
            .map_err(internal)?;
        let response: Vec<Value> = hits
            .iter()
            .map(|h| json!({ "node": h.node, "distance": h.distance }))
            .collect();
        ok_json(json!(response))
    }
}

// ---------------------------------------------------------------------------
// Server handler
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for IssunMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            // Track the SDK's newest vetted protocol revision rather than pinning a date.
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "IssunDB graph database. Tools: add_node, get_node, update_node, delete_node, \
                 add_edge, get_edge, delete_edge, cypher_query, explain, text_search, and \
                 vector_search."
                    .to_string(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// The tool methods are inherent methods on `IssunMcp`, so they are exercised
// directly here against a fresh `TempDir`-backed `Graph`; the JSON-RPC and
// transport layers are not involved. Each test opens its own graph and shares
// no state with the others.
#[cfg(test)]
mod tests {
    use super::*;
    use issundb::TextIndexExt;
    use tempfile::TempDir;

    /// Open a fresh graph in a temp directory and wrap it in an `IssunMcp`. The
    /// `TempDir` is returned so the caller keeps it alive for the test.
    fn fresh() -> (IssunMcp, TempDir) {
        let dir = TempDir::new().expect("temp dir");
        let graph = Graph::open(dir.path(), 1).expect("open graph");
        (IssunMcp::new(Arc::new(graph)), dir)
    }

    /// Parse the JSON text payload a tool returned on success.
    fn body(result: CallToolResult) -> Value {
        let text = result.content[0]
            .as_text()
            .expect("text content")
            .text
            .clone();
        serde_json::from_str(&text).expect("json body")
    }

    fn add_person(mcp: &IssunMcp, name: &str) -> u64 {
        let result = mcp
            .add_node(Parameters(AddNodeArgs {
                label: "Person".to_string(),
                props: json!({ "name": name }),
            }))
            .expect("add_node");
        body(result)["id"].as_u64().expect("id")
    }

    #[test]
    fn add_node_returns_id() {
        let (mcp, _dir) = fresh();
        let result = mcp
            .add_node(Parameters(AddNodeArgs {
                label: "Person".to_string(),
                props: json!({ "name": "Ada" }),
            }))
            .expect("add_node");
        assert!(body(result)["id"].is_u64());
    }

    #[test]
    fn get_node_round_trips_label_and_props() {
        let (mcp, _dir) = fresh();
        let id = add_person(&mcp, "Ada");
        let result = mcp
            .get_node(Parameters(NodeIdArgs { id }))
            .expect("get_node");
        let value = body(result);
        assert_eq!(value["id"].as_u64(), Some(id));
        assert_eq!(value["label"], "Person");
        assert_eq!(value["props"]["name"], "Ada");
    }

    #[test]
    fn get_node_missing_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .get_node(Parameters(NodeIdArgs { id: 999 }))
            .expect_err("missing node");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn update_node_replaces_props() {
        let (mcp, _dir) = fresh();
        let id = add_person(&mcp, "Ada");
        mcp.update_node(Parameters(UpdateNodeArgs {
            id,
            props: json!({ "name": "Grace" }),
        }))
        .expect("update_node");
        let value = body(
            mcp.get_node(Parameters(NodeIdArgs { id }))
                .expect("get_node"),
        );
        assert_eq!(value["props"]["name"], "Grace");
    }

    #[test]
    fn update_node_missing_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .update_node(Parameters(UpdateNodeArgs {
                id: 999,
                props: json!({}),
            }))
            .expect_err("missing node");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn delete_node_marks_deleted_and_removes_it() {
        let (mcp, _dir) = fresh();
        let id = add_person(&mcp, "Ada");
        let value = body(
            mcp.delete_node(Parameters(NodeIdArgs { id }))
                .expect("delete"),
        );
        assert_eq!(value["deleted"], true);
        let err = mcp
            .get_node(Parameters(NodeIdArgs { id }))
            .expect_err("gone");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn delete_node_missing_is_idempotent() {
        // `Graph::delete_node` does not report `NodeNotFound` for an absent id;
        // deletion is idempotent, so the tool reports success.
        let (mcp, _dir) = fresh();
        let value = body(
            mcp.delete_node(Parameters(NodeIdArgs { id: 999 }))
                .expect("delete_node"),
        );
        assert_eq!(value["deleted"], true);
    }

    #[test]
    fn add_edge_returns_id() {
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let b = add_person(&mcp, "Grace");
        let result = mcp
            .add_edge(Parameters(AddEdgeArgs {
                src: a,
                dst: b,
                edge_type: "KNOWS".to_string(),
                props: json!({}),
            }))
            .expect("add_edge");
        assert!(body(result)["id"].is_u64());
    }

    #[test]
    fn add_edge_with_missing_endpoint_currently_succeeds() {
        // Pins the issue #14 behavior: `Graph::add_edge` does not validate that
        // its endpoints exist, so a dangling edge is created instead of an error.
        // When the core adds endpoint validation, the server's `NodeNotFound`
        // arm becomes reachable and this test should flip to expect a rejection.
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let result = mcp
            .add_edge(Parameters(AddEdgeArgs {
                src: a,
                dst: 999,
                edge_type: "KNOWS".to_string(),
                props: json!({}),
            }))
            .expect("add_edge currently succeeds");
        assert!(body(result)["id"].is_u64());
    }

    #[test]
    fn get_edge_round_trips_endpoints_and_type() {
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let b = add_person(&mcp, "Grace");
        let edge_id = body(
            mcp.add_edge(Parameters(AddEdgeArgs {
                src: a,
                dst: b,
                edge_type: "KNOWS".to_string(),
                props: json!({ "since": 2020 }),
            }))
            .expect("add_edge"),
        )["id"]
            .as_u64()
            .expect("edge id");
        let value = body(
            mcp.get_edge(Parameters(EdgeIdArgs { id: edge_id }))
                .expect("get_edge"),
        );
        assert_eq!(value["src"].as_u64(), Some(a));
        assert_eq!(value["dst"].as_u64(), Some(b));
        assert_eq!(value["type"], "KNOWS");
        assert_eq!(value["props"]["since"], 2020);
    }

    #[test]
    fn get_edge_missing_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .get_edge(Parameters(EdgeIdArgs { id: 999 }))
            .expect_err("missing edge");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn delete_edge_marks_deleted() {
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let b = add_person(&mcp, "Grace");
        let edge_id = body(
            mcp.add_edge(Parameters(AddEdgeArgs {
                src: a,
                dst: b,
                edge_type: "KNOWS".to_string(),
                props: json!({}),
            }))
            .expect("add_edge"),
        )["id"]
            .as_u64()
            .expect("edge id");
        let value = body(
            mcp.delete_edge(Parameters(EdgeIdArgs { id: edge_id }))
                .expect("delete_edge"),
        );
        assert_eq!(value["deleted"], true);
    }

    #[test]
    fn delete_edge_missing_is_idempotent() {
        // As with node deletion, `Graph::delete_edge` does not report
        // `EdgeNotFound` for an absent id; the tool reports success.
        let (mcp, _dir) = fresh();
        let value = body(
            mcp.delete_edge(Parameters(EdgeIdArgs { id: 999 }))
                .expect("delete_edge"),
        );
        assert_eq!(value["deleted"], true);
    }

    #[test]
    fn cypher_query_returns_columns_and_records() {
        let (mcp, _dir) = fresh();
        add_person(&mcp, "Ada");
        let result = mcp
            .cypher_query(Parameters(CypherArgs {
                query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
                params: HashMap::new(),
            }))
            .expect("cypher_query");
        let value = body(result);
        assert_eq!(value["columns"], json!(["name"]));
        assert_eq!(value["records"], json!([["Ada"]]));
    }

    #[test]
    fn cypher_query_honors_params() {
        let (mcp, _dir) = fresh();
        add_person(&mcp, "Ada");
        add_person(&mcp, "Grace");
        let mut params = HashMap::new();
        params.insert("who".to_string(), json!("Grace"));
        let result = mcp
            .cypher_query(Parameters(CypherArgs {
                query: "MATCH (n:Person) WHERE n.name = $who RETURN n.name AS name".to_string(),
                params,
            }))
            .expect("cypher_query");
        assert_eq!(body(result)["records"], json!([["Grace"]]));
    }

    #[test]
    fn cypher_query_invalid_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .cypher_query(Parameters(CypherArgs {
                query: "MATCH (n RETURN".to_string(),
                params: HashMap::new(),
            }))
            .expect_err("parse error");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn explain_returns_plan() {
        let (mcp, _dir) = fresh();
        let result = mcp
            .explain(Parameters(ExplainArgs {
                query: "MATCH (n:Person) RETURN n".to_string(),
            }))
            .expect("explain");
        assert!(body(result)["plan"].as_str().is_some_and(|p| !p.is_empty()));
    }

    #[test]
    fn explain_invalid_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .explain(Parameters(ExplainArgs {
                query: "MATCH (n RETURN".to_string(),
            }))
            .expect_err("parse error");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn text_search_returns_ranked_hits() {
        let (mcp, _dir) = fresh();
        mcp.graph
            .create_text_index("Doc", "body")
            .expect("create index");
        let id = mcp
            .graph
            .add_node("Doc", &json!({ "body": "the quick brown fox" }))
            .expect("add doc");
        let result = mcp
            .text_search(Parameters(TextSearchArgs {
                query: "quick fox".to_string(),
                label: Some("Doc".to_string()),
                property: Some("body".to_string()),
                limit: 10,
            }))
            .expect("text_search");
        let hits = body(result);
        assert_eq!(hits.as_array().map(|a| a.len()), Some(1));
        assert_eq!(hits[0]["node"].as_u64(), Some(id));
    }

    #[test]
    fn vector_search_returns_nearest_node() {
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let b = add_person(&mcp, "Grace");
        mcp.graph
            .upsert_vector(a, &[1.0, 0.0, 0.0])
            .expect("upsert a");
        mcp.graph
            .upsert_vector(b, &[0.0, 1.0, 0.0])
            .expect("upsert b");
        let result = mcp
            .vector_search(Parameters(VectorSearchArgs {
                vector: vec![1.0, 0.0, 0.0],
                k: 1,
                label: None,
            }))
            .expect("vector_search");
        let hits = body(result);
        assert_eq!(hits.as_array().map(|a| a.len()), Some(1));
        assert_eq!(hits[0]["node"].as_u64(), Some(a));
    }

    #[test]
    fn vector_search_empty_vector_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .vector_search(Parameters(VectorSearchArgs {
                vector: vec![],
                k: 5,
                label: None,
            }))
            .expect_err("empty vector");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }
}
