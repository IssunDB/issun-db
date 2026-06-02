use crate::ast::{AggFn, Expr, MatchClause, Query, SortItem, WhereClause};
use crate::error::CypherError;
use crate::exec::read::expr_display_name;

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
    Distinct { input: Box<LogicalOperator> },
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
    /// A write clause (CREATE, MERGE, SET, DELETE) in a pipeline query. Executed
    /// for each row produced by the input plan; new bindings are added to the PathMap.
    WritePart {
        input: Box<LogicalOperator>,
        part: crate::ast::QueryPart,
    },
    /// A resolved `CALL` clause. For each input row the operator emits one output
    /// row per entry in `rows`, binding `output_vars` to the corresponding cells.
    /// A void procedure has empty `output_vars` and a single empty row, making the
    /// call an identity over the input.
    ProcedureCall {
        input: Box<LogicalOperator>,
        output_vars: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
    },
}

pub struct LogicalPlanner;

impl LogicalPlanner {
    pub fn plan(query: &Query) -> Result<LogicalOperator, CypherError> {
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

            let mut p = current_plan.ok_or(CypherError::Plan(
                "failed to generate MATCH plan".to_string(),
            ))?;

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
                            return Err(CypherError::Plan(
                                "MATCH part must contain at least one MATCH clause".to_string(),
                            ));
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
                        let mut match_plan = part_match_plan.ok_or(CypherError::Plan(
                            "MATCH part must contain at least one MATCH clause".to_string(),
                        ))?;
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

                        // Split WITH items and extract aggregations from ORDER BY.
                        let (with_group_by, with_aggs, rewritten_with_items) =
                            split_with_items(items);
                        let mut with_aggs = with_aggs;

                        let mut order_by_items = order_by.as_ref().map(|ob| ob.items.clone());

                        let projections: Vec<(Expr, String)> = items
                            .iter()
                            .map(|item| {
                                let target_var = if let Some(ref a) = item.alias {
                                    a.clone()
                                } else {
                                    expr_display_name(&item.expr)
                                };
                                (item.expr.clone(), target_var)
                            })
                            .collect();

                        if let Some(ref mut items) = order_by_items {
                            for si in items.iter_mut() {
                                rewrite_expr_with_aliases(&mut si.expr, &projections);
                            }
                        }

                        if let Some(ref mut items) = order_by_items {
                            let mut count_index = 0;
                            for si in items.iter_mut() {
                                if expr_has_aggregation(&si.expr) {
                                    extract_and_replace_aggs(
                                        &mut si.expr,
                                        &mut with_aggs,
                                        &mut count_index,
                                        "_ord_agg_",
                                    );
                                }
                            }
                        }

                        if !with_aggs.is_empty() {
                            p = LogicalOperator::Aggregate {
                                input: Box::new(p),
                                group_by: with_group_by,
                                aggregations: with_aggs,
                            };
                        }

                        let mut project_items: Vec<(Expr, Option<String>)> = rewritten_with_items
                            .iter()
                            .map(|item| (item.expr.clone(), item.alias.clone()))
                            .collect();

                        // Add pre-computed ORDER BY aggregations to WITH project items so they survive the projection barrier.
                        if let Some(ref items) = order_by_items {
                            let mut ord_aggs = std::collections::HashSet::new();
                            for si in items.iter() {
                                collect_ord_aggs(&si.expr, &mut ord_aggs);
                            }
                            for col in ord_aggs {
                                project_items.push((
                                    Expr::Prop(col.clone(), "".to_string()),
                                    Some(col.clone()),
                                ));
                            }
                        }

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

                            // The WHERE clause of a WITH must see both pre-projection
                            // variables (bound before the WITH) and post-projection aliases
                            // (defined by the WITH items themselves). To support both, first
                            // add the projected aliases into the PathMap without removing
                            // pre-projection variables (non-barrier), then apply the filter,
                            // then apply the barrier cleanup to remove pre-projection vars.
                            p = LogicalOperator::Project {
                                input: Box::new(p),
                                items: project_items.clone(),
                                is_barrier: false, // non-barrier: adds aliases, keeps old vars
                            };
                            p = LogicalOperator::Filter {
                                input: Box::new(p),
                                expression: filter_expr,
                            };
                            // Barrier cleanup: keep only the WITH-projected aliases.
                            p = LogicalOperator::Project {
                                input: Box::new(p),
                                items: project_items,
                                is_barrier: true,
                            };
                        } else {
                            p = LogicalOperator::Project {
                                input: Box::new(p),
                                items: project_items,
                                is_barrier: true,
                            };
                        }

