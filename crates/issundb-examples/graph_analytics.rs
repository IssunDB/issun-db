//! An example that shows the use of IssunDB for graph analytics.
//!
//! This example:
//!   1. Opens a temporary graph database.
//!   2. Populates a synthetic network of software engineers, technologies, and projects.
//!   3. Executes PageRank to determine node influence.
//!   4. Computes degree centrality (in, out, and both directions).
//!   5. Finds the shortest weighted path between two engineers (using Dijkstra's algorithm).
//!   6. Identifies community structures using Label Propagation.
//!   7. Finds connected components within the network.

use issundb::{DegreeDirection, Graph, NodeId};
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open a temporary database
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    println!("IssunDB Graph Analytics Showcase");
    println!("===============================\n");

    // 2. Populate a synthetic social/technology network
    // Let's create some Developer nodes
    let developers = [
        ("Alice", "Rust Developer"),
        ("Bob", "Python Developer"),
        ("Carol", "C++ Developer"),
        ("David", "Go Developer"),
        ("Eva", "Rust Developer"),
        ("Frank", "Java Developer"),
    ];

    let mut dev_ids = HashMap::new();
    for (name, role) in &developers {
        let id = graph.add_node(
            "Developer",
            &json!({
                "name": name,
                "role": role,
            }),
        )?;
        dev_ids.insert(*name, id);
        println!("Created Developer node {:?}: {}", id, name);
    }

    // Let's create some Project nodes
    let projects = [
        ("IssunDB", "Graph Database"),
        ("Mochi-Web", "Web Framework"),
        ("Rusty-CLI", "Command Line Tool"),
    ];

    let mut proj_ids = HashMap::new();
    for (name, desc) in &projects {
        let id = graph.add_node(
            "Project",
            &json!({
                "name": name,
                "description": desc,
            }),
        )?;
        proj_ids.insert(*name, id);
        println!("Created Project node {:?}: {}", id, name);
    }

    // Create COLLABORATES_WITH relationships between developers (undirected simulated with bidirectional edges)
    // Add weights representing collaboration strength (lower weight = closer relationship/faster path)
    let collabs = [
        ("Alice", "Bob", 1.0),
        ("Alice", "Carol", 2.0),
        ("Bob", "Carol", 1.5),
        ("Carol", "David", 3.0),
        ("David", "Eva", 1.0),
        ("Eva", "Alice", 4.0),
    ];

    for &(src, dst, weight) in &collabs {
        let src_id = dev_ids[src];
        let dst_id = dev_ids[dst];

        graph.add_edge(
            src_id,
            dst_id,
            "COLLABORATES_WITH",
            &json!({ "weight": weight }),
        )?;
        graph.add_edge(
            dst_id,
            src_id,
            "COLLABORATES_WITH",
            &json!({ "weight": weight }),
        )?;
    }

    // Create CONTRIBUTES_TO edges from developers to projects with difficulty/weight properties
    let contributions = [
        ("Alice", "IssunDB", 0.5),
        ("Carol", "IssunDB", 1.0),
        ("Eva", "IssunDB", 1.5),
        ("Bob", "Mochi-Web", 1.0),
        ("David", "Rusty-CLI", 2.0),
    ];

    for &(dev, proj, weight) in &contributions {
        let dev_id = dev_ids[dev];
        let proj_id = proj_ids[proj];
        graph.add_edge(
            dev_id,
            proj_id,
            "CONTRIBUTES_TO",
            &json!({ "weight": weight }),
        )?;
    }

    // Manually trigger a CSR rebuild to materialize matrices for GraphBLAS analytics
    graph.rebuild_csr()?;
    println!("\nGraph populated and GraphBLAS CSR matrices materialized.");

    // Helper closure to resolve developer name from NodeId
    let get_node_name = |id: NodeId| -> String {
        if let Ok(Some(rec)) = graph.get_node(id) {
            if let Ok(props) = rmp_serde::from_slice::<serde_json::Value>(&rec.props) {
                if let Some(name) = props.get("name").and_then(|v| v.as_str()) {
                    return name.to_string();
                }
            }
        }
        format!("Node({:?})", id)
    };

    // 3. Compute PageRank
    println!("\n--- 1. PageRank Influence Analysis ---");
    let pageranks = graph.page_rank(20, 0.85)?;
    let mut pr_sorted: Vec<_> = pageranks.into_iter().collect();
    pr_sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (id, rank) in pr_sorted {
        println!("  {:<15} score = {:.6}", get_node_name(id), rank);
    }

    // 4. Compute Degree Centrality
    println!("\n--- 2. Degree Centrality (Both Directions) ---");
    let degrees = graph.degree_centrality(DegreeDirection::Both)?;
    let mut deg_sorted: Vec<_> = degrees.into_iter().collect();
    deg_sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (id, deg) in deg_sorted {
        println!("  {:<15} degree = {}", get_node_name(id), deg);
    }

    // 5. Dijkstra Shortest Paths
    println!("\n--- 3. Dijkstra Shortest Path (Weighted) ---");
    // Find path from Bob to Eva
    let bob_id = dev_ids["Bob"];
    let eva_id = dev_ids["Eva"];
    if let Some(weighted_path) = graph.shortest_path_dijkstra(bob_id, eva_id)? {
        println!(
            "  Shortest path from Bob to Eva (total weight: {:.2}):",
            weighted_path.total_weight
        );
        let path_names: Vec<String> = weighted_path
            .nodes
            .iter()
            .map(|&id| get_node_name(id))
            .collect();
        println!("    Path: {}", path_names.join(" -> "));
    } else {
        println!("  No path found between Bob and Eva.");
    }

    // 6. Label Propagation (Community Detection)
    println!("\n--- 4. Label Propagation Community Detection ---");
    let communities = graph.label_propagation(10)?;
    let mut community_groups: HashMap<u64, Vec<String>> = HashMap::new();
    for (id, community_id) in communities {
        community_groups
            .entry(community_id)
            .or_default()
            .push(get_node_name(id));
    }
    for (comm_id, members) in community_groups {
        println!("  Community {}: {:?}", comm_id, members);
    }

    // 7. Connected Components
    println!("\n--- 5. Connected Components ---");
    let components = graph.connected_components()?;
    let mut component_groups: HashMap<u64, Vec<String>> = HashMap::new();
    for (id, component_id) in components {
        component_groups
            .entry(component_id)
            .or_default()
            .push(get_node_name(id));
    }
    println!("  Total connected components: {}", component_groups.len());
    for (comp_id, members) in component_groups {
        println!("    Component {}: {:?}", comp_id, members);
    }

    Ok(())
}
