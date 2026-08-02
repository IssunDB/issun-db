//! Differential coverage for the shape-specific fast paths.
//!
//! The row pipeline is the oracle: it is the one executor that answers every
//! plan, so whatever it returns is what the fast paths owe. Each query here is
//! run twice over the same graph, once as the planner would normally answer it
//! and once with [`crate::exec_mode`] forcing the general path, and the two
//! results must agree.
//!
//! The corpus targets the paths that reproduce MATCH semantics for themselves:
//! the `PathCount`, `GroupedDegree`, and `TriangleCount` kernels in
//! `issundb-core`, the count window pushed into the grouped kernel, the terminal
//! count-collapse and the multi-hop chains in the columnar executor, and the
//! metadata shortcut for a whole-label count. The fixture is deliberately awkward
//! where those paths have to make a decision: parallel edges (so a count is not a
//! distinct-neighbor count), a self-loop (so relationship uniqueness is not
//! vacuous), a node missing the grouped property and one whose value is of another
//! type (so the null and mixed-kind group cases are live), and a second label
//! sharing the relationship types.

use std::collections::HashMap;

use issundb_core::Graph;
use serde_json::json;
use tempfile::TempDir;

use crate::exec_mode::RowPipelineOnly;
use crate::{QueryResult, execute};

/// A graph small enough to reason about by hand, shaped so the counting paths
/// cannot pass by ignoring the hard cases. `Person` nodes are linked by `KNOWS`
/// and `LIKES`; `Robot` shares both types so a label constraint is load-bearing.
fn fixture() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();

    let ada = g
        .add_node(
            "Person",
            &json!({ "name": "ada", "age": 36, "city": "oslo" }),
        )
        .unwrap();
    let bob = g
        .add_node(
            "Person",
            &json!({ "name": "bob", "age": 41, "city": "oslo" }),
        )
        .unwrap();
    let cal = g
        .add_node(
            "Person",
            &json!({ "name": "cal", "age": 29, "city": "bergen" }),
        )
        .unwrap();
    // No `city` at all, and `age` is a string: the null group and the mixed-kind
    // column both have to be handled by whichever path answers.
    let dot = g
        .add_node("Person", &json!({ "name": "dot", "age": "unknown" }))
        .unwrap();
    let eve = g
        .add_node("Robot", &json!({ "name": "eve", "age": 2, "city": "oslo" }))
        .unwrap();

    g.add_edge(ada, bob, "KNOWS", &json!({})).unwrap();
    // Parallel edge: two rows for one pair, so a row count and a distinct-neighbor
    // count differ.
    g.add_edge(ada, bob, "KNOWS", &json!({})).unwrap();
    g.add_edge(bob, cal, "KNOWS", &json!({})).unwrap();
    g.add_edge(cal, ada, "KNOWS", &json!({})).unwrap();
    g.add_edge(cal, dot, "KNOWS", &json!({})).unwrap();
    // Self-loop, so relationship uniqueness over a two-hop chain is not vacuous.
    g.add_edge(bob, bob, "KNOWS", &json!({})).unwrap();
    // A Robot on both ends of the type, so a label constraint has to be applied
    // rather than assumed.
    g.add_edge(eve, ada, "KNOWS", &json!({})).unwrap();
    g.add_edge(ada, eve, "KNOWS", &json!({})).unwrap();

    g.add_edge(ada, cal, "LIKES", &json!({})).unwrap();
    g.add_edge(bob, dot, "LIKES", &json!({})).unwrap();
    g.add_edge(cal, bob, "LIKES", &json!({})).unwrap();
    g.add_edge(dot, ada, "LIKES", &json!({})).unwrap();

    // A triangle closed by a distinct type, for the triangle kernel.
    let t1 = g.add_node("Hub", &json!({ "n": 1 })).unwrap();
    let t2 = g.add_node("Hub", &json!({ "n": 2 })).unwrap();
    let t3 = g.add_node("Hub", &json!({ "n": 3 })).unwrap();
    for (s, d) in [(t1, t2), (t2, t3), (t3, t1), (t1, t3)] {
        g.add_edge(s, d, "LINKS", &json!({})).unwrap();
    }

    (dir, g)
}