                        // Apply WITH DISTINCT deduplication after project.
                        if *distinct {
                            p = LogicalOperator::Distinct { input: Box::new(p) };
                        }

                        // Apply optional ORDER BY attached to the WITH clause. ORDER BY scope
                        // validation happens at parse time in `validate_statement`.
                        if let Some(ref items) = order_by_items {
                            p = LogicalOperator::Sort {
                                input: Box::new(p),
                                items: items.clone(),
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
                            return Err(CypherError::Plan(
                                "OPTIONAL MATCH part must contain at least one clause".to_string(),
                            ));
                        }
                        let mut part_match_plan: Option<LogicalOperator> = None;
                        // Collect all variables referenced in the pattern for null-filling.
                        let mut null_vars: Vec<String> = Vec::new();
                        for match_clause in match_clauses {
                            if let Some(ref pv) = match_clause.pattern.path_variable {
                                null_vars.push(pv.clone());
                            }
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
                        let mut match_plan = part_match_plan.ok_or(CypherError::Plan(
                            "OPTIONAL MATCH part must contain at least one clause".to_string(),
                        ))?;
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
                    QueryPart::Call { resolved, .. } => {
                        let p = current_plan.unwrap_or(LogicalOperator::SingleRow);
                        // `resolved` is populated by the execution-time resolution
                        // pass; an unresolved call (e.g. from plan inspection) is an
                        // identity over the input.
                        let (output_vars, rows) = match resolved {
                            Some(rc) => (rc.output_vars.clone(), rc.rows.clone()),
                            None => (vec![], vec![vec![]]),
                        };
                        current_plan = Some(LogicalOperator::ProcedureCall {
                            input: Box::new(p),
                            output_vars,
                            rows,
                        });
                    }
                    // Write clause variants are passed through as-is for the physical planner
                    // to compile into WritePart operators. They are not planned as logical
                    // read operators.
                    write_part @ (QueryPart::Create { .. }
                    | QueryPart::Merge { .. }
                    | QueryPart::Set { .. }
                    | QueryPart::Delete { .. }
                    | QueryPart::Remove { .. }) => {
                        let p = current_plan.unwrap_or(LogicalOperator::SingleRow);
                        current_plan = Some(LogicalOperator::WritePart {
                            input: Box::new(p),
                            part: write_part.clone(),
                        });
                    }
                }
            }

