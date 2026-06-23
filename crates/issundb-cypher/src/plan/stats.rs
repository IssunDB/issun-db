use issundb_core::Graph;

/// A trait providing cardinality statistics for labels and relationship types
/// to help with query optimization.
pub trait StatsProvider {
    /// Get the count of nodes matching a string label.
    fn node_count_by_label(&self, label: &str) -> Option<u64>;

    /// Get the count of edges matching a string type.
    fn edge_count_by_type(&self, etype: &str) -> Option<u64>;

    /// Upper-bound estimate of the total node count, used to derive average
    /// relationship fan-out (`edges_of_type / nodes`). Returns `None` when no
    /// estimate is available, in which case the planner falls back to a constant
    /// fan-out.
    fn total_node_count(&self) -> Option<u64> {
        None
    }

    /// Estimated average fan-out for expanding edges of `rel_type` from a node
    /// carrying `src_label`, traversing outgoing edges (or incoming when
    /// `incoming` is true). This is the per-source-label typed degree, which is
    /// more precise than the global `edges_of_type / total_nodes` ratio on a
    /// skewed schema where different labels have different out-degrees. Returns
    /// `None` when no per-label estimate is available, so the planner falls back
    /// to the global average fan-out.
    fn expand_fanout(&self, _src_label: &str, _rel_type: &str, _incoming: bool) -> Option<f64> {
        None
    }

    /// Destination-label-aware refinement of [`StatsProvider::expand_fanout`]:
    /// the average number of `dst_label` neighbors reached when expanding
    /// `rel_type` from a `src_label` node. `None` when no per-triple estimate is
    /// available.
    fn expand_fanout_to(
        &self,
        _src_label: &str,
        _rel_type: &str,
        _dst_label: &str,
        _incoming: bool,
    ) -> Option<f64> {
        None
    }

    /// Whether the data schema contains any directed edge `src_label
    /// --rel_type--> dst_label`. `Some(false)` means the directed pattern is
    /// provably unsatisfiable on committed data; `None` means undecidable
    /// (an unknown label or type, or no statistics), so the caller must not
    /// prune.
    fn schema_has_edge(&self, _src_label: &str, _rel_type: &str, _dst_label: &str) -> Option<bool> {
        None
    }

    /// Check if a node property index exists.
    fn has_node_property_index(&self, _label: &str, _property: &str) -> bool {
        false
    }

    /// Estimate the selectivity of a filter expression.
    fn estimate_filter_selectivity(&self, _expr: &crate::plan::logical::FilterExpr) -> Option<f64> {
        None
    }
}

impl StatsProvider for Graph {
    fn node_count_by_label(&self, label: &str) -> Option<u64> {
        self.node_count_by_label(label).ok()
    }

    fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
        self.edge_count_by_type(etype).ok()
    }

    fn total_node_count(&self) -> Option<u64> {
        self.node_count_hint().ok()
    }

    fn expand_fanout(&self, src_label: &str, rel_type: &str, incoming: bool) -> Option<f64> {
        self.estimate_expand_fanout(src_label, rel_type, incoming)
            .ok()
            .flatten()
    }

    fn expand_fanout_to(
        &self,
        src_label: &str,
        rel_type: &str,
        dst_label: &str,
        incoming: bool,
    ) -> Option<f64> {
        self.estimate_expand_fanout_to(src_label, rel_type, dst_label, incoming)
            .ok()
            .flatten()
    }

    fn schema_has_edge(&self, src_label: &str, rel_type: &str, dst_label: &str) -> Option<bool> {
        self.schema_has_edge(src_label, rel_type, dst_label)
            .ok()
            .flatten()
    }

    fn has_node_property_index(&self, label: &str, property: &str) -> bool {
        self.has_node_property_index(label, property)
            .unwrap_or(false)
    }

    fn estimate_filter_selectivity(&self, expr: &crate::plan::logical::FilterExpr) -> Option<f64> {
        use crate::ast::{Expr, Literal};
        use crate::plan::logical::FilterExpr;

        fn literal_to_value(lit: &Literal) -> serde_json::Value {
            match lit {
                Literal::Str(s) => serde_json::Value::String(s.clone()),
                Literal::Int(i) => serde_json::Value::Number((*i).into()),
                Literal::Float(f) => serde_json::Number::from_f64(*f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                Literal::Bool(b) => serde_json::Value::Bool(*b),
                Literal::Null => serde_json::Value::Null,
                Literal::List(l) => {
                    serde_json::Value::Array(l.iter().map(literal_to_value).collect())
                }
            }
        }

        fn extract_prop_and_literal(
            l: &Expr,
            r: &Expr,
        ) -> Option<(String, serde_json::Value, bool)> {
            match (l, r) {
                (Expr::Prop(_, prop), Expr::Literal(lit)) => {
                    Some((prop.clone(), literal_to_value(lit), true))
                }
                (Expr::Literal(lit), Expr::Prop(_, prop)) => {
                    Some((prop.clone(), literal_to_value(lit), false))
                }
                _ => None,
            }
        }

        match expr {
            FilterExpr::Eq(l, r) => {
                let (prop, val, _) = extract_prop_and_literal(l, r)?;
                self.estimate_equality_selectivity(&prop, &val).ok()?
            }
            FilterExpr::Ne(l, r) => {
                let (prop, val, _) = extract_prop_and_literal(l, r)?;
                let eq_sel = self.estimate_equality_selectivity(&prop, &val).ok()??;
                Some((1.0 - eq_sel).max(0.0))
            }
            // The histogram estimate does not model bound inclusivity, so Lt
            // pairs with Le and Gt pairs with Ge.
            FilterExpr::Lt(l, r) | FilterExpr::Le(l, r) => {
                let (prop, val, is_col_lhs) = extract_prop_and_literal(l, r)?;
                if is_col_lhs {
                    self.estimate_range_selectivity(&prop, None, Some(&val))
                        .ok()?
                } else {
                    self.estimate_range_selectivity(&prop, Some(&val), None)
                        .ok()?
                }
            }
            FilterExpr::Gt(l, r) | FilterExpr::Ge(l, r) => {
                let (prop, val, is_col_lhs) = extract_prop_and_literal(l, r)?;
                if is_col_lhs {
                    self.estimate_range_selectivity(&prop, Some(&val), None)
                        .ok()?
                } else {
                    self.estimate_range_selectivity(&prop, None, Some(&val))
                        .ok()?
                }
            }
            _ => None,
        }
    }
}