fn row_pipeline_execute(graph: &Graph, cypher: &str) -> Result<QueryResult, crate::CypherError> {
    let _guard = RowPipelineOnly::install();
    execute(graph, cypher, &HashMap::new())
}

/// `RETURN *` names its columns by expanding the star over the variables in
/// scope, which only the row pipeline does; the columnar caller derived them by
/// naming each `RETURN` item, so the star sentinel itself became the column name
/// (`__star__()` rather than `agg`). Found by sweeping the openCypher TCK through
/// the row pipeline, where it is `[clauses/with] [5]`.
#[test]
fn return_star_names_its_columns_on_every_path() {
    let (_dir, g) = fixture();
    for cypher in [
        // A projection over a hop, which the columnar path claims.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH a.name AS an, b.name AS bn RETURN *",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH a.name AS an RETURN * ORDER BY an",
        // A grouping-free aggregate, which is where the TCK scenario landed.
        "MATCH (a:Person) WITH count(a) AS total RETURN *",
        "MATCH (a:Person) WITH avg(a.age) + 1 AS shifted RETURN *",
        // A bare scan.
        "MATCH (a:Person) WITH a.name AS n RETURN * ORDER BY n",
    ] {
        assert_paths_agree(&g, cypher);
        let named = {
            // Pinned on for the same reason `assert_paths_agree` pins: under a
            // sweep this would otherwise check the row pipeline's column names,
            // which were never the ones at fault.
            let _guard = crate::exec_mode::fast_paths_required();
            execute(&g, cypher, &HashMap::new()).unwrap()
        };
        assert!(
            named.columns.iter().all(|c| !c.contains("__star__")),
            "the star sentinel leaked into the column names for: {cypher}\n{:?}",
            named.columns
        );
    }
}

/// The same shape over an empty graph, where a grouping-free aggregate must still emit
/// its one row, with the star-expanded column name, on whichever path answers.
#[test]
fn return_star_over_an_empty_graph_agrees() {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let mut params = HashMap::new();
    params.insert("age".to_string(), json!(38));
    let cypher = "MATCH (person) WITH $age + avg(person.age) - 1000 AS agg RETURN *";

    let fast = {
        let _guard = crate::exec_mode::fast_paths_required();
        execute(&g, cypher, &params).unwrap()
    };
    let slow = {
        let _guard = RowPipelineOnly::install();
        execute(&g, cypher, &params).unwrap()
    };

    assert_eq!(
        fast.columns,
        vec!["agg".to_string()],
        "star expansion names the projected variable"
    );
    assert_eq!(fast.columns, slow.columns);
    let rows = |r: &QueryResult| {
        r.records
            .iter()
            .map(|x| x.values.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&fast), vec![vec![serde_json::Value::Null]]);
    assert_eq!(rows(&fast), rows(&slow));
}

/// Compare one query's two answers. Rows are compared as a multiset, because
/// without an `ORDER BY` neither path promises an enumeration order; a query that
/// does sort is additionally compared in order, so the sort itself is covered.
fn assert_paths_agree(graph: &Graph, cypher: &str) {
    let fast = {
        // Pinned on: if the ambient setting were inherited here, a sweep of the
        // suite with the switch on would compare the row pipeline against itself
        // and pass without testing anything.
        let _guard = crate::exec_mode::fast_paths_required();
        execute(graph, cypher, &HashMap::new())
    };
    let slow = row_pipeline_execute(graph, cypher);

    match (fast, slow) {
        (Ok(fast), Ok(slow)) => {
            assert_eq!(fast.columns, slow.columns, "columns for: {cypher}");

            let key = |r: &crate::Record| serde_json::to_string(&r.values).unwrap();
            let mut fast_keys: Vec<String> = fast.records.iter().map(key).collect();
            let mut slow_keys: Vec<String> = slow.records.iter().map(key).collect();
            if cypher.contains("ORDER BY") {
                assert_eq!(fast_keys, slow_keys, "ordered rows for: {cypher}");
            }
            fast_keys.sort();
            slow_keys.sort();
            assert_eq!(fast_keys, slow_keys, "rows for: {cypher}");
        }
        (Err(fast), Err(slow)) => {
            assert_eq!(fast.to_string(), slow.to_string(), "errors for: {cypher}");
        }
        (fast, slow) => panic!("only one path errored for: {cypher}\n{fast:?}\nvs\n{slow:?}"),
    }
}

