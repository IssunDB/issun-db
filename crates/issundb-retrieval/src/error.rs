use thiserror::Error;

/// Structured errors representing all hybrid retrieval faults.
#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("underlying storage error: {0}")]
    Core(#[from] issundb_core::Error),

    #[error("vector index error: {0}")]
    Vector(#[from] issundb_vector::VectorError),

    #[error("text search error: {0}")]
    Text(#[from] issundb_text::TextError),
}
