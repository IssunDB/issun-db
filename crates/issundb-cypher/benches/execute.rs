//! Execution benchmarks for the Cypher engine.
//!
//! Unlike `parse.rs`, which times only the parser, these benchmarks build a
//! populated graph once and time end-to-end query execution. The query shapes
//! map to the engine's known efficiency pressure points so optimization work
//! can be measured rather than asserted:
//!
//!   - `scan_limit`        bounded scan (exercises limit-into-scan)
//!   - `property_filter`   repeated property access on the same record (decode cache)
//!   - `two_hop`           multi-hop expansion
//!   - `var_length`        variable-length path expansion
//!   - `limit_behind_expand` small LIMIT over a single-hop expand (streaming short-circuit)
//!
//! The last two are Phase-3 gate baselines: query shapes whose plans are not
//! yet streamable, so they still fully materialize. They quantify the cost the
//! deferred streaming work (through joins, and streaming aggregation) would
//! target, so a future change can be measured against them rather than asserted:
//!
//!   - `limit_behind_join` small LIMIT over a HashJoin (no short-circuit today)
//!   - `aggregation`        grouped count over a full single-hop expansion
//!
//! The graph is deterministic (no RNG), so run-to-run comparisons are stable.

use std::collections::HashMap;

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_cypher::execute;
use serde_json::json;
use tempfile::TempDir;

const NUM_NODES: usize = 2000;
/// Out-degree per node; the offsets are coprime-ish with `NUM_NODES` so the
/// KNOWS relation forms cycles and a non-trivial multi-hop fan-out.
const OFFSETS: [usize; 3] = [1, 7, 13];

/// Build a deterministic `Person`-`KNOWS` graph held open for the whole bench.
fn build_graph() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let ids: Vec<_> = (0..NUM_NODES)
        .map(|i| {
            g.add_node(
                "Person",
                &json!({ "name": format!("p{i}"), "age": 18 + (i % 60) }),
            )
            .unwrap()
        })
        .collect();
    for i in 0..NUM_NODES {
        for off in OFFSETS {
            g.add_edge(ids[i], ids[(i + off) % NUM_NODES], "KNOWS", &json!({}))
                .unwrap();
        }
    }
    g.rebuild_csr().unwrap();
    (dir, g)
}

fn bench_execution(c: &mut Criterion) {
    let (_dir, g) = build_graph();
    let params: HashMap<String, serde_json::Value> = HashMap::new();

    let mut run = |name: &str, query: &'static str| {
        // Sanity-check the query executes before timing it.
        execute(&g, query, &params).unwrap();
        c.bench_function(name, |b| {
            b.iter(|| {
                criterion::black_box(execute(&g, criterion::black_box(query), &params).unwrap())
            });
        });
    };

    run("exec_scan_limit", "MATCH (n:Person) RETURN n LIMIT 10");
    run(
        "exec_property_filter",
        "MATCH (n:Person) WHERE n.age > 25 AND n.age < 50 RETURN n.name",
    );
    run(
        "exec_two_hop",
        "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN c LIMIT 100",
    );
    run(
        "exec_var_length",
        "MATCH (a:Person)-[:KNOWS*1..3]->(b) RETURN b LIMIT 100",
    );
    // A small LIMIT over a single-hop expand: the full result is NUM_NODES *
    // |OFFSETS| rows, but streaming stops the scan and expansion after a few
    // source nodes, so this should cost a small fraction of the unbounded scan.
    run(
        "exec_limit_behind_expand",
        "MATCH (a:Person)-[:KNOWS]->(b) RETURN b LIMIT 5",
    );
    // Phase-3 gate: a small LIMIT over a HashJoin (two patterns sharing `b`).
    // The plan is `Limit -> Project -> HashJoin`, which `is_streamable` rejects,
    // so both join inputs materialize in full before the limit applies. Streaming
    // through the join would let this stop early; this records today's cost.
    run(
        "exec_limit_behind_join",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         MATCH (c:Person)-[:KNOWS]->(b) RETURN a, c LIMIT 5",
    );
    // Phase-3 gate: a grouped aggregation over the full single-hop expansion.
    // The Aggregate operator drains its child into a `Vec` before grouping, so
    // every expansion row is materialized. Streaming aggregation would consume
    // batches without holding all rows; this records the materialize-then-group
    // baseline.
    run(
        "exec_aggregation",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) AS c",
    );
}

criterion_group!(benches, bench_execution);
criterion_main!(benches);
