//! Comparison harness running the same Cypher workload against IssunDB and
//! LadybugDB (the Kùzu successor, via the `lbug` crate).
//!
//! Both engines load an identical synthetic social graph, then each query in
//! the workload runs on both. The harness reports median wall time per engine
//! and asserts row-set equality, so it doubles as a differential correctness check.
//! The differential check runs before timing: medians for a query the engines
//! disagree on are meaningless, so a mismatched query is reported and not timed.
//!
//! Probe-anchored queries use deterministic degree-percentile probes (cold,
//! median, and hub) computed from the generated graph rather than fixed ids,
//! so traversal anchors are representative under both degree distributions.
//!
//! Dataset sizes, degree skew, repetition counts, the per-query time budget,
//! and the scale sweep come from environment variables; see `Config::from_env`
//! for the knobs and their defaults.

use std::collections::HashSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use issundb::{Graph, GraphQueryExt};
use lbug::{Connection, Database, SystemConfig};

const CITIES: [&str; 7] = [
    "london",
    "paris",
    "berlin",
    "madrid",
    "rome",
    "amsterdam",
    "oslo",
];

/// Zipf exponent for the skewed degree distribution. At 0.8 over 10k nodes the
/// hottest node receives roughly 3.5 percent of all edge endpoints, which is a
/// proper hub without saturating the distinct-edge constraint.
const ZIPF_THETA: f64 = 0.8;

/// Each sweep step multiplies nodes and edges by this factor.
const SWEEP_STEP: u64 = 5;

#[derive(Clone, Copy, PartialEq)]
enum Skew {
    Uniform,
    Zipf,
}

impl Skew {
    fn as_str(self) -> &'static str {
        match self {
            Skew::Uniform => "uniform",
            Skew::Zipf => "zipf",
        }
    }
}

struct Config {
    /// Person node count.
    nodes: u64,
    /// KNOWS edge count (distinct (src, dst) pairs, no self-loops).
    edges: u64,
    /// Timed repetitions per query; the median is reported.
    reps: usize,
    /// Untimed warmup runs per query.
    warmups: usize,
    /// Degree distribution of the generated edges.
    skew: Skew,
    /// When set, runs the workload at base/5, base, and base*5 sizes and
    /// reports per-query scaling ratios between consecutive sizes.
    sweep: bool,
    /// Time budget per query per engine configuration; repetitions stop early
    /// once it is spent (at least one timed repetition always runs).
    budget: Duration,
}

impl Config {
    fn from_env() -> Self {
        fn var(name: &str, default: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let skew = match std::env::var("LADYBUG_COMPARE_SKEW").as_deref() {
            Ok("zipf") => Skew::Zipf,
            Ok("uniform") | Err(_) => Skew::Uniform,
            Ok(other) => panic!("LADYBUG_COMPARE_SKEW must be 'uniform' or 'zipf', got {other:?}"),
        };
        let nodes = var("LADYBUG_COMPARE_NODES", 10_000);
        let edges = var("LADYBUG_COMPARE_EDGES", 50_000);
        let reps = var("LADYBUG_COMPARE_REPS", 10) as usize;
        let sweep = var("LADYBUG_COMPARE_SWEEP", 0) != 0;
        assert!(nodes > 0, "LADYBUG_COMPARE_NODES must be at least 1");
        assert!(
            edges == 0 || nodes > 1,
            "LADYBUG_COMPARE_EDGES requires at least two nodes \
             (edges are distinct non-self-loop pairs)"
        );
        assert!(reps > 0, "LADYBUG_COMPARE_REPS must be at least 1");
        if sweep {
            let (base_nodes, base_edges) = (nodes / SWEEP_STEP, edges / SWEEP_STEP);
            assert!(
                base_nodes > 0,
                "sweep divides the node count by {SWEEP_STEP}; \
                 LADYBUG_COMPARE_NODES is too small"
            );
            assert!(
                base_edges == 0 || base_nodes > 1,
                "the sweep base size has edges but fewer than two nodes"
            );
        }
        Config {
            nodes,
            edges,
            reps,
            warmups: var("LADYBUG_COMPARE_WARMUPS", 3) as usize,
            skew,
            sweep,
            budget: Duration::from_secs(var("LADYBUG_COMPARE_BUDGET_SECS", 30)),
        }
    }
}

/// Deterministic 64-bit LCG (Knuth MMIX constants) so both engines always see
/// the same graph and runs are reproducible without pulling in `rand`.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }

