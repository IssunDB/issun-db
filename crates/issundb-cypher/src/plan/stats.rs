use issundb_core::Graph;

/// A trait providing cardinality statistics for labels and relationship types
/// to help with query optimization.
pub trait StatsProvider {
    /// Get the count of nodes matching a string label.
    fn node_count_by_label(&self, label: &str) -> Option<u64>;

    /// Get the count of edges matching a string type.
    fn edge_count_by_type(&self, etype: &str) -> Option<u64>;

    /// Upper-bound estimate of the total node count, used to derive average
    /// relationship fan-out (`edges_of_type / nodes`). Returns `None` when no
    /// estimate is available, in which case the planner falls back to a constant
    /// fan-out.
    fn total_node_count(&self) -> Option<u64> {
        None
    }

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

    fn total_node_count(&self) -> Option<u64> {
        self.node_count_hint().ok()
    }

    fn has_node_property_index(&self, label: &str, property: &str) -> bool {
        self.has_node_property_index(label, property)
            .unwrap_or(false)
    }
}
