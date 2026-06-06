//! In-memory property columns for the read path.
//!
//! `ColumnsCache` holds one typed column per node property name, indexed by a
//! self-contained dense node mapping (the same pattern as `MatrixSet`). It is
//! built lazily from one full scan of the `nodes` sub-database and kept fresh
//! by a post-commit delta: added and updated nodes are re-read individually,
//! node deletion forces a full rebuild because it reshuffles nothing here but
//! invalidates the dense mapping's completeness guarantee.
//!
//! The cache exists so per-row property access in query execution costs a
//! dense-index read instead of an LMDB point lookup plus a full msgpack
//! decode. Values reconstruct exactly what decoding the stored record yields;
//! properties whose values are not uniformly one scalar kind fall back to a
//! `Json` column so no conversion ever changes a value.

use ahash::AHashMap;
use serde_json::Value;

use crate::error::Error;
use crate::schema::{NodeId, NodeRecord};
use crate::storage::{lmdb::Storage, props};

/// One typed column over dense node indices.
pub(crate) enum PropColumn {
    Int(Vec<Option<i64>>),
    Float(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    /// Dictionary-encoded strings: `idx[dense]` points into `dict`;
    /// `u32::MAX` marks null or missing.
    Str {
        dict: Vec<String>,
        lookup: AHashMap<String, u32>,
        idx: Vec<u32>,
    },
    /// Exact-semantics fallback for mixed-kind, array, object, or
    /// out-of-range numeric values.
    Json(Vec<Option<Value>>),
}

const STR_NULL: u32 = u32::MAX;

/// The scalar kind of one JSON value, used to pick or degrade a column type.
#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Null,
    Int,
    Float,
    Bool,
    Str,
    Other,
}

fn kind_of(v: &Value) -> Kind {
    match v {
        Value::Null => Kind::Null,
        Value::Bool(_) => Kind::Bool,
        Value::Number(n) => {
            if n.is_i64() || (n.is_u64() && n.as_i64().is_some()) {
                Kind::Int
            } else if n.is_f64() {
                Kind::Float
            } else {
                Kind::Other
            }
        }
        Value::String(_) => Kind::Str,
        _ => Kind::Other,
    }
}

