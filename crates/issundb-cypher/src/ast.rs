use std::collections::HashMap;

/// Representation of a parsed Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Create(CreateStatement),
    Set(SetStatement),
    Delete(DeleteStatement),
}

/// A read-only Cypher query containing MATCH, WHERE, and RETURN clauses.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub return_clause: ReturnClause,
}

/// A MATCH clause containing a node and relationship pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub pattern: Pattern,
}

/// A path pattern matching nodes and connecting relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub node: NodePattern,
    pub rels: Vec<(RelationshipPattern, NodePattern)>,
}

/// A pattern matching a node with variable, label, and inline properties.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub label: Option<String>,
    pub properties: Option<HashMap<String, Literal>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelRange {
    pub min: Option<u32>,
    pub max: Option<u32>,
}

/// A pattern matching a relationship type and direction.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    pub variable: Option<String>,
    pub rel_type: Option<String>,
    pub is_incoming: bool,
    pub range: Option<RelRange>,
}

/// A conditional WHERE predicate comparing two expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum WhereClause {
    Eq(Expr, Expr),
    Ne(Expr, Expr),
    Lt(Expr, Expr),
    Gt(Expr, Expr),
    Le(Expr, Expr),
    Ge(Expr, Expr),
}

/// A Cypher expression (property reference, literal, or parameter).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Prop(String, String), // variable.property
    Literal(Literal),
    Param(String), // $parameter
}

/// A literal value representation.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

/// A RETURN clause containing projected variables or properties.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub items: Vec<ReturnItem>,
}

/// An individual item in the RETURN clause projection.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// A CREATE statement pattern for inserting new nodes and edges.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStatement {
    pub pattern: Pattern,
}

/// A SET statement for updating node or edge properties.
#[derive(Debug, Clone, PartialEq)]
pub struct SetStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub set_items: Vec<SetItem>,
}

/// An individual update in the SET statement.
#[derive(Debug, Clone, PartialEq)]
pub struct SetItem {
    pub variable: String,
    pub property: String,
    pub expr: Expr,
}

/// A DELETE statement for removing nodes or edges.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub variables: Vec<String>,
}
