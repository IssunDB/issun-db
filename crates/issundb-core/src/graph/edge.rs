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
        self.debug_assert_not_in_write_txn();
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let edge_id = self.add_edge_impl(&mut wtxn, src, dst, etype, props)?;
        self.commit_and_publish(wtxn, 1)?;
        self.edge_columns.record_touched(edge_id);
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
        // Both endpoints must already exist. Writing adjacency for a nonexistent
        // node id would leave a dangling `in_adj`/`out_adj` entry that a
        // later-allocated node would silently inherit, breaking adjacency
        // consistency. Reads see writes earlier in this same transaction, so a
        // node created before the edge in one `update` batch is visible. This
        // check runs before any write, so a rejected edge leaves no partial state.
        if self.storage.nodes.get(wtxn, &src)?.is_none() {
            return Err(Error::NodeNotFound(src));
        }
        if self.storage.nodes.get(wtxn, &dst)?.is_none() {
            return Err(Error::NodeNotFound(dst));
        }

        let type_id = get_or_create_type(&self.storage, wtxn, etype)?;
        let edge_id = alloc_edge_id(&self.storage, wtxn)?;
        let encoded_props = props::encode(props)?;

        // Validate constraints and populate indexes
        self.write_edge_index_entries(wtxn, edge_id, type_id, etype, &encoded_props)?;

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
        self.debug_assert_not_in_write_txn();
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.update_edge_impl(&mut wtxn, id, props)?;
        // Publishing matters even though no adjacency changed. A property change can
        // alter an edge's weight (`weight`/`cost`/`capacity`/`cap`), which the CSR
        // snapshot's per-edge weights bake in, and those have no incremental
        // maintenance. Advancing the generation here is
        // what marks them stale so the next `ensure_csr_fresh` rebuilds before a
        // weighted algorithm reads them; without it `shortest_path_dijkstra` and
        // friends serve the pre-update weight.
        self.commit_and_publish(wtxn, 1)?;
        self.edge_columns.record_touched(id);
        self.maybe_spawn_rebuild();
        Ok(())
    }

    pub(super) fn update_edge_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        id: EdgeId,
        props: &impl serde::Serialize,
    ) -> Result<(), Error> {
        let existing = self
            .storage
            .edges
            .get(wtxn, &id)?
            .ok_or(Error::EdgeNotFound(id))?;
        let record: EdgeRecord = crate::storage::props::decode(existing)?;
        let etype = self
            .type_name_impl(wtxn, record.edge_type)?
            .ok_or(Error::Corrupt("edge type name missing"))?;

        // Re-index under the new properties: drop the old entries first so the
        // unique check never conflicts with the edge against itself. A
        // constraint violation aborts the uncommitted transaction, so the old
        // entries survive.
        self.delete_edge_index_entries(wtxn, id, &record)?;
        let encoded_props = crate::storage::props::encode(props)?;
        self.write_edge_index_entries(wtxn, id, record.edge_type, &etype, &encoded_props)?;

        let new_record = EdgeRecord {
            src: record.src,
            dst: record.dst,
            edge_type: record.edge_type,
            props: encoded_props,
        };
        self.storage
            .edges
            .put(wtxn, &id, &crate::storage::props::encode(&new_record)?)?;
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
        self.debug_assert_not_in_write_txn();
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        let endpoints = self.delete_edge_impl(&mut wtxn, id)?;
        self.commit_and_publish(wtxn, 1)?;
        if endpoints.is_some() {
            // The deletion reshuffles the dense edge mapping; force a rebuild.
            self.edge_columns.record_force_full();
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
    /// the CSR snapshot algorithms, which have explicit snapshot semantics.
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

    /// Returns whether the node has any incident relationship, reading both
    /// adjacency stores directly. Like [`Self::out_neighbors`] and
    /// [`Self::in_neighbors`], this never consults the CSR snapshot, which lags
    /// writes until the next rebuild. Write-time consistency checks (such as the
    /// DELETE connected-node guard) must see just-applied edge deletions, so they
    /// rely on this method.
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

    /// `add_edge` must reject an endpoint that does not exist, so a
    /// later-allocated node cannot inherit dangling adjacency. A committed node
    /// created earlier in the graph is a valid endpoint.
    #[test]
    fn add_edge_rejects_nonexistent_endpoint() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        // dst 999 has never been allocated.
        assert!(matches!(
            g.add_edge(a, 999, "R", &()),
            Err(Error::NodeNotFound(999))
        ));
        // src 999 likewise.
        assert!(matches!(
            g.add_edge(999, a, "R", &()),
            Err(Error::NodeNotFound(999))
        ));
        // No dangling adjacency was written for the phantom id: a node allocated
        // afterward has no inherited relationships.
        let b = g.add_node("N", &()).unwrap();
        assert!(!g.node_has_relationships(b).unwrap());
        // A valid edge between existing nodes still works.
        assert!(g.add_edge(a, b, "R", &()).is_ok());
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

    /// `update_edge` must replace the stored properties and leave the
    /// endpoints and type untouched.
    #[test]
    fn update_edge_replaces_props() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        let eid = g.add_edge(a, b, "E", &serde_json::json!({"w": 1})).unwrap();

        g.update_edge(eid, &serde_json::json!({"w": 2})).unwrap();

        let rec = g.get_edge(eid).unwrap().expect("edge must still exist");
        assert_eq!(rec.src, a);
        assert_eq!(rec.dst, b);
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["w"], serde_json::json!(2));
    }

    #[test]
    fn update_edge_missing_edge_errors() {
        let (_dir, g) = open_tmp();
        let err = g
            .update_edge(999, &serde_json::json!({"w": 1}))
            .unwrap_err();
        assert!(matches!(err, Error::EdgeNotFound(999)));
    }

    /// `node_has_relationships` must reflect both adjacency directions and
    /// must go back to `false` once the last edge is deleted.
    #[test]
    fn node_has_relationships_reflects_adjacency() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &()).unwrap();
        let b = g.add_node("N", &()).unwrap();
        assert!(!g.node_has_relationships(a).unwrap());
        assert!(!g.node_has_relationships(b).unwrap());

        let eid = g.add_edge(a, b, "E", &()).unwrap();
        assert!(g.node_has_relationships(a).unwrap(), "out edge counts");
        assert!(g.node_has_relationships(b).unwrap(), "in edge counts");

        g.delete_edge(eid).unwrap();
        assert!(!g.node_has_relationships(a).unwrap());
        assert!(!g.node_has_relationships(b).unwrap());
    }
}
