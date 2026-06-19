//! Columnar fast path for the final projection or aggregation over at most a
//! single-hop expansion.
//!
//! The per-row pipeline pays one `SlotRow` clone per expansion row, another
//! per projection row, and one property-columns lookup per property access.
//! For the plan shapes this module recognizes, none of that is needed: the
//! whole result is computable from flat id columns (one bulk expansion, one
//! bulk label or property filter per predicate, and one bulk property gather
//! per variable), so the executor here works column-at-a-time and builds the
//! result records directly.
//!
//! Recognized shape, from the root down:
//!
//! ```text
//! [Limit]? [Sort]? [Distinct]? Project [Aggregate]? Stage* [Expand(1 hop, directed) Stage*] Leaf
//! Leaf  := LabelScan | NodeByIdSeek | NodeIndexScan | NodeRangeScan
//! Stage := Filter(HasLabel)
//!        | Filter(comparison over single-property reads, literals, and parameters)
//! ```
//!
//! with every projected or grouped expression a single-property read on the
//! leaf or expansion variable. `Limit` is accepted only directly above a
//! `Sort` (the row pipeline's top-N shape; a bare `Limit` short-circuits the
//! streaming row path, which the columnar path could not match). Over a plain
//! `Project`, every sort key must resolve to a single-property read on a
//! pipeline variable, either directly (`ORDER BY p.age`) or through a
//! projected alias (`ORDER BY age` for `p.age AS age`); the sort then runs as
//! a bulk key gather plus an index sort, and only the surviving window is
//! projected. Over an `Aggregate` the output is small, so the regular
//! `sort_all` runs on it. A `Distinct` with no limit is transparent here
//! because the caller deduplicates the built records; under a limited sort
//! the row pipeline deduplicates before the limit truncates, so the
//! projection root dedups in-path on the raw projected cells (keeping the
//! first occurrence in row order) before the sort window, which requires the
//! plan's dedup keys to be exactly the projected columns and an `Aggregate`
//! root to decline.
//!
//! Anything else returns `None` and the row pipeline runs unchanged, so
//! correctness never depends on this recognizer; only performance does. Row
//! order, group identity, predicate three-valued logic, aggregate typing, and
//! error surfaces match the row pipeline exactly: the leaf ids come from the
//! shared leaf evaluator (ascending for a label scan), the expansion is
//! flattened per source in scan order, each filter stage compacts the id
//! columns order-preservingly under the same comparison semantics
//! (`evaluate_where` for the structured forms, `eval_binary_op` for
//! comparisons inside `FilterExpr::Expr`), the sort comparator is `sort_all`'s
//! (`json_cmp` falling back to `json_cmp_total`, input index as the tiebreak),
//! the aggregation reuses `AggState`, and the operators above the aggregate
//! are the regular `project_rows` and `sort_all`.

use std::collections::HashMap;

use issundb_core::{Graph, NodeId};
use serde_json::Value;

use crate::ast::{AggFn, BinaryOperator, Expr, ReturnClause, SortItem};
use crate::plan::{FilterExpr, PhysicalOperator};

use super::expr::{cypher_eq, evaluate_expr, is_nan, json_cmp};
use super::read::{
    AggState, eval_leaf, expand_multi_type, group_by_column_name, json_cmp_total, project_rows,
    projected_key, rows_to_records, sort_all, unpack_sentinels,
};
use super::row::{Bindings, SlotRow, SlotSchema};
use super::{GraphBinding, Record};

/// The recognized pipeline: one id-producing leaf, an optional single-hop
/// expansion, vectorizable filter stages, and a projection or aggregation
/// root.
struct VecPipeline<'a> {
    leaf: VecLeaf<'a>,
    /// The variable the leaf binds (and the first expansion starts from); also
    /// `chain_vars[0]`.
    src_var: &'a str,
    /// One id column per node variable in the chain, in column order: the leaf
    /// variable, then each expansion's destination. Length is `expands.len()
    /// + 1`. A property read resolves a variable to its index here.
    chain_vars: Vec<&'a str>,
    /// The directed single hops, in bottom-up (execution) order: `expands[0]`
    /// starts from the leaf, `expands[k]` from `expands[k-1]`'s destination.
    /// Empty for a leaf-only pipeline, one entry for a single hop, two for a
    /// two-hop chain.
    expands: Vec<VecExpand<'a>>,
    /// Filter stages by chain level, in bottom-up application order:
    /// `stages[0]` runs over the leaf column before any expansion, and
    /// `stages[k]` runs after `expands[k-1]`. Length is `expands.len() + 1`.
    /// Within each level the stages are in bottom-up order.
    stages: Vec<Vec<VecStage<'a>>>,
    root: VecRoot<'a>,
    /// Resolved sort keys when a `Sort` sits over a plain projection root.
    project_sort: Option<Vec<VecSortKey<'a>>>,
    /// `(skip, count)` from a `Limit` directly above the `Sort`.
    limit: Option<(usize, usize)>,
    /// A `Distinct` under a limited sort: the projection root deduplicates
    /// on the projected cells, in row order, before the sort window.
    distinct: bool,
}

/// One project-root sort key, resolved to a bulk-gatherable property read on
/// the chain column at index `col`.
struct VecSortKey<'a> {
    col: usize,
    prop: &'a str,
    ascending: bool,
}

enum VecLeaf<'a> {
    /// Ids come from `nodes_by_label` (or `all_nodes`), ascending.
    LabelScan { label: Option<&'a str> },
    /// `NodeByIdSeek`, `NodeIndexScan`, or `NodeRangeScan`: ids come from the
    /// row pipeline's own leaf evaluator, preserving its order and checks.
    Seek(&'a PhysicalOperator),
}

#[derive(Clone, Copy)]
struct VecExpand<'a> {
    src_var: &'a str,
    rel_var: &'a str,
    dst_var: &'a str,
    rel_type: Option<&'a str>,
    is_incoming: bool,
}

/// One vectorizable filter stage.
enum VecStage<'a> {
    /// `FilterExpr::HasLabel`: a bulk label-membership test.
    HasLabel { var: &'a str, label: &'a str },
    /// A single comparison whose operands are single-property reads on
    /// pipeline variables, literals, or parameters.
    Cmp {
        /// True for the structured `FilterExpr::Eq..Ge` forms, which follow
        /// `evaluate_where`; false for a comparison inside `FilterExpr::Expr`,
        /// which follows `eval_binary_op`. The two differ on NaN and null
        /// handling, so each is mirrored exactly.
        structured: bool,
        op: CmpOp,
        lhs: VecOperand<'a>,
        rhs: VecOperand<'a>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

enum VecOperand<'a> {
    /// A single-property read on a pipeline variable.
    Col { var: &'a str, prop: &'a str },
    /// A literal or parameter, evaluated once per stage application.
    Const(&'a Expr),
}

impl<'a> VecStage<'a> {
    /// Pipeline variables the stage reads.
    fn vars(&self) -> Vec<&'a str> {
        match self {
            VecStage::HasLabel { var, .. } => vec![var],
            VecStage::Cmp { lhs, rhs, .. } => [lhs, rhs]
                .into_iter()
                .filter_map(|operand| match operand {
                    VecOperand::Col { var, .. } => Some(*var),
                    VecOperand::Const(_) => None,
                })
                .collect(),
        }
    }
}

fn vec_operand(expr: &Expr) -> Option<VecOperand<'_>> {
    match expr {
        Expr::Prop(var, prop) if !prop.is_empty() => Some(VecOperand::Col { var, prop }),
        Expr::Literal(_) | Expr::Param(_) => Some(VecOperand::Const(expr)),
        _ => None,
    }
}

fn cmp_stage<'a>(structured: bool, op: CmpOp, l: &'a Expr, r: &'a Expr) -> Option<VecStage<'a>> {
    Some(VecStage::Cmp {
        structured,
        op,
        lhs: vec_operand(l)?,
        rhs: vec_operand(r)?,
    })
}

/// Match one `Filter` predicate against the vectorizable stage forms, or
/// `None` to decline the whole pipeline.
fn vec_stage(expression: &FilterExpr) -> Option<VecStage<'_>> {
    match expression {
        FilterExpr::HasLabel(var, label) => Some(VecStage::HasLabel { var, label }),
        FilterExpr::Eq(l, r) => cmp_stage(true, CmpOp::Eq, l, r),
        FilterExpr::Ne(l, r) => cmp_stage(true, CmpOp::Ne, l, r),
        FilterExpr::Lt(l, r) => cmp_stage(true, CmpOp::Lt, l, r),
        FilterExpr::Gt(l, r) => cmp_stage(true, CmpOp::Gt, l, r),
        FilterExpr::Le(l, r) => cmp_stage(true, CmpOp::Le, l, r),
        FilterExpr::Ge(l, r) => cmp_stage(true, CmpOp::Ge, l, r),
        FilterExpr::Expr(Expr::BinaryOp { op, left, right }) => {
            let cmp = match op {
                BinaryOperator::Eq => CmpOp::Eq,
                BinaryOperator::Ne => CmpOp::Ne,
                BinaryOperator::Lt => CmpOp::Lt,
                BinaryOperator::Gt => CmpOp::Gt,
                BinaryOperator::Le => CmpOp::Le,
                BinaryOperator::Ge => CmpOp::Ge,
                _ => return None,
            };
            cmp_stage(false, cmp, left, right)
        }
        FilterExpr::Expr(_) => None,
    }
}

