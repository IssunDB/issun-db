use super::*;

impl ReadTxn<'_> {
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        self.graph.get_node_impl(&self.rtxn, id)
    }

    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        self.graph.get_edge_impl(&self.rtxn, id)
    }

    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.out_neighbors_impl(&self.rtxn, node)
    }

    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.in_neighbors_impl(&self.rtxn, node)
    }

    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        self.graph.nodes_by_label_impl(&self.rtxn, label)
    }

    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        self.graph.edges_by_type_impl(&self.rtxn, etype)
    }

    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        self.graph.label_name_impl(&self.rtxn, id)
    }

    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        self.graph.type_name_impl(&self.rtxn, id)
    }

    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        self.graph.node_count_by_label_impl(&self.rtxn, label)
    }

    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        self.graph.edge_count_by_type_impl(&self.rtxn, etype)
    }

    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        self.graph.all_nodes_impl(&self.rtxn)
    }

    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        self.graph.vector_bytes_impl(&self.rtxn)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_property_index_impl(&self.rtxn, label, property)
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_impl(&self.rtxn, label, property, val)
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
        self.graph.nodes_by_property_range_impl(
            &self.rtxn,
            label,
            property,
            min_val,
            min_inclusive,
            max_val,
            max_inclusive,
        )
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_impl(&self.rtxn, etype, property, val)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_range_impl(&self.rtxn, etype, property, min_val, max_val)
    }

    #[doc(hidden)]
    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_text_index_impl(&self.rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        self.graph.fts_stats_impl(&self.rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        self.graph
            .fts_doc_len_impl(&self.rtxn, label, property, node_id)
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        self.graph
            .fts_postings_impl(&self.rtxn, label, property, term)
    }

    #[doc(hidden)]
    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        self.graph.active_text_indexes_impl(&self.rtxn)
    }
}

