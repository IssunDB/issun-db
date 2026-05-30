use std::collections::HashMap;

/// Representation of a parsed Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Create(CreateStatement),
    Set(SetStatement),
    Delete(DeleteStatement),
    /// A MERGE statement: create the pattern if absent, otherwise update it.
    Merge(MergeStatement),
    /// CREATE INDEX FOR (n:Label) ON (n.property)
    CreateIndex(CreateIndexStatement),
    /// DROP INDEX FOR (n:Label) ON (n.property)
    DropIndex(DropIndexStatement),
    /// REMOVE n.property or REMOVE n:Label
    Remove(RemoveStatement),
    /// `MATCH ... RETURN ... UNION [ALL] MATCH ... RETURN ...`
    Union(UnionStatement),
    /// CREATE ... RETURN ...
    CreateAndReturn(CreateAndReturnStatement),
    /// MATCH ... SET ... RETURN ...
    SetAndReturn(SetAndReturnStatement),
    /// FOREACH (var IN list | stmt ...)
    Foreach(ForeachStatement),
    /// CREATE CONSTRAINT ON (n:Label) ASSERT n.property IS UNIQUE | EXISTS(...)
    CreateConstraint(CreateConstraintStatement),
    /// DROP CONSTRAINT ON (n:Label) ASSERT n.property IS UNIQUE | EXISTS(...)
    DropConstraint(DropConstraintStatement),
    /// MERGE ... [ON CREATE SET ...] [ON MATCH SET ...] RETURN ...
    MergeAndReturn(MergeAndReturnStatement),
    /// MATCH ... DELETE ... RETURN ...
    DeleteAndReturn(DeleteAndReturnStatement),
    /// MATCH ... REMOVE ... RETURN ...
    RemoveAndReturn(RemoveAndReturnStatement),
    /// A sequence of independent statements. Each statement is executed in order;
    /// the result of the last statement is returned. This represents queries like
    /// `CREATE (a) CREATE (b)` or setup scripts with multiple write clauses.
    Pipeline(Vec<Statement>),
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
    OptionalMatch {
        match_clauses: Vec<MatchClause>,
        where_clause: Option<WhereClause>,
    },
    With {
        items: Vec<ReturnItem>,
        where_clause: Option<WhereClause>,
        /// Optional ORDER BY on the WITH clause output.
        order_by: Option<OrderBy>,
        /// Optional SKIP on the WITH clause output.
        skip: Option<Expr>,
        /// Optional LIMIT on the WITH clause output.
        limit: Option<Expr>,
        /// When true, duplicate rows are removed from the WITH output.
        distinct: bool,
    },
    Unwind {
        expr: Expr,
        variable: String,
    },
    /// A CREATE clause inside a pipeline query, e.g. `UNWIND list AS x CREATE (n {v: x}) RETURN n`.
    Create {
        patterns: Vec<Pattern>,
    },
    /// A MERGE clause inside a pipeline query.
    Merge {
        merges: Vec<MergeStatement>,
    },
    /// A SET clause inside a pipeline query.
    Set {
        items: Vec<SetItem>,
    },
    /// A DELETE clause inside a pipeline query. Each target is an expression that
    /// must evaluate to a node, relationship, or path (or a list of them).
    Delete {
        targets: Vec<Expr>,
        detach: bool,
    },
    /// A REMOVE clause inside a pipeline query.
    Remove {
        items: Vec<RemoveItem>,
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
    /// Optional path variable: `p = (...)-[...]-(...)`.
    pub path_variable: Option<String>,
}

/// A pattern matching a node with variable, labels, and inline properties.
///
/// `labels` holds every `:Label` segment in the pattern, in source order. An empty
/// vector means the pattern places no label constraint (MATCH) or creates an
/// unlabeled node (CREATE).
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Option<HashMap<String, Expr>>,
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
    /// True when the relationship is directed inbound: `<-[...]-`.
    pub is_incoming: bool,
    /// True when the relationship has no direction: `-[...]- `.
    /// When `is_undirected` is true, `is_incoming` is ignored.
    pub is_undirected: bool,
    pub range: Option<RelRange>,
    pub properties: Option<HashMap<String, Expr>>,
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
    /// A full boolean expression such as IS NULL, OR, AND, NOT, and quantifiers.
    Expr(Expr),
}

/// An aggregation function applied to an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFn {
    /// `count(*)` or `count(expr)`; with optional `DISTINCT` for the expression form.
    Count {
        distinct: bool,
    },
    Sum {
        distinct: bool,
    },
    Avg {
        distinct: bool,
    },
    Min {
        distinct: bool,
    },
    Max {
        distinct: bool,
    },
    Collect {
        distinct: bool,
    },
    /// Sample standard deviation.
    StDev {
        distinct: bool,
    },
    /// Population standard deviation.
    StDevP {
        distinct: bool,
    },
    /// Discrete percentile (nearest rank). The percentile is an expression so it
    /// may be a literal or a parameter, evaluated once at aggregation time.
    PercentileDisc {
        percentile: Box<Expr>,
    },
    /// Continuous percentile (linear interpolation). The percentile is an
    /// expression, evaluated once at aggregation time.
    PercentileCont {
        percentile: Box<Expr>,
    },
}

