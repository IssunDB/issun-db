use super::*;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

pub(super) fn evaluate_expr(
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
                other => Err(format!(
                    "TypeError: NOT requires a boolean operand, got {}",
                    other
                )),
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
                    // all(x IN list WHERE pred): null if any null result and no false; false if any false; true if all true
                    let mut has_null = false;
                    let mut all_true = true;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        match pred_val {
                            serde_json::Value::Bool(true) => {}
                            serde_json::Value::Bool(false) => {
                                all_true = false;
                                has_null = false;
                                break;
                            }
                            serde_json::Value::Null => {
                                has_null = true;
                            }
                            _ => {
                                all_true = false;
                                break;
                            }
                        }
                    }
                    if !all_true {
                        serde_json::Value::Bool(false)
                    } else if has_null {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Bool(true)
                    }
                }
                QuantifierKind::Any => {
                    // any(x IN list WHERE pred): true if any true; null if any null and no true; false otherwise
                    let mut has_null = false;
                    let mut any_true = false;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        match pred_val {
                            serde_json::Value::Bool(true) => {
                                any_true = true;
                                break;
                            }
                            serde_json::Value::Null => {
                                has_null = true;
                            }
                            _ => {}
                        }
                    }
                    if any_true {
                        serde_json::Value::Bool(true)
                    } else if has_null {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Bool(false)
                    }
                }
                QuantifierKind::None => {
                    // none(x IN list WHERE pred): false if any true; null if any null and no true; true otherwise
                    let mut has_null = false;
                    let mut any_true = false;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        match pred_val {
                            serde_json::Value::Bool(true) => {
                                any_true = true;
                                break;
                            }
                            serde_json::Value::Null => {
                                has_null = true;
                            }
                            _ => {}
                        }
                    }
                    if any_true {
                        serde_json::Value::Bool(false)
                    } else if has_null {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Bool(true)
                    }
                }
                QuantifierKind::Single => {
                    // single(x IN list WHERE pred): null if uncertain (null results and count could be 0 or 1)
                    // true only if exactly 1 true result and no null results that could change the answer
                    let mut count_true = 0usize;
                    let mut has_null = false;
                    for item in &items {
                        let mut inner_path = path.clone();
                        inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                        let pred_val = evaluate_expr(graph, &inner_path, predicate, params)?;
                        match pred_val {
                            serde_json::Value::Bool(true) => {
                                count_true += 1;
                                if count_true > 1 {
                                    break;
                                }
                            }
                            serde_json::Value::Null => {
                                has_null = true;
                            }
                            _ => {}
                        }
                    }
                    if count_true > 1 {
                        // More than one match → definitely false
                        serde_json::Value::Bool(false)
                    } else if count_true == 1 && !has_null {
                        // Exactly one match and no nulls → definitely true
                        serde_json::Value::Bool(true)
                    } else if has_null {
                        // Uncertain due to nulls
                        serde_json::Value::Null
                    } else {
                        // count_true == 0, no nulls → definitely false
                        serde_json::Value::Bool(false)
                    }
                }
            };
            Ok(result)
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            let subject_val = subject
                .as_ref()
                .map(|e| evaluate_expr(graph, path, e, params))
                .transpose()?;
            for arm in arms {
                let when_val = evaluate_expr(graph, path, &arm.when, params)?;
                let matches = match &subject_val {
                    Some(sv) => json_cmp(sv, &when_val) == Some(std::cmp::Ordering::Equal),
                    None => when_val == serde_json::Value::Bool(true),
                };
                if matches {
                    return evaluate_expr(graph, path, &arm.then, params);
                }
            }
            match else_expr {
                Some(e) => evaluate_expr(graph, path, e, params),
                None => Ok(serde_json::Value::Null),
            }
        }
        Expr::Subscript { expr, index } => {
            let val = evaluate_expr(graph, path, expr, params)?;
            let idx = evaluate_expr(graph, path, index, params)?;
            match (val, idx) {
                (serde_json::Value::Null, _) | (_, serde_json::Value::Null) => {
                    Ok(serde_json::Value::Null)
                }
                (serde_json::Value::Array(_), serde_json::Value::Bool(b)) => Err(format!(
                    "TypeError: list index must be an integer, got Boolean({})",
                    b
                )),
                (serde_json::Value::Array(_), serde_json::Value::Array(_)) => {
                    Err("TypeError: list index must be an integer, got List".to_string())
                }
                (serde_json::Value::Array(_), serde_json::Value::Object(_)) => {
                    Err("TypeError: list index must be an integer, got Map".to_string())
                }
                (serde_json::Value::Array(_), serde_json::Value::String(s)) => Err(format!(
                    "TypeError: list index must be an integer, got String({})",
                    s
                )),
                (serde_json::Value::Array(arr), serde_json::Value::Number(n)) => {
                    // Reject non-integer floats.
                    if n.as_i64().is_none() {
                        return Err(format!(
                            "TypeError: list index must be an integer, got Float({})",
                            n
                        ));
                    }
                    let i = n.as_i64().unwrap_or(0);
                    let len = arr.len() as i64;
                    let pos = if i < 0 { len + i } else { i };
                    if pos < 0 || pos >= len {
                        Ok(serde_json::Value::Null)
                    } else {
                        Ok(arr[pos as usize].clone())
                    }
                }
                (serde_json::Value::Object(map), serde_json::Value::String(key)) => {
                    Ok(map.get(&key).cloned().unwrap_or(serde_json::Value::Null))
                }

                // Node/edge property access via subscript: n['propName']
                (node_val, serde_json::Value::String(key)) => {
                    if let serde_json::Value::Object(map) = node_val {
                        Ok(map.get(&key).cloned().unwrap_or(serde_json::Value::Null))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
                // Indexing a non-list/non-map/non-string with an integer: TypeError.
                (non_indexable, serde_json::Value::Number(_)) => Err(format!(
                    "TypeError: cannot index into {} with an integer",
                    match non_indexable {
                        serde_json::Value::Bool(_) => "Boolean",
                        serde_json::Value::Number(_) => "Number",
                        serde_json::Value::String(_) => "String",
                        _ => "value",
                    }
                )),
                _ => Ok(serde_json::Value::Null),
            }
        }
        Expr::Slice { expr, start, end } => {
            let val = evaluate_expr(graph, path, expr, params)?;
            let s_val = start
                .as_ref()
                .map(|e| evaluate_expr(graph, path, e, params))
                .transpose()?;
            let e_val = end
                .as_ref()
                .map(|e| evaluate_expr(graph, path, e, params))
                .transpose()?;
            // If either range bound is null, the result is null.
            if s_val
                .as_ref()
                .is_some_and(|v| *v == serde_json::Value::Null)
                || e_val
                    .as_ref()
                    .is_some_and(|v| *v == serde_json::Value::Null)
            {
                return Ok(serde_json::Value::Null);
            }
            match val {
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                serde_json::Value::Array(arr) => {
                    let len = arr.len() as i64;
                    let from = s_val
                        .as_ref()
                        .and_then(|v| v.as_i64())
                        .map(|i| if i < 0 { (len + i).max(0) } else { i.min(len) })
                        .unwrap_or(0) as usize;
                    let to = e_val
                        .as_ref()
                        .and_then(|v| v.as_i64())
                        .map(|i| if i < 0 { (len + i).max(0) } else { i.min(len) })
                        .unwrap_or(len) as usize;
                    let to = to.max(from);
                    let to = to.min(arr.len());
                    Ok(serde_json::Value::Array(
                        arr[from.min(arr.len())..to].to_vec(),
                    ))
                }
                serde_json::Value::String(s_str) => {
                    let chars: Vec<char> = s_str.chars().collect();
                    let len = chars.len() as i64;
                    let from = s_val
                        .as_ref()
                        .and_then(|v| v.as_i64())
                        .map(|i| if i < 0 { (len + i).max(0) } else { i.min(len) })
                        .unwrap_or(0) as usize;
                    let to = e_val
                        .as_ref()
                        .and_then(|v| v.as_i64())
                        .map(|i| if i < 0 { (len + i).max(0) } else { i.min(len) })
                        .unwrap_or(len) as usize;
                    let to = to.max(from);
                    let to = to.min(chars.len());
                    Ok(serde_json::Value::String(
                        chars[from.min(chars.len())..to].iter().collect(),
                    ))
                }
                _ => Ok(serde_json::Value::Null),
            }
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            transform,
        } => {
            let list_val = evaluate_expr(graph, path, list, params)?;
            let items = match list_val {
                serde_json::Value::Array(arr) => arr,
                serde_json::Value::Null => return Ok(serde_json::Value::Null),
                _ => return Err("list comprehension requires a list".into()),
            };
            let mut result = Vec::new();
            for item in items {
                let mut inner_path = path.clone();
                inner_path.insert(variable.clone(), GraphBinding::Scalar(item.clone()));
                let keep = match predicate {
                    Some(pred) => {
                        evaluate_expr(graph, &inner_path, pred, params)?
                            == serde_json::Value::Bool(true)
                    }
                    None => true,
                };
                if keep {
                    let out = match transform {
                        Some(t) => evaluate_expr(graph, &inner_path, t, params)?,
                        None => item,
                    };
                    result.push(out);
                }
            }
            Ok(serde_json::Value::Array(result))
        }
        Expr::HasLabel { variable, label } => match path.get(variable.as_str()) {
            Some(GraphBinding::Node(id)) => {
                if let Ok(Some(record)) = graph.get_node(*id) {
                    if let Ok(Some(node_label)) = graph.label_name(record.label) {
                        return Ok(serde_json::Value::Bool(node_label == *label));
                    }
                }
                Ok(serde_json::Value::Bool(false))
            }
            Some(GraphBinding::Scalar(serde_json::Value::Null)) | None => {
                Ok(serde_json::Value::Null)
            }
            _ => Ok(serde_json::Value::Bool(false)),
        },
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
                        let mut m = serde_json::Map::new();
                        m.insert(
                            "__type__".to_string(),
                            serde_json::Value::String("__Node__".to_string()),
                        );
                        m.insert(
                            "id".to_string(),
                            serde_json::Value::Number((*node_id as i64).into()),
                        );
                        m.insert("properties".to_string(), actual_json);
                        Ok(serde_json::Value::Object(m))
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
                        let mut m = serde_json::Map::new();
                        m.insert(
                            "__type__".to_string(),
                            serde_json::Value::String("__Edge__".to_string()),
                        );
                        m.insert(
                            "id".to_string(),
                            serde_json::Value::Number((*edge_id as i64).into()),
                        );
                        m.insert("properties".to_string(), actual_json);
                        Ok(serde_json::Value::Object(m))
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
                        if obj.get("__type__").and_then(|t| t.as_str()) == Some("__Node__") {
                            if let Some(id_val) = obj.get("id").and_then(|i| i.as_i64()) {
                                let node_id = id_val as u64;
                                let record = graph
                                    .get_node(node_id)
                                    .map_err(|e| e.to_string())?
                                    .ok_or_else(|| format!("node not found: {}", node_id))?;
                                let actual_json: serde_json::Value =
                                    rmp_serde::from_slice(&record.props)
                                        .map_err(|e| e.to_string())?;
                                Ok(actual_json
                                    .get(prop)
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null))
                            } else {
                                Ok(serde_json::Value::Null)
                            }
                        } else if obj.get("__type__").and_then(|t| t.as_str()) == Some("__Edge__") {
                            if let Some(id_val) = obj.get("id").and_then(|i| i.as_i64()) {
                                let edge_id = id_val as u64;
                                let record = graph
                                    .get_edge(edge_id)
                                    .map_err(|e| e.to_string())?
                                    .ok_or_else(|| format!("edge not found: {}", edge_id))?;
                                let actual_json: serde_json::Value =
                                    rmp_serde::from_slice(&record.props)
                                        .map_err(|e| e.to_string())?;
                                Ok(actual_json
                                    .get(prop)
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null))
                            } else {
                                Ok(serde_json::Value::Null)
                            }
                        } else {
                            Ok(obj.get(prop).cloned().unwrap_or(serde_json::Value::Null))
                        }
                    } else if *val == serde_json::Value::Null {
                        Ok(serde_json::Value::Null)
                    } else {
                        // Type error: property access on non-map scalar
                        Err(format!(
                            "TypeError: property access '{}' on non-map value {}",
                            prop, val
                        ))
                    }
                }
            }
        }
    }
}

