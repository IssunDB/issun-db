use super::*;

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
                (serde_json::Value::Array(arr), serde_json::Value::Number(n)) => {
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
                (serde_json::Value::String(s), serde_json::Value::Number(n)) => {
                    let i = n.as_i64().unwrap_or(0);
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let pos = if i < 0 { len + i } else { i };
                    if pos < 0 || pos >= len {
                        Ok(serde_json::Value::Null)
                    } else {
                        Ok(serde_json::Value::String(chars[pos as usize].to_string()))
                    }
                }
                // Node/edge property access via subscript: n['propName']
                (node_val, serde_json::Value::String(key)) => {
                    // node_val might be an object (from a node binding deserialized)
                    if let serde_json::Value::Object(map) = node_val {
                        Ok(map.get(&key).cloned().unwrap_or(serde_json::Value::Null))
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                }
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
                (serde_json::Value::Null, _) | (_, serde_json::Value::Null) => {
                    Ok(serde_json::Value::Null)
                }
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
pub(super) fn eval_function_call(
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
            // Handle null argument.
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
                        }
                        Some(GraphBinding::Scalar(serde_json::Value::Null)) => {
                            return Ok(serde_json::Value::Null);
                        }
                        _ => {}
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
                serde_json::Value::Bool(b) => Ok(serde_json::Value::String(b.to_string())),
                serde_json::Value::Number(n) => Ok(serde_json::Value::String(n.to_string())),
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
        _ => Err(format!("unknown function: {}", name)),
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
        _ => None,
    }
}
