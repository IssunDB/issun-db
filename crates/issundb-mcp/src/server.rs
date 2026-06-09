use std::{collections::HashMap, sync::Arc};

use issundb::{
    FusionStrategy, Graph, GraphQueryExt, HybridRetrieveOptions, TextGraphExt, TextSearchOptions,
    VectorGraphExt, VectorSearchOptions, retrieve_hybrid,
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
///
/// The tool surface is deliberately curated for an LLM agent: reads, queries,
/// and retrieval. There are no typed mutation, index-administration, or
/// host-operations tools. Graph mutations are expressed as Cypher through
/// `cypher_query` (`CREATE`, `SET`, `DELETE`); index provisioning, vector
/// loading, backups, and tuning are operator concerns driven through the CLI or
/// the Python and REST surfaces, not through an agent.
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
pub struct NodeIdArgs {
    /// Node identifier.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeIdArgs {
    /// Edge identifier.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CypherArgs {
    /// Cypher query text. Supports reads (`MATCH`) and mutations (`CREATE`,
    /// `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `MERGE`).
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

    #[tool(description = "Fetch a node by id; returning its labels and properties.")]
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

    #[tool(description = "Fetch an edge by id; returning its endpoints, type, and properties.")]
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

    #[tool(
        description = "Execute a Cypher query with optional parameters; returns columns and records. Use CREATE, SET, REMOVE, DELETE, and MERGE to mutate the graph."
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
        description = "Nearest-neighbor vector search; returns the k closest nodes by distance (with optional label and property filtering)."
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
        description = "Execute a hybrid retrieval query that combines vector/semantic search, full-text keyword search, and relationship expansion."
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
                "IssunDB graph database. Tools: get_node, get_edge, cypher_query, explain, \
                 text_search, vector_search, and retrieve_hybrid. Mutate the graph by sending \
                 CREATE, SET, REMOVE, DELETE, or MERGE through cypher_query; there are no \
                 separate write, index-administration, or backup tools."
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
// transport layers are not involved. Setup that the curated tool surface does
// not expose (creating nodes, indexes, or vectors) is done through the shared
// `Graph` handle. Each test opens its own graph and shares no state.
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

    /// Seed a Person node directly through the graph (the MCP surface has no
    /// node-creation tool; agents would use `cypher_query` with `CREATE`).
    fn seed_person(mcp: &IssunMcp, name: &str) -> u64 {
        mcp.graph
            .add_node("Person", &json!({ "name": name }))
            .expect("seed person")
    }

    #[test]
    fn get_node_round_trips_label_and_props() {
        let (mcp, _dir) = fresh();
        let id = seed_person(&mcp, "Ada");
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
    fn get_node_missing_is_invalid_params() {
        let (mcp, _dir) = fresh();
        let err = mcp
            .get_node(Parameters(NodeIdArgs { id: 999 }))
            .expect_err("missing node");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn get_edge_round_trips_endpoints_and_type() {
        let (mcp, _dir) = fresh();
        let a = seed_person(&mcp, "Ada");
        let b = seed_person(&mcp, "Grace");
        let edge_id = mcp
            .graph
            .add_edge(a, b, "KNOWS", &json!({ "since": 2020 }))
            .expect("add edge");
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
    fn cypher_query_returns_columns_and_records() {
        let (mcp, _dir) = fresh();
        seed_person(&mcp, "Ada");
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
        seed_person(&mcp, "Ada");
        seed_person(&mcp, "Grace");
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
    fn cypher_query_mutates_via_create_and_delete() {
        // The aggressive MCP surface has no typed write tools; mutation must work
        // through Cypher. Create a node, confirm it, then delete it.
        let (mcp, _dir) = fresh();
        mcp.cypher_query(Parameters(CypherArgs {
            query: "CREATE (n:Person {name: 'Ada'})".to_string(),
            params: HashMap::new(),
        }))
        .expect("create");
        let after_create = body(
            mcp.cypher_query(Parameters(CypherArgs {
                query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
                params: HashMap::new(),
            }))
            .expect("match"),
        );
        assert_eq!(after_create["records"], json!([["Ada"]]));

        mcp.cypher_query(Parameters(CypherArgs {
            query: "MATCH (n:Person {name: 'Ada'}) DELETE n".to_string(),
            params: HashMap::new(),
        }))
        .expect("delete");
        let after_delete = body(
            mcp.cypher_query(Parameters(CypherArgs {
                query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
                params: HashMap::new(),
            }))
            .expect("match"),
        );
        assert_eq!(after_delete["records"], json!([]));
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
        let a = seed_person(&mcp, "Ada");
        let b = seed_person(&mcp, "Grace");
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
        let a = seed_person(&mcp, "Ada");
        let b = seed_person(&mcp, "Grace");
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
    fn retrieve_hybrid_succeeds() {
        let (mcp, _dir) = fresh();
        mcp.graph
            .create_text_index("Person", "name")
            .expect("create text index");
        let id = seed_person(&mcp, "Ada");
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
