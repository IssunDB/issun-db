#[cfg(feature = "graphblas")]
pub use issundb_core::MatrixSet;
pub use issundb_core::{
    EdgeId, EdgeRecord, Error, Graph, Hit, LabelId, NodeId, NodeRecord, TypeId,
};

pub use issundb_cypher::{QueryResult, Record};
#[cfg(feature = "graphblas")]
pub use issundb_rag::retrieve_graphblas;
pub use issundb_rag::{RetrieveOptions, Subgraph, retrieve, retrieve_with};

/// Extension trait to execute Cypher queries on the `Graph` handle.
pub trait GraphQueryExt {
    /// Execute a Cypher query without parameters.
    fn query(&self, cypher: &str) -> Result<QueryResult, String>;

    /// Execute a Cypher query with parameter bindings.
    fn query_with_params(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<QueryResult, String>;
}

impl GraphQueryExt for Graph {
    fn query(&self, cypher: &str) -> Result<QueryResult, String> {
        let params = std::collections::HashMap::new();
        issundb_cypher::execute(self, cypher, &params)
    }

    fn query_with_params(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<QueryResult, String> {
        issundb_cypher::execute(self, cypher, params)
    }
}
