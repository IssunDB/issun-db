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
//! `sort_all` runs on it. A `Distinct` under a limited sort declines, because
//! the row pipeline deduplicates before the limit truncates while the caller
//! deduplicates the built records afterwards.
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

use issundb_core::{EdgeId, Graph, NodeId};
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
    /// The variable the leaf binds (and the expansion starts from).
    src_var: &'a str,
    expand: Option<VecExpand<'a>>,
    /// Stages between the expansion (or the root chain, when there is no
    /// expansion) and the leaf, in bottom-up application order. They may
    /// reference only `src_var`.
    below_stages: Vec<VecStage<'a>>,
    /// Stages above the expansion, in bottom-up application order.
    above_stages: Vec<VecStage<'a>>,
    root: VecRoot<'a>,
    /// Resolved sort keys when a `Sort` sits over a plain projection root.
    project_sort: Option<Vec<VecSortKey<'a>>>,
    /// `(skip, count)` from a `Limit` directly above the `Sort`.
    limit: Option<(usize, usize)>,
}

/// One project-root sort key, resolved to a bulk-gatherable property read.
struct VecSortKey<'a> {
    is_src: bool,
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

struct VecExpand<'a> {
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
    /// A gathered property column: `(from_src_table, column_index)`.
    Cell(bool, usize),
}

