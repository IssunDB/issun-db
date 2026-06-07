//! Ingestion and write throughput benchmarks for the storage engine.
//!
//! These benchmarks measure the performance of single-record auto-commit inserts
//! versus large batched commits (inserting multiple records within a single transaction)
//! for both nodes and edges.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use issundb_core::Graph;
use serde_json::json;
use tempfile::TempDir;

fn bench_single_insert(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();

    let mut node_idx = 0;
    c.bench_function("write_single_node_autocommit", |b| {
        b.iter(|| {
            let label = "Person";
            let props = json!({ "name": format!("p{}", node_idx), "age": 30 });
            node_idx += 1;
            black_box(graph.add_node(label, &props).unwrap());
        });
    });

    let n1 = graph.add_node("Person", &json!({ "name": "n1" })).unwrap();
    let n2 = graph.add_node("Person", &json!({ "name": "n2" })).unwrap();
    c.bench_function("write_single_edge_autocommit", |b| {
        b.iter(|| {
            black_box(graph.add_edge(n1, n2, "KNOWS", &json!({})).unwrap());
        });
    });
}

fn bench_batch_insert(c: &mut Criterion) {
    c.bench_function("write_batch_nodes_1000", |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            let graph = Graph::open(dir.path(), 1).unwrap();

            graph
                .update(|txn| {
                    for i in 0..1000 {
                        let props = json!({ "name": format!("p{}", i), "age": 30 });
                        txn.add_node("Person", &props)?;
                    }
                    Ok(())
                })
                .unwrap();
        });
    });

    c.bench_function("write_batch_edges_1000", |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            let graph = Graph::open(dir.path(), 1).unwrap();

            // Insert nodes first
            let nodes = graph
                .update(|txn| {
                    let mut nodes = Vec::with_capacity(1001);
                    for i in 0..1001 {
                        let props = json!({ "name": format!("p{}", i) });
                        nodes.push(txn.add_node("Person", &props)?);
                    }
                    Ok(nodes)
                })
                .unwrap();

            // Benchmark batched edge writes
            graph
                .update(|txn| {
                    for i in 0..1000 {
                        txn.add_edge(nodes[i], nodes[i + 1], "KNOWS", &json!({}))?;
                    }
                    Ok(())
                })
                .unwrap();
        });
    });
}

criterion_group!(benches, bench_single_insert, bench_batch_insert);
criterion_main!(benches);