enum VecRoot<'a> {
    /// The plan root is the final RETURN projection of single-property reads.
    Project { items: &'a [(Expr, Option<String>)] },
    /// An aggregation feeds the projection (and the optional sort above it).
    Aggregate {
        group_by: &'a [(Expr, Option<String>)],
        aggregations: &'a [(AggFn, Expr, String)],
        project_items: &'a [(Expr, Option<String>)],
        project_is_barrier: bool,
        sort_items: Option<&'a [SortItem]>,
    },
}

/// How one aggregation reads its per-row input.
enum AggIn {
    /// `count(*)` or a non-distinct `count` over a whole bound variable: the
    /// binding is never null in this pipeline, so every row counts.
    RowCount,
    /// A gathered property cell: `(chain_column, index_into_that_column's_props)`.
    Cell(usize, usize),
}

/// Index of `var` among the chain's node variables, or `None` when it names no
/// chain column.
fn col_of(chain_vars: &[&str], var: &str) -> Option<usize> {
    chain_vars.iter().position(|v| *v == var)
}

/// A single-property read on a chain node variable, the only expression form
/// the columnar executor evaluates itself. Returns the chain column index and
/// the property name.
fn prop_read<'a>(expr: &'a Expr, chain_vars: &[&str]) -> Option<(usize, &'a str)> {
    if let Expr::Prop(var, prop) = expr {
        if !prop.is_empty() {
            if let Some(col) = col_of(chain_vars, var) {
                return Some((col, prop.as_str()));
            }
        }
    }
    None
}

/// Resolve the sort keys of a `Sort` over a plain projection root into bulk
/// property reads, or `None` to decline. A key is either a direct property
/// read on a pipeline variable, or a reference to a projected alias whose
/// expression is one; the projected value of such an item is the raw gathered
/// cell, so both forms order rows exactly as `evaluate_sort_key` does.
fn resolve_sort_keys<'a>(
    sort_items: &'a [SortItem],
    items: &'a [(Expr, Option<String>)],
    chain_vars: &[&str],
    rel_vars: &[&str],
) -> Option<Vec<VecSortKey<'a>>> {
    let keys: Vec<String> = items
        .iter()
        .map(|(expr, alias)| projected_key(expr, alias))
        .collect();
    let mut out = Vec::with_capacity(sort_items.len());
    for si in sort_items {
        let (col, prop) = match &si.expr {
            Expr::Prop(var, prop) if prop.is_empty() => {
                // An alias reference. A pipeline variable of the same name
                // would shadow the projected binding in the row pipeline.
                if col_of(chain_vars, var).is_some() || rel_vars.contains(&var.as_str()) {
                    return None;
                }
                let idx = keys.iter().position(|k| k == var)?;
                prop_read(&items[idx].0, chain_vars)?
            }
            expr => {
                let read = prop_read(expr, chain_vars)?;
                // When the property is null, `evaluate_sort_key` falls back
                // to the `var.prop` and bare `prop` projected bindings, so a
                // projected key of either name carrying a different value
                // would diverge from the bulk gather.
                let Expr::Prop(var, _) = expr else {
                    return None;
                };
                let full = format!("{var}.{}", read.1);
                for (key, (item_expr, _)) in keys.iter().zip(items) {
                    if (key == read.1 || *key == full)
                        && prop_read(item_expr, chain_vars) != Some(read)
                    {
                        return None;
                    }
                }
                read
            }
        };
        out.push(VecSortKey {
            col,
            prop,
            ascending: si.ascending,
        });
    }
    Some(out)
}

/// Match `plan` against the recognized shape, or `None` for the row pipeline.
fn recognize(plan: &PhysicalOperator) -> Option<VecPipeline<'_>> {
    // A Limit is recognized only directly above a Sort: the row pipeline's
    // top-N shape. A bare Limit short-circuits the streaming row path.
    let (limit, below_limit) = match plan {
        PhysicalOperator::Limit { input, skip, count }
            if matches!(input.as_ref(), PhysicalOperator::Sort { .. }) =>
        {
            (Some((*skip, *count)), input.as_ref())
        }
        other => (None, other),
    };
    let (sort_items, below_sort) = match below_limit {
        PhysicalOperator::Sort { input, items } => (Some(items.as_slice()), input.as_ref()),
        other => (None, other),
    };
    // RETURN DISTINCT plans a Distinct directly above the projection. With
    // no limit the caller dedups the built records, so the recognizer just
    // sees through it; under a limit the row pipeline deduplicates before
    // the limit truncates, so the executor here must dedup in-path before
    // the window, which needs the dedup key to be exactly the projected
    // columns (checked against the plan keys in the root section below).
    let (has_distinct, distinct_keys, below_distinct) = match below_sort {
        PhysicalOperator::Distinct { input, keys } => (true, keys.as_ref(), input.as_ref()),
        other => (false, None, other),
    };
    let limited_distinct = limit.is_some() && has_distinct;
    let PhysicalOperator::Project {
        input,
        items,
        is_barrier,
    } = below_distinct
    else {
        return None;
    };
    let (root, chain_top) = match input.as_ref() {
        PhysicalOperator::Aggregate {
            input: agg_input,
            group_by,
            aggregations,
        } => (
            VecRoot::Aggregate {
                group_by,
                aggregations,
                project_items: items,
                project_is_barrier: *is_barrier,
                sort_items,
            },
            agg_input.as_ref(),
        ),
        other => {
            // Sort keys over a plain projection evaluate against the
            // projected row; a barrier project drops the pipeline variables,
            // so the bulk gather could diverge from the fallback lookups.
            if sort_items.is_some() && *is_barrier {
                return None;
            }
            (VecRoot::Project { items }, other)
        }
    };

    // Walk the linear chain top-down, alternating a filter region with one
    // directed single hop, capturing up to two hops. `regions_topdown[i]` holds
    // the filters sitting above `expands_topdown[i]`; the final region sits
    // above the leaf. Each expand carries the relationship variables earlier
    // hops bound (openCypher relationship uniqueness), checked below.
    let mut regions_topdown: Vec<Vec<VecStage>> = Vec::new();
    let mut expands_topdown: Vec<(VecExpand, &[String])> = Vec::new();
    let mut cur = chain_top;
    loop {
        let mut region = Vec::new();
        while let PhysicalOperator::Filter { input, expression } = cur {
            region.push(vec_stage(expression)?);
            cur = input.as_ref();
        }
        // Reverse to the bottom-up order the row pipeline applies filters in.
        region.reverse();
        regions_topdown.push(region);
        if expands_topdown.len() >= 2 {
            break;
        }
        match cur {
            PhysicalOperator::Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                rel_type,
                is_incoming,
                is_undirected: false,
                min_hops: 1,
                max_hops: 1,
                unique_rels,
                needs_path: false,
            } => {
                // A self-referencing hop `(a)-->(a)` needs the pre-bound target
                // guard the row pipeline applies; decline it here.
                if src_var == dst_var {
                    return None;
                }
                expands_topdown.push((
                    VecExpand {
                        src_var,
                        rel_var,
                        dst_var,
                        rel_type: rel_type.as_deref(),
                        is_incoming: *is_incoming,
                    },
                    unique_rels.as_slice(),
                ));
                cur = input.as_ref();
            }
            _ => break,
        }
    }

    let (leaf, leaf_var) = match cur {
        PhysicalOperator::LabelScan { variable, label } => (
            VecLeaf::LabelScan {
                label: label.as_deref(),
            },
            variable.as_str(),
        ),
        PhysicalOperator::NodeByIdSeek { variable, .. }
        | PhysicalOperator::NodeIndexScan { variable, .. }
        | PhysicalOperator::NodeRangeScan { variable, .. } => {
            (VecLeaf::Seek(cur), variable.as_str())
        }
        _ => return None,
    };
    let src_var = leaf_var;

    // Bottom-up order: the leaf hop first. `stages[0]` is the leaf region;
    // `stages[k]` runs after `expands[k-1]`.
    let expands_meta: Vec<(VecExpand, &[String])> = expands_topdown.into_iter().rev().collect();
    let mut stages: Vec<Vec<VecStage>> = regions_topdown;
    stages.reverse();
    let expands: Vec<VecExpand> = expands_meta.iter().map(|(e, _)| *e).collect();

    // Chain wiring: each hop starts where the previous ended, the node
    // variables are pairwise distinct, and the leaf binds the first source.
    let mut chain_vars: Vec<&str> = vec![src_var];
    for (i, e) in expands.iter().enumerate() {
        let expected_src = if i == 0 {
            src_var
        } else {
            expands[i - 1].dst_var
        };
        if e.src_var != expected_src {
            return None;
        }
        if chain_vars.contains(&e.dst_var) {
            return None;
        }
        chain_vars.push(e.dst_var);
    }
    let rel_vars: Vec<&str> = expands.iter().map(|e| e.rel_var).collect();

    // Relationship uniqueness. Each hop's `unique_rels` must reference only
    // relationship variables inside this chain (otherwise the pattern extends
    // beyond what we captured and uniqueness could remove rows we never see).
    // For a multi-hop chain we additionally require every hop's relationship
    // type to be present and pairwise distinct, so no single edge can fill two
    // hops; relationship uniqueness is then vacuous and the column fan-out,
    // which does not track edge identity, matches the row pipeline exactly.
    for (_, uniq) in &expands_meta {
        if uniq.iter().any(|u| !rel_vars.contains(&u.as_str())) {
            return None;
        }
    }
    if expands.len() >= 2 {
        for (i, a) in expands.iter().enumerate() {
            let ta = a.rel_type?;
            for b in &expands[i + 1..] {
                if b.rel_type == Some(ta) {
                    return None;
                }
            }
        }
    }

    // Stage variable scoping: `stages[k]` runs after `k` expansions, so the
    // bound columns are `chain_vars[0..=k]`; a stage may read only those.
    for (k, level) in stages.iter().enumerate() {
        for stage in level {
            if stage.vars().iter().any(|v| !chain_vars[..=k].contains(v)) {
                return None;
            }
        }
    }

    // Per-root expression eligibility.
    let mut project_sort = None;
    match &root {
        VecRoot::Project { items } => {
            for (expr, _) in items.iter() {
                prop_read(expr, &chain_vars)?;
            }
            if let Some(sis) = sort_items {
                project_sort = Some(resolve_sort_keys(sis, items, &chain_vars, &rel_vars)?);
            }
            if limited_distinct {
                // The in-path dedup keys on every projected cell, so the
                // plan's dedup keys must name exactly the projected columns
                // (RETURN DISTINCT plans them that way; full-row dedup or a
                // key subset would be a different equivalence).
                let mut plan_keys = distinct_keys?.clone();
                let mut projected: Vec<String> = items
                    .iter()
                    .map(|(expr, alias)| projected_key(expr, alias))
                    .collect();
                plan_keys.sort();
                plan_keys.dedup();
                projected.sort();
                projected.dedup();
                if plan_keys != projected {
                    return None;
                }
            }
        }
        VecRoot::Aggregate {
            group_by,
            aggregations,
            ..
        } => {
            // The aggregate root runs the regular operators above the fold,
            // which have no dedup-before-limit step.
            if limited_distinct {
                return None;
            }
            for (expr, _) in group_by.iter() {
                prop_read(expr, &chain_vars)?;
            }
            for (agg_fn, inner, _) in aggregations.iter() {
                let count_like = match inner {
                    Expr::CountStar => matches!(agg_fn, AggFn::Count { .. }),
                    // A whole bound variable is never null here, so a
                    // non-distinct count over it is a row count. Any other
                    // aggregate over a whole variable needs the row value.
                    Expr::Prop(var, prop) if prop.is_empty() => {
                        matches!(agg_fn, AggFn::Count { distinct: false })
                            && (col_of(&chain_vars, var).is_some()
                                || rel_vars.contains(&var.as_str()))
                    }
                    _ => false,
                };
                if !count_like && prop_read(inner, &chain_vars).is_none() {
                    return None;
                }
            }
            // The projection and sort above the aggregate run through the
            // regular operators, so their expressions need no eligibility.
        }
    }

    Some(VecPipeline {
        leaf,
        src_var,
        chain_vars,
        expands,
        stages,
        root,
        project_sort,
        limit,
        distinct: limited_distinct,
    })
}

