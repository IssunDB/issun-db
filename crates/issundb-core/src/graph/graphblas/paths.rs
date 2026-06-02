use super::*;

impl Graph {
    /// Unweighted SSSP from `src` to `dst` via MinPlus SpMV, with path reconstruction
    /// from the LMDB in-adjacency once the destination is reached.
    #[doc(hidden)]
    pub fn shortest_path_graphblas(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        if src == dst {
            return Ok(Some(vec![src]));
        }

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.shortest_path(src, dst),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;

        let src_dense = match snap.id_to_dense.get(&src) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };
        let dst_dense = match snap.id_to_dense.get(&dst) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        // dist_vals[d] = Some(hop) once node d is reached.
        // dist_vals[d] = hop count from src to dense node d once reached.
        let mut dist_vals: Vec<Option<i32>> = vec![None; n];
        dist_vals[src_dense] = Some(0);

        let mut reached_dst = false;

        for hop in 1..=(n as i32) {
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &dist,
                &Assignment::new(),
                &mut next,
                &dist,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                == 0
            {
                break;
            }

            // Iteration k produces nodes at hop distance k; record before merging.
            let new_indices: Vec<usize> = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for &idx in &new_indices {
                dist_vals[idx] = Some(hop);
                if idx == dst_dense {
                    reached_dst = true;
                }
            }

            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &dist,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist = merged;

            if reached_dst {
                break;
            }
        }

        if !reached_dst {
            return Ok(None);
        }