/// Evaluate a binary operation with three-valued null propagation.
pub(super) fn eval_binary_op(
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
            let rv = evaluate_expr(graph, path, right, params)?;
            // Type check: both must be boolean or null.
            if !matches!(lv, serde_json::Value::Bool(_) | serde_json::Value::Null) {
                return Err(format!(
                    "TypeError: AND requires boolean operands, left operand is {}",
                    lv
                ));
            }
            if !matches!(rv, serde_json::Value::Bool(_) | serde_json::Value::Null) {
                return Err(format!(
                    "TypeError: AND requires boolean operands, right operand is {}",
                    rv
                ));
            }
            if lv == serde_json::Value::Bool(false) || rv == serde_json::Value::Bool(false) {
                return Ok(serde_json::Value::Bool(false));
            }
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            Ok(serde_json::Value::Bool(true))
        }
        BinaryOperator::Or => {
            let lv = evaluate_expr(graph, path, left, params)?;
            let rv = evaluate_expr(graph, path, right, params)?;
            // Type check: both must be boolean or null.
            if !matches!(lv, serde_json::Value::Bool(_) | serde_json::Value::Null) {
                return Err(format!(
                    "TypeError: OR requires boolean operands, left operand is {}",
                    lv
                ));
            }
            if !matches!(rv, serde_json::Value::Bool(_) | serde_json::Value::Null) {
                return Err(format!(
                    "TypeError: OR requires boolean operands, right operand is {}",
                    rv
                ));
            }
            if lv == serde_json::Value::Bool(true) || rv == serde_json::Value::Bool(true) {
                return Ok(serde_json::Value::Bool(true));
            }
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            Ok(serde_json::Value::Bool(false))
        }
        BinaryOperator::Xor => {
            let lv = evaluate_expr(graph, path, left, params)?;
            let rv = evaluate_expr(graph, path, right, params)?;
            match (&lv, &rv) {
                // null XOR null or null XOR bool (or bool XOR null) → null
                (serde_json::Value::Null, serde_json::Value::Null)
                | (serde_json::Value::Null, serde_json::Value::Bool(_))
                | (serde_json::Value::Bool(_), serde_json::Value::Null) => {
                    Ok(serde_json::Value::Null)
                }
                // null XOR non-bool: error (non-bool operand is the problem)
                (serde_json::Value::Null, other) | (other, serde_json::Value::Null) => Err(
                    format!("TypeError: XOR requires boolean operands, got {}", other),
                ),
                (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => {
                    Ok(serde_json::Value::Bool(a ^ b))
                }
                _ => Err(format!(
                    "TypeError: XOR requires boolean operands, got {} XOR {}",
                    lv, rv
                )),
            }
        }
        _ => {
            let lv = evaluate_expr(graph, path, left, params)?;
            let rv = evaluate_expr(graph, path, right, params)?;
            if lv == serde_json::Value::Null || rv == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            match op {
                // Equality: NaN != NaN (IEEE 754).  List equality propagates null when
                // any element in either list is null (openCypher three-valued logic).
                BinaryOperator::Eq => Ok(cypher_eq(&lv, &rv)),
                BinaryOperator::Ne => {
                    let eq = cypher_eq(&lv, &rv);
                    match eq {
                        serde_json::Value::Bool(b) => Ok(serde_json::Value::Bool(!b)),
                        serde_json::Value::Null => Ok(serde_json::Value::Null),
                        _ => unreachable!(),
                    }
                }
                // Ordered comparisons: return null for incompatible types; for NaN vs
                // number return false; for NaN vs non-number return null (openCypher spec).
                BinaryOperator::Lt
                | BinaryOperator::Gt
                | BinaryOperator::Le
                | BinaryOperator::Ge => {
                    // NaN handling
                    if is_nan(&lv) || is_nan(&rv) {
                        let other = if is_nan(&lv) { &rv } else { &lv };
                        return Ok(if other.is_number() || is_nan(other) {
                            serde_json::Value::Bool(false)
                        } else {
                            serde_json::Value::Null
                        });
                    }
                    let c = json_cmp(&lv, &rv);
                    // When json_cmp returns None, the types are incompatible → null.
                    match c {
                        None => Ok(serde_json::Value::Null),
                        Some(c) => Ok(serde_json::Value::Bool(match op {
                            BinaryOperator::Lt => c == std::cmp::Ordering::Less,
                            BinaryOperator::Gt => c == std::cmp::Ordering::Greater,
                            BinaryOperator::Le => c != std::cmp::Ordering::Greater,
                            BinaryOperator::Ge => c != std::cmp::Ordering::Less,
                            _ => unreachable!(),
                        })),
                    }
                }
                BinaryOperator::Add => eval_arithmetic(&lv, &rv, '+'),
                BinaryOperator::Sub => eval_arithmetic(&lv, &rv, '-'),
                BinaryOperator::Mul => eval_arithmetic(&lv, &rv, '*'),
                BinaryOperator::Div => eval_arithmetic(&lv, &rv, '/'),
                BinaryOperator::Mod => eval_arithmetic(&lv, &rv, '%'),
                BinaryOperator::Pow => match (&lv, &rv) {
                    (serde_json::Value::Number(base), serde_json::Value::Number(exp)) => {
                        let b = base.as_f64().unwrap_or(0.0);
                        let e = exp.as_f64().unwrap_or(0.0);
                        Ok(serde_json::Number::from_f64(b.powf(e))
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null))
                    }
                    _ => Ok(serde_json::Value::Null),
                },
                BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Xor => unreachable!(),
            }
        }
    }
}

pub(super) fn eval_arithmetic(
    lv: &serde_json::Value,
    rv: &serde_json::Value,
    op: char,
) -> Result<serde_json::Value, String> {
    // List concatenation with +
    if op == '+' {
        match (lv, rv) {
            (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
                let mut result = a.clone();
                result.extend(b.iter().cloned());
                return Ok(serde_json::Value::Array(result));
            }
            (serde_json::Value::Array(a), v) if *v != serde_json::Value::Null => {
                let mut result = a.clone();
                result.push(v.clone());
                return Ok(serde_json::Value::Array(result));
            }
            _ => {}
        }
    }

    // Temporal arithmetic: Date/LocalDateTime/DateTime ± Duration, Duration ± Duration.
    if let Some(result) = temporal_arithmetic(lv, rv, op) {
        return result;
    }
    if let Some(result) = temporal_arithmetic_date_duration(lv, rv, op) {
        return result;
    }
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
                    // Float division/modulo by zero → NaN (IEEE 754), not null.
                    '/' => lf / rf,
                    '%' => lf % rf,
                    _ => return Ok(serde_json::Value::Null),
                };
                // f64::NAN and f64::INFINITY cannot be stored in serde_json::Number;
                // represent NaN as a sentinel object.
                if result.is_nan() {
                    return Ok(nan_value());
                }
                return Ok(serde_json::Number::from_f64(result)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null));
            }
            Ok(serde_json::Value::Null)
        }
        (serde_json::Value::String(ls), serde_json::Value::String(rs)) if op == '+' => {
            Ok(serde_json::Value::String(format!("{}{}", ls, rs)))
        }
        (lv, rv) => Err(format!(
            "TypeError: cannot apply '{}' to {} and {}",
            op, lv, rv
        )),
    }
}

