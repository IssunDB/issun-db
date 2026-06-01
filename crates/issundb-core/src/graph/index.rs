use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Secondary index queries
    // ------------------------------------------------------------------

    /// Returns all node IDs with the given label, in ascending ID order.
    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.nodes_by_label_impl(&rtxn, label)
    }

    pub(super) fn nodes_by_label_impl(
        &self,
        rtxn: &heed::RoTxn,
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

    /// Returns all edge IDs with the given type, in ascending ID order.
    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.edges_by_type_impl(&rtxn, etype)
    }

    pub(super) fn edges_by_type_impl(
        &self,
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
        id: TypeId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "type:", id)
    }

    pub(super) fn prop_key_name_impl(
        &self,
        rtxn: &heed::RoTxn,
        id: PropKeyId,
    ) -> Result<Option<String>, Error> {
        self.meta_reverse_lookup_impl(rtxn, "prop_key:", id)
    }

    pub(super) fn delete_edge_index_entries(
        &self,
        wtxn: &mut heed::RwTxn,
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

    pub(super) fn node_count_by_label_impl(
        &self,
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
        label_id: LabelId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:node:l:{label_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.iter(rtxn)? {
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
        rtxn: &heed::RoTxn,
        type_id: TypeId,
    ) -> Result<Vec<(PropKeyId, u8)>, Error> {
        let prefix = format!("idx_meta:edge:t:{type_id}:p:");
        let mut active = Vec::new();
        for entry in self.storage.meta.iter(rtxn)? {
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
        wtxn: &mut heed::RwTxn,
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
        let mut seen_values = ahash::AHashSet::new();

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
                if flags == 0x01 && !seen_values.insert(val.clone()) {
                    return Err(Error::UniqueConstraintViolation(
                        label.to_string(),
                        property.to_string(),
                        val.to_string(),
                    ));
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
        wtxn: &mut heed::RwTxn,
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
        wtxn: &mut heed::RwTxn,
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
        let mut seen_values = ahash::AHashSet::new();

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
                if flags == 0x01 && !seen_values.insert(val.clone()) {
                    return Err(Error::UniqueConstraintViolation(
                        etype.to_string(),
                        property.to_string(),
                        val.to_string(),
                    ));
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
        wtxn: &mut heed::RwTxn,
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
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        let val = val.into_json();
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

        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&label_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= 8 {
                let mut node_id_bytes = [0u8; 8];
                node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(node_id_bytes));
            }
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
        rtxn: &heed::RoTxn,
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

        let mut result = Vec::new();
        for entry in self.storage.node_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

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
        rtxn: &heed::RoTxn,
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
        rtxn: &heed::RoTxn,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        let val = val.into_json();
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

        let encoded = match encode_property_value(&val) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
        prefix.extend_from_slice(&type_id.to_be_bytes());
        prefix.extend_from_slice(&prop_key_id.to_be_bytes());
        prefix.extend_from_slice(&encoded);

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= 8 {
                let mut edge_id_bytes = [0u8; 8];
                edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                result.push(u64::from_be_bytes(edge_id_bytes));
            }
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
        rtxn: &heed::RoTxn,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
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

        let mut result = Vec::new();
        for entry in self.storage.edge_prop_idx.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if key.len() >= prefix.len() + 8 {
                let val_bytes = &key[prefix.len()..key.len() - 8];

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
}
