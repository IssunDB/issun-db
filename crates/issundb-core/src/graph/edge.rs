use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Edges
    // ------------------------------------------------------------------

    /// Insert a directed edge `src → dst` with a string type and properties.
    #[instrument(skip(self, props), fields(src = %src, dst = %dst, etype = %etype))]
    pub fn add_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let edge_id = self.add_edge_impl(&mut wtxn, src, dst, etype, props)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(edge_id)
    }

    pub(super) fn add_edge_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let edge_id = alloc_edge_id(&self.storage, wtxn)?;
        let encoded_props = props::encode(props)?;

        // Validate constraints and populate indexes
        let active_indexes = self.get_active_edge_indexes(wtxn, type_id)?;
        if !active_indexes.is_empty() {
            let props_json: serde_json::Value = props::decode(&encoded_props)?;
            for (prop_key_id, flags) in active_indexes {
                if let Some(prop_name) = self.prop_key_name_impl(wtxn, prop_key_id)? {
                    let prop_val = props_json.get(&prop_name);

                    // 1. Required constraint check
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
                            if let Some(encoded) = encode_property_value(val) {
                                // 2. Unique constraint check
                                if flags == 0x01 {
                                    let mut prefix = Vec::with_capacity(4 + 4 + encoded.len());
                                    prefix.extend_from_slice(&type_id.to_be_bytes());
                                    prefix.extend_from_slice(&prop_key_id.to_be_bytes());
                                    prefix.extend_from_slice(&encoded);

                                    for entry in
                                        self.storage.edge_prop_idx.prefix_iter(wtxn, &prefix)?
                                    {
                                        let (key, _) = entry?;
                                        if key.len() >= 8 {
                                            let mut edge_id_bytes = [0u8; 8];
                                            edge_id_bytes.copy_from_slice(&key[key.len() - 8..]);
                                            let found_edge_id = u64::from_be_bytes(edge_id_bytes);
                                            if found_edge_id != edge_id {
                                                return Err(Error::UniqueConstraintViolation(
                                                    etype.to_string(),
                                                    prop_name.to_string(),
                                                    val.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }

                                // 3. Write index entry
                                let idx_key =
                                    edge_prop_index_key(type_id, prop_key_id, &encoded, edge_id);
                                self.storage.edge_prop_idx.put(wtxn, &idx_key, &())?;
                            }
                        }
                    }
                }
            }
        }

        let record = EdgeRecord {
            src,
            dst,
            edge_type: type_id,
            props: encoded_props,
        };
        self.storage
            .edges
            .put(wtxn, &edge_id, &props::encode(&record)?)?;
        self.storage
            .type_idx
            .put(wtxn, &composite_key(type_id, edge_id), &())?;

        self.append_adj(wtxn, src, dst, type_id, edge_id, true)?;
        self.append_adj(wtxn, dst, src, type_id, edge_id, false)?;

        adjust_type_count(&self.storage, wtxn, type_id, 1)?;

        Ok(edge_id)
    }

    /// Update the properties of an existing edge, preserving src, dst, and type.
    pub fn update_edge(&self, id: EdgeId, props: &impl serde::Serialize) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let existing = self
            .storage
            .edges
            .get(&wtxn, &id)?
            .ok_or(Error::EdgeNotFound(id))?;
        let record: EdgeRecord = crate::storage::props::decode(existing)?;
        let new_record = EdgeRecord {
            src: record.src,
            dst: record.dst,
            edge_type: record.edge_type,
            props: crate::storage::props::encode(props)?,
        };
        self.storage
            .edges
            .put(&mut wtxn, &id, &crate::storage::props::encode(&new_record)?)?;
        wtxn.commit()?;
        Ok(())
    }

    /// Fetch an edge record by id.
    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.get_edge_impl(&rtxn, id)
    }

    pub(super) fn get_edge_impl(
        &self,
        txn: &heed::RoTxn,
        id: EdgeId,
    ) -> Result<Option<EdgeRecord>, Error> {
        match self.storage.edges.get(txn, &id)? {
            Some(bytes) => Ok(Some(props::decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Delete an edge.
    #[instrument(skip(self))]
    pub fn delete_edge(&self, id: EdgeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_edge_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(crate) fn delete_edge_impl(&self, wtxn: &mut heed::RwTxn, id: EdgeId) -> Result<(), Error> {
        let record: EdgeRecord = match self.get_edge_impl(wtxn, id)? {
            Some(rec) => rec,
            None => return Ok(()),
        };

        // 1. Delete from edge property index
        self.delete_edge_index_entries(wtxn, id, &record)?;

        // 2. Delete the edge record itself
        self.storage.edges.delete(wtxn, &id)?;

        // 3. Delete from the type index
        self.storage
            .type_idx
            .delete(wtxn, &composite_key(record.edge_type, id))?;

        // 4. Adjust the type count
        adjust_type_count(&self.storage, wtxn, record.edge_type, -1)?;

        // 5. Delete from out_adj (key is src, other is dst)
        let out_entry = AdjEntry {
            edge_type: record.edge_type,
            other: record.dst,
            edge_id: id,
        };
        self.storage
            .out_adj
            .delete_one_duplicate(wtxn, &record.src, out_entry.as_bytes())?;

        // 6. Delete from in_adj (key is dst, other is src)
        let in_entry = AdjEntry {
            edge_type: record.edge_type,
            other: record.src,
            edge_id: id,
        };
        self.storage
            .in_adj
            .delete_one_duplicate(wtxn, &record.dst, in_entry.as_bytes())?;

        Ok(())
    }

    // ------------------------------------------------------------------
    // Traversal
    // ------------------------------------------------------------------

    /// Returns neighbor entries for all outgoing edges of `node`.
    ///
    /// Uses the in-memory CSR snapshot when the node is present in it; falls
    /// back to an LMDB cursor for nodes added since the last rebuild.
    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.out_neighbors_impl(&rtxn, node)
    }

    pub(super) fn out_neighbors_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let snap = self.csr_cache.snapshot.load();
        if let Some(neighbors) = snap.out_neighbors(node) {
            return Ok(neighbors
                .into_iter()
                .map(|(node, edge, edge_type)| NeighborEntry {
                    node,
                    edge,
                    edge_type,
                })
                .collect());
        }
        self.adj_entries_impl(rtxn, node, true)
    }

    /// Returns neighbor entries for all incoming edges of `node`.
    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.in_neighbors_impl(&rtxn, node)
    }

    pub(super) fn in_neighbors_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<NeighborEntry>, Error> {
        self.adj_entries_impl(rtxn, node, false)
    }
}
