//! Pokec social-network benchmarks.
//!
//! The Pokec dataset is the Slovak social network from Stanford SNAP:
//!   <https://snap.stanford.edu/data/soc-pokec.html>
//!
//! # Real data
//!
//! ```text
//! ISSUNDB_BENCH_POKEC_DIR=/path/to/snap \
//! ISSUNDB_BENCH_POKEC_SIZE=medium \
//! cargo bench -p issundb-core --bench pokec
//! ```
//!
//! Expected files in the directory:
//! - `soc-pokec-profiles.txt`: tab-separated columns: user_id, public,
//!   completion_percentage, gender, region, last_login, registration, age, …
//! - `soc-pokec-relationships.txt`: tab-separated: from_user_id, to_user_id
//!
//! # Synthetic data (default)
//!
//! When `ISSUNDB_BENCH_POKEC_DIR` is not set a random graph is generated at
//! the chosen size.
//!
//! Sizes controlled by `ISSUNDB_BENCH_POKEC_SIZE`:
//! - `small` (default): 10 000 nodes, 121 716 edges
//! - `medium`: 100 000 nodes, 1 768 515 edges

use std::{
    collections::HashSet,
    env,
    fs::File,
    io::{self, BufRead},
    path::PathBuf,
};

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Node property schema (mirrors the Pokec user relation)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserProps {
    cmpl_pct: i32,
    gender: Option<String>,
    age: Option<i32>,
}

// ---------------------------------------------------------------------------
// Benchmark state: graph + temp directory + stable node-ID list
// Fields are declared in drop order: graph first (closes LMDB), dir last
// (removes the directory).
// ---------------------------------------------------------------------------

struct BenchState {
    graph: Graph,
    node_ids: Vec<u64>,
    _dir: TempDir,
}

// ---------------------------------------------------------------------------
// Dataset sizes
// ---------------------------------------------------------------------------

fn pokec_size() -> (usize, usize) {
    match env::var("ISSUNDB_BENCH_POKEC_SIZE")
        .unwrap_or_else(|_| "small".into())
        .as_str()
    {
        "medium" => (100_000, 1_768_515),
        "large" => (1_632_803, 30_622_564),
        _ => (10_000, 121_716),
    }
}

// ---------------------------------------------------------------------------
// Graph builders
// ---------------------------------------------------------------------------

fn load_synthetic(n_nodes: usize, n_edges: usize) -> BenchState {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 4).unwrap();
    let mut rng = rand::thread_rng();

    let mut ids = Vec::with_capacity(n_nodes);
    for _ in 0..n_nodes {
        let props = UserProps {
            cmpl_pct: rng.gen_range(0..100),
            gender: if rng.gen_bool(0.9) {
                Some(if rng.gen_bool(0.5) { "male" } else { "female" }.into())
            } else {
                None
            },
            age: if rng.gen_bool(0.8) {
                Some(rng.gen_range(14..80))
            } else {
                None
            },
        };
        ids.push(g.add_node("User", &props).unwrap());
    }

    let n = ids.len();
    for k in 0..n_edges {
        let i = k % n;
        let j = (k
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407))
            % n;
        let src = ids[i];
        let dst = ids[if j == i { (j + 1) % n } else { j }];
        g.add_edge(src, dst, "Friend", &()).unwrap();
    }

    BenchState {
        graph: g,
        node_ids: ids,
        _dir: dir,
    }
}

