//! Comparison harness running the same Cypher workload against IssunDB and
//! LadybugDB (via the `lbug` crate).
//!
//! Both databases load an identical synthetic social graph, then each query in
//! the workload runs on both. The harness reports median wall time per engine
//! and asserts row-set equality, so it doubles as a differential correctness check.
//! The differential check runs before timing: medians for a query the databases
//! disagree on are meaningless, so a divergent query is reported and not timed.
//! Trail-sensitive queries carry an openCypher trail reference computed from
//! the dataset (see `Oracle`), so a known LadybugDB walk-semantics overcount
//! is attributed and reported without failing the run.
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
    /// Generated differential queries per dataset size. Zero keeps a timing run
    /// the length it was.
    generated: usize,
    /// Seed for the generator, printed with the run so a failure replays.
    seed: u64,
    /// Run the differential passes and skip timing entirely.
    diff_only: bool,
}

impl Config {
    fn from_env() -> Self {
        fn var(name: &str, default: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let skew = match std::env::var("LADYBUGDB_COMPARE_SKEW").as_deref() {
            Ok("zipf") => Skew::Zipf,
            Ok("uniform") | Err(_) => Skew::Uniform,
            Ok(other) => {
                panic!("LADYBUGDB_COMPARE_SKEW must be 'uniform' or 'zipf', got {other:?}")
            }
        };
        // The default size is large enough that the scan-and-materialize queries
        // are doing real work rather than measuring per-query fixed overhead. At
        // 10k nodes the dataset sits in cache and a query like `node_count`
        // reports the floor cost of issuing a query, not the cost of scanning.
        let nodes = var("LADYBUGDB_COMPARE_NODES", 50_000);
        // Five edges per node by default, so overriding only the node count keeps
        // the density fixed. A constant edge default silently thins or densifies
        // the graph instead, and density moves the comparison more than size: at
        // 2000 nodes with the old constant 50_000 edges (degree 25 rather than 5)
        // `four_hop_distinct` swung from 0.24 to 29.4 against LadybugDB.
        let edges = var("LADYBUGDB_COMPARE_EDGES", nodes.saturating_mul(5));
        let reps = var("LADYBUGDB_COMPARE_REPS", 10) as usize;
        let sweep = var("LADYBUGDB_COMPARE_SWEEP", 0) != 0;
        assert!(nodes > 0, "LADYBUGDB_COMPARE_NODES must be at least 1");
        assert!(
            edges == 0 || nodes > 1,
            "LADYBUGDB_COMPARE_EDGES requires at least two nodes \
             (edges are distinct non-self-loop pairs)"
        );
        assert!(reps > 0, "LADYBUGDB_COMPARE_REPS must be at least 1");
        if sweep {
            let (base_nodes, base_edges) = (nodes / SWEEP_STEP, edges / SWEEP_STEP);
            assert!(
                base_nodes > 0,
                "sweep divides the node count by {SWEEP_STEP}; \
                 LADYBUGDB_COMPARE_NODES is too small"
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
            warmups: var("LADYBUGDB_COMPARE_WARMUPS", 3) as usize,
            skew,
            sweep,
            budget: Duration::from_secs(var("LADYBUGDB_COMPARE_BUDGET_SECS", 30)),
            generated: var("LADYBUGDB_COMPARE_GENERATED", 0) as usize,
            seed: var("LADYBUGDB_COMPARE_SEED", 0x5EED_1234),
            diff_only: var("LADYBUGDB_COMPARE_DIFF_ONLY", 0) != 0,
        }
    }
}

/// Deterministic 64-bit LCG (Knuth MMIX constants) so both databases always see
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
            "edge sampling saturated; lower LADYBUGDB_COMPARE_EDGES relative to LADYBUGDB_COMPARE_NODES"
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

/// Out-adjacency lists in generation order, shared by probe selection and the
/// trail oracle.
fn out_adjacency(data: &Dataset) -> Vec<Vec<u64>> {
    let mut adjacency: Vec<Vec<u64>> = vec![Vec::new(); data.persons.len()];
    for &(src, dst, _) in &data.knows {
        adjacency[src as usize].push(dst);
    }
    adjacency
}

/// In-adjacency lists in generation order, the incoming counterpart of
/// [`out_adjacency`]. The generated corpus traverses both directions, so bounding
/// its work needs both.
fn in_adjacency(data: &Dataset) -> Vec<Vec<u64>> {
    let mut adjacency: Vec<Vec<u64>> = vec![Vec::new(); data.persons.len()];
    for &(src, dst, _) in &data.knows {
        adjacency[dst as usize].push(src);
    }
    adjacency
}

fn pick_probes(data: &Dataset) -> Probes {
    let nodes = data.persons.len() as u64;
    let adjacency = out_adjacency(data);
    let out_degree: Vec<u64> = adjacency.iter().map(|n| n.len() as u64).collect();
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

fn load_ladybugdb(conn: &Connection, csv_dir: &std::path::Path) -> anyhow::Result<()> {
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

/// Normalizes a result row to plain strings so both databases compare equal on
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

fn ladybugdb_rows(result: lbug::QueryResult) -> Vec<Vec<String>> {
    result
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect()
}

/// Bootstrap resamples used to estimate the confidence interval of the median.
const BOOTSTRAP_RESAMPLES: usize = 2000;

/// A timed measurement: the median wall time, a 95% confidence interval for
/// that median, and the number of timed samples taken.
#[derive(Clone, Copy)]
struct BenchStat {
    median: Duration,
    ci_lo: Duration,
    ci_hi: Duration,
    samples: usize,
}

/// Median of an already-sorted, non-empty slice.
fn median_sorted(sorted: &[Duration]) -> Duration {
    sorted[sorted.len() / 2]
}

/// 95% confidence interval for the median by percentile bootstrap: draw
/// `BOOTSTRAP_RESAMPLES` resamples of size `n` with replacement from the timed
/// rounds, take each resample's median, and return the 2.5th and 97.5th
/// percentiles of those medians. Resampling uses a fixed-seed xorshift
/// generator seeded from the sample values, so the interval is reproducible
/// for a given set of timings and differs across queries.
fn bootstrap_ci95(sorted: &[Duration]) -> (Duration, Duration) {
    let n = sorted.len();
    if n <= 1 {
        let only = sorted.first().copied().unwrap_or_default();
        return (only, only);
    }
    let nanos: Vec<u128> = sorted.iter().map(Duration::as_nanos).collect();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15 ^ (n as u64);
    for &v in &nanos {
        seed = seed.wrapping_mul(0x100_0000_01B3).wrapping_add(v as u64);
    }
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    let mut sample = vec![0u128; n];
    for _ in 0..BOOTSTRAP_RESAMPLES {
        for s in sample.iter_mut() {
            *s = nanos[(next() as usize) % n];
        }
        sample.sort_unstable();
        medians.push(sample[n / 2]);
    }
    medians.sort_unstable();
    let lo = medians[(BOOTSTRAP_RESAMPLES as f64 * 0.025) as usize];
    let hi = medians[((BOOTSTRAP_RESAMPLES as f64 * 0.975) as usize).min(BOOTSTRAP_RESAMPLES - 1)];
    (
        Duration::from_nanos(lo as u64),
        Duration::from_nanos(hi as u64),
    )
}

/// Runs `f` for up to `warmups` untimed and `reps` timed repetitions, stopping
/// each phase early once `budget` is spent in it, and returns the median timed
/// duration with its 95% bootstrap confidence interval plus the number of
/// timed samples actually taken (always at least one).
fn bench(warmups: usize, reps: usize, budget: Duration, mut f: impl FnMut()) -> BenchStat {
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
    times.sort();
    let (ci_lo, ci_hi) = bootstrap_ci95(&times);
    BenchStat {
        median: median_sorted(&times),
        ci_lo,
        ci_hi,
        samples: times.len(),
    }
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

/// How a row-set divergence between the databases is adjudicated.
///
/// openCypher requires pairwise-distinct relationships within a MATCH pattern
/// (trail semantics). LadybugDB matches walks instead: fixed-length chains
/// never enforce cross-hop relationship uniqueness, and the
/// `recursive_pattern_semantic` session setting is accepted but inert in the
/// pinned `lbug` build (see `tests/lbug_trail_semantics.rs`). For the queries
/// where walks and trails can differ, a trail count computed directly from
/// the dataset attributes the divergence instead of failing the run blindly.
/// Two-hop patterns are exempt because a two-edge walk cannot repeat an edge
/// without a self-loop, and the generator emits none.
enum Oracle {
    /// Row sets must match exactly; any divergence fails the run.
    Exact,
    /// Trail count from the median probe with hop counts in `min..=max`.
    TrailCount(u8, u8),
    /// Distinct trail endpoints at exactly `hops` hops from the median probe.
    TrailEndpoints(u8),
}

/// Count trails (edge-distinct paths) from `start` with `min..=max` hops.
fn count_trails(adjacency: &[Vec<u64>], start: u64, min: u8, max: u8) -> u64 {
    fn rec(
        adjacency: &[Vec<u64>],
        node: u64,
        used: &mut Vec<(u64, u64)>,
        depth: u8,
        min: u8,
        max: u8,
        total: &mut u64,
    ) {
        if depth >= min {
            *total += 1;
        }
        if depth == max {
            return;
        }
        for &next in &adjacency[node as usize] {
            let edge = (node, next);
            if used.contains(&edge) {
                continue;
            }
            used.push(edge);
            rec(adjacency, next, used, depth + 1, min, max, total);
            used.pop();
        }
    }
    let mut total = 0;
    rec(adjacency, start, &mut Vec::new(), 0, min, max, &mut total);
    total
}

/// Count distinct endpoints of trails at exactly `hops` hops from `start`.
fn count_trail_endpoints(adjacency: &[Vec<u64>], start: u64, hops: u8) -> u64 {
    fn rec(
        adjacency: &[Vec<u64>],
        node: u64,
        used: &mut Vec<(u64, u64)>,
        depth: u8,
        hops: u8,
        endpoints: &mut HashSet<u64>,
    ) {
        if depth == hops {
            endpoints.insert(node);
            return;
        }
        for &next in &adjacency[node as usize] {
            let edge = (node, next);
            if used.contains(&edge) {
                continue;
            }
            used.push(edge);
            rec(adjacency, next, used, depth + 1, hops, endpoints);
            used.pop();
        }
    }
    let mut endpoints = HashSet::new();
    rec(adjacency, start, &mut Vec::new(), 0, hops, &mut endpoints);
    endpoints.len() as u64
}

/// One benchmark query; the Cypher is sent verbatim to both databases.
struct Query {
    name: &'static str,
    cypher: String,
    scope: Scope,
    oracle: Oracle,
}

/// Median timings for one query at one dataset size. `None` marks a query
/// that was not timed at this size (a reported semantic divergence).
struct QueryTiming {
    name: &'static str,
    scope: Scope,
    issundb: Option<Duration>,
    ladybugdb_1t: Option<Duration>,
}

/// The benchmark queries, anchored at the degree-percentile probes.
/// A differential-only query: run on both databases, compare sorted row sets.
///
/// Kept separate from the timing workload on purpose. That workload is shaped for
/// measurement, so nearly every query in it returns a single `count(...)`, and a
/// scalar count is close to the weakest differential signal available: it cannot
/// see wrong row content, wrong column names, wrong row multiplicity, or two
/// errors that cancel. These return the rows themselves, so the comparison has
/// something to disagree about.
///
/// Two invariants keep this corpus cheap to extend, and `differential_invariants`
/// pins both:
///
/// 1. No pattern here can bind one edge to two relationship slots, which means at
///    most two slots, all in the same direction, and no closing hop. This is
///    narrower than "fixed-length", and the difference matters: walk-against-trail
///    is not a variable-length question. Relationship uniqueness applies to every
///    pattern with two or more slots, so `(a)-[:R]->(b)<-[:R]-(c)` diverges when
///    `c` is `a` and one edge fills both slots, and `(a)<-[:R]-(b)-[:R]->(a)`
///    always does. The pinned LadybugDB build permits that reuse where openCypher
///    forbids it. Two same-direction slots are safe only because the generator
///    emits no self-loops (`generate_produces_distinct_non_self_loop_edges`), so
///    one edge cannot chain to itself. Shapes outside this rule belong in the
///    generated corpus, where `reference_rows` adjudicates them.
/// 2. Row sets stay small and skew-independent, either anchored at a
///    non-hub probe or bounded by an `id` predicate, so the comparison costs the
///    same at every dataset size in a sweep.
///
/// Projections avoid floats and nulls. That is a display-form difference rather
/// than a semantic one, and a corpus that reports formatting as a mismatch gets
/// ignored. The float case is measured, not assumed: a whole-valued weight comes
/// back as `0.0` from IssunDB (serde_json's float form) and as `0` from LadybugDB,
/// while fractional weights agree, so returning a float column reports a mismatch
/// on roughly one row in a thousand of this dataset. Reconciling numeric display
/// across the two databases in `issundb_rows`/`ladybugdb_rows` would let floats join
/// the corpus.
struct DiffQuery {
    name: &'static str,
    cypher: String,
    /// Whether the projection is a grouped aggregate (a group key plus an
    /// aggregate) rather than plain property reads.
    ///
    /// Declared rather than inferred from the query text. The invariant test needs
    /// to reject an *ungrouped* aggregate, which compares a single scalar and so
    /// defeats the corpus's purpose, and sniffing that from strings meant
    /// enumerating aggregate names: a new `max`- or `sum`-grouped query would have
    /// tripped the invariant rather than any real violation.
    ///
    /// Only the invariant test reads it, so a non-test build sees it as dead. It is
    /// declared data about the corpus, not a runtime input, which is the point.
    #[cfg_attr(not(test), allow(dead_code))]
    grouped: bool,
}

fn differential_workload(probes: &Probes) -> Vec<DiffQuery> {
    let median = probes.median;
    let cold = probes.cold;
    let target = probes.expand_target;
    // `q` for a property projection, `qg` for a grouped aggregate.
    let q = |name, cypher| DiffQuery {
        name,
        cypher,
        grouped: false,
    };
    let qg = |name, cypher| DiffQuery {
        name,
        cypher,
        grouped: true,
    };
    vec![
        // ---- Projection fidelity over a bounded slice of the label scan -----
        q(
            "scan_projection",
            "MATCH (p:Person) WHERE p.id < 200 \
             RETURN p.id, p.name, p.age, p.city"
                .to_string(),
        ),
        // Deliberately wider than the engine's small-gather cutoff, so the
        // property read is served by the columnar path rather than as point reads.
        // `scan_projection` above stays under it, so between them both regimes of
        // the same read are compared.
        q(
            "scan_projection_wide",
            "MATCH (p:Person) WHERE p.id < 2000 RETURN p.id, p.age".to_string(),
        ),
        q(
            "range_rows",
            "MATCH (p:Person) WHERE p.id < 500 AND p.age >= 30 AND p.age < 32 \
             RETURN p.id, p.age"
                .to_string(),
        ),
        q(
            "string_equality_rows",
            "MATCH (p:Person) WHERE p.id < 300 AND p.city = 'berlin' \
             RETURN p.id, p.city"
                .to_string(),
        ),
        q(
            "disjunction_rows",
            "MATCH (p:Person) WHERE p.id < 200 AND (p.age < 25 OR p.age >= 60) \
             RETURN p.id, p.age"
                .to_string(),
        ),
        q(
            "negation_rows",
            "MATCH (p:Person) WHERE p.id < 100 AND NOT p.age < 40 RETURN p.id, p.age".to_string(),
        ),
        // ---- One hop, in both directions ------------------------------------
        q(
            "one_hop_rows",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id = {median} \
                 RETURN b.id, b.name"
            ),
        ),
        q(
            "one_hop_cold_rows",
            format!("MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id = {cold} RETURN b.id"),
        ),
        q(
            "incoming_hop_rows",
            format!("MATCH (a:Person)<-[:KNOWS]-(b:Person) WHERE a.id = {median} RETURN b.id"),
        ),
        q(
            "one_hop_filtered_rows",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) \
                 WHERE a.id = {median} AND b.age >= 30 AND b.age < 40 RETURN b.id, b.age"
            ),
        ),
        // ---- Two fixed hops, where relationship uniqueness is observable ----
        q(
            "two_hop_rows",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} RETURN b.id, c.id"
            ),
        ),
        q(
            "two_hop_distinct_rows",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} RETURN DISTINCT c.id"
            ),
        ),
        q(
            "expand_into_rows",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} AND c.id = {target} RETURN b.id"
            ),
        ),
        // No cyclic or direction-mixing pattern here: it could bind one edge to
        // two slots, where LadybugDB's walk semantics legitimately disagree. Those
        // shapes are covered by the generated corpus, which has an adjudicating
        // reference; putting one here would make this gate report a divergence that
        // is not a defect.
        // ---- Grouped aggregation, which emits one row per group -------------
        // The output stays bounded by the group count even where the work is not,
        // and a per-group result catches a wrong group key or a wrong per-group
        // tally that a single total would hide.
        qg(
            "grouped_out_degree",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id < 50 \
             RETURN a.id, count(b)"
                .to_string(),
        ),
        qg(
            "grouped_count_star",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id < 50 \
             RETURN a.id, count(*)"
                .to_string(),
        ),
        qg(
            "grouped_by_city",
            "MATCH (p:Person) WHERE p.id < 1000 RETURN p.city, count(p)".to_string(),
        ),
        qg(
            "grouped_min_max",
            "MATCH (p:Person) WHERE p.id < 1000 RETURN p.city, min(p.age), max(p.age)".to_string(),
        ),
        qg(
            "grouped_two_hop",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {median} RETURN b.id, count(c)"
            ),
        ),
    ]
}

