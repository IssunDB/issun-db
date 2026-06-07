use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Nodes
    // ------------------------------------------------------------------

    /// Insert a node with a single string label and msgpack-serializable properties.
    #[instrument(skip(self, props), fields(label = %label))]
    pub fn add_node(&self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let id = self.add_node_impl(&mut wtxn, &[label], props)?;
        wtxn.commit()?;
        self.csr_cache.record_added_node(id);
        self.prop_columns.record_touched(id);
        self.maybe_spawn_rebuild();
        Ok(id)
    }

    /// Insert a node with zero or more string labels and msgpack-serializable
    /// properties. An empty slice creates an unlabeled node.
    pub fn add_node_multi(&self, labels: &[&str], props: &impl Serialize) -> Result<NodeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let id = self.add_node_impl(&mut wtxn, labels, props)?;
        wtxn.commit()?;
        self.csr_cache.record_added_node(id);
        self.prop_columns.record_touched(id);
        self.maybe_spawn_rebuild();
        Ok(id)
    }

    pub(super) fn add_node_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        labels: &[&str],
        props: &impl Serialize,
    ) -> Result<NodeId, Error> {
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;

        // Resolve labels to ids, preserving insertion order and dropping duplicates.
        let mut resolved: Vec<(LabelId, String)> = Vec::with_capacity(labels.len());
        for &name in labels {
            let id = get_or_create_label(&self.storage, wtxn, name)?;
            if !resolved.iter().any(|(lid, _)| *lid == id) {
                resolved.push((id, name.to_string()));
            }
        }

        let node_id = alloc_node_id(&self.storage, wtxn)?;
        let record = NodeRecord {
            labels: resolved.iter().map(|(id, _)| *id).collect(),
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &node_id, &props::encode(&record)?)?;

        // Index the node under each of its labels: label index, label count,
        // property indexes (with constraint checks), and FTS.
        for (label_id, label_name) in &resolved {
            self.storage
                .label_idx
                .put(wtxn, &composite_key(*label_id, node_id), &())?;
            adjust_label_count(&self.storage, wtxn, *label_id, 1)?;
            self.index_node_for_label(wtxn, *label_id, label_name, node_id, &props_json)?;
        }

        Ok(node_id)
    }

    /// Write all property-index and FTS entries for a node's properties under a
    /// single label, performing unique and required constraint checks. Shared by
    /// node insertion, node update, and label addition.
    fn index_node_for_label(
        &self,
        wtxn: &mut heed::RwTxn,
        label_id: LabelId,
        label_name: &str,
        node_id: NodeId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        // Constraint-bearing and explicit property indexes for this label.
        let active_indexes = self.get_active_node_indexes(wtxn, label_id)?;
        for (prop_key_id, flags) in active_indexes {
            if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                let prop_val = props_json.get(&prop_name);

                // Required constraint check.
                if flags == 0x02
                    && (prop_val.is_none() || prop_val == Some(&serde_json::Value::Null))
                {
                    return Err(Error::RequiredConstraintViolation(
                        label_name.to_string(),
                        prop_name.to_string(),
                    ));
                }

                if let Some(val) = prop_val {
                    if val != &serde_json::Value::Null {
                        if let Some(encoded) = encode_property_value(val) {
                            // Unique constraint check; excludes this node by id so a
                            // re-index of an unchanged value does not conflict with itself.
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
                                                label_name.to_string(),
                                                prop_name.to_string(),
                                                val.to_string(),
                                            ));
                                        }
                                    }
                                }
                            }

                            let idx_key =
                                node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                            self.storage.node_prop_idx.put(wtxn, &idx_key, &())?;
                        }
                    }
                }
            }
        }

        // Auto-index: write every scalar property to node_prop_idx so the Cypher
        // optimizer can use NodeIndexScan without a prior CREATE INDEX.
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

        // FTS indexing hook for this label.
        self.index_node_fts(wtxn, node_id, label_id, props_json)?;

        Ok(())
    }

    /// Delete all property-index and FTS entries for a node's properties under a
    /// single label. Shared by node update, node deletion, and label removal.
    fn unindex_node_for_label(
        &self,
        wtxn: &mut heed::RwTxn,
        label_id: LabelId,
        node_id: NodeId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        let active = self.get_active_node_indexes(wtxn, label_id)?;
        for (prop_key_id, _) in active {
            if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                if let Some(val) = props_json.get(&prop_name) {
                    if let Some(encoded) = encode_property_value(val) {
                        let idx_key = node_prop_index_key(label_id, prop_key_id, &encoded, node_id);
                        self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                    }
                }
            }
        }

        // Auto-index cleanup.
        if let Some(obj) = props_json.as_object() {
            for (prop_name, val) in obj {
                if val.is_null() {
                    continue;
                }
                if let Some(encoded) = encode_property_value(val) {
                    if let Some(pkid) = get_prop_key(&self.storage, &*wtxn, prop_name)? {
                        let idx_key = node_prop_index_key(label_id, pkid, &encoded, node_id);
                        self.storage.node_prop_idx.delete(wtxn, &idx_key)?;
                    }
                }
            }
        }

        // FTS deletion hook for this label.
        self.delete_node_fts(wtxn, node_id, label_id, props_json)?;

        Ok(())
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
        self.prop_columns.record_touched(id);
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

        let labels = old_rec.labels.clone();
        let encoded_props = props::encode(props)?;
        let props_json: serde_json::Value = props::decode(&encoded_props)?;
        let old_props_json: serde_json::Value = props::decode(&old_rec.props)?;

        // The label set is unchanged by a property update. Re-index under each
        // label: drop the old property and FTS entries, then write the new ones
        // with constraint checks. Removing the node's own entries first means the
        // unique check below never conflicts with the node against itself.
        for &label_id in &labels {
            let label_name = self
                .label_name_impl(wtxn, label_id)?
                .unwrap_or_else(|| label_id.to_string());
            self.unindex_node_for_label(wtxn, label_id, id, &old_props_json)?;
            self.index_node_for_label(wtxn, label_id, &label_name, id, &props_json)?;
        }

        let record = NodeRecord {
            labels,
            props: encoded_props,
        };
        self.storage
            .nodes
            .put(wtxn, &id, &props::encode(&record)?)?;
        Ok(())
    }

    /// Add a label to an existing node. No-op if the node already carries it.
    pub fn add_label(&self, id: NodeId, label: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.add_label_impl(&mut wtxn, id, label)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn add_label_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: NodeId,
        label: &str,
    ) -> Result<(), Error> {
        let mut record: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Err(Error::NodeNotFound(id)),
        };
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        if record.labels.contains(&label_id) {
            return Ok(());
        }
        let props_json: serde_json::Value = props::decode(&record.props)?;
        record.labels.push(label_id);
        self.storage
            .nodes
            .put(wtxn, &id, &props::encode(&record)?)?;
        self.storage
            .label_idx
            .put(wtxn, &composite_key(label_id, id), &())?;
        adjust_label_count(&self.storage, wtxn, label_id, 1)?;
        self.index_node_for_label(wtxn, label_id, label, id, &props_json)?;
        Ok(())
    }

    /// Remove a label from an existing node. No-op if the node lacks the label,
    /// the label was never registered, or the node does not exist.
    pub fn remove_label(&self, id: NodeId, label: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.remove_label_impl(&mut wtxn, id, label)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn remove_label_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: NodeId,
        label: &str,
    ) -> Result<(), Error> {
        let mut record: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Ok(()),
        };
        let label_id = match get_label(&self.storage, &*wtxn, label)? {
            Some(lid) => lid,
            None => return Ok(()),
        };
        if let Some(pos) = record.labels.iter().position(|&l| l == label_id) {
            let props_json: serde_json::Value = props::decode(&record.props)?;
            record.labels.remove(pos);
            self.storage
                .nodes
                .put(wtxn, &id, &props::encode(&record)?)?;
            self.storage
                .label_idx
                .delete(wtxn, &composite_key(label_id, id))?;
            adjust_label_count(&self.storage, wtxn, label_id, -1)?;
            self.unindex_node_for_label(wtxn, label_id, id, &props_json)?;
        }
        Ok(())
    }

    /// Return the string labels of a node in insertion order. Returns an empty
    /// vector for an unlabeled or nonexistent node.
    pub fn node_labels(&self, id: NodeId) -> Result<Vec<String>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.node_labels_impl(&rtxn, id)
    }

    pub(super) fn node_labels_impl(
        &self,
        rtxn: &heed::RoTxn,
        id: NodeId,
    ) -> Result<Vec<String>, Error> {
        match self.get_node_impl(rtxn, id)? {
            Some(rec) => {
                let mut names = Vec::with_capacity(rec.labels.len());
                for lid in rec.labels {
                    if let Some(name) = self.label_name_impl(rtxn, lid)? {
                        names.push(name);
                    }
                }
                Ok(names)
            }
            None => Ok(vec![]),
        }
    }

    /// Delete a node.
    #[instrument(skip(self))]
    pub fn delete_node(&self, id: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        println!("DEBUG delete_node: id = {}", id);
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_node_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        // A node deletion reshuffles the sorted dense-index mapping, so the next
        // matrix refresh must rebuild fully rather than patch incrementally.
        self.csr_cache.mark_force_full();
        self.prop_columns.record_force_full();
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn delete_node_impl(&self, wtxn: &mut heed::RwTxn, id: NodeId) -> Result<(), Error> {
        let record: NodeRecord = match self.storage.nodes.get(wtxn, &id)? {
            Some(bytes) => props::decode(bytes)?,
            None => return Ok(()),
        };

        let props_json: serde_json::Value = props::decode(&record.props)?;

        // For each label: remove property and FTS index entries, the label index
        // entry, and decrement the label count.
        for &label_id in &record.labels {
            self.unindex_node_for_label(wtxn, label_id, id, &props_json)?;
            self.storage
                .label_idx
                .delete(wtxn, &composite_key(label_id, id))?;
            adjust_label_count(&self.storage, wtxn, label_id, -1)?;
        }

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

    /// A node created with `add_node_multi` carries every label, is reachable via
    /// `nodes_by_label` for each, and reports them through `node_labels`.
    #[test]
    fn multi_label_add_and_query() {
        let (_dir, g) = open_tmp();
        let id = g
            .add_node_multi(&["A", "B", "C"], &json!({"x": 1}))
            .unwrap();

        let mut labels = g.node_labels(id).unwrap();
        labels.sort();
        assert_eq!(labels, vec!["A", "B", "C"]);

        assert_eq!(g.nodes_by_label("A").unwrap(), vec![id]);
        assert_eq!(g.nodes_by_label("B").unwrap(), vec![id]);
        assert_eq!(g.nodes_by_label("C").unwrap(), vec![id]);
        assert_eq!(g.node_count_by_label("B").unwrap(), 1);
    }

    /// An empty label slice creates an unlabeled node.
    #[test]
    fn multi_label_empty_creates_unlabeled_node() {
        let (_dir, g) = open_tmp();
        let id = g.add_node_multi(&[], &json!({"x": 1})).unwrap();
        assert!(g.node_labels(id).unwrap().is_empty());
        assert!(g.get_node(id).unwrap().unwrap().labels.is_empty());
    }

    /// Duplicate labels passed to `add_node_multi` are stored once.
    #[test]
    fn multi_label_dedups() {
        let (_dir, g) = open_tmp();
        let id = g.add_node_multi(&["A", "A", "B"], &json!({})).unwrap();
        assert_eq!(g.get_node(id).unwrap().unwrap().labels.len(), 2);
    }

    /// `add_label` adds a label and keeps existing ones; it is idempotent.
    #[test]
    fn add_label_is_idempotent_and_additive() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("A", &json!({"x": 1})).unwrap();

        g.add_label(id, "B").unwrap();
        g.add_label(id, "B").unwrap(); // idempotent

        let mut labels = g.node_labels(id).unwrap();
        labels.sort();
        assert_eq!(labels, vec!["A", "B"]);
        assert_eq!(g.nodes_by_label("B").unwrap(), vec![id]);
        assert_eq!(g.node_count_by_label("B").unwrap(), 1);
    }

    /// `remove_label` drops one label, leaves the others, and updates the index.
    #[test]
    fn remove_label_drops_one_keeps_rest() {
        let (_dir, g) = open_tmp();
        let id = g.add_node_multi(&["A", "B"], &json!({})).unwrap();

        g.remove_label(id, "A").unwrap();

        assert_eq!(g.node_labels(id).unwrap(), vec!["B"]);
        assert!(g.nodes_by_label("A").unwrap().is_empty());
        assert_eq!(g.nodes_by_label("B").unwrap(), vec![id]);
        assert_eq!(g.node_count_by_label("A").unwrap(), 0);
    }

    /// Removing a label the node lacks, or one never registered, is a no-op.
    #[test]
    fn remove_label_missing_is_noop() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("A", &json!({})).unwrap();
        g.remove_label(id, "Nonexistent").unwrap();
        g.remove_label(id, "B").unwrap();
        assert_eq!(g.node_labels(id).unwrap(), vec!["A"]);
    }

    /// Properties stay findable under a label added after insertion, and become
    /// unfindable under a label that is removed.
    #[test]
    fn label_mutation_updates_property_index() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("A", &json!({"age": 30})).unwrap();

        g.add_label(id, "B").unwrap();
        assert_eq!(
            g.nodes_by_property("B", "age", PropValue::Int(30)).unwrap(),
            vec![id]
        );

        g.remove_label(id, "B").unwrap();
        assert!(
            g.nodes_by_property("B", "age", PropValue::Int(30))
                .unwrap()
                .is_empty()
        );
        // Still findable under the original label.
        assert_eq!(
            g.nodes_by_property("A", "age", PropValue::Int(30)).unwrap(),
            vec![id]
        );
    }

    /// Deleting a multi-label node clears every label index entry and count.
    #[test]
    fn delete_multi_label_node_clears_all_indexes() {
        let (_dir, g) = open_tmp();
        let id = g.add_node_multi(&["A", "B"], &json!({})).unwrap();
        g.delete_node(id).unwrap();
        assert!(g.nodes_by_label("A").unwrap().is_empty());
        assert!(g.nodes_by_label("B").unwrap().is_empty());
        assert_eq!(g.node_count_by_label("A").unwrap(), 0);
        assert_eq!(g.node_count_by_label("B").unwrap(), 0);
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
