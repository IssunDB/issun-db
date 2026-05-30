pub mod error;
pub mod retrieve;

pub use error::RetrievalError;
pub use retrieve::{
    FusionStrategy, HybridRetrieveOptions, RetrieveOptions, Subgraph, retrieve, retrieve_hybrid,
    retrieve_with,
};