fn props_table(graph: &Graph, ids: &[NodeId], props: &[&str]) -> Result<Vec<Vec<Value>>, String> {
    if props.is_empty() {
        return Ok(Vec::new());
    }
    graph
        .node_props_json_table(ids, props)
        .map_err(|e| match e {
            issundb_core::Error::NodeNotFound(id) => format!("node not found: {}", id),
            other => other.to_string(),
        })
}

/// Single-property gather as one flat column, avoiding the row-major table's
/// per-row vector allocation.
fn prop_column(graph: &Graph, ids: &[NodeId], prop: &str) -> Result<Vec<Value>, String> {
    graph.node_prop_json_column(ids, prop).map_err(|e| match e {
        issundb_core::Error::NodeNotFound(id) => format!("node not found: {}", id),
        other => other.to_string(),
    })
}

/// The pipeline's flat id columns, one per chain node variable in chain order:
/// the leaf column, then one column per expansion target as each hop runs. All
/// columns share the same length; filter stages compact them in lockstep, so a
/// row's bindings survive or drop together.
struct IdCols {
    cols: Vec<Vec<NodeId>>,
}

impl IdCols {
    fn ids_of(&self, col: usize) -> &[NodeId] {
        &self.cols[col]
    }

    /// Number of rows (every column has this length).
    fn len(&self) -> usize {
        self.cols.first().map_or(0, Vec::len)
    }

    /// Order-preserving compaction by a per-row keep mask, applied to every
    /// column.
    fn compact(&mut self, mask: &[bool]) {
        for col in &mut self.cols {
            let mut w = 0;
            for (i, keep) in mask.iter().enumerate() {
                if *keep {
                    col[w] = col[i];
                    w += 1;
                }
            }
            col.truncate(w);
        }
    }
}

/// A resolved comparison operand: one gathered cell per row, or one constant.
enum OperandVals {
    Cells(Vec<Value>),
    Const(Value),
}

impl OperandVals {
    fn get(&self, i: usize) -> &Value {
        match self {
            OperandVals::Cells(cells) => &cells[i],
            OperandVals::Const(value) => value,
        }
    }
}

fn resolve_operand(
    graph: &Graph,
    operand: &VecOperand<'_>,
    params: &HashMap<String, Value>,
    schema: &std::sync::Arc<SlotSchema>,
    chain_vars: &[&str],
    cols: &IdCols,
) -> Result<OperandVals, String> {
    match operand {
        VecOperand::Col { var, prop } => {
            let col = col_of(chain_vars, var).ok_or("vectorized: unbound stage variable")?;
            let ids = cols.ids_of(col);
            Ok(OperandVals::Cells(prop_column(graph, ids, prop)?))
        }
        // Literals and parameters read no row, so an empty row evaluates them
        // exactly as the row pipeline would, including the missing-parameter
        // error.
        VecOperand::Const(expr) => {
            let empty = SlotRow::empty(schema.clone());
            Ok(OperandVals::Const(evaluate_expr(
                graph, &empty, expr, params,
            )?))
        }
    }
}

/// One comparison outcome, mirroring the row pipeline exactly: a row passes
/// only on TRUE, so the null, NaN, and incomparable outcomes all drop it.
fn cmp_keeps(structured: bool, op: CmpOp, lv: &Value, rv: &Value) -> bool {
    use std::cmp::Ordering;
    if structured {
        // The structured `FilterExpr::Eq..Ge` forms follow `evaluate_where`.
        match op {
            CmpOp::Eq => cypher_eq(lv, rv) == Value::Bool(true),
            CmpOp::Ne => cypher_eq(lv, rv) == Value::Bool(false),
            CmpOp::Lt => json_cmp(lv, rv) == Some(Ordering::Less),
            CmpOp::Gt => json_cmp(lv, rv) == Some(Ordering::Greater),
            CmpOp::Le => matches!(json_cmp(lv, rv), Some(Ordering::Less | Ordering::Equal)),
            CmpOp::Ge => matches!(json_cmp(lv, rv), Some(Ordering::Greater | Ordering::Equal)),
        }
    } else {
        // A comparison inside `FilterExpr::Expr` follows `eval_binary_op`.
        if lv.is_null() || rv.is_null() {
            return false;
        }
        match op {
            CmpOp::Eq => cypher_eq(lv, rv) == Value::Bool(true),
            CmpOp::Ne => cypher_eq(lv, rv) == Value::Bool(false),
            CmpOp::Lt | CmpOp::Gt | CmpOp::Le | CmpOp::Ge => {
                // An ordered comparison with NaN is FALSE against a number and
                // NULL otherwise; neither passes the row.
                if is_nan(lv) || is_nan(rv) {
                    return false;
                }
                match json_cmp(lv, rv) {
                    None => false,
                    Some(c) => match op {
                        CmpOp::Lt => c == Ordering::Less,
                        CmpOp::Gt => c == Ordering::Greater,
                        CmpOp::Le => c != Ordering::Greater,
                        CmpOp::Ge => c != Ordering::Less,
                        CmpOp::Eq | CmpOp::Ne => false,
                    },
                }
            }
        }
    }
}

/// Bulk label-membership set for `ids`: a label smaller than the column comes
/// from one `label_idx` prefix scan, a larger one from point lookups on the
/// distinct ids.
fn label_pass_set(
    graph: &Graph,
    ids: &[NodeId],
    label: &str,
) -> Result<ahash::AHashSet<NodeId>, String> {
    let label_count = graph
        .node_count_by_label(label)
        .map_err(|e| e.to_string())? as usize;
    if label_count <= ids.len() {
        Ok(graph
            .nodes_by_label(label)
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect())
    } else {
        let mut distinct = ids.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        Ok(graph
            .label_filter(&distinct, label)
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect())
    }
}

