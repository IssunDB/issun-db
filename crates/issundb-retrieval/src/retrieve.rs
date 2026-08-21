use std::collections::HashMap;

use crate::error::RetrievalError;
use ahash::{AHashMap, AHashSet};
use issundb_core::{EdgeId, Graph, NodeId};
use issundb_text::{TextGraphExt, TextSearchOptions};
use issundb_vector::{VectorError, VectorGraphExt, VectorSearchOptions};

/// A subgraph extracted by a retrieval call.
///
/// Expansion is undirected: each hop from a seed follows outgoing and
/// incoming edges alike, and `edges` holds every stored edge whose two
/// endpoints are both in `nodes`, whichever way it points.
///
/// `nodes` and `edges` are deduplicated but unordered. `scores` maps each seed
/// node to its relevance value; expansion-only nodes are absent from the map.
/// For [`retrieve`] and [`retrieve_with`] the value is the seed's cosine
/// distance from the query (lower is closer). For [`retrieve_hybrid`] it is the
/// fused score produced by the configured [`FusionStrategy`] over the vector
/// and text seeds (higher is more relevant). `truncated` is true when the
/// `max_nodes` cap cut off seeds or expansion that would otherwise have been
/// included, so a capped subgraph (whose missing edges would otherwise read as
/// "these nodes are unconnected") is distinguishable from a complete one.
#[derive(Debug)]
pub struct Subgraph {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
    pub scores: HashMap<NodeId, f32>,
    pub truncated: bool,
}

/// Options for `retrieve_with`.
pub struct RetrieveOptions {
    /// Number of seed nodes returned by the vector search.
    pub k: usize,
    /// BFS expansion depth from each seed node. Each hop is undirected,
    /// following outgoing and incoming edges alike.
    pub hops: u8,
    /// Maximum cosine distance for a vector hit to qualify as a seed.
    /// Hits with `distance > max_distance` are dropped before BFS expansion.
    /// Defaults to `f32::MAX`, which keeps all `k` hits.
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

/// Multi-source breadth-first expansion over both edge directions.
///
/// Retrieval treats the graph as undirected: a seed related only through
/// incoming edges (a chunk that MENTIONS the seeded entity) is context worth
/// returning, so each hop follows outgoing and incoming edges alike. The cap
/// and truncation semantics mirror `Graph::bfs_multi_source`: seeds are
/// admitted in input order and count once each, a hop that would exceed
/// `max_nodes` keeps the lowest node ids up to the cap, and the flag is true
/// when the cap cut off seeds or reachable nodes. A seed no node holds
/// contributes nothing rather than erroring.
fn bfs_multi_source_undirected(
    graph: &Graph,
    seeds: &[NodeId],
    hops: u8,
    max_nodes: Option<usize>,
) -> Result<(Vec<NodeId>, bool), RetrievalError> {
    let mut visited: AHashSet<NodeId> = AHashSet::new();
    let mut frontier: Vec<NodeId> = Vec::new();
    let mut truncated = false;
    for &seed in seeds {
        if visited.contains(&seed) || !graph.node_exists(seed)? {
            continue;
        }
        if max_nodes.is_some_and(|max| visited.len() >= max) {
            truncated = true;
            break;
        }
        visited.insert(seed);
        frontier.push(seed);
    }
    if visited.is_empty() {
        return Ok((Vec::new(), truncated));
    }

    for _ in 0..hops {
        let mut discovered: AHashSet<NodeId> = AHashSet::new();
        for incoming in [false, true] {
            for (_, _, other) in graph.expand_bulk(&frontier, None, incoming)? {
                if !visited.contains(&other) {
                    discovered.insert(other);
                }
            }
        }
        if discovered.is_empty() {
            break;
        }
        let mut next: Vec<NodeId> = discovered.into_iter().collect();
        next.sort_unstable();
        if let Some(max) = max_nodes {
            if visited.len() >= max {
                truncated = true;
                break;
            }
            if visited.len() + next.len() > max {
                truncated = true;
                next.truncate(max - visited.len());
                visited.extend(&next);
                break;
            }
        }
        visited.extend(&next);
        frontier = next;
    }

    let mut nodes: Vec<NodeId> = visited.into_iter().collect();
    nodes.sort_unstable();
    Ok((nodes, truncated))
}

/// Wraps a vector search to `k` seeds and a `hops`-hop undirected BFS
/// expansion into one subgraph materialization.
pub fn retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, RetrievalError> {
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
/// Runs an undirected multi-source breadth-first search from the filtered seed
/// nodes up to `hops` hops, stopping early or capping the result when
/// `max_nodes` is set and reached.
pub fn retrieve_with(
    graph: &Graph,
    q: &[f32],
    opts: &RetrieveOptions,
) -> Result<Subgraph, RetrievalError> {
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
            truncated: false,
        });
    }

    let (node_list, truncated) =
        bfs_multi_source_undirected(graph, &seeds, opts.hops, opts.max_nodes)?;
    let node_set: AHashSet<NodeId> = node_list.into_iter().collect();

    // Keep only scores whose seed node appears in the expansion result. The
    // expansion admits every existing seed, so this retain only matters if
    // that ever breaks upstream.
    scores.retain(|n, _| node_set.contains(n));

    let edges = induced_edges(graph, &node_set)?;

    Ok(Subgraph {
        nodes: node_set.into_iter().collect(),
        edges,
        scores: scores.into_iter().collect(),
        truncated,
    })
}

