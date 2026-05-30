use thiserror::Error;

/// Structured query engine errors representing all parsing, optimization, planning,
/// and runtime execution faults.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CypherError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("planner error: {0}")]
    Plan(String),

    #[error("type error: {0}")]
    TypeMismatch(String),

    #[error("variable not bound: {0}")]
    VariableNotBound(String),

    #[error("math error: {0}")]
    Math(String),

    #[error("execution error: {0}")]
    Execution(String),

    #[error("storage dependency error: {0}")]
    Storage(String),
}