/// Apply one filter stage to the id columns in place.
fn apply_stage(
    graph: &Graph,
    stage: &VecStage<'_>,
    params: &HashMap<String, Value>,
    schema: &std::sync::Arc<SlotSchema>,
    chain_vars: &[&str],
    cols: &mut IdCols,
) -> Result<(), String> {
    let n = cols.len();
    // The row pipeline evaluates a predicate per row, so over zero rows
    // neither its constants nor its property reads run; a missing parameter
    // must not error here either.
    if n == 0 {
        return Ok(());
    }
    let mask: Vec<bool> = match stage {
        VecStage::HasLabel { var, label } => {
            let col = col_of(chain_vars, var).ok_or("vectorized: unbound stage variable")?;
            let ids = cols.ids_of(col);
            let pass = label_pass_set(graph, ids, label)?;
            ids.iter().map(|id| pass.contains(id)).collect()
        }
        VecStage::Cmp {
            structured,
            op,
            lhs,
            rhs,
        } => {
            if let Some(pruned_mask) =
                try_prune_comparison(graph, lhs, rhs, *op, params, schema, chain_vars, cols)
            {
                pruned_mask
            } else {
                let lv = resolve_operand(graph, lhs, params, schema, chain_vars, cols)?;
                let rv = resolve_operand(graph, rhs, params, schema, chain_vars, cols)?;
                (0..n)
                    .map(|i| cmp_keeps(*structured, *op, lv.get(i), rv.get(i)))
                    .collect()
            }
        }
    };
    cols.compact(&mask);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn try_prune_comparison(
    graph: &Graph,
    lhs: &VecOperand<'_>,
    rhs: &VecOperand<'_>,
    op: CmpOp,
    params: &HashMap<String, Value>,
    schema: &std::sync::Arc<SlotSchema>,
    chain_vars: &[&str],
    cols: &IdCols,
) -> Option<Vec<bool>> {
    // Pruning by the property's global min/max only narrows a comparison on the
    // leaf column (col 0); a `node_prop_min_max` bound holds over every node, so
    // an out-of-range constant means no row passes regardless of column, but the
    // leaf restriction matches the prior behavior and keeps the check simple.
    let is_leaf = |var: &str| col_of(chain_vars, var) == Some(0);
    let (prop, const_expr, is_col_lhs) = match (lhs, rhs) {
        (VecOperand::Col { var, prop }, VecOperand::Const(expr)) if is_leaf(var) => {
            (prop, expr, true)
        }
        (VecOperand::Const(expr), VecOperand::Col { var, prop }) if is_leaf(var) => {
            (prop, expr, false)
        }
        _ => return None,
    };

    let mm = graph.node_prop_min_max(prop).ok()??;
    let const_val =
        evaluate_expr(graph, &SlotRow::empty(schema.clone()), const_expr, params).ok()?;
    if const_val.is_null() {
        return None;
    }

    let (min_val, max_val) = mm;
    let n = cols.len();

    let impossible = match op {
        CmpOp::Eq => {
            json_cmp(&const_val, &min_val) == Some(std::cmp::Ordering::Less)
                || json_cmp(&const_val, &max_val) == Some(std::cmp::Ordering::Greater)
        }
        CmpOp::Lt => {
            if is_col_lhs {
                matches!(
                    json_cmp(&const_val, &min_val),
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                )
            } else {
                matches!(
                    json_cmp(&const_val, &max_val),
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                )
            }
        }
        CmpOp::Gt => {
            if is_col_lhs {
                matches!(
                    json_cmp(&const_val, &max_val),
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                )
            } else {
                matches!(
                    json_cmp(&const_val, &min_val),
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                )
            }
        }
        CmpOp::Le => {
            if is_col_lhs {
                json_cmp(&const_val, &min_val) == Some(std::cmp::Ordering::Less)
            } else {
                json_cmp(&const_val, &max_val) == Some(std::cmp::Ordering::Greater)
            }
        }
        CmpOp::Ge => {
            if is_col_lhs {
                json_cmp(&const_val, &max_val) == Some(std::cmp::Ordering::Greater)
            } else {
                json_cmp(&const_val, &min_val) == Some(std::cmp::Ordering::Less)
            }
        }
        CmpOp::Ne => false,
    };

    if impossible {
        Some(vec![false; n])
    } else {
        None
    }
}

/// Extract the leaf's node ids through the row pipeline's leaf evaluator,
/// preserving its emission order. `Ok(None)` declines (the leaf bound
/// something other than a plain node).
fn leaf_node_ids(
    graph: &Graph,
    op: &PhysicalOperator,
    var: &str,
    params: &HashMap<String, Value>,
    schema: &std::sync::Arc<SlotSchema>,
) -> Result<Option<Vec<NodeId>>, String> {
    let rows = eval_leaf(graph, op, params, schema)?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        match row.get_binding(var) {
            Some(GraphBinding::Node(n)) => ids.push(*n),
            _ => return Ok(None),
        }
    }
    Ok(Some(ids))
}

