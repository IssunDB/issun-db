pub mod ast;
pub mod exec;
pub mod parser;
pub mod plan;

pub use exec::{QueryResult, Record, execute};

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
    fn parse_create_statement() {
        let c = parser::parse("CREATE (a:Person {name: \"Alice\", age: 30})").unwrap();
        if let ast::Statement::Create(create) = c {
            assert_eq!(create.pattern.node.variable.as_deref(), Some("a"));
            assert_eq!(create.pattern.node.label.as_deref(), Some("Person"));
            let props = create.pattern.node.properties.as_ref().unwrap();
            assert_eq!(
                props.get("name").unwrap(),
                &ast::Literal::Str("Alice".to_string())
            );
        } else {
            panic!("expected create statement");
        }
    }

    #[test]
    fn parse_set_statement() {
        let s = parser::parse("MATCH (a:Person) WHERE a.name = $name SET a.age = 31").unwrap();
        if let ast::Statement::Set(set) = s {
            assert_eq!(set.match_clauses.len(), 1);
            assert_eq!(set.set_items.len(), 1);
            assert_eq!(set.set_items[0].variable, "a");
            assert_eq!(set.set_items[0].property, "age");
        } else {
            panic!("expected set statement");
        }
    }

    #[test]
    fn parse_delete_statement() {
        let d = parser::parse("MATCH (a:Person) WHERE a.name = \"Alice\" DELETE a").unwrap();
        if let ast::Statement::Delete(delete) = d {
            assert_eq!(delete.match_clauses.len(), 1);
            assert_eq!(delete.variables, vec!["a"]);
        } else {
            panic!("expected delete statement");
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
}
