use crate::ast::{AggFn, Expr, MatchClause, Query, SortItem, WhereClause};

#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    Eq(Expr, Expr),
    Ne(Expr, Expr),
    Lt(Expr, Expr),
    Gt(Expr, Expr),
    Le(Expr, Expr),
    Ge(Expr, Expr),
    HasLabel(String, String), // Bounded variable has a specific label
    /// Arbitrary boolean expression (IS NULL, OR, AND, NOT, quantifiers, etc.)
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOperator {
    /// A single empty row to bootstrap queries.
    SingleRow,
    /// Unwind a list expression and bind each element to a variable.
    Unwind {
        input: Box<LogicalOperator>,
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
        input: Box<LogicalOperator>,
        src_var: String,
        rel_var: String,
        dst_var: String,
        rel_type: Option<String>,
        is_incoming: bool,
        /// When true the relationship has no direction: traverse both outgoing and
        /// incoming edges and deduplicate results.
        is_undirected: bool,
        min_hops: usize,
        max_hops: usize,
    },
    /// Filter records based on expressions/WHERE predicates.
    Filter {
        input: Box<LogicalOperator>,
        expression: FilterExpr,
    },
    /// Project RETURN expressions to form the final table.
    Project {
        input: Box<LogicalOperator>,
        items: Vec<(Expr, Option<String>)>, // (expression, alias)
        is_barrier: bool,
    },
    /// Join two independent logical sub-plans (cross product / hash join).
    Join {
        left: Box<LogicalOperator>,
        right: Box<LogicalOperator>,
    },
    /// Aggregate rows, grouping by non-aggregate expressions and computing
    /// aggregation functions (count, sum, avg, min, max, collect) per group.
    Aggregate {
        input: Box<LogicalOperator>,
        /// Non-aggregate RETURN items used as group-by keys.
        group_by: Vec<(Expr, Option<String>)>,
        /// Aggregate RETURN items: (agg_fn, inner_expr, output_column_name).
        aggregations: Vec<(AggFn, Expr, String)>,
    },
    /// Sort rows by one or more expressions.
    Sort {
        input: Box<LogicalOperator>,
        items: Vec<SortItem>,
    },
    /// Deduplicate rows (RETURN/WITH DISTINCT).
    Distinct {
        input: Box<LogicalOperator>,
    },
    /// Skip and limit the row stream.
    Limit {
        input: Box<LogicalOperator>,
        skip: usize,
        count: usize,
    },
    /// Optional match: evaluate inner plan; if it produces no rows, emit one
    /// null-filled row for each pattern variable in `null_vars`.
    OptionalMatch {
        input: Box<LogicalOperator>,
        null_vars: Vec<String>,
    },
}

pub struct LogicalPlanner;

