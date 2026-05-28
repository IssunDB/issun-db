use super::expr::*;
use super::*;

/// Expand from a set of source nodes, handling pipe-separated OR relationship types.
///
/// If `rel_type` is `Some("A|B|C")`, this expands separately for each type and
/// merges the results, deduplicating by `(EdgeId, NodeId)` pair.
fn expand_multi_type(
    graph: &Graph,
    src_nodes: &[NodeId],
    rel_type: Option<&str>,
    is_incoming: bool,
) -> Result<Vec<(NodeId, EdgeId, NodeId)>, String> {
    match rel_type {
        None => graph
            .expand_spmv_graphblas(src_nodes, None, is_incoming)
            .map_err(|e| e.to_string()),
        Some(t) if !t.contains('|') => graph
            .expand_spmv_graphblas(src_nodes, Some(t), is_incoming)
            .map_err(|e| e.to_string()),
        Some(t) => {
            let mut seen: std::collections::HashSet<(NodeId, EdgeId, NodeId)> =
                std::collections::HashSet::new();
            let mut all = Vec::new();
            for part in t.split('|') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                let partial = graph
                    .expand_spmv_graphblas(src_nodes, Some(part), is_incoming)
                    .map_err(|e| e.to_string())?;
                for triple in partial {
                    if seen.insert(triple) {
                        all.push(triple);
                    }
                }
            }
            Ok(all)
        }
    }
}

pub(super) fn execute_read_query(
    graph: &Graph,
    query: &Query,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // 1. Compile query AST into an optimized physical plan
    let logical = LogicalPlanner::plan(query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));

    // 2. Execute the optimized physical operator tree recursively.
    //    The top-level `PhysicalOperator::Project` in the plan has already
    //    materialized all projected values into the PathMap under their canonical
    //    column-name keys. Reading by key here avoids a second evaluation of the
    //    same expressions (double-projection) against a PathMap that no longer
    //    contains the pre-projection variable names.
    let resolved_paths = execute_physical(graph, &optimized, params)?;

    // Check whether the RETURN clause is RETURN * (the __star__ sentinel).
    let is_return_star = query.return_clause.items.len() == 1
        && matches!(
            &query.return_clause.items[0].expr,
            Expr::FunctionCall { name, .. } if name == "__star__"
        );

    // 3. Derive column names. For RETURN *, use all keys from the first resolved path.
    let columns: Vec<String> = if is_return_star {
        // Collect and sort keys from the first path for deterministic column ordering.
        let mut keys: Vec<String> = resolved_paths
            .first()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    } else {
        query
            .return_clause
            .items
            .iter()
            .map(|item| projected_key(&item.expr, &item.alias))
            .collect()
    };

    // 4. Read each projected value directly from the PathMap by its canonical key.
    let mut records = Vec::new();
    for path in resolved_paths {
        let mut values = Vec::new();
        if is_return_star {
            for key in &columns {
                values.push(binding_to_value(graph, path.get(key))?);
            }
        } else {
            for item in &query.return_clause.items {
                let key = projected_key(&item.expr, &item.alias);
                values.push(binding_to_value(graph, path.get(&key))?);
            }
        }
        records.push(Record { values });
    }

    // 5. Apply RETURN DISTINCT deduplication if requested.
    if query.return_clause.distinct {
        let mut seen = std::collections::HashSet::new();
        records.retain(|r| {
            let key = serde_json::to_string(&r.values).unwrap_or_default();
            seen.insert(key)
        });
    }

    Ok(QueryResult { columns, records })
}

/// Compute the key under which a RETURN/WITH item is stored in the projected PathMap.
///
/// Must exactly match the key-naming logic in the `PhysicalOperator::Project` arm of
/// `execute_physical` so that `execute_read_query` can look up pre-materialized values
/// by key rather than re-evaluating expressions.
pub(super) fn projected_key(expr: &Expr, alias: &Option<String>) -> String {
    if let Some(a) = alias {
        a.clone()
    } else {
        expr_display_name(expr)
    }
}

