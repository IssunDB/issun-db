//! A self-contained tour of IssunDB's graph data science API expressed
//! entirely in Cypher: the `CALL issundb.*` built-in procedures and the
//! `issundb.distance.*` and `issundb.similarity.*` scalar functions.
//!
//! Every analytic here is reachable through the Rust `Graph` API directly (see
//! `graph_analytics.rs`), but this example drives them the way a query author
//! would, so the results flow into ordinary `MATCH`, `WHERE`, and `RETURN`
//! clauses without dropping to Rust.
//!
//! This example:
//!   1. Opens a temporary database.
//!   2. Builds a small Person graph with skills (lists), embeddings (vectors),
//!      bios (full-text), and weighted KNOWS relationships.
//!   3. Runs centrality and community procedures (`pageRank`, `degree`,
//!      `communities`, `connectedComponents`).
//!   4. Runs pathfinding procedures (`dijkstra`, `shortestPath`, `triangleCount`).
//!   5. Runs the GraphRAG retrieval procedures (`retrieve.vector`,
//!      `retrieve.hybrid`).
//!   6. Uses the pairwise comparison functions (`distance.cosine`,
//!      `distance.euclidean`, `similarity.jaccard`, `similarity.overlap`).

use issundb::{Graph, GraphQueryExt, VectorGraphExt};
use serde_json::json;
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- 1. Open a temporary database --------------------------------------
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    println!("IssunDB Graph Data Science in Cypher");
    println!("====================================\n");

    // ---- 2. Build a small Person graph -------------------------------------
    // Each person carries a `skills` list (for the set similarity functions), a
    // 2-dimensional `embedding` (upserted as a vector for retrieval and read by
    // the vector distance functions), and a `bio` (for the full-text index).
    let people = [
        (
            "Alice",
            vec!["rust", "graphs", "databases"],
            [0.90_f32, 0.10],
            "builds graph database storage engines in rust",
        ),
        (
            "Bob",
            vec!["rust", "web"],
            [0.80, 0.20],
            "writes web services and rust tooling",
        ),
        (
            "Carol",
            vec!["graphs", "ml", "databases"],
            [0.25, 0.85],
            "researches graph machine learning over databases",
        ),
        (
            "Dave",
            vec!["ml", "python"],
            [0.15, 0.80],
            "trains python machine learning models",
        ),
        (
            "Erin",
            vec!["rust", "systems"],
            [0.88, 0.12],
            "low level rust systems programming",
        ),
        (
            "Frank",
            vec!["design", "ml"],
            [0.30, 0.70],
            "designs machine learning product interfaces",
        ),
    ];

    let mut ids = std::collections::HashMap::new();
    for (name, skills, emb, bio) in &people {
        let id = graph.add_node(
            "Person",
            &json!({ "name": name, "skills": skills, "bio": bio }),
        )?;
        // Store the embedding as the node's vector so the retrieval procedures
        // and the `distance.*` functions can read it from the node.
        graph.upsert_vector(id, emb.as_slice())?;
        ids.insert(*name, id);
    }

    // Weighted, directed KNOWS edges. Alice -> Bob -> Carol -> Alice forms a
    // directed triangle that `triangleCount` will find.
    let knows = [
        ("Alice", "Bob", 1.0),
        ("Bob", "Carol", 2.0),
        ("Carol", "Alice", 1.5),
        ("Carol", "Dave", 1.0),
        ("Dave", "Erin", 2.0),
        ("Erin", "Alice", 3.0),
        ("Frank", "Dave", 1.0),
    ];
    for &(src, dst, weight) in &knows {
        graph.add_edge(ids[src], ids[dst], "KNOWS", &json!({ "weight": weight }))?;
    }

    // Full-text index over the bio property, then materialize the CSR snapshot
    // once after the bulk load.
    graph.update(|txn| txn.create_node_text_index("Person", "bio"))?;
    graph.rebuild_csr()?;
    println!(
        "Loaded {} people, {} KNOWS edges, embeddings, and a full-text index.\n",
        people.len(),
        knows.len()
    );

    // A helper that runs a Cypher query and prints its result table.
    let run = |title: &str, cypher: &str| -> Result<(), Box<dyn std::error::Error>> {
        println!("--- {title} ---");
        println!("  {}", cypher.replace('\n', "\n  "));
        let result = graph.query(cypher)?;
        println!("  => columns: {:?}", result.columns);
        for record in &result.records {
            println!("     {:?}", record.values);
        }
        println!();
        Ok(())
    };

    // ---- 3. Centrality and community procedures ----------------------------
    // A procedure's YIELD columns flow into the rest of the query: here the
    // yielded `nodeId` is joined back to the Person nodes to recover names.
    run(
        "PageRank influence (CALL issundb.pageRank)",
        "CALL issundb.pageRank({iterations: 20, damping: 0.85}) YIELD nodeId, score \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.name AS name, score ORDER BY score DESC",
    )?;

    run(
        "Out-degree centrality (CALL issundb.degree)",
        "CALL issundb.degree({direction: 'OUT'}) YIELD nodeId, score \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.name AS name, score AS outDegree ORDER BY outDegree DESC, name",
    )?;

    run(
        "Communities, top 2 per community (CALL issundb.communities)",
        "CALL issundb.communities({topPerCommunity: 2}) YIELD communityId, nodeId, rank \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN communityId, rank, p.name AS name ORDER BY communityId, rank",
    )?;

    run(
        "Connected components (CALL issundb.connectedComponents)",
        "CALL issundb.connectedComponents() YIELD nodeId, componentId \
         RETURN componentId, count(*) AS members ORDER BY componentId",
    )?;

    // ---- 4. Pathfinding procedures -----------------------------------------
    // The path procedures take concrete node ids, so format them into the query.
    let dijkstra = format!(
        "CALL issundb.dijkstra({}, {}) YIELD index, nodeId, totalWeight \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN index, p.name AS name, totalWeight ORDER BY index",
        ids["Bob"], ids["Erin"]
    );
    run(
        "Weighted shortest path Bob -> Erin (CALL issundb.dijkstra)",
        &dijkstra,
    )?;

    run(
        "Directed triangle count over KNOWS (CALL issundb.triangleCount)",
        "CALL issundb.triangleCount({relTypes: ['KNOWS', 'KNOWS', 'KNOWS']}) YIELD count \
         RETURN count",
    )?;

    // ---- 5. GraphRAG retrieval procedures ----------------------------------
    // Vector retrieval seeds on nearest embeddings then expands `hops` outward;
    // expansion-only nodes carry a null distance.
    run(
        "Vector retrieval near [0.88, 0.12] (CALL issundb.retrieve.vector)",
        "CALL issundb.retrieve.vector([0.88, 0.12], {k: 2, hops: 1}) YIELD nodeId, distance \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.name AS name, distance ORDER BY distance IS NULL, distance",
    )?;

    // Hybrid retrieval fuses the vector hits with full-text hits for "machine
    // learning" before expanding.
    run(
        "Hybrid retrieval, vector + text (CALL issundb.retrieve.hybrid)",
        "CALL issundb.retrieve.hybrid([0.20, 0.85], 'machine learning', \
         {vectorK: 2, textK: 2, textLabel: 'Person', textProperty: 'bio', hops: 1}) \
         YIELD nodeId, score \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.name AS name, score ORDER BY score IS NULL, score DESC, name",
    )?;

    // ---- 6. Pairwise comparison functions ----------------------------------
    // Vector measures are distances (a node argument resolves to its embedding);
    // set measures are similarities over list properties. The opposite direction
    // of each measure is a trivial inline expression, shown for cosine.
    run(
        "Pairwise comparisons Alice vs Erin (issundb.distance.* / issundb.similarity.*)",
        "MATCH (a:Person {name: 'Alice'}), (e:Person {name: 'Erin'}) \
         RETURN issundb.distance.cosine(a, e) AS cosineDistance, \
                1 - issundb.distance.cosine(a, e) AS cosineSimilarity, \
                issundb.distance.euclidean(a, e) AS l2Distance, \
                issundb.similarity.jaccard(a.skills, e.skills) AS skillJaccard, \
                issundb.similarity.overlap(a.skills, e.skills) AS skillOverlap",
    )?;

    println!("Done.");
    Ok(())
}
