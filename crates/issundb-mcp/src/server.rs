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
