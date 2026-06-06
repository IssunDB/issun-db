//! Positional row slots for the streaming executor.
//!
//! `SlotSchema` maps every variable a physical plan can bind to a dense slot
//! index, built once per query at stream-lowering time. `Row` is the runtime
//! counterpart: a fixed-width slot vector indexed by those positions, replacing
//! the per-row `PathMap` (a `HashMap` keyed by `String`) on the read pipeline.

use std::collections::HashMap;

use crate::plan::PhysicalOperator;

use super::{GraphBinding, PathMap};

/// Per-query mapping from variable names to dense row-slot indices.
///
/// Built by a single walk over the physical plan, so every name the executor
/// can bind at runtime (including planner-derived names such as `_target_N`
/// and the `_path_*` objects materialized when `needs_path` is set) has a slot
/// before execution starts. Slot order follows first-bind order in execution
/// order (inputs before the operators that consume them).
#[derive(Debug, Clone)]
pub(crate) struct SlotSchema {
    /// Slot index to variable name, for result columns and error messages.
    names: Vec<String>,
    /// Variable name to slot index, for plan-time expression resolution.
    index: HashMap<String, usize>,
}

impl SlotSchema {
    /// Collect every variable `op` and its inputs can bind into a dense schema.
    pub(crate) fn from_plan(op: &PhysicalOperator) -> Self {
        let mut schema = SlotSchema {
            names: Vec::new(),
            index: HashMap::new(),
        };
        schema.collect(op);
        schema
    }

