//! Profiling driver for the cyclic triangle-count query, the worst gap the
//! LadybugDB comparison harness exposed. Loads a Zipf-skewed Person/KNOWS
//! graph into a persistent directory (reused across runs, so a profiler can
//! observe query execution without load noise) and runs the triangle count a
//! fixed number of times.
//!
//! Knobs, all environment variables:
//! - `PROFILE_TRIANGLE_NODES`: Person node count (default 2000)
//! - `PROFILE_TRIANGLE_EDGES`: KNOWS edge count (default 8000)
//! - `PROFILE_TRIANGLE_REPS`: query repetitions (default 3)
//! - `PROFILE_TRIANGLE_DB`: database directory (default
//!   `$TMPDIR/issundb-triangle-profile-<nodes>-<edges>`)

use std::collections::HashSet;
use std::time::Instant;

use issundb::{Graph, GraphQueryExt};

/// Zipf exponent matching the comparison harness's skewed mode.
const ZIPF_THETA: f64 = 0.8;

fn var(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Deterministic 64-bit LCG (Knuth MMIX constants), same as the harness.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }

    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 48) as f64
    }
}

/// Cumulative Zipf distribution over node indices, same as the harness.
struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    fn new(n: u64) -> Self {
        let mut cdf = Vec::with_capacity(n as usize);
        let mut acc = 0.0;
        for rank in 1..=n {
            acc += 1.0 / (rank as f64).powf(ZIPF_THETA);
            cdf.push(acc);
        }
        for v in &mut cdf {
            *v /= acc;
        }
        Zipf { cdf }
    }

    fn sample(&self, u: f64) -> u64 {
        self.cdf.partition_point(|&c| c < u) as u64
    }
}

fn load(graph: &Graph, nodes: u64, edges: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut rng = Lcg(0x1554_4ED1);
    let zipf = Zipf::new(nodes);
    let mut node_ids = Vec::with_capacity(nodes as usize);
    for id in 0..nodes {
        node_ids.push(graph.add_node("Person", &serde_json::json!({ "id": id }))?);
    }
    let mut seen = HashSet::new();
    let mut inserted = 0u64;
    while inserted < edges {
        let src = zipf.sample(rng.unit());
        let dst = zipf.sample(rng.unit());
        if src == dst || !seen.insert((src, dst)) {
            continue;
        }
        graph.add_edge(
            node_ids[src as usize],
            node_ids[dst as usize],
            "KNOWS",
            &serde_json::json!({}),
        )?;
        inserted += 1;
    }
    graph.rebuild_csr()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nodes = var("PROFILE_TRIANGLE_NODES", 2_000);
    let edges = var("PROFILE_TRIANGLE_EDGES", 8_000);
    let reps = var("PROFILE_TRIANGLE_REPS", 3);
    let db_dir = std::env::var("PROFILE_TRIANGLE_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("issundb-triangle-profile-{nodes}-{edges}"))
        });

    let fresh = !db_dir.join("data.mdb").exists();
    std::fs::create_dir_all(&db_dir)?;
    let graph = Graph::open(&db_dir, 2)?;
    if fresh {
        let start = Instant::now();
        load(&graph, nodes, edges)?;
        eprintln!(
            "loaded {nodes} nodes, {edges} edges (zipf) into {db_dir:?} in {:?}",
            start.elapsed()
        );
    } else {
        eprintln!("reusing {db_dir:?}");
    }

    let query = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
                 RETURN count(a) AS n";
    for rep in 0..reps {
        let start = Instant::now();
        let result = graph.query(query)?;
        eprintln!(
            "rep {rep}: {:?} (result: {:?})",
            start.elapsed(),
            result.records[0].values[0]
        );
    }
    Ok(())
}
