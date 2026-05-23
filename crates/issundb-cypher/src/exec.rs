use std::collections::{HashMap, HashSet};

use issundb_core::{Graph, NodeId};

use crate::ast::*;
use crate::parser;

/// The tabular result of a Cypher query execution.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub records: Vec<Record>,
}

/// An individual row in the query result table.
#[derive(Debug, Clone)]
pub struct Record {
    pub values: Vec<serde_json::Value>,
}

/// Execute a Cypher query against the `Graph` handle.
pub fn execute(
    graph: &Graph,
    cypher: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let stmt = parser::parse(cypher)?;
    match stmt {
        Statement::Query(q) => execute_read_query(graph, &q, params),
        Statement::Create(c) => execute_create(graph, &c, params),
        Statement::Set(s) => execute_set(graph, &s, params),
        Statement::Delete(d) => execute_delete(graph, &d, params),
    }
}

fn execute_read_query(
    graph: &Graph,
    query: &Query,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Collect all unique variables bound in the MATCH patterns
    let mut bound_paths: Vec<HashMap<String, NodeId>> = vec![HashMap::new()];

    for match_clause in &query.match_clauses {
        bound_paths = evaluate_match(graph, &match_clause.pattern, bound_paths)?;
    }

    // Filter bound paths using the WHERE clause
    let mut filtered_paths = Vec::new();
    for path in bound_paths {
        let is_match = if let Some(ref where_clause) = query.where_clause {
            evaluate_where(graph, &path, where_clause, params)?
        } else {
            true
        };
        if is_match {
            filtered_paths.push(path);
        }
    }

    // Project return items
    let mut columns = Vec::new();
    for item in &query.return_clause.items {
        let col_name = if let Some(ref alias) = item.alias {
            alias.clone()
        } else {
            match &item.expr {
                Expr::Prop(var, prop) => format!("{}.{}", var, prop),
                Expr::Literal(_) => "literal".to_string(),
                Expr::Param(p) => format!("${}", p),
            }
        };
        columns.push(col_name);
    }

    let mut records = Vec::new();
    for path in filtered_paths {
        let mut values = Vec::new();
        for item in &query.return_clause.items {
            let val = evaluate_expr(graph, &path, &item.expr, params)?;
            values.push(val);
        }
        records.push(Record { values });
    }

    Ok(QueryResult { columns, records })
}

