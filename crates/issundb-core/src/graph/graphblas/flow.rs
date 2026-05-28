use super::*;

impl Graph {
    /// Minimum/Maximum Spanning Forest (MSF) optimized over contiguous CSR snapshot arrays.
    pub(in crate::graph) fn spanning_forest_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        weight_property: &str,
        maximum: bool,
    ) -> Result<Vec<EdgeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        let mut edges = Vec::new();

        let n = snap.dense_to_id.len();
        for i in 0..n {
            let start = snap.row_ptr[i];
            let end = snap.row_ptr[i + 1];
            for k in start..end {
                let edge_id = snap.edge_id[k];
                let col = snap.col_idx[k] as usize;

                let weight = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                    let props_json: serde_json::Value = props::decode(&rec.props)?;
                    if let Some(val) = props_json.get(weight_property) {
                        val.as_f64().unwrap_or(1.0)
                    } else {
                        1.0
                    }
                } else {
                    1.0
                };

                edges.push((edge_id, snap.dense_to_id[i], snap.dense_to_id[col], weight));
            }
        }

        if maximum {
            edges.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            edges.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
        }

        let mut parent: HashMap<NodeId, NodeId> = HashMap::new();

        fn find(u: NodeId, parent: &mut HashMap<NodeId, NodeId>) -> NodeId {
            let mut root = u;
            while let Some(&p) = parent.get(&root) {
                if p == root {
                    break;
                }
                root = p;
            }
            let mut curr = u;
            while let Some(&p) = parent.get(&curr) {
                if p == curr {
                    break;
                }
                parent.insert(curr, root);
                curr = p;
            }
            root
        }

        fn union(u: NodeId, v: NodeId, parent: &mut HashMap<NodeId, NodeId>) -> bool {
            let root_u = find(u, parent);
            let root_v = find(v, parent);
            if root_u != root_v {
                parent.insert(root_u, root_v);
                true
            } else {
                false
            }
        }

        let mut forest = Vec::new();
        for (edge_id, src, dst, _) in edges {
            parent.entry(src).or_insert(src);
            parent.entry(dst).or_insert(dst);

            if union(src, dst, &mut parent) {
                forest.push(edge_id);
            }
        }

        Ok(forest)
    }

    /// Edmonds-Karp Maximum Flow algorithm utilizing contiguous CSR snapshot arrays.
    pub(in crate::graph) fn maximum_flow_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        source: NodeId,
        sink: NodeId,
        capacity_property: &str,
    ) -> Result<f64, Error> {
        if source == sink {
            return Ok(0.0);
        }

        let rtxn = self.storage.env.read_txn()?;
        let mut residual: HashMap<(NodeId, NodeId), f64> = HashMap::new();
        let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();

        let n = snap.dense_to_id.len();
        for i in 0..n {
            let start = snap.row_ptr[i];
            let end = snap.row_ptr[i + 1];
            for k in start..end {
                let edge_id = snap.edge_id[k];
                let col = snap.col_idx[k] as usize;

                let capacity = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                    let props_json: serde_json::Value = props::decode(&rec.props)?;
                    if let Some(val) = props_json.get(capacity_property) {
                        val.as_f64().unwrap_or(0.0)
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };

                if capacity > 0.0 {
                    let u = snap.dense_to_id[i];
                    let v = snap.dense_to_id[col];

                    *residual.entry((u, v)).or_insert(0.0) += capacity;
                    residual.entry((v, u)).or_insert(0.0);

                    adj.entry(u).or_default().push(v);
                    adj.entry(v).or_default().push(u);
                }
            }
        }

        for neighbors in adj.values_mut() {
            neighbors.sort_unstable();
            neighbors.dedup();
        }

        if !adj.contains_key(&source) || !adj.contains_key(&sink) {
            return Ok(0.0);
        }

        let mut max_flow = 0.0;

        loop {
            let mut parent = HashMap::new();
            let mut queue = std::collections::VecDeque::new();
            let mut visited = AHashSet::new();

            queue.push_back(source);
            visited.insert(source);

            let mut path_found = false;

            while let Some(curr) = queue.pop_front() {
                if curr == sink {
                    path_found = true;
                    break;
                }

                if let Some(neighbors) = adj.get(&curr) {
                    for &neighbor in neighbors {
                        if !visited.contains(&neighbor) {
                            if let Some(&cap) = residual.get(&(curr, neighbor)) {
                                if cap > 1e-9 {
                                    visited.insert(neighbor);
                                    parent.insert(neighbor, curr);
                                    queue.push_back(neighbor);
                                }
                            }
                        }
                    }
                }
            }

            if !path_found {
                break;
            }

            let mut bottleneck = f64::INFINITY;
            let mut curr = sink;
            while curr != source {
                let p = parent[&curr];
                let cap = residual[&(p, curr)];
                if cap < bottleneck {
                    bottleneck = cap;
                }
                curr = p;
            }

            let mut curr = sink;
            while curr != source {
                let p = parent[&curr];
                if let Some(cap) = residual.get_mut(&(p, curr)) {
                    *cap -= bottleneck;
                }
                if let Some(cap) = residual.get_mut(&(curr, p)) {
                    *cap += bottleneck;
                }
                curr = p;
            }

            max_flow += bottleneck;
        }

        Ok(max_flow)
    }
}