/// Return a human-readable name for an expression, used as the default column name
/// when no alias is specified in a RETURN or WITH clause.
pub(super) fn expr_display_name(expr: &Expr) -> String {
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
        Expr::Agg(fn_, inner) => {
            let inner_name = expr_display_name(inner);
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
        Expr::Case { .. } => "case".to_string(),
        Expr::IsNull(inner) => format!("{} IS NULL", expr_display_name(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_display_name(inner)),
        Expr::Not(inner) => format!("NOT {}", expr_display_name(inner)),
        Expr::Subscript { expr, index } => {
            // Use dot notation when the index is a string literal (represents property access).
            if let Expr::Literal(Literal::Str(prop)) = index.as_ref() {
                let base = expr_display_name(expr);
                // Wrap base in parens if it contains brackets.
                if base.contains('[') || base.contains('(') {
                    return format!("({}).{}", base, prop);
                }
                return format!("{}.{}", base, prop);
            }
            format!("{}[{}]", expr_display_name(expr), expr_display_name(index))
        }
        Expr::Slice { expr, start, end } => {
            let s = start
                .as_ref()
                .map(|e| expr_display_name(e))
                .unwrap_or_default();
            let e = end
                .as_ref()
                .map(|e| expr_display_name(e))
                .unwrap_or_default();
            format!("{}[{}..{}]", expr_display_name(expr), s, e)
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            transform,
        } => {
            let mut s = format!("[{} IN {}", variable, expr_display_name(list));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE {}", expr_display_name(p)));
            }
            if let Some(t) = transform {
                s.push_str(&format!(" | {}", expr_display_name(t)));
            }
            s.push(']');
            s
        }
        Expr::HasLabel { variable, label } => format!("({}:{})", variable, label),
        Expr::BinaryOp { op, left, right } => {
            let op_str = match op {
                BinaryOperator::Eq => "=",
                BinaryOperator::Ne => "<>",
                BinaryOperator::Lt => "<",
                BinaryOperator::Gt => ">",
                BinaryOperator::Le => "<=",
                BinaryOperator::Ge => ">=",
                BinaryOperator::And => "AND",
                BinaryOperator::Or => "OR",
                BinaryOperator::Xor => "XOR",
                BinaryOperator::Pow => "^",
                BinaryOperator::Add => "+",
                BinaryOperator::Sub => "-",
                BinaryOperator::Mul => "*",
                BinaryOperator::Div => "/",
                BinaryOperator::Mod => "%",
            };
            format!(
                "{} {} {}",
                expr_display_name(left),
                op_str,
                expr_display_name(right)
            )
        }
        Expr::FunctionCall { name, args } => {
            // Special internal functions get display names matching source syntax.
            if name == "__list__" {
                let args_str: Vec<String> = args.iter().map(expr_display_name).collect();
                return format!("[{}]", args_str.join(", "));
            }
            if name == "__map__" {
                // Args are alternating key, value pairs.
                let mut parts = Vec::new();
                let mut i = 0;
                while i + 1 < args.len() {
                    let k = expr_display_name(&args[i]);
                    let v = expr_display_name(&args[i + 1]);
                    // Keys are stored as Literal::Str, display without quotes.
                    let key = match &args[i] {
                        Expr::Literal(Literal::Str(s)) => s.clone(),
                        _ => k,
                    };
                    parts.push(format!("{}: {}", key, v));
                    i += 2;
                }
                return format!("{{{}}}", parts.join(", "));
            }
            let args_str: Vec<String> = args.iter().map(expr_display_name).collect();
            // Use canonical display names for known functions.
            let display_name = canonical_function_name(name);
            format!("{}({})", display_name, args_str.join(", "))
        }
        Expr::Quantifier {
            kind,
            variable,
            list,
            predicate,
        } => {
            let kind_str = match kind {
                QuantifierKind::All => "all",
                QuantifierKind::Any => "any",
                QuantifierKind::None => "none",
                QuantifierKind::Single => "single",
            };
            format!(
                "{}({} IN {} WHERE {})",
                kind_str,
                variable,
                expr_display_name(list),
                expr_display_name(predicate)
            )
        }
    }
}

/// Convert a `GraphBinding` entry from a projected `PathMap` into a JSON value.
///
/// `Node` and `Edge` bindings that survive projection (e.g., `WITH n RETURN n`) are
/// resolved by fetching the stored property blob from the graph.
pub(super) fn binding_to_value(
    graph: &Graph,
    binding: Option<&GraphBinding>,
) -> Result<serde_json::Value, String> {
    match binding {
        None => Ok(serde_json::Value::Null),
        Some(GraphBinding::Scalar(v)) => Ok(v.clone()),
        Some(GraphBinding::Node(id)) => {
            let record = graph
                .get_node(*id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node not found: {}", id))?;
            rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())
        }
        Some(GraphBinding::Edge(id)) => {
            let record = graph
                .get_edge(*id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("edge not found: {}", id))?;
            rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())
        }
    }
}

