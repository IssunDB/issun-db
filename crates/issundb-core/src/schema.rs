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

/// Stored in the `nodes` LMDB sub-database as msgpack bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecord {
    pub label: LabelId,
    pub props: Vec<u8>, // msgpack-encoded user properties
}

/// Stored in the `edges` LMDB sub-database as msgpack bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub src: NodeId,
    pub dst: NodeId,
    pub edge_type: TypeId,
    pub props: Vec<u8>, // msgpack-encoded user properties
}
