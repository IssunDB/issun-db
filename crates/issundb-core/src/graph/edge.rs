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
        self.csr_cache.record_added_edge(src, dst);
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
        let endpoints = self.delete_edge_impl(&mut wtxn, id)?;
        wtxn.commit()?;
        if let Some((src, dst)) = endpoints {
            self.csr_cache.record_removed_edge(src, dst);
        }
        self.maybe_spawn_rebuild();
        Ok(())
    }

    /// Delete an edge inside an open write transaction. Returns the deleted
    /// edge's `(src, dst)` endpoints so the caller can record the adjacency
    /// removal, or `None` if no such edge existed.
    pub(crate) fn delete_edge_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: EdgeId,
    ) -> Result<Option<(NodeId, NodeId)>, Error> {
        let record: EdgeRecord = match self.get_edge_impl(wtxn, id)? {
            Some(rec) => rec,
            None => return Ok(None),
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

        Ok(Some((record.src, record.dst)))
    }

    // ------------------------------------------------------------------
    // Traversal
    // ------------------------------------------------------------------

    /// Returns neighbor entries for all outgoing edges of `node`.
    ///
    /// Reads the `out_adj` store directly through the supplied transaction so
    /// the result always reflects committed (and, inside a [`WriteTxn`],
    /// uncommitted) writes. The CSR snapshot is deliberately not consulted here:
    /// it lags writes until the background rebuild runs, so serving point
    /// lookups from it would return deleted edges, hide newly added ones, and
    /// disagree with [`Self::in_neighbors`]. The snapshot remains the basis for
    /// the GraphBLAS matrix algorithms, which have explicit snapshot semantics.
    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.out_neighbors_impl(&rtxn, node)
    }

    pub(super) fn out_neighbors_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<NeighborEntry>, Error> {
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

    /// Returns whether the node has any incident relationship, reading the
    /// adjacency stores directly. Unlike [`Self::out_neighbors`], this never
    /// consults the CSR snapshot, which lags writes until the background rebuild
    /// completes. Write-time consistency checks (such as the DELETE connected-node
    /// guard) must see just-applied edge deletions, so they rely on this method.
    pub fn node_has_relationships(&self, node: NodeId) -> Result<bool, Error> {
        let rtxn = self.storage.env.read_txn()?;
        if !self.adj_entries_impl(&rtxn, node, true)?.is_empty() {
            return Ok(true);
        }
        Ok(!self.adj_entries_impl(&rtxn, node, false)?.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// After a CSR rebuild captures a node into the snapshot, adding an edge to
    /// that node must be visible through `out_neighbors`. The snapshot lags
    /// writes, so consulting it for point lookups would hide the new edge.
    #[test]
    fn out_neighbors_reflects_edge_added_after_snapshot() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();

        // Force a snapshot that includes `a` with zero outgoing edges.
        g.rebuild_csr().unwrap();
        assert!(g.out_neighbors(a).unwrap().is_empty());

        let eid = g.add_edge(a, b, "E", &()).unwrap();

        let out = g.out_neighbors(a).unwrap();
        assert_eq!(out.len(), 1, "new edge must be visible despite stale CSR");
        assert_eq!(out[0].edge, eid);
        assert_eq!(out[0].node, b);
    }

    /// After a CSR rebuild captures an edge into the snapshot, deleting that
    /// edge must remove it from `out_neighbors`. Serving from the stale snapshot
    /// would return the deleted edge.
    #[test]
    fn out_neighbors_reflects_edge_deleted_after_snapshot() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let eid = g.add_edge(a, b, "E", &()).unwrap();

        g.rebuild_csr().unwrap();
        assert_eq!(g.out_neighbors(a).unwrap().len(), 1);

        g.delete_edge(eid).unwrap();

        assert!(
            g.out_neighbors(a).unwrap().is_empty(),
            "deleted edge must not appear, even though CSR still holds it"
        );
    }

    /// `out_neighbors` and `in_neighbors` must agree on the same edge after a
    /// mutation that postdates the snapshot. This is the asymmetry the snapshot
    /// fast path introduced: `in_neighbors` always read LMDB while
    /// `out_neighbors` trusted the snapshot.
    #[test]
    fn out_and_in_neighbors_agree_after_snapshot() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        g.rebuild_csr().unwrap();

        let eid = g.add_edge(a, b, "E", &()).unwrap();

        let out = g.out_neighbors(a).unwrap();
        let inc = g.in_neighbors(b).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(inc.len(), 1);
        assert_eq!(out[0].edge, eid);
        assert_eq!(inc[0].edge, eid);
    }

    /// Inside a write transaction, `out_neighbors` must observe the edge created
    /// earlier in the same uncommitted transaction (read-your-writes).
    #[test]
    fn write_txn_out_neighbors_sees_uncommitted_edge() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        // Snapshot `a` with no outgoing edges so the stale path would return [].
        g.rebuild_csr().unwrap();

        g.update(|txn| {
            let eid = txn.add_edge(a, b, "E", &())?;
            let out = txn.out_neighbors(a)?;
            assert_eq!(out.len(), 1, "uncommitted edge must be visible in-txn");
            assert_eq!(out[0].edge, eid);
            Ok(())
        })
        .unwrap();
    }
}