            // If no parts produced a plan, bootstrap with SingleRow for bare write queries
            // (e.g., a pipeline query with only write parts that was dispatched as a Query).
            current_plan.unwrap_or(LogicalOperator::SingleRow)
        };

        // Split RETURN items into group-by keys and aggregations.
        let (group_by_items, agg_items, rewritten_return_items) = split_return_items(query);
        let mut agg_items = agg_items;

        let mut order_by_items = query.order_by.as_ref().map(|ob| ob.items.clone());

        let projections: Vec<(Expr, String)> = query
            .return_clause
            .items
            .iter()
            .map(|item| {
                let target_var = if let Some(ref a) = item.alias {
                    a.clone()
                } else {
                    expr_display_name(&item.expr)
                };
                (item.expr.clone(), target_var)
            })
            .collect();

        if let Some(ref mut items) = order_by_items {
            for si in items.iter_mut() {
                rewrite_expr_with_aliases(&mut si.expr, &projections);
            }
        }

        if let Some(ref mut items) = order_by_items {
            let mut count_index = 0;
            for si in items.iter_mut() {
                if expr_has_aggregation(&si.expr) {
                    extract_and_replace_aggs(
                        &mut si.expr,
                        &mut agg_items,
                        &mut count_index,
                        "_ord_agg_",
                    );
                }
            }
        }

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
        let mut project_items: Vec<(Expr, Option<String>)> = rewritten_return_items
            .iter()
            .map(|item| (item.expr.clone(), item.alias.clone()))
            .collect();

        // Add pre-computed ORDER BY aggregations to Project items so they survive the projection barrier.
        if let Some(ref items) = order_by_items {
            let mut ord_aggs = std::collections::HashSet::new();
            for si in items.iter() {
                collect_ord_aggs(&si.expr, &mut ord_aggs);
            }
            for col in ord_aggs {
                project_items.push((Expr::Prop(col.clone(), "".to_string()), Some(col.clone())));
            }
        }

        plan = LogicalOperator::Project {
            input: Box::new(plan),
            items: project_items,
            is_barrier: false,
        };

        // INSERT ORDER BY above Project.
        if let Some(ref items) = order_by_items {
            plan = LogicalOperator::Sort {
                input: Box::new(plan),
                items: items.clone(),
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
    fn plan_match(match_clause: &MatchClause) -> Result<LogicalOperator, CypherError> {
        let pattern = &match_clause.pattern;
        let seed_var = pattern
            .node
            .variable
            .clone()
            .unwrap_or_else(|| "_seed".to_string());

        // Scan by the first label; additional labels become HasLabel filters so
        // a multi-label pattern such as (n:A:B) requires the node to carry all of them.
        let mut plan = LogicalOperator::LabelScan {
            variable: seed_var.clone(),
            label: pattern.node.labels.first().cloned(),
        };
        for extra_label in pattern.node.labels.iter().skip(1) {
            plan = LogicalOperator::Filter {
                input: Box::new(plan),
                expression: FilterExpr::HasLabel(seed_var.clone(), extra_label.clone()),
            };
        }

        // Apply inline properties filter on the seed node if specified.
        // Properties are now arbitrary expressions (not just literals), so
        // use FilterExpr::Eq with the expression value directly.
        if let Some(ref props) = pattern.node.properties {
            for (k, v) in props {
                plan = LogicalOperator::Filter {
                    input: Box::new(plan),
                    expression: FilterExpr::Eq(Expr::Prop(seed_var.clone(), k.clone()), v.clone()),
                };
            }
        }

        let mut prev_node_var = seed_var.clone();

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

            // Apply inline properties filter on relationship if specified.
            if let Some(ref props) = rel_pat.properties {
                for (k, v) in props {
                    plan = LogicalOperator::Filter {
                        input: Box::new(plan),
                        expression: FilterExpr::Eq(
                            Expr::Prop(rel_var.clone(), k.clone()),
                            v.clone(),
                        ),
                    };
                }
            }

            // Filter target node by each of its labels.
            for label in &node_pat.labels {
                plan = LogicalOperator::Filter {
                    input: Box::new(plan),
                    expression: FilterExpr::HasLabel(target_var.clone(), label.clone()),
                };
            }

            // Apply inline properties filter on target node.
            if let Some(ref props) = node_pat.properties {
                for (k, v) in props {
                    plan = LogicalOperator::Filter {
                        input: Box::new(plan),
                        expression: FilterExpr::Eq(
                            Expr::Prop(target_var.clone(), k.clone()),
                            v.clone(),
                        ),
                    };
                }
            }

            prev_node_var = target_var;
        }

        if let Some(ref pv) = pattern.path_variable {
            let expr = if pattern.rels.is_empty() {
                Expr::FunctionCall {
                    name: "__path__".to_string(),
                    args: vec![Expr::Prop(seed_var.clone(), "".to_string())],
                }
            } else {
                let last_target_var = if let Some((_, last_node_pat)) = pattern.rels.last() {
                    last_node_pat
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("_target_{}", pattern.rels.len() - 1))
                } else {
                    seed_var.clone()
                };
                Expr::Prop(format!("_path_{}", last_target_var), "".to_string())
            };

            plan = LogicalOperator::Project {
                input: Box::new(plan),
                items: vec![(expr, Some(pv.clone()))],
                is_barrier: false,
            };
        }

        Ok(plan)
    }
}

