use std::collections::{HashMap, HashSet};

use issundb_core::{EdgeId, Graph, NodeId, PropValue};
use tracing::instrument;

use crate::ast::*;
use crate::parser;
use crate::plan::{FilterExpr, LogicalPlanner, Optimizer, PhysicalOperator, PhysicalPlanner};

mod ddl;
mod expr;
mod read;
mod write;

use ddl::{
    execute_create_constraint, execute_create_index, execute_drop_constraint, execute_drop_index,
};
use read::execute_read_query;
use write::{
    execute_create, execute_create_and_return, execute_create_internal_with_context,
    execute_delete, execute_delete_and_return, execute_foreach, execute_merge,
    execute_merge_and_return, execute_remove, execute_remove_and_return, execute_set,
    execute_set_and_return,
};

/// The tabular result of a Cypher query execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub records: Vec<Record>,
}

/// An individual row in the query result table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Record {
    pub values: Vec<serde_json::Value>,
}

/// A binding for a Cypher variable: either a graph node or a graph edge.
///
/// The path map uses this type so that relationship variables are bound to the
/// correct `EdgeId` and node variables are bound to the correct `NodeId`.
/// `evaluate_expr` dispatches on the variant to call `get_node` or `get_edge`
/// as appropriate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GraphBinding {
    Node(NodeId),
    Edge(EdgeId),
    Scalar(serde_json::Value),
}

impl std::hash::Hash for GraphBinding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            GraphBinding::Node(id) => {
                0.hash(state);
                id.hash(state);
            }
            GraphBinding::Edge(id) => {
                1.hash(state);
                id.hash(state);
            }
            GraphBinding::Scalar(val) => {
                2.hash(state);
                val.to_string().hash(state);
            }
        }
    }
}

/// A row of variable bindings produced during plan execution.
type PathMap = HashMap<String, GraphBinding>;

/// Execute a Cypher query against the `Graph` handle.
#[instrument(skip(graph, params), fields(cypher = %cypher))]
pub fn execute(
    graph: &Graph,
    cypher: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let stmt = parser::parse(cypher)?;
    execute_statement(graph, &stmt, params)
}

/// Parse `cypher`, compile it into an optimized physical plan, and return the
/// plan as a human-readable indented tree.
///
/// Non-query statements (CREATE, SET, DELETE, MERGE) return a one-line summary
/// because they do not go through the read-query planner.
pub fn explain(graph: &Graph, cypher: &str) -> Result<String, String> {
    use crate::plan::physical::format_physical_plan;
    use crate::plan::{LogicalPlanner, Optimizer, PhysicalPlanner};

    let stmt = parser::parse(cypher)?;
    match stmt {
        Statement::Query(q) => {
            let logical = LogicalPlanner::plan(&q)?;
            let physical = PhysicalPlanner::plan(&logical);
            let optimized = Optimizer::optimize(physical, Some(graph));
            Ok(format_physical_plan(&optimized, 0))
        }
        Statement::Create(_) => Ok("CreatePattern\n".into()),
        Statement::CreateAndReturn(_) => Ok("CreatePatternReturn\n".into()),
        Statement::Set(_) => Ok("MatchThenSet\n".into()),
        Statement::SetAndReturn(_) => Ok("MatchThenSetReturn\n".into()),
        Statement::Delete(_) => Ok("MatchThenDelete\n".into()),
        Statement::DeleteAndReturn(_) => Ok("MatchThenDeleteReturn\n".into()),
        Statement::Merge(_) => Ok("Merge\n".into()),
        Statement::MergeAndReturn(_) => Ok("MergeReturn\n".into()),
        Statement::CreateIndex(ref ci) => Ok(format!("CreateIndex {}:{}\n", ci.label, ci.property)),
        Statement::DropIndex(ref di) => Ok(format!("DropIndex {}:{}\n", di.label, di.property)),
        Statement::Remove(_) => Ok("Remove\n".into()),
        Statement::RemoveAndReturn(_) => Ok("RemoveReturn\n".into()),
        Statement::Union(_) => Ok("Union\n".into()),
        Statement::Foreach(_) => Ok("Foreach\n".into()),
        Statement::CreateConstraint(ref cc) => {
            Ok(format!("CreateConstraint {}:{}\n", cc.label, cc.property))
        }
        Statement::DropConstraint(ref dc) => {
            Ok(format!("DropConstraint {}:{}\n", dc.label, dc.property))
        }
        Statement::Pipeline(_) => Ok("Pipeline\n".into()),
    }
}

