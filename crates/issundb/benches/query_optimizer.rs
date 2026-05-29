//! Benchmarks for the query optimizer rules.
//!
//! Each group contrasts a query the optimizer accelerates against a
//! semantically-similar query it does not, on the same data, so the ratio shows
//! the rule's value and guards against regressions:
//!
//! - `reduce_count`: `count(*)` (metadata read) versus `count(n.prop)` (full scan).
//! - `utilize_node_by_id`: `WHERE id(n) = k` (primary-key seek) versus
//!   `WHERE n.seq = k` with no index (full scan).
//! - `select_scan_node`: a directed pattern whose rarer endpoint is at the far
//!   end, so the chain is reversed to scan the small label first.
//! - fused multi-hop chains: a three-hop linear traversal.
//!
//! Run with `cargo bench -p issundb --bench query_optimizer`.

use criterion::{Criterion, criterion_group, criterion_main};
use issundb::{Graph, GraphQueryExt, NodeId};
use serde_json::json;
use std::hint::black_box;
use tempfile::TempDir;

const RARE: usize = 50; // City nodes

// The scan-comparison benches (reduce_count, id_seek) use a large label so the
// O(N) scan cost dominates per-query compile overhead and the constant-time
// optimization is visible. The expansion-heavy benches (scan_selection,
// chain_fusion) use a smaller graph as a latency regression guard.
const LARGE: usize = 50_000;
const SMALL: usize = 5_000;

/// Build a graph with many Person nodes, few City nodes, one KNOWS edge per
/// Person to a City, and a sequential `seq` property on each Person. Held alive
/// by the returned `TempDir`.
fn build_graph(common: usize) -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let cities: Vec<NodeId> = (0..RARE)
        .map(|i| {
            g.add_node("City", &json!({ "name": format!("City{i}") }))
                .unwrap()
        })
        .collect();
    let mut persons = Vec::with_capacity(common);
    for i in 0..common {
        let p = g
            .add_node(
                "Person",
                &json!({ "name": format!("P{i}"), "seq": i as i64 }),
            )
            .unwrap();
        g.add_edge(p, cities[i % RARE], "KNOWS", &json!({}))
            .unwrap();
        persons.push(p);
    }
    g.rebuild_csr().unwrap();
    (dir, g, persons)
}

fn bench_reduce_count(c: &mut Criterion) {
    let (_dir, g, _) = build_graph(LARGE);
    let mut group = c.benchmark_group("reduce_count");
    group.bench_function("count_star_reduced", |b| {
        b.iter(|| black_box(g.query("MATCH (n:Person) RETURN count(*)").unwrap()))
    });
    group.bench_function("count_property_scan", |b| {
        b.iter(|| black_box(g.query("MATCH (n:Person) RETURN count(n.seq)").unwrap()))
    });
    group.finish();
}

fn bench_id_seek(c: &mut Criterion) {
    let (_dir, g, persons) = build_graph(LARGE);
    let mid = persons[persons.len() / 2];
    let mid_seq = (persons.len() / 2) as i64;
    let mut group = c.benchmark_group("id_seek");
    // The optimization: O(1) primary-key seek.
    group.bench_function("id_seek", |b| {
        let q = format!("MATCH (n:Person) WHERE id(n) = {mid} RETURN n");
        b.iter(|| black_box(g.query(&q).unwrap()))
    });
    // What the rule avoids: an `id()` predicate has no property index to fall back
    // on, so without the seek it is a full label scan. A modulo predicate the
    // auto-indexer cannot convert is a faithful O(N) proxy for that path.
    group.bench_function("full_scan_unindexable", |b| {
        let q = format!("MATCH (n:Person) WHERE n.seq % {LARGE} = {mid_seq} RETURN n");
        b.iter(|| black_box(g.query(&q).unwrap()))
    });
    // For context: an equality predicate IssunDB auto-indexes is also fast, so the
    // seek is competitive with an indexed lookup (not a regression).
    group.bench_function("indexed_property_lookup", |b| {
        let q = format!("MATCH (n:Person) WHERE n.seq = {mid_seq} RETURN n");
        b.iter(|| black_box(g.query(&q).unwrap()))
    });
    group.finish();
}

fn bench_scan_selection(c: &mut Criterion) {
    let (_dir, g, _) = build_graph(SMALL);
    let mut group = c.benchmark_group("scan_selection");
    // The optimizer reverses this to scan the 50 City nodes first rather than
    // driving from 5000 Person rows.
    group.bench_function("common_to_rare", |b| {
        b.iter(|| {
            black_box(
                g.query("MATCH (a:Person)-[:KNOWS]->(b:City) RETURN a, b")
                    .unwrap(),
            )
        })
    });
    group.finish();
}

fn bench_chain_fusion(c: &mut Criterion) {
    // A separate chained graph: a -> b -> c across three relationship types.
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let hub = g.add_node("Hub", &json!({})).unwrap();
    for _ in 0..SMALL {
        let a = g.add_node("A", &json!({})).unwrap();
        let b = g.add_node("B", &json!({})).unwrap();
        let cc = g.add_node("C", &json!({})).unwrap();
        g.add_edge(a, b, "R1", &json!({})).unwrap();
        g.add_edge(b, cc, "R2", &json!({})).unwrap();
        g.add_edge(cc, hub, "R3", &json!({})).unwrap();
    }
    g.rebuild_csr().unwrap();
    let mut group = c.benchmark_group("chain_fusion");
    group.bench_function("three_hop", |b| {
        b.iter(|| {
            black_box(
                g.query("MATCH (a:A)-[:R1]->(x)-[:R2]->(y)-[:R3]->(z:Hub) RETURN a, z")
                    .unwrap(),
            )
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_reduce_count,
    bench_id_seek,
    bench_scan_selection,
    bench_chain_fusion
);
criterion_main!(benches);
