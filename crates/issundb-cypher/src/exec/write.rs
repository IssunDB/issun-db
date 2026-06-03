use super::expr::evaluate_expr;
use super::read::{
    binding_to_value, column_name, execute_physical, execute_read_query, projected_key,
};
use super::*;
use crate::ast::{
    CreateAndReturnStatement, DeleteAndReturnStatement, ForeachStatement, MergeAndReturnStatement,
    QueryPart, RemoveAndReturnStatement, RemoveItem, RemoveStatement, SetAndReturnStatement,
};

/// Evaluate a property map `HashMap<String, Expr>` using the given path context.
/// Returns a JSON object containing the evaluated properties.
fn eval_properties(
    graph: &Graph,
    props: &HashMap<String, crate::ast::Expr>,
    path: &PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for (k, v) in props {
        let val = evaluate_expr(graph, path, v, params)?;
        // Cypher semantics: assigning null to a property is equivalent to
        // removing it; null-valued properties are not stored.
        if val != serde_json::Value::Null {
            obj.insert(k.clone(), val);
        }
    }
    Ok(serde_json::Value::Object(obj))
}

pub(super) fn execute_create_internal(graph: &Graph, pattern: &Pattern) -> Result<PathMap, String> {
    execute_create_internal_with_context(graph, pattern, &PathMap::new(), &HashMap::new())
}