/// Run the differential corpus on both databases and report. Returns the number of
/// mismatches so the caller can fail the run.
///
/// Nothing here is timed. A divergence is unconditionally a defect (see
/// [`DiffQuery`]), so unlike the timing workload's pre-check there is no oracle to
/// consult and no attributed-divergence category to fall into.
fn run_differential(graph: &Graph, conn: &Connection, queries: &[DiffQuery]) -> usize {
    let mut mismatches = 0;
    let mut unsupported = 0;
    for query in queries {
        match compare_query(graph, conn, &query.cypher) {
            Verdict::Agree => {}
            Verdict::Mismatch(detail) => {
                mismatches += 1;
                println!("  MISMATCH {:<26} {detail}", query.name);
                println!("    {}", query.cypher);
            }
            Verdict::Unsupported(detail) => {
                // A curated query is supposed to be inside both surfaces, so this
                // is a corpus bug rather than a defect in either database; it is
                // does not fail the run.
                unsupported += 1;
                println!("  UNSUPPORTED {:<23} {detail}", query.name);
                println!("    {}", query.cypher);
            }
        }
    }
    println!(
        "differential: {} curated quer{} compared, {mismatches} mismatch(es){}",
        queries.len(),
        if queries.len() == 1 { "y" } else { "ies" },
        if unsupported > 0 {
            format!(", {unsupported} outside one database's surface")
        } else {
            String::new()
        }
    );
    mismatches
}

