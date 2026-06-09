use thiserror::Error;

/// Structured errors representing all vector search and indexing faults.
#[derive(Debug, Error)]
pub enum VectorError {
    #[error("dimension mismatch: index expects {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error(
        "vector index already configured ({existing}); cannot change to {requested} while vectors exist, use reindex_vector_index to rebuild under the new configuration"
    )]
    AlreadyConfigured { existing: String, requested: String },

    #[error("usearch library fault: {0}")]
    IndexFault(String),

    #[error("invalid vector configuration: {0}")]
    InvalidConfig(String),

    #[error("underlying storage error: {0}")]
    Storage(#[from] issundb_core::Error),
}