#[cfg(test)]
thread_local! {
    /// Test-only switch: when true, `try_execute_vectorized` declines every
    /// plan, so the row pipeline executes the identical optimized plan and
    /// the differential tests compare the two executors and nothing else.
    static DISABLE_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Execute `plan` column-at-a-time if it matches the recognized shape,
/// producing the final result records directly. `Ok(None)` means the plan is
/// not eligible and the row pipeline must run instead.
/// Gather one raw key column per project-root sort item over the id columns,
/// reading each key from its chain column.
fn gather_sort_key_cols(
    graph: &Graph,
    sort_keys: &[VecSortKey],
    cols: &[Vec<NodeId>],
) -> Result<Vec<Vec<Value>>, String> {
    let mut key_cols = Vec::with_capacity(sort_keys.len());
    for key in sort_keys {
        key_cols.push(prop_column(graph, &cols[key.col], key.prop)?);
    }
    Ok(key_cols)
}

/// Order the row indices `0..n` with `sort_all`'s comparator (`json_cmp`
/// falling back to `json_cmp_total`, the input index as the tiebreak) over
/// the gathered key columns and return the limit window. The index tiebreak
/// makes the comparator a total order, so a partition at the limit boundary
/// selects exactly the rows a full stable sort would put first.
fn sorted_window(
    key_cols: &[Vec<Value>],
    sort_keys: &[VecSortKey],
    n: usize,
    limit: Option<(usize, usize)>,
) -> Vec<usize> {
    let cmp = |&a: &usize, &b: &usize| {
        for (k, key) in sort_keys.iter().enumerate() {
            let (ka, kb) = (&key_cols[k][a], &key_cols[k][b]);
            let ord = json_cmp(ka, kb).unwrap_or_else(|| json_cmp_total(ka, kb));
            let ord = if key.ascending { ord } else { ord.reverse() };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        a.cmp(&b)
    };
    let mut order: Vec<usize> = (0..n).collect();
    match limit {
        Some((skip, count)) => {
            let hi = skip.saturating_add(count).min(n);
            if hi > 0 {
                if hi < n {
                    order.select_nth_unstable_by(hi - 1, cmp);
                }
                order[..hi].sort_by(cmp);
            }
            order.truncate(hi);
            order.drain(..skip.min(hi));
            order
        }
        None => {
            order.sort_by(cmp);
            order
        }
    }
}

pub(super) fn try_execute_vectorized(
    graph: &Graph,
    plan: &PhysicalOperator,
    return_clause: &ReturnClause,
    params: &HashMap<String, Value>,
    schema: &std::sync::Arc<SlotSchema>,
) -> Result<Option<Vec<Record>>, String> {
    #[cfg(test)]
    if DISABLE_FOR_TEST.with(|d| d.get()) {
        return Ok(None);
    }
    let Some(p) = recognize(plan) else {
        return Ok(None);
    };

    // For the projection root, the records are built positionally from the
    // plan items, so the plan items must correspond one-to-one (by projected
    // key) to the RETURN items the caller will name the columns after, and
    // the keys must be distinct (the row pipeline resolves duplicate keys by
    // last-bind-wins, which positional construction would not reproduce).
    if let VecRoot::Project { items } = &p.root {
        if return_clause.items.len() != items.len() {
            return Ok(None);
        }
        let keys: Vec<String> = items
            .iter()
            .map(|(expr, alias)| projected_key(expr, alias))
            .collect();
        for (item, key) in return_clause.items.iter().zip(&keys) {
            if projected_key(&item.expr, &item.alias) != *key {
                return Ok(None);
            }
        }
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        if unique.len() != keys.len() {
            return Ok(None);
        }
    }

    // 1. Leaf ids, exactly as the row pipeline's leaf emits them: ascending
    //    for a label scan, the leaf evaluator's order for a seek.
    let leaf_ids = match p.leaf {
        VecLeaf::LabelScan { label } => match label {
            Some(l) => graph.nodes_by_label(l).map_err(|e| e.to_string())?,
            None => graph.all_nodes().map_err(|e| e.to_string())?,
        },
        VecLeaf::Seek(op) => match leaf_node_ids(graph, op, p.src_var, params, schema)? {
            Some(ids) => ids,
            None => return Ok(None),
        },
    };
    let mut cols = IdCols {
        cols: vec![leaf_ids],
    };

    // 2. Apply the leaf-level stages, then each hop followed by that hop's
    //    stages, matching the row pipeline's bottom-up filter order. Each hop
    //    bulk-expands the current last column (distinct sources only, sorted
    //    like the row pipeline's chain frontier), then fans out every column in
    //    lockstep, iterating the current rows in order so the emitted row order
    //    matches the row pipeline's depth-first chain threading.
    for stage in &p.stages[0] {
        apply_stage(graph, stage, params, schema, &p.chain_vars, &mut cols)?;
    }
    for (k, x) in p.expands.iter().enumerate() {
        let width = cols.cols.len();
        let mut distinct: Vec<NodeId> = cols.cols[width - 1].clone();
        distinct.sort_unstable();
        distinct.dedup();
        let transitions = expand_multi_type(graph, &distinct, x.rel_type, x.is_incoming)?;
        let mut per_src: ahash::AHashMap<NodeId, Vec<NodeId>> = ahash::AHashMap::new();
        for (s, _e, d) in transitions {
            per_src.entry(s).or_default().push(d);
        }
        let n = cols.len();
        let mut new_cols: Vec<Vec<NodeId>> = vec![Vec::new(); width + 1];
        for i in 0..n {
            if let Some(neighbors) = per_src.get(&cols.cols[width - 1][i]) {
                for &d in neighbors {
                    for (new_col, old_col) in new_cols.iter_mut().zip(&cols.cols) {
                        new_col.push(old_col[i]);
                    }
                    new_cols[width].push(d);
                }
            }
        }
        cols = IdCols { cols: new_cols };
        for stage in &p.stages[k + 1] {
            apply_stage(graph, stage, params, schema, &p.chain_vars, &mut cols)?;
        }
    }

    let n = cols.len();
    let id_cols = cols.cols;

    match p.root {
        VecRoot::Project { items } => {
            // One gather per chain column covers every projected property read
            // on its variable; the record cells are moved out of the tables,
            // not re-cloned. `item_cols[i]` is `(chain column, index into that
            // column's props)`.
            let ncols = id_cols.len();
            let mut col_props: Vec<Vec<&str>> = vec![Vec::new(); ncols];
            let mut item_cols = Vec::with_capacity(items.len());
            for (expr, _) in items.iter() {
                // Already validated by `recognize`; decline rather than panic.
                let Some((col, prop)) = prop_read(expr, &p.chain_vars) else {
                    return Ok(None);
                };
                item_cols.push((col, col_props[col].len()));
                col_props[col].push(prop);
            }

            // Reorder every id column by a selected row order.
            let reorder = |ids: &[Vec<NodeId>], sel: &[usize]| -> Vec<Vec<NodeId>> {
                ids.iter()
                    .map(|c| sel.iter().map(|&i| c[i]).collect())
                    .collect()
            };

            if p.distinct {
                // `recognize` sets `distinct` only under a limited sort over
                // this root; decline rather than panic.
                let (Some(sort_keys), Some(limit)) = (&p.project_sort, p.limit) else {
                    return Ok(None);
                };
                let mut tables: Vec<Vec<Vec<Value>>> = id_cols
                    .iter()
                    .zip(&col_props)
                    .map(|(ids, props)| props_table(graph, ids, props))
                    .collect::<Result<_, _>>()?;
                // The Distinct operator runs below the Sort: dedup on the
                // raw projected cells (the values the row pipeline keys its
                // dedup on), keeping the first occurrence in row order, so a
                // sort key that is not projected reads the surviving row's
                // value.
                let mut seen: ahash::AHashSet<String> = ahash::AHashSet::new();
                let mut survivors: Vec<usize> = Vec::new();
                for i in 0..n {
                    use std::fmt::Write as _;
                    let mut key = String::new();
                    for &(col, j) in &item_cols {
                        // `{:?}` escapes string content, so the separator
                        // cannot collide with a cell.
                        let _ = write!(key, "{:?}\x00", tables[col][i][j]);
                    }
                    if seen.insert(key) {
                        survivors.push(i);
                    }
                }
                let surv_cols = reorder(&id_cols, &survivors);
                let key_cols = gather_sort_key_cols(graph, sort_keys, &surv_cols)?;
                // Survivor positions are in row order, so the window's index
                // tiebreak matches `sort_all`'s stable order over the
                // deduplicated stream.
                let selected = sorted_window(&key_cols, sort_keys, survivors.len(), Some(limit));
                let mut records = Vec::with_capacity(selected.len());
                for &s in &selected {
                    let i = survivors[s];
                    let mut values = Vec::with_capacity(items.len());
                    for &(col, j) in &item_cols {
                        let cell = std::mem::take(&mut tables[col][i][j]);
                        values.push(unpack_sentinels(graph, cell));
                    }
                    records.push(Record { values });
                }
                return Ok(Some(records));
            }

            // Sort over the projection: gather one key column per sort item,
            // order the row indices, and keep only the limit window, so the
            // projection below gathers and builds just the surviving rows.
            let (id_cols, n) = match &p.project_sort {
                Some(sort_keys) => {
                    let key_cols = gather_sort_key_cols(graph, sort_keys, &id_cols)?;
                    let selected = sorted_window(&key_cols, sort_keys, n, p.limit);
                    let m = selected.len();
                    (reorder(&id_cols, &selected), m)
                }
                None => (id_cols, n),
            };
            let mut tables: Vec<Vec<Vec<Value>>> = id_cols
                .iter()
                .zip(&col_props)
                .map(|(ids, props)| props_table(graph, ids, props))
                .collect::<Result<_, _>>()?;

            let mut records = Vec::with_capacity(n);
            for i in 0..n {
                let mut values = Vec::with_capacity(items.len());
                for &(col, j) in &item_cols {
                    let cell = std::mem::take(&mut tables[col][i][j]);
                    values.push(unpack_sentinels(graph, cell));
                }
                records.push(Record { values });
            }
            Ok(Some(records))
        }
        VecRoot::Aggregate {
            group_by,
            aggregations,
            project_items,
            project_is_barrier,
            sort_items,
        } => {
            // Group keys come as dense value-identity codes per group column
            // (no per-row value materialization); aggregate inputs come as
            // gathered cells, each consumed exactly once by the fold.
            let ncols = id_cols.len();
            let mut group_codes = Vec::with_capacity(group_by.len());
            for (expr, _) in group_by.iter() {
                // Already validated by `recognize`; decline rather than panic.
                let Some((col, prop)) = prop_read(expr, &p.chain_vars) else {
                    return Ok(None);
                };
                group_codes.push(graph.node_prop_group_codes(&id_cols[col], prop).map_err(
                    |e| match e {
                        issundb_core::Error::NodeNotFound(id) => {
                            format!("node not found: {}", id)
                        }
                        other => other.to_string(),
                    },
                )?);
            }
            // Aggregate inputs gathered per chain column; `AggIn::Cell(col, j)`
            // indexes the j-th gathered property of that column.
            let mut col_props: Vec<Vec<&str>> = vec![Vec::new(); ncols];
            let mut agg_inputs = Vec::with_capacity(aggregations.len());
            for (_, inner, _) in aggregations.iter() {
                match prop_read(inner, &p.chain_vars) {
                    Some((col, prop)) => {
                        agg_inputs.push(AggIn::Cell(col, col_props[col].len()));
                        col_props[col].push(prop);
                    }
                    None => agg_inputs.push(AggIn::RowCount),
                }
            }
            let mut tables: Vec<Vec<Vec<Value>>> = id_cols
                .iter()
                .zip(&col_props)
                .map(|(ids, props)| props_table(graph, ids, props))
                .collect::<Result<_, _>>()?;

            // Composite group key: the per-column codes packed by stride into
            // one integer. Overflow would need the product of per-column
            // cardinalities to exceed `u64`; decline to the row pipeline then.
            let mut strides = vec![1u64; group_by.len()];
            {
                let mut acc: u64 = 1;
                for k in (0..group_by.len()).rev() {
                    strides[k] = acc;
                    match acc.checked_mul(group_codes[k].1.len().max(1) as u64) {
                        Some(next) => acc = next,
                        None => return Ok(None),
                    }
                }
            }

            // Fold rows into groups; each group remembers its per-column
            // codes so the finalize step can rebuild the group values.
            let mut slot_by_key: ahash::AHashMap<u64, usize> = ahash::AHashMap::new();
            let mut groups: Vec<(Vec<u32>, Vec<AggState>)> = Vec::new();
            if group_by.is_empty() {
                let states = aggregations.iter().map(|_| AggState::new()).collect();
                groups.push((Vec::new(), states));
            }
            for i in 0..n {
                let slot = if group_by.is_empty() {
                    0
                } else {
                    let mut key = 0u64;
                    for (k, (codes, _)) in group_codes.iter().enumerate() {
                        key += codes[i] as u64 * strides[k];
                    }
                    *slot_by_key.entry(key).or_insert_with(|| {
                        let codes = group_codes.iter().map(|(codes, _)| codes[i]).collect();
                        let states = aggregations.iter().map(|_| AggState::new()).collect();
                        groups.push((codes, states));
                        groups.len() - 1
                    })
                };
                let states = &mut groups[slot].1;
                for (k, (agg_fn, _, _)) in aggregations.iter().enumerate() {
                    match agg_inputs[k] {
                        AggIn::RowCount => states[k].fold_count_star(),
                        AggIn::Cell(col, j) => {
                            let cell = std::mem::take(&mut tables[col][i][j]);
                            states[k].fold(agg_fn, cell);
                        }
                    }
                }
            }

            // Finalize each group and order the rows by the same serialized
            // key the row pipeline's BTreeMap fold orders by.
            let group_cols: Vec<String> = group_by
                .iter()
                .map(|(expr, alias)| group_by_column_name(expr, alias))
                .collect();
            let mut keyed_rows = Vec::with_capacity(groups.len());
            for (codes, states) in groups {
                let mut key_parts = Vec::with_capacity(group_cols.len());
                let mut gb = SlotRow::empty(schema.clone());
                for (k, col) in group_cols.iter().enumerate() {
                    let rep = &group_codes[k].1[codes[k] as usize];
                    key_parts.push(rep.to_string());
                    gb.bind_local(col, GraphBinding::Scalar(rep.clone()));
                }
                for (k, (agg_fn, _, col)) in aggregations.iter().enumerate() {
                    let val = states[k].finalize(graph, agg_fn, params)?;
                    gb.bind_local(col, GraphBinding::Scalar(val));
                }
                keyed_rows.push((key_parts.join("\x00"), gb));
            }
            keyed_rows.sort_by(|a, b| a.0.cmp(&b.0));
            let agg_rows: Vec<SlotRow> = keyed_rows.into_iter().map(|(_, gb)| gb).collect();

            // The grouped row set is small; the operators above the
            // aggregate are the regular ones, so their semantics cannot
            // diverge from the row pipeline.
            let rows = project_rows(graph, agg_rows, project_items, project_is_barrier, params)?;
            let bound = p.limit.map(|(skip, count)| skip.saturating_add(count));
            let rows = match sort_items {
                Some(items) => sort_all(graph, rows, items, bound, params),
                None => rows,
            };
            let mut records = rows_to_records(graph, &return_clause.items, rows)?;
            if let Some((skip, count)) = p.limit {
                if skip > 0 {
                    records.drain(..skip.min(records.len()));
                }
                records.truncate(count);
            }
            Ok(Some(records))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::execute;
    use crate::parser;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};
    use issundb_core::Graph;
    use serde_json::json;
    use tempfile::TempDir;

    fn optimized_plan(graph: &Graph, cypher: &str) -> PhysicalOperator {
        let stmt = parser::parse(cypher).unwrap();
        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };
        let logical = LogicalPlanner::plan(&query).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        Optimizer::optimize(physical, Some(graph))
    }

    /// A small Person/KNOWS graph with parallel edges, a missing property, a
    /// mixed-kind property, a multi-label node, and a differently labeled node.
    fn fixture() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ada = g
            .add_node_multi(
                &["Person", "Vip"],
                &json!({"name": "ada", "age": 36, "city": "london"}),
            )
            .unwrap();
        let bob = g
            .add_node("Person", &json!({"name": "bob", "age": 4, "city": "oslo"}))
            .unwrap();
        // No city, and age is a string here (mixed-kind column).
        let cal = g
            .add_node("Person", &json!({"name": "cal", "age": "old"}))
            .unwrap();
        let dot = g
            .add_node("Robot", &json!({"name": "dot", "age": 1, "city": "oslo"}))
            .unwrap();
        g.add_edge(ada, bob, "KNOWS", &json!({})).unwrap();
        // Parallel edge: two rows for the same (src, dst) pair.
        g.add_edge(ada, bob, "KNOWS", &json!({})).unwrap();
        g.add_edge(bob, cal, "KNOWS", &json!({})).unwrap();
        g.add_edge(cal, ada, "KNOWS", &json!({})).unwrap();
        g.add_edge(dot, ada, "KNOWS", &json!({})).unwrap();
        g.add_edge(ada, dot, "LIKES", &json!({})).unwrap();
        // Self-loop.
        g.add_edge(bob, bob, "KNOWS", &json!({})).unwrap();
        (dir, g)
    }

    /// Run `cypher` with the fast path declined, so the row pipeline executes
    /// the identical optimized plan.
    fn row_path_execute(
        graph: &Graph,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<crate::QueryResult, crate::CypherError> {
        DISABLE_FOR_TEST.with(|d| d.set(true));
        let out = execute(graph, cypher, params);
        DISABLE_FOR_TEST.with(|d| d.set(false));
        out
    }

    /// Run `cypher` (vectorized-eligible) through both executors over the same
    /// optimized plan, asserting identical columns and records, in order, or
    /// an identical error.
    fn assert_matches_row_path(graph: &Graph, cypher: &str) {
        assert_matches_row_path_with_params(graph, cypher, &std::collections::HashMap::new());
    }

    fn assert_matches_row_path_with_params(
        graph: &Graph,
        cypher: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) {
        let plan = optimized_plan(graph, cypher);
        assert!(
            recognize(&plan).is_some(),
            "expected a vectorized-eligible plan for: {cypher}\n{plan:?}"
        );
        let fast = execute(graph, cypher, params);
        let slow = row_path_execute(graph, cypher, params);
        match (fast, slow) {
            (Ok(fast), Ok(slow)) => {
                assert_eq!(fast.columns, slow.columns, "columns for: {cypher}");
                let fast_rows: Vec<_> = fast.records.iter().map(|r| &r.values).collect();
                let slow_rows: Vec<_> = slow.records.iter().map(|r| &r.values).collect();
                assert_eq!(fast_rows, slow_rows, "records for: {cypher}");
            }
            (Err(fast), Err(slow)) => {
                assert_eq!(fast.to_string(), slow.to_string(), "errors for: {cypher}");
            }
            (fast, slow) => {
                panic!("one path errored for: {cypher}\nfast: {fast:?}\nslow: {slow:?}")
            }
        }
    }

    #[test]
    fn recognizes_projection_and_aggregate_roots() {
        let (_dir, g) = fixture();
        let plan = optimized_plan(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name, b.age AS age",
        );
        assert!(matches!(
            recognize(&plan),
            Some(VecPipeline {
                root: VecRoot::Project { .. },
                ..
            })
        ));

        let plan = optimized_plan(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, count(a) AS n ORDER BY city",
        );
        match recognize(&plan) {
            Some(VecPipeline {
                root: VecRoot::Aggregate { sort_items, .. },
                ..
            }) => assert!(sort_items.is_some(), "ORDER BY rides on the aggregate root"),
            other => panic!(
                "expected an aggregate root, got eligible={}",
                other.is_some()
            ),
        }
    }

    #[test]
    fn bails_on_unsupported_shapes() {
        let (_dir, g) = fixture();
        for cypher in [
            // Var-length hop.
            "MATCH (a:Person)-[:KNOWS*1..2]->(b:Person) RETURN b.name AS name",
            // Undirected hop.
            "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN b.name AS name",
            // Relationship-property predicate.
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE r.weight > 1 RETURN b.name AS name",
            // Arithmetic inside a comparison operand.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age + 1 > 2 RETURN b.name AS name",
            // IS NULL predicate.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.city IS NULL RETURN b.name AS name",
            // OR predicate.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             WHERE b.age > 90 OR b.age < 2 RETURN b.name AS name",
            // Whole-variable comparison operand.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a = b RETURN b.name AS name",
            // Whole-variable projection.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b",
            // Arithmetic sort key.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p.age + 1 ASC LIMIT 2",
            // Whole-node sort key.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p LIMIT 2",
            // Sort key referencing an alias bound to a different property:
            // the row pipeline's null fallback reads the alias binding, so
            // the bulk gather of `p.city` could diverge on a missing city.
            "MATCH (p:Person) RETURN p.name AS city ORDER BY p.city ASC LIMIT 2",
            // LIMIT without a sort stays on the streaming row path.
            "MATCH (p:Person) RETURN p.name AS name LIMIT 2",
            // DISTINCT under a limited sort over an aggregate root: the
            // operators above the fold have no dedup-before-limit step.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.city AS city, count(a) AS n ORDER BY city ASC LIMIT 1",
            // Two-hop chain.
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN c.name AS name",
            // Relationship property projection.
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r.weight AS w",
            // Named path.
            "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name",
            // Self-referencing pattern variable.
            "MATCH (a:Person)-[:KNOWS]->(a) RETURN a.name AS name",
        ] {
            let plan = optimized_plan(&g, cypher);
            assert!(recognize(&plan).is_none(), "must not vectorize: {cypher}");
        }
    }

    #[test]
    fn projection_matches_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.name AS name, b.age AS age, b.city AS city",
            // Mixed source and target properties, repeated property.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN a.name AS an, b.name AS bn, b.name AS bn2",
            // Incoming direction.
            "MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN b.name AS name",
            // Untyped relationship.
            "MATCH (a:Person)-->(b:Person) RETURN b.name AS name",
            // Unlabeled scan, label only on the target.
            "MATCH (a)-[:KNOWS]->(b:Person) RETURN b.name AS name, a.name AS src",
            // Multi-label source pattern (scan plus a HasLabel filter).
            "MATCH (a:Person:Vip)-[:KNOWS]->(b:Person) RETURN b.name AS name",
            // No alias: canonical column names.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, b.age",
            // Empty result: no such label.
            "MATCH (a:Ghost)-[:KNOWS]->(b:Person) RETURN b.name AS name",
            // Empty result: no such relationship type.
            "MATCH (a:Person)-[:HATES]->(b:Person) RETURN b.name AS name",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn aggregation_matches_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, count(a) AS n ORDER BY city",
            // A grouping-free `count(*)` or `count(var)` over a single hop is
            // promoted to the `PathCount` kernel (see `path_count_exec_tests`),
            // so it no longer reaches the vectorized path; `count(b.city)` over
            // a property still does.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(b.city) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(DISTINCT b.city) AS n",
            // Mixed-kind ages: sum and avg skip the non-numeric, min/max use
            // the total order.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN sum(b.age) AS s, avg(b.age) AS av, min(b.age) AS lo, max(b.age) AS hi",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, collect(b.name) AS names ORDER BY city",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN stDev(b.age) AS sd",
            // Group on a source property, sort descending on the count.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN a.city AS city, count(*) AS n ORDER BY n DESC, city",
            // Empty input keeps the grouping-free zero row and drops groups.
            // (The grouping-free `count(a)` form is now a `PathCount` kernel.)
            "MATCH (a:Ghost)-[:KNOWS]->(b:Person) RETURN b.city AS city, count(a) AS n",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn property_filters_match_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            // Range conjuncts on the expansion target.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             WHERE b.age >= 4 AND b.age < 40 RETURN b.name AS name",
            // Equality on a string property, including a node missing it.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.city = 'oslo' RETURN b.name AS name",
            // Inequality.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age <> 4 RETURN b.name AS name",
            // Constant on the left.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE 4 <= b.age RETURN b.name AS name",
            // Two property reads compared to each other.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > b.age RETURN b.name AS name",
            // Mixed-kind column: cal's age is the string \"old\".
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age > 0 RETURN b.name AS name",
            // Source-side property predicate (pushes below the expand).
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 5 RETURN b.name AS name",
            // Filter feeding an aggregation.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             WHERE b.age >= 4 RETURN b.city AS city, count(a) AS n ORDER BY city",
            // Equality predicate on the source: an index-scan leaf when the
            // optimizer rewrites it, a filter stage otherwise.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'ada' RETURN b.name AS name",
            // Comparison with a null literal matches no row.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age = null RETURN b.name AS name",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn scan_only_pipelines_match_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            // Full-scan projection with no expansion.
            "MATCH (p:Person) RETURN p.name AS name, p.age AS age, p.city AS city",
            // Scan-only filter plus projection.
            "MATCH (p:Person) WHERE p.age >= 4 RETURN p.name AS name",
            // Range conjuncts: an index range-scan leaf when rewritten.
            "MATCH (p:Person) WHERE p.age >= 4 AND p.age < 40 RETURN count(p) AS n",
            // Scan-only grouped aggregation.
            "MATCH (p:Person) RETURN p.city AS city, count(*) AS n ORDER BY city",
            // Scan-only equality.
            "MATCH (p:Person) WHERE p.city = 'oslo' RETURN p.name AS name",
            // Unlabeled scan with a filter.
            "MATCH (p) WHERE p.age <= 4 RETURN p.name AS name",
            // Empty result: no such label.
            "MATCH (p:Ghost) WHERE p.age > 1 RETURN p.name AS name",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn order_by_limit_matches_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            // Alias sort keys over a plain projection, mixed directions.
            "MATCH (p:Person) RETURN p.name AS name, p.age AS age \
             ORDER BY age DESC, name ASC LIMIT 2",
            // Direct property sort key that is not projected.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p.age ASC LIMIT 2",
            // Mixed-kind sort column: cal's age is the string \"old\", so the
            // total-order fallback decides cross-type comparisons.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p.age DESC LIMIT 3",
            // ORDER BY without a LIMIT.
            "MATCH (p:Person) RETURN p.name AS name, p.age AS age ORDER BY age ASC",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name ORDER BY name",
            // Sort on the expansion target; parallel edges produce equal rows,
            // so ties fall back to input order.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.name AS name ORDER BY b.age DESC LIMIT 3",
            // Sort key projected without an alias.
            "MATCH (p:Person) RETURN p.name, p.age ORDER BY p.age ASC LIMIT 2",
            // SKIP with LIMIT, and SKIP alone.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY name ASC SKIP 1 LIMIT 2",
            "MATCH (p:Person) RETURN p.name AS name ORDER BY name ASC SKIP 2",
            // LIMIT 0.
            "MATCH (p:Person) RETURN p.name AS name ORDER BY name ASC LIMIT 0",
            // Sort key missing on some nodes (no city on cal).
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p.city ASC",
            // Filter stage under a limited sort.
            "MATCH (p:Person) WHERE p.age >= 4 \
             RETURN p.name AS name ORDER BY p.age DESC LIMIT 2",
            // Aggregate root under Sort and Limit.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, count(a) AS n ORDER BY n DESC, city ASC LIMIT 1",
            // DISTINCT with an unlimited sort.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.name AS name ORDER BY name DESC",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn distinct_limit_matches_row_path() {
        let (_dir, g) = fixture();
        for cypher in [
            // Duplicates collapse before the limit binds: parallel edges and
            // the shared oslo city produce repeated rows.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.name AS name ORDER BY name ASC LIMIT 2",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.city AS city ORDER BY city ASC LIMIT 2",
            // Filter stage under the limited distinct sort.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age >= 4 \
             RETURN DISTINCT b.city AS city ORDER BY city ASC LIMIT 5",
            // Scan-only pipeline.
            "MATCH (p:Person) RETURN DISTINCT p.city AS city ORDER BY city DESC LIMIT 2",
            // Two projected columns, sort on one.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.name AS name, b.city AS city ORDER BY name ASC LIMIT 3",
            // SKIP with LIMIT over the deduplicated rows.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.name AS name ORDER BY name ASC SKIP 1 LIMIT 2",
            // LIMIT 0.
            "MATCH (p:Person) RETURN DISTINCT p.city AS city ORDER BY city ASC LIMIT 0",
            // Missing property: cal has no city, so a null joins the dedup.
            "MATCH (p:Person) RETURN DISTINCT p.city AS city ORDER BY city ASC LIMIT 3",
            // Mixed-kind dedup column: cal's age is the string "old".
            "MATCH (p:Person) RETURN DISTINCT p.age AS age ORDER BY age DESC LIMIT 3",
        ] {
            assert_matches_row_path(&g, cypher);
        }
    }

    #[test]
    fn param_filters_match_row_path() {
        let (_dir, g) = fixture();
        let params: std::collections::HashMap<String, serde_json::Value> =
            [("min".to_string(), json!(4))].into_iter().collect();
        for cypher in [
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age >= $min RETURN b.name AS name",
            "MATCH (p:Person) WHERE p.age >= $min RETURN p.name AS name",
        ] {
            assert_matches_row_path_with_params(&g, cypher, &params);
        }
    }

    #[test]
    fn missing_param_matches_row_path_errors() {
        let (_dir, g) = fixture();
        let params = std::collections::HashMap::new();
        // With matching rows reaching the filter, both paths surface the same
        // missing-parameter error.
        let cypher = "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                      WHERE b.age >= $min RETURN b.name AS name";
        assert!(recognize(&optimized_plan(&g, cypher)).is_some());
        let fast = execute(&g, cypher, &params).unwrap_err();
        let slow = row_path_execute(&g, cypher, &params).unwrap_err();
        assert_eq!(fast.to_string(), slow.to_string());

        // With no rows reaching the filter, neither path evaluates the
        // parameter, so neither errors.
        let cypher = "MATCH (a:Ghost)-[:KNOWS]->(b:Person) \
                      WHERE b.age >= $min RETURN b.name AS name";
        assert!(recognize(&optimized_plan(&g, cypher)).is_some());
        let fast = execute(&g, cypher, &params).unwrap();
        let slow = row_path_execute(&g, cypher, &params).unwrap();
        assert!(fast.records.is_empty() && slow.records.is_empty());
    }

    #[test]
    fn return_distinct_applies_after_the_fast_path() {
        let (_dir, g) = fixture();
        assert_matches_row_path(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT b.name AS name",
        );
    }

    #[test]
    fn mismatched_return_items_decline() {
        let (_dir, g) = fixture();
        let params = std::collections::HashMap::new();
        // The projection-root records are built positionally from the plan
        // items, so a return clause whose keys do not line up with the plan
        // must decline rather than mislabel the values.
        let plan = optimized_plan(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS x, b.age AS y",
        );
        let other =
            parser::parse("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS x, b.age AS z")
                .unwrap();
        let return_clause = match other {
            crate::ast::Statement::Query(q) => q.return_clause,
            _ => unreachable!(),
        };
        let schema = std::sync::Arc::new(SlotSchema::from_plan(&plan));
        let out = try_execute_vectorized(&g, &plan, &return_clause, &params, &schema).unwrap();
        assert!(out.is_none(), "mismatched return items must decline");
    }

    #[test]
    fn writes_after_first_run_stay_visible() {
        let (_dir, g) = fixture();
        let cypher = "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                      RETURN b.city AS city, count(a) AS n ORDER BY city";
        assert_matches_row_path(&g, cypher);

        let eve = g
            .add_node("Person", &json!({"name": "eve", "age": 9, "city": "rome"}))
            .unwrap();
        let ada = g.nodes_by_label("Person").unwrap()[0];
        g.add_edge(ada, eve, "KNOWS", &json!({})).unwrap();
        assert_matches_row_path(&g, cypher);
    }

    #[test]
    fn vectorized_filter_pruning_via_zone_maps() {
        let (_dir, g) = fixture();
        // `age` is mixed-kind in the fixture, so it has no statistics and the
        // prune declines; these exercise the no-stats fallback.
        assert_matches_row_path(
            &g,
            "MATCH (p:Person) WHERE p.age > 100 RETURN p.name AS name",
        );
        assert_matches_row_path(&g, "MATCH (p:Person) WHERE p.age < 0 RETURN p.name AS name");
        // `name` is a clean string column, so these bounds-impossible filters
        // take the prune path end to end.
        assert_matches_row_path(
            &g,
            "MATCH (p:Person) WHERE p.name > 'zzz' RETURN p.name AS name",
        );
        assert_matches_row_path(
            &g,
            "MATCH (p:Person) WHERE p.name = 'zzz' RETURN p.name AS name",
        );
    }

    #[test]
    fn zone_map_prune_emits_all_false_mask_only_when_impossible() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        for age in [4i64, 17, 36] {
            g.add_node("Person", &json!({ "age": age })).unwrap();
        }
        let srcs = g.nodes_by_label("Person").unwrap();
        let n = srcs.len();
        let cols = IdCols { cols: vec![srcs] };
        let chain_vars: [&str; 1] = ["p"];
        let schema = std::sync::Arc::new(SlotSchema::empty());
        let params = HashMap::new();
        let col = VecOperand::Col {
            var: "p",
            prop: "age",
        };

        // Above the maximum: the prune must fire with an all-false mask.
        let high = Expr::Literal(crate::ast::Literal::Int(1000));
        let mask = try_prune_comparison(
            &g,
            &col,
            &VecOperand::Const(&high),
            CmpOp::Gt,
            &params,
            &schema,
            &chain_vars,
            &cols,
        );
        assert_eq!(mask, Some(vec![false; n]));

        // Reversed operands: `1000 < p.age` is equally impossible.
        let mask = try_prune_comparison(
            &g,
            &VecOperand::Const(&high),
            &col,
            CmpOp::Lt,
            &params,
            &schema,
            &chain_vars,
            &cols,
        );
        assert_eq!(mask, Some(vec![false; n]));

        // Inside the bounds: the prune must decline so rows are compared.
        let mid = Expr::Literal(crate::ast::Literal::Int(10));
        let mask = try_prune_comparison(
            &g,
            &col,
            &VecOperand::Const(&mid),
            CmpOp::Gt,
            &params,
            &schema,
            &chain_vars,
            &cols,
        );
        assert!(mask.is_none());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]
        /// Differential check over random multigraphs: the fast path and the
        /// row pipeline must agree, in order, on a projection and on a
        /// sorted aggregation.
        #[test]
        fn vectorized_matches_row_path_on_random_graphs(
            nodes in proptest::collection::vec((0u8..3, 0u8..4, 0u8..4), 1..10),
            edges in proptest::collection::vec((0usize..10, 0usize..10), 0..25),
        ) {
            let dir = TempDir::new().unwrap();
            let g = Graph::open(dir.path(), 1).unwrap();
            let labels = ["P", "Q", "P"];
            let cities = [json!("x"), json!("y"), serde_json::Value::Null, json!(7)];
            let mut ids = Vec::new();
            for (l, c, age) in &nodes {
                let mut props = serde_json::Map::new();
                let city = cities[*c as usize].clone();
                if !city.is_null() {
                    props.insert("city".into(), city);
                }
                props.insert("age".into(), json!(age));
                ids.push(
                    g.add_node(labels[*l as usize], &serde_json::Value::Object(props))
                        .unwrap(),
                );
            }
            for (s, d) in &edges {
                if *s < ids.len() && *d < ids.len() {
                    g.add_edge(ids[*s], ids[*d], "T", &json!({})).unwrap();
                }
            }
            for cypher in [
                "MATCH (a:P)-[:T]->(b:P) RETURN a.city AS ac, b.city AS bc, b.age AS age",
                "MATCH (a:P)-[:T]->(b:P) \
                 RETURN b.city AS city, count(a) AS n, sum(b.age) AS s ORDER BY city",
                // Property predicates over the mixed-kind city column (strings,
                // a number, and missing values) and the numeric age column.
                "MATCH (a:P)-[:T]->(b:P) WHERE b.age >= 2 RETURN b.city AS bc, b.age AS age",
                "MATCH (a:P)-[:T]->(b:P) WHERE a.city = 'x' AND b.age < 3 \
                 RETURN b.city AS bc, count(a) AS n ORDER BY bc",
                "MATCH (a:P) WHERE a.age <> 1 RETURN a.city AS city, a.age AS age",
                // Top-N over the mixed-kind city column (ties, missing values,
                // and cross-type ordering), keyed by alias and by property.
                "MATCH (a:P)-[:T]->(b:P) \
                 RETURN b.city AS city, b.age AS age ORDER BY city DESC, age ASC LIMIT 3",
                "MATCH (a:P) RETURN a.age AS age ORDER BY a.city ASC SKIP 1 LIMIT 2",
                // Limited distinct over the mixed-kind city column: dedup
                // runs before the sort window.
                "MATCH (a:P)-[:T]->(b:P) \
                 RETURN DISTINCT b.city AS city ORDER BY city ASC LIMIT 2",
                "MATCH (a:P) RETURN DISTINCT a.city AS city, a.age AS age \
                 ORDER BY age DESC, city ASC SKIP 1 LIMIT 2",
            ] {
                assert_matches_row_path(&g, cypher);
            }
        }
    }

    /// A two-hop social fixture: people follow people (`F`), and live in cities
    /// (`L`). The two hops carry distinct relationship types, so the columnar
    /// two-hop path is eligible.
    fn two_hop_fixture() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let cities: Vec<_> = ["london", "oslo", "rome"]
            .iter()
            .enumerate()
            .map(|(i, nm)| {
                g.add_node("City", &json!({"name": nm, "cid": i as i64}))
                    .unwrap()
            })
            .collect();
        let ages = [20i64, 30, 40, 60, 25];
        let people: Vec<_> = ages
            .iter()
            .enumerate()
            .map(|(i, age)| {
                g.add_node("Person", &json!({"id": i as i64, "age": age}))
                    .unwrap()
            })
            .collect();
        let follows = [(0, 1), (0, 3), (1, 2), (1, 4), (3, 2), (2, 4), (4, 0)];
        for (s, d) in follows {
            g.add_edge(people[s], people[d], "F", &json!({})).unwrap();
        }
        // Each person lives in one city (functional second hop).
        for (i, &p) in people.iter().enumerate() {
            g.add_edge(p, cities[i % cities.len()], "L", &json!({}))
                .unwrap();
        }
        g.rebuild_csr().unwrap();
        (dir, g)
    }

    /// The benchmark's `top_followed_city` shape (a grouped count over a
    /// two-hop chain with a sort and limit) takes the columnar two-hop path and
    /// matches the row pipeline exactly.
    #[test]
    fn two_hop_grouped_aggregation_matches_row_path() {
        let (_dir, g) = two_hop_fixture();
        let q = "MATCH (f:Person)-[:F]->(p:Person)-[:L]->(c:City) \
                 RETURN p.id AS id, count(f.id) AS num, c.name AS city \
                 ORDER BY num DESC, id LIMIT 1";
        assert!(
            matches!(
                recognize(&optimized_plan(&g, q)),
                Some(VecPipeline {
                    root: VecRoot::Aggregate { .. },
                    ..
                })
            ),
            "two-hop grouped aggregation must take the columnar path"
        );
        assert_matches_row_path(&g, q);
    }

    /// Two hops of the same relationship type, or a third hop, keep the row
    /// pipeline: relationship uniqueness could then remove rows the
    /// edge-identity-free column fan-out cannot, and three hops exceed the
    /// recognized chain length.
    #[test]
    fn two_hop_same_type_or_three_hops_decline() {
        let (_dir, g) = two_hop_fixture();
        for q in [
            "MATCH (a:Person)-[:F]->(b:Person)-[:F]->(c:Person) \
             RETURN c.id AS id, count(a) AS n ORDER BY id",
            "MATCH (a:Person)-[:F]->(b:Person)-[:L]->(c:City)-[:L]->(d:City) \
             RETURN d.name AS city, count(a) AS n",
        ] {
            assert!(
                recognize(&optimized_plan(&g, q)).is_none(),
                "must decline to the row pipeline: {q}"
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]
        /// Differential check for the columnar two-hop path: a projection and a
        /// grouped aggregation over `(a:P)-[:F]->(b:P)-[:L]->(c:C)` must agree,
        /// in order, with the row pipeline over random graphs. The unordered
        /// projection pins the row order of the two-hop column fan-out against
        /// the row pipeline's depth-first chain threading.
        #[test]
        fn two_hop_vectorized_matches_row_path(
            ages in proptest::collection::vec(0u8..4, 2..7),
            f_edges in proptest::collection::vec((0usize..7, 0usize..7), 0..18),
            l_edges in proptest::collection::vec((0usize..7, 0usize..3), 0..14),
        ) {
            let dir = TempDir::new().unwrap();
            let g = Graph::open(dir.path(), 1).unwrap();
            let cids: Vec<_> = ["x", "y", "z"]
                .iter()
                .map(|nm| g.add_node("C", &json!({"name": nm})).unwrap())
                .collect();
            let pids: Vec<_> = ages
                .iter()
                .enumerate()
                .map(|(i, age)| g.add_node("P", &json!({"id": i as i64, "age": *age as i64})).unwrap())
                .collect();
            for (s, d) in &f_edges {
                if *s < pids.len() && *d < pids.len() {
                    g.add_edge(pids[*s], pids[*d], "F", &json!({})).unwrap();
                }
            }
            for (s, c) in &l_edges {
                if *s < pids.len() && *c < cids.len() {
                    g.add_edge(pids[*s], cids[*c], "L", &json!({})).unwrap();
                }
            }
            for cypher in [
                // Unordered projection over all three chain variables: the
                // strongest check of fan-out row order.
                "MATCH (a:P)-[:F]->(b:P)-[:L]->(c:C) RETURN a.id AS aid, b.age AS age, c.name AS city",
                // Grouped count with sort and limit (the benchmark shape).
                "MATCH (a:P)-[:F]->(b:P)-[:L]->(c:C) \
                 RETURN b.id AS id, count(a) AS num, c.name AS city ORDER BY num DESC, id LIMIT 2",
                // A predicate on the middle node before the second hop.
                "MATCH (a:P)-[:F]->(b:P)-[:L]->(c:C) WHERE b.age >= 2 \
                 RETURN c.name AS city, count(a) AS num ORDER BY num DESC, city",
                // Top-N over a two-hop projection with mixed keys.
                "MATCH (a:P)-[:F]->(b:P)-[:L]->(c:C) \
                 RETURN c.name AS city, b.age AS age ORDER BY city DESC, age ASC LIMIT 3",
            ] {
                assert_matches_row_path(&g, cypher);
            }
        }
    }
}