pub(super) fn execute_create(
    graph: &Graph,
    create: &CreateStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Thread bindings across patterns so that variables created in one pattern
    // (e.g., the first node in `CREATE (a:A), (a)-[:R]->(b:B)`) are available
    // in subsequent patterns within the same CREATE clause.
    let mut shared_bindings = PathMap::new();
    for pattern in &create.patterns {
        let created =
            execute_create_internal_with_context(graph, pattern, &shared_bindings, params)?;
        shared_bindings.extend(created);
    }
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_create_and_return(
    graph: &Graph,
    stmt: &CreateAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Thread bindings across patterns within the same CREATE clause.
    let mut bindings = PathMap::new();
    for pattern in &stmt.patterns {
        let created = execute_create_internal_with_context(graph, pattern, &bindings, params)?;
        bindings.extend(created);
    }

    // Project the RETURN clause over the created bindings.
    let columns: Vec<String> = stmt.return_clause.items.iter().map(column_name).collect();

    let mut values = Vec::new();
    for item in &stmt.return_clause.items {
        let key = projected_key(&item.expr, &item.alias);
        // Try evaluating the expression first.
        let val =
            evaluate_expr(graph, &bindings, &item.expr, params).unwrap_or(serde_json::Value::Null);
        // If null and there's a binding by the key name, use that.
        let val = if val == serde_json::Value::Null {
            if let Some(binding) = bindings.get(&key) {
                binding_to_value(graph, Some(binding)).unwrap_or(serde_json::Value::Null)
            } else {
                val
            }
        } else {
            val
        };
        values.push(val);
    }

    let records = if values.is_empty() {
        vec![]
    } else {
        vec![Record { values }]
    };

    // Apply SKIP/LIMIT.
    let mut result_records = records;
    if let Some(skip_expr) = &stmt.skip {
        let skip_val = evaluate_expr(graph, &PathMap::new(), skip_expr, params)?;
        let skip = skip_val.as_i64().unwrap_or(0).max(0) as usize;
        result_records = result_records.into_iter().skip(skip).collect();
    }
    if let Some(limit_expr) = &stmt.limit {
        let limit_val = evaluate_expr(graph, &PathMap::new(), limit_expr, params)?;
        let limit = limit_val.as_i64().unwrap_or(0).max(0) as usize;
        result_records = result_records.into_iter().take(limit).collect();
    }

    Ok(QueryResult {
        columns,
        records: result_records,
    })
}

pub(super) fn execute_set_and_return(
    graph: &Graph,
    stmt: &SetAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Route through the read-query pipeline with the SET as a write part. The SET runs
    // per matched row, then the RETURN projection (including aggregation, ORDER BY, SKIP,
    // and LIMIT) operates on those same post-mutation rows. The rows are not re-matched,
    // so a SET that changes a matched property or label does not alter the cardinality.
    let synthetic_query = Query {
        match_clauses: Vec::new(),
        where_clause: None,
        return_clause: stmt.return_clause.clone(),
        parts: vec![
            QueryPart::Match {
                match_clauses: stmt.match_clauses.clone(),
                where_clause: stmt.where_clause.clone(),
            },
            QueryPart::Set {
                items: stmt.set_items.clone(),
            },
        ],
        order_by: stmt.order_by.clone(),
        skip: stmt.skip.clone(),
        limit: stmt.limit.clone(),
    };
    execute_read_query(graph, &synthetic_query, params)
}

pub(super) fn execute_set(
    graph: &Graph,
    set: &SetStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query so the MATCH and WHERE clauses go through the same
    // planner and executor pipeline as MATCH ... RETURN queries.
    let synthetic_query = Query {
        match_clauses: set.match_clauses.clone(),
        where_clause: set.where_clause.clone(),
        return_clause: ReturnClause {
            items: vec![],
            distinct: false,
        },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query).map_err(|e| e.to_string())?;
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

    for path in bound_paths {
        apply_set_items(graph, &path, &set.set_items, params)?;
    }

    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_delete(
    graph: &Graph,
    delete: &DeleteStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query so the MATCH and WHERE clauses go through the same
    // planner and executor pipeline as MATCH ... RETURN queries.
    let synthetic_query = Query {
        match_clauses: delete.match_clauses.clone(),
        where_clause: delete.where_clause.clone(),
        return_clause: ReturnClause {
            items: vec![],
            distinct: false,
        },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query).map_err(|e| e.to_string())?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    // Strip the zero-item Project that LogicalPlanner adds for the empty
    // RETURN clause; otherwise execute_physical clears all matched variables.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    delete_over_paths(graph, &bound_paths, &delete.targets, delete.detach, params)?;

    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

/// Resolve a DELETE target expression to the graph elements it refers to.
///
/// A bare variable bound to a node or relationship is taken directly. Any other
/// expression is evaluated and interpreted: `__Node__`, `__Edge__`, and `__Path__`
/// representations and lists of them are accepted; null is skipped; anything else
/// (an integer, a map, a string) is an error, matching openCypher's requirement
/// that DELETE operates on nodes, relationships, and paths.
fn collect_delete_targets(
    graph: &Graph,
    path: &PathMap,
    expr: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<GraphBinding>, String> {
    // Preserve graph identity for a bare bound variable without a JSON round-trip.
    if let Expr::Prop(var, prop) = expr {
        if prop.is_empty() {
            match path.get(var) {
                Some(GraphBinding::Node(id)) => return Ok(vec![GraphBinding::Node(*id)]),
                Some(GraphBinding::Edge(id)) => return Ok(vec![GraphBinding::Edge(*id)]),
                Some(GraphBinding::Scalar(v)) => return value_to_delete_targets(v),
                None => return Err(format!("unbound variable: {}", var)),
            }
        }
    }
    let val = evaluate_expr(graph, path, expr, params)?;
    value_to_delete_targets(&val)
}

/// Interpret an evaluated value as a set of node/edge delete targets.
fn value_to_delete_targets(val: &serde_json::Value) -> Result<Vec<GraphBinding>, String> {
    match val {
        serde_json::Value::Null => Ok(vec![]),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::new();
            for item in arr {
                out.extend(value_to_delete_targets(item)?);
            }
            Ok(out)
        }
        serde_json::Value::Object(obj) => match obj.get("__type__").and_then(|t| t.as_str()) {
            Some("__Node__") => {
                let id = obj
                    .get("id")
                    .and_then(|i| i.as_i64())
                    .ok_or("DELETE: malformed node value")?;
                Ok(vec![GraphBinding::Node(id as u64)])
            }
            Some("__Edge__") => {
                let id = obj
                    .get("id")
                    .and_then(|i| i.as_i64())
                    .ok_or("DELETE: malformed relationship value")?;
                Ok(vec![GraphBinding::Edge(id as u64)])
            }
            Some("__Path__") => {
                // Deleting a path deletes its relationships and then its nodes.
                let mut out = Vec::new();
                if let Some(serde_json::Value::Array(rels)) = obj.get("relationships") {
                    for r in rels {
                        out.extend(value_to_delete_targets(r)?);
                    }
                }
                if let Some(serde_json::Value::Array(nodes)) = obj.get("nodes") {
                    for n in nodes {
                        out.extend(value_to_delete_targets(n)?);
                    }
                }
                Ok(out)
            }
            _ => Err("TypeError: DELETE expects a node, relationship, or path".into()),
        },
        _ => Err("TypeError: DELETE expects a node, relationship, or path".into()),
    }
}

/// Resolve every DELETE target expression for one row and delete the elements.
///
/// Relationships are deleted before nodes so that `DELETE n, r` (where r is one of
/// n's relationships) succeeds. Without DETACH, deleting a node that still has any
/// relationship is an error (openCypher `DeleteConnectedNode`).
pub(super) fn apply_delete_targets(
    graph: &Graph,
    path: &PathMap,
    targets: &[Expr],
    detach: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    delete_over_paths(graph, std::slice::from_ref(path), targets, detach, params)
}

/// Delete the targets of a DELETE clause over the full set of matched rows.
///
/// openCypher evaluates DELETE over the whole result before applying it: all
/// listed relationships are removed first, then all listed nodes. Collecting
/// across every row (rather than per row) is what makes `MATCH (a)-[r]-(b) DELETE
/// r, a, b` succeed, since an undirected expand binds the same edge and nodes in
/// more than one row. Without DETACH, a node that still has a relationship after
/// the listed relationships are gone is an error (`DeleteConnectedNode`).
pub(super) fn delete_over_paths(
    graph: &Graph,
    paths: &[PathMap],
    targets: &[Expr],
    detach: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    let mut nodes: Vec<NodeId> = Vec::new();
    let mut edges: Vec<EdgeId> = Vec::new();
    let mut seen_nodes = std::collections::HashSet::new();
    let mut seen_edges = std::collections::HashSet::new();
    for path in paths {
        for target in targets {
            for binding in collect_delete_targets(graph, path, target, params)? {
                match binding {
                    GraphBinding::Node(id) if seen_nodes.insert(id) => nodes.push(id),
                    GraphBinding::Edge(id) if seen_edges.insert(id) => edges.push(id),
                    _ => {}
                }
            }
        }
    }

    // Relationships first: idempotent, and clears edges that connect nodes also
    // being deleted in the same clause.
    for eid in &edges {
        graph.delete_edge(*eid).map_err(|e| e.to_string())?;
    }

    for nid in nodes {
        if detach {
            for ne in graph.out_neighbors(nid).map_err(|e| e.to_string())? {
                graph.delete_edge(ne.edge).map_err(|e| e.to_string())?;
            }
            for ne in graph.in_neighbors(nid).map_err(|e| e.to_string())? {
                graph.delete_edge(ne.edge).map_err(|e| e.to_string())?;
            }
        } else if graph
            .node_has_relationships(nid)
            .map_err(|e| e.to_string())?
        {
            return Err("ConstraintVerificationFailed: DeleteConnectedNode: Cannot delete node, because it still has relationships. To delete this node, you must first delete its relationships.".to_string());
        }
        graph.delete_node(nid).map_err(|e| e.to_string())?;
    }

    Ok(())
}

pub(super) fn execute_delete_and_return(
    graph: &Graph,
    stmt: &DeleteAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph.with_write_lock(|| {
        // Build a synthetic read query for MATCH + WHERE.
        let synthetic_query = Query {
            match_clauses: stmt.match_clauses.clone(),
            where_clause: stmt.where_clause.clone(),
            return_clause: ReturnClause {
                items: stmt.return_clause.items.clone(),
                distinct: stmt.return_clause.distinct,
            },
            parts: Vec::new(),
            order_by: stmt.order_by.clone(),
            skip: stmt.skip.clone(),
            limit: stmt.limit.clone(),
        };
        // Execute the read to get RETURN results before deletion.
        let read_result = execute_read_query(graph, &synthetic_query, params)?;

        // Now perform the deletion using the same match.
        let binding_query = Query {
            match_clauses: stmt.match_clauses.clone(),
            where_clause: stmt.where_clause.clone(),
            return_clause: ReturnClause {
                items: vec![],
                distinct: false,
            },
            parts: Vec::new(),
            order_by: None,
            skip: None,
            limit: None,
        };
        let logical = LogicalPlanner::plan(&binding_query).map_err(|e| e.to_string())?;
        let physical = PhysicalPlanner::plan(&logical);
        let optimized = Optimizer::optimize(physical, Some(graph));
        let binding_plan = match optimized {
            PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
            other => other,
        };
        let bound_paths = execute_physical(graph, &binding_plan, params)?;

        delete_over_paths(graph, &bound_paths, &stmt.targets, stmt.detach, params)?;

        Ok(read_result)
    })
}

#[instrument(skip_all)]
pub(super) fn execute_merge(
    graph: &Graph,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph.with_write_lock(|| execute_merge_inner(graph, stmt, params))
}

pub(super) fn execute_merge_inner(
    graph: &Graph,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let empty = super::PathMap::new();
    execute_merge_internal_with_context(graph, stmt, &empty, params)?;
    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_merge_and_return(
    graph: &Graph,
    stmt: &MergeAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Route through the read-query pipeline with the MERGE clauses as a write
    // part. Each MERGE binds its pattern variables into the row, then the RETURN
    // projection (including aggregation, ORDER BY, SKIP, and LIMIT) operates on
    // those rows. The whole operation runs under the reentrant write lock so the
    // match-or-create decision and the create are atomic.
    let synthetic_query = Query {
        match_clauses: Vec::new(),
        where_clause: None,
        return_clause: stmt.return_clause.clone(),
        parts: vec![QueryPart::Merge {
            merges: stmt.merges.clone(),
        }],
        order_by: stmt.order_by.clone(),
        skip: stmt.skip.clone(),
        limit: stmt.limit.clone(),
    };
    graph.with_write_lock(|| execute_read_query(graph, &synthetic_query, params))
}

/// Apply a single REMOVE item to the element bound in `path`.
///
/// Handles property removal from nodes and relationships and label removal from
/// nodes. A null or unbound variable is a no-op, matching openCypher's treatment
/// of REMOVE over an OPTIONAL MATCH that produced no row.
pub(super) fn apply_remove_item(
    graph: &Graph,
    item: &RemoveItem,
    path: &super::PathMap,
) -> Result<(), String> {
    match item {
        RemoveItem::Property { variable, property } => match path.get(variable) {
            Some(GraphBinding::Node(id)) => {
                let record = graph
                    .get_node(*id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("node not found: {}", id))?;
                let mut props: serde_json::Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                if let Some(obj) = props.as_object_mut() {
                    obj.remove(property);
                }
                graph.update_node(*id, &props).map_err(|e| e.to_string())?;
            }
            Some(GraphBinding::Edge(id)) => {
                let record = graph
                    .get_edge(*id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge not found: {}", id))?;
                let mut props: serde_json::Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                if let Some(obj) = props.as_object_mut() {
                    obj.remove(property);
                }
                graph.update_edge(*id, &props).map_err(|e| e.to_string())?;
            }
            _ => {} // null, unbound, or scalar: silently skip.
        },
        RemoveItem::Label { variable, label } => {
            if let Some(GraphBinding::Node(id)) = path.get(variable) {
                graph.remove_label(*id, label).map_err(|e| e.to_string())?;
            }
            // null or unbound variable: silently skip.
        }
    }
    Ok(())
}

pub(super) fn execute_remove(
    graph: &Graph,
    stmt: &RemoveStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let synthetic_query = Query {
        match_clauses: stmt.match_clauses.clone(),
        where_clause: stmt.where_clause.clone(),
        return_clause: ReturnClause {
            items: vec![],
            distinct: false,
        },
        parts: Vec::new(),
        order_by: None,
        skip: None,
        limit: None,
    };
    let logical = LogicalPlanner::plan(&synthetic_query).map_err(|e| e.to_string())?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    for path in bound_paths {
        for item in &stmt.items {
            apply_remove_item(graph, item, &path)?;
        }
    }

    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_remove_and_return(
    graph: &Graph,
    stmt: &RemoveAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Route through the read-query pipeline with the REMOVE as a write part, so the
    // RETURN observes the post-removal state while the matched rows (and therefore the
    // result cardinality) are fixed at MATCH time. Re-matching would wrongly drop rows
    // whose matched label was just removed.
    let synthetic_query = Query {
        match_clauses: Vec::new(),
        where_clause: None,
        return_clause: stmt.return_clause.clone(),
        parts: vec![
            QueryPart::Match {
                match_clauses: stmt.match_clauses.clone(),
                where_clause: stmt.where_clause.clone(),
            },
            QueryPart::Remove {
                items: stmt.items.clone(),
            },
        ],
        order_by: stmt.order_by.clone(),
        skip: stmt.skip.clone(),
        limit: stmt.limit.clone(),
    };
    execute_read_query(graph, &synthetic_query, params)
}

pub(super) fn execute_foreach(
    graph: &Graph,
    stmt: &ForeachStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let list_val = evaluate_expr(graph, &PathMap::new(), &stmt.list, params)?;
    let items = match list_val {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Null => vec![],
        other => vec![other],
    };

    for element in items {
        let mut inner_params = params.clone();
        inner_params.insert(stmt.variable.clone(), element);
        for body_stmt in &stmt.body {
            execute_foreach_body(graph, body_stmt, &inner_params)?;
        }
    }

    Ok(QueryResult {
        columns: vec![],
        records: vec![],
    })
}

fn execute_foreach_body(
    graph: &Graph,
    stmt: &crate::ast::Statement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    use crate::ast::Statement;
    match stmt {
        Statement::Create(c) => {
            for pattern in &c.patterns {
                execute_create_internal(graph, pattern)?;
            }
        }
        Statement::Set(s) => {
            execute_set(graph, s, params)?;
        }
        Statement::Delete(d) => {
            execute_delete(graph, d, params)?;
        }
        Statement::Merge(m) => {
            execute_merge_inner(graph, m, params)?;
        }
        Statement::Remove(r) => {
            execute_remove(graph, r, params)?;
        }
        // Write-only pipeline query (e.g., standalone CREATE parsed as Statement::Query).
        Statement::Query(q) if q.return_clause.items.is_empty() => {
            super::read::execute_read_query(graph, q, params).map_err(|e| e.to_string())?;
        }
        _ => {
            return Err("unsupported statement in FOREACH body".into());
        }
    }
    Ok(())
}

pub(super) fn apply_set_items(
    graph: &Graph,
    path: &PathMap,
    set_items: &[SetItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    for item in set_items {
        apply_set_item(graph, path, item, params)?;
    }
    Ok(())
}

/// An element a SET/REMOVE item resolves to: a node, an edge, or nothing
/// (a null or unbound variable, which makes the operation a no-op).
enum SetTarget {
    Node(NodeId),
    Edge(EdgeId),
    Skip,
}

/// Resolve a SET/REMOVE target variable. Handles graph bindings and the
/// `__Node__`/`__Edge__` scalar representations produced by WITH projections.
fn resolve_set_target(path: &PathMap, variable: &str) -> Result<SetTarget, String> {
    match path.get(variable) {
        Some(GraphBinding::Node(id)) => Ok(SetTarget::Node(*id)),
        Some(GraphBinding::Edge(id)) => Ok(SetTarget::Edge(*id)),
        Some(GraphBinding::Scalar(val)) => {
            if val.is_null() {
                return Ok(SetTarget::Skip);
            }
            if let Some(obj) = val.as_object() {
                match obj.get("__type__").and_then(|t| t.as_str()) {
                    Some("__Node__") => {
                        if let Some(id) = obj.get("id").and_then(|i| i.as_i64()) {
                            return Ok(SetTarget::Node(id as u64));
                        }
                    }
                    Some("__Edge__") => {
                        if let Some(id) = obj.get("id").and_then(|i| i.as_i64()) {
                            return Ok(SetTarget::Edge(id as u64));
                        }
                    }
                    _ => {}
                }
            }
            Err(format!(
                "SET on scalar variable '{}' is not supported",
                variable
            ))
        }
        None => Err(format!("unbound variable: {}", variable)),
    }
}

/// Apply a single SET item: a property assignment (on a node or edge) or a
/// label addition (on a node).
pub(super) fn apply_set_item(
    graph: &Graph,
    path: &PathMap,
    item: &SetItem,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    match item {
        SetItem::Property {
            variable,
            property,
            expr,
        } => {
            let new_val = evaluate_expr(graph, path, expr, params)?;
            match resolve_set_target(path, variable)? {
                SetTarget::Node(nid) => {
                    let record = graph
                        .get_node(nid)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("node not found: {}", nid))?;
                    let mut props: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if let Some(obj) = props.as_object_mut() {
                        obj.insert(property.clone(), new_val);
                    }
                    graph.update_node(nid, &props).map_err(|e| e.to_string())?;
                }
                SetTarget::Edge(eid) => {
                    let record = graph
                        .get_edge(eid)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("edge not found: {}", eid))?;
                    let mut props: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if let Some(obj) = props.as_object_mut() {
                        obj.insert(property.clone(), new_val);
                    }
                    graph.update_edge(eid, &props).map_err(|e| e.to_string())?;
                }
                SetTarget::Skip => {}
            }
        }
        SetItem::Labels { variable, labels } => {
            if let SetTarget::Node(nid) = resolve_set_target(path, variable)? {
                for label in labels {
                    graph.add_label(nid, label).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(())
}

/// Create a node pattern with properties evaluated using an expression context (PathMap).
///
/// This function evaluates property expressions using the provided path bindings, which
/// allows referencing pipeline variables (e.g., `CREATE (n {num: x})` where `x` is from UNWIND).
pub(super) fn execute_create_internal_with_context(
    graph: &Graph,
    pattern: &Pattern,
    path: &super::PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<super::PathMap, String> {
    let mut bindings = super::PathMap::new();
    let mut combined_path = path.clone();

    // Evaluate the seed node's properties.
    let seed_id = if let Some(ref var_name) = pattern.node.variable {
        if let Some(GraphBinding::Node(existing_id)) = combined_path.get(var_name.as_str()) {
            *existing_id
        } else {
            let props_map = if let Some(ref props) = pattern.node.properties {
                eval_properties(graph, props, &combined_path, params)?
            } else {
                serde_json::Value::Object(serde_json::Map::new())
            };
            let labels: Vec<&str> = pattern.node.labels.iter().map(|s| s.as_str()).collect();
            graph
                .add_node_multi(&labels, &props_map)
                .map_err(|e| e.to_string())?
        }
    } else {
        let props_map = if let Some(ref props) = pattern.node.properties {
            eval_properties(graph, props, &combined_path, params)?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };
        let labels: Vec<&str> = pattern.node.labels.iter().map(|s| s.as_str()).collect();
        graph
            .add_node_multi(&labels, &props_map)
            .map_err(|e| e.to_string())?
    };

    if let Some(ref var_name) = pattern.node.variable {
        bindings.insert(var_name.clone(), GraphBinding::Node(seed_id));
        combined_path.insert(var_name.clone(), GraphBinding::Node(seed_id));
    }

    let mut created_node_id = seed_id;

    for (rel_pat, node_pat) in &pattern.rels {
        // Check if the target node is an existing variable.
        let target_id = if let Some(ref var) = node_pat.variable {
            if let Some(GraphBinding::Node(existing_id)) = combined_path.get(var.as_str()) {
                // Reuse an existing node bound in this context.
                *existing_id
            } else {
                // Create a new target node.
                let target_props = if let Some(ref props) = node_pat.properties {
                    eval_properties(graph, props, &combined_path, params)?
                } else {
                    serde_json::Value::Object(serde_json::Map::new())
                };
                let labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
                let tid = graph
                    .add_node_multi(&labels, &target_props)
                    .map_err(|e| e.to_string())?;
                bindings.insert(var.clone(), GraphBinding::Node(tid));
                combined_path.insert(var.clone(), GraphBinding::Node(tid));
                tid
            }
        } else {
            // Anonymous target node.
            let target_props = if let Some(ref props) = node_pat.properties {
                eval_properties(graph, props, &combined_path, params)?
            } else {
                serde_json::Value::Object(serde_json::Map::new())
            };
            let labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
            graph
                .add_node_multi(&labels, &target_props)
                .map_err(|e| e.to_string())?
        };

        let rel_type = rel_pat
            .rel_type
            .clone()
            .unwrap_or_else(|| "EDGE".to_string());

        let rel_props = if let Some(ref props) = rel_pat.properties {
            eval_properties(graph, props, &combined_path, params)?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };

        let edge_id = if rel_pat.is_incoming {
            graph
                .add_edge(target_id, created_node_id, &rel_type, &rel_props)
                .map_err(|e| e.to_string())?
        } else {
            graph
                .add_edge(created_node_id, target_id, &rel_type, &rel_props)
                .map_err(|e| e.to_string())?
        };

        if let Some(ref var_name) = rel_pat.variable {
            bindings.insert(var_name.clone(), GraphBinding::Edge(edge_id));
            combined_path.insert(var_name.clone(), GraphBinding::Edge(edge_id));
        }

        created_node_id = target_id;
    }

    Ok(bindings)
}

/// Read a node's stored properties as a JSON object, returning an empty object
/// when the node is missing or has no properties.
fn node_props_json(graph: &Graph, id: NodeId) -> Result<serde_json::Value, String> {
    match graph.get_node(id).map_err(|e| e.to_string())? {
        Some(record) => rmp_serde::from_slice(&record.props).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

/// Read an edge's stored properties as a JSON object.
fn edge_props_json(graph: &Graph, id: EdgeId) -> Result<serde_json::Value, String> {
    match graph.get_edge(id).map_err(|e| e.to_string())? {
        Some(record) => rmp_serde::from_slice(&record.props).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

/// True when every key in `filter` is present on the element with an equal value.
fn props_subset_match(filter: &serde_json::Value, actual: &serde_json::Value) -> bool {
    let (Some(filter_obj), Some(actual_obj)) = (filter.as_object(), actual.as_object()) else {
        return filter.as_object().map(|o| o.is_empty()).unwrap_or(true);
    };
    filter_obj
        .iter()
        .all(|(k, v)| actual_obj.get(k).is_some_and(|a| a == v))
}

/// True when a node carries all of the required labels.
fn node_has_labels(graph: &Graph, id: NodeId, labels: &[String]) -> Result<bool, String> {
    if labels.is_empty() {
        return Ok(true);
    }
    let actual = graph.node_labels(id).map_err(|e| e.to_string())?;
    Ok(labels.iter().all(|l| actual.contains(l)))
}

/// True when a node satisfies both the label and property constraints of a pattern.
fn node_matches(
    graph: &Graph,
    id: NodeId,
    labels: &[String],
    props: &serde_json::Value,
) -> Result<bool, String> {
    if !node_has_labels(graph, id, labels)? {
        return Ok(false);
    }
    Ok(props_subset_match(props, &node_props_json(graph, id)?))
}

/// Enumerate candidate nodes that satisfy a node pattern's label and property
/// constraints. With no labels, this scans all nodes; otherwise it scans the
/// first label and filters the rest.
fn candidate_nodes(
    graph: &Graph,
    labels: &[String],
    props: &serde_json::Value,
) -> Result<Vec<NodeId>, String> {
    let seed: Vec<NodeId> = if let Some(first) = labels.first() {
        graph.nodes_by_label(first).map_err(|e| e.to_string())?
    } else {
        graph.all_nodes().map_err(|e| e.to_string())?
    };
    let mut out = Vec::new();
    for id in seed {
        if node_matches(graph, id, labels, props)? {
            out.push(id);
        }
    }
    Ok(out)
}

/// Find existing matches for a MERGE pattern given the surrounding context. Each
/// returned `PathMap` extends the pattern's variables (relationship and node
/// variables included) over one full match. An empty result means the pattern
/// does not yet exist and the caller should create it.
fn merge_match(
    graph: &Graph,
    pattern: &Pattern,
    ctx: &super::PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<super::PathMap>, String> {
    // Seed candidates for the first node in the chain.
    let seed_props = match &pattern.node.properties {
        Some(p) => eval_properties(graph, p, ctx, params)?,
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    let seed_candidates: Vec<NodeId> =
        match pattern.node.variable.as_deref().and_then(|v| ctx.get(v)) {
            Some(GraphBinding::Node(id)) => vec![*id],
            _ => candidate_nodes(graph, &pattern.node.labels, &seed_props)?,
        };

    // Each partial carries the pattern bindings accumulated so far and the
    // current chain endpoint.
    let mut partials: Vec<(super::PathMap, NodeId)> = Vec::new();
    for nid in seed_candidates {
        let mut pm = super::PathMap::new();
        if let Some(v) = &pattern.node.variable {
            pm.insert(v.clone(), GraphBinding::Node(nid));
        }
        partials.push((pm, nid));
    }

    for (rel_pat, node_pat) in &pattern.rels {
        let rel_types: Vec<&str> = rel_pat
            .rel_type
            .as_deref()
            .map(|t| t.split('|').collect())
            .unwrap_or_default();
        let mut next: Vec<(super::PathMap, NodeId)> = Vec::new();
        for (pm, cur) in &partials {
            let mut combined = ctx.clone();
            for (k, v) in pm {
                combined.insert(k.clone(), v.clone());
            }
            let rel_props = match &rel_pat.properties {
                Some(p) => eval_properties(graph, p, &combined, params)?,
                None => serde_json::Value::Object(serde_json::Map::new()),
            };
            let tgt_props = match &node_pat.properties {
                Some(p) => eval_properties(graph, p, &combined, params)?,
                None => serde_json::Value::Object(serde_json::Map::new()),
            };
            let bound_target = node_pat
                .variable
                .as_deref()
                .and_then(|v| combined.get(v))
                .and_then(|b| match b {
                    GraphBinding::Node(id) => Some(*id),
                    _ => None,
                });

            let neighbors = graph.all_neighbors(*cur).map_err(|e| e.to_string())?;
            for n in neighbors {
                // Direction filter: undirected accepts both, otherwise the edge
                // orientation must match the pattern.
                if !rel_pat.is_undirected {
                    let want_outgoing = !rel_pat.is_incoming;
                    if n.outgoing != want_outgoing {
                        continue;
                    }
                }
                if !rel_types.is_empty() {
                    let tn = graph
                        .type_name(n.edge_type)
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default();
                    if !rel_types.contains(&tn.as_str()) {
                        continue;
                    }
                }
                if !props_subset_match(&rel_props, &edge_props_json(graph, n.edge)?) {
                    continue;
                }
                match bound_target {
                    Some(b) if b != n.node => continue,
                    None if !node_matches(graph, n.node, &node_pat.labels, &tgt_props)? => continue,
                    _ => {}
                }
                let mut npm = pm.clone();
                if let Some(rv) = &rel_pat.variable {
                    npm.insert(rv.clone(), GraphBinding::Edge(n.edge));
                }
                if let Some(tv) = &node_pat.variable {
                    npm.insert(tv.clone(), GraphBinding::Node(n.node));
                }
                next.push((npm, n.node));
            }
        }
        partials = next;
    }

    Ok(partials.into_iter().map(|(pm, _)| pm).collect())
}

/// Match-or-create a MERGE pattern within the given context. Returns one binding
/// extension per matched row, or a single extension for the freshly created
/// pattern. ON MATCH / ON CREATE SET actions are applied with the combined
/// context so they can reference both incoming and pattern variables.
pub(super) fn execute_merge_internal_with_context(
    graph: &Graph,
    stmt: &crate::ast::MergeStatement,
    path: &super::PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<super::PathMap>, String> {
    let matches = merge_match(graph, &stmt.pattern, path, params)?;
    if !matches.is_empty() {
        for m in &matches {
            if !stmt.on_match_set.is_empty() {
                let mut combined = path.clone();
                for (k, v) in m {
                    combined.insert(k.clone(), v.clone());
                }
                apply_set_items(graph, &combined, &stmt.on_match_set, params)?;
            }
        }
        Ok(matches)
    } else {
        let created = execute_create_internal_with_context(graph, &stmt.pattern, path, params)?;
        if !stmt.on_create_set.is_empty() {
            let mut combined = path.clone();
            for (k, v) in &created {
                combined.insert(k.clone(), v.clone());
            }
            apply_set_items(graph, &combined, &stmt.on_create_set, params)?;
        }
        Ok(vec![created])
    }
}