/// Run the generated corpus, attributing and shrinking anything that diverges.
///
/// Returns the count that should fail the run: IssunDB defects plus reference
/// defects. A LadybugDB divergence is counted and reported separately, because it
/// is a known difference in that database rather than a problem here.
fn run_generated(
    graph: &Graph,
    conn: &Connection,
    g: &RefGraph,
    shapes: &[GenQuery],
    seed: u64,
) -> usize {
    let (mut bugs, mut ladybug, mut suspect, mut unsupported) = (0, 0, 0, 0);
    for (i, shape) in shapes.iter().enumerate() {
        // Only the actionable classes are reported in detail. A LadybugDB
        // walk-semantics divergence is expected on any pattern that can bind one
        // edge to two slots, and printing each one would bury the rest.
        let (label, detail) = match classify_generated(graph, conn, g, shape) {
            GenVerdict::Agree => continue,
            GenVerdict::Unsupported => {
                unsupported += 1;
                continue;
            }
            GenVerdict::LadybugDivergence => {
                ladybug += 1;
                continue;
            }
            GenVerdict::IssundbBug(d) => {
                bugs += 1;
                ("ISSUNDB BUG", d)
            }
            GenVerdict::ReferenceSuspect(d) => {
                suspect += 1;
                ("REFERENCE SUSPECT", d)
            }
        };
        let small = shrink(graph, conn, g, shape);
        println!("  {label} generated[{i}] (seed {seed})  {detail}");
        println!("    shrunk:   {}", small.render());
        let original = shape.render();
        if original != small.render() {
            println!("    original: {original}");
        }
    }
    println!(
        "generated:    {} quer{} compared; {bugs} issundb defect(s), \
         {ladybug} ladybugdb walk-semantics divergence(s), {suspect} reference suspect(s){}",
        shapes.len(),
        if shapes.len() == 1 { "y" } else { "ies" },
        if unsupported > 0 {
            format!(", {unsupported} outside one database's surface")
        } else {
            String::new()
        }
    );
    bugs + suspect
}

/// Greedily drop pieces of a failing query while its verdict stays actionable.
///
/// Bounded by `SHRINK_BUDGET` classifications so one failure cannot turn into a
/// long run; the result is the smallest shape reached inside that budget, which is
/// always still a reproducing one.
fn shrink(graph: &Graph, conn: &Connection, g: &RefGraph, start: &GenQuery) -> GenQuery {
    /// Classifications the shrinker may spend on one finding.
    const SHRINK_BUDGET: usize = 60;
    let mut best = start.clone();
    let mut spent = 0;
    'outer: loop {
        for candidate in simplifications(&best) {
            if spent >= SHRINK_BUDGET {
                break 'outer;
            }
            spent += 1;
            if matches!(
                classify_generated(graph, conn, g, &candidate),
                GenVerdict::IssundbBug(_) | GenVerdict::ReferenceSuspect(_)
            ) {
                best = candidate;
                continue 'outer;
            }
        }
        break;
    }
    best
}

/// A generated differential query in structured form.
///
/// Structured rather than a string so a failure can be shrunk mechanically: the
/// shrinker drops pieces and re-tests, which turns a three-hop query carrying four
/// predicates into the smallest shape that still reproduces. Rendering happens in
/// one place ([`GenQuery::render`]), so every generated query is built the same way
/// the invariants assume.
#[derive(Clone)]
struct GenQuery {
    /// One entry per fixed hop; `v0` through `v{hops.len()}` are the variables.
    hops: Vec<Hop>,
    /// Closes the last variable back onto `v0` with one more outgoing hop.
    cycle: bool,
    anchor: Anchor,
    predicates: Vec<Pred>,
    projection: Projection,
    distinct: bool,
}

#[derive(Clone, Copy)]
struct Hop {
    incoming: bool,
}

/// How `v0` is pinned down. Every generated query has an anchor, because an
/// unanchored pattern over a large graph returns a row set too big to compare.
#[derive(Clone, Copy)]
enum Anchor {
    /// A single source node, by indexed id.
    Probe(u64),
    /// An id prefix, for multi-source expansion. The cutoff comes from
    /// [`bounded_id_cutoff`], never from a guess.
    IdBelow(u64),
}

#[derive(Clone, Copy)]
struct Pred {
    var: usize,
    kind: PredKind,
}

