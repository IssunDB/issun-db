//! Differential tests against a NetworkX oracle corpus.
//!
//! The corpus in `tests/fixtures/networkx_oracle.json` is generated offline by
//! `tools/gen_oracle_fixtures.py` (run via `make oracle-fixtures`) and committed
//! to the repository. Each case is a simple directed graph plus the reference
//! output NetworkX computes for a fixed set of algorithms. This test replays
//! each graph through the `issundb` facade and asserts that IssunDB agrees with
//! NetworkX. The NetworkX dependency lives only in the generator, so this test
//! stays hermetic, deterministic, and pure Rust.
//!
//! The exact, spec-unambiguous algorithms (weakly and strongly connected
//! components, unweighted shortest-path length, and maximum flow over integer
//! capacities) are compared over the general corpus in `networkx_oracle.json`;
//! for these a mismatch is unambiguously a bug.
//!
//! PageRank is spec-sensitive, so it is compared over a separate corpus
//! (`networkx_pagerank.json`) restricted to graphs with no dangling nodes. On
//! that subclass IssunDB's fixed-iteration power method (which does not
//! redistribute dangling-node mass) converges to the same stationary
//! distribution NetworkX computes; see `tools/gen_pagerank_fixtures.py` for the
//! convention analysis. The centralities (betweenness, harmonic) remain out of
//! scope pending a verified normalization and directedness match.

use std::collections::HashMap;

use issundb::{DegreeDirection, EdgeId, Graph, NodeId};
use serde::Deserialize;
use serde_json::json;
use tempfile::TempDir;

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    n: usize,
    edges: Vec<[usize; 2]>,
    capacities: Vec<u32>,
    /// Each inner list is one weakly connected component, as node indices.
    wcc: Vec<Vec<usize>>,
    /// Each inner list is one strongly connected component, as node indices.
    scc: Vec<Vec<usize>>,
    /// `[src, dst, len]`; `len == -1` marks an unreachable pair.
    sp_len: Vec<[i64; 3]>,
    /// `[src, sink, value]`; maximum flow over integer capacities.
    maxflow: Vec<[i64; 3]>,
    /// True when the graph is acyclic.
    is_dag: bool,
    /// In, out, and total degree per node.
    degree: Degrees,
    /// `(src, dst, weight)` weighted shortest-path length using `capacity` as the
    /// weight; `weight == -1.0` marks an unreachable pair.
    dijkstra: Vec<(usize, usize, f64)>,
    /// `(src, dst, paths)` for reachable pairs, where `paths` is the set of all
    /// shortest paths as node-index lists, sorted.
    all_sp: Vec<(usize, usize, Vec<Vec<usize>>)>,
    /// `(start, hops, nodes)`; the set of nodes within `hops` of `start`,
    /// sorted. Both `bfs` and `dfs` return this set.
    bfs: Vec<(usize, u8, Vec<usize>)>,
    /// `(src, dst, weights)`; ascending total weights of up to k=3 loopless
    /// shortest paths, weighted by `capacity`.
    top_k: Vec<(usize, usize, Vec<f64>)>,
    /// Minimum and maximum spanning forest total weight and edge count, over the
    /// `capacity` weights.
    spanning: Spanning,
}

#[derive(Deserialize)]
struct Spanning {
    min: SpanningForest,
    max: SpanningForest,
}

#[derive(Deserialize)]
struct SpanningForest {
    /// Total weight of the forest edges.
    weight: f64,
    /// Number of edges in the forest.
    edges: usize,
}

#[derive(Deserialize)]
struct Degrees {
    #[serde(rename = "in")]
    incoming: Vec<u64>,
    out: Vec<u64>,
    both: Vec<u64>,
}

fn load_corpus() -> Corpus {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/networkx_oracle.json"
    );
    let text = std::fs::read_to_string(path).expect("read oracle fixture corpus");
    serde_json::from_str(&text).expect("parse oracle fixture corpus")
}

