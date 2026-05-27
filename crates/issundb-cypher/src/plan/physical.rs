use crate::ast::{AggFn, Expr, Literal, SortItem};
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
    /// Scan nodes using a property index: binds `variable` to nodes matching `label` and `property` value.
    NodeIndexScan {
        variable: String,
        label: String,
        property: String,
        value: Expr,
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
        /// When true, traverse both directions and deduplicate results.
        is_undirected: bool,
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
    /// Aggregate rows, grouping by non-aggregate keys and computing aggregate functions.
    Aggregate {
        input: Box<PhysicalOperator>,
        group_by: Vec<(Expr, Option<String>)>,
        aggregations: Vec<(AggFn, Expr, String)>,
    },
    /// Sort rows by one or more sort keys.
    Sort {
        input: Box<PhysicalOperator>,
        items: Vec<SortItem>,
    },
    /// Skip and limit the row stream.
    Limit {
        input: Box<PhysicalOperator>,
        skip: usize,
        count: usize,
    },
    /// Optional match: evaluate inner plan; if empty, emit one null-filled row.
    OptionalMatch {
        input: Box<PhysicalOperator>,
        null_vars: Vec<String>,
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
                is_undirected,
                min_hops,
                max_hops,
            } => PhysicalOperator::Expand {
                input: Box::new(Self::plan(input)),
                src_var: src_var.clone(),
                rel_var: rel_var.clone(),
                dst_var: dst_var.clone(),
                rel_type: rel_type.clone(),
                is_incoming: *is_incoming,
                is_undirected: *is_undirected,
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
            LogicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::plan(input)),
                group_by: group_by.clone(),
                aggregations: aggregations.clone(),
            },
            LogicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::plan(input)),
                items: items.clone(),
            },
            LogicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::plan(input)),
                skip: *skip,
                count: *count,
            },
            LogicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(Self::plan(input)),
                    null_vars: null_vars.clone(),
                }
            }
        }
    }
}