impl LogicalPlanner {
    pub fn plan(query: &Query) -> Result<LogicalOperator, String> {
        let mut plan = if query.parts.is_empty() {
            // Legacy single-MATCH planning flow.
            // When match_clauses is also empty this is a bare RETURN query (e.g. `RETURN 1 + 2`).
            if query.match_clauses.is_empty() {
                // Bare RETURN: start from a single empty row.
                let p = LogicalOperator::SingleRow;
                let items: Vec<(Expr, Option<String>)> = query
                    .return_clause
                    .items
                    .iter()
                    .map(|ri| (ri.expr.clone(), ri.alias.clone()))
                    .collect();
                return Ok(LogicalOperator::Project {
                    input: Box::new(p),
                    items,
                    is_barrier: true,
                });
            }

            let mut current_plan: Option<LogicalOperator> = None;

            for match_clause in &query.match_clauses {
                let match_plan = Self::plan_match(match_clause)?;
                current_plan = match current_plan {
                    Some(existing) => Some(LogicalOperator::Join {
                        left: Box::new(existing),
                        right: Box::new(match_plan),
                    }),
                    None => Some(match_plan),
                };
            }

            let mut p = current_plan.ok_or_else(|| "failed to generate MATCH plan".to_string())?;

            // Apply WHERE clause if present
            if let Some(ref where_clause) = query.where_clause {
                let filter_expr = match where_clause {
                    WhereClause::Eq(l, r) => FilterExpr::Eq(l.clone(), r.clone()),
                    WhereClause::Ne(l, r) => FilterExpr::Ne(l.clone(), r.clone()),
                    WhereClause::Lt(l, r) => FilterExpr::Lt(l.clone(), r.clone()),
                    WhereClause::Gt(l, r) => FilterExpr::Gt(l.clone(), r.clone()),
                    WhereClause::Le(l, r) => FilterExpr::Le(l.clone(), r.clone()),
                    WhereClause::Ge(l, r) => FilterExpr::Ge(l.clone(), r.clone()),
                    WhereClause::Expr(e) => FilterExpr::Expr(e.clone()),
                };
                p = LogicalOperator::Filter {
                    input: Box::new(p),
                    expression: filter_expr,
                };
            }
            p
        } else {
            // New multi-part sequence planning flow
            use crate::ast::QueryPart;
            let mut current_plan: Option<LogicalOperator> = None;

            for part in &query.parts {
                match part {
                    QueryPart::Match {
                        match_clauses,
                        where_clause,
                    } => {
                        if match_clauses.is_empty() {
                            return Err("MATCH part must contain at least one MATCH clause".into());
                        }
                        let mut part_match_plan: Option<LogicalOperator> = None;
                        for match_clause in match_clauses {
                            let mp = Self::plan_match(match_clause)?;
                            part_match_plan = match part_match_plan {
                                Some(existing) => Some(LogicalOperator::Join {
                                    left: Box::new(existing),
                                    right: Box::new(mp),
                                }),
                                None => Some(mp),
                            };
                        }
                        let mut match_plan = part_match_plan.ok_or_else(|| {
                            "MATCH part must contain at least one MATCH clause".to_string()
                        })?;
                        if let Some(wc) = where_clause {
                            let filter_expr = match wc {
                                WhereClause::Eq(l, r) => FilterExpr::Eq(l.clone(), r.clone()),
                                WhereClause::Ne(l, r) => FilterExpr::Ne(l.clone(), r.clone()),
                                WhereClause::Lt(l, r) => FilterExpr::Lt(l.clone(), r.clone()),
                                WhereClause::Gt(l, r) => FilterExpr::Gt(l.clone(), r.clone()),
                                WhereClause::Le(l, r) => FilterExpr::Le(l.clone(), r.clone()),
                                WhereClause::Ge(l, r) => FilterExpr::Ge(l.clone(), r.clone()),
                                WhereClause::Expr(e) => FilterExpr::Expr(e.clone()),
                            };
                            match_plan = LogicalOperator::Filter {
                                input: Box::new(match_plan),
                                expression: filter_expr,
                            };
                        }

                        current_plan = match current_plan {
                            Some(existing) => Some(LogicalOperator::Join {
                                left: Box::new(existing),
                                right: Box::new(match_plan),
                            }),
                            None => Some(match_plan),
                        };
                    }
                    QueryPart::With {
                        items,
                        where_clause,
                        order_by,
                        skip,
                        limit,
                        distinct,
                    } => {
                        // Bootstrap with SingleRow if WITH is the first clause in the
                        // sequence (e.g., `WITH 1 AS x RETURN x`), matching the
                        // behavior of the Unwind arm above.
                        let mut p = current_plan.unwrap_or(LogicalOperator::SingleRow);

                        // In Cypher semantics, the WHERE predicate of a WITH clause is
                        // evaluated against the pre-projection rows: variables named in
                        // the filter still refer to the current scope, not the projected
                        // output. Apply the filter BEFORE the Project so that references
                        // like `WITH a AS alias WHERE a.prop = …` resolve correctly.
                        if let Some(wc) = where_clause {
                            let filter_expr = match wc {
                                WhereClause::Eq(l, r) => FilterExpr::Eq(l.clone(), r.clone()),
                                WhereClause::Ne(l, r) => FilterExpr::Ne(l.clone(), r.clone()),
                                WhereClause::Lt(l, r) => FilterExpr::Lt(l.clone(), r.clone()),
                                WhereClause::Gt(l, r) => FilterExpr::Gt(l.clone(), r.clone()),
                                WhereClause::Le(l, r) => FilterExpr::Le(l.clone(), r.clone()),
                                WhereClause::Ge(l, r) => FilterExpr::Ge(l.clone(), r.clone()),
                                WhereClause::Expr(e) => FilterExpr::Expr(e.clone()),
                            };
                            p = LogicalOperator::Filter {
                                input: Box::new(p),
                                expression: filter_expr,
                            };
                        }

                        let project_items = items
                            .iter()
                            .map(|item| (item.expr.clone(), item.alias.clone()))
                            .collect();

                        p = LogicalOperator::Project {
                            input: Box::new(p),
                            items: project_items,
                            is_barrier: true,
                        };

                        // Apply WITH DISTINCT deduplication after project.
                        if *distinct {
                            p = LogicalOperator::Distinct {
                                input: Box::new(p),
                            };
                        }

                        // Apply optional ORDER BY attached to the WITH clause.
                        if let Some(ob) = order_by {
                            p = LogicalOperator::Sort {
                                input: Box::new(p),
                                items: ob.items.clone(),
                            };
                        }

                        // Apply optional SKIP / LIMIT attached to the WITH clause.
                        let skip_n = skip.as_ref().map(literal_usize).unwrap_or(0);
                        let limit_n = limit.as_ref().map(literal_usize).unwrap_or(usize::MAX);
                        if skip.is_some() || limit.is_some() {
                            p = LogicalOperator::Limit {
                                input: Box::new(p),
                                skip: skip_n,
                                count: limit_n,
                            };
                        }

                        current_plan = Some(p);
                    }
                    QueryPart::OptionalMatch {
                        match_clauses,
                        where_clause,
                    } => {
                        if match_clauses.is_empty() {
                            return Err(
                                "OPTIONAL MATCH part must contain at least one clause".into()
                            );
                        }
                        let mut part_match_plan: Option<LogicalOperator> = None;
                        // Collect all variables referenced in the pattern for null-filling.
                        let mut null_vars: Vec<String> = Vec::new();
                        for match_clause in match_clauses {
                            if let Some(ref v) = match_clause.pattern.node.variable {
                                null_vars.push(v.clone());
                            }
                            for (rel, target) in &match_clause.pattern.rels {
                                if let Some(ref v) = rel.variable {
                                    null_vars.push(v.clone());
                                }
                                if let Some(ref v) = target.variable {
                                    null_vars.push(v.clone());
                                }
                            }
                            let mp = Self::plan_match(match_clause)?;
                            part_match_plan = match part_match_plan {
                                Some(existing) => Some(LogicalOperator::Join {
                                    left: Box::new(existing),
                                    right: Box::new(mp),
                                }),
                                None => Some(mp),
                            };
                        }
                        let mut match_plan = part_match_plan.ok_or_else(|| {
                            "OPTIONAL MATCH part must contain at least one clause".to_string()
                        })?;
                        if let Some(wc) = where_clause {
                            let filter_expr = match wc {
                                WhereClause::Eq(l, r) => FilterExpr::Eq(l.clone(), r.clone()),
                                WhereClause::Ne(l, r) => FilterExpr::Ne(l.clone(), r.clone()),
                                WhereClause::Lt(l, r) => FilterExpr::Lt(l.clone(), r.clone()),
                                WhereClause::Gt(l, r) => FilterExpr::Gt(l.clone(), r.clone()),
                                WhereClause::Le(l, r) => FilterExpr::Le(l.clone(), r.clone()),
                                WhereClause::Ge(l, r) => FilterExpr::Ge(l.clone(), r.clone()),
                                WhereClause::Expr(e) => FilterExpr::Expr(e.clone()),
                            };
                            match_plan = LogicalOperator::Filter {
                                input: Box::new(match_plan),
                                expression: filter_expr,
                            };
                        }

                        let optional_plan = LogicalOperator::OptionalMatch {
                            input: Box::new(match_plan),
                            null_vars,
                        };

                        current_plan = match current_plan {
                            Some(existing) => Some(LogicalOperator::Join {
                                left: Box::new(existing),
                                right: Box::new(optional_plan),
                            }),
                            None => Some(optional_plan),
                        };
                    }
                    QueryPart::Unwind { expr, variable } => {
                        let p = current_plan.unwrap_or(LogicalOperator::SingleRow);
                        current_plan = Some(LogicalOperator::Unwind {
                            input: Box::new(p),
                            expr: expr.clone(),
                            variable: variable.clone(),
                        });
                    }
                }
            }

            current_plan.ok_or_else(|| "failed to generate plan for parts".to_string())?
        };

        // Split RETURN items into group-by keys and aggregations.
        let (group_by_items, agg_items) = split_return_items(query);

        // Insert Aggregate operator when at least one aggregation is present.
        if !agg_items.is_empty() {
            plan = LogicalOperator::Aggregate {
                input: Box::new(plan),
                group_by: group_by_items,
                aggregations: agg_items,
            };
        }

        // Apply final RETURN projection (only over non-aggregate expressions when
        // aggregation is present; Aggregate already emits aggregate column names).
        let project_items: Vec<(Expr, Option<String>)> = query
            .return_clause
            .items
            .iter()
            .map(|item| (item.expr.clone(), item.alias.clone()))
            .collect();

        plan = LogicalOperator::Project {
            input: Box::new(plan),
            items: project_items,
            is_barrier: false,
        };

        // INSERT ORDER BY above Project.
        if let Some(ref ob) = query.order_by {
            plan = LogicalOperator::Sort {
                input: Box::new(plan),
                items: ob.items.clone(),
            };
        }

        // INSERT SKIP / LIMIT above Sort (or Project if no ORDER BY).
        // Validate SKIP/LIMIT expressions before using them.
        if let Some(skip_expr) = &query.skip {
            validate_skip_limit(skip_expr, "SKIP")?;
        }
        if let Some(limit_expr) = &query.limit {
            validate_skip_limit(limit_expr, "LIMIT")?;
        }
        let skip_n = query.skip.as_ref().map(literal_usize).unwrap_or(0);
        let limit_n = query
            .limit
            .as_ref()
            .map(literal_usize)
            .unwrap_or(usize::MAX);
        if query.skip.is_some() || query.limit.is_some() {
            plan = LogicalOperator::Limit {
                input: Box::new(plan),
                skip: skip_n,
                count: limit_n,
            };
        }

        Ok(plan)
    }

