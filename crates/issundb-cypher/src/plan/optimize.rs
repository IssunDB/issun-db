use std::collections::HashSet;

use crate::ast::{AggFn, BinaryOperator, Expr, Literal};
use crate::plan::logical::FilterExpr;
use crate::plan::physical::PhysicalOperator;
use crate::plan::stats::StatsProvider;

/// An optimizer that applies relational algebra optimization passes to physical plans.
pub struct Optimizer;

impl Optimizer {
    /// Optimize a `PhysicalOperator` plan by standardizing operator sequences,
    /// extracting filter predicates, and pushing them down to the lowest possible nodes.
    pub fn optimize(op: PhysicalOperator, stats: Option<&dyn StatsProvider>) -> PhysicalOperator {
        let (stripped_op, raw_filters) = Self::extract_filters(op, stats);
        // Split top-level AND conjunctions so each conjunct pushes down to its
        // own lowest binder: `a.id = 1 AND b.age > 30` as a whole references
        // both endpoints and would stay above the Expand, while its conjuncts
        // reach the scan and the expansion respectively.
        let mut filters = Vec::with_capacity(raw_filters.len());
        for filter in raw_filters {
            Self::split_conjuncts(filter, &mut filters);
        }
        // Drop statically-true predicates so they are neither pushed down nor
        // evaluated per row. Only provably-true forms are removed; false or
        // unknown predicates are preserved for normal evaluation.
        filters.retain(|f| !Self::is_trivially_true(f));
        let reordered_op = Self::reorder_operators(stripped_op, stats);
        // Choose the lowest-cardinality endpoint as the traversal start, reversing a
        // linear single-hop Expand chain when its far endpoint is cheaper to scan.
        // Runs on the filter-free spine so the chain is contiguous; the HasLabel
        // predicates needed to estimate endpoint cardinality live in `filters`.
        let scan_selected = Self::select_scan_node(reordered_op, &mut filters, stats);
        let mut result = Self::push_down_filters(scan_selected, &mut filters);
        // Any filter whose referenced variables are not bound by any operator in the
        // tree cannot be pushed down. Wrap them above the root so no predicate is
        // silently discarded.
        for filter_expr in filters {
            result = PhysicalOperator::Filter {
                input: Box::new(result),
                expression: filter_expr,
            };
        }
        result = Self::optimize_index_scans(result, stats);
        // Rewrite closing Expand nodes into MultiwayJoin after index-scan optimization
        // so that both passes benefit each other.
        result = rewrite_closing_expands(result);
        // Replace a count aggregation over a bare labeled scan with a constant read
        // from graph metadata, avoiding a full scan.
        result = Self::reduce_count(result, stats);
        // Replace a grouping-free count over a MultiwayJoin-closed directed
        // triangle chain with the core sorted-intersect kernel.
        result = rewrite_triangle_count(result);
        result
    }

    /// Append `filter` to `out`, recursively splitting top-level `AND`
    /// conjunctions into their conjuncts. Each conjunct keeps its original
    /// `Expr` form, so per-conjunct evaluation stays on the same code path;
    /// only the placement changes. The split preserves three-valued logic: a
    /// conjunction passes a row exactly when every conjunct evaluates to TRUE,
    /// which is what sequential Filter nodes compute.
    fn split_conjuncts(filter: FilterExpr, out: &mut Vec<FilterExpr>) {
        if let FilterExpr::Expr(Expr::BinaryOp {
            op: BinaryOperator::And,
            left,
            right,
        }) = filter
        {
            Self::split_conjuncts(FilterExpr::Expr(*left), out);
            Self::split_conjuncts(FilterExpr::Expr(*right), out);
        } else {
            out.push(filter);
        }
    }