/// Render an optimized physical plan as an indented, human-readable tree.
///
/// Each line starts with two spaces per depth level, followed by a one-line
/// description of the operator. Child operators are rendered at `depth + 1`.
pub fn format_physical_plan(op: &PhysicalOperator, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    let mut buf = String::new();

    match op {
        PhysicalOperator::SingleRow => {
            buf.push_str(&format!("{}SingleRow\n", pad));
        }
        PhysicalOperator::LabelScan { variable, label } => {
            let lbl = label.as_deref().unwrap_or("*");
            buf.push_str(&format!("{}LabelScan {}:{}\n", pad, variable, lbl));
        }
        PhysicalOperator::NodeIndexScan {
            variable,
            label,
            property,
            value,
        } => {
            buf.push_str(&format!(
                "{}NodeIndexScan {}:{}.{} = {}\n",
                pad,
                variable,
                label,
                property,
                fmt_expr(value)
            ));
        }
        PhysicalOperator::Expand {
            input,
            src_var,
            rel_var,
            dst_var,
            rel_type,
            is_incoming,
            min_hops,
            max_hops,
            ..
        } => {
            let rtype = rel_type.as_deref().unwrap_or("*");
            let range = if *min_hops == 1 && *max_hops == 1 {
                String::new()
            } else if *max_hops == usize::MAX {
                format!("*{}..", min_hops)
            } else if min_hops == max_hops {
                format!("*{}", min_hops)
            } else {
                format!("*{}..{}", min_hops, max_hops)
            };
            let (left, right) = if *is_incoming {
                ("<-", "-")
            } else {
                ("-", "->")
            };
            buf.push_str(&format!(
                "{}Expand {}{}[{}:{}{}]{}{}\n",
                pad, src_var, left, rel_var, rtype, range, right, dst_var
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Filter { input, expression } => {
            buf.push_str(&format!("{}Filter {}\n", pad, fmt_filter(expression)));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
        } => {
            let cols: Vec<String> = items
                .iter()
                .map(|(e, alias)| {
                    if let Some(a) = alias {
                        format!("{} AS {}", fmt_expr(e), a)
                    } else {
                        fmt_expr(e)
                    }
                })
                .collect();
            let barrier = if *is_barrier { " [barrier]" } else { "" };
            buf.push_str(&format!(
                "{}Project [{}]{}\n",
                pad,
                cols.join(", "),
                barrier
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::HashJoin { left, right } => {
            buf.push_str(&format!("{}HashJoin\n", pad));
            buf.push_str(&format_physical_plan(left, depth + 1));
            buf.push_str(&format_physical_plan(right, depth + 1));
        }
        PhysicalOperator::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            let groups: Vec<String> = group_by.iter().map(|(e, _)| fmt_expr(e)).collect();
            let aggs: Vec<String> = aggregations
                .iter()
                .map(|(f, e, alias)| format!("{}({}) AS {}", fmt_agg(f), fmt_expr(e), alias))
                .collect();
            buf.push_str(&format!(
                "{}Aggregate group=[{}] agg=[{}]\n",
                pad,
                groups.join(", "),
                aggs.join(", ")
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Sort { input, items } => {
            let keys: Vec<String> = items
                .iter()
                .map(|s| {
                    format!(
                        "{} {}",
                        fmt_expr(&s.expr),
                        if s.ascending { "ASC" } else { "DESC" }
                    )
                })
                .collect();
            buf.push_str(&format!("{}Sort [{}]\n", pad, keys.join(", ")));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Limit { input, skip, count } => {
            buf.push_str(&format!("{}Limit skip={} count={}\n", pad, skip, count));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Unwind {
            input,
            expr,
            variable,
        } => {
            buf.push_str(&format!(
                "{}Unwind {} AS {}\n",
                pad,
                fmt_expr(expr),
                variable
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::OptionalMatch { input, null_vars } => {
            buf.push_str(&format!(
                "{}OptionalMatch null_vars=[{}]\n",
                pad,
                null_vars.join(", ")
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
    }

    buf
}

fn fmt_expr(expr: &Expr) -> String {
    match expr {
        Expr::Prop(var, prop) if prop.is_empty() => var.clone(),
        Expr::Prop(var, prop) => format!("{}.{}", var, prop),
        Expr::Literal(lit) => fmt_literal(lit),
        Expr::Param(name) => format!("${}", name),
        Expr::CountStar => "count(*)".to_string(),
        Expr::Agg(f, inner) => format!("{}({})", fmt_agg(f), fmt_expr(inner)),
        _ => "expr".to_string(),
    }
}

fn fmt_literal(lit: &Literal) -> String {
    match lit {
        Literal::Str(s) => format!("'{}'", s),
        Literal::Int(i) => i.to_string(),
        Literal::Float(f) => format!("{}", f),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "null".to_string(),
        Literal::List(items) => format!(
            "[{}]",
            items.iter().map(fmt_literal).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn fmt_filter(expr: &FilterExpr) -> String {
    match expr {
        FilterExpr::Eq(l, r) => format!("{} = {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Ne(l, r) => format!("{} <> {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Lt(l, r) => format!("{} < {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Gt(l, r) => format!("{} > {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Le(l, r) => format!("{} <= {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Ge(l, r) => format!("{} >= {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::HasLabel(var, label) => format!("{}:{}", var, label),
        FilterExpr::Expr(e) => fmt_expr(e),
    }
}

fn fmt_agg(f: &AggFn) -> String {
    match f {
        AggFn::Count { distinct: true } => "count(DISTINCT".to_string(),
        AggFn::Count { distinct: false } => "count".to_string(),
        AggFn::Sum => "sum".to_string(),
        AggFn::Avg => "avg".to_string(),
        AggFn::Min => "min".to_string(),
        AggFn::Max => "max".to_string(),
        AggFn::Collect => "collect".to_string(),
    }
}
