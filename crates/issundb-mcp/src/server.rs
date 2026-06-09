use std::{collections::HashMap, sync::Arc};

use issundb::{
    Error, FusionStrategy, Graph, GraphQueryExt, HybridRetrieveOptions, Language, TextGraphExt,
    TextIndexExt, TextSearchOptions, VectorGraphExt, VectorIndexOptions, VectorMetric,
    VectorQuantization, VectorSearchOptions, retrieve_hybrid,
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
    /// Primary label to attach to the new node. Required unless 'labels' is provided.
    pub label: Option<String>,
    /// List of labels to attach to the new node (multi-label support). Required unless 'label' is provided.
    pub labels: Option<Vec<String>>,
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
    /// Optional property key-value filters. Only nodes matching all filters are returned.
    pub properties: Option<HashMap<String, Value>>,
}

fn default_limit() -> usize {
    10
}

fn default_k() -> usize {
    5
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetThreadCountArgs {
    /// The number of threads to use for GraphBLAS computations (set to 0 to restore default behavior).
    pub count: i32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureVectorIndexArgs {
    /// Distance metric: 'cosine', 'l2', or 'dot' (alias 'ip').
    pub metric: String,
    /// Quantization mode: 'float32', 'float16', or 'int8'.
    pub quantization: String,
    /// Rebuild the index from existing stored vectors under the new configuration.
    #[serde(default)]
    pub reindex: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateTextIndexArgs {
    /// Label to attach the index to.
    pub label: String,
    /// Property name to index.
    pub property: String,
    /// Optional stemming/tokenization language: 'english', 'spanish', 'french', 'german', 'italian', or 'portuguese'.
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DropTextIndexArgs {
    /// Label of the index to drop.
    pub label: String,
    /// Property name of the index to drop.
    pub property: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HybridRetrieveArgs {
    /// Query vector for semantic/vector search. Optional; if omitted, vector search is skipped.
    pub vector: Option<Vec<f32>>,
    /// Query string for full-text search. Optional; if omitted, text search is skipped.
    pub text_query: Option<String>,
    /// Maximum number of seeds to retrieve from vector search (defaults to 10).
    pub vector_k: Option<usize>,
    /// Maximum number of seeds to retrieve from text search (defaults to 10).
    pub text_k: Option<usize>,
    /// Optional label filter for text search.
    pub text_label: Option<String>,
    /// Optional property filter for text search.
    pub text_property: Option<String>,
    /// Optional label filter for vector search.
    pub vector_label: Option<String>,
    /// BFS expansion depth from seed nodes (defaults to 2).
    pub hops: Option<u8>,
    /// Maximum cosine distance for vector hits to qualify as seeds.
    pub max_distance: Option<f32>,
    /// Optional hard cap on total subgraph nodes returned.
    pub max_nodes: Option<usize>,
    /// Fusion strategy: 'rrf' (Reciprocal Rank Fusion) or 'weighted_sum' (defaults to 'rrf').
    pub fusion_strategy: Option<String>,
    /// Constant parameter for 'rrf' strategy (defaults to 60).
    pub rrf_k: Option<u32>,
    /// Vector weight for 'weighted_sum' strategy (defaults to 0.5).
    pub vector_weight: Option<f32>,
    /// Text weight for 'weighted_sum' strategy (defaults to 0.5).
    pub text_weight: Option<f32>,
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
        description = "Create a node with one or more labels and JSON properties; returns the new node id."
    )]
    fn add_node(
        &self,
        Parameters(args): Parameters<AddNodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let labels = match &args.labels {
            Some(l) => l.clone(),
            None => match &args.label {
                Some(l) => vec![l.clone()],
                None => Vec::new(),
            },
        };
        if labels.is_empty() {
            return Err(invalid(
                "a node requires at least one label (provide 'label' or 'labels')",
            ));
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let id = self
            .graph
            .add_node_multi(&refs, &args.props)
            .map_err(internal)?;
        ok_json(json!({ "id": id }))
    }

    #[tool(description = "Fetch a node by id, returning its label, labels, and properties.")]
    fn get_node(
        &self,
        Parameters(args): Parameters<NodeIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.graph.get_node(args.id).map_err(internal)? {
            Some(record) => {
                let labels = self.graph.node_labels(args.id).map_err(internal)?;
                let label = labels.first().cloned().unwrap_or_default();
                let props: Value = rmp_serde::from_slice(&record.props).map_err(internal)?;
                ok_json(json!({ "id": args.id, "label": label, "labels": labels, "props": props }))
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
        description = "Nearest-neighbor vector search; returns the k closest nodes by distance, with optional property filtering."
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
            properties: args.properties,
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

    #[tool(
        description = "Set the thread count for GraphBLAS computations; set to 0 to restore default behavior."
    )]
    fn set_thread_count(
        &self,
        Parameters(args): Parameters<SetThreadCountArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.graph.set_thread_count(args.count).map_err(internal)?;
        ok_json(json!({ "success": true }))
    }

    #[tool(
        description = "Configure or rebuild the vector index. Option metric: 'cosine', 'l2', or 'dot' (alias 'ip'). Option quantization: 'float32', 'float16', or 'int8'."
    )]
    fn configure_vector_index(
        &self,
        Parameters(args): Parameters<ConfigureVectorIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let metric = match args.metric.to_lowercase().as_str() {
            "cosine" => VectorMetric::Cosine,
            "l2" => VectorMetric::L2,
            "dot" | "ip" => VectorMetric::Dot,
            _ => return Err(invalid(format!("invalid metric: {}", args.metric))),
        };
        let quantization = match args.quantization.to_lowercase().as_str() {
            "float32" => VectorQuantization::Float32,
            "float16" => VectorQuantization::Float16,
            "int8" => VectorQuantization::Int8,
            _ => {
                return Err(invalid(format!(
                    "invalid quantization: {}",
                    args.quantization
                )));
            }
        };
        let opts = VectorIndexOptions {
            metric,
            quantization,
        };
        if args.reindex {
            self.graph.reindex_vector_index(opts).map_err(internal)?;
        } else {
            self.graph.configure_vector_index(opts).map_err(internal)?;
        }
        ok_json(json!({ "success": true }))
    }

    #[tool(
        description = "Create a text search index on a label and property, optionally specifying stemming/tokenization language."
    )]
    fn create_text_index(
        &self,
        Parameters(args): Parameters<CreateTextIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let language = match args
            .language
            .as_deref()
            .unwrap_or("english")
            .to_lowercase()
            .as_str()
        {
            "spanish" => Language::Spanish,
            "french" => Language::French,
            "german" => Language::German,
            "italian" => Language::Italian,
            "portuguese" => Language::Portuguese,
            "english" => Language::English,
            lang => return Err(invalid(format!("invalid language: {}", lang))),
        };
        self.graph
            .create_text_index_with_language(&args.label, &args.property, language)
            .map_err(internal)?;
        ok_json(json!({ "success": true }))
    }

    #[tool(description = "Drop an existing text search index.")]
    fn drop_text_index(
        &self,
        Parameters(args): Parameters<DropTextIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.graph
            .drop_text_index(&args.label, &args.property)
            .map_err(internal)?;
        ok_json(json!({ "success": true }))
    }

    #[tool(description = "List all active text search indexes.")]
    fn list_text_indexes(&self) -> Result<CallToolResult, McpError> {
        let indexes = self.graph.list_text_indexes().map_err(internal)?;
        let list: Vec<Value> = indexes
            .into_iter()
            .map(|(label, property, language)| {
                json!({
                    "label": label,
                    "property": property,
                    "language": format!("{:?}", language).to_lowercase(),
                })
            })
            .collect();
        ok_json(json!(list))
    }

    #[tool(
        description = "Execute a hybrid retrieval (GraphRAG) query combining vector/semantic search, full-text keyword search, and relationship expansion."
    )]
    fn retrieve_hybrid(
        &self,
        Parameters(args): Parameters<HybridRetrieveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let vector = args.vector.unwrap_or_default();
        let text_query = args.text_query.unwrap_or_default();

        let fusion = match args
            .fusion_strategy
            .as_deref()
            .unwrap_or("rrf")
            .to_lowercase()
            .as_str()
        {
            "rrf" => FusionStrategy::Rrf {
                k: args.rrf_k.unwrap_or(60),
            },
            "weighted_sum" | "weighted" => FusionStrategy::WeightedSum {
                vector_weight: args.vector_weight.unwrap_or(0.5),
                text_weight: args.text_weight.unwrap_or(0.5),
            },
            s => return Err(invalid(format!("invalid fusion strategy: {}", s))),
        };

        let opts = HybridRetrieveOptions {
            vector_k: args.vector_k.unwrap_or(10),
            text_k: args.text_k.unwrap_or(10),
            text_label: args.text_label,
            text_property: args.text_property,
            hops: args.hops.unwrap_or(2),
            max_distance: args.max_distance.unwrap_or(f32::MAX),
            max_nodes: args.max_nodes,
            vector_label: args.vector_label,
            fusion,
        };

        let subgraph =
            retrieve_hybrid(&self.graph, &vector, &text_query, &opts).map_err(internal)?;

        ok_json(json!({
            "nodes": subgraph.nodes,
            "edges": subgraph.edges,
            "scores": subgraph.scores,
        }))
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
                 add_edge, get_edge, delete_edge, cypher_query, explain, text_search, \
                 vector_search, configure_vector_index, create_text_index, drop_text_index, \
                 list_text_indexes, retrieve_hybrid, and set_thread_count."
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
                label: Some("Person".to_string()),
                labels: None,
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
                label: Some("Person".to_string()),
                labels: None,
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
        assert_eq!(value["labels"], json!(["Person"]));
        assert_eq!(value["props"]["name"], "Ada");
    }

    #[test]
    fn add_node_multi_labels_round_trip() {
        let (mcp, _dir) = fresh();
        let result = mcp
            .add_node(Parameters(AddNodeArgs {
                label: None,
                labels: Some(vec!["Person".to_string(), "Actor".to_string()]),
                props: json!({ "name": "Keanu" }),
            }))
            .expect("add_node");
        let id = body(result)["id"].as_u64().expect("id");

        let result = mcp
            .get_node(Parameters(NodeIdArgs { id }))
            .expect("get_node");
        let value = body(result);
        assert_eq!(value["id"].as_u64(), Some(id));
        assert_eq!(value["label"], "Person");
        assert_eq!(value["labels"], json!(["Person", "Actor"]));
        assert_eq!(value["props"]["name"], "Keanu");
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
                properties: None,
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
                properties: None,
            }))
            .expect_err("empty vector");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn vector_search_with_property_filter_succeeds() {
        let (mcp, _dir) = fresh();
        let a = add_person(&mcp, "Ada");
        let b = add_person(&mcp, "Grace");
        mcp.graph
            .upsert_vector(a, &[1.0, 0.0, 0.0])
            .expect("upsert a");
        mcp.graph
            .upsert_vector(b, &[0.9, 0.1, 0.0])
            .expect("upsert b");

        // Nearest vector is a ("Ada"), but we filter for name == "Grace".
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), json!("Grace"));

        let result = mcp
            .vector_search(Parameters(VectorSearchArgs {
                vector: vec![1.0, 0.0, 0.0],
                k: 1,
                label: None,
                properties: Some(filters),
            }))
            .expect("vector_search with filter");
        let hits = body(result);
        assert_eq!(hits.as_array().map(|a| a.len()), Some(1));
        assert_eq!(hits[0]["node"].as_u64(), Some(b));
    }

    #[test]
    fn set_thread_count_succeeds() {
        let (mcp, _dir) = fresh();
        let result = mcp
            .set_thread_count(Parameters(SetThreadCountArgs { count: 2 }))
            .expect("set_thread_count");
        let value = body(result);
        assert_eq!(value["success"], true);
    }

    #[test]
    fn configure_vector_index_and_reindex_succeeds() {
        let (mcp, _dir) = fresh();
        mcp.configure_vector_index(Parameters(ConfigureVectorIndexArgs {
            metric: "l2".to_string(),
            quantization: "float16".to_string(),
            reindex: false,
        }))
        .expect("configure_vector_index");

        mcp.configure_vector_index(Parameters(ConfigureVectorIndexArgs {
            metric: "cosine".to_string(),
            quantization: "float32".to_string(),
            reindex: true,
        }))
        .expect("reindex_vector_index");
    }

    #[test]
    fn text_index_lifecycle_succeeds() {
        let (mcp, _dir) = fresh();
        mcp.create_text_index(Parameters(CreateTextIndexArgs {
            label: "Doc".to_string(),
            property: "body".to_string(),
            language: Some("german".to_string()),
        }))
        .expect("create_text_index");

        let list = body(mcp.list_text_indexes().expect("list_text_indexes"));
        assert_eq!(list.as_array().map(|a| a.len()), Some(1));
        assert_eq!(list[0]["label"], "Doc");
        assert_eq!(list[0]["property"], "body");
        assert_eq!(list[0]["language"], "german");

        mcp.drop_text_index(Parameters(DropTextIndexArgs {
            label: "Doc".to_string(),
            property: "body".to_string(),
        }))
        .expect("drop_text_index");

        let list_empty = body(mcp.list_text_indexes().expect("list_text_indexes"));
        assert!(list_empty.as_array().unwrap().is_empty());
    }

    #[test]
    fn retrieve_hybrid_succeeds() {
        let (mcp, _dir) = fresh();
        mcp.graph
            .create_text_index("Person", "name")
            .expect("create text index");
        let id = add_person(&mcp, "Ada");
        mcp.graph
            .upsert_vector(id, &[1.0, 0.0, 0.0])
            .expect("upsert vector");

        let result = mcp
            .retrieve_hybrid(Parameters(HybridRetrieveArgs {
                vector: Some(vec![1.0, 0.0, 0.0]),
                text_query: Some("Ada".to_string()),
                vector_k: Some(1),
                text_k: Some(1),
                text_label: Some("Person".to_string()),
                text_property: Some("name".to_string()),
                vector_label: None,
                hops: Some(1),
                max_distance: None,
                max_nodes: None,
                fusion_strategy: Some("rrf".to_string()),
                rrf_k: Some(60),
                vector_weight: None,
                text_weight: None,
            }))
            .expect("retrieve_hybrid");

        let value = body(result);
        assert_eq!(value["nodes"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(value["nodes"][0].as_u64(), Some(id));
    }
}
