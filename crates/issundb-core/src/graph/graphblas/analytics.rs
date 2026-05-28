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
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector, VectorElementList,
                operations::{
                    FromVectorElementList, GetSparseVectorElementIndices,
                    GetSparseVectorElementValue,
                },
            },
            operators::{
                binary_operator::{Assignment, First},
                mask::SelectEntireVector,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::OptionsForOperatorWithMatrixAsFirstArgument,
                semiring::PlusTimes,
            },
        };

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
        let mxv = MatrixVectorMultiplicationOperator::new();
        let opts = OptionsForOperatorWithMatrixAsFirstArgument::new_default();

        let mut rank_vals = vec![init; n];

        for _ in 0..iterations {
            let rank_list = VectorElementList::<f32>::from_element_vector(
                rank_vals
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| (i, v).into())
                    .collect(),
            );
            let rank = SparseVector::<f32>::from_element_list(
                m.context.clone(),
                n,
                rank_list,
                &First::<f32>::new(),
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut raw = SparseVector::<f32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.page_rank_matrix,
                &PlusTimes::<f32>::new(),
                &rank,
                &Assignment::new(),
                &mut raw,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            // Apply damping: rank[i] = d * raw[i] + (1-d)/n; absent entries get base only.
            let mut new_vals = vec![base; n];
            let indices: Vec<usize> = raw
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = raw
                    .element_value_or_default(idx)
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
        snap: &CsrSnapshot,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::sparse_vector::{
                SparseVector, VectorElementList,
                operations::{
                    FromVectorElementList, GetSparseVectorElementIndices,
                    GetSparseVectorElementValue,
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
                semiring::MinFirst,
            },
        };

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let init_list = VectorElementList::<i32>::from_element_vector(
            (0..n).map(|i| (i, i as i32).into()).collect(),
        );
        let mut label = SparseVector::<i32>::from_element_list(
            m.context.clone(),
            n,
            init_list,
            &graphblas_sparse_linear_algebra::operators::binary_operator::First::<i32>::new(),
        )
        .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_min = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_fwd = OptionsForOperatorWithMatrixAsFirstArgument::new_default();
        let opts_rev = OptionsForOperatorWithMatrixAsFirstArgument::new_default();
        let opts_merge = OperatorOptions::new_default();

        for _ in 0..n {
            let mut fwd = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency,
                &MinFirst::<i32>::new(),
                &label,
                &Assignment::new(),
                &mut fwd,
                &SelectEntireVector::new(m.context.clone()),
                &opts_fwd,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut rev = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                &m.adjacency_t,
                &MinFirst::<i32>::new(),
                &label,
                &Assignment::new(),
                &mut rev,
                &SelectEntireVector::new(m.context.clone()),
                &opts_rev,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut merged = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &fwd,
                    &Min::<i32>::new(),
                    &rev,
                    &Assignment::new(),
                    &mut merged,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut next = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            ewise_min
                .apply(
                    &label,
                    &Min::<i32>::new(),
                    &merged,
                    &Assignment::new(),
                    &mut next,
                    &SelectEntireVector::new(m.context.clone()),
                    &opts_merge,
                )
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let new_indices = next
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let old_indices = label
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            let mut changed = new_indices.len() != old_indices.len();
            if !changed {
                for &idx in &new_indices {
                    let new_v = next
                        .element_value_or_default(idx)
                        .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                    let old_v = label
                        .element_value_or_default(idx)
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
            .element_indices()
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;
        let mut result = HashMap::with_capacity(n);
        for idx in indices {
            let comp = label
                .element_value_or_default(idx)
                .map_err(|e| Error::GraphBLAS(e.to_string()))? as u64;
            if let Some(&node_id) = snap.dense_to_id.get(idx) {
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

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut betweenness = vec![0.0f64; n];

        for s in 0..n {
            let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set_value(s, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut sigma = vec![0u64; n];
            sigma[s] = 1;
            let mut levels: Vec<Vec<usize>> = vec![vec![s]];
            let mut pred: Vec<Vec<usize>> = vec![vec![]; n];
            let mut dist_vals: Vec<Option<i32>> = vec![None; n];
            dist_vals[s] = Some(0);

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

                let new_count = next
                    .number_of_stored_elements()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .element_indices()
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

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let mxv = MatrixVectorMultiplicationOperator::new();
        let ewise_add = ElementWiseVectorAdditionMonoidOperator::new();
        let opts_next = OptionsForOperatorWithMatrixAsFirstArgument::new(false, true, true, true);
        let opts_merge = OperatorOptions::new_default();

        let mut centrality = vec![0.0f64; n];

        for src_dense in 0..n {
            let mut dist = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            dist.set_value(src_dense, 0)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut local_sum = 0.0f64;

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

                let new_count = next
                    .number_of_stored_elements()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                if new_count == 0 {
                    break;
                }

                let new_indices = next
                    .element_indices()
                    .map_err(|e| Error::GraphBLAS(e.to_string()))?;
                for _ in &new_indices {
                    local_sum += 1.0 / hop as f64;
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
        snap: &CsrSnapshot,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        use graphblas_sparse_linear_algebra::{
            collections::{
                sparse_matrix::SparseMatrix,
                sparse_vector::{
                    SparseVector, VectorElementList,
                    operations::{
                        FromVectorElementList, GetSparseVectorElementIndices,
                        GetSparseVectorElementValue,
                    },
                },
            },
            operators::{
                binary_operator::{Assignment, First},
                mask::SelectEntireVector,
                multiplication::{MatrixVectorMultiplicationOperator, MultiplyMatrixByVector},
                options::OptionsForOperatorWithMatrixAsFirstArgument,
                semiring::PlusTimes,
            },
        };

        let n = m.n_nodes;
        if n == 0 {
            return Ok(HashMap::new());
        }

        let ones_list = VectorElementList::<i32>::from_element_vector(
            (0..n).map(|i| (i, 1i32).into()).collect(),
        );
        let ones = SparseVector::<i32>::from_element_list(
            m.context.clone(),
            n,
            ones_list,
            &First::<i32>::new(),
        )
        .map_err(|e| Error::GraphBLAS(e.to_string()))?;

        let mxv = MatrixVectorMultiplicationOperator::new();
        let opts = OptionsForOperatorWithMatrixAsFirstArgument::new_default();

        let compute_degree = |matrix: &SparseMatrix<i32>| -> Result<Vec<u64>, Error> {
            let mut out = SparseVector::<i32>::new(m.context.clone(), n)
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            mxv.apply(
                matrix,
                &PlusTimes::<i32>::new(),
                &ones,
                &Assignment::new(),
                &mut out,
                &SelectEntireVector::new(m.context.clone()),
                &opts,
            )
            .map_err(|e| Error::GraphBLAS(e.to_string()))?;

            let mut degrees = vec![0u64; n];
            let indices = out
                .element_indices()
                .map_err(|e| Error::GraphBLAS(e.to_string()))?;
            for idx in indices {
                let v = out
                    .element_value_or_default(idx)
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
        for (dense, &node_id) in snap.dense_to_id.iter().enumerate() {
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