/// Predicate forms over the three comparable property types in the dataset.
/// Deliberately no float and no null: see [`DiffQuery`].
#[derive(Clone, Copy)]
enum PredKind {
    AgeRange(u64, u64),
    AgeAtLeast(u64),
    NotAgeBelow(u64),
    AgeOutside(u64, u64),
    CityEq(&'static str),
    CityNe(&'static str),
    IdBelow(u64),
}

#[derive(Clone)]
enum Projection {
    /// Plain property reads, so the comparison is over row content.
    Props(Vec<(usize, Prop)>),
    /// One group key plus one aggregate, so the comparison is over one row per
    /// group rather than a single total.
    Grouped { key: (usize, Prop), agg: Agg },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prop {
    Id,
    Name,
    Age,
    City,
}

impl Prop {
    fn as_str(self) -> &'static str {
        match self {
            Prop::Id => "id",
            Prop::Name => "name",
            Prop::Age => "age",
            Prop::City => "city",
        }
    }
}

#[derive(Clone, Copy)]
enum Agg {
    CountStar,
    CountVar(usize),
    CountProp(usize, Prop),
    MinAge(usize),
    MaxAge(usize),
}

impl GenQuery {
    fn render(&self) -> String {
        let mut pattern = String::from("(v0:Person)");
        for (i, hop) in self.hops.iter().enumerate() {
            pattern.push_str(if hop.incoming {
                "<-[:KNOWS]-"
            } else {
                "-[:KNOWS]->"
            });
            pattern.push_str(&format!("(v{}:Person)", i + 1));
        }
        if self.cycle {
            pattern.push_str("-[:KNOWS]->(v0)");
        }

        let mut wheres = vec![match self.anchor {
            Anchor::Probe(id) => format!("v0.id = {id}"),
            Anchor::IdBelow(k) => format!("v0.id < {k}"),
        }];
        for p in &self.predicates {
            let v = p.var;
            wheres.push(match p.kind {
                PredKind::AgeRange(lo, hi) => format!("v{v}.age >= {lo} AND v{v}.age < {hi}"),
                PredKind::AgeAtLeast(n) => format!("v{v}.age >= {n}"),
                PredKind::NotAgeBelow(n) => format!("NOT v{v}.age < {n}"),
                PredKind::AgeOutside(lo, hi) => format!("(v{v}.age < {lo} OR v{v}.age >= {hi})"),
                PredKind::CityEq(c) => format!("v{v}.city = '{c}'"),
                PredKind::CityNe(c) => format!("v{v}.city <> '{c}'"),
                PredKind::IdBelow(k) => format!("v{v}.id < {k}"),
            });
        }

        let projection = match &self.projection {
            Projection::Props(items) => items
                .iter()
                .map(|(v, prop)| format!("v{v}.{}", prop.as_str()))
                .collect::<Vec<_>>()
                .join(", "),
            Projection::Grouped { key, agg } => {
                let key_text = format!("v{}.{}", key.0, key.1.as_str());
                let agg_text = match *agg {
                    Agg::CountStar => "count(*)".to_string(),
                    Agg::CountVar(v) => format!("count(v{v})"),
                    Agg::CountProp(v, prop) => format!("count(v{v}.{})", prop.as_str()),
                    Agg::MinAge(v) => format!("min(v{v}.age)"),
                    Agg::MaxAge(v) => format!("max(v{v}.age)"),
                };
                format!("{key_text}, {agg_text}")
            }
        };
        let distinct = if self.distinct { "DISTINCT " } else { "" };
        format!(
            "MATCH {pattern} WHERE {} RETURN {distinct}{projection}",
            wheres.join(" AND ")
        )
    }
}

/// Largest `id` cutoff whose nodes together have at most `edge_budget` outgoing
/// edges, capped at `max_cutoff`.
///
/// Multi-source expansion has to be bounded by construction rather than by a
/// guess, because the skewed generator concentrates edges on **low** ids: under
/// Zipf a plain `id < 100` selects precisely the hubs, and a two-hop expansion
/// from them can enumerate a large fraction of the graph. Measuring the real
/// out-degrees keeps the bound honest under either skew.
fn bounded_id_cutoff(
    out_adj: &[Vec<u64>],
    in_adj: &[Vec<u64>],
    edge_budget: u64,
    max_cutoff: u64,
) -> u64 {
    let mut total = 0u64;
    for id in 0..out_adj.len() as u64 {
        if id >= max_cutoff {
            return max_cutoff;
        }
        // Budget against the larger of the two directions. The generator emits
        // incoming hops as well as outgoing ones, so a cutoff measured only over
        // out-degrees says nothing about the work an incoming chain does, and under
        // Zipf the in-degree hubs are exactly the low ids an `id <` bound selects.
        let out_deg = out_adj[id as usize].len() as u64;
        let in_deg = in_adj[id as usize].len() as u64;
        total += out_deg.max(in_deg);
        if total > edge_budget {
            return id.max(1);
        }
    }
    (out_adj.len() as u64).min(max_cutoff).max(1)
}

/// Generate `count` differential queries from `seed`.
///
/// Deterministic, so a reported failure replays from the seed the run prints. The
/// shapes stay inside the intersection of the two databases' surfaces: fixed-length
/// `:KNOWS` hops, `WHERE` over the three comparable property types, and either a
/// property projection or a single grouped aggregate. No variable-length pattern,
/// no relationship variable (its only property is a float), no float or null in a
/// projection, and no `ORDER BY` (rows are compared as sorted sets, so a tie would
/// be a false mismatch).
fn generate_queries(
    count: usize,
    seed: u64,
    probes: &Probes,
    out_adj: &[Vec<u64>],
    in_adj: &[Vec<u64>],
) -> Vec<GenQuery> {
    let mut rng = Lcg(seed);
    // Edge budget per hop count: the reachable set multiplies with each hop, so
    // the multi-source cutoff has to tighten as the chain grows.
    let cutoff_for = |hops: usize, rng: &mut Lcg| -> u64 {
        let budget = match hops {
            0 | 1 => 2_000,
            2 => 200,
            _ => 40,
        };
        let max = bounded_id_cutoff(out_adj, in_adj, budget, 2_000);
        // Vary the cutoff below the bound so runs cover more than one width.
        1 + rng.next() % max
    };
    let city = |rng: &mut Lcg| CITIES[(rng.next() % CITIES.len() as u64) as usize];
    // Ages are `18 + id % 50`, so bounds are drawn from that range plus a margin
    // either side to cover the empty and total selections.
    let age = |rng: &mut Lcg| 15 + rng.next() % 55;
    let prop = |rng: &mut Lcg| match rng.next() % 4 {
        0 => Prop::Id,
        1 => Prop::Name,
        2 => Prop::Age,
        _ => Prop::City,
    };

    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let hops: Vec<Hop> = (0..rng.next() % 4)
            .map(|_| Hop {
                incoming: rng.next() % 4 == 0,
            })
            .collect();
        let cycle = !hops.is_empty() && rng.next() % 5 == 0;
        let vars = hops.len() + 1;
        let anchor = if hops.is_empty() || rng.next() % 2 == 0 {
            Anchor::IdBelow(cutoff_for(hops.len(), &mut rng))
        } else if rng.next() % 2 == 0 {
            Anchor::Probe(probes.median)
        } else {
            Anchor::Probe(probes.cold)
        };

        let predicates: Vec<Pred> = (0..rng.next() % 4)
            .map(|_| {
                let var = (rng.next() % vars as u64) as usize;
                let kind = match rng.next() % 7 {
                    0 => {
                        let lo = age(&mut rng);
                        PredKind::AgeRange(lo, lo + 1 + rng.next() % 20)
                    }
                    1 => PredKind::AgeAtLeast(age(&mut rng)),
                    2 => PredKind::NotAgeBelow(age(&mut rng)),
                    3 => {
                        let lo = age(&mut rng);
                        PredKind::AgeOutside(lo, lo + 1 + rng.next() % 20)
                    }
                    4 => PredKind::CityEq(city(&mut rng)),
                    5 => PredKind::CityNe(city(&mut rng)),
                    _ => PredKind::IdBelow(1 + rng.next() % 2_000),
                };
                Pred { var, kind }
            })
            .collect();

        let (projection, distinct) = if rng.next() % 5 < 2 {
            let key_var = (rng.next() % vars as u64) as usize;
            let agg_var = (rng.next() % vars as u64) as usize;
            let agg = match rng.next() % 5 {
                0 => Agg::CountStar,
                1 => Agg::CountVar(agg_var),
                2 => Agg::CountProp(agg_var, prop(&mut rng)),
                3 => Agg::MinAge(agg_var),
                _ => Agg::MaxAge(agg_var),
            };
            // No `DISTINCT` over an aggregate: it would deduplicate group rows,
            // which says nothing about traversal or grouping.
            (
                Projection::Grouped {
                    key: (key_var, prop(&mut rng)),
                    agg,
                },
                false,
            )
        } else {
            let mut items: Vec<(usize, Prop)> = (0..1 + rng.next() % 3)
                .map(|_| ((rng.next() % vars as u64) as usize, prop(&mut rng)))
                .collect();
            // Drop repeats wherever they fall. `dedup` alone removes only adjacent
            // ones, so a draw of `[(0, Id), (1, Age), (0, Id)]` kept the repeat and
            // still emitted `RETURN v0.id, v1.age, v0.id`, the exact case this
            // guards: a projected column appearing twice, where the two databases'
            // column naming (which this harness does not compare) is the only thing
            // that could differ. Order is preserved so the seed still reproduces.
            let mut seen = Vec::new();
            items.retain(|item| {
                if seen.contains(item) {
                    false
                } else {
                    seen.push(*item);
                    true
                }
            });
            (Projection::Props(items), rng.next() % 4 == 0)
        };

        out.push(GenQuery {
            hops,
            cycle,
            anchor,
            predicates,
            projection,
            distinct,
        });
    }
    out
}

/// Every one-step simplification of `q`, for the shrinker to try.
fn simplifications(q: &GenQuery) -> Vec<GenQuery> {
    let mut out = Vec::new();
    if q.distinct {
        let mut s = q.clone();
        s.distinct = false;
        out.push(s);
    }
    if q.cycle {
        let mut s = q.clone();
        s.cycle = false;
        out.push(s);
    }
    for i in 0..q.predicates.len() {
        let mut s = q.clone();
        s.predicates.remove(i);
        out.push(s);
    }
    // Dropping the last hop unbinds its variable, so only offer it when nothing
    // still refers to that variable.
    if let Some(highest_var) = q.hops.len().checked_sub(1) {
        let refers = |v: usize| v > highest_var;
        let projection_refers = match &q.projection {
            Projection::Props(items) => items.iter().any(|(v, _)| refers(*v)),
            Projection::Grouped { key, agg } => {
                refers(key.0)
                    || match *agg {
                        Agg::CountStar => false,
                        Agg::CountVar(v)
                        | Agg::CountProp(v, _)
                        | Agg::MinAge(v)
                        | Agg::MaxAge(v) => refers(v),
                    }
            }
        };
        if !q.predicates.iter().any(|p| refers(p.var)) && !projection_refers {
            let mut s = q.clone();
            s.hops.pop();
            out.push(s);
        }
    }
    if let Projection::Props(items) = &q.projection {
        if items.len() > 1 {
            let mut s = q.clone();
            s.projection = Projection::Props(vec![items[0]]);
            out.push(s);
        }
    }
    if let Projection::Grouped { key, agg } = &q.projection {
        if !matches!(agg, Agg::CountStar) {
            let mut s = q.clone();
            s.projection = Projection::Grouped {
                key: *key,
                agg: Agg::CountStar,
            };
            out.push(s);
        }
    }
    out
}

/// Edge-identified adjacency in both directions: `(neighbor, edge index)` per
/// entry, where the index is the position in `Dataset::knows`.
///
/// The edge index is what makes relationship uniqueness expressible, so the
/// reference evaluator needs this rather than the plain neighbour lists
/// [`out_adjacency`] returns.
struct RefGraph {
    out: Vec<Vec<(u64, usize)>>,
    inc: Vec<Vec<(u64, usize)>>,
    /// `(name, age, city)` per node id.
    props: Vec<(String, u64, &'static str)>,
}

impl RefGraph {
    fn build(data: &Dataset) -> Self {
        let n = data.persons.len();
        let mut out = vec![Vec::new(); n];
        let mut inc = vec![Vec::new(); n];
        for (e, &(src, dst, _)) in data.knows.iter().enumerate() {
            out[src as usize].push((dst, e));
            inc[dst as usize].push((src, e));
        }
        let props = data
            .persons
            .iter()
            .map(|(_, name, age, city)| (name.clone(), *age, *city))
            .collect();
        Self { out, inc, props }
    }

