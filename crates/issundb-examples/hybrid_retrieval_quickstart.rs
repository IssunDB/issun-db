//! A self-contained demo of IssunDB hybrid retrieval.
//!
//! This example:
//!   1. Opens a temporary database.
//!   2. Creates six Movie nodes with title, description, and year properties.
//!   3. Connects them with SIMILAR_TO edges.
//!   4. Creates a full-text index on Movie.description.
//!   5. Upserts 4-dimensional float vectors on each node.
//!   6. Runs `retrieve_hybrid` with a query vector and prints the scored subgraph.
//!   7. Runs a Cypher query and prints the result table.
//!   8. Calls `graph.explain(...)` and prints the physical plan.

use issundb::{Graph, GraphQueryExt, HybridRetrieveOptions, VectorGraphExt, retrieve_hybrid};
use serde_json::json;
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- 1. Open a temp database -------------------------------------------
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    println!("IssunDB Hybrid Retrieval Quickstart");
    println!("====================================\n");

    // ---- 2. Create six Movie nodes -----------------------------------------
    let movies = [
        (
            "Inception",
            "A thief who steals corporate secrets through dream-sharing technology",
            2010,
            [0.9_f32, 0.1, 0.2, 0.0],
        ),
        (
            "The Matrix",
            "A computer hacker learns about the true nature of his reality",
            1999,
            [0.8_f32, 0.2, 0.1, 0.1],
        ),
        (
            "Interstellar",
            "A team of explorers travel through a wormhole in space near a black hole",
            2014,
            [0.1_f32, 0.9, 0.3, 0.2],
        ),
        (
            "Gravity",
            "Two astronauts work together to survive after an accident in outer space",
            2013,
            [0.2_f32, 0.8, 0.4, 0.1],
        ),
        (
            "Blade Runner 2049",
            "A young blade runner discovers a secret that leads him on a quest",
            2017,
            [0.7_f32, 0.1, 0.9, 0.3],
        ),
        (
            "Ex Machina",
            "A programmer is selected to participate in a ground-breaking experiment with AI",
            2014,
            [0.6_f32, 0.2, 0.8, 0.4],
        ),
    ];

    let mut node_ids = Vec::new();
    for (title, description, year, _vec) in &movies {
        let id = graph.add_node(
            "Movie",
            &json!({
                "title": title,
                "description": description,
                "year": year,
            }),
        )?;
        node_ids.push(id);
        println!("Created Movie node {:?}: {}", id, title);
    }

    // ---- 3. Connect movies with SIMILAR_TO edges ---------------------------
    // Inception <-> The Matrix (both explore reality themes)
    graph.add_edge(node_ids[0], node_ids[1], "SIMILAR_TO", &json!({}))?;
    graph.add_edge(node_ids[1], node_ids[0], "SIMILAR_TO", &json!({}))?;
    // Interstellar <-> Gravity (both set in space)
    graph.add_edge(node_ids[2], node_ids[3], "SIMILAR_TO", &json!({}))?;
    graph.add_edge(node_ids[3], node_ids[2], "SIMILAR_TO", &json!({}))?;
    // Blade Runner 2049 <-> Ex Machina (both explore AI themes)
    graph.add_edge(node_ids[4], node_ids[5], "SIMILAR_TO", &json!({}))?;
    println!("\nCreated SIMILAR_TO edges between thematically related movies.");

    // ---- 4. Create a full-text index on Movie.description ------------------
    graph.update(|txn| txn.create_node_text_index("Movie", "description"))?;
    println!("Created full-text index on Movie.description.\n");

    // ---- 5. Upsert 4-dimensional vectors on each node ----------------------
    for (i, (_title, _desc, _year, vec)) in movies.iter().enumerate() {
        graph.upsert_vector(node_ids[i], vec.as_slice())?;
    }
    println!("Upserted 4-dimensional embeddings on all 6 Movie nodes.\n");

    // Rebuild CSR so GraphBLAS BFS expansion is current.
    graph.rebuild_csr()?;

    // ---- 6. Run retrieve_hybrid and print the scored subgraph --------------
    println!("--- Hybrid Retrieval (query vector near 'Inception', text: \"space\") ---");
    let query_vec = [0.85_f32, 0.15, 0.2, 0.0];
    let opts = HybridRetrieveOptions {
        vector_k: 3,
        text_k: 3,
        text_label: Some("Movie".into()),
        text_property: Some("description".into()),
        hops: 1,
        ..Default::default()
    };
    let subgraph = retrieve_hybrid(&graph, &query_vec, "space", &opts)?;

    println!("Subgraph nodes ({}):", subgraph.nodes.len());
    let mut scored_nodes: Vec<_> = subgraph
        .nodes
        .iter()
        .map(|&n| {
            let score = subgraph.scores.get(&n).copied();
            (n, score)
        })
        .collect();
    scored_nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (node_id, score) in &scored_nodes {
        if let Ok(Some(record)) = graph.get_node(*node_id) {
            let props: serde_json::Value =
                rmp_serde::from_slice(&record.props).unwrap_or(json!({}));
            let title = props.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(s) = score {
                println!("  {:?}  title={:25}  fused_score={:.4}", node_id, title, s);
            } else {
                println!("  {:?}  title={:25}  (expansion node)", node_id, title);
            }
        }
    }
    println!("Subgraph edges: {}", subgraph.edges.len());

    // ---- 7. Run a Cypher query and print the result table ------------------
    println!("\n--- Cypher: MATCH (m:Movie) RETURN m.title, m.year ORDER BY m.year ---");
    let result = graph.query("MATCH (m:Movie) RETURN m.title, m.year ORDER BY m.year")?;
    println!("{:<35} year", "title");
    println!("{}", "-".repeat(45));
    for record in &result.records {
        let title = record
            .values
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let year = record
            .values
            .get(1)
            .and_then(|v| v.as_i64())
            .map(|y| y.to_string())
            .unwrap_or_else(|| "?".into());
        println!("{:<35} {}", title, year);
    }

    // ---- 8. Explain the same Cypher query and print the plan ---------------
    println!("\n--- Explain Plan ---");
    let plan = graph.explain("MATCH (m:Movie) RETURN m.title, m.year ORDER BY m.year")?;
    println!("{}", plan);

    Ok(())
}
