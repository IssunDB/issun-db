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
//!   - `limit_behind_join`  small LIMIT over a HashJoin (probe-side streaming short-circuit)
//!   - `limit_behind_multiway` small LIMIT over a MultiwayJoin closing hop (streaming short-circuit)
//!   - `aggregation`        grouped count whose streamable child is folded a batch at a time
//!   - `order_by_limit`     ORDER BY ... LIMIT (bounded top-N heap)
//!   - `distinct_limit`     small LIMIT over a streaming DISTINCT (short-circuit)
//!
//! `limit_behind_join` streams the probe side of the hash join, so a small
//! `LIMIT` stops the probe scan/expansion early; the build (hash) side is still
//! materialized, which is the floor on the win. `limit_behind_multiway` streams
//! the input of a closing-hop join. It runs over a separate triangle graph
//! because the main graph's forward-only edges form no short cycles.
//!
//! The graphs are deterministic (no RNG), so run-to-run comparisons are stable.

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

/// Build a graph of disjoint directed triangles `(x)->(y)->(z)->(x)`, so a
/// closing-hop pattern (`MultiwayJoin`) has matches. `num_triangles * 3` nodes.
fn build_triangle_graph(num_triangles: usize) -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    for t in 0..num_triangles {
        let a = g
            .add_node("N", &json!({ "name": format!("a{t}") }))
            .unwrap();
        let b = g
            .add_node("N", &json!({ "name": format!("b{t}") }))
            .unwrap();
        let c = g
            .add_node("N", &json!({ "name": format!("c{t}") }))
            .unwrap();
        g.add_edge(a, b, "R", &json!({})).unwrap();
        g.add_edge(b, c, "R", &json!({})).unwrap();
        g.add_edge(c, a, "R", &json!({})).unwrap();
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
                std::hint::black_box(execute(&g, std::hint::black_box(query), &params).unwrap())
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
    // A small LIMIT over a HashJoin (two patterns sharing `b`). The plan is
    // `Limit -> Project -> HashJoin`; the probe side streams, so the limit stops
    // the probe scan/expansion after a few rows. The build (hash) side is still
    // materialized, which is the floor on the saving.
    run(
        "exec_limit_behind_join",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         MATCH (c:Person)-[:KNOWS]->(b) RETURN a, c LIMIT 5",
    );
    // A property-filtered projection over the full single-hop expansion: the
    // columnar fast path runs the range conjuncts as bulk filter stages over
    // the flat id columns instead of evaluating them per row.
    run(
        "exec_filtered_projection",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         WHERE b.age > 25 AND b.age < 50 RETURN b.name AS name",
    );
    // A grouped aggregation over the full single-hop expansion. The Aggregate
    // operator's child chain is streamable, so it folds rows a batch at a time
    // instead of materializing the whole expansion before grouping. Aggregation
    // is still full-consumption (every row is visited), so the win is bounded
    // peak memory, not a short-circuit.
    run(
        "exec_aggregation",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) AS c",
    );
    // ORDER BY ... LIMIT over the full scan: the plan is `Limit -> Sort`, so the
    // sort keeps only `skip + count` rows in a bounded heap instead of sorting
    // and materializing all NUM_NODES. Sort is blocking, so the scan still reads
    // every node; the saving is the sort cost and the full sorted output.
    run(
        "exec_order_by_limit",
        "MATCH (n:Person) RETURN n.age AS age ORDER BY n.age ASC LIMIT 5",
    );
    // A small LIMIT over a streaming DISTINCT: the plan is `Limit -> Project ->
    // Distinct -> ...`, so the limit stops the scan/expansion once enough distinct
    // rows are emitted instead of deduplicating the whole expansion.
    run(
        "exec_distinct_limit",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH DISTINCT b RETURN b LIMIT 5",
    );
    // DISTINCT under ORDER BY ... LIMIT: the plan is `Limit -> Sort ->
    // Distinct -> Project`, so the sort blocks and every row is deduplicated
    // before the limit truncates. The columnar fast path dedups on the
    // gathered projection cells and sorts only the surviving rows.
    run(
        "exec_distinct_order_limit",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         RETURN DISTINCT b.age AS age ORDER BY age ASC LIMIT 5",
    );

    // A small LIMIT over a MultiwayJoin closing hop, on the triangle graph (the
    // main graph's forward-only edges form no short cycles). The plan is
    // `Limit -> Project -> MultiwayJoin`; the input streams, so the limit stops
    // the scan/expansion after a few matches.
    let (_tdir, tg) = build_triangle_graph(666);
    let triangle_query = "MATCH (a:N)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a LIMIT 5";
    execute(&tg, triangle_query, &params).unwrap();
    c.bench_function("exec_limit_behind_multiway", |b| {
        b.iter(|| {
            std::hint::black_box(
                execute(&tg, std::hint::black_box(triangle_query), &params).unwrap(),
            )
        });
    });
}

criterion_group!(benches, bench_execution);
criterion_main!(benches);
