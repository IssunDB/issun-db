use std::collections::HashMap;

use ahash::{AHashMap, AHashSet};
use issundb_core::{EdgeId, Error, Graph, NodeId};

/// A subgraph extracted by a GraphRAG retrieve call.
///
/// `nodes` and `edges` are deduplicated but unordered. `scores` maps each
/// seed node (a direct vector-search hit) to its cosine distance from the
/// query; expansion-only nodes are absent from the map.
pub struct Subgraph {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
    pub scores: HashMap<NodeId, f32>,
}

/// Options for `retrieve_with`.
pub struct RetrieveOptions {
    /// Number of seed nodes returned by the vector search.
    pub k: usize,
    /// BFS expansion depth from each seed node.
    pub hops: u8,
    /// Maximum cosine distance for a vector hit to qualify as a seed.
    /// Hits with `distance > max_distance` are dropped before BFS expansion.
    /// Default: `f32::MAX` (keep all k hits).
    pub max_distance: f32,
    /// Hard cap on the total number of nodes in the returned subgraph.
    /// BFS stops as soon as this limit is reached.
    /// `None` means no cap.
    pub max_nodes: Option<usize>,
}

impl Default for RetrieveOptions {
    fn default() -> Self {
        Self {
            k: 10,
            hops: 2,
            max_distance: f32::MAX,
            max_nodes: None,
        }
    }
}

/// Convenience wrapper: vector search → k seeds → `hops`-hop BFS expansion →
/// subgraph materialization.
pub fn retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, Error> {
    retrieve_with(
        graph,
        q,
        &RetrieveOptions {
            k,
            hops,
            ..Default::default()
        },
    )
}

/// Full retrieve with configurable options.
///
/// Algorithm:
/// 1. `vector_search(q, k)` → seed hits.
/// 2. Filter seeds by `max_distance`; record cosine distances in `scores`.
/// 3. BFS from all seeds simultaneously up to `hops` hops, following
///    out-edges; stop early if `max_nodes` is reached.
/// 4. For every node in the expanded set, emit any out-edge whose destination
///    is also in the set.
pub fn retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, Error> {
    let hits = graph.vector_search(q, opts.k)?;

    let mut scores: AHashMap<NodeId, f32> = AHashMap::new();
    for hit in &hits {
        if hit.distance <= opts.max_distance {
            scores.insert(hit.node, hit.distance);
        }
    }

    let mut node_set: AHashSet<NodeId> = scores.keys().copied().collect();
    let mut frontier: Vec<NodeId> = node_set.iter().copied().collect();

    let mut capped = false;
    for _ in 0..opts.hops {
        if capped || frontier.is_empty() {
            break;
        }
        let mut next: Vec<NodeId> = Vec::new();
        'outer: for &node in &frontier {
            for (nb, _, _) in graph.out_neighbors(node)? {
                if node_set.insert(nb) {
                    next.push(nb);
                    if opts.max_nodes.is_some_and(|max| node_set.len() >= max) {
                        capped = true;
                        break 'outer;
                    }
                }
            }
        }
        frontier = next;
    }

    let mut edge_set: AHashSet<EdgeId> = AHashSet::new();
    for &node in &node_set {
        for (nb, eid, _) in graph.out_neighbors(node)? {
            if node_set.contains(&nb) {
                edge_set.insert(eid);
            }
        }
    }

    Ok(Subgraph {
        nodes: node_set.into_iter().collect(),
        edges: edge_set.into_iter().collect(),
        scores: scores.into_iter().collect(),
    })
}

