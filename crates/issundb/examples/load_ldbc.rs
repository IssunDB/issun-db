//! A minimal LDBC-style social network fixture with graph analytics.
//!
//! This is a hand-crafted subset of an LDBC Social Network Benchmark graph.
//! For the full benchmark dataset, see <https://ldbcouncil.org/benchmarks/snb/>.
//!
//! Steps demonstrated:
//!   1. Creates 8 Person nodes with id, name, and city properties.
//!   2. Creates KNOWS edges forming a connected social graph.
//!   3. Runs PageRank and prints the top-3 nodes by score.
//!   4. Runs connected_components and prints the component count.
//!   5. Runs betweenness_centrality and prints the top-3 nodes.
//!   6. Runs a Cypher BFS-range query from Alice.

use std::collections::HashMap;

use issundb::{Graph, GraphQueryExt, NodeId};
use serde_json::json;
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("LDBC-Style Social Network Analytics");
    println!("====================================\n");

    // ---- 1. Open a temp database and create 8 Person nodes -----------------
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    let people = [
        (1u64, "Alice", "London"),
        (2u64, "Bob", "Paris"),
        (3u64, "Carol", "Berlin"),
        (4u64, "David", "London"),
        (5u64, "Eva", "Madrid"),
        (6u64, "Frank", "Rome"),
        (7u64, "Grace", "Amsterdam"),
        (8u64, "Henry", "Brussels"),
    ];

    let mut name_to_id: HashMap<&str, NodeId> = HashMap::new();

    for (id, name, city) in &people {
        let node_id = graph.add_node("Person", &json!({ "id": id, "name": name, "city": city }))?;
        name_to_id.insert(name, node_id);
        println!("Created Person {:?}: {}", node_id, name);
    }

    // ---- 2. Create KNOWS edges ---------------------------------------------
    // The graph forms a connected network with multiple paths.
    let edges = [
        ("Alice", "Bob"),
        ("Alice", "Carol"),
        ("Bob", "David"),
        ("Bob", "Eva"),
        ("Carol", "Frank"),
        ("David", "Grace"),
        ("Eva", "Grace"),
        ("Frank", "Henry"),
        ("Grace", "Henry"),
        // A few back edges to increase connectivity.
        ("Henry", "Alice"),
        ("David", "Carol"),
    ];

    println!("\nCreating KNOWS edges...");
    for (src_name, dst_name) in &edges {
        let src = name_to_id[*src_name];
        let dst = name_to_id[*dst_name];
        graph.add_edge(src, dst, "KNOWS", &json!({}))?;
        println!("  {} -> {}", src_name, dst_name);
    }

    // Rebuild the CSR snapshot so GraphBLAS-backed analytics are up to date.
    graph.rebuild_csr()?;

    // ---- 3. PageRank: top-3 nodes by score ---------------------------------
    println!("\n--- PageRank (20 iterations, damping 0.85) ---");
    let pr_scores = graph.page_rank(20, 0.85)?;
    let mut pr_list: Vec<(NodeId, f32)> = pr_scores.into_iter().collect();
    pr_list.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("{:<5} {:<20} {:.6}", "rank", "name", "score");
    println!("{}", "-".repeat(40));
    for (rank, (node_id, score)) in pr_list.iter().take(3).enumerate() {
        let name = resolve_name(&graph, *node_id);
        println!("{:<5} {:<20} {:.6}", rank + 1, name, score);
    }

    // ---- 4. Connected components -------------------------------------------
    println!("\n--- Connected Components ---");
    let components = graph.connected_components()?;
    let unique_components: std::collections::HashSet<u64> = components.values().copied().collect();
    println!(
        "Total nodes: {}  Distinct components: {}",
        components.len(),
        unique_components.len()
    );
    if unique_components.len() == 1 {
        println!("The graph is fully connected.");
    }

    // ---- 5. Betweenness centrality: top-3 nodes ----------------------------
    println!("\n--- Betweenness Centrality (top 3) ---");
    let bc_scores = graph.betweenness_centrality()?;
    let mut bc_list: Vec<(NodeId, f64)> = bc_scores.into_iter().collect();
    bc_list.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("{:<5} {:<20} {:.4}", "rank", "name", "centrality");
    println!("{}", "-".repeat(40));
    for (rank, (node_id, score)) in bc_list.iter().take(3).enumerate() {
        let name = resolve_name(&graph, *node_id);
        println!("{:<5} {:<20} {:.4}", rank + 1, name, score);
    }

    // ---- 6. Cypher BFS range query from Alice ------------------------------
    println!(
        "\n--- Cypher: MATCH (a:Person {{name: 'Alice'}})-[:KNOWS*1..3]->(b:Person) RETURN DISTINCT b.name ---"
    );
    let result = graph.query(
        "MATCH (a:Person {name: 'Alice'})-[:KNOWS*1..3]->(b:Person) RETURN DISTINCT b.name",
    )?;

    println!("Nodes reachable from Alice within 3 KNOWS hops:");
    for record in &result.records {
        let name = record
            .values
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("  {}", name);
    }
    println!("\nTotal: {} Person nodes reachable.", result.records.len());

    Ok(())
}

/// Resolve a NodeId to a person name via a property lookup.
fn resolve_name(graph: &Graph, node_id: NodeId) -> String {
    graph
        .get_node(node_id)
        .ok()
        .flatten()
        .and_then(|rec| {
            let props: serde_json::Value = rmp_serde::from_slice(&rec.props).ok()?;
            props.get("name")?.as_str().map(|s| s.to_owned())
        })
        .unwrap_or_else(|| format!("{:?}", node_id))
}