fn execute_union(
    graph: &Graph,
    stmt: &UnionStatement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let left_result = execute_statement(graph, &stmt.left, params)?;
    let right_result = execute_statement(graph, &stmt.right, params)?;

    if left_result.columns.len() != right_result.columns.len() {
        return Err(format!(
            "UNION: column count mismatch ({} vs {})",
            left_result.columns.len(),
            right_result.columns.len()
        ));
    }

    let columns = left_result.columns.clone();
    let mut records = left_result.records;
    records.extend(right_result.records);

    if !stmt.all {
        // Deduplicate rows by serialized value.
        let mut seen: HashSet<String> = HashSet::new();
        records.retain(|r| {
            let key = serde_json::to_string(&r.values).unwrap_or_default();
            seen.insert(key)
        });
    }

    Ok(QueryResult { columns, records })
}

/// Execute a pipeline of statements, threading node/edge bindings created by
/// CREATE statements so that later statements can reference nodes created earlier.
fn execute_pipeline(
    graph: &Graph,
    stmts: &[Statement],
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    let mut shared_bindings: PathMap = PathMap::new();
    let mut last = QueryResult {
        columns: vec![],
        records: vec![],
    };

    for stmt in stmts {
        match stmt {
            Statement::Create(c) => {
                graph.with_write_lock(|| {
                    for pattern in &c.patterns {
                        let created = execute_create_internal_with_context(
                            graph,
                            pattern,
                            &shared_bindings,
                            params,
                        )?;
                        shared_bindings.extend(created);
                    }
                    Ok::<(), String>(())
                })?;
                last = QueryResult {
                    columns: vec![],
                    records: vec![],
                };
            }
            other => {
                last = execute_statement(graph, other, params)?;
            }
        }
    }

    Ok(last)
}

