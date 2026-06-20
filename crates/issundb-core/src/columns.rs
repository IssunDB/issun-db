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
use crate::schema::{EdgeId, EdgeRecord, NodeId, NodeRecord};
use crate::storage::{lmdb::Storage, props};

/// Abstracts which LMDB sub-database a column set is built from, so the same
/// columnar machinery serves both node and edge properties. A source knows how
/// to enumerate every entity's decoded properties, re-read one entity, and name
/// the not-found error for its entity kind.
pub(crate) trait ColumnSource {
    /// The entity id type (a `u64` for both nodes and edges).
    type Id: Copy + Eq + std::hash::Hash;

    /// Decode every entity's user properties as JSON, in storage iteration
    /// order. Each item is `(id, props_json)`.
    fn scan_all(storage: &Storage) -> Result<Vec<(Self::Id, Value)>, Error>;

    /// Re-read one entity's user properties as JSON; `None` if it no longer
    /// exists (deleted between commit and refresh).
    fn fetch_one(storage: &Storage, id: Self::Id) -> Result<Option<Value>, Error>;

    /// The not-found error for this entity kind.
    fn not_found(id: Self::Id) -> Error;
}

/// Builds node property columns from the `nodes` sub-database.
pub(crate) struct NodeSource;

impl ColumnSource for NodeSource {
    type Id = NodeId;

    fn scan_all(storage: &Storage) -> Result<Vec<(NodeId, Value)>, Error> {
        let rtxn = storage.env.read_txn()?;
        let mut out = Vec::new();
        for entry in storage.nodes.iter(&rtxn)? {
            let (id, bytes) = entry?;
            let rec: NodeRecord = props::decode(bytes)?;
            out.push((id, props::decode(&rec.props)?));
        }
        Ok(out)
    }

    fn fetch_one(storage: &Storage, id: NodeId) -> Result<Option<Value>, Error> {
        let rtxn = storage.env.read_txn()?;
        match storage.nodes.get(&rtxn, &id)? {
            Some(bytes) => {
                let rec: NodeRecord = props::decode(bytes)?;
                Ok(Some(props::decode(&rec.props)?))
            }
            None => Ok(None),
        }
    }

    fn not_found(id: NodeId) -> Error {
        Error::NodeNotFound(id)
    }
}

/// Builds edge property columns from the `edges` sub-database.
pub(crate) struct EdgeSource;

impl ColumnSource for EdgeSource {
    type Id = EdgeId;

    fn scan_all(storage: &Storage) -> Result<Vec<(EdgeId, Value)>, Error> {
        let rtxn = storage.env.read_txn()?;
        let mut out = Vec::new();
        for entry in storage.edges.iter(&rtxn)? {
            let (id, bytes) = entry?;
            let rec: EdgeRecord = props::decode(bytes)?;
            out.push((id, props::decode(&rec.props)?));
        }
        Ok(out)
    }

    fn fetch_one(storage: &Storage, id: EdgeId) -> Result<Option<Value>, Error> {
        let rtxn = storage.env.read_txn()?;
        match storage.edges.get(&rtxn, &id)? {
            Some(bytes) => {
                let rec: EdgeRecord = props::decode(bytes)?;
                Ok(Some(props::decode(&rec.props)?))
            }
            None => Ok(None),
        }
    }

