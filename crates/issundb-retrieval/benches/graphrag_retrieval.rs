//! GraphRAG-modeled hybrid retrieval and context serialization benchmarks.
//!
//! These benchmarks evaluate local search (hybrid retrieval with RRF and
//! weighted sum fusion) and global search (PageRank-based anchor community retrieval),
//! including the serialization of retrieved subgraphs into Markdown context formats.

use std::collections::HashMap;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use issundb_core::{Graph, NodeId};
use issundb_retrieval::{FusionStrategy, HybridRetrieveOptions, Subgraph, retrieve_hybrid};
use issundb_vector::VectorGraphExt;
use serde_json::json;
use tempfile::TempDir;

const DIMS: usize = 128;
const NUM_NODES: usize = 1000;

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }

    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 48) as f64
    }
}

struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    fn new(n: u64, theta: f64) -> Self {
        let mut cdf = Vec::with_capacity(n as usize);
        let mut acc = 0.0;
        for rank in 1..=n {
            acc += 1.0 / (rank as f64).powf(theta);
            cdf.push(acc);
        }
        for v in &mut cdf {
            *v /= acc;
        }
        Zipf { cdf }
    }

    fn sample(&self, u: f64) -> u64 {
        self.cdf.partition_point(|&c| c < u) as u64
    }
}

const TOPICS: [&str; 5] = [
    "neural network deep learning model machine intelligence reinforcement transformer agent layer training",
    "database storage graph database query engine index linear algebra transaction concurrency heed",
    "vector search index embedding similarity cosine distance metric quantization hnsw usearch recall",
    "full text search tokenization stemming inversion term document tfidf bm25 posting ranking",
    "distributed system consensus coordination partitioning replica fault tolerance network protocol",
];

/// Build a deterministic GraphRAG-style knowledge graph.
fn build_graphrag_graph() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(dir.path(), 1).unwrap();

    // 1. Create nodes with structured topic text and perturbed vector embeddings.
    let mut nodes = Vec::with_capacity(NUM_NODES);
    for i in 0..NUM_NODES {
        let topic_idx = i % 5;
        let words: Vec<&str> = TOPICS[topic_idx].split_whitespace().collect();
        // Generate a synthetic document body by repeating topic words
        let body = format!(
            "{} {} {} {} {} {}",
            words[i % words.len()],
            words[(i + 1) % words.len()],
            words[(i + 2) % words.len()],
            words[(i + 3) % words.len()],
            words[(i + 4) % words.len()],
            words[(i + 5) % words.len()]
        );
        let nid = graph.add_node("Entity", &json!({ "body": body })).unwrap();

        // Topic-based base vector + small perturbation
        let mut vec = vec![0.0_f32; DIMS];
        vec[topic_idx * 20] = 1.0_f32;
        vec[topic_idx * 20 + 10] = 0.5_f32;
        let mut lcg = Lcg::new((i + 1) as u64);
        for d in 0..DIMS {
            vec[d] += (lcg.unit() as f32 - 0.5) * 0.1;
        }
        graph.upsert_vector(nid, &vec).unwrap();
        nodes.push(nid);
    }

    // 2. Add RELATED edges using a Zipf distribution (scale-free community structure)
    let zipf = Zipf::new(NUM_NODES as u64, 0.8);
    let mut rng = Lcg::new(0x2F7B_19D5);
    let mut seen = std::collections::HashSet::new();
    while seen.len() < 3000 {
        let src = zipf.sample(rng.unit()) as usize;
        let dst = zipf.sample(rng.unit()) as usize;
        if src == dst || src >= NUM_NODES || dst >= NUM_NODES {
            continue;
        }
        // 70% chance to reject cross-topic links to preserve community clusters
        if (src % 5) != (dst % 5) && rng.unit() < 0.7 {
            continue;
        }
        if seen.insert((src, dst)) {
            graph
                .add_edge(nodes[src], nodes[dst], "RELATED", &json!({}))
                .unwrap();
        }
    }

    // 3. Create full-text index on the Entity nodes
    graph.create_node_text_index("Entity", "body").unwrap();
    graph.rebuild_csr().unwrap();

    (dir, graph)
}

