use crate::error::Error;
use crate::schema::{EdgeId, LabelId, NodeId, TypeId};
use crate::storage::lmdb::Storage;

const KEY_NEXT_NODE: &str = "next_node_id";
const KEY_NEXT_EDGE: &str = "next_edge_id";
const KEY_NEXT_LABEL: &str = "next_label_id";
const KEY_NEXT_TYPE: &str = "next_type_id";

fn bump_counter(storage: &Storage, txn: &mut heed::RwTxn, key: &str) -> Result<u64, Error> {
    let current = storage
        .meta
        .get(txn, key)?
        .map(|b| {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| Error::Corrupt("counter must be 8 bytes"))?;
            Ok::<u64, Error>(u64::from_be_bytes(arr))
        })
        .transpose()?
        .unwrap_or(0);
    storage.meta.put(txn, key, &(current + 1).to_be_bytes())?;
    Ok(current)
}

pub fn alloc_node_id(storage: &Storage, txn: &mut heed::RwTxn) -> Result<NodeId, Error> {
    bump_counter(storage, txn, KEY_NEXT_NODE)
}

pub fn alloc_edge_id(storage: &Storage, txn: &mut heed::RwTxn) -> Result<EdgeId, Error> {
    bump_counter(storage, txn, KEY_NEXT_EDGE)
}

/// Returns the existing `LabelId` for `name`, or allocates a new one.
pub fn get_or_create_label(
    storage: &Storage,
    txn: &mut heed::RwTxn,
    name: &str,
) -> Result<LabelId, Error> {
    let meta_key = format!("label:{name}");
    if let Some(b) = storage.meta.get(txn, &meta_key)? {
        let arr: [u8; 4] = b
            .try_into()
            .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
        return Ok(u32::from_be_bytes(arr));
    }
    let id = bump_counter(storage, txn, KEY_NEXT_LABEL)? as LabelId;
    storage.meta.put(txn, &meta_key, &id.to_be_bytes())?;
    Ok(id)
}

/// Returns the existing `TypeId` for `name`, or allocates a new one.
pub fn get_or_create_type(
    storage: &Storage,
    txn: &mut heed::RwTxn,
    name: &str,
) -> Result<TypeId, Error> {
    let meta_key = format!("type:{name}");
    if let Some(b) = storage.meta.get(txn, &meta_key)? {
        let arr: [u8; 4] = b
            .try_into()
            .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
        return Ok(u32::from_be_bytes(arr));
    }
    let id = bump_counter(storage, txn, KEY_NEXT_TYPE)? as TypeId;
    storage.meta.put(txn, &meta_key, &id.to_be_bytes())?;
    Ok(id)
}

/// Adjusts the count of a label in the meta database.
pub fn adjust_label_count(
    storage: &Storage,
    txn: &mut heed::RwTxn,
    label_id: LabelId,
    delta: i64,
) -> Result<(), Error> {
    let key = format!("stats:l:{label_id}");
    let current = storage
        .meta
        .get(txn, &key)?
        .map(|b| {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| Error::Corrupt("counter must be 8 bytes"))?;
            Ok::<i64, Error>(i64::from_be_bytes(arr))
        })
        .transpose()?
        .unwrap_or(0);
    let new_count = (current + delta).max(0);
    storage.meta.put(txn, &key, &new_count.to_be_bytes())?;
    Ok(())
}

/// Retrieves the count of a label from the meta database.
pub fn get_label_count(
    storage: &Storage,
    txn: &heed::RoTxn,
    label_id: LabelId,
) -> Result<u64, Error> {
    let key = format!("stats:l:{label_id}");
    let count = storage
        .meta
        .get(txn, &key)?
        .map(|b| {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| Error::Corrupt("counter must be 8 bytes"))?;
            Ok::<i64, Error>(i64::from_be_bytes(arr))
        })
        .transpose()?
        .unwrap_or(0);
    Ok(count as u64)
}

/// Adjusts the count of an edge type in the meta database.
pub fn adjust_type_count(
    storage: &Storage,
    txn: &mut heed::RwTxn,
    type_id: TypeId,
    delta: i64,
) -> Result<(), Error> {
    let key = format!("stats:t:{type_id}");
    let current = storage
        .meta
        .get(txn, &key)?
        .map(|b| {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| Error::Corrupt("counter must be 8 bytes"))?;
            Ok::<i64, Error>(i64::from_be_bytes(arr))
        })
        .transpose()?
        .unwrap_or(0);
    let new_count = (current + delta).max(0);
    storage.meta.put(txn, &key, &new_count.to_be_bytes())?;
    Ok(())
}

/// Retrieves the count of an edge type from the meta database.
pub fn get_type_count(storage: &Storage, txn: &heed::RoTxn, type_id: TypeId) -> Result<u64, Error> {
    let key = format!("stats:t:{type_id}");
    let count = storage
        .meta
        .get(txn, &key)?
        .map(|b| {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| Error::Corrupt("counter must be 8 bytes"))?;
            Ok::<i64, Error>(i64::from_be_bytes(arr))
        })
        .transpose()?
        .unwrap_or(0);
    Ok(count as u64)
}
