use deepsize::DeepSizeOf;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub type NodeId = u64;
pub type EdgeId = u64;
pub type LabelId = u32;
pub type TypeId = u32;
pub type PropKeyId = u32;

/// Supported languages for Full-Text Search indexing and stemming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Language {
    English = 1,
    Spanish = 2,
    French = 3,
    German = 4,
    Italian = 5,
    Portuguese = 6,
}

impl Default for Language {
    fn default() -> Self {
        Self::English
    }
}

impl Language {
    pub fn from_u8(val: u8) -> Self {
        match val {
            2 => Self::Spanish,
            3 => Self::French,
            4 => Self::German,
            5 => Self::Italian,
            6 => Self::Portuguese,
            _ => Self::English,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// One adjacency entry stored as a raw LMDB duplicate value.
///
/// Fixed 20-byte `#[repr(C, packed)]` layout satisfies `DUPFIXED` (all
/// duplicate values for a key must have identical size). `DUPSORT` orders
/// duplicates lexicographically over these raw bytes.
#[derive(Clone, Copy, Debug, IntoBytes, FromBytes, Immutable)]
#[repr(C, packed)]
pub struct AdjEntry {
    pub edge_type: TypeId, // 4 bytes
    pub other: NodeId,     // 8 bytes: dst for out_adj, src for in_adj
    pub edge_id: EdgeId,   // 8 bytes
}

impl DeepSizeOf for AdjEntry {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        // AdjEntry is a fixed-size packed struct of primitives: no heap allocations.
        0
    }
}

/// Stored in the `nodes` LMDB sub-database as msgpack bytes.
///
/// A node carries a set of labels. The set may be empty (an unlabeled node) and
/// labels are stored in insertion order. Use [`NodeRecord::primary_label`] when a
/// single representative label is needed for display.
#[derive(Debug, Clone, Serialize, Deserialize, DeepSizeOf)]
pub struct NodeRecord {
    pub labels: Vec<LabelId>,
    pub props: Vec<u8>, // msgpack-encoded user properties
}

impl NodeRecord {
    /// The first label assigned to the node, if any. Used where a single
    /// representative label is needed (display, REST/MCP responses, vector hits).
    pub fn primary_label(&self) -> Option<LabelId> {
        self.labels.first().copied()
    }

    /// Returns true if the node carries the given label id.
    pub fn has_label(&self, id: LabelId) -> bool {
        self.labels.contains(&id)
    }
}

/// Stored in the `edges` LMDB sub-database as msgpack bytes.
#[derive(Debug, Clone, Serialize, Deserialize, DeepSizeOf)]
pub struct EdgeRecord {
    pub src: NodeId,
    pub dst: NodeId,
    pub edge_type: TypeId,
    pub props: Vec<u8>, // msgpack-encoded user properties
}

/// The result of a single adjacency lookup entry returned by
/// [`crate::Graph::out_neighbors`] and [`crate::Graph::in_neighbors`].
#[derive(Debug, Clone, PartialEq)]
pub struct NeighborEntry {
    pub node: NodeId,
    pub edge: EdgeId,
    pub edge_type: TypeId,
}

/// A neighbor entry with a direction flag, returned by [`crate::Graph::all_neighbors`].
#[derive(Debug, Clone, PartialEq)]
pub struct DirectedNeighborEntry {
    pub node: NodeId,
    pub edge: EdgeId,
    pub edge_type: TypeId,
    /// `true` for outgoing edges, `false` for incoming.
    pub outgoing: bool,
}

/// A path with an associated total weight, returned by weighted path algorithms.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedPath {
    pub nodes: Vec<NodeId>,
    pub total_weight: f64,
}

/// A typed property value used in index lookups and range queries.
///
/// Use this instead of raw `serde_json::Value` when querying nodes or edges
/// by property.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl PropValue {
    /// Convert to the `serde_json::Value` representation used in internal
    /// property encoding.
    pub(crate) fn into_json(self) -> serde_json::Value {
        match self {
            PropValue::Bool(b) => serde_json::Value::Bool(b),
            PropValue::Int(i) => serde_json::Value::Number(i.into()),
            PropValue::Float(f) => serde_json::json!(f),
            PropValue::Str(s) => serde_json::Value::String(s),
        }
    }
}

impl From<bool> for PropValue {
    fn from(v: bool) -> Self {
        PropValue::Bool(v)
    }
}
impl From<i64> for PropValue {
    fn from(v: i64) -> Self {
        PropValue::Int(v)
    }
}
impl From<f64> for PropValue {
    fn from(v: f64) -> Self {
        PropValue::Float(v)
    }
}
impl From<String> for PropValue {
    fn from(v: String) -> Self {
        PropValue::Str(v)
    }
}
impl<'a> From<&'a str> for PropValue {
    fn from(v: &'a str) -> Self {
        PropValue::Str(v.to_string())
    }
}