    /// Extract all filter operators from the physical plan, return a stripped tree,
    /// and collect all predicates into a single collection.
    fn extract_filters(
        op: PhysicalOperator,
        stats: Option<&dyn StatsProvider>,
    ) -> (PhysicalOperator, Vec<FilterExpr>) {
        match op {
            PhysicalOperator::Filter { input, expression } => {
                let (inner_op, mut inner_filters) = Self::extract_filters(*input, stats);
                inner_filters.push(expression);
                (inner_op, inner_filters)
            }
            PhysicalOperator::SingleRow => (PhysicalOperator::SingleRow, Vec::new()),
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => {
                let (inner_op, inner_filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Unwind {
                        input: Box::new(inner_op),
                        expr,
                        variable,
                    },
                    inner_filters,
                )
            }
            PhysicalOperator::LabelScan { variable, label } => {
                (PhysicalOperator::LabelScan { variable, label }, Vec::new())
            }
            PhysicalOperator::NodeByIdSeek {
                variable,
                label,
                id_value,
            } => (
                PhysicalOperator::NodeByIdSeek {
                    variable,
                    label,
                    id_value,
                },
                Vec::new(),
            ),
            PhysicalOperator::NodeIndexScan {
                variable,
                label,
                property,
                value,
            } => (
                PhysicalOperator::NodeIndexScan {
                    variable,
                    label,
                    property,
                    value,
                },
                Vec::new(),
            ),
            PhysicalOperator::NodeRangeScan {
                variable,
                label,
                property,
                lo,
                lo_inclusive,
                hi,
                hi_inclusive,
            } => (
                PhysicalOperator::NodeRangeScan {
                    variable,
                    label,
                    property,
                    lo,
                    lo_inclusive,
                    hi,
                    hi_inclusive,
                },
                Vec::new(),
            ),
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
                let (inner_op, inner_filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Expand {
                        input: Box::new(inner_op),
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
                    },
                    inner_filters,
                )
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                if is_barrier {
                    // Barrier projects represent WITH clause boundaries. Filters placed
                    // between a barrier project and its child implement the WITH's WHERE
                    // predicate, which sees pre-projection variables. Extracting those
                    // filters would re-place them above the barrier, where the variables
                    // they reference are no longer in scope. Treat barrier projects as
                    // opaque: do not extract filters from inside them, but optimize their subplan.
                    let optimized_input = Self::optimize(*input, stats);
                    (
                        PhysicalOperator::Project {
                            input: Box::new(optimized_input),
                            items,
                            is_barrier: true,
                        },
                        Vec::new(),
                    )
                } else {
                    let (inner_op, inner_filters) = Self::extract_filters(*input, stats);
                    (
                        PhysicalOperator::Project {
                            input: Box::new(inner_op),
                            items,
                            is_barrier,
                        },
                        inner_filters,
                    )
                }
            }
            PhysicalOperator::HashJoin { left, right } => {
                let (left_op, mut left_filters) = Self::extract_filters(*left, stats);
                let (right_op, right_filters) = Self::extract_filters(*right, stats);
                left_filters.extend(right_filters);
                (
                    PhysicalOperator::HashJoin {
                        left: Box::new(left_op),
                        right: Box::new(right_op),
                    },
                    left_filters,
                )
            }
            // Aggregate, Sort, and Limit live above the join/expand tree and
            // never directly contain Filter nodes. Pass through without collecting.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                let (inner, filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Aggregate {
                        input: Box::new(inner),
                        group_by,
                        aggregations,
                    },
                    filters,
                )
            }
            PhysicalOperator::Sort { input, items } => {
                let (inner, filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Sort {
                        input: Box::new(inner),
                        items,
                    },
                    filters,
                )
            }
            PhysicalOperator::Limit { input, skip, count } => {
                let (inner, filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Limit {
                        input: Box::new(inner),
                        skip,
                        count,
                    },
                    filters,
                )
            }
            // Filters inside an OptionalMatch belong to the optional pattern and must be
            // evaluated before null-row preservation: a predicate such as `m:NonExistent`
            // (an inline label) or a `WHERE` attached to the OPTIONAL MATCH restricts which
            // optional matches exist, and when none survive the left row is preserved with
            // the optional variables set to null. Extracting those filters out of the
            // OptionalMatch hoists them above the join, where they would instead drop the
            // preserved null rows. Leave the input subtree intact and report no filters to
            // the caller. `push_down_filters` never recurses into an OptionalMatch, so the
            // inner filters stay contained; `reorder_operators` handles them in place.
            PhysicalOperator::OptionalMatch { input, null_vars } => (
                PhysicalOperator::OptionalMatch { input, null_vars },
                Vec::new(),
            ),
            PhysicalOperator::Distinct { input, keys } => {
                let (inner, inner_filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::Distinct {
                        input: Box::new(inner),
                        keys,
                    },
                    inner_filters,
                )
            }
            // WritePart operators are opaque: do not extract filters from inside them,
            // but recursively optimize their input subplans.
            PhysicalOperator::WritePart { input, part } => {
                let optimized_input = Self::optimize(*input, stats);
                (
                    PhysicalOperator::WritePart {
                        input: Box::new(optimized_input),
                        part,
                    },
                    Vec::new(),
                )
            }
            // ProcedureCall is opaque: it produces rows from a resolved table and
            // has no filters to extract.
            PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            } => (
                PhysicalOperator::ProcedureCall {
                    input,
                    output_vars,
                    rows,
                },
                Vec::new(),
            ),
            PhysicalOperator::MultiwayJoin {
                input,
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            } => {
                let (inner_op, inner_filters) = Self::extract_filters(*input, stats);
                (
                    PhysicalOperator::MultiwayJoin {
                        input: Box::new(inner_op),
                        closing_src_var,
                        closing_dst_var,
                        closing_rel_type,
                        closing_rel_var,
                        closing_is_incoming,
                        closing_is_undirected,
                        closing_unique_rels,
                    },
                    inner_filters,
                )
            }
            // A leaf produced after this pass; nothing to extract.
            t @ PhysicalOperator::TriangleCount { .. } => (t, Vec::new()),
        }
    }

    /// Standardize traversal sequences by reordering join branches or operators where appropriate.
    fn reorder_operators(
        op: PhysicalOperator,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        match op {
            PhysicalOperator::HashJoin { left, right } => {
                let opt_left = Self::reorder_operators(*left, stats);
                let opt_right = Self::reorder_operators(*right, stats);

                // Standardize join branch order by placing the heavier branch on the left,
                // which guarantees consistent physical structure regardless of Cypher MATCH clause order.
                let left_weight = Self::plan_weight(&opt_left, stats);
                let right_weight = Self::plan_weight(&opt_right, stats);

                if left_weight >= right_weight {
                    PhysicalOperator::HashJoin {
                        left: Box::new(opt_left),
                        right: Box::new(opt_right),
                    }
                } else {
                    PhysicalOperator::HashJoin {
                        left: Box::new(opt_right),
                        right: Box::new(opt_left),
                    }
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
            } => PhysicalOperator::Expand {
                input: Box::new(Self::reorder_operators(*input, stats)),
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
            },
            // Filter nodes inside opaque barrier-Project subtrees are not stripped by
            // `extract_filters`.  Pass them through so that reordering does not panic.
            PhysicalOperator::Filter { input, expression } => PhysicalOperator::Filter {
                input: Box::new(Self::reorder_operators(*input, stats)),
                expression,
            },
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                if is_barrier {
                    // Barrier projects are opaque scoping boundaries. Filters may remain
                    // inside them (implementing the WITH clause's WHERE predicate). Do not
                    // recurse into the child for reordering; leave the interior intact.
                    PhysicalOperator::Project {
                        input,
                        items,
                        is_barrier,
                    }
                } else {
                    PhysicalOperator::Project {
                        input: Box::new(Self::reorder_operators(*input, stats)),
                        items,
                        is_barrier,
                    }
                }
            }
            leaf @ PhysicalOperator::SingleRow => leaf,
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::reorder_operators(*input, stats)),
                expr,
                variable,
            },
            leaf @ PhysicalOperator::LabelScan { .. } => leaf,
            leaf @ PhysicalOperator::NodeByIdSeek { .. } => leaf,
            leaf @ PhysicalOperator::NodeIndexScan { .. } => leaf,
            leaf @ PhysicalOperator::NodeRangeScan { .. } => leaf,
            // Aggregate, Sort, and Limit are placed above the plan by the logical planner
            // after the Join/Expand/Filter tree is built. Reordering does not descend into
            // them; they are transparent pass-throughs here.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::reorder_operators(*input, stats)),
                group_by,
                aggregations,
            },
            PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::reorder_operators(*input, stats)),
                items,
            },
            PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::reorder_operators(*input, stats)),
                skip,
                count,
            },
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(Self::reorder_operators(*input, stats)),
                    null_vars,
                }
            }
            PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
                keys,
                input: Box::new(Self::reorder_operators(*input, stats)),
            },
            // WritePart is opaque: do not descend into it for reordering.
            PhysicalOperator::WritePart { input, part } => {
                PhysicalOperator::WritePart { input, part }
            }
            // ProcedureCall is opaque: do not descend into it for reordering.
            PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            } => PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            },
            PhysicalOperator::MultiwayJoin {
                input,
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            } => PhysicalOperator::MultiwayJoin {
                input: Box::new(Self::reorder_operators(*input, stats)),
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            },
            t @ PhysicalOperator::TriangleCount { .. } => t,
        }
    }

    /// Compute plan complexity/weight to assist with operator reordering.
    fn plan_weight(op: &PhysicalOperator, stats: Option<&dyn StatsProvider>) -> usize {
        match op {
            PhysicalOperator::SingleRow => 1,
            PhysicalOperator::Unwind { input, .. } => 1 + Self::plan_weight(input, stats),
            // A primary-key seek touches at most one node: the cheapest scan.
            PhysicalOperator::NodeByIdSeek { .. } => 1,
            PhysicalOperator::NodeIndexScan { .. } => 2,
            PhysicalOperator::NodeRangeScan { .. } => 3,
            PhysicalOperator::LabelScan { label, .. } => {
                if let Some(lbl) = label {
                    if let Some(s) = stats {
                        s.node_count_by_label(lbl).unwrap_or(1).max(1) as usize
                    } else {
                        1
                    }
                } else {
                    1000
                }
            }
            PhysicalOperator::Expand {
                input, rel_type, ..
            } => {
                let input_weight = Self::plan_weight(input, stats);
                // `rel_weight` is the average fan-out per input row: the number of
                // edges of this type divided by the node count. The previous model
                // used the total typed edge count as the multiplier, which treated
                // every input row as expanding to *every* edge of the type. That
                // inflated chained multi-hop expands (input * edges * edges * ...)
                // until they saturated `usize`, collapsing the cost space so plan
                // ordering became arbitrary. Average fan-out keeps the estimate in
                // a realistic range so ordering stays meaningful across hops.
                let rel_weight = if let Some(rtype) = rel_type {
                    match stats {
                        Some(s) => {
                            let edges = s.edge_count_by_type(rtype).unwrap_or(0);
                            match s.total_node_count() {
                                Some(nodes) if nodes > 0 => {
                                    ((edges as f64 / nodes as f64).ceil() as usize).max(1)
                                }
                                // No node-count estimate: keep the prior typed default.
                                _ => 10,
                            }
                        }
                        None => 10,
                    }
                } else {
                    // Untyped expand: the type is unknown, so assume a higher fan-out.
                    100
                };
                input_weight.saturating_mul(rel_weight)
            }
            // Filter nodes inside barrier-Project subtrees are not stripped by
            // `extract_filters` (barrier Projects are opaque).  When `plan_weight`
            // recurses into such a subtree, treat the Filter as transparent.
            PhysicalOperator::Filter { input, .. } => Self::plan_weight(input, stats),
            PhysicalOperator::Project { input, .. } => Self::plan_weight(input, stats),
            PhysicalOperator::HashJoin { left, right } => {
                Self::plan_weight(left, stats).saturating_mul(Self::plan_weight(right, stats))
            }
            // These operators sit above the core traversal tree; weight them as their child.
            PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::OptionalMatch { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::WritePart { input, .. }
            | PhysicalOperator::ProcedureCall { input, .. } => Self::plan_weight(input, stats),
            // A single kernel call producing one row.
            PhysicalOperator::TriangleCount { .. } => 1,
            // MultiwayJoin is cheaper than a regular Expand because the closing check is O(1)
            // per row after a single bulk expansion. Weight as the input cost.
            PhysicalOperator::MultiwayJoin { input, .. } => Self::plan_weight(input, stats),
        }
    }

    /// Push collected filters down the plan tree to the lowest possible nodes where they can be evaluated.
    fn push_down_filters(op: PhysicalOperator, pending: &mut Vec<FilterExpr>) -> PhysicalOperator {
        match op {
            PhysicalOperator::NodeRangeScan {
                variable,
                label,
                property,
                lo,
                lo_inclusive,
                hi,
                hi_inclusive,
            } => {
                let mut current_node = PhysicalOperator::NodeRangeScan {
                    variable: variable.clone(),
                    label,
                    property,
                    lo,
                    lo_inclusive,
                    hi,
                    hi_inclusive,
                };
                let bound = Self::bound_vars(&current_node);
                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }
                current_node
            }
            PhysicalOperator::NodeIndexScan {
                variable,
                label,
                property,
                value,
            } => {
                let mut current_node = PhysicalOperator::NodeIndexScan {
                    variable: variable.clone(),
                    label,
                    property,
                    value,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                current_node
            }
            PhysicalOperator::NodeByIdSeek {
                variable,
                label,
                id_value,
            } => {
                let mut current_node = PhysicalOperator::NodeByIdSeek {
                    variable: variable.clone(),
                    label,
                    id_value,
                };
                let bound = Self::bound_vars(&current_node);
                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }
                current_node
            }
            PhysicalOperator::LabelScan { variable, label } => {
                let mut current_node = PhysicalOperator::LabelScan {
                    variable: variable.clone(),
                    label,
                };

                let bound = Self::bound_vars(&current_node);

                // Push down all filters whose referenced variables are fully bound by this scan.
                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                current_node
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
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::Expand {
                    input: Box::new(optimized_input),
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
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::HashJoin { left, right } => {
                let left_bound = Self::bound_vars(&left);
                let right_bound = Self::bound_vars(&right);

                let mut left_pending = Vec::new();
                let mut right_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&left_bound) {
                        left_pending.push(filter);
                    } else if ref_vars.is_subset(&right_bound) {
                        right_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_left = Self::push_down_filters(*left, &mut left_pending);
                let optimized_right = Self::push_down_filters(*right, &mut right_pending);

                remaining_pending.extend(left_pending);
                remaining_pending.extend(right_pending);

                let mut current_node = PhysicalOperator::HashJoin {
                    left: Box::new(optimized_left),
                    right: Box::new(optimized_right),
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                if is_barrier {
                    // Barrier projects are opaque scoping boundaries (WITH clauses).
                    // Filters from outside must not be pushed through a barrier because the
                    // variables they reference may not be available on the other side.
                    // Do not recurse into the child for pushdown: the child may contain
                    // Filter nodes that implement the WITH clause's WHERE predicate and
                    // must remain exactly where the logical planner placed them.
                    let mut current_node = PhysicalOperator::Project {
                        input,
                        items,
                        is_barrier,
                    };

                    // Filters from the outer pending set that reference post-barrier variables
                    // can be applied above this barrier node.
                    let bound = Self::bound_vars(&current_node);
                    let mut i = 0;
                    while i < pending.len() {
                        let ref_vars = Self::referenced_vars(&pending[i]);
                        if ref_vars.is_subset(&bound) {
                            let filter_expr = pending.remove(i);
                            current_node = PhysicalOperator::Filter {
                                input: Box::new(current_node),
                                expression: filter_expr,
                            };
                        } else {
                            i += 1;
                        }
                    }

                    current_node
                } else {
                    let child_bound = Self::bound_vars(&input);

                    let mut child_pending = Vec::new();
                    let mut remaining_pending = Vec::new();

                    for filter in pending.drain(..) {
                        let ref_vars = Self::referenced_vars(&filter);
                        if ref_vars.is_subset(&child_bound) {
                            child_pending.push(filter);
                        } else {
                            remaining_pending.push(filter);
                        }
                    }

                    let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                    remaining_pending.extend(child_pending);

                    let mut current_node = PhysicalOperator::Project {
                        input: Box::new(optimized_input),
                        items,
                        is_barrier,
                    };

                    let bound = Self::bound_vars(&current_node);

                    let mut i = 0;
                    while i < remaining_pending.len() {
                        let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                        if ref_vars.is_subset(&bound) {
                            let filter_expr = remaining_pending.remove(i);
                            current_node = PhysicalOperator::Filter {
                                input: Box::new(current_node),
                                expression: filter_expr,
                            };
                        } else {
                            i += 1;
                        }
                    }

                    *pending = remaining_pending;
                    current_node
                }
            }
            PhysicalOperator::SingleRow => {
                let mut current_node = PhysicalOperator::SingleRow;
                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < pending.len() {
                    let ref_vars = Self::referenced_vars(&pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }
                current_node
            }
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => {
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::Unwind {
                    input: Box::new(optimized_input),
                    expr,
                    variable,
                };

                let bound = Self::bound_vars(&current_node);

                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            PhysicalOperator::Filter { .. } => {
                unreachable!("Filter operators must be extracted prior to pushdown optimization")
            }
            // Aggregate, Sort, and Limit live above the join/expand tree. Pushdown does not
            // reach inside them; pass through and recurse into their child.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Aggregate {
                    input: Box::new(optimized),
                    group_by,
                    aggregations,
                }
            }
            PhysicalOperator::Sort { input, items } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Sort {
                    input: Box::new(optimized),
                    items,
                }
            }
            PhysicalOperator::Limit { input, skip, count } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Limit {
                    input: Box::new(optimized),
                    skip,
                    count,
                }
            }
            // Do not push filters into an OptionalMatch; they must remain outside.
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(*input),
                    null_vars,
                }
            }
            PhysicalOperator::Distinct { input, keys } => {
                let optimized = Self::push_down_filters(*input, pending);
                PhysicalOperator::Distinct {
                    input: Box::new(optimized),
                    keys,
                }
            }
            // WritePart is opaque: do not push filters through a write boundary.
            PhysicalOperator::WritePart { input, part } => {
                PhysicalOperator::WritePart { input, part }
            }
            // ProcedureCall is opaque: do not push filters through it.
            PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            } => PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            },
            PhysicalOperator::MultiwayJoin {
                input,
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            } => {
                let child_bound = Self::bound_vars(&input);

                let mut child_pending = Vec::new();
                let mut remaining_pending = Vec::new();

                for filter in pending.drain(..) {
                    let ref_vars = Self::referenced_vars(&filter);
                    if ref_vars.is_subset(&child_bound) {
                        child_pending.push(filter);
                    } else {
                        remaining_pending.push(filter);
                    }
                }

                let optimized_input = Self::push_down_filters(*input, &mut child_pending);
                remaining_pending.extend(child_pending);

                let mut current_node = PhysicalOperator::MultiwayJoin {
                    input: Box::new(optimized_input),
                    closing_src_var,
                    closing_dst_var,
                    closing_rel_type,
                    closing_rel_var,
                    closing_is_incoming,
                    closing_is_undirected,
                    closing_unique_rels,
                };

                let bound = Self::bound_vars(&current_node);
                let mut i = 0;
                while i < remaining_pending.len() {
                    let ref_vars = Self::referenced_vars(&remaining_pending[i]);
                    if ref_vars.is_subset(&bound) {
                        let filter_expr = remaining_pending.remove(i);
                        current_node = PhysicalOperator::Filter {
                            input: Box::new(current_node),
                            expression: filter_expr,
                        };
                    } else {
                        i += 1;
                    }
                }

                *pending = remaining_pending;
                current_node
            }
            // A leaf produced after this pass; no children to push into.
            t @ PhysicalOperator::TriangleCount { .. } => t,
        }
    }

    /// Compute the set of variables that are bound or introduced by a physical operator.
    pub(crate) fn bound_vars(op: &PhysicalOperator) -> HashSet<String> {
        let mut vars = HashSet::new();
        Self::collect_bound_vars(op, &mut vars);
        vars
    }

    /// Recursively collect variables bound by the physical operator.
    fn collect_bound_vars(op: &PhysicalOperator, vars: &mut HashSet<String>) {
        match op {
            PhysicalOperator::SingleRow => {}
            PhysicalOperator::Unwind {
                input, variable, ..
            } => {
                Self::collect_bound_vars(input, vars);
                vars.insert(variable.clone());
            }
            PhysicalOperator::LabelScan { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::NodeByIdSeek { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::NodeIndexScan { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::NodeRangeScan { variable, .. } => {
                vars.insert(variable.clone());
            }
            PhysicalOperator::Expand {
                input,
                rel_var,
                dst_var,
                min_hops,
                max_hops,
                ..
            } => {
                Self::collect_bound_vars(input, vars);
                // rel_var is only bound for single-hop patterns (min=max=1).
                // Variable-length BFS (any other range) does not insert rel_var
                // into the PathMap, so the optimizer must not treat it as bound
                // or it will misplace filters that reference it.
                if *min_hops == 1 && *max_hops == 1 {
                    vars.insert(rel_var.clone());
                }
                vars.insert(dst_var.clone());
            }
            PhysicalOperator::Filter { input, .. } => {
                Self::collect_bound_vars(input, vars);
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => {
                if *is_barrier {
                    for (expr, alias) in items {
                        let output_var = if let Some(a) = alias {
                            a.clone()
                        } else {
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
                                Expr::Agg(_, _) => "agg".to_string(),
                                _ => "expr".to_string(),
                            }
                        };
                        vars.insert(output_var);
                    }
                } else {
                    Self::collect_bound_vars(input, vars);
                    for (expr, alias) in items {
                        let output_var = if let Some(a) = alias {
                            a.clone()
                        } else {
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
                                Expr::Agg(_, _) => "agg".to_string(),
                                _ => "expr".to_string(),
                            }
                        };
                        vars.insert(output_var);
                    }
                }
            }
            PhysicalOperator::HashJoin { left, right } => {
                Self::collect_bound_vars(left, vars);
                Self::collect_bound_vars(right, vars);
            }
            // Aggregate emits group-by column names as bound variables.
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                Self::collect_bound_vars(input, vars);
                for (_fn, _inner, col) in aggregations {
                    vars.insert(col.clone());
                }
                for (expr, alias) in group_by {
                    let name = if let Some(a) = alias {
                        a.clone()
                    } else if let Expr::Prop(var, prop) = expr {
                        if prop.is_empty() {
                            var.clone()
                        } else {
                            format!("{}.{}", var, prop)
                        }
                    } else {
                        continue;
                    };
                    vars.insert(name);
                }
            }
            PhysicalOperator::Sort { input, .. } | PhysicalOperator::Limit { input, .. } => {
                Self::collect_bound_vars(input, vars);
            }
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                Self::collect_bound_vars(input, vars);
                for var in null_vars {
                    vars.insert(var.clone());
                }
            }
            PhysicalOperator::Distinct { input, .. } => {
                Self::collect_bound_vars(input, vars);
            }
            // WritePart binds variables from its input plus newly created node/edge variables.
            // For simplicity, collect from input; newly created variables are added at execution time.
            PhysicalOperator::WritePart { input, part } => {
                Self::collect_bound_vars(input, vars);
                // Add variables from CREATE patterns so that downstream operators can reference them.
                match part {
                    crate::ast::QueryPart::Create { patterns } => {
                        for p in patterns {
                            if let Some(ref v) = p.node.variable {
                                vars.insert(v.clone());
                            }
                            for (rel, target) in &p.rels {
                                if let Some(ref v) = rel.variable {
                                    vars.insert(v.clone());
                                }
                                if let Some(ref v) = target.variable {
                                    vars.insert(v.clone());
                                }
                            }
                        }
                    }
                    crate::ast::QueryPart::Merge { merges } => {
                        for m in merges {
                            if let Some(ref v) = m.pattern.node.variable {
                                vars.insert(v.clone());
                            }
                            for (rel, target) in &m.pattern.rels {
                                if let Some(ref v) = rel.variable {
                                    vars.insert(v.clone());
                                }
                                if let Some(ref v) = target.variable {
                                    vars.insert(v.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            PhysicalOperator::MultiwayJoin {
                input,
                closing_rel_var,
                ..
            } => {
                Self::collect_bound_vars(input, vars);
                // closing_src_var and closing_dst_var are already bound by input;
                // the only new binding introduced here is the closing edge variable.
                vars.insert(closing_rel_var.clone());
            }
            // A CALL binds its YIELD output variables on top of its input bindings.
            PhysicalOperator::ProcedureCall {
                input, output_vars, ..
            } => {
                Self::collect_bound_vars(input, vars);
                for v in output_vars {
                    vars.insert(v.clone());
                }
            }
            PhysicalOperator::TriangleCount { output, .. } => {
                vars.insert(output.clone());
            }
        }
    }

    /// Get all variables referenced by a filter expression.
    fn referenced_vars(expr: &FilterExpr) -> HashSet<String> {
        let mut vars = HashSet::new();
        match expr {
            FilterExpr::Eq(l, r)
            | FilterExpr::Ne(l, r)
            | FilterExpr::Lt(l, r)
            | FilterExpr::Gt(l, r)
            | FilterExpr::Le(l, r)
            | FilterExpr::Ge(l, r) => {
                Self::collect_expr_vars(l, &mut vars);
                Self::collect_expr_vars(r, &mut vars);
            }
            FilterExpr::HasLabel(var, _) => {
                vars.insert(var.clone());
            }
            FilterExpr::Expr(e) => {
                Self::collect_expr_vars(e, &mut vars);
            }
        }
        vars
    }

    /// Recursively collect variables referenced by an expression.
    fn collect_expr_vars(expr: &Expr, vars: &mut HashSet<String>) {
        match expr {
            Expr::Prop(var, _) => {
                vars.insert(var.clone());
            }
            Expr::Literal(_) | Expr::Param(_) | Expr::CountStar => {}
            // Aggregate expressions delegate to recursive collection on their inner expression.
            Expr::Agg(_, inner) => Self::collect_expr_vars(inner, vars),
            Expr::BinaryOp { left, right, .. } => {
                Self::collect_expr_vars(left, vars);
                Self::collect_expr_vars(right, vars);
            }
            Expr::IsNull(inner) | Expr::IsNotNull(inner) | Expr::Not(inner) => {
                Self::collect_expr_vars(inner, vars);
            }
            Expr::FunctionCall { args, .. } => {
                for arg in args {
                    Self::collect_expr_vars(arg, vars);
                }
            }
            Expr::Case {
                subject,
                arms,
                else_expr,
            } => {
                if let Some(s) = subject {
                    Self::collect_expr_vars(s, vars);
                }
                for arm in arms {
                    Self::collect_expr_vars(&arm.when, vars);
                    Self::collect_expr_vars(&arm.then, vars);
                }
                if let Some(e) = else_expr {
                    Self::collect_expr_vars(e, vars);
                }
            }
            Expr::Subscript { expr, index } => {
                Self::collect_expr_vars(expr, vars);
                Self::collect_expr_vars(index, vars);
            }
            Expr::Slice { expr, start, end } => {
                Self::collect_expr_vars(expr, vars);
                if let Some(s) = start {
                    Self::collect_expr_vars(s, vars);
                }
                if let Some(e) = end {
                    Self::collect_expr_vars(e, vars);
                }
            }
            // variable is a local binding; do not insert it. Recurse into list and predicate.
            Expr::Quantifier {
                list, predicate, ..
            } => {
                Self::collect_expr_vars(list, vars);
                Self::collect_expr_vars(predicate, vars);
            }
            // variable is a local binding; do not insert it. Recurse into list, predicate, and transform.
            Expr::ListComprehension {
                list,
                predicate,
                transform,
                ..
            } => {
                Self::collect_expr_vars(list, vars);
                if let Some(p) = predicate {
                    Self::collect_expr_vars(p, vars);
                }
                if let Some(t) = transform {
                    Self::collect_expr_vars(t, vars);
                }
            }
            Expr::Reduce {
                initial,
                list,
                expression,
                ..
            } => {
                Self::collect_expr_vars(initial, vars);
                Self::collect_expr_vars(list, vars);
                Self::collect_expr_vars(expression, vars);
            }
            // The anchor node is an outer reference; the relationship, target-node, and
            // path variables are local bindings, so they are excluded.
            Expr::PatternComprehension {
                pattern,
                predicate,
                transform,
            } => {
                let mut local = HashSet::new();
                if let Some(pv) = &pattern.path_variable {
                    local.insert(pv.clone());
                }
                for (rel, node) in &pattern.rels {
                    if let Some(v) = &rel.variable {
                        local.insert(v.clone());
                    }
                    if let Some(v) = &node.variable {
                        local.insert(v.clone());
                    }
                }
                let mut inner = HashSet::new();
                if let Some(p) = predicate {
                    Self::collect_expr_vars(p, &mut inner);
                }
                Self::collect_expr_vars(transform, &mut inner);
                for v in inner {
                    if !local.contains(&v) {
                        vars.insert(v);
                    }
                }
                if let Some(anchor) = &pattern.node.variable {
                    vars.insert(anchor.clone());
                }
            }
            Expr::HasLabel { variable, .. } => {
                vars.insert(variable.clone());
            }
        }
    }

    /// Recursively optimize LabelScan + Filter combinations into NodeIndexScan or NodeRangeScan.
    ///
    /// - `Eq` filter → `NodeIndexScan` (point lookup)
    /// - `Lt/Gt/Le/Ge` filter → `NodeRangeScan` (range scan)
    /// - A second relational filter stacked on an existing `NodeRangeScan` for the same
    ///   property narrows the bounds rather than adding a post-filter.
    fn optimize_index_scans(
        op: PhysicalOperator,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        match op {
            PhysicalOperator::Filter { input, expression } => {
                let optimized_input = Self::optimize_index_scans(*input, stats);

                // A conjunct split from a top-level AND keeps its `Expr`
                // comparison form. Both forms pass a row only when the
                // comparison is TRUE, and the scan executor re-checks every
                // candidate's actual value, so for scan selection the two
                // forms rewrite identically. An unrewritten filter is wrapped
                // back with its original `expression`, keeping its own
                // evaluation semantics.
                let normalized = match &expression {
                    FilterExpr::Expr(Expr::BinaryOp { op, left, right }) => {
                        let cmp = match op {
                            BinaryOperator::Eq => Some(FilterExpr::Eq as fn(_, _) -> _),
                            BinaryOperator::Lt => Some(FilterExpr::Lt as fn(_, _) -> _),
                            BinaryOperator::Gt => Some(FilterExpr::Gt as fn(_, _) -> _),
                            BinaryOperator::Le => Some(FilterExpr::Le as fn(_, _) -> _),
                            BinaryOperator::Ge => Some(FilterExpr::Ge as fn(_, _) -> _),
                            _ => None,
                        };
                        cmp.map(|f| f((**left).clone(), (**right).clone()))
                    }
                    _ => None,
                };
                let probe = normalized.as_ref().unwrap_or(&expression);

                // `WHERE id(n) = <const>` over a node scan becomes a primary-key seek.
                // Applies to labeled and unlabeled scans; the label, when present, is
                // re-checked by the seek executor.
                if let PhysicalOperator::LabelScan { variable, label } = &optimized_input {
                    if let FilterExpr::Eq(l, r) = probe {
                        if let Some(id_value) = Self::id_seek_value(l, r, variable) {
                            return PhysicalOperator::NodeByIdSeek {
                                variable: variable.clone(),
                                label: label.clone(),
                                id_value,
                            };
                        }
                    }
                }

                // A literal or parameter the index can look up. A null
                // literal is excluded: `prop = null` is never TRUE, and the
                // scan evaluator has no null lookup form; the filter then
                // stays a filter and drops every row. A list literal is
                // excluded because list values are never indexed.
                let indexable_const = |e: &Expr| match e {
                    Expr::Literal(crate::ast::Literal::Null)
                    | Expr::Literal(crate::ast::Literal::List(_)) => false,
                    Expr::Literal(_) | Expr::Param(_) => true,
                    _ => false,
                };
                // Helper: extract (variable, property, value_expr) when a relational filter
                // references a node property on one side and a literal/param on the other.
                let try_prop_literal = |l: &Expr, r: &Expr, var: &str| -> Option<(String, Expr)> {
                    if let Expr::Prop(v, prop) = l {
                        if v == var && indexable_const(r) {
                            return Some((prop.clone(), r.clone()));
                        }
                    }
                    if let Expr::Prop(v, prop) = r {
                        if v == var && indexable_const(l) {
                            return Some((prop.clone(), l.clone()));
                        }
                    }
                    None
                };

                // Check if the filter is a relational predicate on a LabelScan variable.
                if let PhysicalOperator::LabelScan {
                    variable,
                    label: Some(lbl),
                } = &optimized_input
                {
                    if let Some(s) = stats {
                        match probe {
                            FilterExpr::Eq(l, r) => {
                                if let Some((prop, val)) = try_prop_literal(l, r, variable) {
                                    if s.has_node_property_index(lbl, &prop) {
                                        return PhysicalOperator::NodeIndexScan {
                                            variable: variable.clone(),
                                            label: lbl.clone(),
                                            property: prop,
                                            value: val,
                                        };
                                    }
                                }
                            }
                            FilterExpr::Lt(l, r) => {
                                if let Some((prop, val)) = try_prop_literal(l, r, variable) {
                                    // Determine direction: prop < val or val < prop
                                    let prop_on_left =
                                        matches!(l, Expr::Prop(v, _) if v == variable);
                                    if s.has_node_property_index(lbl, &prop) {
                                        return if prop_on_left {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: None,
                                                lo_inclusive: true,
                                                hi: Some(val),
                                                hi_inclusive: false,
                                            }
                                        } else {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: Some(val),
                                                lo_inclusive: false,
                                                hi: None,
                                                hi_inclusive: true,
                                            }
                                        };
                                    }
                                }
                            }
                            FilterExpr::Le(l, r) => {
                                if let Some((prop, val)) = try_prop_literal(l, r, variable) {
                                    let prop_on_left =
                                        matches!(l, Expr::Prop(v, _) if v == variable);
                                    if s.has_node_property_index(lbl, &prop) {
                                        return if prop_on_left {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: None,
                                                lo_inclusive: true,
                                                hi: Some(val),
                                                hi_inclusive: true,
                                            }
                                        } else {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: Some(val),
                                                lo_inclusive: true,
                                                hi: None,
                                                hi_inclusive: true,
                                            }
                                        };
                                    }
                                }
                            }
                            FilterExpr::Gt(l, r) => {
                                if let Some((prop, val)) = try_prop_literal(l, r, variable) {
                                    let prop_on_left =
                                        matches!(l, Expr::Prop(v, _) if v == variable);
                                    if s.has_node_property_index(lbl, &prop) {
                                        return if prop_on_left {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: Some(val),
                                                lo_inclusive: false,
                                                hi: None,
                                                hi_inclusive: true,
                                            }
                                        } else {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: None,
                                                lo_inclusive: true,
                                                hi: Some(val),
                                                hi_inclusive: false,
                                            }
                                        };
                                    }
                                }
                            }
                            FilterExpr::Ge(l, r) => {
                                if let Some((prop, val)) = try_prop_literal(l, r, variable) {
                                    let prop_on_left =
                                        matches!(l, Expr::Prop(v, _) if v == variable);
                                    if s.has_node_property_index(lbl, &prop) {
                                        return if prop_on_left {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: Some(val),
                                                lo_inclusive: true,
                                                hi: None,
                                                hi_inclusive: true,
                                            }
                                        } else {
                                            PhysicalOperator::NodeRangeScan {
                                                variable: variable.clone(),
                                                label: lbl.clone(),
                                                property: prop,
                                                lo: None,
                                                lo_inclusive: true,
                                                hi: Some(val),
                                                hi_inclusive: true,
                                            }
                                        };
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // If the input is already a NodeRangeScan on the same property, narrow its bounds
                // rather than adding a post-filter.  This handles two-sided predicates like
                // `WHERE n.age > 20 AND n.age < 50` which the push-down pass emits as
                // `Filter(NodeRangeScan(lo=20), n.age<50)`.
                if let PhysicalOperator::NodeRangeScan {
                    variable,
                    label,
                    property,
                    lo,
                    lo_inclusive,
                    hi,
                    hi_inclusive,
                } = optimized_input
                {
                    if let Some(s) = stats {
                        if s.has_node_property_index(&label, &property) {
                            let try_same_prop = |l: &Expr, r: &Expr| -> Option<(Expr, bool)> {
                                if let Expr::Prop(v, p) = l {
                                    if *v == variable && *p == property && indexable_const(r) {
                                        return Some((r.clone(), true));
                                    }
                                }
                                if let Expr::Prop(v, p) = r {
                                    if *v == variable && *p == property && indexable_const(l) {
                                        return Some((l.clone(), false));
                                    }
                                }
                                None
                            };
                            match probe {
                                FilterExpr::Lt(l, r) => {
                                    if let Some((val, prop_on_left)) = try_same_prop(l, r) {
                                        return PhysicalOperator::NodeRangeScan {
                                            variable,
                                            label,
                                            property,
                                            lo,
                                            lo_inclusive,
                                            hi: if prop_on_left { Some(val) } else { hi },
                                            hi_inclusive: if prop_on_left {
                                                false
                                            } else {
                                                hi_inclusive
                                            },
                                        };
                                    }
                                }
                                FilterExpr::Le(l, r) => {
                                    if let Some((val, prop_on_left)) = try_same_prop(l, r) {
                                        return PhysicalOperator::NodeRangeScan {
                                            variable,
                                            label,
                                            property,
                                            lo,
                                            lo_inclusive,
                                            hi: if prop_on_left { Some(val) } else { hi },
                                            hi_inclusive: if prop_on_left {
                                                true
                                            } else {
                                                hi_inclusive
                                            },
                                        };
                                    }
                                }
                                FilterExpr::Gt(l, r) => {
                                    if let Some((val, prop_on_left)) = try_same_prop(l, r) {
                                        return PhysicalOperator::NodeRangeScan {
                                            variable,
                                            label,
                                            property,
                                            lo: if prop_on_left { Some(val) } else { lo },
                                            lo_inclusive: if prop_on_left {
                                                false
                                            } else {
                                                lo_inclusive
                                            },
                                            hi,
                                            hi_inclusive,
                                        };
                                    }
                                }
                                FilterExpr::Ge(l, r) => {
                                    if let Some((val, prop_on_left)) = try_same_prop(l, r) {
                                        return PhysicalOperator::NodeRangeScan {
                                            variable,
                                            label,
                                            property,
                                            lo: if prop_on_left { Some(val) } else { lo },
                                            lo_inclusive: if prop_on_left {
                                                true
                                            } else {
                                                lo_inclusive
                                            },
                                            hi,
                                            hi_inclusive,
                                        };
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    // Bounds not merged; wrap back.

                    return PhysicalOperator::Filter {
                        input: Box::new(PhysicalOperator::NodeRangeScan {
                            variable,
                            label,
                            property,
                            lo,
                            lo_inclusive,
                            hi,
                            hi_inclusive,
                        }),
                        expression,
                    };
                }

                PhysicalOperator::Filter {
                    input: Box::new(optimized_input),
                    expression,
                }
            }
            PhysicalOperator::Unwind {
                input,
                expr,
                variable,
            } => PhysicalOperator::Unwind {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                expr,
                variable,
            },
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
            } => PhysicalOperator::Expand {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
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
            },
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } => PhysicalOperator::Project {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                items,
                is_barrier,
            },
            PhysicalOperator::HashJoin { left, right } => PhysicalOperator::HashJoin {
                left: Box::new(Self::optimize_index_scans(*left, stats)),
                right: Box::new(Self::optimize_index_scans(*right, stats)),
            },
            PhysicalOperator::MultiwayJoin {
                input,
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            } => PhysicalOperator::MultiwayJoin {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_rel_var,
                closing_is_incoming,
                closing_is_undirected,
                closing_unique_rels,
            },
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                group_by,
                aggregations,
            },
            PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                items,
            },
            PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                skip,
                count,
            },
            PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                keys,
            },
            PhysicalOperator::OptionalMatch { input, null_vars } => {
                PhysicalOperator::OptionalMatch {
                    input: Box::new(Self::optimize_index_scans(*input, stats)),
                    null_vars,
                }
            }
            PhysicalOperator::WritePart { input, part } => PhysicalOperator::WritePart {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                part,
            },
            PhysicalOperator::ProcedureCall {
                input,
                output_vars,
                rows,
            } => PhysicalOperator::ProcedureCall {
                input: Box::new(Self::optimize_index_scans(*input, stats)),
                output_vars,
                rows,
            },
            leaf => leaf,
        }
    }

    /// Pick the cheapest endpoint of a linear single-hop `Expand` chain as the
    /// traversal start, reversing the chain when its far endpoint is cheaper to
    /// scan than the current start.
    ///
    /// The chain is reversed by swapping each hop's `src`/`dst` and flipping its
    /// direction (a directed hop's `is_incoming` is inverted; an undirected hop is
    /// symmetric and only swaps endpoints). Because `Expand` already honors
    /// `is_incoming`, the reversed plan binds the same `(src, rel, dst)` triples
    /// and needs no executor change. The far endpoint's `HasLabel` predicate
    /// becomes the new scan label; the old start label is re-added to `filters` so
    /// it is still enforced after push-down.
    ///
    /// Reversal is skipped for any non-linear, multi-hop, cyclic, or
    /// label-unknown chain, and for `OptionalMatch` subtrees (whose `HasLabel`
    /// predicates are not extracted and so still interrupt the spine).
    fn select_scan_node(
        op: PhysicalOperator,
        filters: &mut Vec<FilterExpr>,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        match op {
            PhysicalOperator::HashJoin { left, right } => PhysicalOperator::HashJoin {
                left: Box::new(Self::select_scan_node(*left, filters, stats)),
                right: Box::new(Self::select_scan_node(*right, filters, stats)),
            },
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } if !is_barrier => PhysicalOperator::Project {
                input: Box::new(Self::select_scan_node(*input, filters, stats)),
                items,
                is_barrier,
            },
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => PhysicalOperator::Aggregate {
                input: Box::new(Self::select_scan_node(*input, filters, stats)),
                group_by,
                aggregations,
            },
            PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::select_scan_node(*input, filters, stats)),
                items,
            },
            PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::select_scan_node(*input, filters, stats)),
                skip,
                count,
            },
            PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
                keys,
                input: Box::new(Self::select_scan_node(*input, filters, stats)),
            },
            op @ PhysicalOperator::Expand { .. } => Self::try_reverse_chain(op, filters, stats),
            // Barrier projects, OptionalMatch, WritePart, Unwind, and leaf scans are
            // left untouched: their spines either are not contiguous (filters remain
            // inside) or contain no reversible chain.
            other => other,
        }
    }

    /// Attempt to reverse a linear single-hop `Expand` chain rooted at `op` so the
    /// lower-cardinality (or index-backed) endpoint is scanned first. Returns the
    /// original `op` unchanged when reversal does not apply or would not help.
    fn try_reverse_chain(
        op: PhysicalOperator,
        filters: &mut Vec<FilterExpr>,
        stats: Option<&dyn StatsProvider>,
    ) -> PhysicalOperator {
        struct Hop {
            src: String,
            rel_var: String,
            dst: String,
            rel_type: Option<String>,
            is_incoming: bool,
            is_undirected: bool,
            needs_path: bool,
        }

        // Walk the chain top-to-bottom by reference, validating shape without
        // consuming `op`, so a bail-out can return it untouched.
        let mut hops: Vec<Hop> = Vec::new();
        let mut node = &op;
        let (start_var, start_lbl) = loop {
            match node {
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
                    needs_path,
                    ..
                } => {
                    if *min_hops != 1 || *max_hops != 1 {
                        return op; // variable-length hops are not reversed here
                    }
                    hops.push(Hop {
                        src: src_var.clone(),
                        rel_var: rel_var.clone(),
                        dst: dst_var.clone(),
                        rel_type: rel_type.clone(),
                        is_incoming: *is_incoming,
                        is_undirected: *is_undirected,
                        needs_path: *needs_path,
                    });
                    node = input;
                }
                PhysicalOperator::LabelScan {
                    variable,
                    label: Some(lbl),
                } => break (variable.clone(), lbl.clone()),
                // Not a clean chain over a single labeled scan.
                _ => return op,
            }
        };

        // A named path accumulates `_path_*` objects hop by hop in pattern order;
        // executing the chain reversed would build them backwards, so bail out.
        if hops.iter().any(|h| h.needs_path) {
            return op;
        }

        // Validate linear connectivity (hops listed top-to-bottom) and acyclicity.
        // hop[i].src must equal hop[i+1].dst, and the bottom hop's src is the scan.
        let n = hops.len();
        for i in 0..n.saturating_sub(1) {
            if hops[i].src != hops[i + 1].dst {
                return op;
            }
        }
        if hops[n - 1].src != start_var {
            return op;
        }
        // Distinct node variables (no repeated node = no cycle to close).
        let mut node_vars: Vec<&str> = vec![start_var.as_str()];
        for hop in hops.iter().rev() {
            node_vars.push(hop.dst.as_str());
        }
        let distinct: HashSet<&str> = node_vars.iter().copied().collect();
        if distinct.len() != node_vars.len() {
            return op;
        }

        // The far endpoint is the top hop's destination.
        let terminal_var = hops[0].dst.clone();
        let term_lbl = match filters.iter().find_map(|f| match f {
            FilterExpr::HasLabel(v, l) if *v == terminal_var => Some(l.clone()),
            _ => None,
        }) {
            Some(l) => l,
            None => return op, // cannot estimate the far endpoint
        };

        // Decide using index-backed equality first, then label cardinality.
        let start_indexed = Self::has_indexed_eq(&start_var, &start_lbl, filters, stats);
        let term_indexed = Self::has_indexed_eq(&terminal_var, &term_lbl, filters, stats);
        let reverse = match (start_indexed, term_indexed) {
            (false, true) => true,
            (true, false) => false,
            _ => {
                let sc = stats.and_then(|s| s.node_count_by_label(&start_lbl));
                let tc = stats.and_then(|s| s.node_count_by_label(&term_lbl));
                match (sc, tc) {
                    (Some(s), Some(t)) => t < s,
                    _ => return op,
                }
            }
        };
        if !reverse {
            return op;
        }

        // Build the reversed chain: scan the far endpoint, then apply hops from the
        // top down, swapping endpoints and flipping direction for directed hops.
        let mut tree = PhysicalOperator::LabelScan {
            variable: terminal_var.clone(),
            label: Some(term_lbl.clone()),
        };
        // Relationship uniqueness is pairwise within the pattern, so the
        // reversed chain re-derives each hop's predecessors from the new order.
        let mut prior_rels: Vec<String> = Vec::new();
        for hop in hops.iter() {
            tree = PhysicalOperator::Expand {
                input: Box::new(tree),
                src_var: hop.dst.clone(),
                rel_var: hop.rel_var.clone(),
                dst_var: hop.src.clone(),
                rel_type: hop.rel_type.clone(),
                is_incoming: if hop.is_undirected {
                    hop.is_incoming
                } else {
                    !hop.is_incoming
                },
                is_undirected: hop.is_undirected,
                min_hops: 1,
                max_hops: 1,
                unique_rels: prior_rels.clone(),
                needs_path: hop.needs_path,
            };
            prior_rels.push(hop.rel_var.clone());
        }

        // The far endpoint's label is now carried by the scan; drop its HasLabel
        // predicate and re-add the original start label so it is still enforced.
        filters.retain(
            |f| !matches!(f, FilterExpr::HasLabel(v, l) if *v == terminal_var && *l == term_lbl),
        );
        filters.push(FilterExpr::HasLabel(start_var, start_lbl));
        tree
    }

    /// Return true when `filters` holds an equality predicate `var.prop = literal`
    /// (or `literal = var.prop`) backed by an existing node property index.
    fn has_indexed_eq(
        var: &str,
        label: &str,
        filters: &[FilterExpr],
        stats: Option<&dyn StatsProvider>,
    ) -> bool {
        let Some(s) = stats else { return false };
        filters.iter().any(|f| {
            let FilterExpr::Eq(l, r) = f else {
                return false;
            };
            let prop = match (l, r) {
                (Expr::Prop(v, p), Expr::Literal(_) | Expr::Param(_))
                    if v == var && !p.is_empty() =>
                {
                    p
                }
                (Expr::Literal(_) | Expr::Param(_), Expr::Prop(v, p))
                    if v == var && !p.is_empty() =>
                {
                    p
                }
                _ => return false,
            };
            s.has_node_property_index(label, prop)
        })
    }

    /// Return true when a predicate is statically, unconditionally true and can be
    /// dropped. Conservative: only literal-`true` and equality or inequality of two
    /// identical-form literals are recognized; false or unknown predicates are not
    /// touched (folding a false predicate to "drop" would change results).
    fn is_trivially_true(f: &FilterExpr) -> bool {
        match f {
            FilterExpr::Expr(Expr::Literal(Literal::Bool(true))) => true,
            FilterExpr::Eq(Expr::Literal(a), Expr::Literal(b)) => a == b,
            FilterExpr::Ne(Expr::Literal(a), Expr::Literal(b)) => a != b,
            _ => false,
        }
    }

    /// When one side of an equality is `id(var)` and the other is a literal or
    /// parameter, return the constant id expression; otherwise `None`. Used to
    /// rewrite `WHERE id(n) = <const>` into a primary-key seek.
    fn id_seek_value(l: &Expr, r: &Expr, var: &str) -> Option<Expr> {
        let is_id_of_var = |e: &Expr| {
            matches!(e, Expr::FunctionCall { name, args }
                if name == "id"
                    && args.len() == 1
                    && matches!(&args[0], Expr::Prop(v, p) if v == var && p.is_empty()))
        };
        let is_const = |e: &Expr| matches!(e, Expr::Literal(_) | Expr::Param(_));
        if is_id_of_var(l) && is_const(r) {
            Some(r.clone())
        } else if is_id_of_var(r) && is_const(l) {
            Some(l.clone())
        } else {
            None
        }
    }

    /// Replace a `count(*)`/`count(n)` aggregation over a bare labeled node scan,
    /// or over a bare typed single-hop directed expand, with a constant read from
    /// graph metadata. Fires only when the aggregate has no grouping keys and a
    /// single non-distinct count whose inner expression is `count(*)` or a bare
    /// variable, and its input is exactly one of:
    ///
    /// - a `LabelScan` with a known label whose node count is available, or
    /// - an `Expand` of exactly one hop, directed, with a known relationship
    ///   type, over an unlabeled full-node `LabelScan`, whose type count is
    ///   available. Undirected expands are excluded because each edge matches in
    ///   both directions, and labeled or filtered endpoints are excluded because
    ///   type metadata ignores them.
    ///
    /// A zero count is left to normal execution so empty-result semantics are
    /// preserved.
    fn reduce_count(op: PhysicalOperator, stats: Option<&dyn StatsProvider>) -> PhysicalOperator {
        match op {
            PhysicalOperator::Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                if group_by.is_empty() && aggregations.len() == 1 {
                    let (agg_fn, inner, col) = &aggregations[0];
                    let plain_count = matches!(agg_fn, AggFn::Count { distinct: false });
                    let counts_rows = matches!(inner, Expr::CountStar)
                        || matches!(inner, Expr::Prop(_, p) if p.is_empty());
                    if plain_count && counts_rows {
                        let metadata_count = match input.as_ref() {
                            PhysicalOperator::LabelScan {
                                label: Some(lbl), ..
                            } => stats.and_then(|s| s.node_count_by_label(lbl)),
                            PhysicalOperator::Expand {
                                input: scan,
                                rel_type: Some(rtype),
                                is_undirected: false,
                                min_hops: 1,
                                max_hops: 1,
                                ..
                            } if matches!(
                                scan.as_ref(),
                                PhysicalOperator::LabelScan { label: None, .. }
                            ) =>
                            {
                                stats.and_then(|s| s.edge_count_by_type(rtype))
                            }
                            _ => None,
                        };
                        if let Some(n) = metadata_count {
                            if n > 0 {
                                return PhysicalOperator::Project {
                                    input: Box::new(PhysicalOperator::SingleRow),
                                    items: vec![(
                                        Expr::Literal(Literal::Int(n as i64)),
                                        Some(col.clone()),
                                    )],
                                    is_barrier: false,
                                };
                            }
                        }
                    }
                }
                PhysicalOperator::Aggregate {
                    input,
                    group_by,
                    aggregations,
                }
            }
            PhysicalOperator::Project {
                input,
                items,
                is_barrier,
            } if !is_barrier => PhysicalOperator::Project {
                input: Box::new(Self::reduce_count(*input, stats)),
                items,
                is_barrier,
            },
            PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
                input: Box::new(Self::reduce_count(*input, stats)),
                items,
            },
            PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
                input: Box::new(Self::reduce_count(*input, stats)),
                skip,
                count,
            },
            PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
                keys,
                input: Box::new(Self::reduce_count(*input, stats)),
            },
            other => other,
        }
    }
}