    fn plan_match(match_clause: &MatchClause) -> Result<LogicalOperator, String> {
        let pattern = &match_clause.pattern;
        let seed_var = pattern
            .node
            .variable
            .clone()
            .unwrap_or_else(|| "_seed".to_string());

        let mut plan = LogicalOperator::LabelScan {
            variable: seed_var.clone(),
            label: pattern.node.label.clone(),
        };

        // Apply inline properties filter on the seed node if specified
        if let Some(ref props) = pattern.node.properties {
            for (k, v) in props {
                plan = LogicalOperator::Filter {
                    input: Box::new(plan),
                    expression: FilterExpr::Eq(
                        Expr::Prop(seed_var.clone(), k.clone()),
                        Expr::Literal(v.clone()),
                    ),
                };
            }
        }

        let mut prev_node_var = seed_var;

        for (seg_idx, (rel_pat, node_pat)) in pattern.rels.iter().enumerate() {
            // Use segment-indexed fallback names so auto-generated variables do not
            // collide across multiple relationship segments in the same pattern.
            let rel_var = rel_pat
                .variable
                .clone()
                .unwrap_or_else(|| format!("_rel_{}", seg_idx));
            let target_var = node_pat
                .variable
                .clone()
                .unwrap_or_else(|| format!("_target_{}", seg_idx));

            let min_hops = rel_pat.range.as_ref().and_then(|r| r.min).unwrap_or(1) as usize;
            // Three cases must be distinguished (mirrors exec.rs):
            //   range = None              → no `*`; plain single-hop [:R]    → 1
            //   range = Some { max:None } → bare [:R*] unbounded             → usize::MAX
            //   range = Some { max:Some(n) } → [:R*1..n] explicit upper bound → n
            let max_hops = match rel_pat.range.as_ref() {
                None => 1,
                Some(r) => r.max.map(|v| v as usize).unwrap_or(usize::MAX),
            };

            plan = LogicalOperator::Expand {
                input: Box::new(plan),
                src_var: prev_node_var.clone(),
                rel_var: rel_var.clone(),
                dst_var: target_var.clone(),
                rel_type: rel_pat.rel_type.clone(),
                is_incoming: rel_pat.is_incoming,
                is_undirected: rel_pat.is_undirected,
                min_hops,
                max_hops,
            };

            // Apply inline properties filter on relationship if specified
            if let Some(ref props) = rel_pat.properties {
                for (k, v) in props {
                    plan = LogicalOperator::Filter {
                        input: Box::new(plan),
                        expression: FilterExpr::Eq(
                            Expr::Prop(rel_var.clone(), k.clone()),
                            Expr::Literal(v.clone()),
                        ),
                    };
                }
            }

            // Filter target node label if specified
            if let Some(ref label) = node_pat.label {
                plan = LogicalOperator::Filter {
                    input: Box::new(plan),
                    expression: FilterExpr::HasLabel(target_var.clone(), label.clone()),
                };
            }

            // Apply inline properties filter on target node
            if let Some(ref props) = node_pat.properties {
                for (k, v) in props {
                    plan = LogicalOperator::Filter {
                        input: Box::new(plan),
                        expression: FilterExpr::Eq(
                            Expr::Prop(target_var.clone(), k.clone()),
                            Expr::Literal(v.clone()),
                        ),
                    };
                }
            }

            prev_node_var = target_var;
        }

        Ok(plan)
    }
}

