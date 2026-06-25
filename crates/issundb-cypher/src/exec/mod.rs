use crate::error::CypherError;
use std::collections::{HashMap, HashSet};

use issundb_core::{EdgeId, Graph, NodeId, PropValue};
use tracing::instrument;

use crate::ast::*;
use crate::parser;
use crate::plan::{FilterExpr, LogicalPlanner, Optimizer, PhysicalOperator, PhysicalPlanner};

mod copy;
mod ddl;
mod expr;
mod factorize;
pub(crate) mod read;
mod row;
mod vectorized;
mod write;

use copy::{execute_copy, execute_export_db, execute_import_db};
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
) -> Result<QueryResult, CypherError> {
    execute_with_procedures(
        graph,
        cypher,
        params,
        &crate::procedure::ProcedureRegistry::new(),
    )
}

/// Execute a Cypher query, resolving any `CALL` clauses against `registry`.
///
/// The built-in `issundb.*` graph-algorithm procedures (PageRank, the
/// centralities, connected components, and label propagation) are always
/// available and resolved before the registry, so the default `execute` passes
/// an empty registry; callers that register additional table-backed procedures
/// use this entry point.
#[instrument(skip(graph, params, registry), fields(cypher = %cypher))]
pub fn execute_with_procedures(
    graph: &Graph,
    cypher: &str,
    params: &HashMap<String, serde_json::Value>,
    registry: &crate::procedure::ProcedureRegistry,
) -> Result<QueryResult, CypherError> {
    let stmt = parser::parse(cypher)?;
    // Freeze a single wall-clock instant for the whole statement so that all current-time
    // functions (date(), datetime(), and the like) within this query observe the same time.
    let _clock = expr::StatementClock::install();
    execute_statement(graph, &stmt, params, registry)
}

/// Parse `cypher`, compile it into an optimized physical plan, and return the
/// plan as a human-readable indented tree.
///
/// Non-query statements (CREATE, SET, DELETE, MERGE) return a one-line summary
/// because they do not go through the read-query planner.
pub fn explain(graph: &Graph, cypher: &str) -> Result<String, CypherError> {
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
        Statement::Copy(ref c) => Ok(format!("Copy {} FROM '{}'\n", c.target, c.filepath)),
        Statement::ExportDatabase(ref e) => Ok(format!("ExportDatabase '{}'\n", e.filepath)),
        Statement::ImportDatabase(ref i) => Ok(format!("ImportDatabase '{}'\n", i.filepath)),
        Statement::Pipeline(_) => Ok("Pipeline\n".into()),
    }
}

/// Return true if any Union node in the tree mixes UNION and UNION ALL.
fn union_has_mixed_all(stmt: &UnionStatement) -> bool {
    let check = |s: &Statement| -> bool {
        if let Statement::Union(u) = s {
            u.all != stmt.all || union_has_mixed_all(u)
        } else {
            false
        }
    };
    check(&stmt.left) || check(&stmt.right)
}

fn execute_union(
    graph: &Graph,
    stmt: &UnionStatement,
    params: &HashMap<String, serde_json::Value>,
    registry: &crate::procedure::ProcedureRegistry,
) -> Result<QueryResult, String> {
    if union_has_mixed_all(stmt) {
        return Err(
            "SyntaxError: mixing UNION and UNION ALL in the same query is not allowed".to_string(),
        );
    }

    let left_result =
        execute_statement(graph, &stmt.left, params, registry).map_err(|e| e.to_string())?;
    let right_result =
        execute_statement(graph, &stmt.right, params, registry).map_err(|e| e.to_string())?;

    if left_result.columns != right_result.columns {
        return Err(format!(
            "SyntaxError: UNION column mismatch — left {:?}, right {:?}",
            left_result.columns, right_result.columns
        ));
    }

    let columns = left_result.columns.clone();
    let mut records = left_result.records;
    records.extend(right_result.records);

    if !stmt.all {
        let mut seen: HashSet<String> = HashSet::new();
        records.retain(|r| {
            let key = serde_json::to_string(&r.values).unwrap_or_default();
            seen.insert(key)
        });
    }

    Ok(QueryResult { columns, records })
}

/// Classify query execution errors into structured CypherError variants.
fn to_cypher_error(err: String) -> CypherError {
    let lower = err.to_lowercase();
    if lower.contains("type mismatch") || (lower.contains("expected") && lower.contains("got")) {
        CypherError::TypeMismatch(err)
    } else if lower.contains("not bound")
        || lower.contains("undefined")
        || lower.contains("not found")
    {
        CypherError::VariableNotBound(err)
    } else if lower.contains("division by zero")
        || lower.contains("math")
        || lower.contains("overflow")
    {
        CypherError::Math(err)
    } else if lower.contains("storage") || lower.contains("lmdb") || lower.contains("heed") {
        CypherError::Storage(err)
    } else {
        CypherError::Execution(err)
    }
}

/// Execute a pipeline of statements, threading node/edge bindings created by
/// CREATE statements so that later statements can reference nodes created earlier.
fn execute_pipeline(
    graph: &Graph,
    stmts: &[Statement],
    params: &HashMap<String, serde_json::Value>,
    registry: &crate::procedure::ProcedureRegistry,
) -> Result<QueryResult, CypherError> {
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
                        )
                        .map_err(to_cypher_error)?;
                        shared_bindings.extend(created);
                    }
                    Ok::<(), CypherError>(())
                })?;
                last = QueryResult {
                    columns: vec![],
                    records: vec![],
                };
            }
            other => {
                last = execute_statement(graph, other, params, registry)?;
            }
        }
    }

    Ok(last)
}

