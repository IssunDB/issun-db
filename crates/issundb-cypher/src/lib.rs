pub mod ast;
pub mod exec;
pub mod parser;
pub mod plan;

pub use exec::{QueryResult, Record, execute, explain};

#[cfg(test)]
mod tests {
    use super::*;
    use issundb_core::Graph;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn parse_simple_read_query() {
        let q = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = \"Alice\" RETURN b.name, b.age",
        )
        .unwrap();
        if let ast::Statement::Query(query) = q {
            assert_eq!(query.match_clauses.len(), 1);
            assert_eq!(
                query.match_clauses[0].pattern.node.variable.as_deref(),
                Some("a")
            );
            assert_eq!(
                query.match_clauses[0].pattern.node.label.as_deref(),
                Some("Person")
            );
            assert_eq!(query.match_clauses[0].pattern.rels.len(), 1);
            assert_eq!(
                query.match_clauses[0].pattern.rels[0].0.rel_type.as_deref(),
                Some("KNOWS")
            );
            assert_eq!(
                query.match_clauses[0].pattern.rels[0].1.variable.as_deref(),
                Some("b")
            );
            assert!(query.where_clause.is_some());
            assert_eq!(query.return_clause.items.len(), 2);
        } else {
            panic!("expected read query statement");
        }
    }

    #[test]
    fn parse_relationship_pattern_with_properties() {
        let q = parser::parse(
            "MATCH (a:Person)-[r:KNOWS {since: 2026, score: 0.95}]->(b:Person) RETURN r",
        )
        .unwrap();
        if let ast::Statement::Query(query) = q {
            let rel = &query.match_clauses[0].pattern.rels[0].0;
            assert_eq!(rel.variable.as_deref(), Some("r"));
            assert_eq!(rel.rel_type.as_deref(), Some("KNOWS"));
            let props = rel.properties.as_ref().unwrap();
            assert_eq!(
                props.get("since").unwrap(),
                &ast::Expr::Literal(ast::Literal::Int(2026))
            );
            assert_eq!(
                props.get("score").unwrap(),
                &ast::Expr::Literal(ast::Literal::Float(0.95))
            );
        } else {
            panic!("expected read query statement");
        }
    }

    #[test]
    fn parse_create_statement() {
        // CREATE without pipeline context now parses as a Query with a Create part.
        let c = parser::parse("CREATE (a:Person {name: \"Alice\", age: 30})").unwrap();
        let pattern = match &c {
            ast::Statement::Query(q) => {
                if let ast::QueryPart::Create { patterns } = &q.parts[0] {
                    &patterns[0]
                } else {
                    panic!("expected Create part");
                }
            }
            ast::Statement::Create(create) => &create.patterns[0],
            _ => panic!("expected create statement or query"),
        };
        assert_eq!(pattern.node.variable.as_deref(), Some("a"));
        assert_eq!(pattern.node.label.as_deref(), Some("Person"));
        let props = pattern.node.properties.as_ref().unwrap();
        assert_eq!(
            props.get("name").unwrap(),
            &ast::Expr::Literal(ast::Literal::Str("Alice".to_string()))
        );
    }

    #[test]
    fn parse_set_statement() {
        // MATCH + SET may be parsed as Statement::Set or as a write-only Statement::Query
        // (pipeline). Both are semantically equivalent — verify the parser accepts the query.
        let s = parser::parse("MATCH (a:Person) WHERE a.name = $name SET a.age = 31").unwrap();
        match s {
            ast::Statement::Set(set) => {
                assert_eq!(set.match_clauses.len(), 1);
                assert_eq!(set.set_items.len(), 1);
                assert_eq!(set.set_items[0].variable, "a");
                assert_eq!(set.set_items[0].property, "age");
            }
            ast::Statement::Query(q) => {
                // Write-only pipeline form: parts include Match + Set, no RETURN items.
                assert!(q.return_clause.items.is_empty());
                assert!(q.parts.len() >= 2);
            }
            other => panic!("expected set or query statement, got {:?}", other),
        }
    }

    #[test]
    fn parse_delete_statement() {
        // MATCH + DELETE may be parsed as Statement::Delete or as a write-only Statement::Query.
        let d = parser::parse("MATCH (a:Person) WHERE a.name = \"Alice\" DELETE a").unwrap();
        match d {
            ast::Statement::Delete(delete) => {
                assert_eq!(delete.match_clauses.len(), 1);
                assert_eq!(delete.variables, vec!["a"]);
            }
            ast::Statement::Query(q) => {
                assert!(q.return_clause.items.is_empty());
                assert!(q.parts.len() >= 2);
            }
            other => panic!("expected delete or query statement, got {:?}", other),
        }
    }

    #[test]
    fn execute_read_query_match_where_return() {
        let (_dir, g) = open_tmp();
        let alice = g
            .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
            .unwrap();
        let bob = g
            .add_node("Person", &json!({ "name": "Bob", "age": 25 }))
            .unwrap();
        g.add_edge(alice, bob, "KNOWS", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        let res = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = \"Alice\" RETURN b.name AS name, b.age AS age",
            &params,
        )
        .unwrap();

        assert_eq!(res.columns, vec!["name", "age"]);
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], json!("Bob"));
        assert_eq!(res.records[0].values[1], json!(25));
    }

    #[test]
    fn execute_match_relationship_with_properties() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        let charlie = g.add_node("Person", &json!({ "name": "Charlie" })).unwrap();
        g.add_edge(alice, bob, "KNOWS", &json!({ "since": 2020 }))
            .unwrap();
        g.add_edge(alice, charlie, "KNOWS", &json!({ "since": 2026 }))
            .unwrap();

        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        let res = execute(
            &g,
            "MATCH (a:Person)-[r:KNOWS {since: 2026}]->(b:Person) RETURN b.name AS name",
            &params,
        )
        .unwrap();

        assert_eq!(res.columns, vec!["name"]);
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], json!("Charlie"));
    }

    #[test]
    fn execute_create_node_and_edge() {
        let (_dir, g) = open_tmp();
        let params = HashMap::new();

        execute(
            &g,
            "CREATE (a:Person {name: \"Alice\"})-[:KNOWS]->(b:Person {name: \"Bob\"})",
            &params,
        )
        .unwrap();

        let alice_nodes = g.nodes_by_label("Person").unwrap();
        assert_eq!(alice_nodes.len(), 2);
    }

    #[test]
    fn execute_set_property() {
        let (_dir, g) = open_tmp();
        let alice = g
            .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
            .unwrap();

        let mut params = HashMap::new();
        params.insert("new_age".to_string(), json!(31));

        execute(
            &g,
            "MATCH (a:Person) WHERE a.name = \"Alice\" SET a.age = $new_age",
            &params,
        )
        .unwrap();

        let rec = g.get_node(alice).unwrap().unwrap();
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["age"], json!(31));
    }

    #[test]
    fn execute_delete_node() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();

        let params = HashMap::new();
        execute(
            &g,
            "MATCH (a:Person) WHERE a.name = \"Alice\" DELETE a",
            &params,
        )
        .unwrap();

        let rec = g.get_node(alice).unwrap();
        assert!(rec.is_none());
    }

    #[test]
    fn execute_delete_edge() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        let eid = g.add_edge(alice, bob, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        assert!(g.get_edge(eid).unwrap().is_some());

        let params = HashMap::new();
        let _result = execute(
            &g,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r",
            &params,
        )
        .unwrap();

        // Check edge is gone in the graph
        assert!(g.get_edge(eid).unwrap().is_none());
    }

    #[test]
    fn parse_variable_length_relationship_pattern() {
        let q1 = parser::parse("MATCH (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN b.name").unwrap();
        if let ast::Statement::Query(query) = q1 {
            let rel = &query.match_clauses[0].pattern.rels[0].0;
            assert_eq!(rel.rel_type.as_deref(), Some("KNOWS"));
            let range = rel.range.as_ref().unwrap();
            assert_eq!(range.min, Some(1));
            assert_eq!(range.max, Some(3));
        } else {
            panic!("expected query");
        }

        let q2 = parser::parse("MATCH (a:Person)-[:KNOWS*]->(b:Person) RETURN b.name").unwrap();
        if let ast::Statement::Query(query) = q2 {
            let rel = &query.match_clauses[0].pattern.rels[0].0;
            let range = rel.range.as_ref().unwrap();
            assert_eq!(range.min, Some(1));
            assert_eq!(range.max, None);
        } else {
            panic!("expected query");
        }

        let q3 = parser::parse("MATCH (a:Person)-[:KNOWS*3]->(b:Person) RETURN b.name").unwrap();
        if let ast::Statement::Query(query) = q3 {
            let rel = &query.match_clauses[0].pattern.rels[0].0;
            let range = rel.range.as_ref().unwrap();
            assert_eq!(range.min, Some(3));
            assert_eq!(range.max, Some(3));
        } else {
            panic!("expected query");
        }
    }

    #[test]
    fn execute_variable_length_relationship() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        let charlie = g.add_node("Person", &json!({ "name": "Charlie" })).unwrap();
        let david = g.add_node("Person", &json!({ "name": "David" })).unwrap();

        g.add_edge(alice, bob, "KNOWS", &json!({})).unwrap();
        g.add_edge(bob, charlie, "KNOWS", &json!({})).unwrap();
        g.add_edge(charlie, david, "KNOWS", &json!({})).unwrap();

        g.rebuild_csr().unwrap();

        let params = HashMap::new();

        // Match up to 3 hops from Alice
        let res = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS*1..3]->(b:Person) WHERE a.name = \"Alice\" RETURN b.name AS name",
            &params,
        )
        .unwrap();

        assert_eq!(res.columns, vec!["name"]);
        let mut names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "Bob".to_string(),
                "Charlie".to_string(),
                "David".to_string()
            ]
        );

        // Match exactly 2 hops from Alice
        let res2 = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS*2]->(b:Person) WHERE a.name = \"Alice\" RETURN b.name AS name",
            &params,
        )
        .unwrap();

        assert_eq!(res2.columns, vec!["name"]);
        let names2: Vec<String> = res2
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names2, vec!["Charlie".to_string()]);
    }

    #[test]
    fn unbounded_variable_length_traverses_all_hops() {
        // Regression for: max_hops defaulted to 1 when RelRange::max is None,
        // silently capping [:R*] to a single hop.
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "name": "a" })).unwrap();
        let b = g.add_node("N", &json!({ "name": "b" })).unwrap();
        let c = g.add_node("N", &json!({ "name": "c" })).unwrap();
        let d = g.add_node("N", &json!({ "name": "d" })).unwrap();
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(b, c, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        let res = execute(
            &g,
            "MATCH (x:N)-[:E*]->(y:N) WHERE x.name = \"a\" RETURN y.name AS name",
            &params,
        )
        .unwrap();

        let mut names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names.sort_unstable();
        // All three nodes reachable from a must be returned.
        assert_eq!(
            names,
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn variable_length_diamond_no_duplicate_rows() {
        // Regression for: completed_targets was a Vec, so nodes reachable via
        // multiple paths (diamond topology) produced duplicate result rows.
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({ "name": "a" })).unwrap();
        let b = g.add_node("N", &json!({ "name": "b" })).unwrap();
        let c = g.add_node("N", &json!({ "name": "c" })).unwrap();
        let d = g.add_node("N", &json!({ "name": "d" })).unwrap();
        // Diamond: a→b→d and a→c→d
        g.add_edge(a, b, "E", &json!({})).unwrap();
        g.add_edge(a, c, "E", &json!({})).unwrap();
        g.add_edge(b, d, "E", &json!({})).unwrap();
        g.add_edge(c, d, "E", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        let res = execute(
            &g,
            "MATCH (x:N)-[:E*1..2]->(y:N) WHERE x.name = \"a\" RETURN y.name AS name",
            &params,
        )
        .unwrap();

        let mut names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names.sort_unstable();
        // b, c, and d must each appear exactly once.
        assert_eq!(
            names,
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn multi_segment_anonymous_intermediate_node() {
        // Regression for: auto-generated _target_N names collided across segments,
        // causing all paths through anonymous intermediate nodes to be dropped.
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let b = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        let c = g.add_node("Person", &json!({ "name": "Charlie" })).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        // Anonymous intermediate node: (a:Person)-[:KNOWS]->()-[:KNOWS]->(c:Person)
        let res = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS]->()-[:KNOWS]->(c:Person) WHERE a.name = \"Alice\" RETURN c.name AS name",
            &params,
        )
        .unwrap();

        let names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["Charlie".to_string()]);
    }

    #[test]
    fn execute_unwind_literal_list() {
        let (_dir, g) = open_tmp();
        let params = HashMap::new();

        let res = execute(&g, "UNWIND [10, 20, 30] AS x RETURN x", &params).unwrap();
        assert_eq!(res.columns, vec!["x"]);
        assert_eq!(res.records.len(), 3);
        assert_eq!(res.records[0].values[0], json!(10));
        assert_eq!(res.records[1].values[0], json!(20));
        assert_eq!(res.records[2].values[0], json!(30));
    }

    #[test]
    fn execute_unwind_and_match() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        let query = format!(
            "UNWIND [{}, {}] AS node_id MATCH (p:Person) WHERE p.name = \"Bob\" RETURN p.name AS name",
            alice, bob
        );
        let res = execute(&g, &query, &params).unwrap();

        assert_eq!(res.columns, vec!["name"]);
        assert_eq!(res.records.len(), 2);
        let names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["Bob".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn execute_with_projection_barrier() {
        let (_dir, g) = open_tmp();
        let alice = g
            .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
            .unwrap();
        let bob = g
            .add_node("Person", &json!({ "name": "Bob", "age": 25 }))
            .unwrap();
        g.add_edge(alice, bob, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();

        let query_empty = "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH b, b.age AS age WHERE age > 26 RETURN b.name AS name, age";
        let res_empty = execute(&g, query_empty, &params).unwrap();
        assert_eq!(res_empty.records.len(), 0);

        let query_match = "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH b, b.age AS age WHERE age < 26 RETURN b.name AS name, age";
        let res_match = execute(&g, query_match, &params).unwrap();
        assert_eq!(res_match.records.len(), 1);
        assert_eq!(res_match.records[0].values[0], json!("Bob"));
        assert_eq!(res_match.records[0].values[1], json!(25));
    }

    // Regression: parse_set_statement used upper.find("SET") which matched "SET"
    // inside a label name such as "RESET", returning a wrong byte offset.
    #[test]
    fn set_on_node_with_label_containing_set_substring() {
        let (_dir, g) = open_tmp();
        g.add_node("RESET", &json!({ "x": 1 })).unwrap();

        let params = HashMap::new();
        // Parser must find the actual SET keyword, not the one inside "RESET".
        let _res = execute(&g, "MATCH (n:RESET) SET n.x = 42", &params).unwrap();

        let after = execute(&g, "MATCH (n:RESET) RETURN n.x AS x", &params).unwrap();
        assert_eq!(after.records[0].values[0], json!(42));
    }

    // Regression: execute_set resolved a node's label by scanning a hardcoded
    // five-entry list. Nodes with labels outside that list were silently
    // re-labeled to "Node", corrupting label_idx.
    #[test]
    fn set_preserves_unlisted_label() {
        let (_dir, g) = open_tmp();
        g.add_node("Employee", &json!({ "salary": 50000 })).unwrap();

        let params = HashMap::new();
        execute(&g, "MATCH (n:Employee) SET n.salary = 60000", &params).unwrap();

        // The node must still be reachable under "Employee", not "Node".
        let res = execute(&g, "MATCH (n:Employee) RETURN n.salary AS s", &params).unwrap();
        assert_eq!(
            res.records.len(),
            1,
            "node must still be indexed under Employee"
        );
        assert_eq!(res.records[0].values[0], json!(60000));

        // And it must not appear under "Node".
        let under_node = execute(&g, "MATCH (n:Node) RETURN n.salary AS s", &params).unwrap();
        assert_eq!(
            under_node.records.len(),
            0,
            "node must not be re-indexed under Node"
        );
    }

    // Regression: variable-length Expand with min_hops=0 never emitted the
    // source node itself and skipped source nodes with no outgoing edges.
    #[test]
    fn variable_length_zero_min_hops_includes_source() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let b = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        // *0..1 from Alice must include Alice herself (0-hop) and Bob (1-hop).
        let res = execute(
            &g,
            "MATCH (a:Person {name: 'Alice'})-[:KNOWS*0..1]->(b) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        let mut names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Alice", "Bob"]);

        // Isolated node (no outgoing edges) must still appear for *0..2.
        let c = g.add_node("Person", &json!({ "name": "Carol" })).unwrap();
        let _ = c; // no edges
        g.rebuild_csr().unwrap();
        let res2 = execute(
            &g,
            "MATCH (a:Person {name: 'Carol'})-[*0..2]->(b) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(res2.records.len(), 1);
        assert_eq!(res2.records[0].values[0], json!("Carol"));
    }

    #[test]
    fn explain_returns_non_empty_plan_string() {
        let (_dir, g) = open_tmp();
        let plan = explain(
            &g,
            "MATCH (n:Person)-[:KNOWS]->(m:Person) WHERE n.age > 30 RETURN n.name, m.name",
        )
        .unwrap();
        assert!(
            !plan.is_empty(),
            "explain must return a non-empty plan string"
        );
        // The plan must contain the label scan and the expand operator.
        assert!(
            plan.contains("Person"),
            "plan must reference the Person label"
        );
        assert!(
            plan.contains("KNOWS"),
            "plan must reference the KNOWS relationship type"
        );
    }

    // Regression: collect_bound_vars for Expand unconditionally reported
    // rel_var as bound even for variable-length paths, causing the optimizer
    // to place filters referencing rel_var above the Expand where it is absent
    // from the PathMap, producing "unbound variable" errors at runtime.
    #[test]
    fn variable_length_where_on_rel_var_errors_cleanly() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();
        let b = g.add_node("Person", &json!({ "name": "Bob" })).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({ "since": 2020 }))
            .unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();
        // Querying only dst_var from a variable-length path must work.
        let res = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS*1..2]->(b:Person) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], json!("Bob"));

        // Referencing rel_var in a variable-length WHERE must produce a clear error,
        // not a silent wrong result. (rel_var is undefined for multi-hop paths.)
        let err = execute(
            &g,
            "MATCH (a:Person)-[r*1..2]->(b:Person) WHERE r.since = 2020 RETURN b.name AS name",
            &params,
        );
        assert!(
            err.is_err(),
            "filter on rel_var in variable-length path should error"
        );
    }

    // Proposal B: the factorized Filter-over-Expand path must produce the same
    // results as the default path for a property filter on the source node.
    // The factorized path skips all destinations of rejected sources with zero
    // PathMap clones; this test verifies correctness of that fast path.
    #[test]
    fn factorized_filter_over_expand_source_predicate() {
        let (_dir, g) = open_tmp();

        let a = g
            .add_node("Person", &json!({ "name": "Alice", "age": 30 }))
            .unwrap();
        let b = g
            .add_node("Person", &json!({ "name": "Bob", "age": 20 }))
            .unwrap();
        let c = g
            .add_node("Person", &json!({ "name": "Carol", "age": 30 }))
            .unwrap();

        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let params = HashMap::new();

        // Filter on source (shared prefix): only Alice (age=30) passes.
        // The factorized path evaluates the filter once for Alice, once for Bob.
        // Bob is rejected and its destination (Carol) is skipped with no clone.
        let res = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN b.name AS name",
            &params,
        )
        .unwrap();

        let mut names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Bob", "Carol"]);

        // Filter on destination (expansion variable): falls through to per-row path.
        // Both Alice→Carol and Bob→Carol satisfy b.age = 30, so two rows are returned.
        let res2 = execute(
            &g,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age = 30 RETURN b.name AS name",
            &params,
        )
        .unwrap();

        let mut names2: Vec<String> = res2
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        names2.sort();
        assert_eq!(names2, vec!["Carol", "Carol"]);
    }

    // Proposal A: NodeIndexScan should activate for any property with data,
    // not only properties with an explicit CREATE INDEX.
    #[test]
    fn auto_index_enables_node_index_scan_without_create_index() {
        let (_dir, g) = open_tmp();

        for i in 0..20i64 {
            g.add_node("Person", &json!({ "age": i })).unwrap();
        }
        g.rebuild_csr().unwrap();

        let params = HashMap::new();

        // No CREATE INDEX has been run. The auto-index written on insertion must
        // allow NodeIndexScan to serve this equality predicate.
        let res = execute(
            &g,
            "MATCH (n:Person) WHERE n.age = 5 RETURN n.age AS age",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], json!(5));

        // Confirm the plan uses NodeIndexScan, not a full LabelScan + Filter.
        let plan = explain(&g, "MATCH (n:Person) WHERE n.age = 5 RETURN n.age").unwrap();
        assert!(
            plan.contains("NodeIndexScan"),
            "plan must use NodeIndexScan when auto-index is present; got:\n{plan}"
        );
    }
}
