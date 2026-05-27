//! Wikipedia PageRank benchmark.
//!
//! The Wikipedia article link graph is available from Stanford SNAP:
//!   <https://snap.stanford.edu/data/wiki-topcats.html>
//!
//! # Real data
//!
//! ```text
//! ISSUNDB_BENCH_WIKI_DIR=/path/to/snapdir \
//! cargo bench -p issundb-core --bench wiki_pagerank
//! ```
//!
//! Expected file: `wikipedia-articles.el` with space-separated pairs `fr to`
//! on each line.
//!
//! # Synthetic data (default)
//!
//! When `ISSUNDB_BENCH_WIKI_DIR` is not set a synthetic directed graph with
//! 500 000 nodes and 2 000 000 edges is generated.

use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{self, BufRead},
    path::PathBuf,
};

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use rand::Rng;
use tempfile::TempDir;

// Synthetic-graph parameters.
const SYNTH_NODES: usize = 500_000;
const SYNTH_EDGES: usize = 2_000_000;

// ---------------------------------------------------------------------------
// Benchmark state
// Fields are declared in drop order: graph first, dir last.
// ---------------------------------------------------------------------------

struct BenchState {
    graph: Graph,
    _dir: TempDir,
}

// ---------------------------------------------------------------------------
// Graph builders
// ---------------------------------------------------------------------------

fn load_synthetic() -> BenchState {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 8).unwrap();
    let mut rng = rand::thread_rng();

    let mut ids = Vec::with_capacity(SYNTH_NODES);
    for i in 0..SYNTH_NODES {
        ids.push(g.add_node("Article", &i).unwrap());
    }
    let n = ids.len();
    for i in 0..SYNTH_EDGES {
        let src = ids[i % n];
        let dst = ids[(i
            .wrapping_mul(6_364_136_223_846_793_005_usize)
            .wrapping_add(rng.gen_range(0..n)))
            % n];
        g.add_edge(src, dst, "Link", &()).unwrap();
    }
    BenchState {
        graph: g,
        _dir: dir,
    }
}

fn load_snap(data_dir: &str) -> BenchState {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 8).unwrap();

    let file_path = PathBuf::from(data_dir).join("wikipedia-articles.el");
    let file = File::open(&file_path).expect("wikipedia-articles.el not found");

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let line = line.unwrap();
        if line.len() < 2 {
            continue;
        }
        let mut s = line.split_whitespace();
        let fr: i64 = match s.next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let to: i64 = match s.next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        pairs.push((fr, to));
    }

    // Insert one node per unique article ID, then insert edges.
    let mut id_map: HashMap<i64, u64> = HashMap::new();
    for &(fr, to) in &pairs {
        id_map
            .entry(fr)
            .or_insert_with(|| g.add_node("Article", &fr).unwrap());
        id_map
            .entry(to)
            .or_insert_with(|| g.add_node("Article", &to).unwrap());
    }
    for (fr, to) in pairs {
        g.add_edge(id_map[&fr], id_map[&to], "Link", &()).unwrap();
    }

    BenchState {
        graph: g,
        _dir: dir,
    }
}

fn setup() -> BenchState {
    match env::var("ISSUNDB_BENCH_WIKI_DIR") {
        Ok(dir) => {
            eprintln!("[wiki_pagerank] loading SNAP data from {dir}");
            load_snap(&dir)
        }
        Err(_) => {
            eprintln!(
                "[wiki_pagerank] generating synthetic graph \
                 ({SYNTH_NODES} nodes, {SYNTH_EDGES} edges)"
            );
            load_synthetic()
        }
    }
}

// ---------------------------------------------------------------------------
// Benchmark
// ---------------------------------------------------------------------------

fn bench_wikipedia_pagerank(c: &mut Criterion) {
    let state = setup();
    let mut group = c.benchmark_group("wiki_pagerank");
    group.sample_size(10);
    group.bench_function("wikipedia_pagerank", |b| {
        b.iter(|| state.graph.page_rank(20, 0.85).unwrap())
    });
    group.finish();
}

criterion_group!(wiki_benches, bench_wikipedia_pagerank);
criterion_main!(wiki_benches);
