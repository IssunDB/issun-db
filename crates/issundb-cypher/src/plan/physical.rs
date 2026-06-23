use crate::ast::{AggFn, Expr, Literal, SortItem};
use crate::plan::logical::{FilterExpr, LogicalOperator};

/// A physical representation of a query execution operator.
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalOperator {
    /// A single empty row to bootstrap queries.
    SingleRow,
    /// Unwind a list expression and bind each element to a variable.
    Unwind {
        input: Box<PhysicalOperator>,
        expr: Expr,
        variable: String,
    },
    /// Scan nodes by label: binds `variable` to nodes matching `label`.
    LabelScan {
        variable: String,
        label: Option<String>,
    },
    /// Seek a single node by its internal id: binds `variable` to the node whose
    /// id equals `id_value`, if it exists (and matches `label` when present).
    ///
    /// Emitted by the optimizer when a `WHERE id(n) = <const>` predicate sits over
    /// a node scan, replacing a full label scan with an O(1) primary-key lookup.
    NodeByIdSeek {
        variable: String,
        label: Option<String>,
        id_value: Expr,
    },
    /// Scan nodes using a property index: binds `variable` to nodes matching `label` and `property` value.
    NodeIndexScan {
        variable: String,
        label: String,
        property: String,
        value: Expr,
    },
    /// Scan nodes using a property range index: binds `variable` to nodes where `property` falls
    /// within [`lo`, `hi`] (inclusive/exclusive per the flags). At least one bound must be `Some`.
    NodeRangeScan {
        variable: String,
        label: String,
        property: String,
        lo: Option<Expr>,
        lo_inclusive: bool,
        hi: Option<Expr>,
        hi_inclusive: bool,
    },
    /// Expand relationships: starts from `src_var`, traverses relationship `rel_type`
    /// in direction `is_incoming` up to range bounds, and binds relationship to `rel_var`
    /// and target to `dst_var`.
    Expand {
        input: Box<PhysicalOperator>,
        src_var: String,
        rel_var: String,
        dst_var: String,
        rel_type: Option<String>,
        is_incoming: bool,
        /// When true, traverse both directions and deduplicate results.
        is_undirected: bool,
        min_hops: usize,
        max_hops: usize,
        /// Relationship variables bound by earlier hops of the same pattern;
        /// this hop must bind a different relationship (openCypher
        /// relationship uniqueness, scoped to one pattern).
        unique_rels: Vec<String>,
        /// True only when the pattern binds a path variable (`MATCH p = ...`);
        /// the executor then materializes a `_path_*` object per emitted row,
        /// and the fused-chain fast path (which skips path objects) must not
        /// apply.
        needs_path: bool,
    },
    /// Filter records based on expressions/WHERE predicates.
    Filter {
        input: Box<PhysicalOperator>,
        expression: FilterExpr,
    },
    /// Project RETURN expressions to form the final table.
    Project {
        input: Box<PhysicalOperator>,
        items: Vec<(Expr, Option<String>)>, // (expression, alias)
        is_barrier: bool,
    },
    /// Join two independent physical sub-plans (cross product or hash join).
    HashJoin {
        left: Box<PhysicalOperator>,
        right: Box<PhysicalOperator>,
    },
    /// Aggregate rows, grouping by non-aggregate keys and computing aggregate functions.
    Aggregate {
        input: Box<PhysicalOperator>,
        group_by: Vec<(Expr, Option<String>)>,
        aggregations: Vec<(AggFn, Expr, String)>,
    },
    /// Sort rows by one or more sort keys.
    Sort {
        input: Box<PhysicalOperator>,
        items: Vec<SortItem>,
    },
    /// Skip and limit the row stream.
    Limit {
        input: Box<PhysicalOperator>,
        skip: usize,
        count: usize,
    },
    /// Optional match: evaluate inner plan; if empty, emit one null-filled row.
    OptionalMatch {
        input: Box<PhysicalOperator>,
        null_vars: Vec<String>,
    },
    /// Deduplicate rows (DISTINCT). `keys` selects the binding names forming
    /// the dedup key; `None` dedups on the full row.
    Distinct {
        input: Box<PhysicalOperator>,
        keys: Option<Vec<String>>,
    },
    /// Execute write operations (CREATE, MERGE, SET, DELETE) for each input row.
    /// Binds new nodes or edges from CREATE into the PathMap and passes each row downstream.
    WritePart {
        input: Box<PhysicalOperator>,
        part: crate::ast::QueryPart,
    },
    /// A resolved `CALL`: emit one output row per entry in `rows` for each input
    /// row, binding `output_vars` to the corresponding cells.
    ProcedureCall {
        input: Box<PhysicalOperator>,
        output_vars: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
    },
    /// Worst-case optimal join (WCOJ) for closing a cyclic pattern.
    ///
    /// Emitted by the optimizer when an `Expand` node's `dst_var` is already
    /// bound by an earlier operator in the same plan (a "closing hop"). Instead
    /// of iterating every neighbor of `closing_src_var` and filtering by value,
    /// the executor bulk-fetches neighbor sets once per unique source node and
    /// performs an O(1) hash-map lookup per row.
    ///
    /// For a triangle pattern `(a)-[r1]->(b)-[r2]->(c)-[r3]->(a)` this replaces
    /// the final `Expand(c → a via r3)` with a `MultiwayJoin` that checks each
    /// `(c, a)` pair in one pass over the pre-built neighbor index.
    MultiwayJoin {
        input: Box<PhysicalOperator>,
        /// Node at the open end of the closing edge.
        closing_src_var: String,
        /// Already-bound node the closing edge must connect to.
        closing_dst_var: String,
        /// Relationship type of the closing edge; `None` matches any type.
        closing_rel_type: Option<String>,
        /// Variable to bind the closing edge's `EdgeId`.
        closing_rel_var: String,
        /// Direction of the closing edge: `true` = incoming to `closing_src_var`.
        /// Ignored when `closing_is_undirected` is true.
        closing_is_incoming: bool,
        /// When true, the closing edge matches in either direction; the executor
        /// checks both the outgoing and incoming adjacency of `closing_src_var`.
        closing_is_undirected: bool,
        /// Relationship variables bound by earlier hops of the same pattern;
        /// the closing edge must differ from all of them (openCypher
        /// relationship uniqueness).
        closing_unique_rels: Vec<String>,
    },
    /// Whole-pattern count of the directed triangle cycle
    /// `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)`, evaluated by the core
    /// sorted-intersect kernel without materializing any rows.
    ///
    /// Emitted by the optimizer when a grouping-free, non-distinct `count`
    /// aggregate sits directly on the `MultiwayJoin`-closed triangle chain
    /// and nothing else (filters beyond the per-variable labels, projections,
    /// or variable references) needs the per-row bindings. Produces a single
    /// row binding `output` to the count.
    TriangleCount {
        /// Relationship types for the hops `a -> b`, `b -> c`, and `c -> a`.
        rel_types: [Option<String>; 3],
        /// Labels required on `a`, `b`, and `c`.
        labels: [Option<String>; 3],
        /// Output column the count is bound to.
        output: String,
    },
    /// Whole-pattern count of an open directed path of one or two hops,
    /// `(v0)-[t1]->(v1)` or `(v0)-[t1]->(v1)-[t2]->(v2)`, evaluated by the core
    /// kernel without materializing any rows.
    ///
    /// Emitted by the optimizer when a grouping-free, non-distinct `count`
    /// aggregate sits directly on a one-hop or two-hop directed expansion and
    /// nothing else (filters beyond the per-variable labels, projections, or
    /// variable references) needs the per-row bindings. Produces a single row
    /// binding `output` to the count.
    PathCount {
        /// Relationship type per hop, in path order. Length 1 or 2.
        rel_types: Vec<Option<String>>,
        /// Label per node variable, in path order. Length `rel_types.len() + 1`.
        labels: Vec<Option<String>>,
        /// Per-vertex property predicates pushed down into the kernel, in path
        /// order (length matches `labels`). Each inner vector holds the
        /// `prop CMP literal` constraints on that node variable; the executor
        /// resolves them to an allow-set via the property index so the filtered
        /// count stays a kernel call. An empty inner vector means the variable
        /// carries no property predicate.
        vertex_filters: Vec<Vec<VertexPred>>,
        /// Output column the count is bound to.
        output: String,
    },
    /// Per-group `count` over a single directed hop, grouped by one endpoint of
    /// the hop, evaluated by the core grouped-degree kernel as an integer pass
    /// over adjacency instead of expanding and folding every edge row.
    ///
    /// Emitted by the optimizer when an `Aggregate` whose group keys are all
    /// single-property reads on one endpoint of a one-hop directed expansion
    /// carries exactly one non-distinct `count` over the other endpoint (or
    /// `count(*)`). Produces one row per surviving group, binding each group
    /// column (as the row-pipeline `Aggregate` would) plus `output` to the
    /// count, so the `Project`/`Sort`/`Limit` above it run unchanged.
    GroupedDegree {
        /// Relationship type of the hop, or `None` for any type.
        rel_type: Option<String>,
        /// Group by the edge destination (count incoming) when true; by the
        /// edge source (count outgoing) when false.
        group_is_dst: bool,
        /// Label required on the group endpoint, if any.
        group_label: Option<String>,
        /// Label required on the counted endpoint, if any.
        counted_label: Option<String>,
        /// Property that must be non-null on the counted endpoint for an edge
        /// to count (`count(v.prop)`); `None` for `count(*)` or `count(v)`.
        counted_nonnull_prop: Option<String>,
        /// The variable bound to the group endpoint node, so the executor can
        /// evaluate the group-by expressions against it.
        group_var: String,
        /// The aggregate's group-by expressions, each a single-property read on
        /// `group_var`, with the same aliases the row pipeline would carry.
        group_by: Vec<(Expr, Option<String>)>,
        /// Output column the count is bound to.
        output: String,
    },
    /// Approximate k-nearest-neighbor scan over the HNSW vector index, binding
    /// `variable` to the `k` nodes nearest to `query` in ascending distance
    /// order under the graph's configured metric.
    ///
    /// Emitted by the optimizer when a `Limit` over a `Sort` whose single
    /// ascending key is `vector_dist(variable, query)` sits over a bare
    /// `LabelScan(variable)` (optionally with pushed-down property-equality
    /// filters). It replaces the scan-plus-sort with one index search, so the
    /// engine never computes a distance for every node. The `Sort` is dropped
    /// because the operator emits in ranked order; the enclosing `Limit`
    /// applies `skip`. `query` is evaluated once against empty bindings, so it
    /// must not reference any bound variable. Any unrecognized shape (descending
    /// order, a non-constant query, a non-`vector_dist` key) keeps the row
    /// pipeline, so correctness never depends on this rewrite.
    VectorTopK {
        /// Node variable bound to each ranked result.
        variable: String,
        /// Label every result must carry, if the scan was labeled.
        label: Option<String>,
        /// The query-vector expression, evaluated once with no row context.
        query: Expr,
        /// Number of nearest neighbors to fetch (`skip + count` of the `Limit`).
        k: usize,
        /// Equality pre-filters on the result variable's properties, pushed down
        /// into the index traversal. Each entry is `(property, value_expr)` and
        /// the value expression is evaluated once with no row context.
        prop_filters: Vec<(String, Expr)>,
    },
}