/// Build a graph from a case and return the node IDs indexed by case node index.
///
/// Capacities are stored as the `capacity` edge property so `maximum_flow` can
/// read them back by name.
fn build(case: &Case) -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let ids: Vec<NodeId> = (0..case.n)
        .map(|_| g.add_node("N", &json!({})).unwrap())
        .collect();
    for (edge, &cap) in case.edges.iter().zip(&case.capacities) {
        g.add_edge(ids[edge[0]], ids[edge[1]], "E", &json!({ "capacity": cap }))
            .unwrap();
    }
    g.rebuild_csr().unwrap();
    (dir, g, ids)
}

/// Canonicalize a component map into sorted lists of node indices, matching the
/// shape the generator emits. The component label values are irrelevant; only
/// the induced partition (which nodes share a component) is compared.
fn canonical_partition(labels: &HashMap<NodeId, u64>, ids: &[NodeId]) -> Vec<Vec<usize>> {
    let index_of: HashMap<NodeId, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for (&id, &label) in labels {
        groups.entry(label).or_default().push(index_of[&id]);
    }
    let mut parts: Vec<Vec<usize>> = groups
        .into_values()
        .map(|mut g| {
            g.sort_unstable();
            g
        })
        .collect();
    parts.sort();
    parts
}

#[test]
fn connected_components_match_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        let got = canonical_partition(&g.connected_components().unwrap(), &ids);
        assert_eq!(
            got, case.wcc,
            "case {}: weakly connected components mismatch",
            case.id
        );
    }
}

#[test]
fn strongly_connected_components_match_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        let got = canonical_partition(&g.strongly_connected_components().unwrap(), &ids);
        assert_eq!(
            got, case.scc,
            "case {}: strongly connected components mismatch",
            case.id
        );
    }
}

#[test]
fn shortest_path_lengths_match_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for &[s, t, expected] in &case.sp_len {
            let path = g.shortest_path(ids[s as usize], ids[t as usize]).unwrap();
            let got: i64 = match path {
                Some(p) => (p.len() as i64) - 1,
                None => -1,
            };
            assert_eq!(
                got, expected,
                "case {}: shortest_path({s}, {t}) length mismatch",
                case.id
            );
        }
    }
}

#[test]
fn maximum_flow_matches_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for &[s, t, expected] in &case.maxflow {
            let got = g
                .maximum_flow(ids[s as usize], ids[t as usize], "capacity")
                .unwrap();
            assert!(
                (got - expected as f64).abs() < 1e-6,
                "case {}: maximum_flow({s}, {t}) = {got}, expected {expected}",
                case.id
            );
        }
    }
}

#[test]
fn detect_cycle_matches_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, _ids) = build(case);
        let has_cycle = g.detect_cycle().unwrap();
        assert_eq!(
            has_cycle, !case.is_dag,
            "case {}: detect_cycle = {has_cycle}, is_dag = {}",
            case.id, case.is_dag
        );
    }
}

#[test]
fn degree_centrality_matches_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        let cases = [
            (DegreeDirection::In, &case.degree.incoming, "in"),
            (DegreeDirection::Out, &case.degree.out, "out"),
            (DegreeDirection::Both, &case.degree.both, "both"),
        ];
        for (dir, expected, name) in cases {
            let got = g.degree_centrality(dir).unwrap();
            for (i, &id) in ids.iter().enumerate() {
                assert_eq!(
                    got[&id], expected[i],
                    "case {}: {name}-degree[{i}] = {}, expected {}",
                    case.id, got[&id], expected[i]
                );
            }
        }
    }
}

