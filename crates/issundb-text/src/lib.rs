use issundb_core::{Graph, NodeId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TextError {
    #[error("text search is not implemented yet")]
    NotImplemented,
}

/// A single ranked full-text search result.
pub struct TextHit {
    pub node: NodeId,
    pub score: f32,
}

/// Options for text search.
pub struct TextSearchOptions {
    pub limit: usize,
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self { limit: 10 }
    }
}

/// Full-text search operations for `Graph`.
pub trait TextGraphExt {
    fn text_search(&self, query: &str, opts: &TextSearchOptions)
    -> Result<Vec<TextHit>, TextError>;
}

impl TextGraphExt for Graph {
    fn text_search(
        &self,
        _query: &str,
        _opts: &TextSearchOptions,
    ) -> Result<Vec<TextHit>, TextError> {
        Err(TextError::NotImplemented)
    }
}