/// Rewrite `Expand` nodes whose `dst_var` is already bound by an ancestor
/// operator into `MultiwayJoin` nodes. Applies to single-hop patterns, directed
/// or undirected, where the closing check would otherwise iterate all neighbors
/// and filter by value. Undirected closing hops set `closing_is_undirected` so
/// the executor checks both edge directions.
fn rewrite_closing_expands(op: PhysicalOperator) -> PhysicalOperator {
    match op {
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
            let new_input = rewrite_closing_expands(*input);
            let input_bound = Optimizer::bound_vars(&new_input);
            // A closing join never extends `_path_*` objects, so a pattern that
            // binds a path variable keeps its plain Expand.
            if min_hops == 1 && max_hops == 1 && !needs_path && input_bound.contains(&dst_var) {
                PhysicalOperator::MultiwayJoin {
                    input: Box::new(new_input),
                    closing_src_var: src_var,
                    closing_dst_var: dst_var,
                    closing_rel_type: rel_type,
                    closing_rel_var: rel_var,
                    closing_is_incoming: is_incoming,
                    closing_is_undirected: is_undirected,
                    closing_unique_rels: unique_rels,
                }
            } else {
                PhysicalOperator::Expand {
                    input: Box::new(new_input),
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
                }
            }
        }
        PhysicalOperator::Filter { input, expression } => PhysicalOperator::Filter {
            input: Box::new(rewrite_closing_expands(*input)),
            expression,
        },
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
        } => PhysicalOperator::Project {
            input: Box::new(rewrite_closing_expands(*input)),
            items,
            is_barrier,
        },
        PhysicalOperator::HashJoin { left, right } => PhysicalOperator::HashJoin {
            left: Box::new(rewrite_closing_expands(*left)),
            right: Box::new(rewrite_closing_expands(*right)),
        },
        PhysicalOperator::Aggregate {
            input,
            group_by,
            aggregations,
        } => PhysicalOperator::Aggregate {
            input: Box::new(rewrite_closing_expands(*input)),
            group_by,
            aggregations,
        },
        PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
            input: Box::new(rewrite_closing_expands(*input)),
            items,
        },
        PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
            input: Box::new(rewrite_closing_expands(*input)),
            skip,
            count,
        },
        PhysicalOperator::Unwind {
            input,
            expr,
            variable,
        } => PhysicalOperator::Unwind {
            input: Box::new(rewrite_closing_expands(*input)),
            expr,
            variable,
        },
        PhysicalOperator::OptionalMatch { input, null_vars } => PhysicalOperator::OptionalMatch {
            input: Box::new(rewrite_closing_expands(*input)),
            null_vars,
        },
        PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
            keys,
            input: Box::new(rewrite_closing_expands(*input)),
        },
        PhysicalOperator::WritePart { input, part } => PhysicalOperator::WritePart {
            input: Box::new(rewrite_closing_expands(*input)),
            part,
        },
        // MultiwayJoin is already a closing join; recurse into its input only.
        PhysicalOperator::MultiwayJoin {
            input,
            closing_src_var,
            closing_dst_var,
            closing_rel_type,
            closing_rel_var,
            closing_is_incoming,
            closing_is_undirected,
            closing_unique_rels,
        } => PhysicalOperator::MultiwayJoin {
            input: Box::new(rewrite_closing_expands(*input)),
            closing_src_var,
            closing_dst_var,
            closing_rel_type,
            closing_rel_var,
            closing_is_incoming,
            closing_is_undirected,
            closing_unique_rels,
        },
        leaf => leaf,
    }
}