#[test]
fn shortest_path_dijkstra_matches_networkx() {
    // IssunDB's weighted shortest path reads the `weight`-then-`capacity` edge
    // property; the corpus stores capacities, so it weights on `capacity`.
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for &(s, t, expected) in &case.dijkstra {
            let path = g.shortest_path_dijkstra(ids[s], ids[t]).unwrap();
            match path {
                Some(p) => assert!(
                    expected >= 0.0 && (p.total_weight - expected).abs() < 1e-6,
                    "case {}: dijkstra({s}, {t}) = {}, expected {expected}",
                    case.id,
                    p.total_weight
                ),
                None => assert!(
                    expected < 0.0,
                    "case {}: dijkstra({s}, {t}) = unreachable, expected {expected}",
                    case.id
                ),
            }
        }
    }
}

#[test]
fn all_shortest_paths_match_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        let index_of: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        for (s, t, expected) in &case.all_sp {
            let got = g.all_shortest_paths(ids[*s], ids[*t]).unwrap();
            let mut got_idx: Vec<Vec<usize>> = got
                .into_iter()
                .map(|p| p.into_iter().map(|id| index_of[&id]).collect())
                .collect();
            got_idx.sort();
            assert_eq!(
                &got_idx, expected,
                "case {}: all_shortest_paths({s}, {t}) mismatch",
                case.id
            );
        }
    }
}

/// Returned node IDs mapped back to case node indices and sorted, for set
/// comparison against the fixture's node-index lists.
fn sorted_indices(nodes: Vec<NodeId>, ids: &[NodeId]) -> Vec<usize> {
    let index_of: HashMap<NodeId, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut v: Vec<usize> = nodes.into_iter().map(|id| index_of[&id]).collect();
    v.sort_unstable();
    v
}

#[test]
fn bfs_matches_networkx() {
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for (start, hops, expected) in &case.bfs {
            let got = sorted_indices(g.bfs(ids[*start], *hops).unwrap(), &ids);
            assert_eq!(
                &got, expected,
                "case {}: bfs({start}, {hops}) reachable-set mismatch",
                case.id
            );
        }
    }
}

#[test]
fn dfs_matches_networkx() {
    // DFS visits in a different order than BFS but must reach the same set of
    // nodes within `hops`.
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for (start, hops, expected) in &case.bfs {
            let got = sorted_indices(g.dfs(ids[*start], *hops).unwrap(), &ids);
            assert_eq!(
                &got, expected,
                "case {}: dfs({start}, {hops}) reachable-set mismatch",
                case.id
            );
        }
    }
}

#[test]
fn shortest_path_top_k_matches_networkx() {
    const K: usize = 3;
    const TOL: f64 = 1e-6;
    let corpus = load_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build(case);
        for (s, t, expected) in &case.top_k {
            let paths = g
                .shortest_path_top_k(ids[*s], ids[*t], K, "capacity")
                .unwrap();
            let mut got: Vec<f64> = paths.iter().map(|p| p.total_weight).collect();
            got.sort_by(|a, b| a.partial_cmp(b).unwrap());
            assert_eq!(
                got.len(),
                expected.len(),
                "case {}: top_k({s}, {t}) returned {} paths, expected {}",
                case.id,
                got.len(),
                expected.len()
            );
            for (g_w, e_w) in got.iter().zip(expected) {
                assert!(
                    (g_w - e_w).abs() < TOL,
                    "case {}: top_k({s}, {t}) weight {g_w}, expected {e_w}",
                    case.id
                );
            }
        }
    }
}

#[derive(Deserialize)]
struct PageRankCorpus {
    meta: PageRankMeta,
    cases: Vec<PageRankCase>,
}

#[derive(Deserialize)]
struct PageRankMeta {
    alpha: f32,
}

#[derive(Deserialize)]
struct PageRankCase {
    id: String,
    n: usize,
    edges: Vec<[usize; 2]>,
    /// Reference PageRank indexed by node, computed by NetworkX to a tight
    /// tolerance; the values sum to 1.
    pagerank: Vec<f64>,
}