fn execute_statement(
    graph: &Graph,
    stmt: &Statement,
    params: &HashMap<String, serde_json::Value>,
) -> Result<QueryResult, String> {
    match stmt {
        Statement::Query(q) => execute_read_query(graph, q, params),
        Statement::Create(c) => graph.with_write_lock(|| execute_create(graph, c, params)),
        Statement::CreateAndReturn(c) => {
            graph.with_write_lock(|| execute_create_and_return(graph, c, params))
        }
        Statement::Set(s) => graph.with_write_lock(|| execute_set(graph, s, params)),
        Statement::SetAndReturn(s) => {
            graph.with_write_lock(|| execute_set_and_return(graph, s, params))
        }
        Statement::Delete(d) => graph.with_write_lock(|| execute_delete(graph, d, params)),
        Statement::DeleteAndReturn(d) => execute_delete_and_return(graph, d, params),
        Statement::Merge(m) => execute_merge(graph, m, params),
        Statement::MergeAndReturn(m) => execute_merge_and_return(graph, m, params),
        Statement::CreateIndex(ci) => execute_create_index(graph, ci),
        Statement::DropIndex(di) => execute_drop_index(graph, di),
        Statement::Remove(r) => graph.with_write_lock(|| execute_remove(graph, r, params)),
        Statement::RemoveAndReturn(r) => execute_remove_and_return(graph, r, params),
        Statement::Union(u) => execute_union(graph, u, params),
        Statement::Foreach(f) => graph.with_write_lock(|| execute_foreach(graph, f, params)),
        Statement::CreateConstraint(cc) => execute_create_constraint(graph, cc),
        Statement::DropConstraint(dc) => execute_drop_constraint(graph, dc),
        Statement::Pipeline(stmts) => {
            // Execute each statement in order, returning the last result.
            // A shared PathMap threads variable bindings created by CREATE
            // statements so that later statements in the pipeline can reference
            // nodes created earlier (e.g., `CREATE (a:A) CREATE (a)-[:R]->(b:B)`).
            execute_pipeline(graph, stmts, params)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use issundb_core::Graph;
    use tempfile::TempDir;

    fn setup_graph() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        (dir, graph)
    }

    fn insert_person(graph: &Graph, name: &str, age: i64, city: &str) -> issundb_core::NodeId {
        let props = serde_json::json!({"name": name, "age": age, "city": city});
        graph.add_node("Person", &props).unwrap()
    }

    // Helper: run a simple Cypher and return all records.
    fn run(graph: &Graph, cypher: &str) -> Vec<Vec<serde_json::Value>> {
        let params = HashMap::new();
        execute(graph, cypher, &params)
            .unwrap()
            .records
            .into_iter()
            .map(|r| r.values)
            .collect()
    }

    #[test]
    fn order_by_age_asc() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY n.age ASC",
        );
        let ages: Vec<i64> = rows.iter().map(|r| r[1].as_i64().unwrap()).collect();
        assert_eq!(ages, vec![25, 30, 40]);
    }

    #[test]
    fn order_by_name_desc() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.name AS name ORDER BY n.name DESC",
        );
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Carol", "Bob", "Alice"]);
    }

    #[test]
    fn limit_returns_at_most_n_rows() {
        let (_dir, graph) = setup_graph();
        for i in 0..10i64 {
            graph
                .add_node("Item", &serde_json::json!({"i": i}))
                .unwrap();
        }
        let rows = run(&graph, "MATCH (n:Item) RETURN n.i AS i LIMIT 3");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn skip_and_limit_pagination() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");
        insert_person(&graph, "Dave", 35, "LA");
        insert_person(&graph, "Eve", 28, "NY");

        // ORDER BY age ASC, then SKIP 1 LIMIT 2 gives the 2nd and 3rd youngest: 28, 30.
        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.age AS age ORDER BY n.age ASC SKIP 1 LIMIT 2",
        );
        assert_eq!(rows.len(), 2);
        let ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        assert_eq!(ages, vec![28, 30]);
    }

    #[test]
    fn count_star_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");

        let rows = run(&graph, "MATCH (n:Person) RETURN count(*) AS c");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_i64().unwrap(), 3);
    }

    #[test]
    fn sum_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 20, "X");
        insert_person(&graph, "Carol", 30, "X");

        let rows = run(&graph, "MATCH (n:Person) RETURN sum(n.age) AS total");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 60.0);
    }

    #[test]
    fn avg_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 30, "X");

        let rows = run(&graph, "MATCH (n:Person) RETURN avg(n.age) AS a");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 20.0);
    }

    #[test]
    fn min_max_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 10, "X");
        insert_person(&graph, "Bob", 30, "X");
        insert_person(&graph, "Carol", 20, "X");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN min(n.age) AS lo, max(n.age) AS hi",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_f64().unwrap(), 10.0);
        assert_eq!(rows[0][1].as_f64().unwrap(), 30.0);
    }

    #[test]
    fn collect_aggregation() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "NY");

        let rows = run(&graph, "MATCH (n:Person) RETURN collect(n.name) AS names");
        assert_eq!(rows.len(), 1);
        let arr = rows[0][0].as_array().unwrap();
        let mut names: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Alice", "Bob"]);
    }

    #[test]
    fn group_by_city_count() {
        let (_dir, graph) = setup_graph();
        insert_person(&graph, "Alice", 25, "NY");
        insert_person(&graph, "Bob", 30, "LA");
        insert_person(&graph, "Carol", 40, "NY");

        let rows = run(
            &graph,
            "MATCH (n:Person) RETURN n.city AS city, count(*) AS c ORDER BY n.city ASC",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_str().unwrap(), "LA");
        assert_eq!(rows[0][1].as_i64().unwrap(), 1);
        assert_eq!(rows[1][0].as_str().unwrap(), "NY");
        assert_eq!(rows[1][1].as_i64().unwrap(), 2);
    }

    #[test]
    fn merge_creates_node_when_absent() {
        let (_dir, graph) = setup_graph();

        let params = HashMap::new();
        execute(&graph, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(&graph, "MATCH (n:Person) RETURN n.name AS name", &params).unwrap();
        assert_eq!(result.records.len(), 1);
    }

    #[test]
    fn merge_does_not_duplicate_existing_node() {
        let (_dir, graph) = setup_graph();

        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();
        execute(&graph, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(&graph, "MATCH (n:Person) RETURN n.name AS name", &params).unwrap();
        assert_eq!(result.records.len(), 1, "MERGE must not create a duplicate");
    }

    #[test]
    fn optional_match_returns_nulls_when_no_match() {
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Person", &serde_json::json!({"name": "Alice"}))
            .unwrap();

        let params = HashMap::new();
        let result = execute(&graph, "OPTIONAL MATCH (n:NonExistent) RETURN n", &params).unwrap();
        // Should return one row with n = null.
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].values[0], serde_json::Value::Null);
    }

    #[test]
    fn create_index_and_drop_index_execute_without_error() {
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Movie", &serde_json::json!({"title": "Inception"}))
            .unwrap();

        let params = HashMap::new();
        execute(&graph, "CREATE INDEX FOR (n:Movie) ON (n.title)", &params).unwrap();
        assert!(graph.has_node_text_index("Movie", "title").unwrap());

        execute(&graph, "DROP INDEX FOR (n:Movie) ON (n.title)", &params).unwrap();
        assert!(!graph.has_node_text_index("Movie", "title").unwrap());
    }

    #[test]
    fn merge_concurrent_safety() {
        let (_dir, graph) = setup_graph();
        let graph_arc = std::sync::Arc::new(graph);
        let mut threads = Vec::new();
        for _ in 0..10 {
            let g = graph_arc.clone();
            threads.push(std::thread::spawn(move || {
                let params = HashMap::new();
                execute(&g, "MERGE (n:Person {name: 'Alice'})", &params).unwrap();
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let params = HashMap::new();
        let result = execute(
            &graph_arc,
            "MATCH (n:Person) RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(
            result.records.len(),
            1,
            "Concurrency race in MERGE created duplicate nodes"
        );
    }

    // --- Feature 1: DETACH DELETE ---

    #[test]
    fn detach_delete_removes_node_and_its_edges() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();

        // Build the graph directly via Graph API to guarantee the edge exists.
        let alice_id = graph
            .add_node("Person", &serde_json::json!({"name": "Alice"}))
            .unwrap();
        let bob_id = graph
            .add_node("Person", &serde_json::json!({"name": "Bob"}))
            .unwrap();
        graph
            .add_edge(alice_id, bob_id, "KNOWS", &serde_json::json!({}))
            .unwrap();

        // DETACH DELETE Alice should remove Alice and the KNOWS edge.
        execute(
            &graph,
            "MATCH (a:Person {name: 'Alice'}) DETACH DELETE a",
            &params,
        )
        .unwrap();

        // Alice should be gone.
        let after_alice = execute(
            &graph,
            "MATCH (n:Person {name: 'Alice'}) RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(after_alice.records.len(), 0, "Alice should be deleted");

        // Bob should still exist.
        let after_bob = execute(
            &graph,
            "MATCH (n:Person {name: 'Bob'}) RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(after_bob.records.len(), 1, "Bob should still exist");
    }

    // --- Feature 2: REMOVE ---

    #[test]
    fn remove_property_deletes_it_from_node() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (n:Person {name: 'Alice', age: 30})",
            &params,
        )
        .unwrap();

        execute(
            &graph,
            "MATCH (n:Person) WHERE n.name = 'Alice' REMOVE n.age",
            &params,
        )
        .unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.age AS age",
            &params,
        )
        .unwrap();
        assert_eq!(result.records.len(), 1);
        assert_eq!(
            result.records[0].values[0],
            serde_json::Value::Null,
            "age should be null after REMOVE"
        );
    }

    // --- Feature 3: CASE Expression ---

    #[test]
    fn case_expression_searched_form() {
        let (_dir, graph) = setup_graph();
        let rows = run(
            &graph,
            "RETURN CASE WHEN 1 > 0 THEN 'yes' ELSE 'no' END AS result",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_str().unwrap(), "yes");
    }

    #[test]
    fn case_expression_simple_form() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(&graph, "CREATE (n:Item {status: 'active'})", &params).unwrap();
        let rows = run(
            &graph,
            "MATCH (n:Item) RETURN CASE n.status WHEN 'active' THEN 1 ELSE 0 END AS v",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_i64().unwrap(), 1);
    }

    // --- Feature 4: UNION / UNION ALL ---

    #[test]
    fn union_combines_results() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();
        execute(&graph, "CREATE (n:Company {name: 'Acme'})", &params).unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person) RETURN n.name AS name UNION MATCH (n:Company) RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(result.records.len(), 2, "UNION should return both rows");
    }

    #[test]
    fn union_deduplicates() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person {name: 'Alice'}) RETURN n.name AS name UNION MATCH (n:Person {name: 'Alice'}) RETURN n.name AS name",
            &params,
        ).unwrap();
        assert_eq!(result.records.len(), 1, "UNION should deduplicate rows");
    }

    #[test]
    fn union_all_keeps_duplicates() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person {name: 'Alice'}) RETURN n.name AS name UNION ALL MATCH (n:Person {name: 'Alice'}) RETURN n.name AS name",
            &params,
        ).unwrap();
        assert_eq!(
            result.records.len(),
            2,
            "UNION ALL should keep duplicate rows"
        );
    }

    // --- Feature 5: FOREACH ---

    #[test]
    fn foreach_creates_nodes_from_list() {
        let (_dir, graph) = setup_graph();
        // FOREACH iterates over a literal list; each iteration executes CREATE (:Person).
        // The body CREATE uses no properties — we simply verify that one node is created
        // per list element (two elements → two nodes).
        execute(
            &graph,
            "FOREACH (x IN [1, 2] | CREATE (:Person))",
            &HashMap::new(),
        )
        .unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person) RETURN count(*) AS c",
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            result.records[0].values[0].as_i64().unwrap(),
            2,
            "FOREACH should create 2 Person nodes"
        );
    }

    // --- Feature 6: CREATE / DROP CONSTRAINT ---

    #[test]
    fn create_and_drop_unique_constraint() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE CONSTRAINT ON (n:User) ASSERT n.email IS UNIQUE",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "DROP CONSTRAINT ON (n:User) ASSERT n.email IS UNIQUE",
            &params,
        )
        .unwrap();
    }

    #[test]
    fn create_and_drop_exists_constraint() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE CONSTRAINT ON (n:Task) ASSERT EXISTS(n.title)",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "DROP CONSTRAINT ON (n:Task) ASSERT EXISTS(n.title)",
            &params,
        )
        .unwrap();
    }

    // --- Feature 7: Regex matching (=~) ---

    #[test]
    fn regex_match_filters_correctly() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(&graph, "CREATE (n:Person {name: 'Alice'})", &params).unwrap();
        execute(&graph, "CREATE (n:Person {name: 'Bob'})", &params).unwrap();

        let result = execute(
            &graph,
            "MATCH (n:Person) WHERE n.name =~ 'Ali.*' RETURN n.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].values[0].as_str().unwrap(), "Alice");
    }

    // --- Feature 8: Statistical Aggregation Functions ---

    #[test]
    fn stdev_aggregation() {
        let (_dir, graph) = setup_graph();
        // Insert nodes with ages 2, 4, 4, 4, 5, 5, 7, 9.
        // Mean = 5, sum of squared deviations = 9+1+1+1+0+0+4+16 = 32,
        // sample variance = 32/7, sample stDev = sqrt(32/7) ≈ 2.1381.
        for age in [2i64, 4, 4, 4, 5, 5, 7, 9] {
            graph
                .add_node("Num", &serde_json::json!({"age": age}))
                .unwrap();
        }
        let rows = run(&graph, "MATCH (n:Num) RETURN stDev(n.age) AS s");
        assert_eq!(rows.len(), 1);
        let sd = rows[0][0].as_f64().unwrap();
        let expected = (32.0f64 / 7.0).sqrt();
        assert!(
            (sd - expected).abs() < 1e-9,
            "expected stDev ~{}, got {}",
            expected,
            sd
        );
    }

    #[test]
    fn merge_on_create_set_does_not_affect_existing_nodes() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        // Create an existing Bob.
        execute(&graph, "CREATE (b:Person {name: 'Bob', age: 40})", &params).unwrap();

        // MERGE a relationship between a new Alice (absent) and Bob (name: 'Bob').
        // Since the relationship (and Alice) is absent, the pattern is created.
        execute(
            &graph,
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'}) ON CREATE SET b.age = 50",
            &params,
        )
        .unwrap();

        // Verify the original Bob's age remains 40.
        let r1 = execute(
            &graph,
            "MATCH (b:Person {name: 'Bob', age: 40}) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(r1.records.len(), 1);

        // Verify the new Bob's age is 50.
        let r2 = execute(
            &graph,
            "MATCH (b:Person {name: 'Bob', age: 50}) RETURN b.name AS name",
            &params,
        )
        .unwrap();
        assert_eq!(r2.records.len(), 1);
    }
}
