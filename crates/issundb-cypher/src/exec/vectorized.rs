//! Columnar fast path for the final projection or aggregation over a single-hop expansion.
//!
//! The per-row pipeline pays one `SlotRow` clone per expansion row, another
//! per projection row, and one property-columns lookup per property access.
//! For the plan shape this module recognizes, none of that is needed: the
//! whole result is computable from flat id columns (one bulk expansion, one
//! bulk label filter per predicate, one bulk property gather per variable),
//! so the executor here works column-at-a-time and builds the result records
//! directly.
//!
//! Recognized shape, from the root down:
//!
//! ```text
//! [Sort]? Project [Aggregate]? Filter(HasLabel)* Expand(1 hop, directed) LabelScan
//! ```
//!
//! with every projected or grouped expression a single-property read on the
//! scan or expansion variable. `Sort` is accepted only over an `Aggregate`
//! (where the output is small enough that the regular `sort_all` runs on it).
//! Anything else returns `None` and the row pipeline runs unchanged, so
//! correctness never depends on this recognizer; only performance does. Row
//! order, group identity, aggregate typing, and error surfaces match the row
//! pipeline exactly: the expansion is flattened per source in scan order, the
//! aggregation reuses `AggState`, and the operators above the aggregate are
//! the regular `project_rows` and `sort_all`.

use std::collections::HashMap;

use issundb_core::{EdgeId, Graph, NodeId};
use serde_json::Value;

use crate::ast::{AggFn, Expr, ReturnClause, SortItem};
use crate::plan::{FilterExpr, PhysicalOperator};

use super::read::{
    AggState, expand_multi_type, group_by_column_name, project_rows, projected_key,
    rows_to_records, sort_all, unpack_sentinels,
};
use super::row::{Bindings, SlotRow, SlotSchema};
use super::{GraphBinding, Record};

/// The recognized pipeline: one scan, one expansion, label predicates, and a
/// projection or aggregation root.
struct VecPipeline<'a> {
    scan_label: Option<&'a str>,
    src_var: &'a str,
    dst_var: &'a str,
    rel_type: Option<&'a str>,
    is_incoming: bool,
    /// `HasLabel` predicates above the expand, each on the source or target
    /// node variable.
    label_filters: Vec<(&'a str, &'a str)>,
    root: VecRoot<'a>,
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

/// A single-property read on the scan or expansion variable, the only
/// expression form the columnar executor evaluates itself.
fn prop_read<'a>(expr: &'a Expr, src_var: &str, dst_var: &str) -> Option<(bool, &'a str)> {
    if let Expr::Prop(var, prop) = expr {
        if !prop.is_empty() {
            if var == src_var {
                return Some((true, prop.as_str()));
            }
            if var == dst_var {
                return Some((false, prop.as_str()));
            }
        }
    }
    None
}

