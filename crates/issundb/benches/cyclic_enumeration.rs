//! Measurement driver for the wedge blowup in cyclic pattern enumeration.
//!
//! The executor closes a cyclic MATCH by materializing every open-path row
//! from the expand chain and then hash-probing the closing edge per row
//! (`multiway_join_rows`). On a skewed graph the open-path count (wedges for a
//! triangle, 3-paths for a 4-cycle) can dwarf both the output and the work an
//! intersection-based closing hop would pay, which is the gap the Free Join
//! paper's factoring optimization targets. This driver sizes that gap before
//! any operator work: it loads a Zipf-skewed Person/KNOWS graph, computes the
//! analytic counts, and times the current execution paths.
//!
//! Reported per pattern:
//! - open paths: intermediates the expand chain materializes today.
//! - intersection bound: work a factored closing hop would pay instead, the
//!   sum over open prefixes of the smaller closing adjacency list.
//! - output rows: matches the pattern actually returns.
//! - timings: the count form (lowered to the `TriangleCount` kernel where it
//!   applies) against row-pipeline forms whose plans cannot lower.
//!
//! Knobs, all environment variables:
//! - `CYCLIC_ENUM_NODES`: Person node count (default 2000)
//! - `CYCLIC_ENUM_EDGES`: KNOWS edge count (default 8000)
//! - `CYCLIC_ENUM_REPS`: query repetitions (default 3)
//! - `CYCLIC_ENUM_DB`: database directory (default
//!   `$TMPDIR/issundb-cyclic-enum-<nodes>-<edges>`)

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use issundb::{Graph, GraphQueryExt, NodeId};

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

/// In-driver adjacency snapshot for the analytic counts.
struct Adj {
    out: HashMap<NodeId, Vec<NodeId>>,
    inc: HashMap<NodeId, Vec<NodeId>>,
    out_sets: HashMap<NodeId, HashSet<NodeId>>,
}

impl Adj {
    fn build(graph: &Graph) -> Result<Self, Box<dyn std::error::Error>> {
        let mut out: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut inc: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for node in graph.all_nodes()? {
            out.insert(
                node,
                graph
                    .out_neighbors(node)?
                    .into_iter()
                    .map(|e| e.node)
                    .collect(),
            );
            inc.insert(
                node,
                graph
                    .in_neighbors(node)?
                    .into_iter()
                    .map(|e| e.node)
                    .collect(),
            );
        }
        let out_sets = out
            .iter()
            .map(|(&n, v)| (n, v.iter().copied().collect()))
            .collect();
        Ok(Adj { out, inc, out_sets })
    }

    fn outdeg(&self, n: NodeId) -> u64 {
        self.out.get(&n).map_or(0, |v| v.len() as u64)
    }

    fn indeg(&self, n: NodeId) -> u64 {
        self.inc.get(&n).map_or(0, |v| v.len() as u64)
    }
}

/// Counts for the directed triangle `(a)->(b)->(c)->(a)`.
///
/// Open paths are the wedges `a->b->c` the expand chain materializes before
/// the closing probe. The intersection bound is the work a factored closing
/// hop would pay per edge `(a, b)`: iterate the smaller of `N_out(b)` and
/// `N_in(a)` and probe the other. Output rows count the closed triangles,
/// including rotations, matching Cypher row semantics.
fn triangle_counts(adj: &Adj) -> (u64, u64, u64) {
    let mut wedges = 0u64;
    let mut bound = 0u64;
    let mut rows = 0u64;
    for (&a, outs) in &adj.out {
        for &b in outs {
            wedges += adj.outdeg(b);
            bound += adj.outdeg(b).min(adj.indeg(a));
            // Count closures c with b->c and c->a by probing the smaller side.
            if adj.outdeg(b) <= adj.indeg(a) {
                rows += adj.out[&b]
                    .iter()
                    .filter(|c| adj.out_sets.get(c).is_some_and(|s| s.contains(&a)))
                    .count() as u64;
            } else {
                rows += adj.inc[&a]
                    .iter()
                    .filter(|c| adj.out_sets[&b].contains(c))
                    .count() as u64;
            }
        }
    }
    (wedges, bound, rows)
}

/// Counts for the directed 4-cycle `(a)->(b)->(c)->(d)->(a)`.
///
/// Open paths are the 3-paths `a->b->c->d`; the intersection bound factors
/// the closing hop per wedge `a->b->c`. Both are raw path counts that ignore
/// relationship uniqueness (the only overlap, reciprocal `a->b->a->b` pairs,
/// is a small correction on a simple digraph).
fn four_cycle_counts(adj: &Adj) -> (u64, u64) {
    let mut three_paths = 0u64;
    let mut bound = 0u64;
    for (&a, outs) in &adj.out {
        for &b in outs {
            for &c in &adj.out[&b] {
                three_paths += adj.outdeg(c);
                bound += adj.outdeg(c).min(adj.indeg(a));
            }
        }
    }
    (three_paths, bound)
}

fn time_query(
    graph: &Graph,
    label: &str,
    query: &str,
    reps: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("--- {label}\n{}", graph.explain(query)?.trim_end());
    for rep in 0..reps {
        let start = Instant::now();
        let result = graph.query(query)?;
        let first = result
            .records
            .first()
            .map(|r| format!("{:?}", r.values))
            .unwrap_or_else(|| "no rows".into());
        eprintln!(
            "{label} rep {rep}: {:?} ({} rows, first: {first})",
            start.elapsed(),
            result.records.len(),
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nodes = var("CYCLIC_ENUM_NODES", 2_000);
    let edges = var("CYCLIC_ENUM_EDGES", 8_000);
    let reps = var("CYCLIC_ENUM_REPS", 3);
    let db_dir = std::env::var("CYCLIC_ENUM_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("issundb-cyclic-enum-{nodes}-{edges}"))
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

    let adj = Adj::build(&graph)?;
    let (wedges, tri_bound, tri_rows) = triangle_counts(&adj);
    let (three_paths, quad_bound) = four_cycle_counts(&adj);
    eprintln!("=== analytic counts");
    eprintln!(
        "triangle: open paths (wedges) {wedges}, intersection bound {tri_bound}, output rows {tri_rows}"
    );
    eprintln!("4-cycle:  open paths (3-paths) {three_paths}, intersection bound {quad_bound}");

    eprintln!("=== timings");
    let tri = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) ";
    // The count form lowers to the TriangleCount kernel: the reference point.
    time_query(
        &graph,
        "triangle count",
        &format!("{tri}RETURN count(a) AS n"),
        reps,
    )?;
    // A non-count aggregate keeps the row pipeline with a single-row output,
    // isolating wedge materialization from result construction.
    time_query(
        &graph,
        "triangle min-aggregate",
        &format!("{tri}RETURN min(id(a)) AS m"),
        reps,
    )?;
    // Full enumeration adds per-row output construction on top.
    time_query(
        &graph,
        "triangle enumeration",
        &format!("{tri}RETURN id(a) AS x, id(b) AS y, id(c) AS z"),
        reps,
    )?;

    let quad = "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person)-[:KNOWS]->(a) ";
    time_query(
        &graph,
        "4-cycle count",
        &format!("{quad}RETURN count(a) AS n"),
        reps,
    )?;
    time_query(
        &graph,
        "4-cycle min-aggregate",
        &format!("{quad}RETURN min(id(a)) AS m"),
        reps,
    )?;
    Ok(())
}