/// Replace a grouping-free, non-distinct `count` aggregate sitting directly on
/// a `MultiwayJoin`-closed directed triangle chain with the leaf
/// `TriangleCount` operator, which the executor answers via one core kernel
/// call instead of materializing every row of the pattern.
///
/// Recursion is limited to the operators that can sit between the query root
/// and a RETURN-level aggregate (`Project`, `Limit`, `Sort`, `Distinct`, and
/// `Filter`); anywhere else the plan is left untouched, which only forfeits
/// the fast path, never correctness.
fn rewrite_triangle_count(op: PhysicalOperator) -> PhysicalOperator {
    if let Some(t) = try_triangle_count(&op) {
        return t;
    }
    match op {
        PhysicalOperator::Project {
            input,
            items,
            is_barrier,
        } => PhysicalOperator::Project {
            input: Box::new(rewrite_triangle_count(*input)),
            items,
            is_barrier,
        },
        PhysicalOperator::Limit { input, skip, count } => PhysicalOperator::Limit {
            input: Box::new(rewrite_triangle_count(*input)),
            skip,
            count,
        },
        PhysicalOperator::Sort { input, items } => PhysicalOperator::Sort {
            input: Box::new(rewrite_triangle_count(*input)),
            items,
        },
        PhysicalOperator::Distinct { input, keys } => PhysicalOperator::Distinct {
            keys,
            input: Box::new(rewrite_triangle_count(*input)),
        },
        PhysicalOperator::Filter { input, expression } => PhysicalOperator::Filter {
            input: Box::new(rewrite_triangle_count(*input)),
            expression,
        },
        other => other,
    }
}

