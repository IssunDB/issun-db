use std::collections::{HashMap, HashSet};

use issundb_core::{EdgeId, Graph, NodeId, PropValue};
use tracing::instrument;

use crate::ast::*;
use crate::parser;
use crate::plan::{FilterExpr, LogicalPlanner, Optimizer, PhysicalOperator, PhysicalPlanner};

/// The tabular result of a Cypher query execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub records: Vec<Record>,
}

/// An individual row in the query result table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Record {
    pub values: Vec<serde_json::Value>,
}

/// A binding for a Cypher variable: either a graph node or a graph edge.
///
/// The path map uses this type so that relationship variables are bound to the
/// correct `EdgeId` and node variables are bound to the correct `NodeId`.
/// `evaluate_expr` dispatches on the variant to call `get_node` or `get_edge`
/// as appropriate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GraphBinding {
    Node(NodeId),
    Edge(EdgeId),
    Scalar(serde_json::Value),
}

impl std::hash::Hash for GraphBinding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            GraphBinding::Node(id) => {
                0.hash(state);
                id.hash(state);
            }
            GraphBinding::Edge(id) => {
                1.hash(state);
                id.hash(state);
            }
            GraphBinding::Scalar(val) => {
                2.hash(state);
                val.to_string().hash(state);
            }
        }
    }
}

/// A row of variable bindings produced during plan execution.
type PathMap = HashMap<String, GraphBinding>;

/// Execute a Cypher query against the `Graph` handle.
#[instrument(skip(graph, params), fields(cypher = %cypher))]
pub fn execute(
    graph: &Graph,
    cypher: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let stmt = parser::parse(cypher)?;
    match stmt {
        Statement::Query(q) => execute_read_query(graph, &q, params),
        Statement::Create(c) => graph.with_write_lock(|| execute_create(graph, &c, params)),
        Statement::Set(s) => graph.with_write_lock(|| execute_set(graph, &s, params)),
        Statement::Delete(d) => graph.with_write_lock(|| execute_delete(graph, &d, params)),
        Statement::Merge(m) => execute_merge(graph, &m, params),
        Statement::CreateIndex(ci) => execute_create_index(graph, &ci),
        Statement::DropIndex(di) => execute_drop_index(graph, &di),
    }
}

/// Parse `cypher`, compile it into an optimized physical plan, and return the
/// plan as a human-readable indented tree.
///
/// Non-query statements (CREATE, SET, DELETE, MERGE) return a one-line summary
/// because they do not go through the read-query planner.
pub fn explain(graph: &Graph, cypher: &str) -> Result<String, String> {
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};

    let stmt = parser::parse(cypher)?;
    match stmt {
        Statement::Query(q) => {
            let logical = LogicalPlanner::plan(&q)?;
            let physical = PhysicalPlanner::plan(&logical);
            let optimized = Optimizer::optimize(physical, Some(graph));
            Ok(format_physical_plan(&optimized, 0))
        }
        Statement::Create(_) => Ok("CreatePattern\n".into()),
        Statement::Set(_) => Ok("MatchThenSet\n".into()),
        Statement::Delete(_) => Ok("MatchThenDelete\n".into()),
        Statement::Merge(_) => Ok("Merge\n".into()),
        Statement::CreateIndex(ref ci) => Ok(format!("CreateIndex {}:{}\n", ci.label, ci.property)),
        Statement::DropIndex(ref di) => Ok(format!("DropIndex {}:{}\n", di.label, di.property)),
    }
}

#[instrument(skip_all)]
fn execute_read_query(
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

    // 3. Derive column names using the same key-naming logic as the Project arm.
    let mut columns = Vec::new();
    for item in &query.return_clause.items {
        columns.push(projected_key(&item.expr, &item.alias));
    }

    // 4. Read each projected value directly from the PathMap by its canonical key.
    let mut records = Vec::new();
    for path in resolved_paths {
        let mut values = Vec::new();
        for item in &query.return_clause.items {
            let key = projected_key(&item.expr, &item.alias);
            values.push(binding_to_value(graph, path.get(&key))?);
        }
        records.push(Record { values });
    }

    Ok(QueryResult { columns, records })
}

/// Compute the key under which a RETURN/WITH item is stored in the projected PathMap.
///
/// Must exactly match the key-naming logic in the `PhysicalOperator::Project` arm of
/// `execute_physical` so that `execute_read_query` can look up pre-materialized values
/// by key rather than re-evaluating expressions.
fn projected_key(expr: &Expr, alias: &Option<String>) -> String {
    if let Some(a) = alias {
        a.clone()
    } else {
        expr_display_name(expr)
    }
}

