use super::*;

impl Graph {
    /// PageRank via iterative SpMV over the column-stochastic matrix.
    ///
    /// Each iteration computes `raw = M * rank` using PlusTimes, then applies the
    /// damping formula `rank[i] = d * raw[i] + (1 - d) / n` in Rust. Dangling
    /// nodes (no incoming edges) receive only the teleportation term.
    #[doc(hidden)]
    pub fn page_rank_graphblas(
        &self,
        iterations: u32,
        damping: f32,
    ) -> Result<HashMap<NodeId, f32>, Error> {
        use issundb_graphblas::{Descriptor, Reducer, Semiring, Vector, mxv};

        let guard = self.matrices.read();
        let m = match guard.as_ref() {
            Some(m) => m,
            None => return self.page_rank(iterations, damping),
        };
        let snap = self.csr_cache.snapshot.load();
        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let init = 1.0f32 / n as f32;
        let base = (1.0 - damping) / n as f32;

        let mut rank_vals = vec![init; n];

        for _ in 0..iterations {
            let rank_pairs: Vec<(usize, f32)> =
                rank_vals.iter().enumerate().map(|(i, &v)| (i, v)).collect();
            let rank = Vector::<f32>::from_pairs(m.context.clone(), n, &rank_pairs, Reducer::First)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut raw = Vector::<f32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut raw,
                None,
                Semiring::PlusTimes,
                &m.page_rank_matrix,
                &rank,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            // Apply damping: rank[i] = d * raw[i] + (1-d)/n; absent entries get base only.
            let mut new_vals = vec![base; n];
            let indices: Vec<usize> = raw.indices().map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = raw
                    .get_or_default(idx)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                new_vals[idx] = damping * v + base;
            }
            rank_vals = new_vals;
        }