/// Strip one `HasLabel` filter, returning the label predicate and the inner
/// operator. Any other filter shape stays in place and fails the pattern
/// match in `try_triangle_count`.
fn peel_one_haslabel(op: &PhysicalOperator) -> (Option<(&str, &str)>, &PhysicalOperator) {
    if let PhysicalOperator::Filter {
        input,
        expression: FilterExpr::HasLabel(var, label),
    } = op
    {
        (Some((var.as_str(), label.as_str())), input.as_ref())
    } else {
        (None, op)
    }
}

/// Match the exact triangle-count plan shape and build its replacement.
///
/// Required shape, bottom-up: `LabelScan a`, `Expand a->b` (single-hop,
/// directed, no path), optional `HasLabel` on `b`, `Expand b->c` (same
/// restrictions, unique against hop 1), optional `HasLabel` on `c`,
/// `MultiwayJoin` closing `c->a` (directed, unique against both hops), and an
/// `Aggregate` with no grouping whose only aggregation is a non-distinct
/// `count(*)` or `count(var)` over a pattern variable. The `unique_rels`
/// wiring is checked exactly so the chain is one MATCH pattern; relationship
/// uniqueness across separate patterns does not apply and must not be folded
/// into the kernel.
fn try_triangle_count(op: &PhysicalOperator) -> Option<PhysicalOperator> {
    let PhysicalOperator::Aggregate {
        input,
        group_by,
        aggregations,
    } = op
    else {
        return None;
    };
    if !group_by.is_empty() || aggregations.len() != 1 {
        return None;
    }
    let (agg_fn, count_expr, out_name) = &aggregations[0];
    if !matches!(agg_fn, AggFn::Count { distinct: false }) {
        return None;
    }

    let PhysicalOperator::MultiwayJoin {
        input: mj_input,
        closing_src_var,
        closing_dst_var,
        closing_rel_type,
        closing_rel_var,
        closing_is_incoming: false,
        closing_is_undirected: false,
        closing_unique_rels,
    } = input.as_ref()
    else {
        return None;
    };

    let (c_label, exp2_op) = peel_one_haslabel(mj_input);
    let PhysicalOperator::Expand {
        input: e2_input,
        src_var: src2,
        rel_var: rel2,
        dst_var: dst2,
        rel_type: t2,
        is_incoming: false,
        is_undirected: false,
        min_hops: 1,
        max_hops: 1,
        unique_rels: unique2,
        needs_path: false,
    } = exp2_op
    else {
        return None;
    };

    let (b_label, exp1_op) = peel_one_haslabel(e2_input);
    let PhysicalOperator::Expand {
        input: e1_input,
        src_var: src1,
        rel_var: rel1,
        dst_var: dst1,
        rel_type: t1,
        is_incoming: false,
        is_undirected: false,
        min_hops: 1,
        max_hops: 1,
        unique_rels: unique1,
        needs_path: false,
    } = exp1_op
    else {
        return None;
    };

    let PhysicalOperator::LabelScan {
        variable: a_var,
        label: a_label,
    } = e1_input.as_ref()
    else {
        return None;
    };

    // Cycle wiring: a -> b -> c -> a over three distinct node variables.
    let (b_var, c_var) = (dst1, dst2);
    if src1 != a_var || src2 != b_var || closing_src_var != c_var || closing_dst_var != a_var {
        return None;
    }
    if a_var == b_var || b_var == c_var || a_var == c_var {
        return None;
    }
    // Peeled label filters must apply to the variable they sit above.
    if let Some((v, _)) = c_label {
        if v != c_var {
            return None;
        }
    }
    if let Some((v, _)) = b_label {
        if v != b_var {
            return None;
        }
    }
    // One-pattern relationship uniqueness, exactly as the kernel encodes it.
    if !unique1.is_empty()
        || unique2.as_slice() != [rel1.clone()]
        || closing_unique_rels.as_slice() != [rel1.clone(), rel2.clone()]
    {
        return None;
    }
    // The counted expression must be `*` or a bare pattern variable, all of
    // which are non-null in every match.
    match count_expr {
        Expr::CountStar => {}
        Expr::Prop(v, p)
            if p.is_empty()
                && (v == a_var
                    || v == b_var
                    || v == c_var
                    || v == rel1
                    || v == rel2
                    || v == closing_rel_var) => {}
        _ => return None,
    }

    Some(PhysicalOperator::TriangleCount {
        rel_types: [t1.clone(), t2.clone(), closing_rel_type.clone()],
        labels: [
            a_label.clone(),
            b_label.map(|(_, l)| l.to_string()),
            c_label.map(|(_, l)| l.to_string()),
        ],
        output: out_name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::plan::logical::LogicalPlanner;
    use crate::plan::physical::PhysicalPlanner;

    #[test]
    fn test_filter_pushdown_basic() {
        // Query: MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name
        let stmt = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name",
        )
        .unwrap();

        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };

        let logical_plan = LogicalPlanner::plan(&query).unwrap();
        let physical_plan = PhysicalPlanner::plan(&logical_plan);
        let optimized_plan = Optimizer::optimize(physical_plan, None);

        // Optimized physical plan structure should have:
        // Project
        //   Filter (b:Person label check)
        //     Expand (a -> b)
        //       Filter (a.age = 30)
        //         LabelScan (a:Person)

        if let PhysicalOperator::Project { input, .. } = optimized_plan {
            if let PhysicalOperator::Filter {
                input: filter_input,
                expression,
            } = *input
            {
                assert!(
                    matches!(expression, FilterExpr::HasLabel(ref var, ref label) if var == "b" && label == "Person")
                );

                if let PhysicalOperator::Expand {
                    input: expand_input,
                    src_var,
                    dst_var,
                    ..
                } = *filter_input
                {
                    assert_eq!(src_var, "a");
                    assert_eq!(dst_var, "b");

                    if let PhysicalOperator::Filter {
                        input: scan_input,
                        expression: scan_expr,
                    } = *expand_input
                    {
                        assert!(
                            matches!(scan_expr, FilterExpr::Eq(Expr::Prop(ref var, ref prop), Expr::Literal(_)) if var == "a" && prop == "age")
                        );

                        assert!(
                            matches!(*scan_input, PhysicalOperator::LabelScan { ref variable, ref label } if variable == "a" && label.as_deref() == Some("Person"))
                        );
                    } else {
                        panic!("expected Filter for a.age = 30 wrapping LabelScan");
                    }
                } else {
                    panic!("expected Expand operator");
                }
            } else {
                panic!("expected Filter for b label check");
            }
        } else {
            panic!("expected Project operator");
        }
    }

    /// Peel consecutive `Filter` nodes off `op`, returning the collected
    /// predicates and the first non-Filter operator.
    fn peel_filters(mut op: PhysicalOperator) -> (Vec<FilterExpr>, PhysicalOperator) {
        let mut filters = Vec::new();
        while let PhysicalOperator::Filter { input, expression } = op {
            filters.push(expression);
            op = *input;
        }
        (filters, op)
    }

    #[test]
    fn test_and_conjuncts_split_and_push_independently() {
        // The conjunction as a whole references both `a` and `b`, but each
        // conjunct references one variable, so the split must let `a.age = 30`
        // reach the scan below the Expand while `b.age = 40` stays above it.
        let stmt = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             WHERE a.age = 30 AND b.age = 40 RETURN b.name AS name",
        )
        .unwrap();
        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };

        let logical_plan = LogicalPlanner::plan(&query).unwrap();
        let physical_plan = PhysicalPlanner::plan(&logical_plan);
        let optimized = Optimizer::optimize(physical_plan, None);

        let PhysicalOperator::Project { input, .. } = optimized else {
            panic!("expected Project at the root");
        };
        let (above, expand) = peel_filters(*input);
        let PhysicalOperator::Expand { input, .. } = expand else {
            panic!("expected Expand below the b-side filters, got {expand:?}");
        };
        let (below, scan) = peel_filters(*input);
        assert!(matches!(
            scan,
            PhysicalOperator::LabelScan { ref variable, .. } if variable == "a"
        ));

        let refs_only = |f: &FilterExpr, var: &str| {
            let vars = Optimizer::referenced_vars(f);
            vars.len() == 1 && vars.contains(var)
        };
        assert!(
            above.iter().all(|f| refs_only(f, "b")),
            "only b-conjuncts may stay above the Expand, got {above:?}"
        );
        assert!(
            above.iter().any(|f| !matches!(f, FilterExpr::HasLabel(..))),
            "the b.age conjunct must survive the split, got {above:?}"
        );
        assert_eq!(
            below.len(),
            1,
            "exactly the a.age conjunct belongs below the Expand, got {below:?}"
        );
        assert!(refs_only(&below[0], "a"));
    }

    #[test]
    fn test_filter_pushdown_join_and_boundary() {
        let plan = PhysicalOperator::HashJoin {
            left: Box::new(PhysicalOperator::LabelScan {
                variable: "a".to_string(),
                label: Some("Person".to_string()),
            }),
            right: Box::new(PhysicalOperator::LabelScan {
                variable: "b".to_string(),
                label: Some("Company".to_string()),
            }),
        };

        // Let's add a filter that references 'a' only: a.name = "Alice"
        let filter_a = FilterExpr::Eq(
            Expr::Prop("a".to_string(), "name".to_string()),
            Expr::Literal(crate::ast::Literal::Str("Alice".to_string())),
        );

        // Let's add a filter that references 'b' only: b.industry = "Tech"
        let filter_b = FilterExpr::Eq(
            Expr::Prop("b".to_string(), "industry".to_string()),
            Expr::Literal(crate::ast::Literal::Str("Tech".to_string())),
        );

        // Let's add a filter that references both 'a' and 'b': a.id = b.id
        let filter_join = FilterExpr::Eq(
            Expr::Prop("a".to_string(), "id".to_string()),
            Expr::Prop("b".to_string(), "id".to_string()),
        );

        // Let's wrap our join plan with these filters
        let plan_with_filters = PhysicalOperator::Filter {
            input: Box::new(PhysicalOperator::Filter {
                input: Box::new(PhysicalOperator::Filter {
                    input: Box::new(plan),
                    expression: filter_join.clone(),
                }),
                expression: filter_b.clone(),
            }),
            expression: filter_a.clone(),
        };

        let optimized = Optimizer::optimize(plan_with_filters, None);

        // Expected optimized plan:
        // Filter (a.id = b.id) [Join level]
        //   HashJoin
        //     left: Filter (a.name = "Alice") [Pushed down to a]
        //       LabelScan (a)
        //     right: Filter (b.industry = "Tech") [Pushed down to b]
        //       LabelScan (b)

        if let PhysicalOperator::Filter {
            input: join_input,
            expression: join_expr,
        } = optimized
        {
            assert_eq!(join_expr, filter_join);

            if let PhysicalOperator::HashJoin { left, right } = *join_input {
                if let PhysicalOperator::Filter {
                    input: left_scan,
                    expression: left_expr,
                } = *left
                {
                    assert_eq!(left_expr, filter_a);
                    assert!(
                        matches!(*left_scan, PhysicalOperator::LabelScan { ref variable, .. } if variable == "a")
                    );
                } else {
                    panic!("expected pushed-down filter on left branch");
                }

                if let PhysicalOperator::Filter {
                    input: right_scan,
                    expression: right_expr,
                } = *right
                {
                    assert_eq!(right_expr, filter_b);
                    assert!(
                        matches!(*right_scan, PhysicalOperator::LabelScan { ref variable, .. } if variable == "b")
                    );
                } else {
                    panic!("expected pushed-down filter on right branch");
                }
            } else {
                panic!("expected HashJoin operator");
            }
        } else {
            panic!("expected Filter at root for join predicate");
        }
    }

    #[test]
    fn test_operator_reordering() {
        let simple_branch = PhysicalOperator::LabelScan {
            variable: "a".to_string(),
            label: Some("Person".to_string()),
        };

        let complex_branch = PhysicalOperator::Expand {
            input: Box::new(PhysicalOperator::LabelScan {
                variable: "b".to_string(),
                label: Some("Company".to_string()),
            }),
            src_var: "b".to_string(),
            rel_var: "r".to_string(),
            dst_var: "c".to_string(),
            rel_type: Some("EMPLOYEE".to_string()),
            is_incoming: false,
            is_undirected: false,
            min_hops: 1,
            max_hops: 1,
            unique_rels: vec![],
            needs_path: false,
        };

        let join_plan = PhysicalOperator::HashJoin {
            left: Box::new(simple_branch.clone()),
            right: Box::new(complex_branch.clone()),
        };

        let optimized = Optimizer::optimize(join_plan, None);

        if let PhysicalOperator::HashJoin { left, right } = optimized {
            // Complex branch (weight 10 = LabelScan(1) * Expand rel_weight(10)) should be left.
            assert_eq!(*left, complex_branch);
            assert_eq!(*right, simple_branch);
        } else {
            panic!("expected HashJoin");
        }
    }

    #[test]
    fn test_expand_weight_uses_average_fanout() {
        // With a node-count estimate available, the Expand weight scales with the
        // average fan-out (edges / nodes), not the total edge count, so chained
        // multi-hop expands stay in a realistic range instead of saturating.
        struct DegreeStats;
        impl StatsProvider for DegreeStats {
            fn node_count_by_label(&self, label: &str) -> Option<u64> {
                (label == "Person").then_some(1000)
            }
            fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
                (etype == "KNOWS").then_some(5000)
            }
            fn total_node_count(&self) -> Option<u64> {
                Some(1000)
            }
        }
        let stats = DegreeStats;

        let scan = PhysicalOperator::LabelScan {
            variable: "a".to_string(),
            label: Some("Person".to_string()),
        };
        let expand = |input: PhysicalOperator| PhysicalOperator::Expand {
            input: Box::new(input),
            src_var: "a".to_string(),
            rel_var: "r".to_string(),
            dst_var: "b".to_string(),
            rel_type: Some("KNOWS".to_string()),
            is_incoming: false,
            is_undirected: false,
            min_hops: 1,
            max_hops: 1,
            unique_rels: vec![],
            needs_path: false,
        };

        // Average fan-out = ceil(5000 / 1000) = 5; input weight (Person) = 1000.
        let one_hop = expand(scan.clone());
        assert_eq!(Optimizer::plan_weight(&one_hop, Some(&stats)), 1000 * 5);

        // A three-hop chain stays at 1000 * 5^3 = 125_000. The old total-edge
        // multiplier would have been 1000 * 5000^3, which saturates `usize`.
        let three_hop = expand(expand(expand(scan)));
        let w = Optimizer::plan_weight(&three_hop, Some(&stats));
        assert_eq!(w, 1000 * 5 * 5 * 5);
        assert!(w < usize::MAX, "multi-hop estimate must not saturate");
    }

    struct MockStats {
        node_counts: std::collections::HashMap<String, u64>,
        edge_counts: std::collections::HashMap<String, u64>,
    }

    impl StatsProvider for MockStats {
        fn node_count_by_label(&self, label: &str) -> Option<u64> {
            self.node_counts.get(label).copied()
        }

        fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
            self.edge_counts.get(etype).copied()
        }
    }

    #[test]
    fn test_cost_based_operator_reordering() {
        let scan_a = PhysicalOperator::LabelScan {
            variable: "a".to_string(),
            label: Some("Person".to_string()),
        };
        let scan_b = PhysicalOperator::LabelScan {
            variable: "b".to_string(),
            label: Some("Company".to_string()),
        };

        let join_plan = PhysicalOperator::HashJoin {
            left: Box::new(scan_a.clone()),
            right: Box::new(scan_b.clone()),
        };

        // Case 1: Person (10) < Company (100).
        // Since HashJoin reorders heavier branch to the left, Company should be left, Person right.
        let mut node_counts = std::collections::HashMap::new();
        node_counts.insert("Person".to_string(), 10);
        node_counts.insert("Company".to_string(), 100);
        let stats1 = MockStats {
            node_counts: node_counts.clone(),
            edge_counts: std::collections::HashMap::new(),
        };

        let optimized1 = Optimizer::optimize(join_plan.clone(), Some(&stats1));
        if let PhysicalOperator::HashJoin { left, right } = optimized1 {
            assert_eq!(*left, scan_b);
            assert_eq!(*right, scan_a);
        } else {
            panic!("expected HashJoin");
        }

        // Case 2: Person (500) > Company (20).
        // Person should be left, Company right.
        let mut node_counts = std::collections::HashMap::new();
        node_counts.insert("Person".to_string(), 500);
        node_counts.insert("Company".to_string(), 20);
        let stats2 = MockStats {
            node_counts,
            edge_counts: std::collections::HashMap::new(),
        };

        let optimized2 = Optimizer::optimize(join_plan, Some(&stats2));
        if let PhysicalOperator::HashJoin { left, right } = optimized2 {
            assert_eq!(*left, scan_a);
            assert_eq!(*right, scan_b);
        } else {
            panic!("expected HashJoin");
        }
    }

    #[test]
    fn test_rewrite_closing_expands_triangle() {
        // A triangle pattern MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c)-[:KNOWS]->(a)
        // should have its final Expand (c → a) rewritten to MultiwayJoin because
        // `a` is already bound by the LabelScan.
        let plan = PhysicalOperator::Expand {
            input: Box::new(PhysicalOperator::Expand {
                input: Box::new(PhysicalOperator::Expand {
                    input: Box::new(PhysicalOperator::LabelScan {
                        variable: "a".to_string(),
                        label: Some("Person".to_string()),
                    }),
                    src_var: "a".to_string(),
                    rel_var: "r1".to_string(),
                    dst_var: "b".to_string(),
                    rel_type: Some("KNOWS".to_string()),
                    is_incoming: false,
                    is_undirected: false,
                    min_hops: 1,
                    max_hops: 1,
                    unique_rels: vec![],
                    needs_path: false,
                }),
                src_var: "b".to_string(),
                rel_var: "r2".to_string(),
                dst_var: "c".to_string(),
                rel_type: Some("KNOWS".to_string()),
                is_incoming: false,
                is_undirected: false,
                min_hops: 1,
                max_hops: 1,
                unique_rels: vec![],
                needs_path: false,
            }),
            src_var: "c".to_string(),
            rel_var: "r3".to_string(),
            dst_var: "a".to_string(), // already bound; this is the closing hop

            rel_type: Some("KNOWS".to_string()),
            is_incoming: false,
            is_undirected: false,
            min_hops: 1,
            max_hops: 1,
            unique_rels: vec![],
            needs_path: false,
        };

        let rewritten = rewrite_closing_expands(plan);

        // The top-level operator must now be MultiwayJoin.
        match rewritten {
            PhysicalOperator::MultiwayJoin {
                closing_src_var,
                closing_dst_var,
                closing_rel_type,
                closing_is_incoming,
                ..
            } => {
                assert_eq!(closing_src_var, "c");
                assert_eq!(closing_dst_var, "a");
                assert_eq!(closing_rel_type.as_deref(), Some("KNOWS"));
                assert!(!closing_is_incoming);
            }
            other => panic!("expected MultiwayJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_rewrite_closing_expands_open_chain_unchanged() {
        // An open 3-hop chain (no cycle) must not be rewritten.
        let plan = PhysicalOperator::Expand {
            input: Box::new(PhysicalOperator::Expand {
                input: Box::new(PhysicalOperator::LabelScan {
                    variable: "a".to_string(),
                    label: Some("Person".to_string()),
                }),
                src_var: "a".to_string(),
                rel_var: "r1".to_string(),
                dst_var: "b".to_string(),
                rel_type: Some("KNOWS".to_string()),
                is_incoming: false,
                is_undirected: false,
                min_hops: 1,
                max_hops: 1,
                unique_rels: vec![],
                needs_path: false,
            }),
            src_var: "b".to_string(),
            rel_var: "r2".to_string(),
            dst_var: "c".to_string(), // fresh variable
            rel_type: Some("KNOWS".to_string()),
            is_incoming: false,
            is_undirected: false,
            min_hops: 1,
            max_hops: 1,
            unique_rels: vec![],
            needs_path: false,
        };

        let rewritten = rewrite_closing_expands(plan);

        assert!(
            matches!(rewritten, PhysicalOperator::Expand { dst_var, .. } if dst_var == "c"),
            "open chain must remain as Expand"
        );
    }

    #[test]
    fn test_rewrite_closing_expands_undirected() {
        // An undirected single-hop into an already-bound destination is also a
        // closing hop: MATCH (a)-[:KNOWS]->(b), (a)-[r]-(b) closes on `b`. It
        // should rewrite to a MultiwayJoin that knows it is undirected, so the
        // executor checks both edge directions instead of expanding all
        // neighbors and filtering.
        let plan = PhysicalOperator::Expand {
            input: Box::new(PhysicalOperator::Expand {
                input: Box::new(PhysicalOperator::LabelScan {
                    variable: "a".to_string(),
                    label: Some("Person".to_string()),
                }),
                src_var: "a".to_string(),
                rel_var: "r1".to_string(),
                dst_var: "b".to_string(),
                rel_type: Some("KNOWS".to_string()),
                is_incoming: false,
                is_undirected: false,
                min_hops: 1,
                max_hops: 1,
                unique_rels: vec![],
                needs_path: false,
            }),
            src_var: "a".to_string(),
            rel_var: "r".to_string(),
            dst_var: "b".to_string(), // already bound; undirected closing hop

            rel_type: Some("KNOWS".to_string()),
            is_incoming: false,
            is_undirected: true,
            min_hops: 1,
            max_hops: 1,
            unique_rels: vec![],
            needs_path: false,
        };

        let rewritten = rewrite_closing_expands(plan);

        match rewritten {
            PhysicalOperator::MultiwayJoin {
                closing_src_var,
                closing_dst_var,
                closing_is_undirected,
                ..
            } => {
                assert_eq!(closing_src_var, "a");
                assert_eq!(closing_dst_var, "b");
                assert!(
                    closing_is_undirected,
                    "undirected closing hop must set closing_is_undirected"
                );
            }
            other => panic!("expected MultiwayJoin, got {other:?}"),
        }
    }

    /// A `StatsProvider` with fixed label/type counts and an optional set of
    /// node property indexes, for exercising cost-driven optimizer passes.
    struct TestStats {
        labels: std::collections::HashMap<String, u64>,
        types: std::collections::HashMap<String, u64>,
        indexes: HashSet<(String, String)>,
    }

    impl TestStats {
        fn new(labels: &[(&str, u64)]) -> Self {
            Self {
                labels: labels.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
                types: std::collections::HashMap::new(),
                indexes: HashSet::new(),
            }
        }

        fn with_index(mut self, label: &str, property: &str) -> Self {
            self.indexes
                .insert((label.to_string(), property.to_string()));
            self
        }

        fn with_type(mut self, etype: &str, count: u64) -> Self {
            self.types.insert(etype.to_string(), count);
            self
        }
    }

    impl StatsProvider for TestStats {
        fn node_count_by_label(&self, label: &str) -> Option<u64> {
            self.labels.get(label).copied()
        }
        fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
            self.types.get(etype).copied()
        }
        fn has_node_property_index(&self, label: &str, property: &str) -> bool {
            self.indexes
                .contains(&(label.to_string(), property.to_string()))
        }
    }

    fn optimize_query(cypher: &str, stats: &dyn StatsProvider) -> PhysicalOperator {
        let stmt = parser::parse(cypher).unwrap();
        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };
        let logical = LogicalPlanner::plan(&query).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        Optimizer::optimize(physical, Some(stats))
    }

    /// Return the variable and label of the deepest `LabelScan` in a plan.
    fn bottom_scan(op: &PhysicalOperator) -> Option<(String, Option<String>)> {
        match op {
            PhysicalOperator::LabelScan { variable, label } => {
                Some((variable.clone(), label.clone()))
            }
            PhysicalOperator::Expand { input, .. }
            | PhysicalOperator::Filter { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => bottom_scan(input),
            _ => None,
        }
    }

    fn has_haslabel(op: &PhysicalOperator, var: &str, label: &str) -> bool {
        match op {
            PhysicalOperator::Filter { input, expression } => {
                matches!(expression, FilterExpr::HasLabel(v, l) if v == var && l == label)
                    || has_haslabel(input, var, label)
            }
            PhysicalOperator::Expand { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => has_haslabel(input, var, label),
            _ => false,
        }
    }

    #[test]
    fn select_scan_reverses_to_rarer_endpoint() {
        // Person is common, City is rare: start the traversal from City and walk
        // the KNOWS edge incoming back to Person.
        let stats = TestStats::new(&[("Person", 1000), ("City", 10)]);
        let plan = optimize_query("MATCH (a:Person)-[:KNOWS]->(b:City) RETURN a, b", &stats);

        let (var, label) = bottom_scan(&plan).expect("a label scan");
        assert_eq!(var, "b", "should scan the rarer City endpoint first");
        assert_eq!(label.as_deref(), Some("City"));
        // The original start label must still be enforced as a HasLabel predicate.
        assert!(
            has_haslabel(&plan, "a", "Person"),
            "old start label must be re-added as a filter"
        );
    }

    #[test]
    fn select_scan_keeps_rarer_start_unchanged() {
        // Start endpoint is already the rarer one: no reversal.
        let stats = TestStats::new(&[("Person", 10), ("City", 1000)]);
        let plan = optimize_query("MATCH (a:Person)-[:KNOWS]->(b:City) RETURN a, b", &stats);

        let (var, label) = bottom_scan(&plan).expect("a label scan");
        assert_eq!(var, "a", "rarer start endpoint must be kept");
        assert_eq!(label.as_deref(), Some("Person"));
    }

    /// Collect the `needs_path` flag of every `Expand` in a plan, top-down.
    fn expand_path_flags(op: &PhysicalOperator, out: &mut Vec<bool>) {
        match op {
            PhysicalOperator::Expand {
                input, needs_path, ..
            } => {
                out.push(*needs_path);
                expand_path_flags(input, out);
            }
            PhysicalOperator::Filter { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => expand_path_flags(input, out),
            _ => {}
        }
    }

    /// `needs_path` is set per pattern: plain patterns plan every hop with
    /// `false` (no per-row `_path_*` materialization), and a pattern that binds
    /// a path variable plans every hop with `true`.
    #[test]
    fn needs_path_flag_follows_path_variable() {
        let stats = TestStats::new(&[("Person", 100)]);

        let plain = optimize_query(
            "MATCH (a:Person)-[:KNOWS]->()-[:KNOWS]->(c) RETURN c",
            &stats,
        );
        let mut flags = Vec::new();
        expand_path_flags(&plain, &mut flags);
        assert_eq!(flags, vec![false, false]);

        let named = optimize_query(
            "MATCH p = (a:Person)-[:KNOWS]->()-[:KNOWS]->(c) RETURN p",
            &stats,
        );
        let mut flags = Vec::new();
        expand_path_flags(&named, &mut flags);
        assert_eq!(flags, vec![true, true]);
    }

    /// A cyclic named-path pattern keeps its closing hop as a plain `Expand`:
    /// the `MultiwayJoin` rewrite never extends `_path_*` objects, so applying
    /// it would drop the path binding.
    #[test]
    fn closing_hop_stays_expand_for_named_path() {
        fn has_multiway(op: &PhysicalOperator) -> bool {
            match op {
                PhysicalOperator::MultiwayJoin { .. } => true,
                PhysicalOperator::Expand { input, .. }
                | PhysicalOperator::Filter { input, .. }
                | PhysicalOperator::Project { input, .. }
                | PhysicalOperator::Aggregate { input, .. }
                | PhysicalOperator::Sort { input, .. }
                | PhysicalOperator::Limit { input, .. }
                | PhysicalOperator::Distinct { input, .. } => has_multiway(input),
                _ => false,
            }
        }
        let stats = TestStats::new(&[("Person", 100)]);
        let plan = optimize_query(
            "MATCH p = (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(a) RETURN p",
            &stats,
        );
        assert!(
            !has_multiway(&plan),
            "closing hop of a named path must stay a plain Expand"
        );
    }

    #[test]
    fn select_scan_prefers_indexed_endpoint_over_cardinality() {
        // City is rarer by count, but Person has an index-backed equality filter,
        // so the start must stay on Person despite the larger label count.
        let stats = TestStats::new(&[("Person", 1000), ("City", 10)]).with_index("Person", "name");
        let plan = optimize_query(
            "MATCH (a:Person)-[:KNOWS]->(b:City) WHERE a.name = 'Alice' RETURN a, b",
            &stats,
        );

        // The start stays on Person, so the index-scan pass turns it into a
        // NodeIndexScan on `a` rather than reversing toward City.
        fn finds_index_scan_on(op: &PhysicalOperator, var: &str) -> bool {
            match op {
                PhysicalOperator::NodeIndexScan { variable, .. } => variable == var,
                PhysicalOperator::Expand { input, .. }
                | PhysicalOperator::Filter { input, .. }
                | PhysicalOperator::Project { input, .. }
                | PhysicalOperator::Aggregate { input, .. }
                | PhysicalOperator::Sort { input, .. }
                | PhysicalOperator::Limit { input, .. }
                | PhysicalOperator::Distinct { input, .. }
                | PhysicalOperator::MultiwayJoin { input, .. } => finds_index_scan_on(input, var),
                _ => false,
            }
        }
        assert!(
            finds_index_scan_on(&plan, "a"),
            "index-backed endpoint must win over raw cardinality: {plan:?}"
        );
    }

    /// True when the plan contains a `NodeIndexScan` binding `var`, looking
    /// through every single-input operator.
    fn contains_index_scan_on(op: &PhysicalOperator, var: &str) -> bool {
        match op {
            PhysicalOperator::NodeIndexScan { variable, .. } => variable == var,
            PhysicalOperator::Expand { input, .. }
            | PhysicalOperator::Filter { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => contains_index_scan_on(input, var),
            _ => false,
        }
    }

    #[test]
    fn index_scan_rewrite_reaches_below_an_aggregate() {
        // The probe-count shape: an equality filter on the anchored start node
        // under a grouping-free count. The Aggregate sits between the plan root
        // and the filtered scan, and must not stop the index-scan pass.
        let stats = TestStats::new(&[("Person", 10_000)]).with_index("Person", "id");
        let plan = optimize_query(
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
             WHERE a.id = 1 RETURN count(c) AS n",
            &stats,
        );
        assert!(
            contains_index_scan_on(&plan, "a"),
            "aggregate root must not block the index-scan rewrite: {plan:?}"
        );
    }

    #[test]
    fn index_scan_rewrite_applies_to_split_conjuncts() {
        // A conjunct split from a top-level AND keeps its `Expr` comparison
        // form; the index-scan pass must treat it like the structured form.
        let stats = TestStats::new(&[("Person", 10_000)]).with_index("Person", "id");
        let plan = optimize_query(
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
             WHERE a.id = 1 AND c.id = 2 RETURN count(b) AS n",
            &stats,
        );
        assert!(
            contains_index_scan_on(&plan, "a"),
            "an AND-split equality conjunct must rewrite to NodeIndexScan: {plan:?}"
        );
    }

    #[test]
    fn range_scan_rewrite_applies_to_split_conjuncts() {
        fn find_range_scan(op: &PhysicalOperator) -> Option<(&Option<Expr>, &Option<Expr>)> {
            match op {
                PhysicalOperator::NodeRangeScan { lo, hi, .. } => Some((lo, hi)),
                PhysicalOperator::Expand { input, .. }
                | PhysicalOperator::Filter { input, .. }
                | PhysicalOperator::Project { input, .. }
                | PhysicalOperator::Aggregate { input, .. } => find_range_scan(input),
                _ => None,
            }
        }
        let stats = TestStats::new(&[("Person", 10_000)]).with_index("Person", "age");
        let plan = optimize_query(
            "MATCH (a:Person) WHERE a.age >= 30 AND a.age < 40 RETURN count(a) AS n",
            &stats,
        );
        let (lo, hi) = find_range_scan(&plan)
            .unwrap_or_else(|| panic!("AND-split range conjuncts must rewrite: {plan:?}"));
        assert!(
            lo.is_some() && hi.is_some(),
            "both conjuncts must narrow into one NodeRangeScan: {plan:?}"
        );
    }

    #[test]
    fn index_scan_rewrite_declines_null_literals() {
        // `prop = null` is never TRUE, but a NodeIndexScan with a null value
        // errors at evaluation (`json_to_prop_value` has no null form); the
        // filter must stay a filter and drop every row instead.
        let stats = TestStats::new(&[("Person", 10_000)]).with_index("Person", "age");
        let plan = optimize_query(
            "MATCH (a:Person) WHERE a.age = null RETURN count(a) AS n",
            &stats,
        );
        assert!(
            !contains_index_scan_on(&plan, "a"),
            "a null-literal equality must not become an index scan: {plan:?}"
        );
    }

    #[test]
    fn index_scan_rewrite_reaches_below_sort_distinct_and_limit() {
        let stats = TestStats::new(&[("Person", 10_000)]).with_index("Person", "city");
        let plan = optimize_query(
            "MATCH (a:Person) WHERE a.city = 'london' \
             RETURN DISTINCT a.age AS age ORDER BY age LIMIT 5",
            &stats,
        );
        assert!(
            contains_index_scan_on(&plan, "a"),
            "Limit, Sort, and Distinct must not block the index-scan rewrite: {plan:?}"
        );
    }

    #[test]
    fn select_scan_reverses_multi_hop_chain() {
        // (a:Person)-->(b)-->(c:City): reverse the whole two-hop chain to start at c.
        let stats = TestStats::new(&[("Person", 1000), ("City", 5)]);
        let plan = optimize_query(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:LIVES_IN]->(c:City) RETURN a, c",
            &stats,
        );

        let (var, label) = bottom_scan(&plan).expect("a label scan");
        assert_eq!(var, "c");
        assert_eq!(label.as_deref(), Some("City"));
        assert!(has_haslabel(&plan, "a", "Person"));
    }

    #[test]
    fn reduce_count_replaces_scan_with_constant() {
        let stats = TestStats::new(&[("Person", 42)]);
        let plan = optimize_query("MATCH (n:Person) RETURN count(*)", &stats);

        // Expect Project[42 AS count(*)] over SingleRow, with no scan at all.
        assert!(
            bottom_scan(&plan).is_none(),
            "count over a label scan must not scan: {plan:?}"
        );
        fn finds_literal_42(op: &PhysicalOperator) -> bool {
            match op {
                PhysicalOperator::Project { input, items, .. } => {
                    items
                        .iter()
                        .any(|(e, _)| matches!(e, Expr::Literal(Literal::Int(42))))
                        || finds_literal_42(input)
                }
                _ => false,
            }
        }
        assert!(
            finds_literal_42(&plan),
            "count constant 42 must be projected"
        );
    }

    #[test]
    fn reduce_count_skips_grouped_and_property_counts() {
        let stats = TestStats::new(&[("Person", 42)]);
        // count(n.age) counts non-null properties, not rows: must not be reduced.
        let plan = optimize_query("MATCH (n:Person) RETURN count(n.age)", &stats);
        assert!(
            bottom_scan(&plan).is_some(),
            "property count must still scan"
        );
    }

    fn finds_id_seek(op: &PhysicalOperator, var: &str) -> bool {
        match op {
            PhysicalOperator::NodeByIdSeek { variable, .. } => variable == var,
            PhysicalOperator::Expand { input, .. }
            | PhysicalOperator::Filter { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => finds_id_seek(input, var),
            PhysicalOperator::HashJoin { left, right } => {
                finds_id_seek(left, var) || finds_id_seek(right, var)
            }
            _ => false,
        }
    }

    #[test]
    fn id_equality_becomes_node_seek() {
        let stats = TestStats::new(&[("Person", 1000)]);
        let plan = optimize_query("MATCH (n:Person) WHERE id(n) = 42 RETURN n", &stats);
        assert!(finds_id_seek(&plan, "n"), "expected NodeByIdSeek: {plan:?}");
        assert!(
            bottom_scan(&plan).is_none(),
            "id seek must not fall back to a label scan"
        );
    }

    #[test]
    fn id_equality_flipped_operands_becomes_node_seek() {
        let stats = TestStats::new(&[("Person", 1000)]);
        let plan = optimize_query("MATCH (n:Person) WHERE 42 = id(n) RETURN n", &stats);
        assert!(finds_id_seek(&plan, "n"), "expected NodeByIdSeek: {plan:?}");
    }

    #[test]
    fn id_seek_works_on_unlabeled_scan() {
        let stats = TestStats::new(&[]);
        let plan = optimize_query("MATCH (n) WHERE id(n) = 7 RETURN n", &stats);
        assert!(finds_id_seek(&plan, "n"), "expected NodeByIdSeek: {plan:?}");
    }

    #[test]
    fn non_id_equality_is_not_a_seek() {
        let stats = TestStats::new(&[("Person", 1000)]);
        let plan = optimize_query("MATCH (n:Person) WHERE n.age = 42 RETURN n", &stats);
        assert!(
            !finds_id_seek(&plan, "n"),
            "property eq must not seek: {plan:?}"
        );
    }

    /// Count the `Filter` operators remaining in a plan.
    fn count_filters(op: &PhysicalOperator) -> usize {
        match op {
            PhysicalOperator::Filter { input, .. } => 1 + count_filters(input),
            PhysicalOperator::Expand { input, .. }
            | PhysicalOperator::Project { input, .. }
            | PhysicalOperator::Aggregate { input, .. }
            | PhysicalOperator::Sort { input, .. }
            | PhysicalOperator::Limit { input, .. }
            | PhysicalOperator::Distinct { input, .. }
            | PhysicalOperator::Unwind { input, .. }
            | PhysicalOperator::OptionalMatch { input, .. }
            | PhysicalOperator::MultiwayJoin { input, .. } => count_filters(input),
            PhysicalOperator::HashJoin { left, right } => {
                count_filters(left) + count_filters(right)
            }
            _ => 0,
        }
    }

    #[test]
    fn eliminate_true_filter_drops_literal_true() {
        let stats = TestStats::new(&[("Person", 5)]);
        let plan = optimize_query("MATCH (n:Person) WHERE true RETURN n", &stats);
        assert_eq!(
            count_filters(&plan),
            0,
            "WHERE true must be dropped: {plan:?}"
        );
        // The scan and projection survive.
        assert!(bottom_scan(&plan).is_some());
    }

    #[test]
    fn eliminate_true_filter_keeps_real_predicate() {
        let stats = TestStats::new(&[("Person", 5)]);
        let plan = optimize_query("MATCH (n:Person) WHERE n.age > 18 RETURN n", &stats);
        assert_eq!(count_filters(&plan), 1, "a real predicate must be kept");
    }

    #[test]
    fn eliminate_true_filter_does_not_drop_false() {
        let stats = TestStats::new(&[("Person", 5)]);
        // `1 = 2` is statically false and must be preserved (dropping it would
        // wrongly turn an empty result into all rows).
        let plan = optimize_query("MATCH (n:Person) WHERE 1 = 2 RETURN n", &stats);
        assert_eq!(count_filters(&plan), 1, "a false predicate must be kept");
    }

    /// True when the plan projects the given integer literal somewhere on its
    /// Project spine, the shape `reduce_count` leaves behind.
    fn finds_int_literal(op: &PhysicalOperator, n: i64) -> bool {
        match op {
            PhysicalOperator::Project { input, items, .. } => {
                items
                    .iter()
                    .any(|(e, _)| matches!(e, Expr::Literal(Literal::Int(v)) if *v == n))
                    || finds_int_literal(input, n)
            }
            _ => false,
        }
    }

    #[test]
    fn reduce_count_replaces_typed_edge_expand_with_constant() {
        let stats = TestStats::new(&[]).with_type("KNOWS", 7);
        for q in [
            "MATCH ()-[r:KNOWS]->() RETURN count(r)",
            "MATCH ()-[:KNOWS]->() RETURN count(*)",
            "MATCH ()<-[r:KNOWS]-() RETURN count(r)",
        ] {
            let plan = optimize_query(q, &stats);
            assert!(
                bottom_scan(&plan).is_none(),
                "edge count must not scan for {q}: {plan:?}"
            );
            assert!(
                finds_int_literal(&plan, 7),
                "edge count constant 7 must be projected for {q}: {plan:?}"
            );
        }
    }

    #[test]
    fn reduce_count_skips_non_reducible_edge_counts() {
        let stats = TestStats::new(&[("Person", 42)]).with_type("KNOWS", 7);
        for q in [
            // Undirected: each edge matches in both directions, so the type
            // metadata would undercount by half.
            "MATCH ()-[r:KNOWS]-() RETURN count(r)",
            // A labeled endpoint constrains the rows; type metadata ignores labels.
            "MATCH (a:Person)-[r:KNOWS]->() RETURN count(r)",
            // An untyped relationship has no metadata count.
            "MATCH ()-[r]->() RETURN count(r)",
            // Variable-length: row count is path count, not edge count.
            "MATCH ()-[:KNOWS*1..2]->() RETURN count(*)",
            // A residual predicate must be evaluated per row.
            "MATCH (a)-[r:KNOWS]->() WHERE a.age > 18 RETURN count(r)",
        ] {
            let plan = optimize_query(q, &stats);
            assert!(
                bottom_scan(&plan).is_some(),
                "edge count must still scan for {q}: {plan:?}"
            );
        }
    }

    #[test]
    fn reduce_count_skips_zero_count() {
        // Unknown/empty label yields no metadata count, so the scan is preserved
        // and empty-result semantics are unchanged.
        let stats = TestStats::new(&[("Person", 0)]);
        let plan = optimize_query("MATCH (n:Person) RETURN count(*)", &stats);
        assert!(
            bottom_scan(&plan).is_some(),
            "zero count must fall through to normal execution"
        );
    }
}
