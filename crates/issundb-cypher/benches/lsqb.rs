//! LSQB-modeled subgraph pattern matching benchmarks for the Cypher engine.
//!
//! These benchmarks evaluate multi-way join performance, multi-hop chains,
//! and filtering over a synthetic social network containing Person, City,
//! Post, and Comment nodes.

use std::collections::HashMap;

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_core::Graph;
use issundb_cypher::{QueryResult, execute};
use serde_json::json;
use tempfile::TempDir;

const NUM_PERSONS: usize = 1000;
const NUM_CITIES: usize = 5;
const NUM_POSTS: usize = 1000;
const NUM_COMMENTS: usize = 1500;
const Q7_QUERY: &str = "MATCH (post:PostWithComments) \
    OPTIONAL MATCH (post)-[:HAS_COMMENT]->(comment:Comment) \
    OPTIONAL MATCH (comment)-[:HAS_CREATOR]->(commenter:Person) \
    RETURN count(*)";
const Q7_VALIDATION_QUERY: &str = "MATCH (post:PostWithComments) \
    OPTIONAL MATCH (post)-[:HAS_COMMENT]->(comment:Comment) \
    OPTIONAL MATCH (comment)-[:HAS_CREATOR]->(commenter:Person) \
    RETURN post.title, comment.content, commenter.name";

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

    // 7. Add directed triangles and reciprocal pairs. These make cycle and
    // endpoint-inequality predicates exercise both matching and rejected rows.
    for base in (700..997).step_by(3) {
        g.add_edge(persons[base + 2], persons[base], "KNOWS", &json!({}))
            .unwrap();
    }
    for base in 600..650 {
        g.add_edge(persons[base + 1], persons[base], "KNOWS", &json!({}))
            .unwrap();
    }

    // 8. Connect Posts with HAS_CREATOR to Persons.
    for i in 0..NUM_POSTS {
        g.add_edge(
            posts[i],
            persons[i % NUM_PERSONS],
            "HAS_CREATOR",
            &json!({}),
        )
        .unwrap();
    }

    // 9. Create three comment cohorts: known creators, unknown creators, and
    // comments without an author. Posts 1 through 502 form the Q7 anchor set.
    for i in 0..500 {
        g.add_edge(comments[i], persons[i], "HAS_CREATOR", &json!({}))
            .unwrap();
        g.add_edge(comments[i], posts[i + 1], "REPLY_OF", &json!({}))
            .unwrap();
        g.add_edge(posts[i + 1], comments[i], "HAS_COMMENT", &json!({}))
            .unwrap();
    }
    for i in 0..500 {
        let comment = comments[500 + i];
        g.add_edge(comment, persons[i], "HAS_CREATOR", &json!({}))
            .unwrap();
        g.add_edge(comment, posts[i + 2], "REPLY_OF", &json!({}))
            .unwrap();
        g.add_edge(posts[i + 2], comment, "HAS_COMMENT", &json!({}))
            .unwrap();
    }
    for i in 0..500 {
        g.add_edge(comments[1000 + i], posts[i + 3], "REPLY_OF", &json!({}))
            .unwrap();
        g.add_edge(posts[i + 3], comments[1000 + i], "HAS_COMMENT", &json!({}))
            .unwrap();
    }

    for post in &posts[1..503] {
        g.add_label(*post, "PostWithComments").unwrap();
    }

    g.rebuild_csr().unwrap();
    (dir, g)
}

fn count_result(result: &QueryResult) -> u64 {
    assert_eq!(result.records.len(), 1, "count query must return one row");
    result.records[0].values[0]
        .as_u64()
        .expect("count query must return an unsigned integer")
}

fn validate_lsqb_fixture(graph: &Graph, params: &HashMap<String, serde_json::Value>) {
    let optional = execute(graph, Q7_VALIDATION_QUERY, params).unwrap();
    assert_eq!(optional.records.len(), NUM_COMMENTS);
    assert_eq!(
        count_result(&execute(graph, Q7_QUERY, params).unwrap()),
        NUM_COMMENTS as u64
    );
    let missing_comments = optional
        .records
        .iter()
        .filter(|record| record.values[1].is_null())
        .count();
    let missing_commenters = optional
        .records
        .iter()
        .filter(|record| record.values[2].is_null())
        .count();
    assert_eq!(missing_comments, 0);
    assert!(
        missing_commenters > 0 && missing_commenters < optional.records.len(),
        "the commenter OPTIONAL MATCH must produce both matched and null rows"
    );

    let all_paths = execute(
        graph,
        "MATCH (p1:Person)-[:KNOWS]->(p2:Person) \
         MATCH (p2)-[:KNOWS]->(p3:Person) RETURN count(*)",
        params,
    )
    .unwrap();
    let unequal_paths = execute(
        graph,
        "MATCH (p1:Person)-[:KNOWS]->(p2:Person) \
         MATCH (p2)-[:KNOWS]->(p3:Person) \
         WHERE p1.name <> p3.name RETURN count(*)",
        params,
    )
    .unwrap();
    let all_count = count_result(&all_paths);
    let unequal_count = count_result(&unequal_paths);
    assert!(unequal_count > 0 && unequal_count < all_count);
}

fn bench_lsqb(c: &mut Criterion) {
    let (_dir, g) = build_lsqb_graph();
    let params: HashMap<String, serde_json::Value> = HashMap::new();
    validate_lsqb_fixture(&g, &params);

    let mut run = |name: &str, query: &'static str| {
        // Sanity-check the query executes before timing it.
        let result = execute(&g, query, &params).unwrap();
        assert!(
            count_result(&result) > 0,
            "{name} fixture must produce at least one match"
        );
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

    // LSQB Q2: Long chain (3-hop KNOWS path)
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

    // LSQB Q8: Pattern Negation via Pattern Comprehension (Comment-REPLY_OF-Post where commenter does not know creator)
    run(
        "lsqb_q8",
        "MATCH (comment:Comment)-[:REPLY_OF]->(post:Post) \
         MATCH (comment)-[:HAS_CREATOR]->(commenter:Person) \
         MATCH (post)-[:HAS_CREATOR]->(creator:Person) \
         WHERE size([ (commenter)-[:KNOWS]->(knownCreator) WHERE knownCreator.name = creator.name | 1 ]) = 0 AND commenter.name <> creator.name \
         RETURN count(*)",
    );

    // LSQB Q9: Triangle Negation via Pattern Comprehension (Person-KNOWS-Person-KNOWS-Person where p1 does not know p3)
    run(
        "lsqb_q9",
        "MATCH (person1:Person)-[:KNOWS]->(person2:Person) \
         MATCH (person2)-[:KNOWS]->(person3:Person) \
         WHERE size([ (person1)-[:KNOWS]->(knownThird) WHERE knownThird.name = person3.name | 1 ]) = 0 AND person1.name <> person3.name \
         RETURN count(*)",
    );

    drop(run);
    c.bench_function("lsqb_q7", |b| {
        b.iter(|| {
            criterion::black_box(execute(&g, criterion::black_box(Q7_QUERY), &params).unwrap())
        });
    });
}

criterion_group!(benches, bench_lsqb);
criterion_main!(benches);