#[test]
fn pagerank_matches_networkx() {
    // The Rust side runs a fixed iteration count comfortably past convergence
    // (alpha^200 is far below the comparison tolerance), and the slack tolerance
    // absorbs f32 rounding across platforms while staying far below the shift any
    // formula error would cause: the corpus agrees to better than 1e-5 here.
    const ITERATIONS: u32 = 200;
    const TOL: f64 = 1e-4;

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/networkx_pagerank.json"
    );
    let text = std::fs::read_to_string(path).expect("read pagerank fixture corpus");
    let corpus: PageRankCorpus =
        serde_json::from_str(&text).expect("parse pagerank fixture corpus");

    for case in &corpus.cases {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..case.n)
            .map(|_| g.add_node("N", &json!({})).unwrap())
            .collect();
        for edge in &case.edges {
            g.add_edge(ids[edge[0]], ids[edge[1]], "E", &json!({}))
                .unwrap();
        }
        g.rebuild_csr().unwrap();

        let ranks = g.page_rank(ITERATIONS, corpus.meta.alpha).unwrap();
        for (i, &id) in ids.iter().enumerate() {
            let got = ranks[&id] as f64;
            let expected = case.pagerank[i];
            assert!(
                (got - expected).abs() < TOL,
                "case {}: pagerank[{i}] = {got}, expected {expected}",
                case.id
            );
        }
    }
}

#[derive(Deserialize)]
struct CentralityCorpus {
    cases: Vec<CentralityCase>,
}

#[derive(Deserialize)]
struct CentralityCase {
    id: String,
    n: usize,
    edges: Vec<[usize; 2]>,
    /// Directed Brandes betweenness, unnormalized, no endpoints, indexed by node.
    betweenness: Vec<f64>,
    /// Out-distance harmonic centrality (sum of 1/d(u, v) over v reachable from
    /// u), indexed by node. See `tools/gen_centrality_fixtures.py` for why the
    /// generator reverses the graph before calling NetworkX.
    harmonic: Vec<f64>,
}

fn load_centrality_corpus() -> CentralityCorpus {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/networkx_centrality.json"
    );
    let text = std::fs::read_to_string(path).expect("read centrality fixture corpus");
    serde_json::from_str(&text).expect("parse centrality fixture corpus")
}

/// Build an unweighted directed graph from a centrality case.
fn build_unweighted(case: &CentralityCase) -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let ids: Vec<NodeId> = (0..case.n)
        .map(|_| g.add_node("N", &json!({})).unwrap())
        .collect();
    for edge in &case.edges {
        g.add_edge(ids[edge[0]], ids[edge[1]], "E", &json!({}))
            .unwrap();
    }
    g.rebuild_csr().unwrap();
    (dir, g, ids)
}

#[test]
fn betweenness_centrality_matches_networkx() {
    // Brandes accumulation is exact rational arithmetic in f64, so the only
    // discrepancy is floating-point rounding.
    const TOL: f64 = 1e-6;
    let corpus = load_centrality_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build_unweighted(case);
        let got = g.betweenness_centrality().unwrap();
        for (i, &id) in ids.iter().enumerate() {
            let expected = case.betweenness[i];
            assert!(
                (got[&id] - expected).abs() < TOL,
                "case {}: betweenness[{i}] = {}, expected {expected}",
                case.id,
                got[&id]
            );
        }
    }
}

#[test]
fn harmonic_centrality_matches_networkx() {
    const TOL: f64 = 1e-6;
    let corpus = load_centrality_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build_unweighted(case);
        let got = g.harmonic_centrality().unwrap();
        for (i, &id) in ids.iter().enumerate() {
            let expected = case.harmonic[i];
            assert!(
                (got[&id] - expected).abs() < TOL,
                "case {}: harmonic[{i}] = {}, expected {expected}",
                case.id,
                got[&id]
            );
        }
    }
}

#[derive(Deserialize)]
struct PathsCorpus {
    cases: Vec<PathsCase>,
}

