#[cfg(feature = "graphblas")]
pub use issundb_core::MatrixSet;
pub use issundb_core::{
    EdgeId, EdgeRecord, Error, Graph, Hit, LabelId, NodeId, NodeRecord, TypeId,
};

#[cfg(feature = "graphblas")]
pub use issundb_rag::retrieve_graphblas;
pub use issundb_rag::{RetrieveOptions, Subgraph, retrieve, retrieve_with};