/// A single-property read on the leaf or expansion variable, the only
/// expression form the columnar executor evaluates itself.
fn prop_read<'a>(expr: &'a Expr, src_var: &str, dst_var: Option<&str>) -> Option<(bool, &'a str)> {
    if let Expr::Prop(var, prop) = expr {
        if !prop.is_empty() {
            if var == src_var {
                return Some((true, prop.as_str()));
            }
            if dst_var == Some(var.as_str()) {
                return Some((false, prop.as_str()));
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
    src_var: &str,
    dst_var: Option<&str>,
    rel_var: Option<&str>,
) -> Option<Vec<VecSortKey<'a>>> {
    let keys: Vec<String> = items
        .iter()
        .map(|(expr, alias)| projected_key(expr, alias))
        .collect();
    let mut out = Vec::with_capacity(sort_items.len());
    for si in sort_items {
        let (is_src, prop) = match &si.expr {
            Expr::Prop(var, prop) if prop.is_empty() => {
                // An alias reference. A pipeline variable of the same name
                // would shadow the projected binding in the row pipeline.
                if var == src_var || dst_var == Some(var.as_str()) || rel_var == Some(var.as_str())
                {
                    return None;
                }
                let idx = keys.iter().position(|k| k == var)?;
                prop_read(&items[idx].0, src_var, dst_var)?
            }
            expr => {
                let read = prop_read(expr, src_var, dst_var)?;
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
                        && prop_read(item_expr, src_var, dst_var) != Some(read)
                    {
                        return None;
                    }
                }
                read
            }
        };
        out.push(VecSortKey {
            is_src,
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
    // RETURN DISTINCT plans a Distinct directly above the projection; the
    // caller dedups the built records, so the recognizer sees through it.
    let (has_distinct, below_distinct) = match below_sort {
        PhysicalOperator::Distinct { input, .. } => (true, input.as_ref()),
        other => (false, other),
    };
    // The row pipeline deduplicates below the sort, before the limit
    // truncates; the caller's dedup runs after, so the combination declines.
    if limit.is_some() && has_distinct {
        return None;
    }
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

    // Filter stages under the root chain. Reversed below to the bottom-up
    // order the row pipeline applies them in.
    let mut upper_stages = Vec::new();
    let mut cur = chain_top;
    while let PhysicalOperator::Filter { input, expression } = cur {
        upper_stages.push(vec_stage(expression)?);
        cur = input.as_ref();
    }
    upper_stages.reverse();

    // Optional single-hop directed expansion. Source labels and predicates
    // (`(a:Person:Vip)`, a pushed-down conjunct on `a`) plan as filters
    // between the expand and the leaf.
    let (expand, expand_src, below_stages, above_stages, cur) = match cur {
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
            // A single hop has no sibling relationships; a populated list
            // means the optimizer wired this hop into a larger pattern.
            if !unique_rels.is_empty() {
                return None;
            }
            // A self-referencing hop `(a)-->(a)` needs the pre-bound target
            // guard.
            if src_var == dst_var {
                return None;
            }
            let mut below_stages = Vec::new();
            let mut under = input.as_ref();
            while let PhysicalOperator::Filter { input, expression } = under {
                below_stages.push(vec_stage(expression)?);
                under = input.as_ref();
            }
            below_stages.reverse();
            let expand = VecExpand {
                rel_var,
                dst_var,
                rel_type: rel_type.as_deref(),
                is_incoming: *is_incoming,
            };
            (
                Some(expand),
                Some(src_var.as_str()),
                below_stages,
                upper_stages,
                under,
            )
        }
        other => (None, None, upper_stages, Vec::new(), other),
    };

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
    // With an expansion the leaf must bind the expansion's source variable.
    if let Some(src) = expand_src {
        if leaf_var != src {
            return None;
        }
    }
    let src_var = leaf_var;
    let dst_var = expand.as_ref().map(|x| x.dst_var);

    // Stage variable scoping: below the expansion only the leaf variable is
    // bound; above it, the leaf and target variables are.
    for stage in &below_stages {
        if stage.vars().iter().any(|v| *v != src_var) {
            return None;
        }
    }
    for stage in &above_stages {
        if stage
            .vars()
            .iter()
            .any(|v| *v != src_var && Some(*v) != dst_var)
        {
            return None;
        }
    }

    // Per-root expression eligibility.
    let mut project_sort = None;
    match &root {
        VecRoot::Project { items } => {
            for (expr, _) in items.iter() {
                prop_read(expr, src_var, dst_var)?;
            }
            if let Some(sis) = sort_items {
                let rel_var = expand.as_ref().map(|x| x.rel_var);
                project_sort = Some(resolve_sort_keys(sis, items, src_var, dst_var, rel_var)?);
            }
        }
        VecRoot::Aggregate {
            group_by,
            aggregations,
            ..
        } => {
            for (expr, _) in group_by.iter() {
                prop_read(expr, src_var, dst_var)?;
            }
            for (agg_fn, inner, _) in aggregations.iter() {
                let count_like = match inner {
                    Expr::CountStar => matches!(agg_fn, AggFn::Count { .. }),
                    // A whole bound variable is never null here, so a
                    // non-distinct count over it is a row count. Any other
                    // aggregate over a whole variable needs the row value.
                    Expr::Prop(var, prop) if prop.is_empty() => {
                        matches!(agg_fn, AggFn::Count { distinct: false })
                            && (var == src_var
                                || expand
                                    .as_ref()
                                    .is_some_and(|x| var == x.dst_var || var == x.rel_var))
                    }
                    _ => false,
                };
                if !count_like && prop_read(inner, src_var, dst_var).is_none() {
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
        expand,
        below_stages,
        above_stages,
        root,
        project_sort,
        limit,
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

/// The pipeline's flat id columns: the leaf column, plus the expansion target
/// column once the expansion ran. Filter stages compact both in lockstep, so
/// row pairs survive or drop together.
struct IdCols {
    srcs: Vec<NodeId>,
    dsts: Option<Vec<NodeId>>,
}

impl IdCols {
    fn ids_of(&self, is_src: bool) -> &[NodeId] {
        if is_src {
            &self.srcs
        } else {
            self.dsts.as_deref().unwrap_or(&[])
        }
    }

    /// Order-preserving compaction by a per-row keep mask.
    fn compact(&mut self, mask: &[bool]) {
        let mut w = 0;
        for (i, keep) in mask.iter().enumerate() {
            if *keep {
                self.srcs[w] = self.srcs[i];
                if let Some(dsts) = &mut self.dsts {
                    dsts[w] = dsts[i];
                }
                w += 1;
            }
        }
        self.srcs.truncate(w);
        if let Some(dsts) = &mut self.dsts {
            dsts.truncate(w);
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
    src_var: &str,
    cols: &IdCols,
) -> Result<OperandVals, String> {
    match operand {
        VecOperand::Col { var, prop } => {
            let ids = cols.ids_of(*var == src_var);
            let cells = props_table(graph, ids, &[prop])?
                .into_iter()
                .map(|mut row| row.pop().unwrap_or(Value::Null))
                .collect();
            Ok(OperandVals::Cells(cells))
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
    src_var: &str,
    cols: &mut IdCols,
) -> Result<(), String> {
    let n = cols.srcs.len();
    // The row pipeline evaluates a predicate per row, so over zero rows
    // neither its constants nor its property reads run; a missing parameter
    // must not error here either.
    if n == 0 {
        return Ok(());
    }
    let mask: Vec<bool> = match stage {
        VecStage::HasLabel { var, label } => {
            let ids = cols.ids_of(*var == src_var);
            let pass = label_pass_set(graph, ids, label)?;
            ids.iter().map(|id| pass.contains(id)).collect()
        }
        VecStage::Cmp {
            structured,
            op,
            lhs,
            rhs,
        } => {
            let lv = resolve_operand(graph, lhs, params, schema, src_var, cols)?;
            let rv = resolve_operand(graph, rhs, params, schema, src_var, cols)?;
            (0..n)
                .map(|i| cmp_keeps(*structured, *op, lv.get(i), rv.get(i)))
                .collect()
        }
    };
    cols.compact(&mask);
    Ok(())
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
    let dst_var = p.expand.as_ref().map(|x| x.dst_var);

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
        srcs: leaf_ids,
        dsts: None,
    };

    // 2. Stages below the expansion restrict the leaf rows, in plan order;
    //    they read only the leaf variable, so restricting before expansion
    //    yields the same rows in the same order as the row pipeline.
    for stage in &p.below_stages {
        apply_stage(graph, stage, params, schema, p.src_var, &mut cols)?;
    }

    // 3. Bulk expansion, flattened per source in scan order: the row order
    //    the per-row pipeline produces.
    if let Some(x) = &p.expand {
        let transitions = expand_multi_type(graph, &cols.srcs, x.rel_type, x.is_incoming)?;
        let mut per_src: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
        for (s, e, d) in transitions {
            per_src.entry(s).or_default().push((e, d));
        }
        let row_estimate: usize = per_src.values().map(Vec::len).sum();
        let mut srcs: Vec<NodeId> = Vec::with_capacity(row_estimate);
        let mut dsts: Vec<NodeId> = Vec::with_capacity(row_estimate);
        for &s in &cols.srcs {
            if let Some(v) = per_src.get(&s) {
                for &(_, d) in v {
                    srcs.push(s);
                    dsts.push(d);
                }
            }
        }
        cols = IdCols {
            srcs,
            dsts: Some(dsts),
        };
    }

    // 4. Stages above the expansion, in plan order, each compacting the id
    //    columns in lockstep.
    for stage in &p.above_stages {
        apply_stage(graph, stage, params, schema, p.src_var, &mut cols)?;
    }

    let n = cols.srcs.len();
    let srcs = cols.srcs;
    let dsts = cols.dsts.unwrap_or_default();

    match p.root {
        VecRoot::Project { items } => {
            // Sort over the projection: gather one key column per sort item,
            // order the row indices with `sort_all`'s comparator (the input
            // index is the tiebreak, matching its stable order), and keep
            // only the limit window, so the projection below gathers and
            // builds just the surviving rows.
            let (srcs, dsts, n) = match &p.project_sort {
                Some(sort_keys) => {
                    let mut key_cols: Vec<Vec<Value>> = Vec::with_capacity(sort_keys.len());
                    for key in sort_keys {
                        let ids = if key.is_src { &srcs } else { &dsts };
                        let cells = props_table(graph, ids, &[key.prop])?
                            .into_iter()
                            .map(|mut row| row.pop().unwrap_or(Value::Null))
                            .collect();
                        key_cols.push(cells);
                    }
                    // The index tiebreak makes this a total order, so a
                    // partition at the limit boundary selects exactly the
                    // rows a full stable sort would put first.
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
                    let selected = match p.limit {
                        Some((skip, count)) => {
                            let hi = skip.saturating_add(count).min(n);
                            if hi > 0 {
                                if hi < n {
                                    order.select_nth_unstable_by(hi - 1, cmp);
                                }
                                order[..hi].sort_by(cmp);
                            }
                            &order[skip.min(n)..hi]
                        }
                        None => {
                            order.sort_by(cmp);
                            &order[..]
                        }
                    };
                    let sorted_srcs: Vec<NodeId> = selected.iter().map(|&i| srcs[i]).collect();
                    let sorted_dsts: Vec<NodeId> = if p.expand.is_some() {
                        selected.iter().map(|&i| dsts[i]).collect()
                    } else {
                        Vec::new()
                    };
                    let m = sorted_srcs.len();
                    (sorted_srcs, sorted_dsts, m)
                }
                None => (srcs, dsts, n),
            };
            // One gather per variable covers every projected property; the
            // record cells are moved out of the tables, not re-cloned.
            let mut src_props: Vec<&str> = Vec::new();
            let mut dst_props: Vec<&str> = Vec::new();
            let mut item_cols = Vec::with_capacity(items.len());
            for (expr, _) in items.iter() {
                // Already validated by `recognize`; decline rather than panic.
                let Some((is_src, prop)) = prop_read(expr, p.src_var, dst_var) else {
                    return Ok(None);
                };
                if is_src {
                    item_cols.push((true, src_props.len()));
                    src_props.push(prop);
                } else {
                    item_cols.push((false, dst_props.len()));
                    dst_props.push(prop);
                }
            }
            let mut src_table = props_table(graph, &srcs, &src_props)?;
            let mut dst_table = props_table(graph, &dsts, &dst_props)?;

            let mut records = Vec::with_capacity(n);
            for i in 0..n {
                let mut values = Vec::with_capacity(items.len());
                for &(is_src, j) in &item_cols {
                    let cell = if is_src {
                        std::mem::take(&mut src_table[i][j])
                    } else {
                        std::mem::take(&mut dst_table[i][j])
                    };
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
            let mut group_codes = Vec::with_capacity(group_by.len());
            for (expr, _) in group_by.iter() {
                // Already validated by `recognize`; decline rather than panic.
                let Some((is_src, prop)) = prop_read(expr, p.src_var, dst_var) else {
                    return Ok(None);
                };
                let ids = if is_src { &srcs } else { &dsts };
                group_codes.push(
                    graph
                        .node_prop_group_codes(ids, prop)
                        .map_err(|e| match e {
                            issundb_core::Error::NodeNotFound(id) => {
                                format!("node not found: {}", id)
                            }
                            other => other.to_string(),
                        })?,
                );
            }
            let mut src_props: Vec<&str> = Vec::new();
            let mut dst_props: Vec<&str> = Vec::new();
            let mut agg_inputs = Vec::with_capacity(aggregations.len());
            for (_, inner, _) in aggregations.iter() {
                match prop_read(inner, p.src_var, dst_var) {
                    Some((is_src, prop)) => {
                        let props = if is_src {
                            &mut src_props
                        } else {
                            &mut dst_props
                        };
                        agg_inputs.push(AggIn::Cell(is_src, props.len()));
                        props.push(prop);
                    }
                    None => agg_inputs.push(AggIn::RowCount),
                }
            }
            let mut src_table = props_table(graph, &srcs, &src_props)?;
            let mut dst_table = props_table(graph, &dsts, &dst_props)?;

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
                        AggIn::Cell(is_src, j) => {
                            let cell = if is_src {
                                std::mem::take(&mut src_table[i][j])
                            } else {
                                std::mem::take(&mut dst_table[i][j])
                            };
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
            // DISTINCT under a limited sort: the row pipeline deduplicates
            // before the limit truncates.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN DISTINCT b.name AS name ORDER BY name ASC LIMIT 2",
            // LIMIT without a sort stays on the streaming row path.
            "MATCH (p:Person) RETURN p.name AS name LIMIT 2",
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
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*) AS n",
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
            "MATCH (a:Ghost)-[:KNOWS]->(b:Person) RETURN count(a) AS n",
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
            ] {
                assert_matches_row_path(&g, cypher);
            }
        }
    }
}
