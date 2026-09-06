use crate::ast::{
    AutoIndexStatement, ConstraintKind, CreateConstraintStatement, DropConstraintStatement,
    SchemaTarget,
};

use super::*;

pub(super) fn execute_create_index(
    graph: &Graph,
    stmt: &CreateIndexStatement,
) -> Result<QueryResult, String> {
    match stmt.target {
        // Node property lookups are served by the always-on auto-index, so a
        // node CREATE INDEX provisions the full-text index instead.
        SchemaTarget::Node => graph
            .create_node_text_index(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        SchemaTarget::Relationship => graph
            .create_edge_property_index(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_drop_index(
    graph: &Graph,
    stmt: &DropIndexStatement,
) -> Result<QueryResult, String> {
    match stmt.target {
        SchemaTarget::Node => graph
            .drop_node_text_index(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        SchemaTarget::Relationship => graph
            .drop_edge_property_index(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_set_auto_index(
    graph: &Graph,
    stmt: &AutoIndexStatement,
) -> Result<QueryResult, String> {
    if stmt.label.starts_with("()-[") {
        return Err(
            "AUTO INDEX applies to node labels only; relationships have no property auto-index"
                .to_string(),
        );
    }
    graph
        .set_label_auto_index(&stmt.label, stmt.enabled)
        .map_err(|e| e.to_string())?;
    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_create_constraint(
    graph: &Graph,
    stmt: &CreateConstraintStatement,
) -> Result<QueryResult, String> {
    match (stmt.target, &stmt.kind) {
        (SchemaTarget::Node, ConstraintKind::Unique) => graph
            .create_node_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Node, ConstraintKind::Exists) => graph
            .create_node_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Relationship, ConstraintKind::Unique) => graph
            .create_edge_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Relationship, ConstraintKind::Exists) => graph
            .create_edge_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_drop_constraint(
    graph: &Graph,
    stmt: &DropConstraintStatement,
) -> Result<QueryResult, String> {
    match (stmt.target, &stmt.kind) {
        (SchemaTarget::Node, ConstraintKind::Unique) => graph
            .drop_node_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Node, ConstraintKind::Exists) => graph
            .drop_node_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Relationship, ConstraintKind::Unique) => graph
            .drop_edge_unique_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
        (SchemaTarget::Relationship, ConstraintKind::Exists) => graph
            .drop_edge_required_constraint(&stmt.label, &stmt.property)
            .map_err(|e| e.to_string())?,
    }
    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}
