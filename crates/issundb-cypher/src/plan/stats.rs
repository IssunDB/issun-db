use issundb_core::Graph;

/// A trait providing cardinality statistics for labels and relationship types
/// to help with query optimization.
pub trait StatsProvider {
    /// Get the count of nodes matching a string label.
    fn node_count_by_label(&self, label: &str) -> Option<u64>;

    /// Get the count of edges matching a string type.
    fn edge_count_by_type(&self, etype: &str) -> Option<u64>;

    /// Check if a node property index exists.
    fn has_node_property_index(&self, _label: &str, _property: &str) -> bool {
        false
    }
}

impl StatsProvider for Graph {
    fn node_count_by_label(&self, label: &str) -> Option<u64> {
        self.node_count_by_label(label).ok()
    }

    fn edge_count_by_type(&self, etype: &str) -> Option<u64> {
        self.edge_count_by_type(etype).ok()
    }

    fn has_node_property_index(&self, label: &str, property: &str) -> bool {
        self.has_node_property_index(label, property)
            .unwrap_or(false)
    }
}
