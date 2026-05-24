use crate::ast::Expr;
use crate::plan::logical::{FilterExpr, LogicalOperator};

/// A physical representation of a query execution operator.
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalOperator {
    /// A single empty row to bootstrap queries.
    SingleRow,
    /// Unwind a list expression and bind each element to a variable.
    Unwind {
        input: Box<PhysicalOperator>,
        expr: Expr,
        variable: String,
    },
    /// Scan nodes by label: binds `variable` to nodes matching `label`.
    LabelScan {
        variable: String,
        label: Option<String>,
    },
    /// Expand relationships: starts from `src_var`, traverses relationship `rel_type`
    /// in direction `is_incoming` up to range bounds, and binds relationship to `rel_var`
    /// and target to `dst_var`.
    Expand {
        input: Box<PhysicalOperator>,
        src_var: String,
        rel_var: String,
        dst_var: String,
        rel_type: Option<String>,
        is_incoming: bool,
        min_hops: usize,
        max_hops: usize,
    },
    /// Filter records based on expressions/WHERE predicates.
    Filter {
        input: Box<PhysicalOperator>,
        expression: FilterExpr,
    },
    /// Project RETURN expressions to form the final table.
    Project {
        input: Box<PhysicalOperator>,
        items: Vec<(Expr, Option<String>)>, // (expression, alias)
        is_barrier: bool,
    },
    /// Join two independent physical sub-plans (cross product or hash join).
    HashJoin {
        left: Box<PhysicalOperator>,
        right: Box<PhysicalOperator>,
    },
}

/// A physical planner that compiles logical query plans into physical, executable plans.
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Compile a `LogicalOperator` plan into a `PhysicalOperator` plan.
    pub fn plan(logical: &LogicalOperator) -> PhysicalOperator {
        match logical {
            LogicalOperator::SingleRow => PhysicalOperator::SingleRow,
            LogicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::plan(input)),
                expr: expr.clone(),
                variable: variable.clone(),
            },
            LogicalOperator::LabelScan { variable, label } => PhysicalOperator::LabelScan {
                variable: variable.clone(),
                label: label.clone(),
            },
            LogicalOperator::Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                min_hops,
                max_hops,
            } => PhysicalOperator::Expand {
                input: Box::new(Self::plan(input)),
                src_var: src_var.clone(),
                rel_var: rel_var.clone(),
                dst_var: dst_var.clone(),
                rel_type: rel_type.clone(),
                is_incoming: *is_incoming,
                min_hops: *min_hops,
                max_hops: *max_hops,
            },
            LogicalOperator::Filter { input, expression } => PhysicalOperator::Filter {
                input: Box::new(Self::plan(input)),
                expression: expression.clone(),
            },
            LogicalOperator::Project {
                input,
                items,
                is_barrier,
            } => PhysicalOperator::Project {
                input: Box::new(Self::plan(input)),
                items: items.clone(),
                is_barrier: *is_barrier,
            },
            LogicalOperator::Join { left, right } => PhysicalOperator::HashJoin {
                left: Box::new(Self::plan(left)),
                right: Box::new(Self::plan(right)),
            },
        }
    }
}