    fn prop(&self, id: u64, prop: Prop) -> String {
        let (name, age, city) = &self.props[id as usize];
        match prop {
            Prop::Id => id.to_string(),
            Prop::Name => name.clone(),
            Prop::Age => age.to_string(),
            Prop::City => (*city).to_string(),
        }
    }

    fn age(&self, id: u64) -> u64 {
        self.props[id as usize].1
    }

    fn city(&self, id: u64) -> &'static str {
        self.props[id as usize].2
    }
}

/// Evaluate a generated query directly over the dataset, under openCypher
/// semantics, and return its rows normalized the way [`issundb_rows`] normalizes.
///
/// This is the adjudicating oracle for the generated corpus, and it is what makes a
/// divergence attributable instead of merely visible. Both databases are compared
/// against it, so a disagreement names the database at fault rather than leaving a
/// human to work out which of two answers is right.
///
/// It exists because the assumption that a fixed-length pattern needs no
/// adjudication is false. Relationship uniqueness applies to every pattern with two
/// or more relationship slots: `(a)-[:R]->(b)<-[:R]-(c)` can bind one edge to both
/// slots when `c` is `a`, and `(a)<-[:R]-(b)-[:R]->(a)` always can. openCypher
/// forbids that; the pinned LadybugDB build permits it, which is the same
/// walk-against-trail difference the timed workload's `Oracle` adjudicates for its
/// longer chains.
///
/// Brute force on purpose: it enumerates assignments and filters, so it is easy to
/// read against the specification and has no plan to be wrong about. The generated
/// anchors are bounded ([`bounded_id_cutoff`]) precisely so this stays cheap.
fn reference_rows(g: &RefGraph, q: &GenQuery) -> Vec<Vec<String>> {
    let node_count = g.props.len() as u64;
    let sources: Vec<u64> = match q.anchor {
        Anchor::Probe(id) => {
            if id < node_count {
                vec![id]
            } else {
                Vec::new()
            }
        }
        Anchor::IdBelow(k) => (0..k.min(node_count)).collect(),
    };

    // Depth-first over the hop chain, carrying the bound variables and the edges
    // already used so no edge fills two slots.
    let mut assignments: Vec<Vec<u64>> = Vec::new();
    let mut stack: Vec<(Vec<u64>, Vec<usize>)> =
        sources.into_iter().map(|s| (vec![s], Vec::new())).collect();
    while let Some((vars, used)) = stack.pop() {
        let depth = vars.len() - 1;
        if depth == q.hops.len() {
            if !q.cycle {
                assignments.push(vars);
                continue;
            }
            // The closing hop is always outgoing from the last variable to `v0`.
            let last = *vars.last().unwrap();
            let first = vars[0];
            for &(dst, e) in &g.out[last as usize] {
                if dst == first && !used.contains(&e) {
                    assignments.push(vars.clone());
                }
            }
            continue;
        }
        let from = vars[depth];
        let edges = if q.hops[depth].incoming {
            &g.inc[from as usize]
        } else {
            &g.out[from as usize]
        };
        for &(next, e) in edges {
            if used.contains(&e) {
                continue;
            }
            let mut vars_next = vars.clone();
            vars_next.push(next);
            let mut used_next = used.clone();
            used_next.push(e);
            stack.push((vars_next, used_next));
        }
    }

    // Predicates apply to whole assignments, exactly as a `WHERE` over the bound
    // variables does.
    assignments.retain(|vars| {
        q.predicates.iter().all(|p| {
            let id = vars[p.var];
            match p.kind {
                PredKind::AgeRange(lo, hi) => g.age(id) >= lo && g.age(id) < hi,
                PredKind::AgeAtLeast(n) => g.age(id) >= n,
                PredKind::NotAgeBelow(n) => g.age(id) >= n,
                PredKind::AgeOutside(lo, hi) => g.age(id) < lo || g.age(id) >= hi,
                PredKind::CityEq(c) => g.city(id) == c,
                PredKind::CityNe(c) => g.city(id) != c,
                PredKind::IdBelow(k) => id < k,
            }
        })
    });

    match &q.projection {
        Projection::Props(items) => {
            let mut rows: Vec<Vec<String>> = assignments
                .iter()
                .map(|vars| {
                    items
                        .iter()
                        .map(|(v, prop)| g.prop(vars[*v], *prop))
                        .collect()
                })
                .collect();
            if q.distinct {
                rows.sort();
                rows.dedup();
            }
            rows
        }
        Projection::Grouped { key, agg } => {
            // Group in first-seen order; the caller sorts, so only the grouping
            // itself has to match.
            let mut groups: Vec<(String, Vec<u64>)> = Vec::new();
            for vars in &assignments {
                let k = g.prop(vars[key.0], key.1);
                let agg_input = match *agg {
                    // Every property in this dataset is present on every node, so
                    // `count(v.prop)` and `count(v)` both count the row.
                    Agg::CountStar | Agg::CountVar(_) | Agg::CountProp(_, _) => 0,
                    Agg::MinAge(v) | Agg::MaxAge(v) => g.age(vars[v]),
                };
                match groups.iter_mut().find(|(gk, _)| *gk == k) {
                    Some((_, acc)) => acc.push(agg_input),
                    None => groups.push((k, vec![agg_input])),
                }
            }
            groups
                .into_iter()
                .map(|(k, acc)| {
                    let value = match *agg {
                        Agg::CountStar | Agg::CountVar(_) | Agg::CountProp(_, _) => {
                            acc.len() as u64
                        }
                        Agg::MinAge(_) => acc.iter().copied().min().unwrap_or(0),
                        Agg::MaxAge(_) => acc.iter().copied().max().unwrap_or(0),
                    };
                    vec![k, value.to_string()]
                })
                .collect()
        }
    }
}

/// What a generated query established, once both databases are compared against the
/// reference evaluator.
enum GenVerdict {
    /// Both databases match the reference.
    Agree,
    /// IssunDB matches the reference and LadybugDB does not: the walk-against-trail
    /// difference. Reported, and not a failure of IssunDB.
    LadybugDivergence,
    /// IssunDB does not match the reference. This is the finding worth having.
    IssundbBug(String),
    /// The reference disagrees with both databases, which agree with each other. Two
    /// independent implementations agreeing is strong evidence the reference is the
    /// thing that is wrong, so this fails the run as a harness defect rather than
    /// being attributed to either database.
    ReferenceSuspect(String),
    /// One database rejected the query. LadybugDB's surface is narrower, so this is
    /// not a defect.
    Unsupported,
}

fn classify_generated(graph: &Graph, conn: &Connection, g: &RefGraph, q: &GenQuery) -> GenVerdict {
    let cypher = q.render();
    let (is_res, lb_res) = match (graph.query(&cypher), conn.query(&cypher)) {
        (Ok(a), Ok(b)) => (a, b),
        // LadybugDB's surface is narrower, so it rejecting a query is expected.
        (Ok(_), Err(_)) => return GenVerdict::Unsupported,
        // IssunDB rejecting one is not. The generator stays inside its surface by
        // construction, and the reference is right here to say what the answer
        // should have been, so this is at least as strong a signal as the mutual
        // rejection below, which is already failed.
        (Err(e), Ok(_)) => {
            let expected = reference_rows(g, q);
            return GenVerdict::IssundbBug(format!(
                "issundb rejected a query ladybugdb answered and the reference gives \
                 {} row(s) for; issundb said: {e}",
                expected.len()
            ));
        }
        (Err(e), Err(_)) => {
            // Both rejected it. That is only agreement if the query really has no
            // rows; consulting the reference here is the whole point of having an
            // adjudicator, since a shape IssunDB has just regressed on would
            // otherwise be scored as agreement by the oracle added to catch it.
            let expected = reference_rows(g, q);
            return if expected.is_empty() {
                GenVerdict::Agree
            } else {
                GenVerdict::IssundbBug(format!(
                    "both databases rejected a query the reference answers with {} row(s); \
                     issundb said: {e}",
                    expected.len()
                ))
            };
        }
    };
    let sorted = |mut rows: Vec<Vec<String>>| {
        rows.sort();
        rows
    };
    let is_rows = sorted(issundb_rows(&is_res));
    let lb_rows = sorted(ladybugdb_rows(lb_res));
    let ref_rows = sorted(reference_rows(g, q));

    let summary = |a: &[Vec<String>], b: &[Vec<String>], b_name: &str| {
        let first = a
            .iter()
            .zip(b)
            .find(|(x, y)| x != y)
            .map(|(x, y)| format!("reference {x:?} against {b_name} {y:?}"))
            .unwrap_or_else(|| "no differing shared row; the row counts differ".to_string());
        format!(
            "reference {} row(s), {b_name} {} row(s); first difference: {first}",
            a.len(),
            b.len()
        )
    };

    match (is_rows == ref_rows, lb_rows == ref_rows) {
        (true, true) => GenVerdict::Agree,
        (true, false) => GenVerdict::LadybugDivergence,
        (false, _) if is_rows == lb_rows => {
            GenVerdict::ReferenceSuspect(summary(&ref_rows, &is_rows, "both databases"))
        }
        (false, _) => GenVerdict::IssundbBug(summary(&ref_rows, &is_rows, "issundb")),
    }
}

/// Compare one query on both databases.
///
/// `Ok(Verdict::Agree)` when the sorted row sets match. A query one database rejects
/// and the other accepts is a surface difference, not a wrong answer: LadybugDB
/// supports a narrower Cypher surface, so that must not fail the run or a single
/// unsupported construct would stop the whole sweep.
fn compare_query(graph: &Graph, conn: &Connection, cypher: &str) -> Verdict {
    let is_result = graph.query(cypher);
    let lb_result = conn.query(cypher);
    match (is_result, lb_result) {
        (Ok(is_res), Ok(lb_res)) => {
            let mut is_rows = issundb_rows(&is_res);
            let mut lb_rows = ladybugdb_rows(lb_res);
            is_rows.sort();
            lb_rows.sort();
            if is_rows == lb_rows {
                return Verdict::Agree;
            }
            let first_diff = is_rows
                .iter()
                .zip(&lb_rows)
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("issundb {a:?} against ladybugdb {b:?}"))
                .unwrap_or_else(|| "no differing shared row; the row counts differ".to_string());
            Verdict::Mismatch(format!(
                "issundb {} row(s), ladybugdb {} row(s); first difference: {first_diff}",
                is_rows.len(),
                lb_rows.len()
            ))
        }
        // Both rejected it. Unlike `classify_generated`, this has no reference to
        // adjudicate against, so it cannot know whether the query should have
        // returned rows. It must not call that agreement either: a curated query
        // IssunDB has regressed on will usually also be rejected by LadybugDB, whose
        // surface is narrower, which is precisely how the corpus would go dark one
        // query at a time while the gate kept exiting zero.
        (Err(is_err), Err(lb_err)) => Verdict::Mismatch(format!(
            "both databases rejected a curated query; issundb said: {is_err}; \
             ladybugdb said: {lb_err}"
        )),
        // LadybugDB's surface is narrower, so it rejecting a query is tolerated.
        // IssunDB rejecting one is not: the curated corpus is by construction inside
        // IssunDB's surface, so a rejection there is a regression, and scoring it as
        // merely "unsupported" would let the whole corpus go dark one query at a
        // time while the gate kept exiting zero.
        (Ok(_), Err(e)) => Verdict::Unsupported(format!("ladybugdb rejected it: {e}")),
        (Err(e), Ok(_)) => Verdict::Mismatch(format!("issundb rejected a curated query: {e}")),
    }
}

