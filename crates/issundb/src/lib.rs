pub use issundb_core::metrics::{MetricsCollector, NoOpMetrics};
pub use issundb_core::{
    DegreeDirection, DirectedNeighborEntry, EdgeId, EdgeRecord, Error, Graph, LabelId, Language,
    NeighborEntry, NodeId, NodeRecord, PropValue, ReadTxn, TypeId, WeightedPath, WriteTxn,
};
pub use issundb_cypher::{CypherError, QueryResult, Record};
pub use issundb_retrieval::{
    FusionStrategy, HybridRetrieveOptions, RetrievalError, RetrieveOptions, Subgraph, retrieve,
    retrieve_hybrid, retrieve_with,
};
pub use issundb_text::{
    Bm25Scorer, BooleanMode, Scorer, TextError, TextGraphExt, TextHit, TextIndexExt,
    TextSearchOptions, TfIdfScorer,
};
pub use issundb_vector::{
    Hit, VectorError, VectorGraphExt, VectorIndex, VectorIndexOptions, VectorMetric,
    VectorQuantization, VectorSearchOptions,
};

/// Extension trait to execute Cypher queries on the `Graph` handle.
pub trait GraphQueryExt {
    /// Execute a Cypher query without parameters.
    fn query(&self, cypher: &str) -> Result<QueryResult, CypherError>;

    /// Execute a Cypher query with parameter bindings.
    fn query_with_params(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<QueryResult, CypherError>;

    /// Parse `cypher`, compile and optimize the physical plan, and return it as
    /// an indented human-readable tree. Useful for debugging query performance.
    fn explain(&self, cypher: &str) -> Result<String, CypherError>;
}

impl GraphQueryExt for Graph {
    fn query(&self, cypher: &str) -> Result<QueryResult, CypherError> {
        let params = std::collections::HashMap::new();
        issundb_cypher::execute(self, cypher, &params)
    }

    fn query_with_params(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<QueryResult, CypherError> {
        issundb_cypher::execute(self, cypher, params)
    }

    fn explain(&self, cypher: &str) -> Result<String, CypherError> {
        issundb_cypher::explain(self, cypher)
    }
}