        Ok((0..n)
            .filter_map(|i| snap.dense_to_id.get(i).map(|&id| (id, rank_vals[i])))
            .collect())
    }

    /// Connected Components (WCC) using iterative label-propagation via SpMV.
    pub(in crate::graph) fn connected_components_graphblas(
        &self,
        m: &MatrixSet,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use issundb_graphblas::{Descriptor, Monoid, Reducer, Semiring, Vector, ewise_add, mxv};

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        // Labels are 1-based: node at dense index `i` starts with label `i + 1`.
        // The semiring is MinSecond so the SpMV propagates each neighbor's label
        // (the vector entry), not the adjacency matrix value: fwd[i] is the
        // minimum label over i's neighbors. A 0-based label would give index 0
        // the value 0, which the sparse Min monoid treats as an implicit zero,
        // so index 0's label would be dropped from storage and would never
        // propagate and the node would be stranded in its own component. Readout
        // subtracts 1 to recover the 0-based representative index.
        let init_pairs: Vec<(usize, i32)> = (0..n).map(|i| (i, (i + 1) as i32)).collect();
        let mut label =
            Vector::<i32>::from_pairs(m.context.clone(), n, &init_pairs, Reducer::First)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        for _ in 0..n {
            let mut fwd = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut fwd,
                None,
                Semiring::MinSecond,
                &m.adjacency,
                &label,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut rev = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut rev,
                None,
                Semiring::MinSecond,
                &m.adjacency_t,
                &label,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut merged = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add(&mut merged, None, Monoid::Min, &fwd, &rev, Descriptor::NULL)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut next = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_add(
                &mut next,
                None,
                Monoid::Min,
                &label,
                &merged,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let new_indices = next
                .indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let old_indices = label
                .indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let mut changed = new_indices.len() != old_indices.len();
            if !changed {
                for &idx in &new_indices {
                    let new_v = next
                        .get_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    let old_v = label
                        .get_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    if new_v != old_v {
                        changed = true;
                        break;
                    }
                }
            }

            label = next;
            if !changed {
                break;
            }
        }

        let indices = label
            .indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let mut result = HashMap::with_capacity(n);
        for idx in indices {
            // Undo the 1-based offset applied at initialization.
            let comp = (label
                .get_or_default(idx)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?
                - 1) as u64;
            if let Some(&node_id) = m.dense_to_id.get(idx) {
                result.insert(node_id, comp);
            }
        }
        Ok(result)
    }

    /// Strongly Connected Components (SCC) using Tarjan's algorithm optimized over contiguous CSR snapshot arrays.
    pub(in crate::graph) fn strongly_connected_components_graphblas(
        &self,
        _m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(HashMap::new());
        }

        struct Env<'a> {
            snap: &'a CsrSnapshot,
            index: usize,
            indices: Vec<Option<usize>>,
            lowlinks: Vec<usize>,
            on_stack: Vec<bool>,
            stack: Vec<usize>,
            components: HashMap<NodeId, u64>,
            next_comp_id: u64,
        }

        let mut env = Env {
            snap,
            index: 0,
            indices: vec![None; n],
            lowlinks: vec![0; n],
            on_stack: vec![false; n],
            stack: Vec::with_capacity(n),
            components: HashMap::with_capacity(n),
            next_comp_id: 0,
        };

        fn strongconnect(u: usize, env: &mut Env) {
            env.indices[u] = Some(env.index);
            env.lowlinks[u] = env.index;
            env.index += 1;
            env.stack.push(u);
            env.on_stack[u] = true;

            let start = env.snap.row_ptr[u];
            let end = env.snap.row_ptr[u + 1];
            for k in start..end {
                let v = env.snap.col_idx[k] as usize;
                if env.indices[v].is_none() {
                    strongconnect(v, env);
                    env.lowlinks[u] = std::cmp::min(env.lowlinks[u], env.lowlinks[v]);
                } else if env.on_stack[v] {
                    if let Some(iv) = env.indices[v] {
                        env.lowlinks[u] = std::cmp::min(env.lowlinks[u], iv);
                    }
                }
            }

            if Some(env.lowlinks[u]) == env.indices[u] {
                let comp_id = env.next_comp_id;
                env.next_comp_id += 1;

                while let Some(w) = env.stack.pop() {
                    env.on_stack[w] = false;
                    if let Some(&node_id) = env.snap.dense_to_id.get(w) {
                        env.components.insert(node_id, comp_id);
                    }
                    if w == u {
                        break;
                    }
                }
            }
        }

        for u in 0..n {
            if env.indices[u].is_none() {
                strongconnect(u, &mut env);
            }
        }

        Ok(env.components)
    }

    /// Betweenness Centrality (Brandes' algorithm) using SpMV BFS frontier exploration.
    pub(in crate::graph) fn betweenness_centrality_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        use issundb_graphblas::{Descriptor, Monoid, Semiring, Vector, ewise_add, mxv};

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let opts_next = Descriptor::new(false, true, true, true);

        let mut betweenness = vec![0.0f64; n];

        for s in 0..n {
            let mut dist = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set(s, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut sigma = vec![0u64; n];
            sigma[s] = 1;
            let mut levels: Vec<Vec<usize>> = vec![vec![s]];
            let mut pred: Vec<Vec<usize>> = vec![vec![]; n];
            let mut dist_vals: Vec<Option<i32>> = vec![None; n];
            dist_vals[s] = Some(0);

            for hop in 1..=(n as i32) {
                let mut next = Vector::<i32>::new(m.context.clone(), n)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                mxv(
                    &mut next,
                    Some(&dist),
                    Semiring::MinPlus,
                    &m.adjacency,
                    &dist,
                    opts_next,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

                let new_count = next.nvals().map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .indices()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;

                let mut level_nodes = Vec::with_capacity(new_indices.len());
                for &w in &new_indices {
                    dist_vals[w] = Some(hop);
                    level_nodes.push(w);
                }

                for &w in &new_indices {
                    if let Some(prev_level) = levels.last() {
                        for &v in prev_level {
                            let start = snap.row_ptr[v];
                            let end = snap.row_ptr[v + 1];
                            for k in start..end {
                                if snap.col_idx[k] as usize == w {
                                    sigma[w] += sigma[v];
                                    pred[w].push(v);
                                    break;
                                }
                            }
                        }
                    }
                }

                levels.push(level_nodes);

                let mut merged = Vector::<i32>::new(m.context.clone(), n)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                ewise_add(
                    &mut merged,
                    None,
                    Monoid::Plus,
                    &dist,
                    &next,
                    Descriptor::NULL,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                dist = merged;
            }

            let mut delta = vec![0.0f64; n];
            for level in levels.iter().rev() {
                for &w in level {
                    if w == s {
                        continue;
                    }
                    let dw = delta[w];
                    for &v in &pred[w] {
                        if sigma[w] > 0 {
                            delta[v] += sigma[v] as f64 / sigma[w] as f64 * (1.0 + dw);
                        }
                    }
                    betweenness[w] += dw;
                }
            }
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, betweenness[d]))
            .collect())
    }

    /// Harmonic Centrality using all-pairs BFS distances computed via MinPlus SpMV.
    #[allow(clippy::needless_range_loop)]
    pub(in crate::graph) fn harmonic_centrality_graphblas(
        &self,
        m: &MatrixSet,
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, f64>, Error> {
        use issundb_graphblas::{Descriptor, Monoid, Semiring, Vector, ewise_add, mxv};

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let opts_next = Descriptor::new(false, true, true, true);

        let mut centrality = vec![0.0f64; n];

        for src_dense in 0..n {
            let mut dist = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set(src_dense, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut local_sum = 0.0f64;

            for hop in 1..=(n as i32) {
                let mut next = Vector::<i32>::new(m.context.clone(), n)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                mxv(
                    &mut next,
                    Some(&dist),
                    Semiring::MinPlus,
                    &m.adjacency,
                    &dist,
                    opts_next,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

                let new_count = next.nvals().map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .indices()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                for _ in &new_indices {
                    local_sum += 1.0 / hop as f64;
                }

                let mut merged = Vector::<i32>::new(m.context.clone(), n)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                ewise_add(
                    &mut merged,
                    None,
                    Monoid::Plus,
                    &dist,
                    &next,
                    Descriptor::NULL,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                dist = merged;
            }

            centrality[src_dense] = local_sum;
        }

        Ok(snap
            .dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, centrality[d]))
            .collect())
    }

    /// Degree Centrality via row/column reduces utilizing SpMV with standard arithmetic semiring.
    pub(in crate::graph) fn degree_centrality_graphblas(
        &self,
        m: &MatrixSet,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use issundb_graphblas::{Descriptor, Matrix, Reducer, Semiring, Vector, mxv};

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let ones_pairs: Vec<(usize, i32)> = (0..n).map(|i| (i, 1i32)).collect();
        let ones = Vector::<i32>::from_pairs(m.context.clone(), n, &ones_pairs, Reducer::First)
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let compute_degree = |matrix: &Matrix<i32>| -> Result<Vec<u64>, Error> {
            let mut out = Vector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv(
                &mut out,
                None,
                Semiring::PlusTimes,
                matrix,
                &ones,
                Descriptor::NULL,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut degrees = vec![0u64; n];
            let indices = out.indices().map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = out
                    .get_or_default(idx)
                    .map_err(|e| Error::GraphBLAS(e.to_string()))? as u64;
                degrees[idx] = v;
            }
            Ok(degrees)
        };

        let out_degrees = if matches!(direction, DegreeDirection::Out | DegreeDirection::Both) {
            compute_degree(&m.adjacency)?
        } else {
            vec![0; n]
        };

        let in_degrees = if matches!(direction, DegreeDirection::In | DegreeDirection::Both) {
            compute_degree(&m.adjacency_t)?
        } else {
            vec![0; n]
        };

        let mut result = HashMap::with_capacity(n);
        for (dense, &node_id) in m.dense_to_id.iter().enumerate() {
            let count = match direction {
                DegreeDirection::Out => out_degrees[dense],
                DegreeDirection::In => in_degrees[dense],
                DegreeDirection::Both => out_degrees[dense] + in_degrees[dense],
            };
            result.insert(node_id, count);
        }
        Ok(result)
    }

    /// Community Detection via Label Propagation (CDLP / LPA).
    pub(in crate::graph) fn label_propagation_graphblas(
        &self,
        _m: &MatrixSet,
        _snap: &CsrSnapshot,
        max_iterations: usize,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        let nodes = self.all_nodes()?;
        let mut labels: HashMap<NodeId, u64> = nodes.iter().map(|&n| (n, n)).collect();

        for _ in 0..max_iterations {
            let mut next_labels = labels.clone();
            let mut changed = false;

            for &u in &nodes {
                let neighbors = self.all_neighbors(u)?;
                if neighbors.is_empty() {
                    continue;
                }

                let mut counts: HashMap<u64, usize> = HashMap::new();
                for ne in &neighbors {
                    if let Some(&label) = labels.get(&ne.node) {
                        *counts.entry(label).or_insert(0) += 1;
                    }
                }

                let mut max_label = labels[&u];
                let mut max_count = 0;

                for (&label, &count) in &counts {
                    if count > max_count {
                        max_count = count;
                        max_label = label;
                    } else if count == max_count && label < max_label {
                        max_label = label;
                    }
                }

                if max_label != labels[&u] {
                    next_labels.insert(u, max_label);
                    changed = true;
                }
            }

            labels = next_labels;
            if !changed {
                break;
            }
        }

        Ok(labels)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use tempfile::TempDir;

    use super::*;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// Connect every unordered pair among `nodes` with a single directed edge.
    /// `all_neighbors` is direction-agnostic, so one edge per pair yields a
    /// symmetric neighbor relation, i.e. an undirected clique.
    fn add_clique(g: &Graph, nodes: &[NodeId]) {
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                g.add_edge(nodes[i], nodes[j], "E", &()).unwrap();
            }
        }
    }

    /// Canonicalize a label map into sorted groups of node IDs. Only the induced
    /// partition is significant, not the label values.
    fn partition(labels: &HashMap<NodeId, u64>) -> Vec<Vec<NodeId>> {
        let mut groups: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for (&id, &label) in labels {
            groups.entry(label).or_default().push(id);
        }
        let mut parts: Vec<Vec<NodeId>> = groups
            .into_values()
            .map(|mut part| {
                part.sort_unstable();
                part
            })
            .collect();
        parts.sort();
        parts
    }

    /// LPA cannot be compared against NetworkX, whose label propagation is
    /// randomized and yields no canonical partition. Instead these tests pin the
    /// invariants the deterministic implementation must satisfy.
    ///
    /// Three disjoint triangles must collapse to exactly three communities, one
    /// per triangle, matching the weakly connected components. A clique of size
    /// three or more contains an odd cycle, so the synchronous update converges;
    /// a two-clique is bipartite and would oscillate, so triangles are the
    /// smallest safe building block.
    #[test]
    fn label_propagation_resolves_disjoint_cliques_to_components() {
        let (_dir, g) = open_tmp();
        let mut triangles = Vec::new();
        for _ in 0..3 {
            let nodes: Vec<NodeId> = (0..3).map(|_| g.add_node("N", &()).unwrap()).collect();
            add_clique(&g, &nodes);
            triangles.push(nodes);
        }
        g.rebuild_csr().unwrap();

        let labels = g.label_propagation(100).unwrap();
        assert_eq!(
            partition(&labels),
            partition(&g.connected_components().unwrap()),
            "community partition must match the connected components"
        );

        let distinct: HashSet<u64> = labels.values().copied().collect();
        assert_eq!(distinct.len(), 3, "expected one community per triangle");
        for tri in &triangles {
            let label = labels[&tri[0]];
            assert!(
                tri.iter().all(|n| labels[n] == label),
                "a triangle was split across communities"
            );
        }
    }

    /// The implementation iterates a `HashMap` of neighbor label counts, whose
    /// order is randomized per process, but breaks ties toward the smallest
    /// label. The result must therefore be identical run to run.
    #[test]
    fn label_propagation_is_deterministic() {
        let (_dir, g) = open_tmp();
        let nodes: Vec<NodeId> = (0..6).map(|_| g.add_node("N", &()).unwrap()).collect();
        add_clique(&g, &nodes[0..3]);
        add_clique(&g, &nodes[3..6]);
        g.add_edge(nodes[2], nodes[3], "E", &()).unwrap();
        g.rebuild_csr().unwrap();

        let first = g.label_propagation(100).unwrap();
        let second = g.label_propagation(100).unwrap();
        assert_eq!(first, second, "label propagation must be run-to-run stable");
    }
}