/// Group-by item: `(expression, optional alias)`.
type GroupByItem = (Expr, Option<String>);
/// Aggregation spec: `(function, inner expression, output column name)`.
type AggItem = (AggFn, Expr, String);

/// Compute a display name for an aggregation expression (used as the default column name).
fn agg_display_name(fn_: &AggFn, inner: &Expr) -> String {
    let inner_name = inner_expr_name(inner);
    match fn_ {
        AggFn::Count { distinct: true } => format!("count(DISTINCT {})", inner_name),
        AggFn::Count { distinct: false } => format!("count({})", inner_name),
        AggFn::Sum { .. } => format!("sum({})", inner_name),
        AggFn::Avg { .. } => format!("avg({})", inner_name),
        AggFn::Min { .. } => format!("min({})", inner_name),
        AggFn::Max { .. } => format!("max({})", inner_name),
        AggFn::Collect { .. } => format!("collect({})", inner_name),
        AggFn::StDev { .. } => format!("stDev({})", inner_name),
        AggFn::StDevP { .. } => format!("stDevP({})", inner_name),
        AggFn::PercentileDisc { .. } => format!("percentileDisc({})", inner_name),
        AggFn::PercentileCont { .. } => format!("percentileCont({})", inner_name),
    }
}

fn inner_expr_name(expr: &Expr) -> String {
    match expr {
        Expr::CountStar => "*".to_string(),
        Expr::Prop(var, prop) => {
            if prop.is_empty() {
                var.clone()
            } else {
                format!("{}.{}", var, prop)
            }
        }
        Expr::Literal(lit) => lit.to_string(),
        _ => "expr".to_string(),
    }
}

