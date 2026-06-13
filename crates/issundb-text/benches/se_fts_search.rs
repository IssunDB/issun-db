//! Full-text search benchmark over the Stack Exchange multi-vector datasets.
//!
//! This benchmark indexes the real `body` text of Stack Exchange posts and
//! measures single-term and multi-term search latency. It is gated on the
//! benchmark data being present:
//!
//! ```text
//! scripts/download_search_datasets.sh
//! ISSUNDB_BENCH_SEARCH_DIR=$(pwd)/data/multi-vector-search \
//!   cargo bench -p issundb-text --bench se_fts_search
//! ```
//!
//! Knobs (environment variables):
//! - `ISSUNDB_BENCH_SEARCH_DIR`: directory holding the parquet files (required;
//!   the benchmark is skipped when unset).
//! - `ISSUNDB_BENCH_SEARCH_DATASET`: `cs` (default), `ds`, or `p`.
//! - `ISSUNDB_BENCH_SEARCH_LIMIT`: maximum rows to index (default 5000).

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_text::{TextGraphExt, TextSearchOptions};
use serde_json::json;
use tempfile::TempDir;

mod se_dataset;
use se_dataset::Row;

/// Build a graph of `Post` nodes with title, body, and tags, indexed on `body`.
fn setup(rows: &[Row]) -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 2).unwrap();
    for row in rows {
        graph
            .add_node(
                "Post",
                &json!({ "title": row.title, "body": row.body, "tags": row.tags }),
            )
            .unwrap();
    }
    graph.create_node_text_index("Post", "body").unwrap();
    (dir, graph)
}

/// Derive a single-term and a multi-term query from the corpus tags so the
/// queries are guaranteed to match the indexed text.
fn queries(rows: &[Row]) -> (String, String) {
    let tags: Vec<&str> = rows
        .iter()
        .flat_map(|r| r.tags.split_whitespace())
        .take(3)
        .collect();
    let single = tags.first().copied().unwrap_or("the").to_string();
    let multi = if tags.is_empty() {
        single.clone()
    } else {
        tags.join(" ")
    };
    (single, multi)
}

fn opts() -> TextSearchOptions {
    TextSearchOptions {
        label: Some("Post".to_string()),
        property: Some("body".to_string()),
        limit: 10,
        ..Default::default()
    }
}

fn bench_se_fts_single_term(c: &mut Criterion) {
    let Some(dir) = se_dataset::data_dir() else {
        eprintln!("se_fts_search: ISSUNDB_BENCH_SEARCH_DIR not set; skipping");
        return;
    };
    let rows = se_dataset::load(&dir);
    let (_dir, graph) = setup(&rows);
    let (single, _) = queries(&rows);
    let opts = opts();

    c.bench_function("se_fts_search_single_term", |b| {
        b.iter(|| {
            criterion::black_box(
                graph
                    .text_search(criterion::black_box(&single), &opts)
                    .unwrap(),
            )
        });
    });
}

fn bench_se_fts_multi_term(c: &mut Criterion) {
    let Some(dir) = se_dataset::data_dir() else {
        return;
    };
    let rows = se_dataset::load(&dir);
    let (_dir, graph) = setup(&rows);
    let (_, multi) = queries(&rows);
    let opts = opts();

    c.bench_function("se_fts_search_multi_term", |b| {
        b.iter(|| {
            criterion::black_box(
                graph
                    .text_search(criterion::black_box(&multi), &opts)
                    .unwrap(),
            )
        });
    });
}

criterion_group!(benches, bench_se_fts_single_term, bench_se_fts_multi_term);
criterion_main!(benches);