/// Grouping-free counts, which the `PathCount` kernel claims, plus the whole-label
/// count that reads the stored per-label counter instead of scanning.
#[test]
fn grouping_free_counts_agree_with_the_row_pipeline() {
    let (_dir, g) = fixture();
    for cypher in [
        // Whole-label count from graph metadata against a scan.
        "MATCH (n:Person) RETURN count(*)",
        "MATCH (n:Person) RETURN count(n)",
        // One hop, typed and untyped, labeled on one and both ends.
        "MATCH (a:Person)-[:KNOWS]->(b) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Robot) RETURN count(*)",
        "MATCH (a)-[:KNOWS]->(b) RETURN count(*)",
        // Two hops with distinct types, where the self-loop makes relationship
        // uniqueness observable.
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Person) RETURN count(*)",
        // Two hops of the same type: uniqueness has to exclude the self-loop
        // filling both.
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN count(*)",
        // Per-vertex predicates, which the kernel takes as resolved allow-sets.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.city = 'oslo' RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 AND b.age < 40 RETURN count(*)",
        // A predicate over a property one node lacks entirely.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.city IS NULL RETURN count(*)",
        // Counting a property rather than a row: nulls must not count.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(b.city)",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// Relationship-type alternation, which no counting kernel can express.
///
/// `Expand::rel_type` holds the raw pattern text, so `-[:KNOWS|LIKES]->` reaches
/// a rewrite as the single string `"KNOWS|LIKES"`. A kernel resolves one
/// registered type name, and that string is not one, so every lowered
/// alternation counted zero: `PathCount` and `TriangleCount` answered `0` and
/// `GroupedDegree` returned no rows at all, on patterns whose row form is
/// ordinary openCypher. Each shape below is a count the planner would otherwise
/// hand to a kernel, so this fails if a rewrite starts claiming a multi-type hop
/// again.
#[test]
fn multi_type_counts_agree_with_the_row_pipeline() {
    let (_dir, g) = fixture();
    for cypher in [
        // Grouping-free, the `PathCount` shape, one hop and two.
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) RETURN count(*)",
        "MATCH (a:Person)-[r:KNOWS|LIKES]->(b:Person) RETURN count(r)",
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person)-[:LIKES]->(c:Person) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS|LIKES]->(c:Person) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person)-[:KNOWS|LIKES]->(c:Person) RETURN count(*)",
        // Grouped by an endpoint, the `GroupedDegree` shape, with and without
        // the top-N window that pushes into it.
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) RETURN b.name, count(*) ORDER BY b.name",
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) RETURN a.name, count(*) ORDER BY a.name",
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) RETURN b.name, count(*) AS c \
         ORDER BY c DESC, b.name LIMIT 2",
        // The closing hop of a triangle, the `TriangleCount` shape.
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person)-[:KNOWS|LIKES]->(c:Person)\
         -[:KNOWS|LIKES]->(a) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Person)-[:KNOWS|LIKES]->(a) \
         RETURN count(*)",
        // A filtered multi-type count, so the vertex-allow pushdown is covered too.
        "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) WHERE a.age > 30 RETURN count(*)",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// Every built-in procedure, run through both executors over the same graph.
