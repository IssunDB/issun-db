use super::expr::*;
use super::factorize::{FactorizedRecordGroup, filter_refs_in_expr};
use super::row::{Bindings, SlotRow, SlotSchema};
use super::*;
use issundb_vector::VectorGraphExt;

/// Expand from a set of source nodes, handling pipe-separated OR relationship types.
///
/// If `rel_type` is `Some("A|B|C")`, this expands separately for each type and
/// merges the results, deduplicating by `(EdgeId, NodeId)` pair.
///
/// Every returned triple references existing node and edge records, so no
/// per-transition validation is needed here. `expand_spmv_graphblas` sources
/// transitions either from the CSR snapshot, whose build only admits an edge
/// when both endpoints exist in the node store (see `CsrSnapshot::build`), or
/// from committed `out_adj`/`in_adj`, which `delete_node` and `delete_edge`
/// keep transactionally consistent with the node and edge stores. There is no
/// path through the public API that leaves a dangling adjacency entry.
pub(super) fn expand_multi_type(
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

    // One slot schema per query, walked from the optimized plan: every row of
    // this execution (join build sides included) binds against these slots.
    let schema = std::sync::Arc::new(SlotSchema::from_plan(&optimized));

    // 2. Execute the optimized physical operator tree recursively.
    //    The top-level `PhysicalOperator::Project` in the plan has already
    //    materialized all projected values into the row under their canonical
    //    column-name keys. Reading by key here avoids a second evaluation of the
    //    same expressions (double-projection) against a row that no longer
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

    // Read-only queries install a statement-scoped property cache so repeated
    // property access on the same node or edge decodes once. Queries with write
    // parts mutate records mid-execution, so they leave the cache uninstalled to
    // avoid serving a stale decode; the guard drops at the end of this function,
    // covering both expansion and the projection below.
    let _prop_cache = (!has_write_parts).then(expr::PropCache::install);

    // Columnar fast path: a recognized final projection or aggregation over a
    // single-hop expansion executes column-at-a-time and produces the result
    // records directly. Any other shape (and every write query) takes the row
    // pipeline below.
    if !has_write_parts && !query.return_clause.items.is_empty() {
        if let Some(mut records) = super::vectorized::try_execute_vectorized(
            graph,
            &optimized,
            &query.return_clause,
            params,
            &schema,
        )? {
            let columns: Vec<String> = query.return_clause.items.iter().map(column_name).collect();
            if query.return_clause.distinct {
                dedup_records(&mut records);
            }
            return Ok(QueryResult { columns, records });
        }
    }

    let resolved_paths = if has_write_parts {
        graph.with_write_lock(|| execute_physical(graph, &optimized, params, &schema))?
    } else {
        execute_physical(graph, &optimized, params, &schema)?
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
            .map(|p| p.bound_entries().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_else(|| {
                let mut scope = std::collections::HashSet::new();
                for part in &query.parts {
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
                        QueryPart::Create { patterns } => {
                            for p in patterns {
                                collect_pattern_vars(p, &mut scope);
                            }
                        }
                        QueryPart::Merge { merges } => {
                            for m in merges {
                                collect_pattern_vars(&m.pattern, &mut scope);
                            }
                        }
                        QueryPart::With { items, .. } => {
                            let is_star = items.len() == 1
                                && matches!(
                                    &items[0].expr,
                                    Expr::FunctionCall { name, .. } if name == "__star__"
                                );
                            if is_star {
                                for item in items {
                                    if let Some(alias) = &item.alias {
                                        scope.insert(alias.clone());
                                    }
                                }
                            } else {
                                let mut next = std::collections::HashSet::new();
                                for item in items {
                                    if let Some(alias) = &item.alias {
                                        next.insert(alias.clone());
                                    } else if let Expr::Prop(v, p) = &item.expr {
                                        if p.is_empty() {
                                            next.insert(v.clone());
                                        }
                                    }
                                }
                                scope = next;
                            }
                        }
                        QueryPart::Call {
                            yields: Some(ys), ..
                        } => {
                            for (name, alias) in ys {
                                let v = alias.as_ref().unwrap_or(name);
                                scope.insert(v.clone());
                            }
                        }
                        _ => {}
                    }
                }
                scope.into_iter().collect()
            });
        keys.sort();
        keys
    } else {
        query.return_clause.items.iter().map(column_name).collect()
    };

    // 4. Read each projected value directly from the row by its canonical key.
    let mut records = if is_return_star {
        let mut star_records = Vec::with_capacity(resolved_paths.len());
        for path in resolved_paths {
            let mut values = Vec::with_capacity(columns.len());
            for key in &columns {
                values.push(binding_to_value(graph, path.get_binding(key))?);
            }
            star_records.push(Record { values });
        }
        star_records
    } else {
        rows_to_records(graph, &query.return_clause.items, resolved_paths)?
    };

    // 5. Apply RETURN DISTINCT deduplication for RETURN *. Every other
    // DISTINCT projection deduplicates inside the plan (a Distinct operator
    // below Sort and Limit), so the limit caps distinct rows.
    if query.return_clause.distinct && is_return_star {
        dedup_records(&mut records);
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
                // Built-in `issundb.*` graph-algorithm procedures run against the
                // live graph and synthesize a procedure on the fly; everything
                // else resolves against the table-backed registry. The built-ins
                // reuse the registry's YIELD and validation logic via
                // `resolve_against`.
                let rc = match crate::builtin_procs::build(graph, name, &arg_values)? {
                    Some(proc) => crate::procedure::resolve_against(
                        &proc,
                        &arg_values,
                        *implicit_args,
                        !standalone,
                        yields,
                        *yield_star,
                        &scope,
                        params,
                    )?,
                    None => registry.resolve(
                        name,
                        &arg_values,
                        *implicit_args,
                        !standalone,
                        yields,
                        *yield_star,
                        &scope,
                        params,
                    )?,
                };
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

/// Compute the key under which a RETURN/WITH item is stored in the projected row.
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
                    return if label_str.is_empty() {
                        format!("({})", format_cypher_value(&props))
                    } else {
                        format!("({} {})", label_str, format_cypher_value(&props))
                    };
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
                    return if type_str.is_empty() {
                        format_cypher_value(&props)
                    } else {
                        format!("{} {}", type_str, format_cypher_value(&props))
                    };
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

/// Convert a `GraphBinding` entry from a projected row into a JSON value.
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

/// Materialize result records for `items` from projected rows: one
/// `binding_to_value` read per cell, by each item's canonical projected key.
/// The keys are derived once, not per row.
pub(super) fn rows_to_records(
    graph: &Graph,
    items: &[crate::ast::ReturnItem],
    rows: Vec<SlotRow>,
) -> Result<Vec<Record>, String> {
    let keys: Vec<String> = items
        .iter()
        .map(|it| projected_key(&it.expr, &it.alias))
        .collect();
    let mut records = Vec::with_capacity(rows.len());
    for path in rows {
        let mut values = Vec::with_capacity(keys.len());
        for key in &keys {
            values.push(binding_to_value(graph, path.get_binding(key))?);
        }
        records.push(Record { values });
    }
    Ok(records)
}

/// Apply RETURN DISTINCT deduplication in place, keyed by the serialized row.
pub(super) fn dedup_records(records: &mut Vec<Record>) {
    let mut seen = std::collections::HashSet::new();
    records.retain(|r| {
        let key = serde_json::to_string(&r.values).unwrap_or_default();
        seen.insert(key)
    });
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

/// True when `eid` is already bound to one of `unique_rels` in `path`.
/// Implements openCypher relationship uniqueness: the hops of one pattern must
/// bind pairwise-distinct relationships, while separate MATCH clauses may
/// reuse a relationship (so the check covers sibling hops only, never the whole row).
fn edge_bound_to_sibling_rel(path: &SlotRow, unique_rels: &[String], eid: EdgeId) -> bool {
    unique_rels
        .iter()
        .any(|v| matches!(path.get_binding(v.as_str()), Some(GraphBinding::Edge(e)) if *e == eid))
}

/// Bulk-expand a batch of pre-expansion rows and apply a `Filter` predicate that
/// sits directly over a single-hop directed `Expand`. The predicate is evaluated
/// once per source row when it touches only pre-expansion variables (factorized),
/// or per `(src, dst)` row otherwise. Driven by the `RowStream::FilterOverExpand`
/// node; each source row is processed independently and in order, so streaming a
/// batch at a time yields the same rows in the same order as one whole-input call.
#[allow(clippy::too_many_arguments)]
fn filter_over_expand_batch(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    src_var: &str,
    rel_var: &str,
    dst_var: &str,
    rel_type: Option<&str>,
    is_incoming: bool,
    unique_rels: &[String],
    expression: &FilterExpr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<SlotRow>, String> {
    // Determine whether the filter references the new hop-local bindings.
    let refs = filter_refs_in_expr(expression);
    let filter_touches_expansion = refs.contains(rel_var) || refs.contains(dst_var);

    if child_paths.is_empty() {
        return Ok(vec![]);
    }

    // Bulk-expand from all unique source nodes.
    let mut src_nodes: Vec<NodeId> = child_paths
        .iter()
        .filter_map(|p| match p.get_binding(src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    src_nodes.sort_unstable();
    src_nodes.dedup();

    let transitions = expand_multi_type(graph, &src_nodes, rel_type, is_incoming)?;
    let mut transition_map: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
    for (src, eid, dst) in transitions {
        transition_map.entry(src).or_default().push((eid, dst));
    }

    let mut next_paths = Vec::new();

    // HasLabel on a shared variable: bulk-filter sources with GraphBLAS, then expand survivors.
    if let FilterExpr::HasLabel(variable, label) = expression {
        if variable != rel_var && variable != dst_var {
            let mut active: Vec<NodeId> = child_paths
                .iter()
                .filter_map(|p| match p.get_binding(variable.as_str()) {
                    Some(GraphBinding::Node(n)) => Some(*n),
                    _ => None,
                })
                .collect();
            active.sort_unstable();
            active.dedup();
            let filtered = graph
                .label_filter(&active, label)
                .map_err(|e| e.to_string())?;
            let pass_set: ahash::AHashSet<NodeId> = filtered.into_iter().collect();

            for path in &child_paths {
                if let Some(GraphBinding::Node(n)) = path.get_binding(variable.as_str()) {
                    if !pass_set.contains(n) {
                        continue;
                    }
                }
                let src_node = match path.get_binding(src_var) {
                    Some(GraphBinding::Node(n)) => *n,
                    _ => continue,
                };
                if let Some(dests) = transition_map.get(&src_node) {
                    for &(eid, dst_node) in dests {
                        if edge_bound_to_sibling_rel(path, unique_rels, eid) {
                            continue;
                        }
                        // Closing-hop guard: a pre-bound dst must match.
                        if path
                            .get_binding(dst_var)
                            .is_some_and(|e| *e != GraphBinding::Node(dst_node))
                        {
                            continue;
                        }
                        let mut new_path = path.clone();
                        new_path.bind_local(rel_var, GraphBinding::Edge(eid));
                        new_path.bind_local(dst_var, GraphBinding::Node(dst_node));
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
        // This avoids O(avg_degree) row clones per rejected source.
        let where_clause = filter_expr_to_where_clause(expression);
        for path in &child_paths {
            if !evaluate_where(graph, path, &where_clause, params)? {
                continue; // source fails; skip every destination for free
            }
            let src_node = match path.get_binding(src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            if let Some(dests) = transition_map.get(&src_node) {
                // Build factorized groups: `Arc` around the shared prefix so the
                // prefix row is owned once per source, not copied per destination.
                let group = FactorizedRecordGroup {
                    shared: std::sync::Arc::new(path.clone()),
                    extensions: dests
                        .iter()
                        .filter_map(|&(eid, dst_node)| {
                            if edge_bound_to_sibling_rel(path, unique_rels, eid) {
                                return None;
                            }
                            // Guard closing-hop mismatches (normally handled by MultiwayJoin).
                            if let Some(existing) = path.get_binding(dst_var) {
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
            let src_node = match path.get_binding(src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            if let Some(dests) = transition_map.get(&src_node) {
                for &(eid, dst_node) in dests {
                    if edge_bound_to_sibling_rel(path, unique_rels, eid) {
                        continue;
                    }
                    // Closing-hop guard: a pre-bound dst must match.
                    if path
                        .get_binding(dst_var)
                        .is_some_and(|e| *e != GraphBinding::Node(dst_node))
                    {
                        continue;
                    }
                    let mut new_path = path.clone();
                    new_path.bind_local(rel_var, GraphBinding::Edge(eid));
                    new_path.bind_local(dst_var, GraphBinding::Node(dst_node));
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

/// Build the adjacency of every node reachable from `src_nodes` within
/// `max_hops` edges, merging the requested directions and deduplicating each
/// node's `(edge, neighbor)` entries the same way `transition_map` does.
///
/// Each batched frontier expansion resolves the relationship type and runs one
/// SpMV for the whole frontier, so the variable-length BFS that consumes this
/// map pays a hash-map lookup per step instead of a per-node graph query. A
/// node's neighbors are computed once (the first time it enters the frontier);
/// `seen` makes the build terminate on cyclic graphs and unbounded ranges.
fn build_reachable_adjacency(
    graph: &Graph,
    src_nodes: &[NodeId],
    rel_type: Option<&str>,
    directions: &[bool],
    max_hops: usize,
) -> Result<ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>>, String> {
    let mut closure: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
    let mut seen: ahash::AHashSet<NodeId> = src_nodes.iter().copied().collect();
    let mut frontier: Vec<NodeId> = src_nodes.to_vec();

    // The trail BFS expands nodes at hop distance 0..max_hops-1 from a source,
    // so building adjacency through that many frontier rounds covers every node
    // it can reach. An unbounded range (usize::MAX) stops when the frontier
    // drains.
    let mut hop = 0usize;
    while !frontier.is_empty() && hop < max_hops {
        hop += 1;
        let mut next_frontier: Vec<NodeId> = Vec::new();
        for &dir in directions {
            let transitions = expand_multi_type(graph, &frontier, rel_type, dir)?;
            for (src, eid, dst) in transitions {
                let entry = closure.entry(src).or_default();
                if !entry.iter().any(|&(e, d)| e == eid && d == dst) {
                    entry.push((eid, dst));
                }
                if seen.insert(dst) {
                    next_frontier.push(dst);
                }
            }
        }
        frontier = next_frontier;
    }

    Ok(closure)
}

/// Execute the body of an `Expand` operator given pre-computed child paths.
///
/// Shared by the `RowStream::Expand` node (per batch) without duplicating the
/// BFS and single-hop expansion logic.
#[allow(clippy::too_many_arguments)]
fn expand_from_paths(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    src_var: &str,
    rel_var: &str,
    dst_var: &str,
    rel_type: Option<&str>,
    is_incoming: bool,
    is_undirected: bool,
    min_hops: usize,
    max_hops: usize,
    unique_rels: &[String],
    needs_path: bool,
) -> Result<Vec<SlotRow>, String> {
    let mut next_paths = Vec::new();

    let mut src_nodes: Vec<NodeId> = child_paths
        .iter()
        .filter_map(|p| match p.get_binding(src_var) {
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

    let mut transition_map: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
    for &dir in directions {
        let transitions = expand_multi_type(graph, &src_nodes, rel_type, dir)?;
        for (src, eid, dst) in transitions {
            let entry = transition_map.entry(src).or_default();
            if !entry.iter().any(|&(e, d)| e == eid && d == dst) {
                entry.push((eid, dst));
            }
        }
    }

    // Variable-length traversals walk one node at a time per source path. Rather
    // than issue a single-source graph query for every node at every hop (each
    // resolves the relationship type and runs an SpMV), build the adjacency for
    // the whole reachable closure once with batched frontier expansions and walk
    // that in-memory map. `transition_map` already covers the single-hop case.
    let closure_map: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> =
        if min_hops == 1 && max_hops == 1 {
            ahash::AHashMap::new()
        } else {
            build_reachable_adjacency(graph, &src_nodes, rel_type, directions, max_hops)?
        };

    for path in child_paths {
        let src_node = match path.get_binding(src_var) {
            Some(GraphBinding::Node(n)) => *n,
            _ => continue,
        };

        if min_hops == 1 && max_hops == 1 {
            if let Some(dests) = transition_map.get(&src_node) {
                let shared = std::sync::Arc::new(path);
                for &(eid, dst_node) in dests {
                    if edge_bound_to_sibling_rel(&shared, unique_rels, eid) {
                        continue;
                    }
                    if let Some(existing) = shared.get_binding(dst_var) {
                        if *existing != GraphBinding::Node(dst_node) {
                            continue;
                        }
                    }
                    let mut new_path = (*shared).clone();
                    new_path.bind_local(rel_var, GraphBinding::Edge(eid));
                    new_path.bind_local(dst_var, GraphBinding::Node(dst_node));

                    // Build and insert the Path object only when the pattern binds a
                    // path variable: it costs three record decodes per emitted row.
                    if needs_path {
                        let existing_path = match shared.get_binding(&format!("_path_{}", src_var))
                        {
                            Some(GraphBinding::Scalar(v)) => Some(v),
                            _ => None,
                        };
                        if let Ok(path_obj) =
                            extend_or_create_path(graph, existing_path, src_node, eid, dst_node)
                        {
                            new_path.bind_local(
                                &format!("_path_{}", dst_var),
                                GraphBinding::Scalar(path_obj),
                            );
                        }
                    }
                    next_paths.push(new_path);
                }
            }
        } else {
            // The traversed element list feeds only the Path object, so it stays
            // empty (no per-step record decodes) unless the pattern binds a path.
            let initial_traversed = if needs_path {
                vec![get_node_representation(graph, src_node)?]
            } else {
                Vec::new()
            };
            // openCypher trail semantics: a relationship may appear at most once
            // per path, nodes may repeat, and every distinct trail is one result
            // row. The per-path edge list is a Vec with a linear membership
            // check because trails are short; termination on unbounded ranges
            // follows from the finite edge set emptying the queue.
            let mut queue = vec![(src_node, initial_traversed.clone(), Vec::<EdgeId>::new())];
            let mut completed_paths: Vec<(NodeId, Vec<serde_json::Value>)> = Vec::new();

            if min_hops == 0 {
                completed_paths.push((src_node, initial_traversed));
            }

            for hop in 1..=max_hops {
                let mut next_queue = Vec::new();
                for (node, traversed, used_edges) in queue {
                    let neighbors = match closure_map.get(&node) {
                        Some(n) => n.as_slice(),
                        None => continue,
                    };
                    for &(eid, neigh_node) in neighbors {
                        if used_edges.contains(&eid) {
                            continue;
                        }
                        if edge_bound_to_sibling_rel(&path, unique_rels, eid) {
                            continue;
                        }
                        let mut next_used = used_edges.clone();
                        next_used.push(eid);

                        let mut next_traversed = traversed.clone();
                        if needs_path {
                            next_traversed.push(get_edge_representation(graph, eid)?);
                            next_traversed.push(get_node_representation(graph, neigh_node)?);
                        }

                        if hop >= min_hops {
                            completed_paths.push((neigh_node, next_traversed.clone()));
                        }
                        next_queue.push((neigh_node, next_traversed, next_used));
                    }
                }
                queue = next_queue;
                if queue.is_empty() {
                    break;
                }
            }

            for (neigh_node, path_elements) in completed_paths {
                // Closing-hop guard: a pre-bound dst must match.
                if path
                    .get_binding(dst_var)
                    .is_some_and(|existing| *existing != GraphBinding::Node(neigh_node))
                {
                    continue;
                }
                let mut new_path = path.clone();
                new_path.bind_local(dst_var, GraphBinding::Node(neigh_node));

                // Build the Path object only when the pattern binds a path variable.
                if needs_path {
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

                    new_path.bind_local(
                        &format!("_path_{}", dst_var),
                        GraphBinding::Scalar(path_obj),
                    );
                }
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
    // expansion machinery can traverse from it. A non-node anchor yields no
    // matches. The comprehension has no plan to derive a slot schema from, so
    // the seed row runs entirely on the locals overflow.
    let mut seed = SlotRow::from_path_map(std::sync::Arc::new(SlotSchema::empty()), outer);
    match seed.get_binding(&anchor_var) {
        Some(GraphBinding::Node(_)) => {}
        Some(GraphBinding::Scalar(v)) => {
            let id = v
                .as_object()
                .filter(|o| o.get("__type__").and_then(|t| t.as_str()) == Some("__Node__"))
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_i64());
            match id {
                Some(id) => {
                    seed.bind_local(&anchor_var, GraphBinding::Node(id as u64));
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
    // Relationship uniqueness within the comprehension's pattern.
    let mut prior_rel_vars: Vec<String> = Vec::new();

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
            &prior_rel_vars,
            pattern.path_variable.is_some(),
        )?;
        prior_rel_vars.push(rel_var.clone());
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
            if let Some(GraphBinding::Scalar(obj)) =
                path.get_binding(&format!("_path_{}", last_dst_var))
            {
                let obj = obj.clone();
                path.bind_local(pv, GraphBinding::Scalar(obj));
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
    paths: Vec<SlotRow>,
    dst_var: &str,
    labels: &[String],
    properties: &Option<HashMap<String, Expr>>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<SlotRow>, String> {
    if labels.is_empty() && properties.is_none() {
        return Ok(paths);
    }
    let mut out = Vec::new();
    for path in paths {
        let nid = match path.get_binding(dst_var) {
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

/// Decide which side of a `HashJoin` is the probe (required) side, which is the
/// build (hash) side, and the outer-join null-fill vars (`Some` for a left-outer
/// join). Used by `build_stream`'s `HashJoin` arm to assign the probe and build
/// streams consistently.
///
/// Whichever side carries the `OptionalMatch` is the build/optional side; the
/// other is the probe/required side, so every required row is emitted even when
/// the optional pattern finds no match (left-outer semantics). For an inner join
/// the probe is `left` (the optimizer standardizes the heavier branch onto the
/// left) and the build is `right`, so the smaller side becomes the hash table
/// and the larger side is the streamed probe.
fn join_roles<'a>(
    left: &'a PhysicalOperator,
    right: &'a PhysicalOperator,
) -> (
    &'a PhysicalOperator,
    &'a PhysicalOperator,
    Option<&'a [String]>,
) {
    if let PhysicalOperator::OptionalMatch { input, null_vars } = left {
        (right, input.as_ref(), Some(null_vars))
    } else if let PhysicalOperator::OptionalMatch { input, null_vars } = right {
        (left, input.as_ref(), Some(null_vars))
    } else {
        (left, right, None)
    }
}

/// Variables bound by both join sides: the equi-join keys. Computed from the
/// plans (not the `OptionalMatch` wrapper, whose `null_vars` would inflate the
/// set), so the streaming and materializing paths derive identical keys.
fn join_common_vars(probe_op: &PhysicalOperator, build_op: &PhysicalOperator) -> Vec<String> {
    let probe_bound = Optimizer::bound_vars(probe_op);
    let build_bound = Optimizer::bound_vars(build_op);
    probe_bound.intersection(&build_bound).cloned().collect()
}

/// Build the SIP node-id sets (one per common var that is bound to nodes on the
/// build side) used to restrict the probe-side scans. Only meaningful for inner
/// joins; an empty result means SIP cannot prune.
fn build_sip(common_vars: &[String], build_rows: &[SlotRow]) -> HashMap<String, HashSet<NodeId>> {
    common_vars
        .iter()
        .filter_map(|var| {
            let ids: HashSet<NodeId> = build_rows
                .iter()
                .filter_map(|p| match p.get_binding(var) {
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
        .collect()
}

/// Build the equi-join hash table from the fully materialized build side,
/// keyed by the common-variable bindings. Rows missing a common var are dropped
/// (they cannot match), matching the materializing executor.
fn build_hash_table(
    common_vars: &[String],
    build_rows: Vec<SlotRow>,
) -> HashMap<Vec<GraphBinding>, Vec<SlotRow>> {
    let mut hash_table: HashMap<Vec<GraphBinding>, Vec<SlotRow>> = HashMap::new();
    for op in build_rows {
        let key: Option<Vec<GraphBinding>> = common_vars
            .iter()
            .map(|v| op.get_binding(v).cloned())
            .collect();
        if let Some(key) = key {
            hash_table.entry(key).or_default().push(op);
        }
    }
    hash_table
}

/// Fill any `null_vars` not already present in `path` with `null`. Used for the
/// left-outer case when a required row has no optional match.
fn null_fill(path: &mut SlotRow, null_vars: &[String]) {
    for v in null_vars {
        if path.get_binding(v.as_str()).is_none() {
            path.bind_local(v, GraphBinding::Scalar(serde_json::Value::Null));
        }
    }
}

/// The fully prepared build side of a `HashJoin`, ready to probe row by row.
/// `Cartesian` is used when the two sides share no variables (cross product);
/// `Equi` is the hashed equi-join.
enum JoinProbeData {
    Equi {
        common_vars: Vec<String>,
        hash_table: HashMap<Vec<GraphBinding>, Vec<SlotRow>>,
    },
    Cartesian {
        build_rows: Vec<SlotRow>,
    },
}

/// Join one batch of probe rows against an already-built `JoinProbeData`,
/// applying left-outer null-fill when `null_vars` is `Some`. This is the inner
/// loop of the `HashJoin` executor arm, factored out so the materializing path
/// and the streaming `RowStream::HashJoin` share one implementation; processing
/// rows in batches versus all at once changes neither the output rows nor their
/// order, because the build side is fully materialized before any probe row is
/// joined.
fn hash_join_rows(
    probe_batch: Vec<SlotRow>,
    data: &JoinProbeData,
    null_vars: Option<&[String]>,
) -> Vec<SlotRow> {
    let mut out = Vec::new();
    for rp in probe_batch {
        match data {
            JoinProbeData::Equi {
                common_vars,
                hash_table,
            } => {
                let key: Option<Vec<GraphBinding>> = common_vars
                    .iter()
                    .map(|v| rp.get_binding(v).cloned())
                    .collect();
                // A row missing a common var cannot join; drop it.
                if let Some(key) = key {
                    if let Some(matches) = hash_table.get(&key) {
                        for op in matches {
                            let mut merged = rp.clone();
                            merged.merge_from(op);
                            out.push(merged);
                        }
                    } else if let Some(null_vars) = null_vars {
                        let mut merged = rp;
                        null_fill(&mut merged, null_vars);
                        out.push(merged);
                    }
                }
            }
            JoinProbeData::Cartesian { build_rows } => {
                if build_rows.is_empty() {
                    if let Some(null_vars) = null_vars {
                        let mut merged = rp;
                        null_fill(&mut merged, null_vars);
                        out.push(merged);
                    }
                } else {
                    for op in build_rows {
                        let mut merged = rp.clone();
                        merged.merge_from(op);
                        out.push(merged);
                    }
                }
            }
        }
    }
    out
}

/// Close a `MultiwayJoin` over one batch of child rows: bulk-expand the distinct
/// `closing_src_var` nodes once, index the transitions, then emit a row per
/// matching `(closing_src, closing_dst)` pair. Factored out of the executor arm
/// so the materializing path and the streaming `RowStream::MultiwayJoin` share
/// one implementation. When streaming, the bulk expansion runs once per batch
/// rather than once over all rows; for typed relationships that is a cheap
/// per-source adjacency loop, and a `LIMIT` bounds the number of batches.
#[allow(clippy::too_many_arguments)]
fn multiway_join_rows(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    closing_src_var: &str,
    closing_dst_var: &str,
    closing_rel_type: Option<&str>,
    closing_rel_var: &str,
    closing_is_incoming: bool,
    closing_is_undirected: bool,
    closing_unique_rels: &[String],
) -> Result<Vec<SlotRow>, String> {
    if child_paths.is_empty() {
        return Ok(vec![]);
    }

    // Collect unique closing-src node IDs for a single bulk expansion. Paying
    // O(sum of degrees of unique sources) once is far cheaper than iterating all
    // neighbors for every input row.
    let mut src_nodes: Vec<NodeId> = child_paths
        .iter()
        .filter_map(|p| match p.get_binding(closing_src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    src_nodes.sort_unstable();
    src_nodes.dedup();

    let mut next_paths = Vec::new();

    if closing_is_undirected {
        // Undirected closing hop: the edge may run either way between the two
        // bound nodes, so check both adjacency directions. Index every distinct
        // edge (deduplicated by `(src, eid, dst)` to match the undirected Expand
        // path: one row per edge, self-loops counted once) keyed by
        // `closing_src → (closing_dst → [EdgeId])`.
        let mut join_map: HashMap<NodeId, HashMap<NodeId, Vec<EdgeId>>> = HashMap::new();
        let mut seen: HashSet<(NodeId, EdgeId, NodeId)> = HashSet::new();
        for dir in [false, true] {
            let transitions = expand_multi_type(graph, &src_nodes, closing_rel_type, dir)?;
            for (src, eid, dst) in transitions {
                if seen.insert((src, eid, dst)) {
                    join_map
                        .entry(src)
                        .or_default()
                        .entry(dst)
                        .or_default()
                        .push(eid);
                }
            }
        }

        for path in child_paths {
            let closing_src = match path.get_binding(closing_src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            let closing_dst = match path.get_binding(closing_dst_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };

            if let Some(eids) = join_map.get(&closing_src).and_then(|m| m.get(&closing_dst)) {
                for &eid in eids {
                    if edge_bound_to_sibling_rel(&path, closing_unique_rels, eid) {
                        continue;
                    }
                    let mut new_path = path.clone();
                    new_path.bind_local(closing_rel_var, GraphBinding::Edge(eid));
                    next_paths.push(new_path);
                }
            }
        }
    } else {
        let transitions =
            expand_multi_type(graph, &src_nodes, closing_rel_type, closing_is_incoming)?;

        // Index the transitions as (closing_src, closing_dst) → [EdgeId] for O(1)
        // lookup. The value is a list, not a single EdgeId, because parallel
        // edges between the same pair are distinct matches and each must emit
        // its own row.
        let mut join_map: HashMap<NodeId, HashMap<NodeId, Vec<EdgeId>>> = HashMap::new();
        for (src, eid, dst) in transitions {
            join_map
                .entry(src)
                .or_default()
                .entry(dst)
                .or_default()
                .push(eid);
        }

        for path in child_paths {
            let closing_src = match path.get_binding(closing_src_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };
            let closing_dst = match path.get_binding(closing_dst_var) {
                Some(GraphBinding::Node(n)) => *n,
                _ => continue,
            };

            if let Some(eids) = join_map.get(&closing_src).and_then(|m| m.get(&closing_dst)) {
                for &eid in eids {
                    if edge_bound_to_sibling_rel(&path, closing_unique_rels, eid) {
                        continue;
                    }
                    let mut new_path = path.clone();
                    new_path.bind_local(closing_rel_var, GraphBinding::Edge(eid));
                    next_paths.push(new_path);
                }
            }
        }
    }

    Ok(next_paths)
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
    /// Sibling relationship variables of the same pattern this hop's edge must
    /// differ from (openCypher relationship uniqueness).
    unique_rels: &'a [String],
}

/// Execute a maximal linear chain of single-hop directed Expands as one fused
/// operation. Each hop level is bulk-expanded once (distinct sources only), then
/// output rows are produced by threading every base path through all hops,
/// cloning a base path exactly once per emitted row regardless of chain length.
/// This generalizes the former two-hop fast path to N hops and preserves path
/// multiplicity and all `(rel, dst)` bindings.
fn execute_expand_chain_n(
    graph: &Graph,
    base_paths: Vec<SlotRow>,
    hops: &[ChainHop<'_>],
) -> Result<Vec<SlotRow>, String> {
    if base_paths.is_empty() || hops.is_empty() {
        return Ok(vec![]);
    }

    // Bulk-expand each hop level. The chain is linear, so level i's source set is
    // the set of nodes reached at level i-1; level 0's sources come from the base.
    let mut level_maps: Vec<ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>>> =
        Vec::with_capacity(hops.len());
    let mut frontier: Vec<NodeId> = base_paths
        .iter()
        .filter_map(|p| match p.get_binding(hops[0].src_var) {
            Some(GraphBinding::Node(n)) => Some(*n),
            _ => None,
        })
        .collect();
    frontier.sort_unstable();
    frontier.dedup();

    for hop in hops {
        let expanded = expand_multi_type(graph, &frontier, hop.rel_type, hop.is_incoming)?;
        let mut map: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
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
        let Some(GraphBinding::Node(start)) = base_path.get_binding(hops[0].src_var) else {
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
/// and are materialized into a single cloned row at the leaf.
#[allow(clippy::too_many_arguments)]
fn thread_chain(
    base_path: &SlotRow,
    src: NodeId,
    hops: &[ChainHop<'_>],
    level_maps: &[ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>>],
    hop_idx: usize,
    stack: &mut Vec<(EdgeId, NodeId)>,
    out: &mut Vec<SlotRow>,
) {
    if hop_idx == hops.len() {
        let mut new_path = base_path.clone();
        for (i, &(eid, dst)) in stack.iter().enumerate() {
            new_path.bind_local(hops[i].rel_var, GraphBinding::Edge(eid));
            new_path.bind_local(hops[i].dst_var, GraphBinding::Node(dst));
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
        if let Some(existing) = base_path.get_binding(hop.dst_var) {
            if *existing != GraphBinding::Node(dst) {
                continue;
            }
        }
        // Relationship uniqueness: the edge must differ from every sibling
        // hop's edge, whether that sibling was bound by the base path (a
        // partially fused pattern) or by an earlier hop of this chain.
        if edge_bound_to_sibling_rel(base_path, hop.unique_rels, eid)
            || stack.iter().enumerate().any(|(i, &(prev_eid, _))| {
                prev_eid == eid && hop.unique_rels.iter().any(|v| v == hops[i].rel_var)
            })
        {
            continue;
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

/// Apply a `Project` operator's RETURN/WITH items to each input row. Extracted
/// from the `Project` execution arm so the bounded-scan path can reuse it.
pub(super) fn project_rows(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    items: &[(Expr, Option<String>)],
    is_barrier: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<SlotRow>, String> {
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
        let mut projected_path: SlotRow = if is_barrier {
            SlotRow::empty(path.schema_arc())
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
                // the computed value in the row under `target_var`. Pull it
                // directly rather than trying to re-evaluate the expression.
                Expr::CountStar | Expr::Agg(_, _) => {
                    if let Some(binding) = path.get_binding(&target_var) {
                        projected_path.bind_local(&target_var, binding.clone());
                    } else {
                        projected_path
                            .bind_local(&target_var, GraphBinding::Scalar(serde_json::Value::Null));
                    }
                }
                Expr::Prop(var, prop) if prop.is_empty() => {
                    // Whole-variable reference: first try the node binding,
                    // then fall back to a scalar already in the PathMap
                    // (e.g., a group-by column emitted by Aggregate).
                    if let Some(binding) = path.get_binding(var) {
                        projected_path.bind_local(&target_var, binding.clone());
                    } else if let Some(binding) = path.get_binding(&target_var) {
                        projected_path.bind_local(&target_var, binding.clone());
                    }
                }
                _ => match evaluate_expr(graph, &path, expr, params) {
                    Ok(val) => {
                        projected_path.bind_local(&target_var, GraphBinding::Scalar(val));
                    }
                    Err(err) => {
                        if let Some(binding) = path.get_binding(&target_var) {
                            projected_path.bind_local(&target_var, binding.clone());
                            continue;
                        }
                        return Err(err);
                    }
                },
            }
        }

        next_paths.push(projected_path);
    }

    Ok(next_paths)
}

/// Apply a `Filter` operator's predicate to an already-materialized batch of
/// rows. `FilterExpr::HasLabel` routes through the bulk GraphBLAS label filter
/// (one set-membership pass over the distinct bound nodes); every other
/// predicate is evaluated row by row. This is the shared body of the
/// `PhysicalOperator::Filter` default path and the streaming `Filter` node, so
/// both produce identical rows in identical order.
pub(super) fn apply_filter(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    expression: &FilterExpr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<SlotRow>, String> {
    let mut next_paths = Vec::new();

    if let FilterExpr::HasLabel(variable, label) = expression {
        // Collect the distinct node IDs bound to this variable for bulk label filtering.
        let mut active_nodes: Vec<NodeId> = child_paths
            .iter()
            .filter_map(|p| match p.get_binding(variable) {
                Some(GraphBinding::Node(n)) => Some(*n),
                _ => None,
            })
            .collect();
        active_nodes.sort_unstable();
        active_nodes.dedup();

        let filtered_nodes = graph
            .label_filter(&active_nodes, label)
            .map_err(|e| e.to_string())?;
        let filtered_set: ahash::AHashSet<NodeId> = filtered_nodes.into_iter().collect();

        for path in child_paths {
            if let Some(GraphBinding::Node(node)) = path.get_binding(variable) {
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

/// Number of rows a streaming operator yields per `next_batch` pull. Large
/// enough to amortize per-pull overhead, small enough that a `LIMIT` stops the
/// upstream scan and expansion after only a few batches.
const STREAM_BATCH: usize = 256;

/// A pull-based, batch-at-a-time row producer. This is the engine's only
/// execution path: there is one `RowStream` variant per `PhysicalOperator`, and
/// `execute_physical` builds a stream and drains it. Each `next_batch` returns up
/// to `STREAM_BATCH` rows; an empty return signals exhaustion. The bits each node
/// needs are cloned from the plan at build time so the stream does not borrow the
/// operator tree.
///
/// Pipelined nodes (`Project`, `Filter`, `Expand`, `Unwind`, the streaming join
/// probe, and so on) delegate their row transform to a shared helper
/// (`expand_from_paths`, `apply_filter`, `project_rows`, `hash_join_rows`, ...),
/// so the result rows and their order are exactly what a single whole-input pass
/// would produce. Blocking nodes (`Sort`, `Aggregate`, `WritePart`, and a hash
/// join's build side) consume their full input before emitting; a `Limit` then
/// short-circuits any pipelined input, and a `Sort` directly under a `Limit`
/// keeps only the top rows.
enum RowStream {
    /// Lazy label scan: candidate ids are fetched once on the first pull, then
    /// emitted `STREAM_BATCH` at a time so an upstream `LIMIT` never forces the
    /// whole label into rows.
    LabelScan {
        variable: String,
        label: Option<String>,
        /// SIP restriction: when `Some`, only these node ids are emitted (the
        /// scan is intersected with the build side of an enclosing streaming
        /// hash join).
        allowed: Option<HashSet<NodeId>>,
        ids: Option<std::vec::IntoIter<NodeId>>,
    },
    /// A streamable leaf other than `LabelScan` (id seek, index scan, range
    /// scan, or the single bootstrap row). These are already bounded or small,
    /// so they materialize once via `execute_physical` and drain in batches.
    Materialized {
        op: Box<PhysicalOperator>,
        rows: Option<std::vec::IntoIter<SlotRow>>,
    },
    Project {
        input: Box<RowStream>,
        items: Vec<(Expr, Option<String>)>,
        is_barrier: bool,
    },
    Filter {
        input: Box<RowStream>,
        expression: FilterExpr,
    },
    Expand {
        input: Box<RowStream>,
        src_var: String,
        rel_var: String,
        dst_var: String,
        rel_type: Option<String>,
        is_incoming: bool,
        is_undirected: bool,
        min_hops: usize,
        max_hops: usize,
        unique_rels: Vec<String>,
        /// True only when the pattern binds a path variable; the expansion then
        /// materializes a `_path_*` object per emitted row.
        needs_path: bool,
        /// Holds expansion output beyond `STREAM_BATCH`: one input row can fan
        /// out to many neighbors, so the overflow is buffered and served on
        /// later pulls before the next input batch is fetched.
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// A hash join whose probe side streams. The build side is materialized once
    /// and hashed on the first pull (`state`); thereafter the probe side is
    /// pulled a batch at a time and joined against the hash table, so an upstream
    /// `LIMIT` stops the probe scan/expansion early. The build side cannot be
    /// short-circuited (the hash table must be complete before any probe row is
    /// joined), so it is the floor on the work this saves.
    HashJoin {
        build_op: Box<PhysicalOperator>,
        probe_op: Box<PhysicalOperator>,
        /// `Some` for a left-outer join: probe rows with no match are null-filled.
        null_vars: Option<Vec<String>>,
        /// Built lazily on the first pull (build side materialized + hashed,
        /// probe stream constructed with SIP applied).
        state: Option<Box<HashJoinState>>,
        /// Overflow when one probe batch joins to more than `STREAM_BATCH` rows.
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// A closing-hop (multiway) join whose input streams. Each input batch is
    /// closed against the bound target via `multiway_join_rows`, so an upstream
    /// `LIMIT` stops the input scan/expansion early.
    MultiwayJoin {
        input: Box<RowStream>,
        closing_src_var: String,
        closing_dst_var: String,
        closing_rel_type: Option<String>,
        closing_rel_var: String,
        closing_is_incoming: bool,
        closing_is_undirected: bool,
        closing_unique_rels: Vec<String>,
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// Fused linear chain of single-hop directed Expands: the streaming form of
    /// the `execute_expand_chain_n` fast path. Each input batch is threaded
    /// through all hops at once; overflow beyond `STREAM_BATCH` is buffered.
    ExpandChain {
        base: Box<RowStream>,
        hops: Vec<OwnedChainHop>,
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// A `Filter` directly over a single-hop directed `Expand`: the streaming form
    /// of the `filter_over_expand_batch` factorization fast path. The predicate is
    /// applied once per source row (or per `(src, dst)` row when it references the
    /// expansion bindings); overflow is buffered.
    FilterOverExpand {
        base: Box<RowStream>,
        src_var: String,
        rel_var: String,
        dst_var: String,
        rel_type: Option<String>,
        is_incoming: bool,
        unique_rels: Vec<String>,
        expression: FilterExpr,
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// Blocking sort: drains `input` fully on the first pull, sorts it (or keeps
    /// the top `bound` rows), then emits the result in batches. A `Sort` directly
    /// under a `Limit` gets `bound = Some(skip + count)` for a top-N selection.
    Sort {
        input: Box<RowStream>,
        items: Vec<crate::ast::SortItem>,
        bound: Option<usize>,
        out: Option<std::vec::IntoIter<SlotRow>>,
    },
    /// Streaming DISTINCT: a stateful pass-through emitting each row whose dedup
    /// key has not been seen, so an upstream `LIMIT` can short-circuit. `keys`
    /// narrows the dedup key to those binding names; `None` keys the full row.
    Distinct {
        input: Box<RowStream>,
        keys: Option<Vec<String>>,
        seen: HashSet<String>,
    },
    /// Blocking aggregate: drains and folds `input` on the first pull, then emits
    /// the grouped rows in batches.
    Aggregate {
        input: Box<RowStream>,
        group_by: Vec<(Expr, Option<String>)>,
        aggregations: Vec<(AggFn, Expr, String)>,
        out: Option<std::vec::IntoIter<SlotRow>>,
    },
    /// OPTIONAL MATCH: forwards `input` rows; if the entire input is empty, emits
    /// exactly one null-filled row for the pattern variables.
    OptionalMatch {
        input: Box<RowStream>,
        null_vars: Vec<String>,
        any: bool,
        done: bool,
    },
    /// UNWIND: 1:N expansion of a list expression with an overflow buffer.
    Unwind {
        input: Box<RowStream>,
        expr: Expr,
        variable: String,
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// A resolved `CALL`: the N:M cross of each input row with the procedure's
    /// output rows, with an overflow buffer.
    ProcedureCall {
        input: Box<RowStream>,
        output_vars: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        buf: std::collections::VecDeque<SlotRow>,
    },
    /// SKIP/LIMIT: drops the first `skip` rows then emits up to `count`, returning
    /// empty (stopping upstream pulls) once `count` rows are emitted.
    Limit {
        input: Box<RowStream>,
        skip: usize,
        count: usize,
        skipped: usize,
        emitted: usize,
        validated: bool,
    },
    /// A write clause executed over the input. Blocking: the input is drained
    /// fully and the writes applied before any row is emitted, so `DELETE` sees
    /// the whole result and a trailing `LIMIT` cannot skip writes.
    WritePart {
        input: Box<RowStream>,
        part: crate::ast::QueryPart,
        out: Option<std::vec::IntoIter<SlotRow>>,
    },
}

/// Owned form of `ChainHop` for a streaming `ExpandChain` node. The borrowed
/// `ChainHop` slice is rebuilt from this at call time so `execute_expand_chain_n`
/// is reused unchanged.
struct OwnedChainHop {
    src_var: String,
    rel_var: String,
    dst_var: String,
    rel_type: Option<String>,
    is_incoming: bool,
    unique_rels: Vec<String>,
}

/// The prepared build side of a streaming `HashJoin`, constructed on the first
/// pull: the build side materialized and hashed (`data`), plus the probe-side
/// `RowStream` (built with SIP applied for inner joins).
struct HashJoinState {
    data: JoinProbeData,
    probe: Box<RowStream>,
}

/// Build the `RowStream` for `op`. Total over `PhysicalOperator`: every operator
/// maps to a variant. Leaves other than `LabelScan` become a `Materialized` node
/// that calls `eval_leaf` once on the first pull.
fn build_stream(op: &PhysicalOperator) -> RowStream {
    build_stream_with_sip(op, &HashMap::new())
}

/// Like `build_stream`, but threads a SIP node-id map down to the `LabelScan`
/// leaves: a scan whose variable is in `sip` is restricted to those ids, so a
/// streaming join's probe side prunes exactly as the build side allows. SIP is a
/// performance restriction only (it never changes results), so it is propagated
/// through the pipelined operators and dropped at the `Materialized` and nested
/// `HashJoin` boundaries (a nested join derives its own SIP from its build side).
fn build_stream_with_sip(
    op: &PhysicalOperator,
    sip: &HashMap<String, HashSet<NodeId>>,
) -> RowStream {
    match op {
        PhysicalOperator::LabelScan { variable, label } => RowStream::LabelScan {
            variable: variable.clone(),
            label: label.clone(),
            allowed: sip.get(variable).cloned(),
            ids: None,
        },
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
        } => RowStream::Project {
            input: Box::new(build_stream_with_sip(input, sip)),
            items: items.clone(),
            is_barrier: *is_barrier,
        },
        PhysicalOperator::Filter { input, expression } => {
            // Factorization fast path: a non-HasLabel predicate directly over a
            // single-hop directed Expand applies once per source row. Mirrors the
            // precedence of the materializing `Filter` arm.
            if let PhysicalOperator::Expand {
                input: expand_input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected: false,
                min_hops: 1,
                max_hops: 1,
                unique_rels,
                // The factorized path never builds `_path_*` objects.
                needs_path: false,
            } = input.as_ref()
            {
                if !matches!(expression, FilterExpr::HasLabel(..)) {
                    return RowStream::FilterOverExpand {
                        base: Box::new(build_stream_with_sip(expand_input, sip)),
                        src_var: src_var.clone(),
                        rel_var: rel_var.clone(),
                        dst_var: dst_var.clone(),
                        rel_type: rel_type.clone(),
                        is_incoming: *is_incoming,
                        unique_rels: unique_rels.clone(),
                        expression: expression.clone(),
                        buf: std::collections::VecDeque::new(),
                    };
                }
            }
            RowStream::Filter {
                input: Box::new(build_stream_with_sip(input, sip)),
                expression: expression.clone(),
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
            unique_rels,
            needs_path,
        } => {
            // Fused-chain fast path: collapse a maximal linear chain of single-hop
            // directed Expands, mirroring the materializing `Expand` arm. The fused
            // chain never builds `_path_*` objects, so hops of a named-path pattern
            // are excluded.
            if *min_hops == 1 && *max_hops == 1 && !*is_undirected && !*needs_path {
                let mut hops = vec![OwnedChainHop {
                    src_var: src_var.clone(),
                    rel_var: rel_var.clone(),
                    dst_var: dst_var.clone(),
                    rel_type: rel_type.clone(),
                    is_incoming: *is_incoming,
                    unique_rels: unique_rels.clone(),
                }];
                let mut bottom_src = src_var.as_str();
                let mut base = input.as_ref();
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
                    unique_rels: inner_unique_rels,
                    needs_path: false,
                } = base
                {
                    if bottom_src != inner_dst_var {
                        break;
                    }
                    hops.push(OwnedChainHop {
                        src_var: inner_src_var.clone(),
                        rel_var: inner_rel_var.clone(),
                        dst_var: inner_dst_var.clone(),
                        rel_type: inner_rel_type.clone(),
                        is_incoming: *inner_is_incoming,
                        unique_rels: inner_unique_rels.clone(),
                    });
                    bottom_src = inner_src_var.as_str();
                    base = inner_input.as_ref();
                }
                if hops.len() >= 2 {
                    // Collected top-to-bottom; execute bottom-to-top.
                    hops.reverse();
                    return RowStream::ExpandChain {
                        base: Box::new(build_stream_with_sip(base, sip)),
                        hops,
                        buf: std::collections::VecDeque::new(),
                    };
                }
            }
            RowStream::Expand {
                input: Box::new(build_stream_with_sip(input, sip)),
                src_var: src_var.clone(),
                rel_var: rel_var.clone(),
                dst_var: dst_var.clone(),
                rel_type: rel_type.clone(),
                is_incoming: *is_incoming,
                is_undirected: *is_undirected,
                min_hops: *min_hops,
                max_hops: *max_hops,
                unique_rels: unique_rels.clone(),
                needs_path: *needs_path,
                buf: std::collections::VecDeque::new(),
            }
        }
        PhysicalOperator::MultiwayJoin {
            input,
            closing_src_var,
            closing_dst_var,
            closing_rel_type,
            closing_rel_var,
            closing_is_incoming,
            closing_is_undirected,
            closing_unique_rels,
        } => RowStream::MultiwayJoin {
            input: Box::new(build_stream_with_sip(input, sip)),
            closing_src_var: closing_src_var.clone(),
            closing_dst_var: closing_dst_var.clone(),
            closing_rel_type: closing_rel_type.clone(),
            closing_rel_var: closing_rel_var.clone(),
            closing_is_incoming: *closing_is_incoming,
            closing_is_undirected: *closing_is_undirected,
            closing_unique_rels: closing_unique_rels.clone(),
            buf: std::collections::VecDeque::new(),
        },
        PhysicalOperator::Limit { input, skip, count } => {
            // Top-N: a `Sort` directly under the `Limit` keeps only `skip + count`
            // rows, so the sort never materializes its full output.
            let inner = if let PhysicalOperator::Sort {
                input: sort_input,
                items,
            } = input.as_ref()
            {
                RowStream::Sort {
                    input: Box::new(build_stream_with_sip(sort_input, sip)),
                    items: items.clone(),
                    bound: Some(skip.saturating_add(*count)),
                    out: None,
                }
            } else {
                build_stream_with_sip(input, sip)
            };
            RowStream::Limit {
                input: Box::new(inner),
                skip: *skip,
                count: *count,
                skipped: 0,
                emitted: 0,
                validated: false,
            }
        }
        PhysicalOperator::Sort { input, items } => RowStream::Sort {
            input: Box::new(build_stream_with_sip(input, sip)),
            items: items.clone(),
            bound: None,
            out: None,
        },
        PhysicalOperator::Distinct { input, keys } => RowStream::Distinct {
            input: Box::new(build_stream_with_sip(input, sip)),
            keys: keys.clone(),
            seen: HashSet::new(),
        },
        PhysicalOperator::Aggregate {
            input,
            group_by,
            aggregations,
        } => RowStream::Aggregate {
            input: Box::new(build_stream_with_sip(input, sip)),
            group_by: group_by.clone(),
            aggregations: aggregations.clone(),
            out: None,
        },
        PhysicalOperator::OptionalMatch { input, null_vars } => RowStream::OptionalMatch {
            input: Box::new(build_stream_with_sip(input, sip)),
            null_vars: null_vars.clone(),
            any: false,
            done: false,
        },
        PhysicalOperator::Unwind {
            input,
            expr,
            variable,
        } => RowStream::Unwind {
            input: Box::new(build_stream_with_sip(input, sip)),
            expr: expr.clone(),
            variable: variable.clone(),
            buf: std::collections::VecDeque::new(),
        },
        PhysicalOperator::ProcedureCall {
            input,
            output_vars,
            rows,
        } => RowStream::ProcedureCall {
            input: Box::new(build_stream_with_sip(input, sip)),
            output_vars: output_vars.clone(),
            rows: rows.clone(),
            buf: std::collections::VecDeque::new(),
        },
        PhysicalOperator::WritePart { input, part } => RowStream::WritePart {
            input: Box::new(build_stream_with_sip(input, sip)),
            part: part.clone(),
            out: None,
        },
        PhysicalOperator::HashJoin { left, right } => {
            // The probe side's own SIP is derived from this join's build side at
            // run time (it needs the materialized build rows), so it is built
            // lazily in `next_batch`; the outer `sip` does not propagate into a
            // nested join, matching the former `execute_with_sip`.
            let (probe_op, build_op, null_vars) = join_roles(left, right);
            RowStream::HashJoin {
                build_op: Box::new(build_op.clone()),
                probe_op: Box::new(probe_op.clone()),
                null_vars: null_vars.map(|v| v.to_vec()),
                state: None,
                buf: std::collections::VecDeque::new(),
            }
        }
        // The remaining streamable leaves (`NodeByIdSeek`, `NodeIndexScan`,
        // `NodeRangeScan`, `SingleRow`): evaluate eagerly on the first pull via
        // `eval_leaf` and drain in batches. SIP is not applied here.
        other => RowStream::Materialized {
            op: Box::new(other.clone()),
            rows: None,
        },
    }
}

impl RowStream {
    /// Produce the next batch of up to `STREAM_BATCH` rows. An empty `Vec`
    /// means the stream is exhausted. Size-reducing nodes (Filter) and the
    /// Expand buffer-refill loop pull repeatedly so an empty intermediate batch
    /// is never mistaken for end-of-stream.
    fn next_batch(
        &mut self,
        graph: &Graph,
        params: &HashMap<String, serde_json::Value>,
        schema: &std::sync::Arc<SlotSchema>,
    ) -> Result<Vec<SlotRow>, String> {
        match self {
            RowStream::LabelScan {
                variable,
                label,
                allowed,
                ids,
            } => {
                let iter = match ids {
                    Some(it) => it,
                    None => {
                        // Candidate set: the label's nodes (or all nodes),
                        // intersected with the SIP-allowed ids when present.
                        let candidates: Vec<NodeId> = match (label.as_deref(), allowed.as_ref()) {
                            (Some(lbl), Some(set)) => graph
                                .nodes_by_label(lbl)
                                .map_err(|e| e.to_string())?
                                .into_iter()
                                .filter(|id| set.contains(id))
                                .collect(),
                            (None, Some(set)) => {
                                let mut v: Vec<NodeId> = set.iter().copied().collect();
                                v.sort_unstable();
                                v
                            }
                            (Some(lbl), None) => {
                                graph.nodes_by_label(lbl).map_err(|e| e.to_string())?
                            }
                            (None, None) => graph.all_nodes().map_err(|e| e.to_string())?,
                        };
                        ids.insert(candidates.into_iter())
                    }
                };
                let out: Vec<SlotRow> = iter
                    .by_ref()
                    .take(STREAM_BATCH)
                    .map(|nid| {
                        let mut path = SlotRow::empty(schema.clone());
                        path.bind_local(variable, GraphBinding::Node(nid));
                        path
                    })
                    .collect();
                Ok(out)
            }
            RowStream::Materialized { op, rows } => {
                let iter = match rows {
                    Some(it) => it,
                    None => rows.insert(eval_leaf(graph, op, params, schema)?.into_iter()),
                };
                Ok(iter.by_ref().take(STREAM_BATCH).collect())
            }
            RowStream::Project {
                input,
                items,
                is_barrier,
            } => {
                // Project is a 1:1 transform, so a non-empty input batch yields a
                // non-empty output batch; loop only to pass through end-of-stream.
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                project_rows(graph, batch, items, *is_barrier, params)
            }
            RowStream::Filter { input, expression } => {
                // A filter can empty a batch without exhausting the input, so
                // keep pulling until a batch survives or the input runs out.
                loop {
                    let batch = input.next_batch(graph, params, schema)?;
                    if batch.is_empty() {
                        return Ok(vec![]);
                    }
                    let kept = apply_filter(graph, batch, expression, params)?;
                    if !kept.is_empty() {
                        return Ok(kept);
                    }
                }
            }
            RowStream::Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected,
                min_hops,
                max_hops,
                unique_rels,
                needs_path,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                let expanded = expand_from_paths(
                    graph,
                    batch,
                    src_var,
                    rel_var,
                    dst_var,
                    rel_type.as_deref(),
                    *is_incoming,
                    *is_undirected,
                    *min_hops,
                    *max_hops,
                    unique_rels,
                    *needs_path,
                )?;
                buf.extend(expanded);
            },
            RowStream::HashJoin {
                build_op,
                probe_op,
                null_vars,
                state,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                // First pull: materialize and hash the build side, then build the
                // probe stream (with SIP for inner joins). The hash table is
                // complete before any probe row is joined, so the streamed join
                // yields exactly the rows the materializing join would.
                let st = match state {
                    Some(s) => s,
                    None => {
                        let build_rows = execute_physical(graph, build_op, params, schema)?;
                        let common_vars = join_common_vars(probe_op, build_op);
                        let (data, probe) = if common_vars.is_empty() {
                            (
                                JoinProbeData::Cartesian { build_rows },
                                Box::new(build_stream(probe_op)),
                            )
                        } else {
                            let sip = build_sip(&common_vars, &build_rows);
                            let probe = if sip.is_empty() || null_vars.is_some() {
                                Box::new(build_stream(probe_op))
                            } else {
                                Box::new(build_stream_with_sip(probe_op, &sip))
                            };
                            let hash_table = build_hash_table(&common_vars, build_rows);
                            (
                                JoinProbeData::Equi {
                                    common_vars,
                                    hash_table,
                                },
                                probe,
                            )
                        };
                        state.insert(Box::new(HashJoinState { data, probe }))
                    }
                };
                let probe_batch = st.probe.next_batch(graph, params, schema)?;
                if probe_batch.is_empty() {
                    return Ok(vec![]);
                }
                let rows = hash_join_rows(probe_batch, &st.data, null_vars.as_deref());
                buf.extend(rows);
            },
            RowStream::MultiwayJoin {
                input,
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                let rows = multiway_join_rows(
                    graph,
                    batch,
                    closing_src_var,
                    closing_dst_var,
                    closing_rel_type.as_deref(),
                    closing_rel_var,
                    *closing_is_incoming,
                    *closing_is_undirected,
                    closing_unique_rels,
                )?;
                buf.extend(rows);
            },
            RowStream::ExpandChain { base, hops, buf } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = base.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                // Rebuild the borrowed hop slice the fused-chain helper expects.
                let borrowed: Vec<ChainHop<'_>> = hops
                    .iter()
                    .map(|h| ChainHop {
                        src_var: &h.src_var,
                        rel_var: &h.rel_var,
                        dst_var: &h.dst_var,
                        rel_type: h.rel_type.as_deref(),
                        is_incoming: h.is_incoming,
                        unique_rels: &h.unique_rels,
                    })
                    .collect();
                let expanded = execute_expand_chain_n(graph, batch, &borrowed)?;
                buf.extend(expanded);
            },
            RowStream::FilterOverExpand {
                base,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                unique_rels,
                expression,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = base.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                // The factorized filter may drop a whole batch; the outer loop
                // refills until rows survive or the base is exhausted.
                let rows = filter_over_expand_batch(
                    graph,
                    batch,
                    src_var,
                    rel_var,
                    dst_var,
                    rel_type.as_deref(),
                    *is_incoming,
                    unique_rels,
                    expression,
                    params,
                )?;
                buf.extend(rows);
            },
            RowStream::Sort {
                input,
                items,
                bound,
                out,
            } => {
                let iter = match out {
                    Some(it) => it,
                    None => {
                        let mut rows = Vec::new();
                        loop {
                            let batch = input.next_batch(graph, params, schema)?;
                            if batch.is_empty() {
                                break;
                            }
                            rows.extend(batch);
                        }
                        let sorted = sort_all(graph, rows, items, *bound, params);
                        out.insert(sorted.into_iter())
                    }
                };
                Ok(iter.by_ref().take(STREAM_BATCH).collect())
            }
            RowStream::Distinct { input, keys, seen } => loop {
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                let kept: Vec<SlotRow> = batch
                    .into_iter()
                    .filter(|path| {
                        let key = match keys {
                            // Keyed dedup: only the selected bindings (the
                            // RETURN DISTINCT projection) form the key.
                            Some(keys) => keys
                                .iter()
                                .map(|k| format!("{:?}", path.get_binding(k)))
                                .collect::<Vec<_>>()
                                .join("|"),
                            // Full-row dedup: `bound_entries` iterates slots in
                            // schema order then locals, so the key is
                            // deterministic.
                            None => path
                                .bound_entries()
                                .map(|(k, v)| format!("{}={:?}", k, v))
                                .collect::<Vec<_>>()
                                .join("|"),
                        };
                        seen.insert(key)
                    })
                    .collect();
                if !kept.is_empty() {
                    return Ok(kept);
                }
            },
            RowStream::Aggregate {
                input,
                group_by,
                aggregations,
                out,
            } => {
                let iter = match out {
                    Some(it) => it,
                    None => {
                        let rows =
                            aggregate_all(graph, input, group_by, aggregations, params, schema)?;
                        out.insert(rows.into_iter())
                    }
                };
                Ok(iter.by_ref().take(STREAM_BATCH).collect())
            }
            RowStream::OptionalMatch {
                input,
                null_vars,
                any,
                done,
            } => {
                let batch = input.next_batch(graph, params, schema)?;
                if !batch.is_empty() {
                    *any = true;
                    return Ok(batch);
                }
                // Input exhausted: emit one null-filled row only if the entire
                // input was empty, and only once.
                if !*any && !*done {
                    *done = true;
                    let mut null_row = SlotRow::empty(schema.clone());
                    for var in null_vars.iter() {
                        null_row.bind_local(var, GraphBinding::Scalar(serde_json::Value::Null));
                    }
                    return Ok(vec![null_row]);
                }
                Ok(vec![])
            }
            RowStream::Unwind {
                input,
                expr,
                variable,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                for path in batch {
                    let list_val = evaluate_expr(graph, &path, expr, params)?;
                    if let serde_json::Value::Array(elems) = list_val {
                        for item in elems {
                            let mut new_path = path.clone();
                            new_path.bind_local(variable, GraphBinding::Scalar(item));
                            buf.push_back(new_path);
                        }
                    } else if list_val != serde_json::Value::Null {
                        let mut new_path = path.clone();
                        new_path.bind_local(variable, GraphBinding::Scalar(list_val));
                        buf.push_back(new_path);
                    }
                }
            },
            RowStream::ProcedureCall {
                input,
                output_vars,
                rows,
                buf,
            } => loop {
                if !buf.is_empty() {
                    let take = buf.len().min(STREAM_BATCH);
                    return Ok(buf.drain(..take).collect());
                }
                let batch = input.next_batch(graph, params, schema)?;
                if batch.is_empty() {
                    return Ok(vec![]);
                }
                for path in batch {
                    for row in rows.iter() {
                        let mut new_path = path.clone();
                        for (var, value) in output_vars.iter().zip(row.iter()) {
                            new_path.bind_local(var, GraphBinding::Scalar(value.clone()));
                        }
                        buf.push_back(new_path);
                    }
                }
            },
            RowStream::Limit {
                input,
                skip,
                count,
                skipped,
                emitted,
                validated,
            } => {
                if !*validated {
                    if *skip > 1_000_000_000 {
                        return Err(format!("SKIP value too large: {}", skip));
                    }
                    *validated = true;
                }
                loop {
                    if *emitted >= *count {
                        return Ok(vec![]);
                    }
                    let batch = input.next_batch(graph, params, schema)?;
                    if batch.is_empty() {
                        return Ok(vec![]);
                    }
                    let mut out = Vec::new();
                    for path in batch {
                        if *skipped < *skip {
                            *skipped += 1;
                            continue;
                        }
                        if *emitted >= *count {
                            break;
                        }
                        out.push(path);
                        *emitted += 1;
                    }
                    if !out.is_empty() {
                        return Ok(out);
                    }
                    // The whole batch was skipped; pull more.
                }
            }
            RowStream::WritePart { input, part, out } => {
                let iter = match out {
                    Some(it) => it,
                    None => {
                        let mut rows = Vec::new();
                        loop {
                            let batch = input.next_batch(graph, params, schema)?;
                            if batch.is_empty() {
                                break;
                            }
                            rows.extend(batch);
                        }
                        let result = write_part_rows(graph, rows, part, params)?;
                        out.insert(result.into_iter())
                    }
                };
                Ok(iter.by_ref().take(STREAM_BATCH).collect())
            }
        }
    }
}

/// Group and aggregate every row drained from `input`, returning one row per
/// group with the aggregate columns bound. The fold is associative and
/// order-independent, so a streamed (batch-at-a-time) input and a fully
/// materialized one produce identical groups; output rows are emitted in
/// `BTreeMap` key order. Shared by the materializing `Aggregate` arm and the
/// streaming `RowStream::Aggregate` node.
/// Per-group accumulator for one aggregation function. The fold and finalize
/// steps are shared by the row-at-a-time `aggregate_all` and the columnar
/// vectorized aggregate, so both produce identical values.
pub(super) struct AggState {
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
    pub(super) fn new() -> Self {
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

    /// Fold one `count(*)` row (no input expression to evaluate).
    pub(super) fn fold_count_star(&mut self) {
        self.count += 1;
    }

    /// Fold one already-evaluated input value, applying openCypher null and
    /// DISTINCT semantics for `agg_fn`.
    pub(super) fn fold(&mut self, agg_fn: &AggFn, val: serde_json::Value) {
        match agg_fn {
            AggFn::Count { distinct } => {
                if val != serde_json::Value::Null {
                    if *distinct {
                        if self.distinct_seen.insert(val.to_string()) {
                            self.count += 1;
                        }
                    } else {
                        self.count += 1;
                    }
                }
            }
            AggFn::Sum { distinct } => {
                if val != serde_json::Value::Null {
                    if *distinct && !self.distinct_seen.insert(val.to_string()) {
                        // already seen, skip
                    } else if let Some(n) = val.as_f64() {
                        if val.as_i64().is_none() {
                            self.sum_is_float = true;
                        }
                        self.sum += n;
                    }
                }
            }
            AggFn::Avg { distinct } => {
                if val != serde_json::Value::Null {
                    if *distinct && !self.distinct_seen.insert(val.to_string()) {
                        // already seen, skip
                    } else if let Some(n) = val.as_f64() {
                        self.sum += n;
                        self.count += 1;
                    }
                }
            }
            AggFn::Min { .. } => {
                if val != serde_json::Value::Null {
                    self.min = Some(match self.min.take() {
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
                if val != serde_json::Value::Null {
                    self.max = Some(match self.max.take() {
                        None => val,
                        Some(prev) => {
                            if json_cmp_total(&val, &prev) == std::cmp::Ordering::Greater {
                                val
                            } else {
                                prev
                            }
                        }
                    });
                }
            }
            AggFn::Collect { distinct } => {
                if val != serde_json::Value::Null {
                    if *distinct && !self.distinct_seen.insert(val.to_string()) {
                        // already seen, skip
                    } else {
                        self.collect.push(val);
                    }
                }
            }
            AggFn::StDev { .. } | AggFn::StDevP { .. } => {
                if let Some(n) = val.as_f64() {
                    self.values.push(n);
                }
            }
            AggFn::PercentileDisc { .. } | AggFn::PercentileCont { .. } => {
                if let Some(n) = val.as_f64() {
                    self.values.push(n);
                }
            }
        }
    }

    /// Produce the final aggregate value for `agg_fn` from this state.
    pub(super) fn finalize(
        &self,
        graph: &Graph,
        agg_fn: &AggFn,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let state = self;
        Ok(match agg_fn {
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
                    let variance = state.values.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
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
                        state.values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
                    serde_json::Number::from_f64(variance.sqrt())
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null)
                }
            }
            AggFn::PercentileDisc { percentile } => {
                let percentile = evaluate_expr(graph, &PathMap::new(), percentile, params)?
                    .as_f64()
                    .ok_or("percentileDisc(): percentile must be a number")?;
                if !(0.0..=1.0).contains(&percentile) {
                    return Err(
                        "ArgumentError(NumberOutOfRange): percentile must be in [0.0, 1.0]"
                            .to_string(),
                    );
                }
                let mut sorted = state.values.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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
                let percentile = evaluate_expr(graph, &PathMap::new(), percentile, params)?
                    .as_f64()
                    .ok_or("percentileCont(): percentile must be a number")?;
                if !(0.0..=1.0).contains(&percentile) {
                    return Err(
                        "ArgumentError(NumberOutOfRange): percentile must be in [0.0, 1.0]"
                            .to_string(),
                    );
                }
                let mut sorted = state.values.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let n = sorted.len();
                if n == 0 {
                    serde_json::Value::Null
                } else {
                    let rank = percentile * (n - 1) as f64;
                    let lower = rank.floor() as usize;
                    let upper = rank.ceil() as usize;
                    let frac = rank - lower as f64;
                    let val = sorted[lower] + frac * (sorted[upper.min(n - 1)] - sorted[lower]);
                    serde_json::Number::from_f64(val)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null)
                }
            }
        })
    }
}

/// The output column name of a group-by item: its alias when present,
/// otherwise the canonical display form of the expression. Shared by the
/// row-at-a-time and vectorized aggregate folds so group rows bind under
/// identical keys.
pub(super) fn group_by_column_name(expr: &Expr, alias: &Option<String>) -> String {
    if let Some(a) = alias {
        a.clone()
    } else {
        match expr {
            Expr::Prop(var, prop) if prop.is_empty() => var.clone(),
            Expr::Prop(var, prop) => format!("{}.{}", var, prop),
            Expr::Literal(lit) => lit.to_string(),
            Expr::Param(p) => format!("${}", p),
            other => expr_display_name(other),
        }
    }
}

fn aggregate_all(
    graph: &Graph,
    input: &mut RowStream,
    group_by: &[(Expr, Option<String>)],
    aggregations: &[(AggFn, Expr, String)],
    params: &HashMap<String, serde_json::Value>,
    schema: &std::sync::Arc<SlotSchema>,
) -> Result<Vec<SlotRow>, String> {
    use std::collections::BTreeMap;

    // group_key -> (group-by row, per-aggregation state Vec)
    let mut groups: BTreeMap<String, (SlotRow, Vec<AggState>)> = BTreeMap::new();
    if group_by.is_empty() {
        let states = aggregations.iter().map(|_| AggState::new()).collect();
        groups.insert("".to_string(), (SlotRow::empty(schema.clone()), states));
    }

    // Fold one input row into the group table.
    let fold_path = |groups: &mut BTreeMap<String, (SlotRow, Vec<AggState>)>,
                     path: SlotRow|
     -> Result<(), String> {
        let mut key_parts = Vec::new();
        let mut gb_path = SlotRow::empty(path.schema_arc());
        for (expr, alias) in group_by {
            let col = group_by_column_name(expr, alias);
            // Grouping by a bare node or edge variable (`Prop(var, "")`) keys on
            // its identity. Evaluating it would materialize the whole element to
            // a JSON object and stringifying the key would serialize the entire
            // property bag for every input row; the id is the identity, and
            // keeping the `Node`/`Edge` binding lets the downstream projection
            // read properties through the columnar fast path rather than a
            // pre-built object.
            if let Expr::Prop(var, prop) = expr {
                if prop.is_empty() {
                    match path.get_binding(var.as_str()) {
                        Some(GraphBinding::Node(id)) => {
                            key_parts.push(format!("\x01N{}", id));
                            gb_path.bind_local(&col, GraphBinding::Node(*id));
                            continue;
                        }
                        Some(GraphBinding::Edge(id)) => {
                            key_parts.push(format!("\x01E{}", id));
                            gb_path.bind_local(&col, GraphBinding::Edge(*id));
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            let val = evaluate_expr(graph, &path, expr, params)?;
            key_parts.push(val.to_string());
            gb_path.bind_local(&col, GraphBinding::Scalar(val));
        }
        let group_key = key_parts.join("\x00");

        let entry = groups.entry(group_key).or_insert_with(|| {
            let states = aggregations.iter().map(|_| AggState::new()).collect();
            (gb_path, states)
        });

        for (i, (agg_fn, inner_expr, _col)) in aggregations.iter().enumerate() {
            let state = &mut entry.1[i];
            if matches!(agg_fn, AggFn::Count { .. }) && matches!(inner_expr, Expr::CountStar) {
                state.fold_count_star();
            } else {
                let val = evaluate_expr(graph, &path, inner_expr, params)?;
                state.fold(agg_fn, val);
            }
        }
        Ok(())
    };

    // Drain the input stream a batch at a time so peak memory is one batch plus
    // the group table.
    loop {
        let batch = input.next_batch(graph, params, schema)?;
        if batch.is_empty() {
            break;
        }
        for path in batch {
            fold_path(&mut groups, path)?;
        }
    }

    let mut result = Vec::new();
    for (_key, (mut gb_path, states)) in groups {
        for (i, (agg_fn, _inner, col)) in aggregations.iter().enumerate() {
            let state = &states[i];
            let agg_val = state.finalize(graph, agg_fn, params)?;
            gb_path.bind_local(col, GraphBinding::Scalar(agg_val));
        }
        result.push(gb_path);
    }

    Ok(result)
}

/// Sort `child_paths` by `items`. With `bound = None` this is a full stable sort
/// (matching the materializing `Sort` arm). With `bound = Some(k)` it keeps only
/// the first `k` rows a stable sort would yield, trimming a bounded buffer as it
/// fills (the `Sort -> Limit` top-N optimization): the input-index tiebreak makes
/// the trimmed set byte-identical to sort-then-truncate while bounding memory.
pub(super) fn sort_all(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    items: &[crate::ast::SortItem],
    bound: Option<usize>,
    params: &HashMap<String, serde_json::Value>,
) -> Vec<SlotRow> {
    // Primary comparison by the sort keys, honoring per-key ASC/DESC.
    let cmp = |ka: &[serde_json::Value], kb: &[serde_json::Value]| -> std::cmp::Ordering {
        for (i, si) in items.iter().enumerate() {
            let ord = json_cmp(&ka[i], &kb[i]).unwrap_or_else(|| json_cmp_total(&ka[i], &kb[i]));
            let ord = if si.ascending { ord } else { ord.reverse() };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    };
    let keys_of = |path: &SlotRow| -> Vec<serde_json::Value> {
        items
            .iter()
            .map(|si| evaluate_sort_key(graph, path, &si.expr, params))
            .collect()
    };

    match bound {
        None => {
            let mut keyed: Vec<(Vec<serde_json::Value>, SlotRow)> = child_paths
                .into_iter()
                .map(|path| {
                    let keys = keys_of(&path);
                    (keys, path)
                })
                .collect();
            keyed.sort_by(|(ka, _), (kb, _)| cmp(ka, kb));
            keyed.into_iter().map(|(_, path)| path).collect()
        }
        Some(0) => Vec::new(),
        Some(k) => {
            // Total order with the input index as a final tiebreak, so trimming
            // keeps exactly the rows a stable sort would put first.
            let order = |a: &(Vec<serde_json::Value>, usize, SlotRow),
                         b: &(Vec<serde_json::Value>, usize, SlotRow)| {
                cmp(&a.0, &b.0).then(a.1.cmp(&b.1))
            };
            let mut buf: Vec<(Vec<serde_json::Value>, usize, SlotRow)> = Vec::new();
            for (idx, path) in child_paths.into_iter().enumerate() {
                let keys = keys_of(&path);
                buf.push((keys, idx, path));
                // Saturating: a SKIP without a LIMIT saturates `k` to
                // `usize::MAX`, and `2 * k` would overflow in debug builds.
                if buf.len() >= k.saturating_mul(2) {
                    buf.sort_by(&order);
                    buf.truncate(k);
                }
            }
            buf.sort_by(&order);
            buf.truncate(k);
            buf.into_iter().map(|(_, _, path)| path).collect()
        }
    }
}

/// Apply a write clause (`CREATE`, `MERGE`, `SET`, `DELETE`, `REMOVE`) over a
/// fully materialized batch of input rows, returning the downstream rows. This is
/// blocking by design: `DELETE` operates over the whole result at once, and a
/// trailing `LIMIT` must not skip writes, so the caller drains the input fully
/// before running this (matching the former materializing `WritePart` arm). The
/// graph write lock is held for the whole statement by `execute_read_query`.
fn write_part_rows(
    graph: &Graph,
    child_paths: Vec<SlotRow>,
    part: &crate::ast::QueryPart,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<SlotRow>, String> {
    use super::write::execute_create_internal_with_context;
    use super::write::execute_merge_internal_with_context;
    use crate::ast::QueryPart;

    // The write executors in write.rs are still keyed by name; rows bridge
    // through `to_path_map` at this boundary, and the names a CREATE or MERGE
    // binds come back through `bind_local` (pattern variables have slots from
    // the plan walk, so they re-enter positionally).

    // DELETE is evaluated over the whole result at once: all listed
    // relationships are removed before any node, so an undirected expand that
    // binds the same edge in more than one row still succeeds. The rows pass
    // through unchanged for a following RETURN.
    if let QueryPart::Delete { targets, detach } = part {
        use super::write::delete_over_paths;
        let maps: Vec<PathMap> = child_paths.iter().map(|p| p.to_path_map()).collect();
        delete_over_paths(graph, &maps, targets, *detach, params)?;
        return Ok(child_paths);
    }

    let mut result_paths = Vec::new();

    for path in child_paths {
        match part {
            QueryPart::Create { patterns } => {
                let path_map = path.to_path_map();
                let mut new_path = path.clone();
                for pattern in patterns {
                    let created =
                        execute_create_internal_with_context(graph, pattern, &path_map, params)?;
                    for (name, binding) in created {
                        new_path.bind_local(&name, binding);
                    }
                }
                result_paths.push(new_path);
            }
            QueryPart::Merge { merges } => {
                // Each MERGE in the clause extends every current row, and may fan
                // out to multiple rows when it matches more than one existing
                // pattern.
                let mut current = vec![path.clone()];
                for merge_stmt in merges {
                    let mut next = Vec::new();
                    for p in &current {
                        let extensions = execute_merge_internal_with_context(
                            graph,
                            merge_stmt,
                            &p.to_path_map(),
                            params,
                        )?;
                        for ext in extensions {
                            let mut row = p.clone();
                            for (name, binding) in ext {
                                row.bind_local(&name, binding);
                            }
                            next.push(row);
                        }
                    }
                    current = next;
                }
                result_paths.extend(current);
            }
            QueryPart::Set { items } => {
                use super::write::apply_set_items;
                apply_set_items(graph, &path.to_path_map(), items, params)?;
                result_paths.push(path);
            }
            QueryPart::Delete { targets, detach } => {
                use super::write::apply_delete_targets;
                apply_delete_targets(graph, &path.to_path_map(), targets, *detach, params)?;
                result_paths.push(path);
            }
            QueryPart::Remove { items } => {
                use super::write::apply_remove_item;
                let path_map = path.to_path_map();
                for item in items {
                    apply_remove_item(graph, item, &path_map)?;
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

/// Resolve the verify value of `property` for each index-scan candidate.
///
/// The index encoding is not exact (string keys are NUL-terminated and range
/// bounds compare encoded bytes), so every candidate's actual value must be
/// re-checked. The in-memory property columns answer that in bulk without
/// decoding node records; a missing property comes back as `Value::Null`,
/// which no equality or range check matches, same as the per-record skip. If
/// the bulk gather fails (a candidate deleted between the index read and the
/// columns refresh), fall back to per-record reads, which skip such nodes.
fn index_verify_values(
    graph: &Graph,
    candidates: &[NodeId],
    property: &str,
) -> Vec<(NodeId, serde_json::Value)> {
    match graph.node_prop_json_column(candidates, property) {
        Ok(col) => candidates.iter().copied().zip(col).collect(),
        Err(_) => candidates
            .iter()
            .filter_map(|&cand| {
                let record = graph.get_node(cand).ok()??;
                let props = rmp_serde::from_slice::<serde_json::Value>(&record.props).ok()?;
                Some((cand, props.get(property).cloned()?))
            })
            .collect(),
    }
}

/// Resolve one pushed-down per-vertex `PathCount` predicate (`property CMP
/// literal` on a labeled node) to the set of node ids that satisfy it. Mirrors
/// the `NodeIndexScan` / `NodeRangeScan` executors: the index gives candidates,
/// then each candidate's stored value is re-checked against the predicate so an
/// inexact index encoding never admits a wrong node. A null comparison value
/// matches nothing, the same as those executors.
fn resolve_path_count_pred(
    graph: &Graph,
    label: &str,
    pred: &crate::plan::physical::VertexPred,
    params: &HashMap<String, serde_json::Value>,
) -> Result<std::collections::HashSet<NodeId>, String> {
    use crate::plan::physical::VertexCmp;
    use std::cmp::Ordering;

    let val = evaluate_expr(graph, &PathMap::new(), &pred.value, params)?;
    if val.is_null() {
        return Ok(std::collections::HashSet::new());
    }
    let prop_val = json_to_prop_value(&val)
        .ok_or_else(|| format!("unsupported property value type for path-count filter: {val}"))?;
    let property = pred.property.as_str();
    let candidates = match pred.cmp {
        VertexCmp::Eq => graph.nodes_by_property(label, property, prop_val),
        VertexCmp::Lt => {
            graph.nodes_by_property_range(label, property, None, false, Some(prop_val), false)
        }
        VertexCmp::Le => {
            graph.nodes_by_property_range(label, property, None, false, Some(prop_val), true)
        }
        VertexCmp::Gt => {
            graph.nodes_by_property_range(label, property, Some(prop_val), false, None, false)
        }
        VertexCmp::Ge => {
            graph.nodes_by_property_range(label, property, Some(prop_val), true, None, false)
        }
    }
    .map_err(|e| e.to_string())?;

    let mut out = std::collections::HashSet::new();
    for (cand, actual) in index_verify_values(graph, &candidates, property) {
        let keep = match pred.cmp {
            VertexCmp::Eq => json_vals_are_equal(&actual, &val),
            VertexCmp::Lt => json_val_cmp(&actual, &val) == Some(Ordering::Less),
            VertexCmp::Le => {
                matches!(
                    json_val_cmp(&actual, &val),
                    Some(Ordering::Less | Ordering::Equal)
                )
            }
            VertexCmp::Gt => json_val_cmp(&actual, &val) == Some(Ordering::Greater),
            VertexCmp::Ge => {
                matches!(
                    json_val_cmp(&actual, &val),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            }
        };
        if keep {
            out.insert(cand);
        }
    }
    Ok(out)
}

/// Directly evaluate a streamable leaf operator that is not a `LabelScan`
/// (`NodeByIdSeek`, `NodeIndexScan`, `NodeRangeScan`, or `SingleRow`). These are
/// O(1) or index-bounded, so `RowStream::Materialized` evaluates them once on
/// the first pull and drains the result. Kept separate from the `execute_physical`
/// sink so the sink/leaf relationship cannot recurse.
pub(super) fn eval_leaf(
    graph: &Graph,
    op: &PhysicalOperator,
    params: &HashMap<String, serde_json::Value>,
    schema: &std::sync::Arc<SlotSchema>,
) -> Result<Vec<SlotRow>, String> {
    match op {
        PhysicalOperator::NodeIndexScan {
            variable,
            label,
            property,
            value,
        } => {
            let val = evaluate_expr(graph, &PathMap::new(), value, params)?;
            // A null lookup value (a parameter resolving to null) matches
            // nothing: `prop = null` is never TRUE.
            if val.is_null() {
                return Ok(vec![]);
            }
            let prop_val = json_to_prop_value(&val)
                .ok_or_else(|| format!("unsupported property value type for index scan: {val}"))?;
            let candidates = graph
                .nodes_by_property(label, property, prop_val)
                .map_err(|e| e.to_string())?;

            let mut filtered = Vec::new();
            for (cand, actual_val) in index_verify_values(graph, &candidates, property) {
                if json_vals_are_equal(&actual_val, &val) {
                    let mut path = SlotRow::empty(schema.clone());
                    path.bind_local(variable, GraphBinding::Node(cand));
                    filtered.push(path);
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
            // A null bound (a parameter resolving to null) matches nothing:
            // an ordered comparison with null is never TRUE.
            if lo_val.as_ref().is_some_and(|v| v.is_null())
                || hi_val.as_ref().is_some_and(|v| v.is_null())
            {
                return Ok(vec![]);
            }

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
            for (cand, actual_val) in index_verify_values(graph, &candidates, property) {
                let mut ok = true;
                if let Some(ref l_val) = lo_val {
                    match json_val_cmp(&actual_val, l_val) {
                        Some(std::cmp::Ordering::Greater) => {}
                        Some(std::cmp::Ordering::Equal) if *lo_inclusive => {}
                        _ => ok = false,
                    }
                }
                if ok {
                    if let Some(ref h_val) = hi_val {
                        match json_val_cmp(&actual_val, h_val) {
                            Some(std::cmp::Ordering::Less) => {}
                            Some(std::cmp::Ordering::Equal) if *hi_inclusive => {}
                            _ => ok = false,
                        }
                    }
                }
                if ok {
                    let mut path = SlotRow::empty(schema.clone());
                    path.bind_local(variable, GraphBinding::Node(cand));
                    filtered.push(path);
                }
            }
            Ok(filtered)
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
                let mut path = SlotRow::empty(schema.clone());
                path.bind_local(variable, GraphBinding::Node(nid));
                Ok(vec![path])
            } else {
                Ok(vec![])
            }
        }
        PhysicalOperator::SingleRow => Ok(vec![SlotRow::empty(schema.clone())]),
        PhysicalOperator::TriangleCount {
            rel_types,
            labels,
            output,
        } => {
            // One kernel call over the CSR snapshot replaces the whole
            // expand-and-close pipeline; the count comes back as one row.
            let spec = issundb_core::TriangleCountSpec {
                rel_types: [
                    rel_types[0].as_deref(),
                    rel_types[1].as_deref(),
                    rel_types[2].as_deref(),
                ],
                labels: [
                    labels[0].as_deref(),
                    labels[1].as_deref(),
                    labels[2].as_deref(),
                ],
            };
            let count = graph
                .count_triangle_cycles(&spec)
                .map_err(|e| e.to_string())?;
            let mut path = SlotRow::empty(schema.clone());
            path.bind_local(output, GraphBinding::Scalar(serde_json::Value::from(count)));
            Ok(vec![path])
        }
        PhysicalOperator::PathCount {
            rel_types,
            labels,
            vertex_filters,
            output,
        } => {
            // Resolve each variable's pushed-down property predicates to an
            // allow-set of node ids via the property index, then hand the sets to
            // the kernel as per-variable masks. The recognizer guarantees a
            // filtered variable carries a label (the index is label-scoped), so
            // the label lookup below is always present when predicates exist.
            let mut vertex_allow: Vec<Option<Vec<NodeId>>> = Vec::with_capacity(labels.len());
            for (i, preds) in vertex_filters.iter().enumerate() {
                if preds.is_empty() {
                    vertex_allow.push(None);
                    continue;
                }
                let label = labels[i]
                    .as_deref()
                    .ok_or("path-count vertex filter requires a label")?;
                // Intersect the verified candidate set across this variable's
                // predicates (each conjunct narrows the allow-set).
                let mut allow: Option<std::collections::HashSet<NodeId>> = None;
                for pred in preds {
                    let resolved = resolve_path_count_pred(graph, label, pred, params)?;
                    allow = Some(match allow {
                        None => resolved,
                        Some(acc) => acc.intersection(&resolved).copied().collect(),
                    });
                }
                vertex_allow.push(Some(allow.unwrap_or_default().into_iter().collect()));
            }

            // One kernel call over the CSR snapshot replaces the row-pipeline
            // expansion; the count of matching paths comes back as one row.
            let spec = issundb_core::PathCountSpec {
                rel_types: rel_types.iter().map(|t| t.as_deref()).collect(),
                labels: labels.iter().map(|l| l.as_deref()).collect(),
                vertex_allow,
            };
            let count = graph.count_linear_paths(&spec).map_err(|e| e.to_string())?;
            let mut path = SlotRow::empty(schema.clone());
            path.bind_local(output, GraphBinding::Scalar(serde_json::Value::from(count)));
            Ok(vec![path])
        }
        PhysicalOperator::GroupedDegree {
            rel_type,
            group_is_dst,
            group_label,
            counted_label,
            counted_nonnull_prop,
            // `group_var` names the group endpoint for the plan display; the
            // executor reads the group properties by name, so it is not used here.
            group_var: _,
            group_by,
            output,
        } => {
            // One kernel pass over adjacency yields the per-group-node counts;
            // it groups by node identity, so re-group by the group-by value
            // tuple to merge nodes that share a key (the row pipeline groups by
            // value, not identity). The merge is over the distinct-group-node
            // set, far smaller than the edge set the fold would touch.
            let spec = issundb_core::GroupedDegreeSpec {
                rel_type: rel_type.as_deref(),
                group_is_dst: *group_is_dst,
                group_label: group_label.as_deref(),
                counted_label: counted_label.as_deref(),
                counted_nonnull_prop: counted_nonnull_prop.as_deref(),
            };
            let pairs = graph
                .grouped_edge_counts(&spec)
                .map_err(|e| e.to_string())?;
            // Bulk-gather the group-by properties for the group nodes in one
            // columns pass (the recognizer guarantees each key is a
            // single-property read on `group_var`), rather than re-reading each
            // node per key. `col_names[j]`/`props[j]` align with `group_by[j]`.
            let ids: Vec<NodeId> = pairs.iter().map(|(n, _)| *n).collect();
            let mut props: Vec<&str> = Vec::with_capacity(group_by.len());
            let mut col_names: Vec<String> = Vec::with_capacity(group_by.len());
            for (expr, alias) in group_by {
                match expr {
                    Expr::Prop(_, p) if !p.is_empty() => props.push(p.as_str()),
                    // The recognizer admits only single-property reads here.
                    _ => return Err("grouped-degree group key must be a property read".into()),
                }
                col_names.push(group_by_column_name(expr, alias));
            }
            let table = graph
                .node_props_json_table(&ids, &props)
                .map_err(|e| e.to_string())?;

            // Re-group by the group-by value tuple, summing counts, so nodes
            // that share a key merge exactly as the value-keyed row pipeline
            // aggregate would (the kernel groups by node identity).
            let mut groups: ahash::AHashMap<String, (SlotRow, u64)> =
                ahash::AHashMap::with_capacity(pairs.len());
            for (i, (_node, cnt)) in pairs.iter().enumerate() {
                let row = &table[i];
                let mut key_parts = Vec::with_capacity(col_names.len());
                for val in row {
                    key_parts.push(val.to_string());
                }
                let key = key_parts.join("\x00");
                match groups.get_mut(&key) {
                    Some((_, c)) => *c += *cnt,
                    None => {
                        let mut gb = SlotRow::empty(schema.clone());
                        for (col, val) in col_names.iter().zip(row) {
                            gb.bind_local(col, GraphBinding::Scalar(val.clone()));
                        }
                        groups.insert(key, (gb, *cnt));
                    }
                }
            }
            let mut out = Vec::with_capacity(groups.len());
            for (_key, (mut gb, cnt)) in groups {
                gb.bind_local(output, GraphBinding::Scalar(serde_json::Value::from(cnt)));
                out.push(gb);
            }
            Ok(out)
        }
        PhysicalOperator::VectorTopK {
            variable,
            label,
            query,
            k,
            prop_filters,
        } => {
            // Resolve the query vector and any pushed-down equality filters once,
            // with no row context: the recognizer guarantees these expressions
            // reference no bound variable.
            let q = match resolve_query_vector(graph, query, params)? {
                Some(v) => v,
                // A null query vector (a null parameter) matches nothing.
                None => return Ok(vec![]),
            };
            let properties = if prop_filters.is_empty() {
                None
            } else {
                let mut m = std::collections::HashMap::with_capacity(prop_filters.len());
                for (prop, value_expr) in prop_filters {
                    let v = evaluate_expr(graph, &PathMap::new(), value_expr, params)?;
                    m.insert(prop.clone(), v);
                }
                Some(m)
            };
            let opts = issundb_vector::VectorSearchOptions {
                k: *k,
                label: label.clone(),
                properties,
                rescore_factor: None,
            };
            let hits = graph
                .vector_search_with(&q, &opts)
                .map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(hits.len());
            for hit in hits {
                let mut path = SlotRow::empty(schema.clone());
                path.bind_local(variable, GraphBinding::Node(hit.node));
                out.push(path);
            }
            Ok(out)
        }
        other => Err(format!("eval_leaf called on non-leaf operator: {other:?}")),
    }
}

/// Resolve a `VectorTopK` query expression to an `f32` vector with no row
/// context. Returns `None` when the expression evaluates to null (a null
/// parameter). Errors when it is not a numeric vector.
fn resolve_query_vector(
    graph: &Graph,
    query: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Option<Vec<f32>>, String> {
    match evaluate_expr(graph, &PathMap::new(), query, params)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(items) => {
            let mut v = Vec::with_capacity(items.len());
            for item in items {
                let f = item
                    .as_f64()
                    .ok_or_else(|| "vector query elements must be numbers".to_string())?;
                v.push(f as f32);
            }
            Ok(Some(v))
        }
        _ => Err("vector query must be a numeric vector".into()),
    }
}

/// Execute a physical operator tree, fully materializing the result rows.
///
/// This is the single execution path: it builds the pull-based `RowStream` for
/// `op` and drains it into a `Vec`. Every operator is a `RowStream` variant, so
/// there is no separate materializing executor; callers that need the whole
/// result (the top-level projection, the write binding plans) drain here, while
/// a `Limit` inside the tree still short-circuits its streamable input.
pub(super) fn execute_physical(
    graph: &Graph,
    op: &PhysicalOperator,
    params: &HashMap<String, serde_json::Value>,
    schema: &std::sync::Arc<SlotSchema>,
) -> Result<Vec<SlotRow>, String> {
    let mut stream = build_stream(op);
    let mut out = Vec::new();
    loop {
        let batch = stream.next_batch(graph, params, schema)?;
        if batch.is_empty() {
            break;
        }
        out.extend(batch);
    }
    Ok(out)
}

/// `execute_physical` for callers that still consume name-keyed rows (the
/// write-path binding plans in write.rs). Builds the slot schema for `op`,
/// executes, and bridges each row back to a `PathMap`.
pub(super) fn execute_physical_pathmaps(
    graph: &Graph,
    op: &PhysicalOperator,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<PathMap>, String> {
    let schema = std::sync::Arc::new(SlotSchema::from_plan(op));
    Ok(execute_physical(graph, op, params, &schema)?
        .iter()
        .map(|r| r.to_path_map())
        .collect())
}

pub(super) fn evaluate_where<B: Bindings>(
    graph: &Graph,
    path: &B,
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
pub(super) fn evaluate_sort_key<B: Bindings>(
    graph: &Graph,
    path: &B,
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
    if let Some(GraphBinding::Scalar(v)) = path.get_binding(&col_name) {
        return v.clone();
    }
    // Try just the property name as a fallback alias (e.g., `n.age` stored as `"age"`).
    if let Expr::Prop(_, prop) = expr {
        if !prop.is_empty() {
            if let Some(GraphBinding::Scalar(v)) = path.get_binding(prop) {
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

#[cfg(test)]
mod stream_join_tests {
    //! Correctness guards for streaming `LIMIT` through `HashJoin` and
    //! `MultiwayJoin`. Each test asserts the plan is actually streamable and
    //! carries the expected join operator, then proves the streamed result
    //! equals the materializing result: streaming the whole plan reproduces the
    //! full result as a multiset, and a small `LIMIT` returns exactly that many
    //! valid result rows. These exercise the new `RowStream::HashJoin` and
    //! `RowStream::MultiwayJoin` arms directly, not just the public `execute`.
    use super::*;
    use crate::ast::Statement;
    use crate::parser;
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};
    use issundb_core::Graph;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn exec(graph: &Graph, cypher: &str) {
        super::execute(graph, cypher, &HashMap::new()).unwrap();
    }

    fn optimized_plan(graph: &Graph, cypher: &str) -> PhysicalOperator {
        let stmt = parser::parse(cypher).unwrap();
        let q = match stmt {
            Statement::Query(q) => q,
            _ => panic!("not a read query: {cypher}"),
        };
        let logical = LogicalPlanner::plan(&q).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        Optimizer::optimize(physical, Some(graph))
    }

    type Row = Vec<serde_json::Value>;

    fn run_rows(graph: &Graph, cypher: &str) -> Vec<Row> {
        super::execute(graph, cypher, &HashMap::new())
            .unwrap()
            .records
            .into_iter()
            .map(|r| r.values)
            .collect()
    }

    fn sorted_rows(rows: &[Row]) -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        v.sort();
        v
    }

    /// Every row in `sub` is a row of `full`, counting multiplicity.
    fn assert_subset(sub: &[Row], full: &[Row]) {
        let mut counts: HashMap<String, i32> = HashMap::new();
        for r in full {
            *counts.entry(serde_json::to_string(r).unwrap()).or_default() += 1;
        }
        for r in sub {
            let k = serde_json::to_string(r).unwrap();
            let c = counts
                .get_mut(&k)
                .unwrap_or_else(|| panic!("streamed row not in full result: {k}"));
            assert!(*c > 0, "streamed row appears too often vs full result: {k}");
            *c -= 1;
        }
    }

    /// The shared assertion. First proves the expected operator is in the plan
    /// (`must_contain`). Then compares user-visible results: with one execution
    /// path, a small `LIMIT` rides the short-circuit, so streaming the whole
    /// result (`LIMIT 1000000`) must reproduce the unbounded result as a multiset,
    /// and a small `LIMIT` must return exactly that many valid rows.
    fn assert_streaming_matches(graph: &Graph, cypher: &str, must_contain: &str) {
        let limit_query = format!("{cypher} LIMIT 1000000");
        let plan = optimized_plan(graph, &limit_query);
        let input = match &plan {
            PhysicalOperator::Limit { input, .. } => input.as_ref(),
            other => other,
        };
        let rendered = format_physical_plan(input, 0);
        assert!(
            rendered.contains(must_contain),
            "plan for `{cypher}` does not contain {must_contain}:\n{rendered}"
        );

        let full = run_rows(graph, cypher);
        assert!(!full.is_empty(), "fixture for `{cypher}` produced no rows");

        // Streaming the whole result reproduces the materialized result.
        let all = run_rows(graph, &limit_query);
        assert_eq!(
            sorted_rows(&all),
            sorted_rows(&full),
            "streamed-all != materialized for `{cypher}`"
        );

        // A small LIMIT returns exactly that many valid rows; a SKIP/LIMIT
        // window is a valid single row. Both ride the short-circuit path.
        if full.len() >= 2 {
            let limited = run_rows(graph, &format!("{cypher} LIMIT 2"));
            assert_eq!(limited.len(), 2, "LIMIT 2 count for `{cypher}`");
            assert_subset(&limited, &full);

            let windowed = run_rows(graph, &format!("{cypher} SKIP 1 LIMIT 1"));
            assert_eq!(windowed.len(), 1, "SKIP 1 LIMIT 1 count for `{cypher}`");
            assert_subset(&windowed, &full);
        }
    }

    #[test]
    fn streaming_inner_join_matches_materialized() {
        let (_dir, graph) = setup();
        // a1, a2, c1 all KNOW the shared target b: the two patterns join on b.
        exec(
            &graph,
            "CREATE (b:Person {name: 'b'}), (a1:Person {name: 'a1'}), \
             (a2:Person {name: 'a2'}), (c1:Person {name: 'c1'}) \
             CREATE (a1)-[:KNOWS]->(b), (a2)-[:KNOWS]->(b), (c1)-[:KNOWS]->(b)",
        );
        graph.rebuild_csr().unwrap();
        assert_streaming_matches(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             MATCH (c:Person)-[:KNOWS]->(b) RETURN a.name AS a, c.name AS c",
            "HashJoin",
        );
    }

    #[test]
    fn streaming_outer_join_preserves_null_rows() {
        let (_dir, graph) = setup();
        // a1 knows b; a2 knows nobody, so the OPTIONAL side null-fills for a2.
        exec(
            &graph,
            "CREATE (b:Person {name: 'b'}), (a1:Person {name: 'a1'}), (a2:Person {name: 'a2'}) \
             CREATE (a1)-[:KNOWS]->(b)",
        );
        graph.rebuild_csr().unwrap();
        assert_streaming_matches(
            &graph,
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
             RETURN a.name AS a, b.name AS b",
            "HashJoin",
        );
    }

    #[test]
    fn streaming_cartesian_join_matches_materialized() {
        let (_dir, graph) = setup();
        // Two patterns with no shared variable: a cross product.
        exec(
            &graph,
            "CREATE (:Person {name: 'p1'}), (:Person {name: 'p2'}), \
             (:City {name: 'c1'}), (:City {name: 'c2'})",
        );
        graph.rebuild_csr().unwrap();
        assert_streaming_matches(
            &graph,
            "MATCH (p:Person) MATCH (c:City) RETURN p.name AS p, c.name AS c",
            "HashJoin",
        );
    }

    /// A directed triangle: the planner linearizes the cycle into a chain of
    /// expands whose final hop closes onto the already-bound start node, which
    /// the optimizer rewrites to a `MultiwayJoin`.
    fn triangle_graph() -> (TempDir, Graph) {
        let (dir, graph) = setup();
        exec(
            &graph,
            "CREATE (x:N {name: 'x'}), (y:N {name: 'y'}), (z:N {name: 'z'}) \
             CREATE (x)-[:R]->(y), (y)-[:R]->(z), (z)-[:R]->(x)",
        );
        graph.rebuild_csr().unwrap();
        (dir, graph)
    }

    #[test]
    fn streaming_directed_multiway_join_matches_materialized() {
        let (_dir, graph) = triangle_graph();
        assert_streaming_matches(
            &graph,
            "MATCH (a:N)-[:R]->(b)-[:R]->(c)-[:R]->(a) \
             RETURN a.name AS a, b.name AS b, c.name AS c",
            "MultiwayJoin",
        );
    }

    #[test]
    fn streaming_undirected_multiway_join_matches_materialized() {
        let (_dir, graph) = triangle_graph();
        // The closing hop is undirected, so the MultiwayJoin checks both edge
        // directions between the bound pair.
        assert_streaming_matches(
            &graph,
            "MATCH (a:N)-[:R]->(b)-[:R]->(c)-[r:R]-(a) \
             RETURN a.name AS a, b.name AS b, c.name AS c",
            "MultiwayJoin",
        );
    }

    /// One probe batch fanning out to more than `STREAM_BATCH` joined rows must
    /// exercise the overflow buffer without losing or duplicating rows.
    #[test]
    fn streaming_inner_join_crosses_stream_batch() {
        let (_dir, graph) = setup();
        let b = graph
            .add_node("Person", &serde_json::json!({"name": "b"}))
            .unwrap();
        // 60 sources on the `a` side, 5 on the `c` side, all KNOWS b: the join
        // emits 60 * 5 = 300 rows (> STREAM_BATCH = 256) from one probe batch.
        for i in 0..60 {
            let a = graph
                .add_node("Person", &serde_json::json!({"name": format!("a{i}")}))
                .unwrap();
            graph
                .add_edge(a, b, "KNOWS", &serde_json::json!({}))
                .unwrap();
        }
        for i in 0..5 {
            let c = graph
                .add_node("Person", &serde_json::json!({"name": format!("c{i}")}))
                .unwrap();
            graph
                .add_edge(c, b, "KNOWS", &serde_json::json!({}))
                .unwrap();
        }
        graph.rebuild_csr().unwrap();
        assert_streaming_matches(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             MATCH (c:Person)-[:KNOWS]->(b) RETURN a.name AS a, c.name AS c",
            "HashJoin",
        );
    }

    /// `WITH DISTINCT` lowers to a `Distinct` operator: streaming it lets a
    /// downstream `LIMIT` short-circuit, and streamed-all must reproduce the
    /// deduplicated result.
    #[test]
    fn streaming_distinct_matches_materialized() {
        let (_dir, graph) = setup();
        // a1, a2 both know b1; a3 knows b2: DISTINCT b collapses to two targets.
        exec(
            &graph,
            "CREATE (b1:Person {name: 'b1'}), (b2:Person {name: 'b2'}), \
             (a1:Person {name: 'a1'}), (a2:Person {name: 'a2'}), (a3:Person {name: 'a3'}) \
             CREATE (a1)-[:KNOWS]->(b1), (a2)-[:KNOWS]->(b1), (a3)-[:KNOWS]->(b2)",
        );
        graph.rebuild_csr().unwrap();
        assert_streaming_matches(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH DISTINCT b RETURN b.name AS b",
            "Distinct",
        );
    }

    /// UNWIND streams 1:N, so a small `LIMIT` stops after a few elements.
    #[test]
    fn streaming_unwind_matches_materialized() {
        let (_dir, graph) = setup();
        assert_streaming_matches(
            &graph,
            "UNWIND [1, 2, 3, 4, 5, 6, 7] AS x RETURN x AS x",
            "Unwind",
        );
    }

    /// A leading OPTIONAL MATCH forwards its rows when the pattern matches, and
    /// emits exactly one null-filled row when it does not. A `LIMIT` short-circuits
    /// the pass-through.
    #[test]
    fn streaming_optional_match_passes_through_and_null_fills() {
        let (_dir, graph) = setup();
        exec(
            &graph,
            "CREATE (:Person {name: 'p1'}), (:Person {name: 'p2'})",
        );
        graph.rebuild_csr().unwrap();

        let mut rows = run_rows(&graph, "OPTIONAL MATCH (p:Person) RETURN p.name AS p");
        rows.sort_by_key(|r| r[0].to_string());
        assert_eq!(
            rows,
            vec![vec![serde_json::json!("p1")], vec![serde_json::json!("p2")],]
        );

        // No match: a single null-filled row.
        let none = run_rows(&graph, "OPTIONAL MATCH (x:Nonexistent) RETURN x.name AS x");
        assert_eq!(none, vec![vec![serde_json::Value::Null]]);

        // LIMIT short-circuits the forwarded rows.
        let limited = run_rows(
            &graph,
            "OPTIONAL MATCH (p:Person) RETURN p.name AS p LIMIT 1",
        );
        assert_eq!(limited.len(), 1);
    }

    /// `ORDER BY ... LIMIT k` selects a bounded top-N via the heap path; the result
    /// must equal a full sort truncated to the window, for ASC, DESC, and a
    /// SKIP/LIMIT window.
    #[test]
    fn sort_limit_topn_matches_full_sort() {
        let (_dir, graph) = setup();
        for i in 0..50 {
            exec(&graph, &format!("CREATE (:P {{name: 'n{i}', age: {i}}})"));
        }
        graph.rebuild_csr().unwrap();

        // The top-N path engages only when the Limit's input is a Sort.
        let plan = optimized_plan(
            &graph,
            "MATCH (n:P) RETURN n.age AS age ORDER BY n.age ASC LIMIT 5",
        );
        assert!(
            matches!(
                &plan,
                PhysicalOperator::Limit { input, .. } if matches!(input.as_ref(), PhysicalOperator::Sort { .. })
            ),
            "expected Limit over Sort, got:\n{}",
            format_physical_plan(&plan, 0)
        );

        let full = run_rows(&graph, "MATCH (n:P) RETURN n.age AS age ORDER BY n.age ASC");
        let top = run_rows(
            &graph,
            "MATCH (n:P) RETURN n.age AS age ORDER BY n.age ASC LIMIT 5",
        );
        assert_eq!(top, full[..5].to_vec(), "ASC top-5");

        let full_desc = run_rows(
            &graph,
            "MATCH (n:P) RETURN n.age AS age ORDER BY n.age DESC",
        );
        let top_desc = run_rows(
            &graph,
            "MATCH (n:P) RETURN n.age AS age ORDER BY n.age DESC LIMIT 5",
        );
        assert_eq!(top_desc, full_desc[..5].to_vec(), "DESC top-5");

        let window = run_rows(
            &graph,
            "MATCH (n:P) RETURN n.age AS age ORDER BY n.age ASC SKIP 3 LIMIT 4",
        );
        assert_eq!(window, full[3..7].to_vec(), "ASC SKIP 3 LIMIT 4 window");
    }

    /// Top-N over more than `STREAM_BATCH` input rows must still match a full sort,
    /// exercising the bounded buffer's trim-on-fill across batches.
    #[test]
    fn sort_limit_topn_crosses_stream_batch() {
        let (_dir, graph) = setup();
        // 600 rows (> STREAM_BATCH = 256), ages chosen so order is unambiguous.
        for i in 0..600 {
            exec(&graph, &format!("CREATE (:Q {{age: {i}}})"));
        }
        graph.rebuild_csr().unwrap();
        let full = run_rows(&graph, "MATCH (n:Q) RETURN n.age AS age ORDER BY n.age ASC");
        let top = run_rows(
            &graph,
            "MATCH (n:Q) RETURN n.age AS age ORDER BY n.age ASC LIMIT 10",
        );
        assert_eq!(top, full[..10].to_vec());
    }

    /// `ORDER BY ... SKIP` without a `LIMIT` plans a `Limit` whose count is
    /// `usize::MAX`, so the top-N bound saturates to `usize::MAX`. The bounded
    /// sort buffer must not overflow on its `2 * k` trim threshold (a debug-build
    /// panic caught by the TCK skip scenarios).
    #[test]
    fn sort_skip_without_limit_does_not_overflow() {
        let (_dir, graph) = setup();
        for i in 0..5 {
            exec(&graph, &format!("CREATE (:R {{age: {i}}})"));
        }
        graph.rebuild_csr().unwrap();
        let full = run_rows(&graph, "MATCH (n:R) RETURN n.age AS age ORDER BY n.age ASC");
        let skipped = run_rows(
            &graph,
            "MATCH (n:R) RETURN n.age AS age ORDER BY n.age ASC SKIP 2",
        );
        assert_eq!(skipped, full[2..].to_vec());
    }
}

#[cfg(test)]
mod triangle_count_exec_tests {
    //! Plan-shape and parity tests for the `TriangleCount` kernel rewrite.
    //! The forced row path appends an always-true `IS NULL` predicate: its
    //! pushed-down `Filter` breaks the rewrite's exact shape match without
    //! changing the result set, so the same query runs through the
    //! `MultiwayJoin` row pipeline as a semantic oracle.
    use super::*;
    use crate::ast::Statement;
    use crate::parser;
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};
    use issundb_core::Graph;
    use tempfile::TempDir;

    const KERNEL_Q: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)\
         -[:KNOWS]->(a) RETURN count(a) AS n";
    const ROW_Q: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)\
         -[:KNOWS]->(a) WHERE a.__force_row_path IS NULL RETURN count(a) AS n";

    fn setup() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn plan_text(graph: &Graph, cypher: &str) -> String {
        let stmt = parser::parse(cypher).unwrap();
        let q = match stmt {
            Statement::Query(q) => q,
            _ => panic!("not a read query: {cypher}"),
        };
        let logical = LogicalPlanner::plan(&q).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        format_physical_plan(&Optimizer::optimize(physical, Some(graph)), 0)
    }

    fn count_result(graph: &Graph, cypher: &str) -> u64 {
        let result = super::execute(graph, cypher, &HashMap::new()).unwrap();
        assert_eq!(result.records.len(), 1, "count query must yield one row");
        result.records[0].values[0].as_u64().unwrap()
    }

    fn triangle_fixture() -> (TempDir, Graph) {
        let (dir, graph) = setup();
        super::execute(
            &graph,
            "CREATE (x:Person), (y:Person), (z:Person) \
             CREATE (x)-[:KNOWS]->(y), (y)-[:KNOWS]->(z), (z)-[:KNOWS]->(x)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        (dir, graph)
    }

    /// The harness triangle query plans to the kernel leaf and returns one
    /// row per rotation of the single cycle.
    #[test]
    fn kernel_plan_shape_and_result() {
        let (_dir, graph) = triangle_fixture();
        let plan = plan_text(&graph, KERNEL_Q);
        assert!(
            plan.contains("TriangleCount"),
            "expected the kernel leaf:\n{plan}"
        );
        assert_eq!(count_result(&graph, KERNEL_Q), 3);

        // The forced row path keeps the MultiwayJoin pipeline and agrees.
        let row_plan = plan_text(&graph, ROW_Q);
        assert!(
            !row_plan.contains("TriangleCount") && row_plan.contains("MultiwayJoin"),
            "forcing predicate failed to disable the rewrite:\n{row_plan}"
        );
        assert_eq!(count_result(&graph, ROW_Q), 3);
    }

    /// `count(*)` and counts over the other pattern variables take the kernel
    /// path too; every variable is bound in every match, so the counts agree.
    #[test]
    fn kernel_accepts_count_star_and_other_variables() {
        let (_dir, graph) = triangle_fixture();
        for ret in ["count(*)", "count(b)", "count(c)", "count(r1)"] {
            let q = format!(
                "MATCH (a:Person)-[r1:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
                 RETURN {ret} AS n"
            );
            let plan = plan_text(&graph, &q);
            assert!(
                plan.contains("TriangleCount"),
                "expected the kernel leaf for {ret}:\n{plan}"
            );
            assert_eq!(count_result(&graph, &q), 3, "count mismatch for {ret}");
        }
    }

    /// Shapes outside the kernel contract must stay on the row pipeline:
    /// extra predicates, grouping, DISTINCT, undirected or reversed or
    /// var-length hops, and a missing closing join.
    #[test]
    fn unsupported_shapes_keep_the_row_path() {
        let (_dir, graph) = triangle_fixture();
        let bail_queries = [
            // Property predicate on a pattern variable.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             WHERE b.age > 1 RETURN count(a) AS n",
            // Grouped aggregation.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             RETURN b.city AS city, count(a) AS n",
            // DISTINCT count.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             RETURN count(DISTINCT a) AS n",
            // Undirected middle hop.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]-(c:Person)-[:KNOWS]->(a) \
             RETURN count(a) AS n",
            // Reversed closing hop: not a directed cycle.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)<-[:KNOWS]-(a) \
             RETURN count(a) AS n",
            // Var-length hop.
            "MATCH (a:Person)-[:KNOWS*1..2]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             RETURN count(a) AS n",
            // Open chain: no closing join at all.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
             RETURN count(a) AS n",
        ];
        for q in bail_queries {
            let plan = plan_text(&graph, q);
            assert!(
                !plan.contains("TriangleCount"),
                "rewrite must not fire for `{q}`:\n{plan}"
            );
            // The query still executes on the row path.
            super::execute(&graph, q, &HashMap::new()).unwrap();
        }
    }

    /// Differential parity on random multigraphs: the kernel path and the
    /// forced row path must agree on every graph, including self-loops and
    /// parallel edges.
    #[test]
    fn kernel_matches_row_path_on_random_multigraphs() {
        use proptest::prelude::*;

        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 24,
            ..ProptestConfig::default()
        });
        let strategy = (
            1usize..=6,
            proptest::collection::vec((0usize..6, 0usize..6), 0..24),
        );
        runner
            .run(&strategy, |(n_nodes, edges)| {
                let (_dir, graph) = setup();
                let mut create = String::from("CREATE ");
                for i in 0..n_nodes {
                    if i > 0 {
                        create.push_str(", ");
                    }
                    create.push_str(&format!("(p{i}:Person)"));
                }
                super::execute(&graph, &create, &HashMap::new()).unwrap();
                let mut ids = Vec::new();
                for i in 0..n_nodes {
                    ids.push(i as u64);
                }
                for (s, d) in &edges {
                    let (s, d) = (s % n_nodes, d % n_nodes);
                    let q = format!(
                        "MATCH (a:Person), (b:Person) WHERE id(a) = {} AND id(b) = {} \
                         CREATE (a)-[:KNOWS]->(b)",
                        ids[s], ids[d]
                    );
                    super::execute(&graph, &q, &HashMap::new()).unwrap();
                }
                graph.rebuild_csr().unwrap();

                let kernel = count_result(&graph, KERNEL_Q);
                let row = count_result(&graph, ROW_Q);
                prop_assert_eq!(kernel, row, "kernel vs row path on {} nodes", n_nodes);
                Ok(())
            })
            .unwrap();
    }
}

#[cfg(test)]
mod path_count_exec_tests {
    //! Plan-shape and parity tests for the `PathCount` kernel rewrite over
    //! open one-hop and two-hop expansions. As with the triangle tests, an
    //! always-true `IS NULL` predicate pushes a `Filter` that breaks the exact
    //! shape match without changing the result, forcing the row pipeline as a
    //! semantic oracle.
    use super::*;
    use crate::ast::Statement;
    use crate::parser;
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};
    use issundb_core::Graph;
    use tempfile::TempDir;

    const ONE_HOP_KERNEL: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*) AS n";
    const ONE_HOP_ROW: &str =
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.__force IS NULL RETURN count(*) AS n";
    const TWO_HOP_KERNEL: &str =
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN count(*) AS n";
    const TWO_HOP_ROW: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
         WHERE a.__force IS NULL RETURN count(*) AS n";

    fn setup() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn plan_text(graph: &Graph, cypher: &str) -> String {
        let stmt = parser::parse(cypher).unwrap();
        let q = match stmt {
            Statement::Query(q) => q,
            _ => panic!("not a read query: {cypher}"),
        };
        let logical = LogicalPlanner::plan(&q).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        format_physical_plan(&Optimizer::optimize(physical, Some(graph)), 0)
    }

    fn count_result(graph: &Graph, cypher: &str) -> u64 {
        let result = super::execute(graph, cypher, &HashMap::new()).unwrap();
        assert_eq!(result.records.len(), 1, "count query must yield one row");
        result.records[0].values[0].as_u64().unwrap()
    }

    /// The one-hop and two-hop count queries plan to the `PathCount` leaf,
    /// while forcing a pushed-down predicate keeps the row pipeline.
    #[test]
    fn kernel_plan_shape() {
        let (_dir, graph) = setup();
        super::execute(
            &graph,
            "CREATE (x:Person), (y:Person), (z:Person) \
             CREATE (x)-[:KNOWS]->(y), (y)-[:KNOWS]->(z)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        for q in [ONE_HOP_KERNEL, TWO_HOP_KERNEL] {
            let plan = plan_text(&graph, q);
            assert!(plan.contains("PathCount"), "expected kernel leaf:\n{plan}");
        }
        for q in [ONE_HOP_ROW, TWO_HOP_ROW] {
            let plan = plan_text(&graph, q);
            assert!(
                !plan.contains("PathCount"),
                "forcing predicate failed to disable the rewrite:\n{plan}"
            );
        }
    }

    /// Shapes outside the kernel contract stay on the row pipeline.
    #[test]
    fn unsupported_shapes_keep_the_row_path() {
        let (_dir, graph) = setup();
        super::execute(
            &graph,
            "CREATE (x:Person)-[:KNOWS]->(y:Person)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let bail = [
            // Cross-variable comparison: neither side is a literal, so it cannot
            // become an index allow-set and must stay a per-row filter.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age > a.age RETURN count(*) AS n",
            // Non-comparison predicate on a pattern variable.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age IS NOT NULL RETURN count(*) AS n",
            // Grouped aggregation.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.city AS c, count(*) AS n",
            // DISTINCT count.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(DISTINCT a) AS n",
            // Reversed hop binds the scan node as the destination.
            "MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN count(*) AS n",
            // Var-length hop.
            "MATCH (a:Person)-[:KNOWS*1..2]->(b:Person) RETURN count(*) AS n",
            // Three hops.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person) \
             RETURN count(*) AS n",
        ];
        for q in bail {
            let plan = plan_text(&graph, q);
            assert!(
                !plan.contains("PathCount"),
                "rewrite must not fire for `{q}`:\n{plan}"
            );
            super::execute(&graph, q, &HashMap::new()).unwrap();
        }
    }

    /// A filtered two-hop count fires the kernel and agrees with the forced row
    /// pipeline, and the predicate actually removes paths the unfiltered count
    /// includes.
    #[test]
    fn filtered_two_hop_matches_row_path() {
        let (_dir, graph) = setup();
        super::execute(
            &graph,
            "CREATE (a:Person {age: 20}), (b:Person {age: 30}), (c:Person {age: 40}), \
                    (d:Person {age: 60}), (e:Person {age: 25}) \
             CREATE (a)-[:KNOWS]->(b), (a)-[:KNOWS]->(d), (b)-[:KNOWS]->(c), \
                    (b)-[:KNOWS]->(e), (d)-[:KNOWS]->(c), (c)-[:KNOWS]->(e)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let kernel_q = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                        WHERE b.age < 50 AND c.age > 25 RETURN count(*) AS n";
        // The trailing always-true `IS NULL` conjunct pushes a residual Filter
        // onto the source scan, breaking the kernel shape so the same query runs
        // the row pipeline as a semantic oracle.
        let row_q = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                     WHERE b.age < 50 AND c.age > 25 AND a.__force IS NULL RETURN count(*) AS n";

        assert!(
            plan_text(&graph, kernel_q).contains("PathCount"),
            "filtered query must fire the kernel:\n{}",
            plan_text(&graph, kernel_q)
        );
        assert!(
            !plan_text(&graph, row_q).contains("PathCount"),
            "forced query must keep the row pipeline:\n{}",
            plan_text(&graph, row_q)
        );

        assert_eq!(count_result(&graph, kernel_q), count_result(&graph, row_q));
        assert!(
            count_result(&graph, kernel_q) < count_result(&graph, TWO_HOP_KERNEL),
            "the filter must exclude some unfiltered paths"
        );
    }

    /// Differential parity for filtered two-hop counts on random graphs with
    /// random ages: the kernel allow-set path equals the forced row pipeline for
    /// predicates on the middle and destination nodes.
    #[test]
    fn filtered_kernel_matches_row_path_on_random_graphs() {
        use proptest::prelude::*;

        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 32,
            ..ProptestConfig::default()
        });
        let strategy = (
            2usize..=6,
            proptest::collection::vec((0usize..6, 0usize..6), 0..24),
            proptest::collection::vec(0i64..100, 6),
        );
        runner
            .run(&strategy, |(n_nodes, edges, ages)| {
                let (_dir, graph) = setup();
                let mut create = String::from("CREATE ");
                for i in 0..n_nodes {
                    if i > 0 {
                        create.push_str(", ");
                    }
                    create.push_str(&format!("(p{i}:Person {{age: {}}})", ages[i % ages.len()]));
                }
                super::execute(&graph, &create, &HashMap::new()).unwrap();
                for (s, d) in &edges {
                    let (s, d) = (s % n_nodes, d % n_nodes);
                    let q = format!(
                        "MATCH (a:Person), (b:Person) WHERE id(a) = {s} AND id(b) = {d} \
                         CREATE (a)-[:KNOWS]->(b)"
                    );
                    super::execute(&graph, &q, &HashMap::new()).unwrap();
                }
                graph.rebuild_csr().unwrap();

                let kernel_q = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                                WHERE b.age < 50 AND c.age >= 25 RETURN count(*) AS n";
                let row_q = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                             WHERE b.age < 50 AND c.age >= 25 AND a.__force IS NULL \
                             RETURN count(*) AS n";
                prop_assert_eq!(
                    count_result(&graph, kernel_q),
                    count_result(&graph, row_q),
                    "filtered kernel vs row on {} nodes",
                    n_nodes
                );
                Ok(())
            })
            .unwrap();
    }

    /// Differential parity on random multigraphs (self-loops and parallel
    /// edges included): the kernel path and the forced row path agree for both
    /// the one-hop and two-hop counts.
    #[test]
    fn kernel_matches_row_path_on_random_multigraphs() {
        use proptest::prelude::*;

        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 32,
            ..ProptestConfig::default()
        });
        let strategy = (
            1usize..=6,
            proptest::collection::vec((0usize..6, 0usize..6), 0..24),
        );
        runner
            .run(&strategy, |(n_nodes, edges)| {
                let (_dir, graph) = setup();
                let mut create = String::from("CREATE ");
                for i in 0..n_nodes {
                    if i > 0 {
                        create.push_str(", ");
                    }
                    create.push_str(&format!("(p{i}:Person)"));
                }
                super::execute(&graph, &create, &HashMap::new()).unwrap();
                for (s, d) in &edges {
                    let (s, d) = (s % n_nodes, d % n_nodes);
                    let q = format!(
                        "MATCH (a:Person), (b:Person) WHERE id(a) = {s} AND id(b) = {d} \
                         CREATE (a)-[:KNOWS]->(b)"
                    );
                    super::execute(&graph, &q, &HashMap::new()).unwrap();
                }
                graph.rebuild_csr().unwrap();

                prop_assert_eq!(
                    count_result(&graph, ONE_HOP_KERNEL),
                    count_result(&graph, ONE_HOP_ROW),
                    "one-hop kernel vs row on {} nodes",
                    n_nodes
                );
                prop_assert_eq!(
                    count_result(&graph, TWO_HOP_KERNEL),
                    count_result(&graph, TWO_HOP_ROW),
                    "two-hop kernel vs row on {} nodes",
                    n_nodes
                );
                Ok(())
            })
            .unwrap();
    }
}

#[cfg(test)]
mod grouped_degree_exec_tests {
    //! Plan-shape and parity tests for the `GroupedDegree` kernel rewrite over a
    //! `count` grouped by one endpoint of a single directed hop. As with the
    //! path-count tests, an always-true `IS NULL` predicate pushes a `Filter`
    //! that breaks the exact shape match without changing the result, forcing the
    //! row pipeline as a semantic oracle.
    use super::*;
    use crate::ast::Statement;
    use crate::parser;
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};
    use issundb_core::Graph;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn plan_text(graph: &Graph, cypher: &str) -> String {
        let stmt = parser::parse(cypher).unwrap();
        let q = match stmt {
            Statement::Query(q) => q,
            _ => panic!("not a read query: {cypher}"),
        };
        let logical = LogicalPlanner::plan(&q).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        format_physical_plan(&Optimizer::optimize(physical, Some(graph)), 0)
    }

    /// Execute `cypher` and return its records sorted by their serialized form,
    /// so the comparison is order-insensitive (a grouped aggregate without
    /// `ORDER BY` yields groups in hash order on both paths).
    fn sorted_records(graph: &Graph, cypher: &str) -> Vec<Vec<serde_json::Value>> {
        let mut rows: Vec<Vec<serde_json::Value>> = super::execute(graph, cypher, &HashMap::new())
            .unwrap()
            .records
            .into_iter()
            .map(|r| r.values)
            .collect();
        rows.sort_by_key(|r| serde_json::to_string(r).unwrap());
        rows
    }

    /// The grouped count over a one-hop expansion plans to the `GroupedDegree`
    /// leaf; a forcing predicate, a different group variable, or a second hop
    /// keep the row pipeline.
    #[test]
    fn kernel_plan_shape() {
        let (_dir, graph) = setup();
        super::execute(
            &graph,
            "CREATE (x:Person {id: 1}), (y:Person {id: 2}), (z:Person {id: 3}) \
             CREATE (x)-[:FOLLOWS]->(y), (z)-[:FOLLOWS]->(y), (x)-[:FOLLOWS]->(z)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let fire = [
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(f.id) AS num",
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(*) AS num",
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(f) AS num",
        ];
        for q in fire {
            let plan = plan_text(&graph, q);
            assert!(
                plan.contains("GroupedDegree"),
                "expected kernel leaf:\n{plan}"
            );
        }

        let bail = [
            // Forcing predicate on the counted node breaks the exact shape.
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) WHERE f.__force IS NULL \
             RETURN p.id AS id, count(f.id) AS num",
            // DISTINCT count is not a plain degree.
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(DISTINCT f.id) AS num",
            // Grouping-free count is the PathCount kernel's job.
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN count(*) AS num",
            // A property predicate on a grouped node is not lowered here.
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) WHERE p.id > 1 \
             RETURN p.id AS id, count(f.id) AS num",
            // Two hops fall back.
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person)-[:FOLLOWS]->(q:Person) \
             RETURN q.id AS id, count(f.id) AS num",
        ];
        for q in bail {
            let plan = plan_text(&graph, q);
            assert!(
                !plan.contains("GroupedDegree"),
                "rewrite must not fire for `{q}`:\n{plan}"
            );
            super::execute(&graph, q, &HashMap::new()).unwrap();
        }
    }

    /// A `count(v.prop)` where some sources lack the property must exclude those
    /// edges, matching `count(prop)` null semantics, not raw in-degree.
    #[test]
    fn counts_nonnull_property_only() {
        let (_dir, graph) = setup();
        // f1 and f2 carry `tag`; f3 does not. All three follow p.
        super::execute(
            &graph,
            "CREATE (p:Person {id: 100}), (f1:Person {id: 1, tag: 'a'}), \
                    (f2:Person {id: 2, tag: 'b'}), (f3:Person {id: 3}) \
             CREATE (f1)-[:FOLLOWS]->(p), (f2)-[:FOLLOWS]->(p), (f3)-[:FOLLOWS]->(p)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let kernel_q =
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(f.tag) AS num";
        let row_q = "MATCH (f:Person)-[:FOLLOWS]->(p:Person) WHERE f.__force IS NULL \
                     RETURN p.id AS id, count(f.tag) AS num";
        assert!(plan_text(&graph, kernel_q).contains("GroupedDegree"));
        assert!(!plan_text(&graph, row_q).contains("GroupedDegree"));
        // Two of three followers have a tag.
        assert_eq!(
            sorted_records(&graph, kernel_q),
            vec![vec![serde_json::json!(100), serde_json::json!(2)]]
        );
        assert_eq!(
            sorted_records(&graph, kernel_q),
            sorted_records(&graph, row_q)
        );
    }

    /// Distinct nodes that share the group key merge into one group, summing
    /// counts, exactly as the value-keyed row pipeline aggregate does.
    #[test]
    fn merges_groups_sharing_a_key() {
        let (_dir, graph) = setup();
        // p1 and p2 share id=7; each is followed once. The group keyed on id=7
        // must sum to 2, not appear as two rows.
        super::execute(
            &graph,
            "CREATE (p1:Person {id: 7}), (p2:Person {id: 7}), \
                    (f1:Person {id: 1}), (f2:Person {id: 2}) \
             CREATE (f1)-[:FOLLOWS]->(p1), (f2)-[:FOLLOWS]->(p2)",
            &HashMap::new(),
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let kernel_q =
            "MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN p.id AS id, count(f.id) AS num";
        let row_q = "MATCH (f:Person)-[:FOLLOWS]->(p:Person) WHERE f.__force IS NULL \
                     RETURN p.id AS id, count(f.id) AS num";
        assert!(plan_text(&graph, kernel_q).contains("GroupedDegree"));
        assert_eq!(
            sorted_records(&graph, kernel_q),
            sorted_records(&graph, row_q)
        );
        assert_eq!(
            sorted_records(&graph, kernel_q),
            vec![vec![serde_json::json!(7), serde_json::json!(2)]]
        );
    }

    /// Differential parity on random multigraphs (self-loops, parallel edges,
    /// some sources without `id`, a second label): the grouped-degree kernel and
    /// the forced row pipeline agree for `count(*)`, `count(f)`, and
    /// `count(f.id)`, grouped on one and on two destination properties.
    #[test]
    fn kernel_matches_row_path_on_random_graphs() {
        use proptest::prelude::*;

        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 48,
            ..ProptestConfig::default()
        });
        let strategy = (
            1usize..=7,
            proptest::collection::vec((0usize..7, 0usize..7), 0..28),
            // Per node: whether it carries an `id`/`name` (some null), keyed by index.
            proptest::collection::vec(any::<bool>(), 7),
        );
        runner
            .run(&strategy, |(n_nodes, edges, has_id)| {
                let (_dir, graph) = setup();
                let mut create = String::from("CREATE ");
                for i in 0..n_nodes {
                    if i > 0 {
                        create.push_str(", ");
                    }
                    // Non-unique id (i % 3) exercises group-key merging; some
                    // nodes omit id entirely to exercise the non-null filter.
                    if has_id[i % has_id.len()] {
                        create.push_str(&format!("(p{i}:Person {{id: {}}})", i % 3));
                    } else {
                        create.push_str(&format!("(p{i}:Person)"));
                    }
                }
                super::execute(&graph, &create, &HashMap::new()).unwrap();
                for (s, d) in &edges {
                    let (s, d) = (s % n_nodes, d % n_nodes);
                    let q = format!(
                        "MATCH (a:Person), (b:Person) WHERE id(a) = {s} AND id(b) = {d} \
                         CREATE (a)-[:FOLLOWS]->(b)"
                    );
                    super::execute(&graph, &q, &HashMap::new()).unwrap();
                }
                graph.rebuild_csr().unwrap();

                for ret in [
                    "p.id AS id, count(*) AS num",
                    "p.id AS id, count(f) AS num",
                    "p.id AS id, count(f.id) AS num",
                ] {
                    let kernel_q = format!("MATCH (f:Person)-[:FOLLOWS]->(p:Person) RETURN {ret}");
                    let row_q = format!(
                        "MATCH (f:Person)-[:FOLLOWS]->(p:Person) WHERE f.__force IS NULL \
                         RETURN {ret}"
                    );
                    prop_assert!(
                        plan_text(&graph, &kernel_q).contains("GroupedDegree"),
                        "kernel query did not lower: {}",
                        plan_text(&graph, &kernel_q)
                    );
                    prop_assert!(!plan_text(&graph, &row_q).contains("GroupedDegree"));
                    prop_assert_eq!(
                        sorted_records(&graph, &kernel_q),
                        sorted_records(&graph, &row_q),
                        "grouped-degree vs row for `{}` on {} nodes",
                        ret,
                        n_nodes
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}