pub(super) fn execute_physical(
    graph: &Graph,
    op: &PhysicalOperator,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<PathMap>, String> {
    match op {
        PhysicalOperator::NodeIndexScan {
            variable,
            label,
            property,
            value,
        } => {
            let val = evaluate_expr(graph, &PathMap::new(), value, params)?;
            let prop_val = json_to_prop_value(&val)
                .ok_or_else(|| format!("unsupported property value type for index scan: {val}"))?;
            let candidates = graph
                .nodes_by_property(label, property, prop_val)
                .map_err(|e| e.to_string())?;

            Ok(candidates
                .into_iter()
                .map(|cand| {
                    let mut path = PathMap::new();
                    path.insert(variable.clone(), GraphBinding::Node(cand));
                    path
                })
                .collect())
        }
        PhysicalOperator::LabelScan { variable, label } => {
            let candidates = if let Some(lbl) = label {
                graph.nodes_by_label(lbl).map_err(|e| e.to_string())?
            } else {
                graph.all_nodes().map_err(|e| e.to_string())?
            };

            Ok(candidates
                .into_iter()
                .map(|cand| {
                    let mut path = PathMap::new();
                    path.insert(variable.clone(), GraphBinding::Node(cand));
                    path
                })
                .collect())
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
            let child_paths = execute_physical(graph, input, params)?;
            let mut next_paths = Vec::new();

            // Collect unique source node IDs across all child rows for bulk expansion.
            let mut src_nodes: Vec<NodeId> = child_paths
                .iter()
                .filter_map(|p| match p.get(src_var) {
                    Some(GraphBinding::Node(n)) => Some(*n),
                    _ => None,
                })
                .collect();
            src_nodes.sort_unstable();
            src_nodes.dedup();

            // Build the set of directions to traverse. For undirected patterns both
            // outgoing (is_incoming=false) and incoming (is_incoming=true) are traversed,
            // and results are deduplicated by (edge_id, dst_node) key.
            let directions: &[bool] = if *is_undirected {
                &[false, true]
            } else {
                std::slice::from_ref(is_incoming)
            };

            // Bulk single-hop expansion for all required directions.
            let mut transition_map: HashMap<NodeId, Vec<(EdgeId, NodeId)>> = HashMap::new();
            for &dir in directions {
                let transitions =
                    expand_multi_type(graph, &src_nodes, rel_type.as_deref(), dir)?;
                for (src, eid, dst) in transitions {
                    let entry = transition_map.entry(src).or_default();
                    if !entry.iter().any(|&(e, d)| e == eid && d == dst) {
                        entry.push((eid, dst));
                    }
                }
            }

            for path in child_paths {
                let src_node = match path.get(src_var) {
                    Some(GraphBinding::Node(n)) => *n,
                    _ => continue,
                };

                if *min_hops == 1 && *max_hops == 1 {
                    // Single-hop: use the pre-built transition_map and bind rel_var to
                    // the actual EdgeId.
                    if let Some(dests) = transition_map.get(&src_node) {
                        for &(eid, dst_node) in dests {
                            let mut new_path = path.clone();
                            new_path.insert(rel_var.clone(), GraphBinding::Edge(eid));
                            if new_path
                                .insert(dst_var.clone(), GraphBinding::Node(dst_node))
                                .is_some_and(|existing| existing != GraphBinding::Node(dst_node))
                            {
                                continue;
                            }
                            next_paths.push(new_path);
                        }
                    }
                } else {
                    // Variable-length BFS. Each BFS state tracks visited nodes to
                    // prevent cycles within a single path. The BFS runs unconditionally
                    // (not gated on transition_map) so that min_hops=0 patterns correctly
                    // include src_node itself even when it has no outgoing edges.
                    let mut queue = vec![(src_node, vec![src_node])];
                    let mut completed_targets: HashSet<NodeId> = HashSet::new();

                    // A zero-hop path binds src_node to dst_var when min_hops == 0.
                    if *min_hops == 0 {
                        completed_targets.insert(src_node);
                    }

                    for hop in 1..=*max_hops {
                        let mut next_queue = Vec::new();
                        for (node, path_nodes) in queue {
                            for &dir in directions {
                                let neighbors =
                                    expand_multi_type(graph, &[node], rel_type.as_deref(), dir)?;

                                for (_, _, neigh_node) in neighbors {
                                    if path_nodes.contains(&neigh_node) {
                                        continue;
                                    }
                                    let mut next_path_nodes = path_nodes.clone();
                                    next_path_nodes.push(neigh_node);

                                    if hop >= *min_hops {
                                        completed_targets.insert(neigh_node);
                                    }
                                    if !next_queue.iter().any(|(n, _)| *n == neigh_node) {
                                        next_queue.push((neigh_node, next_path_nodes));
                                    }
                                }
                            }
                        }
                        queue = next_queue;
                        if queue.is_empty() {
                            break;
                        }
                    }

                    // Variable-length paths traverse multiple edges; rel_var is not
                    // bound to a single EdgeId (only dst_var is bound).
                    for neigh_node in completed_targets {
                        let mut new_path = path.clone();
                        if new_path
                            .insert(dst_var.clone(), GraphBinding::Node(neigh_node))
                            .is_some_and(|existing| existing != GraphBinding::Node(neigh_node))
                        {
                            continue;
                        }
                        next_paths.push(new_path);
                    }
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::Filter { input, expression } => {
            let child_paths = execute_physical(graph, input, params)?;
            let mut next_paths = Vec::new();

            if let FilterExpr::HasLabel(variable, label) = expression {
                // Collect the distinct node IDs bound to this variable for bulk label filtering.
                let mut active_nodes: Vec<NodeId> = child_paths
                    .iter()
                    .filter_map(|p| match p.get(variable) {
                        Some(GraphBinding::Node(n)) => Some(*n),
                        _ => None,
                    })
                    .collect();
                active_nodes.sort_unstable();
                active_nodes.dedup();

                let filtered_nodes = graph
                    .label_filter_and_graphblas(&active_nodes, label)
                    .map_err(|e| e.to_string())?;
                let filtered_set: HashSet<NodeId> = filtered_nodes.into_iter().collect();

                for path in child_paths {
                    if let Some(GraphBinding::Node(node)) = path.get(variable) {
                        if filtered_set.contains(node) {
                            next_paths.push(path);
                        }
                    }
                }
            } else {
                let where_clause = match expression {
                    FilterExpr::Eq(l, r) => WhereClause::Eq(l.clone(), r.clone()),
                    FilterExpr::Ne(l, r) => WhereClause::Ne(l.clone(), r.clone()),
                    FilterExpr::Lt(l, r) => WhereClause::Lt(l.clone(), r.clone()),
                    FilterExpr::Gt(l, r) => WhereClause::Gt(l.clone(), r.clone()),
                    FilterExpr::Le(l, r) => WhereClause::Le(l.clone(), r.clone()),
                    FilterExpr::Ge(l, r) => WhereClause::Ge(l.clone(), r.clone()),
                    FilterExpr::HasLabel(_, _) => unreachable!(),
                    FilterExpr::Expr(e) => WhereClause::Expr(e.clone()),
                };

                for path in child_paths {
                    if evaluate_where(graph, &path, &where_clause, params)? {
                        next_paths.push(path);
                    }
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::HashJoin { left, right } => {
            let left_paths = execute_physical(graph, left, params)?;
            let right_paths = execute_physical(graph, right, params)?;

            // Determine common variables by sampling the first row of each side.
            // All operators produce uniform-schema rows (every row from a given subtree
            // carries the same key set), so sampling row 0 is sufficient.
            let left_vars: HashSet<String> = left_paths
                .first()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            let right_vars: HashSet<String> = right_paths
                .first()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();

            let common_vars: Vec<String> = left_vars.intersection(&right_vars).cloned().collect();

            let mut next_paths = Vec::new();

            if common_vars.is_empty() {
                // Independent MATCH clauses: emit the Cartesian product.
                for lp in &left_paths {
                    for rp in &right_paths {
                        let mut merged = lp.clone();
                        merged.extend(rp.iter().map(|(k, v)| (k.clone(), v.clone())));
                        next_paths.push(merged);
                    }
                }
            } else {
                // Equi-join on shared variables. Build a hash table from the right side,
                // then probe with each left row.
                let mut hash_table: HashMap<Vec<GraphBinding>, Vec<PathMap>> = HashMap::new();
                for rp in right_paths {
                    // skip rows missing any common variable (should not occur for uniform-schema
                    // rows, but avoids a panic if an upstream operator ever produces sparse rows).
                    let key: Option<Vec<GraphBinding>> =
                        common_vars.iter().map(|v| rp.get(v).cloned()).collect();
                    if let Some(key) = key {
                        hash_table.entry(key).or_default().push(rp);
                    }
                }

                for lp in left_paths {
                    let key: Option<Vec<GraphBinding>> =
                        common_vars.iter().map(|v| lp.get(v).cloned()).collect();
                    if let Some(key) = key {
                        if let Some(matches) = hash_table.get(&key) {
                            for rp in matches {
                                let mut merged = lp.clone();
                                merged.extend(rp.iter().map(|(k, v)| (k.clone(), v.clone())));
                                next_paths.push(merged);
                            }
                        }
                    }
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::SingleRow => Ok(vec![PathMap::new()]),
        PhysicalOperator::Unwind {
            input,
            expr,
            variable,
        } => {
            let child_paths = execute_physical(graph, input, params)?;
            let mut next_paths = Vec::new();

            for path in child_paths {
                let list_val = evaluate_expr(graph, &path, expr, params)?;
                if let serde_json::Value::Array(items) = list_val {
                    for item in items {
                        let mut new_path = path.clone();
                        // Always bind as a Scalar: the integer values in a list literal
                        // (e.g., [1, 2, 3]) are plain data, not NodeId references.
                        new_path.insert(variable.clone(), GraphBinding::Scalar(item));
                        next_paths.push(new_path);
                    }
                } else if list_val != serde_json::Value::Null {
                    let mut new_path = path.clone();
                    new_path.insert(variable.clone(), GraphBinding::Scalar(list_val));
                    next_paths.push(new_path);
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::Project {
            input,
            items,
            is_barrier: _,
        } => {
            let child_paths = execute_physical(graph, input, params)?;
            let mut next_paths = Vec::new();

            for path in child_paths {
                // RETURN * / WITH * passes all current bindings through unchanged.
                let is_star = items.len() == 1
                    && matches!(
                        &items[0].0,
                        Expr::FunctionCall { name, .. } if name == "__star__"
                    );
                if is_star {
                    next_paths.push(path);
                    continue;
                }

                let mut projected_path = PathMap::new();

                for (expr, alias) in items {
                    let target_var = if let Some(alias_name) = alias {
                        alias_name.clone()
                    } else {
                        expr_display_name(expr)
                    };

                    match expr {
                        // For CountStar / Agg, the Aggregate operator has already placed
                        // the computed value in the PathMap under `target_var`. Pull it
                        // directly rather than trying to re-evaluate the expression.
                        Expr::CountStar | Expr::Agg(_, _) => {
                            if let Some(binding) = path.get(&target_var) {
                                projected_path.insert(target_var, binding.clone());
                            } else {
                                projected_path.insert(
                                    target_var,
                                    GraphBinding::Scalar(serde_json::Value::Null),
                                );
                            }
                        }
                        Expr::Prop(var, prop) if prop.is_empty() => {
                            // Whole-variable reference: first try the node binding,
                            // then fall back to a scalar already in the PathMap
                            // (e.g., a group-by column emitted by Aggregate).
                            if let Some(binding) = path.get(var) {
                                projected_path.insert(target_var, binding.clone());
                            } else if let Some(binding) = path.get(&target_var) {
                                projected_path.insert(target_var, binding.clone());
                            }
                        }
                        _ => {
                            // For property expressions (n.age), first check whether the
                            // Aggregate already emitted a scalar under the target column
                            // name (e.g., the group-by key alias). If so, reuse it.
                            // Exception: if the existing binding is a Node/Edge (not a Scalar),
                            // and there's an alias, always evaluate the expression to avoid
                            // shadowing issues like `a.id AS a` where a = Node(...).
                            if let Some(binding) = path.get(&target_var) {
                                match binding {
                                    GraphBinding::Scalar(_) => {
                                        // Pre-computed scalar (from Aggregate) → reuse.
                                        projected_path.insert(target_var, binding.clone());
                                    }
                                    GraphBinding::Node(_) | GraphBinding::Edge(_) => {
                                        // Node/Edge with a matching key: only reuse if there's
                                        // no alias (variable pass-through).
                                        if alias.is_none() {
                                            projected_path.insert(target_var, binding.clone());
                                        } else {
                                            let val = evaluate_expr(graph, &path, expr, params)?;
                                            projected_path
                                                .insert(target_var, GraphBinding::Scalar(val));
                                        }
                                    }
                                }
                            } else {
                                let val = evaluate_expr(graph, &path, expr, params)?;
                                projected_path.insert(target_var, GraphBinding::Scalar(val));
                            }
                        }
                    }
                }

                next_paths.push(projected_path);
            }

            Ok(next_paths)
        }
        PhysicalOperator::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            use std::collections::BTreeMap;

            let child_paths = execute_physical(graph, input, params)?;

            struct AggState {
                count: i64,
                sum: f64,
                min: Option<serde_json::Value>,
                max: Option<serde_json::Value>,
                collect: Vec<serde_json::Value>,
                distinct_seen: std::collections::HashSet<String>,
                /// Accumulated numeric values for stDev, stDevP, percentile functions.
                values: Vec<f64>,
            }
            impl AggState {
                fn new() -> Self {
                    Self {
                        count: 0,
                        sum: 0.0,
                        min: None,
                        max: None,
                        collect: Vec::new(),
                        distinct_seen: std::collections::HashSet::new(),
                        values: Vec::new(),
                    }
                }
            }

            // group_key -> (group-by PathMap, per-aggregation state Vec)
            let mut groups: BTreeMap<String, (PathMap, Vec<AggState>)> = BTreeMap::new();

            for path in child_paths {
                let mut key_parts = Vec::new();
                let mut gb_path = PathMap::new();
                for (expr, alias) in group_by {
                    let val = evaluate_expr(graph, &path, expr, params)?;
                    let col = if let Some(a) = alias {
                        a.clone()
                    } else {
                        match expr {
                            Expr::Prop(var, prop) if prop.is_empty() => var.clone(),
                            Expr::Prop(var, prop) => format!("{}.{}", var, prop),
                            Expr::Literal(lit) => lit.to_string(),
                            Expr::Param(p) => format!("${}", p),
                            other => expr_display_name(other),
                        }
                    };
                    key_parts.push(val.to_string());
                    gb_path.insert(col, GraphBinding::Scalar(val));
                }
                let group_key = key_parts.join("\x00");

                let entry = groups.entry(group_key).or_insert_with(|| {
                    let states = aggregations.iter().map(|_| AggState::new()).collect();
                    (gb_path, states)
                });

                for (i, (agg_fn, inner_expr, _col)) in aggregations.iter().enumerate() {
                    let state = &mut entry.1[i];
                    match agg_fn {
                        AggFn::Count { distinct } => {
                            if matches!(inner_expr, Expr::CountStar) {
                                state.count += 1;
                            } else {
                                let val = evaluate_expr(graph, &path, inner_expr, params)?;
                                if val != serde_json::Value::Null {
                                    if *distinct {
                                        if state.distinct_seen.insert(val.to_string()) {
                                            state.count += 1;
                                        }
                                    } else {
                                        state.count += 1;
                                    }
                                }
                            }
                        }
                        AggFn::Sum { distinct } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                if *distinct && !state.distinct_seen.insert(val.to_string()) {
                                    // already seen, skip
                                } else if let Some(n) = val.as_f64() {
                                    state.sum += n;
                                }
                            }
                        }
                        AggFn::Avg { distinct } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                if *distinct && !state.distinct_seen.insert(val.to_string()) {
                                    // already seen, skip
                                } else if let Some(n) = val.as_f64() {
                                    state.sum += n;
                                    state.count += 1;
                                }
                            }
                        }
                        AggFn::Min { .. } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                state.min = Some(match state.min.take() {
                                    None => val,
                                    Some(prev) => {
                                        if json_cmp_total(&val, &prev) == std::cmp::Ordering::Less {
                                            val
                                        } else {
                                            prev
                                        }
                                    }
                                });
                            }
                        }
                        AggFn::Max { .. } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                state.max = Some(match state.max.take() {
                                    None => val,
                                    Some(prev) => {
                                        if json_cmp_total(&val, &prev)
                                            == std::cmp::Ordering::Greater
                                        {
                                            val
                                        } else {
                                            prev
                                        }
                                    }
                                });
                            }
                        }
                        AggFn::Collect { distinct } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                if *distinct && !state.distinct_seen.insert(val.to_string()) {
                                    // already seen, skip
                                } else {
                                    state.collect.push(val);
                                }
                            }
                        }
                        AggFn::StDev { .. } | AggFn::StDevP { .. } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if let Some(n) = val.as_f64() {
                                state.values.push(n);
                            }
                        }
                        AggFn::PercentileDisc { .. } | AggFn::PercentileCont { .. } => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if let Some(n) = val.as_f64() {
                                state.values.push(n);
                            }
                        }
                    }
                }
            }

            let mut result = Vec::new();
            for (_key, (mut gb_path, states)) in groups {
                for (i, (agg_fn, _inner, col)) in aggregations.iter().enumerate() {
                    let state = &states[i];
                    let agg_val = match agg_fn {
                        AggFn::Count { .. } => serde_json::Value::Number(state.count.into()),
                        AggFn::Sum { .. } => serde_json::Number::from_f64(state.sum)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                        AggFn::Avg { .. } => {
                            if state.count == 0 {
                                serde_json::Value::Null
                            } else {
                                let avg = state.sum / state.count as f64;
                                serde_json::Number::from_f64(avg)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        AggFn::Min { .. } => state.min.clone().unwrap_or(serde_json::Value::Null),
                        AggFn::Max { .. } => state.max.clone().unwrap_or(serde_json::Value::Null),
                        AggFn::Collect { .. } => serde_json::Value::Array(state.collect.clone()),
                        AggFn::StDev { .. } => {
                            let n = state.values.len();
                            if n < 2 {
                                serde_json::Value::Number(
                                    serde_json::Number::from_f64(0.0)
                                        .unwrap_or_else(|| serde_json::Number::from(0)),
                                )
                            } else {
                                let mean = state.values.iter().sum::<f64>() / n as f64;
                                let variance =
                                    state.values.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                                        / (n - 1) as f64;
                                serde_json::Number::from_f64(variance.sqrt())
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        AggFn::StDevP { .. } => {
                            let n = state.values.len();
                            if n == 0 {
                                serde_json::Value::Number(
                                    serde_json::Number::from_f64(0.0)
                                        .unwrap_or_else(|| serde_json::Number::from(0)),
                                )
                            } else {
                                let mean = state.values.iter().sum::<f64>() / n as f64;
                                let variance =
                                    state.values.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                                        / n as f64;
                                serde_json::Number::from_f64(variance.sqrt())
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        AggFn::PercentileDisc { percentile } => {
                            let mut sorted = state.values.clone();
                            sorted.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let n = sorted.len();
                            if n == 0 {
                                serde_json::Value::Null
                            } else {
                                let idx = ((percentile * n as f64).ceil() as usize)
                                    .saturating_sub(1)
                                    .min(n - 1);
                                serde_json::Number::from_f64(sorted[idx])
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        AggFn::PercentileCont { percentile } => {
                            let mut sorted = state.values.clone();
                            sorted.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let n = sorted.len();
                            if n == 0 {
                                serde_json::Value::Null
                            } else {
                                let rank = percentile * (n - 1) as f64;
                                let lower = rank.floor() as usize;
                                let upper = rank.ceil() as usize;
                                let frac = rank - lower as f64;
                                let val = sorted[lower]
                                    + frac * (sorted[upper.min(n - 1)] - sorted[lower]);
                                serde_json::Number::from_f64(val)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                    };
                    gb_path.insert(col.clone(), GraphBinding::Scalar(agg_val));
                }
                result.push(gb_path);
            }

            Ok(result)
        }
        PhysicalOperator::Sort { input, items } => {
            let mut child_paths = execute_physical(graph, input, params)?;

            let mut keyed: Vec<(Vec<serde_json::Value>, PathMap)> = child_paths
                .drain(..)
                .map(|path| {
                    let keys: Vec<serde_json::Value> = items
                        .iter()
                        .map(|si| evaluate_sort_key(graph, &path, &si.expr, params))
                        .collect();
                    (keys, path)
                })
                .collect();

            keyed.sort_by(|(ka, _), (kb, _)| {
                for (i, si) in items.iter().enumerate() {
                    let ord = json_cmp(&ka[i], &kb[i]).unwrap_or(std::cmp::Ordering::Equal);
                    let ord = if si.ascending { ord } else { ord.reverse() };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });

            Ok(keyed.into_iter().map(|(_, path)| path).collect())
        }
        PhysicalOperator::Limit { input, skip, count } => {
            // Validate skip and count.
            if *skip > 1_000_000_000 {
                return Err(format!("SKIP value too large: {}", skip));
            }
            let child_paths = execute_physical(graph, input, params)?;
            Ok(child_paths.into_iter().skip(*skip).take(*count).collect())
        }
        PhysicalOperator::Distinct { input } => {
            let child_paths = execute_physical(graph, input, params)?;
            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<PathMap> = child_paths
                .into_iter()
                .filter(|path| {
                    let key = path
                        .iter()
                        .map(|(k, v)| format!("{}={:?}", k, v))
                        .collect::<Vec<_>>()
                        .join("|");
                    seen.insert(key)
                })
                .collect();
            Ok(deduped)
        }
        PhysicalOperator::OptionalMatch { input, null_vars } => {
            let child_paths = execute_physical(graph, input, params)?;
            if child_paths.is_empty() {
                // Produce one null-filled row for the pattern variables.
                let mut null_row: PathMap = HashMap::new();
                for var in null_vars {
                    null_row.insert(var.clone(), GraphBinding::Scalar(serde_json::Value::Null));
                }
                Ok(vec![null_row])
            } else {
                Ok(child_paths)
            }
        }
    }
}

pub(super) fn evaluate_where(
    graph: &Graph,
    path: &PathMap,
    where_clause: &WhereClause,
    params: &HashMap<String, serde_json::Value>,
) -> Result<bool, String> {
    match where_clause {
        WhereClause::Eq(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            Ok(lv == rv)
        }
        WhereClause::Ne(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            Ok(lv != rv)
        }
        WhereClause::Lt(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            // Use `==` not `<`: `Some(Less) < Some(Less)` is false, so the
            // relational check would never match any comparable pair.
            Ok(json_cmp(&lv, &rv) == Some(std::cmp::Ordering::Less))
        }
        WhereClause::Gt(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            // Symmetric fix: `Some(Greater) > Some(Greater)` is also false.
            Ok(json_cmp(&lv, &rv) == Some(std::cmp::Ordering::Greater))
        }
        WhereClause::Le(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            let cmp = json_cmp(&lv, &rv);
            Ok(cmp == Some(std::cmp::Ordering::Less) || cmp == Some(std::cmp::Ordering::Equal))
        }
        WhereClause::Ge(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            let cmp = json_cmp(&lv, &rv);
            Ok(cmp == Some(std::cmp::Ordering::Greater) || cmp == Some(std::cmp::Ordering::Equal))
        }
        WhereClause::Expr(e) => {
            let val = evaluate_expr(graph, path, e, params)?;
            Ok(val == serde_json::Value::Bool(true))
        }
    }
}

/// Evaluate a sort-key expression. First tries a normal evaluate_expr; if the variable is
/// unbound (because Project has already stripped node bindings), falls back to looking up
/// the expression's natural projected column name as a pre-computed scalar in the PathMap.
pub(super) fn evaluate_sort_key(
    graph: &Graph,
    path: &PathMap,
    expr: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    // Fast path: expression evaluates directly.
    if let Ok(val) = evaluate_expr(graph, path, expr, params) {
        if val != serde_json::Value::Null {
            return val;
        }
    }

    // Fallback: look up the projected column name.
    let col_name = match expr {
        Expr::Prop(var, prop) if prop.is_empty() => var.clone(),
        Expr::Prop(var, prop) => format!("{}.{}", var, prop),
        Expr::Literal(lit) => return literal_to_value(lit),
        Expr::Param(p) => {
            return params.get(p).cloned().unwrap_or(serde_json::Value::Null);
        }
        Expr::CountStar => "count(*)".to_string(),
        Expr::Agg(_, _) => return serde_json::Value::Null,
        _ => return serde_json::Value::Null,
    };

    // Try the full `var.prop` column name, then just `prop` alone (alias forms).
    if let Some(GraphBinding::Scalar(v)) = path.get(&col_name) {
        return v.clone();
    }
    // Try just the property name as a fallback alias (e.g., `n.age` stored as `"age"`).
    if let Expr::Prop(_, prop) = expr {
        if !prop.is_empty() {
            if let Some(GraphBinding::Scalar(v)) = path.get(prop) {
                return v.clone();
            }
        }
    }

    serde_json::Value::Null
}

/// Return the canonical display name for a built-in function.
/// Functions are stored in lowercase internally but may need mixed-case display.
pub(super) fn canonical_function_name(name: &str) -> &str {
    match name {
        "tointeger" | "toint" => "toInteger",
        "tofloat" => "toFloat",
        "tostring" => "toString",
        "toboolean" => "toBoolean",
        "isnull" => "isNull",
        "isnotnull" => "isNotNull",
        "startnode" => "startNode",
        "endnode" => "endNode",
        other => other,
    }
}

/// Total ordering for JSON values used by min/max aggregation.
/// Numbers compare numerically; strings compare lexicographically.
/// Cross-type order: null < bool < number < string < array < object.
pub(super) fn json_cmp_total(l: &serde_json::Value, r: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    fn type_rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Number(_) => 2,
            Value::String(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
        }
    }
    let lr = type_rank(l);
    let rr = type_rank(r);
    if lr != rr {
        return lr.cmp(&rr);
    }
    match (l, r) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => {
            if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
                ai.cmp(&bi)
            } else {
                let af = a.as_f64().unwrap_or(0.0);
                let bf = b.as_f64().unwrap_or(0.0);
                af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
            }
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => json_cmp(l, r).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Convert a `serde_json::Value` to a `PropValue` for property index lookups.
/// Returns `None` for unsupported value types (null, arrays, objects).
pub(super) fn json_to_prop_value(v: &serde_json::Value) -> Option<PropValue> {
    match v {
        serde_json::Value::Bool(b) => Some(PropValue::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(PropValue::Int(i))
            } else {
                n.as_f64().map(PropValue::Float)
            }
        }
        serde_json::Value::String(s) => Some(PropValue::Str(s.clone())),
        _ => None,
    }
}