/// Comparison operator of a per-vertex property predicate pushed into the
/// `PathCount` kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexCmp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A single per-vertex property predicate `property CMP value` bound to one
/// node variable of a `PathCount` pattern. `value` is a constant expression (a
/// literal), evaluated once at execution and resolved to a node-id allow-set
/// through the property index.
#[derive(Debug, Clone, PartialEq)]
pub struct VertexPred {
    pub property: String,
    pub cmp: VertexCmp,
    pub value: Expr,
}

/// A physical planner that compiles logical query plans into physical, executable plans.
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Compile a `LogicalOperator` plan into a `PhysicalOperator` plan.
    pub fn plan(logical: &LogicalOperator) -> PhysicalOperator {
        match logical {
            LogicalOperator::SingleRow => PhysicalOperator::SingleRow,
            LogicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::plan(input)),
                expr: expr.clone(),
                variable: variable.clone(),
            },
            LogicalOperator::LabelScan { variable, label } => PhysicalOperator::LabelScan {
                variable: variable.clone(),
                label: label.clone(),
            },
            LogicalOperator::Expand {
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
            } => PhysicalOperator::Expand {
                input: Box::new(Self::plan(input)),
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
            },
            LogicalOperator::Filter { input, expression } => PhysicalOperator::Filter {
                input: Box::new(Self::plan(input)),
                expression: expression.clone(),
            },
            LogicalOperator::Project {
                input,
                items,
                is_barrier,
            } => PhysicalOperator::Project {
                input: Box::new(Self::plan(input)),
                items: items.clone(),
                is_barrier: *is_barrier,
            },
            LogicalOperator::Join { left, right } => PhysicalOperator::HashJoin {
                left: Box::new(Self::plan(left)),
                right: Box::new(Self::plan(right)),
            },
            LogicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::plan(input)),
                group_by: group_by.clone(),
                aggregations: aggregations.clone(),
            },
            LogicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::plan(input)),
                items: items.clone(),
            },
            LogicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::plan(input)),
                skip: *skip,
                count: *count,
            },
            LogicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(Self::plan(input)),
                    null_vars: null_vars.clone(),
                }
            }
            LogicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
                input: Box::new(Self::plan(input)),
                keys: keys.clone(),
            },
            LogicalOperator::WritePart { input, part } => PhysicalOperator::WritePart {
                input: Box::new(Self::plan(input)),
                part: part.clone(),
            },
            LogicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            } => PhysicalOperator::ProcedureCall {
                input: Box::new(Self::plan(input)),
                output_vars: output_vars.clone(),
                rows: rows.clone(),
            },
        }
    }
}