    fn not_found(id: EdgeId) -> Error {
        Error::EdgeNotFound(id)
    }
}

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

    /// Whether the slot at `dense` holds a non-null value, without
    /// materializing it (a string slot would otherwise clone its dictionary
    /// entry). Backs the grouped-degree kernel's `count(v.prop)` null filter.
    pub(crate) fn is_present(&self, dense: usize) -> bool {
        match self {
            Self::Int(v) => v[dense].is_some(),
            Self::Float(v) => v[dense].is_some(),
            Self::Bool(v) => v[dense].is_some(),
            Self::Str { idx, .. } => idx[dense] != STR_NULL,
            Self::Json(v) => v[dense].is_some(),
        }
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

/// Lazily computed distribution statistics over one typed column's non-null
/// values: bounds, an equi-depth histogram, and the most common values.
pub(crate) struct PropStats {
    pub(crate) min: Value,
    pub(crate) max: Value,
    pub(crate) histogram: crate::histogram::Histogram,
    /// Up to [`MCV_LIMIT`] `(value, row_count)` pairs, most frequent first.
    pub(crate) mcvs: Vec<(Value, u64)>,
}

const MCV_LIMIT: usize = 8;
const HISTOGRAM_BUCKETS: usize = 10;

impl PropStats {
    /// Estimated fraction of non-null rows equal to `value`: the exact share
    /// when `value` is a most-common value, the histogram's uniform-in-bucket
    /// estimate otherwise.
    pub(crate) fn equality_selectivity(&self, value: &Value) -> f64 {
        for (v, count) in &self.mcvs {
            if v == value {
                return *count as f64 / self.histogram.total_rows as f64;
            }
        }
        self.histogram.estimate_equality_selectivity(value)
    }
}

/// The materialized column set with its own dense entity mapping. Generic over
/// the [`ColumnSource`] that supplies the entities (nodes or edges).
pub(crate) struct PropColumns<S: ColumnSource> {
    pub(crate) id_to_dense: AHashMap<S::Id, u32>,
    pub(crate) dense_to_id: Vec<S::Id>,
    pub(crate) cols: AHashMap<String, PropColumn>,
    /// Per-property stats, computed on first access through [`prop_stats`]
    /// and invalidated wholesale by [`patch`] (a patch clears the touched
    /// rows in every column, so every property's distribution may change).
    /// `None` is cached for columns with no usable stats (`Json` fallback
    /// columns and all-null columns).
    stats: AHashMap<String, Option<PropStats>>,
}

impl<S: ColumnSource> PropColumns<S> {
    /// Build columns for every property name present, from one full scan.
    fn build(storage: &Storage) -> Result<Self, Error> {
        let items = S::scan_all(storage)?;
        let n = items.len();
        let mut dense_to_id = Vec::with_capacity(n);
        let mut id_to_dense: AHashMap<S::Id, u32> = AHashMap::with_capacity(n);
        for (i, (id, _)) in items.iter().enumerate() {
            dense_to_id.push(*id);
            id_to_dense.insert(*id, i as u32);
        }

        let mut values: AHashMap<String, Vec<Option<Value>>> = AHashMap::new();
        for (dense, (_, json)) in items.into_iter().enumerate() {
            if let Value::Object(map) = json {
                for (k, v) in map {
                    let col = values.entry(k).or_insert_with(|| vec![None; n]);
                    col[dense] = Some(v);
                }
            }
        }
        let cols: AHashMap<String, PropColumn> = values
            .into_iter()
            .map(|(k, v)| (k, PropColumn::from_values(v)))
            .collect();
        Ok(Self {
            id_to_dense,
            dense_to_id,
            cols,
            stats: AHashMap::new(),
        })
    }

    /// Statistics for `prop`, computed on first access and cached until the
    /// next patch or rebuild. `None` when the property has no column, the
    /// column is the `Json` fallback, or it holds no non-null values.
    pub(crate) fn prop_stats(&mut self, prop: &str) -> Option<&PropStats> {
        if !self.stats.contains_key(prop) {
            let computed = self.cols.get(prop).and_then(compute_prop_stats);
            self.stats.insert(prop.to_string(), computed);
        }
        self.stats.get(prop).and_then(|s| s.as_ref())
    }

    /// Gather `props` for each id in `ids`, row-major: `out[i][j]` is the
    /// value of `props[j]` on `ids[i]`. Each id resolves to its dense index
    /// once, so the per-cell cost is one typed column read. A missing property
    /// (or a property name with no column) reads as `Value::Null`; a missing
    /// node is an error, matching the per-row executor path.
    pub(crate) fn props_table(
        &self,
        ids: &[S::Id],
        props: &[&str],
    ) -> Result<Vec<Vec<Value>>, Error> {
        let cols: Vec<Option<&PropColumn>> = props.iter().map(|p| self.cols.get(*p)).collect();
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let dense = *self
                .id_to_dense
                .get(&id)
                .ok_or_else(|| S::not_found(id))? as usize;
            out.push(
                cols.iter()
                    .map(|c| c.and_then(|c| c.get_json_opt(dense)).unwrap_or(Value::Null))
                    .collect(),
            );
        }
        Ok(out)
    }

    /// Gather one property for each id in `ids`: `out[i]` is the value of
    /// `prop` on `ids[i]`. The single-property form of `props_table`,
    /// returning one flat vector so the gather does not pay one row vector
    /// allocation per id. Same semantics: a missing property (or a property
    /// name with no column) reads as `Value::Null`; a missing node is an
    /// error.
    pub(crate) fn prop_column(&self, ids: &[S::Id], prop: &str) -> Result<Vec<Value>, Error> {
        let col = self.cols.get(prop);
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let dense = *self
                .id_to_dense
                .get(&id)
                .ok_or_else(|| S::not_found(id))? as usize;
            out.push(
                col.and_then(|c| c.get_json_opt(dense))
                    .unwrap_or(Value::Null),
            );
        }
        Ok(out)
    }

    /// Assign one dense group code per id under exact value identity of
    /// `prop`, plus one representative value per code (the first occurrence).
    /// Null and missing values share one code whose representative is
    /// `Value::Null`. Two ids get the same code exactly when decoding their
    /// records yields equal property values, which for scalar JSON values is
    /// also serialization equality, so grouping by code matches grouping by
    /// the serialized value. A missing node is an error.
    ///
    /// On a typed column the per-row cost is one dense-index read plus one
    /// native-keyed intern (for the dictionary-encoded string column, a plain
    /// array index); no `Value` is built per row.
    pub(crate) fn group_codes(
        &self,
        ids: &[S::Id],
        prop: &str,
    ) -> Result<(Vec<u32>, Vec<Value>), Error> {
        let mut codes = Vec::with_capacity(ids.len());
        let mut reps: Vec<Value> = Vec::new();

        let Some(col) = self.cols.get(prop) else {
            // No such column: every (existing) id is one null group.
            for &id in ids {
                if !self.id_to_dense.contains_key(&id) {
                    return Err(S::not_found(id));
                }
            }
            if !ids.is_empty() {
                reps.push(Value::Null);
                codes.resize(ids.len(), 0);
            }
            return Ok((codes, reps));
        };

        // Null gets its code lazily so an all-present column never spends one.
        let mut null_code: Option<u32> = None;
        let mut intern_null = |reps: &mut Vec<Value>| -> u32 {
            *null_code.get_or_insert_with(|| {
                reps.push(Value::Null);
                (reps.len() - 1) as u32
            })
        };

        match col {
            PropColumn::Int(v) => {
                let mut seen: AHashMap<i64, u32> = AHashMap::new();
                for &id in ids {
                    let dense = *self
                        .id_to_dense
                        .get(&id)
                        .ok_or_else(|| S::not_found(id))?
                        as usize;
                    codes.push(match v[dense] {
                        None => intern_null(&mut reps),
                        Some(n) => *seen.entry(n).or_insert_with(|| {
                            reps.push(Value::from(n));
                            (reps.len() - 1) as u32
                        }),
                    });
                }
            }
            PropColumn::Float(v) => {
                // Keyed by bit pattern: JSON numbers cannot be NaN, and the
                // shortest-roundtrip formatting is injective on f64, so bit
                // identity is serialization identity.
                let mut seen: AHashMap<u64, u32> = AHashMap::new();
                for &id in ids {
                    let dense = *self
                        .id_to_dense
                        .get(&id)
                        .ok_or_else(|| S::not_found(id))?
                        as usize;
                    codes.push(match v[dense] {
                        None => intern_null(&mut reps),
                        Some(f) => *seen.entry(f.to_bits()).or_insert_with(|| {
                            reps.push(Value::from(f));
                            (reps.len() - 1) as u32
                        }),
                    });
                }
            }
            PropColumn::Bool(v) => {
                let mut seen: [Option<u32>; 2] = [None, None];
                for &id in ids {
                    let dense = *self
                        .id_to_dense
                        .get(&id)
                        .ok_or_else(|| S::not_found(id))?
                        as usize;
                    codes.push(match v[dense] {
                        None => intern_null(&mut reps),
                        Some(b) => *seen[b as usize].get_or_insert_with(|| {
                            reps.push(Value::from(b));
                            (reps.len() - 1) as u32
                        }),
                    });
                }
            }
            PropColumn::Str { dict, idx, .. } => {
                // The dictionary index is already a dense value identity; the
                // per-row work is two array reads.
                let mut dict_code: Vec<u32> = vec![u32::MAX; dict.len()];
                for &id in ids {
                    let dense = *self
                        .id_to_dense
                        .get(&id)
                        .ok_or_else(|| S::not_found(id))?
                        as usize;
                    codes.push(match idx[dense] {
                        STR_NULL => intern_null(&mut reps),
                        i => {
                            if dict_code[i as usize] == u32::MAX {
                                reps.push(Value::String(dict[i as usize].clone()));
                                dict_code[i as usize] = (reps.len() - 1) as u32;
                            }
                            dict_code[i as usize]
                        }
                    });
                }
            }
            PropColumn::Json(v) => {
                // Mixed kinds: key by the serialized value, the exact group
                // identity the executor's string-keyed fold uses.
                let mut seen: AHashMap<String, u32> = AHashMap::new();
                for &id in ids {
                    let dense = *self
                        .id_to_dense
                        .get(&id)
                        .ok_or_else(|| S::not_found(id))?
                        as usize;
                    codes.push(match &v[dense] {
                        None => intern_null(&mut reps),
                        Some(val) => *seen.entry(val.to_string()).or_insert_with(|| {
                            reps.push(val.clone());
                            (reps.len() - 1) as u32
                        }),
                    });
                }
            }
        }
        Ok((codes, reps))
    }

    /// Re-read `touched` node records and patch their slots in place. New
    /// nodes extend the dense mapping; new property names start a new column.
    fn patch(&mut self, storage: &Storage, touched: &[S::Id]) -> Result<(), Error> {
        // A patch clears the touched rows in every column before re-setting
        // the present properties, so every cached distribution may be stale,
        // including ones whose property the new records no longer carry.
        if !touched.is_empty() {
            self.stats.clear();
        }
        for &id in touched {
            let json = match S::fetch_one(storage, id)? {
                Some(j) => j,
                // Deleted between commit and refresh; deletion also sets
                // force_full, so this patch run's result is discarded anyway.
                None => continue,
            };
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

/// Sorted non-null values of a typed column. `None` for the `Json` fallback,
/// whose mixed kinds have no total order to summarize.
fn sorted_non_null_values(col: &PropColumn) -> Option<Vec<Value>> {
    let mut vals: Vec<Value> = match col {
        PropColumn::Int(v) => v.iter().flatten().map(|&x| Value::from(x)).collect(),
        // NaN is excluded: it is unordered, and a NaN cell fails every
        // comparison the estimates model, so leaving it out keeps bounds
        // and histogram mass conservative.
        PropColumn::Float(v) => v
            .iter()
            .flatten()
            .filter(|x| !x.is_nan())
            .map(|&x| Value::from(x))
            .collect(),
        PropColumn::Bool(v) => v.iter().flatten().map(|&x| Value::Bool(x)).collect(),
        PropColumn::Str { dict, idx, .. } => idx
            .iter()
            .filter(|&&i| i != STR_NULL)
            .map(|&i| Value::String(dict[i as usize].clone()))
            .collect(),
        PropColumn::Json(_) => return None,
    };
    vals.sort_unstable_by(|a, b| {
        crate::histogram::compare_values(a, b).unwrap_or(std::cmp::Ordering::Equal)
    });
    Some(vals)
}

fn compute_prop_stats(col: &PropColumn) -> Option<PropStats> {
    let vals = sorted_non_null_values(col)?;
    let (min, max) = match (vals.first(), vals.last()) {
        (Some(mn), Some(mx)) => (mn.clone(), mx.clone()),
        _ => return None,
    };
    let histogram = crate::histogram::Histogram::build(&vals, HISTOGRAM_BUCKETS);

    let mut runs: Vec<(Value, u64)> = Vec::new();
    for v in &vals {
        match runs.last_mut() {
            Some((last, count)) if last == v => *count += 1,
            _ => runs.push((v.clone(), 1)),
        }
    }
    runs.sort_by(|a, b| b.1.cmp(&a.1));
    runs.truncate(MCV_LIMIT);

    Some(PropStats {
        min,
        max,
        histogram,
        mcvs: runs,
    })
}

/// Pending entity mutations the columns have not absorbed yet.
struct ColumnsDelta<Id> {
    touched: Vec<Id>,
    force_full: bool,
}

impl<Id> Default for ColumnsDelta<Id> {
    fn default() -> Self {
        Self {
            touched: Vec::new(),
            force_full: false,
        }
    }
}

/// Thread-safe lazy holder for [`PropColumns`], fed post-commit by the write
/// path and refreshed on read access. Generic over the [`ColumnSource`] so one
/// instance serves nodes and another serves edges.
pub(crate) struct ColumnsCache<S: ColumnSource> {
    columns: parking_lot::RwLock<Option<PropColumns<S>>>,
    pending: parking_lot::Mutex<ColumnsDelta<S::Id>>,
}

impl<S: ColumnSource> Default for ColumnsCache<S> {
    fn default() -> Self {
        Self {
            columns: parking_lot::RwLock::new(None),
            pending: parking_lot::Mutex::new(ColumnsDelta::default()),
        }
    }
}

impl<S: ColumnSource> ColumnsCache<S> {
    /// Record an added or updated entity. Called post-commit.
    pub(crate) fn record_touched(&self, id: S::Id) {
        let mut p = self.pending.lock();
        if !p.force_full {
            p.touched.push(id);
        }
    }

    pub(crate) fn record_touched_many(&self, ids: &[S::Id]) {
        let mut p = self.pending.lock();
        if !p.force_full {
            p.touched.extend_from_slice(ids);
        }
    }

    /// Record a deletion (which reshuffles the dense mapping). Called
    /// post-commit.
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
        f: impl FnOnce(&PropColumns<S>) -> T,
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

    /// Like [`with_fresh`], but with mutable column access, for readers that
    /// fill the lazy statistics cache. Takes the write lock for the whole
    /// call, so this is for optimizer-time stats reads, not the gather path.
    pub(crate) fn with_fresh_mut<T>(
        &self,
        storage: &Storage,
        f: impl FnOnce(&mut PropColumns<S>) -> T,
    ) -> Result<T, Error> {
        let mut guard = self.columns.write();
        loop {
            let delta = std::mem::take(&mut *self.pending.lock());
            if delta.touched.is_empty() && !delta.force_full {
                if let Some(cols) = guard.as_mut() {
                    return Ok(f(cols));
                }
            }
            match guard.as_mut() {
                Some(cols) if !delta.force_full => cols.patch(storage, &delta.touched)?,
                _ => *guard = Some(PropColumns::build(storage)?),
            }
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
    fn props_table_gathers_rows_in_input_order() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("N", &json!({ "name": "ada", "age": 36, "city": "london" }))
            .unwrap();
        let b = g
            .add_node("N", &json!({ "name": "bob", "age": 4 }))
            .unwrap();

        // Duplicate ids are allowed and each occurrence gets its own row.
        let table = g
            .node_props_json_table(&[b, a, b], &["name", "age", "city"])
            .unwrap();
        assert_eq!(
            table,
            vec![
                vec![json!("bob"), json!(4), serde_json::Value::Null],
                vec![json!("ada"), json!(36), json!("london")],
                vec![json!("bob"), json!(4), serde_json::Value::Null],
            ]
        );

        // An unknown property name yields a null column, not an error.
        let table = g.node_props_json_table(&[a], &["nope"]).unwrap();
        assert_eq!(table, vec![vec![serde_json::Value::Null]]);

        // Empty inputs are fine.
        assert!(g.node_props_json_table(&[], &["name"]).unwrap().is_empty());
        assert_eq!(
            g.node_props_json_table(&[a], &[]).unwrap(),
            vec![Vec::<serde_json::Value>::new()]
        );
    }

    #[test]
    fn group_codes_match_value_identity() {
        let (_dir, g) = open_tmp();
        // One mixed-kind property (a Json column): 1, "1", 1.0, true, and a
        // missing value must each get their own group; equal values share.
        let vals = [
            json!({ "v": 1 }),
            json!({ "v": "1" }),
            json!({ "v": 1.0 }),
            json!({ "v": true }),
            json!({}),
            json!({ "v": 1 }),
            json!({ "v": "1" }),
        ];
        let ids: Vec<_> = vals.iter().map(|p| g.add_node("N", p).unwrap()).collect();

        let (codes, reps) = g.node_prop_group_codes(&ids, "v").unwrap();
        assert_eq!(codes.len(), ids.len());
        // Equal values share a code; the representative is the value itself.
        assert_eq!(codes[0], codes[5]);
        assert_eq!(codes[1], codes[6]);
        let distinct: std::collections::HashSet<u32> = codes.iter().copied().collect();
        assert_eq!(distinct.len(), 5);
        for (i, &c) in codes.iter().enumerate() {
            let expected = vals[i].get("v").cloned().unwrap_or(serde_json::Value::Null);
            assert_eq!(reps[c as usize], expected, "representative for row {i}");
        }
    }

    #[test]
    fn group_codes_cover_typed_columns_and_unknown_props() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "s": "x", "i": 7 })).unwrap();
        let b = g.add_node("N", &json!({ "s": "y", "i": 7 })).unwrap();
        let c = g.add_node("N", &json!({ "s": "x" })).unwrap();

        // Dictionary-encoded string column: same string, same code.
        let (codes, reps) = g.node_prop_group_codes(&[a, b, c, a], "s").unwrap();
        assert_eq!(codes[0], codes[2]);
        assert_eq!(codes[0], codes[3]);
        assert_ne!(codes[0], codes[1]);
        assert_eq!(reps[codes[0] as usize], json!("x"));

        // Int column with a null slot.
        let (codes, reps) = g.node_prop_group_codes(&[a, b, c], "i").unwrap();
        assert_eq!(codes[0], codes[1]);
        assert_ne!(codes[0], codes[2]);
        assert_eq!(reps[codes[2] as usize], serde_json::Value::Null);

        // Unknown property: every row is one null group.
        let (codes, reps) = g.node_prop_group_codes(&[a, b], "nope").unwrap();
        assert_eq!(codes, vec![0, 0]);
        assert_eq!(reps, vec![serde_json::Value::Null]);

        // Missing node is an error, like the table gather.
        assert!(g.node_prop_group_codes(&[a + 999], "s").is_err());
    }

    #[test]
    fn prop_column_gathers_in_input_order() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("N", &json!({ "name": "ada", "age": 36 }))
            .unwrap();
        let b = g.add_node("N", &json!({ "name": "bob" })).unwrap();

        // Duplicate ids are allowed; a missing property reads as null.
        let col = g.node_prop_json_column(&[b, a, b], "age").unwrap();
        assert_eq!(
            col,
            vec![serde_json::Value::Null, json!(36), serde_json::Value::Null]
        );

        // An unknown property name yields a null column, not an error.
        let col = g.node_prop_json_column(&[a, b], "nope").unwrap();
        assert_eq!(col, vec![serde_json::Value::Null; 2]);

        // Empty input is fine; a missing node is an error, like the table.
        assert!(g.node_prop_json_column(&[], "age").unwrap().is_empty());
        let err = g.node_prop_json_column(&[a + 999], "age").unwrap_err();
        assert!(matches!(err, crate::error::Error::NodeNotFound(id) if id == a + 999));
    }

    #[test]
    fn props_table_errors_on_missing_node() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        let err = g.node_props_json_table(&[a, a + 999], &["x"]).unwrap_err();
        assert!(matches!(err, crate::error::Error::NodeNotFound(id) if id == a + 999));
    }

    #[test]
    fn props_table_sees_committed_writes_immediately() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "x": 1 })).unwrap();
        let table = g.node_props_json_table(&[a], &["x"]).unwrap();
        assert_eq!(table, vec![vec![json!(1)]]);

        g.update_node(a, &json!({ "x": 2 })).unwrap();
        let table = g.node_props_json_table(&[a], &["x"]).unwrap();
        assert_eq!(table, vec![vec![json!(2)]]);
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

    #[test]
    fn prop_column_min_max_bounds() {
        let (_dir, g) = open_tmp();
        let _a = g
            .add_node(
                "N",
                &json!({ "age": 30, "weight": 70.5, "active": true, "name": "ada" }),
            )
            .unwrap();
        let _b = g
            .add_node(
                "N",
                &json!({ "age": 40, "weight": 80.2, "active": false, "name": "bob" }),
            )
            .unwrap();
        let c = g
            .add_node(
                "N",
                &json!({ "age": 20, "weight": 60.1, "active": true, "name": "charlie" }),
            )
            .unwrap();

        let (min_age, max_age) = g.node_prop_min_max("age").unwrap().unwrap();
        assert_eq!(min_age, json!(20));
        assert_eq!(max_age, json!(40));

        let (min_w, max_w) = g.node_prop_min_max("weight").unwrap().unwrap();
        assert_eq!(min_w, json!(60.1));
        assert_eq!(max_w, json!(80.2));

        let (min_act, max_act) = g.node_prop_min_max("active").unwrap().unwrap();
        assert_eq!(min_act, json!(false));
        assert_eq!(max_act, json!(true));

        let (min_name, max_name) = g.node_prop_min_max("name").unwrap().unwrap();
        assert_eq!(min_name, json!("ada"));
        assert_eq!(max_name, json!("charlie"));

        // An unknown property has no statistics.
        assert!(g.node_prop_min_max("nope").unwrap().is_none());

        // An update invalidates the cached statistics: 20 is gone and 50 is
        // the new maximum.
        g.update_node(c, &json!({ "age": 50, "weight": 90.0 }))
            .unwrap();
        let (min_age, max_age) = g.node_prop_min_max("age").unwrap().unwrap();
        assert_eq!(min_age, json!(30));
        assert_eq!(max_age, json!(50));
    }

    #[test]
    fn prop_stats_refresh_when_update_removes_the_property() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "age": 10 })).unwrap();
        let _b = g.add_node("N", &json!({ "age": 99 })).unwrap();
        let (_, max_age) = g.node_prop_min_max("age").unwrap().unwrap();
        assert_eq!(max_age, json!(99));

        // The new record no longer carries `age` at all, so the key is absent
        // from the patched property map; the stats must still refresh.
        g.update_node(a + 1, &json!({ "renamed": 1 })).unwrap();
        let (min_age, max_age) = g.node_prop_min_max("age").unwrap().unwrap();
        assert_eq!(min_age, json!(10));
        assert_eq!(max_age, json!(10));
    }

    #[test]
    fn equality_selectivity_uses_most_common_values() {
        let (_dir, g) = open_tmp();
        for _ in 0..90 {
            g.add_node("N", &json!({ "team": "blue" })).unwrap();
        }
        for i in 0..10 {
            g.add_node("N", &json!({ "team": format!("t{i}") }))
                .unwrap();
        }
        let sel = g
            .estimate_equality_selectivity("team", &json!("blue"))
            .unwrap()
            .unwrap();
        assert!((sel - 0.9).abs() < 1e-9, "got {sel}");
        // A value outside the column's bounds estimates to zero.
        let sel = g
            .estimate_equality_selectivity("team", &json!("zzz"))
            .unwrap()
            .unwrap();
        assert_eq!(sel, 0.0);
        // No statistics exist for an unknown property.
        assert!(
            g.estimate_equality_selectivity("nope", &json!(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn range_selectivity_estimates_fraction() {
        let (_dir, g) = open_tmp();
        for i in 0..100 {
            g.add_node("N", &json!({ "age": i })).unwrap();
        }
        let sel = g
            .estimate_range_selectivity("age", Some(&json!(50)), None)
            .unwrap()
            .unwrap();
        assert!((sel - 0.5).abs() < 0.05, "got {sel}");
        let sel = g
            .estimate_range_selectivity("age", None, Some(&json!(1000)))
            .unwrap()
            .unwrap();
        assert!((sel - 1.0).abs() < 1e-9, "got {sel}");
    }

    // ------------------------------------------------------------------
    // Edge property columns
    // ------------------------------------------------------------------

    #[test]
    fn edge_typed_values_round_trip_and_missing_semantics() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let e = g
            .add_edge(a, b, "E", &json!({ "i": 7, "s": "hit", "f": 1.5, "b": true }))
            .unwrap();

        assert_eq!(g.edge_prop_json(e, "i").unwrap(), Some(json!(7)));
        assert_eq!(g.edge_prop_json(e, "s").unwrap(), Some(json!("hit")));
        assert_eq!(g.edge_prop_json(e, "f").unwrap(), Some(json!(1.5)));
        assert_eq!(g.edge_prop_json(e, "b").unwrap(), Some(json!(true)));
        // Missing property is null; nonexistent edge is None.
        assert_eq!(g.edge_prop_json(e, "nope").unwrap(), Some(serde_json::Value::Null));
        assert_eq!(g.edge_prop_json(e + 999, "i").unwrap(), None);
    }

    #[test]
    fn edge_table_and_column_gather_in_input_order() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let e1 = g.add_edge(a, b, "E", &json!({ "pa": 1, "hr": 1 })).unwrap();
        let e2 = g.add_edge(a, b, "E", &json!({ "pa": 1 })).unwrap();

        let table = g
            .edge_props_json_table(&[e2, e1, e2], &["pa", "hr"])
            .unwrap();
        assert_eq!(
            table,
            vec![
                vec![json!(1), serde_json::Value::Null],
                vec![json!(1), json!(1)],
                vec![json!(1), serde_json::Value::Null],
            ]
        );
        let col = g.edge_prop_json_column(&[e2, e1], "hr").unwrap();
        assert_eq!(col, vec![serde_json::Value::Null, json!(1)]);
        // A nonexistent edge is an error, like the node gather.
        assert!(g.edge_props_json_table(&[e1 + 999], &["pa"]).is_err());
    }

    #[test]
    fn edge_group_codes_match_value_identity() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let e1 = g.add_edge(a, b, "E", &json!({ "hand": "L" })).unwrap();
        let e2 = g.add_edge(a, b, "E", &json!({ "hand": "R" })).unwrap();
        let e3 = g.add_edge(a, b, "E", &json!({ "hand": "L" })).unwrap();

        let (codes, reps) = g.edge_prop_group_codes(&[e1, e2, e3], "hand").unwrap();
        assert_eq!(codes[0], codes[2]);
        assert_ne!(codes[0], codes[1]);
        assert_eq!(reps[codes[0] as usize], json!("L"));
        assert_eq!(reps[codes[1] as usize], json!("R"));
    }

    #[test]
    fn edge_update_is_visible_and_delete_forces_rebuild() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let e1 = g.add_edge(a, b, "E", &json!({ "w": 1 })).unwrap();
        let e2 = g.add_edge(a, b, "E", &json!({ "w": 2 })).unwrap();
        assert_eq!(g.edge_prop_json(e1, "w").unwrap(), Some(json!(1)));

        g.update_edge(e1, &json!({ "w": 10 })).unwrap();
        assert_eq!(g.edge_prop_json(e1, "w").unwrap(), Some(json!(10)));

        g.delete_edge(e1).unwrap();
        // The surviving edge is still readable after the forced rebuild.
        assert_eq!(g.edge_prop_json(e2, "w").unwrap(), Some(json!(2)));
        assert_eq!(g.edge_prop_json(e1, "w").unwrap(), None);
    }

    #[test]
    fn edge_columns_built_in_batch_transaction_are_visible() {
        let (_dir, g) = open_tmp();
        let (a, b) = g
            .update(|txn| {
                let a = txn.add_node("N", &())?;
                let b = txn.add_node("N", &())?;
                Ok((a, b))
            })
            .unwrap();
        let e = g
            .update(|txn| txn.add_edge(a, b, "E", &json!({ "k": 42 })))
            .unwrap();
        assert_eq!(g.edge_prop_json(e, "k").unwrap(), Some(json!(42)));
    }
}