/// Classify RETURN items into group-by keys (non-aggregate) and aggregation specs.
///
/// Returns `(group_by, aggregations)` where:
/// - `group_by` contains `(expr, alias)` pairs for non-aggregate items.
/// - `aggregations` contains `(agg_fn, inner_expr, output_name)` triples for aggregate items.
fn split_return_items(query: &Query) -> (Vec<GroupByItem>, Vec<AggItem>) {
    let mut group_by = Vec::new();
    let mut aggregations = Vec::new();

    for item in &query.return_clause.items {
        match &item.expr {
            Expr::CountStar => {
                let col = item.alias.clone().unwrap_or_else(|| "count(*)".to_string());
                aggregations.push((AggFn::Count { distinct: false }, Expr::CountStar, col));
            }
            Expr::Agg(fn_, inner) => {
                let col = item.alias.clone().unwrap_or_else(|| agg_display_name(fn_, inner));
                aggregations.push((fn_.clone(), *inner.clone(), col));
            }
            other => {
                group_by.push((other.clone(), item.alias.clone()));
            }
        }
    }

    (group_by, aggregations)
}

/// Extract a `usize` value from a literal `Expr::Literal(Int(...))`.
/// Other expression forms are treated as 0.
fn literal_usize(expr: &Expr) -> usize {
    match expr {
        Expr::Literal(crate::ast::Literal::Int(n)) => (*n).max(0) as usize,
        _ => 0,
    }
}

