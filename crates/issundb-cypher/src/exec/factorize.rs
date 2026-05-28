use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::Expr;
use crate::plan::FilterExpr;

use super::GraphBinding;

type PathMap = HashMap<String, GraphBinding>;

/// A factorized batch produced by the single-hop Expand executor.
///
/// Stores a shared prefix (variable bindings from all ancestor hops) plus a
/// list of per-row extensions for the current hop. Sharing the prefix via `Arc`
/// avoids an O(shared_vars) HashMap clone for every destination: only the two
/// new bindings introduced by the hop are paid per output row.
///
/// Callers can apply predicates that reference only shared variables once per
/// group (rather than once per row) and call `flatten` to materialize when a
/// downstream operator requires individual PathMaps.
pub(super) struct FactorizedRecordGroup {
    /// Bindings from all ancestor hops, shared across every row in this group.
    pub shared: Arc<PathMap>,
    /// Per-row extensions: `(rel_var_name, rel_binding, dst_var_name, dst_binding)`.
    pub extensions: Vec<(String, GraphBinding, String, GraphBinding)>,
}

impl FactorizedRecordGroup {
    /// Materialize the group into individual PathMaps.
    pub fn flatten(self) -> impl Iterator<Item = PathMap> {
        let shared = self.shared;
        self.extensions.into_iter().map(move |(rk, rv, dk, dv)| {
            let mut path = (*shared).clone();
            path.insert(rk, rv);
            path.insert(dk, dv);
            path
        })
    }
}

/// Returns the set of top-level variable names referenced by a filter expression.
///
/// Used to decide whether a predicate touches only the shared prefix of a
/// `FactorizedRecordGroup` (and can therefore be evaluated once per group) or
/// also references the expansion variables (and must be evaluated per row).
pub(super) fn filter_refs_in_expr(expr: &FilterExpr) -> HashSet<String> {
    let mut vars = HashSet::new();
    match expr {
        FilterExpr::Eq(l, r)
        | FilterExpr::Ne(l, r)
        | FilterExpr::Lt(l, r)
        | FilterExpr::Gt(l, r)
        | FilterExpr::Le(l, r)
        | FilterExpr::Ge(l, r) => {
            collect_expr_vars(l, &mut vars);
            collect_expr_vars(r, &mut vars);
        }
        FilterExpr::HasLabel(var, _) => {
            vars.insert(var.clone());
        }
        FilterExpr::Expr(e) => {
            collect_expr_vars(e, &mut vars);
        }
    }
    vars
}

fn collect_expr_vars(expr: &Expr, vars: &mut HashSet<String>) {
    match expr {
        Expr::Prop(var, _) => {
            vars.insert(var.clone());
        }
        Expr::Agg(_, inner) => collect_expr_vars(inner, vars),
        _ => {}
    }
}