/// Evaluate a built-in function call.
pub(super) fn eval_function_call(
    graph: &Graph,
    path: &PathMap,
    name: &str,
    args: &[Expr],
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let name_lc = name.to_ascii_lowercase();
    let name = name_lc.as_str();
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
            if start_val == serde_json::Value::Null || end_val == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            let start = start_val
                .as_i64()
                .ok_or_else(|| "range() start must be an integer".to_string())?;
            let end = end_val
                .as_i64()
                .ok_or_else(|| "range() end must be an integer".to_string())?;
            let step = if args.len() == 3 {
                let sv = evaluate_expr(graph, path, &args[2], params)?;
                if sv == serde_json::Value::Null {
                    return Ok(serde_json::Value::Null);
                }
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
            let val = evaluate_expr(graph, path, &args[0], params)?;
            if val == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    match path.get(var.as_str()) {
                        Some(GraphBinding::Edge(eid)) => {
                            let record = graph
                                .get_edge(*eid)
                                .map_err(|e| e.to_string())?
                                .ok_or_else(|| format!("edge not found: {}", eid))?;
                            if let Ok(name) = graph.type_name(record.edge_type) {
                                return Ok(name
                                    .map(serde_json::Value::String)
                                    .unwrap_or(serde_json::Value::Null));
                            }
                            return Ok(serde_json::Value::Null);
                        }
                        Some(GraphBinding::Scalar(serde_json::Value::Null)) => {
                            return Ok(serde_json::Value::Null);
                        }
                        Some(GraphBinding::Node(_)) => {
                            return Err(
                                "TypeError: type() requires a relationship, got a node".into()
                            );
                        }
                        _ => {}
                    }
                }
            }
            // Non-variable argument that resolved to a non-null, non-edge value: TypeError.
            match &val {
                serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)
                | serde_json::Value::Array(_)
                | serde_json::Value::Object(_) => {
                    Err("TypeError: type() requires a relationship argument".into())
                }
                _ => Ok(serde_json::Value::Null),
            }
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
                serde_json::Value::Bool(b) => Ok(serde_json::Value::String(b.to_string())),
                serde_json::Value::Number(n) => Ok(serde_json::Value::String(n.to_string())),
                // Temporal values are stored as objects carrying their canonical ISO string
                // in `__str__`; toString() returns that representation.
                serde_json::Value::Object(ref map) if map.contains_key("__str__") => map
                    .get("__str__")
                    .cloned()
                    .ok_or_else(|| "toString() temporal value missing __str__".into()),
                // Lists, maps, nodes, and edges cannot be converted to string.
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                    Err("TypeError: toString() cannot convert list or map to string".into())
                }
            }
        }
        "tointeger" | "toint" => {
            if args.len() != 1 {
                return Err("toInteger() requires exactly 1 argument".into());
            }
            let val = evaluate_expr(graph, path, &args[0], params)?;
            fn coerce_to_int(v: serde_json::Value) -> Result<serde_json::Value, String> {
                match v {
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Ok(serde_json::Value::Number(i.into()))
                        } else if let Some(f) = n.as_f64() {
                            Ok(serde_json::Value::Number((f as i64).into()))
                        } else {
                            Ok(serde_json::Value::Null)
                        }
                    }
                    serde_json::Value::String(s) => {
                        // Try parsing the string as an integer or float.
                        if let Ok(i) = s.trim().parse::<i64>() {
                            Ok(serde_json::Value::Number(i.into()))
                        } else if let Ok(f) = s.trim().parse::<f64>() {
                            Ok(serde_json::Value::Number((f as i64).into()))
                        } else {
                            Ok(serde_json::Value::Null)
                        }
                    }
                    serde_json::Value::Null => Ok(serde_json::Value::Null),
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        Err("TypeError: toInteger() cannot convert list or map".into())
                    }
                    serde_json::Value::Bool(_) => {
                        Err("TypeError: toInteger() cannot convert boolean".into())
                    }
                }
            }
            coerce_to_int(val)
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
                serde_json::Value::String(s) => {
                    // Try parsing the string as a float.
                    if let Ok(f) = s.trim().parse::<f64>() {
                        Ok(serde_json::Number::from_f64(f)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
                serde_json::Value::Null => Ok(serde_json::Value::Null),
                other => Err(format!("TypeError: toFloat() not applicable to {}", other)),
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
            // Check if the arg is a node/edge binding directly.
            let node_props = if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    match path.get(var.as_str()) {
                        Some(GraphBinding::Node(nid)) => {
                            if let Ok(Some(record)) = graph.get_node(*nid) {
                                rmp_serde::from_slice::<serde_json::Value>(&record.props).ok()
                            } else {
                                None
                            }
                        }
                        Some(GraphBinding::Edge(eid)) => {
                            if let Ok(Some(record)) = graph.get_edge(*eid) {
                                rmp_serde::from_slice::<serde_json::Value>(&record.props).ok()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let val = if let Some(props) = node_props {
                props
            } else {
                evaluate_expr(graph, path, &args[0], params)?
            };

            match val {
                serde_json::Value::Object(map) => {
                    let mut keys: Vec<serde_json::Value> = map
                        .keys()
                        .map(|k| serde_json::Value::String(k.clone()))
                        .collect();
                    keys.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
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
                        let eq = cypher_eq(&needle, item);
                        if eq == serde_json::Value::Bool(true) {
                            return Ok(serde_json::Value::Bool(true));
                        }
                        if eq == serde_json::Value::Null {
                            found_null = true;
                        }
                    }
                    if found_null {
                        Ok(serde_json::Value::Null)
                    } else {
                        Ok(serde_json::Value::Bool(false))
                    }
                }
                other => Err(format!(
                    "TypeError: IN requires a list as its right operand, got {}",
                    other
                )),
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
            if args.len() != 2 {
                return Err("=~ requires 2 arguments".into());
            }
            let left = evaluate_expr(graph, path, &args[0], params)?;
            let right = evaluate_expr(graph, path, &args[1], params)?;
            if left == serde_json::Value::Null || right == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            let text = left.as_str().unwrap_or("");
            let pattern = right.as_str().unwrap_or("");
            let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
            Ok(serde_json::Value::Bool(re.is_match(text)))
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
        "rand" => {
            // Returns a random float in [0.0, 1.0).
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            use std::time::SystemTime;
            let mut h = DefaultHasher::new();
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
                .hash(&mut h);
            // Mix with thread id for uniqueness.
            std::thread::current().id().hash(&mut h);
            let bits = h.finish();
            let f = (bits as f64) / (u64::MAX as f64);
            Ok(serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null))
        }
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
                // Integer → null (not an error per openCypher)
                serde_json::Value::Number(n) if n.as_i64().is_some() => Ok(serde_json::Value::Null),
                other => Err(format!(
                    "TypeError: toBoolean() not applicable to {}",
                    other
                )),
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
            // Handle node/edge bindings directly.
            if let Expr::Prop(var, prop) = &args[0] {
                if prop.is_empty() {
                    match path.get(var.as_str()) {
                        Some(GraphBinding::Node(nid)) => {
                            if let Ok(Some(record)) = graph.get_node(*nid) {
                                return rmp_serde::from_slice::<serde_json::Value>(&record.props)
                                    .map_err(|e| e.to_string());
                            }
                        }
                        Some(GraphBinding::Edge(eid)) => {
                            if let Ok(Some(record)) = graph.get_edge(*eid) {
                                return rmp_serde::from_slice::<serde_json::Value>(&record.props)
                                    .map_err(|e| e.to_string());
                            }
                        }
                        _ => {}
                    }
                }
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
        // ── Temporal constructors ──────────────────────────────────────────────────
        "date" => {
            if args.is_empty() {
                // date() with no args = current date; we return null since we have no clock.
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_date(arg)
        }
        "localtime" => {
            if args.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_localtime(arg)
        }
        "time" => {
            if args.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_time(arg)
        }
        "localdatetime" => {
            if args.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_localdatetime(arg)
        }
        "datetime" => {
            if args.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_datetime(arg)
        }
        "duration" => {
            if args.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let arg = evaluate_expr(graph, path, &args[0], params)?;
            make_duration(arg)
        }
        // ── Temporal component accessors ──────────────────────────────────────────
        // Truncation functions
        "date.truncate"
        | "datetime.truncate"
        | "localdatetime.truncate"
        | "localtime.truncate"
        | "time.truncate" => {
            if args.is_empty() {
                return Err(format!("{name}() requires at least 1 argument"));
            }
            let unit = evaluate_expr(graph, path, &args[0], params)?;
            let temporal = if args.len() > 1 {
                evaluate_expr(graph, path, &args[1], params)?
            } else {
                serde_json::Value::Null
            };
            let override_map = if args.len() > 2 {
                Some(evaluate_expr(graph, path, &args[2], params)?)
            } else {
                None
            };
            temporal_truncate(name, &unit, &temporal, override_map.as_ref())
        }
        // duration.between, duration.inDays, etc.
        "duration.between" | "duration.indays" | "duration.inmonths" | "duration.inseconds" => {
            if args.len() < 2 {
                return Err(format!("{name}() requires 2 arguments"));
            }
            let a = evaluate_expr(graph, path, &args[0], params)?;
            let b = evaluate_expr(graph, path, &args[1], params)?;
            if a == serde_json::Value::Null || b == serde_json::Value::Null {
                return Ok(serde_json::Value::Null);
            }
            temporal_duration_between(&a, &b, name)
        }
        // Current-time functions — not implemented; return null to propagate gracefully.
        "date.transaction"
        | "date.statement"
        | "date.realtime"
        | "datetime.transaction"
        | "datetime.statement"
        | "datetime.realtime"
        | "localtime.transaction"
        | "localtime.statement"
        | "localtime.realtime"
        | "localdatetime.transaction"
        | "localdatetime.statement"
        | "localdatetime.realtime"
        | "time.transaction"
        | "time.statement"
        | "time.realtime" => Ok(serde_json::Value::Null),
        _ => Err(format!("unknown function: {}", name)),
    }
}

// ─── Temporal helpers ─────────────────────────────────────────────────────────

fn temporal_obj(
    type_name: &str,
    fields: serde_json::Map<String, serde_json::Value>,
    str_repr: String,
    sort_key: String,
) -> serde_json::Value {
    let mut m = fields;
    m.insert(
        "__type__".to_string(),
        serde_json::Value::String(type_name.to_string()),
    );
    m.insert("__str__".to_string(), serde_json::Value::String(str_repr));
    m.insert(
        "__sort_key__".to_string(),
        serde_json::Value::String(sort_key),
    );
    serde_json::Value::Object(m)
}

fn naive_date_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveDate, String> {
    let get_i64 = |k: &str| -> Option<i64> { map.get(k)?.as_i64() };

    // Calendar date: year + month + day
    if let (Some(year), Some(month), Some(day)) =
        (get_i64("year"), get_i64("month"), get_i64("day"))
    {
        return NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
            .ok_or_else(|| format!("invalid calendar date: {}-{}-{}", year, month, day));
    }

    // Ordinal date: year + ordinalDay / dayOfYear
    if let Some(year) = get_i64("year") {
        let ordinal = get_i64("ordinalDay").or_else(|| get_i64("dayOfYear"));
        if let Some(ord) = ordinal {
            return NaiveDate::from_yo_opt(year as i32, ord as u32)
                .ok_or_else(|| format!("invalid ordinal date: {}-{}", year, ord));
        }
    }

    // Week date: year + week (+ optional dayOfWeek, default Monday=1)
    if let (Some(year), Some(week)) = (get_i64("year"), get_i64("week")) {
        let dow_num = get_i64("dayOfWeek").unwrap_or(1);
        let wd = num_to_weekday(dow_num as u32)
            .ok_or_else(|| format!("invalid dayOfWeek: {}", dow_num))?;
        return NaiveDate::from_isoywd_opt(year as i32, week as u32, wd)
            .ok_or_else(|| format!("invalid ISO week date: {}-W{:02}-{}", year, week, dow_num));
    }

    // Quarter date: year + quarter + dayOfQuarter
    if let (Some(year), Some(quarter)) = (get_i64("year"), get_i64("quarter")) {
        let doq = get_i64("dayOfQuarter").unwrap_or(1);
        let start_month = ((quarter - 1) * 3 + 1) as u32;
        let start = NaiveDate::from_ymd_opt(year as i32, start_month, 1)
            .ok_or_else(|| format!("invalid quarter: {}", quarter))?;
        return Ok(start + Duration::days(doq - 1));
    }

    // Year only: year + optional month (default 1) + default day (1)
    if let Some(year) = get_i64("year") {
        let month = get_i64("month").unwrap_or(1) as u32;
        let day = get_i64("day").unwrap_or(1) as u32;
        return NaiveDate::from_ymd_opt(year as i32, month, day)
            .ok_or_else(|| format!("invalid date from year: {}-{}-{}", year, month, day));
    }

    Err("date() map must include at least 'year'".to_string())
}

fn num_to_weekday(n: u32) -> Option<Weekday> {
    match n {
        1 => Some(Weekday::Mon),
        2 => Some(Weekday::Tue),
        3 => Some(Weekday::Wed),
        4 => Some(Weekday::Thu),
        5 => Some(Weekday::Fri),
        6 => Some(Weekday::Sat),
        7 => Some(Weekday::Sun),
        _ => None,
    }
}

fn naive_date_from_str(s: &str) -> Result<NaiveDate, String> {
    let s = s.trim();

    match s.len() {
        // "YYYY" → year only, default to Jan 1
        4 => {
            if let Ok(y) = s.parse::<i32>() {
                return NaiveDate::from_ymd_opt(y, 1, 1)
                    .ok_or_else(|| format!("cannot parse date: '{}'", s));
            }
        }
        // "YYYYMM" → year-month, default to day 1
        6 => {
            if let (Ok(y), Ok(m)) = (s[..4].parse::<i32>(), s[4..].parse::<u32>()) {
                return NaiveDate::from_ymd_opt(y, m, 1)
                    .ok_or_else(|| format!("cannot parse date: '{}'", s));
            }
        }
        // "YYYY-MM" (7) or "YYYYWww" (7)
        7 => {
            if s.contains('W') || s.contains('w') {
                // "YYYYWww" → week without day, default to Monday (weekday 1)
                let (year_part, week_part) = s
                    .split_once(['W', 'w'])
                    .ok_or_else(|| format!("cannot parse date: '{}'", s))?;
                if let (Ok(y), Ok(w)) = (year_part.parse::<i32>(), week_part.parse::<u32>()) {
                    return NaiveDate::from_isoywd_opt(y, w, Weekday::Mon)
                        .ok_or_else(|| format!("cannot parse date: '{}'", s));
                }
            } else if let Some(dash) = s.find('-') {
                // "YYYY-MM" → year-month, default to day 1
                let year_str = &s[..dash];
                let month_str = &s[dash + 1..];
                if let (Ok(y), Ok(m)) = (year_str.parse::<i32>(), month_str.parse::<u32>()) {
                    return NaiveDate::from_ymd_opt(y, m, 1)
                        .ok_or_else(|| format!("cannot parse date: '{}'", s));
                }
            }
        }
        // "YYYYMMDD" (8) or "YYYYWwwD" (8) or "YYYY-DDD" (8) or "YYYY-Www" (8)
        8 => {
            if (s.contains('W') || s.contains('w')) && s.as_bytes()[4] == b'-' {
                // "YYYY-Www" → week without day, default Monday
                let year_str = &s[..4];
                let week_str = &s[6..];
                if let (Ok(y), Ok(w)) = (year_str.parse::<i32>(), week_str.parse::<u32>()) {
                    return NaiveDate::from_isoywd_opt(y, w, Weekday::Mon)
                        .ok_or_else(|| format!("cannot parse date: '{}'", s));
                }
            } else if s.contains('W') || s.contains('w') {
                // "YYYYWwwD" → week + weekday
                if let Ok(d) = NaiveDate::parse_from_str(s, "%GW%V%u") {
                    return Ok(d);
                }
            } else if s.as_bytes()[4] == b'-' {
                // "YYYY-DDD" → ordinal
                if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%j") {
                    return Ok(d);
                }
            } else {
                // "YYYYMMDD"
                if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%m%d") {
                    return Ok(d);
                }
            }
        }
        // "YYYY-W30" (8 chars with dash+W, handled above) or "YYYY-Www" with dash
        // Also handle "YYYY-W30" = 8 chars but contains dash and W
        _ => {}
    }

    // "YYYY-MM-DD" (10)
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d);
    }
    // "YYYY-Www-D" (10) — ISO week with dash separator and weekday
    if let Ok(d) = NaiveDate::parse_from_str(s, "%G-W%V-%u") {
        return Ok(d);
    }
    // "YYYY-Www" (8) → ISO week without day (defaults to Monday)
    // Format: "YYYY-Www" where W is literal and ww is 2-digit week
    if s.len() == 8 && s.chars().nth(5) == Some('W') && s.chars().nth(4) == Some('-') {
        let year_str = &s[..4];
        let week_str = &s[6..];
        if let (Ok(y), Ok(w)) = (year_str.parse::<i32>(), week_str.parse::<u32>()) {
            return NaiveDate::from_isoywd_opt(y, w, Weekday::Mon)
                .ok_or_else(|| format!("cannot parse date: '{}'", s));
        }
    }
    // "YYYY-Www" (9 chars: "YYYY-W30") — dash + W + 2 digit week
    if s.len() == 9 && s.starts_with(|c: char| c.is_ascii_digit()) {
        if let Some(w_pos) = s.find('W').or_else(|| s.find('w')) {
            let year_str = &s[..w_pos - 1]; // before dash
            let week_str = &s[w_pos + 1..];
            if let (Ok(y), Ok(w)) = (year_str.parse::<i32>(), week_str.parse::<u32>()) {
                return NaiveDate::from_isoywd_opt(y, w, Weekday::Mon)
                    .ok_or_else(|| format!("cannot parse date: '{}'", s));
            }
        }
    }
    // "YYYYDDD" (7) → ordinal (no dash) — after other 7-char checks fail
    if s.len() == 7 && !s.contains('-') && !s.contains('W') && !s.contains('w') {
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%j") {
            return Ok(d);
        }
    }

    Err(format!("cannot parse date: '{}'", s))
}

