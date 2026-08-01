use super::*;

/// Cached committed-state label scans for one write generation: the shared
/// sorted id vector per label behind [`Graph::nodes_by_label_arc`]. `gen` is
/// the [`crate::csr::CsrCache`] write generation the entries reflect; a
/// mismatch discards them all, so an entry can never outlive the commit that
/// invalidated it.
#[derive(Default)]
pub(crate) struct LabelScanCache {
    generation: u64,
    by_label: AHashMap<String, std::sync::Arc<Vec<NodeId>>>,
}

impl Graph {
    // ------------------------------------------------------------------
    // Secondary index queries
    // ------------------------------------------------------------------

    /// Returns all node IDs with the given label, in ascending ID order.
    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        Ok(self.nodes_by_label_arc(label)?.as_ref().clone())
    }

    /// [`Graph::nodes_by_label`] without the copy: the shared, cached scan
    /// result, in ascending ID order. Repeated reads of one label within one
    /// write generation serve the same vector; any committed write invalidates
    /// the whole cache.
    ///
    /// The generation is read while holding the cache lock and the scan runs
    /// under a transaction opened after that read, so an entry can be fresher
    /// than the generation it is filed under (a commit landing mid-scan, which
    /// the very next read discards) but never staler: data for a generation is
    /// committed before the counter reports it, so a transaction opened after
    /// the counter read observes everything the stamped generation promises.
    pub fn nodes_by_label_arc(&self, label: &str) -> Result<std::sync::Arc<Vec<NodeId>>, Error> {
        let mut cache = self.label_scans.lock();
        let generation = self.csr_cache.current_gen();
        if cache.generation != generation {
            cache.by_label.clear();
            cache.generation = generation;
        }
        if let Some(hit) = cache.by_label.get(label) {
            return Ok(hit.clone());
        }
        let ids = {
            let rtxn = self.storage.env.read_txn()?;
            std::sync::Arc::new(self.nodes_by_label_impl(&rtxn, label)?)
        };
        cache.by_label.insert(label.to_string(), ids.clone());
        Ok(ids)
    }

    pub(super) fn nodes_by_label_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
    ) -> Result<Vec<NodeId>, Error> {
        let label_id = {
            let key = format!("label:{label}");
            match self.storage.meta.get(rtxn, &key)? {
                Some(b) => {
                    let arr: [u8; 4] = b
                        .try_into()
                        .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                    u32::from_be_bytes(arr)
                }
                None => return Ok(vec![]),
            }
        };
        let prefix = label_id.to_be_bytes();
        let iter = self.storage.label_idx.prefix_iter(rtxn, &prefix)?;
        let mut ids = Vec::new();
        for result in iter {
            let (key, _) = result?;
            let id_bytes: [u8; 8] = key[4..]
                .try_into()
                .map_err(|_| Error::Corrupt("label_idx key has wrong length"))?;
            ids.push(u64::from_be_bytes(id_bytes));
        }
        Ok(ids)
    }

    /// Returns the subset of `nodes` that carry `label`, preserving input
    /// order. One `label_idx` point lookup per candidate, so the cost scales
    /// with the candidate set rather than the label population.
    #[doc(hidden)]
    pub fn label_filter(&self, nodes: &[NodeId], label: &str) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let label_id = match get_label(&self.storage, &rtxn, label)? {
            Some(id) => id,
            None => return Ok(vec![]),
        };
        let mut out = Vec::new();
        for &n in nodes {
            if self
                .storage
                .label_idx
                .get(&rtxn, &composite_key(label_id, n))?
                .is_some()
            {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// Returns all edge IDs with the given type, in ascending ID order.
    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_type_impl(&rtxn, etype)
    }

    pub(super) fn edges_by_type_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
    ) -> Result<Vec<EdgeId>, Error> {
        let type_id = {
            let key = format!("type:{etype}");
            match self.storage.meta.get(rtxn, &key)? {
                Some(b) => {
                    let arr: [u8; 4] = b
                        .try_into()
                        .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                    u32::from_be_bytes(arr)
                }
                None => return Ok(vec![]),
            }
        };
        let prefix = type_id.to_be_bytes();
        let iter = self.storage.type_idx.prefix_iter(rtxn, &prefix)?;
        let mut ids = Vec::new();
        for result in iter {
            let (key, _) = result?;
            let id_bytes: [u8; 8] = key[4..]
                .try_into()
                .map_err(|_| Error::Corrupt("type_idx key has wrong length"))?;
            ids.push(u64::from_be_bytes(id_bytes));
        }
        Ok(ids)
    }

    // ------------------------------------------------------------------
    // Registry reverse lookups
    // ------------------------------------------------------------------

    /// Resolves a `LabelId` back to its string name.
    ///
    /// Scans the `meta` sub-database for the matching `label:{name}` entry.
    /// Returns `None` for ids that are not in the registry.
    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.label_name_impl(&rtxn, id)
    }

    pub(super) fn label_name_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        id: LabelId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "label:", id)
    }

    /// Resolves a `TypeId` back to its string name.
    ///
    /// Scans the `meta` sub-database for the matching `type:{name}` entry.
    /// Returns `None` for ids that are not in the registry.
    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.type_name_impl(&rtxn, id)
    }

    pub(super) fn type_name_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        id: TypeId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "type:", id)
    }

    pub(super) fn prop_key_name_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        id: PropKeyId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "prop_key:", id)
    }

    /// Validate the active edge constraints for `etype` against the edge's
    /// encoded properties and write one `edge_prop_idx` entry per indexed
    /// property. Shared by `add_edge` and `update_edge`; `update_edge` must
    /// drop the edge's old entries first so the unique check never conflicts
    /// with the edge itself.
    pub(super) fn write_edge_index_entries(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        edge_id: EdgeId,
        type_id: TypeId,
        etype: &str,
        encoded_props: &[u8],
    ) -> Result<(), Error> {
        let active_indexes = self.get_active_edge_indexes(wtxn, type_id)?;
        if active_indexes.is_empty() {
            return Ok(());
        }
        let props_json: serde_json::Value = props::decode(encoded_props)?;
        for (prop_key_id, flags) in active_indexes {
            if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                let prop_val = props_json.get(&prop_name);

                if flags == 0x02
                    && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null))
                {
                    return Err(Error::RequiredConstraintViolation(
                        etype.to_string(),
                        prop_name.to_string(),
                    ));
                }

                if let Some(val) = prop_val {
                    if val != &serde_json::Value::Null {
                        // Runs for every non-null value, including a string too
                        // long to index (absent from `edge_prop_idx`), which
                        // falls back to a type scan so the constraint still holds.
                        if flags == 0x01 {
                            self.check_edge_property_unique(
                                wtxn,
                                type_id,
                                etype,
                                prop_key_id,
                                &prop_name,
                                val,
                                edge_id,
                            )?;
                        }

                        if let Some(encoded) = encode_property_value(val) {
                            let idx_key =
                                edge_prop_index_key(type_id, prop_key_id, &encoded, edge_id);
                            self.storage.edge_prop_idx.put(wtxn, &idx_key, &())?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Enforce a unique constraint for one edge property value, excluding the
    /// edge itself. An index-encodable value is checked via `edge_prop_idx`
    /// (exact encoded-value match, so `30` and `30.0` conflict); a value too
    /// long to index falls back to a type scan comparing stored values, so the
    /// constraint holds for long strings that never reach the index. Mirrors
    /// `check_node_property_unique`.
    #[allow(clippy::too_many_arguments)]
    fn check_edge_property_unique(
        &self,
        wtxn: &crate::storage::RwTxn,
        type_id: TypeId,
        etype: &str,
        prop_key_id: PropKeyId,
        prop_name: &str,
        val: &serde_json::Value,
        edge_id: EdgeId,
    ) -> Result<(), Error> {
        let violation = || {
            Error::UniqueConstraintViolation(
                etype.to_string(),
                prop_name.to_string(),
                val.to_string(),
            )
        };
        if let Some(encoded) = encode_property_value(val) {
            let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
            prefix.extend_from_slice(&type_id.to_be_bytes());
            prefix.extend_from_slice(&prop_key_id.to_be_bytes());
            prefix.extend_from_slice(&encoded);
            for entry in self.storage.edge_prop_idx.prefix_iter(wtxn, &prefix)? {
                let (key, _) = entry?;
                // Only an exact encoded-value match conflicts; a prefix-only
                // match is a distinct string value (see `exact_prop_index_id`).
                if let Some(found_edge_id) = exact_prop_index_id(key, &encoded) {
                    if found_edge_id != edge_id {
                        return Err(violation());
                    }
                }
            }
        } else {
            // Too long to index: compare the stored value on every other edge
            // of this type.
            for other in self.edges_by_type_impl(wtxn, etype)? {
                if other == edge_id {
                    continue;
                }
                if let Some(record) = self.get_edge_impl(wtxn, other)? {
                    let props: serde_json::Value = props::decode(&record.props)?;
                    if props.get(prop_name) == Some(val) {
                        return Err(violation());
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn delete_edge_index_entries(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        edge_id: EdgeId,
        record: &EdgeRecord,
    ) -> Result<(), Error> {
        let active_indexes = self.get_active_edge_indexes(wtxn, record.edge_type)?;
        if !active_indexes.is_empty() {
            let props_json: serde_json::Value = props::decode(&record.props)?;
            for (prop_key_id, _) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    if let Some(val) = props_json.get(&prop_name) {
                        if let Some(encoded) = encode_property_value(val) {
                            let idx_key = edge_prop_index_key(
                                record.edge_type,
                                prop_key_id,
                                &encoded,
                                edge_id,
                            );
                            self.storage.edge_prop_idx.delete(wtxn, &idx_key)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Get the count of nodes matching a string label.
    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.node_count_by_label_impl(&rtxn, label)
    }

    /// Estimates the node count from the node-id high-water mark, an upper bound. It
    /// does not decrease when a node is deleted, so it is not an exact live
    /// count; it exists for query-planner cardinality estimates (for example,
    /// average relationship fan-out). O(1).
    pub fn node_count_hint(&self) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        crate::storage::ids::node_high_water(&self.storage, &rtxn)
    }

    pub(super) fn node_count_by_label_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
    ) -> Result<u64, Error> {
        let meta_key = format!("label:{label}");
        if let Some(b) = self.storage.meta.get(rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
            let label_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_label_count(&self.storage, rtxn, label_id)
        } else {
            Ok(0)
        }
    }

    /// Get the count of edges matching a string type.
    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edge_count_by_type_impl(&rtxn, etype)
    }

    pub(super) fn edge_count_by_type_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
    ) -> Result<u64, Error> {
        let meta_key = format!("type:{etype}");
        if let Some(b) = self.storage.meta.get(rtxn, &meta_key)? {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
            let type_id = u32::from_be_bytes(arr);
            crate::storage::ids::get_type_count(&self.storage, rtxn, type_id)
        } else {
            Ok(0)
        }
    }

    pub(super) fn meta_reverse_lookup_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        prefix: &str,
        id: u32,
    ) -> Result<Option<String>, Error> {
        for entry in self.storage.meta.iter(rtxn)? {
            let (key, val) = entry?;
            if let Some(name) = key.strip_prefix(prefix) {
                if val.len() == 4 {
                    let stored = u32::from_be_bytes([val[0], val[1], val[2], val[3]]);
                    if stored == id {
                        return Ok(Some(name.to_owned()));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(super) fn get_active_node_indexes(
        &self,
        rtxn: &crate::storage::RoTxn,
        label_id: LabelId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:node:l:{label_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, &prefix)? {
            let (key, val) = entry?;
            if let Some(prop_str) = key.strip_prefix(&prefix) {
                let prop_key_id: PropKeyId = prop_str
                    .parse()
                    .map_err(|_| Error::Corrupt("prop key id in meta must be integer"))?;
                let flags = val.first().copied().unwrap_or(0x00);
                active.push((prop_key_id, flags));
            }
        }
        Ok(active)
    }

    pub(super) fn get_active_edge_indexes(
        &self,
        rtxn: &crate::storage::RoTxn,
        type_id: TypeId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:edge:t:{type_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, &prefix)? {
            let (key, val) = entry?;
            if let Some(prop_str) = key.strip_prefix(&prefix) {
                let prop_key_id: PropKeyId = prop_str
                    .parse()
                    .map_err(|_| Error::Corrupt("prop key id in meta must be integer"))?;
                let flags = val.first().copied().unwrap_or(0x00);
                active.push((prop_key_id, flags));
            }
        }
        Ok(active)
    }

    pub fn create_node_property_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_node_unique_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_node_required_constraint(
        &self,
        label: &str,
        property: &str,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_index_impl(&mut wtxn, label, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn create_node_index_impl(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        label: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:node:l:{label_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                return Ok(());
            }
        }

        let node_ids = self.nodes_by_label_impl(wtxn, label)?;
        // Dedup on the same notion of value identity the insert-time check uses:
        // an encodable value by its order-preserving encoding (so `30` and `30.0`
        // collide, as they do at insert time), a value too long to index by a
        // tagged copy of its exact JSON form (the `0xFF` tag cannot collide with
        // an encoded value's type tag). Without this the backfill accepted data
        // that a later insert would reject, creating an unenforceable constraint.
        let mut seen_values: ahash::AHashSet<Vec<u8>> = ahash::AHashSet::new();

        for node_id in &node_ids {
            let record = self
                .get_node_impl(wtxn, *node_id)?
                .ok_or(Error::NodeNotFound(*node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            let prop_val = props_json.get(property);

            if flags == 0x02 && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null)) {
                return Err(Error::RequiredConstraintViolation(
                    label.to_string(),
                    property.to_string(),
                ));
            }

            if let Some(val) = prop_val {
                if flags == 0x01 && val != &serde_json::Value::Null {
                    let key = encode_property_value(val).unwrap_or_else(|| {
                        let mut k = vec![0xFF];
                        k.extend_from_slice(val.to_string().as_bytes());
                        k
                    });
                    if !seen_values.insert(key) {
                        return Err(Error::UniqueConstraintViolation(
                            label.to_string(),
                            property.to_string(),
                            val.to_string(),
                        ));
                    }
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[flags])?;

        for node_id in node_ids {
            let record = self
                .get_node_impl(wtxn, node_id)?
                .ok_or(Error::NodeNotFound(node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(val) = props_json.get(property) {
                if let Some(encoded) = encode_property_value(val) {
                    let idx_key = node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                    self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        Ok(())
    }

    pub fn drop_node_property_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_node_unique_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_node_required_constraint(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_index_impl(&mut wtxn, label, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn drop_node_index_impl(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        label: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:node:l:{label_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                self.storage.meta.delete(wtxn, &meta_key)?;

                // `node_prop_idx` doubles as the always-on auto-index for scalar
                // properties (see `index_node_for_label`). Dropping an explicit
                // index or constraint must not remove those baseline entries, or
                // `nodes_by_property` and the Cypher NodeIndexScan would return
                // wrong (empty) results for still-present nodes. Remove only the
                // entries the auto-index never maintains: null-valued entries
                // written by `create_node_index_impl`.
                let mut prefix = Vec::with_capacity(8);
                prefix.extend_from_slice(&label_id.to_be_bytes());
                prefix.extend_from_slice(&prop_key_id.to_be_bytes());

                let mut to_delete = Vec::new();
                for entry in self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)? {
                    let (key, _) = entry?;
                    if key.len() >= prefix.len() + 8 {
                        let encoded_val = &key[prefix.len()..key.len() - 8];
                        if encoded_val == [crate::graph::ENCODED_NULL].as_slice() {
                            to_delete.push(key.to_vec());
                        }
                    }
                }

                for key in to_delete {
                    self.storage.node_prop_idx.delete(wtxn, &key)?;
                }
            }
        }

        Ok(())
    }

    pub fn create_edge_property_index(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_edge_unique_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn create_edge_required_constraint(
        &self,
        etype: &str,
        property: &str,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_edge_index_impl(&mut wtxn, etype, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn create_edge_index_impl(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        etype: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:edge:t:{type_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                return Ok(());
            }
        }

        let edge_ids = self.edges_by_type_impl(wtxn, etype)?;
        // Dedup on the same notion of value identity the insert-time check uses
        // (see `create_node_index_impl`): an encodable value by its
        // order-preserving encoding (so `30` and `30.0` collide, as they do at
        // insert time), a value too long to index by a tagged copy of its exact
        // JSON form. Explicit nulls never conflict, matching the insert path.
        let mut seen_values: ahash::AHashSet<Vec<u8>> = ahash::AHashSet::new();

        for edge_id in &edge_ids {
            let record = self
                .get_edge_impl(wtxn, *edge_id)?
                .ok_or(Error::EdgeNotFound(*edge_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            let prop_val = props_json.get(property);

            if flags == 0x02 && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null)) {
                return Err(Error::RequiredConstraintViolation(
                    etype.to_string(),
                    property.to_string(),
                ));
            }

            if let Some(val) = prop_val {
                if flags == 0x01 && val != &serde_json::Value::Null {
                    let key = encode_property_value(val).unwrap_or_else(|| {
                        let mut k = vec![0xFF];
                        k.extend_from_slice(val.to_string().as_bytes());
                        k
                    });
                    if !seen_values.insert(key) {
                        return Err(Error::UniqueConstraintViolation(
                            etype.to_string(),
                            property.to_string(),
                            val.to_string(),
                        ));
                    }
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[flags])?;

        for edge_id in edge_ids {
            let record = self
                .get_edge_impl(wtxn, edge_id)?
                .ok_or(Error::EdgeNotFound(edge_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(val) = props_json.get(property) {
                if let Some(encoded) = encode_property_value(val) {
                    let idx_key = edge_prop_index_key(type_id, prop_key_id, &encoded, edge_id);
                    self.storage.edge_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        Ok(())
    }

    pub fn drop_edge_property_index(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x00)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_edge_unique_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x01)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn drop_edge_required_constraint(&self, etype: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_edge_index_impl(&mut wtxn, etype, property, 0x02)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn drop_edge_index_impl(
        &self,
        wtxn: &mut crate::storage::RwTxn,
        etype: &str,
        property: &str,
        flags: u8,
    ) -> Result<(), Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("idx_meta:edge:t:{type_id}:p:{prop_key_id}");

        if let Some(existing_val) = self.storage.meta.get(wtxn, &meta_key)? {
            if !existing_val.is_empty() && existing_val[0] == flags {
                self.storage.meta.delete(wtxn, &meta_key)?;

                let mut prefix = Vec::with_capacity(8);
                prefix.extend_from_slice(&type_id.to_be_bytes());
                prefix.extend_from_slice(&prop_key_id.to_be_bytes());

                let mut to_delete = Vec::new();
                for entry in self.storage.edge_prop_idx.prefix_iter(wtxn, &prefix)? {
                    let (key, _) = entry?;
                    to_delete.push(key.to_vec());
                }

                for key in to_delete {
                    self.storage.edge_prop_idx.delete(wtxn, &key)?;
                }
            }
        }

        Ok(())
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_property_impl(&rtxn, label, property, val)
    }

    pub(super) fn nodes_by_property_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        let val = val.into_json();

        // A value that cannot be index-encoded (currently only a string longer
        // than `MAX_INDEXED_STRING_LEN`) is absent from `node_prop_idx`, and its
        // property may have no `prop_key` registered at all, so the index path
        // below would wrongly report no matches. Fall back to a label scan that
        // compares the stored value directly. This must precede the `label_id`
        // and `prop_key_id` lookups, which short-circuit to an empty result.
        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return self.scan_label_for_property_eq(rtxn, label, property, &val),
        };

        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            // A prefix match on the encoded value is not enough: the
            // NUL-terminated string encoding lets a lookup for "a" prefix-match a
            // stored "a\0", so require the value segment to equal `encoded`
            // exactly.
            if let Some(node_id) = exact_prop_index_id(key, &encoded) {
                result.push(node_id);
            }
        }
        Ok(result)
    }

    /// Equality lookup fallback for a node property whose value is not present
    /// in `node_prop_idx` (an over-long string). Scans the label and compares
    /// the stored property value directly, preserving ascending ID order.
    fn scan_label_for_property_eq(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
        property: &str,
        val: &serde_json::Value,
    ) -> Result<Vec<NodeId>, Error> {
        let mut result = Vec::new();
        for id in self.nodes_by_label_impl(rtxn, label)? {
            if let Some(record) = self.get_node_impl(rtxn, id)? {
                let props: serde_json::Value = props::decode(&record.props)?;
                if props.get(property) == Some(val) {
                    result.push(id);
                }
            }
        }
        Ok(result)
    }

    /// Range lookup fallback for a node string property whose values may exceed
    /// the index encoding limit. Scans the label and compares stored string
    /// values directly, in ascending ID order. A non-string bound excludes every
    /// string value (a string never compares to a numeric or boolean bound under
    /// openCypher), so it yields an empty result. Non-string stored values are
    /// skipped for the same reason.
    #[allow(clippy::too_many_arguments)]
    fn scan_label_for_property_str_range(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        min_inclusive: bool,
        max_val: Option<PropValue>,
        max_inclusive: bool,
    ) -> Result<Vec<NodeId>, Error> {
        let lo = match min_val {
            Some(PropValue::Str(s)) => Some(s),
            None => None,
            Some(_) => return Ok(Vec::new()),
        };
        let hi = match max_val {
            Some(PropValue::Str(s)) => Some(s),
            None => None,
            Some(_) => return Ok(Vec::new()),
        };
        let mut result = Vec::new();
        for id in self.nodes_by_label_impl(rtxn, label)? {
            let Some(record) = self.get_node_impl(rtxn, id)? else {
                continue;
            };
            let props: serde_json::Value = props::decode(&record.props)?;
            let Some(serde_json::Value::String(s)) = props.get(property) else {
                continue;
            };
            if !str_in_range(
                s,
                lo.as_deref(),
                min_inclusive,
                hi.as_deref(),
                max_inclusive,
            ) {
                continue;
            }
            result.push(id);
        }
        Ok(result)
    }

    pub fn nodes_by_property_range(
        &self,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        min_inclusive: bool,
        max_val: Option<PropValue>,
        max_inclusive: bool,
    ) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_property_range_impl(
            &rtxn,
            label,
            property,
            min_val,
            min_inclusive,
            max_val,
            max_inclusive,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn nodes_by_property_range_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
        property: &str,
        min_val: Option<PropValue>,
        min_inclusive: bool,
        max_val: Option<PropValue>,
        max_inclusive: bool,
    ) -> Result<Vec<NodeId>, Error> {
        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        // A string value longer than `MAX_INDEXED_STRING_LEN` is absent from
        // `node_prop_idx`, so an index-only scan would silently drop it. A string
        // bound admits only string values (a string never compares to a numeric
        // or boolean bound under openCypher three-valued logic), some of which
        // may be unindexed, so fall back to a full label scan that compares the
        // stored string directly. Numeric and boolean bounds keep the index fast
        // path because those values are always index-encodable. This mirrors the
        // equality fallback in `nodes_by_property_impl`.
        if matches!(min_val, Some(PropValue::Str(_))) || matches!(max_val, Some(PropValue::Str(_)))
        {
            return self.scan_label_for_property_str_range(
                rtxn,
                label,
                property,
                min_val,
                min_inclusive,
                max_val,
                max_inclusive,
            );
        }

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(8);
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());

        let min_encoded = min_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);
        let max_encoded = max_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);

        // A one-sided bound must not admit values of another type family that
        // merely sort past it in the tagged encoding (see `encoded_tag_family`).
        let bound_family = match (&min_encoded, &max_encoded) {
            (Some(lo), Some(hi)) => {
                if encoded_tag_family(lo[0]) != encoded_tag_family(hi[0]) {
                    return Ok(Vec::new());
                }
                Some(encoded_tag_family(lo[0]))
            }
            (Some(e), None) | (None, Some(e)) => Some(encoded_tag_family(e[0])),
            (None, None) => None,
        };

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

                if let Some(family) = bound_family {
                    if val_bytes.is_empty() || encoded_tag_family(val_bytes[0]) != family {
                        continue;
                    }
                }
                if let Some(ref min_enc) = min_encoded {
                    if min_inclusive {
                        if val_bytes < min_enc.as_slice() {
                            continue;
                        }
                    } else if val_bytes <= min_enc.as_slice() {
                        continue;
                    }
                }
                if let Some(ref max_enc) = max_encoded {
                    if max_inclusive {
                        if val_bytes > max_enc.as_slice() {
                            continue;
                        }
                    } else if val_bytes >= max_enc.as_slice() {
                        continue;
                    }
                }

                let mut node_id_bytes = [0u8; 8];
                node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(node_id_bytes));
            }
        }
        Ok(result)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.has_node_property_index_impl(&rtxn, label, property)
    }

    pub(super) fn has_node_property_index_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<bool, Error> {
        let label_key = format!("label:{label}");
        let label_id = match self.storage.meta.get(rtxn, &label_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("label id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(false),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(false),
        };

        // Use a prefix seek on node_prop_idx: if any entry exists for this
        // label+property combination the auto-index (or a user-created index)
        // has data, so the optimizer may use NodeIndexScan.
        let mut prefix = Vec::with_capacity(8);
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        let mut iter = self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)?;
        Ok(iter.next().is_some())
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_property_impl(&rtxn, etype, property, val)
    }

    pub(super) fn edges_by_property_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        let val = val.into_json();

        // See `nodes_by_property_impl`: an unindexable value (long string) is
        // absent from `edge_prop_idx` and may have no registered `prop_key`, so
        // fall back to a type scan before the short-circuiting meta lookups.
        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return self.scan_type_for_property_eq(rtxn, etype, property, &val),
        };

        let type_key = format!("type:{etype}");
        let type_id = match self.storage.meta.get(rtxn, &type_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&type_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            // Require an exact encoded-value match, not just a prefix, so a
            // stored "a\0" is not returned for a lookup of "a" (see
            // `exact_prop_index_id`).
            if let Some(edge_id) = exact_prop_index_id(key, &encoded) {
                result.push(edge_id);
            }
        }
        Ok(result)
    }

    /// Equality lookup fallback for an edge property whose value is not present
    /// in `edge_prop_idx` (an over-long string). Scans the type and compares the
    /// stored property value directly, preserving ascending ID order.
    fn scan_type_for_property_eq(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
        property: &str,
        val: &serde_json::Value,
    ) -> Result<Vec<EdgeId>, Error> {
        let mut result = Vec::new();
        for id in self.edges_by_type_impl(rtxn, etype)? {
            if let Some(record) = self.get_edge_impl(rtxn, id)? {
                let props: serde_json::Value = props::decode(&record.props)?;
                if props.get(property) == Some(val) {
                    result.push(id);
                }
            }
        }
        Ok(result)
    }

    /// Range lookup fallback for an edge string property whose values may exceed
    /// the index encoding limit. Both bounds are inclusive, matching
    /// `edges_by_property_range_impl`. See `scan_label_for_property_str_range`.
    fn scan_type_for_property_str_range(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        let lo = match min_val {
            Some(PropValue::Str(s)) => Some(s),
            None => None,
            Some(_) => return Ok(Vec::new()),
        };
        let hi = match max_val {
            Some(PropValue::Str(s)) => Some(s),
            None => None,
            Some(_) => return Ok(Vec::new()),
        };
        let mut result = Vec::new();
        for id in self.edges_by_type_impl(rtxn, etype)? {
            let Some(record) = self.get_edge_impl(rtxn, id)? else {
                continue;
            };
            let props: serde_json::Value = props::decode(&record.props)?;
            let Some(serde_json::Value::String(s)) = props.get(property) else {
                continue;
            };
            if !str_in_range(s, lo.as_deref(), true, hi.as_deref(), true) {
                continue;
            }
            result.push(id);
        }
        Ok(result)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_property_range_impl(&rtxn, etype, property, min_val, max_val)
    }

    pub(super) fn edges_by_property_range_impl(
        &self,
        rtxn: &crate::storage::RoTxn,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        // See `nodes_by_property_range_impl`: a string value too long to index is
        // absent from `edge_prop_idx`, so a string bound falls back to a full type
        // scan that compares stored strings directly. The bounds are inclusive on
        // both sides, matching this method's index comparison below.
        if matches!(min_val, Some(PropValue::Str(_))) || matches!(max_val, Some(PropValue::Str(_)))
        {
            return self.scan_type_for_property_str_range(rtxn, etype, property, min_val, max_val);
        }

        let type_key = format!("type:{etype}");
        let type_id = match self.storage.meta.get(rtxn, &type_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("type id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let prop_key = format!("prop_key:{property}");
        let prop_key_id = match self.storage.meta.get(rtxn, &prop_key)? {
            Some(b) => {
                let arr: [u8; 4] = b
                    .try_into()
                    .map_err(|_| Error::Corrupt("prop key id must be 4 bytes"))?;
                u32::from_be_bytes(arr)
            }
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(8);
        prefix.extend_from_slice(&type_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());

        let min_encoded = min_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);
        let max_encoded = max_val
            .map(|v| v.into_json())
            .as_ref()
            .and_then(encode_property_value);

        // See `nodes_by_property_range_impl`: a one-sided bound must not admit
        // values of another type family that merely sort past it.
        let bound_family = match (&min_encoded, &max_encoded) {
            (Some(lo), Some(hi)) => {
                if encoded_tag_family(lo[0]) != encoded_tag_family(hi[0]) {
                    return Ok(Vec::new());
                }
                Some(encoded_tag_family(lo[0]))
            }
            (Some(e), None) | (None, Some(e)) => Some(encoded_tag_family(e[0])),
            (None, None) => None,
        };

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

                if let Some(family) = bound_family {
                    if val_bytes.is_empty() || encoded_tag_family(val_bytes[0]) != family {
                        continue;
                    }
                }
                if let Some(ref min_enc) = min_encoded {
                    if val_bytes < min_enc.as_slice() {
                        continue;
                    }
                }
                if let Some(ref max_enc) = max_encoded {
                    if val_bytes > max_enc.as_slice() {
                        continue;
                    }
                }

                let mut edge_id_bytes = [0u8; 8];
                edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(edge_id_bytes));
            }
        }
        Ok(result)
    }

    pub fn list_node_indexes_and_constraints(&self) -> Result<Vec<(String, String, u8)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut result = Vec::new();
        for entry in self.storage.meta.iter(&rtxn)? {
            let (key, val) = entry?;
            if let Some(rest) = key.strip_prefix("idx_meta:node:l:") {
                let parts: Vec<&str> = rest.split(":p:").collect();
                if parts.len() == 2 {
                    if let (Ok(label_id), Ok(prop_key_id)) =
                        (parts[0].parse::<u32>(), parts[1].parse::<u32>())
                    {
                        if let (Some(label_name), Some(prop_name)) = (
                            self.label_name_impl(&rtxn, label_id)?,
                            crate::storage::ids::get_prop_key_name(
                                &self.storage,
                                &rtxn,
                                prop_key_id,
                            )?,
                        ) {
                            let flags = val.first().copied().unwrap_or(0x00);
                            result.push((label_name, prop_name, flags));
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn list_edge_indexes_and_constraints(&self) -> Result<Vec<(String, String, u8)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut result = Vec::new();
        for entry in self.storage.meta.iter(&rtxn)? {
            let (key, val) = entry?;
            if let Some(rest) = key.strip_prefix("idx_meta:edge:t:") {
                let parts: Vec<&str> = rest.split(":p:").collect();
                if parts.len() == 2 {
                    if let (Ok(type_id), Ok(prop_key_id)) =
                        (parts[0].parse::<u32>(), parts[1].parse::<u32>())
                    {
                        if let (Some(type_name), Some(prop_name)) = (
                            self.type_name_impl(&rtxn, type_id)?,
                            crate::storage::ids::get_prop_key_name(
                                &self.storage,
                                &rtxn,
                                prop_key_id,
                            )?,
                        ) {
                            let flags = val.first().copied().unwrap_or(0x00);
                            result.push((type_name, prop_name, flags));
                        }
                    }
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// The cached label scan must serve repeated reads of one label without
    /// rescanning (the same shared vector back), and any committed write must
    /// invalidate it, a label add and a label remove included, so a scan never
    /// misses a member or reports one that is gone.
    #[test]
    fn label_scan_cache_serves_repeats_and_tracks_writes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Other", &json!({})).unwrap();

        let first = g.nodes_by_label_arc("Person").unwrap();
        assert_eq!(*first, vec![a, b]);
        let second = g.nodes_by_label_arc("Person").unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "a repeat with no intervening write must serve the cached scan"
        );
        // The plain form agrees with the cached one.
        assert_eq!(g.nodes_by_label("Person").unwrap(), *first);

        // A label add lands.
        g.add_label(c, "Person").unwrap();
        assert_eq!(*g.nodes_by_label_arc("Person").unwrap(), vec![a, b, c]);

        // A label remove lands.
        g.remove_label(c, "Person").unwrap();
        assert_eq!(*g.nodes_by_label_arc("Person").unwrap(), vec![a, b]);

        // A node delete lands.
        g.delete_node(b).unwrap();
        assert_eq!(*g.nodes_by_label_arc("Person").unwrap(), vec![a]);

        // An unknown label is empty, and cached emptiness also tracks writes.
        assert!(g.nodes_by_label_arc("Nope").unwrap().is_empty());
        let d = g.add_node("Nope", &json!({})).unwrap();
        assert_eq!(*g.nodes_by_label_arc("Nope").unwrap(), vec![d]);
    }

    /// A string equality lookup must match the exact value, not merely a prefix.
    /// The NUL-terminated string encoding plus leading-zero ids would otherwise
    /// let a lookup for "a" also return a node whose value is "a\0".
    #[test]
    fn string_equality_lookup_is_exact_across_nul_boundary() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("L", &json!({ "k": "a" })).unwrap();
        let a_nul = g.add_node("L", &json!({ "k": "a\u{0}" })).unwrap();

        assert_eq!(
            g.nodes_by_property("L", "k", PropValue::Str("a".to_string()))
                .unwrap(),
            vec![a],
            "lookup of \"a\" must not return \"a\\0\""
        );
        assert_eq!(
            g.nodes_by_property("L", "k", PropValue::Str("a\u{0}".to_string()))
                .unwrap(),
            vec![a_nul],
            "lookup of \"a\\0\" must not return \"a\""
        );
    }

    /// The edge equality lookup has the same exactness requirement. Edge
    /// properties are indexed only under an explicit edge index, so create one
    /// before inserting the edges.
    #[test]
    fn edge_string_equality_lookup_is_exact_across_nul_boundary() {
        let (_dir, g) = open_tmp();
        g.create_edge_property_index("R", "k").unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let e = g.add_edge(a, b, "R", &json!({ "k": "a" })).unwrap();
        let _e_nul = g.add_edge(a, b, "R", &json!({ "k": "a\u{0}" })).unwrap();

        assert_eq!(
            g.edges_by_property("R", "k", PropValue::Str("a".to_string()))
                .unwrap(),
            vec![e],
            "edge lookup of \"a\" must not return \"a\\0\""
        );
    }

    /// A unique constraint treats numerically equal values (`30` and `30.0`) as
    /// duplicates consistently at both constraint-creation (backfill) and
    /// insert time, matching openCypher value equality.
    #[test]
    fn unique_constraint_treats_int_and_float_as_equal() {
        // Insert-time: constraint first, then a numerically-equal insert fails.
        let (_dir, g) = open_tmp();
        g.create_node_unique_constraint("L", "k").unwrap();
        g.add_node("L", &json!({ "k": 30 })).unwrap();
        assert!(
            g.add_node("L", &json!({ "k": 30.0 })).is_err(),
            "30.0 duplicates the existing 30 under numeric equality"
        );

        // Backfill: creating the constraint over pre-existing {30, 30.0} fails,
        // rather than succeeding into a constraint the insert path would reject.
        let (_dir2, g2) = open_tmp();
        g2.add_node("L", &json!({ "k": 30 })).unwrap();
        g2.add_node("L", &json!({ "k": 30.0 })).unwrap();
        assert!(g2.create_node_unique_constraint("L", "k").is_err());
    }

    /// A unique constraint is enforced for string values too long to index, at
    /// both insert time and constraint creation.
    #[test]
    fn unique_constraint_enforced_for_over_long_strings() {
        let long_a = format!("A{}", "x".repeat(600));
        let long_b = format!("B{}", "y".repeat(600));

        // Insert-time: the second identical long value is rejected; a different
        // long value is accepted.
        let (_dir, g) = open_tmp();
        g.create_node_unique_constraint("L", "k").unwrap();
        g.add_node("L", &json!({ "k": long_a })).unwrap();
        assert!(
            g.add_node("L", &json!({ "k": long_a })).is_err(),
            "a duplicate over-long value must be rejected"
        );
        assert!(g.add_node("L", &json!({ "k": long_b })).is_ok());

        // Backfill: pre-existing duplicate long values block constraint creation.
        let (_dir2, g2) = open_tmp();
        g2.add_node("L", &json!({ "k": long_a })).unwrap();
        g2.add_node("L", &json!({ "k": long_a })).unwrap();
        assert!(g2.create_node_unique_constraint("L", "k").is_err());
    }

    /// The edge unique-constraint backfill must use the same value identity as
    /// the insert-time check: `30` and `30.0` are duplicates under numeric
    /// equality, at constraint creation as well as at insert time.
    #[test]
    fn edge_unique_constraint_treats_int_and_float_as_equal() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({ "k": 30 })).unwrap();
        g.add_edge(a, b, "R", &json!({ "k": 30.0 })).unwrap();
        assert!(
            g.create_edge_unique_constraint("R", "k").is_err(),
            "30 and 30.0 duplicate each other under numeric equality"
        );
    }

    /// Explicit null values never conflict under a unique constraint, so the
    /// edge backfill must not reject a pre-existing pair of nulls that the
    /// insert-time check would have allowed.
    #[test]
    fn edge_unique_constraint_backfill_allows_multiple_nulls() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({ "k": null })).unwrap();
        g.add_edge(a, b, "R", &json!({ "k": null })).unwrap();
        assert!(
            g.create_edge_unique_constraint("R", "k").is_ok(),
            "explicit nulls must not count as duplicates"
        );
    }

    /// An edge unique constraint is enforced for string values too long to
    /// index, falling back to a type scan, mirroring the node path.
    #[test]
    fn edge_unique_constraint_enforced_for_over_long_strings() {
        let long_a = format!("A{}", "x".repeat(600));
        let long_b = format!("B{}", "y".repeat(600));

        let (_dir, g) = open_tmp();
        g.create_edge_unique_constraint("R", "k").unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({ "k": long_a.clone() }))
            .unwrap();
        assert!(
            g.add_edge(a, b, "R", &json!({ "k": long_a })).is_err(),
            "a duplicate over-long value must be rejected"
        );
        assert!(g.add_edge(a, b, "R", &json!({ "k": long_b })).is_ok());
    }

    /// A one-sided numeric range must not return values of other JSON types
    /// that happen to sort past the bound in the tagged encoding: a string is
    /// never comparable to a numeric bound under openCypher, and neither is a
    /// boolean or a null.
    #[test]
    fn numeric_range_excludes_other_value_types() {
        let (_dir, g) = open_tmp();
        let n_int = g.add_node("L", &json!({ "age": 30 })).unwrap();
        g.add_node("L", &json!({ "age": "old" })).unwrap();
        g.add_node("L", &json!({ "age": true })).unwrap();

        let lo = g
            .nodes_by_property_range("L", "age", Some(PropValue::Int(20)), true, None, false)
            .unwrap();
        assert_eq!(
            lo,
            vec![n_int],
            "a lower-bound-only numeric range must exclude string values"
        );

        let hi = g
            .nodes_by_property_range("L", "age", None, false, Some(PropValue::Int(40)), true)
            .unwrap();
        assert_eq!(
            hi,
            vec![n_int],
            "an upper-bound-only numeric range must exclude boolean values"
        );
    }

    /// The edge range scan has the same type-family requirement as the node
    /// range scan.
    #[test]
    fn edge_numeric_range_excludes_other_value_types() {
        let (_dir, g) = open_tmp();
        g.create_edge_property_index("R", "w").unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let e_int = g.add_edge(a, b, "R", &json!({ "w": 5 })).unwrap();
        g.add_edge(a, b, "R", &json!({ "w": "heavy" })).unwrap();
        g.add_edge(a, b, "R", &json!({ "w": true })).unwrap();

        let lo = g
            .edges_by_property_range("R", "w", Some(PropValue::Int(1)), None)
            .unwrap();
        assert_eq!(
            lo,
            vec![e_int],
            "a lower-bound-only numeric range must exclude string values"
        );

        let hi = g
            .edges_by_property_range("R", "w", None, Some(PropValue::Int(10)))
            .unwrap();
        assert_eq!(
            hi,
            vec![e_int],
            "an upper-bound-only numeric range must exclude boolean values"
        );
    }

    /// Distinct string values that share a NUL-boundary relationship must not
    /// trigger a spurious unique-constraint violation.
    #[test]
    fn unique_constraint_distinguishes_nul_boundary_strings() {
        let (_dir, g) = open_tmp();
        g.create_node_unique_constraint("L", "k").unwrap();
        g.add_node("L", &json!({ "k": "a" })).unwrap();
        // "a\0" is a distinct value, so this insert must succeed.
        let res = g.add_node("L", &json!({ "k": "a\u{0}" }));
        assert!(
            res.is_ok(),
            "\"a\\0\" is distinct from \"a\" and must not violate unique(L.k)"
        );
        // A genuine duplicate still fails.
        assert!(
            g.add_node("L", &json!({ "k": "a" })).is_err(),
            "a true duplicate must still be rejected"
        );
    }

    /// A string property value too long to index (over `MAX_INDEXED_STRING_LEN`)
    /// must still be returned by a range scan: the range path falls back to a
    /// label scan for string bounds, mirroring the equality fallback.
    #[test]
    fn string_range_returns_over_long_value() {
        let (_dir, g) = open_tmp();
        let short = g.add_node("L", &json!({ "k": "Nectarine" })).unwrap();
        let long_val = format!("Z{}", "x".repeat(600));
        let long = g.add_node("L", &json!({ "k": long_val })).unwrap();
        g.add_node("L", &json!({ "k": "Apple" })).unwrap();

        // k > "M" (exclusive lower bound) includes "Nectarine" and the long "Z...".
        let mut hits = g
            .nodes_by_property_range(
                "L",
                "k",
                Some(PropValue::Str("M".into())),
                false,
                None,
                false,
            )
            .unwrap();
        hits.sort_unstable();
        let mut expected = vec![short, long];
        expected.sort_unstable();
        assert_eq!(hits, expected, "the over-long value must not be dropped");
    }

    /// A string range must place a NUL-suffixed value on the correct side of the
    /// bound: Cypher orders "a" < "a\0" < "ab" (a prefix is smaller), and the
    /// order-preserving encoding reproduces that, so ranges stay exact across the
    /// NUL boundary without any re-verification.
    #[test]
    fn string_range_respects_nul_boundary() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("L", &json!({ "k": "a" })).unwrap();
        let a_nul = g.add_node("L", &json!({ "k": "a\u{0}" })).unwrap();
        let ab = g.add_node("L", &json!({ "k": "ab" })).unwrap();

        // k >= "a\0" excludes "a", includes "a\0" and "ab".
        let mut hits = g
            .nodes_by_property_range(
                "L",
                "k",
                Some(PropValue::Str("a\u{0}".into())),
                true,
                None,
                false,
            )
            .unwrap();
        hits.sort_unstable();
        assert_eq!(hits, vec![a_nul, ab]);

        // k <= "a" includes only "a".
        let hits = g
            .nodes_by_property_range(
                "L",
                "k",
                None,
                false,
                Some(PropValue::Str("a".into())),
                true,
            )
            .unwrap();
        assert_eq!(hits, vec![a]);

        // k > "a" (exclusive) excludes "a", includes "a\0" and "ab".
        let mut hits = g
            .nodes_by_property_range(
                "L",
                "k",
                Some(PropValue::Str("a".into())),
                false,
                None,
                false,
            )
            .unwrap();
        hits.sort_unstable();
        assert_eq!(hits, vec![a_nul, ab]);
    }

    /// Dropping an explicit property index must leave the always-on auto-index
    /// intact so `nodes_by_property` still finds existing nodes.
    #[test]
    fn drop_index_preserves_auto_index() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({"age": 30})).unwrap();

        g.create_node_property_index("Person", "age").unwrap();
        g.drop_node_property_index("Person", "age").unwrap();

        assert_eq!(
            g.nodes_by_property("Person", "age", PropValue::Int(30))
                .unwrap(),
            vec![id],
            "auto-index entries must survive dropping the explicit index"
        );
    }

    /// A string property too long to fit an LMDB index key is left unindexed,
    /// so an equality lookup must fall back to a label scan rather than wrongly
    /// reporting no matches.
    #[test]
    fn nodes_by_property_finds_unindexed_long_string() {
        let (_dir, g) = open_tmp();
        let long = "word ".repeat(4000); // ~20 KB, well over the index key bound
        let id = g
            .add_node("Post", &json!({ "body": long.clone() }))
            .unwrap();
        // A different long body must not match.
        g.add_node("Post", &json!({ "body": "other ".repeat(4000) }))
            .unwrap();

        assert_eq!(
            g.nodes_by_property("Post", "body", PropValue::Str(long))
                .unwrap(),
            vec![id],
            "equality lookup on an unindexed long string must scan and match"
        );
    }

    /// Dropping a unique constraint must keep property lookups working and stop
    /// enforcing uniqueness.
    #[test]
    fn drop_unique_constraint_preserves_lookups() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("User", &json!({"email": "a@b.c"})).unwrap();

        g.create_node_unique_constraint("User", "email").unwrap();
        g.drop_node_unique_constraint("User", "email").unwrap();

        assert_eq!(
            g.nodes_by_property("User", "email", PropValue::Str("a@b.c".into()))
                .unwrap(),
            vec![id]
        );

        // Uniqueness is no longer enforced; a duplicate value is accepted and
        // both nodes are findable.
        let id2 = g.add_node("User", &json!({"email": "a@b.c"})).unwrap();
        let mut hits = g
            .nodes_by_property("User", "email", PropValue::Str("a@b.c".into()))
            .unwrap();
        hits.sort();
        let mut expected = vec![id, id2];
        expected.sort();
        assert_eq!(hits, expected);
    }

    /// Two nodes with integer properties beyond 2^53 must be distinguishable by
    /// `nodes_by_property`; the values previously collapsed through `f64`.
    #[test]
    fn large_integer_property_no_false_match() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("Item", &json!({"sid": 9_007_199_254_740_992_i64}))
            .unwrap();
        let b = g
            .add_node("Item", &json!({"sid": 9_007_199_254_740_993_i64}))
            .unwrap();

        assert_eq!(
            g.nodes_by_property("Item", "sid", PropValue::Int(9_007_199_254_740_992))
                .unwrap(),
            vec![a]
        );
        assert_eq!(
            g.nodes_by_property("Item", "sid", PropValue::Int(9_007_199_254_740_993))
                .unwrap(),
            vec![b]
        );
    }

    /// An integer-valued property must still be findable when queried with the
    /// equal float, matching Cypher's `30 = 30.0` semantics.
    #[test]
    fn integer_property_matches_float_query() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({"age": 30})).unwrap();
        assert_eq!(
            g.nodes_by_property("Person", "age", PropValue::Float(30.0))
                .unwrap(),
            vec![id]
        );
    }

    /// `node_count_hint` is the node-id high-water mark: it tracks allocations
    /// and must not decrease when a node is deleted.
    #[test]
    fn node_count_hint_is_high_water_mark() {
        let (_dir, g) = open_tmp();
        assert_eq!(g.node_count_hint().unwrap(), 0);

        let a = g.add_node("N", &()).unwrap();
        g.add_node("N", &()).unwrap();
        assert_eq!(g.node_count_hint().unwrap(), 2);

        g.delete_node(a).unwrap();
        assert_eq!(g.node_count_hint().unwrap(), 2);
    }

    /// An edge property index created before any edges exist must be populated
    /// by `add_edge`, and one created afterwards must backfill existing edges.
    #[test]
    fn edge_property_index_lookup() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();

        // Backfill path: the edge exists before the index.
        let e1 = g.add_edge(a, b, "ROAD", &json!({"cost": 5})).unwrap();
        g.create_edge_property_index("ROAD", "cost").unwrap();

        // Insert path: the edge arrives after the index.
        let e2 = g.add_edge(b, a, "ROAD", &json!({"cost": 7})).unwrap();

        assert_eq!(
            g.edges_by_property("ROAD", "cost", PropValue::Int(5))
                .unwrap(),
            vec![e1]
        );
        assert_eq!(
            g.edges_by_property("ROAD", "cost", PropValue::Int(7))
                .unwrap(),
            vec![e2]
        );
        assert_eq!(
            g.edges_by_property_range(
                "ROAD",
                "cost",
                Some(PropValue::Int(5)),
                Some(PropValue::Int(7)),
            )
            .unwrap(),
            vec![e1, e2]
        );
    }

    #[test]
    fn drop_edge_property_index_removes_entries() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_property_index("ROAD", "cost").unwrap();
        g.add_edge(a, b, "ROAD", &json!({"cost": 5})).unwrap();

        g.drop_edge_property_index("ROAD", "cost").unwrap();
        assert_eq!(
            g.edges_by_property("ROAD", "cost", PropValue::Int(5))
                .unwrap(),
            Vec::<EdgeId>::new()
        );
    }

    #[test]
    fn edge_unique_constraint_rejects_duplicate_insert() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_unique_constraint("ROAD", "toll_id").unwrap();

        g.add_edge(a, b, "ROAD", &json!({"toll_id": 1})).unwrap();
        let err = g
            .add_edge(b, a, "ROAD", &json!({"toll_id": 1}))
            .unwrap_err();
        assert!(matches!(err, Error::UniqueConstraintViolation(..)));
    }

    #[test]
    fn edge_unique_constraint_rejects_existing_duplicates() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.add_edge(a, b, "ROAD", &json!({"toll_id": 1})).unwrap();
        g.add_edge(b, a, "ROAD", &json!({"toll_id": 1})).unwrap();

        let err = g
            .create_edge_unique_constraint("ROAD", "toll_id")
            .unwrap_err();
        assert!(matches!(err, Error::UniqueConstraintViolation(..)));
    }

    #[test]
    fn edge_required_constraint_rejects_missing_property() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_required_constraint("ROAD", "cost").unwrap();

        let err = g.add_edge(a, b, "ROAD", &json!({})).unwrap_err();
        assert!(matches!(err, Error::RequiredConstraintViolation(..)));

        // Creating the constraint must also reject pre-existing violations.
        g.add_edge(a, b, "RAIL", &json!({})).unwrap();
        let err = g
            .create_edge_required_constraint("RAIL", "cost")
            .unwrap_err();
        assert!(matches!(err, Error::RequiredConstraintViolation(..)));
    }

    /// `update_edge` must re-index the edge under its new property values:
    /// the old index entry disappears and the new one is findable.
    #[test]
    fn update_edge_reindexes_edge_properties() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_property_index("ROAD", "cost").unwrap();
        let eid = g.add_edge(a, b, "ROAD", &json!({"cost": 5})).unwrap();

        g.update_edge(eid, &json!({"cost": 7})).unwrap();

        assert_eq!(
            g.edges_by_property("ROAD", "cost", PropValue::Int(5))
                .unwrap(),
            Vec::<EdgeId>::new(),
            "stale index entry must be removed"
        );
        assert_eq!(
            g.edges_by_property("ROAD", "cost", PropValue::Int(7))
                .unwrap(),
            vec![eid],
            "new value must be indexed"
        );
    }

    #[test]
    fn update_edge_enforces_unique_constraint() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_unique_constraint("ROAD", "toll_id").unwrap();
        g.add_edge(a, b, "ROAD", &json!({"toll_id": 1})).unwrap();
        let e2 = g.add_edge(b, a, "ROAD", &json!({"toll_id": 2})).unwrap();

        let err = g.update_edge(e2, &json!({"toll_id": 1})).unwrap_err();
        assert!(matches!(err, Error::UniqueConstraintViolation(..)));

        // Updating an edge to keep its own value must not self-conflict.
        g.update_edge(e2, &json!({"toll_id": 2})).unwrap();
    }

    #[test]
    fn update_edge_enforces_required_constraint() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.create_edge_required_constraint("ROAD", "cost").unwrap();
        let eid = g.add_edge(a, b, "ROAD", &json!({"cost": 5})).unwrap();

        let err = g.update_edge(eid, &json!({})).unwrap_err();
        assert!(matches!(err, Error::RequiredConstraintViolation(..)));
    }
}

#[cfg(test)]
mod label_filter_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn label_filter_keeps_only_labeled_nodes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("City", &json!({})).unwrap();
        let c = g.add_node_multi(&["City", "Person"], &json!({})).unwrap();

        let filtered = g.label_filter(&[a, b, c], "Person").unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains(&a));
        assert!(filtered.contains(&c));
    }

    #[test]
    fn label_filter_unknown_label_is_empty() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        assert!(g.label_filter(&[a], "Ghost").unwrap().is_empty());
    }

    #[test]
    fn label_filter_sees_committed_writes_immediately() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        g.add_label(a, "Admin").unwrap();
        assert_eq!(g.label_filter(&[a], "Admin").unwrap(), vec![a]);
        g.remove_label(a, "Admin").unwrap();
        assert!(g.label_filter(&[a], "Admin").unwrap().is_empty());
    }
}