/// Render an optimized physical plan as an indented, human-readable tree.
///
/// Each line starts with two spaces per depth level, followed by a one-line
/// description of the operator. Child operators are rendered at `depth + 1`.
pub fn format_physical_plan(op: &PhysicalOperator, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    let mut buf = String::new();

    match op {
        PhysicalOperator::SingleRow => {
            buf.push_str(&format!("{}SingleRow\n", pad));
        }
        PhysicalOperator::LabelScan { variable, label } => {
            let lbl = label.as_deref().unwrap_or("*");
            buf.push_str(&format!("{}LabelScan {}:{}\n", pad, variable, lbl));
        }
        PhysicalOperator::NodeByIdSeek {
            variable,
            label,
            id_value,
        } => {
            let lbl = label.as_deref().unwrap_or("*");
            buf.push_str(&format!(
                "{}NodeByIdSeek {}:{} id={}\n",
                pad,
                variable,
                lbl,
                fmt_expr(id_value)
            ));
        }
        PhysicalOperator::NodeIndexScan {
            variable,
            label,
            property,
            value,
        } => {
            buf.push_str(&format!(
                "{}NodeIndexScan {}:{}.{} = {}\n",
                pad,
                variable,
                label,
                property,
                fmt_expr(value)
            ));
        }
        PhysicalOperator::Expand {
            input,
            src_var,
            rel_var,
            dst_var,
            rel_type,
            is_incoming,
            min_hops,
            max_hops,
            ..
        } => {
            let rtype = rel_type.as_deref().unwrap_or("*");
            let range = if *min_hops == 1 && *max_hops == 1 {
                String::new()
            } else if *max_hops == usize::MAX {
                format!("*{}..", min_hops)
            } else if min_hops == max_hops {
                format!("*{}", min_hops)
            } else {
                format!("*{}..{}", min_hops, max_hops)
            };
            let (left, right) = if *is_incoming {
                ("<-", "-")
            } else {
                ("-", "->")
            };
            buf.push_str(&format!(
                "{}Expand {}{}[{}:{}{}]{}{}\n",
                pad, src_var, left, rel_var, rtype, range, right, dst_var
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Filter { input, expression } => {
            buf.push_str(&format!("{}Filter {}\n", pad, fmt_filter(expression)));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
        } => {
            let cols: Vec<String> = items
                .iter()
                .map(|(e, alias)| {
                    if let Some(a) = alias {
                        format!("{} AS {}", fmt_expr(e), a)
                    } else {
                        fmt_expr(e)
                    }
                })
                .collect();
            let barrier = if *is_barrier { " [barrier]" } else { "" };
            buf.push_str(&format!(
                "{}Project [{}]{}\n",
                pad,
                cols.join(", "),
                barrier
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
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
            let lo_s = match lo {
                Some(e) => format!("{}{}", if *lo_inclusive { ">=" } else { ">" }, fmt_expr(e)),
                None => String::new(),
            };
            let hi_s = match hi {
                Some(e) => format!("{}{}", if *hi_inclusive { "<=" } else { "<" }, fmt_expr(e)),
                None => String::new(),
            };
            let range = match (lo_s.is_empty(), hi_s.is_empty()) {
                (false, false) => format!("{} AND {}", lo_s, hi_s),
                (false, true) => lo_s,
                (true, false) => hi_s,
                (true, true) => "*".to_string(),
            };
            buf.push_str(&format!(
                "{}NodeRangeScan {}:{}.{} {}\n",
                pad, variable, label, property, range
            ));
        }
        PhysicalOperator::HashJoin { left, right } => {
            buf.push_str(&format!("{}HashJoin\n", pad));
            buf.push_str(&format_physical_plan(left, depth + 1));
            buf.push_str(&format_physical_plan(right, depth + 1));
        }
        PhysicalOperator::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            let groups: Vec<String> = group_by.iter().map(|(e, _)| fmt_expr(e)).collect();
            let aggs: Vec<String> = aggregations
                .iter()
                .map(|(f, e, alias)| format!("{}({}) AS {}", fmt_agg(f), fmt_expr(e), alias))
                .collect();
            buf.push_str(&format!(
                "{}Aggregate group=[{}] agg=[{}]\n",
                pad,
                groups.join(", "),
                aggs.join(", ")
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Sort { input, items } => {
            let keys: Vec<String> = items
                .iter()
                .map(|s| {
                    format!(
                        "{} {}",
                        fmt_expr(&s.expr),
                        if s.ascending { "ASC" } else { "DESC" }
                    )
                })
                .collect();
            buf.push_str(&format!("{}Sort [{}]\n", pad, keys.join(", ")));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Limit { input, skip, count } => {
            buf.push_str(&format!("{}Limit skip={} count={}\n", pad, skip, count));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Unwind {
            input,
            expr,
            variable,
        } => {
            buf.push_str(&format!(
                "{}Unwind {} AS {}\n",
                pad,
                fmt_expr(expr),
                variable
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::OptionalMatch { input, null_vars } => {
            buf.push_str(&format!(
                "{}OptionalMatch null_vars=[{}]\n",
                pad,
                null_vars.join(", ")
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::Distinct { input, keys } => {
            match keys {
                Some(keys) => buf.push_str(&format!("{}Distinct [{}]\n", pad, keys.join(", "))),
                None => buf.push_str(&format!("{}Distinct\n", pad)),
            }
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::WritePart { input, part } => {
            let part_name = match part {
                crate::ast::QueryPart::Create { .. } => "Create",
                crate::ast::QueryPart::Merge { .. } => "Merge",
                crate::ast::QueryPart::Set { .. } => "Set",
                crate::ast::QueryPart::Delete { .. } => "Delete",
                _ => "WritePart",
            };
            buf.push_str(&format!("{}WritePart({})\n", pad, part_name));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::ProcedureCall {
            input, output_vars, ..
        } => {
            buf.push_str(&format!(
                "{}ProcedureCall({})\n",
                pad,
                output_vars.join(", ")
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::MultiwayJoin {
            input,
            closing_src_var,
            closing_dst_var,
            closing_rel_type,
            closing_rel_var,
            closing_is_incoming,
            closing_is_undirected,
            ..
        } => {
            let dir = if *closing_is_undirected {
                "-"
            } else if *closing_is_incoming {
                "<-"
            } else {
                "->"
            };
            let rel = closing_rel_type.as_deref().unwrap_or("*");
            buf.push_str(&format!(
                "{}MultiwayJoin ({}{dir}[{closing_rel_var}:{rel}]{dir}{closing_dst_var})\n",
                pad, closing_src_var
            ));
            buf.push_str(&format_physical_plan(input, depth + 1));
        }
        PhysicalOperator::TriangleCount {
            rel_types,
            labels,
            output,
        } => {
            let lbl = |l: &Option<String>| l.as_deref().unwrap_or("").to_string();
            let rel = |r: &Option<String>| r.as_deref().unwrap_or("*").to_string();
            buf.push_str(&format!(
                "{}TriangleCount (a{la})-[:{r1}]->(b{lb})-[:{r2}]->(c{lc})-[:{r3}]->(a) AS {output}\n",
                pad,
                la = if labels[0].is_some() {
                    format!(":{}", lbl(&labels[0]))
                } else {
                    String::new()
                },
                lb = if labels[1].is_some() {
                    format!(":{}", lbl(&labels[1]))
                } else {
                    String::new()
                },
                lc = if labels[2].is_some() {
                    format!(":{}", lbl(&labels[2]))
                } else {
                    String::new()
                },
                r1 = rel(&rel_types[0]),
                r2 = rel(&rel_types[1]),
                r3 = rel(&rel_types[2]),
            ));
        }
        PhysicalOperator::PathCount {
            rel_types,
            labels,
            vertex_filters,
            output,
        } => {
            let cmp = |c: &VertexCmp| match c {
                VertexCmp::Eq => "=",
                VertexCmp::Lt => "<",
                VertexCmp::Le => "<=",
                VertexCmp::Gt => ">",
                VertexCmp::Ge => ">=",
            };
            let node = |i: usize| {
                let label = match labels.get(i).and_then(|l| l.as_deref()) {
                    Some(l) => format!(":{l}"),
                    None => String::new(),
                };
                let preds = match vertex_filters.get(i) {
                    Some(ps) if !ps.is_empty() => {
                        let parts: Vec<String> = ps
                            .iter()
                            .map(|p| {
                                format!(
                                    "v{i}.{} {} {}",
                                    p.property,
                                    cmp(&p.cmp),
                                    fmt_expr(&p.value)
                                )
                            })
                            .collect();
                        format!(" {{{}}}", parts.join(", "))
                    }
                    _ => String::new(),
                };
                format!("(v{i}{label}{preds})")
            };
            let mut pattern = node(0);
            for (h, rt) in rel_types.iter().enumerate() {
                let rel = rt.as_deref().unwrap_or("*");
                pattern.push_str(&format!("-[:{rel}]->{}", node(h + 1)));
            }
            buf.push_str(&format!("{pad}PathCount {pattern} AS {output}\n"));
        }
        PhysicalOperator::GroupedDegree {
            rel_type,
            group_is_dst,
            group_label,
            counted_label,
            counted_nonnull_prop,
            group_var,
            group_by,
            output,
        } => {
            let rel = rel_type.as_deref().unwrap_or("*");
            let glab = group_label
                .as_deref()
                .map(|l| format!(":{l}"))
                .unwrap_or_default();
            let clab = counted_label
                .as_deref()
                .map(|l| format!(":{l}"))
                .unwrap_or_default();
            let arrow = if *group_is_dst {
                format!("(c{clab})-[:{rel}]->({group_var}{glab})")
            } else {
                format!("({group_var}{glab})-[:{rel}]->(c{clab})")
            };
            let keys: Vec<String> = group_by.iter().map(|(e, _)| fmt_expr(e)).collect();
            let counted = match counted_nonnull_prop {
                Some(p) => format!("count(c.{p})"),
                None => "count(*)".to_string(),
            };
            buf.push_str(&format!(
                "{pad}GroupedDegree {arrow} group=[{}] {counted} AS {output}\n",
                keys.join(", ")
            ));
        }
        PhysicalOperator::VectorTopK {
            variable,
            label,
            query,
            k,
            prop_filters,
        } => {
            let lbl = label.as_deref().unwrap_or("*");
            let filters = if prop_filters.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = prop_filters
                    .iter()
                    .map(|(p, v)| format!("{variable}.{p} = {}", fmt_expr(v)))
                    .collect();
                format!(" {{{}}}", parts.join(", "))
            };
            buf.push_str(&format!(
                "{pad}VectorTopK {variable}:{lbl}{filters} order=vector_dist({variable}, {}) k={k}\n",
                fmt_expr(query)
            ));
        }
    }

    buf
}

fn fmt_expr(expr: &Expr) -> String {
    match expr {
        Expr::Prop(var, prop) if prop.is_empty() => var.clone(),
        Expr::Prop(var, prop) => format!("{}.{}", var, prop),
        Expr::Literal(lit) => fmt_literal(lit),
        Expr::Param(name) => format!("${}", name),
        Expr::CountStar => "count(*)".to_string(),
        Expr::Agg(f, inner) => format!("{}({})", fmt_agg(f), fmt_expr(inner)),
        _ => "expr".to_string(),
    }
}

fn fmt_literal(lit: &Literal) -> String {
    match lit {
        Literal::Str(s) => format!("'{}'", s),
        Literal::Int(i) => i.to_string(),
        Literal::Float(f) => format!("{}", f),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "null".to_string(),
        Literal::List(items) => format!(
            "[{}]",
            items.iter().map(fmt_literal).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn fmt_filter(expr: &FilterExpr) -> String {
    match expr {
        FilterExpr::Eq(l, r) => format!("{} = {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Ne(l, r) => format!("{} <> {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Lt(l, r) => format!("{} < {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Gt(l, r) => format!("{} > {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Le(l, r) => format!("{} <= {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::Ge(l, r) => format!("{} >= {}", fmt_expr(l), fmt_expr(r)),
        FilterExpr::HasLabel(var, label) => format!("{}:{}", var, label),
        FilterExpr::Expr(e) => fmt_expr(e),
    }
}

fn fmt_agg(f: &AggFn) -> String {
    match f {
        AggFn::Count { distinct: true } => "count(DISTINCT".to_string(),
        AggFn::Count { distinct: false } => "count".to_string(),
        AggFn::Sum { .. } => "sum".to_string(),
        AggFn::Avg { .. } => "avg".to_string(),
        AggFn::Min { .. } => "min".to_string(),
        AggFn::Max { .. } => "max".to_string(),
        AggFn::Collect { .. } => "collect".to_string(),
        AggFn::StDev { .. } => "stDev".to_string(),
        AggFn::StDevP { .. } => "stDevP".to_string(),
        AggFn::PercentileDisc { .. } => "percentileDisc".to_string(),
        AggFn::PercentileCont { .. } => "percentileCont".to_string(),
    }
}