/// Match `plan` against the recognized shape, or `None` for the row pipeline.
fn recognize(plan: &PhysicalOperator) -> Option<VecPipeline<'_>> {
    let (sort_items, below_sort) = match plan {
        PhysicalOperator::Sort { input, items } => (Some(items.as_slice()), input.as_ref()),
        other => (None, other),
    };
    let PhysicalOperator::Project {
        input,
        items,
        is_barrier,
    } = below_sort
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
            // A sort over a plain projection would need the full row set; the
            // top-N and row paths handle those shapes.
            if sort_items.is_some() {
                return None;
            }
            (VecRoot::Project { items }, other)
        }
    };

    let mut label_filters = Vec::new();
    let mut cur = chain_top;
    while let PhysicalOperator::Filter { input, expression } = cur {
        let FilterExpr::HasLabel(var, label) = expression else {
            return None;
        };
        label_filters.push((var.as_str(), label.as_str()));
        cur = input.as_ref();
    }

    let PhysicalOperator::Expand {
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
    } = cur
    else {
        return None;
    };
    // A single hop has no sibling relationships; a populated list means the
    // optimizer wired this hop into a larger pattern.
    if !unique_rels.is_empty() {
        return None;
    }
    // A self-referencing hop `(a)-->(a)` needs the pre-bound target guard.
    if src_var == dst_var {
        return None;
    }
    // Additional source labels (`(a:Person:Vip)`) plan as `HasLabel` filters
    // between the expand and the scan; they restrict the same source rows.
    let mut below = input.as_ref();
    while let PhysicalOperator::Filter { input, expression } = below {
        let FilterExpr::HasLabel(var, label) = expression else {
            return None;
        };
        label_filters.push((var.as_str(), label.as_str()));
        below = input.as_ref();
    }
    let PhysicalOperator::LabelScan { variable, label } = below else {
        return None;
    };
    if variable != src_var {
        return None;
    }
    for (var, _) in &label_filters {
        if *var != src_var && *var != dst_var {
            return None;
        }
    }

    let pipeline = VecPipeline {
        scan_label: label.as_deref(),
        src_var,
        dst_var,
        rel_type: rel_type.as_deref(),
        is_incoming: *is_incoming,
        label_filters,
        root,
    };

    // Per-root expression eligibility.
    match &pipeline.root {
        VecRoot::Project { items } => {
            for (expr, _) in items.iter() {
                prop_read(expr, pipeline.src_var, pipeline.dst_var)?;
            }
        }
        VecRoot::Aggregate {
            group_by,
            aggregations,
            ..
        } => {
            for (expr, _) in group_by.iter() {
                prop_read(expr, pipeline.src_var, pipeline.dst_var)?;
            }
            for (agg_fn, inner, _) in aggregations.iter() {
                let count_like = match inner {
                    Expr::CountStar => matches!(agg_fn, AggFn::Count { .. }),
                    // A whole bound variable is never null here, so a
                    // non-distinct count over it is a row count. Any other
                    // aggregate over a whole variable needs the row value.
                    Expr::Prop(var, prop) if prop.is_empty() => {
                        matches!(agg_fn, AggFn::Count { distinct: false })
                            && (var == pipeline.src_var
                                || var == pipeline.dst_var
                                || var == rel_var)
                    }
                    _ => false,
                };
                if !count_like && prop_read(inner, pipeline.src_var, pipeline.dst_var).is_none() {
                    return None;
                }
            }
            // The projection and sort above the aggregate run through the
            // regular operators, so their expressions need no eligibility.
        }
    }

    Some(pipeline)
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

    // 1. Scan ids, ascending, exactly as the label scan emits them.
    let mut src_ids = match p.scan_label {
        Some(l) => graph.nodes_by_label(l).map_err(|e| e.to_string())?,
        None => graph.all_nodes().map_err(|e| e.to_string())?,
    };

    // 2. Source-side label predicates read only the source, so restricting
    //    the sources before expansion yields the same rows in the same order
    //    as the row pipeline's post-expansion filter.
    for (_, label) in p.label_filters.iter().filter(|(v, _)| *v == p.src_var) {
        src_ids = graph
            .label_filter(&src_ids, label)
            .map_err(|e| e.to_string())?;
    }

    // 3. Bulk expansion, flattened per source in scan order: the row order
    //    the per-row pipeline produces.
    let transitions = expand_multi_type(graph, &src_ids, p.rel_type, p.is_incoming)?;
    let mut per_src: ahash::AHashMap<NodeId, Vec<(EdgeId, NodeId)>> = ahash::AHashMap::new();
    for (s, e, d) in transitions {
        per_src.entry(s).or_default().push((e, d));
    }
    let row_estimate: usize = per_src.values().map(Vec::len).sum();
    let mut srcs: Vec<NodeId> = Vec::with_capacity(row_estimate);
    let mut dsts: Vec<NodeId> = Vec::with_capacity(row_estimate);
    for &s in &src_ids {
        if let Some(v) = per_src.get(&s) {
            for &(_, d) in v {
                srcs.push(s);
                dsts.push(d);
            }
        }
    }

    // 4. Target-side label predicates: one bulk membership set, then an
    //    order-preserving compaction of the id columns. A label smaller than
    //    the target column comes from one `label_idx` prefix scan; a larger
    //    one from point lookups on the distinct targets.
    for (_, label) in p.label_filters.iter().filter(|(v, _)| *v == p.dst_var) {
        let label_count = graph
            .node_count_by_label(label)
            .map_err(|e| e.to_string())? as usize;
        let pass: ahash::AHashSet<NodeId> = if label_count <= dsts.len() {
            graph
                .nodes_by_label(label)
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect()
        } else {
            let mut distinct = dsts.clone();
            distinct.sort_unstable();
            distinct.dedup();
            graph
                .label_filter(&distinct, label)
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect()
        };
        let mut w = 0;
        for i in 0..dsts.len() {
            if pass.contains(&dsts[i]) {
                srcs[w] = srcs[i];
                dsts[w] = dsts[i];
                w += 1;
            }
        }
        srcs.truncate(w);
        dsts.truncate(w);
    }
    let n = dsts.len();

    match p.root {
        VecRoot::Project { items } => {
            // One gather per variable covers every projected property; the
            // record cells are moved out of the tables, not re-cloned.
            let mut src_props: Vec<&str> = Vec::new();
            let mut dst_props: Vec<&str> = Vec::new();
            let mut item_cols = Vec::with_capacity(items.len());
            for (expr, _) in items.iter() {
                // Already validated by `recognize`; decline rather than panic.
                let Some((is_src, prop)) = prop_read(expr, p.src_var, p.dst_var) else {
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
                let Some((is_src, prop)) = prop_read(expr, p.src_var, p.dst_var) else {
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
                match prop_read(inner, p.src_var, p.dst_var) {
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
            let rows = match sort_items {
                Some(items) => sort_all(graph, rows, items, None, params),
                None => rows,
            };
            Ok(Some(rows_to_records(graph, &return_clause.items, rows)?))
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

    /// Run `cypher` (vectorized-eligible) and its row-path twin, asserting
    /// identical columns and records, in order. The twin disables the fast
    /// path with a residual filter no recognizer arm accepts.
    fn assert_matches_row_path(graph: &Graph, cypher: &str) {
        let plan = optimized_plan(graph, cypher);
        assert!(
            recognize(&plan).is_some(),
            "expected a vectorized-eligible plan for: {cypher}\n{plan:?}"
        );
        let forced = match cypher.find(" RETURN ") {
            Some(idx) => format!(
                "{} WHERE a.__force_row_path IS NULL {}",
                &cypher[..idx],
                &cypher[idx + 1..]
            ),
            None => panic!("query has no RETURN"),
        };
        assert!(
            recognize(&optimized_plan(graph, &forced)).is_none(),
            "the forced twin must not vectorize: {forced}"
        );
        let params = std::collections::HashMap::new();
        let fast = execute(graph, cypher, &params).unwrap();
        let slow = execute(graph, &forced, &params).unwrap();
        assert_eq!(fast.columns, slow.columns, "columns for: {cypher}");
        let fast_rows: Vec<_> = fast.records.iter().map(|r| &r.values).collect();
        let slow_rows: Vec<_> = slow.records.iter().map(|r| &r.values).collect();
        assert_eq!(fast_rows, slow_rows, "records for: {cypher}");
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
            // Non-label residual predicate.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age > 1 RETURN b.name AS name",
            // Whole-variable projection.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b",
            // Sort over a plain projection.
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name ORDER BY name",
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
            ] {
                assert_matches_row_path(&g, cypher);
            }
        }
    }
}