/// The kind of quantifier expression.
#[derive(Debug, Clone, PartialEq)]
pub enum QuantifierKind {
    All,
    Any,
    None,
    Single,
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
    /// `all(x IN list WHERE predicate)`, `any(...)`, `none(...)`, `single(...)`.
    Quantifier {
        kind: QuantifierKind,
        variable: String,
        list: Box<Expr>,
        predicate: Box<Expr>,
    },
    /// Built-in function call: `range(start, end)`, `range(start, end, step)`, `size(expr)`, etc.
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    /// Binary arithmetic or logical operation.
    BinaryOp {
        op: BinaryOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `expr IS NULL`
    IsNull(Box<Expr>),
    /// `expr IS NOT NULL`
    IsNotNull(Box<Expr>),
    /// Unary negation: `NOT expr`.
    Not(Box<Expr>),
    /// `CASE [subject] WHEN ... THEN ... [ELSE ...] END`
    Case {
        /// None for searched CASE; Some(expr) for simple CASE.
        subject: Option<Box<Expr>>,
        arms: Vec<CaseArm>,
        else_expr: Option<Box<Expr>>,
    },
    /// `expr[index]` — list element or map key lookup.
    Subscript {
        expr: Box<Expr>,
        index: Box<Expr>,
    },
    /// `expr[start..end]` — list slice (start and end are optional).
    Slice {
        expr: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
    /// `[variable IN list WHERE predicate | transform]` — list comprehension.
    ListComprehension {
        variable: String,
        list: Box<Expr>,
        predicate: Option<Box<Expr>>,
        transform: Option<Box<Expr>>,
    },
    /// `reduce(accumulator = initial, variable IN list | expression)`
    Reduce {
        accumulator: String,
        initial: Box<Expr>,
        variable: String,
        list: Box<Expr>,
        expression: Box<Expr>,
    },
    /// `variable:Label` — boolean check whether the node has the given label.
    HasLabel {
        variable: String,
        label: String,
    },
}

/// Binary operator for use in expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum BinaryOperator {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    /// Exclusive disjunction (`XOR`).
    Xor,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// Exponentiation (`^`).
    Pow,
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
    /// When true, duplicate rows are removed from the result.
    pub distinct: bool,
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
    /// All comma-separated patterns in the CREATE clause.
    pub patterns: Vec<Pattern>,
}

/// A CREATE ... RETURN ... statement that creates nodes/edges and projects results.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateAndReturnStatement {
    pub patterns: Vec<Pattern>,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
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
pub enum SetItem {
    /// `SET n.prop = expr`
    Property {
        variable: String,
        property: String,
        expr: Expr,
    },
    /// `SET n:Label` or `SET n:Label1:Label2`
    Labels {
        variable: String,
        labels: Vec<String>,
    },
}

impl SetItem {
    /// The variable this item updates.
    pub fn variable(&self) -> &str {
        match self {
            SetItem::Property { variable, .. } | SetItem::Labels { variable, .. } => variable,
        }
    }
}

/// A DELETE statement for removing nodes or edges.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    /// Expressions that evaluate to nodes, relationships, or paths to delete.
    pub targets: Vec<Expr>,
    /// When true, incident edges are deleted before deleting the node itself.
    pub detach: bool,
}

/// A MERGE statement with optional ON CREATE SET and ON MATCH SET sub-clauses.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStatement {
    pub pattern: Pattern,
    pub on_create_set: Vec<SetItem>,
    pub on_match_set: Vec<SetItem>,
}

/// A CREATE INDEX statement targeting a single label and property.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStatement {
    pub label: String,
    pub property: String,
}

/// A DROP INDEX statement targeting a single label and property.
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStatement {
    pub label: String,
    pub property: String,
}

/// Items that can appear in a REMOVE clause.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    Property { variable: String, property: String },
    Label { variable: String, label: String },
}

/// A REMOVE statement for deleting properties or labels from graph elements.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub items: Vec<RemoveItem>,
}

/// A single arm of a CASE expression.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseArm {
    pub when: Expr,
    pub then: Expr,
}

/// A SET statement combined with RETURN.
#[derive(Debug, Clone, PartialEq)]
pub struct SetAndReturnStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub set_items: Vec<SetItem>,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// Two queries composed with UNION or UNION ALL.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionStatement {
    pub left: Box<Statement>,
    pub right: Box<Statement>,
    /// When true, all rows from both sides are kept; when false, duplicates are removed.
    pub all: bool,
}

/// A FOREACH statement that iterates over a list and executes write operations.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachStatement {
    pub variable: String,
    pub list: Expr,
    pub body: Vec<Statement>,
}

/// The kind of constraint on a node property.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    Unique,
    Exists,
}

/// A CREATE CONSTRAINT statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateConstraintStatement {
    pub label: String,
    pub property: String,
    pub kind: ConstraintKind,
}

/// A DROP CONSTRAINT statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DropConstraintStatement {
    pub label: String,
    pub property: String,
    pub kind: ConstraintKind,
}

/// A REMOVE statement followed by a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveAndReturnStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub items: Vec<RemoveItem>,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// A DELETE statement followed by a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteAndReturnStatement {
    pub match_clauses: Vec<MatchClause>,
    pub where_clause: Option<WhereClause>,
    /// Expressions that evaluate to nodes, relationships, or paths to delete.
    pub targets: Vec<Expr>,
    pub detach: bool,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// One or more MERGE blocks followed by a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeAndReturnStatement {
    pub merges: Vec<MergeStatement>,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderBy>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}
