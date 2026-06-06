//! Profiling driver for an arbitrary Cypher query over the comparison
//! harness's Person/KNOWS schema (name, age, and city properties). Loads a
//! uniform random graph into a persistent directory (reused across runs, so a
//! profiler observes query execution without load noise) and reruns the query
//! a fixed number of times.
//!
//! Knobs, all environment variables:
//! - `PROFILE_QUERY`: the Cypher query to run (default: the harness's
//!   `prop_projection` query)
//! - `PROFILE_QUERY_NODES`: Person node count (default 10000)
//! - `PROFILE_QUERY_EDGES`: KNOWS edge count (default 50000)
//! - `PROFILE_QUERY_REPS`: query repetitions (default 3)
//! - `PROFILE_QUERY_DB`: database directory (default
//!   `$TMPDIR/issundb-query-profile-<nodes>-<edges>`)

use std::collections::HashSet;
use std::time::Instant;

use issundb::{Graph, GraphQueryExt};

const CITIES: [&str; 7] = [
    "london",
    "paris",
    "berlin",
    "madrid",
    "rome",
    "amsterdam",
    "oslo",
];

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
}

fn load(graph: &Graph, nodes: u64, edges: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut rng = Lcg(0x1554_4ED1);
    graph.update(|txn| {
        let mut node_ids = Vec::with_capacity(nodes as usize);
        for id in 0..nodes {
            let name = format!("person{id}");
            let age = 18 + (id % 60);
            let city = CITIES[(id % CITIES.len() as u64) as usize];
            node_ids.push(txn.add_node(
                "Person",
                &serde_json::json!({ "id": id, "name": name, "age": age, "city": city }),
            )?);
        }
        let mut seen = HashSet::new();
        let mut inserted = 0u64;
        while inserted < edges {
            let src = rng.next() % nodes;
            let dst = rng.next() % nodes;
            if src == dst || !seen.insert((src, dst)) {
                continue;
            }
            txn.add_edge(
                node_ids[src as usize],
                node_ids[dst as usize],
                "KNOWS",
                &serde_json::json!({ "weight": 1.0 }),
            )?;
            inserted += 1;
        }
        Ok(())
    })?;
    graph.rebuild_csr()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nodes = var("PROFILE_QUERY_NODES", 10_000);
    let edges = var("PROFILE_QUERY_EDGES", 50_000);
    let reps = var("PROFILE_QUERY_REPS", 3);
    let query = std::env::var("PROFILE_QUERY").unwrap_or_else(|_| {
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         RETURN b.name AS name, b.age AS age, b.city AS city"
            .to_string()
    });
    let db_dir = std::env::var("PROFILE_QUERY_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("issundb-query-profile-{nodes}-{edges}"))
        });

    let fresh = !db_dir.join("data.mdb").exists();
    std::fs::create_dir_all(&db_dir)?;
    let graph = Graph::open(&db_dir, 2)?;
    if fresh {
        let start = Instant::now();
        load(&graph, nodes, edges)?;
        eprintln!(
            "loaded {nodes} nodes, {edges} edges (uniform) into {db_dir:?} in {:?}",
            start.elapsed()
        );
    } else {
        eprintln!("reusing {db_dir:?}");
    }

    for rep in 0..reps {
        let start = Instant::now();
        let result = graph.query(&query)?;
        eprintln!("rep {rep}: {:?} ({} rows)", start.elapsed(), result.records.len());
    }
    Ok(())
}