/// Every edge whose two endpoints are both in `node_set`, direction included.
/// Walking the outgoing adjacency of each included node is enough: any edge
/// between two included nodes has its source among them, so its incoming
/// appearance at the other endpoint names the same edge.
fn induced_edges(
    graph: &Graph,
    node_set: &AHashSet<NodeId>,
) -> Result<Vec<EdgeId>, RetrievalError> {
    let mut edge_set: AHashSet<EdgeId> = AHashSet::new();
    for &node in node_set {
        for ne in graph.out_neighbors(node)? {
            if node_set.contains(&ne.node) {
                edge_set.insert(ne.edge);
            }
        }
    }
    Ok(edge_set.into_iter().collect())
}

/// Strategy for fusing vector and text relevance scores.
#[derive(Debug, Clone)]
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion, scoring an item as Σ 1 / (k + rank + 1) summed over
    /// the modalities that ranked it, where `rank` is 0-based. `k` is a smoothing
    /// constant; default 60.
    Rrf { k: u32 },
    /// Weighted linear combination, scoring an item as α·vector_score +
    /// β·text_score.
    WeightedSum {
        vector_weight: f32,
        text_weight: f32,
    },
}

impl Default for FusionStrategy {
    fn default() -> Self {
        Self::Rrf { k: 60 }
    }
}

/// Options for `retrieve_hybrid`.
pub struct HybridRetrieveOptions {
    /// Number of seed nodes from the vector search. `0` disables vector search.
    pub vector_k: usize,
    /// Number of seed nodes from the text search. `0` disables text search.
    pub text_k: usize,
    /// Label to restrict the text search. `None` searches all indexed labels.
    pub text_label: Option<String>,
    /// Property to restrict the text search. `None` searches all indexed properties.
    pub text_property: Option<String>,
    /// BFS expansion depth from each seed. Each hop is undirected,
    /// following outgoing and incoming edges alike.
    pub hops: u8,
    /// Maximum cosine distance for a vector hit to qualify as a seed.
    pub max_distance: f32,
    /// Hard cap on total subgraph nodes.
    pub max_nodes: Option<usize>,
    /// If set, only nodes with this label qualify as vector-search seeds.
    pub vector_label: Option<String>,
    /// Score fusion strategy.
    pub fusion: FusionStrategy,
}

impl Default for HybridRetrieveOptions {
    fn default() -> Self {
        Self {
            vector_k: 10,
            text_k: 10,
            text_label: None,
            text_property: None,
            hops: 2,
            max_distance: f32::MAX,
            max_nodes: None,
            vector_label: None,
            fusion: FusionStrategy::default(),
        }
    }
}