fn evaluate_match(
    graph: &Graph,
    pattern: &Pattern,
    current_paths: Vec<HashMap<String, NodeId>>,
) -> Result<Vec<HashMap<String, NodeId>>, String> {
    let mut next_paths = Vec::new();

    // Process the seed node pattern
    let seed_var = pattern
        .node
        .variable
        .clone()
        .unwrap_or_else(|| "_seed".to_string());
    let seed_candidates = if let Some(ref label) = pattern.node.label {
        graph.nodes_by_label(label).map_err(|e| e.to_string())?
    } else {
        graph.all_nodes().map_err(|e| e.to_string())?
    };

    for path in &current_paths {
        for &cand in &seed_candidates {
            // Check if inline properties match
            if let Some(ref props) = pattern.node.properties {
                if !check_inline_properties(graph, cand, props)? {
                    continue;
                }
            }

            let mut new_path = path.clone();
            if new_path
                .insert(seed_var.clone(), cand)
                .is_some_and(|existing| existing != cand)
            {
                continue;
            }

            // Traverse relationships
            let mut branch_paths = vec![new_path];
            let mut prev_node_var = seed_var.clone();
            for (seg_idx, (rel_pat, node_pat)) in pattern.rels.iter().enumerate() {
                let mut temp_paths = Vec::new();
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

                for b_path in branch_paths {
                    let current_node = b_path[&prev_node_var];

                    let min_hops = rel_pat.range.as_ref().and_then(|r| r.min).unwrap_or(1) as usize;
                    // Three cases must be distinguished:
                    //   range = None            → no `*` at all; plain single-hop [:R]  → 1
                    //   range = Some { max:None } → bare [:R*] unbounded              → usize::MAX
                    //   range = Some { max:Some(n) } → [:R*1..n] bounded             → n
                    // A flat and_then chain collapses the first two into the same None
                    // and cannot tell them apart, so use an explicit match here.
                    let max_hops = match rel_pat.range.as_ref() {
                        None => 1,
                        Some(r) => r.max.map(|v| v as usize).unwrap_or(usize::MAX),
                    };

                    // We start with a queue of paths: each element is (current_node_id, visited_nodes_in_path)
                    let mut queue = vec![(current_node, vec![current_node])];
                    // HashSet deduplicates nodes reachable via multiple paths so convergent
                    // topologies (e.g. diamond graphs) do not emit duplicate result rows.
                    let mut completed_targets: HashSet<NodeId> = HashSet::new();

                    for hop in 1..=max_hops {
                        let mut next_queue = Vec::new();
                        for (node, path_nodes) in queue {
                            let neighbors = if rel_pat.is_incoming {
                                graph.in_neighbors(node).map_err(|e| e.to_string())?
                            } else {
                                graph.out_neighbors(node).map_err(|e| e.to_string())?
                            };

                            for (neigh_node, _edge_id, type_id) in neighbors {
                                // Check relationship type if specified
                                if let Some(ref rel_type) = rel_pat.rel_type {
                                    let actual_name =
                                        graph.type_name(type_id).map_err(|e| e.to_string())?;
                                    match actual_name {
                                        Some(ref name) if name == rel_type => {}
                                        _ => continue,
                                    }
                                }

                                // Prevent cycles: do not traverse same node twice in a single path
                                if path_nodes.contains(&neigh_node) {
                                    continue;
                                }

                                let mut next_path_nodes = path_nodes.clone();
                                next_path_nodes.push(neigh_node);

                                if hop >= min_hops {
                                    completed_targets.insert(neigh_node);
                                }

                                next_queue.push((neigh_node, next_path_nodes));
                            }
                        }
                        queue = next_queue;
                        if queue.is_empty() {
                            break;
                        }
                    }

                    for neigh_node in completed_targets {
                        // Check target node label if specified
                        if let Some(ref label) = node_pat.label {
                            let label_nodes =
                                graph.nodes_by_label(label).map_err(|e| e.to_string())?;
                            if !label_nodes.contains(&neigh_node) {
                                continue;
                            }
                        }

                        // Check inline properties of the target node
                        if let Some(ref props) = node_pat.properties {
                            if !check_inline_properties(graph, neigh_node, props)? {
                                continue;
                            }
                        }

                        let mut step_path = b_path.clone();
                        step_path.insert(rel_var.clone(), neigh_node);
                        if step_path
                            .insert(target_var.clone(), neigh_node)
                            .is_some_and(|existing| existing != neigh_node)
                        {
                            continue;
                        }
                        temp_paths.push(step_path);
                    }
                }
                prev_node_var = target_var;
                branch_paths = temp_paths;
            }

            next_paths.extend(branch_paths);
        }
    }

    Ok(next_paths)
}