fn execute_statement(
    graph: &Graph,
    stmt: &Statement,
    params: &HashMap<String, serde_json::Value>,
    registry: &crate::procedure::ProcedureRegistry,
) -> Result<QueryResult, CypherError> {
    match stmt {
        Statement::Query(q) => {
            // Resolve CALL clauses against the registry before planning. Cloning is
            // cheap relative to execution and keeps the parsed AST immutable.
            if q.parts.iter().any(|p| matches!(p, QueryPart::Call { .. })) {
                let mut resolved = q.clone();
                read::resolve_call_parts(graph, &mut resolved, registry, params)
                    .map_err(to_cypher_error)?;
                execute_read_query(graph, &resolved, params).map_err(to_cypher_error)
            } else {
                execute_read_query(graph, q, params).map_err(to_cypher_error)
            }
        }
        Statement::Create(c) => graph
            .with_write_lock(|| execute_create(graph, c, params))
            .map_err(to_cypher_error),
        Statement::CreateAndReturn(c) => graph
            .with_write_lock(|| execute_create_and_return(graph, c, params))
            .map_err(to_cypher_error),
        Statement::Set(s) => graph
            .with_write_lock(|| execute_set(graph, s, params))
            .map_err(to_cypher_error),
        Statement::SetAndReturn(s) => graph
            .with_write_lock(|| execute_set_and_return(graph, s, params))
            .map_err(to_cypher_error),
        Statement::Delete(d) => graph
            .with_write_lock(|| execute_delete(graph, d, params))
            .map_err(to_cypher_error),
        Statement::DeleteAndReturn(d) => {
            execute_delete_and_return(graph, d, params).map_err(to_cypher_error)
        }
        Statement::Merge(m) => execute_merge(graph, m, params).map_err(to_cypher_error),
        Statement::MergeAndReturn(m) => {
            execute_merge_and_return(graph, m, params).map_err(to_cypher_error)
        }
        Statement::CreateIndex(ci) => execute_create_index(graph, ci).map_err(to_cypher_error),
        Statement::DropIndex(di) => execute_drop_index(graph, di).map_err(to_cypher_error),
        Statement::Remove(r) => graph
            .with_write_lock(|| execute_remove(graph, r, params))
            .map_err(to_cypher_error),
        Statement::RemoveAndReturn(r) => {
            execute_remove_and_return(graph, r, params).map_err(to_cypher_error)
        }
        Statement::Union(u) => execute_union(graph, u, params, registry).map_err(to_cypher_error),
        Statement::Foreach(f) => graph
            .with_write_lock(|| execute_foreach(graph, f, params))
            .map_err(to_cypher_error),
        Statement::CreateConstraint(cc) => {
            execute_create_constraint(graph, cc).map_err(to_cypher_error)
        }
        Statement::DropConstraint(dc) => {
            execute_drop_constraint(graph, dc).map_err(to_cypher_error)
        }
        Statement::Copy(c) => execute_copy(graph, c, params).map_err(to_cypher_error),
        Statement::ExportDatabase(e) => {
            execute_export_db(graph, e, params).map_err(to_cypher_error)
        }
        Statement::ImportDatabase(i) => {
            execute_import_db(graph, i, params).map_err(to_cypher_error)
        }
        Statement::Pipeline(stmts) => execute_pipeline(graph, stmts, params, registry),
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

    /// A typed hop between two labeled endpoints that the data schema never
    /// connects is pruned to a zero-row plan: the query returns no rows (and a
    /// grouping-free `count` returns 0), and `EXPLAIN` shows the pruning `Limit`.
    /// A satisfiable pattern over the same graph is untouched and returns rows.
    #[test]
    fn type_inference_prunes_unsatisfiable_pattern() {
        let (_dir, graph) = setup_graph();
        // Cities KNOW only other cities; people KNOW only other people. The
        // schema therefore contains City-KNOWS->City and Person-KNOWS->Person,
        // but never City-KNOWS->Person.
        let london = graph
            .add_node("City", &serde_json::json!({"n": "London"}))
            .unwrap();
        let paris = graph
            .add_node("City", &serde_json::json!({"n": "Paris"}))
            .unwrap();
        let alice = insert_person(&graph, "Alice", 30, "London");
        let bob = insert_person(&graph, "Bob", 25, "Paris");
        graph
            .add_edge(london, paris, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(alice, bob, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph.rebuild_csr().unwrap();

        let params = HashMap::new();

        // Unsatisfiable: no City KNOWS a Person.
        let res = execute(
            &graph,
            "MATCH (a:City)-[:KNOWS]->(b:Person) RETURN b.n AS n",
            &params,
        )
        .unwrap();
        assert_eq!(
            res.records.len(),
            0,
            "unsatisfiable pattern returns no rows"
        );

        // The grouping-free count over the same impossible pattern is 0, in one row.
        let res = execute(
            &graph,
            "MATCH (a:City)-[:KNOWS]->(b:Person) RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0].as_i64().unwrap(), 0);

        // The plan carries the pruning zero-row Limit.
        let plan = explain(
            &graph,
            "MATCH (a:City)-[:KNOWS]->(b:Person) RETURN b.n AS n",
        )
        .unwrap();
        assert!(
            plan.contains("count=0"),
            "expected a zero-row Limit in the pruned plan, got:\n{plan}"
        );

        // Satisfiable over the same graph: City-KNOWS->City exists, so the
        // pattern is not pruned and returns the real row.
        let res = execute(
            &graph,
            "MATCH (a:City)-[:KNOWS]->(b:City) RETURN b.n AS n",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0].as_str().unwrap(), "Paris");
        let plan = explain(&graph, "MATCH (a:City)-[:KNOWS]->(b:City) RETURN b.n AS n").unwrap();
        assert!(
            !plan.contains("count=0"),
            "satisfiable pattern must not be pruned, got:\n{plan}"
        );
    }

    /// `ORDER BY vector_dist(n, $q) LIMIT k` over a labeled scan lowers to the
    /// `VectorTopK` index search, and returns the same ranking the exact row
    /// pipeline would (here, ascending L2 from the origin).
    #[test]
    fn vector_topk_lowers_and_matches_exact() {
        use issundb_vector::{
            VectorGraphExt, VectorIndexOptions, VectorMetric, VectorQuantization,
        };
        let (_dir, graph) = setup_graph();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::L2,
                quantization: VectorQuantization::Float32,
            })
            .unwrap();
        // Five docs at increasing distance from the origin.
        let coords = [
            (10, [0.0_f32, 0.0]),
            (20, [1.0, 0.0]),
            (30, [2.0, 0.0]),
            (40, [3.0, 0.0]),
            (50, [4.0, 0.0]),
        ];
        for (id, v) in coords {
            let n = graph
                .add_node("Doc", &serde_json::json!({"id": id}))
                .unwrap();
            graph.upsert_vector(n, &v).unwrap();
        }
        graph.rebuild_csr().unwrap();

        let mut params = HashMap::new();
        params.insert("q".to_string(), serde_json::json!([0.0, 0.0]));

        let q = "MATCH (n:Doc) RETURN n.id AS id ORDER BY vector_dist(n, $q) LIMIT 3";

        // The plan must use the index search, not a full sort.
        let plan = explain(&graph, q).unwrap();
        assert!(
            plan.contains("VectorTopK"),
            "expected VectorTopK in plan, got:\n{plan}"
        );

        let res = execute(&graph, q, &params).unwrap();
        let ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![10, 20, 30]);

        // SKIP applies after the index ranking: skip the 2 nearest, take 2.
        let res = execute(
            &graph,
            "MATCH (n:Doc) RETURN n.id AS id ORDER BY vector_dist(n, $q) SKIP 2 LIMIT 2",
            &params,
        )
        .unwrap();
        let ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![30, 40]);
    }

    /// A `WHERE` equality predicate over the ranked variable is pushed into the
    /// index traversal as a pre-filter, so the top-k is taken among only the matching nodes.
    /// The nearer non-matching nodes are skipped, not returned and then discarded.
    #[test]
    fn vector_topk_pushes_down_equality_prefilter() {
        use issundb_vector::{
            VectorGraphExt, VectorIndexOptions, VectorMetric, VectorQuantization,
        };
        let (_dir, graph) = setup_graph();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::L2,
                quantization: VectorQuantization::Float32,
            })
            .unwrap();
        // The two nodes nearest the origin are French; the English ones are
        // farther. A pre-filtered top-2 must return the English ones.
        let rows = [
            (1, "fr", [0.0_f32, 0.0]),
            (2, "fr", [1.0, 0.0]),
            (3, "en", [2.0, 0.0]),
            (4, "en", [3.0, 0.0]),
            (5, "en", [9.0, 0.0]),
        ];
        for (id, lang, v) in rows {
            let n = graph
                .add_node("Doc", &serde_json::json!({"id": id, "lang": lang}))
                .unwrap();
            graph.upsert_vector(n, &v).unwrap();
        }
        graph.rebuild_csr().unwrap();

        let mut params = HashMap::new();
        params.insert("q".to_string(), serde_json::json!([0.0, 0.0]));

        let q = "MATCH (n:Doc) WHERE n.lang = 'en' RETURN n.id AS id \
                 ORDER BY vector_dist(n, $q) LIMIT 2";
        let plan = explain(&graph, q).unwrap();
        assert!(
            plan.contains("VectorTopK") && plan.contains("n.lang = "),
            "expected VectorTopK with pushed-down lang filter, got:\n{plan}"
        );
        let res = execute(&graph, q, &params).unwrap();
        let ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4]);
    }

    /// Descending order is not index-accelerable, so the plan keeps the row
    /// pipeline (`Sort`) and still returns the farthest-first ranking.
    #[test]
    fn vector_topk_descending_falls_back_to_sort() {
        use issundb_vector::{
            VectorGraphExt, VectorIndexOptions, VectorMetric, VectorQuantization,
        };
        let (_dir, graph) = setup_graph();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::L2,
                quantization: VectorQuantization::Float32,
            })
            .unwrap();
        for (id, v) in [(1, [0.0_f32, 0.0]), (2, [5.0, 0.0])] {
            let n = graph
                .add_node("Doc", &serde_json::json!({"id": id}))
                .unwrap();
            graph.upsert_vector(n, &v).unwrap();
        }
        graph.rebuild_csr().unwrap();

        let mut params = HashMap::new();
        params.insert("q".to_string(), serde_json::json!([0.0, 0.0]));

        let q = "MATCH (n:Doc) RETURN n.id AS id ORDER BY vector_dist(n, $q) DESC LIMIT 2";
        let plan = explain(&graph, q).unwrap();
        assert!(
            !plan.contains("VectorTopK"),
            "descending order must not lower to VectorTopK:\n{plan}"
        );
        let res = execute(&graph, q, &params).unwrap();
        let ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![2, 1]);
    }

    /// `vector_dist(a, b)` computes the distance under the graph's configured
    /// metric. Either argument may be a node (its stored embedding is resolved)
    /// or a numeric vector. A node with no embedding yields `null`.
    #[test]
    fn vector_dist_scalar_function() {
        use issundb_vector::{
            VectorGraphExt, VectorIndexOptions, VectorMetric, VectorQuantization,
        };
        let (_dir, graph) = setup_graph();
        graph
            .configure_vector_index(VectorIndexOptions {
                metric: VectorMetric::L2,
                quantization: VectorQuantization::Float32,
            })
            .unwrap();
        let a = graph
            .add_node("Doc", &serde_json::json!({"id": 1}))
            .unwrap();
        let b = graph
            .add_node("Doc", &serde_json::json!({"id": 2}))
            .unwrap();
        // A third node with no embedding.
        graph
            .add_node("Doc", &serde_json::json!({"id": 3}))
            .unwrap();
        graph.upsert_vector(a, &[0.0, 0.0]).unwrap();
        graph.upsert_vector(b, &[3.0, 4.0]).unwrap();
        // c has no embedding.
        graph.rebuild_csr().unwrap();

        let mut params = HashMap::new();
        params.insert("q".to_string(), serde_json::json!([0.0, 0.0]));

        // Explicit two-vector form: squared L2 between (0,0) and (3,4) is 25.
        let res = execute(
            &graph,
            "RETURN vector_dist([0.0, 0.0], [3.0, 4.0]) AS d",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0].as_f64().unwrap(), 25.0);

        // Node-vs-query-vector form, ordered ascending by distance: a (0) before b (25).
        let res = execute(
            &graph,
            "MATCH (n:Doc) WHERE n.id <> 3 RETURN n.id AS id ORDER BY vector_dist(n, $q)",
            &params,
        )
        .unwrap();
        let ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2]);

        // A node without an embedding yields null.
        let res = execute(
            &graph,
            "MATCH (n:Doc) WHERE n.id = 3 RETURN vector_dist(n, $q) AS d",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::Value::Null);
    }

    /// Run `setup` then `query`, returning the single scalar value of the one expected row.
    fn agg_scalar(setup: &[&str], query: &str) -> serde_json::Value {
        let params = HashMap::new();
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(dir.path(), 1).unwrap();
        for s in setup {
            execute(&graph, s, &params).unwrap();
        }
        let res = execute(&graph, query, &params).unwrap();
        assert_eq!(res.records.len(), 1, "expected exactly one aggregated row");
        res.records[0].values[0].clone()
    }

    /// `sum()` over integers preserves integer typing (openCypher numeric rules).
    #[test]
    fn sum_over_integers_is_integer() {
        let v = agg_scalar(&[], "UNWIND [1, 2, 3, 4, 5] AS x RETURN sum(x) AS s");
        assert_eq!(v, serde_json::json!(15));
        assert!(
            v.is_i64(),
            "sum of integers must be an integer, got {:?}",
            v
        );
    }

    /// `percentileDisc` accepts the percentile as a parameter, evaluated at run time.
    #[test]
    fn percentile_disc_with_parameter() {
        let mut params = HashMap::new();
        params.insert("p".to_string(), serde_json::json!(0.5));
        let (_dir, graph) = setup_graph();
        for q in [
            "CREATE ({price: 10.0})",
            "CREATE ({price: 20.0})",
            "CREATE ({price: 30.0})",
        ] {
            execute(&graph, q, &params).unwrap();
        }
        let res = execute(
            &graph,
            "MATCH (n) RETURN percentileDisc(n.price, $p) AS p",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!(20.0));
    }

    /// An out-of-range percentile is an error.
    #[test]
    fn percentile_out_of_range_is_error() {
        let mut params = HashMap::new();
        params.insert("p".to_string(), serde_json::json!(1.1));
        let (_dir, graph) = setup_graph();
        execute(&graph, "CREATE ({price: 10.0})", &params).unwrap();
        assert!(
            execute(
                &graph,
                "MATCH (n) RETURN percentileDisc(n.price, $p) AS p",
                &params,
            )
            .is_err()
        );
    }

    /// An aggregation in WHERE is rejected.
    #[test]
    fn aggregation_in_where_is_rejected() {
        assert!(parser::parse("MATCH (n) WHERE count(n) > 1 RETURN n").is_err());
    }

    /// A non-variable WITH expression must be aliased.
    #[test]
    fn unaliased_with_expression_is_rejected() {
        assert!(parser::parse("MATCH (n) WITH n.name RETURN n").is_err());
    }

    /// DELETE over an undirected expand deletes the shared edge before the nodes.
    #[test]
    fn delete_undirected_expand_then_count() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        execute(&graph, "CREATE ()-[:R]->()", &params).unwrap();
        graph.rebuild_csr().unwrap();
        let res = execute(
            &graph,
            "MATCH (a)-[r]-(b) DELETE r, a, b RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!(2));
    }

    /// Property access on a node reached through an expression reads user
    /// properties, not the wrapper's internal id.
    #[test]
    fn property_access_on_expression_node() {
        let v = agg_scalar(
            &["CREATE (:Person {name: 'Alice', age: 30})"],
            "MATCH (p:Person) WITH [p] AS list RETURN list[0].age AS a",
        );
        assert_eq!(v, serde_json::json!(30));
    }

    /// `type()` resolves a relationship reached through an Any-typed expression.
    #[test]
    fn type_of_relationship_via_any() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        execute(&graph, "CREATE ()-[:KNOWS]->()", &params).unwrap();
        graph.rebuild_csr().unwrap();
        let res = execute(
            &graph,
            "MATCH ()-[r]->() WITH [r, 1] AS list RETURN type(list[0]) AS t",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!("KNOWS"));
    }

    /// `labels()` on null returns null.
    #[test]
    fn labels_on_null_is_null() {
        let v = agg_scalar(&[], "OPTIONAL MATCH (n:DoesNotExist) RETURN labels(n) AS l");
        assert_eq!(v, serde_json::json!(null));
    }

    /// `properties()` on a non-graph scalar is an error.
    #[test]
    fn properties_on_scalar_is_error() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        assert!(execute(&graph, "RETURN properties(1) AS p", &params).is_err());
    }

    /// `type()` on a node variable is rejected at compile time.
    #[test]
    fn type_on_node_variable_is_rejected() {
        assert!(parser::parse("MATCH (r) RETURN type(r)").is_err());
    }

    /// Deleting a non-graph expression (arithmetic) is rejected at compile time.
    #[test]
    fn delete_integer_expression_is_rejected() {
        assert!(parser::parse("MATCH () DELETE 1 + 1").is_err());
    }

    /// `DELETE n:Label` is rejected; labels are removed with REMOVE, not DELETE.
    #[test]
    fn delete_label_is_rejected() {
        assert!(parser::parse("MATCH (n) DELETE n:Person").is_err());
    }

    /// MERGE binds the relationship variable so a following RETURN can reference it.
    #[test]
    fn merge_binds_relationship_variable() {
        let v = agg_scalar(&[], "MERGE (a:A)-[r:KNOWS]->(b:B) RETURN type(r) AS t");
        assert_eq!(v, serde_json::json!("KNOWS"));
    }

    /// MERGE on an existing pattern matches it rather than creating a duplicate.
    #[test]
    fn merge_matches_existing_without_duplicating() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        execute(&graph, "CREATE (:A)-[:KNOWS]->(:B)", &params).unwrap();
        let res = execute(
            &graph,
            "MERGE (a:A)-[r:KNOWS]->(b:B) RETURN type(r) AS t",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1, "MERGE should match exactly one row");
        // No second relationship was created. Rebuild the CSR snapshot first so the
        // read-path MATCH sees the storage state.
        graph.rebuild_csr().unwrap();
        let count = execute(
            &graph,
            "MATCH (:A)-[r:KNOWS]->(:B) RETURN count(r) AS c",
            &params,
        )
        .unwrap();
        assert_eq!(count.records[0].values[0], serde_json::json!(1));
    }

    /// ON MATCH SET runs when the pattern already exists; ON CREATE SET does not.
    #[test]
    fn merge_on_match_runs_for_existing() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        execute(&graph, "CREATE (:A {tag: 'old'})", &params).unwrap();
        let res = execute(
            &graph,
            "MERGE (a:A) ON CREATE SET a.tag = 'created' ON MATCH SET a.tag = 'matched' RETURN a.tag AS tag",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!("matched"));
    }

    /// MERGE can reference a bound node's property to constrain the merged pattern.
    #[test]
    fn merge_uses_bound_node_property() {
        let v = agg_scalar(
            &["CREATE (:Person {name: 'A'})"],
            "MATCH (p:Person) MERGE (c:City {name: p.name}) RETURN c.name AS n",
        );
        assert_eq!(v, serde_json::json!("A"));
    }

    /// A merged relationship without a type is rejected at compile time.
    #[test]
    fn merge_relationship_without_type_is_rejected() {
        assert!(parser::parse("MERGE (a)-[r]->(b)").is_err());
    }

    /// Merging a node that is already bound is rejected.
    #[test]
    fn merge_already_bound_node_is_rejected() {
        assert!(parser::parse("MATCH (a) MERGE (a)").is_err());
    }

    /// An undefined variable in an ON MATCH SET action is rejected.
    #[test]
    fn merge_undefined_variable_in_on_clause_is_rejected() {
        assert!(parser::parse("MERGE (n) ON MATCH SET x.num = 1").is_err());
    }

    /// Aggregating in RETURN after CREATE reflects the created rows.
    #[test]
    fn aggregate_after_create() {
        let v = agg_scalar(
            &[],
            "UNWIND [1, 2, 3, 4, 5] AS x CREATE (n:N {num: x}) RETURN sum(n.num) AS sum",
        );
        assert_eq!(v, serde_json::json!(15));
    }

    /// Aggregating in RETURN after SET reflects the post-mutation property values.
    #[test]
    fn aggregate_after_set_property() {
        let setup = [
            "CREATE (:N {num:1})",
            "CREATE (:N {num:2})",
            "CREATE (:N {num:3})",
            "CREATE (:N {num:4})",
            "CREATE (:N {num:5})",
        ];
        let v = agg_scalar(
            &setup,
            "MATCH (n:N) SET n.num = n.num + 1 RETURN sum(n.num) AS sum",
        );
        assert_eq!(v, serde_json::json!(20));
    }

    /// Adding a label via SET does not change the matched-row cardinality.
    #[test]
    fn aggregate_after_set_label() {
        let setup = [
            "CREATE (:N {num:1})",
            "CREATE (:N {num:2})",
            "CREATE (:N {num:3})",
            "CREATE (:N {num:4})",
            "CREATE (:N {num:5})",
        ];
        let v = agg_scalar(&setup, "MATCH (n:N) SET n:Foo RETURN sum(n.num) AS sum");
        assert_eq!(v, serde_json::json!(15));
    }

    /// Removing the matched label does not drop rows from the aggregation.
    #[test]
    fn aggregate_after_remove_label() {
        let setup = ["CREATE (:N)", "CREATE (:N)", "CREATE (:N)"];
        let v = agg_scalar(&setup, "MATCH (n:N) REMOVE n:N RETURN count(*) AS c");
        assert_eq!(v, serde_json::json!(3));
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

    /// Without an `AS` alias, a projected column takes the verbatim source text of
    /// the expression (preserving case and whitespace); an explicit alias overrides it.
    #[test]
    fn verbatim_column_names_preserve_source_text() {
        let params = HashMap::new();
        let (_dir, graph) = setup_graph();
        graph.add_node("N", &serde_json::json!({})).unwrap();
        graph.rebuild_csr().unwrap();

        let res = execute(&graph, "MATCH (n) RETURN cOuNt( * )", &params).unwrap();
        assert_eq!(res.columns, vec!["cOuNt( * )".to_string()]);

        let res = execute(&graph, "RETURN 1 +  2", &params).unwrap();
        assert_eq!(res.columns, vec!["1 +  2".to_string()]);

        let res = execute(&graph, "RETURN 1 + 2 AS total", &params).unwrap();
        assert_eq!(res.columns, vec!["total".to_string()]);
    }

    #[test]
    fn call_procedure_standalone_and_in_query() {
        use crate::procedure::{CypherType, Procedure, ProcedureRegistry};
        let (_dir, graph) = setup_graph();
        let mut registry = ProcedureRegistry::new();
        registry.register(Procedure {
            name: "test.labels".to_string(),
            inputs: vec![],
            outputs: vec![("label".to_string(), CypherType::String)],
            rows: vec![vec![serde_json::json!("A")], vec![serde_json::json!("B")]],
        });
        let params = HashMap::new();

        // Standalone: a non-void call auto-projects its output field as a column.
        let res =
            execute_with_procedures(&graph, "CALL test.labels()", &params, &registry).unwrap();
        assert_eq!(res.columns, vec!["label".to_string()]);
        assert_eq!(res.records.len(), 2);

        // In-query: YIELD binds the output for a following RETURN.
        let res = execute_with_procedures(
            &graph,
            "CALL test.labels() YIELD label RETURN label",
            &params,
            &registry,
        )
        .unwrap();
        assert_eq!(res.columns, vec!["label".to_string()]);
        assert_eq!(res.records.len(), 2);
    }

    #[test]
    fn call_procedure_filters_by_argument_and_checks_type() {
        use crate::procedure::{CypherType, Procedure, ProcedureRegistry};
        let (_dir, graph) = setup_graph();
        let mut registry = ProcedureRegistry::new();
        registry.register(Procedure {
            name: "test.my.proc".to_string(),
            inputs: vec![("in".to_string(), CypherType::Integer)],
            outputs: vec![("out".to_string(), CypherType::String)],
            rows: vec![
                vec![serde_json::json!(1), serde_json::json!("one")],
                vec![serde_json::json!(2), serde_json::json!("two")],
            ],
        });
        let params = HashMap::new();

        let res = execute_with_procedures(
            &graph,
            "CALL test.my.proc(2) YIELD out RETURN out",
            &params,
            &registry,
        )
        .unwrap();
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], serde_json::json!("two"));

        // A wrong argument type is a compile-time error.
        assert!(
            execute_with_procedures(&graph, "CALL test.my.proc(true)", &params, &registry).is_err()
        );
    }

    /// A pattern comprehension introduces a new target-node variable usable in the
    /// transform, yields one element per match, and yields an empty list for an anchor
    /// with no matching outgoing relationship (TCK Pattern2 [4]).
    #[test]
    fn pattern_comprehension_introduces_target_variable() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a)-[:T]->(b {name: 'val'})-[:T]->(c)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let mut rows = run(&graph, "MATCH (n) RETURN [(n)-[:T]->(b) | b.name] AS list");
        rows.sort_by_key(|r| serde_json::to_string(&r[0]).unwrap());
        let mut expected = vec![
            vec![serde_json::json!([])],
            vec![serde_json::json!([null])],
            vec![serde_json::json!(["val"])],
        ];
        expected.sort_by_key(|r| serde_json::to_string(&r[0]).unwrap());
        assert_eq!(rows, expected);
    }

    /// A pattern comprehension introduces a relationship variable usable in the transform
    /// (TCK Pattern2 [5]).
    #[test]
    fn pattern_comprehension_introduces_relationship_variable() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a), (b), (c) CREATE (a)-[:T {name: 'val'}]->(b), (b)-[:T]->(c)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let mut rows = run(&graph, "MATCH (n) RETURN [(n)-[r:T]->() | r.name] AS list");
        rows.sort_by_key(|r| serde_json::to_string(&r[0]).unwrap());
        let mut expected = vec![
            vec![serde_json::json!([])],
            vec![serde_json::json!([null])],
            vec![serde_json::json!(["val"])],
        ];
        expected.sort_by_key(|r| serde_json::to_string(&r[0]).unwrap());
        assert_eq!(rows, expected);
    }

    /// A pattern comprehension may be the argument of an aggregation: `count(...)` over a
    /// comprehension counts one per anchor row, including empty matches (TCK Pattern2 [6]).
    #[test]
    fn pattern_comprehension_under_aggregation() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a:A), (:A), (:A) CREATE (a)-[:HAS]->()",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let rows = run(
            &graph,
            "MATCH (n:A) RETURN count([p = (n)-[:HAS]->() | p]) AS c",
        );
        assert_eq!(rows, vec![vec![serde_json::json!(3)]]);
    }

    /// A pattern comprehension nested inside a list comprehension resolves its anchor from
    /// a `__Node__` scalar (an element of `nodes(p)`) (TCK Pattern2 [7]).
    #[test]
    fn pattern_comprehension_nested_in_list_comprehension() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (n1:X {n: 1}), (m1:Y), (i1:Y), (i2:Y) \
             CREATE (n1)-[:T]->(m1), (m1)-[:T]->(i1), (m1)-[:T]->(i2) \
             CREATE (n2:X {n: 2}), (m2), (i3:L), (i4:Y) \
             CREATE (n2)-[:T]->(m2), (m2)-[:T]->(i3), (m2)-[:T]->(i4)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let mut rows = run(
            &graph,
            "MATCH p = (n:X)-->() RETURN n.n AS n, [x IN nodes(p) | size([(x)-->(:Y) | 1])] AS list",
        );
        rows.sort_by_key(|r| r[0].as_i64().unwrap());
        assert_eq!(rows[0][1], serde_json::json!([1, 2]));
        assert_eq!(rows[1][1], serde_json::json!([0, 1]));
    }

    /// A named path over a fusable linear chain (two directed single-hop expands with
    /// no filter between them) must still bind the path variable; the fused
    /// `ExpandChain` fast path skips `_path_` construction entirely, so fusion must
    /// not apply when the pattern binds a path.
    #[test]
    fn named_path_survives_fused_expand_chain() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a:X {n: 1}), (b), (c) CREATE (a)-[:T]->(b), (b)-[:T]->(c)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let rows = run(
            &graph,
            "MATCH p = (a:X)-[:T]->()-[:T]->() RETURN length(p) AS len, size(nodes(p)) AS n",
        );
        assert_eq!(rows, vec![vec![serde_json::json!(2), serde_json::json!(3)]]);
    }

    /// An inline label predicate on an OPTIONAL MATCH target node belongs to the optional
    /// pattern: when it eliminates every match, the bound left row is preserved with the
    /// optional variables null, not dropped (TCK Match7 [28]).
    fn optional_match_label_fixture() -> (TempDir, Graph) {
        let (dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (s:Single), (a:A {num: 42}), (b:B {num: 46}), (c:C) \
             CREATE (s)-[:REL]->(a), (s)-[:REL]->(b), (a)-[:REL]->(c), (b)-[:LOOP]->(b)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        (dir, graph)
    }

    #[test]
    fn optional_match_inline_label_preserves_null_row() {
        let (_dir, graph) = optional_match_label_fixture();
        let rows = run(
            &graph,
            "MATCH (n:Single) OPTIONAL MATCH (n)-[r]-(m:NonExistent) RETURN r",
        );
        assert_eq!(rows, vec![vec![serde_json::Value::Null]]);
    }

    /// An undirected single hop between two already-bound nodes is a closing hop:
    /// the optimizer rewrites it to a `MultiwayJoin` with `closing_is_undirected`.
    /// The executor must then check both edge directions, matching every
    /// undirected edge between the pair rather than only the outgoing ones.
    #[test]
    fn undirected_closing_hop_matches_both_directions() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a:Person {name: 'A'}), (b:Person {name: 'B'}) \
             CREATE (a)-[:KNOWS]->(b), (a)-[:LIKES]->(b), (b)-[:LIKES]->(a)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        // The first MATCH binds both `a` and `b` via the single KNOWS edge; the
        // second is an undirected closing hop on LIKES, which exists in both
        // directions between the pair, so it must match exactly twice.
        let rows = run(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             MATCH (a)-[r:LIKES]-(b) RETURN a.name, b.name",
        );
        assert_eq!(
            rows.len(),
            2,
            "undirected closing hop must match LIKES in both directions"
        );
        for row in &rows {
            assert_eq!(row[0], serde_json::json!("A"));
            assert_eq!(row[1], serde_json::json!("B"));
        }
    }

    /// A directed closing hop must remain directional: the same fixture matched
    /// with `->` sees only the single forward LIKES edge, confirming the
    /// undirected branch is not over-matching.
    #[test]
    fn directed_closing_hop_matches_one_direction() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a:Person {name: 'A'}), (b:Person {name: 'B'}) \
             CREATE (a)-[:KNOWS]->(b), (a)-[:LIKES]->(b), (b)-[:LIKES]->(a)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let rows = run(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             MATCH (a)-[r:LIKES]->(b) RETURN a.name, b.name",
        );
        assert_eq!(
            rows.len(),
            1,
            "directed closing hop must match only the forward LIKES edge"
        );
    }

    /// A directed closing hop must preserve parallel-edge multiplicity: two
    /// distinct LIKES edges between the same bound pair are two matches, one
    /// per edge, exactly like a plain `Expand` would produce.
    #[test]
    fn directed_closing_hop_matches_parallel_edges() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a:Person {name: 'A'}), (b:Person {name: 'B'}) \
             CREATE (a)-[:KNOWS]->(b), (a)-[:LIKES {n: 1}]->(b), (a)-[:LIKES {n: 2}]->(b)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        // The cyclic single pattern makes the LIKES hop a closing hop (both
        // endpoints already bound), which the optimizer rewrites to a
        // `MultiwayJoin`; a separate MATCH clause would plan as a `HashJoin`
        // and never exercise this path.
        let mut rows = run(
            &graph,
            "MATCH (a:Person)-[:KNOWS]->(b:Person)<-[r:LIKES]-(a) RETURN r.n",
        );
        rows.sort_by_key(|r| r[0].as_i64().unwrap());
        assert_eq!(
            rows,
            vec![vec![serde_json::json!(1)], vec![serde_json::json!(2)],],
            "directed closing hop must emit one row per parallel edge"
        );
    }

    /// `LIMIT` behind an `Expand` streams: the result is a prefix of the
    /// unbounded result, and asking for fewer rows than exist returns exactly
    /// that many. This exercises the lazy scan-then-expand short-circuit path.
    #[test]
    fn limit_behind_expand_returns_prefix() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        // A small star: one hub KNOWS several leaves, so a single source node
        // fans out to many Expand output rows.
        execute(
            &graph,
            "CREATE (h:Person {name: 'hub'}), \
             (a:Person {name: 'a'}), (b:Person {name: 'b'}), \
             (c:Person {name: 'c'}), (d:Person {name: 'd'}) \
             CREATE (h)-[:KNOWS]->(a), (h)-[:KNOWS]->(b), \
             (h)-[:KNOWS]->(c), (h)-[:KNOWS]->(d)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let full = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name",
        );
        assert_eq!(full.len(), 4, "fixture must expose four KNOWS edges");

        // LIMIT below the full count returns exactly that many rows, each one a
        // row the unbounded query also produced (LIMIT without ORDER BY may
        // return any subset, so we assert membership, not position).
        let limited = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name LIMIT 2",
        );
        assert_eq!(limited.len(), 2, "LIMIT 2 must cap the streamed expansion");
        for row in &limited {
            assert!(full.contains(row), "limited row {row:?} not in full result");
        }

        // A LIMIT at or above the full count returns the whole result.
        let limited_all = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name LIMIT 100",
        );
        assert_eq!(limited_all.len(), 4);
    }

    /// `SKIP ... LIMIT` behind an `Expand` is consistent with the unbounded
    /// result: skip + limit selects a contiguous window of the streamed rows.
    #[test]
    fn skip_limit_behind_expand_is_consistent() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (h:Person {name: 'hub'}), \
             (a:Person {name: 'a'}), (b:Person {name: 'b'}), (c:Person {name: 'c'}) \
             CREATE (h)-[:KNOWS]->(a), (h)-[:KNOWS]->(b), (h)-[:KNOWS]->(c)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let full = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name",
        );
        let windowed = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name SKIP 1 LIMIT 1",
        );
        assert_eq!(windowed.len(), 1, "SKIP 1 LIMIT 1 yields one row");
        assert!(full.contains(&windowed[0]));
    }

    /// `RETURN DISTINCT ... LIMIT n` deduplicates before the limit applies: the
    /// limit caps the distinct rows, not the raw expansion rows.
    #[test]
    fn distinct_applies_before_skip_and_limit() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        // One hub fanning out to eight leaves over five distinct cities, with
        // the duplicated cities first in insertion order so a limit applied
        // before deduplication surfaces fewer than five cities.
        execute(
            &graph,
            "CREATE (h:Person {name: 'hub', city: 'hub'}), \
             (a:Person {name: 'a', city: 'ams'}), (b:Person {name: 'b', city: 'ams'}), \
             (c:Person {name: 'c', city: 'ams'}), (d:Person {name: 'd', city: 'ber'}), \
             (e:Person {name: 'e', city: 'ber'}), (f:Person {name: 'f', city: 'kyoto'}), \
             (g:Person {name: 'g', city: 'oslo'}), (i:Person {name: 'i', city: 'rio'}) \
             CREATE (h)-[:KNOWS]->(a), (h)-[:KNOWS]->(b), (h)-[:KNOWS]->(c), \
             (h)-[:KNOWS]->(d), (h)-[:KNOWS]->(e), (h)-[:KNOWS]->(f), \
             (h)-[:KNOWS]->(g), (h)-[:KNOWS]->(i)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();

        let rows = run(
            &graph,
            "MATCH (h:Person)-[:KNOWS]->(b:Person) WHERE h.name = 'hub' \
             RETURN DISTINCT b.city AS city ORDER BY city LIMIT 5",
        );
        assert_eq!(
            rows,
            vec![
                vec![serde_json::json!("ams")],
                vec![serde_json::json!("ber")],
                vec![serde_json::json!("kyoto")],
                vec![serde_json::json!("oslo")],
                vec![serde_json::json!("rio")],
            ],
            "DISTINCT must deduplicate before LIMIT caps the rows"
        );

        // SKIP windows over the deduplicated, sorted rows as well.
        let windowed = run(
            &graph,
            "MATCH (h:Person)-[:KNOWS]->(b:Person) WHERE h.name = 'hub' \
             RETURN DISTINCT b.city AS city ORDER BY city SKIP 1 LIMIT 2",
        );
        assert_eq!(
            windowed,
            vec![
                vec![serde_json::json!("ber")],
                vec![serde_json::json!("kyoto")],
            ],
            "SKIP and LIMIT must window the distinct rows"
        );
    }

    /// A grouped aggregation over a streamable child folds rows a batch at a
    /// time. The fixture has more expansion rows than `STREAM_BATCH`, so the
    /// per-group counters must accumulate correctly across batch boundaries and
    /// match the materialized result.
    #[test]
    fn streaming_aggregation_groups_across_batches() {
        let (_dir, graph) = setup_graph();
        // Two sink nodes; many sources, each pointing at one sink. The expansion
        // produces 600 rows (more than one 256-row batch), split into two groups.
        let even = graph
            .add_node("Person", &serde_json::json!({"name": "even"}))
            .unwrap();
        let odd = graph
            .add_node("Person", &serde_json::json!({"name": "odd"}))
            .unwrap();
        let (mut n_even, mut n_odd) = (0i64, 0i64);
        for i in 0..600 {
            let src = graph
                .add_node("Person", &serde_json::json!({"name": format!("s{i}")}))
                .unwrap();
            if i % 2 == 0 {
                graph
                    .add_edge(src, even, "KNOWS", &serde_json::json!({}))
                    .unwrap();
                n_even += 1;
            } else {
                graph
                    .add_edge(src, odd, "KNOWS", &serde_json::json!({}))
                    .unwrap();
                n_odd += 1;
            }
        }
        graph.rebuild_csr().unwrap();

        let rows = run(
            &graph,
            "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q.name AS name, count(*) AS c",
        );
        let mut counts: HashMap<String, i64> = HashMap::new();
        for row in rows {
            let name = row[0].as_str().expect("group key is a string").to_string();
            let c = row[1].as_i64().expect("count is an integer");
            counts.insert(name, c);
        }
        assert_eq!(counts.get("even").copied(), Some(n_even));
        assert_eq!(counts.get("odd").copied(), Some(n_odd));
    }

    /// A WHERE attached to an OPTIONAL MATCH is part of the optional pattern: filtering out
    /// every optional match still preserves the left row with null optional vars (TCK
    /// MatchWhere6 [2]).
    #[test]
    fn optional_match_where_label_preserves_null_row() {
        let (_dir, graph) = optional_match_label_fixture();
        let rows = run(
            &graph,
            "MATCH (n:Single) OPTIONAL MATCH (n)-[r]-(m) WHERE m:NonExistent RETURN r",
        );
        assert_eq!(rows, vec![vec![serde_json::Value::Null]]);
    }

    /// A WHERE on an OPTIONAL MATCH that some rows satisfy keeps the satisfying matches and
    /// does not drop the anchoring left row (TCK MatchWhere6 [1]).
    #[test]
    fn optional_match_where_keeps_satisfying_match() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a {name: 'A'}), (b:B {name: 'B'}), (c:C {name: 'C'}), (d:D {name: 'C'}) \
             CREATE (a)-[:T]->(b), (a)-[:T]->(c), (a)-[:T]->(d)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let rows = run(
            &graph,
            "MATCH (a)-->(b) WHERE b:B OPTIONAL MATCH (a)-->(c) WHERE c:C RETURN a.name",
        );
        assert_eq!(rows, vec![vec![serde_json::json!("A")]]);
    }

    /// A property predicate in a WHERE on an OPTIONAL MATCH that matches nothing preserves
    /// the left row (TCK MatchWhere6 [4]).
    #[test]
    fn optional_match_where_property_predicate_preserves_left_row() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        execute(
            &graph,
            "CREATE (a), (b {name: 'Mark'}) CREATE (a)-[:T]->(b)",
            &params,
        )
        .unwrap();
        graph.rebuild_csr().unwrap();
        let rows = run(
            &graph,
            "MATCH (n)-->(x0) OPTIONAL MATCH (x0)-->(x1) WHERE x1.name = 'bar' RETURN x0.name",
        );
        assert_eq!(rows, vec![vec![serde_json::json!("Mark")]]);
    }

    // Helper: run a bare `RETURN <expr> AS v` and return the single scalar value.
    fn scalar(graph: &Graph, expr: &str) -> serde_json::Value {
        let cypher = format!("RETURN {} AS v", expr);
        let rows = run(graph, &cypher);
        assert_eq!(rows.len(), 1, "expected exactly one row for `{}`", expr);
        rows.into_iter().next().unwrap().into_iter().next().unwrap()
    }

    /// A parenthesized comparison is a complete boolean expression, not a link in a
    /// chained comparison. `(a = b) = c` must evaluate the left comparison and compare
    /// its boolean result to `c`, not desugar into the chain `a = b AND b = c`.
    #[test]
    fn parenthesized_comparison_is_not_chained() {
        let (_dir, graph) = setup_graph();
        // (1 = 1) = (1 = 1) is true = true = true. Chaining would give 1=1 AND 1=true = false.
        assert_eq!(scalar(&graph, "(1 = 1) = (1 = 1)"), serde_json::json!(true));
        assert_eq!(scalar(&graph, "(1 = 2) = (3 = 4)"), serde_json::json!(true));
        assert_eq!(
            scalar(&graph, "(1 = 1) = (1 = 2)"),
            serde_json::json!(false)
        );
        // IS NULL binds tighter than comparison, and the grouped form must agree.
        assert_eq!(
            scalar(&graph, "(false = true IS NULL) = (false = (true IS NULL))"),
            serde_json::json!(true)
        );
        // A genuine chained comparison (no parentheses) must still desugar correctly.
        assert_eq!(scalar(&graph, "1 < 2 < 3"), serde_json::json!(true));
        assert_eq!(scalar(&graph, "1 < 2 < 1"), serde_json::json!(false));
    }

    #[test]
    fn duration_between_extreme_years_calendar() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.between(date('-999999999-01-01'), date('+999999999-12-31')))"
            ),
            serde_json::Value::String("P1999999998Y11M30D".to_string())
        );
    }

    #[test]
    fn duration_inseconds_extreme_years() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inSeconds(localdatetime('-999999999-01-01'), localdatetime('+999999999-12-31T23:59:59')))"
            ),
            serde_json::Value::String("PT17531639991215H59M59S".to_string())
        );
    }

    #[test]
    fn duration_parse_fractional_month_cascades_into_days() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration('P0.75M'))"),
            serde_json::Value::String("P22DT19H51M49.5S".to_string())
        );
    }

    #[test]
    fn duration_parse_extended_iso_format() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration('P2012-02-02T14:37:21.545'))"),
            serde_json::Value::String("P2012Y2M2DT14H37M21.545S".to_string())
        );
    }

    #[test]
    fn datetime_fromepoch_builds_utc_datetime() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(datetime.fromepoch(416779, 999999999))"),
            serde_json::Value::String("1970-01-05T19:46:19.999999999Z".to_string())
        );
    }

    #[test]
    fn datetime_fromepochmillis_builds_utc_datetime() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(datetime.fromepochmillis(237821673987))"),
            serde_json::Value::String("1977-07-15T13:34:33.987Z".to_string())
        );
    }

    #[test]
    fn duration_cumulative_and_of_second_accessors() {
        let (_dir, graph) = setup_graph();
        let expr = "duration({years: 1, months: 4, days: 10, hours: 1, minutes: 1, seconds: 1, nanoseconds: 111111111})";
        let cols = "d.years, d.quarters, d.months, d.weeks, d.days, d.hours, d.minutes, d.seconds, d.milliseconds, d.microseconds, d.nanoseconds, d.quartersOfYear, d.monthsOfQuarter, d.monthsOfYear, d.daysOfWeek, d.minutesOfHour, d.secondsOfMinute, d.millisecondsOfSecond, d.microsecondsOfSecond, d.nanosecondsOfSecond";
        let rows = run(&graph, &format!("WITH {expr} AS d RETURN {cols}"));
        let got: Vec<i64> = rows[0].iter().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(
            got,
            vec![
                1,
                5,
                16,
                1,
                10,
                1,
                61,
                3661,
                3661111,
                3661111111,
                3661111111111,
                1,
                1,
                4,
                3,
                1,
                1,
                111,
                111111,
                111111111
            ]
        );
    }

    #[test]
    fn duration_between_negative_subsecond_normalizes_to_nonnegative_nanos() {
        let (_dir, graph) = setup_graph();
        let rows = run(
            &graph,
            "WITH duration.between(localdatetime('2018-01-02T10:00:00.1'), localdatetime('2018-01-01T10:00:00.2')) AS dur \
             RETURN toString(dur), dur.seconds, dur.nanosecondsOfSecond",
        );
        assert_eq!(
            rows[0][0],
            serde_json::Value::String("PT-23H-59M-59.9S".to_string())
        );
        assert_eq!(rows[0][1].as_i64(), Some(-86400));
        assert_eq!(rows[0][2].as_i64(), Some(100000000));
    }

    #[test]
    fn current_time_functions_share_one_statement_instant() {
        let (_dir, graph) = setup_graph();
        // Two current-time calls in one statement observe the same instant, so the difference
        // between them is exactly zero.
        for f in [
            "localtime()",
            "time()",
            "date()",
            "localdatetime()",
            "datetime()",
        ] {
            assert_eq!(
                scalar(&graph, &format!("toString(duration.inSeconds({f}, {f}))")),
                serde_json::Value::String("PT0S".to_string()),
                "expected zero duration for {f}"
            );
        }
    }

    // Cluster 1: toString() must serialize temporal values via their canonical ISO string,
    // not reject them as "list or map".
    #[test]
    fn tostring_of_date_returns_iso_string() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(date('1984-10-11'))"),
            serde_json::Value::String("1984-10-11".to_string())
        );
    }

    #[test]
    fn tostring_of_duration_returns_iso_string() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration('PT45S'))"),
            serde_json::Value::String("PT45S".to_string())
        );
    }

    // Cluster 2: fractional duration components must cascade into smaller units
    // rather than being truncated and discarded.
    #[test]
    fn duration_fractional_day_cascades_to_hours() {
        let (_dir, graph) = setup_graph();
        // 0.5 days = 12 hours.
        assert_eq!(
            scalar(&graph, "toString(duration('P0.5D'))"),
            serde_json::Value::String("PT12H".to_string())
        );
    }

    #[test]
    fn duration_fractional_minute_cascades_to_seconds() {
        let (_dir, graph) = setup_graph();
        // 0.75 minutes = 45 seconds.
        assert_eq!(
            scalar(&graph, "toString(duration('PT0.75M'))"),
            serde_json::Value::String("PT45S".to_string())
        );
    }

    #[test]
    fn duration_fractional_year_cascades_to_months() {
        let (_dir, graph) = setup_graph();
        // 1.5 years = 1 year, 6 months.
        assert_eq!(
            scalar(&graph, "toString(duration('P1.5Y'))"),
            serde_json::Value::String("P1Y6M".to_string())
        );
    }

    #[test]
    fn duration_mixed_fractional_day_with_months() {
        let (_dir, graph) = setup_graph();
        // 5 months + 1.5 days = 5 months, 1 day, 12 hours.
        assert_eq!(
            scalar(&graph, "toString(duration('P5M1.5D'))"),
            serde_json::Value::String("P5M1DT12H".to_string())
        );
    }

    // Cluster 4: selecting (projecting) a date from a base date plus a coarse override
    // must inherit the finer components from the base, not reset them to defaults.
    #[test]
    fn date_select_week_inherits_day_of_week_from_base() {
        let (_dir, graph) = setup_graph();
        // Base 1984-11-11 is a Sunday; selecting week 1 keeps that weekday, so the result is
        // the Sunday of ISO week 1 of 1984, which is 1984-01-08 (not the Monday 1984-01-02).
        assert_eq!(
            scalar(
                &graph,
                "toString(date({date: date({year: 1984, month: 11, day: 11}), week: 1}))"
            ),
            serde_json::Value::String("1984-01-08".to_string())
        );
    }

    #[test]
    fn date_select_quarter_inherits_day_of_quarter_from_base() {
        let (_dir, graph) = setup_graph();
        // Base 1984-11-11 has dayOfQuarter 42 (within Q4); selecting quarter 3 keeps that
        // dayOfQuarter, so the result is Jul 1 + 41 days = 1984-08-11 (not 1984-07-11).
        assert_eq!(
            scalar(
                &graph,
                "toString(date({date: date({year: 1984, month: 11, day: 11}), quarter: 3}))"
            ),
            serde_json::Value::String("1984-08-11".to_string())
        );
    }

    // Cluster 3 (offset rows only): a datetime string with an explicit numeric offset and a
    // bracketed IANA zone name must preserve the offset (normalized to +HH:MM) and keep the
    // zone-name suffix. Cases that require DST/historical resolution are out of scope here.
    #[test]
    fn datetime_named_zone_preserves_explicit_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime('2015-07-21T21:40:32.142+02:00[Europe/Stockholm]'))"
            ),
            serde_json::Value::String(
                "2015-07-21T21:40:32.142+02:00[Europe/Stockholm]".to_string()
            )
        );
    }

    #[test]
    fn datetime_named_zone_normalizes_compact_offset() {
        let (_dir, graph) = setup_graph();
        // +0845 must normalize to +08:45.
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime('2015-07-21T21:40:32.142+0845[Australia/Eucla]'))"
            ),
            serde_json::Value::String("2015-07-21T21:40:32.142+08:45[Australia/Eucla]".to_string())
        );
    }

    #[test]
    fn datetime_named_zone_normalizes_hour_only_offset() {
        let (_dir, graph) = setup_graph();
        // -04 must normalize to -04:00.
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime('2015-07-21T21:40:32.142-04[America/New_York]'))"
            ),
            serde_json::Value::String(
                "2015-07-21T21:40:32.142-04:00[America/New_York]".to_string()
            )
        );
    }

    // A timezone-aware datetime constructed without an explicit zone defaults to UTC, which
    // serializes with the `Z` designator. A local datetime stays zoneless.
    #[test]
    fn datetime_default_zone_serializes_with_z() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:123456789}))"
            ),
            serde_json::Value::String("1984-10-11T12:31:14.123456789Z".to_string())
        );
    }

    #[test]
    fn datetime_week_construction_defaults_to_utc() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(datetime({year:1816, week:1}))"),
            serde_json::Value::String("1816-01-01T00:00Z".to_string())
        );
    }

    #[test]
    fn localdatetime_stays_zoneless() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({year:1984, month:10, day:11, hour:12, minute:31, second:14}))"
            ),
            serde_json::Value::String("1984-10-11T12:31:14".to_string())
        );
    }

    // truncate: the output type follows the function name, not the input type, and time-field
    // overrides from the map (e.g. {nanosecond: 2}) must apply to the result.
    #[test]
    fn datetime_truncate_day_applies_nanosecond_override() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime.truncate('day', datetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:645876123, timezone:'+01:00'}), {nanosecond:2}))"
            ),
            serde_json::Value::String("1984-10-11T00:00:00.000000002+01:00".to_string())
        );
    }

    #[test]
    fn localtime_truncate_over_datetime_yields_zoneless_time() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localtime.truncate('day', datetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:645876123, timezone:'+01:00'}), {nanosecond:2}))"
            ),
            serde_json::Value::String("00:00:00.000000002".to_string())
        );
    }

    #[test]
    fn time_truncate_over_datetime_keeps_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time.truncate('hour', datetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:645876123, timezone:'+01:00'}), {nanosecond:2}))"
            ),
            serde_json::Value::String("12:00:00.000000002+01:00".to_string())
        );
    }

    #[test]
    fn localdatetime_truncate_microsecond_floors_subsecond() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime.truncate('microsecond', localdatetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:645876123})))"
            ),
            serde_json::Value::String("1984-10-11T12:31:14.645876".to_string())
        );
    }

    // Selection into localdatetime: a `{date: .., time: ..}` map must combine the selected
    // date and time. Previously localdatetime() lacked the selection branch and rejected these
    // maps with "must include at least 'year'".
    #[test]
    fn localdatetime_selects_date_and_time() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({date: date({year:1984, month:10, day:11}), time: localtime({hour:12, minute:31, second:14, nanosecond:645876123})}))"
            ),
            serde_json::Value::String("1984-10-11T12:31:14.645876123".to_string())
        );
    }

    #[test]
    fn localdatetime_selects_date_with_time_components() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({date: date({year:1984, month:10, day:11}), hour:10, minute:10, second:10}))"
            ),
            serde_json::Value::String("1984-10-11T10:10:10".to_string())
        );
    }

    // Field overrides combined with a time selector must replace only the named field and keep
    // the selected value's sub-second precision: overriding `second` keeps `.645876123`.
    #[test]
    fn localdatetime_time_override_keeps_subsecond() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({year:1984, month:10, day:11, time: localtime({hour:12, minute:31, second:14, nanosecond:645876123}), second:42}))"
            ),
            serde_json::Value::String("1984-10-11T12:31:42.645876123".to_string())
        );
    }

    // A `{datetime: ..}` selector with overrides applies the date override and keeps the
    // selected sub-second precision.
    #[test]
    fn localdatetime_selects_datetime_with_overrides() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({datetime: localdatetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, millisecond:645}), day:28, second:42}))"
            ),
            serde_json::Value::String("1984-10-28T12:31:42.645".to_string())
        );
    }

    // The same selection path for datetime() must default the zone to Z and keep sub-second
    // precision through a `second` override.
    #[test]
    fn datetime_time_override_keeps_subsecond_and_defaults_z() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime({year:1984, month:10, day:11, time: localtime({hour:12, minute:31, second:14, nanosecond:645876123}), second:42}))"
            ),
            serde_json::Value::String("1984-10-11T12:31:42.645876123Z".to_string())
        );
    }

    // A zoned time() with no explicit zone defaults to UTC and serializes with `Z`.
    #[test]
    fn time_construct_defaults_to_z() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time({hour:12, minute:31, second:14, nanosecond:645876123}))"
            ),
            serde_json::Value::String("12:31:14.645876123Z".to_string())
        );
    }

    // Selecting a time and overriding a field keeps the source sub-second precision; a local
    // source attaches the default `Z`.
    #[test]
    fn time_selects_localtime_override_defaults_z() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time({time: localtime({hour:12, minute:31, second:14, nanosecond:645876123}), second:42}))"
            ),
            serde_json::Value::String("12:31:42.645876123Z".to_string())
        );
    }

    // A timezone override on a zoned source preserves the instant: the wall-clock time shifts by
    // the offset difference.
    #[test]
    fn time_selects_zoned_with_timezone_override_shifts_walltime() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time({time: time({hour:12, minute:31, second:14, microsecond:645876, timezone:'+01:00'}), timezone:'+05:00'}))"
            ),
            serde_json::Value::String("16:31:14.645876+05:00".to_string())
        );
    }

    // A timezone override on a local source attaches the zone without shifting the wall clock.
    #[test]
    fn time_selects_local_with_timezone_override_attaches_no_shift() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time({time: localtime({hour:12, minute:31, second:14, nanosecond:645876123}), timezone:'+05:00'}))"
            ),
            serde_json::Value::String("12:31:14.645876123+05:00".to_string())
        );
    }

    // localtime() selection drops the source zone and keeps the wall-clock time.
    #[test]
    fn localtime_selects_time_drops_zone() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localtime({time: time({hour:12, minute:31, second:14, microsecond:645876, timezone:'+01:00'}), second:42}))"
            ),
            serde_json::Value::String("12:31:42.645876".to_string())
        );
    }

    // duration.inDays returns only whole days; the sub-day remainder is discarded.
    #[test]
    fn duration_in_days_discards_subday_remainder() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inDays(date('1984-10-11'), localdatetime('2016-07-21T21:45:22.142')))"
            ),
            serde_json::Value::String("P11606D".to_string())
        );
    }

    // When either operand is a pure time, duration.between compares only the time of day; the
    // date is dropped and months/days are zero.
    #[test]
    fn duration_between_time_and_datetime_compares_time_of_day() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.between(localtime('14:30'), localdatetime('2016-07-21T21:45:22.142')))"
            ),
            serde_json::Value::String("PT7H15M22.142S".to_string())
        );
    }

    // Two zoned operands are compared as instants: their offsets shift the wall clock before the
    // time-of-day difference.
    #[test]
    fn duration_in_seconds_zoned_time_and_datetime_adjusts_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inSeconds(time('14:30'), datetime('2015-07-21T21:40:32.142+0100')))"
            ),
            serde_json::Value::String("PT6H10M32.142S".to_string())
        );
    }

    // The whole-month count accounts for the time of day: going back to a later time of day on the
    // same day-of-month leaves the final month incomplete.
    #[test]
    fn duration_in_months_accounts_for_time_of_day() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inMonths(date('2018-07-21'), datetime('2016-07-21T21:40:32.142+0100')))"
            ),
            serde_json::Value::String("P-1Y-11M".to_string())
        );
    }

    // A negative time-of-day difference borrows a day so the components share one sign.
    #[test]
    fn duration_between_borrows_day_for_negative_time() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.between(datetime('2014-07-21T21:40:36.143+0200'), date('2015-06-24')))"
            ),
            serde_json::Value::String("P11M2DT2H19M23.857S".to_string())
        );
    }

    // Two zoned datetimes are reconciled to a common zone before the calendar difference.
    #[test]
    fn duration_between_two_zoned_datetimes_adjusts_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.between(datetime('2014-07-21T21:40:36.143+0200'), datetime('2015-07-21T21:40:32.142+0100')))"
            ),
            serde_json::Value::String("P1YT59M55.999S".to_string())
        );
    }

    // A sub-second-only negative duration keeps its sign even though the seconds field is zero.
    #[test]
    fn duration_negative_subsecond_keeps_sign() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inSeconds(localdatetime('2014-07-21T21:40:36.143'), localdatetime('2014-07-21T21:40:36.142')))"
            ),
            serde_json::Value::String("PT-0.001S".to_string())
        );
    }

    // Date plus or minus a duration ignores the duration's time component entirely.
    #[test]
    fn date_plus_duration_ignores_time_component() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(date({year:1984, month:10, day:11}) - duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:2}))"
            ),
            serde_json::Value::String("1972-04-27".to_string())
        );
    }

    // LocalTime plus a duration uses only the time component and wraps modulo 24 hours, keeping
    // nanosecond precision.
    #[test]
    fn localtime_plus_duration_wraps_modulo_day() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localtime({hour:12, minute:31, second:14, nanosecond:1}) + duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:2}))"
            ),
            serde_json::Value::String("04:44:24.000000003".to_string())
        );
    }

    // Time arithmetic keeps the zone offset.
    #[test]
    fn time_minus_duration_keeps_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time({hour:12, minute:31, second:14, nanosecond:1, timezone:'+01:00'}) - duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:2}))"
            ),
            serde_json::Value::String("20:18:03.999999999+01:00".to_string())
        );
    }

    // LocalDateTime arithmetic shifts the date by months and days, then adds the time component
    // (which can roll the date over), keeping nanosecond precision.
    #[test]
    fn localdatetime_plus_duration_rolls_over_date() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localdatetime({year:1984, month:10, day:11, hour:12, minute:31, second:14, nanosecond:1}) + duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:2}))"
            ),
            serde_json::Value::String("1997-03-26T04:44:24.000000003".to_string())
        );
    }

    // Multiplying a duration by an integer scales every component.
    #[test]
    fn duration_times_integer_scales_components() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:1}) * 2)"
            ),
            serde_json::Value::String("P24Y10M28DT32H26M20.000000002S".to_string())
        );
    }

    // Dividing a duration cascades the fractional month into days (at 30.436875 days per month)
    // and the fractional day into seconds.
    #[test]
    fn duration_divided_cascades_fractional_month() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration({years:12, months:5, days:14, hours:16, minutes:12, seconds:70, nanoseconds:1}) / 2)"
            ),
            serde_json::Value::String("P6Y2M22DT13H21M8S".to_string())
        );
    }

    // A fractional month in duration construction cascades into days (at 30.436875 days per
    // month) and the fractional day into seconds.
    #[test]
    fn duration_construct_fractional_month_cascades() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration({months: 0.75}))"),
            serde_json::Value::String("P22DT19H51M49.5S".to_string())
        );
    }

    // A fractional day cascades into the time component.
    #[test]
    fn duration_construct_fractional_day_cascades() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration({months: 5, days: 1.5}))"),
            serde_json::Value::String("P5M1DT12H".to_string())
        );
    }

    // A fractional minute cascades into seconds.
    #[test]
    fn duration_construct_fractional_minute_cascades() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(&graph, "toString(duration({minutes: 1.5, seconds: 1}))"),
            serde_json::Value::String("PT1M31S".to_string())
        );
    }

    // A fractional-component duration added to a date rolls whole days out of the seconds and
    // drops the sub-day remainder.
    #[test]
    fn date_plus_fractional_duration_rolls_whole_days() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(date({year:1984, month:10, day:11}) + duration({years:12.5, months:5.5, days:14.5, hours:16.5, minutes:12.5, seconds:70.5, nanoseconds:3}))"
            ),
            serde_json::Value::String("1997-10-11".to_string())
        );
    }

    // A named IANA zone resolves to its summer (DST) offset at a summer instant.
    #[test]
    fn datetime_named_zone_resolves_summer_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime({year: 2017, month: 8, day: 8, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: 'Europe/Stockholm'}))"
            ),
            serde_json::Value::String(
                "2017-08-08T12:31:14.645876123+02:00[Europe/Stockholm]".to_string()
            )
        );
    }

    // The same zone resolves to its winter (standard) offset at a winter instant.
    #[test]
    fn datetime_named_zone_resolves_winter_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, timezone: 'Europe/Stockholm'}))"
            ),
            serde_json::Value::String("1984-10-11T12:31+01:00[Europe/Stockholm]".to_string())
        );
    }

    // A historical instant before standardized zones resolves to the sub-minute local-mean-time
    // offset, formatted with seconds.
    #[test]
    fn datetime_named_zone_historical_lmt_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime('1818-07-21T21:40:32.142[Europe/Stockholm]'))"
            ),
            serde_json::Value::String(
                "1818-07-21T21:40:32.142+00:53:28[Europe/Stockholm]".to_string()
            )
        );
    }

    // A zone name on a parsed string with no explicit offset resolves at the instant.
    #[test]
    fn datetime_parse_named_zone_resolves_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(datetime('2015-07-21T21:40:32.142[Europe/London]'))"
            ),
            serde_json::Value::String("2015-07-21T21:40:32.142+01:00[Europe/London]".to_string())
        );
    }

    // Accessors report the IANA name in `timezone`, the resolved numeric `offset`, and the UTC
    // instant in `epochSeconds`.
    #[test]
    fn datetime_named_zone_accessors() {
        let (_dir, graph) = setup_graph();
        let q = "datetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: 'Europe/Stockholm'})";
        assert_eq!(
            scalar(&graph, &format!("({}).timezone", q)),
            serde_json::Value::String("Europe/Stockholm".to_string())
        );
        assert_eq!(
            scalar(&graph, &format!("({}).offset", q)),
            serde_json::Value::String("+01:00".to_string())
        );
        assert_eq!(
            scalar(&graph, &format!("({}).offsetMinutes", q)),
            serde_json::json!(60)
        );
        assert_eq!(
            scalar(&graph, &format!("({}).offsetSeconds", q)),
            serde_json::json!(3600)
        );
        assert_eq!(
            scalar(&graph, &format!("({}).epochSeconds", q)),
            serde_json::json!(469020674)
        );
    }

    // duration.between two zoned datetimes on opposite sides of a DST transition counts the real
    // elapsed seconds, accounting for the differing offsets.
    #[test]
    fn duration_between_zoned_across_dst_counts_real_seconds() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.between(datetime('2017-10-28T23:00+02:00[Europe/Stockholm]'), datetime('2017-10-29T04:00+01:00[Europe/Stockholm]')))"
            ),
            serde_json::Value::String("PT6H".to_string())
        );
    }

    // duration.inSeconds between a zoned datetime and a local one on a DST-transition day counts
    // real elapsed seconds: the local operand adopts the zoned operand's zone, so the fall-back
    // hour makes 00:00 to 04:00 span five hours, not four.
    #[test]
    fn duration_in_seconds_mixed_zone_local_across_dst_counts_real_hours() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inSeconds(datetime({year: 2017, month: 10, day: 29, hour: 0, timezone: 'Europe/Stockholm'}), localdatetime({year: 2017, month: 10, day: 29, hour: 4})))"
            ),
            serde_json::Value::String("PT5H".to_string())
        );
    }

    // The same holds when the local operand is a pure time that borrows the zoned operand's date.
    #[test]
    fn duration_in_seconds_zoned_datetime_vs_local_time_across_dst() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(duration.inSeconds(datetime({year: 2017, month: 10, day: 29, hour: 0, timezone: 'Europe/Stockholm'}), localtime({hour: 4})))"
            ),
            serde_json::Value::String("PT5H".to_string())
        );
    }

    // Truncating to a unit then setting a finer field keeps the truncated value of the coarser
    // sub-second components: truncate to the millisecond keeps 645ms, then nanosecond: 2 sets only
    // the nanosecond component.
    #[test]
    fn truncate_to_millisecond_with_nanosecond_override_keeps_millisecond() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(localtime.truncate('millisecond', localtime({hour:12, minute:31, second:14, nanosecond: 645876123}), {nanosecond: 2}))"
            ),
            serde_json::Value::String("12:31:14.645000002".to_string())
        );
    }

    // Sub-second accessors report cumulative totals: microsecond is the whole microseconds and
    // nanosecond the full sub-second in nanoseconds.
    #[test]
    fn time_accessors_report_cumulative_subsecond_totals() {
        let (_dir, graph) = setup_graph();
        let q = "localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123})";
        assert_eq!(
            scalar(&graph, &format!("({}).millisecond", q)),
            serde_json::json!(645)
        );
        assert_eq!(
            scalar(&graph, &format!("({}).microsecond", q)),
            serde_json::json!(645876)
        );
        assert_eq!(
            scalar(&graph, &format!("({}).nanosecond", q)),
            serde_json::json!(645876123)
        );
    }

    // A date exposes `weekDay` (the ISO day of week) and `dayOfQuarter` accessors.
    #[test]
    fn date_exposes_week_day_and_day_of_quarter() {
        let (_dir, graph) = setup_graph();
        let q = "date({year: 1984, month: 10, day: 11})";
        assert_eq!(
            scalar(&graph, &format!("({}).weekDay", q)),
            serde_json::json!(4)
        );
        assert_eq!(
            scalar(&graph, &format!("({}).dayOfQuarter", q)),
            serde_json::json!(11)
        );
    }

    // Reinterpreting a stored zoned datetime as a Time inherits its resolved numeric offset, not
    // the IANA zone name, and preserves the full sub-second.
    #[test]
    fn time_from_zoned_datetime_inherits_numeric_offset() {
        let (_dir, graph) = setup_graph();
        assert_eq!(
            scalar(
                &graph,
                "toString(time(datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: 'Europe/Stockholm'})))"
            ),
            serde_json::Value::String("12:31:14.645876+01:00".to_string())
        );
    }

    // with-orderBy: after a WITH projection, ORDER BY may only reference variables that the
    // WITH projects. Referencing an out-of-scope or never-defined variable is a compile error.
    #[test]
    fn order_by_out_of_scope_variable_after_with_is_error() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        // `c` is dropped by the intervening `WITH a, b`, so it is out of scope at the final
        // WITH's ORDER BY (neither its input scope {a, b} nor its output scope {a}).
        let res = execute(
            &graph,
            "WITH 1 AS a, 3 AS b, 5 AS c WITH a, b WITH a ORDER BY a, c RETURN a",
            &params,
        );
        assert!(res.is_err(), "expected error, got {:?}", res);
    }

    #[test]
    fn order_by_input_scope_variable_after_with_succeeds() {
        // A variable in the immediate input scope (bound upstream, not projected) is still
        // visible to ORDER BY, e.g. `ORDER BY a.count` after `WITH a.count AS count`.
        let (_dir, graph) = setup_graph();
        graph
            .add_node("N", &serde_json::json!({"count": 1}))
            .unwrap();
        let params = HashMap::new();
        let res = execute(
            &graph,
            "MATCH (a) WITH a.count AS count ORDER BY a.count RETURN count",
            &params,
        );
        assert!(res.is_ok(), "expected ok, got {:?}", res);
    }

    #[test]
    fn order_by_never_defined_variable_after_with_is_error() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        let res = execute(&graph, "WITH 1 AS a WITH a ORDER BY a, e RETURN a", &params);
        assert!(res.is_err(), "expected error, got {:?}", res);
    }

    #[test]
    fn order_by_in_scope_expression_after_with_succeeds() {
        // Guard against over-eager validation: an ORDER BY expression over projected
        // variables must still be accepted.
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();
        let res = execute(
            &graph,
            "WITH 1 AS a, 5 AS b WITH a, b ORDER BY a + b RETURN a",
            &params,
        );
        assert!(res.is_ok(), "expected ok, got {:?}", res);
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
    fn skip_and_limit_without_order_returns_correct_count() {
        // SKIP 2 LIMIT 3 over a bare scan exercises the bounded-scan fast path,
        // which caps the scan at skip + count. Without ORDER BY the rows are
        // arbitrary, but the count must be exactly 3 (not fewer because the cap
        // dropped rows needed for the skip).
        let (_dir, graph) = setup_graph();
        for i in 0..10i64 {
            graph
                .add_node("Item", &serde_json::json!({"i": i}))
                .unwrap();
        }
        let rows = run(&graph, "MATCH (n:Item) RETURN n.i AS i SKIP 2 LIMIT 3");
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
    fn relationship_uniqueness_within_pattern() {
        // The canonical co-developer query: a relationship matched by one hop
        // of a pattern must not be reused by another hop of the same pattern,
        // so marko is not his own co-developer through the single marko-CREATED->lop relationship.
        let (_dir, graph) = setup_graph();
        let marko = graph
            .add_node("Person", &serde_json::json!({"name": "marko"}))
            .unwrap();
        let josh = graph
            .add_node("Person", &serde_json::json!({"name": "josh"}))
            .unwrap();
        let lop = graph
            .add_node("Software", &serde_json::json!({"name": "lop"}))
            .unwrap();
        graph.add_edge(marko, lop, "CREATED", &()).unwrap();
        graph.add_edge(josh, lop, "CREATED", &()).unwrap();

        let rows = run(
            &graph,
            "MATCH (m:Person {name: 'marko'})-[:CREATED]->(s)<-[:CREATED]-(c) \
             RETURN c.name AS name",
        );
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(names, ["josh"]);
    }

    #[test]
    fn relationship_uniqueness_blocks_undirected_backtrack() {
        // With a single KNOWS relationship, an undirected two-hop pattern has
        // no valid assignment: the second hop may not traverse the first
        // hop's relationship back to the start.
        let (_dir, graph) = setup_graph();
        let a = graph
            .add_node("Person", &serde_json::json!({"name": "a"}))
            .unwrap();
        let b = graph
            .add_node("Person", &serde_json::json!({"name": "b"}))
            .unwrap();
        graph.add_edge(a, b, "KNOWS", &()).unwrap();

        let rows = run(
            &graph,
            "MATCH (x:Person {name: 'a'})-[:KNOWS]-(y)-[:KNOWS]-(z) RETURN z.name AS name",
        );
        assert!(
            rows.is_empty(),
            "backtracking over the same relationship must be rejected"
        );
    }

    #[test]
    fn relationship_reuse_across_match_clauses_is_allowed() {
        // Uniqueness is scoped to a single pattern: two separate MATCH
        // clauses may bind the same relationship.
        let (_dir, graph) = setup_graph();
        let a = graph
            .add_node("Person", &serde_json::json!({"name": "a"}))
            .unwrap();
        let b = graph
            .add_node("Person", &serde_json::json!({"name": "b"}))
            .unwrap();
        graph.add_edge(a, b, "KNOWS", &()).unwrap();

        let rows = run(
            &graph,
            "MATCH (x:Person {name: 'a'})-[r1:KNOWS]->(y) \
             MATCH (x)-[r2:KNOWS]->(y) \
             RETURN y.name AS name",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_str().unwrap(), "b");
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

    /// `CREATE INDEX FOR ()-[r:TYPE]-() ON (r.prop)` must create an edge
    /// property index that `add_edge` populates, and `DROP INDEX` must remove its entries.
    #[test]
    fn edge_index_ddl_roundtrip() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();

        execute(
            &graph,
            "CREATE INDEX FOR ()-[r:ROAD]-() ON (r.cost)",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "CREATE (a:City)-[r:ROAD {cost: 5}]->(b:City)",
            &params,
        )
        .unwrap();

        let hits = graph
            .edges_by_property("ROAD", "cost", issundb_core::PropValue::Int(5))
            .unwrap();
        assert_eq!(hits.len(), 1, "edge must be findable through the index");

        execute(&graph, "DROP INDEX FOR ()-[r:ROAD]-() ON (r.cost)", &params).unwrap();
        let hits = graph
            .edges_by_property("ROAD", "cost", issundb_core::PropValue::Int(5))
            .unwrap();
        assert!(hits.is_empty(), "dropped index must lose its entries");
    }

    /// A relationship unique constraint created via Cypher must reject a
    /// duplicate value on edge creation and stop doing so once dropped.
    #[test]
    fn edge_unique_constraint_ddl() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();

        execute(
            &graph,
            "CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT r.toll_id IS UNIQUE",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "CREATE (a:City)-[r:ROAD {toll_id: 1}]->(b:City)",
            &params,
        )
        .unwrap();
        let err = execute(
            &graph,
            "CREATE (a:City)-[r:ROAD {toll_id: 1}]->(b:City)",
            &params,
        );
        assert!(
            err.is_err(),
            "duplicate toll_id must violate the constraint"
        );

        execute(
            &graph,
            "DROP CONSTRAINT ON ()-[r:ROAD]-() ASSERT r.toll_id IS UNIQUE",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "CREATE (a:City)-[r:ROAD {toll_id: 1}]->(b:City)",
            &params,
        )
        .unwrap();
    }

    /// A relationship existence constraint created via Cypher must reject an
    /// edge that lacks the property.
    #[test]
    fn edge_exists_constraint_ddl() {
        let (_dir, graph) = setup_graph();
        let params = HashMap::new();

        execute(
            &graph,
            "CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT EXISTS(r.cost)",
            &params,
        )
        .unwrap();
        let err = execute(&graph, "CREATE (a:City)-[r:ROAD]->(b:City)", &params);
        assert!(err.is_err(), "missing cost must violate the constraint");

        execute(
            &graph,
            "DROP CONSTRAINT ON ()-[r:ROAD]-() ASSERT EXISTS(r.cost)",
            &params,
        )
        .unwrap();
        execute(&graph, "CREATE (a:City)-[r:ROAD]->(b:City)", &params).unwrap();
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
        // The body CREATE uses no properties; we simply verify that one node is created

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

    // --- collect_expr_vars regression: compound WHERE must not be pushed below bound vars ---

    #[test]
    fn where_and_does_not_cause_unbound_variable() {
        let (_dir, graph) = setup_graph();
        graph.add_node("N", &serde_json::json!({"v": 10})).unwrap();
        graph.add_node("N", &serde_json::json!({"v": 20})).unwrap();
        graph.add_node("N", &serde_json::json!({})).unwrap();
        // IS NOT NULL AND > in same WHERE must not push filter before the LabelScan.
        let rows = run(
            &graph,
            "MATCH (n:N) WHERE n.v IS NOT NULL AND n.v > 15 RETURN n.v AS v",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_i64().unwrap(), 20);
    }

    #[test]
    fn where_label_predicate_n_colon_label() {
        let (_dir, graph) = setup_graph();
        graph.add_node("A", &serde_json::json!({"x": 1})).unwrap();
        graph.add_node("B", &serde_json::json!({"x": 2})).unwrap();
        let rows = run(&graph, "MATCH (n) WHERE n:A RETURN n.x AS x");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_i64().unwrap(), 1);
    }

    #[test]
    fn optional_match_left_outer_join_nulls_unmatched() {
        let (_dir, graph) = setup_graph();
        let a = graph
            .add_node("A", &serde_json::json!({"name": "a"}))
            .unwrap();
        let b = graph
            .add_node("B", &serde_json::json!({"name": "b"}))
            .unwrap();
        graph.add_edge(a, b, "HAS", &serde_json::json!({})).unwrap();
        graph.rebuild_csr().unwrap();

        // MATCH finds the A node; OPTIONAL MATCH finds no MISSING edge → r should be null.
        let rows = run(
            &graph,
            "MATCH (n:A) OPTIONAL MATCH (n)-[r:MISSING]->(x) RETURN n.name AS n, r",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_str().unwrap(), "a");
        assert_eq!(rows[0][1], serde_json::Value::Null);
    }

    #[test]
    fn chained_optional_matches_bind_second_branch() {
        let (_dir, graph) = setup_graph();
        let p1 = graph
            .add_node("Post", &serde_json::json!({"title": "p1"}))
            .unwrap();
        let p2 = graph
            .add_node("Post", &serde_json::json!({"title": "p2"}))
            .unwrap();
        let c1 = graph
            .add_node("Comment", &serde_json::json!({"content": "c1"}))
            .unwrap();
        let c2 = graph
            .add_node("Comment", &serde_json::json!({"content": "c2"}))
            .unwrap();
        graph
            .add_edge(p1, c1, "HAS_COMMENT", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(p2, c2, "HAS_COMMENT", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(p1, c1, "HAS_FEATURED_COMMENT", &serde_json::json!({}))
            .unwrap();
        graph.rebuild_csr().unwrap();

        let query = "MATCH (post:Post) \
             OPTIONAL MATCH (post)-[:HAS_COMMENT]->(comment:Comment) \
             OPTIONAL MATCH (post)-[:HAS_FEATURED_COMMENT]->(featured:Comment) \
             RETURN post.title, comment.content, featured.content ORDER BY post.title";
        let rows = run(&graph, query);
        assert_eq!(
            rows,
            vec![
                vec![
                    serde_json::json!("p1"),
                    serde_json::json!("c1"),
                    serde_json::json!("c1"),
                ],
                vec![
                    serde_json::json!("p2"),
                    serde_json::json!("c2"),
                    serde_json::Value::Null,
                ],
            ]
        );
    }

    // --- Range predicate index pushdown (NodeRangeScan) ---

    #[test]
    fn range_scan_gt_filters_correctly() {
        let (_dir, graph) = setup_graph();
        for age in [10i64, 20, 30, 40, 50] {
            graph
                .add_node("Person", &serde_json::json!({"age": age}))
                .unwrap();
        }
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age > 25 RETURN n.age AS age",
        );
        let mut ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        ages.sort_unstable();
        assert_eq!(ages, vec![30, 40, 50]);
    }

    #[test]
    fn range_scan_lt_filters_correctly() {
        let (_dir, graph) = setup_graph();
        for age in [10i64, 20, 30, 40, 50] {
            graph
                .add_node("Person", &serde_json::json!({"age": age}))
                .unwrap();
        }
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age < 35 RETURN n.age AS age",
        );
        let mut ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        ages.sort_unstable();
        assert_eq!(ages, vec![10, 20, 30]);
    }

    #[test]
    fn range_scan_between_inclusive_exclusive() {
        let (_dir, graph) = setup_graph();
        for age in [10i64, 20, 30, 40, 50] {
            graph
                .add_node("Person", &serde_json::json!({"age": age}))
                .unwrap();
        }
        // >20 AND <=40: should include 30 and 40, not 20
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age > 20 AND n.age <= 40 RETURN n.age AS age",
        );
        let mut ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        ages.sort_unstable();
        assert_eq!(ages, vec![30, 40]);
    }

    #[test]
    fn range_scan_ge_le_both_inclusive() {
        let (_dir, graph) = setup_graph();
        for age in [10i64, 20, 30, 40, 50] {
            graph
                .add_node("Person", &serde_json::json!({"age": age}))
                .unwrap();
        }
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age >= 20 AND n.age <= 40 RETURN n.age AS age",
        );
        let mut ages: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        ages.sort_unstable();
        assert_eq!(ages, vec![20, 30, 40]);
    }

    #[test]
    fn null_equality_with_declared_index_returns_no_rows() {
        // `prop = null` is never TRUE. With a declared index the equality
        // must still evaluate as a filter that drops every row, not become an
        // index scan that errors on the null lookup value.
        let (_dir, graph) = setup_graph();
        run(&graph, "CREATE INDEX FOR (n:Person) ON (n.age)");
        graph
            .add_node("Person", &serde_json::json!({"age": 30}))
            .unwrap();
        graph
            .add_node(
                "Person",
                &serde_json::json!({"age": serde_json::Value::Null}),
            )
            .unwrap();
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age = null RETURN count(n) AS n",
        );
        assert_eq!(rows, vec![vec![serde_json::json!(0)]]);
    }

    #[test]
    fn null_parameter_with_declared_index_returns_no_rows() {
        // The planner cannot see parameter values, so `n.age = $p` plans an
        // index scan; a null parameter must then match nothing at evaluation,
        // for both the equality and range forms.
        let (_dir, graph) = setup_graph();
        run(&graph, "CREATE INDEX FOR (n:Person) ON (n.age)");
        graph
            .add_node("Person", &serde_json::json!({"age": 30}))
            .unwrap();
        let params: HashMap<String, serde_json::Value> =
            [("p".to_string(), serde_json::Value::Null)].into();
        for cypher in [
            "MATCH (n:Person) WHERE n.age = $p RETURN count(n) AS n",
            "MATCH (n:Person) WHERE n.age >= $p RETURN count(n) AS n",
        ] {
            let result = execute(&graph, cypher, &params).unwrap();
            assert_eq!(
                result.records[0].values,
                vec![serde_json::json!(0)],
                "{cypher}"
            );
        }
    }

    #[test]
    fn index_scan_verifies_string_with_embedded_nul() {
        // The index encodes strings NUL-terminated, so the prefix scan for
        // "a" also matches the entry for "a\0b"; the verify step must filter
        // the false positive.
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Person", &serde_json::json!({"name": "a"}))
            .unwrap();
        graph
            .add_node("Person", &serde_json::json!({"name": "a\u{0}b"}))
            .unwrap();
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.name = 'a' RETURN n.name AS name",
        );
        assert_eq!(rows, vec![vec![serde_json::json!("a")]]);
    }

    #[test]
    fn range_scan_excludes_mixed_kind_values() {
        // A min-bound-only range: strings sort after numbers in the encoded
        // index, so a string-valued candidate reaches the verify step and
        // must be excluded there; null and missing values never match.
        let (_dir, graph) = setup_graph();
        graph
            .add_node("Person", &serde_json::json!({"age": 30}))
            .unwrap();
        graph
            .add_node("Person", &serde_json::json!({"age": "thirty"}))
            .unwrap();
        graph
            .add_node(
                "Person",
                &serde_json::json!({"age": serde_json::Value::Null}),
            )
            .unwrap();
        graph
            .add_node("Person", &serde_json::json!({"name": "no-age"}))
            .unwrap();
        let rows = run(
            &graph,
            "MATCH (n:Person) WHERE n.age >= 30 RETURN n.age AS age",
        );
        assert_eq!(rows, vec![vec![serde_json::json!(30)]]);
    }

    // --- Chained two-hop factorized expand ---

    #[test]
    fn chained_two_hop_expand_correctness() {
        let (_dir, graph) = setup_graph();
        let a = graph
            .add_node("Person", &serde_json::json!({"name": "a"}))
            .unwrap();
        let b = graph
            .add_node("Person", &serde_json::json!({"name": "b"}))
            .unwrap();
        let c = graph
            .add_node("Person", &serde_json::json!({"name": "c"}))
            .unwrap();
        let d = graph
            .add_node("Person", &serde_json::json!({"name": "d"}))
            .unwrap();
        graph
            .add_edge(a, b, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(b, c, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(b, d, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph.rebuild_csr().unwrap();

        let rows = run(
            &graph,
            "MATCH (x:Person)-[:KNOWS]->(y:Person)-[:KNOWS]->(z:Person) WHERE x.name = 'a' RETURN z.name AS z",
        );
        let mut names: Vec<String> = rows
            .iter()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["c", "d"]);
    }

    // --- SIP (Sideways Information Passing) correctness ---

    /// Two MATCH clauses sharing a variable: the SIP filter must restrict the
    /// probe side's LabelScan to only the nodes produced by the build side,
    /// while still returning the same rows as a brute-force equi-join.
    #[test]
    fn sip_multi_match_shared_variable_correctness() {
        let (_dir, graph) = setup_graph();
        // Insert 20 Person nodes; only two have age = 30.
        for i in 0..20i64 {
            let age = if i < 2 { 30 } else { i + 40 };
            graph
                .add_node(
                    "Person",
                    &serde_json::json!({"name": format!("p{i}"), "age": age}),
                )
                .unwrap();
        }

        // Two separate MATCH clauses joined on `a`.  The second clause restricts
        // to age = 30; the SIP filter should propagate those IDs to the first clause.
        let rows = run(
            &graph,
            "MATCH (a:Person) MATCH (a:Person) WHERE a.age = 30 RETURN a.name AS name",
        );
        assert_eq!(
            rows.len(),
            2,
            "expected exactly 2 rows with age=30, got {}",
            rows.len()
        );
        let mut names: Vec<String> = rows
            .iter()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["p0", "p1"]);
    }

    /// Verifies that SIP does not produce spurious rows: a join on a shared
    /// variable with no matching build-side rows must return an empty result.
    #[test]
    fn sip_empty_build_side_returns_no_rows() {
        let (_dir, graph) = setup_graph();
        for i in 0..10i64 {
            graph
                .add_node("Person", &serde_json::json!({"age": i + 50}))
                .unwrap();
        }

        // No Person has age = 30, so the build side is empty; result must be empty.

        let rows = run(
            &graph,
            "MATCH (a:Person) MATCH (a:Person) WHERE a.age = 30 RETURN a",
        );
        assert!(rows.is_empty(), "expected no rows, got {}", rows.len());
    }

    /// Multi-MATCH with an Expand on the probe side: SIP must thread through
    /// the Expand and restrict its inner LabelScan, then the expansion must
    /// produce only edges reachable from the SIP-filtered nodes.
    #[test]
    fn sip_probe_side_expand_correctness() {
        let (_dir, graph) = setup_graph();
        // p0 (age=30) -[:KNOWS]-> q0
        // p1 (age=99) -[:KNOWS]-> q1
        // SIP should restrict probe to p0 only, so result has exactly one (a, b) pair.
        let p0 = graph
            .add_node("Person", &serde_json::json!({"name": "p0", "age": 30}))
            .unwrap();
        let p1 = graph
            .add_node("Person", &serde_json::json!({"name": "p1", "age": 99}))
            .unwrap();
        let q0 = graph
            .add_node("Person", &serde_json::json!({"name": "q0", "age": 1}))
            .unwrap();
        let q1 = graph
            .add_node("Person", &serde_json::json!({"name": "q1", "age": 2}))
            .unwrap();
        graph
            .add_edge(p0, q0, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph
            .add_edge(p1, q1, "KNOWS", &serde_json::json!({}))
            .unwrap();
        graph.rebuild_csr().unwrap();

        // First MATCH expands to KNOWS neighbors (probe, heavier).
        // Second MATCH finds age=30 persons (build, lighter).
        // The shared WHERE restricts `a` to age=30; after filter pushdown
        // both sides carry the predicate, so SIP propagates p0's NodeId
        // into the Expand's inner LabelScan.
        let rows = run(
            &graph,
            "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b:Person) WHERE a.age = 30 RETURN a.name AS a, b.name AS b",
        );
        assert_eq!(rows.len(), 1, "expected 1 row, got {}", rows.len());
        assert_eq!(rows[0][0].as_str().unwrap(), "p0");
        assert_eq!(rows[0][1].as_str().unwrap(), "q0");
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

    #[test]
    fn test_copy_statement_execution() {
        use std::io::Write;
        let (tempdir, graph) = setup_graph();
        let params = HashMap::new();

        // 1. Test CSV Import
        let csv_path = tempdir.path().join("users.csv");
        {
            let mut file = std::fs::File::create(&csv_path).unwrap();
            writeln!(file, "name,age,active").unwrap();
            writeln!(file, "Alice,30,true").unwrap();
            writeln!(file, "Bob,40,false").unwrap();
            writeln!(file, "Charlie,,true").unwrap();
        }

        let query_csv = format!(
            "COPY Person FROM '{}' WITH {{header: true, delimiter: ','}}",
            csv_path.display()
        );
        let res_csv = execute(&graph, &query_csv, &params).unwrap();
        assert_eq!(res_csv.columns, vec!["nodes_imported".to_string()]);
        assert_eq!(res_csv.records[0].values[0], serde_json::json!(3));

        // Query and verify CSV nodes
        let query_verify_csv = "MATCH (n:Person) RETURN n.name, n.age, n.active ORDER BY n.name";
        let res_verify = execute(&graph, query_verify_csv, &params).unwrap();
        assert_eq!(res_verify.records.len(), 3);
        assert_eq!(
            res_verify.records[0].values,
            vec![
                serde_json::json!("Alice"),
                serde_json::json!(30),
                serde_json::json!(true)
            ]
        );
        assert_eq!(
            res_verify.records[1].values,
            vec![
                serde_json::json!("Bob"),
                serde_json::json!(40),
                serde_json::json!(false)
            ]
        );
        assert_eq!(
            res_verify.records[2].values,
            vec![
                serde_json::json!("Charlie"),
                serde_json::Value::Null,
                serde_json::json!(true)
            ]
        );

        // 2. Test JSONL Import
        let jsonl_path = tempdir.path().join("users.jsonl");
        {
            let mut file = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(file, "{{\"name\": \"David\", \"age\": 25}}").unwrap();
            writeln!(file, "{{\"props\": {{\"name\": \"Eve\", \"age\": 35}}}}").unwrap();
        }

        let query_jsonl = format!("COPY Person FROM '{}'", jsonl_path.display());
        let res_jsonl = execute(&graph, &query_jsonl, &params).unwrap();
        assert_eq!(res_jsonl.records[0].values[0], serde_json::json!(2));

        // Query and verify JSONL nodes
        let query_verify_jsonl = "MATCH (n:Person) WHERE n.name = 'David' OR n.name = 'Eve' RETURN n.name, n.age ORDER BY n.name";
        let res_verify_j = execute(&graph, query_verify_jsonl, &params).unwrap();
        assert_eq!(res_verify_j.records.len(), 2);
        assert_eq!(
            res_verify_j.records[0].values,
            vec![serde_json::json!("David"), serde_json::json!(25)]
        );
        assert_eq!(
            res_verify_j.records[1].values,
            vec![serde_json::json!("Eve"), serde_json::json!(35)]
        );
    }

    #[test]
    fn test_copy_retains_user_id_property() {
        use std::io::Write;
        let (tempdir, graph) = setup_graph();
        let params = HashMap::new();

        // A bare `id` column is a user-defined property and must survive the
        // import; only the system-prefixed `_id` key is structural metadata.
        let jsonl_path = tempdir.path().join("people.jsonl");
        {
            let mut file = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(file, "{{\"id\": 108, \"name\": \"Alice\"}}").unwrap();
            writeln!(file, "{{\"_id\": 7, \"id\": 109, \"name\": \"Bob\"}}").unwrap();
        }

        let query = format!("COPY Person FROM '{}'", jsonl_path.display());
        let res = execute(&graph, &query, &params).unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!(2));

        let res_id = execute(
            &graph,
            "MATCH (p:Person) WHERE p.id = 108 RETURN p.name",
            &params,
        )
        .unwrap();
        assert_eq!(res_id.records.len(), 1);
        assert_eq!(res_id.records[0].values, vec![serde_json::json!("Alice")]);

        // `id` is retained even when `_id` is also present, and `_id` itself
        // is never stored as a property.
        let res_both = execute(
            &graph,
            "MATCH (p:Person) WHERE p.id = 109 RETURN p.name, p._id",
            &params,
        )
        .unwrap();
        assert_eq!(res_both.records.len(), 1);
        assert_eq!(
            res_both.records[0].values,
            vec![serde_json::json!("Bob"), serde_json::Value::Null]
        );
    }

    #[test]
    fn test_parquet_export_import() {
        let (tempdir, graph) = setup_graph();
        let params = HashMap::new();

        // Let's add some nodes and edges to export
        execute(
            &graph,
            "CREATE (a:Person {name: 'Alice', age: 30, active: true})",
            &params,
        )
        .unwrap();
        execute(
            &graph,
            "CREATE (b:Person {name: 'Bob', age: 40, active: false})",
            &params,
        )
        .unwrap();
        execute(&graph, "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS {since: 2020}]->(b)", &params).unwrap();

        // Export database to Parquet
        let export_dir = tempdir.path().join("parquet_export");
        let export_query = format!(
            "EXPORT DATABASE '{}' WITH {{format: 'parquet'}}",
            export_dir.display()
        );
        let res_export = execute(&graph, &export_query, &params).unwrap();
        assert_eq!(
            res_export.records[0].values[0],
            serde_json::Value::Bool(true)
        );

        // Let's create a fresh new graph to import into
        let (_, graph2) = setup_graph();

        // Import the database from the exported directory
        let import_query = format!("IMPORT DATABASE '{}'", export_dir.display());
        let res_import = execute(&graph2, &import_query, &params).unwrap();
        assert_eq!(
            res_import.records[0].values[0],
            serde_json::Value::Bool(true)
        );

        // Verify the imported nodes and properties
        let verify_query = "MATCH (n:Person) RETURN n.name, n.age, n.active ORDER BY n.name";
        let res_verify = execute(&graph2, verify_query, &params).unwrap();
        assert_eq!(res_verify.records.len(), 2);
        assert_eq!(
            res_verify.records[0].values,
            vec![
                serde_json::json!("Alice"),
                serde_json::json!(30),
                serde_json::json!(true)
            ]
        );
        assert_eq!(
            res_verify.records[1].values,
            vec![
                serde_json::json!("Bob"),
                serde_json::json!(40),
                serde_json::json!(false)
            ]
        );

        // Verify the imported relationship
        let verify_rel = "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since";
        let res_verify_rel = execute(&graph2, verify_rel, &params).unwrap();
        assert_eq!(res_verify_rel.records.len(), 1);
        assert_eq!(
            res_verify_rel.records[0].values,
            vec![
                serde_json::json!("Alice"),
                serde_json::json!("Bob"),
                serde_json::json!(2020)
            ]
        );
    }

    // --- Correlated index seek (index nested-loop join) ---

    fn add_user(graph: &Graph, id: i64) -> issundb_core::NodeId {
        graph
            .add_node(
                "User",
                &serde_json::json!({"Id": id, "name": format!("u{id}")}),
            )
            .unwrap()
    }

    /// A `WHERE n.Id = x` whose key `x` is bound by an enclosing `UNWIND` lowers
    /// to a `CorrelatedIndexSeek` instead of a full label scan hashed against the
    /// outer rows, and returns the matching nodes.
    #[test]
    fn correlated_unwind_key_lowers_to_index_seek() {
        let (_dir, graph) = setup_graph();
        for id in 1..=50 {
            add_user(&graph, id);
        }

        let q = "UNWIND [3, 7, 42] AS x MATCH (n:User) WHERE n.Id = x RETURN n.Id AS id";
        let plan = explain(&graph, q).unwrap();
        assert!(
            plan.contains("CorrelatedIndexSeek"),
            "expected a correlated seek, got:\n{plan}"
        );
        assert!(
            !plan.contains("HashJoin"),
            "the hash join should be gone, got:\n{plan}"
        );

        let res = execute(&graph, q, &HashMap::new()).unwrap();
        let mut ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![3, 7, 42]);
    }

    /// The seek result matches the row-pipeline (hash-join) result, including the
    /// multiplicity when the outer list repeats a key and when several nodes share
    /// a key value.
    #[test]
    fn correlated_seek_matches_unfiltered_semantics() {
        let (_dir, graph) = setup_graph();
        add_user(&graph, 1);
        add_user(&graph, 2);
        // A second node with Id = 2: a key lookup must return both.
        graph
            .add_node("User", &serde_json::json!({"Id": 2, "name": "dup"}))
            .unwrap();

        // Key 2 appears twice in the list and matches two nodes => four rows.
        let q = "UNWIND [2, 2, 1] AS x MATCH (n:User) WHERE n.Id = x RETURN n.name AS name";
        let res = execute(&graph, q, &HashMap::new()).unwrap();
        assert_eq!(res.records.len(), 5);
        let names: Vec<String> = res
            .records
            .iter()
            .map(|r| r.values[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names.iter().filter(|n| n.as_str() == "u1").count(), 1);
        assert_eq!(
            names
                .iter()
                .filter(|n| n.as_str() == "u2" || n.as_str() == "dup")
                .count(),
            4
        );
    }

    /// A correlated key supplied through a query parameter list also lowers to the
    /// seek.
    #[test]
    fn correlated_param_keys_lower_to_index_seek() {
        let (_dir, graph) = setup_graph();
        for id in 1..=20 {
            add_user(&graph, id);
        }
        let mut params = HashMap::new();
        params.insert("ids".to_string(), serde_json::json!([4, 11]));

        let q = "UNWIND $ids AS x MATCH (n:User) WHERE n.Id = x RETURN n.Id AS id";
        assert!(explain(&graph, q).unwrap().contains("CorrelatedIndexSeek"));

        let res = execute(&graph, q, &params).unwrap();
        let mut ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![4, 11]);
    }

    /// A correlated `id(n) = x` lowers to an id-based correlated seek (no property
    /// index needed) and returns the addressed nodes.
    #[test]
    fn correlated_id_key_lowers_to_seek() {
        let (_dir, graph) = setup_graph();
        let a = add_user(&graph, 100);
        let _b = add_user(&graph, 200);
        let c = add_user(&graph, 300);

        let q = format!("UNWIND [{a}, {c}] AS x MATCH (n:User) WHERE id(n) = x RETURN n.Id AS id");
        let plan = explain(&graph, &q).unwrap();
        assert!(
            plan.contains("CorrelatedIndexSeek"),
            "expected an id seek, got:\n{plan}"
        );

        let res = execute(&graph, &q, &HashMap::new()).unwrap();
        let mut ids: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![100, 300]);
    }

    /// A constant key is unaffected: it still lowers to a plain `NodeIndexScan`,
    /// not a correlated seek.
    #[test]
    fn constant_key_still_uses_plain_index_scan() {
        let (_dir, graph) = setup_graph();
        add_user(&graph, 9);
        let plan = explain(&graph, "MATCH (n:User) WHERE n.Id = 9 RETURN n").unwrap();
        assert!(plan.contains("NodeIndexScan"), "got:\n{plan}");
        assert!(!plan.contains("CorrelatedIndexSeek"), "got:\n{plan}");
    }

    /// A query string deep enough to overflow the stack is rejected by the parser
    /// guard, so `execute` returns an error rather than aborting the process.
    #[test]
    fn deeply_nested_query_errors_instead_of_aborting() {
        let (_dir, graph) = setup_graph();
        let deep = std::iter::repeat("RETURN 1 AS x")
            .take(5000)
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        assert!(execute(&graph, &deep, &HashMap::new()).is_err());
    }

    /// The parser guard's budget keeps an accepted query's recursion (parse,
    /// plan, optimize, and execute) within a small worker-thread stack. Running
    /// the deepest accepted shapes on a 2 MiB stack must complete, not overflow.
    /// If a future change loosens the budget past what the executor can handle on
    /// a small stack, this test aborts and flags the regression.
    #[test]
    fn near_budget_queries_run_on_a_small_stack() {
        // A nested-list expression close to the bracket budget, plus a long
        // operator chain close to the operator budget. Both pass the guard.
        let mut list = String::from("1");
        for _ in 0..12 {
            list = format!("[{}]", list);
        }
        let and = std::iter::repeat("1 = 1")
            .take(20)
            .collect::<Vec<_>>()
            .join(" AND ");
        let queries = vec![format!("RETURN {list} AS x"), format!("RETURN {and} AS x")];

        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                // Consume some stack first to mimic a real handler call chain.
                let pad = [0u8; 256 * 1024];
                std::hint::black_box(&pad);
                let dir = TempDir::new().unwrap();
                let graph = Graph::open(dir.path(), 1).unwrap();
                for q in &queries {
                    // Parsing and execution must complete without overflowing.
                    let _ = execute(&graph, q, &HashMap::new());
                }
            })
            .unwrap();
        handle.join().unwrap();
    }
}
