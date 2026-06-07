//! OLTP transactional read query benchmarks for the Cypher engine.
//!
//! These benchmarks evaluate end-to-end latency on transactional query shapes
//! modeled after the LDBC Interactive Short specifications.

use std::collections::HashMap;

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_cypher::execute;
use serde_json::json;
use tempfile::TempDir;

const NUM_PERSONS: usize = 2000;
const NUM_CITIES: usize = 5;
const NUM_POSTS: usize = 1000;

/// Build a deterministic OLTP-style graph.
fn build_oltp_graph() -> (TempDir, Graph) {
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

    // 4. Connect Persons with LIVES_IN to Cities
    for i in 0..NUM_PERSONS {
        g.add_edge(persons[i], cities[i % NUM_CITIES], "LIVES_IN", &json!({}))
            .unwrap();
    }

    // 5. Connect Persons with KNOWS (coprime offsets)
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

    // 6. Give each of the first 50 people 20 posts so IS2 performs a real
    // top-10 selection instead of sorting a single row.
    for i in 0..NUM_POSTS {
        g.add_edge(posts[i], persons[i % 50], "HAS_CREATOR", &json!({}))
            .unwrap();
    }

    g.rebuild_csr().unwrap();
    (dir, g)
}

fn bench_oltp(c: &mut Criterion) {
    let (_dir, g) = build_oltp_graph();

    let mut feed_params = HashMap::new();
    feed_params.insert("name".to_string(), json!("p20"));
    let feed_count = execute(
        &g,
        "MATCH (p:Person) WHERE p.name = $name \
         MATCH (post:Post)-[:HAS_CREATOR]->(p) RETURN count(*)",
        &feed_params,
    )
    .unwrap();
    assert_eq!(feed_count.records[0].values[0].as_u64(), Some(20));

    let mut run =
        |name: &str, query: &'static str, name_param: &'static str, expected_rows: usize| {
            let mut params = HashMap::new();
            params.insert("name".to_string(), json!(name_param));

            // Sanity-check the query executes before timing it.
            let result = execute(&g, query, &params).unwrap();
            assert_eq!(
                result.records.len(),
                expected_rows,
                "{name} fixture returned an unexpected row count"
            );
            if name == "oltp_is2" {
                assert!(result.records.windows(2).all(|rows| {
                    rows[0].values[0].as_str().unwrap() >= rows[1].values[0].as_str().unwrap()
                }));
            }
            c.bench_function(name, |b| {
                b.iter(|| {
                    criterion::black_box(
                        execute(
                            &g,
                            criterion::black_box(query),
                            criterion::black_box(&params),
                        )
                        .unwrap(),
                    )
                });
            });
        };

    // OLTP IS1: Profile Lookup & Location
    run(
        "oltp_is1",
        "MATCH (n:Person) WHERE n.name = $name MATCH (n)-[:LIVES_IN]->(c:City) RETURN n.age, c.name",
        "p20",
        1,
    );

    // OLTP IS2: Recent Feed / Ordered List
    run(
        "oltp_is2",
        "MATCH (p:Person) WHERE p.name = $name MATCH (post:Post)-[:HAS_CREATOR]->(p) RETURN post.title ORDER BY post.title DESC LIMIT 10",
        "p20",
        10,
    );

    // OLTP IS3: Friendships Lookup
    run(
        "oltp_is3",
        "MATCH (p:Person) WHERE p.name = $name MATCH (p)-[:KNOWS]->(friend:Person) RETURN friend.name",
        "p20",
        3,
    );
}

criterion_group!(benches, bench_oltp);
criterion_main!(benches);