/// Return a human-readable name for an expression, used as the default column name
/// when no alias is specified in a RETURN or WITH clause.
fn expr_display_name(expr: &Expr) -> String {
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
            let fn_name = match fn_ {
                AggFn::Count { distinct: true } => "count(DISTINCT ".to_string(),
                AggFn::Count { distinct: false } => "count".to_string(),
                AggFn::Sum => "sum".to_string(),
                AggFn::Avg => "avg".to_string(),
                AggFn::Min => "min".to_string(),
                AggFn::Max => "max".to_string(),
                AggFn::Collect => "collect".to_string(),
            };
            format!("{}({})", fn_name, expr_display_name(inner))
        }
        Expr::IsNull(inner) => format!("{} IS NULL", expr_display_name(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_display_name(inner)),
        Expr::Not(inner) => format!("NOT {}", expr_display_name(inner)),
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
            let args_str: Vec<String> = args.iter().map(expr_display_name).collect();
            format!("{}({})", name, args_str.join(", "))
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
fn binding_to_value(
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

fn execute_physical(
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
                let transitions = graph
                    .expand_spmv_graphblas(&src_nodes, rel_type.as_deref(), dir)
                    .map_err(|e| e.to_string())?;
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
                                let neighbors = graph
                                    .expand_spmv_graphblas(&[node], rel_type.as_deref(), dir)
                                    .map_err(|e| e.to_string())?;

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
                            if let Some(binding) = path.get(&target_var) {
                                projected_path.insert(target_var, binding.clone());
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
                        AggFn::Sum => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if let Some(n) = val.as_f64() {
                                state.sum += n;
                            }
                        }
                        AggFn::Avg => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if let Some(n) = val.as_f64() {
                                state.sum += n;
                                state.count += 1;
                            }
                        }
                        AggFn::Min => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                state.min = Some(match state.min.take() {
                                    None => val,
                                    Some(prev) => {
                                        if json_cmp(&val, &prev) == Some(std::cmp::Ordering::Less) {
                                            val
                                        } else {
                                            prev
                                        }
                                    }
                                });
                            }
                        }
                        AggFn::Max => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                state.max = Some(match state.max.take() {
                                    None => val,
                                    Some(prev) => {
                                        if json_cmp(&val, &prev)
                                            == Some(std::cmp::Ordering::Greater)
                                        {
                                            val
                                        } else {
                                            prev
                                        }
                                    }
                                });
                            }
                        }
                        AggFn::Collect => {
                            let val = evaluate_expr(graph, &path, inner_expr, params)?;
                            if val != serde_json::Value::Null {
                                state.collect.push(val);
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
                        AggFn::Sum => serde_json::Number::from_f64(state.sum)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                        AggFn::Avg => {
                            if state.count == 0 {
                                serde_json::Value::Null
                            } else {
                                let avg = state.sum / state.count as f64;
                                serde_json::Number::from_f64(avg)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        AggFn::Min => state.min.clone().unwrap_or(serde_json::Value::Null),
                        AggFn::Max => state.max.clone().unwrap_or(serde_json::Value::Null),
                        AggFn::Collect => serde_json::Value::Array(state.collect.clone()),
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
            let child_paths = execute_physical(graph, input, params)?;
            Ok(child_paths.into_iter().skip(*skip).take(*count).collect())
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

fn evaluate_where(
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
fn evaluate_sort_key(
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

/// Convert a `serde_json::Value` to a `PropValue` for property index lookups.
/// Returns `None` for unsupported value types (null, arrays, objects).
fn json_to_prop_value(v: &serde_json::Value) -> Option<PropValue> {
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

fn evaluate_expr(
    graph: &Graph,
    path: &PathMap,
    expr: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Param(p) => params
            .get(p)
            .cloned()
            .ok_or_else(|| format!("missing parameter: {}", p)),
        // CountStar and Agg are resolved by the Aggregate operator, not here.
        // If evaluate_expr is called on them outside of an aggregation context
        // (e.g., in a sort key), return null rather than panic.
        Expr::CountStar => Ok(serde_json::Value::Null),
        Expr::Agg(_, inner) => evaluate_expr(graph, path, inner, params),
        Expr::IsNull(inner) => {
            let val = evaluate_expr(graph, path, inner, params)?;
            Ok(serde_json::Value::Bool(val == serde_json::Value::Null))
        }
        Expr::IsNotNull(inner) => {
            let val = evaluate_expr(graph, path, inner, params)?;
            Ok(serde_json::Value::Bool(val != serde_json::Value::Null))
        }
        Expr::Not(inner) => {
            let val = evaluate_expr(graph, path, inner, params)?;
            match val {
                serde_json::Value::Bool(b) => Ok(serde_json::Value::Bool(!b)),
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        Expr::BinaryOp { op, left, right } => eval_binary_op(graph, path, op, left, right, params),
        Expr::FunctionCall { name, args } => eval_function_call(graph, path, name, args, params),
        Expr::Quantifier {
            kind,
            variable,
            list,
            predicate,
        } => {
            let list_val = evaluate_expr(graph, path, list, params)?;
            let items = match list_val {
                serde_json::Value::Array(arr) => arr,
                serde_json::Value::Null => vec![],
                other => vec![other],
            };

            let result = match kind {
                QuantifierKind::All => {
                    let mut all_true = true;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        match pred_val {
                            serde_json::Value::Bool(true) => {}
                            _ => {
                                all_true = false;
                                break;
                            }
                        }
                    }
                    serde_json::Value::Bool(all_true)
                }
                QuantifierKind::Any => {
                    let mut any_true = false;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        if pred_val == serde_json::Value::Bool(true) {
                            any_true = true;
                            break;
                        }
                    }
                    serde_json::Value::Bool(any_true)
                }
                QuantifierKind::None => {
                    let mut all_false = true;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        if pred_val == serde_json::Value::Bool(true) {
                            all_false = false;
                            break;
                        }
                    }
                    serde_json::Value::Bool(all_false)
                }
                QuantifierKind::Single => {
                    let mut count = 0usize;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        if pred_val == serde_json::Value::Bool(true) {
                            count += 1;
                            if count > 1 {
                                break;
                            }
                        }
                    }
                    serde_json::Value::Bool(count == 1)
                }
            };
            Ok(result)
        }
        Expr::Prop(var, prop) => {
            let binding = path
                .get(var)
                .ok_or_else(|| format!("unbound variable: {}", var))?;
            match binding {
                GraphBinding::Node(node_id) => {
                    let record = graph
                        .get_node(*node_id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("node not found: {}", node_id))?;
                    let actual_json: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if prop.is_empty() {
                        Ok(actual_json)
                    } else {
                        Ok(actual_json
                            .get(prop)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null))
                    }
                }
                GraphBinding::Edge(edge_id) => {
                    let record = graph
                        .get_edge(*edge_id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("edge not found: {}", edge_id))?;
                    let actual_json: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if prop.is_empty() {
                        Ok(actual_json)
                    } else {
                        Ok(actual_json
                            .get(prop)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null))
                    }
                }
                GraphBinding::Scalar(val) => {
                    if prop.is_empty() {
                        Ok(val.clone())
                    } else if let Some(obj) = val.as_object() {
                        Ok(obj.get(prop).cloned().unwrap_or(serde_json::Value::Null))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
            }
        }
    }
}

/// Evaluate a binary operation with three-valued null propagation.
fn eval_binary_op(
    graph: &Graph,
    path: &PathMap,
    op: &BinaryOperator,
    left: &Expr,
    right: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match op {
        BinaryOperator::And => {
            let lv = evaluate_expr(graph, path, left, params)?;
            if lv == serde_json::Value::Bool(false) {
                return Ok(serde_json::Value::Bool(false));
            }
            let rv = evaluate_expr(graph, path, right, params)?;
            if rv == serde_json::Value::Bool(false) {
                return Ok(serde_json::Value::Bool(false));
            }
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            Ok(serde_json::Value::Bool(true))
        }
        BinaryOperator::Or => {
            let lv = evaluate_expr(graph, path, left, params)?;
            if lv == serde_json::Value::Bool(true) {
                return Ok(serde_json::Value::Bool(true));
            }
            let rv = evaluate_expr(graph, path, right, params)?;
            if rv == serde_json::Value::Bool(true) {
                return Ok(serde_json::Value::Bool(true));
            }
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            Ok(serde_json::Value::Bool(false))
        }
        _ => {
            let lv = evaluate_expr(graph, path, left, params)?;
            let rv = evaluate_expr(graph, path, right, params)?;
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            match op {
                BinaryOperator::Eq => Ok(serde_json::Value::Bool(lv == rv)),
                BinaryOperator::Ne => Ok(serde_json::Value::Bool(lv != rv)),
                BinaryOperator::Lt => Ok(serde_json::Value::Bool(
                    json_cmp(&lv, &rv) == Some(std::cmp::Ordering::Less),
                )),
                BinaryOperator::Gt => Ok(serde_json::Value::Bool(
                    json_cmp(&lv, &rv) == Some(std::cmp::Ordering::Greater),
                )),
                BinaryOperator::Le => {
                    let c = json_cmp(&lv, &rv);
                    Ok(serde_json::Value::Bool(
                        c == Some(std::cmp::Ordering::Less) || c == Some(std::cmp::Ordering::Equal),
                    ))
                }
                BinaryOperator::Ge => {
                    let c = json_cmp(&lv, &rv);
                    Ok(serde_json::Value::Bool(
                        c == Some(std::cmp::Ordering::Greater)
                            || c == Some(std::cmp::Ordering::Equal),
                    ))
                }
                BinaryOperator::Add => eval_arithmetic(&lv, &rv, '+'),
                BinaryOperator::Sub => eval_arithmetic(&lv, &rv, '-'),
                BinaryOperator::Mul => eval_arithmetic(&lv, &rv, '*'),
                BinaryOperator::Div => eval_arithmetic(&lv, &rv, '/'),
                BinaryOperator::Mod => eval_arithmetic(&lv, &rv, '%'),
                BinaryOperator::And | BinaryOperator::Or => unreachable!(),
            }
        }
    }
}

fn eval_arithmetic(
    lv: &serde_json::Value,
    rv: &serde_json::Value,
    op: char,
) -> Result<serde_json::Value, String> {
    match (lv, rv) {
        (serde_json::Value::Number(ln), serde_json::Value::Number(rn)) => {
            if let (Some(li), Some(ri)) = (ln.as_i64(), rn.as_i64()) {
                let result = match op {
                    '+' => li.checked_add(ri).map(serde_json::Value::from),
                    '-' => li.checked_sub(ri).map(serde_json::Value::from),
                    '*' => li.checked_mul(ri).map(serde_json::Value::from),
                    '/' => {
                        if ri == 0 {
                            return Ok(serde_json::Value::Null);
                        }
                        li.checked_div(ri).map(serde_json::Value::from)
                    }
                    '%' => {
                        if ri == 0 {
                            return Ok(serde_json::Value::Null);
                        }
                        li.checked_rem(ri).map(serde_json::Value::from)
                    }
                    _ => None,
                };
                if let Some(v) = result {
                    return Ok(v);
                }
            }
            if let (Some(lf), Some(rf)) = (ln.as_f64(), rn.as_f64()) {
                let result = match op {
                    '+' => lf + rf,
                    '-' => lf - rf,
                    '*' => lf * rf,
                    '/' => {
                        if rf == 0.0 {
                            return Ok(serde_json::Value::Null);
                        }
                        lf / rf
                    }
                    '%' => {
                        if rf == 0.0 {
                            return Ok(serde_json::Value::Null);
                        }
                        lf % rf
                    }
                    _ => return Ok(serde_json::Value::Null),
                };
                return Ok(serde_json::Number::from_f64(result)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null));
            }
            Ok(serde_json::Value::Null)
        }
        (serde_json::Value::String(ls), serde_json::Value::String(rs)) if op == '+' => {
            Ok(serde_json::Value::String(format!("{}{}", ls, rs)))
        }
        _ => Ok(serde_json::Value::Null),
    }
}

/// Evaluate a built-in function call.
fn eval_function_call(
    graph: &Graph,
    path: &PathMap,
    name: &str,
    args: &[Expr],
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match name {
        "__list__" => {
            let mut items = Vec::new();
            for arg in args {
                items.push(evaluate_expr(graph, path, arg, params)?);
            }
            Ok(serde_json::Value::Array(items))
        }
        "__map__" => {
            // Args are alternating key (Literal::Str) and value.
            let mut map = serde_json::Map::new();
            let mut i = 0;
            while i + 1 < args.len() {
                let key_val = evaluate_expr(graph, path, &args[i], params)?;
                let val = evaluate_expr(graph, path, &args[i + 1], params)?;
                if let serde_json::Value::String(k) = key_val {
                    map.insert(k, val);
                }
                i += 2;
            }
            Ok(serde_json::Value::Object(map))
        }
        "range" => {
            if args.len() < 2 || args.len() > 3 {
                return Err("range() requires 2 or 3 arguments".into());
            }
            let start_val = evaluate_expr(graph, path, &args[0], params)?;
            let end_val = evaluate_expr(graph, path, &args[1], params)?;
            let start = start_val
                .as_i64()
                .ok_or_else(|| "range() start must be an integer".to_string())?;
            let end = end_val
                .as_i64()
                .ok_or_else(|| "range() end must be an integer".to_string())?;
            let step = if args.len() == 3 {
                let sv = evaluate_expr(graph, path, &args[2], params)?;
                let s = sv
                    .as_i64()
                    .ok_or_else(|| "range() step must be an integer".to_string())?;
                if s == 0 {
                    return Err("range() step must not be zero".into());
                }
                s
            } else {
                1i64
            };

            let mut result = Vec::new();
            if step > 0 {
                let mut v = start;
                while v <= end {
                    result.push(serde_json::Value::Number(v.into()));
                    v += step;
                }
            } else {
                let mut v = start;
                while v >= end {
                    result.push(serde_json::Value::Number(v.into()));
                    v += step;
                }
            }
            Ok(serde_json::Value::Array(result))
        }
        "size" => {
            if args.len() != 1 {
                return Err("size() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Array(arr) => {
                    Ok(serde_json::Value::Number((arr.len() as i64).into()))
                }
                serde_json::Value::String(s) => {
                    Ok(serde_json::Value::Number((s.chars().count() as i64).into()))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("size() argument must be a list or string".into()),
            }
        }
        "type" => {
            if args.len() != 1 {
                return Err("type() requires exactly 1 argument".into());
            }
            if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    if let Some(GraphBinding::Edge(eid)) = path.get(var.as_str()) {
                        let record = graph
                            .get_edge(*eid)
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| format!("edge not found: {}", eid))?;
                        if let Ok(name) = graph.type_name(record.edge_type) {
                            return Ok(name
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null));
                        }
                    }
                }
            }
            Ok(serde_json::Value::Null)
        }
        "id" => {
            if args.len() != 1 {
                return Err("id() requires exactly 1 argument".into());
            }
            if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    match path.get(var.as_str()) {
                        Some(GraphBinding::Node(nid)) => {
                            return Ok(serde_json::Value::Number((*nid).into()));
                        }
                        Some(GraphBinding::Edge(eid)) => {
                            return Ok(serde_json::Value::Number((*eid).into()));
                        }
                        _ => {}
                    }
                }
            }
            Ok(serde_json::Value::Null)
        }
        "coalesce" => {
            for arg in args {
                let val = evaluate_expr(graph, path, arg, params)?;
                if val != serde_json::Value::Null {
                    return Ok(val);
                }
            }
            Ok(serde_json::Value::Null)
        }
        "tostring" => {
            if args.len() != 1 {
                return Err("toString() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::String(s) => Ok(serde_json::Value::String(s)),
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                other => Ok(serde_json::Value::String(other.to_string())),
            }
        }
        "tointeger" | "toint" => {
            if args.len() != 1 {
                return Err("toInteger() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Ok(serde_json::Value::Number(i.into()))
                    } else if let Some(f) = n.as_f64() {
                        Ok(serde_json::Value::Number((f as i64).into()))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "tofloat" => {
            if args.len() != 1 {
                return Err("toFloat() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    Ok(serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0))
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "abs" => {
            if args.len() != 1 {
                return Err("abs() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Ok(i.abs().into())
                    } else if let Some(f) = n.as_f64() {
                        Ok(serde_json::Number::from_f64(f.abs())
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("abs() requires a numeric argument".into()),
            }
        }
        "keys" => {
            if args.len() != 1 {
                return Err("keys() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Object(map) => {
                    let keys: Vec<serde_json::Value> = map
                        .keys()
                        .map(|k| serde_json::Value::String(k.clone()))
                        .collect();
                    Ok(serde_json::Value::Array(keys))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Array(Vec::new())),
            }
        }
        "head" => {
            if args.len() != 1 {
                return Err("head() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Array(arr) => {
                    Ok(arr.into_iter().next().unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("head() requires a list argument".into()),
            }
        }
        "last" => {
            if args.len() != 1 {
                return Err("last() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Array(arr) => {
                    Ok(arr.into_iter().last().unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("last() requires a list argument".into()),
            }
        }
        "tail" => {
            if args.len() != 1 {
                return Err("tail() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Array(arr) => {
                    let tail: Vec<_> = arr.into_iter().skip(1).collect();
                    Ok(serde_json::Value::Array(tail))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("tail() requires a list argument".into()),
            }
        }
        "__in__" => {
            // expr IN list; null-safe: null IN list = null unless list is empty
            if args.len() != 2 {
                return Err("IN requires 2 arguments".into());
            }
            let needle = evaluate_expr(graph, path, &args[0], params)?;
            let haystack = evaluate_expr(graph, path, &args[1], params)?;
            match haystack {
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                serde_json::Value::Array(arr) => {
                    if arr.is_empty() {
                        return Ok(serde_json::Value::Bool(false));
                    }
                    let mut found_null = false;
                    for item in &arr {
                        if *item == needle {
                            return Ok(serde_json::Value::Bool(true));
                        }
                        if *item == serde_json::Value::Null || needle == serde_json::Value::Null {
                            found_null = true;
                        }
                    }
                    if found_null {
                        Ok(serde_json::Value::Null)
                    } else {
                        Ok(serde_json::Value::Bool(false))
                    }
                }
                _ => Ok(serde_json::Value::Bool(false)),
            }
        }
        "__contains__" => {
            if args.len() != 2 {
                return Err("CONTAINS requires 2 arguments".into());
            }
            let left = evaluate_expr(graph, path, &args[0], params)?;
            let right = evaluate_expr(graph, path, &args[1], params)?;
            if left == serde_json::Value::Null || right == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            match (left, right) {
                (serde_json::Value::String(s), serde_json::Value::String(sub)) => {
                    Ok(serde_json::Value::Bool(s.contains(&*sub)))
                }
                _ => Ok(serde_json::Value::Bool(false)),
            }
        }
        "__starts_with__" => {
            if args.len() != 2 {
                return Err("STARTS WITH requires 2 arguments".into());
            }
            let left = evaluate_expr(graph, path, &args[0], params)?;
            let right = evaluate_expr(graph, path, &args[1], params)?;
            if left == serde_json::Value::Null || right == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            match (left, right) {
                (serde_json::Value::String(s), serde_json::Value::String(prefix)) => {
                    Ok(serde_json::Value::Bool(s.starts_with(&*prefix)))
                }
                _ => Ok(serde_json::Value::Bool(false)),
            }
        }
        "__ends_with__" => {
            if args.len() != 2 {
                return Err("ENDS WITH requires 2 arguments".into());
            }
            let left = evaluate_expr(graph, path, &args[0], params)?;
            let right = evaluate_expr(graph, path, &args[1], params)?;
            if left == serde_json::Value::Null || right == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            match (left, right) {
                (serde_json::Value::String(s), serde_json::Value::String(suffix)) => {
                    Ok(serde_json::Value::Bool(s.ends_with(&*suffix)))
                }
                _ => Ok(serde_json::Value::Bool(false)),
            }
        }
        "__regex__" => {
            // Basic regex match; return false (not implemented fully)
            Ok(serde_json::Value::Bool(false))
        }
        "sqrt" => {
            if args.len() != 1 {
                return Err("sqrt() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0);
                    Ok(serde_json::Number::from_f64(f.sqrt())
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("sqrt() requires a numeric argument".into()),
            }
        }
        "floor" => {
            if args.len() != 1 {
                return Err("floor() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0).floor();
                    Ok(serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("floor() requires a numeric argument".into()),
            }
        }
        "ceil" | "ceiling" => {
            if args.len() != 1 {
                return Err("ceil() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0).ceil();
                    Ok(serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("ceil() requires a numeric argument".into()),
            }
        }
        "round" => {
            if args.is_empty() || args.len() > 2 {
                return Err("round() requires 1 or 2 arguments".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0);
                    let precision = if args.len() == 2 {
                        let pv = evaluate_expr(graph, path, &args[1], params)?;
                        pv.as_i64().unwrap_or(0) as u32
                    } else {
                        0
                    };
                    let factor = 10f64.powi(precision as i32);
                    let rounded = (f * factor).round() / factor;
                    Ok(serde_json::Number::from_f64(rounded)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("round() requires a numeric argument".into()),
            }
        }
        "sign" => {
            if args.len() != 1 {
                return Err("sign() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0);
                    let s = if f > 0.0 {
                        1i64
                    } else if f < 0.0 {
                        -1i64
                    } else {
                        0i64
                    };
                    Ok(serde_json::Value::Number(s.into()))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("sign() requires a numeric argument".into()),
            }
        }
        "log" => {
            if args.len() != 1 {
                return Err("log() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0).ln();
                    Ok(serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("log() requires a numeric argument".into()),
            }
        }
        "log10" => {
            if args.len() != 1 {
                return Err("log10() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0).log10();
                    Ok(serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("log10() requires a numeric argument".into()),
            }
        }
        "exp" => {
            if args.len() != 1 {
                return Err("exp() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0).exp();
                    Ok(serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err("exp() requires a numeric argument".into()),
            }
        }
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" => {
            if args.len() != 1 {
                return Err(format!("{}() requires exactly 1 argument", name));
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(0.0);
                    let result = match name {
                        "sin" => f.sin(),
                        "cos" => f.cos(),
                        "tan" => f.tan(),
                        "asin" => f.asin(),
                        "acos" => f.acos(),
                        "atan" => f.atan(),
                        _ => unreachable!(),
                    };
                    Ok(serde_json::Number::from_f64(result)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Err(format!("{}() requires a numeric argument", name)),
            }
        }
        "atan2" => {
            if args.len() != 2 {
                return Err("atan2() requires exactly 2 arguments".into());
            }
            let y = evaluate_expr(graph, path, &args[0], params)?;
            let x = evaluate_expr(graph, path, &args[1], params)?;
            match (y, x) {
                (serde_json::Value::Number(y_n), serde_json::Value::Number(x_n)) => {
                    let result = y_n
                        .as_f64()
                        .unwrap_or(0.0)
                        .atan2(x_n.as_f64().unwrap_or(0.0));
                    Ok(serde_json::Number::from_f64(result)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null))
                }
                _ => Ok(serde_json::Value::Null),
            }
        }
        "pi" => Ok(serde_json::Number::from_f64(std::f64::consts::PI)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        "e" => Ok(serde_json::Number::from_f64(std::f64::consts::E)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        "max" if args.len() > 1 => {
            // max(a, b, ...) as a function call (not aggregation)
            let mut best: Option<serde_json::Value> = None;
            for arg in args {
                let val = evaluate_expr(graph, path, arg, params)?;
                if val == serde_json::Value::Null {
                    continue;
                }
                best = Some(match best {
                    None => val,
                    Some(b) => {
                        if json_cmp(&val, &b) == Some(std::cmp::Ordering::Greater) {
                            val
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.unwrap_or(serde_json::Value::Null))
        }
        "min" if args.len() > 1 => {
            // min(a, b, ...) as a function call (not aggregation)
            let mut best: Option<serde_json::Value> = None;
            for arg in args {
                let val = evaluate_expr(graph, path, arg, params)?;
                if val == serde_json::Value::Null {
                    continue;
                }
                best = Some(match best {
                    None => val,
                    Some(b) => {
                        if json_cmp(&val, &b) == Some(std::cmp::Ordering::Less) {
                            val
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.unwrap_or(serde_json::Value::Null))
        }
        "toboolean" => {
            if args.len() != 1 {
                return Err("toBoolean() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Bool(b) => Ok(serde_json::Value::Bool(b)),
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                serde_json::Value::String(s) => match s.to_ascii_lowercase().as_str() {
                    "true" => Ok(serde_json::Value::Bool(true)),
                    "false" => Ok(serde_json::Value::Bool(false)),
                    _ => Ok(serde_json::Value::Null),
                },
                _ => Ok(serde_json::Value::Null),
            }
        }
        "labels" => {
            if args.len() != 1 {
                return Err("labels() requires exactly 1 argument".into());
            }
            if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    if let Some(GraphBinding::Node(nid)) = path.get(var.as_str()) {
                        if let Ok(Some(record)) = graph.get_node(*nid) {
                            if let Ok(Some(label)) = graph.label_name(record.label) {
                                return Ok(serde_json::Value::Array(vec![
                                    serde_json::Value::String(label),
                                ]));
                            }
                        }
                    }
                }
            }
            Ok(serde_json::Value::Array(vec![]))
        }
        "length" => {
            if args.len() != 1 {
                return Err("length() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Array(arr) => {
                    Ok(serde_json::Value::Number((arr.len() as i64).into()))
                }
                serde_json::Value::String(s) => {
                    Ok(serde_json::Value::Number((s.chars().count() as i64).into()))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "substring" => {
            if args.len() < 2 || args.len() > 3 {
                return Err("substring() requires 2 or 3 arguments".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            let start_v = evaluate_expr(graph, path, &args[1], params)?;
            match (val, start_v) {
                (serde_json::Value::String(s), serde_json::Value::Number(start_n)) => {
                    let start = start_n.as_i64().unwrap_or(0).max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    let end = if args.len() == 3 {
                        let len_v = evaluate_expr(graph, path, &args[2], params)?;
                        let len = len_v.as_i64().unwrap_or(0).max(0) as usize;
                        (start + len).min(chars.len())
                    } else {
                        chars.len()
                    };
                    let result: String = chars[start.min(chars.len())..end].iter().collect();
                    Ok(serde_json::Value::String(result))
                }
                (serde_json::Value::Null, _) | (_, serde_json::Value::Null) => {
                    Ok(serde_json::Value::Null)
                }
                _ => Ok(serde_json::Value::Null),
            }
        }
        "trim" | "ltrim" | "rtrim" => {
            if args.len() != 1 {
                return Err(format!("{}() requires exactly 1 argument", name));
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::String(s) => {
                    let result = match name {
                        "trim" => s.trim().to_string(),
                        "ltrim" => s.trim_start().to_string(),
                        "rtrim" => s.trim_end().to_string(),
                        _ => s,
                    };
                    Ok(serde_json::Value::String(result))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "toupper" | "tolower" => {
            if args.len() != 1 {
                return Err(format!("{}() requires exactly 1 argument", name));
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::String(s) => {
                    let result = if name == "toupper" {
                        s.to_uppercase()
                    } else {
                        s.to_lowercase()
                    };
                    Ok(serde_json::Value::String(result))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "replace" => {
            if args.len() != 3 {
                return Err("replace() requires exactly 3 arguments".into());
            }
            let original = evaluate_expr(graph, path, &args[0], params)?;
            let search = evaluate_expr(graph, path, &args[1], params)?;
            let replacement = evaluate_expr(graph, path, &args[2], params)?;
            match (original, search, replacement) {
                (
                    serde_json::Value::String(o),
                    serde_json::Value::String(s),
                    serde_json::Value::String(r),
                ) => Ok(serde_json::Value::String(o.replace(&*s, &r))),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "split" => {
            if args.len() != 2 {
                return Err("split() requires exactly 2 arguments".into());
            }
            let text = evaluate_expr(graph, path, &args[0], params)?;
            let delim = evaluate_expr(graph, path, &args[1], params)?;
            match (text, delim) {
                (serde_json::Value::String(t), serde_json::Value::String(d)) => {
                    let parts: Vec<serde_json::Value> = t
                        .split(&*d)
                        .map(|s| serde_json::Value::String(s.to_string()))
                        .collect();
                    Ok(serde_json::Value::Array(parts))
                }
                _ => Ok(serde_json::Value::Null),
            }
        }
        "reverse" => {
            if args.len() != 1 {
                return Err("reverse() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::String(s) => {
                    Ok(serde_json::Value::String(s.chars().rev().collect()))
                }
                serde_json::Value::Array(arr) => {
                    Ok(serde_json::Value::Array(arr.into_iter().rev().collect()))
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Null),
            }
        }
        "nodes" => {
            // Returns the nodes of a path; simplified: return empty list
            Ok(serde_json::Value::Array(vec![]))
        }
        "relationships" | "rels" => {
            // Returns the relationships of a path; simplified: return empty list
            Ok(serde_json::Value::Array(vec![]))
        }
        "properties" => {
            if args.len() != 1 {
                return Err("properties() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            match val {
                serde_json::Value::Object(_) => Ok(val),
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                _ => Ok(serde_json::Value::Object(serde_json::Map::new())),
            }
        }
        "startnode" | "endnode" => {
            // Stub for path functions
            Ok(serde_json::Value::Null)
        }
        "isnull" => {
            if args.len() != 1 {
                return Err("isNull() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            Ok(serde_json::Value::Bool(val == serde_json::Value::Null))
        }
        "isnotnull" => {
            if args.len() != 1 {
                return Err("isNotNull() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            Ok(serde_json::Value::Bool(val != serde_json::Value::Null))
        }
        _ => Err(format!("unknown function: {}", name)),
    }
}

fn literal_to_value(l: &Literal) -> serde_json::Value {
    match l {
        Literal::Str(s) => serde_json::Value::String(s.clone()),
        Literal::Int(i) => serde_json::Value::Number((*i).into()),
        Literal::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Literal::Bool(b) => serde_json::Value::Bool(*b),
        Literal::Null => serde_json::Value::Null,
        Literal::List(items) => {
            serde_json::Value::Array(items.iter().map(literal_to_value).collect())
        }
    }
}

fn json_cmp(l: &serde_json::Value, r: &serde_json::Value) -> Option<std::cmp::Ordering> {
    match (l, r) {
        (serde_json::Value::Number(n1), serde_json::Value::Number(n2)) => {
            if let (Some(i1), Some(i2)) = (n1.as_i64(), n2.as_i64()) {
                Some(i1.cmp(&i2))
            } else if let (Some(f1), Some(f2)) = (n1.as_f64(), n2.as_f64()) {
                f1.partial_cmp(&f2)
            } else {
                None
            }
        }
        (serde_json::Value::String(s1), serde_json::Value::String(s2)) => Some(s1.cmp(s2)),
        _ => None,
    }
}

fn execute_create_internal(graph: &Graph, pattern: &Pattern) -> Result<PathMap, String> {
    let mut bindings = HashMap::new();
    let mut props_map = HashMap::new();
    if let Some(ref props) = pattern.node.properties {
        for (k, v) in props {
            props_map.insert(k.clone(), literal_to_value(v));
        }
    }

    let label = pattern
        .node
        .label
        .clone()
        .unwrap_or_else(|| "Node".to_string());
    let seed_id = graph
        .add_node(&label, &props_map)
        .map_err(|e| e.to_string())?;

    if let Some(ref var_name) = pattern.node.variable {
        bindings.insert(var_name.clone(), GraphBinding::Node(seed_id));
    }

    let mut created_node_id = seed_id;
    for (rel_pat, node_pat) in &pattern.rels {
        let mut target_props = HashMap::new();
        if let Some(ref props) = node_pat.properties {
            for (k, v) in props {
                target_props.insert(k.clone(), literal_to_value(v));
            }
        }
        let target_label = node_pat.label.clone().unwrap_or_else(|| "Node".to_string());
        let target_id = graph
            .add_node(&target_label, &target_props)
            .map_err(|e| e.to_string())?;

        if let Some(ref var_name) = node_pat.variable {
            bindings.insert(var_name.clone(), GraphBinding::Node(target_id));
        }

        let rel_type = rel_pat
            .rel_type
            .clone()
            .unwrap_or_else(|| "EDGE".to_string());
        let empty_props: HashMap<String, serde_json::Value> = HashMap::new();

        let edge_id = if rel_pat.is_incoming {
            graph
                .add_edge(target_id, created_node_id, &rel_type, &empty_props)
                .map_err(|e| e.to_string())?
        } else {
            graph
                .add_edge(created_node_id, target_id, &rel_type, &empty_props)
                .map_err(|e| e.to_string())?
        };

        if let Some(ref var_name) = rel_pat.variable {
            bindings.insert(var_name.clone(), GraphBinding::Edge(edge_id));
        }

        created_node_id = target_id;
    }

    Ok(bindings)
}

fn execute_create(
    graph: &Graph,
    create: &CreateStatement,
    _params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    execute_create_internal(graph, &create.pattern)?;
    Ok(QueryResult {
        columns: vec!["nodes_created".to_string()],
        records: vec![Record {
            values: vec![serde_json::Value::Number(1.into())],
        }],
    })
}

fn execute_set(
    graph: &Graph,
    set: &SetStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query so the MATCH and WHERE clauses go through the same
    // planner and executor pipeline as MATCH ... RETURN queries.
    let synthetic_query = Query {
        match_clauses: set.match_clauses.clone(),
        where_clause: set.where_clause.clone(),
        return_clause: ReturnClause { items: vec![] },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    // `LogicalPlanner` always wraps the plan in a final Project. When the
    // RETURN clause is empty (as in this synthetic query) that Project would
    // produce PathMaps with no variables at all, making SET assignments
    // impossible. Strip the zero-item Project so SET can see matched variables.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    let mut matched_count = 0;
    for path in bound_paths {
        for set_item in &set.set_items {
            let node_id = match path.get(&set_item.variable) {
                Some(GraphBinding::Node(id)) => *id,
                Some(GraphBinding::Edge(_)) => {
                    return Err(format!(
                        "SET on edge variable '{}' is not supported",
                        set_item.variable
                    ));
                }
                Some(GraphBinding::Scalar(_)) => {
                    return Err(format!(
                        "SET on scalar variable '{}' is not supported",
                        set_item.variable
                    ));
                }
                None => return Err(format!("unbound variable: {}", set_item.variable)),
            };
            let record = graph
                .get_node(node_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node not found: {}", node_id))?;
            let mut actual_json: serde_json::Value =
                rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

            let new_val = evaluate_expr(graph, &path, &set_item.expr, params)?;
            if let Some(obj) = actual_json.as_object_mut() {
                obj.insert(set_item.property.clone(), new_val);
            }

            graph
                .update_node(node_id, &actual_json)
                .map_err(|e| e.to_string())?;
        }
        matched_count += 1;
    }

    Ok(QueryResult {
        columns: vec!["properties_set".to_string()],
        records: vec![Record {
            values: vec![serde_json::Value::Number(matched_count.into())],
        }],
    })
}

fn execute_delete(
    graph: &Graph,
    delete: &DeleteStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query so the MATCH and WHERE clauses go through the same
    // planner and executor pipeline as MATCH ... RETURN queries.
    let synthetic_query = Query {
        match_clauses: delete.match_clauses.clone(),
        where_clause: delete.where_clause.clone(),
        return_clause: ReturnClause { items: vec![] },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    // Strip the zero-item Project that LogicalPlanner adds for the empty
    // RETURN clause; otherwise execute_physical clears all matched variables.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    let mut nodes_deleted = 0;
    let mut relationships_deleted = 0;
    for path in bound_paths {
        for var in &delete.variables {
            match path.get(var) {
                Some(GraphBinding::Node(id)) => {
                    graph.delete_node(*id).map_err(|e| e.to_string())?;
                    nodes_deleted += 1;
                }
                Some(GraphBinding::Edge(id)) => {
                    graph.delete_edge(*id).map_err(|e| e.to_string())?;
                    relationships_deleted += 1;
                }
                Some(GraphBinding::Scalar(_)) => {
                    return Err(format!(
                        "DELETE on scalar variable '{}' is not supported",
                        var
                    ));
                }
                None => return Err(format!("unbound variable: {}", var)),
            }
        }
    }

    Ok(QueryResult {
        columns: vec![
            "nodes_deleted".to_string(),
            "relationships_deleted".to_string(),
        ],
        records: vec![Record {
            values: vec![
                serde_json::Value::Number(nodes_deleted.into()),
                serde_json::Value::Number(relationships_deleted.into()),
            ],
        }],
    })
}

#[instrument(skip_all)]
fn execute_merge(
    graph: &Graph,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph.with_write_lock(|| execute_merge_inner(graph, stmt, params))
}

fn execute_merge_inner(
    graph: &Graph,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query to test whether the pattern already exists.
    let synthetic_query = Query {
        match_clauses: vec![MatchClause {
            pattern: stmt.pattern.clone(),
        }],
        where_clause: None,
        return_clause: ReturnClause { items: vec![] },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    // Strip the zero-item Project that LogicalPlanner adds for the empty RETURN clause.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    let empty_result = QueryResult {
        columns: vec![],
        records: vec![Record { values: vec![] }],
    };

    if bound_paths.is_empty() {
        // Pattern does not exist; create it.
        let created_bindings = execute_create_internal(graph, &stmt.pattern)?;

        // Apply ON CREATE SET actions if any.
        if !stmt.on_create_set.is_empty() {
            apply_set_items(graph, &created_bindings, &stmt.on_create_set, params)?;
        }
    } else {
        // Pattern exists; apply ON MATCH SET actions.
        for path in &bound_paths {
            apply_set_items(graph, path, &stmt.on_match_set, params)?;
        }
    }

    Ok(empty_result)
}

fn apply_set_items(
    graph: &Graph,
    path: &PathMap,
    set_items: &[SetItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    for item in set_items {
        let node_id = match path.get(&item.variable) {
            Some(GraphBinding::Node(id)) => *id,
            Some(GraphBinding::Edge(_)) => {
                return Err(format!(
                    "SET on edge variable '{}' is not supported",
                    item.variable
                ));
            }
            Some(GraphBinding::Scalar(_)) => {
                return Err(format!(
                    "SET on scalar variable '{}' is not supported",
                    item.variable
                ));
            }
            None => return Err(format!("unbound variable: {}", item.variable)),
        };
        let record = graph
            .get_node(node_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("node not found: {}", node_id))?;
        let mut actual_json: serde_json::Value =
            rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
        let new_val = evaluate_expr(graph, path, &item.expr, params)?;
        if let Some(obj) = actual_json.as_object_mut() {
            obj.insert(item.property.clone(), new_val);
        }
        graph
            .update_node(node_id, &actual_json)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn execute_create_index(graph: &Graph, stmt: &CreateIndexStatement) -> Result<QueryResult, String> {
    graph
        .create_node_text_index(&stmt.label, &stmt.property)
        .map_err(|e| e.to_string())?;
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

fn execute_drop_index(graph: &Graph, stmt: &DropIndexStatement) -> Result<QueryResult, String> {
    graph
        .drop_node_text_index(&stmt.label, &stmt.property)
        .map_err(|e| e.to_string())?;
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use issundb_core::Graph;
    use tempfile::TempDir;

    fn setup_graph() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn insert_person(graph: &Graph, name: &str, age: i64, city: &str) -> issundb_core::NodeId {
        let props = serde_json::json!({"name": name, "age": age, "city": city});
        graph.add_node("Person", &props).unwrap()
    }

    // Helper: run a simple Cypher and return all records.
    fn run(graph: &Graph, cypher: &str) -> Vec<Vec<serde_json::Value>> {
        let params = HashMap::new();
        execute(graph, cypher, &params)
            .unwrap()
            .records
            .into_iter()
            .map(|r| r.values)
            .collect()
    }

    #[test]
    fn order_by_age_asc() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY n.age ASC",
        );
        let ages: Vec<i64> = rows.iter().map(|r| r[1].as_i64().unwrap()).collect();
        assert_eq!(ages, vec![25, 30, 40]);
    }

    #[test]
    fn order_by_name_desc() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.name AS name ORDER BY n.name DESC",
        );
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Carol", "Bob", "Alice"]);
    }

    #[test]
    fn limit_returns_at_most_n_rows() {
        let (_dir, graph) = setup_graph();
        for i in 0..10i64 {
            graph
                .add_node("Item", &serde_json::json!({"i": i}))
                .unwrap();
        }
        let rows = run(&graph, "MATCH (n:Item) RETURN n.i AS i LIMIT 3");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn skip_and_limit_pagination() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Dave", 35, "LA");
        insert_person(&graph, "Eve", 28, "NY");

        // ORDER BY age ASC, then SKIP 1 LIMIT 2 gives the 2nd and 3rd youngest: 28, 30.
        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.age AS age ORDER BY n.age ASC SKIP 1 LIMIT 2",
        );
        assert_eq!(rows.len(), 2);
        let ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        assert_eq!(ages, vec![28, 30]);
    }

    #[test]
    fn count_star_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");

        let rows = run(&graph, "MATCH (n:Person) RETURN count(*) AS c");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_i64().unwrap(), 3);
    }

    #[test]
    fn sum_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 20, "X");
        insert_person(&graph, "Carol", 30, "X");

        let rows = run(&graph, "MATCH (n:Person) RETURN sum(n.age) AS total");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 60.0);
    }

    #[test]
    fn avg_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 30, "X");

        let rows = run(&graph, "MATCH (n:Person) RETURN avg(n.age) AS a");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 20.0);
    }

    #[test]
    fn min_max_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 30, "X");
        insert_person(&graph, "Carol", 20, "X");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN min(n.age) AS lo, max(n.age) AS hi",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 10.0);
        assert_eq!(rows[0][1].as_f64().unwrap(), 30.0);
    }

    #[test]
    fn collect_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "NY");

        let rows = run(&graph, "MATCH (n:Person) RETURN collect(n.name) AS names");
        assert_eq!(rows.len(), 1);
        let arr = rows[0][0].as_array().unwrap();
        let mut names: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Alice", "Bob"]);
    }

    #[test]
    fn group_by_city_count() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.city AS city, count(*) AS c ORDER BY n.city ASC",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_str().unwrap(), "LA");
        assert_eq!(rows[0][1].as_i64().unwrap(), 1);
        assert_eq!(rows[1][0].as_str().unwrap(), "NY");
        assert_eq!(rows[1][1].as_i64().unwrap(), 2);
    }

    #[test]
    fn merge_creates_node_when_absent() {
        let (_dir, graph) = setup_graph();

        let params = HashMap::new();
        execute(&graph, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(&graph, "MATCH (n:Person) RETURN n.name AS name", &params).unwrap();
        assert_eq!(result.records.len(), 1);
    }

    #[test]
    fn merge_does_not_duplicate_existing_node() {
        let (_dir, graph) = setup_graph();

        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();
        execute(&graph, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(&graph, "MATCH (n:Person) RETURN n.name AS name", &params).unwrap();
        assert_eq!(result.records.len(), 1, "MERGE must not create a duplicate");
    }

    #[test]
    fn optional_match_returns_nulls_when_no_match() {
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Person", &serde_json::json!({"name": "Alice"}))
            .unwrap();

        let params = HashMap::new();
        let result = execute(&graph, "OPTIONAL MATCH (n:NonExistent) RETURN n", &params).unwrap();
        // Should return one row with n = null.
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].values[0], serde_json::Value::Null);
    }

    #[test]
    fn create_index_and_drop_index_execute_without_error() {
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Movie", &serde_json::json!({"title": "Inception"}))
            .unwrap();

        let params = HashMap::new();
        execute(&graph, "CREATE INDEX FOR (n:Movie) ON (n.title)", &params).unwrap();
        assert!(graph.has_node_text_index("Movie", "title").unwrap());

        execute(&graph, "DROP INDEX FOR (n:Movie) ON (n.title)", &params).unwrap();
        assert!(!graph.has_node_text_index("Movie", "title").unwrap());
    }

    #[test]
    fn merge_concurrent_safety() {
        let (_dir, graph) = setup_graph();
        let graph_arc = std::sync::Arc::new(graph);
        let mut threads = Vec::new();
        for _ in 0..10 {
            let g = graph_arc.clone();
            threads.push(std::thread::spawn(move || {
                let params = HashMap::new();
                execute(&g, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let params = HashMap::new();
        let result = execute(
            &graph_arc,
            "MATCH (n:Person) RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(
            result.records.len(),
            1,
            "Concurrency race in MERGE created duplicate nodes"
        );
    }

    #[test]
    fn merge_on_create_set_does_not_affect_existing_nodes() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        // Create an existing Bob.
        execute(&graph, "CREATE (b:Person {name: 'Bob', age: 40})", &params).unwrap();

        // MERGE a relationship between a new Alice (absent) and Bob (name: 'Bob').
        // Since the relationship (and Alice) is absent, the pattern is created.
        execute(
            &graph,
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'}) ON CREATE SET b.age = 50",
            &params,
        )
        .unwrap();

        // Verify the original Bob's age remains 40.
        let r1 = execute(
            &graph,
            "MATCH (b:Person {name: 'Bob', age: 40}) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(r1.records.len(), 1);

        // Verify the new Bob's age is 50.
        let r2 = execute(
            &graph,
            "MATCH (b:Person {name: 'Bob', age: 50}) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(r2.records.len(), 1);
    }
}
