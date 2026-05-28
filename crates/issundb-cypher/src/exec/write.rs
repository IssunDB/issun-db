use super::expr::evaluate_expr;
use super::expr::literal_to_value;
use super::read::{binding_to_value, execute_physical, projected_key};
use super::*;
use crate::ast::{
    CreateAndReturnStatement, DeleteAndReturnStatement, ForeachStatement, MergeAndReturnStatement,
    RemoveAndReturnStatement, RemoveItem, RemoveStatement, SetAndReturnStatement,
};

pub(super) fn execute_create_internal(graph: &Graph, pattern: &Pattern) -> Result<PathMap, String> {
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

pub(super) fn execute_create(
    graph: &Graph,
    create: &CreateStatement,
    _params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    for pattern in &create.patterns {
        execute_create_internal(graph, pattern)?;
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
    let mut bindings = PathMap::new();
    for pattern in &stmt.patterns {
        let created = execute_create_internal(graph, pattern)?;
        bindings.extend(created);
    }

    // Project the RETURN clause over the created bindings.
    let columns: Vec<String> = stmt
        .return_clause
        .items
        .iter()
        .map(|item| projected_key(&item.expr, &item.alias))
        .collect();

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
    // First run the MATCH to get bindings.
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
    let logical = LogicalPlanner::plan(&synthetic_query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    // Apply SET items.
    for path in &bound_paths {
        for set_item in &stmt.set_items {
            let node_id = match path.get(&set_item.variable) {
                Some(GraphBinding::Node(id)) => *id,
                _ => continue,
            };
            let record = graph
                .get_node(node_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node not found: {}", node_id))?;
            let mut actual_json: serde_json::Value =
                rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
            let new_val = evaluate_expr(graph, path, &set_item.expr, params)?;
            if let Some(obj) = actual_json.as_object_mut() {
                obj.insert(set_item.property.clone(), new_val);
            }
            graph
                .update_node(node_id, &actual_json)
                .map_err(|e| e.to_string())?;
        }
    }

    // Re-run the MATCH to get updated data for RETURN (read-your-writes in same query).
    // For simplicity, project from already-bound paths (pre-SET values were read).
    // The TCK expects post-SET values, so we re-query.
    let mut post_paths = execute_physical(graph, &binding_plan, params)?;

    // Apply ORDER BY if present.
    if let Some(ob) = &stmt.order_by {
        use super::expr::json_cmp;
        use super::read::evaluate_sort_key;
        let mut keyed: Vec<(Vec<serde_json::Value>, PathMap)> = post_paths
            .drain(..)
            .map(|path| {
                let keys: Vec<serde_json::Value> = ob
                    .items
                    .iter()
                    .map(|si| evaluate_sort_key(graph, &path, &si.expr, params))
                    .collect();
                (keys, path)
            })
            .collect();

        keyed.sort_by(|(ka, _), (kb, _)| {
            for (i, si) in ob.items.iter().enumerate() {
                let ord = json_cmp(&ka[i], &kb[i]).unwrap_or(std::cmp::Ordering::Equal);
                let ord = if si.ascending { ord } else { ord.reverse() };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        post_paths = keyed.into_iter().map(|(_, path)| path).collect();
    }

    // Project RETURN.
    let columns: Vec<String> = stmt
        .return_clause
        .items
        .iter()
        .map(|item| projected_key(&item.expr, &item.alias))
        .collect();

    let mut records = Vec::new();
    for path in post_paths {
        let mut values = Vec::new();
        for item in &stmt.return_clause.items {
            let val =
                evaluate_expr(graph, &path, &item.expr, params).unwrap_or(serde_json::Value::Null);
            values.push(val);
        }
        records.push(Record { values });
    }

    if let Some(skip_expr) = &stmt.skip {
        let skip_val = evaluate_expr(graph, &PathMap::new(), skip_expr, params)?;
        let skip = skip_val.as_i64().unwrap_or(0).max(0) as usize;
        records = records.into_iter().skip(skip).collect();
    }
    if let Some(limit_expr) = &stmt.limit {
        let limit_val = evaluate_expr(graph, &PathMap::new(), limit_expr, params)?;
        let limit = limit_val.as_i64().unwrap_or(0).max(0) as usize;
        records = records.into_iter().take(limit).collect();
    }

    Ok(QueryResult { columns, records })
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

    for path in bound_paths {
        for var in &delete.variables {
            match path.get(var) {
                Some(GraphBinding::Node(id)) => {
                    if delete.detach {
                        // Delete all incident edges before deleting the node.
                        let out_edges = graph.out_neighbors(*id).map_err(|e| e.to_string())?;
                        for ne in out_edges {
                            graph.delete_edge(ne.edge).map_err(|e| e.to_string())?;
                        }
                        let in_edges = graph.in_neighbors(*id).map_err(|e| e.to_string())?;
                        for ne in in_edges {
                            graph.delete_edge(ne.edge).map_err(|e| e.to_string())?;
                        }
                    }
                    graph.delete_node(*id).map_err(|e| e.to_string())?;
                }
                Some(GraphBinding::Edge(id)) => {
                    graph.delete_edge(*id).map_err(|e| e.to_string())?;
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
        columns: vec![],
        records: vec![],
    })
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
            return_clause: ReturnClause { items: vec![], distinct: false },
            parts: Vec::new(),
            order_by: None,
            skip: None,
            limit: None,
        };
        let logical = LogicalPlanner::plan(&binding_query)?;
        let physical = PhysicalPlanner::plan(&logical);
        let optimized = Optimizer::optimize(physical, Some(graph));
        let binding_plan = match optimized {
            PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
            other => other,
        };
        let bound_paths = execute_physical(graph, &binding_plan, params)?;

        for path in bound_paths {
            for var in &stmt.variables {
                match path.get(var) {
                    Some(GraphBinding::Node(id)) => {
                        if stmt.detach {
                            let out_edges =
                                graph.out_neighbors(*id).map_err(|e| e.to_string())?;
                            for ne in out_edges {
                                let _ = graph.delete_edge(ne.edge);
                            }
                            let in_edges =
                                graph.in_neighbors(*id).map_err(|e| e.to_string())?;
                            for ne in in_edges {
                                let _ = graph.delete_edge(ne.edge);
                            }
                        }
                        let _ = graph.delete_node(*id);
                    }
                    Some(GraphBinding::Edge(id)) => {
                        let _ = graph.delete_edge(*id);
                    }
                    _ => {}
                }
            }
        }

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
    // Build a synthetic read query to test whether the pattern already exists.
    let synthetic_query = Query {
        match_clauses: vec![MatchClause {
            pattern: stmt.pattern.clone(),
        }],
        where_clause: None,
        return_clause: ReturnClause {
            items: vec![],
            distinct: false,
        },
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
    graph.with_write_lock(|| {
        // Run each MERGE block and collect the final bindings.
        let mut bindings = PathMap::new();
        for merge in &stmt.merges {
            execute_merge_inner(graph, merge, params)?;
            // Re-query the pattern to get bindings for the RETURN clause.
            let synthetic_query = Query {
                match_clauses: vec![MatchClause {
                    pattern: merge.pattern.clone(),
                }],
                where_clause: None,
                return_clause: ReturnClause {
                    items: vec![],
                    distinct: false,
                },
                parts: Vec::new(),
                order_by: None,
                skip: None,
                limit: None,
            };
            let logical = LogicalPlanner::plan(&synthetic_query)?;
            let physical = PhysicalPlanner::plan(&logical);
            let optimized = Optimizer::optimize(physical, Some(graph));
            let binding_plan = match optimized {
                PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
                other => other,
            };
            let paths = execute_physical(graph, &binding_plan, params)?;
            for path in paths {
                bindings.extend(path);
            }
        }

        let columns: Vec<String> = stmt
            .return_clause
            .items
            .iter()
            .map(|item| projected_key(&item.expr, &item.alias))
            .collect();

        let mut values = Vec::new();
        for item in &stmt.return_clause.items {
            let key = projected_key(&item.expr, &item.alias);
            let val = binding_to_value(graph, bindings.get(&key))?;
            values.push(val);
        }

        // If we could not resolve by key, evaluate the expressions directly.
        if values.iter().all(|v| *v == serde_json::Value::Null) && !stmt.return_clause.items.is_empty() {
            values.clear();
            for item in &stmt.return_clause.items {
                let val = evaluate_expr(graph, &bindings, &item.expr, params)
                    .unwrap_or(serde_json::Value::Null);
                values.push(val);
            }
        }

        Ok(QueryResult {
            columns,
            records: if values.is_empty() { vec![] } else { vec![Record { values }] },
        })
    })
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
    let logical = LogicalPlanner::plan(&synthetic_query)?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));
    let binding_plan = match optimized {
        PhysicalOperator::Project { input, items, .. } if items.is_empty() => *input,
        other => other,
    };
    let bound_paths = execute_physical(graph, &binding_plan, params)?;

    for path in bound_paths {
        for item in &stmt.items {
            match item {
                RemoveItem::Property { variable, property } => {
                    let node_id = match path.get(variable) {
                        Some(GraphBinding::Node(id)) => *id,
                        Some(GraphBinding::Edge(_)) => {
                            return Err(format!(
                                "REMOVE property on edge variable '{}' is not supported",
                                variable
                            ));
                        }
                        Some(GraphBinding::Scalar(_)) => {
                            return Err(format!(
                                "REMOVE property on scalar variable '{}' is not supported",
                                variable
                            ));
                        }
                        None => return Err(format!("unbound variable: {}", variable)),
                    };
                    let record = graph
                        .get_node(node_id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("node not found: {}", node_id))?;
                    let mut actual_json: serde_json::Value =
                        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                    if let Some(obj) = actual_json.as_object_mut() {
                        obj.remove(property);
                    }
                    graph
                        .update_node(node_id, &actual_json)
                        .map_err(|e| e.to_string())?;
                }
                RemoveItem::Label { .. } => {
                    return Err("REMOVE label is not supported".into());
                }
            }
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
    graph.with_write_lock(|| {
        // Execute the read query for the RETURN clause first (before removal).
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
        let read_result = execute_read_query(graph, &synthetic_query, params)?;

        // Now perform the REMOVE using the binding plan.
        let remove_stmt = RemoveStatement {
            match_clauses: stmt.match_clauses.clone(),
            where_clause: stmt.where_clause.clone(),
            items: stmt.items.clone(),
        };
        execute_remove(graph, &remove_stmt, params)?;

        Ok(read_result)
    })
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