fn check_inline_properties(
    graph: &Graph,
    node: NodeId,
    expected_props: &HashMap<String, Literal>,
) -> Result<bool, String> {
    let record = match graph.get_node(node).map_err(|e| e.to_string())? {
        Some(r) => r,
        None => return Ok(false),
    };
    let actual_json: serde_json::Value =
        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

    for (k, v) in expected_props {
        let expected_json = literal_to_value(v);
        if actual_json.get(k) != Some(&expected_json) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn evaluate_where(
    graph: &Graph,
    path: &HashMap<String, NodeId>,
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
            Ok(json_cmp(&lv, &rv) < Some(std::cmp::Ordering::Less))
        }
        WhereClause::Gt(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            Ok(json_cmp(&lv, &rv) > Some(std::cmp::Ordering::Greater))
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
    }
}

fn evaluate_expr(
    graph: &Graph,
    path: &HashMap<String, NodeId>,
    expr: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Param(p) => params
            .get(p)
            .cloned()
            .ok_or_else(|| format!("missing parameter: {}", p)),
        Expr::Prop(var, prop) => {
            let &node_id = path
                .get(var)
                .ok_or_else(|| format!("unbound variable: {}", var))?;
            let record = graph
                .get_node(node_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node not found: {}", node_id))?;
            let actual_json: serde_json::Value =
                rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
            Ok(actual_json
                .get(prop)
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        }
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

fn execute_create(
    graph: &Graph,
    create: &CreateStatement,
    _params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let mut props_map = HashMap::new();
    if let Some(ref props) = create.pattern.node.properties {
        for (k, v) in props {
            props_map.insert(k.clone(), literal_to_value(v));
        }
    }

    let label = create
        .pattern
        .node
        .label
        .clone()
        .unwrap_or_else(|| "Node".to_string());
    let seed_id = graph
        .add_node(&label, &props_map)
        .map_err(|e| e.to_string())?;

    let mut created_node_id = seed_id;
    for (rel_pat, node_pat) in &create.pattern.rels {
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

        let rel_type = rel_pat
            .rel_type
            .clone()
            .unwrap_or_else(|| "EDGE".to_string());
        let empty_props: HashMap<String, serde_json::Value> = HashMap::new();

        if rel_pat.is_incoming {
            graph
                .add_edge(target_id, created_node_id, &rel_type, &empty_props)
                .map_err(|e| e.to_string())?;
        } else {
            graph
                .add_edge(created_node_id, target_id, &rel_type, &empty_props)
                .map_err(|e| e.to_string())?;
        }

        created_node_id = target_id;
    }

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
    let mut bound_paths: Vec<HashMap<String, NodeId>> = vec![HashMap::new()];

    for match_clause in &set.match_clauses {
        bound_paths = evaluate_match(graph, &match_clause.pattern, bound_paths)?;
    }

    let mut matched_count = 0;
    for path in bound_paths {
        let is_match = if let Some(ref where_clause) = set.where_clause {
            evaluate_where(graph, &path, where_clause, params)?
        } else {
            true
        };
        if is_match {
            for set_item in &set.set_items {
                let &node_id = path
                    .get(&set_item.variable)
                    .ok_or_else(|| format!("unbound variable: {}", set_item.variable))?;
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

                // Resolve label name
                // To update, we must pass the string label. Let's obtain the label name.
                // We know label name is stored in the meta registry, but we can also obtain it using nodes_by_label scans
                // or keep a default fallback. To be robust, let's find the label by scanning known labels or use "Node" as a fallback.
                let mut label_name = "Node".to_string();
                for l in &["Person", "Company", "Organization", "User", "Node"] {
                    let nodes = graph.nodes_by_label(l).unwrap_or_default();
                    if nodes.contains(&node_id) {
                        label_name = l.to_string();
                        break;
                    }
                }

                graph
                    .update_node(node_id, &label_name, &actual_json)
                    .map_err(|e| e.to_string())?;
            }
            matched_count += 1;
        }
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
    let mut bound_paths: Vec<HashMap<String, NodeId>> = vec![HashMap::new()];

    for match_clause in &delete.match_clauses {
        bound_paths = evaluate_match(graph, &match_clause.pattern, bound_paths)?;
    }

    let mut deleted_count = 0;
    for path in bound_paths {
        let is_match = if let Some(ref where_clause) = delete.where_clause {
            evaluate_where(graph, &path, where_clause, params)?
        } else {
            true
        };
        if is_match {
            for var in &delete.variables {
                let &node_id = path
                    .get(var)
                    .ok_or_else(|| format!("unbound variable: {}", var))?;
                graph.delete_node(node_id).map_err(|e| e.to_string())?;
                deleted_count += 1;
            }
        }
    }

    Ok(QueryResult {
        columns: vec!["nodes_deleted".to_string()],
        records: vec![Record {
            values: vec![serde_json::Value::Number(deleted_count.into())],
        }],
    })
}
