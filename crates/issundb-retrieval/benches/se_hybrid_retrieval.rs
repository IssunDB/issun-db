//! Hybrid retrieval benchmark over the Stack Exchange multi-vector datasets.
//!
//! Builds one graph where each `Post` node carries both its `body` text (full-
//! text indexed) and one projected embedding (vector indexed), with `RELATED`
//! edges chaining posts that share a primary tag so two-hop expansion has real
//! community structure. Measures fused vector + text + traversal retrieval.
//!
//! ```text
//! scripts/download_search_datasets.sh
//! ISSUNDB_BENCH_SEARCH_DIR=$(pwd)/data/multi-vector-search \
//!   cargo bench -p issundb-retrieval --bench se_hybrid_retrieval
//! ```
//!
//! Knobs: `ISSUNDB_BENCH_SEARCH_DIR` (required), `ISSUNDB_BENCH_SEARCH_DATASET`
//! (`cs`/`ds`/`p`), `ISSUNDB_BENCH_SEARCH_LIMIT`, and `ISSUNDB_BENCH_SEARCH_VEC`.

use std::collections::HashMap;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use issundb_core::{Graph, NodeId};
use issundb_retrieval::{FusionStrategy, HybridRetrieveOptions, retrieve_hybrid};
use issundb_vector::VectorGraphExt;
use serde_json::json;
use tempfile::TempDir;

mod se_dataset;
use se_dataset::Row;

/// Build a graph with body text, embeddings, and shared-tag community edges.
fn setup(rows: &[Row]) -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 2).unwrap();

    let mut ids: Vec<NodeId> = Vec::with_capacity(rows.len());
    for row in rows {
        let nid = graph
            .add_node("Post", &json!({ "body": row.body, "tags": row.tags }))
            .unwrap();
        graph.upsert_vector(nid, &row.vec).unwrap();
        ids.push(nid);
    }

    // Chain posts that share a primary (first) tag, giving topic communities.
    let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let tag = row.tags.split_whitespace().next().unwrap_or("");
        buckets.entry(tag).or_default().push(i);
    }
    for members in buckets.values() {
        for w in members.windows(2) {
            graph
                .add_edge(ids[w[0]], ids[w[1]], "RELATED", &json!({}))
                .unwrap();
        }
    }

    graph.create_node_text_index("Post", "body").unwrap();
    graph.rebuild_csr().unwrap();
    (dir, graph)
}

fn text_query(rows: &[Row]) -> String {
    rows.iter()
        .flat_map(|r| r.tags.split_whitespace())
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn bench_se_hybrid_rrf(c: &mut Criterion) {
    let Some(dir) = se_dataset::data_dir() else {
        eprintln!("se_hybrid_retrieval: ISSUNDB_BENCH_SEARCH_DIR not set; skipping");
        return;
    };
    let rows = se_dataset::load(&dir);
    let (_dir, graph) = setup(&rows);
    let query_vec = rows[0].vec.clone();
    let query_text = text_query(&rows);

    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Post".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::Rrf { k: 60 },
        ..Default::default()
    };

    c.bench_function("se_hybrid_retrieve_rrf", |b| {
        b.iter(|| {
            black_box(
                retrieve_hybrid(
                    black_box(&graph),
                    black_box(&query_vec),
                    black_box(&query_text),
                    black_box(&opts),
                )
                .unwrap(),
            )
        });
    });
}

fn bench_se_hybrid_weighted(c: &mut Criterion) {
    let Some(dir) = se_dataset::data_dir() else {
        return;
    };
    let rows = se_dataset::load(&dir);
    let (_dir, graph) = setup(&rows);
    let query_vec = rows[0].vec.clone();
    let query_text = text_query(&rows);

    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Post".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::WeightedSum {
            vector_weight: 0.6,
            text_weight: 0.4,
        },
        ..Default::default()
    };

    c.bench_function("se_hybrid_retrieve_weighted", |b| {
        b.iter(|| {
            black_box(
                retrieve_hybrid(
                    black_box(&graph),
                    black_box(&query_vec),
                    black_box(&query_text),
                    black_box(&opts),
                )
                .unwrap(),
            )
        });
    });
}

criterion_group!(benches, bench_se_hybrid_rrf, bench_se_hybrid_weighted);
criterion_main!(benches);