fn load_snap(data_dir: &str, n_nodes: usize, n_edges: usize) -> BenchState {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 4).unwrap();

    let profiles_path = PathBuf::from(data_dir).join("soc-pokec-profiles.txt");
    let rels_path = PathBuf::from(data_dir).join("soc-pokec-relationships.txt");

    // uid_to_node[uid] = IssunDB NodeId (uid is 1-based in SNAP files).
    let mut uid_to_node = vec![0u64; n_nodes + 1];

    let profiles = File::open(&profiles_path).expect("soc-pokec-profiles.txt not found");
    let mut loaded = 0usize;
    for line in io::BufReader::new(profiles).lines() {
        if loaded >= n_nodes {
            break;
        }
        let line = line.unwrap();
        let mut cols = line.split('\t');
        let uid: usize = match cols.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let _public = cols.next();
        let cmpl_pct: i32 = cols.next().unwrap_or("0").parse().unwrap_or(0);
        let gender_raw = cols.next().unwrap_or("null");
        let gender = if gender_raw == "null" {
            None
        } else {
            Some(gender_raw.into())
        };
        let _region = cols.next();
        let _last_login = cols.next();
        let _registration = cols.next();
        let age_raw = cols.next().unwrap_or("null");
        let age: Option<i32> = if age_raw == "null" {
            None
        } else {
            age_raw.parse().ok()
        };

        let node_id = g
            .add_node(
                "User",
                &UserProps {
                    cmpl_pct,
                    gender,
                    age,
                },
            )
            .unwrap();
        if uid <= n_nodes {
            uid_to_node[uid] = node_id;
        }
        loaded += 1;
    }

    let rels = File::open(&rels_path).expect("soc-pokec-relationships.txt not found");
    let mut edge_count = 0usize;
    for line in io::BufReader::new(rels).lines() {
        if edge_count >= n_edges {
            break;
        }
        let line = line.unwrap();
        let mut cols = line.split('\t');
        let fr: usize = match cols.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let to: usize = match cols.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        if fr <= n_nodes && to <= n_nodes && uid_to_node[fr] != 0 && uid_to_node[to] != 0 {
            g.add_edge(uid_to_node[fr], uid_to_node[to], "Friend", &())
                .unwrap();
            edge_count += 1;
        }
    }

    let ids: Vec<u64> = uid_to_node.into_iter().filter(|&id| id != 0).collect();
    BenchState {
        graph: g,
        node_ids: ids,
        _dir: dir,
    }
}

