use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_text::{TextGraphExt, TextSearchOptions};
use serde_json::json;
use tempfile::TempDir;

/// Words cycled across documents so each document has a distinct mix.
const WORDS: &[&str] = &[
    "graph",
    "database",
    "storage",
    "query",
    "index",
    "search",
    "node",
    "edge",
    "path",
    "traversal",
];

/// Insert 100 `Article` nodes whose `body` contains varying word combinations,
/// then create the full-text index. Returns the live `Graph` and the `TempDir`
/// (which must outlive the `Graph`).
fn setup() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();

    for i in 0..100_usize {
        // Each document picks four words from the pool, offset by node index.
        let w0 = WORDS[i % WORDS.len()];
        let w1 = WORDS[(i + 1) % WORDS.len()];
        let w2 = WORDS[(i + 2) % WORDS.len()];
        let w3 = WORDS[(i + 3) % WORDS.len()];
        let body = format!("{w0} {w1} {w2} {w3} document number {i}");
        graph.add_node("Article", &json!({ "body": body })).unwrap();
    }

    graph.create_node_text_index("Article", "body").unwrap();
    (dir, graph)
}

fn bench_fts_index_build(c: &mut Criterion) {
    c.bench_function("fts_index_build_100_docs", |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            let graph = Graph::open(dir.path(), 1).unwrap();
            for i in 0..100_usize {
                let w0 = WORDS[i % WORDS.len()];
                let w1 = WORDS[(i + 1) % WORDS.len()];
                let body = format!("{w0} {w1} document {i}");
                graph.add_node("Article", &json!({ "body": body })).unwrap();
            }
            graph.create_node_text_index("Article", "body").unwrap();
            std::hint::black_box(());
            // Hold `dir` alive until end of iteration.
            drop(dir);
        });
    });
}

fn bench_fts_search_single_term(c: &mut Criterion) {
    let (_dir, graph) = setup();
    let opts = TextSearchOptions {
        label: Some("Article".to_string()),
        property: Some("body".to_string()),
        limit: 10,
        ..Default::default()
    };

    c.bench_function("fts_search_single_term", |b| {
        b.iter(|| {
            std::hint::black_box(
                graph
                    .text_search(std::hint::black_box("graph"), &opts)
                    .unwrap(),
            )
        });
    });
}

fn bench_fts_search_multi_term(c: &mut Criterion) {
    let (_dir, graph) = setup();
    let opts = TextSearchOptions {
        label: Some("Article".to_string()),
        property: Some("body".to_string()),
        limit: 10,
        ..Default::default()
    };

    c.bench_function("fts_search_multi_term", |b| {
        b.iter(|| {
            std::hint::black_box(
                graph
                    .text_search(std::hint::black_box("graph database storage"), &opts)
                    .unwrap(),
            )
        });
    });
}

criterion_group!(
    benches,
    bench_fts_index_build,
    bench_fts_search_single_term,
    bench_fts_search_multi_term,
);
criterion_main!(benches);