/// Validate a SKIP or LIMIT expression. Returns an error if the value is non-constant,
/// negative, or a float.
pub(crate) fn validate_skip_limit(expr: &Expr, keyword: &str) -> Result<(), String> {
    match expr {
        Expr::Literal(crate::ast::Literal::Int(n)) => {
            if *n < 0 {
                Err(format!(
                    "SyntaxError: {} value must not be negative, got {}",
                    keyword, n
                ))
            } else {
                Ok(())
            }
        }
        Expr::Literal(crate::ast::Literal::Float(_)) => Err(format!(
            "SyntaxError: {} value must be an integer, not a float",
            keyword
        )),
        Expr::Param(_) => {
            // Parameter-based SKIP/LIMIT is allowed in Cypher; validation happens at runtime.
            Ok(())
        }
        _ => Err(format!(
            "SyntaxError: {} value must be a constant integer expression",
            keyword
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn plan_single_hop_match_query() {
        let stmt = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name",
        )
        .unwrap();

        if let crate::ast::Statement::Query(query) = stmt {
            let plan = LogicalPlanner::plan(&query).unwrap();

            // Expected structure:
            // Project
            //   Filter (WHERE clause age = 30)
            //     Filter (target node label Person)
            //       Expand (KNOWS relationship)
            //         LabelScan (Person label for a)

            if let LogicalOperator::Project { input, items, .. } = plan {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].1.as_deref(), Some("name"));

                if let LogicalOperator::Filter {
                    input: filter_input,
                    expression,
                } = *input
                {
                    assert!(matches!(expression, FilterExpr::Eq(_, _)));

                    if let LogicalOperator::Filter {
                        input: label_input,
                        expression: label_expr,
                    } = *filter_input
                    {
                        assert_eq!(
                            label_expr,
                            FilterExpr::HasLabel("b".to_string(), "Person".to_string())
                        );

                        if let LogicalOperator::Expand {
                            input: expand_input,
                            src_var,
                            rel_var: _,
                            dst_var,
                            rel_type,
                            min_hops,
                            max_hops,
                            ..
                        } = *label_input
                        {
                            assert_eq!(src_var, "a");
                            assert_eq!(dst_var, "b");
                            assert_eq!(rel_type.as_deref(), Some("KNOWS"));
                            assert_eq!(min_hops, 1);
                            assert_eq!(max_hops, 1);

                            assert!(matches!(*expand_input, LogicalOperator::LabelScan { .. }));
                        } else {
                            panic!("expected Expand operator");
                        }
                    } else {
                        panic!("expected target node LabelFilter operator");
                    }
                } else {
                    panic!("expected WHERE clause Filter operator");
                }
            } else {
                panic!("expected Project operator");
            }
        } else {
            panic!("expected read Query");
        }
    }

    #[test]
    fn plan_unbounded_variable_length_uses_max_sentinel() {
        // Regression: unwrap_or(1) was silently capping unbounded [:R*] at 1 hop.
        // The logical plan must encode max_hops = usize::MAX for patterns with no
        // explicit upper bound so that a future physical executor traverses until
        // the frontier is exhausted.
        let cases = [
            (
                "MATCH (a)-[:R*]->(b) RETURN b.name AS name",
                1usize,
                usize::MAX,
            ),
            (
                "MATCH (a)-[:R*2..]->(b) RETURN b.name AS name",
                2,
                usize::MAX,
            ),
            ("MATCH (a)-[:R*1..3]->(b) RETURN b.name AS name", 1, 3),
            ("MATCH (a)-[:R*4]->(b) RETURN b.name AS name", 4, 4),
        ];

        for (cypher, expected_min, expected_max) in cases {
            let stmt = parser::parse(cypher).unwrap();
            if let crate::ast::Statement::Query(query) = stmt {
                let plan = LogicalPlanner::plan(&query).unwrap();
                // Unwrap Project → Expand (no WHERE, no label filter in these queries).
                let expand = unwrap_to_expand(plan);
                assert_eq!(expand.0, expected_min, "min_hops mismatch for: {cypher}");
                assert_eq!(expand.1, expected_max, "max_hops mismatch for: {cypher}");
            } else {
                panic!("expected Query for: {cypher}");
            }
        }
    }

    /// Walk a `Project → [Filter*] → Expand` tree and return `(min_hops, max_hops)`.
    fn unwrap_to_expand(plan: LogicalOperator) -> (usize, usize) {
        match plan {
            LogicalOperator::Project { input, .. } => unwrap_to_expand(*input),
            LogicalOperator::Filter { input, .. } => unwrap_to_expand(*input),
            LogicalOperator::Expand {
                min_hops, max_hops, ..
            } => (min_hops, max_hops),
            other => panic!("unexpected operator: {:?}", other),
        }
    }
}