impl PropColumn {
    /// Build the tightest column for `values` (one slot per dense index).
    fn from_values(values: Vec<Option<Value>>) -> Self {
        let mut kind = Kind::Null;
        for v in values.iter().flatten() {
            let k = kind_of(v);
            if k == Kind::Null {
                continue;
            }
            if kind == Kind::Null {
                kind = k;
            } else if kind != k {
                kind = Kind::Other;
                break;
            }
        }
        match kind {
            Kind::Int => Self::Int(
                values
                    .into_iter()
                    .map(|v| v.and_then(|v| v.as_i64()))
                    .collect(),
            ),
            Kind::Float => Self::Float(
                values
                    .into_iter()
                    .map(|v| v.and_then(|v| v.as_f64()))
                    .collect(),
            ),
            Kind::Bool => Self::Bool(
                values
                    .into_iter()
                    .map(|v| v.and_then(|v| v.as_bool()))
                    .collect(),
            ),
            Kind::Str => {
                let mut dict = Vec::new();
                let mut lookup: AHashMap<String, u32> = AHashMap::new();
                let mut idx = Vec::with_capacity(values.len());
                for v in values {
                    match v {
                        Some(Value::String(s)) => idx.push(intern(&mut dict, &mut lookup, s)),
                        _ => idx.push(STR_NULL),
                    }
                }
                Self::Str { dict, lookup, idx }
            }
            // All-null columns are stored as Json so a later patch of any kind
            // fits without a degrade.
            Kind::Null | Kind::Other => Self::Json(
                values
                    .into_iter()
                    .map(|v| v.filter(|v| !v.is_null()))
                    .collect(),
            ),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Int(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Bool(v) => v.len(),
            Self::Str { idx, .. } => idx.len(),
            Self::Json(v) => v.len(),
        }
    }

    /// Grow the column with nulls to cover `len` dense slots.
    fn grow(&mut self, len: usize) {
        match self {
            Self::Int(v) => v.resize(len, None),
            Self::Float(v) => v.resize(len, None),
            Self::Bool(v) => v.resize(len, None),
            Self::Str { idx, .. } => idx.resize(len, STR_NULL),
            Self::Json(v) => v.resize(len, None),
        }
    }

    /// Clear one slot to null.
    fn clear(&mut self, dense: usize) {
        match self {
            Self::Int(v) => v[dense] = None,
            Self::Float(v) => v[dense] = None,
            Self::Bool(v) => v[dense] = None,
            Self::Str { idx, .. } => idx[dense] = STR_NULL,
            Self::Json(v) => v[dense] = None,
        }
    }

    /// Set one slot, degrading the column to `Json` when the value's kind does
    /// not match the column type.
    fn set(&mut self, dense: usize, value: Value) {
        match (&mut *self, kind_of(&value)) {
            (_, Kind::Null) => self.clear(dense),
            (Self::Int(v), Kind::Int) => v[dense] = value.as_i64(),
            (Self::Float(v), Kind::Float) => v[dense] = value.as_f64(),
            (Self::Bool(v), Kind::Bool) => v[dense] = value.as_bool(),
            (Self::Str { dict, lookup, idx }, Kind::Str) => {
                if let Value::String(s) = value {
                    idx[dense] = intern(dict, lookup, s);
                }
            }
            (Self::Json(v), _) => v[dense] = Some(value),
            _ => {
                self.degrade_to_json();
                self.set(dense, value);
            }
        }
    }

    /// Convert a typed column to the `Json` fallback in place, exactly.
    fn degrade_to_json(&mut self) {
        let json = |col: &Self| -> Vec<Option<Value>> {
            (0..col.len()).map(|d| col.get_json_opt(d)).collect()
        };
        *self = Self::Json(json(self));
    }

    /// The value at `dense`, or `None` for null/missing.
    pub(crate) fn get_json_opt(&self, dense: usize) -> Option<Value> {
        match self {
            Self::Int(v) => v[dense].map(Value::from),
            Self::Float(v) => v[dense].map(Value::from),
            Self::Bool(v) => v[dense].map(Value::from),
            Self::Str { dict, idx, .. } => match idx[dense] {
                STR_NULL => None,
                i => Some(Value::String(dict[i as usize].clone())),
            },
            Self::Json(v) => v[dense].clone(),
        }
    }
}

fn intern(dict: &mut Vec<String>, lookup: &mut AHashMap<String, u32>, s: String) -> u32 {
    if let Some(&i) = lookup.get(&s) {
        return i;
    }
    let i = dict.len() as u32;
    dict.push(s.clone());
    lookup.insert(s, i);
    i
}

/// The materialized column set with its own dense node mapping.
pub(crate) struct PropColumns {
    pub(crate) id_to_dense: AHashMap<NodeId, u32>,
    pub(crate) dense_to_id: Vec<NodeId>,
    pub(crate) cols: AHashMap<String, PropColumn>,
}

impl PropColumns {
    /// Build columns for every property name present, from one full scan.
    fn build(storage: &Storage) -> Result<Self, Error> {
        let rtxn = storage.env.read_txn()?;
        let mut dense_to_id = Vec::new();
        let mut decoded: Vec<Value> = Vec::new();
        for entry in storage.nodes.iter(&rtxn)? {
            let (id, bytes) = entry?;
            let rec: NodeRecord = props::decode(bytes)?;
            dense_to_id.push(id);
            decoded.push(props::decode(&rec.props)?);
        }
        let n = dense_to_id.len();
        let id_to_dense: AHashMap<NodeId, u32> = dense_to_id
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as u32))
            .collect();

        let mut values: AHashMap<String, Vec<Option<Value>>> = AHashMap::new();
        for (dense, json) in decoded.into_iter().enumerate() {
            if let Value::Object(map) = json {
                for (k, v) in map {
                    let col = values.entry(k).or_insert_with(|| vec![None; n]);
                    col[dense] = Some(v);
                }
            }
        }
        let cols = values
            .into_iter()
            .map(|(k, v)| (k, PropColumn::from_values(v)))
            .collect();
        Ok(Self {
            id_to_dense,
            dense_to_id,
            cols,
        })
    }

    /// Re-read `touched` node records and patch their slots in place. New
    /// nodes extend the dense mapping; new property names start a new column.
    fn patch(&mut self, storage: &Storage, touched: &[NodeId]) -> Result<(), Error> {
        let rtxn = storage.env.read_txn()?;
        for &id in touched {
            let bytes = match storage.nodes.get(&rtxn, &id)? {
                Some(b) => b,
                // Deleted between commit and refresh; deletion also sets
                // force_full, so this patch run's result is discarded anyway.
                None => continue,
            };
            let rec: NodeRecord = props::decode(bytes)?;
            let json: Value = props::decode(&rec.props)?;
            let dense = match self.id_to_dense.get(&id) {
                Some(&d) => d as usize,
                None => {
                    let d = self.dense_to_id.len();
                    self.dense_to_id.push(id);
                    self.id_to_dense.insert(id, d as u32);
                    d
                }
            };
            let n = self.dense_to_id.len();
            for col in self.cols.values_mut() {
                col.grow(n);
                col.clear(dense);
            }
            if let Value::Object(map) = json {
                for (k, v) in map {
                    let col = self.cols.entry(k).or_insert_with(|| {
                        let mut c = PropColumn::Json(Vec::new());
                        c.grow(n);
                        c
                    });
                    col.grow(n);
                    col.set(dense, v);
                }
            }
        }
        Ok(())
    }
}

/// Pending node mutations the columns have not absorbed yet.
#[derive(Default)]
struct ColumnsDelta {
    touched: Vec<NodeId>,
    force_full: bool,
}