/// Group-by item: `(expression, optional alias)`.
type GroupByItem = (Expr, Option<String>);
/// Aggregation spec: `(function, inner expression, output column name)`.
type AggItem = (AggFn, Expr, String);

/// Classify RETURN items into group-by keys (non-aggregate) and aggregation specs.
///
/// Returns `(group_by, aggregations)` where:
/// - `group_by` contains `(expr, alias)` pairs for non-aggregate items.
/// - `aggregations` contains `(agg_fn, inner_expr, output_name)` triples for aggregate items.
fn split_return_items(
    query: &Query,
) -> (Vec<GroupByItem>, Vec<AggItem>, Vec<crate::ast::ReturnItem>) {
    let mut group_by = Vec::new();
    let mut aggregations = Vec::new();
    let mut count_index = 0;

    let mut rewritten_items = query.return_clause.items.clone();
    for item in &mut rewritten_items {
        if expr_has_aggregation(&item.expr) {
            if item.alias.is_none() {
                item.alias = Some(expr_display_name(&item.expr));
            }
            extract_and_replace_aggs(
                &mut item.expr,
                &mut aggregations,
                &mut count_index,
                "_ret_agg_",
            );
        } else {
            group_by.push((item.expr.clone(), item.alias.clone()));
        }
    }

    (group_by, aggregations, rewritten_items)
}

/// Return true when an expression contains any aggregation function (CountStar
/// or Agg(...)) at any depth.
#[allow(dead_code)]
fn expr_has_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::CountStar | Expr::Agg(_, _) => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_aggregation(left) || expr_has_aggregation(right)
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_aggregation(inner)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_aggregation),
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            subject.as_ref().is_some_and(|s| expr_has_aggregation(s))
                || arms
                    .iter()
                    .any(|a| expr_has_aggregation(&a.when) || expr_has_aggregation(&a.then))
                || else_expr.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::Subscript { expr, index } => {
            expr_has_aggregation(expr) || expr_has_aggregation(index)
        }
        Expr::Slice { expr, start, end } => {
            expr_has_aggregation(expr)
                || start.as_ref().is_some_and(|s| expr_has_aggregation(s))
                || end.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            expr_has_aggregation(list)
                || predicate.as_ref().is_some_and(|p| expr_has_aggregation(p))
                || transform.as_ref().is_some_and(|t| expr_has_aggregation(t))
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            expr_has_aggregation(initial)
                || expr_has_aggregation(list)
                || expr_has_aggregation(expression)
        }
        Expr::Quantifier {
            list, predicate, ..
        } => expr_has_aggregation(list) || expr_has_aggregation(predicate),
        _ => false,
    }
}