    /// Uniform sample in [0, 1) from the 48 output bits.
    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 48) as f64
    }
}

/// Cumulative Zipf distribution over node indices `0..n` with exponent
/// `ZIPF_THETA`. Skewed sampling concentrates edge endpoints on low indices,
/// producing hub nodes whose degrees follow a power law, as in real social
/// graphs; uniform sampling gives every node roughly the average degree and
/// hides hub-driven join blowup.
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

    /// Maps a uniform sample in [0, 1) to a node index.
    fn sample(&self, u: f64) -> u64 {
        self.cdf.partition_point(|&c| c < u) as u64
    }
}

struct Dataset {
    /// (id, name, age, city)
    persons: Vec<(u64, String, u64, &'static str)>,
    /// (src id, dst id, weight)
    knows: Vec<(u64, u64, f64)>,
}

fn generate(nodes: u64, edges: u64, skew: Skew) -> Dataset {
    let mut rng = Lcg(0x1554_4ED1);
    let persons = (0..nodes)
        .map(|id| {
            (
                id,
                format!("p{id}"),
                18 + id % 50,
                CITIES[(id % CITIES.len() as u64) as usize],
            )
        })
        .collect();

    let zipf = match skew {
        Skew::Zipf => Some(Zipf::new(nodes)),
        Skew::Uniform => None,
    };
    let mut seen = HashSet::new();
    let mut knows = Vec::with_capacity(edges as usize);
    // Skewed sampling rejects more duplicates around the hubs; the cap turns a
    // pathological nodes-to-edges ratio into a clear failure instead of a hang.
    let max_attempts = edges.saturating_mul(100);
    let mut attempts = 0u64;
    while (knows.len() as u64) < edges {
        attempts += 1;
        assert!(
            attempts <= max_attempts,
            "edge sampling saturated; lower LADYBUG_COMPARE_EDGES relative to LADYBUG_COMPARE_NODES"
        );
        let (src, dst) = match &zipf {
            Some(z) => (z.sample(rng.unit()), z.sample(rng.unit())),
            None => (rng.next() % nodes, rng.next() % nodes),
        };
        if src == dst || !seen.insert((src, dst)) {
            continue;
        }
        let weight = (rng.next() % 1000) as f64 / 1000.0;
        knows.push((src, dst, weight));
    }
    Dataset { persons, knows }
}

/// Probe nodes chosen from the generated out-degree distribution, so traversal
/// anchors are deterministic and representative under both skews instead of
/// landing on an accidental degree (under Zipf skew, a fixed mid-range id is
/// nearly isolated).
struct Probes {
    /// Lowest out-degree node (ties broken by id): a floor measurement of
    /// per-query fixed overhead.
    cold: u64,
    /// Median out-degree node: representative traversal work.
    median: u64,
    /// Highest out-degree node: hub fan-out (the proper hub under Zipf skew,
    /// the busiest ordinary node under uniform skew).
    hub: u64,
    /// A node reachable from `median` in exactly two hops when one exists, so
    /// `expand_into` joins toward a target with actual matching paths; the
    /// wrapped successor id otherwise, where the count is simply zero.
    expand_target: u64,
}

fn pick_probes(data: &Dataset) -> Probes {
    let nodes = data.persons.len() as u64;
    let mut out_degree = vec![0u64; nodes as usize];
    let mut adjacency: Vec<Vec<u64>> = vec![Vec::new(); nodes as usize];
    for &(src, dst, _) in &data.knows {
        out_degree[src as usize] += 1;
        adjacency[src as usize].push(dst);
    }
    let mut by_degree: Vec<u64> = (0..nodes).collect();
    by_degree.sort_by_key(|&id| (out_degree[id as usize], id));
    let cold = by_degree[0];
    let median = by_degree[by_degree.len() / 2];
    let hub = *by_degree.last().unwrap();

    // First two-hop successor of `median` other than itself, in generation
    // order; generation is seeded, so the choice is deterministic.
    let expand_target = adjacency[median as usize]
        .iter()
        .flat_map(|&b| adjacency[b as usize].iter().copied())
        .find(|&c| c != median)
        .unwrap_or((median + 1) % nodes);
    Probes {
        cold,
        median,
        hub,
        expand_target,
    }
}

/// Writes the dataset as CSV files for LadybugDB's `COPY FROM` bulk loader.
fn write_csvs(data: &Dataset, dir: &std::path::Path) -> anyhow::Result<()> {
    let mut persons = std::io::BufWriter::new(std::fs::File::create(dir.join("persons.csv"))?);
    writeln!(persons, "id,name,age,city")?;
    for (id, name, age, city) in &data.persons {
        writeln!(persons, "{id},{name},{age},{city}")?;
    }
    persons.flush()?;

    let mut knows = std::io::BufWriter::new(std::fs::File::create(dir.join("knows.csv"))?);
    writeln!(knows, "from,to,weight")?;
    for (src, dst, weight) in &data.knows {
        writeln!(knows, "{src},{dst},{weight}")?;
    }
    knows.flush()?;
    Ok(())
}

fn load_ladybug(conn: &Connection, csv_dir: &std::path::Path) -> anyhow::Result<()> {
    conn.query(
        "CREATE NODE TABLE Person(id INT64, name STRING, age INT64, city STRING, \
         PRIMARY KEY(id));",
    )?;
    conn.query("CREATE REL TABLE KNOWS(FROM Person TO Person, weight DOUBLE);")?;
    let persons = csv_dir.join("persons.csv");
    let knows = csv_dir.join("knows.csv");
    conn.query(&format!(
        "COPY Person FROM '{}' (HEADER=true);",
        persons.display()
    ))?;
    conn.query(&format!(
        "COPY KNOWS FROM '{}' (HEADER=true);",
        knows.display()
    ))?;
    Ok(())
}

fn load_issundb(graph: &Graph, data: &Dataset) -> anyhow::Result<()> {
    // Single write transaction: one commit for the whole dataset, matching
    // LadybugDB's COPY FROM ingestion model instead of per-record commits.
    graph.update(|txn| {
        // Node ids are dense (0..n), so insertion order doubles as the id map.
        let mut node_ids = Vec::with_capacity(data.persons.len());
        for (id, name, age, city) in &data.persons {
            let nid = txn.add_node(
                "Person",
                &serde_json::json!({ "id": id, "name": name, "age": age, "city": city }),
            )?;
            node_ids.push(nid);
        }
        for (src, dst, weight) in &data.knows {
            txn.add_edge(
                node_ids[*src as usize],
                node_ids[*dst as usize],
                "KNOWS",
                &serde_json::json!({ "weight": weight }),
            )?;
        }
        Ok(())
    })?;
    // Index-backed point lookups, matching LadybugDB's PRIMARY KEY hash index.
    graph.query("CREATE INDEX FOR (p:Person) ON (p.id)")?;
    graph.rebuild_csr()?;
    Ok(())
}

/// Normalizes a result row to plain strings so both engines compare equal on
/// identical logical values. Strings drop their JSON quoting; everything else
/// keeps its display form. The workload avoids floats in projections, so no
/// float formatting reconciliation is needed.
fn issundb_rows(result: &issundb::QueryResult) -> Vec<Vec<String>> {
    result
        .records
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .collect()
}

fn ladybug_rows(result: lbug::QueryResult) -> Vec<Vec<String>> {
    result
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

/// Runs `f` for up to `warmups` untimed and `reps` timed repetitions, stopping
/// each phase early once `budget` is spent in it, and returns the median timed
/// duration plus the number of timed samples actually taken (always at least
/// one).
fn bench(warmups: usize, reps: usize, budget: Duration, mut f: impl FnMut()) -> (Duration, usize) {
    let warmup_start = Instant::now();
    for _ in 0..warmups {
        f();
        if warmup_start.elapsed() > budget {
            break;
        }
    }
    let mut times = Vec::with_capacity(reps);
    let timed_start = Instant::now();
    for _ in 0..reps {
        let start = Instant::now();
        f();
        times.push(start.elapsed());
        if timed_start.elapsed() > budget {
            break;
        }
    }
    let samples = times.len();
    (median(times), samples)
}

/// How a query's work grows with dataset size, used to split the sweep's
/// scaling table: a 5x-per-step threshold only means "superlinear" for queries
/// whose work tracks the dataset.
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    /// Scan, full traversal, or global aggregate: work tracks dataset size.
    Global,
    /// Anchored at a probe node: work tracks the probe's degree, not dataset
    /// size, so near-flat ratios are expected under uniform skew.
    ProbeLocal,
}

/// One benchmark query; the Cypher is sent verbatim to both engines.
struct Query {
    name: &'static str,
    cypher: String,
    scope: Scope,
}

/// Median timings for one query at one dataset size.
struct QueryTiming {
    name: &'static str,
    scope: Scope,
    issundb: Duration,
    ladybug_1t: Duration,
}

/// The benchmark queries, anchored at the degree-percentile probes.
fn workload(probes: &Probes) -> Vec<Query> {
    let cold = probes.cold;
    let median = probes.median;
    let hub = probes.hub;
    let target = probes.expand_target;
    let q = |name, scope, cypher| Query {
        name,
        cypher,
        scope,
    };
    vec![
        q(
            "node_count",
            Scope::Global,
            "MATCH (p:Person) RETURN count(p) AS n".to_string(),
        ),
        q(
            "edge_count",
            Scope::Global,
            "MATCH ()-[r:KNOWS]->() RETURN count(r) AS n".to_string(),
        ),
        q(
            "point_lookup",
            Scope::ProbeLocal,
            format!("MATCH (p:Person) WHERE p.id = {median} RETURN p.name AS name"),
        ),
        // Range predicate on age. Access paths differ by design: IssunDB
        // auto-indexes scalar properties, while LadybugDB only carries its
        // primary-key index, so this compares an index range scan against a
        // table scan rather than identical plans.
        q(
            "range_filter",
            Scope::Global,
            "MATCH (p:Person) WHERE p.age >= 30 AND p.age < 40 RETURN count(p) AS n".to_string(),
        ),
        // Lowest-degree probe: a floor measurement of per-query fixed
        // overhead (parse, plan, and dispatch) with almost no traversal work.
        q(
            "one_hop_cold",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                 WHERE a.id = {cold} RETURN count(b) AS n"
            ),
        ),
        q(
            "one_hop_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                 WHERE a.id = {median} RETURN count(b) AS n"
            ),
        ),
        q(
            "two_hop_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} RETURN count(c) AS n"
            ),
        ),
        q(
            "three_hop_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 -[:KNOWS]->(d:Person) WHERE a.id = {median} RETURN count(d) AS n"
            ),
        ),
        // Unlike the path-counting hops above, this counts distinct endpoints
        // (count(DISTINCT e)) to bound the four-hop combinatorial blowup; the
        // name records the different semantics.
        q(
            "four_hop_distinct",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 -[:KNOWS]->(d:Person)-[:KNOWS]->(e:Person) \
                 WHERE a.id = {median} RETURN count(DISTINCT e) AS n"
            ),
        ),
        q(
            "one_or_two_hop",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS*1..2]->(b:Person) \
                 WHERE a.id = {median} RETURN count(DISTINCT b) AS n"
            ),
        ),
        // Two-hop fan-out from the highest out-degree node: a proper hub
        // under Zipf skew, the busiest ordinary node under uniform skew.
        q(
            "two_hop_hub",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {hub} RETURN count(c) AS n"
            ),
        ),
        q(
            "filter_after_expand",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                 WHERE a.id = {median} AND b.age >= 30 AND b.age < 40 RETURN count(b) AS n"
            ),
        ),
        // Both endpoints are fixed, so this exercises an expand-into-shaped
        // two-hop join rather than fan-out from only the source. The target
        // is a known two-hop successor of the probe, so the join has matching
        // paths instead of an empty build side an engine could short-circuit.
        q(
            "expand_into",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} AND c.id = {target} RETURN count(b) AS n"
            ),
        ),
        q(
            "var_length_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS*2..3]->(c:Person) \
                 WHERE a.id = {median} RETURN count(c) AS n"
            ),
        ),
        q(
            "order_limit",
            Scope::Global,
            "MATCH (p:Person) RETURN p.name AS name, p.age AS age \
             ORDER BY age DESC, name ASC LIMIT 10"
                .to_string(),
        ),
        // Hub fan-out reaches many duplicate cities (seven exist), so the
        // DISTINCT collapses real duplicates and the LIMIT binds; a
        // median-degree probe sees about as many cities as the limit and
        // leaves both clauses idle.
        q(
            "distinct_limit",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id = {hub} \
                 RETURN DISTINCT b.city AS city ORDER BY city LIMIT 5"
            ),
        ),
        // Full-scan projection of three properties per row, so per-row property
        // decode cost shows up instead of being hidden behind count(...).
        q(
            "prop_projection",
            Scope::Global,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.name AS name, b.age AS age, b.city AS city"
                .to_string(),
        ),
        q(
            "triangle_count",
            Scope::Global,
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             RETURN count(a) AS n"
                .to_string(),
        ),
        q(
            "agg_over_traversal",
            Scope::Global,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, count(a) AS n ORDER BY city"
                .to_string(),
        ),
    ]
}

