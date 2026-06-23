//! Benchmarks for the high-order, schema-aware optimizer passes on a skewed
//! schema, where node labels sharing a relationship type have very different
//! fan-out and some label/type/label triples never occur. A uniform schema
//! (every label expands at the same rate, every typed hop is realizable) hides
//! the value of these passes, so the fixture here is deliberately skewed:
//!
//! - Many `Person` nodes, a few `City` nodes.
//! - `KNOWS` only ever runs `Person -> Person`; a handful of hub Persons carry a
//!   large `KNOWS` out-degree while the rest carry a small one.
//! - `LIVES_IN` only ever runs `Person -> City`.
//!
//! So the schema realizes `(Person, KNOWS, Person)` and `(Person, LIVES_IN,
//! City)`, but never `(Person, KNOWS, City)`, `(City, KNOWS, _)`, or `(Person,
//! LIVES_IN, Person)`. Three passes exploit that:
//!
//! - `prune_unsatisfiable` (type inference): a typed hop between two labeled
//!   endpoints the schema does not connect is provably empty, so the optimizer
//!   wraps it in a zero-row operator that never touches storage. The
//!   `type_inference` group contrasts a query whose second hop is unsatisfiable
//!   (`Person -KNOWS-> City`) against the same shape over a realizable hop
//!   (`Person -LIVES_IN-> City`): the first returns immediately, the second does
//!   the full expansion the first avoids.
//! - per-source-label expand ratio and multi-hop chaining: the `expand_ratio`
//!   and `multi_hop` groups measure the latency of join-ordering- and
//!   chaining-sensitive queries on the skewed schema as regression guards.
//!
//! Run with `cargo bench -p issundb --bench skewed_schema`.

use criterion::{Criterion, criterion_group, criterion_main};
use issundb::{Graph, GraphQueryExt, NodeId};
use serde_json::json;
use std::hint::black_box;
use tempfile::TempDir;

const PERSONS: usize = 10_000;
const CITIES: usize = 50;
// The first `HUBS` Persons carry a large KNOWS out-degree; the rest carry two.
// This skews the per-source-label fan-out the global `edges / nodes` ratio
// cannot express.
const HUBS: usize = 20;
const HUB_DEGREE: usize = 200;

/// Build the skewed social-and-geography graph described in the module docs.
/// Held alive by the returned `TempDir`.
fn build_graph() -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 2).unwrap();

    let cities: Vec<NodeId> = (0..CITIES)
        .map(|i| {
            g.add_node("City", &json!({ "name": format!("City{i}") }))
                .unwrap()
        })
        .collect();

    let persons: Vec<NodeId> = (0..PERSONS)
        .map(|i| {
            g.add_node(
                "Person",
                &json!({ "name": format!("P{i}"), "seq": i as i64 }),
            )
            .unwrap()
        })
        .collect();

    for (i, &p) in persons.iter().enumerate() {
        // Every Person lives in one City.
        g.add_edge(p, cities[i % CITIES], "LIVES_IN", &json!({}))
            .unwrap();
        // Every Person knows two others; hubs know many.
        let degree = if i < HUBS { HUB_DEGREE } else { 2 };
        for step in 1..=degree {
            let target = persons[(i + step) % PERSONS];
            g.add_edge(p, target, "KNOWS", &json!({})).unwrap();
        }
    }

    g.rebuild_csr().unwrap();
    (dir, g, persons)
}

fn bench_type_inference(c: &mut Criterion) {
    let (_dir, g, _) = build_graph();
    let mut group = c.benchmark_group("type_inference");

    // The second hop `Person -KNOWS-> City` is unrealizable: KNOWS only targets
    // Person. The optimizer proves the whole pattern empty and returns without
    // expanding the first hop. `EXPLAIN` of this query shows a `count=0` Limit.
    group.bench_function("pruned_unsatisfiable", |b| {
        b.iter(|| {
            black_box(
                g.query("MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:City) RETURN c")
                    .unwrap(),
            )
        })
    });

    // The same two-hop shape over a realizable second hop (`Person -LIVES_IN->
    // City`) cannot be pruned, so it does the expansion work the query above
    // avoids: this is the cost the type-inference pass saves.
    group.bench_function("realizable_baseline", |b| {
        b.iter(|| {
            black_box(
                g.query("MATCH (a:Person)-[:KNOWS]->(b:Person)-[:LIVES_IN]->(c:City) RETURN c")
                    .unwrap(),
            )
        })
    });
    group.finish();
}

fn bench_expand_ratio(c: &mut Criterion) {
    let (_dir, g, _) = build_graph();
    let mut group = c.benchmark_group("expand_ratio");

    // Two MATCH clauses sharing the pivot `a`. The per-source-label expand ratio
    // sharpens the cost of each branch so the optimizer drives from the cheaper
    // expansion. Measured as a latency guard for the plan choice.
    group.bench_function("shared_pivot_join", |b| {
        b.iter(|| {
            black_box(
                g.query(
                    "MATCH (a:Person)-[:LIVES_IN]->(c:City) \
                     MATCH (a)-[:KNOWS]->(b:Person) \
                     RETURN a, b, c LIMIT 100",
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

fn bench_multi_hop(c: &mut Criterion) {
    let (_dir, g, _) = build_graph();
    let mut group = c.benchmark_group("multi_hop");

    // A labeled three-hop chain. Multi-hop fan-out chaining applies the
    // per-source-label expand ratio at every hop whose source variable carries a
    // resolvable label, not just the first, so the chain weight reflects the
    // skewed degree. Latency guard.
    group.bench_function("labeled_three_hop", |b| {
        b.iter(|| {
            black_box(
                g.query(
                    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(d:Person) \
                     RETURN a, d LIMIT 100",
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_type_inference,
    bench_expand_ratio,
    bench_multi_hop
);
criterion_main!(benches);