/// GraphBLAS SpMV k-hop BFS expansion.
///
/// Runs multi-source SpMV BFS from the filtered seed nodes up to `hops` hops.
/// Stops early or caps the results if `max_nodes` is specified and exceeded.
#[cfg(feature = "graphblas")]
pub fn retrieve_graphblas(
    graph: &Graph,
    q: &[f32],
    opts: &RetrieveOptions,
) -> Result<Subgraph, Error> {
    let hits = graph.vector_search(q, opts.k)?;

    let mut scores: AHashMap<NodeId, f32> = AHashMap::new();
    let mut seeds = Vec::new();
    for hit in &hits {
        if hit.distance <= opts.max_distance {
            scores.insert(hit.node, hit.distance);
            seeds.push(hit.node);
        }
    }

    if seeds.is_empty() {
        return Ok(Subgraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            scores: HashMap::new(),
        });
    }

    let node_list = graph.bfs_multi_source_graphblas(&seeds, opts.hops, opts.max_nodes)?;
    let node_set: AHashSet<NodeId> = node_list.into_iter().collect();

    // Keep only scores whose seed node actually appears in the BFS result.
    // `bfs_multi_source_graphblas` guarantees this when every seed is present in
    // the CSR snapshot; this retain is a defensive guard to ensure
    // `scores.keys() ⊆ nodes` even if that invariant is ever broken upstream.
    scores.retain(|n, _| node_set.contains(n));

    let mut edge_set: AHashSet<EdgeId> = AHashSet::new();
    for &node in &node_set {
        for (nb, eid, _) in graph.out_neighbors(node)? {
            if node_set.contains(&nb) {
                edge_set.insert(eid);
            }
        }
    }

    Ok(Subgraph {
        nodes: node_set.into_iter().collect(),
        edges: edge_set.into_iter().collect(),
        scores: scores.into_iter().collect(),
    })
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
    fn retrieve_empty_vector_index_returns_empty_subgraph() {
        let (_dir, g) = open_tmp();
        let sub = retrieve(&g, &[1.0f32, 0.0], 5, 2).unwrap();
        assert!(sub.nodes.is_empty());
        assert!(sub.edges.is_empty());
        assert!(sub.scores.is_empty());
    }

    #[test]
    fn retrieve_hops_zero_returns_only_seed_nodes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();

        // hops=0: no BFS expansion; c is only reachable via a's out-edge.
        let sub = retrieve(&g, &[1.0f32, 0.0, 0.0], 1, 0).unwrap();
        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0], a);
        assert!(!sub.nodes.contains(&c));
    }

    #[test]
    fn retrieve_expands_bfs_to_correct_depth() {
        let (_dir, g) = open_tmp();
        // Chain: a → b → c → d; only a has a vector.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();

        let sub1 = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        let sub2 = retrieve(&g, &[1.0f32, 0.0], 1, 2).unwrap();

        let mut n1 = sub1.nodes.clone();
        n1.sort_unstable();
        assert_eq!(n1, vec![a, b]);

        let mut n2 = sub2.nodes.clone();
        n2.sort_unstable();
        assert_eq!(n2, vec![a, b, c]);
    }

    #[test]
    fn retrieve_subgraph_edges_connect_only_nodes_in_set() {
        let (_dir, g) = open_tmp();
        // a → b → c; only a and b are in the subgraph (hops=1 from a).
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        let e_ab = g.add_edge(a, b, "E", &json!({})).unwrap();
        let _e_bc = g.add_edge(b, c, "E", &json!({})).unwrap();

        let sub = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        assert!(sub.edges.contains(&e_ab));
        // b→c edge must NOT appear: c is outside the 1-hop subgraph.
        assert_eq!(sub.edges.len(), 1);
    }

    #[test]
    fn retrieve_scores_map_contains_seed_distances() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();

        let sub = retrieve(&g, &[1.0f32, 0.0], 1, 0).unwrap();
        assert!(sub.scores.contains_key(&a));
        assert!(sub.scores[&a] < 1e-5);
    }

    #[test]
    fn retrieve_with_max_distance_filters_far_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        // a is at distance ~0 from the query; b is orthogonal (distance ~1).
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();

        let sub = retrieve_with(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 0,
                max_distance: 0.1,
                max_nodes: None,
            },
        )
        .unwrap();

        // Only a is within 0.1 cosine distance of the query.
        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0], a);
    }

    #[test]
    fn retrieve_with_max_nodes_caps_subgraph() {
        let (_dir, g) = open_tmp();
        // Star: a → b, c, d, e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(a, d, "E", &json!({})).unwrap();
        g.add_edge(a, e, "E", &json!({})).unwrap();

        let sub = retrieve_with(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: Some(3),
            },
        )
        .unwrap();

        assert!(sub.nodes.len() <= 3);
    }

    #[test]
    fn retrieve_with_multiple_seeds_each_expand_independently() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains: a → b → c; d → e → f
        // Both a and d have vectors and qualify as seeds.
        // With hops=1 the subgraph must include {a, b, d, e} but not {c, f}.
        // With hops=2 it must include all six nodes.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        let f = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(d, &[0.0f32, 1.0, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(d, e, "E", &json!({})).unwrap();
        g.add_edge(e, f, "E", &json!({})).unwrap();

        let sub1 = retrieve_with(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();
        let mut n1 = sub1.nodes.clone();
        n1.sort_unstable();
        assert!(n1.contains(&a), "seed a must be present at hops=1");
        assert!(n1.contains(&b), "b is 1 hop from seed a");
        assert!(n1.contains(&d), "seed d must be present at hops=1");
        assert!(n1.contains(&e), "e is 1 hop from seed d");
        assert!(!n1.contains(&c), "c is 2 hops from a, out of range");
        assert!(!n1.contains(&f), "f is 2 hops from d, out of range");
        assert_eq!(n1.len(), 4);

        let sub2 = retrieve_with(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 2,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();
        assert_eq!(sub2.nodes.len(), 6, "all six nodes reachable within 2 hops");
        assert!(sub2.scores.contains_key(&a));
        assert!(sub2.scores.contains_key(&d));
    }

    // --- retrieve_graphblas ---
    //
    // Each test calls `rebuild_csr()` after graph mutations so the GraphBLAS
    // adjacency matrix is current before retrieve_graphblas is invoked.

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_k_hop_expansion() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();

        assert_eq!(sub.nodes.len(), 2);
        assert!(sub.nodes.contains(&a));
        assert!(sub.nodes.contains(&b));
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_hops_zero_returns_only_seed() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 0,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();

        assert_eq!(sub.nodes, vec![a]);
        assert!(sub.edges.is_empty(), "no edges when hops=0");
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_scores_keys_are_subset_of_nodes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.9f32, 0.1, 0.0]).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();

        // Every key in scores must be present in nodes.
        for node_id in sub.scores.keys() {
            assert!(
                sub.nodes.contains(node_id),
                "scores key {node_id:?} is absent from nodes"
            );
        }
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_edges_connect_only_nodes_in_subgraph() {
        let (_dir, g) = open_tmp();
        // Chain: a → b → c → d; seed is a (hops=1 → {a, b}).
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        let e_ab = g.add_edge(a, b, "E", &json!({})).unwrap();
        let _e_bc = g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();

        assert!(sub.nodes.contains(&a));
        assert!(sub.nodes.contains(&b));
        assert!(!sub.nodes.contains(&c));
        assert!(sub.edges.contains(&e_ab), "edge a→b must be in subgraph");
        assert_eq!(sub.edges.len(), 1, "only a→b is within the 1-hop subgraph");
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_max_distance_filters_far_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        // a is close to query; b is orthogonal (distance ~1).
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 0,
                max_distance: 0.1,
                max_nodes: None,
            },
        )
        .unwrap();

        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0], a);
        assert!(sub.scores.contains_key(&a));
        assert!(!sub.scores.contains_key(&b));
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_max_nodes_caps_subgraph() {
        let (_dir, g) = open_tmp();
        // Star: a → b, c, d, e
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(a, d, "E", &json!({})).unwrap();
        g.add_edge(a, e, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: Some(3),
            },
        )
        .unwrap();

        assert!(
            sub.nodes.len() <= 3,
            "expected at most 3 nodes, got {}",
            sub.nodes.len()
        );
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_scores_contain_seed_distances() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0],
            &RetrieveOptions {
                k: 1,
                hops: 0,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();

        assert!(sub.scores.contains_key(&a));
        assert!(
            sub.scores[&a] < 1e-5,
            "distance to identical vector must be ~0"
        );
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_empty_vector_index_returns_empty() {
        let (_dir, g) = open_tmp();
        g.rebuild_csr().unwrap();

        let sub = retrieve_graphblas(&g, &[1.0f32, 0.0], &RetrieveOptions::default()).unwrap();

        assert!(sub.nodes.is_empty());
        assert!(sub.edges.is_empty());
        assert!(sub.scores.is_empty());
    }

    #[cfg(feature = "graphblas")]
    #[test]
    fn graphblas_retrieve_multiple_seeds_each_expand_independently() {
        let (_dir, g) = open_tmp();
        // Mirrors the non-graphblas variant: two disconnected chains
        // a → b → c; d → e → f, with vectors on a and d.
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        let e = g.add_node("N", &json!({})).unwrap();
        let f = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(d, &[0.0f32, 1.0, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(d, e, "E", &json!({})).unwrap();
        g.add_edge(e, f, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub1 = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 1,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();
        assert!(sub1.nodes.contains(&a), "seed a must be present at hops=1");
        assert!(sub1.nodes.contains(&b), "b is 1 hop from seed a");
        assert!(sub1.nodes.contains(&d), "seed d must be present at hops=1");
        assert!(sub1.nodes.contains(&e), "e is 1 hop from seed d");
        assert!(!sub1.nodes.contains(&c), "c is 2 hops from a, out of range");
        assert!(!sub1.nodes.contains(&f), "f is 2 hops from d, out of range");
        assert_eq!(sub1.nodes.len(), 4);

        let sub2 = retrieve_graphblas(
            &g,
            &[1.0f32, 0.0, 0.0],
            &RetrieveOptions {
                k: 2,
                hops: 2,
                max_distance: f32::MAX,
                max_nodes: None,
            },
        )
        .unwrap();
        assert_eq!(sub2.nodes.len(), 6, "all six nodes reachable within 2 hops");
        assert!(sub2.scores.contains_key(&a));
        assert!(sub2.scores.contains_key(&d));
    }
}
