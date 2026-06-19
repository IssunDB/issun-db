//! Vector search benchmark over the Stack Exchange multi-vector datasets.
//!
//! Indexes one projected 768-dimensional embedding per post (body vector by
//! default) and measures k-NN search latency. Gated on the benchmark data:
//!
//! ```text
//! scripts/download_search_datasets.sh
//! ISSUNDB_BENCH_SEARCH_DIR=$(pwd)/data/multi-vector-search \
//!   cargo bench -p issundb-vector --bench se_vector_search
//! ```
//!
//! Knobs: `ISSUNDB_BENCH_SEARCH_DIR` (required), `ISSUNDB_BENCH_SEARCH_DATASET`
//! (`cs`/`ds`/`p`), `ISSUNDB_BENCH_SEARCH_LIMIT`, and `ISSUNDB_BENCH_SEARCH_VEC`
//! (0 = title, 1 = body, 2 = tags).

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_vector::VectorGraphExt;
use serde_json::json;
use tempfile::TempDir;

mod se_dataset;
use se_dataset::Row;

/// Build a graph with one embedding per `Post` node.
fn setup(rows: &[Row]) -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 2).unwrap();
    for row in rows {
        let nid = graph.add_node("Post", &json!({})).unwrap();
        graph.upsert_vector(nid, &row.vec).unwrap();
    }
    (dir, graph)
}

fn bench_se_vector_search(c: &mut Criterion) {
    let Some(dir) = se_dataset::data_dir() else {
        eprintln!("se_vector_search: ISSUNDB_BENCH_SEARCH_DIR not set; skipping");
        return;
    };
    let rows = se_dataset::load(&dir);
    if rows.is_empty() {
        eprintln!("se_vector_search: dataset loaded zero rows; skipping");
        return;
    }
    let (_dir, graph) = setup(&rows);
    // Query with the first post's own vector so the search exercises a real
    // neighborhood rather than an arbitrary point.
    let query = rows[0].vec.clone();

    for k in [10usize, 100] {
        c.bench_function(&format!("se_vector_search_k{k}"), |b| {
            b.iter(|| {
                std::hint::black_box(
                    graph
                        .vector_search(std::hint::black_box(&query), std::hint::black_box(k))
                        .unwrap(),
                )
            });
        });
    }
}

criterion_group!(benches, bench_se_vector_search);
criterion_main!(benches);
