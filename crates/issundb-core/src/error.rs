use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("storage: {0}")]
    Storage(#[from] heed::Error),

    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    #[error("node {0} not found")]
    NodeNotFound(u64),

    #[error("edge {0} not found")]
    EdgeNotFound(u64),

    #[error("corrupt storage: {0}")]
    Corrupt(&'static str),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("vector index: {0}")]
    Vector(String),

    #[error("graphblas: {0}")]
    GraphBLAS(String),
}