/// Classify WITH items into group-by keys (non-aggregate) and aggregation specs.
///
/// Mirrors `split_return_items` but operates on a slice of `ReturnItem` rather than a `Query`.
/// When a WITH item contains a nested aggregation expression (e.g., `$age + avg(n.age) AS agg`),
/// the whole expression is treated as an aggregation so that the Aggregate operator is inserted.
fn split_with_items(
    items: &[crate::ast::ReturnItem],
) -> (Vec<GroupByItem>, Vec<AggItem>, Vec<crate::ast::ReturnItem>) {
    let mut group_by = Vec::new();
    let mut aggregations = Vec::new();
    let mut count_index = 0;

    let mut rewritten_items = items.to_vec();
    for item in &mut rewritten_items {
        if expr_has_aggregation(&item.expr) {
            if item.alias.is_none() {
                item.alias = Some(expr_display_name(&item.expr));
            }
            extract_and_replace_aggs(
                &mut item.expr,
                &mut aggregations,
                &mut count_index,
                "_ret_agg_",
            );
        } else {
            group_by.push((item.expr.clone(), item.alias.clone()));
        }
    }

    (group_by, aggregations, rewritten_items)
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
pub(crate) fn validate_skip_limit(expr: &Expr, keyword: &str) -> Result<(), CypherError> {
    match expr {
        Expr::Literal(crate::ast::Literal::Int(n)) => {
            if *n < 0 {
                Err(CypherError::Plan(format!(
                    "SyntaxError: {} value must not be negative, got {}",
                    keyword, n
                )))
            } else {
                Ok(())
            }
        }
        Expr::Literal(crate::ast::Literal::Float(_)) => Err(CypherError::Plan(format!(
            "SyntaxError: {} value must be an integer, not a float",
            keyword
        ))),
        Expr::Param(_) => {
            // Parameter-based SKIP/LIMIT is allowed in Cypher; validation happens at runtime.
            Ok(())
        }
        _ => Err(CypherError::Plan(format!(
            "SyntaxError: {} value must be a constant integer expression",
            keyword
        ))),
    }
}

fn extract_and_replace_aggs(
    expr: &mut Expr,
    aggregations: &mut Vec<AggItem>,
    count_index: &mut usize,
    prefix: &str,
) {
    match expr {
        Expr::CountStar => {
            let mut found_col = None;
            for (fn_, inner, col) in aggregations.iter() {
                if matches!(fn_, AggFn::Count { distinct: false })
                    && matches!(inner, Expr::CountStar)
                {
                    found_col = Some(col.clone());
                    break;
                }
            }
            let col = found_col.unwrap_or_else(|| {
                let name = format!("{}{}", prefix, *count_index);
                *count_index += 1;
                aggregations.push((
                    AggFn::Count { distinct: false },
                    Expr::CountStar,
                    name.clone(),
                ));
                name
            });
            *expr = Expr::Prop(col, "".to_string());
        }
        Expr::Agg(fn_, inner) => {
            let mut found_col = None;
            for (f, i, col) in aggregations.iter() {
                if f == fn_ && i == inner.as_ref() {
                    found_col = Some(col.clone());
                    break;
                }
            }
            let col = found_col.unwrap_or_else(|| {
                let name = format!("{}{}", prefix, *count_index);
                *count_index += 1;
                aggregations.push((fn_.clone(), *inner.clone(), name.clone()));
                name
            });
            *expr = Expr::Prop(col, "".to_string());
        }
        Expr::BinaryOp { left, right, .. } => {
            extract_and_replace_aggs(left, aggregations, count_index, prefix);
            extract_and_replace_aggs(right, aggregations, count_index, prefix);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            extract_and_replace_aggs(inner, aggregations, count_index, prefix);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                extract_and_replace_aggs(arg, aggregations, count_index, prefix);
            }
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                extract_and_replace_aggs(s, aggregations, count_index, prefix);
            }
            for arm in arms {
                extract_and_replace_aggs(&mut arm.when, aggregations, count_index, prefix);
                extract_and_replace_aggs(&mut arm.then, aggregations, count_index, prefix);
            }
            if let Some(e) = else_expr {
                extract_and_replace_aggs(e, aggregations, count_index, prefix);
            }
        }
        Expr::Subscript { expr, index } => {
            extract_and_replace_aggs(expr, aggregations, count_index, prefix);
            extract_and_replace_aggs(index, aggregations, count_index, prefix);
        }
        Expr::Slice { expr, start, end } => {
            extract_and_replace_aggs(expr, aggregations, count_index, prefix);
            if let Some(s) = start {
                extract_and_replace_aggs(s, aggregations, count_index, prefix);
            }
            if let Some(e) = end {
                extract_and_replace_aggs(e, aggregations, count_index, prefix);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            extract_and_replace_aggs(list, aggregations, count_index, prefix);
            if let Some(p) = predicate {
                extract_and_replace_aggs(p, aggregations, count_index, prefix);
            }
            if let Some(t) = transform {
                extract_and_replace_aggs(t, aggregations, count_index, prefix);
            }
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            extract_and_replace_aggs(initial, aggregations, count_index, prefix);
            extract_and_replace_aggs(list, aggregations, count_index, prefix);
            extract_and_replace_aggs(expression, aggregations, count_index, prefix);
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            extract_and_replace_aggs(list, aggregations, count_index, prefix);
            extract_and_replace_aggs(predicate, aggregations, count_index, prefix);
        }
        _ => {}
    }
}