/// Loads both engines at the given size, runs the workload, prints the result
/// table, and returns the per-query timings for the sweep's scaling summary.
fn run_at(cfg: &Config, nodes: u64, edges: u64) -> anyhow::Result<Vec<QueryTiming>> {
    println!(
        "dataset: {nodes} Person nodes, {edges} KNOWS edges ({} skew); \
         {} reps ({} warmups) per query\n",
        cfg.skew.as_str(),
        cfg.reps,
        cfg.warmups
    );

    let data = generate(nodes, edges, cfg.skew);
    let probes = pick_probes(&data);
    let csv_dir = tempfile::tempdir()?;
    write_csvs(&data, csv_dir.path())?;

    // ---- Load both engines, timing each once ------------------------------
    let lb_dir = tempfile::tempdir()?;
    let db = Database::new(lb_dir.path().join("db"), SystemConfig::default())?;
    let mut conn = Connection::new(&db)?;
    // LadybugDB defaults to WALK semantics for variable-length patterns, where
    // a relationship may repeat within a path; openCypher (and IssunDB) use
    // TRAIL semantics. Pin TRAIL so both engines match the same paths.
    conn.query("CALL recursive_pattern_semantic = 'TRAIL';")?;
    let default_threads = conn.get_max_num_threads_for_exec();
    let start = Instant::now();
    load_ladybug(&conn, csv_dir.path())?;
    let lb_load = start.elapsed();

    let is_dir = tempfile::tempdir()?;
    let graph = Graph::open(is_dir.path(), 2)?;
    let start = Instant::now();
    load_issundb(&graph, &data)?;
    let is_load = start.elapsed();

    println!("load: issundb {is_load:?} (single write txn), ladybug {lb_load:?} (COPY FROM)\n");

    println!(
        "{:<20} {:>12} {:>12} {:>14} {:>10}  diff",
        "query", "issundb", "ladybug", "ladybug(1t)", "result"
    );
    // A trailing `*` marks a median taken from fewer than the requested reps
    // because the per-query budget ran out.
    let fmt = |d: Duration, samples: usize| {
        let s = format!("{d:.2?}");
        if samples < cfg.reps {
            format!("{s}*")
        } else {
            s
        }
    };
    let mut timings = Vec::new();
    let mut truncated = false;
    let mut mismatches = 0;
    for query in &workload(&probes) {
        let (name, cypher) = (query.name, &query.cypher);

        // Differential check before timing: medians for a query the engines
        // disagree on are meaningless (an engine doing the wrong amount of
        // work can look faster), so a mismatch is reported and the query is
        // not timed. Sorted row sets must match exactly.
        let mut is_rows = issundb_rows(&graph.query(cypher)?);
        let mut lb_rows = ladybug_rows(conn.query(cypher)?);
        is_rows.sort();
        lb_rows.sort();
        if is_rows != lb_rows {
            mismatches += 1;
            println!(
                "{name:<20} {:>12} {:>12} {:>14} {:>10}  MISMATCH \
                 (issundb {} rows: {:?}..., ladybug {} rows: {:?}...)",
                "-",
                "-",
                "-",
                "-",
                is_rows.len(),
                is_rows.first(),
                lb_rows.len(),
                lb_rows.first()
            );
            continue;
        }

        // A single scalar result is printed verbatim, so aggregate values
        // (the actual count behind a count(...) query) are visible in the
        // table; multi-row results print their cardinality.
        let result = if is_rows.len() == 1 && is_rows[0].len() == 1 {
            is_rows[0][0].clone()
        } else {
            format!("{} rows", is_rows.len())
        };

        let (is_time, is_n) = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            graph.query(cypher).unwrap();
        });

        conn.set_max_num_threads_for_exec(default_threads);
        let (lb_time, lb_n) = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            for _row in conn.query(cypher).unwrap() {}
        });
        conn.set_max_num_threads_for_exec(1);
        let (lb_time_1t, lb_1t_n) = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            for _row in conn.query(cypher).unwrap() {}
        });
        truncated |= is_n < cfg.reps || lb_n < cfg.reps || lb_1t_n < cfg.reps;

        println!(
            "{name:<20} {:>12} {:>12} {:>14} {:>10}  OK",
            fmt(is_time, is_n),
            fmt(lb_time, lb_n),
            fmt(lb_time_1t, lb_1t_n),
            result
        );
        timings.push(QueryTiming {
            name,
            scope: query.scope,
            issundb: is_time,
            ladybug_1t: lb_time_1t,
        });
    }
    if truncated {
        println!(
            "* median from fewer than {} reps; {}s per-query budget reached",
            cfg.reps,
            cfg.budget.as_secs()
        );
    }

    if mismatches > 0 {
        anyhow::bail!("{mismatches} differential mismatch(es)");
    }
    Ok(timings)
}

fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env();
    let sizes: Vec<(u64, u64)> = if cfg.sweep {
        vec![
            (cfg.nodes / SWEEP_STEP, cfg.edges / SWEEP_STEP),
            (cfg.nodes, cfg.edges),
            (cfg.nodes * SWEEP_STEP, cfg.edges * SWEEP_STEP),
        ]
    } else {
        vec![(cfg.nodes, cfg.edges)]
    };

    let mut reports = Vec::new();
    for (i, &(nodes, edges)) in sizes.iter().enumerate() {
        if i > 0 {
            println!();
        }
        reports.push(run_at(&cfg, nodes, edges)?);
    }

    if reports.len() > 1 {
        // Each size regenerates the graph and re-derives the probes, so the
        // probes keep their degree percentile rather than their id; the
        // superlinear threshold only applies to queries whose work tracks
        // dataset size, so the table is split by scope.
        println!("\nscaling per step (dataset grows {SWEEP_STEP}x per step):");
        let sections = [
            (
                Scope::Global,
                format!(
                    "global queries (work tracks dataset size; \
                     ratios above {SWEEP_STEP}.0x are superlinear)"
                ),
            ),
            (
                Scope::ProbeLocal,
                "probe-anchored queries (work tracks probe degree: \
                 near-flat ratios expected under uniform skew, hub growth under zipf)"
                    .to_string(),
            ),
        ];
        for (scope, note) in sections {
            println!("\n{note}:");
            println!("{:<20} {:>16} {:>16}", "query", "issundb", "ladybug(1t)");
            for qi in 0..reports[0].len() {
                if reports[0][qi].scope != scope {
                    continue;
                }
                let ratios = |get: fn(&QueryTiming) -> Duration| -> String {
                    (1..reports.len())
                        .map(|i| {
                            let prev = get(&reports[i - 1][qi]).as_secs_f64();
                            let next = get(&reports[i][qi]).as_secs_f64();
                            format!("{:>6.1}x", next / prev.max(f64::EPSILON))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                println!(
                    "{:<20} {:>16} {:>16}",
                    reports[0][qi].name,
                    ratios(|t| t.issundb),
                    ratios(|t| t.ladybug_1t)
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcg_is_deterministic() {
        let mut a = Lcg(0x1554_4ED1);
        let mut b = Lcg(0x1554_4ED1);
        for _ in 0..5 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn zipf_cdf_is_monotonic_and_samples_in_range() {
        let z = Zipf::new(1_000);
        assert!(z.cdf.windows(2).all(|w| w[0] < w[1]));
        assert!((z.cdf.last().unwrap() - 1.0).abs() < 1e-9);
        let mut rng = Lcg(42);
        for _ in 0..10_000 {
            assert!(z.sample(rng.unit()) < 1_000);
        }
    }

    #[test]
    fn generate_produces_distinct_non_self_loop_edges() {
        for skew in [Skew::Uniform, Skew::Zipf] {
            let data = generate(1_000, 5_000, skew);
            assert_eq!(data.persons.len(), 1_000);
            assert_eq!(data.knows.len(), 5_000);
            let mut seen = HashSet::new();
            for &(src, dst, _) in &data.knows {
                assert_ne!(src, dst);
                assert!(seen.insert((src, dst)));
                assert!(src < 1_000 && dst < 1_000);
            }
        }
    }

    #[test]
    fn zipf_skew_produces_degree_hubs() {
        let data = generate(1_000, 5_000, Skew::Zipf);
        let mut in_degree = vec![0u64; 1_000];
        for &(_, dst, _) in &data.knows {
            in_degree[dst as usize] += 1;
        }
        // The average in-degree is 5; a Zipf hub must sit far above it.
        let max = *in_degree.iter().max().unwrap();
        assert!(
            max >= 50,
            "max in-degree {max} is too small for a skewed graph"
        );
    }

    #[test]
    fn workload_covers_core_read_scenarios() {
        let data = generate(1_000, 5_000, Skew::Uniform);
        let probes = pick_probes(&data);
        let names: HashSet<_> = workload(&probes).into_iter().map(|q| q.name).collect();
        for expected in [
            "node_count",
            "edge_count",
            "point_lookup",
            "range_filter",
            "one_hop_cold",
            "one_hop_count",
            "two_hop_count",
            "three_hop_count",
            "four_hop_distinct",
            "one_or_two_hop",
            "two_hop_hub",
            "filter_after_expand",
            "expand_into",
            "var_length_count",
            "order_limit",
            "distinct_limit",
            "prop_projection",
            "triangle_count",
            "agg_over_traversal",
        ] {
            assert!(
                names.contains(expected),
                "missing workload scenario {expected}"
            );
        }
    }

    #[test]
    fn probes_follow_the_degree_percentiles() {
        for skew in [Skew::Uniform, Skew::Zipf] {
            let data = generate(1_000, 5_000, skew);
            let probes = pick_probes(&data);
            let mut out_degree = vec![0u64; 1_000];
            for &(src, _, _) in &data.knows {
                out_degree[src as usize] += 1;
            }
            let cold = out_degree[probes.cold as usize];
            let median = out_degree[probes.median as usize];
            let hub = out_degree[probes.hub as usize];
            assert_eq!(cold, *out_degree.iter().min().unwrap());
            assert_eq!(hub, *out_degree.iter().max().unwrap());
            assert!(
                cold <= median && median <= hub,
                "degree ordering violated: cold {cold}, median {median}, hub {hub}"
            );
        }
    }

    #[test]
    fn expand_target_is_a_two_hop_successor_when_one_exists() {
        for skew in [Skew::Uniform, Skew::Zipf] {
            let data = generate(1_000, 5_000, skew);
            let probes = pick_probes(&data);
            let mut adjacency: Vec<Vec<u64>> = vec![Vec::new(); 1_000];
            for &(src, dst, _) in &data.knows {
                adjacency[src as usize].push(dst);
            }
            let two_hop: HashSet<u64> = adjacency[probes.median as usize]
                .iter()
                .flat_map(|&b| adjacency[b as usize].iter().copied())
                .filter(|&c| c != probes.median)
                .collect();
            if !two_hop.is_empty() {
                assert!(
                    two_hop.contains(&probes.expand_target),
                    "expand_target {} is not a two-hop successor of probe {}",
                    probes.expand_target,
                    probes.median
                );
            }
        }
    }

    #[test]
    fn probes_are_deterministic() {
        let a = pick_probes(&generate(1_000, 5_000, Skew::Zipf));
        let b = pick_probes(&generate(1_000, 5_000, Skew::Zipf));
        assert_eq!(
            (a.cold, a.median, a.hub, a.expand_target),
            (b.cold, b.median, b.hub, b.expand_target)
        );
    }
}