        // Reconstruct path by tracing backward from dst using LMDB in-neighbors.
        // At each step we look for a predecessor with dist == current_dist - 1.
        let mut path = vec![dst_dense];
        let mut cur = dst_dense;
        while cur != src_dense {
            let cur_dist = match dist_vals[cur] {
                Some(d) => d,
                None => return Ok(None),
            };
            let cur_id = snap.dense_to_id[cur];
            let in_neighbors = self.adj_entries(cur_id, false)?;
            let mut moved = false;
            for ne in in_neighbors {
                let pred_id = ne.node;
                if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                    let pred_d = pred_d as usize;
                    if dist_vals[pred_d] == Some(cur_dist - 1) {
                        path.push(pred_d);
                        cur = pred_d;
                        moved = true;
                        break;
                    }
                }
            }
            if !moved {
                return Ok(None);
            }
        }

        path.reverse();
        Ok(Some(
            path.into_iter().map(|d| snap.dense_to_id[d]).collect(),
        ))
    }

    /// Dijkstra weighted shortest path from `src` to `dst` using MinPlus SpMV on `m.weight_matrix`.
    pub(in crate::graph) fn shortest_path_dijkstra_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<WeightedPath>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector,
                operations::{
                    GetSparseVectorElementIndices, GetSparseVectorElementValue,
                    SetSparseVectorElement,
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Min,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        if src == dst {
            return Ok(Some(WeightedPath {
                nodes: vec![src],
                total_weight: 0.0,
            }));
        }

        let n = m.n_nodes;
        let src_dense = match snap.id_to_dense.get(&src) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };
        let dst_dense = match snap.id_to_dense.get(&dst) {
            Some(&d) => d as usize,
            None => return Ok(None),
        };

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_min = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, false, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<f64>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0.0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mut dist_vals: Vec<Option<f64>> = vec![None; n];
        dist_vals[src_dense] = Some(0.0);

        for _ in 1..=n {
            let mut next = SparseVector::<f64>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.weight_matrix,
                &MinPlus::<f64>::new(),
                &dist,
                &Assignment::new(),
                &mut next,
                &SelectEntireVector::new(m.context.clone()),
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut merged = SparseVector::<f64>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &dist,
                    &Min::<f64>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let new_indices = merged
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let old_indices = dist
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut changed = new_indices.len() != old_indices.len();
            if !changed {
                for &idx in &new_indices {
                    let new_v = merged
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    let old_v = dist
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    if (new_v - old_v).abs() > 1e-9 {
                        changed = true;
                        break;
                    }
                }
            }

            dist = merged;
            if !changed {
                break;
            }
        }

        let indices = dist
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        for idx in indices {
            let v = dist
                .element_value_or_default(idx)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist_vals[idx] = Some(v);
        }

        let total_cost = match dist_vals[dst_dense] {
            Some(c) => c,
            None => return Ok(None),
        };

        let mut path = vec![dst_dense];
        let mut cur = dst_dense;
        while cur != src_dense {
            let cur_dist = match dist_vals[cur] {
                Some(d) => d,
                None => return Ok(None),
            };
            let cur_id = snap.dense_to_id[cur];
            let in_neighbors = self.adj_entries(cur_id, false)?;
            let mut moved = false;
            for ne in in_neighbors {
                let (pred_id, edge_id) = (ne.node, ne.edge);
                if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                    let pred_d = pred_d as usize;
                    if let Some(pred_dist) = dist_vals[pred_d] {
                        let rtxn = self.storage.env.read_txn()?;
                        let weight = if let Some(rec) = self.get_edge_impl(&rtxn, edge_id)? {
                            let props_json: serde_json::Value = props::decode(&rec.props)?;
                            props_json
                                .get("weight")
                                .or_else(|| props_json.get("cost"))
                                .or_else(|| props_json.get("capacity"))
                                .or_else(|| props_json.get("cap"))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(1.0)
                        } else {
                            1.0
                        };
                        if (pred_dist + weight - cur_dist).abs() < 1e-9 {
                            path.push(pred_d);
                            cur = pred_d;
                            moved = true;
                            break;
                        }
                    }
                }
            }
            if !moved {
                return Ok(None);
            }
        }

        path.reverse();
        Ok(Some(WeightedPath {
            nodes: path.into_iter().map(|d| snap.dense_to_id[d]).collect(),
            total_weight: total_cost,
        }))
    }

    /// Depth-First Search (DFS) optimized over contiguous CSR snapshot arrays.
    pub(in crate::graph) fn dfs_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        start: NodeId,
        hops: u8,
    ) -> Result<Vec<NodeId>, Error> {
        // Track the shallowest depth at which each node has been reached. A
        // plain visited set would under-report: a node first discovered via a
        // longer branch gets pruned, so nodes that are within `hops` along a
        // shorter path (and their deeper neighbors) would be missed. Re-expand
        // whenever a node is reached at a strictly shallower depth so the result
        // is every node within `hops`, in DFS discovery order.
        let mut best_depth: AHashMap<NodeId, u8> = AHashMap::new();
        let mut order: Vec<NodeId> = Vec::new();

        fn dfs_recurse(
            snap: &CsrSnapshot,
            node: NodeId,
            depth: u8,
            max_depth: u8,
            best_depth: &mut AHashMap<NodeId, u8>,
            order: &mut Vec<NodeId>,
        ) {
            match best_depth.get(&node) {
                Some(&d) if d <= depth => return,
                Some(_) => {}
                None => order.push(node),
            }
            best_depth.insert(node, depth);

            if depth < max_depth {
                if let Some(dense) = snap.id_to_dense.get(&node) {
                    let start_idx = snap.row_ptr[*dense as usize];
                    let end_idx = snap.row_ptr[*dense as usize + 1];
                    for k in start_idx..end_idx {
                        let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                        dfs_recurse(snap, neighbor, depth + 1, max_depth, best_depth, order);
                    }
                }
            }
        }

        dfs_recurse(snap, start, 0, hops, &mut best_depth, &mut order);
        Ok(order)
    }

    /// Directed Cycle Detection using 3-color DFS over contiguous CSR snapshot arrays.
    pub(in crate::graph) fn detect_cycle_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<bool, Error> {
        let n = snap.dense_to_id.len();
        let mut state = vec![0u8; n]; // 0 = White, 1 = Gray, 2 = Black

        fn has_cycle(snap: &CsrSnapshot, u: usize, state: &mut Vec<u8>) -> bool {
            state[u] = 1; // Gray

            let start = snap.row_ptr[u];
            let end = snap.row_ptr[u + 1];
            for k in start..end {
                let v = snap.col_idx[k] as usize;
                if state[v] == 1 || (state[v] == 0 && has_cycle(snap, v, state)) {
                    return true;
                }
            }

            state[u] = 2; // Black
            false
        }

        for u in 0..n {
            if state[u] == 0 && has_cycle(snap, u, &mut state) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// All simple paths between `src` and `dst` using contiguous CSR snapshot arrays.
    pub(in crate::graph) fn all_paths_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
        let mut paths = Vec::new();
        let mut current_path = vec![src];
        let mut visited = AHashSet::new();
        visited.insert(src);

        fn find_paths(
            snap: &CsrSnapshot,
            u: NodeId,
            dst: NodeId,
            visited: &mut AHashSet<NodeId>,
            current_path: &mut Vec<NodeId>,
            paths: &mut Vec<Vec<NodeId>>,
        ) {
            if u == dst {
                paths.push(current_path.clone());
                return;
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let start = snap.row_ptr[u_dense as usize];
                let end = snap.row_ptr[u_dense as usize + 1];
                for k in start..end {
                    let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                    if !visited.contains(&neighbor) {
                        visited.insert(neighbor);
                        current_path.push(neighbor);
                        find_paths(snap, neighbor, dst, visited, current_path, paths);
                        current_path.pop();
                        visited.remove(&neighbor);
                    }
                }
            }
        }

        find_paths(snap, src, dst, &mut visited, &mut current_path, &mut paths);
        Ok(paths)
    }

    /// All unweighted shortest paths between `src` and `dst` using MinPlus SpMV distances.
    pub(in crate::graph) fn all_shortest_paths_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Vec<Vec<NodeId>>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                Collection,
                sparse_vector::{
                    SparseVector,
                    operations::{GetSparseVectorElementIndices, SetSparseVectorElement},
                },
            },
            operators::{
                binary_operator::Assignment,
                element_wise_addition::{
                    ApplyElementWiseVectorAdditionMonoidOperator,
                    ElementWiseVectorAdditionMonoidOperator,
                },
                mask::SelectEntireVector,
                monoid::Plus,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::{OperatorOptions, OptionsForOperatorWithMatrixAsFirstArgument},
                semiring::MinPlus,
            },
        };

        if src == dst {
            return Ok(vec![vec![src]]);
        }

        let n = m.n_nodes;
        let src_dense = match snap.id_to_dense.get(&src) {
            Some(&d) => d as usize,
            None => return Ok(vec![]),
        };
        let dst_dense = match snap.id_to_dense.get(&dst) {
            Some(&d) => d as usize,
            None => return Ok(vec![]),
        };

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        dist.set_value(src_dense, 0)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mut dist_vals: Vec<Option<i32>> = vec![None; n];
        dist_vals[src_dense] = Some(0);

        let mut reached_dst = false;

        for hop in 1..=(n as i32) {
            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinPlus::<i32>::new(),
                &dist,
                &Assignment::new(),
                &mut next,
                &dist,
                &opts_next,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            if next
                .number_of_stored_elements()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                == 0
            {
                break;
            }

            let new_indices: Vec<usize> = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for &idx in &new_indices {
                dist_vals[idx] = Some(hop);
                if idx == dst_dense {
                    reached_dst = true;
                }
            }

            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add
                .apply(
                    &dist,
                    &Plus::<i32>::new(),
                    &next,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist = merged;

            if reached_dst {
                break;
            }
        }

        if !reached_dst {
            return Ok(vec![]);
        }

        let mut paths = Vec::new();
        let mut current_path = vec![dst];

        fn reconstruct(
            graph: &Graph,
            snap: &CsrSnapshot,
            u: NodeId,
            src: NodeId,
            dist_vals: &[Option<i32>],
            current_path: &mut Vec<NodeId>,
            paths: &mut Vec<Vec<NodeId>>,
        ) -> Result<(), Error> {
            if u == src {
                let mut p = current_path.clone();
                p.reverse();
                paths.push(p);
                return Ok(());
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let cur_dist = match dist_vals[u_dense as usize] {
                    Some(d) => d,
                    None => return Ok(()),
                };
                let in_neighbors = graph.adj_entries(u, false)?;
                for ne in in_neighbors {
                    let pred_id = ne.node;
                    if let Some(&pred_d) = snap.id_to_dense.get(&pred_id) {
                        if dist_vals[pred_d as usize] == Some(cur_dist - 1) {
                            current_path.push(pred_id);
                            reconstruct(graph, snap, pred_id, src, dist_vals, current_path, paths)?;
                            current_path.pop();
                        }
                    }
                }
            }
            Ok(())
        }

        reconstruct(
            self,
            snap,
            dst,
            src,
            &dist_vals,
            &mut current_path,
            &mut paths,
        )?;
        Ok(paths)
    }

    /// Yen's K Shortest Paths algorithm optimized over contiguous CSR snapshot arrays.
    pub(in crate::graph) fn shortest_path_top_k_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
        k: usize,
        weight_property: &str,
    ) -> Result<Vec<(Vec<NodeId>, f64)>, Error> {
        if k == 0 {
            return Ok(vec![]);
        }

        let rtxn = self.storage.env.read_txn()?;

        let find_shortest_path = |s: NodeId,
                                  t: NodeId,
                                  blocked_nodes: &AHashSet<NodeId>,
                                  blocked_edges: &AHashSet<(NodeId, NodeId)>|
         -> Result<Option<(Vec<NodeId>, f64)>, Error> {
            if s == t {
                return Ok(Some((vec![s], 0.0)));
            }

            use std::cmp::Ordering;

            #[derive(Debug, PartialEq)]
            struct MinNonNan(f64);

            impl Eq for MinNonNan {}

            impl PartialOrd for MinNonNan {
                fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                    Some(self.cmp(other))
                }
            }

            impl Ord for MinNonNan {
                fn cmp(&self, other: &Self) -> Ordering {
                    self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
                }
            }

            #[derive(Debug, PartialEq, Eq)]
            struct State {
                cost: std::cmp::Reverse<MinNonNan>,
                node: NodeId,
            }

            impl Ord for State {
                fn cmp(&self, other: &Self) -> Ordering {
                    self.cost.cmp(&other.cost)
                }
            }

            impl PartialOrd for State {
                fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                    Some(self.cmp(other))
                }
            }

            let mut dist: HashMap<NodeId, f64> = HashMap::new();
            let mut pred: HashMap<NodeId, NodeId> = HashMap::new();
            let mut heap = std::collections::BinaryHeap::new();

            dist.insert(s, 0.0);
            heap.push(State {
                cost: std::cmp::Reverse(MinNonNan(0.0)),
                node: s,
            });

            while let Some(State {
                cost: std::cmp::Reverse(MinNonNan(cost)),
                node,
            }) = heap.pop()
            {
                if node == t {
                    let mut path = vec![t];
                    let mut cur = t;
                    while cur != s {
                        cur = pred[&cur];
                        path.push(cur);
                    }
                    path.reverse();
                    return Ok(Some((path, cost)));
                }

                if cost > *dist.get(&node).unwrap_or(&f64::INFINITY) {
                    continue;
                }

                if let Some(&node_dense) = snap.id_to_dense.get(&node) {
                    let start = snap.row_ptr[node_dense as usize];
                    let end = snap.row_ptr[node_dense as usize + 1];
                    for k in start..end {
                        let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                        let edge_id = snap.edge_id[k];
                        if blocked_nodes.contains(&neighbor) {
                            continue;
                        }
                        if blocked_edges.contains(&(node, neighbor)) {
                            continue;
                        }

                        let weight = if let Some(edge_record) =
                            self.get_edge_impl(&rtxn, edge_id)?
                        {
                            let props_json: serde_json::Value = props::decode(&edge_record.props)?;
                            if let Some(val) = props_json.get(weight_property) {
                                val.as_f64().unwrap_or(1.0)
                            } else {
                                1.0
                            }
                        } else {
                            1.0
                        };

                        let next_cost = cost + weight;
                        let current_best = *dist.get(&neighbor).unwrap_or(&f64::INFINITY);

                        if next_cost < current_best {
                            dist.insert(neighbor, next_cost);
                            pred.insert(neighbor, node);
                            heap.push(State {
                                cost: std::cmp::Reverse(MinNonNan(next_cost)),
                                node: neighbor,
                            });
                        }
                    }
                }
            }

            Ok(None)
        };

        let first_path_opt = find_shortest_path(src, dst, &AHashSet::new(), &AHashSet::new())?;
        let mut paths = Vec::new();
        if let Some((first_path, first_cost)) = first_path_opt {
            paths.push((first_path, first_cost));
        } else {
            return Ok(vec![]);
        }

        let mut candidates: Vec<(Vec<NodeId>, f64)> = Vec::new();

        for i in 1..k {
            let prev_path = &paths[i - 1].0;

            for j in 0..prev_path.len() - 1 {
                let spur_node = prev_path[j];
                let root_path = &prev_path[0..=j];

                let mut blocked_edges = AHashSet::new();
                let mut blocked_nodes = AHashSet::new();

                for (p, _) in &paths {
                    if p.len() > j && &p[0..=j] == root_path {
                        blocked_edges.insert((p[j], p[j + 1]));
                    }
                }

                for &node in root_path {
                    if node != spur_node {
                        blocked_nodes.insert(node);
                    }
                }

                let spur_path_opt =
                    find_shortest_path(spur_node, dst, &blocked_nodes, &blocked_edges)?;
                if let Some((spur_path, spur_cost)) = spur_path_opt {
                    let mut total_path = root_path.to_vec();
                    total_path.extend_from_slice(&spur_path[1..]);

                    let mut root_cost = 0.0;
                    for m_idx in 0..root_path.len() - 1 {
                        let u = root_path[m_idx];
                        let v = root_path[m_idx + 1];
                        let mut min_w = f64::INFINITY;
                        if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                            let start = snap.row_ptr[u_dense as usize];
                            let end = snap.row_ptr[u_dense as usize + 1];
                            for k_idx in start..end {
                                let neighbor = snap.dense_to_id[snap.col_idx[k_idx] as usize];
                                let edge_id = snap.edge_id[k_idx];
                                if neighbor == v {
                                    let weight = if let Some(edge_record) =
                                        self.get_edge_impl(&rtxn, edge_id)?
                                    {
                                        let props_json: serde_json::Value =
                                            props::decode(&edge_record.props)?;
                                        if let Some(val) = props_json.get(weight_property) {
                                            val.as_f64().unwrap_or(1.0)
                                        } else {
                                            1.0
                                        }
                                    } else {
                                        1.0
                                    };
                                    if weight < min_w {
                                        min_w = weight;
                                    }
                                }
                            }
                        }
                        if min_w == f64::INFINITY {
                            root_cost += 1.0;
                        } else {
                            root_cost += min_w;
                        }
                    }

                    let total_cost = root_cost + spur_cost;
                    if !candidates.iter().any(|(p, _)| p == &total_path) {
                        candidates.push((total_path, total_cost));
                    }
                }
            }

            if candidates.is_empty() {
                break;
            }

            candidates.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.0.cmp(&a.0))
            });

            if let Some(best_cand) = candidates.pop() {
                paths.push(best_cand);
            } else {
                break;
            }
        }

        Ok(paths)
    }

    /// Longest Path between `src` and `dst` using contiguous CSR snapshot arrays.
    pub(in crate::graph) fn longest_path_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        let mut max_path: Option<Vec<NodeId>> = None;
        let mut current_path = vec![src];
        let mut visited = AHashSet::new();
        visited.insert(src);

        fn find_longest(
            snap: &CsrSnapshot,
            u: NodeId,
            dst: NodeId,
            visited: &mut AHashSet<NodeId>,
            current_path: &mut Vec<NodeId>,
            max_path: &mut Option<Vec<NodeId>>,
        ) {
            if u == dst {
                if let Some(max) = max_path.as_ref() {
                    if current_path.len() > max.len() {
                        *max_path = Some(current_path.clone());
                    }
                } else {
                    *max_path = Some(current_path.clone());
                }
                return;
            }

            if let Some(&u_dense) = snap.id_to_dense.get(&u) {
                let start = snap.row_ptr[u_dense as usize];
                let end = snap.row_ptr[u_dense as usize + 1];
                for k in start..end {
                    let neighbor = snap.dense_to_id[snap.col_idx[k] as usize];
                    if !visited.contains(&neighbor) {
                        visited.insert(neighbor);
                        current_path.push(neighbor);
                        find_longest(snap, neighbor, dst, visited, current_path, max_path);
                        current_path.pop();
                        visited.remove(&neighbor);
                    }
                }
            }
        }

        find_longest(
            snap,
            src,
            dst,
            &mut visited,
            &mut current_path,
            &mut max_path,
        );
        Ok(max_path)
    }
}
