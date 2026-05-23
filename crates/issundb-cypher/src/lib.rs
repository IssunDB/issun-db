pub mod ast;
pub mod parser;
pub mod exec;

pub use exec::{QueryResult, Record, execute};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use serde_json::json;
    use tempfile::TempDir;
    use issundb_core::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    #[test]
    fn parse_simple_read_query() {
        let q = parser::parse("MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = \"Alice\" RETURN b.name, b.age").unwrap();
        if let ast::Statement::Query(query) = q {
            assert_eq!(query.match_clauses.len(), 1);
            assert_eq!(query.match_clauses[0].pattern.node.variable.as_deref(), Some("a"));
            assert_eq!(query.match_clauses[0].pattern.node.label.as_deref(), Some("Person"));
            assert_eq!(query.match_clauses[0].pattern.rels.len(), 1);
            assert_eq!(query.match_clauses[0].pattern.rels[0].0.rel_type.as_deref(), Some("KNOWS"));
            assert_eq!(query.match_clauses[0].pattern.rels[0].1.variable.as_deref(), Some("b"));
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
            assert_eq!(props.get("name").unwrap(), &ast::Literal::Str("Alice".to_string()));
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
        let alice = g.add_node("Person", &json!({ "name": "Alice", "age": 30 })).unwrap();
        let bob = g.add_node("Person", &json!({ "name": "Bob", "age": 25 })).unwrap();
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

        execute(&g, "CREATE (a:Person {name: \"Alice\"})-[:KNOWS]->(b:Person {name: \"Bob\"})", &params).unwrap();

        let alice_nodes = g.nodes_by_label("Person").unwrap();
        assert_eq!(alice_nodes.len(), 2);
    }

    #[test]
    fn execute_set_property() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice", "age": 30 })).unwrap();

        let mut params = HashMap::new();
        params.insert("new_age".to_string(), json!(31));

        execute(&g, "MATCH (a:Person) WHERE a.name = \"Alice\" SET a.age = $new_age", &params).unwrap();

        let rec = g.get_node(alice).unwrap().unwrap();
        let props: serde_json::Value = rmp_serde::from_slice(&rec.props).unwrap();
        assert_eq!(props["age"], json!(31));
    }

    #[test]
    fn execute_delete_node() {
        let (_dir, g) = open_tmp();
        let alice = g.add_node("Person", &json!({ "name": "Alice" })).unwrap();

        let params = HashMap::new();
        execute(&g, "MATCH (a:Person) WHERE a.name = \"Alice\" DELETE a", &params).unwrap();

        let rec = g.get_node(alice).unwrap();
        assert!(rec.is_none());
    }
}
