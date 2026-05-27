use std::collections::HashSet;

use crate::ast::Expr;
use crate::plan::logical::FilterExpr;
use crate::plan::physical::PhysicalOperator;
use crate::plan::stats::StatsProvider;

/// An optimizer that applies relational algebra optimization passes to physical plans.
pub struct Optimizer;

impl Optimizer {
    /// Optimize a `PhysicalOperator` plan by standardizing operator sequences,
    /// extracting filter predicates, and pushing them down to the lowest possible nodes.
    pub fn optimize(op: PhysicalOperator, stats: Option<&dyn StatsProvider>) -> PhysicalOperator {
        let (stripped_op, mut filters) = Self::extract_filters(op);
        let reordered_op = Self::reorder_operators(stripped_op, stats);
        let mut result = Self::push_down_filters(reordered_op, &mut filters);
        // Any filter whose referenced variables are not bound by any operator in the
        // tree cannot be pushed down. Wrap them above the root so no predicate is
        // silently discarded.
        for filter_expr in filters {
            result = PhysicalOperator::Filter {
                input: Box::new(result),
                expression: filter_expr,
            };
        }
        result = Self::optimize_index_scans(result, stats);
        result
    }

    /// Extract all filter operators from the physical plan, return a stripped tree,
    /// and collect all predicates into a single collection.
    fn extract_filters(op: PhysicalOperator) -> (PhysicalOperator, Vec<FilterExpr>) {
        match op {
            PhysicalOperator::Filter { input, expression } => {
                let (inner_op, mut inner_filters) = Self::extract_filters(*input);
                inner_filters.push(expression);
                (inner_op, inner_filters)
            }
            PhysicalOperator::SingleRow => (PhysicalOperator::SingleRow, Vec::new()),
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => {
                let (inner_op, inner_filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Unwind {
                        input: Box::new(inner_op),
                        expr,
                        variable,
                    },
                    inner_filters,
                )
            }
            PhysicalOperator::LabelScan { variable, label } => {
                (PhysicalOperator::LabelScan { variable, label }, Vec::new())
            }
            PhysicalOperator::NodeIndexScan {
                variable,
                label,
                property,
                value,
            } => (
                PhysicalOperator::NodeIndexScan {
                    variable,
                    label,
                    property,
                    value,
                },
                Vec::new(),
            ),
            PhysicalOperator::Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
            } => {
                let (inner_op, inner_filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Expand {
                        input: Box::new(inner_op),
                        src_var,
                        rel_var,
                        dst_var,
                        rel_type,
                        is_incoming,
                        is_undirected,
                        min_hops,
                        max_hops,
                    },
                    inner_filters,
                )
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                let (inner_op, inner_filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Project {
                        input: Box::new(inner_op),
                        items,
                        is_barrier,
                    },
                    inner_filters,
                )
            }
            PhysicalOperator::HashJoin { left, right } => {
                let (left_op, mut left_filters) = Self::extract_filters(*left);
                let (right_op, right_filters) = Self::extract_filters(*right);
                left_filters.extend(right_filters);
                (
                    PhysicalOperator::HashJoin {
                        left: Box::new(left_op),
                        right: Box::new(right_op),
                    },
                    left_filters,
                )
            }
            // Aggregate, Sort, and Limit live above the join/expand tree and
            // never directly contain Filter nodes. Pass through without collecting.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                let (inner, filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Aggregate {
                        input: Box::new(inner),
                        group_by,
                        aggregations,
                    },
                    filters,
                )
            }
            PhysicalOperator::Sort { input, items } => {
                let (inner, filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Sort {
                        input: Box::new(inner),
                        items,
                    },
                    filters,
                )
            }
            PhysicalOperator::Limit { input, skip, count } => {
                let (inner, filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::Limit {
                        input: Box::new(inner),
                        skip,
                        count,
                    },
                    filters,
                )
            }
            // OptionalMatch must not have filters extracted out of it; doing so would break
            // the null-row semantics. Pass through transparently.
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                let (inner, inner_filters) = Self::extract_filters(*input);
                (
                    PhysicalOperator::OptionalMatch {
                        input: Box::new(inner),
                        null_vars,
                    },
                    inner_filters,
                )
            }
        }
    }

    /// Standardize traversal sequences by reordering join branches or operators where appropriate.
    fn reorder_operators(
        op: PhysicalOperator,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        match op {
            PhysicalOperator::HashJoin { left, right } => {
                let opt_left = Self::reorder_operators(*left, stats);
                let opt_right = Self::reorder_operators(*right, stats);

                // Standardize join branch order by placing the heavier branch on the left,
                // which guarantees consistent physical structure regardless of Cypher MATCH clause order.
                let left_weight = Self::plan_weight(&opt_left, stats);
                let right_weight = Self::plan_weight(&opt_right, stats);

                if left_weight >= right_weight {
                    PhysicalOperator::HashJoin {
                        left: Box::new(opt_left),
                        right: Box::new(opt_right),
                    }
                } else {
                    PhysicalOperator::HashJoin {
                        left: Box::new(opt_right),
                        right: Box::new(opt_left),
                    }
                }
            }
            PhysicalOperator::Expand {
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
                input: Box::new(Self::reorder_operators(*input, stats)),
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
            },
            PhysicalOperator::Filter { .. } => {
                unreachable!(
                    "`extract_filters` must be called before `reorder_operators`; Filter nodes must not be present at this stage"
                )
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => PhysicalOperator::Project {
                input: Box::new(Self::reorder_operators(*input, stats)),
                items,
                is_barrier,
            },
            leaf @ PhysicalOperator::SingleRow => leaf,
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::reorder_operators(*input, stats)),
                expr,
                variable,
            },
            leaf @ PhysicalOperator::LabelScan { .. } => leaf,
            leaf @ PhysicalOperator::NodeIndexScan { .. } => leaf,
            // Aggregate, Sort, and Limit are placed above the plan by the logical planner
            // after the Join/Expand/Filter tree is built. Reordering does not descend into
            // them; they are transparent pass-throughs here.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::reorder_operators(*input, stats)),
                group_by,
                aggregations,
            },
            PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::reorder_operators(*input, stats)),
                items,
            },
            PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::reorder_operators(*input, stats)),
                skip,
                count,
            },
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(Self::reorder_operators(*input, stats)),
                    null_vars,
                }
            }
        }
    }

    /// Compute plan complexity/weight to assist with operator reordering.
    fn plan_weight(op: &PhysicalOperator, stats: Option<&dyn StatsProvider>) -> usize {
        match op {
            PhysicalOperator::SingleRow => 1,
            PhysicalOperator::Unwind { input, .. } => 1 + Self::plan_weight(input, stats),
            PhysicalOperator::NodeIndexScan { .. } => 2,
            PhysicalOperator::LabelScan { label, .. } => {
                if let Some(lbl) = label {
                    if let Some(s) = stats {
                        s.node_count_by_label(lbl).unwrap_or(1).max(1) as usize
                    } else {
                        1
                    }
                } else {
                    1000
                }
            }
            PhysicalOperator::Expand {
                input, rel_type, ..
            } => {
                let input_weight = Self::plan_weight(input, stats);
                let rel_weight = if let Some(rtype) = rel_type {
                    if let Some(s) = stats {
                        s.edge_count_by_type(rtype).unwrap_or(10).max(1) as usize
                    } else {
                        10
                    }
                } else {
                    100
                };
                input_weight.saturating_mul(rel_weight)
            }
            // Filter nodes are stripped by `extract_filters` before `plan_weight` is ever
            // called; reaching this arm means the optimization pipeline was bypassed.
            PhysicalOperator::Filter { .. } => {
                unreachable!(
                    "`extract_filters` must run before `plan_weight`; \
                     Filter nodes must not be present at this stage"
                )
            }
            PhysicalOperator::Project { input, .. } => Self::plan_weight(input, stats),
            PhysicalOperator::HashJoin { left, right } => {
                Self::plan_weight(left, stats).saturating_mul(Self::plan_weight(right, stats))
            }
            // These operators sit above the core traversal tree; weight them as their child.
            PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::OptionalMatch { input, .. } => Self::plan_weight(input, stats),
        }
    }

    /// Push collected filters down the plan tree to the lowest possible nodes where they can be evaluated.
    fn push_down_filters(op: PhysicalOperator, pending: &mut Vec<FilterExpr>) -> PhysicalOperator {
        match op {
            PhysicalOperator::NodeIndexScan {
                variable,
                label,
                property,
                value,
            } => {
                let mut current_node = PhysicalOperator::NodeIndexScan {
                    variable: variable.clone(),
                    label,
                    property,
                    value,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                current_node
            }
            PhysicalOperator::LabelScan { variable, label } => {
                let mut current_node = PhysicalOperator::LabelScan {
                    variable: variable.clone(),
                    label,
                };

                let bound = Self::bound_vars(&current_node);

                // Push down all filters whose referenced variables are fully bound by this scan.
                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                current_node
            }
            PhysicalOperator::Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
            } => {
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::Expand {
                    input: Box::new(optimized_input),
                    src_var,
                    rel_var,
                    dst_var,
                    rel_type,
                    is_incoming,
                    is_undirected,
                    min_hops,
                    max_hops,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::HashJoin { left, right } => {
                let left_bound = Self::bound_vars(&left);
                let right_bound = Self::bound_vars(&right);

                let mut left_pending = Vec::new();
                let mut right_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&left_bound) {
                        left_pending.push(filter);
                    } else if ref_vars.is_subset(&right_bound) {
                        right_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_left = Self::push_down_filters(*left, &mut left_pending);
                let optimized_right = Self::push_down_filters(*right, &mut right_pending);

                remaining_pending.extend(left_pending);
                remaining_pending.extend(right_pending);

                let mut current_node = PhysicalOperator::HashJoin {
                    left: Box::new(optimized_left),
                    right: Box::new(optimized_right),
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::Project {
                    input: Box::new(optimized_input),
                    items,
                    is_barrier,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::SingleRow => {
                let mut current_node = PhysicalOperator::SingleRow;
                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }
                current_node
            }
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => {
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::Unwind {
                    input: Box::new(optimized_input),
                    expr,
                    variable,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::Filter { .. } => {
                unreachable!("Filter operators must be extracted prior to pushdown optimization")
            }
            // Aggregate, Sort, and Limit live above the join/expand tree. Pushdown does not
            // reach inside them; pass through and recurse into their child.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Aggregate {
                    input: Box::new(optimized),
                    group_by,
                    aggregations,
                }
            }
            PhysicalOperator::Sort { input, items } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Sort {
                    input: Box::new(optimized),
                    items,
                }
            }
            PhysicalOperator::Limit { input, skip, count } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Limit {
                    input: Box::new(optimized),
                    skip,
                    count,
                }
            }
            // Do not push filters into an OptionalMatch; they must remain outside.
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(*input),
                    null_vars,
                }
            }
        }
    }

    /// Compute the set of variables that are bound or introduced by a physical operator.
    fn bound_vars(op: &PhysicalOperator) -> HashSet<String> {
        let mut vars = HashSet::new();
        Self::collect_bound_vars(op, &mut vars);
        vars
    }

    /// Recursively collect variables bound by the physical operator.
    fn collect_bound_vars(op: &PhysicalOperator, vars: &mut HashSet<String>) {
        match op {
            PhysicalOperator::SingleRow => {}
            PhysicalOperator::Unwind {
                input, variable, ..
            } => {
                Self::collect_bound_vars(input, vars);
                vars.insert(variable.clone());
            }
            PhysicalOperator::LabelScan { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::NodeIndexScan { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::Expand {
                input,
                rel_var,
                dst_var,
                min_hops,
                max_hops,
                ..
            } => {
                Self::collect_bound_vars(input, vars);
                // rel_var is only bound for single-hop patterns (min=max=1).
                // Variable-length BFS (any other range) does not insert rel_var
                // into the PathMap, so the optimizer must not treat it as bound
                // or it will misplace filters that reference it.
                if *min_hops == 1 && *max_hops == 1 {
                    vars.insert(rel_var.clone());
                }
                vars.insert(dst_var.clone());
            }
            PhysicalOperator::Filter { input, .. } => {
                Self::collect_bound_vars(input, vars);
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                if *is_barrier {
                    for (expr, alias) in items {
                        let output_var = if let Some(a) = alias {
                            a.clone()
                        } else {
                            match expr {
                                Expr::Prop(var, prop) => {
                                    if prop.is_empty() {
                                        var.clone()
                                    } else {
                                        format!("{}.{}", var, prop)
                                    }
                                }
                                Expr::Literal(lit) => lit.to_string(),
                                Expr::Param(p) => format!("${}", p),
                                Expr::CountStar => "count(*)".to_string(),
                                Expr::Agg(_, _) => "agg".to_string(),
                                _ => "expr".to_string(),
                            }
                        };
                        vars.insert(output_var);
                    }
                } else {
                    Self::collect_bound_vars(input, vars);
                    for (expr, alias) in items {
                        let output_var = if let Some(a) = alias {
                            a.clone()
                        } else {
                            match expr {
                                Expr::Prop(var, prop) => {
                                    if prop.is_empty() {
                                        var.clone()
                                    } else {
                                        format!("{}.{}", var, prop)
                                    }
                                }
                                Expr::Literal(lit) => lit.to_string(),
                                Expr::Param(p) => format!("${}", p),
                                Expr::CountStar => "count(*)".to_string(),
                                Expr::Agg(_, _) => "agg".to_string(),
                                _ => "expr".to_string(),
                            }
                        };
                        vars.insert(output_var);
                    }
                }
            }
            PhysicalOperator::HashJoin { left, right } => {
                Self::collect_bound_vars(left, vars);
                Self::collect_bound_vars(right, vars);
            }
            // Aggregate emits group-by column names as bound variables.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                Self::collect_bound_vars(input, vars);
                for (_fn, _inner, col) in aggregations {
                    vars.insert(col.clone());
                }
                for (expr, alias) in group_by {
                    let name = if let Some(a) = alias {
                        a.clone()
                    } else if let Expr::Prop(var, prop) = expr {
                        if prop.is_empty() {
                            var.clone()
                        } else {
                            format!("{}.{}", var, prop)
                        }
                    } else {
                        continue;
                    };
                    vars.insert(name);
                }
            }
            PhysicalOperator::Sort { input, .. } | PhysicalOperator::Limit { input, .. } => {
                Self::collect_bound_vars(input, vars);
            }
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                Self::collect_bound_vars(input, vars);
                for var in null_vars {
                    vars.insert(var.clone());
                }
            }
        }
    }

    /// Get all variables referenced by a filter expression.
    fn referenced_vars(expr: &FilterExpr) -> HashSet<String> {
        let mut vars = HashSet::new();
        match expr {
            FilterExpr::Eq(l, r)
            | FilterExpr::Ne(l, r)
            | FilterExpr::Lt(l, r)
            | FilterExpr::Gt(l, r)
            | FilterExpr::Le(l, r)
            | FilterExpr::Ge(l, r) => {
                Self::collect_expr_vars(l, &mut vars);
                Self::collect_expr_vars(r, &mut vars);
            }
            FilterExpr::HasLabel(var, _) => {
                vars.insert(var.clone());
            }
            FilterExpr::Expr(e) => {
                Self::collect_expr_vars(e, &mut vars);
            }
        }
        vars
    }

    /// Recursively collect variables referenced by an expression.
    fn collect_expr_vars(expr: &Expr, vars: &mut HashSet<String>) {
        match expr {
            Expr::Prop(var, _) => {
                vars.insert(var.clone());
            }
            Expr::Literal(_) | Expr::Param(_) => {}
            // Aggregate expressions and CountStar reference variables inside their
            // inner expressions; delegate to recursive collection if needed.
            Expr::CountStar => {}
            Expr::Agg(_, inner) => Self::collect_expr_vars(inner, vars),
            _ => {}
        }
    }

    /// Recursively optimize LabelScan + Filter combinations into NodeIndexScan if an index is available.
    fn optimize_index_scans(
        op: PhysicalOperator,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        match op {
            PhysicalOperator::Filter { input, expression } => {
                let optimized_input = Self::optimize_index_scans(*input, stats);
                if let PhysicalOperator::LabelScan {
                    variable,
                    label: Some(lbl),
                } = &optimized_input
                {
                    if let Some(s) = stats {
                        if let FilterExpr::Eq(l, r) = &expression {
                            if let Expr::Prop(var, prop) = l {
                                if var == variable {
                                    if let Expr::Literal(_) | Expr::Param(_) = r {
                                        if s.has_node_property_index(lbl, prop) {
                                            return PhysicalOperator::NodeIndexScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop.clone(),
                                                value: r.clone(),
                                            };
                                        }
                                    }
                                }
                            }
                            if let Expr::Prop(var, prop) = r {
                                if var == variable {
                                    if let Expr::Literal(_) | Expr::Param(_) = l {
                                        if s.has_node_property_index(lbl, prop) {
                                            return PhysicalOperator::NodeIndexScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop.clone(),
                                                value: l.clone(),
                                            };
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                PhysicalOperator::Filter {
                    input: Box::new(optimized_input),
                    expression,
                }
            }
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                expr,
                variable,
            },
            PhysicalOperator::Expand {
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
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
            },
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => PhysicalOperator::Project {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                items,
                is_barrier,
            },
            PhysicalOperator::HashJoin { left, right } => PhysicalOperator::HashJoin {
                left: Box::new(Self::optimize_index_scans(*left, stats)),
                right: Box::new(Self::optimize_index_scans(*right, stats)),
            },
            leaf => leaf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::plan::logical::LogicalPlanner;
    use crate::plan::physical::PhysicalPlanner;

    #[test]
    fn test_filter_pushdown_basic() {
        // Query: MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name
        let stmt = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name",
        )
        .unwrap();

        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };

        let logical_plan = LogicalPlanner::plan(&query).unwrap();
        let physical_plan = PhysicalPlanner::plan(&logical_plan);
        let optimized_plan = Optimizer::optimize(physical_plan, None);

        // Optimized physical plan structure should have:
        // Project
        //   Filter (b:Person label check)
        //     Expand (a -> b)
        //       Filter (a.age = 30)
        //         LabelScan (a:Person)

        if let PhysicalOperator::Project { input, .. } = optimized_plan {
            if let PhysicalOperator::Filter {
                input: filter_input,
                expression,
            } = *input
            {
                assert!(
                    matches!(expression, FilterExpr::HasLabel(ref var, ref label) if var == "b" && label == "Person")
                );

                if let PhysicalOperator::Expand {
                    input: expand_input,
                    src_var,
                    dst_var,
                    ..
                } = *filter_input
                {
                    assert_eq!(src_var, "a");
                    assert_eq!(dst_var, "b");

                    if let PhysicalOperator::Filter {
                        input: scan_input,
                        expression: scan_expr,
                    } = *expand_input
                    {
                        assert!(
                            matches!(scan_expr, FilterExpr::Eq(Expr::Prop(ref var, ref prop), Expr::Literal(_)) if var == "a" && prop == "age")
                        );

                        assert!(
                            matches!(*scan_input, PhysicalOperator::LabelScan { ref variable, ref label } if variable == "a" && label.as_deref() == Some("Person"))
                        );
                    } else {
                        panic!("expected Filter for a.age = 30 wrapping LabelScan");
                    }
                } else {
                    panic!("expected Expand operator");
                }
            } else {
                panic!("expected Filter for b label check");
            }
        } else {
            panic!("expected Project operator");
        }
    }

    #[test]
    fn test_filter_pushdown_join_and_boundary() {
        let plan = PhysicalOperator::HashJoin {
            left: Box::new(PhysicalOperator::LabelScan {
                variable: "a".to_string(),
                label: Some("Person".to_string()),
            }),
            right: Box::new(PhysicalOperator::LabelScan {
                variable: "b".to_string(),
                label: Some("Company".to_string()),
            }),
        };

        // Let's add a filter that references 'a' only: a.name = "Alice"
        let filter_a = FilterExpr::Eq(
            Expr::Prop("a".to_string(), "name".to_string()),
            Expr::Literal(crate::ast::Literal::Str("Alice".to_string())),
        );

        // Let's add a filter that references 'b' only: b.industry = "Tech"
        let filter_b = FilterExpr::Eq(
            Expr::Prop("b".to_string(), "industry".to_string()),
            Expr::Literal(crate::ast::Literal::Str("Tech".to_string())),
        );

        // Let's add a filter that references both 'a' and 'b': a.id = b.id
        let filter_join = FilterExpr::Eq(
            Expr::Prop("a".to_string(), "id".to_string()),
            Expr::Prop("b".to_string(), "id".to_string()),
        );

        // Let's wrap our join plan with these filters
        let plan_with_filters = PhysicalOperator::Filter {
            input: Box::new(PhysicalOperator::Filter {
                input: Box::new(PhysicalOperator::Filter {
                    input: Box::new(plan),
                    expression: filter_join.clone(),
                }),
                expression: filter_b.clone(),
            }),
            expression: filter_a.clone(),
        };

        let optimized = Optimizer::optimize(plan_with_filters, None);

        // Expected optimized plan:
        // Filter (a.id = b.id) [Join level]
        //   HashJoin
        //     left: Filter (a.name = "Alice") [Pushed down to a]
        //       LabelScan (a)
        //     right: Filter (b.industry = "Tech") [Pushed down to b]
        //       LabelScan (b)

        if let PhysicalOperator::Filter {
            input: join_input,
            expression: join_expr,
        } = optimized
        {
            assert_eq!(join_expr, filter_join);

            if let PhysicalOperator::HashJoin { left, right } = *join_input {
                if let PhysicalOperator::Filter {
                    input: left_scan,
                    expression: left_expr,
                } = *left
                {
                    assert_eq!(left_expr, filter_a);
                    assert!(
                        matches!(*left_scan, PhysicalOperator::LabelScan { ref variable, .. } if variable == "a")
                    );
                } else {
                    panic!("expected pushed-down filter on left branch");
                }

                if let PhysicalOperator::Filter {
                    input: right_scan,
                    expression: right_expr,
                } = *right
                {
                    assert_eq!(right_expr, filter_b);
                    assert!(
                        matches!(*right_scan, PhysicalOperator::LabelScan { ref variable, .. } if variable == "b")
                    );
                } else {
                    panic!("expected pushed-down filter on right branch");
                }
            } else {
                panic!("expected HashJoin operator");
            }
        } else {
            panic!("expected Filter at root for join predicate");
        }
    }

    #[test]
    fn test_operator_reordering() {
        let simple_branch = PhysicalOperator::LabelScan {
            variable: "a".to_string(),
            label: Some("Person".to_string()),
        };

        let complex_branch = PhysicalOperator::Expand {
            input: Box::new(PhysicalOperator::LabelScan {
                variable: "b".to_string(),
                label: Some("Company".to_string()),
            }),
            src_var: "b".to_string(),
            rel_var: "r".to_string(),
            dst_var: "c".to_string(),
            rel_type: Some("EMPLOYEE".to_string()),
            is_incoming: false,
            is_undirected: false,
            min_hops: 1,
            max_hops: 1,
        };

        let join_plan = PhysicalOperator::HashJoin {
            left: Box::new(simple_branch.clone()),
            right: Box::new(complex_branch.clone()),
        };

        let optimized = Optimizer::optimize(join_plan, None);

        if let PhysicalOperator::HashJoin { left, right } = optimized {
            // Complex branch (weight 10 = LabelScan(1) * Expand rel_weight(10)) should be left.
            assert_eq!(*left, complex_branch);
            assert_eq!(*right, simple_branch);
        } else {
            panic!("expected HashJoin");
        }
    }

    struct MockStats {
        node_counts: std::collections::HashMap<String, u64>,
        edge_counts: std::collections::HashMap<String, u64>,
    }

    impl StatsProvider for MockStats {
        fn node_count_by_label(&self, label: &str) -> Option<u64> {
            self.node_counts.get(label).copied()
        }

        fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
            self.edge_counts.get(etype).copied()
        }
    }

    #[test]
    fn test_cost_based_operator_reordering() {
        let scan_a = PhysicalOperator::LabelScan {
            variable: "a".to_string(),
            label: Some("Person".to_string()),
        };
        let scan_b = PhysicalOperator::LabelScan {
            variable: "b".to_string(),
            label: Some("Company".to_string()),
        };

        let join_plan = PhysicalOperator::HashJoin {
            left: Box::new(scan_a.clone()),
            right: Box::new(scan_b.clone()),
        };

        // Case 1: Person (10) < Company (100).
        // Since HashJoin reorders heavier branch to the left, Company should be left, Person right.
        let mut node_counts = std::collections::HashMap::new();
        node_counts.insert("Person".to_string(), 10);
        node_counts.insert("Company".to_string(), 100);
        let stats1 = MockStats {
            node_counts: node_counts.clone(),
            edge_counts: std::collections::HashMap::new(),
        };

        let optimized1 = Optimizer::optimize(join_plan.clone(), Some(&stats1));
        if let PhysicalOperator::HashJoin { left, right } = optimized1 {
            assert_eq!(*left, scan_b);
            assert_eq!(*right, scan_a);
        } else {
            panic!("expected HashJoin");
        }

        // Case 2: Person (500) > Company (20).
        // Person should be left, Company right.
        let mut node_counts = std::collections::HashMap::new();
        node_counts.insert("Person".to_string(), 500);
        node_counts.insert("Company".to_string(), 20);
        let stats2 = MockStats {
            node_counts,
            edge_counts: std::collections::HashMap::new(),
        };

        let optimized2 = Optimizer::optimize(join_plan, Some(&stats2));
        if let PhysicalOperator::HashJoin { left, right } = optimized2 {
            assert_eq!(*left, scan_a);
            assert_eq!(*right, scan_b);
        } else {
            panic!("expected HashJoin");
        }
    }
}
