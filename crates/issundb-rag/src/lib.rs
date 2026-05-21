pub mod retrieve;

#[cfg(feature = "graphblas")]
pub use retrieve::retrieve_graphblas;
pub use retrieve::{RetrieveOptions, Subgraph, retrieve, retrieve_with};
