//! LSQB-modeled subgraph pattern matching benchmarks for the Cypher engine.
//!
//! These benchmarks evaluate multi-way join performance, multi-hop chains,
//! and filtering over a synthetic social network containing Person, City,
//! Post, and Comment nodes.

use std::collections::HashMap;

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_cypher::execute;
use serde_json::json;
use tempfile::TempDir;

const NUM_PERSONS: usize = 1000;
const NUM_CITIES: usize = 5;
const NUM_POSTS: usize = 1000;
const NUM_COMMENTS: usize = 2000;

/// Build a deterministic LSQB-style graph.
fn build_lsqb_graph() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();

    // 1. Add Cities
    let cities: Vec<_> = (0..NUM_CITIES)
        .map(|i| {
            g.add_node("City", &json!({ "name": format!("city{i}") }))
                .unwrap()
        })
        .collect();

    // 2. Add Persons
    let persons: Vec<_> = (0..NUM_PERSONS)
        .map(|i| {
            g.add_node(
                "Person",
                &json!({ "name": format!("p{i}"), "age": 18 + (i % 60) }),
            )
            .unwrap()
        })
        .collect();

    // 3. Add Posts
    let posts: Vec<_> = (0..NUM_POSTS)
        .map(|i| {
            g.add_node("Post", &json!({ "title": format!("post{i}") }))
                .unwrap()
        })
        .collect();

    // 4. Add Comments
    let comments: Vec<_> = (0..NUM_COMMENTS)
        .map(|i| {
            g.add_node("Comment", &json!({ "content": format!("comment{i}") }))
                .unwrap()
        })
        .collect();

    // 5. Connect Persons with LIVES_IN to Cities (deterministic)
    for i in 0..NUM_PERSONS {
        g.add_edge(persons[i], cities[i % NUM_CITIES], "LIVES_IN", &json!({}))
            .unwrap();
    }

    // 6. Connect Persons with KNOWS (coprime offsets to form loops/traversals)
    let knows_offsets = [1, 7, 13];
    for i in 0..NUM_PERSONS {
        for off in knows_offsets {
            g.add_edge(
                persons[i],
                persons[(i + off) % NUM_PERSONS],
                "KNOWS",
                &json!({}),
            )
            .unwrap();
        }
    }

    // 7. Connect Posts with HAS_CREATOR to Persons
    for i in 0..NUM_POSTS {
        g.add_edge(
            posts[i],
            persons[i % NUM_PERSONS],
            "HAS_CREATOR",
            &json!({}),
        )
        .unwrap();
    }

    // 8. Connect Comments with HAS_CREATOR to Persons and REPLY_OF to Posts
    for i in 0..NUM_COMMENTS {
        g.add_edge(
            comments[i],
            persons[i % NUM_PERSONS],
            "HAS_CREATOR",
            &json!({}),
        )
        .unwrap();
        g.add_edge(comments[i], posts[i % NUM_POSTS], "REPLY_OF", &json!({}))
            .unwrap();
    }

    g.rebuild_csr().unwrap();
    (dir, g)
}

fn bench_lsqb(c: &mut Criterion) {
    let (_dir, g) = build_lsqb_graph();
    let params: HashMap<String, serde_json::Value> = HashMap::new();

    let mut run = |name: &str, query: &'static str| {
        // Sanity-check the query executes before timing it.
        execute(&g, query, &params).unwrap();
        c.bench_function(name, |b| {
            b.iter(|| {
                criterion::black_box(execute(&g, criterion::black_box(query), &params).unwrap())
            });
        });
    };

    // LSQB Q1: Path + Triangle pattern (Person-KNOWS-Person, Comment HAS_CREATOR & REPLY_OF)
    run(
        "lsqb_q1",
        "MATCH (p1:Person)-[:KNOWS]->(p2:Person) \
         MATCH (comment:Comment)-[:HAS_CREATOR]->(p1) \
         MATCH (comment)-[:REPLY_OF]->(post:Post) \
         MATCH (post)-[:HAS_CREATOR]->(p2) \
         RETURN count(*)",
    );

    // LSQB Q2: Long Chain (3-hop KNOWS path)
    run(
        "lsqb_q2",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         MATCH (b)-[:KNOWS]->(c:Person) \
         MATCH (c)-[:KNOWS]->(d:Person) \
         RETURN count(*)",
    );

    // LSQB Q3: Triangle Cycle (3-node KNOWS cycle)
    run(
        "lsqb_q3",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         MATCH (b)-[:KNOWS]->(c:Person) \
         MATCH (c)-[:KNOWS]->(a) \
         RETURN count(*)",
    );

    // LSQB Q4: Star pattern (Person as the center, connected to Comment, Post, and City)
    run(
        "lsqb_q4",
        "MATCH (p:Person)<-[:HAS_CREATOR]-(c:Comment) \
         MATCH (p)<-[:HAS_CREATOR]-(post:Post) \
         MATCH (p)-[:LIVES_IN]->(city:City) \
         RETURN count(*)",
    );

    // LSQB Q5: Path with filtering (2-hop KNOWS path with age inequalities)
    run(
        "lsqb_q5",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         MATCH (b)-[:KNOWS]->(c:Person) \
         WHERE a.age < b.age AND b.age < c.age \
         RETURN count(*)",
    );

    // LSQB Q6: Multi-hop Join with Inequality (Person-KNOWS-Person-KNOWS-Person LIVES_IN City)
    run(
        "lsqb_q6",
        "MATCH (p1:Person)-[:KNOWS]->(p2:Person) \
         MATCH (p2)-[:KNOWS]->(p3:Person) \
         MATCH (p3)-[:LIVES_IN]->(c:City) \
         WHERE p1.name <> p3.name \
         RETURN count(*)",
    );

    // LSQB Q7: Double OPTIONAL MATCH Join (Post HAS_CREATOR Person, OPTIONAL Comment REPLY_OF, OPTIONAL Comment HAS_CREATOR)
    run(
        "lsqb_q7",
        "MATCH (post:Post)-[:HAS_CREATOR]->(creator:Person) \
         OPTIONAL MATCH (comment:Comment)-[:REPLY_OF]->(post) \
         OPTIONAL MATCH (comment)-[:HAS_CREATOR]->(commenter:Person) \
         RETURN count(*)",
    );
}

criterion_group!(benches, bench_lsqb);
criterion_main!(benches);
