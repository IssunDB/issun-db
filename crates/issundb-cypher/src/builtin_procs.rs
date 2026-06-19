//! Built-in graph-algorithm procedures for the `CALL` clause.
//!
//! Unlike the table-backed [`crate::procedure::ProcedureRegistry`], these
//! procedures run against the live `Graph` at call time and yield one row per
//! node. They cover the analytics and community algorithms that have no native
//! Cypher syntax: PageRank, the betweenness, harmonic, and degree centralities,
//! weakly and strongly connected components, and label propagation. Path and
//! traversal algorithms stay expressed as Cypher patterns, so they are not
//! mirrored here. The `issundb.*` namespace keeps these distinct from
//! user-registered procedures.
//!
//! The built-in forms are unparameterized: PageRank and label propagation run
//! with fixed default iteration counts. Because every binding routes Cypher
//! through `crate::exec::execute`, registering the algorithms here exposes them
//! through the facade, the REST and MCP servers, and the Python bindings without
//! per-binding wiring.

use crate::procedure::{CypherType, Procedure};
use issundb_core::{DegreeDirection, Graph, NodeId};
use serde_json::{Value, json};

/// Default PageRank iteration count for the unparameterized procedure form.
const PAGE_RANK_ITERATIONS: u32 = 20;
/// Default PageRank damping factor.
const PAGE_RANK_DAMPING: f32 = 0.85;
/// Default label-propagation iteration cap.
const LABEL_PROP_ITERATIONS: usize = 20;

/// Build the concrete [`Procedure`] for a built-in `issundb.*` algorithm name by
/// running the algorithm against `graph`.
///
/// Returns `Ok(None)` when `name` is not a built-in, so the caller falls back to
/// the table-backed registry. Returns an error string when a built-in is called
/// with arguments (the built-in forms take none) or when the underlying
/// algorithm fails. Every produced row is `(nodeId, value)`, sorted by node id
/// for deterministic output.
pub fn build(graph: &Graph, name: &str, args: &[Value]) -> Result<Option<Procedure>, String> {
    let known = matches!(
        name,
        "issundb.pageRank"
            | "issundb.betweenness"
            | "issundb.harmonic"
            | "issundb.degree"
            | "issundb.connectedComponents"
            | "issundb.stronglyConnectedComponents"
            | "issundb.labelPropagation"
    );
    if !known {
        return Ok(None);
    }
    // Reject arguments before running the algorithm: the built-in forms are
    // unparameterized, so there is no point computing a result just to fail
    // argument validation afterwards.
    if !args.is_empty() {
        return Err(format!(
            "SyntaxError(InvalidNumberOfArguments): procedure `{}` takes 0 argument(s) but {} given",
            name,
            args.len()
        ));
    }

    let to_err = |e: issundb_core::Error| format!("ProcedureCallFailed: {e}");

    let (value_col, rows): (&str, Vec<Vec<Value>>) = match name {
        "issundb.pageRank" => {
            let scores = graph
                .page_rank(PAGE_RANK_ITERATIONS, PAGE_RANK_DAMPING)
                .map_err(to_err)?;
            (
                "score",
                float_rows(scores.into_iter().map(|(n, s)| (n, s as f64))),
            )
        }
        "issundb.betweenness" => (
            "score",
            float_rows(graph.betweenness_centrality().map_err(to_err)?.into_iter()),
        ),
        "issundb.harmonic" => (
            "score",
            float_rows(graph.harmonic_centrality().map_err(to_err)?.into_iter()),
        ),
        "issundb.degree" => (
            "score",
            int_rows(
                graph
                    .degree_centrality(DegreeDirection::Both)
                    .map_err(to_err)?
                    .into_iter(),
            ),
        ),
        "issundb.connectedComponents" => (
            "componentId",
            int_rows(graph.connected_components().map_err(to_err)?.into_iter()),
        ),
        "issundb.stronglyConnectedComponents" => (
            "componentId",
            int_rows(
                graph
                    .strongly_connected_components()
                    .map_err(to_err)?
                    .into_iter(),
            ),
        ),
        "issundb.labelPropagation" => (
            "communityId",
            int_rows(
                graph
                    .label_propagation(LABEL_PROP_ITERATIONS)
                    .map_err(to_err)?
                    .into_iter(),
            ),
        ),
        _ => unreachable!("name was checked against the built-in set above"),
    };

    let value_type = if value_col == "score" {
        CypherType::Float
    } else {
        CypherType::Integer
    };

    Ok(Some(Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![
            ("nodeId".to_string(), CypherType::Integer),
            (value_col.to_string(), value_type),
        ],
        rows,
    }))
}

/// Materialize `(nodeId, score)` rows for a float-valued algorithm result,
/// sorted ascending by node id.
fn float_rows(it: impl Iterator<Item = (NodeId, f64)>) -> Vec<Vec<Value>> {
    let mut v: Vec<(NodeId, f64)> = it.collect();
    v.sort_by_key(|(n, _)| *n);
    v.into_iter()
        .map(|(n, s)| vec![json!(n), json!(s)])
        .collect()
}

/// Materialize `(nodeId, value)` rows for an integer-valued algorithm result
/// (degree count, component id, or community id), sorted ascending by node id.
fn int_rows(it: impl Iterator<Item = (NodeId, u64)>) -> Vec<Vec<Value>> {
    let mut v: Vec<(NodeId, u64)> = it.collect();
    v.sort_by_key(|(n, _)| *n);
    v.into_iter()
        .map(|(n, c)| vec![json!(n), json!(c)])
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::exec::execute;
    use issundb_core::Graph;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// A directed triangle of three labeled nodes, with a fresh CSR snapshot.
    fn triangle() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let params = HashMap::new();
        execute(
            &g,
            "CREATE (a:N), (b:N), (c:N), (a)-[:T]->(b), (b)-[:T]->(c), (c)-[:T]->(a)",
            &params,
        )
        .unwrap();
        g.rebuild_csr().unwrap();
        (dir, g)
    }

    #[test]
    fn page_rank_yields_a_scored_row_per_node() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.pageRank() YIELD nodeId, score RETURN nodeId, score",
            &params,
        )
        .unwrap();
        assert_eq!(res.columns, vec!["nodeId".to_string(), "score".to_string()]);
        assert_eq!(res.records.len(), 3);
    }

    #[test]
    fn standalone_connected_components_projects_outputs() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(&g, "CALL issundb.connectedComponents()", &params).unwrap();
        assert_eq!(
            res.columns,
            vec!["nodeId".to_string(), "componentId".to_string()]
        );
        assert_eq!(res.records.len(), 3);
    }

    #[test]
    fn label_propagation_is_callable() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.labelPropagation() YIELD nodeId, communityId RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], serde_json::json!(3));
    }

    #[test]
    fn builtin_rejects_arguments() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        assert!(execute(&g, "CALL issundb.pageRank(5)", &params).is_err());
    }

    #[test]
    fn unknown_issundb_procedure_is_not_a_builtin() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        // Not in the built-in set, and the default registry is empty, so this is
        // a ProcedureNotFound error rather than a silent success.
        assert!(execute(&g, "CALL issundb.notARealProcedure()", &params).is_err());
    }
}
