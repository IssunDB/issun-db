use std::collections::HashMap;

/// Representation of a parsed Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Create(CreateStatement),
    Set(SetStatement),
    Delete(DeleteStatement),
}

/// A read-only Cypher query containing MATCH, WHERE, RETURN, ORDER BY, SKIP, and LIMIT clauses.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub return_clause: ReturnClause,
    pub parts: Vec<QueryPart>,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// An ORDER BY clause containing one or more sort keys.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub items: Vec<SortItem>,
}

/// A single sort key with direction.
#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expr: Expr,
    pub ascending: bool,
}

/// A clause/part in a sequential Cypher query sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryPart {
    Match {
        match_clauses: Vec<MatchClause>,
        where_clause: Option<WhereClause>,
    },
    With {
        items: Vec<ReturnItem>,
        where_clause: Option<WhereClause>,
    },
    Unwind {
        expr: Expr,
        variable: String,
    },
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
    pub properties: Option<HashMap<String, Literal>>,
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

/// An aggregation function applied to an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFn {
    /// `count(*)` or `count(expr)` — with optional `DISTINCT` for the expression form.
    Count {
        distinct: bool,
    },
    Sum,
    Avg,
    Min,
    Max,
    Collect,
}

/// A Cypher expression (property reference, literal, parameter, or aggregation).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Prop(String, String), // variable.property
    Literal(Literal),
    Param(String), // $parameter
    /// `count(*)` with no inner expression.
    CountStar,
    /// Aggregation function applied to an inner expression.
    Agg(AggFn, Box<Expr>),
}

/// A literal value representation.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    List(Vec<Literal>),
}

impl std::fmt::Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Literal::Str(s) => write!(f, "'{}'", s),
            Literal::Int(i) => write!(f, "{}", i),
            Literal::Float(v) => write!(f, "{}", v),
            Literal::Bool(b) => write!(f, "{}", b),
            Literal::Null => write!(f, "null"),
            Literal::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
        }
    }
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