#[derive(Deserialize)]
struct PathsCase {
    id: String,
    n: usize,
    edges: Vec<[usize; 2]>,
    /// `(src, dst, paths)` for reachable pairs: the set of all simple paths as
    /// node-index lists, sorted.
    all_paths: Vec<(usize, usize, Vec<Vec<usize>>)>,
    /// `(src, dst, node_count)`: number of nodes on the longest simple path.
    longest: Vec<(usize, usize, usize)>,
}

fn load_paths_corpus() -> PathsCorpus {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/networkx_paths.json"
    );
    let text = std::fs::read_to_string(path).expect("read paths fixture corpus");
    serde_json::from_str(&text).expect("parse paths fixture corpus")
}

fn build_paths(case: &PathsCase) -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let ids: Vec<NodeId> = (0..case.n)
        .map(|_| g.add_node("N", &json!({})).unwrap())
        .collect();
    for edge in &case.edges {
        g.add_edge(ids[edge[0]], ids[edge[1]], "E", &json!({}))
            .unwrap();
    }
    g.rebuild_csr().unwrap();
    (dir, g, ids)
}

#[test]
fn all_paths_match_networkx() {
    let corpus = load_paths_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build_paths(case);
        let index_of: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        for (s, t, expected) in &case.all_paths {
            let got = g.all_paths(ids[*s], ids[*t]).unwrap();
            let mut got_idx: Vec<Vec<usize>> = got
                .into_iter()
                .map(|p| p.into_iter().map(|id| index_of[&id]).collect())
                .collect();
            got_idx.sort();
            assert_eq!(
                &got_idx, expected,
                "case {}: all_paths({s}, {t}) mismatch",
                case.id
            );
        }
    }
}

#[test]
fn spanning_forest_matches_networkx() {
    // The minimum/maximum spanning forest is not unique under weight ties, so
    // IssunDB's chosen edge set may differ from NetworkX's. The total weight and
    // edge count are unique, so the oracle compares those. Capacities are small
    // integers, so the weight totals are exact in f64; the tolerance only guards
    // against summation order.
    const TOL: f64 = 1e-9;
    let corpus = load_corpus();
    for case in &corpus.cases {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..case.n)
            .map(|_| g.add_node("N", &json!({})).unwrap())
            .collect();
        let mut weight_of: HashMap<EdgeId, f64> = HashMap::new();
        for (edge, &cap) in case.edges.iter().zip(&case.capacities) {
            let eid = g
                .add_edge(ids[edge[0]], ids[edge[1]], "E", &json!({ "capacity": cap }))
                .unwrap();
            weight_of.insert(eid, cap as f64);
        }
        g.rebuild_csr().unwrap();

        for (maximum, expected) in [(false, &case.spanning.min), (true, &case.spanning.max)] {
            let kind = if maximum { "max" } else { "min" };
            let forest = g.spanning_forest("capacity", maximum).unwrap();
            assert_eq!(
                forest.len(),
                expected.edges,
                "case {}: {kind} spanning forest edge count = {}, expected {}",
                case.id,
                forest.len(),
                expected.edges
            );
            let total: f64 = forest.iter().map(|eid| weight_of[eid]).sum();
            assert!(
                (total - expected.weight).abs() < TOL,
                "case {}: {kind} spanning forest weight = {total}, expected {}",
                case.id,
                expected.weight
            );
        }
    }
}

#[test]
fn longest_path_matches_networkx() {
    let corpus = load_paths_corpus();
    for case in &corpus.cases {
        let (_dir, g, ids) = build_paths(case);
        for (s, t, expected_len) in &case.longest {
            let got = g.longest_path(ids[*s], ids[*t]).unwrap();
            let got_len = got.map(|p| p.len());
            assert_eq!(
                got_len,
                Some(*expected_len),
                "case {}: longest_path({s}, {t}) node count mismatch",
                case.id
            );
        }
    }
}