fn date_to_obj(d: NaiveDate) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("year".to_string(), d.year().into());
    m.insert("month".to_string(), (d.month() as i64).into());
    m.insert("day".to_string(), (d.day() as i64).into());
    m.insert("quarter".to_string(), ((d.month0() / 3 + 1) as i64).into());
    m.insert(
        "dayOfWeek".to_string(),
        (d.weekday().num_days_from_monday() as i64 + 1).into(),
    );
    m.insert("dayOfYear".to_string(), (d.ordinal() as i64).into());
    m.insert("ordinalDay".to_string(), (d.ordinal() as i64).into());
    // ISO week fields
    let iso_week = d.iso_week();
    m.insert("week".to_string(), (iso_week.week() as i64).into());
    m.insert("weekYear".to_string(), (iso_week.year() as i64).into());
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    m.insert(
        "epochDays".to_string(),
        (d.signed_duration_since(epoch).num_days()).into(),
    );
    let str_repr = d.format("%Y-%m-%d").to_string();
    temporal_obj("Date", m, str_repr.clone(), str_repr)
}

fn make_date(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            let d = naive_date_from_str(&s)?;
            Ok(date_to_obj(d))
        }
        serde_json::Value::Object(ref map)
            if map.get("__type__").and_then(|v| v.as_str()).is_some() =>
        {
            // Temporal object passed directly → extract date portion
            let d = obj_to_naive_date(map)?;
            Ok(date_to_obj(d))
        }
        serde_json::Value::Object(map) => {
            // Check if map has a "date" key referencing another temporal value.
            if let Some(base_val) = map.get("date") {
                let base_date = match base_val {
                    serde_json::Value::Object(bmap)
                        if bmap.get("__type__").and_then(|v| v.as_str()).is_some() =>
                    {
                        obj_to_naive_date(bmap)?
                    }
                    serde_json::Value::String(s) => naive_date_from_str(s)?,
                    _ => return Err("date() 'date' field must be a temporal value".to_string()),
                };
                // Apply overrides from the rest of the map
                let d = apply_date_overrides(base_date, &map)?;
                return Ok(date_to_obj(d));
            }
            let d = naive_date_from_map(&map)?;
            Ok(date_to_obj(d))
        }
        other => Err(format!(
            "date() argument must be a string or map, got {}",
            other
        )),
    }
}

fn apply_date_overrides(
    base: NaiveDate,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveDate, String> {
    let get_i64 = |k: &str| -> Option<i64> { map.get(k)?.as_i64() };

    // If week override is specified, rebuild from ISO week. The dayOfWeek is inherited from
    // the base date (per openCypher selection semantics), not reset to Monday.
    if let Some(week) = get_i64("week") {
        let year = get_i64("year").unwrap_or(base.iso_week().year() as i64) as i32;
        let dow_num = get_i64("dayOfWeek").unwrap_or(base.weekday().number_from_monday() as i64);
        let wd = num_to_weekday(dow_num as u32)
            .ok_or_else(|| format!("invalid dayOfWeek: {}", dow_num))?;
        return NaiveDate::from_isoywd_opt(year, week as u32, wd)
            .ok_or_else(|| format!("invalid ISO week override: {}-W{}", year, week));
    }

    // If quarter override is specified. The dayOfQuarter is inherited from the base date
    // (per openCypher selection semantics), not the base day-of-month.
    if let Some(quarter) = get_i64("quarter") {
        let year = get_i64("year").unwrap_or(base.year() as i64) as i32;
        let start_month = ((quarter - 1) * 3 + 1) as u32;
        let day_of_quarter = match get_i64("dayOfQuarter") {
            Some(d) => d,
            None => {
                let base_q_start_month = ((base.month() as i64 - 1) / 3) * 3 + 1;
                let base_q_start =
                    NaiveDate::from_ymd_opt(base.year(), base_q_start_month as u32, 1)
                        .ok_or_else(|| "invalid base quarter start".to_string())?;
                base.signed_duration_since(base_q_start).num_days() + 1
            }
        };
        return NaiveDate::from_ymd_opt(year, start_month, 1)
            .map(|d| d + Duration::days(day_of_quarter - 1))
            .ok_or_else(|| format!("invalid quarter override: {}", quarter));
    }

    // If ordinalDay override
    if let Some(ord) = get_i64("ordinalDay") {
        let year = get_i64("year").unwrap_or(base.year() as i64) as i32;
        return NaiveDate::from_yo_opt(year, ord as u32)
            .ok_or_else(|| format!("invalid ordinalDay override: {}", ord));
    }

    // Calendar overrides: year, month, day
    let year = get_i64("year").unwrap_or(base.year() as i64) as i32;
    let month = get_i64("month").unwrap_or(base.month() as i64) as u32;
    let day = get_i64("day").unwrap_or(base.day() as i64) as u32;
    NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| format!("invalid date override: {}-{}-{}", year, month, day))
}

fn naive_time_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveTime, String> {
    let get_u32 = |k: &str| -> u32 { map.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as u32 };
    let get_u32_ns = |k: &str| -> u32 { map.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as u32 };
    let h = get_u32("hour");
    let min = get_u32("minute");
    let s = get_u32("second");
    let ms = get_u32("millisecond");
    let us = get_u32("microsecond");
    let ns = get_u32_ns("nanosecond");
    let total_ns = ns + us * 1_000 + ms * 1_000_000;
    NaiveTime::from_hms_nano_opt(h, min, s, total_ns)
        .ok_or_else(|| format!("invalid time: {:02}:{:02}:{:02}.{:09}", h, min, s, total_ns))
}

fn naive_time_from_str(s: &str) -> Result<NaiveTime, String> {
    let (time_str, _) = split_tz(s);
    parse_time_str(time_str).map_err(|_| format!("cannot parse time: '{}'", s))
}

fn parse_time_str(s: &str) -> Result<NaiveTime, String> {
    // With colons
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M:%S%.f") {
        return Ok(t);
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M:%S") {
        return Ok(t);
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M") {
        return Ok(t);
    }
    // Compact (no colons)
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M%S%.f") {
        return Ok(t);
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M%S") {
        return Ok(t);
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M") {
        return Ok(t);
    }
    // Hour only
    if s.len() == 2 {
        if let Ok(h) = s.parse::<u32>() {
            return NaiveTime::from_hms_opt(h, 0, 0).ok_or_else(|| format!("invalid hour: {}", h));
        }
    }
    Err(format!("cannot parse time: '{}'", s))
}

fn time_to_obj(t: NaiveTime, tz: Option<&str>, type_name: &str) -> serde_json::Value {
    let ns = t.nanosecond();
    let ms = ns / 1_000_000;
    let us = (ns % 1_000_000) / 1_000;
    let ns_only = ns % 1_000;
    let mut m = serde_json::Map::new();
    m.insert("hour".to_string(), (t.hour() as i64).into());
    m.insert("minute".to_string(), (t.minute() as i64).into());
    m.insert("second".to_string(), (t.second() as i64).into());
    m.insert("millisecond".to_string(), (ms as i64).into());
    m.insert("microsecond".to_string(), (us as i64).into());
    m.insert("nanosecond".to_string(), (ns_only as i64).into());

    let str_repr = if t.nanosecond() == 0 && t.second() == 0 {
        // Omit seconds when zero.
        if let Some(z) = tz {
            format!("{}{}", t.format("%H:%M"), z)
        } else {
            t.format("%H:%M").to_string()
        }
    } else if t.nanosecond() == 0 {
        if let Some(z) = tz {
            format!("{}{}", t.format("%H:%M:%S"), z)
        } else {
            t.format("%H:%M:%S").to_string()
        }
    } else {
        let nano_str = format!("{:09}", t.nanosecond());
        let trimmed = nano_str.trim_end_matches('0');
        if let Some(z) = tz {
            format!("{}.{}{}", t.format("%H:%M:%S"), trimmed, z)
        } else {
            format!("{}.{}", t.format("%H:%M:%S"), trimmed)
        }
    };
    let local_ns = t.hour() as i64 * 3_600_000_000_000
        + t.minute() as i64 * 60_000_000_000
        + t.second() as i64 * 1_000_000_000
        + t.nanosecond() as i64;
    let sort_key = if let Some(z) = tz {
        let offset_ns = tz_offset_seconds(z) * 1_000_000_000;
        let utc_ns = (local_ns - offset_ns).rem_euclid(86_400_000_000_000);
        format!("{:020}", utc_ns)
    } else {
        format!("{:020}", local_ns)
    };
    if let Some(z) = tz {
        m.insert(
            "timezone".to_string(),
            serde_json::Value::String(z.to_string()),
        );
        m.insert(
            "offset".to_string(),
            serde_json::Value::String(z.to_string()),
        );
        m.insert("offsetSeconds".to_string(), tz_offset_seconds(z).into());
    }
    temporal_obj(type_name, m, str_repr.clone(), sort_key)
}