    /// A schema with no slots: every binding overflows into the row's locals.
    /// Used by contexts with no plan to walk (pattern comprehension expansion).
    pub(crate) fn empty() -> Self {
        SlotSchema {
            names: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Register `name`, assigning the next dense slot; idempotent on rebinds
    /// (a variable bound by several operators keeps its first slot).
    fn bind(&mut self, name: &str) {
        if !self.index.contains_key(name) {
            self.index.insert(name.to_string(), self.names.len());
            self.names.push(name.to_string());
        }
    }

    /// Register every variable in a CREATE or MERGE pattern: node variables,
    /// relationship variables, and the rewritten `_path_*` name when the
    /// pattern binds a path variable.
    fn bind_pattern(&mut self, pattern: &crate::ast::Pattern) {
        if let Some(v) = &pattern.node.variable {
            self.bind(v);
        }
        for (rel, node) in &pattern.rels {
            if let Some(v) = &rel.variable {
                self.bind(v);
            }
            if let Some(v) = &node.variable {
                self.bind(v);
            }
        }
        if pattern.path_variable.is_some() {
            if let Some((_, last)) = pattern.rels.last() {
                if let Some(v) = &last.variable {
                    self.bind(&format!("_path_{v}"));
                }
            }
        }
    }

    /// Walk `op` bottom-up (inputs first), registering each variable the
    /// operator can bind at runtime.
    fn collect(&mut self, op: &PhysicalOperator) {
        use PhysicalOperator::*;
        match op {
            SingleRow => {}
            Unwind {
                input, variable, ..
            } => {
                self.collect(input);
                self.bind(variable);
            }
            LabelScan { variable, .. }
            | NodeByIdSeek { variable, .. }
            | NodeIndexScan { variable, .. }
            | NodeRangeScan { variable, .. } => self.bind(variable),
            Expand {
                input,
                src_var,
                rel_var,
                dst_var,
                needs_path,
                ..
            } => {
                self.collect(input);
                // src_var is normally bound by the input, but binding here is
                // idempotent and covers plans where the source arrives from a
                // join side rather than this operator's own input chain.
                self.bind(src_var);
                self.bind(rel_var);
                self.bind(dst_var);
                if *needs_path {
                    self.bind(&format!("_path_{src_var}"));
                    self.bind(&format!("_path_{dst_var}"));
                }
            }
            Filter { input, .. } => self.collect(input),
            Project { input, items, .. } => {
                self.collect(input);
                for (expr, alias) in items {
                    match alias {
                        Some(alias) => self.bind(alias),
                        // A bare variable in WITH (`WITH a`) keeps its name.
                        None => {
                            if let crate::ast::Expr::Prop(var, prop) = expr {
                                if prop.is_empty() {
                                    self.bind(var);
                                }
                            }
                        }
                    }
                }
            }
            HashJoin { left, right } => {
                self.collect(left);
                self.collect(right);
            }
            Aggregate {
                input,
                group_by,
                aggregations,
            } => {
                self.collect(input);
                for (expr, alias) in group_by {
                    match alias {
                        Some(alias) => self.bind(alias),
                        None => {
                            if let crate::ast::Expr::Prop(var, prop) = expr {
                                if prop.is_empty() {
                                    self.bind(var);
                                }
                            }
                        }
                    }
                }
                for (_, _, output) in aggregations {
                    self.bind(output);
                }
            }
            Sort { input, .. } | Limit { input, .. } | Distinct { input } => self.collect(input),
            OptionalMatch { input, null_vars } => {
                self.collect(input);
                for v in null_vars {
                    self.bind(v);
                }
            }
            WritePart { input, part } => {
                self.collect(input);
                match part {
                    crate::ast::QueryPart::Create { patterns } => {
                        for p in patterns {
                            self.bind_pattern(p);
                        }
                    }
                    crate::ast::QueryPart::Merge { merges } => {
                        for m in merges {
                            self.bind_pattern(&m.pattern);
                        }
                    }
                    // SET, REMOVE, DELETE, and FOREACH bind no new variables
                    // visible outside the part.
                    _ => {}
                }
            }
            ProcedureCall {
                input, output_vars, ..
            } => {
                self.collect(input);
                for v in output_vars {
                    self.bind(v);
                }
            }
            MultiwayJoin {
                input,
                closing_rel_var,
                ..
            } => {
                self.collect(input);
                self.bind(closing_rel_var);
            }
            TriangleCount { output, .. } => {
                self.bind(output);
            }
        }
    }

    /// Resolve a variable name to its slot index, if the plan can bind it.
    pub(crate) fn slot(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    /// The variable name bound at `slot`.
    #[allow(dead_code)]
    pub(crate) fn name(&self, slot: usize) -> &str {
        &self.names[slot]
    }

    /// The number of slots a `Row` for this schema must hold.
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

/// A row of positional bindings; `None` is an unbound slot.
///
/// Cloning a `Row` is one allocation plus a per-slot clone, with no hashing
/// and no per-key `String` allocations, which is the point of replacing
/// `PathMap` on the hot pipeline.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    slots: Box<[Option<GraphBinding>]>,
}

impl Row {
    /// An all-unbound row with `len` slots.
    pub(crate) fn new(len: usize) -> Self {
        Row {
            slots: vec![None; len].into_boxed_slice(),
        }
    }

    /// The binding at `slot`, or `None` when unbound.
    pub(crate) fn get(&self, slot: usize) -> Option<&GraphBinding> {
        self.slots[slot].as_ref()
    }

    /// Bind `slot` to `value`, replacing any previous binding.
    pub(crate) fn set(&mut self, slot: usize, value: GraphBinding) {
        self.slots[slot] = Some(value);
    }

    /// Clear the binding at `slot` (OPTIONAL MATCH null-fill, REMOVE paths).
    #[allow(dead_code)]
    pub(crate) fn unset(&mut self, slot: usize) {
        self.slots[slot] = None;
    }
}

/// Read and shadow access to per-row variable bindings during expression
/// evaluation.
///
/// `evaluate_expr` and its helpers are generic over this trait so the same
/// code serves both row representations during the slot migration: `PathMap`
/// (the legacy `HashMap` row) and `RowEnv` (a positional `Row` plus its
/// `SlotSchema`). `bind_local` is for expression-local variables (quantifiers,
/// list comprehensions, `reduce`), which shadow row bindings of the same name
/// for the lifetime of the cloned environment.
pub(crate) trait Bindings: Clone {
    /// The binding for `name`, innermost local first.
    fn get_binding(&self, name: &str) -> Option<&GraphBinding>;
    /// Shadow `name` with `value` in this environment only. Takes `&str` so a
    /// positional row pays no per-bind allocation on the slot hit path.
    fn bind_local(&mut self, name: &str, value: GraphBinding);
    /// Materialize a `PathMap` for cold paths still keyed by name
    /// (pattern comprehension expansion, write-part shims).
    fn to_path_map(&self) -> PathMap;
}

impl Bindings for PathMap {
    fn get_binding(&self, name: &str) -> Option<&GraphBinding> {
        self.get(name)
    }

    fn bind_local(&mut self, name: &str, value: GraphBinding) {
        self.insert(name.to_string(), value);
    }

    fn to_path_map(&self) -> PathMap {
        self.clone()
    }
}

/// An owned positional row plus its schema: the row representation of the
/// read pipeline.
///
/// Variables with a slot in the schema bind positionally; names the plan walk
/// could not see (projection canonical keys, comprehension-internal names)
/// overflow into `locals`, a small association vector. Correctness therefore
/// never depends on schema completeness, only performance does. Cloning is a
/// slot memcpy plus an `Arc` bump, with no rehash of every binding, which is
/// the point of replacing `PathMap` per row.
#[derive(Clone)]
pub(crate) struct SlotRow {
    row: Row,
    schema: std::sync::Arc<SlotSchema>,
    /// Bindings without a slot, plus expression-local shadows; innermost last.
    locals: Vec<(String, GraphBinding)>,
}

impl SlotRow {
    /// An all-unbound row for `schema`.
    pub(crate) fn empty(schema: std::sync::Arc<SlotSchema>) -> Self {
        SlotRow {
            row: Row::new(schema.len()),
            schema,
            locals: Vec::new(),
        }
    }

    /// Convert a name-keyed row at a `PathMap` boundary (write parts, the
    /// materialized fallback, pattern comprehension).
    pub(crate) fn from_path_map(schema: std::sync::Arc<SlotSchema>, map: &PathMap) -> Self {
        let mut out = Self::empty(schema);
        for (name, binding) in map {
            out.bind_local(name, binding.clone());
        }
        out
    }

    /// A clone of the schema handle, for creating sibling rows.
    pub(crate) fn schema_arc(&self) -> std::sync::Arc<SlotSchema> {
        self.schema.clone()
    }

    /// Every bound `(name, binding)` pair, slots in schema order then locals in
    /// insertion order. Deterministic, unlike `HashMap` iteration, so dedup
    /// keys and RETURN * column sets derived from it are stable.
    pub(crate) fn bound_entries(&self) -> impl Iterator<Item = (&str, &GraphBinding)> {
        let slots = self
            .schema
            .names
            .iter()
            .enumerate()
            .filter_map(|(slot, name)| self.row.get(slot).map(|b| (name.as_str(), b)));
        slots.chain(self.locals.iter().map(|(n, b)| (n.as_str(), b)))
    }

    /// Copy every bound entry of `other` into `self`, overwriting on conflict
    /// (the hash-join row merge). Rows from one query share one schema, so the
    /// common case is a direct slot-to-slot copy.
    pub(crate) fn merge_from(&mut self, other: &SlotRow) {
        if std::sync::Arc::ptr_eq(&self.schema, &other.schema) {
            for (slot, binding) in other.row.slots.iter().enumerate() {
                if let Some(b) = binding {
                    self.row.set(slot, b.clone());
                }
            }
            for (name, binding) in &other.locals {
                self.bind_local(name, binding.clone());
            }
        } else {
            for (name, binding) in other.bound_entries() {
                self.bind_local(name, binding.clone());
            }
        }
    }
}

impl Bindings for SlotRow {
    fn get_binding(&self, name: &str) -> Option<&GraphBinding> {
        // Innermost local first, so expression-local variables shadow slot
        // bindings exactly as the PathMap clone-and-insert used to.
        self.locals
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b)
            .or_else(|| self.schema.slot(name).and_then(|s| self.row.get(s)))
    }

    fn bind_local(&mut self, name: &str, value: GraphBinding) {
        match self.schema.slot(name) {
            Some(slot) => self.row.set(slot, value),
            None => {
                // Overwrite an existing overflow binding rather than letting
                // the vector grow per rebind.
                if let Some(entry) = self.locals.iter_mut().find(|(n, _)| n == name) {
                    entry.1 = value;
                } else {
                    self.locals.push((name.to_string(), value));
                }
            }
        }
    }

    fn to_path_map(&self) -> PathMap {
        let mut map = PathMap::new();
        for (slot, name) in self.schema.names.iter().enumerate() {
            if let Some(binding) = self.row.get(slot) {
                map.insert(name.clone(), binding.clone());
            }
        }
        // Locals last, in insertion order, so the innermost shadow wins.
        for (name, binding) in &self.locals {
            map.insert(name.clone(), binding.clone());
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};

    fn physical_plan_for(cypher: &str) -> PhysicalOperator {
        let stmt = parser::parse(cypher).unwrap();
        let query = match stmt {
            crate::ast::Statement::Query(q) => q,
            _ => panic!("expected Query"),
        };
        let logical = LogicalPlanner::plan(&query).unwrap();
        let physical = PhysicalPlanner::plan(&logical);
        Optimizer::optimize(physical, None)
    }

    #[test]
    fn slot_schema_assigns_dense_slots_in_bind_order() {
        let plan = physical_plan_for(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name",
        );
        let schema = SlotSchema::from_plan(&plan);

        let a = schema.slot("a").expect("scan variable a has a slot");
        let r = schema
            .slot("r")
            .expect("relationship variable r has a slot");
        let b = schema.slot("b").expect("expand target b has a slot");

        // First-bind order in execution order: the scan binds a, then the
        // expand binds r and b.
        assert_eq!((a, r, b), (0, 1, 2));
        assert!(schema.slot("missing").is_none());
        assert_eq!(schema.name(a), "a");
        assert!(schema.len() >= 3);
    }

    #[test]
    fn slot_schema_registers_derived_path_variables() {
        let plan = physical_plan_for("MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p");
        let schema = SlotSchema::from_plan(&plan);

        // RETURN p is rewritten at plan time to read `_path_b`, the path
        // object the needs_path expand materializes for its target.
        assert!(
            schema.slot("_path_b").is_some(),
            "needs_path expand must register its _path_* binding"
        );
    }

    #[test]
    fn slot_schema_registers_aggregate_and_with_outputs() {
        let plan =
            physical_plan_for("MATCH (a:Person) WITH a.city AS city, count(a) AS n RETURN city, n");
        let schema = SlotSchema::from_plan(&plan);

        assert!(schema.slot("city").is_some(), "group-by alias has a slot");
        assert!(schema.slot("n").is_some(), "aggregate output has a slot");
    }

    #[test]
    fn row_env_evaluates_property_access_and_preserves_unbound_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = issundb_core::Graph::open(dir.path(), 1).unwrap();
        let id = graph
            .add_node("Person", &serde_json::json!({"age": 30}))
            .unwrap();

        let plan = physical_plan_for("MATCH (a:Person) RETURN a.age AS age");
        let schema = std::sync::Arc::new(SlotSchema::from_plan(&plan));
        assert!(schema.slot("a").is_some(), "a binds positionally");
        let mut env = SlotRow::empty(schema);
        env.bind_local("a", GraphBinding::Node(id));
        let params = HashMap::new();

        let val = crate::exec::expr::evaluate_expr(
            &graph,
            &env,
            &crate::ast::Expr::Prop("a".into(), "age".into()),
            &params,
        )
        .unwrap();
        assert_eq!(val, serde_json::json!(30));

        // The unbound-variable error message is part of the TCK-observable
        // surface and must survive the row representation change.
        let err = crate::exec::expr::evaluate_expr(
            &graph,
            &env,
            &crate::ast::Expr::Prop("zzz".into(), "age".into()),
            &params,
        )
        .unwrap_err();
        assert_eq!(err, "unbound variable: zzz");
    }

    #[test]
    fn slot_row_shadows_in_clones_and_overflows_unslotted_names() {
        let plan = physical_plan_for("MATCH (a:Person) RETURN a");
        let schema = std::sync::Arc::new(SlotSchema::from_plan(&plan));
        let mut outer = SlotRow::empty(schema);
        outer.bind_local("a", GraphBinding::Node(1));
        assert_eq!(outer.get_binding("a"), Some(&GraphBinding::Node(1)));

        // A quantifier or comprehension variable named `a` shadows the row
        // binding in its cloned environment without touching the outer one.
        let mut inner = outer.clone();
        inner.bind_local("a", GraphBinding::Scalar(serde_json::json!(99)));
        assert_eq!(
            inner.get_binding("a"),
            Some(&GraphBinding::Scalar(serde_json::json!(99)))
        );
        assert_eq!(outer.get_binding("a"), Some(&GraphBinding::Node(1)));

        // A name with no slot in the schema lands in the locals overflow and
        // still binds, reads back, and survives the PathMap bridge.
        inner.bind_local("q", GraphBinding::Scalar(serde_json::json!("x")));
        assert_eq!(
            inner.get_binding("q"),
            Some(&GraphBinding::Scalar(serde_json::json!("x")))
        );

        let bridged = inner.to_path_map();
        assert_eq!(
            bridged.get("a"),
            Some(&GraphBinding::Scalar(serde_json::json!(99)))
        );
        assert_eq!(
            bridged.get("q"),
            Some(&GraphBinding::Scalar(serde_json::json!("x")))
        );

        // The round trip back from a PathMap restores both binding kinds.
        let restored = SlotRow::from_path_map(inner.schema.clone(), &bridged);
        assert_eq!(
            restored.get_binding("a"),
            Some(&GraphBinding::Scalar(serde_json::json!(99)))
        );
        assert_eq!(
            restored.get_binding("q"),
            Some(&GraphBinding::Scalar(serde_json::json!("x")))
        );
    }

    #[test]
    fn row_slots_default_unbound_and_round_trip() {
        let mut row = Row::new(3);
        assert!(row.get(0).is_none());
        assert!(row.get(2).is_none());

        row.set(1, GraphBinding::Node(7));
        assert_eq!(row.get(1), Some(&GraphBinding::Node(7)));

        let cloned = row.clone();
        assert_eq!(cloned.get(1), row.get(1));

        row.unset(1);
        assert!(row.get(1).is_none());
        // The clone is independent of the original.
        assert_eq!(cloned.get(1), Some(&GraphBinding::Node(7)));
    }
}