fn collect_ord_aggs(expr: &Expr, set: &mut std::collections::HashSet<String>) {
    match expr {
        Expr::Prop(col, prop) => {
            if (col.starts_with("_ord_agg_") || col.starts_with("_ret_agg_")) && prop.is_empty() {
                set.insert(col.clone());
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_ord_aggs(left, set);
            collect_ord_aggs(right, set);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_ord_aggs(inner, set);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_ord_aggs(arg, set);
            }
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                collect_ord_aggs(s, set);
            }
            for arm in arms {
                collect_ord_aggs(&arm.when, set);
                collect_ord_aggs(&arm.then, set);
            }
            if let Some(e) = else_expr {
                collect_ord_aggs(e, set);
            }
        }
        Expr::Subscript { expr, index } => {
            collect_ord_aggs(expr, set);
            collect_ord_aggs(index, set);
        }
        Expr::Slice { expr, start, end } => {
            collect_ord_aggs(expr, set);
            if let Some(s) = start {
                collect_ord_aggs(s, set);
            }
            if let Some(e) = end {
                collect_ord_aggs(e, set);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            collect_ord_aggs(list, set);
            if let Some(p) = predicate {
                collect_ord_aggs(p, set);
            }
            if let Some(t) = transform {
                collect_ord_aggs(t, set);
            }
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            collect_ord_aggs(initial, set);
            collect_ord_aggs(list, set);
            collect_ord_aggs(expression, set);
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            collect_ord_aggs(list, set);
            collect_ord_aggs(predicate, set);
        }
        _ => {}
    }
}

fn rewrite_expr_with_aliases(expr: &mut Expr, projections: &[(Expr, String)]) {
    // Check if the current expression matches any projected source expression exactly.
    for (source_expr, target_var) in projections {
        if expr == source_expr {
            *expr = Expr::Prop(target_var.clone(), "".to_string());
            return;
        }
    }

    // Otherwise, recursively rewrite child expressions.
    match expr {
        Expr::Prop(_, _) | Expr::Literal(_) | Expr::Param(_) | Expr::CountStar => {}
        Expr::Agg(_, inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) | Expr::Not(inner) => {
            rewrite_expr_with_aliases(inner, projections);
        }
        Expr::BinaryOp { left, right, .. } => {
            rewrite_expr_with_aliases(left, projections);
            rewrite_expr_with_aliases(right, projections);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                rewrite_expr_with_aliases(arg, projections);
            }
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            rewrite_expr_with_aliases(list, projections);
            rewrite_expr_with_aliases(predicate, projections);
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                rewrite_expr_with_aliases(s, projections);
            }
            for arm in arms {
                rewrite_expr_with_aliases(&mut arm.when, projections);
                rewrite_expr_with_aliases(&mut arm.then, projections);
            }
            if let Some(e) = else_expr {
                rewrite_expr_with_aliases(e, projections);
            }
        }
        Expr::Subscript { expr: inner, index } => {
            rewrite_expr_with_aliases(inner, projections);
            rewrite_expr_with_aliases(index, projections);
        }
        Expr::Slice {
            expr: inner,
            start,
            end,
        } => {
            rewrite_expr_with_aliases(inner, projections);
            if let Some(s) = start {
                rewrite_expr_with_aliases(s, projections);
            }
            if let Some(e) = end {
                rewrite_expr_with_aliases(e, projections);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            rewrite_expr_with_aliases(list, projections);
            if let Some(p) = predicate {
                rewrite_expr_with_aliases(p, projections);
            }
            if let Some(t) = transform {
                rewrite_expr_with_aliases(t, projections);
            }
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            rewrite_expr_with_aliases(initial, projections);
            rewrite_expr_with_aliases(list, projections);
            rewrite_expr_with_aliases(expression, projections);
        }
        Expr::PatternComprehension {
            predicate,
            transform,
            ..
        } => {
            if let Some(p) = predicate {
                rewrite_expr_with_aliases(p, projections);
            }
            rewrite_expr_with_aliases(transform, projections);
        }
        Expr::HasLabel { .. } => {}
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
