//! Comparison harness running the same Cypher workload against IssunDB and
//! LadybugDB (the Kùzu successor, via the `lbug` crate).
//!
//! Both engines load an identical synthetic social graph, then each query in
//! the workload runs on both. The harness reports median wall time per engine
//! and asserts row-set equality, so it doubles as a differential correctness check.
//!
//! Dataset sizes and repetition counts come from environment variables; see
//! `Config::from_env` for the knobs and their defaults.

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

struct Config {
    /// Person node count.
    nodes: u64,
    /// KNOWS edge count (distinct (src, dst) pairs, no self-loops).
    edges: u64,
    /// Timed repetitions per query; the median is reported.
    reps: usize,
    /// Untimed warmup runs per query.
    warmups: usize,
}

impl Config {
    fn from_env() -> Self {
        fn var(name: &str, default: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        Config {
            nodes: var("LADYBUG_COMPARE_NODES", 10_000),
            edges: var("LADYBUG_COMPARE_EDGES", 50_000),
            reps: var("LADYBUG_COMPARE_REPS", 10) as usize,
            warmups: var("LADYBUG_COMPARE_WARMUPS", 3) as usize,
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
}

struct Dataset {
    /// (id, name, age, city)
    persons: Vec<(u64, String, u64, &'static str)>,
    /// (src id, dst id, weight)
    knows: Vec<(u64, u64, f64)>,
}

fn generate(cfg: &Config) -> Dataset {
    let mut rng = Lcg(0x1554_4ED1);
    let persons = (0..cfg.nodes)
        .map(|id| {
            (
                id,
                format!("p{id}"),
                18 + id % 50,
                CITIES[(id % CITIES.len() as u64) as usize],
            )
        })
        .collect();

    let mut seen = HashSet::new();
    let mut knows = Vec::with_capacity(cfg.edges as usize);
    while knows.len() < cfg.edges as usize {
        let src = rng.next() % cfg.nodes;
        let dst = rng.next() % cfg.nodes;
        if src == dst || !seen.insert((src, dst)) {
            continue;
        }
        let weight = (rng.next() % 1000) as f64 / 1000.0;
        knows.push((src, dst, weight));
    }
    Dataset { persons, knows }
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
    // Node ids are dense (0..n), so insertion order doubles as the id map.
    let mut node_ids = Vec::with_capacity(data.persons.len());
    for (id, name, age, city) in &data.persons {
        let nid = graph.add_node(
            "Person",
            &serde_json::json!({ "id": id, "name": name, "age": age, "city": city }),
        )?;
        node_ids.push(nid);
    }
    for (src, dst, weight) in &data.knows {
        graph.add_edge(
            node_ids[*src as usize],
            node_ids[*dst as usize],
            "KNOWS",
            &serde_json::json!({ "weight": weight }),
        )?;
    }
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

fn bench(warmups: usize, reps: usize, mut f: impl FnMut()) -> Duration {
    for _ in 0..warmups {
        f();
    }
    let times = (0..reps)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    median(times)
}

fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env();
    println!(
        "dataset: {} Person nodes, {} KNOWS edges; {} reps ({} warmups) per query\n",
        cfg.nodes, cfg.edges, cfg.reps, cfg.warmups
    );

    let data = generate(&cfg);
    let csv_dir = tempfile::tempdir()?;
    write_csvs(&data, csv_dir.path())?;

    // ---- Load both engines, timing each once ------------------------------
    let lb_dir = tempfile::tempdir()?;
    let db = Database::new(lb_dir.path().join("db"), SystemConfig::default())?;
    let mut conn = Connection::new(&db)?;
    let default_threads = conn.get_max_num_threads_for_exec();
    let start = Instant::now();
    load_ladybug(&conn, csv_dir.path())?;
    let lb_load = start.elapsed();

    let is_dir = tempfile::tempdir()?;
    let graph = Graph::open(is_dir.path(), 2)?;
    let start = Instant::now();
    load_issundb(&graph, &data)?;
    let is_load = start.elapsed();

    println!("load: issundb {is_load:?} (per-record inserts), ladybug {lb_load:?} (COPY FROM)\n");

    // ---- Workload ----------------------------------------------------------
    // Every query string is sent verbatim to both engines.
    let probe = cfg.nodes / 2;
    let queries: Vec<(&str, String)> = vec![
        (
            "point_lookup",
            format!("MATCH (p:Person) WHERE p.id = {probe} RETURN p.name AS name"),
        ),
        (
            "two_hop_count",
            format!(
                "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                 WHERE a.id = {probe} RETURN count(c) AS n"
            ),
        ),
        (
            "triangle_count",
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
             RETURN count(a) AS n"
                .to_string(),
        ),
        (
            "agg_over_traversal",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN b.city AS city, count(a) AS n ORDER BY city"
                .to_string(),
        ),
    ];

    println!(
        "{:<20} {:>12} {:>12} {:>14} {:>7}  {}",
        "query", "issundb", "ladybug", "ladybug(1t)", "rows", "diff"
    );
    let mut mismatches = 0;
    for (name, cypher) in &queries {
        let is_time = bench(cfg.warmups, cfg.reps, || {
            graph.query(cypher).unwrap();
        });

        conn.set_max_num_threads_for_exec(default_threads);
        let lb_time = bench(cfg.warmups, cfg.reps, || {
            for _row in conn.query(cypher).unwrap() {}
        });
        conn.set_max_num_threads_for_exec(1);
        let lb_time_1t = bench(cfg.warmups, cfg.reps, || {
            for _row in conn.query(cypher).unwrap() {}
        });

        // Differential check: sorted row sets must match exactly.
        let mut is_rows = issundb_rows(&graph.query(cypher)?);
        let mut lb_rows = ladybug_rows(conn.query(cypher)?);
        is_rows.sort();
        lb_rows.sort();
        let verdict = if is_rows == lb_rows {
            "OK".to_string()
        } else {
            mismatches += 1;
            format!(
                "MISMATCH (issundb {} rows: {:?}..., ladybug {} rows: {:?}...)",
                is_rows.len(),
                is_rows.first(),
                lb_rows.len(),
                lb_rows.first()
            )
        };

        println!(
            "{name:<20} {is_time:>12.2?} {lb_time:>12.2?} {lb_time_1t:>14.2?} {:>7}  {verdict}",
            is_rows.len()
        );
    }

    if mismatches > 0 {
        anyhow::bail!("{mismatches} differential mismatch(es)");
    }
    Ok(())
}
