use super::expr::*;
use super::factorize::{FactorizedRecordGroup, filter_refs_in_expr};
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

/// Validate a SKIP or LIMIT parameter value at runtime.
///
/// Cypher requires SKIP/LIMIT to be non-negative integers. When the value
/// comes from a query parameter (`SKIP $n`), this validation must happen at
/// execution time when the parameter is resolved.
fn validate_skip_limit_param(
    expr: &Expr,
    keyword: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    if let Expr::Param(name) = expr {
        match params.get(name) {
            None => return Err(format!("ParameterMissing: parameter ${name} is not set")),
            Some(serde_json::Value::Number(n)) => {
                if n.as_f64().is_some_and(|f| f != f.floor()) {
                    return Err(format!(
                        "SyntaxError: {keyword} requires an integer but got a float (${name})"
                    ));
                }
                if let Some(i) = n.as_i64() {
                    if i < 0 {
                        return Err(format!(
                            "SyntaxError: {keyword} value must not be negative (got {i})"
                        ));
                    }
                } else if n.as_f64().is_some_and(|f| f < 0.0) {
                    return Err(format!("SyntaxError: {keyword} value must not be negative"));
                }
            }
            Some(v) => {
                return Err(format!(
                    "SyntaxError: {keyword} requires an integer, got {v}"
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn execute_read_query(
    graph: &Graph,
    query: &Query,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    // Validate parameter-based SKIP/LIMIT at runtime before planning.
    if let Some(ref skip_expr) = query.skip {
        validate_skip_limit_param(skip_expr, "SKIP", params)?;
    }
    if let Some(ref limit_expr) = query.limit {
        validate_skip_limit_param(limit_expr, "LIMIT", params)?;
    }

    // 1. Compile query AST into an optimized physical plan
    let logical = LogicalPlanner::plan(query).map_err(|e| e.to_string())?;
    let physical = PhysicalPlanner::plan(&logical);
    let optimized = Optimizer::optimize(physical, Some(graph));

    // 2. Execute the optimized physical operator tree recursively.
    //    The top-level `PhysicalOperator::Project` in the plan has already
    //    materialized all projected values into the PathMap under their canonical
    //    column-name keys. Reading by key here avoids a second evaluation of the
    //    same expressions (double-projection) against a PathMap that no longer
    //    contains the pre-projection variable names.
    // Any query containing write clauses must hold the graph write lock for the
    // entire execution to prevent concurrent races (e.g., MERGE from two threads).
    // This holds whether or not the query also projects a RETURN clause: a chained
    // query such as `MATCH (a) SET a.x = 1 WITH a RETURN a` reaches this path with a
    // non-empty RETURN, and the per-part write executors inside `execute_physical`
    // assume the caller holds the lock (they call the `_internal` variants, which do
    // not re-acquire it).
    let has_write_parts = query.parts.iter().any(|p| {
        matches!(
            p,
            QueryPart::Create { .. }
                | QueryPart::Merge { .. }
                | QueryPart::Set { .. }
                | QueryPart::Delete { .. }
                | QueryPart::Remove { .. }
        )
    });

    let resolved_paths = if has_write_parts {
        graph.with_write_lock(|| execute_physical(graph, &optimized, params))?
    } else {
        execute_physical(graph, &optimized, params)?
    };

    // A query with an empty RETURN clause is a write-only pipeline query.
    if query.return_clause.items.is_empty() {
        return Ok(QueryResult {
            columns: vec![],
            records: vec![],
        });
    }

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
        query.return_clause.items.iter().map(column_name).collect()
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

/// Collect the variable names a path pattern binds (node, relationship, and path
/// variables) into `vars`.
fn collect_pattern_vars(
    pattern: &crate::ast::Pattern,
    vars: &mut std::collections::HashSet<String>,
) {
    if let Some(v) = &pattern.node.variable {
        vars.insert(v.clone());
    }
    if let Some(v) = &pattern.path_variable {
        vars.insert(v.clone());
    }
    for (rel, node) in &pattern.rels {
        if let Some(v) = &rel.variable {
            vars.insert(v.clone());
        }
        if let Some(v) = &node.variable {
            vars.insert(v.clone());
        }
    }
}

/// Resolve every `CALL` part in `query` against the procedure `registry`,
/// embedding the concrete output rows into each `QueryPart::Call` so the planner
/// can lower it without registry access.
///
/// A `CALL` is standalone when it is the query's sole part and there is no
/// explicit RETURN; otherwise it is treated as in-query, which forbids implicit
/// arguments to a procedure with inputs and `YIELD *`. For a standalone, non-void
/// call this also synthesizes a RETURN of the output variables so they surface as
/// result columns (a void standalone call keeps its empty RETURN and yields no
/// rows). Errors carry an openCypher error name and are surfaced as compile-time
/// `SyntaxError`s.
pub(super) fn resolve_call_parts(
    graph: &Graph,
    query: &mut crate::ast::Query,
    registry: &crate::procedure::ProcedureRegistry,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    use crate::ast::{Expr, QueryPart, ReturnItem};

    if !query
        .parts
        .iter()
        .any(|p| matches!(p, QueryPart::Call { .. }))
    {
        return Ok(());
    }

    let standalone = query.parts.len() == 1
        && matches!(query.parts[0], QueryPart::Call { .. })
        && query.return_clause.items.is_empty();

    let mut standalone_output_vars: Option<Vec<String>> = None;
    // Track the variables in scope before each part so a CALL can reject a YIELD
    // that shadows an already-bound variable. WITH resets the scope to its outputs.
    let mut scope: std::collections::HashSet<String> = std::collections::HashSet::new();

    for part in query.parts.iter_mut() {
        match part {
            QueryPart::Match { match_clauses, .. }
            | QueryPart::OptionalMatch { match_clauses, .. } => {
                for mc in match_clauses {
                    collect_pattern_vars(&mc.pattern, &mut scope);
                }
            }
            QueryPart::Unwind { variable, .. } => {
                scope.insert(variable.clone());
            }
            QueryPart::With { items, .. } => {
                let mut next: std::collections::HashSet<String> = std::collections::HashSet::new();
                for item in items {
                    if let Some(alias) = &item.alias {
                        next.insert(alias.clone());
                    } else if let crate::ast::Expr::FunctionCall { name, .. } = &item.expr {
                        if name == "__star__" {
                            next.extend(scope.iter().cloned());
                        }
                    } else if let crate::ast::Expr::Prop(v, p) = &item.expr {
                        if p.is_empty() {
                            next.insert(v.clone());
                        }
                    }
                }
                scope = next;
            }
            QueryPart::Call {
                name,
                args,
                implicit_args,
                yields,
                yield_star,
                resolved,
            } => {
                let mut arg_values = Vec::with_capacity(args.len());
                for arg in args.iter() {
                    arg_values.push(evaluate_expr(graph, &PathMap::new(), arg, params)?);
                }
                let rc = registry.resolve(
                    name,
                    &arg_values,
                    *implicit_args,
                    !standalone,
                    yields,
                    *yield_star,
                    &scope,
                    params,
                )?;
                for v in &rc.output_vars {
                    scope.insert(v.clone());
                }
                if standalone {
                    standalone_output_vars = Some(rc.output_vars.clone());
                }
                *resolved = Some(rc);
            }
            _ => {}
        }
    }

    if standalone {
        if let Some(vars) = standalone_output_vars {
            if !vars.is_empty() {
                query.return_clause.items = vars
                    .into_iter()
                    .map(|v| ReturnItem {
                        expr: Expr::Prop(v.clone(), String::new()),
                        alias: Some(v),
                        source_text: None,
                    })
                    .collect();
            }
        }
    }

    Ok(())
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

/// Compute the output column name for a RETURN/WITH item. An explicit alias wins;
/// otherwise openCypher uses the verbatim source text of the expression (preserving
/// case and whitespace), falling back to a reconstructed display name when the raw
/// source was not captured. This is distinct from `projected_key`, which is the
/// internal storage key and stays stable on the reconstructed display name.
pub(super) fn column_name(item: &crate::ast::ReturnItem) -> String {
    if let Some(a) = &item.alias {
        a.clone()
    } else if let Some(text) = &item.source_text {
        text.clone()
    } else {
        expr_display_name(&item.expr)
    }
}

/// Return a human-readable name for an expression, used as the default column name
/// when no alias is specified in a RETURN or WITH clause.
/// Render a path pattern as Cypher source for use as a default column name, for example
/// `(n)-[:T]->(b)`. Only the structural shape needed by pattern comprehension display is
/// reproduced (variable, labels, direction, and relationship type).
fn pattern_display_name(pattern: &crate::ast::Pattern) -> String {
    fn node(n: &crate::ast::NodePattern) -> String {
        let mut s = String::from("(");
        if let Some(v) = &n.variable {
            s.push_str(v);
        }
        for label in &n.labels {
            s.push(':');
            s.push_str(label);
        }
        s.push(')');
        s
    }
    let mut s = node(&pattern.node);
    for (rel, target) in &pattern.rels {
        let mut inner = String::new();
        if let Some(v) = &rel.variable {
            inner.push_str(v);
        }
        if let Some(t) = &rel.rel_type {
            inner.push(':');
            inner.push_str(t);
        }
        if rel.range.is_some() {
            inner.push('*');
        }
        let body = if inner.is_empty() {
            String::new()
        } else {
            format!("[{}]", inner)
        };
        if rel.is_undirected {
            s.push_str(&format!("-{}-", body));
        } else if rel.is_incoming {
            s.push_str(&format!("<-{}-", body));
        } else {
            s.push_str(&format!("-{}->", body));
        }
        s.push_str(&node(target));
    }
    s
}

pub(crate) fn expr_display_name(expr: &Expr) -> String {
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
        Expr::PatternComprehension {
            pattern,
            predicate,
            transform,
        } => {
            let mut s = String::from("[");
            if let Some(pv) = &pattern.path_variable {
                s.push_str(pv);
                s.push_str(" = ");
            }
            s.push_str(&pattern_display_name(pattern));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE {}", expr_display_name(p)));
            }
            s.push_str(&format!(" | {}]", expr_display_name(transform)));
            s
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list,
            expression,
        } => {
            format!(
                "reduce({} = {}, {} IN {} | {})",
                accumulator,
                expr_display_name(initial),
                variable,
                expr_display_name(list),
                expr_display_name(expression)
            )
        }
        Expr::HasLabel { variable, label } => format!("{}:{}", variable, label),
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
            // `__grouped__` is a transparent parenthesized-comparison marker; display
            // its single inner expression (redundant parentheses are dropped, matching
            // the reference column-name behavior).
            if name == "__grouped__" {
                if let Some(inner) = args.first() {
                    return expr_display_name(inner);
                }
            }
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

fn format_cypher_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(format_cypher_value).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(map) => {
            let mut items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_cypher_value(v)))
                .collect();
            items.sort();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn format_node_literal(graph: &Graph, nid: NodeId) -> String {
    let mut label_str = String::new();
    if let Ok(Some(record)) = graph.get_node(nid) {
        if let Ok(labels) = graph.node_labels(nid) {
            for label in labels {
                label_str.push(':');
                label_str.push_str(&label);
            }
        }
        if let Ok(props) = rmp_serde::from_slice::<serde_json::Value>(&record.props) {
            if let Some(map) = props.as_object() {
                if !map.is_empty() {
                    return format!("({} {})", label_str, format_cypher_value(&props));
                }
            }
        }
    }
    format!("({})", label_str)
}

fn format_edge_literal_string(graph: &Graph, eid: EdgeId) -> String {
    let mut type_str = String::new();
    if let Ok(Some(record)) = graph.get_edge(eid) {
        if let Ok(Some(etype)) = graph.type_name(record.edge_type) {
            type_str = format!(":{}", etype);
        }
        if let Ok(props) = rmp_serde::from_slice::<serde_json::Value>(&record.props) {
            if let Some(map) = props.as_object() {
                if !map.is_empty() {
                    return format!("{} {}", type_str, format_cypher_value(&props));
                }
            }
        }
    }
    type_str
}

pub(super) fn unpack_sentinels(graph: &Graph, val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(t)) = map.get("__type__") {
                if t == "__Node__" {
                    if let Some(id_val) = map.get("id").and_then(|i| i.as_i64()) {
                        let id = id_val as u64;
                        let formatted = format_node_literal(graph, id);
                        return serde_json::Value::String(formatted);
                    }
                } else if t == "__Edge__" {
                    if let Some(id_val) = map.get("id").and_then(|i| i.as_i64()) {
                        let id = id_val as u64;
                        let formatted = format_edge_literal_string(graph, id);
                        return serde_json::Value::Array(vec![serde_json::Value::String(
                            formatted,
                        )]);
                    }
                }
            }
            serde_json::Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, unpack_sentinels(graph, v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.into_iter()
                .map(|v| unpack_sentinels(graph, v))
                .collect(),
        ),
        other => other,
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
        Some(GraphBinding::Scalar(v)) => Ok(unpack_sentinels(graph, v.clone())),
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

/// Convert a `FilterExpr` to the `WhereClause` representation used by `evaluate_where`.
fn filter_expr_to_where_clause(expression: &FilterExpr) -> WhereClause {
    match expression {
        FilterExpr::Eq(l, r) => WhereClause::Eq(l.clone(), r.clone()),
        FilterExpr::Ne(l, r) => WhereClause::Ne(l.clone(), r.clone()),
        FilterExpr::Lt(l, r) => WhereClause::Lt(l.clone(), r.clone()),
        FilterExpr::Gt(l, r) => WhereClause::Gt(l.clone(), r.clone()),
        FilterExpr::Le(l, r) => WhereClause::Le(l.clone(), r.clone()),
        FilterExpr::Ge(l, r) => WhereClause::Ge(l.clone(), r.clone()),
        FilterExpr::HasLabel(_, _) => {
            unreachable!("HasLabel handled before filter_expr_to_where_clause")
        }
        FilterExpr::Expr(e) => WhereClause::Expr(e.clone()),
    }
}

/// Parameters extracted from a single-hop `Expand` operator for the factorized filter executor.
struct ExpandParams<'a> {
    input: &'a PhysicalOperator,
    src_var: &'a str,
    rel_var: &'a str,
    dst_var: &'a str,
    rel_type: Option<&'a str>,
    is_incoming: bool,
}

/// Optimized execution for `Filter { input: Expand(single-hop, directed), expression }`.
///
/// Evaluates the filter predicate once per source path when the predicate only
/// references variables that are bound BEFORE the expansion (the shared prefix).
/// Sources that fail are skipped with zero PathMap clones, avoiding the
/// O(avg_degree) clone cost that the default path incurs per rejected source.
///
/// When the predicate references `rel_var` or `dst_var` (the new expansion
/// bindings), falls back to per-row evaluation.
fn execute_filter_over_expand(
    graph: &Graph,
    expand: ExpandParams<'_>,
    expression: &FilterExpr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<PathMap>, String> {
    let ExpandParams {
        input: expand_input,
        src_var,
        rel_var,
        dst_var,
        rel_type,
        is_incoming,
    } = expand;
    // Determine whether the filter references the new hop-local bindings.
    let refs = filter_refs_in_expr(expression);
    let filter_touches_expansion = refs.contains(rel_var) || refs.contains(dst_var);

    let child_paths = execute_physical(graph, expand_input, params)?;
    if child_paths.is_empty() {
        return Ok(vec![]);
    }

    // Bulk-expand from all unique source nodes.
    let mut src_nodes: Vec<NodeId> = child_paths
        .iter()
        .filter_map(|p| match p.get(src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    src_nodes.sort_unstable();
    src_nodes.dedup();

    let transitions = expand_multi_type(graph, &src_nodes, rel_type, is_incoming)?;
    let mut transition_map: HashMap<NodeId, Vec<(EdgeId, NodeId)>> = HashMap::new();
    for (src, eid, dst) in transitions {
        transition_map.entry(src).or_default().push((eid, dst));
    }

    let mut next_paths = Vec::new();

    // HasLabel on a shared variable: bulk-filter sources with GraphBLAS, then expand survivors.
    if let FilterExpr::HasLabel(variable, label) = expression {
        if variable != rel_var && variable != dst_var {
            let mut active: Vec<NodeId> = child_paths
                .iter()
                .filter_map(|p| match p.get(variable.as_str()) {
                    Some(GraphBinding::Node(n)) => Some(*n),
                    _ => None,
                })
                .collect();
            active.sort_unstable();
            active.dedup();
            let filtered = graph
                .label_filter_and_graphblas(&active, label)
                .map_err(|e| e.to_string())?;
            let pass_set: HashSet<NodeId> = filtered.into_iter().collect();

            for path in &child_paths {
                if let Some(GraphBinding::Node(n)) = path.get(variable.as_str()) {
                    if !pass_set.contains(n) {
                        continue;
                    }
                }
                let src_node = match path.get(src_var) {
                    Some(GraphBinding::Node(n)) => *n,
                    _ => continue,
                };
                if let Some(dests) = transition_map.get(&src_node) {
                    for &(eid, dst_node) in dests {
                        let mut new_path = path.clone();
                        new_path.insert(rel_var.to_string(), GraphBinding::Edge(eid));
                        if new_path
                            .insert(dst_var.to_string(), GraphBinding::Node(dst_node))
                            .is_some_and(|e| e != GraphBinding::Node(dst_node))
                        {
                            continue;
                        }
                        next_paths.push(new_path);
                    }
                }
            }
            return Ok(next_paths);
        }
        // HasLabel on dst_var: fall through to per-row path below
    }

    if !filter_touches_expansion {
        // Factorization fast path: predicate is on shared variables only.
        // Evaluate once per source path; skip all destinations if the source fails.
        // This avoids O(avg_degree) PathMap clones per rejected source.
        let where_clause = filter_expr_to_where_clause(expression);
        for path in &child_paths {
            if !evaluate_where(graph, path, &where_clause, params)? {
                continue; // source fails — skip every destination for free
            }
            let src_node = match path.get(src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            if let Some(dests) = transition_map.get(&src_node) {
                // Build factorized groups: `Arc` around the shared prefix so the
                // PathMap bytes are owned once per source, not copied per destination.
                let group = FactorizedRecordGroup {
                    shared: std::sync::Arc::new(path.clone()),
                    extensions: dests
                        .iter()
                        .filter_map(|&(eid, dst_node)| {
                            // Guard closing-hop mismatches (normally handled by MultiwayJoin).
                            if let Some(existing) = path.get(dst_var) {
                                if *existing != GraphBinding::Node(dst_node) {
                                    return None;
                                }
                            }
                            Some((
                                rel_var.to_string(),
                                GraphBinding::Edge(eid),
                                dst_var.to_string(),
                                GraphBinding::Node(dst_node),
                            ))
                        })
                        .collect(),
                };
                next_paths.extend(group.flatten());
            }
        }
    } else {
        // Per-row path: filter references expansion variables; evaluate after each expand.
        let where_clause = filter_expr_to_where_clause(expression);
        for path in &child_paths {
            let src_node = match path.get(src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            if let Some(dests) = transition_map.get(&src_node) {
                for &(eid, dst_node) in dests {
                    let mut new_path = path.clone();
                    new_path.insert(rel_var.to_string(), GraphBinding::Edge(eid));
                    if new_path
                        .insert(dst_var.to_string(), GraphBinding::Node(dst_node))
                        .is_some_and(|e| e != GraphBinding::Node(dst_node))
                    {
                        continue;
                    }
                    if evaluate_where(graph, &new_path, &where_clause, params)? {
                        next_paths.push(new_path);
                    }
                }
            }
        }
    }

    Ok(next_paths)
}

fn get_node_representation(graph: &Graph, nid: NodeId) -> Result<serde_json::Value, String> {
    let record = graph
        .get_node(nid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node not found: {}", nid))?;
    let actual_json: serde_json::Value =
        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
    let mut node_obj = serde_json::Map::new();
    node_obj.insert(
        "__type__".to_string(),
        serde_json::Value::String("__Node__".to_string()),
    );
    node_obj.insert(
        "id".to_string(),
        serde_json::Value::Number((nid as i64).into()),
    );
    node_obj.insert("properties".to_string(), actual_json);
    Ok(serde_json::Value::Object(node_obj))
}

fn get_edge_representation(graph: &Graph, eid: EdgeId) -> Result<serde_json::Value, String> {
    let record = graph
        .get_edge(eid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("edge not found: {}", eid))?;
    let actual_json: serde_json::Value =
        rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
    let mut edge_obj = serde_json::Map::new();
    edge_obj.insert(
        "__type__".to_string(),
        serde_json::Value::String("__Edge__".to_string()),
    );
    edge_obj.insert(
        "id".to_string(),
        serde_json::Value::Number((eid as i64).into()),
    );
    edge_obj.insert(
        "startNode".to_string(),
        serde_json::Value::Number((record.src as i64).into()),
    );
    edge_obj.insert(
        "endNode".to_string(),
        serde_json::Value::Number((record.dst as i64).into()),
    );
    edge_obj.insert("properties".to_string(), actual_json);
    Ok(serde_json::Value::Object(edge_obj))
}

fn extend_or_create_path(
    graph: &Graph,
    existing_path: Option<&serde_json::Value>,
    src_node: NodeId,
    eid: EdgeId,
    dst_node: NodeId,
) -> Result<serde_json::Value, String> {
    let mut nodes = Vec::new();
    let mut relationships = Vec::new();

    if let Some(serde_json::Value::Object(m)) = existing_path {
        if m.get("__type__").and_then(|t| t.as_str()) == Some("__Path__") {
            if let Some(serde_json::Value::Array(n)) = m.get("nodes") {
                nodes.extend(n.clone());
            }
            if let Some(serde_json::Value::Array(r)) = m.get("relationships") {
                relationships.extend(r.clone());
            }
        }
    }

    if nodes.is_empty() {
        nodes.push(get_node_representation(graph, src_node)?);
    }
    relationships.push(get_edge_representation(graph, eid)?);
    nodes.push(get_node_representation(graph, dst_node)?);

    let mut m = serde_json::Map::new();
    m.insert(
        "__type__".to_string(),
        serde_json::Value::String("__Path__".to_string()),
    );
    m.insert("nodes".to_string(), serde_json::Value::Array(nodes));
    m.insert(
        "relationships".to_string(),
        serde_json::Value::Array(relationships),
    );
    Ok(serde_json::Value::Object(m))
}

/// Execute the body of an `Expand` operator given pre-computed child paths.
///
/// Extracted from `execute_physical` so it can be reused by `execute_with_sip`
/// without duplicating the BFS and single-hop expansion logic.
#[allow(clippy::too_many_arguments)]
fn expand_from_paths(
    graph: &Graph,
    child_paths: Vec<PathMap>,
    src_var: &str,
    rel_var: &str,
    dst_var: &str,
    rel_type: Option<&str>,
    is_incoming: bool,
    is_undirected: bool,
    min_hops: usize,
    max_hops: usize,
) -> Result<Vec<PathMap>, String> {
    let mut next_paths = Vec::new();

    let mut src_nodes: Vec<NodeId> = child_paths
        .iter()
        .filter_map(|p| match p.get(src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    src_nodes.sort_unstable();
    src_nodes.dedup();

    let directions: &[bool] = if is_undirected {
        &[false, true]
    } else {
        std::slice::from_ref(&is_incoming)
    };

    let mut transition_map: HashMap<NodeId, Vec<(EdgeId, NodeId)>> = HashMap::new();
    for &dir in directions {
        let transitions = expand_multi_type(graph, &src_nodes, rel_type, dir)?;
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

        if min_hops == 1 && max_hops == 1 {
            if let Some(dests) = transition_map.get(&src_node) {
                let shared = std::sync::Arc::new(path);
                for &(eid, dst_node) in dests {
                    if let Some(existing) = shared.get(dst_var) {
                        if *existing != GraphBinding::Node(dst_node) {
                            continue;
                        }
                    }
                    let mut new_path = (*shared).clone();
                    new_path.insert(rel_var.to_string(), GraphBinding::Edge(eid));
                    new_path.insert(dst_var.to_string(), GraphBinding::Node(dst_node));

                    // Build and insert Path object
                    let existing_path = match shared.get(&format!("_path_{}", src_var)) {
                        Some(GraphBinding::Scalar(v)) => Some(v),
                        _ => None,
                    };
                    if let Ok(path_obj) =
                        extend_or_create_path(graph, existing_path, src_node, eid, dst_node)
                    {
                        new_path
                            .insert(format!("_path_{}", dst_var), GraphBinding::Scalar(path_obj));
                    }
                    next_paths.push(new_path);
                }
            }
        } else {
            let src_rep = get_node_representation(graph, src_node)?;
            let mut visited = HashSet::new();
            visited.insert(src_node);
            let mut queue = vec![(src_node, vec![src_rep], visited)];
            let mut completed_paths: Vec<(NodeId, Vec<serde_json::Value>)> = Vec::new();
            let mut completed_targets = HashSet::new();

            if min_hops == 0 {
                completed_paths.push((src_node, vec![get_node_representation(graph, src_node)?]));
                completed_targets.insert(src_node);
            }

            for hop in 1..=max_hops {
                let mut next_queue = Vec::new();
                for (node, traversed, visited_nodes) in queue {
                    for &dir in directions {
                        let neighbors = expand_multi_type(graph, &[node], rel_type, dir)?;
                        for (_, eid, neigh_node) in neighbors {
                            if visited_nodes.contains(&neigh_node) {
                                continue;
                            }
                            let mut next_visited = visited_nodes.clone();
                            next_visited.insert(neigh_node);

                            let mut next_traversed = traversed.clone();
                            next_traversed.push(get_edge_representation(graph, eid)?);
                            next_traversed.push(get_node_representation(graph, neigh_node)?);

                            if hop >= min_hops && completed_targets.insert(neigh_node) {
                                completed_paths.push((neigh_node, next_traversed.clone()));
                            }
                            next_queue.push((neigh_node, next_traversed, next_visited));
                        }
                    }
                }
                queue = next_queue;
                if queue.is_empty() {
                    break;
                }
            }

            for (neigh_node, path_elements) in completed_paths {
                let mut new_path = path.clone();
                if new_path
                    .insert(dst_var.to_string(), GraphBinding::Node(neigh_node))
                    .is_some_and(|existing| existing != GraphBinding::Node(neigh_node))
                {
                    continue;
                }

                // Build Path object
                let mut nodes = Vec::new();
                let mut relationships = Vec::new();
                for (idx, item) in path_elements.into_iter().enumerate() {
                    if idx % 2 == 0 {
                        nodes.push(item);
                    } else {
                        relationships.push(item);
                    }
                }
                let mut m = serde_json::Map::new();
                m.insert(
                    "__type__".to_string(),
                    serde_json::Value::String("__Path__".to_string()),
                );
                m.insert("nodes".to_string(), serde_json::Value::Array(nodes));
                m.insert(
                    "relationships".to_string(),
                    serde_json::Value::Array(relationships),
                );
                let path_obj = serde_json::Value::Object(m);

                new_path.insert(format!("_path_{}", dst_var), GraphBinding::Scalar(path_obj));
                next_paths.push(new_path);
            }
        }
    }

    Ok(next_paths)
}

/// Evaluate a pattern comprehension (`[ p = (n)-->(b) WHERE pred | transform ]`).
///
/// The pattern is matched starting from the anchor node, which must already be bound
/// in `outer` (either as a node binding or as a `__Node__` scalar produced by, for
/// example, `nodes(p)`). Each match yields one element: the `transform` expression is
/// evaluated with the relationship and target-node variables introduced by the pattern
/// bound, plus the optional path variable bound to the matched path. Matches that fail
/// the optional `WHERE` predicate are dropped. An anchor that is not bound to a node
/// yields an empty list rather than an error.
pub(super) fn eval_pattern_comprehension(
    graph: &Graph,
    outer: &PathMap,
    pattern: &crate::ast::Pattern,
    predicate: Option<&Expr>,
    transform: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let anchor_var = match &pattern.node.variable {
        Some(v) => v.clone(),
        None => return Ok(serde_json::Value::Array(Vec::new())),
    };

    // Seed the expansion with the surrounding bindings, normalizing a `__Node__`
    // scalar anchor (e.g. an element of `nodes(p)`) to a node binding so the
    // expansion machinery can traverse from it. A non-node anchor yields no matches.
    let mut seed = outer.clone();
    match seed.get(&anchor_var) {
        Some(GraphBinding::Node(_)) => {}
        Some(GraphBinding::Scalar(v)) => {
            let id = v
                .as_object()
                .filter(|o| o.get("__type__").and_then(|t| t.as_str()) == Some("__Node__"))
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_i64());
            match id {
                Some(id) => {
                    seed.insert(anchor_var.clone(), GraphBinding::Node(id as u64));
                }
                None => return Ok(serde_json::Value::Array(Vec::new())),
            }
        }
        _ => return Ok(serde_json::Value::Array(Vec::new())),
    }

    let mut current_paths = vec![seed];
    let mut src_var = anchor_var.clone();
    let mut last_dst_var = anchor_var;
    let mut anon = 0usize;

    for (rel, node) in &pattern.rels {
        let rel_var = rel.variable.clone().unwrap_or_else(|| {
            anon += 1;
            format!("__pc_rel_{}", anon)
        });
        let dst_var = node.variable.clone().unwrap_or_else(|| {
            anon += 1;
            format!("__pc_node_{}", anon)
        });
        let min_hops = rel.range.as_ref().and_then(|r| r.min).unwrap_or(1) as usize;
        let max_hops = match rel.range.as_ref() {
            None => 1,
            Some(r) => r.max.map(|v| v as usize).unwrap_or(usize::MAX),
        };

        current_paths = expand_from_paths(
            graph,
            current_paths,
            &src_var,
            &rel_var,
            &dst_var,
            rel.rel_type.as_deref(),
            rel.is_incoming,
            rel.is_undirected,
            min_hops,
            max_hops,
        )?;
        current_paths = filter_paths_by_node(
            graph,
            current_paths,
            &dst_var,
            &node.labels,
            &node.properties,
            params,
        )?;

        src_var = dst_var.clone();
        last_dst_var = dst_var;
    }

    let mut result = Vec::new();
    for mut path in current_paths {
        if let Some(pv) = &pattern.path_variable {
            if let Some(GraphBinding::Scalar(obj)) = path.get(&format!("_path_{}", last_dst_var)) {
                let obj = obj.clone();
                path.insert(pv.clone(), GraphBinding::Scalar(obj));
            }
        }
        if let Some(pred) = predicate {
            if evaluate_expr(graph, &path, pred, params)? != serde_json::Value::Bool(true) {
                continue;
            }
        }
        result.push(evaluate_expr(graph, &path, transform, params)?);
    }
    Ok(serde_json::Value::Array(result))
}

/// Retain only the paths whose `dst_var` node carries every required label and matches
/// every inline property in `properties`.
fn filter_paths_by_node(
    graph: &Graph,
    paths: Vec<PathMap>,
    dst_var: &str,
    labels: &[String],
    properties: &Option<HashMap<String, Expr>>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<PathMap>, String> {
    if labels.is_empty() && properties.is_none() {
        return Ok(paths);
    }
    let mut out = Vec::new();
    for path in paths {
        let nid = match path.get(dst_var) {
            Some(GraphBinding::Node(n)) => *n,
            _ => continue,
        };
        if !labels.is_empty() {
            let node_labels = graph.node_labels(nid).map_err(|e| e.to_string())?;
            if !labels.iter().all(|l| node_labels.iter().any(|x| x == l)) {
                continue;
            }
        }
        if let Some(props) = properties {
            let record = graph
                .get_node(nid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node not found: {}", nid))?;
            let actual: serde_json::Value =
                rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
            let mut keep = true;
            for (key, expr) in props {
                let want = evaluate_expr(graph, &path, expr, params)?;
                let got = actual.get(key).cloned().unwrap_or(serde_json::Value::Null);
                if got != want {
                    keep = false;
                    break;
                }
            }
            if !keep {
                continue;
            }
        }
        out.push(path);
    }
    Ok(out)
}

/// Execute a physical operator tree with a Sideways Information Passing (SIP) filter.
///
/// `sip` maps variable names to the set of `NodeId`s that the build side of a
/// `HashJoin` produced for that variable.  Any `LabelScan` whose variable appears
/// in `sip` is restricted to the intersection of the label's nodes and the allowed
/// set, avoiding a full-table scan when the build side is selective.
///
/// The filter threads down through `Expand` and `Filter` operators (the two
/// structural wrappers that appear between a `HashJoin` and its inner `LabelScan`)
/// and delegates all other operators to `execute_physical` unchanged.
fn execute_with_sip(
    graph: &Graph,
    op: &PhysicalOperator,
    params: &HashMap<String, serde_json::Value>,
    sip: &HashMap<String, HashSet<NodeId>>,
) -> Result<Vec<PathMap>, String> {
    match op {
        PhysicalOperator::LabelScan { variable, label } => {
            if let Some(allowed) = sip.get(variable) {
                // Intersect the label scan with the SIP-allowed node IDs.
                let candidates: Vec<NodeId> = if let Some(lbl) = label {
                    let all = graph.nodes_by_label(lbl).map_err(|e| e.to_string())?;
                    all.into_iter().filter(|id| allowed.contains(id)).collect()
                } else {
                    let mut ids: Vec<NodeId> = allowed.iter().copied().collect();
                    ids.sort_unstable();
                    ids
                };
                Ok(candidates
                    .into_iter()
                    .map(|id| {
                        let mut path = PathMap::new();
                        path.insert(variable.clone(), GraphBinding::Node(id));
                        path
                    })
                    .collect())
            } else {
                execute_physical(graph, op, params)
            }
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
            // Thread SIP into the input so the inner LabelScan is restricted.
            let child_paths = execute_with_sip(graph, input, params, sip)?;
            expand_from_paths(
                graph,
                child_paths,
                src_var,
                rel_var,
                dst_var,
                rel_type.as_deref(),
                *is_incoming,
                *is_undirected,
                *min_hops,
                *max_hops,
            )
        }
        PhysicalOperator::Filter { input, expression } => {
            // Thread SIP into the input, then apply the predicate to the
            // reduced result set.  Skip the factorized fast-path used by the
            // normal executor — SIP has already shrunk the input, so the
            // per-row cost is low.
            let child_paths = execute_with_sip(graph, input, params, sip)?;
            let mut next_paths = Vec::new();
            if let FilterExpr::HasLabel(variable, label) = expression {
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
                let where_clause = filter_expr_to_where_clause(expression);
                for path in child_paths {
                    if evaluate_where(graph, &path, &where_clause, params)? {
                        next_paths.push(path);
                    }
                }
            }
            Ok(next_paths)
        }
        // For all other operators (NodeIndexScan, MultiwayJoin, etc.) SIP cannot be
        // applied structurally; delegate to the standard executor unchanged.
        _ => execute_physical(graph, op, params),
    }
}

/// One hop of a fused linear expand chain. `src_var` is the node the hop starts
/// from (bound by the base or an earlier hop), and the hop binds `rel_var` to the
/// traversed edge and `dst_var` to the reached node.
struct ChainHop<'a> {
    src_var: &'a str,
    rel_var: &'a str,
    dst_var: &'a str,
    rel_type: Option<&'a str>,
    is_incoming: bool,
}

/// Execute a maximal linear chain of single-hop directed Expands as one fused
/// operation. Each hop level is bulk-expanded once (distinct sources only), then
/// output rows are produced by threading every base path through all hops,
/// cloning a base path exactly once per emitted row regardless of chain length.
/// This generalizes the former two-hop fast path to N hops and preserves path
/// multiplicity and all `(rel, dst)` bindings.
fn execute_expand_chain_n(
    graph: &Graph,
    base_paths: Vec<PathMap>,
    hops: &[ChainHop<'_>],
) -> Result<Vec<PathMap>, String> {
    if base_paths.is_empty() || hops.is_empty() {
        return Ok(vec![]);
    }

    // Bulk-expand each hop level. The chain is linear, so level i's source set is
    // the set of nodes reached at level i-1; level 0's sources come from the base.
    let mut level_maps: Vec<HashMap<NodeId, Vec<(EdgeId, NodeId)>>> =
        Vec::with_capacity(hops.len());
    let mut frontier: Vec<NodeId> = base_paths
        .iter()
        .filter_map(|p| match p.get(hops[0].src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    frontier.sort_unstable();
    frontier.dedup();

    for hop in hops {
        let expanded = expand_multi_type(graph, &frontier, hop.rel_type, hop.is_incoming)?;
        let mut map: HashMap<NodeId, Vec<(EdgeId, NodeId)>> = HashMap::new();
        for (src, eid, dst) in expanded {
            map.entry(src).or_default().push((eid, dst));
        }
        // Next level's sources are this level's distinct destinations.
        let mut next: Vec<NodeId> = map
            .values()
            .flat_map(|v| v.iter().map(|(_, d)| *d))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        next.sort_unstable();
        frontier = next;
        level_maps.push(map);
    }

    // Recursively thread each base path through all hops, cloning once per leaf.
    let mut out = Vec::new();
    let mut stack: Vec<(EdgeId, NodeId)> = Vec::with_capacity(hops.len());
    for base_path in &base_paths {
        let Some(GraphBinding::Node(start)) = base_path.get(hops[0].src_var) else {
            continue;
        };
        thread_chain(
            base_path,
            *start,
            hops,
            &level_maps,
            0,
            &mut stack,
            &mut out,
        );
    }
    Ok(out)
}

/// Depth-first expansion of one base path through the remaining hops. `src` is the
/// node the current hop expands from. Edge and node bindings accumulate in `stack`
/// and are materialized into a single cloned PathMap at the leaf.
#[allow(clippy::too_many_arguments)]
fn thread_chain(
    base_path: &PathMap,
    src: NodeId,
    hops: &[ChainHop<'_>],
    level_maps: &[HashMap<NodeId, Vec<(EdgeId, NodeId)>>],
    hop_idx: usize,
    stack: &mut Vec<(EdgeId, NodeId)>,
    out: &mut Vec<PathMap>,
) {
    if hop_idx == hops.len() {
        let mut new_path = base_path.clone();
        for (i, &(eid, dst)) in stack.iter().enumerate() {
            new_path.insert(hops[i].rel_var.to_string(), GraphBinding::Edge(eid));
            new_path.insert(hops[i].dst_var.to_string(), GraphBinding::Node(dst));
        }
        out.push(new_path);
        return;
    }
    let Some(dests) = level_maps[hop_idx].get(&src) else {
        return;
    };
    let hop = &hops[hop_idx];
    for &(eid, dst) in dests {
        // Closing-hop guard: if dst_var is already pinned (by the base path or an
        // earlier hop in this chain), only keep matching destinations.
        if let Some(existing) = base_path.get(hop.dst_var) {
            if *existing != GraphBinding::Node(dst) {
                continue;
            }
        }
        stack.push((eid, dst));
        thread_chain(base_path, dst, hops, level_maps, hop_idx + 1, stack, out);
        stack.pop();
    }
}

fn json_vals_are_equal(v1: &serde_json::Value, v2: &serde_json::Value) -> bool {
    match (v1, v2) {
        (serde_json::Value::Number(n1), serde_json::Value::Number(n2)) => {
            if let (Some(i1), Some(i2)) = (n1.as_i64(), n2.as_i64()) {
                i1 == i2
            } else {
                n1.as_f64() == n2.as_f64()
            }
        }
        _ => v1 == v2,
    }
}

fn json_val_cmp(l: &serde_json::Value, r: &serde_json::Value) -> Option<std::cmp::Ordering> {
    match (l, r) {
        (serde_json::Value::Number(n1), serde_json::Value::Number(n2)) => {
            if let (Some(i1), Some(i2)) = (n1.as_i64(), n2.as_i64()) {
                Some(i1.cmp(&i2))
            } else {
                n1.as_f64().partial_cmp(&n2.as_f64())
            }
        }
        _ => json_cmp(l, r),
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

            let mut filtered = Vec::new();
            for cand in candidates {
                if let Ok(Some(record)) = graph.get_node(cand) {
                    if let Ok(props) = rmp_serde::from_slice::<serde_json::Value>(&record.props) {
                        if let Some(actual_val) = props.get(property) {
                            if json_vals_are_equal(actual_val, &val) {
                                let mut path = PathMap::new();
                                path.insert(variable.clone(), GraphBinding::Node(cand));
                                filtered.push(path);
                            }
                        }
                    }
                }
            }
            Ok(filtered)
        }
        PhysicalOperator::NodeRangeScan {
            variable,
            label,
            property,
            lo,
            lo_inclusive,
            hi,
            hi_inclusive,
        } => {
            let lo_val = lo
                .as_ref()
                .map(|e| evaluate_expr(graph, &PathMap::new(), e, params))
                .transpose()?;
            let hi_val = hi
                .as_ref()
                .map(|e| evaluate_expr(graph, &PathMap::new(), e, params))
                .transpose()?;

            let lo_prop = lo_val.as_ref().and_then(json_to_prop_value);
            let hi_prop = hi_val.as_ref().and_then(json_to_prop_value);

            let candidates = graph
                .nodes_by_property_range(
                    label,
                    property,
                    lo_prop,
                    *lo_inclusive,
                    hi_prop,
                    *hi_inclusive,
                )
                .map_err(|e| e.to_string())?;

            let mut filtered = Vec::new();
            for cand in candidates {
                if let Ok(Some(record)) = graph.get_node(cand) {
                    if let Ok(props) = rmp_serde::from_slice::<serde_json::Value>(&record.props) {
                        if let Some(actual_val) = props.get(property) {
                            let mut ok = true;
                            if let Some(ref l_val) = lo_val {
                                match json_val_cmp(actual_val, l_val) {
                                    Some(std::cmp::Ordering::Greater) => {}
                                    Some(std::cmp::Ordering::Equal) if *lo_inclusive => {}
                                    _ => ok = false,
                                }
                            }
                            if ok {
                                if let Some(ref h_val) = hi_val {
                                    match json_val_cmp(actual_val, h_val) {
                                        Some(std::cmp::Ordering::Less) => {}
                                        Some(std::cmp::Ordering::Equal) if *hi_inclusive => {}
                                        _ => ok = false,
                                    }
                                }
                            }
                            if ok {
                                let mut path = PathMap::new();
                                path.insert(variable.clone(), GraphBinding::Node(cand));
                                filtered.push(path);
                            }
                        }
                    }
                }
            }
            Ok(filtered)
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
        PhysicalOperator::NodeByIdSeek {
            variable,
            label,
            id_value,
        } => {
            // Resolve the constant id, then fetch the single node directly. A
            // non-integer, negative, or missing id yields no rows.
            let id_json = evaluate_expr(graph, &PathMap::new(), id_value, params)?;
            let nid = match id_json.as_u64() {
                Some(n) => n as NodeId,
                None => return Ok(vec![]),
            };
            // Fetch the node and resolve its label in a single read transaction,
            // enforcing the label predicate the seek replaced.
            let matched = graph
                .view(|txn| {
                    let Some(record) = txn.get_node(nid)? else {
                        return Ok(false);
                    };
                    match label {
                        Some(lbl) => {
                            // A node matches the predicate if any of its labels is `lbl`.
                            let mut found = false;
                            for lid in &record.labels {
                                if txn.label_name(*lid)?.as_deref() == Some(lbl.as_str()) {
                                    found = true;
                                    break;
                                }
                            }
                            Ok(found)
                        }
                        None => Ok(true),
                    }
                })
                .map_err(|e| e.to_string())?;
            if matched {
                let mut path = PathMap::new();
                path.insert(variable.clone(), GraphBinding::Node(nid));
                Ok(vec![path])
            } else {
                Ok(vec![])
            }
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
            // Fused-chain fast path: collect a maximal linear chain of single-hop
            // directed Expands and run all hops together, cloning the base path
            // once per output row instead of materializing one PathMap per hop
            // level. Falls through to the normal path for single hops, variable
            // length, undirected, or non-contiguous chains (e.g. a Filter between
            // hops, which the planner inserts for a labeled intermediate node).
            if *min_hops == 1 && *max_hops == 1 && !is_undirected {
                let mut hops = vec![ChainHop {
                    src_var,
                    rel_var,
                    dst_var,
                    rel_type: rel_type.as_deref(),
                    is_incoming: *is_incoming,
                }];
                // Source variable of the deepest hop collected so far; the next
                // inner hop must reach it for the chain to stay linear.
                let mut bottom_src = src_var.as_str();
                let mut base = input.as_ref();
                // Walk down through linearly connected single-hop directed Expands.
                while let PhysicalOperator::Expand {
                    input: inner_input,
                    src_var: inner_src_var,
                    rel_var: inner_rel_var,
                    dst_var: inner_dst_var,
                    rel_type: inner_rel_type,
                    is_incoming: inner_is_incoming,
                    is_undirected: false,
                    min_hops: 1,
                    max_hops: 1,
                } = base
                {
                    // Linear only: this hop's source must be the inner hop's target.
                    if bottom_src != inner_dst_var {
                        break;
                    }
                    hops.push(ChainHop {
                        src_var: inner_src_var,
                        rel_var: inner_rel_var,
                        dst_var: inner_dst_var,
                        rel_type: inner_rel_type.as_deref(),
                        is_incoming: *inner_is_incoming,
                    });
                    bottom_src = inner_src_var.as_str();
                    base = inner_input.as_ref();
                }
                if hops.len() >= 2 {
                    // hops were collected top-to-bottom; execute bottom-to-top.
                    hops.reverse();
                    let base_paths = execute_physical(graph, base, params)?;
                    return execute_expand_chain_n(graph, base_paths, &hops);
                }
            }
            let child_paths = execute_physical(graph, input, params)?;
            expand_from_paths(
                graph,
                child_paths,
                src_var,
                rel_var,
                dst_var,
                rel_type.as_deref(),
                *is_incoming,
                *is_undirected,
                *min_hops,
                *max_hops,
            )
        }
        PhysicalOperator::Filter { input, expression } => {
            // Factorization fast-path: when the child is a single-hop directed Expand
            // and the filter touches only pre-expansion (shared-prefix) variables, apply
            // the predicate once per source path rather than once per (src, dst) row.
            // Sources that fail the predicate skip all their destinations with zero clones.
            if let PhysicalOperator::Expand {
                input: expand_input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
            } = input.as_ref()
            {
                // HasLabel is handled by the bulk GraphBLAS path below; only route
                // non-HasLabel predicates through the factorized executor.
                if *min_hops == 1
                    && *max_hops == 1
                    && !is_undirected
                    && !matches!(expression, FilterExpr::HasLabel(..))
                {
                    return execute_filter_over_expand(
                        graph,
                        ExpandParams {
                            input: expand_input,
                            src_var,
                            rel_var,
                            dst_var,
                            rel_type: rel_type.as_deref(),
                            is_incoming: *is_incoming,
                        },
                        expression,
                        params,
                    );
                }
            }

            // Default path: materialize child, then filter row by row.
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
                let where_clause = filter_expr_to_where_clause(expression);
                for path in child_paths {
                    if evaluate_where(graph, &path, &where_clause, params)? {
                        next_paths.push(path);
                    }
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::HashJoin { left, right } => {
            // Determine common variables statically from plan trees before running either side.
            // Detect OptionalMatch on either side (the optimizer may place it on left or right
            // depending on cardinality estimates).  Whichever side holds the OptionalMatch is
            // the "optional" side; the other is the "required" side.  We always probe with the
            // required side so that every required row is emitted even when the optional pattern
            // finds no match for that row (left-outer-join semantics).
            let (required_op, optional_inner, opt_null_vars): (
                &PhysicalOperator,
                &PhysicalOperator,
                Option<&[String]>,
            ) = if let PhysicalOperator::OptionalMatch { input, null_vars } = left.as_ref() {
                // OptionalMatch ended up on the left — swap roles.
                (right.as_ref(), input.as_ref(), Some(null_vars))
            } else if let PhysicalOperator::OptionalMatch { input, null_vars } = right.as_ref() {
                (left.as_ref(), input.as_ref(), Some(null_vars))
            } else {
                // No OptionalMatch: standard inner join.
                (left.as_ref(), right.as_ref(), None)
            };

            // Compute common join variables from the actual plans (not the OptionalMatch
            // wrapper, whose null_vars would inflate bound_vars).
            let required_bound = Optimizer::bound_vars(required_op);
            let optional_bound = Optimizer::bound_vars(optional_inner);
            let common_vars: Vec<String> = required_bound
                .intersection(&optional_bound)
                .cloned()
                .collect();

            let mut next_paths = Vec::new();

            if common_vars.is_empty() {
                // No shared variables: Cartesian product, or outer-product for optional.
                let required_paths = execute_physical(graph, required_op, params)?;
                let opt_paths = execute_physical(graph, optional_inner, params)?;
                if opt_paths.is_empty() {
                    if let Some(null_vars) = opt_null_vars {
                        for rp in required_paths {
                            let mut merged = rp;
                            for v in null_vars {
                                if !merged.contains_key(v.as_str()) {
                                    merged.insert(
                                        v.clone(),
                                        GraphBinding::Scalar(serde_json::Value::Null),
                                    );
                                }
                            }
                            next_paths.push(merged);
                        }
                    }
                } else {
                    for rp in &required_paths {
                        for op in &opt_paths {
                            let mut merged = rp.clone();
                            merged.extend(op.iter().map(|(k, v)| (k.clone(), v.clone())));
                            next_paths.push(merged);
                        }
                    }
                }
            } else {
                // Equi-join on shared variables.
                //
                // Strategy: build hash table from the optional inner plan, probe with the
                // required plan.  SIP can restrict the required-side LabelScans when the
                // optional inner is selective.
                let opt_paths = execute_physical(graph, optional_inner, params)?;

                let sip: HashMap<String, HashSet<NodeId>> = common_vars
                    .iter()
                    .filter_map(|var| {
                        let ids: HashSet<NodeId> = opt_paths
                            .iter()
                            .filter_map(|p| match p.get(var) {
                                Some(GraphBinding::Node(n)) => Some(*n),
                                _ => None,
                            })
                            .collect();
                        if ids.is_empty() {
                            None
                        } else {
                            Some((var.clone(), ids))
                        }
                    })
                    .collect();

                // SIP is only applied for inner joins; for outer joins it would suppress
                // required rows that have no optional match, which is incorrect.
                let required_paths = if sip.is_empty() || opt_null_vars.is_some() {
                    execute_physical(graph, required_op, params)?
                } else {
                    execute_with_sip(graph, required_op, params, &sip)?
                };

                // Build hash table from the optional inner side.
                let mut hash_table: HashMap<Vec<GraphBinding>, Vec<PathMap>> = HashMap::new();
                for op in opt_paths {
                    let key: Option<Vec<GraphBinding>> =
                        common_vars.iter().map(|v| op.get(v).cloned()).collect();
                    if let Some(key) = key {
                        hash_table.entry(key).or_default().push(op);
                    }
                }

                // Probe with required rows.  Unmatched required rows get null-filled optional
                // vars when this is an outer join.
                for rp in required_paths {
                    let key: Option<Vec<GraphBinding>> =
                        common_vars.iter().map(|v| rp.get(v).cloned()).collect();
                    if let Some(key) = key {
                        if let Some(matches) = hash_table.get(&key) {
                            for op in matches {
                                let mut merged = rp.clone();
                                merged.extend(op.iter().map(|(k, v)| (k.clone(), v.clone())));
                                next_paths.push(merged);
                            }
                        } else if let Some(null_vars) = opt_null_vars {
                            // Outer join: no optional match for this required row → null-fill.
                            let mut merged = rp;
                            for v in null_vars {
                                if !merged.contains_key(v.as_str()) {
                                    merged.insert(
                                        v.clone(),
                                        GraphBinding::Scalar(serde_json::Value::Null),
                                    );
                                }
                            }
                            next_paths.push(merged);
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
        PhysicalOperator::ProcedureCall {
            input,
            output_vars,
            rows,
        } => {
            let child_paths = execute_physical(graph, input, params)?;
            let mut next_paths = Vec::new();

            // For each input row, emit one output row per resolved procedure row,
            // binding the YIELD output variables. A void procedure has empty
            // `output_vars` and a single empty row, so this passes rows through.
            for path in child_paths {
                for row in rows {
                    let mut new_path = path.clone();
                    for (var, value) in output_vars.iter().zip(row.iter()) {
                        new_path.insert(var.clone(), GraphBinding::Scalar(value.clone()));
                    }
                    next_paths.push(new_path);
                }
            }

            Ok(next_paths)
        }
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
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

                // For non-barrier projects (intermediate projections in WITH-WHERE
                // pipelines), start with all existing bindings so that the filter
                // after this project can still see pre-projection variables.
                // Barrier projects (WITH clause boundaries) always start fresh.
                let mut projected_path: PathMap = if *is_barrier {
                    PathMap::new()
                } else {
                    path.clone()
                };

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
                /// True once a non-integer value has been summed. `sum()` over only
                /// integers returns an integer, matching openCypher numeric typing.
                sum_is_float: bool,
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
                        sum_is_float: false,
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
            if group_by.is_empty() {
                let states = aggregations.iter().map(|_| AggState::new()).collect();
                groups.insert("".to_string(), (PathMap::new(), states));
            }

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
                                    if val.as_i64().is_none() {
                                        state.sum_is_float = true;
                                    }
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
                        AggFn::Sum { .. } => {
                            // Preserve integer typing: a sum over only integer inputs
                            // is an integer, as in openCypher.
                            if !state.sum_is_float
                                && state.sum.fract() == 0.0
                                && state.sum.abs() < i64::MAX as f64
                            {
                                serde_json::Value::Number((state.sum as i64).into())
                            } else {
                                serde_json::Number::from_f64(state.sum)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
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
                            let percentile =
                                evaluate_expr(graph, &PathMap::new(), percentile, params)?
                                    .as_f64()
                                    .ok_or("percentileDisc(): percentile must be a number")?;
                            if !(0.0..=1.0).contains(&percentile) {
                                return Err("ArgumentError(NumberOutOfRange): percentile must be in [0.0, 1.0]".to_string());
                            }
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
                            let percentile =
                                evaluate_expr(graph, &PathMap::new(), percentile, params)?
                                    .as_f64()
                                    .ok_or("percentileCont(): percentile must be a number")?;
                            if !(0.0..=1.0).contains(&percentile) {
                                return Err("ArgumentError(NumberOutOfRange): percentile must be in [0.0, 1.0]".to_string());
                            }
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
                    let ord =
                        json_cmp(&ka[i], &kb[i]).unwrap_or_else(|| json_cmp_total(&ka[i], &kb[i]));
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
        PhysicalOperator::WritePart { input, part } => {
            use super::write::execute_create_internal_with_context;
            use super::write::execute_merge_internal_with_context;
            use crate::ast::QueryPart;

            let child_paths = execute_physical(graph, input, params)?;

            // DELETE is evaluated over the whole result at once: all listed
            // relationships are removed before any node, so an undirected expand
            // that binds the same edge in more than one row still succeeds. The
            // rows pass through unchanged for a following RETURN.
            if let QueryPart::Delete { targets, detach } = part {
                use super::write::delete_over_paths;
                delete_over_paths(graph, &child_paths, targets, *detach, params)?;
                return Ok(child_paths);
            }

            let mut result_paths = Vec::new();

            for path in child_paths {
                match part {
                    QueryPart::Create { patterns } => {
                        let mut new_path = path.clone();
                        for pattern in patterns {
                            let created = execute_create_internal_with_context(
                                graph, pattern, &path, params,
                            )?;
                            new_path.extend(created);
                        }
                        result_paths.push(new_path);
                    }
                    QueryPart::Merge { merges } => {
                        // Each MERGE in the clause extends every current row, and
                        // may fan out to multiple rows when it matches more than one
                        // existing pattern.
                        let mut current = vec![path.clone()];
                        for merge_stmt in merges {
                            let mut next = Vec::new();
                            for p in &current {
                                let extensions = execute_merge_internal_with_context(
                                    graph, merge_stmt, p, params,
                                )?;
                                for ext in extensions {
                                    let mut row = p.clone();
                                    row.extend(ext);
                                    next.push(row);
                                }
                            }
                            current = next;
                        }
                        result_paths.extend(current);
                    }
                    QueryPart::Set { items } => {
                        // Apply SET for each row — supports node/edge properties and node labels.
                        use super::write::apply_set_items;
                        apply_set_items(graph, &path, items, params)?;
                        result_paths.push(path);
                    }
                    QueryPart::Delete { targets, detach } => {
                        use super::write::apply_delete_targets;
                        apply_delete_targets(graph, &path, targets, *detach, params)?;
                        result_paths.push(path);
                    }
                    QueryPart::Remove { items } => {
                        use super::write::apply_remove_item;
                        for item in items {
                            apply_remove_item(graph, item, &path)?;
                        }
                        result_paths.push(path);
                    }
                    _ => {
                        // Other QueryPart variants are not write clauses; pass through.
                        result_paths.push(path);
                    }
                }
            }

            Ok(result_paths)
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
        PhysicalOperator::MultiwayJoin {
            input,
            closing_src_var,
            closing_dst_var,
            closing_rel_type,
            closing_rel_var,
            closing_is_incoming,
        } => {
            let child_paths = execute_physical(graph, input, params)?;
            if child_paths.is_empty() {
                return Ok(vec![]);
            }

            // Collect unique closing-src node IDs for a single bulk expansion.
            // Paying O(sum of degrees of unique sources) once is far cheaper than
            // iterating all neighbors for every input row.
            let mut src_nodes: Vec<NodeId> = child_paths
                .iter()
                .filter_map(|p| match p.get(closing_src_var) {
                    Some(GraphBinding::Node(n)) => Some(*n),
                    _ => None,
                })
                .collect();
            src_nodes.sort_unstable();
            src_nodes.dedup();

            let transitions = expand_multi_type(
                graph,
                &src_nodes,
                closing_rel_type.as_deref(),
                *closing_is_incoming,
            )?;

            // Index the transitions as (closing_src, closing_dst) → EdgeId for O(1) lookup.
            let mut join_map: HashMap<NodeId, HashMap<NodeId, EdgeId>> = HashMap::new();
            for (src, eid, dst) in transitions {
                join_map.entry(src).or_default().insert(dst, eid);
            }

            let mut next_paths = Vec::new();
            for path in child_paths {
                let closing_src = match path.get(closing_src_var) {
                    Some(GraphBinding::Node(n)) => *n,
                    _ => continue,
                };
                let closing_dst = match path.get(closing_dst_var) {
                    Some(GraphBinding::Node(n)) => *n,
                    _ => continue,
                };

                if let Some(dst_map) = join_map.get(&closing_src) {
                    if let Some(&eid) = dst_map.get(&closing_dst) {
                        let mut new_path = path.clone();
                        new_path.insert(closing_rel_var.clone(), GraphBinding::Edge(eid));
                        next_paths.push(new_path);
                    }
                }
            }

            Ok(next_paths)
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
            Ok(cypher_eq(&lv, &rv) == serde_json::Value::Bool(true))
        }
        WhereClause::Ne(l, r) => {
            let lv = evaluate_expr(graph, path, l, params)?;
            let rv = evaluate_expr(graph, path, r, params)?;
            Ok(cypher_eq(&lv, &rv) == serde_json::Value::Bool(false))
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
/// Cross-type order according to openCypher: Map < List < String < Boolean < Number < Null.
pub(super) fn json_cmp_total(l: &serde_json::Value, r: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    fn type_rank(v: &Value) -> u8 {
        if crate::exec::expr::is_nan(v) {
            return 4;
        }
        match v {
            Value::Object(_) => 0,
            Value::Array(_) => 1,
            Value::String(_) => 2,
            Value::Bool(_) => 3,
            Value::Number(_) => 4,
            Value::Null => 5,
        }
    }
    let is_l_nan = crate::exec::expr::is_nan(l);
    let is_r_nan = crate::exec::expr::is_nan(r);
    match (is_l_nan, is_r_nan) {
        (true, true) => return std::cmp::Ordering::Equal,
        (true, false) => {
            if type_rank(r) == 4 {
                return std::cmp::Ordering::Greater;
            }
        }
        (false, true) => {
            if type_rank(l) == 4 {
                return std::cmp::Ordering::Less;
            }
        }
        (false, false) => {}
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
        (Value::Array(a), Value::Array(b)) => {
            let min_len = a.len().min(b.len());
            for i in 0..min_len {
                let cmp = json_cmp_total(&a[i], &b[i]);
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            a.len().cmp(&b.len())
        }
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
