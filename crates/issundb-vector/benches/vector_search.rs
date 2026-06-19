use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_vector::VectorGraphExt;
use serde_json::json;
use tempfile::TempDir;

const DIMS: usize = 128;

/// Insert 100 nodes, upsert a 128-dimensional vector for each one, and return
/// the live `Graph` together with the `TempDir` that must outlive it.
fn setup() -> (TempDir, Graph, Vec<issundb_core::NodeId>) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();

    let mut nodes = Vec::with_capacity(100);
    for i in 0..100_usize {
        let nid = graph.add_node("Item", &json!({})).unwrap();
        let v: Vec<f32> = (0..DIMS).map(|d| (i * DIMS + d) as f32 / 1000.0).collect();
        graph.upsert_vector(nid, &v).unwrap();
        nodes.push(nid);
    }
    (dir, graph, nodes)
}

fn bench_vector_upsert(c: &mut Criterion) {
    let (_dir, graph, nodes) = setup();

    // Build a deterministic vector to upsert repeatedly.
    let new_vec: Vec<f32> = (0..DIMS).map(|d| d as f32 / 500.0).collect();
    let target = nodes[0];

    c.bench_function("vector_upsert_into_100_entry_index", |b| {
        b.iter(|| {
            graph
                .upsert_vector(std::hint::black_box(target), std::hint::black_box(&new_vec))
                .unwrap();
            std::hint::black_box(())
        });
    });
}

fn bench_vector_search_k1(c: &mut Criterion) {
    let (_dir, graph, _nodes) = setup();
    let query: Vec<f32> = vec![1.0_f32; DIMS];

    c.bench_function("vector_search_k1", |b| {
        b.iter(|| {
            std::hint::black_box(
                graph
                    .vector_search(std::hint::black_box(&query), std::hint::black_box(1))
                    .unwrap(),
            )
        });
    });
}

fn bench_vector_search_k10(c: &mut Criterion) {
    let (_dir, graph, _nodes) = setup();
    let query: Vec<f32> = vec![1.0_f32; DIMS];

    c.bench_function("vector_search_k10", |b| {
        b.iter(|| {
            std::hint::black_box(
                graph
                    .vector_search(std::hint::black_box(&query), std::hint::black_box(10))
                    .unwrap(),
            )
        });
    });
}

criterion_group!(
    benches,
    bench_vector_upsert,
    bench_vector_search_k1,
    bench_vector_search_k10,
);
criterion_main!(benches);