fn setup() -> BenchState {
    let (n_nodes, n_edges) = pokec_size();
    match env::var("ISSUNDB_BENCH_POKEC_DIR") {
        Ok(dir) => {
            eprintln!("[pokec] loading SNAP data from {dir} ({n_nodes} nodes, {n_edges} edges)");
            load_snap(&dir, n_nodes, n_edges)
        }
        Err(_) => {
            eprintln!("[pokec] generating synthetic graph ({n_nodes} nodes, {n_edges} edges)");
            load_synthetic(n_nodes, n_edges)
        }
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn random_id(ids: &[u64]) -> u64 {
    ids[rand::thread_rng().gen_range(0..ids.len())]
}

fn age_from_props(props: &[u8]) -> Option<i32> {
    rmp_serde::from_slice::<UserProps>(props).ok()?.age
}

fn is_adult(graph: &Graph, id: u64) -> bool {
    graph
        .get_node(id)
        .ok()
        .flatten()
        .and_then(|r| age_from_props(&r.props))
        .is_some_and(|age| age >= 18)
}

fn expand_k(graph: &Graph, start: u64, hops: u8) -> Vec<u64> {
    let mut frontier = vec![start];
    let mut visited: HashSet<u64> = HashSet::from([start]);
    for _ in 0..hops {
        let mut next = Vec::new();
        for node in &frontier {
            for ne in graph.out_neighbors(*node).unwrap_or_default() {
                if visited.insert(ne.node) {
                    next.push(ne.node);
                }
            }
        }
        frontier = next;
    }
    visited.into_iter().collect()
}

fn expand_k_filter(graph: &Graph, start: u64, hops: u8) -> Vec<u64> {
    expand_k(graph, start, hops)
        .into_iter()
        .filter(|&id| is_adult(graph, id))
        .collect()
}

fn neighbours_2(graph: &Graph, start: u64, filter: bool, fetch_data: bool) -> Vec<u64> {
    let hop1: Vec<u64> = graph
        .out_neighbors(start)
        .unwrap_or_default()
        .into_iter()
        .map(|ne| ne.node)
        .collect();
    let mut result: HashSet<u64> = hop1.iter().copied().collect();
    for &mid in &hop1 {
        for ne in graph.out_neighbors(mid).unwrap_or_default() {
            result.insert(ne.node);
        }
    }
    let mut ids: Vec<u64> = result.into_iter().collect();
    if filter {
        ids.retain(|&id| is_adult(graph, id));
    }
    if fetch_data {
        for &id in &ids {
            let _ = graph.get_node(id);
        }
    }
    ids
}

fn find_4hop_chain(graph: &Graph, start: u64) -> Option<u64> {
    for ne2 in graph.out_neighbors(start).unwrap_or_default() {
        for ne3 in graph.out_neighbors(ne2.node).unwrap_or_default() {
            for ne4 in graph.out_neighbors(ne3.node).unwrap_or_default() {
                if let Some(ne5) = graph
                    .in_neighbors(ne4.node)
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                {
                    return Some(ne5.node);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Benchmark functions
// ---------------------------------------------------------------------------

fn bench_single_vertex_read(c: &mut Criterion, state: &BenchState) {
    c.bench_function("single_vertex_read", |b| {
        b.iter(|| {
            let id = random_id(&state.node_ids);
            state.graph.get_node(id).unwrap()
        })
    });
}

fn bench_single_vertex_write(c: &mut Criterion, state: &BenchState) {
    c.bench_function("single_vertex_write", |b| {
        b.iter(|| {
            let props = UserProps {
                cmpl_pct: 0,
                gender: None,
                age: None,
            };
            state.graph.add_node("User", &props).unwrap()
        })
    });
}

fn bench_single_edge_write(c: &mut Criterion, state: &BenchState) {
    let ids = &state.node_ids;
    c.bench_function("single_edge_write", |b| {
        b.iter(|| {
            let mut rng = rand::thread_rng();
            let i = rng.gen_range(0..ids.len());
            let mut j = rng.gen_range(0..ids.len());
            while j == i {
                j = rng.gen_range(0..ids.len());
            }
            state.graph.add_edge(ids[i], ids[j], "Friend", &()).unwrap()
        })
    });
}

fn bench_single_vertex_update(c: &mut Criterion, state: &BenchState) {
    c.bench_function("single_vertex_update", |b| {
        b.iter(|| {
            let id = random_id(&state.node_ids);
            let props = UserProps {
                cmpl_pct: -1,
                gender: None,
                age: None,
            };
            state.graph.update_node(id, &props).unwrap()
        })
    });
}

fn bench_expansion_1_plain(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_1_plain", |b| {
        b.iter(|| expand_k(&state.graph, random_id(&state.node_ids), 1))
    });
}

fn bench_expansion_2_plain(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_2_plain", |b| {
        b.iter(|| expand_k(&state.graph, random_id(&state.node_ids), 2))
    });
}

fn bench_expansion_3_plain(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_3_plain", |b| {
        b.iter(|| expand_k(&state.graph, random_id(&state.node_ids), 3))
    });
}

fn bench_expansion_4_plain(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_4_plain", |b| {
        b.iter(|| expand_k(&state.graph, random_id(&state.node_ids), 4))
    });
}

fn bench_expansion_1_filter(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_1_filter", |b| {
        b.iter(|| expand_k_filter(&state.graph, random_id(&state.node_ids), 1))
    });
}

fn bench_expansion_2_filter(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_2_filter", |b| {
        b.iter(|| expand_k_filter(&state.graph, random_id(&state.node_ids), 2))
    });
}

fn bench_expansion_3_filter(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_3_filter", |b| {
        b.iter(|| expand_k_filter(&state.graph, random_id(&state.node_ids), 3))
    });
}