fn tz_offset_seconds(tz: &str) -> i64 {
    // Drop an optional bracketed zone-name suffix, e.g. "+02:00[Europe/Stockholm]".
    let tz = tz.split('[').next().unwrap_or(tz);
    // Parse "+HH:MM" or "-HH:MM" or "Z"
    if tz == "Z" {
        return 0;
    }
    let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
    let s = tz.trim_start_matches(['+', '-']);
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() == 2 {
        let h: i64 = parts[0].parse().unwrap_or(0);
        let m: i64 = parts[1].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    } else {
        0
    }
}

fn make_localtime(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            let t = naive_time_from_str(&s)?;
            Ok(time_to_obj(t, None, "LocalTime"))
        }
        serde_json::Value::Object(map) => {
            let t = naive_time_from_map(&map)?;
            Ok(time_to_obj(t, None, "LocalTime"))
        }
        other => Err(format!(
            "localtime() argument must be a string or map, got {}",
            other
        )),
    }
}

fn parse_tz(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    map.get("timezone")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            map.get("offset")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

fn make_time(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            // Split off timezone suffix
            let (time_part, tz_part) = split_tz(&s);
            let t = naive_time_from_str(time_part)?;
            Ok(time_to_obj(t, tz_part.as_deref(), "Time"))
        }
        serde_json::Value::Object(map) => {
            let t = naive_time_from_map(&map)?;
            let tz = parse_tz(&map);
            Ok(time_to_obj(t, tz.as_deref(), "Time"))
        }
        other => Err(format!(
            "time() argument must be a string or map, got {}",
            other
        )),
    }
}

/// Split a time/datetime string into the time portion and an optional normalized timezone.
/// Handles: Z, +HH:MM, -HH:MM, +HHMM, -HHMM, +HH, -HH
/// Normalizes all to +HH:MM form, with +00:00 / -00:00 → "Z".
fn split_tz(s: &str) -> (&str, Option<String>) {
    // Strip an optional bracketed IANA zone-name suffix, e.g. "[Europe/Stockholm]". The zone
    // name is preserved and appended to the resolved offset (e.g. "+02:00[Europe/Stockholm]").
    let (s, zone) = match (s.rfind('['), s.strip_suffix(']')) {
        (Some(open), Some(_)) => (&s[..open], Some(s[open + 1..s.len() - 1].to_string())),
        _ => (s, None),
    };

    // Append the zone-name suffix (if any) to a resolved numeric offset.
    let with_zone = |offset: String| -> String {
        match &zone {
            Some(z) => format!("{}[{}]", offset, z),
            None => offset,
        }
    };

    if let Some(rest) = s.strip_suffix('Z') {
        return (rest, Some(with_zone("Z".to_string())));
    }
    // Find last +/- that's after position 1 (avoid matching sign in time itself).
    // We search from the right for +/-.
    let bytes = s.as_bytes();
    for i in (1..s.len()).rev() {
        let c = bytes[i] as char;
        if c == '+' || c == '-' {
            let tz_raw = &s[i..];
            let time_part = &s[..i];
            // Parse the timezone offset.
            if let Some(tz) = normalize_tz(tz_raw) {
                return (time_part, Some(with_zone(tz)));
            }
        }
    }
    // No numeric offset present. If a zone name was given, resolve it to an offset.
    if let Some(z) = zone {
        return (s, Some(resolve_iana_tz(&z)));
    }
    (s, None)
}

/// Normalize a timezone string (+0100, +01:00, +01, -00:00, etc.) to canonical form.
/// Returns None if not a valid timezone.
/// Resolve an IANA timezone name to a `"+HH:MM[Name]"` representation.
/// For numeric offsets, normalize to "+HH:MM" or "Z".
/// For named timezones, use a hardcoded offset and append `"[Name]"`.
fn resolve_iana_tz(tz: &str) -> String {
    // If it already looks like a numeric offset (+HH:MM etc.), normalize it.
    if tz == "Z" || tz.starts_with('+') || tz.starts_with('-') {
        if let Some(n) = normalize_tz(tz) {
            return n;
        }
    }
    // Hardcoded IANA offsets (winter/standard time — simplified)
    let offset = match tz {
        "UTC" | "GMT" => "Z",
        "Europe/Stockholm" | "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid"
        | "Europe/Amsterdam" | "Europe/Brussels" | "CET" => "+01:00",
        "Europe/London" | "WET" => "Z",
        "Europe/Helsinki" | "Europe/Athens" | "EET" => "+02:00",
        "Europe/Moscow" | "MSK" => "+03:00",
        "America/New_York" | "US/Eastern" | "EST" => "-05:00",
        "America/Chicago" | "US/Central" => "-06:00",
        "America/Denver" | "US/Mountain" | "MST" => "-07:00",
        "America/Los_Angeles" | "US/Pacific" | "PST" => "-08:00",
        "America/Honolulu" | "Pacific/Honolulu" | "US/Hawaii" => "-10:00",
        "Asia/Tokyo" | "JST" => "+09:00",
        "Asia/Shanghai" | "Asia/Beijing" | "CST" => "+08:00",
        "Asia/Kolkata" | "IST" => "+05:30",
        "Australia/Sydney" | "AEST" => "+10:00",
        _ => "+00:00",
    };
    if offset == "Z" {
        format!("Z[{}]", tz)
    } else {
        format!("{}[{}]", offset, tz)
    }
}

fn normalize_tz(tz: &str) -> Option<String> {
    if tz.is_empty() {
        return None;
    }
    let sign = match tz.chars().next()? {
        '+' => '+',
        '-' => '-',
        _ => return None,
    };
    let rest = &tz[1..];

    let (hours, minutes) = if rest.contains(':') {
        // HH:MM format
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }
        let h: u32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        (h, m)
    } else if rest.len() == 4 {
        // HHMM format
        let h: u32 = rest[..2].parse().ok()?;
        let m: u32 = rest[2..].parse().ok()?;
        (h, m)
    } else if rest.len() == 2 {
        // HH format
        let h: u32 = rest.parse().ok()?;
        (h, 0)
    } else {
        return None;
    };

    if hours > 18 || minutes > 59 {
        return None;
    }

    // UTC → Z
    if hours == 0 && minutes == 0 {
        return Some("Z".to_string());
    }

    Some(format!("{}{:02}:{:02}", sign, hours, minutes))
}

fn datetime_to_obj(dt: NaiveDateTime, tz: Option<&str>, type_name: &str) -> serde_json::Value {
    let d = dt.date();
    let t = dt.time();
    let ns = t.nanosecond();
    let ms = ns / 1_000_000;
    let us = (ns % 1_000_000) / 1_000;
    let ns_only = ns % 1_000;
    let mut m = serde_json::Map::new();
    // Date fields
    m.insert("year".to_string(), d.year().into());
    m.insert("month".to_string(), (d.month() as i64).into());
    m.insert("day".to_string(), (d.day() as i64).into());
    m.insert("quarter".to_string(), ((d.month0() / 3 + 1) as i64).into());
    m.insert(
        "dayOfWeek".to_string(),
        (d.weekday().num_days_from_monday() as i64 + 1).into(),
    );
    m.insert("dayOfYear".to_string(), (d.ordinal() as i64).into());
    m.insert("ordinalDay".to_string(), (d.ordinal() as i64).into());
    let iso_week = d.iso_week();
    m.insert("week".to_string(), (iso_week.week() as i64).into());
    m.insert("weekYear".to_string(), (iso_week.year() as i64).into());
    // Time fields
    m.insert("hour".to_string(), (t.hour() as i64).into());
    m.insert("minute".to_string(), (t.minute() as i64).into());
    m.insert("second".to_string(), (t.second() as i64).into());
    m.insert("millisecond".to_string(), (ms as i64).into());
    m.insert("microsecond".to_string(), (us as i64).into());
    m.insert("nanosecond".to_string(), (ns_only as i64).into());
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap_or_default()
        .and_hms_opt(0, 0, 0)
        .unwrap_or_default();
    m.insert(
        "epochSeconds".to_string(),
        (dt.signed_duration_since(epoch).num_seconds()).into(),
    );
    m.insert(
        "epochMillis".to_string(),
        (dt.signed_duration_since(epoch).num_milliseconds()).into(),
    );
    if let Some(z) = tz {
        m.insert(
            "timezone".to_string(),
            serde_json::Value::String(z.to_string()),
        );
        m.insert(
            "offset".to_string(),
            serde_json::Value::String(z.to_string()),
        );
        m.insert("offsetSeconds".to_string(), tz_offset_seconds(z).into());
    }
    let time_repr = if t.nanosecond() == 0 && t.second() == 0 {
        t.format("%H:%M").to_string()
    } else if t.nanosecond() == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        let nano_str = format!("{:09}", t.nanosecond());
        let trimmed = nano_str.trim_end_matches('0');
        format!("{}.{}", t.format("%H:%M:%S"), trimmed)
    };
    let str_repr = if let Some(z) = tz {
        format!("{}T{}{}", d.format("%Y-%m-%d"), time_repr, z)
    } else {
        format!("{}T{}", d.format("%Y-%m-%d"), time_repr)
    };
    // Sort key: for local datetimes use ISO string (already lexicographically correct);
    // for timezone-aware datetimes shift to UTC so offsets compare correctly.
    let sort_key = if let Some(z) = tz {
        // Convert local datetime to UTC by subtracting the offset.
        let offset_secs = tz_offset_seconds(z);
        let utc_dt = dt - chrono::Duration::seconds(offset_secs);
        format!(
            "{}T{}",
            utc_dt.format("%Y-%m-%d"),
            utc_dt.format("%H:%M:%S%.9f")
        )
    } else {
        // Local datetime: ISO string sorts correctly for all dates in any year.
        format!("{}T{}", dt.format("%Y-%m-%d"), dt.format("%H:%M:%S%.9f"))
    };
    temporal_obj(type_name, m, str_repr, sort_key)
}

fn naive_datetime_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveDateTime, String> {
    let d = naive_date_from_map(map)?;
    let t = naive_time_from_map(map)?;
    Ok(d.and_time(t))
}

fn naive_datetime_from_str(s: &str) -> Result<(NaiveDateTime, Option<String>), String> {
    // Split date and time at the 'T' separator.
    let (date_str, time_str, tz) = if let Some(t_pos) = s.find('T') {
        let date_part = &s[..t_pos];
        let rest = &s[t_pos + 1..];
        let (time_part, tz_part) = split_tz(rest);
        (date_part, time_part, tz_part)
    } else {
        // No T separator: try date-only formats (defaulting time to midnight).
        let d = naive_date_from_str(s).map_err(|_| format!("cannot parse datetime: '{}'", s))?;
        return Ok((
            d.and_hms_opt(0, 0, 0)
                .ok_or_else(|| format!("failed to construct midnight for date: '{}'", s))?,
            None,
        ));
    };

    // Parse date and time independently using the comprehensive parsers.
    // This handles all ISO 8601 variants: extended, basic, week, ordinal, compact.
    let date =
        naive_date_from_str(date_str).map_err(|_| format!("cannot parse datetime: '{}'", s))?;
    let time = if time_str.is_empty() {
        NaiveTime::from_hms_opt(0, 0, 0)
            .ok_or_else(|| "failed to construct midnight NaiveTime".to_string())?
    } else {
        parse_time_str(time_str).map_err(|_| format!("cannot parse datetime: '{}'", s))?
    };
    Ok((NaiveDateTime::new(date, time), tz))
}