///
/// A procedure is answered by one kernel rather than by two competing paths, so
/// what this pins is not the kernel's arithmetic (`issundb`'s `tests/oracle.rs`
/// compares those against NetworkX) but everything wrapped around it: argument
/// resolution before planning, the `YIELD` projection, and whatever the
/// optimizer does to the query the `CALL` sits inside. That surrounding shape is
/// exactly where a fast path can claim a plan and answer differently, which is
/// what the multi-type counts above turned out to be, so every procedure is
/// listed rather than a representative few.
///
/// `PROCEDURE_COVERAGE` below is checked against the engine's own dispatch, so a
/// procedure added without a case here fails rather than passing unnoticed.
const PROCEDURE_COVERAGE: &[&str] = &[
    "issundb.pageRank",
    "issundb.betweenness",
    "issundb.harmonic",
    "issundb.closeness",
    "issundb.degree",
    "issundb.eigenvector",
    "issundb.katz",
    "issundb.clusteringCoefficient",
    "issundb.louvain",
    "issundb.labelPropagation",
    "issundb.communities",
    "issundb.connectedComponents",
    "issundb.wcc",
    "issundb.stronglyConnectedComponents",
    "issundb.scc",
    "issundb.shortestPath",
    "issundb.dijkstra",
    "issundb.triangleCount",
    "issundb.retrieve.vector",
    "issundb.retrieve.hybrid",
];

