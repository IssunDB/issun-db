use crate::ast::{ConstraintKind, CreateConstraintStatement, DropConstraintStatement};

use super::*;

pub(super) fn execute_create_index(
    graph: &Graph,
    stmt: &CreateIndexStatement,
) -> Result<QueryResult, String> {
    graph
        .create_node_text_index(&stmt.label, &stmt.property)
        .map_err(|e| e.to_string())?;
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_drop_index(
    graph: &Graph,
    stmt: &DropIndexStatement,
) -> Result<QueryResult, String> {
    graph
        .drop_node_text_index(&stmt.label, &stmt.property)
        .map_err(|e| e.to_string())?;
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_create_constraint(
    graph: &Graph,
    stmt: &CreateConstraintStatement,
) -> Result<QueryResult, String> {
    match stmt.kind {
        ConstraintKind::Unique => graph
            .create_node_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        ConstraintKind::Exists => graph
            .create_node_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_drop_constraint(
    graph: &Graph,
    stmt: &DropConstraintStatement,
) -> Result<QueryResult, String> {
    match stmt.kind {
        ConstraintKind::Unique => graph
            .drop_node_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        ConstraintKind::Exists => graph
            .drop_node_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}
