pub use issundb_core::matrices::MatrixSet;
pub use issundb_core::{
    AdjEntry, DegreeDirection, EdgeId, EdgeRecord, Error, Graph, LabelId, Language, NodeId,
    NodeRecord, PropKeyId, ReadTxn, TypeId, WriteTxn,
};
pub use issundb_cypher::{QueryResult, Record};
pub use issundb_retrieval::{RetrieveOptions, Subgraph, retrieve, retrieve_with};
pub use issundb_text::{TextError, TextGraphExt, TextHit, TextSearchOptions};
pub use issundb_vector::{Hit, VectorGraphExt, VectorIndex};

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
