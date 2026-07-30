use super::expr::evaluate_expr;
use super::read::{
    binding_to_value, column_name, execute_physical, execute_physical_pathmaps, execute_read_query,
    projected_key,
};
use super::row::{Bindings, SlotSchema};
use super::*;
use crate::ast::{
    CreateAndReturnStatement, DeleteAndReturnStatement, ForeachStatement, MergeAndReturnStatement,
    QueryPart, RemoveAndReturnStatement, RemoveItem, RemoveStatement, SetAndReturnStatement,
};

/// Evaluate a property map `HashMap<String, Expr>` using the given path context.
/// Returns a JSON object containing the evaluated properties.
fn eval_properties(
    txn: &issundb_core::WriteTxn,
    props: &HashMap<String, crate::ast::Expr>,
    path: &PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for (k, v) in props {
        let val = evaluate_expr(txn.graph(), path, v, params)?;
        // Cypher semantics: assigning null to a property is equivalent to
        // removing it; null-valued properties are not stored.
        if val != serde_json::Value::Null {
            obj.insert(k.clone(), val);
        }
    }
    Ok(serde_json::Value::Object(obj))
}

/// Convert a Cypher-layer `String` error into an `issundb_core::Error` so it
/// can propagate out of a `Graph::update` closure via `?` (which requires
/// `Result<_, issundb_core::Error>`). Pair with `unwrap_txn_error` once
/// `update` returns.
pub(super) fn as_txn_error(msg: String) -> issundb_core::Error {
    issundb_core::Error::InvalidArgument(msg)
}

/// Unwrap the message a `Graph::update` closure produced via `as_txn_error`
/// back to its original Cypher-layer string. A genuine `issundb_core::Error`
/// from a `WriteTxn` mutation (a real constraint violation, a missing node,
/// and so on) is not produced via `as_txn_error` anywhere in this module's
/// closures, so `Display` is the correct message for every other variant.
pub(super) fn unwrap_txn_error(err: issundb_core::Error) -> String {
    match err {
        issundb_core::Error::InvalidArgument(msg) => msg,
        other => other.to_string(),
    }
}

pub(super) fn execute_create_internal(
    txn: &mut issundb_core::WriteTxn,
    pattern: &Pattern,
) -> Result<PathMap, String> {
    execute_create_internal_with_context(txn, pattern, &PathMap::new(), &HashMap::new())
}

pub(super) fn execute_create(
    graph: &Graph,
    create: &CreateStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            // Thread bindings across patterns so that variables created in one
            // pattern (e.g., the first node in `CREATE (a:A), (a)-[:R]->(b:B)`)
            // are available in subsequent patterns within the same CREATE
            // clause. All patterns share this one transaction, so a later
            // pattern's failure (e.g. a unique constraint violation) rolls
            // back every node/edge created earlier in the same clause.
            let mut shared_bindings = PathMap::new();
            for pattern in &create.patterns {
                let created =
                    execute_create_internal_with_context(txn, pattern, &shared_bindings, params)
                        .map_err(as_txn_error)?;
                shared_bindings.extend(created);
            }
            Ok(QueryResult {
                statement_count: 1,
                columns: vec![],
                records: vec![],
            })
        })
        .map_err(unwrap_txn_error)
}

