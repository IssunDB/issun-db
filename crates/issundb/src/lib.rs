//! IssunDB is an embedded graph database with vector search, full-text search,
//! and hybrid retrieval.
//!
//! This is the main crate of IssunDB, and exposes the `Graph` type, Cypher query execution,
//! vector search, full-text search, and hybrid retrieval APIs. Application code, bindings,
//! and tools depend on this crate only; the internal crates (`issundb-core`, `issundb-vector`,
//! `issundb-text`, `issundb-retrieval`, and `issundb-cypher`) are not part of
//! the stable API.
//!
//! # Entry Points
//!
//! - [`Graph`] is the central handle. Open it with [`Graph::open`], then use its
//!   methods for node and edge CRUD, adjacency, and graph algorithms.
//! - [`GraphQueryExt`] adds Cypher execution ([`query`](GraphQueryExt::query),
//!   [`query_with_params`](GraphQueryExt::query_with_params), and
//!   [`explain`](GraphQueryExt::explain)) to [`Graph`].
//! - [`VectorGraphExt`] adds vector indexing and search to [`Graph`].
//! - [`TextGraphExt`] and [`TextIndexExt`] add full-text indexing and search.
//! - [`retrieve`], [`retrieve_with`], and [`retrieve_hybrid`] run hybrid
//!   retrieval over vector hits, text hits, and graph expansion.
//!
//! # Working with Query Results
//!
//! Query parameters and [`Record`] values use `serde_json::Value`. The
//! `serde_json` crate is re-exported as [`issundb::serde_json`](serde_json) so
//! callers do not need to track a separate, version-compatible dependency.

pub use issundb_core::{
    DegreeDirection, DirectedNeighborEntry, EdgeId, EdgeRecord, Error, Graph, LabelId, Language,
    NeighborEntry, NodeId, NodeRecord, PropValue, ReadTxn, TriangleCountSpec, TypeId, WeightedPath,
    WriteTxn,
};
pub use issundb_cypher::{
    CypherError, CypherType, Procedure, ProcedureRegistry, QueryResult, Record,
};
pub use issundb_retrieval::{
    FusionStrategy, HybridRetrieveOptions, RetrievalError, RetrieveOptions, Subgraph, retrieve,
    retrieve_hybrid, retrieve_with,
};
pub use issundb_text::{
    Bm25Scorer, BooleanMode, Scorer, TextError, TextGraphExt, TextHit, TextIndexExt,
    TextSearchOptions, TfIdfScorer,
};
pub use issundb_vector::{
    Hit, VectorError, VectorGraphExt, VectorIndexOptions, VectorMetric, VectorQuantization,
    VectorSearchOptions,
};

/// Re-export of the `serde_json` crate. Query parameters and [`Record`] values
/// are `serde_json::Value`, so callers construct and inspect them through this
/// re-export without depending on a separate `serde_json` version.
pub use serde_json;

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

    /// Execute a Cypher query with parameter bindings, resolving `CALL` clauses
    /// against the supplied procedure registry.
    fn query_with_procedures(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        registry: &ProcedureRegistry,
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

    fn query_with_procedures(
        &self,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        registry: &ProcedureRegistry,
    ) -> Result<QueryResult, CypherError> {
        issundb_cypher::execute_with_procedures(self, cypher, params, registry)
    }

    fn explain(&self, cypher: &str) -> Result<String, CypherError> {
        issundb_cypher::explain(self, cypher)
    }
}
