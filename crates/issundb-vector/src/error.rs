use thiserror::Error;

/// Structured errors representing all vector search and indexing faults.
#[derive(Debug, Error)]
pub enum VectorError {
    #[error("dimension mismatch: index expects {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error("vector index not initialized for label {0}")]
    IndexNotInitialized(String),

    #[error("usearch library fault: {0}")]
    IndexFault(String),

    #[error("underlying storage error: {0}")]
    Storage(#[from] issundb_core::Error),
}