fn make_localdatetime(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            let (dt, _) = naive_datetime_from_str(&s)?;
            Ok(datetime_to_obj(dt, None, "LocalDateTime"))
        }
        serde_json::Value::Object(map) => {
            let dt = naive_datetime_from_map(&map)?;
            Ok(datetime_to_obj(dt, None, "LocalDateTime"))
        }
        other => Err(format!(
            "localdatetime() argument must be a string or map, got {}",
            other
        )),
    }
}

fn make_datetime(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            let (dt, tz) = naive_datetime_from_str(&s)?;
            Ok(datetime_to_obj(dt, tz.as_deref(), "DateTime"))
        }
        serde_json::Value::Object(ref map)
            if map.get("__type__").and_then(|v| v.as_str()).is_some() =>
        {
            // Temporal object → convert/reinterpret as DateTime
            let d = obj_to_naive_date(map)?;
            let t = obj_to_naive_time(map);
            let tz = map
                .get("timezone")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(datetime_to_obj(d.and_time(t), tz.as_deref(), "DateTime"))
        }
        serde_json::Value::Object(map) => {
            let tz_override = parse_tz(&map);

            // Check for {date: ..., time: ...} selection pattern.
            let date_val = map.get("date").cloned();
            let time_val = map.get("time").cloned();

            if date_val.is_some() || time_val.is_some() {
                let base_date = match &date_val {
                    Some(serde_json::Value::Object(dm)) if dm.get("__type__").is_some() => {
                        obj_to_naive_date(dm)?
                    }
                    Some(serde_json::Value::String(s)) => naive_date_from_str(s)?,
                    _ => NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default(),
                };

                let (base_time, base_tz) = match &time_val {
                    Some(serde_json::Value::Object(tm)) if tm.get("__type__").is_some() => {
                        let t = obj_to_naive_time(tm);
                        let tz = tm
                            .get("timezone")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        (t, tz)
                    }
                    _ => (NaiveTime::default(), None),
                };

                // Apply field overrides from the rest of the map.
                let final_date = apply_date_overrides(base_date, &map)?;
                let mut dt = final_date.and_time(base_time);

                // Apply time field overrides.
                let get_i64 = |k: &str| -> Option<i64> { map.get(k)?.as_i64() };
                let h = get_i64("hour").unwrap_or(dt.hour() as i64) as u32;
                let m = get_i64("minute").unwrap_or(dt.minute() as i64) as u32;
                let s = get_i64("second").unwrap_or(dt.second() as i64) as u32;
                let ms = get_i64("millisecond").unwrap_or(0) as u32;
                let us = get_i64("microsecond").unwrap_or(0) as u32;
                let ns_only = get_i64("nanosecond").unwrap_or(0) as u32;
                let total_ns = ns_only + us * 1_000 + ms * 1_000_000;
                if let Some(t) = NaiveTime::from_hms_nano_opt(h, m, s, total_ns) {
                    dt = final_date.and_time(t);
                }

                let effective_tz = tz_override.or(base_tz);
                // Default to Z when constructing from a LocalTime (no tz)
                let effective_tz = if time_val.is_some() && effective_tz.is_none() {
                    Some("Z".to_string())
                } else {
                    effective_tz
                };
                return Ok(datetime_to_obj(dt, effective_tz.as_deref(), "DateTime"));
            }

            let dt = naive_datetime_from_map(&map)?;
            Ok(datetime_to_obj(dt, tz_override.as_deref(), "DateTime"))
        }
        other => Err(format!(
            "datetime() argument must be a string or map, got {}",
            other
        )),
    }
}

fn obj_to_naive_time(obj: &serde_json::Map<String, serde_json::Value>) -> NaiveTime {
    let get_i64 = |k: &str| -> u32 { obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as u32 };
    let h = get_i64("hour");
    let m = get_i64("minute");
    let s = get_i64("second");
    let ms = get_i64("millisecond");
    let us = get_i64("microsecond");
    let ns = get_i64("nanosecond");
    let total_ns = ns + us * 1_000 + ms * 1_000_000;
    NaiveTime::from_hms_nano_opt(h, m, s, total_ns).unwrap_or_default()
}

// ── Duration ──────────────────────────────────────────────────────────────────

fn duration_obj(months: i64, days: i64, seconds: i64, nanos: i64) -> serde_json::Value {
    let total_months = months;
    let total_days = days;
    let total_seconds = seconds;
    let total_nanos = nanos;

    let years = total_months / 12;
    let rem_months = total_months % 12;
    let hours = total_seconds / 3600;
    let rem_secs = total_seconds % 3600;
    let minutes = rem_secs / 60;
    let secs = rem_secs % 60;
    let ms = total_nanos / 1_000_000;
    let us = (total_nanos % 1_000_000) / 1_000;
    let ns = total_nanos % 1_000;

    let mut m = serde_json::Map::new();
    m.insert("years".to_string(), years.into());
    m.insert("months".to_string(), total_months.into());
    m.insert("days".to_string(), total_days.into());
    m.insert("hours".to_string(), hours.into());
    m.insert("minutes".to_string(), minutes.into());
    m.insert("seconds".to_string(), total_seconds.into());
    m.insert("milliseconds".to_string(), ms.into());
    m.insert("microseconds".to_string(), us.into());
    m.insert("nanoseconds".to_string(), total_nanos.into());
    m.insert("quartersOfYear".to_string(), (rem_months / 3).into());
    m.insert("monthsOfYear".to_string(), rem_months.into());
    m.insert("monthsOfQuarter".to_string(), (rem_months % 3).into());
    m.insert("daysOfWeek".to_string(), (total_days / 7).into());
    m.insert("hoursOfDay".to_string(), (hours % 24).into());
    m.insert("minutesOfHour".to_string(), minutes.into());
    m.insert("secondsOfMinute".to_string(), secs.into());
    m.insert("millisecondsOfSecond".to_string(), (ms % 1000).into());
    m.insert("microsecondsOfSecond".to_string(), us.into());
    m.insert("nanosecondsOfSecond".to_string(), ns.into());

    // ISO 8601 duration string
    let str_repr = format_duration(
        years, rem_months, total_days, hours, minutes, secs, ms, us, ns,
    );
    let sort_key = format!(
        "P{:020}Y{:02}M{:020}DT{:020}H{:02}M{:02}.{:03}{:03}{:03}S",
        years.abs(),
        rem_months.abs(),
        total_days.abs(),
        hours.abs(),
        minutes.abs(),
        secs.abs(),
        ms.abs() % 1000,
        us.abs(),
        ns.abs()
    );
    temporal_obj("Duration", m, str_repr, sort_key)
}

#[allow(clippy::too_many_arguments)]
fn format_duration(
    years: i64,
    months: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    secs: i64,
    ms: i64,
    us: i64,
    ns: i64,
) -> String {
    let has_time = hours != 0 || minutes != 0 || secs != 0 || ms != 0 || us != 0 || ns != 0;
    let mut s = String::from("P");
    if years != 0 {
        s.push_str(&format!("{}Y", years));
    }
    if months != 0 {
        s.push_str(&format!("{}M", months));
    }
    if days != 0 {
        s.push_str(&format!("{}D", days));
    }
    if has_time {
        s.push('T');
        if hours != 0 {
            s.push_str(&format!("{}H", hours));
        }
        if minutes != 0 {
            s.push_str(&format!("{}M", minutes));
        }
        if secs != 0 || ms != 0 || us != 0 || ns != 0 {
            let frac = ms * 1_000_000 + us * 1_000 + ns;
            if frac == 0 {
                s.push_str(&format!("{}S", secs));
            } else {
                let frac_str = format!("{:09}", frac.abs());
                let trimmed = frac_str.trim_end_matches('0');
                s.push_str(&format!("{}.{}S", secs, trimmed));
            }
        }
    }
    if s == "P" {
        s.push_str("T0S");
    }
    s
}

