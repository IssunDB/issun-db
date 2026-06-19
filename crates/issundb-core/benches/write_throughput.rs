//! Ingestion and write throughput benchmarks for the storage engine.
//!
//! These benchmarks measure the performance of single-record auto-commit inserts
//! versus large batched commits (inserting multiple records within a single transaction)
//! for both nodes and edges.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use serde_json::json;
use tempfile::TempDir;

const BATCH_SIZE: usize = 1000;

fn open_graph() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();
    (dir, graph)
}

fn bench_single_insert(c: &mut Criterion) {
    c.bench_function("write_single_node_autocommit", |b| {
        b.iter_batched(
            open_graph,
            |(dir, graph)| {
                black_box(
                    graph
                        .add_node("Person", &json!({ "name": "person", "age": 30 }))
                        .unwrap(),
                );
                (dir, graph)
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("write_single_edge_autocommit", |b| {
        b.iter_batched(
            || {
                let (dir, graph) = open_graph();
                let src = graph.add_node("Person", &json!({ "name": "src" })).unwrap();
                let dst = graph.add_node("Person", &json!({ "name": "dst" })).unwrap();
                graph.rebuild_csr().unwrap();
                (dir, graph, src, dst)
            },
            |(dir, graph, src, dst)| {
                black_box(graph.add_edge(src, dst, "KNOWS", &json!({})).unwrap());
                (dir, graph)
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_batch_insert(c: &mut Criterion) {
    c.bench_function("write_batch_nodes_1000", |b| {
        b.iter_batched(
            open_graph,
            |(dir, graph)| {
                graph
                    .update(|txn| {
                        for i in 0..BATCH_SIZE {
                            let props = json!({ "name": format!("p{i}"), "age": 30 });
                            txn.add_node("Person", &props)?;
                        }
                        Ok(())
                    })
                    .unwrap();
                (dir, graph)
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("write_batch_edges_1000", |b| {
        b.iter_batched(
            || {
                let (dir, graph) = open_graph();
                let nodes = graph
                    .update(|txn| {
                        let mut nodes = Vec::with_capacity(BATCH_SIZE - 1);
                        for i in 0..BATCH_SIZE - 1 {
                            nodes
                                .push(txn.add_node("Person", &json!({ "name": format!("p{i}") }))?);
                        }
                        Ok(nodes)
                    })
                    .unwrap();
                graph.rebuild_csr().unwrap();
                (dir, graph, nodes)
            },
            |(dir, graph, nodes)| {
                graph
                    .update(|txn| {
                        for i in 0..BATCH_SIZE {
                            let src = nodes[i % nodes.len()];
                            let dst = nodes[(i + 1) % nodes.len()];
                            txn.add_edge(src, dst, "KNOWS", &json!({}))?;
                        }
                        Ok(())
                    })
                    .unwrap();
                (dir, graph)
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_single_insert, bench_batch_insert
}
criterion_main!(benches);
