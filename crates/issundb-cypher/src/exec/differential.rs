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