impl WriteTxn<'_> {
    pub fn get_node(&self, id: NodeId) -> Result<Option<NodeRecord>, Error> {
        self.graph.get_node_impl(&self.wtxn, id)
    }

    pub fn get_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, Error> {
        self.graph.get_edge_impl(&self.wtxn, id)
    }

    pub fn out_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.out_neighbors_impl(&self.wtxn, node)
    }

    pub fn in_neighbors(&self, node: NodeId) -> Result<Vec<NeighborEntry>, Error> {
        self.graph.in_neighbors_impl(&self.wtxn, node)
    }

    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<NodeId>, Error> {
        self.graph.nodes_by_label_impl(&self.wtxn, label)
    }

    pub fn edges_by_type(&self, etype: &str) -> Result<Vec<EdgeId>, Error> {
        self.graph.edges_by_type_impl(&self.wtxn, etype)
    }

    pub fn label_name(&self, id: LabelId) -> Result<Option<String>, Error> {
        self.graph.label_name_impl(&self.wtxn, id)
    }

    pub fn type_name(&self, id: TypeId) -> Result<Option<String>, Error> {
        self.graph.type_name_impl(&self.wtxn, id)
    }

    pub fn node_count_by_label(&self, label: &str) -> Result<u64, Error> {
        self.graph.node_count_by_label_impl(&self.wtxn, label)
    }

    pub fn edge_count_by_type(&self, etype: &str) -> Result<u64, Error> {
        self.graph.edge_count_by_type_impl(&self.wtxn, etype)
    }

    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        self.graph.all_nodes_impl(&self.wtxn)
    }

    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        self.graph.vector_bytes_impl(&self.wtxn)
    }

    pub fn has_node_property_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        self.graph
            .has_node_property_index_impl(&self.wtxn, label, property)
    }

    pub fn nodes_by_property(
        &self,
        label: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<NodeId>, Error> {
        self.graph
            .nodes_by_property_impl(&self.wtxn, label, property, val)
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
        self.graph.nodes_by_property_range_impl(
            &self.wtxn,
            label,
            property,
            min_val,
            min_inclusive,
            max_val,
            max_inclusive,
        )
    }

    pub fn edges_by_property(
        &self,
        etype: &str,
        property: &str,
        val: PropValue,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_impl(&self.wtxn, etype, property, val)
    }

    pub fn edges_by_property_range(
        &self,
        etype: &str,
        property: &str,
        min_val: Option<PropValue>,
        max_val: Option<PropValue>,
    ) -> Result<Vec<EdgeId>, Error> {
        self.graph
            .edges_by_property_range_impl(&self.wtxn, etype, property, min_val, max_val)
    }

    pub fn add_node(&mut self, label: &str, props: &impl Serialize) -> Result<NodeId, Error> {
        let node_id = self.graph.add_node_impl(&mut self.wtxn, &[label], props)?;
        self.mutations_count += 1;
        self.delta.added_nodes.push(node_id);
        Ok(node_id)
    }

    /// Insert a node with zero or more labels inside this write transaction.
    pub fn add_node_multi(
        &mut self,
        labels: &[&str],
        props: &impl Serialize,
    ) -> Result<NodeId, Error> {
        let node_id = self.graph.add_node_impl(&mut self.wtxn, labels, props)?;
        self.mutations_count += 1;
        self.delta.added_nodes.push(node_id);
        Ok(node_id)
    }

    pub fn update_node(&mut self, id: NodeId, props: &impl Serialize) -> Result<(), Error> {
        self.graph.update_node_impl(&mut self.wtxn, id, props)?;
        self.mutations_count += 1;
        Ok(())
    }

    /// Add a label to an existing node inside this write transaction.
    pub fn add_label(&mut self, id: NodeId, label: &str) -> Result<(), Error> {
        self.graph.add_label_impl(&mut self.wtxn, id, label)?;
        self.mutations_count += 1;
        Ok(())
    }

    /// Remove a label from an existing node inside this write transaction.
    pub fn remove_label(&mut self, id: NodeId, label: &str) -> Result<(), Error> {
        self.graph.remove_label_impl(&mut self.wtxn, id, label)?;
        self.mutations_count += 1;
        Ok(())
    }

    pub fn delete_node(&mut self, id: NodeId) -> Result<(), Error> {
        self.graph.delete_node_impl(&mut self.wtxn, id)?;
        self.mutations_count += 1;
        // A node deletion reshuffles the sorted dense-index mapping, so the next
        // refresh must rebuild fully rather than patch incrementally.
        self.delta.force_full = true;
        Ok(())
    }

    pub fn delete_edge(&mut self, id: EdgeId) -> Result<(), Error> {
        if let Some((src, dst)) = self.graph.delete_edge_impl(&mut self.wtxn, id)? {
            self.delta.removed_edges.push((src, dst));
        }
        self.mutations_count += 1;
        Ok(())
    }

    pub fn add_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        etype: &str,
        props: &impl Serialize,
    ) -> Result<EdgeId, Error> {
        let edge_id = self
            .graph
            .add_edge_impl(&mut self.wtxn, src, dst, etype, props)?;
        self.mutations_count += 1;
        self.delta.added_edges.push((src, dst));
        Ok(edge_id)
    }

    #[doc(hidden)]
    pub fn put_vector_bytes(&mut self, n: NodeId, bytes: &[u8]) -> Result<(), Error> {
        self.graph.put_vector_bytes_impl(&mut self.wtxn, n, bytes)?;
        self.mutations_count += 1;
        Ok(())
    }

    /// Delete the raw vector bytes for `n` from LMDB. No-op if absent.
    #[doc(hidden)]
    pub fn delete_vector_bytes(&mut self, n: NodeId) -> Result<(), Error> {
        self.graph.delete_vector_bytes_impl(&mut self.wtxn, n)?;
        self.mutations_count += 1;
        Ok(())
    }

    #[doc(hidden)]
    pub fn create_node_text_index(&mut self, label: &str, property: &str) -> Result<(), Error> {
        self.graph.create_node_text_index_impl(
            &mut self.wtxn,
            label,
            property,
            Language::English,
        )?;
        self.mutations_count += 1;
        Ok(())
    }

    #[doc(hidden)]
    pub fn drop_node_text_index(&mut self, label: &str, property: &str) -> Result<(), Error> {
        self.graph
            .drop_node_text_index_impl(&mut self.wtxn, label, property)?;
        self.mutations_count += 1;
        Ok(())
    }

    #[doc(hidden)]
    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.has_node_text_index_impl(rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_stats_impl(rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_doc_len_impl(rtxn, label, property, node_id)
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.fts_postings_impl(rtxn, label, property, term)
    }

    #[doc(hidden)]
    pub fn create_node_text_index_with_language(
        &mut self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        self.graph
            .create_node_text_index_impl(&mut self.wtxn, label, property, lang)?;
        self.mutations_count += 1;
        Ok(())
    }

    #[doc(hidden)]
    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        let rtxn: &heed::RoTxn = &self.wtxn;
        self.graph.active_text_indexes_impl(rtxn)
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

    #[test]
    fn test_transaction_read_only() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({"name": "Alice"})).unwrap();
        let b = g.add_node("Person", &json!({"name": "Bob"})).unwrap();

        g.view(|txn| {
            let node_a = txn.get_node(a).unwrap().unwrap();
            let props_a: serde_json::Value = rmp_serde::from_slice(&node_a.props).unwrap();
            assert_eq!(props_a["name"], "Alice");

            let node_b = txn.get_node(b).unwrap().unwrap();
            let props_b: serde_json::Value = rmp_serde::from_slice(&node_b.props).unwrap();
            assert_eq!(props_b["name"], "Bob");

            let nodes = txn.all_nodes().unwrap();
            assert_eq!(nodes.len(), 2);

            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_transaction_write_commit() {
        let (_dir, g) = open_tmp();

        let (a, b) = g
            .update(|txn| {
                let a = txn.add_node("Person", &json!({"name": "Alice"})).unwrap();
                let b = txn.add_node("Person", &json!({"name": "Bob"})).unwrap();
                txn.add_edge(a, b, "KNOWS", &json!({"since": 2020}))
                    .unwrap();
                Ok((a, b))
            })
            .unwrap();

        // After commit, data should be in the DB
        let node_a = g.get_node(a).unwrap().unwrap();
        let props_a: serde_json::Value = rmp_serde::from_slice(&node_a.props).unwrap();
        assert_eq!(props_a["name"], "Alice");

        let neighbors = g.out_neighbors(a).unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node, b);
    }

    #[test]
    fn test_transaction_write_rollback() {
        let (_dir, g) = open_tmp();

        let res: Result<(), Error> = g.update(|txn| {
            txn.add_node("Person", &json!({"name": "Alice"})).unwrap();
            // Intentionally fail the transaction
            Err(Error::Corrupt("simulated failure"))
        });

        assert!(res.is_err());

        // The node should NOT be present in the database since the transaction rolled back
        let nodes = g.all_nodes().unwrap();
        assert_eq!(nodes.len(), 0);
    }

    // --- bfs_multi_source_graphblas ---
    //
    // Each test calls `rebuild_csr()` after mutating the graph so the GraphBLAS
    // adjacency matrix reflects the inserted edges before BFS is invoked.

    #[test]
    fn graphblas_multi_source_empty_seeds_returns_empty() {
        let (_dir, g) = open_tmp();
        g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let result = g.bfs_multi_source_graphblas(&[], 2, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn graphblas_multi_source_hops_zero_returns_only_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let mut result = g.bfs_multi_source_graphblas(&[a, b], 0, None).unwrap();
        result.sort_unstable();
        assert_eq!(result, vec![a, b]);
        assert!(!result.contains(&c));
    }

    #[test]
    fn graphblas_multi_source_expands_to_correct_depth() {
        let (_dir, g) = open_tmp();
        // Chain: a → b → c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let r1 = g.bfs_multi_source_graphblas(&[a], 1, None).unwrap();
        assert!(r1.contains(&a));
        assert!(r1.contains(&b));
        assert!(!r1.contains(&c));
        assert!(!r1.contains(&d));

        let r2 = g.bfs_multi_source_graphblas(&[a], 2, None).unwrap();
        assert!(r2.contains(&a));
        assert!(r2.contains(&b));
        assert!(r2.contains(&c));
        assert!(!r2.contains(&d));
    }

    #[test]
    fn graphblas_multi_source_max_nodes_cap_respected() {
        let (_dir, g) = open_tmp();
        // Star + tail: a → b, c, d; b → e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(a, d, "E", &json!({})).unwrap();
        g.add_edge(b, e, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a], 2, Some(3)).unwrap();
        assert!(
            result.len() <= 3,
            "expected at most 3 nodes, got {}",
            result.len()
        );
    }

    #[test]
    fn graphblas_multi_source_two_seeds_union_disconnected_components() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains: a → b; c → d
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a, c], 1, None).unwrap();
        assert!(result.contains(&a));
        assert!(result.contains(&b));
        assert!(result.contains(&c));
        assert!(result.contains(&d));
    }

    #[test]
    fn graphblas_multi_source_deduplicates_shared_neighbors() {
        let (_dir, g) = open_tmp();
        // a → c; b → c; c must appear once.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let result = g.bfs_multi_source_graphblas(&[a, b], 1, None).unwrap();
        let count_c = result.iter().filter(|&&n| n == c).count();
        assert_eq!(count_c, 1);
        assert_eq!(result.len(), 3); // a, b, c
    }

    #[test]
    fn graphblas_multi_source_handles_newly_added_seeds_via_dynamic_materialization() {
        let (_dir, g) = open_tmp();
        // Seed a is in the CSR; b is added after rebuild_csr (making snapshot/matrices stale).
        // The function must detect the new nodes, dynamically rebuild the CSR/matrices, and run successfully.
        let a = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        // b is inserted AFTER rebuild, so it makes the existing MatrixSet stale.
        let b = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();

        // Both seeds must appear in the result; d must be reachable from b via the dynamically rematerialized matrices.
        let result = g.bfs_multi_source_graphblas(&[a, b], 1, None).unwrap();
        assert!(result.contains(&a), "seed a must be present");
        assert!(result.contains(&b), "seed b must be present");
        assert!(result.contains(&c), "c reachable from a");
        assert!(result.contains(&d), "d reachable from b");
    }

    // --- node_count_by_label / edge_count_by_type stats ---

    #[test]
    fn label_count_increments_on_add_node() {
        let (_dir, g) = open_tmp();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);
        g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
        g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 2);
        // Other labels are not affected.
        assert_eq!(g.node_count_by_label("Company").unwrap(), 0);
    }

    #[test]
    fn label_count_decrements_on_delete_node() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 2);

        g.delete_node(a).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);

        g.delete_node(b).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);

        // Deleting a non-existent node is a no-op; count stays at 0.
        g.delete_node(b).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 0);
    }

    #[test]
    fn label_count_unchanged_on_update_node() {
        let (_dir, g) = open_tmp();
        let id = g.add_node("Person", &json!({})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);

        // update_node does not change the label; the count must stay at 1.
        g.update_node(id, &json!({"name": "Alice"})).unwrap();
        assert_eq!(g.node_count_by_label("Person").unwrap(), 1);
    }

    #[test]
    fn update_node_returns_not_found_for_missing_node() {
        let (_dir, g) = open_tmp();
        let res = g.update_node(9999, &json!({}));
        assert!(matches!(res, Err(Error::NodeNotFound(9999))));
    }

    #[test]
    fn type_count_increments_on_add_edge() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);

        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 1);

        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 2);

        // Different type is not affected.
        assert_eq!(g.edge_count_by_type("WORKS_AT").unwrap(), 0);
    }

    #[test]
    fn type_count_decrements_on_delete_node_cascade() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, a, "KNOWS", &json!({})).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 2);

        // Deleting node a cascades and removes both edges touching a.
        g.delete_node(a).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);
    }

    #[test]
    fn delete_edge_correctness() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let eid = g.add_edge(a, b, "KNOWS", &json!({})).unwrap();

        // 1. Verify exists
        assert!(g.get_edge(eid).unwrap().is_some());
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 1);

        // 2. Verify adjacency lists
        let out_neighs = g.out_neighbors(a).unwrap();
        assert_eq!(out_neighs.len(), 1);
        assert_eq!(out_neighs[0].node, b);
        assert_eq!(out_neighs[0].edge, eid);

        let in_neighs = g.in_neighbors(b).unwrap();
        assert_eq!(in_neighs.len(), 1);
        assert_eq!(in_neighs[0].node, a);
        assert_eq!(in_neighs[0].edge, eid);

        // 3. Delete the edge
        g.delete_edge(eid).unwrap();

        // 4. Verify gone
        assert!(g.get_edge(eid).unwrap().is_none());
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);

        // 5. Verify adjacency lists updated
        assert_eq!(g.out_neighbors(a).unwrap().len(), 0);
        assert_eq!(g.in_neighbors(b).unwrap().len(), 0);

        // 6. Idempotence: delete non-existent edge
        g.delete_edge(eid).unwrap();
        assert_eq!(g.edge_count_by_type("KNOWS").unwrap(), 0);
    }

    #[test]
    fn test_node_property_secondary_index_and_scans() {
        let (_dir, g) = open_tmp();

        // Add nodes
        let n1 = g
            .add_node("Person", &json!({"name": "Alice", "age": 30}))
            .unwrap();
        let n2 = g
            .add_node("Person", &json!({"name": "Bob", "age": 25}))
            .unwrap();
        let n3 = g
            .add_node("Person", &json!({"name": "Charlie", "age": 30}))
            .unwrap();
        let _n4 = g
            .add_node("Employee", &json!({"name": "Alice", "age": 40}))
            .unwrap();

        // Create index on Person(age)
        g.create_node_property_index("Person", "age").unwrap();

        // Check index exists
        assert!(g.has_node_property_index("Person", "age").unwrap());

        // Point queries
        let p30 = g
            .nodes_by_property("Person", "age", PropValue::Int(30))
            .unwrap();
        assert_eq!(p30.len(), 2);
        assert!(p30.contains(&n1));
        assert!(p30.contains(&n3));

        let p25 = g
            .nodes_by_property("Person", "age", PropValue::Int(25))
            .unwrap();
        assert_eq!(p25.len(), 1);
        assert!(p25.contains(&n2));

        // Range queries (e.g. age between 20 and 28)
        let pr = g
            .nodes_by_property_range(
                "Person",
                "age",
                Some(PropValue::Int(20)),
                true,
                Some(PropValue::Int(28)),
                true,
            )
            .unwrap();
        assert_eq!(pr.len(), 1);
        assert!(pr.contains(&n2));

        // Let's create an index on Person(name) to test string sorting/prefix
        g.create_node_property_index("Person", "name").unwrap();
        let p_alice = g
            .nodes_by_property("Person", "name", PropValue::Str("Alice".to_string()))
            .unwrap();
        assert_eq!(p_alice.len(), 1);
        assert!(p_alice.contains(&n1));
    }

    #[test]
    fn test_unique_property_constraint() {
        let (_dir, g) = open_tmp();

        // Create unique constraint on User(email)
        g.create_node_unique_constraint("User", "email").unwrap();

        // Add first user
        let _u1 = g
            .add_node(
                "User",
                &json!({"email": "user1@example.com", "name": "User 1"}),
            )
            .unwrap();

        // Add second user with duplicate email - should fail
        let res2 = g.add_node(
            "User",
            &json!({"email": "user1@example.com", "name": "User 2"}),
        );
        assert!(res2.is_err());
        assert!(matches!(
            res2.unwrap_err(),
            Error::UniqueConstraintViolation { .. }
        ));

        // Add second user with unique email - should succeed
        let u2 = g
            .add_node(
                "User",
                &json!({"email": "user2@example.com", "name": "User 2"}),
            )
            .unwrap();

        // Update u2 to have u1's email - should fail
        let update_res =
            g.update_node(u2, &json!({"email": "user1@example.com", "name": "User 2"}));
        assert!(update_res.is_err());
        assert!(matches!(
            update_res.unwrap_err(),
            Error::UniqueConstraintViolation { .. }
        ));
    }

    #[test]
    fn test_required_property_constraint() {
        let (_dir, g) = open_tmp();

        // Create required constraint on Task(title)
        g.create_node_required_constraint("Task", "title").unwrap();

        // Add task with title - should succeed
        let t1 = g
            .add_node("Task", &json!({"title": "Do homework", "done": false}))
            .unwrap();

        // Add task without title - should fail
        let res2 = g.add_node("Task", &json!({"done": false}));
        assert!(res2.is_err());
        assert!(matches!(
            res2.unwrap_err(),
            Error::RequiredConstraintViolation { .. }
        ));

        // Update t1 to remove title - should fail
        let update_res = g.update_node(t1, &json!({"done": true}));
        assert!(update_res.is_err());
        assert!(matches!(
            update_res.unwrap_err(),
            Error::RequiredConstraintViolation { .. }
        ));
    }

    #[test]
    fn test_index_cleanup_on_delete() {
        let (_dir, g) = open_tmp();

        // Create unique index on Account(number)
        g.create_node_unique_constraint("Account", "number")
            .unwrap();

        let a1 = g.add_node("Account", &json!({"number": "12345"})).unwrap();

        // Delete a1
        g.delete_node(a1).unwrap();

        // Now we should be able to reuse the account number because index was cleaned up!
        let a2 = g.add_node("Account", &json!({"number": "12345"}));
        assert!(a2.is_ok());
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        let dir = TempDir::new().unwrap();
        let backup_file = dir.path().join("snapshot.mdb");
        let restore_dir = dir.path().join("restored");

        // Write data.
        let n;
        {
            let g = Graph::open(&dir.path().join("primary"), 1).unwrap();
            n = g
                .add_node("BackupTest", &serde_json::json!({"x": 42}))
                .unwrap();
            g.backup(&backup_file).unwrap();
        }

        // Restore and verify.
        Graph::restore(&backup_file, &restore_dir).unwrap();
        let g2 = Graph::open(&restore_dir, 1).unwrap();
        let rec = g2
            .get_node(n)
            .unwrap()
            .expect("node must exist in restored graph");
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["x"], serde_json::json!(42));
    }

    #[test]
    fn backup_compact_and_restore_roundtrip() {
        let dir = TempDir::new().unwrap();
        let backup_file = dir.path().join("compact.mdb");
        let restore_dir = dir.path().join("restored");

        // Write data, delete some of it, then take a compacted snapshot.
        let kept;
        {
            let g = Graph::open(&dir.path().join("primary"), 1).unwrap();
            let doomed = g
                .add_node("BackupTest", &serde_json::json!({"x": 1}))
                .unwrap();
            kept = g
                .add_node("BackupTest", &serde_json::json!({"x": 42}))
                .unwrap();
            g.delete_node(doomed).unwrap();
            g.backup_compact(&backup_file).unwrap();
        }

        // Restore and verify the surviving data round-trips.
        Graph::restore(&backup_file, &restore_dir).unwrap();
        let g2 = Graph::open(&restore_dir, 1).unwrap();
        let rec = g2
            .get_node(kept)
            .unwrap()
            .expect("node must exist in restored graph");
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["x"], serde_json::json!(42));
        assert_eq!(g2.nodes_by_label("BackupTest").unwrap(), vec![kept]);
    }
}