fn bench_expansion_4_filter(c: &mut Criterion, state: &BenchState) {
    c.bench_function("expansion_4_filter", |b| {
        b.iter(|| expand_k_filter(&state.graph, random_id(&state.node_ids), 4))
    });
}

fn bench_neighbours_2_plain(c: &mut Criterion, state: &BenchState) {
    c.bench_function("neighbours_2_plain", |b| {
        b.iter(|| neighbours_2(&state.graph, random_id(&state.node_ids), false, false))
    });
}

fn bench_neighbours_2_filter_only(c: &mut Criterion, state: &BenchState) {
    c.bench_function("neighbours_2_filter_only", |b| {
        b.iter(|| neighbours_2(&state.graph, random_id(&state.node_ids), true, false))
    });
}

fn bench_neighbours_2_data_only(c: &mut Criterion, state: &BenchState) {
    c.bench_function("neighbours_2_data_only", |b| {
        b.iter(|| neighbours_2(&state.graph, random_id(&state.node_ids), false, true))
    });
}

fn bench_neighbours_2_filter_data(c: &mut Criterion, state: &BenchState) {
    c.bench_function("neighbours_2_filter_data", |b| {
        b.iter(|| neighbours_2(&state.graph, random_id(&state.node_ids), true, true))
    });
}

fn bench_pattern_cycle(c: &mut Criterion, state: &BenchState) {
    c.bench_function("pattern_cycle", |b| {
        b.iter(|| {
            let id = random_id(&state.node_ids);
            let out: HashSet<u64> = state
                .graph
                .out_neighbors(id)
                .unwrap_or_default()
                .into_iter()
                .map(|ne| ne.node)
                .collect();
            let incoming: HashSet<u64> = state
                .graph
                .in_neighbors(id)
                .unwrap_or_default()
                .into_iter()
                .map(|ne| ne.node)
                .collect();
            out.intersection(&incoming).copied().collect::<Vec<_>>()
        })
    });
}

fn bench_pattern_long(c: &mut Criterion, state: &BenchState) {
    c.bench_function("pattern_long", |b| {
        b.iter(|| find_4hop_chain(&state.graph, random_id(&state.node_ids)))
    });
}

fn bench_pattern_short(c: &mut Criterion, state: &BenchState) {
    c.bench_function("pattern_short", |b| {
        b.iter(|| {
            let id = random_id(&state.node_ids);
            state
                .graph
                .out_neighbors(id)
                .unwrap_or_default()
                .into_iter()
                .next()
                .map(|ne| ne.node)
        })
    });
}

fn bench_pagerank(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("pokec_pagerank");
    group.sample_size(10);
    group.bench_function("pagerank", |b| {
        b.iter(|| state.graph.page_rank(20, 0.85).unwrap())
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

fn pokec_benchmarks(c: &mut Criterion) {
    let state = setup();

    bench_single_vertex_read(c, &state);
    bench_single_vertex_write(c, &state);
    bench_single_edge_write(c, &state);
    bench_single_vertex_update(c, &state);

    bench_expansion_1_plain(c, &state);
    bench_expansion_2_plain(c, &state);
    bench_expansion_3_plain(c, &state);
    bench_expansion_4_plain(c, &state);
    bench_expansion_1_filter(c, &state);
    bench_expansion_2_filter(c, &state);
    bench_expansion_3_filter(c, &state);
    bench_expansion_4_filter(c, &state);

    bench_neighbours_2_plain(c, &state);
    bench_neighbours_2_filter_only(c, &state);
    bench_neighbours_2_data_only(c, &state);
    bench_neighbours_2_filter_data(c, &state);

    bench_pattern_cycle(c, &state);
    bench_pattern_long(c, &state);
    bench_pattern_short(c, &state);

    bench_pagerank(c, &state);
}

criterion_group!(pokec_benches, pokec_benchmarks);
criterion_main!(pokec_benches);