enum Verdict {
    Agree,
    Mismatch(String),
    Unsupported(String),
}

fn workload(probes: &Probes) -> Vec<Query> {
    let cold = probes.cold;
    let median = probes.median;
    let hub = probes.hub;
    let target = probes.expand_target;
    let q = |name, scope, cypher| Query {
        name,
        cypher,
        scope,
        oracle: Oracle::Exact,
    };
    let qo = |name, scope, cypher, oracle| Query {
        name,
        cypher,
        scope,
        oracle,
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
        qo(
            "three_hop_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 -[:KNOWS]->(d:Person) WHERE a.id = {median} RETURN count(d) AS n"
            ),
            Oracle::TrailCount(3, 3),
        ),
        // Unlike the path-counting hops above, this counts distinct endpoints
        // (count(DISTINCT e)) to bound the four-hop combinatorial blowup; the
        // name records the different semantics.
        qo(
            "four_hop_distinct",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 -[:KNOWS]->(d:Person)-[:KNOWS]->(e:Person) \
                 WHERE a.id = {median} RETURN count(DISTINCT e) AS n"
            ),
            Oracle::TrailEndpoints(4),
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
        qo(
            "var_length_count",
            Scope::ProbeLocal,
            format!(
                "MATCH (a:Person)-[:KNOWS*2..3]->(c:Person) \
                 WHERE a.id = {median} RETURN count(c) AS n"
            ),
            Oracle::TrailCount(2, 3),
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

/// Loads both databases at the given size, runs the workload, prints the result
/// table, and returns the per-query timings for the sweep's scaling summary.
fn run_at(cfg: &Config, nodes: u64, edges: u64) -> anyhow::Result<Vec<QueryTiming>> {
    if cfg.diff_only {
        println!(
            "dataset: {nodes} Person nodes, {edges} KNOWS edges ({} skew); \
             differential only, no timing\n",
            cfg.skew.as_str()
        );
    } else {
        println!(
            "dataset: {nodes} Person nodes, {edges} KNOWS edges ({} skew); \
             {} reps ({} warmups) per query\n",
            cfg.skew.as_str(),
            cfg.reps,
            cfg.warmups
        );
    }

    let data = generate(nodes, edges, cfg.skew);
    let probes = pick_probes(&data);
    let csv_dir = tempfile::tempdir()?;
    write_csvs(&data, csv_dir.path())?;

    // ---- Load both databases, timing each once ------------------------------
    let lb_dir = tempfile::tempdir()?;
    let db = Database::new(lb_dir.path().join("db"), SystemConfig::default())?;
    let mut conn = Connection::new(&db)?;
    // LadybugDB defaults to WALK semantics, where a relationship may repeat
    // within a path; openCypher (and IssunDB) require pairwise-distinct
    // relationships. Pin TRAIL for the day the pinned `lbug` build honors it;
    // today the setting registers but is inert (see
    // `tests/lbug_trail_semantics.rs`), so the trail-sensitive queries carry
    // an `Oracle` that adjudicates row-set divergences instead.
    conn.query("CALL recursive_pattern_semantic = 'TRAIL';")?;
    let default_threads = conn.get_max_num_threads_for_exec();
    let start = Instant::now();
    load_ladybugdb(&conn, csv_dir.path())?;
    let lb_load = start.elapsed();

    let is_dir = tempfile::tempdir()?;
    let graph = Graph::open(is_dir.path(), 2)?;
    let start = Instant::now();
    load_issundb(&graph, &data)?;
    let is_load = start.elapsed();

    let load_ratio = is_load.as_secs_f64() / lb_load.as_secs_f64().max(f64::EPSILON);
    println!(
        "load: issundb {is_load:?} (single write txn), ladybugdb {lb_load:?} (COPY FROM), \
         idb/ldb {load_ratio:.2}\n"
    );

    // Differential pass first, on row-returning queries, before anything is
    // timed. It runs at every dataset size in a sweep on purpose: the engine
    // switches internal strategies at size thresholds (a small gather served from
    // storage against built property columns, a per-source adjacency read against
    // a rebuilt snapshot), so the same corpus at 10k and 250k nodes is not the
    // same test.
    let mut diff_mismatches = run_differential(&graph, &conn, &differential_workload(&probes));
    if cfg.generated > 0 {
        let shapes = generate_queries(
            cfg.generated,
            cfg.seed,
            &probes,
            &out_adjacency(&data),
            &in_adjacency(&data),
        );
        diff_mismatches += run_generated(&graph, &conn, &RefGraph::build(&data), &shapes, cfg.seed);
    }
    println!();

    if cfg.diff_only {
        if diff_mismatches > 0 {
            anyhow::bail!("{diff_mismatches} differential mismatch(es)");
        }
        return Ok(Vec::new());
    }

    println!(
        "{:<20} {:>16} {:>16} {:>16} {:>9} {:>12} {:>10}  diff",
        "query", "issundb", "ladybugdb", "ladybugdb(1t)", "idb/ldb", "idb/ldb(1t)", "result"
    );
    // Render a measured timing as `median±h%`, where the percentage is the
    // 95% CI half-width relative to the median.
    let fmt = |b: &BenchStat| {
        let med = b.median.as_nanos().max(1) as f64;
        let half = (b.ci_hi.as_nanos() as f64 - b.ci_lo.as_nanos() as f64) / 2.0;
        let pct = (half / med * 100.0).round() as i64;
        let s = format!("{:.2?}±{pct}%", b.median);
        if b.samples < cfg.reps {
            format!("{s}*")
        } else {
            s
        }
    };
    // The issundb median over a ladybugdb median. Below 1.0 favors issundb. The
    // sub-1.0 range carries the interesting detail once issundb is an order of
    // magnitude ahead, so it gets an extra digit.
    let ratio = |a: &BenchStat, b: &BenchStat| {
        let r = a.median.as_secs_f64() / b.median.as_secs_f64().max(f64::EPSILON);
        if r < 1.0 {
            format!("{r:.3}")
        } else {
            format!("{r:.2}")
        }
    };
    let mut timings = Vec::new();
    let mut truncated = false;
    let mut mismatches = 0;
    let mut divergences = 0;
    for query in &workload(&probes) {
        let (name, cypher) = (query.name, &query.cypher);

        // Differential check before timing: medians for a query the databases
        // disagree on are meaningless (an engine doing the wrong amount of
        // work can look faster), so a divergent query is reported and not
        // timed. Sorted row sets must match exactly; for the trail-sensitive
        // queries, the dataset-computed trail reference adjudicates which
        // engine diverged from openCypher.
        let mut is_rows = issundb_rows(&graph.query(cypher)?);
        let mut lb_rows = ladybugdb_rows(conn.query(cypher)?);
        is_rows.sort();
        lb_rows.sort();
        if is_rows != lb_rows {
            let reference = match query.oracle {
                Oracle::Exact => None,
                Oracle::TrailCount(min, max) => {
                    Some(count_trails(&out_adjacency(&data), probes.median, min, max))
                }
                Oracle::TrailEndpoints(hops) => Some(count_trail_endpoints(
                    &out_adjacency(&data),
                    probes.median,
                    hops,
                )),
            };
            let issundb_matches_reference =
                reference.is_some_and(|n| is_rows == vec![vec![n.to_string()]]);
            timings.push(QueryTiming {
                name,
                scope: query.scope,
                issundb: None,
                ladybugdb_1t: None,
            });
            if issundb_matches_reference {
                // A known LadybugDB walk-semantics overcount, reported but
                // not a harness failure; the run stays usable.
                divergences += 1;
                println!(
                    "{name:<20} {:>16} {:>16} {:>16} {:>9} {:>12} {:>10}  DIVERGENT \
                     (ladybugdb walk semantics: ladybugdb {}, openCypher trails {})",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    lb_rows
                        .first()
                        .map(|row| row.join(","))
                        .unwrap_or_else(|| "no rows".to_string()),
                    reference.unwrap()
                );
            } else {
                mismatches += 1;
                println!(
                    "{name:<20} {:>16} {:>16} {:>16} {:>9} {:>12} {:>10}  MISMATCH \
                     (issundb {} rows: {:?}..., ladybugdb {} rows: {:?}...)",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    is_rows.len(),
                    is_rows.first(),
                    lb_rows.len(),
                    lb_rows.first()
                );
            }
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

        let is_stat = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            graph.query(cypher).unwrap();
        });

        conn.set_max_num_threads_for_exec(default_threads);
        let lb_stat = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            for _row in conn.query(cypher).unwrap() {}
        });
        conn.set_max_num_threads_for_exec(1);
        let lb_1t_stat = bench(cfg.warmups, cfg.reps, cfg.budget, || {
            for _row in conn.query(cypher).unwrap() {}
        });
        truncated |= is_stat.samples < cfg.reps
            || lb_stat.samples < cfg.reps
            || lb_1t_stat.samples < cfg.reps;

        println!(
            "{name:<20} {:>16} {:>16} {:>16} {:>9} {:>12} {:>10}  OK",
            fmt(&is_stat),
            fmt(&lb_stat),
            fmt(&lb_1t_stat),
            ratio(&is_stat, &lb_stat),
            ratio(&is_stat, &lb_1t_stat),
            result
        );
        timings.push(QueryTiming {
            name,
            scope: query.scope,
            issundb: Some(is_stat.median),
            ladybugdb_1t: Some(lb_1t_stat.median),
        });
    }
    // Legend below the table, where it does not push the rows off a first screen.
    println!(
        "\ntimings are the median ±the 95% bootstrap confidence interval half-width \
         over {} rounds.",
        cfg.reps
    );
    println!(
        "idb/ldb is the issundb median over the ladybugdb median, so below 1.0 favors \
         issundb. idb/ldb(1t) uses single-threaded ladybugdb, matching issundb's one \
         execution thread."
    );
    if truncated {
        println!(
            "* median and CI from fewer than {} reps; {}s per-query budget reached",
            cfg.reps,
            cfg.budget.as_secs()
        );
    }

    if divergences > 0 {
        println!(
            "{divergences} known walk-semantics divergence(s); \
             the affected queries are reported, not timed"
        );
    }
    let total_mismatches = mismatches + diff_mismatches;
    if total_mismatches > 0 {
        anyhow::bail!(
            "{total_mismatches} differential mismatch(es) \
             ({diff_mismatches} in the row-returning corpus, {mismatches} in the timed workload)"
        );
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
            println!("{:<20} {:>16} {:>16}", "query", "issundb", "ladybugdb(1t)");
            for qi in 0..reports[0].len() {
                if reports[0][qi].scope != scope {
                    continue;
                }
                let ratios = |get: fn(&QueryTiming) -> Option<Duration>| -> String {
                    (1..reports.len())
                        .map(|i| {
                            match (get(&reports[i - 1][qi]), get(&reports[i][qi])) {
                                (Some(prev), Some(next)) => {
                                    let ratio =
                                        next.as_secs_f64() / prev.as_secs_f64().max(f64::EPSILON);
                                    format!("{ratio:>6.1}x")
                                }
                                // Untimed at one of the sizes (a reported
                                // semantic divergence): no ratio.
                                _ => format!("{:>7}", "-"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                println!(
                    "{:<20} {:>16} {:>16}",
                    reports[0][qi].name,
                    ratios(|t| t.issundb),
                    ratios(|t| t.ladybugdb_1t)
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

    /// The generator must stay inside the shapes the reference evaluator and both
    /// databases can all handle, and it must be reproducible from its seed.
    #[test]
    fn generated_queries_are_deterministic_and_in_surface() {
        let data = generate(1_000, 5_000, Skew::Uniform);
        let probes = pick_probes(&data);
        let out_adj = out_adjacency(&data);
        let in_adj = in_adjacency(&data);
        let a = generate_queries(200, 0x1234, &probes, &out_adj, &in_adj);
        let b = generate_queries(200, 0x1234, &probes, &out_adj, &in_adj);
        let rendered: Vec<String> = a.iter().map(GenQuery::render).collect();
        assert_eq!(
            rendered,
            b.iter().map(GenQuery::render).collect::<Vec<_>>(),
            "the same seed must produce the same queries, or a report cannot be replayed"
        );
        assert!(
            generate_queries(200, 0x99, &probes, &out_adj, &in_adj)
                .iter()
                .map(GenQuery::render)
                .collect::<Vec<_>>()
                != rendered,
            "a different seed should explore different shapes"
        );

        for (q, text) in a.iter().zip(&rendered) {
            assert!(text.starts_with("MATCH (v0:Person)"), "{text}");
            assert!(text.contains(" WHERE "), "every query is anchored: {text}");
            // A variable-length pattern would leave the fixed-length grammar the
            // reference evaluator implements.
            assert!(
                !text.contains("*1") && !text.contains(".."),
                "no variable-length pattern: {text}"
            );
            // A relationship variable would expose the float weight.
            assert!(!text.contains("[r"), "no relationship variable: {text}");
            assert!(!text.contains("weight"), "no float projection: {text}");
            assert!(
                !text.contains("ORDER BY"),
                "rows are compared sorted: {text}"
            );
            // Every referenced variable must be bound by the pattern.
            let bound = q.hops.len();
            for p in &q.predicates {
                assert!(p.var <= bound, "predicate on an unbound variable: {text}");
            }
        }
    }

    /// The reference evaluator has to enforce relationship uniqueness, since that
    /// is the whole reason it exists. A two-hop pattern that doubles back can only
    /// match by binding one edge to both slots, so under openCypher it matches
    /// nothing.
    #[test]
    fn reference_evaluator_enforces_relationship_uniqueness() {
        let data = generate(50, 100, Skew::Uniform);
        let g = RefGraph::build(&data);
        let anchor = data.knows[0].1;

        let doubling_back = GenQuery {
            hops: vec![Hop { incoming: true }],
            cycle: true,
            anchor: Anchor::Probe(anchor),
            predicates: Vec::new(),
            projection: Projection::Props(vec![(1, Prop::Id)]),
            distinct: false,
        };
        assert!(
            reference_rows(&g, &doubling_back).is_empty(),
            "one edge cannot fill both relationship slots"
        );

        // The same pattern without the closing hop does match: one hop, one edge.
        let single_hop = GenQuery {
            cycle: false,
            ..doubling_back.clone()
        };
        assert_eq!(
            reference_rows(&g, &single_hop).len(),
            g.inc[anchor as usize].len(),
            "one incoming hop matches once per incoming edge"
        );
    }

    /// Shrinking must only ever return a shape reachable by dropping pieces, and
    /// must terminate. The simplification set is what both properties rest on.
    #[test]
    fn simplifications_strictly_shrink() {
        let q = GenQuery {
            hops: vec![Hop { incoming: false }, Hop { incoming: true }],
            cycle: true,
            anchor: Anchor::IdBelow(10),
            predicates: vec![
                Pred {
                    var: 0,
                    kind: PredKind::AgeAtLeast(30),
                },
                Pred {
                    var: 1,
                    kind: PredKind::CityEq("oslo"),
                },
            ],
            projection: Projection::Props(vec![(0, Prop::Id), (1, Prop::Name)]),
            distinct: true,
        };
        // The metric has to score the aggregate, not just the projection kind, or
        // the `Grouped -> CountStar` simplification is not strictly smaller and the
        // invariant this test states is violated by a branch it cannot see.
        let size = |q: &GenQuery| {
            q.hops.len()
                + q.predicates.len()
                + usize::from(q.cycle)
                + usize::from(q.distinct)
                + match &q.projection {
                    Projection::Props(items) => items.len(),
                    // `count(*)` is the floor; every other aggregate can still
                    // simplify to it, so it must score higher.
                    Projection::Grouped {
                        agg: Agg::CountStar,
                        ..
                    } => 1,
                    Projection::Grouped { .. } => 2,
                }
        };
        // Both projection kinds, so the grouped branch of `simplifications` is
        // actually generated here rather than only existing.
        let grouped = GenQuery {
            projection: Projection::Grouped {
                key: (0, Prop::City),
                agg: Agg::MaxAge(1),
            },
            distinct: false,
            ..q.clone()
        };
        for start in [&q, &grouped] {
            let candidates = simplifications(start);
            assert!(!candidates.is_empty());
            for c in &candidates {
                assert!(
                    size(c) < size(start),
                    "every simplification must be strictly smaller, or shrinking loops"
                );
            }
        }
        // A minimal shape offers nothing further to drop, which is what terminates
        // the shrink loop.
        let minimal = GenQuery {
            hops: Vec::new(),
            cycle: false,
            anchor: Anchor::Probe(1),
            predicates: Vec::new(),
            projection: Projection::Props(vec![(0, Prop::Id)]),
            distinct: false,
        };
        assert!(simplifications(&minimal).is_empty());
    }

    /// The differential corpus rests on two invariants, and neither is enforced by
    /// the type system, so they are pinned here.
    ///
    /// A variable-length pattern would reintroduce the walk-versus-trail question
    /// the timing workload needs a hand-written `Oracle` to answer, and turn a
    /// mismatch back into something a human has to adjudicate. A projection of
    /// nothing but an aggregate over the whole match would make the query a scalar
    /// comparison again, which is the weakness this corpus exists to avoid.
    #[test]
    fn differential_corpus_is_fixed_length_and_row_returning() {
        let data = generate(1_000, 5_000, Skew::Uniform);
        let probes = pick_probes(&data);
        let corpus = differential_workload(&probes);
        assert!(
            corpus.len() >= 15,
            "the corpus should be broad, not a token"
        );

        for q in &corpus {
            assert!(
                !q.cypher.contains('*') || q.cypher.contains("count(*)"),
                "{}: a variable-length pattern needs an adjudicating oracle, \
                 so it belongs in the generated corpus instead",
                q.name
            );
            // No pattern may be able to bind one edge to two relationship slots.
            let outgoing = q.cypher.matches("-[:KNOWS]->").count();
            let incoming = q.cypher.matches("<-[:KNOWS]-").count();
            assert!(
                outgoing + incoming <= 2 && (outgoing == 0 || incoming == 0),
                "{}: at most two same-direction hops, or LadybugDB's walk semantics \
                 may disagree without either database being wrong",
                q.name
            );
            assert!(
                !q.cypher.contains("->(a)") && !q.cypher.contains("->(v0)"),
                "{}: a closing hop can always reuse an edge",
                q.name
            );
            // A grouped aggregate returns one row per group, which is the point; an
            // aggregate with no grouping key collapses to a single row and is back to
            // comparing one number. The declared flag decides, so adding a query with
            // any aggregate function cannot trip this by accident.
            let has_aggregate = ["count(", "min(", "max(", "sum(", "avg("]
                .iter()
                .any(|f| q.cypher.contains(f));
            assert!(
                q.grouped || !has_aggregate,
                "{}: an aggregate in a projection declared non-grouped compares a \
                 single scalar; use the grouped constructor if it has a group key",
                q.name
            );
            if q.grouped {
                assert!(
                    has_aggregate,
                    "{}: declared grouped but projects no aggregate",
                    q.name
                );
            }
        }

        let names: HashSet<_> = corpus.iter().map(|q| q.name).collect();
        assert_eq!(names.len(), corpus.len(), "query names must be unique");
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

    /// The trail oracle against an independently computed reference (a
    /// brute-force Python reimplementation of the seeded generator and a
    /// trail DFS produced these values for the 200-node, 1000-edge uniform
    /// dataset from its median probe).
    #[test]
    fn trail_oracle_matches_independent_reference() {
        let data = generate(200, 1_000, Skew::Uniform);
        let probes = pick_probes(&data);
        let adjacency = out_adjacency(&data);
        assert_eq!(probes.median, 93);
        assert_eq!(count_trails(&adjacency, probes.median, 3, 3), 133);
        assert_eq!(count_trails(&adjacency, probes.median, 2, 3), 158);
        // Walk counts differ here (134 three-hop walks, 159 walks at 2..3),
        // so this dataset is exactly the shape that distinguishes the
        // semantics.
    }

    /// A two-node cycle separates trail endpoints from walk endpoints at
    /// even hop counts: the only four-hop walk 0->1->0->1->0 reuses edges,
    /// so no four-hop trail exists.
    #[test]
    fn trail_endpoints_exclude_edge_reusing_walks() {
        let adjacency = vec![vec![1], vec![0]];
        assert_eq!(count_trail_endpoints(&adjacency, 0, 2), 1); // 0->1->0
        assert_eq!(count_trail_endpoints(&adjacency, 0, 4), 0);
        assert_eq!(count_trails(&adjacency, 0, 2, 3), 1);
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
