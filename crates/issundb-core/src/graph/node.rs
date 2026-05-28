use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Nodes
    // ------------------------------------------------------------------

    /// Insert a node with a string label and msgpack-serializable properties.
    #[instrument(skip(self, props), fields(label = %label))]
    pub fn add_node(&self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let id = self.add_node_impl(&mut wtxn, label, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(id)
    }

    pub(super) fn add_node_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        props: &impl Serialize,
    ) -> Result<NodeId, Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let node_id = alloc_node_id(&self.storage, wtxn)?;
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;

        // Validate constraints and populate indexes
        let active_indexes = self.get_active_node_indexes(wtxn, label_id)?;
        if !active_indexes.is_empty() {
            for (prop_key_id, flags) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    let prop_val = props_json.get(&prop_name);

                    // 1. Required constraint check
                    if flags == 0x02
                        && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null))
                    {
                        return Err(Error::RequiredConstraintViolation(
                            label.to_string(),
                            prop_name.to_string(),
                        ));
                    }

                    if let Some(val) = prop_val {
                        if val != &serde_json::Value::Null {
                            if let Some(encoded) = encode_property_value(val) {
                                // 2. Unique constraint check
                                if flags == 0x01 {
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
                                    prefix.extend_from_slice(&label_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded);

                                    for entry in
                                        self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut node_id_bytes = [0u8; 8];
                                            node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_node_id = u64::from_be_bytes(node_id_bytes);
                                            if found_node_id != node_id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    label.to_string(),
                                                    prop_name.to_string(),
                                                    val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }

                                // 3. Write index entry
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                                self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        let record = NodeRecord {
            label: label_id,
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &node_id, &props::encode(&record)?)?;
        self.storage
            .label_idx
            .put(wtxn, &composite_key(label_id, node_id), &())?;

        // FTS indexing hook
        self.index_node_fts(wtxn, node_id, label_id, &props_json)?;

        adjust_label_count(&self.storage, wtxn, label_id, 1)?;

        // Auto-index: write every scalar property to node_prop_idx so the
        // Cypher optimizer can use NodeIndexScan without a prior CREATE INDEX.
        if let Some(obj) = props_json.as_object() {
            for (prop_name, val) in obj {
                if val.is_null() {
                    continue;
                }
                if let Some(encoded) = encode_property_value(val) {
                    let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, prop_name)?;
                    let idx_key = node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                    self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        Ok(node_id)
    }

    /// Fetch a node record by id.
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.get_node_impl(&rtxn, id)
    }

    pub(super) fn get_node_impl(
        &self,
        txn: &heed::RoTxn,
        id: NodeId,
    ) -> Result<Option<NodeRecord>, Error> {
        match self.storage.nodes.get(txn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Update the properties of an existing node. The node's label is unchanged.
    ///
    /// # Deadlock warning
    ///
    /// Do not call this method from inside a [`Graph::update`] closure. Use
    /// [`WriteTxn::update_node`] inside the closure instead.
    pub fn update_node(&self, id: NodeId, props: &impl Serialize) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.update_node_impl(&mut wtxn, id, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn update_node_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: NodeId,
        props: &impl Serialize,
    ) -> Result<(), Error> {
        let old_rec: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Err(Error::NodeNotFound(id)),
        };

        let label_id = old_rec.label;
        let label_name = self
            .label_name_impl(wtxn, label_id)?
            .unwrap_or_else(|| label_id.to_string());
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;
        let old_props_json: serde_json::Value = props::decode(&old_rec.props)?;

        // Same label: delete old, validate and write new property index entries.
        let active = self.get_active_node_indexes(wtxn, label_id)?;
        for (prop_key_id, flags) in active {
            if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                let old_val = old_props_json.get(&prop_name);
                let new_val = props_json.get(&prop_name);

                if old_val != new_val {
                    // Property value changed.
                    if flags == 0x02
                        && (new_val.is_none() || new_val == Some(&serde_json::Value::Null))
                    {
                        return Err(Error::RequiredConstraintViolation(
                            label_name.clone(),
                            prop_name.to_string(),
                        ));
                    }

                    // 1. Delete old
                    if let Some(o_val) = old_val {
                        if o_val != &serde_json::Value::Null {
                            if let Some(encoded_old) = encode_property_value(o_val) {
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded_old, id);
                                self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                            }
                        }
                    }

                    // 2. Validate and write new
                    if let Some(n_val) = new_val {
                        if n_val != &serde_json::Value::Null {
                            if let Some(encoded_new) = encode_property_value(n_val) {
                                if flags == 0x01 {
                                    // Unique check
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded_new.len());
                                    prefix.extend_from_slice(&label_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded_new);
                                    for entry in
                                        self.storage.node_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut node_id_bytes = [0u8; 8];
                                            node_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_node_id = u64::from_be_bytes(node_id_bytes);
                                            if found_node_id != id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    label_name.clone(),
                                                    prop_name.to_string(),
                                                    n_val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                let idx_key =
                                    node_prop_index_key(label_id, prop_key_id, &encoded_new, id);
                                self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        self.update_node_fts(
            wtxn,
            id,
            old_rec.label,
            label_id,
            &old_props_json,
            &props_json,
        )?;

        // Auto-index: delete old scalar entries and write new ones.
        if let Some(obj) = old_props_json.as_object() {
            for (prop_name, val) in obj {
                if val.is_null() {
                    continue;
                }
                if let Some(encoded) = encode_property_value(val) {
                    if let Some(pkid) = get_prop_key(&self.storage, &*wtxn, prop_name)? {
                        let idx_key = node_prop_index_key(label_id, pkid, &encoded, id);
                        self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                    }
                }
            }
        }
        if let Some(obj) = props_json.as_object() {
            for (prop_name, val) in obj {
                if val.is_null() {
                    continue;
                }
                if let Some(encoded) = encode_property_value(val) {
                    let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, prop_name)?;
                    let idx_key = node_prop_index_key(label_id, prop_key_id, &encoded, id);
                    self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                }
            }
        }

        let record = NodeRecord {
            label: label_id,
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &id, &props::encode(&record)?)?;
        Ok(())
    }

    /// Delete a node.
    #[instrument(skip(self))]
    pub fn delete_node(&self, id: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_node_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn delete_node_impl(&self, wtxn: &mut heed::RwTxn, id: NodeId) -> Result<(), Error> {
        let record: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Ok(()),
        };

        let props_json: serde_json::Value = props::decode(&record.props)?;

        // FTS indexing deletion hook
        self.delete_node_fts(wtxn, id, record.label, &props_json)?;

        // 0. Delete from node property index
        let active = self.get_active_node_indexes(wtxn, record.label)?;
        if !active.is_empty() {
            for (prop_key_id, _) in active {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    if let Some(val) = props_json.get(&prop_name) {
                        if let Some(encoded) = encode_property_value(val) {
                            let idx_key =
                                node_prop_index_key(record.label, prop_key_id, &encoded, id);
                            self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                        }
                    }
                }
            }
        }

        // Auto-index cleanup: delete all scalar property entries for this node.
        if let Some(obj) = props_json.as_object() {
            for (prop_name, val) in obj {
                if val.is_null() {
                    continue;
                }
                if let Some(encoded) = encode_property_value(val) {
                    if let Some(pkid) = get_prop_key(&self.storage, &*wtxn, prop_name)? {
                        let idx_key = node_prop_index_key(record.label, pkid, &encoded, id);
                        self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                    }
                }
            }
        }

        // 1. Delete from label index
        self.storage
            .label_idx
            .delete(wtxn, &composite_key(record.label, id))?;

        adjust_label_count(&self.storage, wtxn, record.label, -1)?;

        // 2. Process all outgoing neighbors (out_adj)
        let mut out_edges = Vec::new();
        if let Some(iter) = self.storage.out_adj.get_duplicates(wtxn, &id)? {
            for result in iter {
                let (_, bytes) = result?;
                let entry = AdjEntry::read_from_bytes(bytes)
                    .ok()
                    .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
                out_edges.push(entry);
            }
        }

        for entry in out_edges {
            let edge_id = entry.edge_id;
            let other = entry.other;
            if let Some(edge_rec) = self.get_edge_impl(wtxn, edge_id)? {
                self.delete_edge_index_entries(wtxn, edge_id, &edge_rec)?;
            }
            // Delete edge and type index
            self.storage.edges.delete(wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, wtxn, entry.edge_type, -1)?;

            // Delete the corresponding in_adj entry on the neighbor
            let in_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .in_adj
                .delete_one_duplicate(wtxn, &other, in_entry.as_bytes())?;
        }

        // 3. Process all incoming neighbors (in_adj)
        let mut in_edges = Vec::new();
        if let Some(iter) = self.storage.in_adj.get_duplicates(wtxn, &id)? {
            for result in iter {
                let (_, bytes) = result?;
                let entry = AdjEntry::read_from_bytes(bytes)
                    .ok()
                    .ok_or(Error::Corrupt("AdjEntry value is not exactly 20 bytes"))?;
                in_edges.push(entry);
            }
        }

        for entry in in_edges {
            let edge_id = entry.edge_id;
            let other = entry.other;
            if let Some(edge_rec) = self.get_edge_impl(wtxn, edge_id)? {
                self.delete_edge_index_entries(wtxn, edge_id, &edge_rec)?;
            }
            // Delete edge and type index
            self.storage.edges.delete(wtxn, &edge_id)?;
            self.storage
                .type_idx
                .delete(wtxn, &composite_key(entry.edge_type, edge_id))?;

            adjust_type_count(&self.storage, wtxn, entry.edge_type, -1)?;

            // Delete the corresponding out_adj entry on the neighbor
            let out_entry = AdjEntry {
                edge_type: entry.edge_type,
                other: id,
                edge_id,
            };
            self.storage
                .out_adj
                .delete_one_duplicate(wtxn, &other, out_entry.as_bytes())?;
        }

        // 4. Delete the adjacency list keys themselves
        self.storage.out_adj.delete(wtxn, &id)?;
        self.storage.in_adj.delete(wtxn, &id)?;

        // 5. Delete persisted vector bytes
        self.storage.vectors.delete(wtxn, &id)?;

        // 6. Delete from primary nodes database
        self.storage.nodes.delete(wtxn, &id)?;

        Ok(())
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

    /// Verify that scalar properties are automatically indexed on insert,
    /// without a prior `create_node_property_index` call.
    #[test]
    fn auto_index_on_insert() {
        let (_dir, g) = open_tmp();

        let node_id = g
            .add_node("Person", &json!({"name": "Alice", "age": 30}))
            .unwrap();

        // nodes_by_property must find the node without an explicit index call.
        let hits = g
            .nodes_by_property("Person", "age", PropValue::Int(30))
            .unwrap();
        assert_eq!(hits, vec![node_id]);

        // has_node_property_index must report true based on stored data.
        assert!(g.has_node_property_index("Person", "age").unwrap());
        assert!(g.has_node_property_index("Person", "name").unwrap());
    }

    /// Verify that the auto-index is updated correctly when a node is updated.
    #[test]
    fn auto_index_on_update() {
        let (_dir, g) = open_tmp();

        let node_id = g
            .add_node("Person", &json!({"name": "Bob", "age": 25}))
            .unwrap();

        // Update age to 26.
        g.update_node(node_id, &json!({"name": "Bob", "age": 26}))
            .unwrap();

        // Old value must not be found.
        let old_hits = g
            .nodes_by_property("Person", "age", PropValue::Int(25))
            .unwrap();
        assert!(old_hits.is_empty());

        // New value must be found.
        let new_hits = g
            .nodes_by_property("Person", "age", PropValue::Int(26))
            .unwrap();
        assert_eq!(new_hits, vec![node_id]);
    }

    /// Verify that the auto-index is cleaned up when a node is deleted.
    #[test]
    fn auto_index_on_delete() {
        let (_dir, g) = open_tmp();

        let node_id = g
            .add_node("Person", &json!({"name": "Carol", "age": 40}))
            .unwrap();

        g.delete_node(node_id).unwrap();

        let hits = g
            .nodes_by_property("Person", "age", PropValue::Int(40))
            .unwrap();
        assert!(hits.is_empty());
    }
}
