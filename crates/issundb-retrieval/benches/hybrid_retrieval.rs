use criterion::{Criterion, black_box, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_retrieval::{
    FusionStrategy, HybridRetrieveOptions, RetrieveOptions, retrieve_hybrid, retrieve_with,
};
use issundb_vector::VectorGraphExt;
use serde_json::json;
use tempfile::TempDir;

const DIMS: usize = 128;

fn setup() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();

    let mut nodes = Vec::with_capacity(200);
    for i in 0..200_usize {
        let body = format!("database graph index search query tokenization document number {i}");
        let nid = graph.add_node("Article", &json!({ "body": body })).unwrap();

        let v: Vec<f32> = (0..DIMS).map(|d| (i * DIMS + d) as f32 / 1000.0).collect();
        graph.upsert_vector(nid, &v).unwrap();
        nodes.push(nid);
    }

    for i in 0..199 {
        graph
            .add_edge(nodes[i], nodes[i + 1], "RELATED", &json!({}))
            .unwrap();
    }

    graph.create_node_text_index("Article", "body").unwrap();
    graph.rebuild_csr().unwrap();

    (dir, graph)
}

fn bench_retrieve_with(c: &mut Criterion) {
    let (_dir, graph) = setup();
    let query: Vec<f32> = vec![1.0_f32; DIMS];
    let opts = RetrieveOptions {
        k: 10,
        hops: 2,
        ..Default::default()
    };

    c.bench_function("hybrid_retrieve_vector_bfs", |b| {
        b.iter(|| {
            black_box(
                retrieve_with(black_box(&graph), black_box(&query), black_box(&opts)).unwrap(),
            )
        });
    });
}

fn bench_retrieve_hybrid_rrf(c: &mut Criterion) {
    let (_dir, graph) = setup();
    let query: Vec<f32> = vec![1.0_f32; DIMS];
    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Article".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::Rrf { k: 60 },
        ..Default::default()
    };

    c.bench_function("hybrid_retrieve_rrf_vector_fts_bfs", |b| {
        b.iter(|| {
            black_box(
                retrieve_hybrid(
                    black_box(&graph),
                    black_box(&query),
                    black_box("graph database"),
                    black_box(&opts),
                )
                .unwrap(),
            )
        });
    });
}

fn bench_retrieve_hybrid_weighted(c: &mut Criterion) {
    let (_dir, graph) = setup();
    let query: Vec<f32> = vec![1.0_f32; DIMS];
    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Article".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::WeightedSum {
            vector_weight: 0.7,
            text_weight: 0.3,
        },
        ..Default::default()
    };

    c.bench_function("hybrid_retrieve_weighted_vector_fts_bfs", |b| {
        b.iter(|| {
            black_box(
                retrieve_hybrid(
                    black_box(&graph),
                    black_box(&query),
                    black_box("graph database"),
                    black_box(&opts),
                )
                .unwrap(),
            )
        });
    });
}

criterion_group!(
    benches,
    bench_retrieve_with,
    bench_retrieve_hybrid_rrf,
    bench_retrieve_hybrid_weighted,
);
criterion_main!(benches);
