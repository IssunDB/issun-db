# Hybrid Retrieval

IssunDB supports hybrid retrieval pipelines designed for GraphRAG (Retrieval-Augmented Generation) applications.
These pipelines combine vector similarity search, full-text search, and multi-source graph traversal to construct relevant context subgraphs.

## Retrieval Concepts

A hybrid retrieval workflow follows three sequential steps:

1. Seed selection: Query inputs (vector embeddings, text queries, or both) identify initial "seed" nodes using vector indexes and full-text indexes.
2. Score fusion: Relevance scores from different index hits are combined into a single ranking using a configurable fusion strategy.
3. Graph traversal: A multi-source Breadth-First Search (BFS) expands outward from the top-ranked seed nodes to gather neighboring nodes and edges, materializing a self-contained subgraph.

---

## Retrieval Interfaces

Import the retrieval functions and types from the facade:

```rust
use issundb::{retrieve, retrieve_with, retrieve_hybrid};
use issundb::{RetrieveOptions, HybridRetrieveOptions, FusionStrategy, Subgraph};
```

### Vector Search with Expansion

For queries using only vector embeddings, use `retrieve` or `retrieve_with`:

- `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, RetrievalError>`  
  Performs a vector search to find `k` seed nodes, then runs a BFS traversal up to `hops` depth to build the result subgraph.
- `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, RetrievalError>`  
  Allows finer control over seed selection and traversal limits.

#### RetrieveOptions Structure

```rust
pub struct RetrieveOptions {
    /// Number of seed nodes returned by the vector search.
    pub k: usize,
    /// BFS expansion depth from each seed node.
    pub hops: u8,
    /// Maximum cosine distance for a vector hit to qualify as a seed.
    pub max_distance: f32,
    /// Hard cap on the total number of nodes in the returned subgraph.
    pub max_nodes: Option<usize>,
}
```

### Multi-Index Hybrid Search

To combine vector search and full-text search with graph expansion, use `retrieve_hybrid`:

- `retrieve_hybrid(graph: &Graph, q: &[f32], text_query: &str, opts: &HybridRetrieveOptions) -> Result<Subgraph, RetrievalError>`  
  Identifies seeds from both index types, fuses the ranks, and expands the neighborhood using GraphBLAS operations.

#### HybridRetrieveOptions Structure

```rust
pub struct HybridRetrieveOptions {
    /// Number of seed nodes from the vector search. Set to 0 to disable.
    pub vector_k: usize,
    /// Number of seed nodes from the text search. Set to 0 to disable.
    pub text_k: usize,
    /// Label to restrict the text search. None searches all indexed labels.
    pub text_label: Option<String>,
    /// Property to restrict the text search. None searches all indexed properties.
    pub text_property: Option<String>,
    /// BFS expansion depth from each seed.
    pub hops: u8,
    /// Maximum cosine distance for a vector hit to qualify as a seed.
    pub max_distance: f32,
    /// Hard cap on total subgraph nodes.
    pub max_nodes: Option<usize>,
    /// If set, only nodes with this label qualify as vector-search seeds.
    pub vector_label: Option<String>,
    /// Score fusion strategy.
    pub fusion: FusionStrategy,
}
```

---

## Score Fusion Strategies

Relevance scores from different sources are merged using one of the following strategies:

* Reciprocal Rank Fusion (RRF): Merges ranked lists using the reciprocal of the rank: `score = Σ 1 / (k + rank)`. This is the default strategy and is effective when relevance scores have different scales.
* Weighted Linear Combination: Combines raw relevance scores linearly: `score = α * vector_score + β * text_score`. Use this when you want to prioritize one index over another.

```rust
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion. k is a smoothing constant (default: 60).
    Rrf { k: u32 },
    /// Weighted linear combination.
    WeightedSum {
        vector_weight: f32,
        text_weight: f32,
    },
}
```

---

## Integration Example

Below is a self-contained example demonstrating how to configure and execute a hybrid retrieval query in Rust:

```rust
use std::path::Path;
use issundb::{Graph, VectorGraphExt, TextIndexExt, retrieve_hybrid, HybridRetrieveOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let graph = Graph::open(Path::new("./data"), 10)?;

    // Create a full-text search index
    graph.create_text_index("Document", "content")?;

    // Add nodes with properties and vector embeddings
    let id1 = graph.add_node("Document", &serde_json::json!({
        "title": "Rust Storage",
        "content": "ACID transactional storage engine in Rust."
    }))?;
    graph.upsert_vector(id1, &[0.1, 0.8, 0.3])?;

    let id2 = graph.add_node("Document", &serde_json::json!({
        "title": "Vector Search",
        "content": "Vector search integration."
    }))?;
    graph.upsert_vector(id2, &[0.8, 0.2, 0.1])?;

    // Create an edge establishing a relationship
    graph.add_edge(id1, id2, "REFERENCES", &serde_json::json!({}))?;

    // Rebuild the CSR snapshot to prepare traversal matrices
    graph.rebuild_csr()?;

    // Configure hybrid retrieval options
    let opts = HybridRetrieveOptions {
        vector_k: 5,
        text_k: 5,
        text_label: Some("Document".into()),
        text_property: Some("content".into()),
        hops: 1,
        ..Default::default()
    };

    // Run hybrid retrieval (query vector and query text)
    let query_vector = vec![0.15, 0.75, 0.35];
    let subgraph = retrieve_hybrid(&graph, &query_vector, "transactional storage", &opts)?;

    println!("Retrieved subgraph containing {} nodes and {} edges.",
             subgraph.nodes.len(),
             subgraph.edges.len());

    Ok(())
}
```