#[test]
fn every_procedure_agrees_across_the_paths() {
    let (_dir, g) = fixture();
    // `ada` is 0 and `dot` is 3: the fixture allocates them in that order, and a
    // procedure argument is resolved before planning, so it has to be a literal.
    for cypher in [
        "CALL issundb.pageRank({iterations: 10, damping: 0.85}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.betweenness() YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.harmonic() YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.closeness() YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.degree({direction: 'OUT'}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.degree({direction: 'IN'}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.degree({direction: 'BOTH'}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.eigenvector({iterations: 20, tolerance: 0.000001}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.katz({alpha: 0.1, beta: 1.0, iterations: 20, tolerance: 0.000001}) \
         YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.clusteringCoefficient() YIELD nodeId, score \
         RETURN nodeId, score ORDER BY nodeId",
        "CALL issundb.louvain() YIELD nodeId, communityId \
         RETURN communityId, count(nodeId) AS n ORDER BY communityId",
        "CALL issundb.labelPropagation({maxIterations: 10}) YIELD nodeId, communityId \
         RETURN communityId, count(nodeId) AS n ORDER BY communityId",
        "CALL issundb.communities({topPerCommunity: 2}) YIELD communityId, nodeId, rank \
         RETURN communityId, rank, nodeId ORDER BY communityId, rank, nodeId",
        "CALL issundb.connectedComponents() YIELD nodeId, componentId \
         RETURN componentId, count(nodeId) AS n ORDER BY componentId",
        "CALL issundb.wcc() YIELD nodeId, componentId \
         RETURN componentId, count(nodeId) AS n ORDER BY componentId",
        "CALL issundb.stronglyConnectedComponents() YIELD nodeId, componentId \
         RETURN componentId, count(nodeId) AS n ORDER BY componentId",
        "CALL issundb.scc() YIELD nodeId, componentId \
         RETURN componentId, count(nodeId) AS n ORDER BY componentId",
        "CALL issundb.shortestPath(0, 3) YIELD nodeId, index RETURN nodeId, index ORDER BY index",
        "CALL issundb.dijkstra(0, 3) YIELD nodeId, index, totalWeight \
         RETURN nodeId, index, totalWeight ORDER BY index",
        "CALL issundb.triangleCount() YIELD count RETURN count",
        // No embeddings and no text index on this fixture, so both of these fail.
        // Included deliberately: the two paths owe the same error, and an error
        // that appeared on one path only would be its own defect.
        "CALL issundb.retrieve.vector([1.0, 0.0, 0.25], {k: 2, hops: 1}) YIELD nodeId, distance \
         RETURN nodeId, distance ORDER BY nodeId",
        "CALL issundb.retrieve.hybrid([1.0, 0.0, 0.25], 'ada', {vectorK: 2, textK: 2, hops: 1}) \
         YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
        // A procedure inside a larger query, where the surrounding shape is what
        // a fast path can claim.
        "CALL issundb.degree({direction: 'OUT'}) YIELD nodeId, score \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.name AS name, score ORDER BY score DESC, name",
        "CALL issundb.pageRank({iterations: 5, damping: 0.85}) YIELD nodeId, score \
         MATCH (p:Person) WHERE id(p) = nodeId \
         RETURN p.city AS city, count(*) AS n ORDER BY city",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// The procedure list above has to keep up with the engine.
///
/// `builtin_procs::build` is what decides whether a name is a built-in, so asking
/// it directly is the one check that cannot drift: a name in the coverage list
/// that the engine has dropped or renamed fails here rather than sitting in the
/// corpus untested. The playground's own catalog is checked separately, by
/// `make playground-check`.
#[test]
fn the_procedure_coverage_list_matches_the_engine() {
    let (_dir, g) = fixture();
    for name in PROCEDURE_COVERAGE {
        // An empty argument list is enough to tell "no such procedure" (`Ok(None)`)
        // from "known, and these arguments are wrong" (`Err`), which is all this
        // asks.
        let resolved = crate::builtin_procs::build(&g, name, &[]);
        assert!(
            !matches!(resolved, Ok(None)),
            "{name} is in the coverage list but the engine does not resolve it",
        );
    }
}

/// Every built-in function, run through both executors.
///
/// The graph-reading functions (the `issundb.link.*` family) are evaluated per
/// row, so the row set the surrounding query produces is what decides how often
/// they run, and that row set is exactly what a fast path rewrites. The pure
/// value functions carry no graph at all and are here for completeness, since a
/// projection over them is still a plan a rewrite can touch.
#[test]
fn every_function_agrees_across_the_paths() {
    let (_dir, g) = fixture();
    for cypher in [
        // Neighborhood link prediction over every ordered pair.
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.commonNeighbors(a, b) AS v \
         ORDER BY a, b",
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.jaccard(a, b) AS v ORDER BY a, b",
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.adamicAdar(a, b) AS v ORDER BY a, b",
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.resourceAllocation(a, b) AS v \
         ORDER BY a, b",
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.preferentialAttachment(a, b) AS v \
         ORDER BY a, b",
        // Sorted and limited, so the top-N shapes see the function too.
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN a.name AS a, b.name AS b, issundb.link.adamicAdar(a, b) AS v \
         ORDER BY v DESC, a, b LIMIT 3",
        // Aggregated over a function, which the columnar aggregate can claim.
        "MATCH (a:Person), (b:Person) WHERE id(a) < id(b) \
         RETURN count(*) AS pairs, sum(issundb.link.commonNeighbors(a, b)) AS total",
        // The pure value functions, projected per row rather than once.
        "MATCH (p:Person) RETURN p.name AS name, \
         issundb.similarity.jaccard([1, 2, 3], [2, 3, 4]) AS v ORDER BY name",
        "MATCH (p:Person) RETURN p.name AS name, \
         issundb.similarity.overlap([1, 2], [1, 2, 3, 4]) AS v ORDER BY name",
        "MATCH (p:Person) RETURN p.name AS name, \
         issundb.distance.cosine([1.0, 0.0], [0.0, 1.0]) AS v ORDER BY name",
        "MATCH (p:Person) RETURN p.name AS name, \
         issundb.distance.euclidean([3.0, 4.0], [0.0, 0.0]) AS v ORDER BY name",
        "MATCH (p:Person) RETURN p.name AS name, \
         vector_dist([1.0, 0.0, 0.25], [0.0, 1.0, 0.25]) AS v ORDER BY name",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// The closed-form functions against hand-computed values.
///
/// Path agreement says the two executors concur; it cannot say they concur on
/// the right number. These have an answer that can be written down, so they are
/// checked against it rather than against each other, on both paths.
///
/// The comparison carries a tolerance because the answers are computed rather
/// than looked up: cosine distance is `1 - dot / (|a| * |b|)`, so two identical
/// vectors give `1 - 0.9999999999999998`, which is `2.2e-16` and not the `0.0` a
/// reader would write down. That is ordinary floating-point rounding rather than
/// a defect, and demanding exact equality would pin the test to the order the
/// arithmetic happens to be written in.
#[test]
fn the_value_functions_return_their_closed_forms() {
    const TOLERANCE: f64 = 1e-12;
    let (_dir, g) = fixture();
    for (cypher, expected) in [
        // |{2,3}| / |{1,2,3,4}|
        (
            "RETURN issundb.similarity.jaccard([1, 2, 3], [2, 3, 4]) AS v",
            0.5,
        ),
        (
            "RETURN issundb.similarity.jaccard([1, 2], [3, 4]) AS v",
            0.0,
        ),
        // |{1,2}| / min(2, 4)
        (
            "RETURN issundb.similarity.overlap([1, 2], [1, 2, 3, 4]) AS v",
            1.0,
        ),
        // Orthogonal unit vectors: cosine distance is 1 - 0.
        (
            "RETURN issundb.distance.cosine([1.0, 0.0], [0.0, 1.0]) AS v",
            1.0,
        ),
        // Identical vectors: no distance between them.
        (
            "RETURN issundb.distance.cosine([1.0, 2.0], [1.0, 2.0]) AS v",
            0.0,
        ),
        // Opposed vectors: the far end of the range.
        (
            "RETURN issundb.distance.cosine([1.0, 0.0], [-1.0, 0.0]) AS v",
            2.0,
        ),
        // The 3-4-5 triangle.
        (
            "RETURN issundb.distance.euclidean([3.0, 4.0], [0.0, 0.0]) AS v",
            5.0,
        ),
        (
            "RETURN issundb.distance.euclidean([0.0, 0.0], [0.0, 0.0]) AS v",
            0.0,
        ),
        // `vector_dist` is the cosine distance under another name, so the same
        // orthogonal pair has to give the same answer.
        ("RETURN vector_dist([1.0, 0.0], [0.0, 1.0]) AS v", 1.0),
    ] {
        for forced_row_pipeline in [false, true] {
            let result = if forced_row_pipeline {
                row_pipeline_execute(&g, cypher)
            } else {
                let _guard = crate::exec_mode::fast_paths_required();
                execute(&g, cypher, &HashMap::new())
            }
            .unwrap_or_else(|e| panic!("{cypher} failed: {e}"));
            let got = result.records[0].values[0]
                .as_f64()
                .unwrap_or_else(|| panic!("{cypher} did not return a number"));
            assert!(
                (got - expected).abs() < TOLERANCE,
                "{cypher} (row pipeline forced: {forced_row_pipeline}): \
                 expected {expected}, got {got}",
            );
        }
    }
}

/// `issundb.triangleCount` against the pattern it lowers from.
///
/// The kernel and the `MATCH` form are two implementations of one definition, so
/// they are each other's oracle: the pattern spelled out row by row is what the
/// count means, and the row pipeline evaluates it without the kernel.
#[test]
fn the_triangle_procedure_agrees_with_its_pattern() {
    let (_dir, g) = fixture();
    let procedure = execute(
        &g,
        "CALL issundb.triangleCount() YIELD count RETURN count",
        &HashMap::new(),
    )
    .unwrap();
    let pattern = row_pipeline_execute(
        &g,
        "MATCH (a)-[t1]->(b)-[t2]->(c)-[t3]->(a) RETURN count(*) AS count",
    )
    .unwrap();
    assert_eq!(
        procedure.records[0].values[0], pattern.records[0].values[0],
        "the triangle kernel and the pattern it lowers from must agree",
    );
}

/// Counts grouped by one endpoint, which the `GroupedDegree` kernel claims, and
/// the top-N window pushed into it.
#[test]
fn grouped_counts_agree_with_the_row_pipeline() {
    let (_dir, g) = fixture();
    for cypher in [
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) ORDER BY b.name",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, count(*) ORDER BY a.name",
        // Grouped by a property one node lacks, so the null group is real.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.city, count(*) ORDER BY b.city",
        // Grouped by a mixed-kind column.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.age, count(*) ORDER BY b.age",
        // count(prop) under a group: the non-null test is per counted endpoint.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(a.city) ORDER BY b.name",
        // The count window: every group reaching the n-th best count survives,
        // boundary ties included.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) AS c ORDER BY c DESC LIMIT 2",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) AS c ORDER BY c DESC LIMIT 1",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*) AS c ORDER BY c ASC LIMIT 2",
        // Grouped count with a filter, so the allow-set and the grouping combine.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name, count(*) ORDER BY b.name",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// The directed triangle the `TriangleCount` kernel claims, with and without the
/// label and type constraints it can carry.
#[test]
fn triangle_counts_agree_with_the_row_pipeline() {
    let (_dir, g) = fixture();
    for cypher in [
        "MATCH (a:Hub)-[:LINKS]->(b:Hub)-[:LINKS]->(c:Hub)-[:LINKS]->(a) RETURN count(*)",
        "MATCH (a)-[:LINKS]->(b)-[:LINKS]->(c)-[:LINKS]->(a) RETURN count(*)",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) RETURN count(*)",
        // Mixed types around the cycle.
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Person)-[:KNOWS]->(a) RETURN count(*)",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// Projections and aggregations the columnar executor claims: multi-hop chains,
/// the terminal count-collapse, and the distinct-and-limit path.
#[test]
fn columnar_projections_agree_with_the_row_pipeline() {
    let (_dir, g) = fixture();
    for cypher in [
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name ORDER BY a.name, b.name",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Person) RETURN a.name, c.name ORDER BY a.name, c.name",
        // Terminal count-collapse: a count over the last variable that feeds no
        // group key.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.city, count(b) ORDER BY a.city",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age > 30 RETURN a.city, count(b) ORDER BY a.city",
        "MATCH (a:Person)-[:KNOWS]->(b:Robot) RETURN a.name, count(b) ORDER BY a.name",
        // Distinct, sort, and limit in the projection.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT b.name ORDER BY b.name",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT b.city ORDER BY b.city",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name ORDER BY b.name LIMIT 3",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT a.name, b.name ORDER BY a.name, b.name LIMIT 4",
        // Aggregations other than count over a hop.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.city, min(b.age), max(b.age) ORDER BY a.city",
        // A range filter over the scan, then a hop.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age >= 30 AND a.age <= 40 RETURN a.name, b.name ORDER BY a.name, b.name",
    ] {
        assert_paths_agree(&g, cypher);
    }
}

/// The switch has to actually change how a query is answered, or the comparisons
/// above would be one path against itself. This pins that the kernels and the
/// columnar executor really do leave the plan, using the plan text as the
/// evidence.
#[test]
fn the_switch_removes_the_fast_paths_from_the_plan() {
    let (_dir, g) = fixture();
    // The "by default" halves below mean the default, not whatever the ambient
    // environment happens to be.
    let _default = crate::exec_mode::fast_paths_required();

    let with_kernel =
        crate::explain(&g, "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*)").unwrap();
    assert!(
        with_kernel.contains("PathCount"),
        "expected the count kernel by default, got:\n{with_kernel}"
    );
    let without = {
        let _guard = RowPipelineOnly::install();
        crate::explain(&g, "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*)").unwrap()
    };
    assert!(
        !without.contains("PathCount"),
        "the switch must keep the count kernel out of the plan, got:\n{without}"
    );

    let grouped = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name, count(*)";
    assert!(
        crate::explain(&g, grouped)
            .unwrap()
            .contains("GroupedDegree"),
        "expected the grouped-degree kernel by default"
    );
    let grouped_without = {
        let _guard = RowPipelineOnly::install();
        crate::explain(&g, grouped).unwrap()
    };
    assert!(
        !grouped_without.contains("GroupedDegree"),
        "the switch must keep the grouped-degree kernel out of the plan, got:\n{grouped_without}"
    );
}