/// Serialize a retrieved Subgraph into Markdown.
fn serialize_subgraph(graph: &Graph, subgraph: &Subgraph) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("# Subgraph Context\n\n");
    out.push_str("## Nodes\n");
    for &nid in &subgraph.nodes {
        let label = graph.node_labels(nid).unwrap_or_default().join(", ");
        let body = match graph.node_prop_json(nid, "body") {
            Ok(Some(serde_json::Value::String(s))) => s,
            _ => "No content".to_string(),
        };
        let score = subgraph
            .scores
            .get(&nid)
            .map(|s| format!(" [score: {:.4}]", s))
            .unwrap_or_default();
        out.push_str(&format!("- Node {nid} ({label}){score}: \"{body}\"\n"));
    }
    out.push_str("\n## Relationships\n");
    for &eid in &subgraph.edges {
        if let Ok(Some(record)) = graph.get_edge(eid) {
            let label = graph
                .type_name(record.edge_type)
                .unwrap_or_default()
                .unwrap_or_else(|| "RELATED".to_string());
            out.push_str(&format!(
                "- Node {} -[:{}]-> Node {}\n",
                record.src, label, record.dst
            ));
        }
    }
    out
}

fn bench_local_search_rrf(c: &mut Criterion) {
    let (_dir, graph) = build_graphrag_graph();
    // Perturbed vector for Topic 1 (databases)
    let mut query_vec = vec![0.0_f32; DIMS];
    query_vec[1 * 20] = 1.0_f32;
    query_vec[1 * 20 + 10] = 0.5_f32;
    let query_text = "database graph query engine";

    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Entity".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::Rrf { k: 60 },
        ..Default::default()
    };

    c.bench_function("graphrag_local_search_rrf", |b| {
        b.iter(|| {
            let sub = retrieve_hybrid(
                black_box(&graph),
                black_box(&query_vec),
                black_box(query_text),
                black_box(&opts),
            )
            .unwrap();
            let context = serialize_subgraph(black_box(&graph), &sub);
            black_box(context);
        });
    });
}

fn bench_local_search_weighted(c: &mut Criterion) {
    let (_dir, graph) = build_graphrag_graph();
    // Perturbed vector for Topic 2 (vector search)
    let mut query_vec = vec![0.0_f32; DIMS];
    query_vec[2 * 20] = 1.0_f32;
    query_vec[2 * 20 + 10] = 0.5_f32;
    let query_text = "vector search similarity cosine";

    let opts = HybridRetrieveOptions {
        vector_k: 10,
        text_k: 10,
        text_label: Some("Entity".to_string()),
        text_property: Some("body".to_string()),
        hops: 2,
        fusion: FusionStrategy::WeightedSum {
            vector_weight: 0.6,
            text_weight: 0.4,
        },
        ..Default::default()
    };

    c.bench_function("graphrag_local_search_weighted", |b| {
        b.iter(|| {
            let sub = retrieve_hybrid(
                black_box(&graph),
                black_box(&query_vec),
                black_box(query_text),
                black_box(&opts),
            )
            .unwrap();
            let context = serialize_subgraph(black_box(&graph), &sub);
            black_box(context);
        });
    });
}

fn bench_global_search_pagerank(c: &mut Criterion) {
    let (_dir, graph) = build_graphrag_graph();

    c.bench_function("graphrag_global_search_pagerank", |b| {
        b.iter(|| {
            // 1. Identify global community anchors using PageRank
            let pr = graph.page_rank(10, 0.85).unwrap();
            let mut sorted_nodes: Vec<NodeId> = pr.keys().copied().collect();
            sorted_nodes.sort_by(|x, y| pr[y].partial_cmp(&pr[x]).unwrap());

            // Take the top 5 central entities
            let anchors = &sorted_nodes[0..5];

            // 2. Expand their 1-hop neighborhoods
            let node_list = graph.bfs_multi_source_graphblas(anchors, 1, None).unwrap();
            let node_set: std::collections::HashSet<NodeId> = node_list.into_iter().collect();

            // 3. Extract subgraph edges connecting those nodes
            let mut edges = Vec::new();
            for &nid in &node_set {
                if let Ok(neighbors) = graph.out_neighbors(nid) {
                    for ne in neighbors {
                        if node_set.contains(&ne.node) {
                            edges.push(ne.edge);
                        }
                    }
                }
            }

            // 4. Construct subgraph scores using PageRank
            let mut scores = HashMap::new();
            for &nid in &node_set {
                if let Some(&score) = pr.get(&nid) {
                    scores.insert(nid, score);
                }
            }

            let sub = Subgraph {
                nodes: node_set.into_iter().collect(),
                edges,
                scores,
            };

            // 5. Serialize global context
            let context = serialize_subgraph(black_box(&graph), &sub);
            black_box(context);
        });
    });
}

criterion_group!(
    benches,
    bench_local_search_rrf,
    bench_local_search_weighted,
    bench_global_search_pagerank,
);
criterion_main!(benches);