pub(super) fn execute_create_and_return(
    graph: &Graph,
    stmt: &CreateAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            // Thread bindings across patterns within the same CREATE clause.
            let mut bindings = PathMap::new();
            for pattern in &stmt.patterns {
                let created = execute_create_internal_with_context(txn, pattern, &bindings, params)
                    .map_err(as_txn_error)?;
                bindings.extend(created);
            }

            // Project the RETURN clause over the created bindings, still
            // inside this transaction: a failure here (e.g. a division by
            // zero) rolls back the CREATE above rather than leaving it
            // committed with no visible result.
            let columns: Vec<String> = stmt.return_clause.items.iter().map(column_name).collect();

            let mut values = Vec::new();
            for item in &stmt.return_clause.items {
                let key = projected_key(&item.expr, &item.alias);
                let val = evaluate_expr(txn.graph(), &bindings, &item.expr, params)
                    .map_err(as_txn_error)?;
                let val = if val == serde_json::Value::Null {
                    if let Some(binding) = bindings.get(&key) {
                        binding_to_value(txn.graph(), Some(binding))
                            .unwrap_or(serde_json::Value::Null)
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

            let mut result_records = records;
            if let Some(skip_expr) = &stmt.skip {
                let skip_val = evaluate_expr(txn.graph(), &PathMap::new(), skip_expr, params)
                    .map_err(as_txn_error)?;
                let skip = skip_val.as_i64().unwrap_or(0).max(0) as usize;
                result_records = result_records.into_iter().skip(skip).collect();
            }
            if let Some(limit_expr) = &stmt.limit {
                let limit_val = evaluate_expr(txn.graph(), &PathMap::new(), limit_expr, params)
                    .map_err(as_txn_error)?;
                let limit = limit_val.as_i64().unwrap_or(0).max(0) as usize;
                result_records = result_records.into_iter().take(limit).collect();
            }

            Ok(QueryResult {
                statement_count: 1,
                columns,
                records: result_records,
            })
        })
        .map_err(unwrap_txn_error)
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
    // The whole statement (SET and RETURN) shares one transaction, so a failing RETURN
    // expression rolls back the SET.
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
    graph
        .update(|txn| {
            execute_read_query(graph, &synthetic_query, params, Some(txn)).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

pub(super) fn execute_set(
    graph: &Graph,
    set: &SetStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            execute_set_in(txn, set, params).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

/// Core SET logic, run inside an already-open write transaction. Used both by
/// [`execute_set`] above (which opens its own transaction) and by a SET
/// statement in a FOREACH body (which is already inside one, opened by the
/// enclosing `execute_foreach`); LMDB permits only one open writer
/// transaction at a time, so this must never open a second one itself.
pub(super) fn execute_set_in(
    txn: &mut issundb_core::WriteTxn,
    set: &SetStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Build a synthetic read query so the MATCH and WHERE clauses go through the
    // same planner and executor pipeline as MATCH ... RETURN queries.
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
    let optimized = Optimizer::optimize(physical, Some(txn.graph()));
    // `LogicalPlanner` always wraps the plan in a final Project. When the
    // RETURN clause is empty (as in this synthetic query) that Project would
    // produce PathMaps with no variables at all, making SET assignments
    // impossible. Strip the zero-item Project so SET can see matched variables.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths =
        execute_physical_pathmaps(txn.graph(), &binding_plan, params, Some(&mut *txn))?;

    for path in bound_paths {
        apply_set_items(txn, &path, &set.set_items, params)?;
    }

    Ok(QueryResult {
        statement_count: 1,
        columns: vec![],
        records: vec![],
    })
}

pub(super) fn execute_delete(
    graph: &Graph,
    delete: &DeleteStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph
        .update(|txn| execute_delete_in(txn, delete, params).map_err(as_txn_error))
        .map_err(unwrap_txn_error)
}

/// Core DELETE logic (matching plus the delete itself), run inside an
/// already-open write transaction. Used both by [`execute_delete`] above
/// (which opens its own transaction) and by a DELETE statement in a FOREACH
/// body (which is already inside one, opened by the enclosing
/// `execute_foreach`); LMDB permits only one open writer transaction at a
/// time, so this must never open a second one itself.
pub(super) fn execute_delete_in(
    txn: &mut issundb_core::WriteTxn,
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
    let optimized = Optimizer::optimize(physical, Some(txn.graph()));
    // Strip the zero-item Project that LogicalPlanner adds for the empty
    // RETURN clause; otherwise execute_physical clears all matched variables.
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths =
        execute_physical_pathmaps(txn.graph(), &binding_plan, params, Some(&mut *txn))?;

    delete_over_paths_in(txn, &bound_paths, &delete.targets, delete.detach, params)?;

    Ok(QueryResult {
        statement_count: 1,
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
                // A variable-length relationship variable is a list; deleting it
                // deletes each relationship in the trail.
                Some(GraphBinding::EdgeList(ids)) => {
                    return Ok(ids.iter().map(|id| GraphBinding::Edge(*id)).collect());
                }
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
    txn: &mut issundb_core::WriteTxn,
    path: &PathMap,
    targets: &[Expr],
    detach: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    delete_over_paths_in(txn, std::slice::from_ref(path), targets, detach, params)
}

/// Delete the targets of a DELETE clause over the full set of matched rows,
/// inside the write transaction the enclosing statement already opened.
///
/// openCypher evaluates DELETE over the whole result before applying it: all
/// listed relationships are removed first, then all listed nodes. Collecting
/// across every row (rather than per row) is what makes `MATCH (a)-[r]-(b) DELETE
/// r, a, b` succeed, since an undirected expand binds the same edge and nodes in
/// more than one row. Without DETACH, a node that still has a relationship after
/// the listed relationships are gone is an error (`DeleteConnectedNode`).
/// LMDB permits only one open writer transaction at a time, so this must
/// never open a second one itself.
pub(super) fn delete_over_paths_in(
    txn: &mut issundb_core::WriteTxn,
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
            for binding in collect_delete_targets(txn.graph(), path, target, params)? {
                match binding {
                    GraphBinding::Node(id) if seen_nodes.insert(id) => nodes.push(id),
                    GraphBinding::Edge(id) if seen_edges.insert(id) => edges.push(id),
                    _ => {}
                }
            }
        }
    }

    // Relationships first: clears edges that connect nodes also being deleted
    // in the same clause.
    for eid in &edges {
        txn.delete_edge(*eid).map_err(|e| e.to_string())?;
    }
    for nid in &nodes {
        if detach {
            for ne in txn.out_neighbors(*nid).map_err(|e| e.to_string())? {
                txn.delete_edge(ne.edge).map_err(|e| e.to_string())?;
            }
            for ne in txn.in_neighbors(*nid).map_err(|e| e.to_string())? {
                txn.delete_edge(ne.edge).map_err(|e| e.to_string())?;
            }
        } else if !txn
            .out_neighbors(*nid)
            .map_err(|e| e.to_string())?
            .is_empty()
            || !txn
                .in_neighbors(*nid)
                .map_err(|e| e.to_string())?
                .is_empty()
        {
            return Err(DELETE_CONNECTED_NODE_MSG.to_string());
        }
        txn.delete_node(*nid).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// openCypher error for deleting a node that still has relationships.
const DELETE_CONNECTED_NODE_MSG: &str = "ConstraintVerificationFailed: DeleteConnectedNode: Cannot delete node, because it still has relationships. To delete this node, you must first delete its relationships.";

pub(super) fn execute_delete_and_return(
    graph: &Graph,
    stmt: &DeleteAndReturnStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // One transaction covers the RETURN pass, the binding pass, and the
    // delete itself, so no concurrent writer can slip in between the rows
    // reported by RETURN and the rows actually deleted, and a failing delete
    // (a still-connected node without DETACH) leaves nothing behind. The two
    // read passes run before any write in this transaction, so their reads of
    // committed state are exactly the statement's snapshot.
    graph
        .update(|txn| {
            let return_query = Query {
                match_clauses: stmt.match_clauses.clone(),
                where_clause: stmt.where_clause.clone(),
                return_clause: stmt.return_clause.clone(),
                parts: Vec::new(),
                order_by: stmt.order_by.clone(),
                skip: stmt.skip.clone(),
                limit: stmt.limit.clone(),
            };
            let return_result =
                execute_read_query(graph, &return_query, params, None).map_err(as_txn_error)?;

            let binding_query = Query {
                match_clauses: stmt.match_clauses.clone(),
                where_clause: stmt.where_clause.clone(),
                return_clause: ReturnClause {
                    items: vec![],
                    distinct: false,
                },
                parts: Vec::new(),
                // DELETE affects every matched element; ORDER BY, SKIP, and LIMIT
                // restrict only the RETURN projection (computed above in
                // `return_query`), not the set of rows fed to the delete.
                order_by: None,
                skip: None,
                limit: None,
            };
            let logical =
                LogicalPlanner::plan(&binding_query).map_err(|e| as_txn_error(e.to_string()))?;
            let physical = PhysicalPlanner::plan(&logical);
            let optimized = Optimizer::optimize(physical, Some(graph));
            let binding_plan = match &optimized {
                PhysicalOperator::Project { input, items, .. } if items.is_empty() => {
                    input.as_ref().clone()
                }
                other => other.clone(),
            };

            let schema = std::sync::Arc::new(SlotSchema::from_plan(&binding_plan));
            let bound_rows = execute_physical(graph, &binding_plan, params, &schema, None)
                .map_err(as_txn_error)?;

            let bound_paths: Vec<PathMap> = bound_rows.iter().map(|r| r.to_path_map()).collect();
            delete_over_paths_in(txn, &bound_paths, &stmt.targets, stmt.detach, params)
                .map_err(as_txn_error)?;

            Ok(return_result)
        })
        .map_err(unwrap_txn_error)
}

#[instrument(skip_all)]
pub(super) fn execute_merge(
    graph: &Graph,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            execute_merge_inner(txn, stmt, params).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

pub(super) fn execute_merge_inner(
    txn: &mut issundb_core::WriteTxn,
    stmt: &MergeStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let empty = super::PathMap::new();
    execute_merge_internal_with_context(txn, stmt, &empty, params)?;
    Ok(QueryResult {
        statement_count: 1,
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
    // those rows. The whole statement shares one transaction, so the
    // match-or-create decision, the create, and the RETURN are all atomic.
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
    graph
        .update(|txn| {
            execute_read_query(graph, &synthetic_query, params, Some(txn)).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

/// Apply a single REMOVE item to the element bound in `path`.
///
/// Handles property removal from nodes and relationships and label removal from
/// nodes. A null or unbound variable is a no-op, matching openCypher's treatment
/// of REMOVE over an OPTIONAL MATCH that produced no row.
pub(super) fn apply_remove_item(
    txn: &mut issundb_core::WriteTxn,
    item: &RemoveItem,
    path: &super::PathMap,
) -> Result<(), String> {
    match item {
        RemoveItem::Property { variable, property } => match path.get(variable) {
            Some(GraphBinding::Node(id)) => {
                let record = txn
                    .get_node(*id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("node not found: {}", id))?;
                let mut props: serde_json::Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                if let Some(obj) = props.as_object_mut() {
                    obj.remove(property);
                }
                txn.update_node(*id, &props).map_err(|e| e.to_string())?;
                expr::PendingWrites::update_node_props(txn, *id, props);
            }
            Some(GraphBinding::Edge(id)) => {
                let record = txn
                    .get_edge(*id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge not found: {}", id))?;
                let mut props: serde_json::Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                if let Some(obj) = props.as_object_mut() {
                    obj.remove(property);
                }
                txn.update_edge(*id, &props).map_err(|e| e.to_string())?;
                expr::PendingWrites::update_edge_props(txn, *id, props);
            }
            _ => {} // null, unbound, or scalar: silently skip.
        },
        RemoveItem::Label { variable, label } => {
            if let Some(GraphBinding::Node(id)) = path.get(variable) {
                txn.remove_label(*id, label).map_err(|e| e.to_string())?;
                expr::PendingWrites::remove_label(txn, *id, label);
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
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            execute_remove_in(txn, stmt, params).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

/// Core REMOVE logic, run inside an already-open write transaction. Used both
/// by [`execute_remove`] above (which opens its own transaction) and by a
/// REMOVE statement in a FOREACH body (which is already inside one, opened by
/// the enclosing `execute_foreach`); LMDB permits only one open writer
/// transaction at a time, so this must never open a second one itself.
pub(super) fn execute_remove_in(
    txn: &mut issundb_core::WriteTxn,
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
    let optimized = Optimizer::optimize(physical, Some(txn.graph()));
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths =
        execute_physical_pathmaps(txn.graph(), &binding_plan, params, Some(&mut *txn))?;

    for path in bound_paths {
        for item in &stmt.items {
            apply_remove_item(txn, item, &path)?;
        }
    }

    Ok(QueryResult {
        statement_count: 1,
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
    // whose matched label was just removed. The whole statement shares one transaction.
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
    graph
        .update(|txn| {
            execute_read_query(graph, &synthetic_query, params, Some(txn)).map_err(as_txn_error)
        })
        .map_err(unwrap_txn_error)
}

pub(super) fn execute_foreach(
    graph: &Graph,
    stmt: &ForeachStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // The whole loop (every iteration, every body statement) shares one
    // transaction, so a failure partway through rolls back every mutation
    // FOREACH already made in earlier iterations.
    graph
        .update(|txn| {
            let _pending = expr::PendingWrites::install();
            let list_val = evaluate_expr(txn.graph(), &PathMap::new(), &stmt.list, params)
                .map_err(as_txn_error)?;
            let items = match list_val {
                serde_json::Value::Array(arr) => arr,
                serde_json::Value::Null => vec![],
                other => vec![other],
            };

            // The body statements reference the loop variable as a plain variable,
            // but the body executors evaluate expressions against fresh rows that do
            // not bind it. Rewrite the body once, replacing every loop-variable
            // reference with a parameter access, and carry the element value in the
            // per-iteration parameter map under the same name.
            let body: Vec<crate::ast::Statement> = stmt
                .body
                .iter()
                .map(|s| subst_loop_var_stmt(s, &stmt.variable))
                .collect();

            for element in items {
                let mut inner_params = params.clone();
                inner_params.insert(stmt.variable.clone(), element);
                for body_stmt in &body {
                    execute_foreach_body(txn, body_stmt, &inner_params).map_err(as_txn_error)?;
                }
            }

            Ok(QueryResult {
                statement_count: 1,
                columns: vec![],
                records: vec![],
            })
        })
        .map_err(unwrap_txn_error)
}

/// Replace every reference to the FOREACH loop variable in `expr` with a
/// parameter access of the same name: a bare `x` becomes `$x`, and `x.prop`
/// becomes `$x["prop"]`. Scoping constructs that rebind the name (a
/// quantifier, list comprehension, or reduce over the same variable) shadow
/// the substitution inside their body. `HasLabel` and pattern-bound variables
/// are graph-element positions the loop value cannot occupy; they are left
/// for the executor's normal unbound/type errors.
fn subst_loop_var_expr(expr: &Expr, var: &str) -> Expr {
    use crate::ast::Expr as E;
    let sub = |e: &Expr| subst_loop_var_expr(e, var);
    let sub_box = |e: &Expr| Box::new(subst_loop_var_expr(e, var));
    match expr {
        E::Prop(v, p) if v == var => {
            if p.is_empty() {
                E::Param(var.to_string())
            } else {
                E::Subscript {
                    expr: Box::new(E::Param(var.to_string())),
                    index: Box::new(E::Literal(crate::ast::Literal::Str(p.clone()))),
                }
            }
        }
        E::Prop(..) | E::Literal(_) | E::Param(_) | E::CountStar | E::HasLabel { .. } => {
            expr.clone()
        }
        E::Agg(f, inner) => E::Agg(f.clone(), sub_box(inner)),
        E::Quantifier {
            kind,
            variable,
            list,
            predicate,
        } => E::Quantifier {
            kind: kind.clone(),
            variable: variable.clone(),
            list: sub_box(list),
            predicate: if variable == var {
                predicate.clone()
            } else {
                sub_box(predicate)
            },
        },
        E::FunctionCall { name, args } => E::FunctionCall {
            name: name.clone(),
            args: args.iter().map(sub).collect(),
        },
        E::BinaryOp { op, left, right } => E::BinaryOp {
            op: op.clone(),
            left: sub_box(left),
            right: sub_box(right),
        },
        E::IsNull(inner) => E::IsNull(sub_box(inner)),
        E::IsNotNull(inner) => E::IsNotNull(sub_box(inner)),
        E::Not(inner) => E::Not(sub_box(inner)),
        E::Case {
            subject,
            arms,
            else_expr,
        } => E::Case {
            subject: subject.as_ref().map(|s| sub_box(s)),
            arms: arms
                .iter()
                .map(|a| crate::ast::CaseArm {
                    when: sub(&a.when),
                    then: sub(&a.then),
                })
                .collect(),
            else_expr: else_expr.as_ref().map(|e| sub_box(e)),
        },
        E::Subscript { expr, index } => E::Subscript {
            expr: sub_box(expr),
            index: sub_box(index),
        },
        E::Slice { expr, start, end } => E::Slice {
            expr: sub_box(expr),
            start: start.as_ref().map(|s| sub_box(s)),
            end: end.as_ref().map(|e| sub_box(e)),
        },
        E::ListComprehension {
            variable,
            list,
            predicate,
            transform,
        } => {
            let shadowed = variable == var;
            E::ListComprehension {
                variable: variable.clone(),
                list: sub_box(list),
                predicate: predicate
                    .as_ref()
                    .map(|p| if shadowed { p.clone() } else { sub_box(p) }),
                transform: transform
                    .as_ref()
                    .map(|t| if shadowed { t.clone() } else { sub_box(t) }),
            }
        }
        E::PatternComprehension {
            pattern,
            predicate,
            transform,
        } => E::PatternComprehension {
            pattern: Box::new(subst_loop_var_pattern(pattern, var)),
            predicate: predicate.as_ref().map(|p| sub_box(p)),
            transform: sub_box(transform),
        },
        E::Reduce {
            accumulator,
            initial,
            variable,
            list,
            expression,
        } => {
            let shadowed = variable == var || accumulator == var;
            E::Reduce {
                accumulator: accumulator.clone(),
                initial: sub_box(initial),
                variable: variable.clone(),
                list: sub_box(list),
                expression: if shadowed {
                    expression.clone()
                } else {
                    sub_box(expression)
                },
            }
        }
    }
}

/// Substitute the loop variable inside a pattern's inline property maps.
fn subst_loop_var_pattern(pattern: &crate::ast::Pattern, var: &str) -> crate::ast::Pattern {
    let sub_props = |props: &Option<HashMap<String, Expr>>| {
        props.as_ref().map(|m| {
            m.iter()
                .map(|(k, e)| (k.clone(), subst_loop_var_expr(e, var)))
                .collect()
        })
    };
    let mut out = pattern.clone();
    out.node.properties = sub_props(&pattern.node.properties);
    for (i, (rel, target)) in pattern.rels.iter().enumerate() {
        out.rels[i].0.properties = sub_props(&rel.properties);
        out.rels[i].1.properties = sub_props(&target.properties);
    }
    out
}

/// Substitute the loop variable inside one SET item's value expression.
fn subst_loop_var_set_item(item: &SetItem, var: &str) -> SetItem {
    match item {
        SetItem::Property {
            variable,
            property,
            expr,
        } => SetItem::Property {
            variable: variable.clone(),
            property: property.clone(),
            expr: subst_loop_var_expr(expr, var),
        },
        other => other.clone(),
    }
}

/// Substitute the loop variable through a FOREACH body statement. Only the
/// statement forms `execute_foreach_body` accepts need coverage; anything
/// else passes through unchanged and fails there with its normal error.
fn subst_loop_var_stmt(stmt: &crate::ast::Statement, var: &str) -> crate::ast::Statement {
    use crate::ast::Statement as S;
    let sub_where = |w: &Option<WhereClause>| -> Option<WhereClause> {
        w.as_ref().map(|w| match w {
            WhereClause::Eq(l, r) => {
                WhereClause::Eq(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Ne(l, r) => {
                WhereClause::Ne(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Lt(l, r) => {
                WhereClause::Lt(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Gt(l, r) => {
                WhereClause::Gt(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Le(l, r) => {
                WhereClause::Le(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Ge(l, r) => {
                WhereClause::Ge(subst_loop_var_expr(l, var), subst_loop_var_expr(r, var))
            }
            WhereClause::Expr(e) => WhereClause::Expr(subst_loop_var_expr(e, var)),
        })
    };
    let sub_matches = |mcs: &[crate::ast::MatchClause]| -> Vec<crate::ast::MatchClause> {
        mcs.iter()
            .map(|mc| crate::ast::MatchClause {
                pattern: subst_loop_var_pattern(&mc.pattern, var),
            })
            .collect()
    };
    match stmt {
        S::Create(c) => S::Create(crate::ast::CreateStatement {
            patterns: c
                .patterns
                .iter()
                .map(|p| subst_loop_var_pattern(p, var))
                .collect(),
        }),
        S::Set(s) => S::Set(crate::ast::SetStatement {
            match_clauses: sub_matches(&s.match_clauses),
            where_clause: sub_where(&s.where_clause),
            set_items: s
                .set_items
                .iter()
                .map(|i| subst_loop_var_set_item(i, var))
                .collect(),
        }),
        S::Delete(d) => S::Delete(crate::ast::DeleteStatement {
            match_clauses: sub_matches(&d.match_clauses),
            where_clause: sub_where(&d.where_clause),
            targets: d
                .targets
                .iter()
                .map(|t| subst_loop_var_expr(t, var))
                .collect(),
            detach: d.detach,
        }),
        S::Merge(m) => S::Merge(crate::ast::MergeStatement {
            pattern: subst_loop_var_pattern(&m.pattern, var),
            on_create_set: m
                .on_create_set
                .iter()
                .map(|i| subst_loop_var_set_item(i, var))
                .collect(),
            on_match_set: m
                .on_match_set
                .iter()
                .map(|i| subst_loop_var_set_item(i, var))
                .collect(),
        }),
        // A standalone body clause (e.g. a bare CREATE) parses as a write-only
        // pipeline query; substitute through its clauses.
        S::Query(q) => {
            let mut q = q.clone();
            q.match_clauses = sub_matches(&q.match_clauses);
            q.where_clause = sub_where(&q.where_clause);
            for part in &mut q.parts {
                match part {
                    QueryPart::Match {
                        match_clauses,
                        where_clause,
                    }
                    | QueryPart::OptionalMatch {
                        match_clauses,
                        where_clause,
                    } => {
                        *match_clauses = sub_matches(match_clauses);
                        *where_clause = sub_where(where_clause);
                    }
                    QueryPart::Create { patterns } => {
                        *patterns = patterns
                            .iter()
                            .map(|p| subst_loop_var_pattern(p, var))
                            .collect();
                    }
                    QueryPart::Merge { merges } => {
                        for m in merges.iter_mut() {
                            m.pattern = subst_loop_var_pattern(&m.pattern, var);
                            m.on_create_set = m
                                .on_create_set
                                .iter()
                                .map(|i| subst_loop_var_set_item(i, var))
                                .collect();
                            m.on_match_set = m
                                .on_match_set
                                .iter()
                                .map(|i| subst_loop_var_set_item(i, var))
                                .collect();
                        }
                    }
                    QueryPart::Set { items } => {
                        *items = items
                            .iter()
                            .map(|i| subst_loop_var_set_item(i, var))
                            .collect();
                    }
                    QueryPart::Delete { targets, .. } => {
                        *targets = targets
                            .iter()
                            .map(|t| subst_loop_var_expr(t, var))
                            .collect();
                    }
                    QueryPart::Unwind { expr, variable } if variable != var => {
                        *expr = subst_loop_var_expr(expr, var);
                    }
                    _ => {}
                }
            }
            S::Query(q)
        }
        S::Foreach(f) if f.variable != *var => S::Foreach(ForeachStatement {
            variable: f.variable.clone(),
            list: subst_loop_var_expr(&f.list, var),
            body: f.body.iter().map(|s| subst_loop_var_stmt(s, var)).collect(),
        }),
        // An inner FOREACH over the same name shadows the outer variable in
        // its body; only its list expression sees the outer binding.
        S::Foreach(f) => S::Foreach(ForeachStatement {
            variable: f.variable.clone(),
            list: subst_loop_var_expr(&f.list, var),
            body: f.body.clone(),
        }),
        other => other.clone(),
    }
}

/// Execute one FOREACH body statement inside the transaction the enclosing
/// `execute_foreach` already opened. Dispatches to each write clause's `_in`
/// (or, for CREATE, pattern-level `_internal`) core rather than its public
/// self-wrapping entry point: those each open their own `Graph::update`,
/// which would deadlock if called while this one is already open.
fn execute_foreach_body(
    txn: &mut issundb_core::WriteTxn,
    stmt: &crate::ast::Statement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    use crate::ast::Statement;
    match stmt {
        Statement::Create(c) => {
            for pattern in &c.patterns {
                execute_create_internal(txn, pattern)?;
            }
        }
        Statement::Set(s) => {
            execute_set_in(txn, s, params)?;
        }
        Statement::Delete(d) => {
            execute_delete_in(txn, d, params)?;
        }
        Statement::Merge(m) => {
            execute_merge_inner(txn, m, params)?;
        }
        Statement::Remove(r) => {
            execute_remove_in(txn, r, params)?;
        }
        // Write-only pipeline query (e.g., standalone CREATE parsed as Statement::Query).
        Statement::Query(q) if q.return_clause.items.is_empty() => {
            super::read::execute_read_query(txn.graph(), q, params, Some(txn))
                .map_err(|e| e.to_string())?;
        }
        _ => {
            return Err("unsupported statement in FOREACH body".into());
        }
    }
    Ok(())
}

pub(super) fn apply_set_items(
    txn: &mut issundb_core::WriteTxn,
    path: &PathMap,
    set_items: &[SetItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    for item in set_items {
        apply_set_item(txn, path, item, params)?;
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
        // A variable-length relationship variable is a list, not a single graph
        // element, so it cannot be a SET/REMOVE target.
        Some(GraphBinding::EdgeList(_)) => Err(format!(
            "SET/REMOVE on a list of relationships '{}' is not supported",
            variable
        )),
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
    txn: &mut issundb_core::WriteTxn,
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
            let new_val = evaluate_expr(txn.graph(), path, expr, params)?;
            match resolve_set_target(path, variable)? {
                SetTarget::Node(nid) => {
                    let record = txn
                        .get_node(nid)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("node not found: {}", nid))?;
                    let mut props: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if let Some(obj) = props.as_object_mut() {
                        if new_val.is_null() {
                            obj.remove(property);
                        } else {
                            obj.insert(property.clone(), new_val);
                        }
                    }
                    txn.update_node(nid, &props).map_err(|e| e.to_string())?;
                    // `expr` above is this arm's `SetItem::Property` field, which
                    // shadows the `expr` module; qualify explicitly.
                    super::expr::PendingWrites::update_node_props(txn, nid, props);
                }
                SetTarget::Edge(eid) => {
                    let record = txn
                        .get_edge(eid)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("edge not found: {}", eid))?;
                    let mut props: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if let Some(obj) = props.as_object_mut() {
                        if new_val.is_null() {
                            obj.remove(property);
                        } else {
                            obj.insert(property.clone(), new_val);
                        }
                    }
                    txn.update_edge(eid, &props).map_err(|e| e.to_string())?;
                    super::expr::PendingWrites::update_edge_props(txn, eid, props);
                }
                SetTarget::Skip => {}
            }
        }
        SetItem::Labels { variable, labels } => {
            if let SetTarget::Node(nid) = resolve_set_target(path, variable)? {
                for label in labels {
                    txn.add_label(nid, label).map_err(|e| e.to_string())?;
                    super::expr::PendingWrites::add_label(txn, nid, label);
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
    txn: &mut issundb_core::WriteTxn,
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
                eval_properties(txn, props, &combined_path, params)?
            } else {
                serde_json::Value::Object(serde_json::Map::new())
            };
            let labels: Vec<&str> = pattern.node.labels.iter().map(|s| s.as_str()).collect();
            let id = txn
                .add_node_multi(&labels, &props_map)
                .map_err(|e| e.to_string())?;
            expr::PendingWrites::record_node(id, pattern.node.labels.clone(), props_map);
            id
        }
    } else {
        let props_map = if let Some(ref props) = pattern.node.properties {
            eval_properties(txn, props, &combined_path, params)?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };
        let labels: Vec<&str> = pattern.node.labels.iter().map(|s| s.as_str()).collect();
        let id = txn
            .add_node_multi(&labels, &props_map)
            .map_err(|e| e.to_string())?;
        expr::PendingWrites::record_node(id, pattern.node.labels.clone(), props_map);
        id
    };

    if let Some(ref var_name) = pattern.node.variable {
        bindings.insert(var_name.clone(), GraphBinding::Node(seed_id));
        combined_path.insert(var_name.clone(), GraphBinding::Node(seed_id));
    }

    let mut created_node_id = seed_id;

    for (rel_pat, node_pat) in &pattern.rels {
        let target_id = if let Some(ref var) = node_pat.variable {
            if let Some(GraphBinding::Node(existing_id)) = combined_path.get(var.as_str()) {
                // Reuse an existing node bound in this context.
                *existing_id
            } else {
                // Create a new target node.
                let target_props = if let Some(ref props) = node_pat.properties {
                    eval_properties(txn, props, &combined_path, params)?
                } else {
                    serde_json::Value::Object(serde_json::Map::new())
                };
                let labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
                let tid = txn
                    .add_node_multi(&labels, &target_props)
                    .map_err(|e| e.to_string())?;
                expr::PendingWrites::record_node(tid, node_pat.labels.clone(), target_props);
                bindings.insert(var.clone(), GraphBinding::Node(tid));
                combined_path.insert(var.clone(), GraphBinding::Node(tid));
                tid
            }
        } else {
            // Anonymous target node.
            let target_props = if let Some(ref props) = node_pat.properties {
                eval_properties(txn, props, &combined_path, params)?
            } else {
                serde_json::Value::Object(serde_json::Map::new())
            };
            let labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
            let tid = txn
                .add_node_multi(&labels, &target_props)
                .map_err(|e| e.to_string())?;
            expr::PendingWrites::record_node(tid, node_pat.labels.clone(), target_props);
            tid
        };

        let rel_type = rel_pat
            .rel_type
            .clone()
            .unwrap_or_else(|| "EDGE".to_string());

        let rel_props = if let Some(ref props) = rel_pat.properties {
            eval_properties(txn, props, &combined_path, params)?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };

        let (src, dst) = if rel_pat.is_incoming {
            (target_id, created_node_id)
        } else {
            (created_node_id, target_id)
        };
        let edge_id = txn
            .add_edge(src, dst, &rel_type, &rel_props)
            .map_err(|e| e.to_string())?;
        expr::PendingWrites::record_edge(edge_id, src, dst, rel_type.clone(), rel_props);

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
fn node_props_json(txn: &issundb_core::WriteTxn, id: NodeId) -> Result<serde_json::Value, String> {
    match txn.get_node(id).map_err(|e| e.to_string())? {
        Some(record) => rmp_serde::from_slice(&record.props).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

/// Read an edge's stored properties as a JSON object.
fn edge_props_json(txn: &issundb_core::WriteTxn, id: EdgeId) -> Result<serde_json::Value, String> {
    match txn.get_edge(id).map_err(|e| e.to_string())? {
        Some(record) => rmp_serde::from_slice(&record.props).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

/// True when every key in `filter` is present on the element with an equal value.
fn props_subset_match(filter: &serde_json::Value, actual: &serde_json::Value) -> bool {
    let (Some(filter_obj), Some(actual_obj)) = (filter.as_object(), actual.as_object()) else {
        return filter.as_object().map(|o| o.is_empty()).unwrap_or(true);
    };
    // Cypher value equality, not structural JSON equality: an integer-stored
    // property matches a float literal of the same value (30 = 30.0), so
    // `MERGE (n {x: 30.0})` finds a node created with `{x: 30}`.
    filter_obj.iter().all(|(k, v)| {
        actual_obj
            .get(k)
            .is_some_and(|a| super::expr::cypher_eq(a, v) == serde_json::Value::Bool(true))
    })
}

/// Evaluate a MERGE pattern's inline property map. Unlike CREATE, where a
/// null property value simply is not stored, MERGE cannot match on null: the
/// filter key would vanish and the pattern would match every candidate, so
/// openCypher requires an error instead.
fn eval_merge_properties(
    txn: &issundb_core::WriteTxn,
    props: &HashMap<String, crate::ast::Expr>,
    path: &PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for (k, v) in props {
        let val = evaluate_expr(txn.graph(), path, v, params)?;
        if val == serde_json::Value::Null {
            return Err(format!(
                "SemanticError(InvalidArgumentValue): cannot merge using null property value for '{k}'"
            ));
        }
        obj.insert(k.clone(), val);
    }
    Ok(serde_json::Value::Object(obj))
}

/// True when a node carries all of the required labels.
fn node_has_labels(
    txn: &issundb_core::WriteTxn,
    id: NodeId,
    labels: &[String],
) -> Result<bool, String> {
    if labels.is_empty() {
        return Ok(true);
    }
    let actual = txn.node_labels(id).map_err(|e| e.to_string())?;
    Ok(labels.iter().all(|l| actual.contains(l)))
}

/// True when a node satisfies both the label and property constraints of a pattern.
fn node_matches(
    txn: &issundb_core::WriteTxn,
    id: NodeId,
    labels: &[String],
    props: &serde_json::Value,
) -> Result<bool, String> {
    if !node_has_labels(txn, id, labels)? {
        return Ok(false);
    }
    Ok(props_subset_match(props, &node_props_json(txn, id)?))
}

/// Enumerate candidate nodes that satisfy a node pattern's label and property
/// constraints. With no labels, this scans all nodes; otherwise it scans the
/// first label and filters the rest.
fn candidate_nodes(
    txn: &issundb_core::WriteTxn,
    labels: &[String],
    props: &serde_json::Value,
) -> Result<Vec<NodeId>, String> {
    let seed: Vec<NodeId> = if let Some(first) = labels.first() {
        txn.nodes_by_label(first).map_err(|e| e.to_string())?
    } else {
        txn.all_nodes().map_err(|e| e.to_string())?
    };
    let mut out = Vec::new();
    for id in seed {
        if node_matches(txn, id, labels, props)? {
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
    txn: &issundb_core::WriteTxn,
    pattern: &Pattern,
    ctx: &super::PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<super::PathMap>, String> {
    // Seed candidates for the first node in the chain.
    let seed_props = match &pattern.node.properties {
        Some(p) => eval_merge_properties(txn, p, ctx, params)?,
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    let seed_candidates: Vec<NodeId> =
        match pattern.node.variable.as_deref().and_then(|v| ctx.get(v)) {
            Some(GraphBinding::Node(id)) => vec![*id],
            _ => candidate_nodes(txn, &pattern.node.labels, &seed_props)?,
        };

    // Each partial carries the pattern bindings accumulated so far, the
    // current chain endpoint, and the edge ids already consumed by earlier
    // hops: openCypher relationship uniqueness forbids one relationship from
    // satisfying two hops of the same pattern, so a match that would reuse an
    // edge is not a match (and MERGE must create instead).
    let mut partials: Vec<(super::PathMap, NodeId, Vec<EdgeId>)> = Vec::new();
    for nid in seed_candidates {
        let mut pm = super::PathMap::new();
        if let Some(v) = &pattern.node.variable {
            pm.insert(v.clone(), GraphBinding::Node(nid));
        }
        partials.push((pm, nid, Vec::new()));
    }

    for (rel_pat, node_pat) in &pattern.rels {
        let rel_types: Vec<&str> = rel_pat
            .rel_type
            .as_deref()
            .map(|t| t.split('|').collect())
            .unwrap_or_default();
        let mut next: Vec<(super::PathMap, NodeId, Vec<EdgeId>)> = Vec::new();
        for (pm, cur, used_edges) in &partials {
            let mut combined = ctx.clone();
            for (k, v) in pm {
                combined.insert(k.clone(), v.clone());
            }
            let rel_props = match &rel_pat.properties {
                Some(p) => eval_merge_properties(txn, p, &combined, params)?,
                None => serde_json::Value::Object(serde_json::Map::new()),
            };
            let tgt_props = match &node_pat.properties {
                Some(p) => eval_merge_properties(txn, p, &combined, params)?,
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

            let neighbors = txn.all_neighbors(*cur).map_err(|e| e.to_string())?;
            for n in neighbors {
                if used_edges.contains(&n.edge) {
                    continue;
                }
                // Direction filter: undirected accepts both, otherwise the edge
                // orientation must match the pattern.
                if !rel_pat.is_undirected {
                    let want_outgoing = !rel_pat.is_incoming;
                    if n.outgoing != want_outgoing {
                        continue;
                    }
                }
                if !rel_types.is_empty() {
                    let tn = txn
                        .type_name(n.edge_type)
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default();
                    if !rel_types.contains(&tn.as_str()) {
                        continue;
                    }
                }
                if !props_subset_match(&rel_props, &edge_props_json(txn, n.edge)?) {
                    continue;
                }
                match bound_target {
                    Some(b) if b != n.node => continue,
                    None if !node_matches(txn, n.node, &node_pat.labels, &tgt_props)? => continue,
                    _ => {}
                }
                let mut npm = pm.clone();
                if let Some(rv) = &rel_pat.variable {
                    npm.insert(rv.clone(), GraphBinding::Edge(n.edge));
                }
                if let Some(tv) = &node_pat.variable {
                    npm.insert(tv.clone(), GraphBinding::Node(n.node));
                }
                let mut nused = used_edges.clone();
                nused.push(n.edge);
                next.push((npm, n.node, nused));
            }
        }
        partials = next;
    }

    Ok(partials.into_iter().map(|(pm, _, _)| pm).collect())
}

/// Match-or-create a MERGE pattern within the given context. Returns one binding
/// extension per matched row, or a single extension for the freshly created
/// pattern. ON MATCH / ON CREATE SET actions are applied with the combined
/// context so they can reference both incoming and pattern variables.
pub(super) fn execute_merge_internal_with_context(
    txn: &mut issundb_core::WriteTxn,
    stmt: &crate::ast::MergeStatement,
    path: &super::PathMap,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<super::PathMap>, String> {
    let matches = merge_match(txn, &stmt.pattern, path, params)?;
    if !matches.is_empty() {
        for m in &matches {
            if !stmt.on_match_set.is_empty() {
                let mut combined = path.clone();
                for (k, v) in m {
                    combined.insert(k.clone(), v.clone());
                }
                apply_set_items(txn, &combined, &stmt.on_match_set, params)?;
            }
        }
        Ok(matches)
    } else {
        let created = execute_create_internal_with_context(txn, &stmt.pattern, path, params)?;
        if !stmt.on_create_set.is_empty() {
            let mut combined = path.clone();
            for (k, v) in &created {
                combined.insert(k.clone(), v.clone());
            }
            apply_set_items(txn, &combined, &stmt.on_create_set, params)?;
        }
        Ok(vec![created])
    }
}
