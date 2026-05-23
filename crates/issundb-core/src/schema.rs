use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub type NodeId = u64;
pub type EdgeId = u64;
pub type LabelId = u32;
pub type TypeId = u32;

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
