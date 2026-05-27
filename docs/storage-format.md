# Storage Format

IssunDB uses LMDB as its storage engine. A single LMDB environment holds twelve
named sub-databases. All integers are stored big-endian so that LMDB's
lexicographic byte ordering coincides with numeric ordering.

## LMDB Sub-Databases

| Database | Key | Value | Flags | Purpose |
|---|---|---|---|---|
| `nodes` | `NodeId` (u64 BE) | msgpack `NodeRecord` | — | Node label and properties |
| `edges` | `EdgeId` (u64 BE) | msgpack `EdgeRecord` | — | Edge src, dst, type, properties |
| `out_adj` | `NodeId` (u64 BE) | `AdjEntry` (20 bytes) | DUPSORT + DUPFIXED | Outgoing adjacency list |
| `in_adj` | `NodeId` (u64 BE) | `AdjEntry` (20 bytes) | DUPSORT + DUPFIXED | Incoming adjacency list |
| `label_idx` | `(LabelId u32 BE, NodeId u64 BE)` (12 bytes) | unit | — | Secondary index: label → node IDs |
| `type_idx` | `(TypeId u32 BE, EdgeId u64 BE)` (12 bytes) | unit | — | Secondary index: edge type → edge IDs |
| `node_prop_idx` | `(LabelId u32 BE, PropKeyId u32 BE, value bytes, NodeId u64 BE)` | unit | — | Property value index for nodes |
| `edge_prop_idx` | `(TypeId u32 BE, PropKeyId u32 BE, value bytes, EdgeId u64 BE)` | unit | — | Property value index for edges |
| `fts_postings` | `(label_str, '\0', prop_str, '\0', term_str)` | `(NodeId u64 BE, tf u32 BE)` | DUPSORT + DUPFIXED | Inverted index: term → (doc, TF) pairs |
| `fts_docs` | `(label_str, '\0', prop_str, '\0', NodeId u64 BE)` | `doc_len u32 BE` | — | Per-document token count for BM25 |
| `vectors` | `NodeId` (u64 BE) | `[f32]` little-endian bytes | — | Raw float embeddings for cold-start |
| `meta` | string key | various | — | ID counters, label/type registries, FTS stats |

## Adjacency Entry Layout (`AdjEntry`)

```
Offset  Size  Field
------  ----  -----
0       4     edge_type  (TypeId, u32)
4       8     other      (NodeId: dst for out_adj, src for in_adj, u64)
12      8     edge_id    (EdgeId, u64)
```

Total: 20 bytes. `DUPFIXED` requires all duplicate values under a key to have
the same size; `DUPSORT` orders them lexicographically over these raw bytes.

## ID Allocation

Node IDs and edge IDs are monotonically increasing u64 counters persisted in the
`meta` database under the keys `"next_node_id"` and `"next_edge_id"`. Label and
edge-type strings are mapped to u32 integers and persisted under
`"label:<name>"` and `"type:<name>"` respectively.

## msgpack Encoding

Node and edge records are serialized with `rmp-serde` (MessagePack). The schema
types (`NodeRecord`, `EdgeRecord`) implement `serde::Serialize` and
`serde::Deserialize`. User-supplied properties are pre-encoded as msgpack bytes
and stored as an opaque `Vec<u8>` inside the record; this avoids a double
deserialization on every read.

## FTS Stats

Full-text index statistics (total document count and sum of document lengths per
`(label, property)` index) are stored in the `meta` database under the key
`"fts_stats:<label>\0<property>"`, serialized as msgpack `(u64, u64)`.