/// Thread-safe lazy holder for [`PropColumns`], fed post-commit by the write
/// path and refreshed on read access.
#[derive(Default)]
pub(crate) struct ColumnsCache {
    columns: parking_lot::RwLock<Option<PropColumns>>,
    pending: parking_lot::Mutex<ColumnsDelta>,
}

impl ColumnsCache {
    /// Record an added or updated node. Called post-commit.
    pub(crate) fn record_touched(&self, id: NodeId) {
        let mut p = self.pending.lock();
        if !p.force_full {
            p.touched.push(id);
        }
    }

    pub(crate) fn record_touched_many(&self, ids: &[NodeId]) {
        let mut p = self.pending.lock();
        if !p.force_full {
            p.touched.extend_from_slice(ids);
        }
    }

    /// Record a node deletion. Called post-commit.
    pub(crate) fn record_force_full(&self) {
        let mut p = self.pending.lock();
        p.force_full = true;
        p.touched.clear();
    }

    /// Run `f` against fresh columns, building or patching them first if the
    /// cache is stale or absent.
    pub(crate) fn with_fresh<T>(
        &self,
        storage: &Storage,
        f: impl FnOnce(&PropColumns) -> T,
    ) -> Result<T, Error> {
        loop {
            {
                let guard = self.columns.read();
                if let Some(cols) = guard.as_ref() {
                    let p = self.pending.lock();
                    if p.touched.is_empty() && !p.force_full {
                        drop(p);
                        return Ok(f(cols));
                    }
                }
            }
            let mut guard = self.columns.write();
            let delta = std::mem::take(&mut *self.pending.lock());
            match guard.as_mut() {
                Some(cols) if !delta.force_full => cols.patch(storage, &delta.touched)?,
                _ => *guard = Some(PropColumns::build(storage)?),
            }
            // Loop back to the fast path so a delta that landed during the
            // rebuild is also absorbed before serving.
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn typed_values_round_trip_exactly() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node(
                "N",
                &json!({ "i": 42, "f": 1.5, "s": "hello", "b": true, "arr": [1, 2] }),
            )
            .unwrap();

        assert_eq!(g.node_prop_json(a, "i").unwrap(), Some(json!(42)));
        assert_eq!(g.node_prop_json(a, "f").unwrap(), Some(json!(1.5)));
        assert_eq!(g.node_prop_json(a, "s").unwrap(), Some(json!("hello")));
        assert_eq!(g.node_prop_json(a, "b").unwrap(), Some(json!(true)));
        assert_eq!(g.node_prop_json(a, "arr").unwrap(), Some(json!([1, 2])));
    }

    #[test]
    fn missing_property_is_null_and_missing_node_is_none() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        assert_eq!(
            g.node_prop_json(a, "nope").unwrap(),
            Some(serde_json::Value::Null)
        );
        assert_eq!(g.node_prop_json(a + 999, "x").unwrap(), None);
    }

    #[test]
    fn mixed_kind_property_keeps_exact_values() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "v": 1 })).unwrap();
        let b = g.add_node("N", &json!({ "v": "one" })).unwrap();
        assert_eq!(g.node_prop_json(a, "v").unwrap(), Some(json!(1)));
        assert_eq!(g.node_prop_json(b, "v").unwrap(), Some(json!("one")));
    }

    #[test]
    fn update_node_is_visible_and_can_remove_and_degrade() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1, "y": 2 })).unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), Some(json!(1)));

        // x changes kind (degrade), y disappears (cleared slot).
        g.update_node(a, &json!({ "x": "now a string" })).unwrap();
        assert_eq!(
            g.node_prop_json(a, "x").unwrap(),
            Some(json!("now a string"))
        );
        assert_eq!(
            g.node_prop_json(a, "y").unwrap(),
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn nodes_added_after_first_build_are_visible() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), Some(json!(1)));

        let b = g.add_node("N", &json!({ "x": 2, "fresh": "yes" })).unwrap();
        assert_eq!(g.node_prop_json(b, "x").unwrap(), Some(json!(2)));
        assert_eq!(g.node_prop_json(b, "fresh").unwrap(), Some(json!("yes")));
        // The new property name reads as null on the older node.
        assert_eq!(
            g.node_prop_json(a, "fresh").unwrap(),
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn delete_node_forces_rebuild() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        let b = g.add_node("N", &json!({ "x": 2 })).unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), Some(json!(1)));

        g.delete_node(a).unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), None);
        assert_eq!(g.node_prop_json(b, "x").unwrap(), Some(json!(2)));
    }

    #[test]
    fn batch_transaction_writes_are_visible() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), Some(json!(1)));

        let b = g
            .update(|txn| {
                txn.update_node(a, &json!({ "x": 10 }))?;
                txn.add_node("N", &json!({ "x": 20 }))
            })
            .unwrap();
        assert_eq!(g.node_prop_json(a, "x").unwrap(), Some(json!(10)));
        assert_eq!(g.node_prop_json(b, "x").unwrap(), Some(json!(20)));
    }
}