fn parse_iso_duration(s: &str) -> Result<(i64, i64, i64, i64), String> {
    // Returns (months, days, seconds, nanoseconds)
    let s = s.trim();
    if !s.starts_with('P') {
        return Err(format!("invalid duration string: '{}'", s));
    }
    let (date_part, time_part) = if let Some(t_pos) = s.find('T') {
        (&s[1..t_pos], Some(&s[t_pos + 1..]))
    } else {
        (&s[1..], None)
    };

    // Accumulate into the three duration groups (months, days, seconds) as f64 so that a
    // fractional component cascades into the next smaller unit, per openCypher duration
    // semantics: e.g. P0.5D = PT12H, PT0.75M = PT45S, P1.5Y = P1Y6M. Cascades use exact
    // ratios only (year=12 months, week=7 days, day=86400 s, hour=3600 s, minute=60 s); a
    // fractional month has no exact day ratio and so is truncated.
    let mut months_f = 0f64;
    let mut days_f = 0f64;
    let mut seconds_f = 0f64;

    let mut parse_component = |part: &str, is_time: bool| -> Result<(), String> {
        let mut buf = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' || c == '-' {
                buf.push(c);
            } else {
                let v: f64 = buf.parse().unwrap_or(0.0);
                buf.clear();
                if is_time {
                    match c {
                        'H' => seconds_f += v * 3600.0,
                        'M' => seconds_f += v * 60.0,
                        'S' => seconds_f += v,
                        _ => {}
                    }
                } else {
                    match c {
                        'Y' => months_f += v * 12.0,
                        'M' => months_f += v,
                        'W' => days_f += v * 7.0,
                        'D' => days_f += v,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    };

    parse_component(date_part, false)?;
    if let Some(tp) = time_part {
        parse_component(tp, true)?;
    }

    // Cascade the fractional day part down into seconds before splitting seconds and nanos.
    seconds_f += days_f.fract() * 86_400.0;

    let total_months = months_f.trunc() as i64;
    let total_days = days_f.trunc() as i64;
    let total_seconds = seconds_f.trunc() as i64;
    let nanos = (seconds_f.fract() * 1_000_000_000.0).round() as i64;
    Ok((total_months, total_days, total_seconds, nanos))
}

fn make_duration(arg: serde_json::Value) -> Result<serde_json::Value, String> {
    match arg {
        serde_json::Value::Null => Ok(serde_json::Value::Null),
        serde_json::Value::String(s) => {
            let (months, days, seconds, nanos) = parse_iso_duration(&s)?;
            Ok(duration_obj(months, days, seconds, nanos))
        }
        serde_json::Value::Object(map) => {
            let get_i64 = |k: &str| -> i64 { map.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
            let get_f64 = |k: &str| -> f64 { map.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0) };

            let years = get_i64("years");
            let months = get_i64("months");
            let weeks = get_i64("weeks");
            let days = get_i64("days");
            let hours = get_i64("hours");
            let minutes = get_i64("minutes");
            let secs_f = get_f64("seconds");
            let secs = secs_f as i64;
            let ms = get_i64("milliseconds");
            let us = get_i64("microseconds");
            let ns = get_i64("nanoseconds");

            let total_months = years * 12 + months;
            let total_days = weeks * 7 + days;
            let total_seconds = hours * 3600 + minutes * 60 + secs;
            let frac_nanos = ((secs_f.fract()) * 1_000_000_000.0) as i64;
            let total_nanos = frac_nanos + ms * 1_000_000 + us * 1_000 + ns;

            Ok(duration_obj(
                total_months,
                total_days,
                total_seconds,
                total_nanos,
            ))
        }
        other => Err(format!(
            "duration() argument must be a string or map, got {}",
            other
        )),
    }
}

fn get_duration_fields(obj: &serde_json::Map<String, serde_json::Value>) -> (i64, i64, i64, i64) {
    let get_i64 = |k: &str| -> i64 { obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
    let months = get_i64("months");
    let days = get_i64("days");
    let seconds = get_i64("seconds");
    let nanos = get_i64("nanoseconds");
    (months, days, seconds, nanos)
}

fn temporal_type(obj: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    obj.get("__type__")?.as_str()
}

pub(super) fn temporal_arithmetic(
    lv: &serde_json::Value,
    rv: &serde_json::Value,
    op: char,
) -> Option<Result<serde_json::Value, String>> {
    let lo = lv.as_object()?;
    let ro = rv.as_object()?;
    let lt = temporal_type(lo)?;
    let rt = temporal_type(ro)?;

    // Duration ± Duration
    if lt == "Duration" && rt == "Duration" {
        let (lm, ld, ls, ln) = get_duration_fields(lo);
        let (rm, rd, rs, rn) = get_duration_fields(ro);
        let (months, days, seconds, nanos) = match op {
            '+' => (lm + rm, ld + rd, ls + rs, ln + rn),
            '-' => (lm - rm, ld - rd, ls - rs, ln - rn),
            _ => return None,
        };
        return Some(Ok(duration_obj(months, days, seconds, nanos)));
    }

    // Date - Date → Duration
    let is_date_type = |t: &str| matches!(t, "Date" | "LocalDateTime" | "DateTime");
    if is_date_type(lt) && is_date_type(rt) && op == '-' {
        let d1 = obj_to_naive_date(lo).ok()?;
        let d2 = obj_to_naive_date(ro).ok()?;
        let diff_days = d1.signed_duration_since(d2).num_days();
        return Some(Ok(duration_obj(0, diff_days, 0, 0)));
    }

    None
}

// Separate non-overlapping case for Date ± Duration
pub(super) fn temporal_arithmetic_date_duration(
    lv: &serde_json::Value,
    rv: &serde_json::Value,
    op: char,
) -> Option<Result<serde_json::Value, String>> {
    fn is_date_like(t: Option<&str>) -> bool {
        matches!(
            t,
            Some("Date" | "LocalDateTime" | "DateTime" | "LocalTime" | "Time")
        )
    }
    let (date_obj, dur_obj, sign) = match (lv, rv, op) {
        (serde_json::Value::Object(lo), serde_json::Value::Object(ro), '+')
            if is_date_like(temporal_type(lo)) && temporal_type(ro) == Some("Duration") =>
        {
            (lo, ro, 1i64)
        }
        (serde_json::Value::Object(lo), serde_json::Value::Object(ro), '-')
            if is_date_like(temporal_type(lo)) && temporal_type(ro) == Some("Duration") =>
        {
            (lo, ro, -1i64)
        }
        (serde_json::Value::Object(ro), serde_json::Value::Object(lo), '+')
            if temporal_type(ro) == Some("Duration") && is_date_like(temporal_type(lo)) =>
        {
            (lo, ro, 1i64)
        }
        _ => return None,
    };

    let type_name = temporal_type(date_obj)?;
    let (months, days, secs, _nanos) = get_duration_fields(dur_obj);

    match type_name {
        "Date" => {
            let d = obj_to_naive_date(date_obj).ok()?;
            let d2 = add_months_days(d, sign * months, sign * days);
            let d3 = d2 + Duration::seconds(sign * secs);
            Some(Ok(date_to_obj(d3.date())))
        }
        "LocalDateTime" | "DateTime" => {
            let dt = obj_to_naive_datetime(date_obj).ok()?;
            let tz = date_obj
                .get("timezone")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let d = dt.date();
            let d2 = add_months_days(d, sign * months, sign * days);
            let dt2 = d2 + Duration::seconds(sign * secs);
            Some(Ok(datetime_to_obj(dt2, tz.as_deref(), type_name)))
        }
        _ => None,
    }
}

fn add_months_days(d: NaiveDate, months: i64, days: i64) -> NaiveDateTime {
    let year = d.year() as i64;
    let month = d.month() as i64;
    let total_months = year * 12 + month - 1 + months;
    let new_year = (total_months / 12) as i32;
    let new_month = (total_months % 12 + 1) as u32;
    let max_day = days_in_month(new_year, new_month);
    let new_day = d.day().min(max_day);
    let base = NaiveDate::from_ymd_opt(new_year, new_month, new_day)
        .unwrap_or(d)
        .and_hms_opt(0, 0, 0)
        .unwrap_or_else(|| d.and_time(NaiveTime::default()));
    base + Duration::days(days)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn obj_to_naive_date(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveDate, String> {
    let get_i64 = |k: &str| -> i64 { obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
    let year = get_i64("year") as i32;
    let month = get_i64("month") as u32;
    let day = get_i64("day") as u32;
    NaiveDate::from_ymd_opt(year, month.max(1), day.max(1))
        .ok_or_else(|| format!("invalid date fields: {}-{}-{}", year, month, day))
}

fn obj_to_naive_datetime(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<NaiveDateTime, String> {
    let d = obj_to_naive_date(obj)?;
    let get_i64 = |k: &str| -> i64 { obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
    let h = get_i64("hour") as u32;
    let min = get_i64("minute") as u32;
    let s = get_i64("second") as u32;
    let ns = get_i64("nanosecond") as u32;
    let t = NaiveTime::from_hms_nano_opt(h, min, s, ns)
        .ok_or_else(|| format!("invalid time fields: {}:{}:{}", h, min, s))?;
    Ok(d.and_time(t))
}

fn temporal_duration_between(
    a: &serde_json::Value,
    b: &serde_json::Value,
    name: &str,
) -> Result<serde_json::Value, String> {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    let zero_time = NaiveTime::default();

    let ao = match a.as_object() {
        Some(m) => m,
        None => return Ok(duration_obj(0, 0, 0, 0)),
    };
    let bo = match b.as_object() {
        Some(m) => m,
        None => return Ok(duration_obj(0, 0, 0, 0)),
    };

    let ta = temporal_type(ao).unwrap_or("");
    let tb = temporal_type(bo).unwrap_or("");

    let a_is_date = matches!(ta, "Date" | "LocalDateTime" | "DateTime");
    let b_is_date = matches!(tb, "Date" | "LocalDateTime" | "DateTime");
    let a_is_time = matches!(ta, "LocalTime" | "Time");
    let b_is_time = matches!(tb, "LocalTime" | "Time");

    let a_date = if a_is_date {
        obj_to_naive_date(ao).unwrap_or(epoch)
    } else {
        epoch
    };
    let b_date = if b_is_date {
        obj_to_naive_date(bo).unwrap_or(epoch)
    } else {
        epoch
    };
    let a_time = obj_to_naive_time(ao);
    let b_time = obj_to_naive_time(bo);

    match name {
        "duration.inmonths" => {
            if !(a_is_date && b_is_date) {
                return Ok(duration_obj(0, 0, 0, 0));
            }
            let months = calendar_month_diff(a_date, b_date);
            Ok(duration_obj(months, 0, 0, 0))
        }
        "duration.indays" => {
            if !(a_is_date && b_is_date) {
                return Ok(duration_obj(0, 0, 0, 0));
            }
            let a_dt = a_date.and_time(a_time);
            let b_dt = b_date.and_time(b_time);
            let total_secs = b_dt.signed_duration_since(a_dt).num_seconds();
            let days = total_secs / 86_400;
            let rem_secs = total_secs % 86_400;
            Ok(duration_obj(0, days, rem_secs, 0))
        }
        "duration.inseconds" => {
            let a_dt = if a_is_date || a_is_time {
                a_date.and_time(a_time)
            } else {
                epoch.and_time(zero_time)
            };
            let b_dt = if b_is_date || b_is_time {
                b_date.and_time(b_time)
            } else {
                epoch.and_time(zero_time)
            };
            let diff_ns = b_dt
                .signed_duration_since(a_dt)
                .num_nanoseconds()
                .unwrap_or(0);
            let secs = diff_ns / 1_000_000_000;
            let ns = diff_ns % 1_000_000_000;
            Ok(duration_obj(0, 0, secs, ns))
        }
        _ => {
            // duration.between: full calendar diff
            if a_is_date && b_is_date {
                let (months, days, secs, ns) = calendar_full_diff(a_date, a_time, b_date, b_time);
                Ok(duration_obj(months, days, secs, ns))
            } else if a_is_time && b_is_time {
                let diff_ns = b_time
                    .signed_duration_since(a_time)
                    .num_nanoseconds()
                    .unwrap_or(0);
                let secs = diff_ns / 1_000_000_000;
                let ns = diff_ns % 1_000_000_000;
                Ok(duration_obj(0, 0, secs, ns))
            } else if a_is_date && b_is_time {
                // date vs time: treat date time as 00:00, return just the time part
                let diff_ns = b_time
                    .signed_duration_since(zero_time)
                    .num_nanoseconds()
                    .unwrap_or(0);
                let secs = diff_ns / 1_000_000_000;
                let ns = diff_ns % 1_000_000_000;
                Ok(duration_obj(0, 0, secs, ns))
            } else if a_is_time && b_is_date {
                let diff_ns = zero_time
                    .signed_duration_since(a_time)
                    .num_nanoseconds()
                    .unwrap_or(0);
                let secs = diff_ns / 1_000_000_000;
                let ns = diff_ns % 1_000_000_000;
                Ok(duration_obj(0, 0, secs, ns))
            } else {
                Ok(duration_obj(0, 0, 0, 0))
            }
        }
    }
}

/// Compute the number of whole calendar months from `from` to `to` (signed).
fn calendar_month_diff(from: NaiveDate, to: NaiveDate) -> i64 {
    let from_ym = from.year() as i64 * 12 + from.month() as i64 - 1;
    let to_ym = to.year() as i64 * 12 + to.month() as i64 - 1;
    let mut months = to_ym - from_ym;

    // Adjust: if the day in `to` hasn't reached `from`'s day yet (going forward),
    // or if going backward and the day has passed `from`'s day, reduce by 1.
    let clamped_from_day = {
        let ym = from_ym + months;
        let (y, m) = ((ym / 12) as i32, (ym % 12 + 1) as u32);
        from.day().min(days_in_month(y, m))
    };

    if months >= 0 && to.day() < clamped_from_day {
        months -= 1;
    } else if months < 0 && to.day() > clamped_from_day {
        months += 1;
    }
    months
}

/// Full calendar difference: (months, days, seconds, nanoseconds).
fn calendar_full_diff(
    a_date: NaiveDate,
    a_time: NaiveTime,
    b_date: NaiveDate,
    b_time: NaiveTime,
) -> (i64, i64, i64, i64) {
    let forward = b_date > a_date || (b_date == a_date && b_time >= a_time);
    let (from_d, from_t, to_d, to_t) = if forward {
        (a_date, a_time, b_date, b_time)
    } else {
        (b_date, b_time, a_date, a_time)
    };

    // Count full months
    let months = calendar_month_diff(from_d, to_d);
    let (intermediate_year, intermediate_month) = {
        let ym = from_d.year() as i64 * 12 + from_d.month() as i64 - 1 + months;
        ((ym / 12) as i32, (ym % 12 + 1) as u32)
    };
    let intermediate_day = from_d
        .day()
        .min(days_in_month(intermediate_year, intermediate_month));
    let intermediate =
        NaiveDate::from_ymd_opt(intermediate_year, intermediate_month, intermediate_day)
            .unwrap_or(from_d);

    // Remaining days
    let days = (to_d - intermediate).num_days();

    // Remaining seconds/nanos from time difference
    let time_diff_ns = to_t
        .signed_duration_since(from_t)
        .num_nanoseconds()
        .unwrap_or(0);
    let secs = time_diff_ns / 1_000_000_000;
    let ns = time_diff_ns % 1_000_000_000;

    let sign: i64 = if forward { 1 } else { -1 };
    (sign * months, sign * days, sign * secs, sign * ns)
}

fn temporal_truncate(
    name: &str,
    unit: &serde_json::Value,
    temporal: &serde_json::Value,
    overrides: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let unit_str = match unit {
        serde_json::Value::String(s) => s.to_ascii_lowercase(),
        _ => return Err(format!("{name}() unit must be a string")),
    };

    let get_override_i64 = |k: &str| -> Option<i64> {
        overrides
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(k))
            .and_then(|v| v.as_i64())
    };

    match temporal {
        serde_json::Value::Object(obj) => {
            let t_type = temporal_type(obj).unwrap_or("").to_string();
            match t_type.as_str() {
                "Date" | "LocalDateTime" | "DateTime" => {
                    let d = obj_to_naive_date(obj)?;
                    let ov_map = overrides.and_then(|v| v.as_object());
                    let get_ov = |k: &str| -> Option<i64> {
                        ov_map.and_then(|m| m.get(k)).and_then(|v| v.as_i64())
                    };

                    // Time-component units only apply to DateTime/LocalDateTime; for Date
                    // they degenerate to 'day' (the date is unchanged).
                    let time_unit = matches!(
                        unit_str.as_str(),
                        "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
                    );

                    // Compute the truncated date.
                    let truncated = match unit_str.as_str() {
                        "millennium" => {
                            NaiveDate::from_ymd_opt((d.year() / 1000) * 1000, 1, 1).unwrap_or(d)
                        }
                        "century" => {
                            NaiveDate::from_ymd_opt((d.year() / 100) * 100, 1, 1).unwrap_or(d)
                        }
                        "decade" => {
                            NaiveDate::from_ymd_opt((d.year() / 10) * 10, 1, 1).unwrap_or(d)
                        }
                        "year" => NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d),
                        "quarter" => {
                            let q_month = ((d.month0() / 3) * 3 + 1) as u32;
                            NaiveDate::from_ymd_opt(d.year(), q_month, 1).unwrap_or(d)
                        }
                        "month" => NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d),
                        "weekyear" => {
                            // Truncate to start of ISO week-year (Monday of week 1).
                            let iso = d.iso_week();
                            NaiveDate::from_isoywd_opt(iso.year(), 1, Weekday::Mon).unwrap_or(d)
                        }
                        "week" => {
                            let target_dow = get_ov("dayOfWeek").unwrap_or(1);
                            let wd = num_to_weekday(target_dow as u32).unwrap_or(Weekday::Mon);
                            let monday =
                                d - Duration::days(d.weekday().num_days_from_monday() as i64);
                            let offset = wd.num_days_from_monday() as i64;
                            monday + Duration::days(offset)
                        }
                        // Time-component units: keep the date as-is.
                        _ => d,
                    };

                    // Apply calendar field overrides (excluding time-component units which don't modify date).
                    let final_date = if !time_unit {
                        // For week truncation, dayOfWeek was already handled above; skip it in apply_date_overrides.
                        let empty = serde_json::Map::new();
                        let map = ov_map.unwrap_or(&empty);
                        if unit_str == "week" {
                            // dayOfWeek was used above; only apply other overrides.
                            let filtered: serde_json::Map<String, serde_json::Value> = map
                                .iter()
                                .filter(|(k, _)| *k != "dayOfWeek")
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            if filtered.is_empty() {
                                truncated
                            } else {
                                apply_date_overrides(truncated, &filtered)?
                            }
                        } else if overrides.is_some() && unit_str != "weekyear" {
                            apply_date_overrides(truncated, map)?
                        } else if overrides.is_some() && unit_str == "weekyear" {
                            // For weekYear, only day override makes sense
                            apply_date_overrides(truncated, map)?
                        } else {
                            truncated
                        }
                    } else {
                        truncated
                    };

                    // The output type is determined by the function name, not the input type.
                    let output_type = match name {
                        "date.truncate" => "Date",
                        "datetime.truncate" => "DateTime",
                        "localdatetime.truncate" => "LocalDateTime",
                        _ if t_type == "Date" => "Date",
                        _ => "LocalDateTime",
                    };

                    if output_type == "Date" {
                        Ok(date_to_obj(final_date))
                    } else {
                        // For time-component units: keep existing time, zero trailing components.
                        let base_time = obj_to_naive_time(obj);
                        let truncated_time = match unit_str.as_str() {
                            "hour" => {
                                let h = base_time.hour();
                                NaiveTime::from_hms_nano_opt(h, 0, 0, 0).unwrap_or(base_time)
                            }
                            "minute" => NaiveTime::from_hms_nano_opt(
                                base_time.hour(),
                                base_time.minute(),
                                0,
                                0,
                            )
                            .unwrap_or(base_time),
                            "second" => NaiveTime::from_hms_nano_opt(
                                base_time.hour(),
                                base_time.minute(),
                                base_time.second(),
                                0,
                            )
                            .unwrap_or(base_time),
                            "millisecond" => {
                                let ns = (base_time.nanosecond() / 1_000_000) * 1_000_000;
                                NaiveTime::from_hms_nano_opt(
                                    base_time.hour(),
                                    base_time.minute(),
                                    base_time.second(),
                                    ns,
                                )
                                .unwrap_or(base_time)
                            }
                            "microsecond" => {
                                let ns = (base_time.nanosecond() / 1_000) * 1_000;
                                NaiveTime::from_hms_nano_opt(
                                    base_time.hour(),
                                    base_time.minute(),
                                    base_time.second(),
                                    ns,
                                )
                                .unwrap_or(base_time)
                            }
                            _ => NaiveTime::default(),
                        };
                        // Timezone: override map → input → default (Z for DateTime, none for LocalDateTime)
                        let override_tz = ov_map
                            .and_then(|m| m.get("timezone"))
                            .and_then(|v| v.as_str())
                            .map(resolve_iana_tz);
                        let input_tz = obj
                            .get("timezone")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let tz = override_tz.or(input_tz).or_else(|| {
                            if output_type == "DateTime" {
                                Some("Z".to_string())
                            } else {
                                None
                            }
                        });
                        Ok(datetime_to_obj(
                            final_date.and_time(truncated_time),
                            tz.as_deref(),
                            output_type,
                        ))
                    }
                }
                "LocalTime" | "Time" => {
                    // Truncate time component
                    let get_i64 =
                        |k: &str| -> i64 { obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
                    let (h, m, s, ns) = match unit_str.as_str() {
                        "hour" => (get_i64("hour"), 0, 0, 0),
                        "minute" => (get_i64("hour"), get_i64("minute"), 0, 0),
                        "second" => (get_i64("hour"), get_i64("minute"), get_i64("second"), 0),
                        "millisecond" => {
                            let ms = get_i64("millisecond");
                            (
                                get_i64("hour"),
                                get_i64("minute"),
                                get_i64("second"),
                                ms * 1_000_000,
                            )
                        }
                        "microsecond" => {
                            let ms = get_i64("millisecond");
                            let us = get_i64("microsecond");
                            (
                                get_i64("hour"),
                                get_i64("minute"),
                                get_i64("second"),
                                ms * 1_000_000 + us * 1_000,
                            )
                        }
                        _ => (
                            get_i64("hour"),
                            get_i64("minute"),
                            get_i64("second"),
                            get_i64("nanosecond"),
                        ),
                    };
                    let h = get_override_i64("hour").unwrap_or(h);
                    let m = get_override_i64("minute").unwrap_or(m);
                    let s = get_override_i64("second").unwrap_or(s);
                    let t = NaiveTime::from_hms_nano_opt(h as u32, m as u32, s as u32, ns as u32)
                        .unwrap_or_default();
                    let tz = obj
                        .get("timezone")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    Ok(time_to_obj(t, tz.as_deref(), &t_type))
                }
                _ => Ok(temporal.clone()),
            }
        }
        _ => Ok(temporal.clone()),
    }
}

pub(super) fn literal_to_value(l: &Literal) -> serde_json::Value {
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

/// Sentinel JSON object used to represent IEEE 754 NaN.
pub(super) fn nan_value() -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert(
        "__type__".to_string(),
        serde_json::Value::String("__NaN__".to_string()),
    );
    serde_json::Value::Object(m)
}

pub(super) fn is_nan(v: &serde_json::Value) -> bool {
    v.as_object()
        .and_then(|m| m.get("__type__"))
        .and_then(|t| t.as_str())
        .is_some_and(|s| s == "__NaN__")
}

pub(super) fn cypher_eq(lv: &serde_json::Value, rv: &serde_json::Value) -> serde_json::Value {
    if lv == &serde_json::Value::Null || rv == &serde_json::Value::Null {
        return serde_json::Value::Null;
    }
    if is_nan(lv) || is_nan(rv) {
        return serde_json::Value::Bool(false);
    }
    if let (serde_json::Value::Array(la), serde_json::Value::Array(ra)) = (lv, rv) {
        if la.len() != ra.len() {
            return serde_json::Value::Bool(false);
        }
        let mut has_null = false;
        for (a, b) in la.iter().zip(ra.iter()) {
            let item_eq = cypher_eq(a, b);
            if item_eq == serde_json::Value::Null {
                has_null = true;
            } else if item_eq == serde_json::Value::Bool(false) {
                return serde_json::Value::Bool(false);
            }
        }
        if has_null {
            serde_json::Value::Null
        } else {
            serde_json::Value::Bool(true)
        }
    } else {
        serde_json::Value::Bool(lv == rv)
    }
}

pub(super) fn json_cmp(l: &serde_json::Value, r: &serde_json::Value) -> Option<std::cmp::Ordering> {
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
        // Booleans sort: false < true
        (serde_json::Value::Bool(b1), serde_json::Value::Bool(b2)) => Some(b1.cmp(b2)),
        // Lists: lexicographic element-by-element comparison.
        // A null element or incompatible element types make the comparison uncertain (None).
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            let min_len = a.len().min(b.len());
            for i in 0..min_len {
                if a[i] == serde_json::Value::Null || b[i] == serde_json::Value::Null {
                    return None;
                }
                match json_cmp(&a[i], &b[i]) {
                    None => return None,
                    Some(std::cmp::Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(a.len().cmp(&b.len()))
        }
        // Temporal objects: compare by __sort_key field (ISO string, lexicographically correct)
        (serde_json::Value::Object(m1), serde_json::Value::Object(m2)) => {
            let t1 = m1.get("__type__").and_then(|v| v.as_str());
            let t2 = m2.get("__type__").and_then(|v| v.as_str());
            match (t1, t2) {
                (Some(a), Some(b)) if a == b => {
                    let k1 = m1.get("__sort_key__").and_then(|v| v.as_str());
                    let k2 = m2.get("__sort_key__").and_then(|v| v.as_str());
                    match (k1, k2) {
                        (Some(s1), Some(s2)) => Some(s1.cmp(s2)),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}