/// Merges vector search seeds with full-text search seeds, fuses their scores
/// using `opts.fusion`, then expands via undirected BFS.
///
/// Vector search is run when `opts.vector_k > 0` and `q` is non-empty.
/// Text search is run when `opts.text_k > 0` and `text_query` is non-empty.
/// Both may run simultaneously; their ranked lists are merged before BFS.
/// When neither would run (both inputs empty or both disabled), the call
/// returns `RetrievalError::NoQuery` instead of a silently empty subgraph.
/// A graph with no embeddings fails the vector arm with
/// `VectorError::EmptyIndex`; when the text arm is active that is treated as
/// zero vector seeds, and the error propagates only when vector search is the
/// sole active modality.
pub fn retrieve_hybrid(
    graph: &Graph,
    q: &[f32],
    text_query: &str,
    opts: &HybridRetrieveOptions,
) -> Result<Subgraph, RetrievalError> {
    let vector_active = opts.vector_k > 0 && !q.is_empty();
    let text_active = opts.text_k > 0 && !text_query.is_empty();
    if !vector_active && !text_active {
        return Err(RetrievalError::NoQuery);
    }

    // ---- collect vector hits -----------------------------------------------
    let mut vec_ranks: AHashMap<NodeId, usize> = AHashMap::new();
    let mut vec_scores: AHashMap<NodeId, f32> = AHashMap::new();

    if vector_active {
        let search = graph.vector_search_with(
            q,
            &VectorSearchOptions {
                k: opts.vector_k,
                label: opts.vector_label.clone(),
                properties: None,
                rescore_factor: None,
            },
        );
        match search {
            Ok(hits) => {
                for (rank, hit) in hits.iter().enumerate() {
                    if hit.distance <= opts.max_distance {
                        vec_ranks.insert(hit.node, rank);
                        vec_scores.insert(hit.node, hit.distance);
                    }
                }
            }
            // An empty vector index means zero vector seeds, not a failed
            // call, when the text arm can still serve: the contract reserves
            // an error for a request where neither modality would run. With
            // vector as the only active modality the error still propagates.
            Err(VectorError::EmptyIndex) if text_active => {}
            Err(e) => return Err(e.into()),
        }
    }

    // ---- collect text hits -------------------------------------------------
    let mut text_ranks: AHashMap<NodeId, usize> = AHashMap::new();

    if text_active {
        let text_opts = TextSearchOptions {
            label: opts.text_label.clone(),
            property: opts.text_property.clone(),
            limit: opts.text_k,
            ..Default::default()
        };
        let text_hits = graph.text_search(text_query, &text_opts)?;
        for (rank, hit) in text_hits.iter().enumerate() {
            text_ranks.insert(hit.node, rank);
        }
    }

    // ---- fuse scores -------------------------------------------------------
    let mut fused: AHashMap<NodeId, f32> = AHashMap::new();

    let all_nodes: AHashSet<NodeId> = vec_ranks.keys().chain(text_ranks.keys()).copied().collect();

    for node in &all_nodes {
        let score = match &opts.fusion {
            FusionStrategy::Rrf { k } => {
                let kf = *k as f32;
                let vs = vec_ranks
                    .get(node)
                    .map(|r| 1.0 / (kf + *r as f32 + 1.0))
                    .unwrap_or(0.0);
                let ts = text_ranks
                    .get(node)
                    .map(|r| 1.0 / (kf + *r as f32 + 1.0))
                    .unwrap_or(0.0);
                vs + ts
            }
            FusionStrategy::WeightedSum {
                vector_weight,
                text_weight,
            } => {
                let total_vec = opts.vector_k.max(1) as f32;
                let total_txt = opts.text_k.max(1) as f32;
                let vs = vec_ranks
                    .get(node)
                    .map(|r| (total_vec - *r as f32) / total_vec)
                    .unwrap_or(0.0);
                let ts = text_ranks
                    .get(node)
                    .map(|r| (total_txt - *r as f32) / total_txt)
                    .unwrap_or(0.0);
                vector_weight * vs + text_weight * ts
            }
        };
        fused.insert(*node, score);
    }

    // Seed the expansion from the highest-scored fused hits first: the
    // multi-source BFS keeps only the first `max_nodes` seeds, so an arbitrary
    // `AHashMap` iteration order would drop top-scored seeds nondeterministically.
    // Sort by fused score descending, breaking ties by node id for a stable,
    // reproducible seed set.
    let mut ranked: Vec<(NodeId, f32)> = fused.iter().map(|(n, s)| (*n, *s)).collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let seeds: Vec<NodeId> = ranked.into_iter().map(|(n, _)| n).collect();

    if seeds.is_empty() {
        return Ok(Subgraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            scores: HashMap::new(),
            truncated: false,
        });
    }

    // ---- BFS expansion -----------------------------------------------------
    let (node_list, truncated) =
        bfs_multi_source_undirected(graph, &seeds, opts.hops, opts.max_nodes)?;
    let node_set: AHashSet<NodeId> = node_list.into_iter().collect();

    let mut scores: AHashMap<NodeId, f32> = fused;
    scores.retain(|n, _| node_set.contains(n));

    let edges = induced_edges(graph, &node_set)?;

    Ok(Subgraph {
        nodes: node_set.into_iter().collect(),
        edges,
        scores: scores.into_iter().collect(),
        truncated,
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
    fn retrieve_empty_vector_index_is_an_error() {
        let (_dir, g) = open_tmp();
        let err = retrieve(&g, &[1.0f32, 0.0], 5, 2).unwrap_err();
        assert!(
            matches!(
                err,
                RetrievalError::Vector(issundb_vector::VectorError::EmptyIndex)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn hybrid_retrieve_without_any_query_is_an_error() {
        let (_dir, g) = open_tmp();
        let err = retrieve_hybrid(&g, &[], "", &HybridRetrieveOptions::default()).unwrap_err();
        assert!(matches!(err, RetrievalError::NoQuery), "got {err:?}");

        // Disabling both modalities is the same misuse as omitting both inputs.
        let opts = HybridRetrieveOptions {
            vector_k: 0,
            text_k: 0,
            ..Default::default()
        };
        let err = retrieve_hybrid(&g, &[1.0f32, 0.0], "cassava", &opts).unwrap_err();
        assert!(matches!(err, RetrievalError::NoQuery), "got {err:?}");
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
        // Chain: a to b to c to d; only a has a vector.
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
        // a to b to c; only a and b are in the subgraph (hops=1 from a).
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        let e_ab = g.add_edge(a, b, "E", &json!({})).unwrap();
        let _e_bc = g.add_edge(b, c, "E", &json!({})).unwrap();

        let sub = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        assert!(sub.edges.contains(&e_ab));
        // b to c edge must NOT appear: c is outside the 1-hop subgraph.
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
        // Star: a to b, c, d, e
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
        assert!(
            sub.truncated,
            "the cap dropped reachable nodes, so the subgraph must say so"
        );
    }

    #[test]
    fn retrieve_without_a_cap_is_not_truncated() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();

        let sub = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        assert!(!sub.truncated);
    }

    #[test]
    fn retrieve_with_multiple_seeds_each_expand_independently() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains: a to b to c; d to e to f
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

    // --- retrieve_with ---
    //
    // Each test calls `rebuild_csr()` after graph mutations so the
    // CSR snapshot is current before retrieve_with is invoked.

    #[test]
    fn retrieve_k_hop_expansion() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_with(
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

    #[test]
    fn retrieve_hops_zero_returns_only_seed() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_with(
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

    #[test]
    fn retrieve_scores_keys_are_subset_of_nodes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.9f32, 0.1, 0.0]).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_with(
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

    #[test]
    fn retrieve_edges_connect_only_nodes_in_subgraph() {
        let (_dir, g) = open_tmp();
        // Chain: a to b to c to d; seed is a (hops=1 includes {a, b}).
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        let e_ab = g.add_edge(a, b, "E", &json!({})).unwrap();
        let _e_bc = g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_with(
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
        assert!(sub.edges.contains(&e_ab), "edge a to b must be in subgraph");
        assert_eq!(
            sub.edges.len(),
            1,
            "only a to b is within the 1-hop subgraph"
        );
    }

    #[test]
    fn retrieve_max_distance_filters_far_seeds() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        // a is close to query; b is orthogonal (distance ~1).
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        g.rebuild_csr().unwrap();

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

        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0], a);
        assert!(sub.scores.contains_key(&a));
        assert!(!sub.scores.contains_key(&b));
    }

    #[test]
    fn retrieve_max_nodes_caps_subgraph() {
        let (_dir, g) = open_tmp();
        // Star: a to b, c, d, e
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

        assert!(
            sub.nodes.len() <= 3,
            "expected at most 3 nodes, got {}",
            sub.nodes.len()
        );
        assert!(sub.truncated, "the cap dropped reachable nodes");
    }

    #[test]
    fn retrieve_scores_contain_seed_distances() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_with(
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

    #[test]
    fn retrieve_with_over_an_empty_vector_index_is_an_error() {
        let (_dir, g) = open_tmp();
        g.rebuild_csr().unwrap();

        let err = retrieve_with(&g, &[1.0f32, 0.0], &RetrieveOptions::default()).unwrap_err();
        assert!(
            matches!(
                err,
                RetrievalError::Vector(issundb_vector::VectorError::EmptyIndex)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn retrieve_multiple_seeds_each_expand_independently() {
        let (_dir, g) = open_tmp();
        // Two disconnected chains
        // a to b to c; d to e to f, with vectors on a and d.
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
        assert!(sub1.nodes.contains(&a), "seed a must be present at hops=1");
        assert!(sub1.nodes.contains(&b), "b is 1 hop from seed a");
        assert!(sub1.nodes.contains(&d), "seed d must be present at hops=1");
        assert!(sub1.nodes.contains(&e), "e is 1 hop from seed d");
        assert!(!sub1.nodes.contains(&c), "c is 2 hops from a, out of range");
        assert!(!sub1.nodes.contains(&f), "f is 2 hops from d, out of range");
        assert_eq!(sub1.nodes.len(), 4);

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

    #[test]
    fn hybrid_retrieve_vector_only_matches_pure_vector_search() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0, 0.0]).unwrap();
        g.rebuild_csr().unwrap();

        let sub = retrieve_hybrid(
            &g,
            &[1.0f32, 0.0, 0.0],
            "",
            &HybridRetrieveOptions {
                vector_k: 1,
                text_k: 0,
                hops: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0], a);
    }

    /// `retrieve_hybrid` must keep the highest-scored seeds when `max_nodes`
    /// caps the result, not an arbitrary subset. Regression for score-blind,
    /// nondeterministic seed truncation.
    #[test]
    fn hybrid_retrieve_keeps_top_scored_seeds_under_max_nodes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        let d = g.add_node("N", &json!({})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0, 0.0]).unwrap(); // closest to the query
        g.upsert_vector(b, &[0.9f32, 0.1, 0.0]).unwrap(); // second closest
        g.upsert_vector(c, &[0.2f32, 1.0, 0.0]).unwrap(); // far
        g.upsert_vector(d, &[0.0f32, 0.0, 1.0]).unwrap(); // far
        g.rebuild_csr().unwrap();

        let opts = HybridRetrieveOptions {
            vector_k: 4,
            text_k: 0,
            hops: 0,
            max_distance: 2.0, // admit all four as seeds so truncation is exercised
            max_nodes: Some(2),
            ..Default::default()
        };
        let sub = retrieve_hybrid(&g, &[1.0f32, 0.0, 0.0], "", &opts).unwrap();
        let mut nodes = sub.nodes.clone();
        nodes.sort_unstable();
        let mut expected = vec![a, b];
        expected.sort_unstable();
        assert_eq!(
            nodes, expected,
            "the two highest-scored seeds must survive the max_nodes cap"
        );
        assert!(sub.truncated, "the cap dropped two of the four seeds");
    }

    #[test]
    fn hybrid_retrieve_fuses_both_sources() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("Doc", &json!({"body": "rust graph database storage"}))
            .unwrap();
        let b = g
            .add_node("Doc", &json!({"body": "vector search nearest neighbor"}))
            .unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0]).unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();
        g.rebuild_csr().unwrap();

        // b has text match for "vector"; a has vector match for [1, 0].
        let sub = retrieve_hybrid(
            &g,
            &[1.0f32, 0.0],
            "vector",
            &HybridRetrieveOptions {
                vector_k: 1,
                text_k: 1,
                text_label: Some("Doc".into()),
                text_property: Some("body".into()),
                hops: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sub.nodes.contains(&a), "vector hit a must be present");
        assert!(sub.nodes.contains(&b), "text hit b must be present");
    }

    #[test]
    fn hybrid_retrieve_weighted_sum_produces_correct_scores() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Doc", &json!({"body": "alpha bravo"})).unwrap();
        let b = g
            .add_node("Doc", &json!({"body": "charlie delta"}))
            .unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0]).unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();
        g.rebuild_csr().unwrap();

        // a is the top vector hit (rank 0) and b is the top text hit (rank 0).
        // vector_k=1, text_k=1, so normalized rank score = (k - rank) / k = 1.0.
        // WeightedSum: score = 0.7 * vec_norm + 0.3 * text_norm.
        // a: vec_norm = 1.0, text_norm = 0.0 => 0.7
        // b: vec_norm = 0.0, text_norm = 1.0 => 0.3
        let sub = retrieve_hybrid(
            &g,
            &[1.0f32, 0.0],
            "charlie",
            &HybridRetrieveOptions {
                vector_k: 1,
                text_k: 1,
                text_label: Some("Doc".into()),
                text_property: Some("body".into()),
                hops: 0,
                fusion: FusionStrategy::WeightedSum {
                    vector_weight: 0.7,
                    text_weight: 0.3,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            sub.scores.contains_key(&a),
            "vector seed a must have a score"
        );
        assert!(sub.scores.contains_key(&b), "text seed b must have a score");
        assert!(
            (sub.scores[&a] - 0.7).abs() < 1e-5,
            "a score should be 0.7, got {}",
            sub.scores[&a]
        );
        assert!(
            (sub.scores[&b] - 0.3).abs() < 1e-5,
            "b score should be 0.3, got {}",
            sub.scores[&b]
        );
    }

    /// A graph with a text index but no embeddings can still serve the text
    /// arm: an empty vector index contributes zero vector seeds instead of
    /// failing the whole call, because the contract reserves an error for a
    /// call where neither modality would run.
    #[test]
    fn hybrid_retrieve_with_text_active_survives_an_empty_vector_index() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("Doc", &json!({"body": "quantum computing research"}))
            .unwrap();
        let _b = g
            .add_node("Doc", &json!({"body": "classical music orchestra"}))
            .unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();

        let sub = retrieve_hybrid(
            &g,
            &[1.0f32, 0.0],
            "quantum",
            &HybridRetrieveOptions {
                vector_k: 5,
                text_k: 5,
                text_label: Some("Doc".into()),
                text_property: Some("body".into()),
                hops: 0,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(sub.nodes, vec![a], "the text seed alone forms the subgraph");
        assert!(sub.scores.contains_key(&a));
    }

    /// When the vector arm is the only active modality, an empty index is
    /// still an error: there is no other arm to serve the call.
    #[test]
    fn hybrid_retrieve_vector_only_over_an_empty_index_still_errors() {
        let (_dir, g) = open_tmp();
        g.add_node("Doc", &json!({"body": "quantum"})).unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();

        let err = retrieve_hybrid(
            &g,
            &[1.0f32, 0.0],
            "",
            &HybridRetrieveOptions {
                vector_k: 5,
                text_k: 5,
                hops: 0,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                RetrievalError::Vector(issundb_vector::VectorError::EmptyIndex)
            ),
            "got {err:?}"
        );
    }

    /// The GraphRAG shape: `(:Chunk)-[:MENTIONS]->(:Entity)`, seeded on the
    /// entity. Expansion is undirected, so the chunk one incoming hop away is
    /// part of the subgraph, and so is the edge into the seed.
    #[test]
    fn retrieve_expands_over_incoming_edges() {
        let (_dir, g) = open_tmp();
        let entity = g.add_node("Entity", &json!({})).unwrap();
        let chunk = g.add_node("Chunk", &json!({})).unwrap();
        let e = g.add_edge(chunk, entity, "MENTIONS", &json!({})).unwrap();
        g.upsert_vector(entity, &[1.0f32, 0.0]).unwrap();

        let sub = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        let mut nodes = sub.nodes.clone();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![entity, chunk]);
        assert_eq!(sub.edges, vec![e], "the edge into the seed is collected");
        assert!(!sub.truncated);
        assert!(sub.scores.contains_key(&entity));
        assert!(
            !sub.scores.contains_key(&chunk),
            "expansion-only nodes carry no score"
        );
    }

    /// Hop depth counts undirected steps: on the chain a to b to c, a seed on
    /// c reaches b at one hop and a at two.
    #[test]
    fn retrieve_expands_incoming_chain_to_depth() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.upsert_vector(c, &[1.0f32, 0.0]).unwrap();

        let sub1 = retrieve(&g, &[1.0f32, 0.0], 1, 1).unwrap();
        let mut n1 = sub1.nodes.clone();
        n1.sort_unstable();
        assert_eq!(n1, vec![b, c]);

        let sub2 = retrieve(&g, &[1.0f32, 0.0], 1, 2).unwrap();
        let mut n2 = sub2.nodes.clone();
        n2.sort_unstable();
        assert_eq!(n2, vec![a, b, c]);
    }

    /// The `max_nodes` cap and the `truncated` flag hold on the undirected
    /// path: an incoming star larger than the cap is cut off and says so.
    #[test]
    fn retrieve_undirected_expansion_caps_and_reports_truncation() {
        let (_dir, g) = open_tmp();
        let hub = g.add_node("N", &json!({})).unwrap();
        for _ in 0..4 {
            let leaf = g.add_node("N", &json!({})).unwrap();
            g.add_edge(leaf, hub, "E", &json!({})).unwrap();
        }
        g.upsert_vector(hub, &[1.0f32, 0.0]).unwrap();

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
        assert!(sub.nodes.contains(&hub), "the seed survives the cap");
        assert!(
            sub.truncated,
            "the cap dropped reachable incoming neighbors"
        );
    }

    /// `retrieve_hybrid` expands over both directions too: a text-seeded
    /// entity pulls in the chunk that mentions it.
    #[test]
    fn hybrid_retrieve_expands_over_incoming_edges() {
        let (_dir, g) = open_tmp();
        let entity = g
            .add_node("Entity", &json!({"name": "cassava root"}))
            .unwrap();
        let chunk = g.add_node("Chunk", &json!({})).unwrap();
        let e = g.add_edge(chunk, entity, "MENTIONS", &json!({})).unwrap();
        g.update(|txn| txn.create_node_text_index("Entity", "name"))
            .unwrap();

        let sub = retrieve_hybrid(
            &g,
            &[],
            "cassava",
            &HybridRetrieveOptions {
                vector_k: 0,
                text_k: 5,
                hops: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let mut nodes = sub.nodes.clone();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![entity, chunk]);
        assert_eq!(sub.edges, vec![e]);
    }

    #[test]
    fn hybrid_retrieve_text_only_returns_text_seeds() {
        let (_dir, g) = open_tmp();
        let a = g
            .add_node("Doc", &json!({"body": "quantum computing research"}))
            .unwrap();
        let b = g
            .add_node("Doc", &json!({"body": "classical music orchestra"}))
            .unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();
        g.rebuild_csr().unwrap();

        // vector_k=0 disables vector search; only text seeds are used.
        let sub = retrieve_hybrid(
            &g,
            &[],
            "quantum",
            &HybridRetrieveOptions {
                vector_k: 0,
                text_k: 5,
                text_label: Some("Doc".into()),
                text_property: Some("body".into()),
                hops: 0,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            sub.nodes.len(),
            1,
            "only the text-matching node should appear"
        );
        assert_eq!(sub.nodes[0], a);
        assert!(sub.scores.contains_key(&a));
        assert!(!sub.nodes.contains(&b), "non-matching node must be absent");
    }
}
