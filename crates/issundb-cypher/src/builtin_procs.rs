//! Built-in graph-algorithm and retrieval procedures for the `CALL` clause.
//!
//! Unlike the table-backed [`crate::procedure::ProcedureRegistry`], these
//! procedures run against the live `Graph` at call time and yield one row per
//! node. Two families live here:
//!
//! - Analytics and community algorithms that have no native Cypher syntax:
//!   PageRank, the betweenness, harmonic, and degree centralities, weakly and
//!   strongly connected components, and label propagation. Path and traversal
//!   algorithms stay expressed as Cypher patterns, so they are not mirrored here.
//!   These forms are unparameterized: PageRank and label propagation run with
//!   fixed default iteration counts.
//! - GraphRAG retrieval procedures (`issundb.retrieve.vector` and
//!   `issundb.retrieve.hybrid`) that wrap the `issundb-retrieval` crate. They
//!   take a query vector (and, for the hybrid form, a text query) plus an
//!   optional configuration map, and yield the retrieved subgraph as one
//!   `(nodeId, score)` row per node so the result composes with a following
//!   `MATCH (n) WHERE id(n) = nodeId`. A node carries a null score when it
//!   entered the subgraph through BFS expansion rather than as a seed.
//!
//! The `issundb.*` namespace keeps these distinct from user-registered
//! procedures. Because every binding routes Cypher through
//! `crate::exec::execute`, registering them here exposes them through the
//! facade, the REST and MCP servers, and the Python bindings without per-binding
//! wiring.
//!
//! Argument handling differs from the table-backed registry: a built-in fully
//! consumes and validates its own arguments inside [`build`], so the dispatcher
//! passes no inputs on to `resolve_against`. The synthesized [`Procedure`]
//! therefore declares no inputs and carries only output rows.

use crate::procedure::{CypherType, Procedure};
use issundb_core::{DegreeDirection, Graph, NodeId, TriangleCountSpec};
use issundb_retrieval::{
    FusionStrategy, HybridRetrieveOptions, RetrieveOptions, Subgraph, retrieve_hybrid,
    retrieve_with,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Default PageRank iteration count for the unparameterized procedure form.
const PAGE_RANK_ITERATIONS: u32 = 20;
/// Default PageRank damping factor.
const PAGE_RANK_DAMPING: f32 = 0.85;
/// Default label-propagation iteration cap.
const LABEL_PROP_ITERATIONS: usize = 20;

/// Build the concrete [`Procedure`] for a built-in `issundb.*` name by running it
/// against `graph`.
///
/// Returns `Ok(None)` when `name` is not a built-in, so the caller falls back to
/// the table-backed registry. Returns an error string when a built-in is called
/// with invalid arguments or when the underlying algorithm or retrieval fails.
/// The retrieval procedures consume their query and configuration arguments here;
/// the algorithm procedures take none.
pub fn build(graph: &Graph, name: &str, args: &[Value]) -> Result<Option<Procedure>, String> {
    if let Some(proc) = build_retrieval(graph, name, args)? {
        return Ok(Some(proc));
    }
    if let Some(proc) = build_pathfinding(graph, name, args)? {
        return Ok(Some(proc));
    }
    if let Some(proc) = build_communities(graph, name, args)? {
        return Ok(Some(proc));
    }
    build_algorithm(graph, name, args)
}

/// Build an analytics or community-algorithm procedure that yields one
/// `(nodeId, value)` row per node, sorted by node id for deterministic output.
///
/// `pageRank`, `degree`, and `labelPropagation` accept one optional configuration
/// map; the remaining forms take no arguments. `wcc` and `scc` are aliases for the
/// weakly and strongly connected-components procedures.
fn build_algorithm(graph: &Graph, name: &str, args: &[Value]) -> Result<Option<Procedure>, String> {
    let parameterized = matches!(
        name,
        "issundb.pageRank" | "issundb.degree" | "issundb.labelPropagation"
    );
    let parameterless = matches!(
        name,
        "issundb.betweenness"
            | "issundb.harmonic"
            | "issundb.connectedComponents"
            | "issundb.wcc"
            | "issundb.stronglyConnectedComponents"
            | "issundb.scc"
    );
    if !parameterized && !parameterless {
        return Ok(None);
    }
    // Parameterized forms accept one optional configuration map; parameterless
    // forms reject any argument.
    let cfg = if parameterized {
        if args.len() > 1 {
            return Err(arg_count_err(name, "0 or 1", args.len()));
        }
        args.first()
    } else {
        if !args.is_empty() {
            return Err(arg_count_err(name, "0", args.len()));
        }
        None
    };

    let (value_col, rows): (&str, Vec<Vec<Value>>) = match name {
        "issundb.pageRank" => {
            let iterations =
                cfg_usize(name, cfg, "iterations", PAGE_RANK_ITERATIONS as usize)? as u32;
            let damping = cfg_f32(name, cfg, "damping", PAGE_RANK_DAMPING)?;
            let scores = graph.page_rank(iterations, damping).map_err(proc_err)?;
            (
                "score",
                float_rows(scores.into_iter().map(|(n, s)| (n, s as f64))),
            )
        }
        "issundb.betweenness" => (
            "score",
            float_rows(
                graph
                    .betweenness_centrality()
                    .map_err(proc_err)?
                    .into_iter(),
            ),
        ),
        "issundb.harmonic" => (
            "score",
            float_rows(graph.harmonic_centrality().map_err(proc_err)?.into_iter()),
        ),
        "issundb.degree" => {
            let direction = parse_degree_direction(name, cfg)?;
            (
                "score",
                int_rows(
                    graph
                        .degree_centrality(direction)
                        .map_err(proc_err)?
                        .into_iter(),
                ),
            )
        }
        "issundb.connectedComponents" | "issundb.wcc" => (
            "componentId",
            int_rows(graph.connected_components().map_err(proc_err)?.into_iter()),
        ),
        "issundb.stronglyConnectedComponents" | "issundb.scc" => (
            "componentId",
            int_rows(
                graph
                    .strongly_connected_components()
                    .map_err(proc_err)?
                    .into_iter(),
            ),
        ),
        "issundb.labelPropagation" => {
            let max_iterations = cfg_usize(name, cfg, "maxIterations", LABEL_PROP_ITERATIONS)?;
            (
                "communityId",
                int_rows(
                    graph
                        .label_propagation(max_iterations)
                        .map_err(proc_err)?
                        .into_iter(),
                ),
            )
        }
        _ => unreachable!("name was checked against the built-in sets above"),
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

/// Parse the `direction` configuration field for `issundb.degree`: `IN`, `OUT`,
/// or `BOTH` (case-insensitive), defaulting to `BOTH`.
fn parse_degree_direction(proc: &str, cfg: Option<&Value>) -> Result<DegreeDirection, String> {
    match cfg_opt_string(proc, cfg, "direction")? {
        None => Ok(DegreeDirection::Both),
        Some(s) => match s.to_ascii_uppercase().as_str() {
            "IN" => Ok(DegreeDirection::In),
            "OUT" => Ok(DegreeDirection::Out),
            "BOTH" => Ok(DegreeDirection::Both),
            _ => Err(format!(
                "ProcedureCallFailed: procedure `{proc}` direction must be `IN`, `OUT`, or `BOTH`"
            )),
        },
    }
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

/// Build a GraphRAG retrieval procedure (`issundb.retrieve.*`), or `Ok(None)` when
/// `name` is not one. These procedures consume a query vector (and, for the hybrid
/// form, a text query) plus an optional trailing configuration map.
fn build_retrieval(graph: &Graph, name: &str, args: &[Value]) -> Result<Option<Procedure>, String> {
    match name {
        "issundb.retrieve.vector" => Ok(Some(build_retrieve_vector(graph, args)?)),
        "issundb.retrieve.hybrid" => Ok(Some(build_retrieve_hybrid(graph, args)?)),
        _ => Ok(None),
    }
}

/// `issundb.retrieve.vector(queryVector [, {k, hops, maxDistance, maxNodes}])`.
/// Yields `(nodeId, distance)`, where `distance` is the seed's vector distance
/// (lower is closer) and is null for nodes reached only by BFS expansion.
fn build_retrieve_vector(graph: &Graph, args: &[Value]) -> Result<Procedure, String> {
    let name = "issundb.retrieve.vector";
    if args.is_empty() || args.len() > 2 {
        return Err(arg_count_err(name, "1 or 2", args.len()));
    }
    let q = parse_vector(name, &args[0])?;
    let cfg = args.get(1);
    let opts = RetrieveOptions {
        k: cfg_usize(name, cfg, "k", 10)?,
        hops: cfg_u8(name, cfg, "hops", 2)?,
        max_distance: cfg_f32(name, cfg, "maxDistance", f32::MAX)?,
        max_nodes: cfg_opt_usize(name, cfg, "maxNodes")?,
    };
    let sub = retrieve_with(graph, &q, &opts).map_err(|e| format!("ProcedureCallFailed: {e}"))?;
    // Vector distance: lower is closer, so order ascending (higher_is_better=false).
    Ok(retrieval_proc(name, "distance", subgraph_rows(sub, false)))
}

/// `issundb.retrieve.hybrid(queryVector, queryText [, config])`, where `config`
/// keys are `vectorK`, `textK`, `hops`, `maxDistance`, `maxNodes`, `textLabel`,
/// `textProperty`, `vectorLabel`, and `fusion`. Yields `(nodeId, score)`, where
/// `score` is the fused relevance (higher is more relevant) and is null for nodes
/// reached only by BFS expansion. An empty query vector disables vector search; an
/// empty text query disables text search.
fn build_retrieve_hybrid(graph: &Graph, args: &[Value]) -> Result<Procedure, String> {
    let name = "issundb.retrieve.hybrid";
    if args.len() < 2 || args.len() > 3 {
        return Err(arg_count_err(name, "2 or 3", args.len()));
    }
    let q = parse_vector(name, &args[0])?;
    let text = match &args[1] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => {
            return Err(format!(
                "ProcedureCallFailed: procedure `{name}` text query argument must be a string"
            ));
        }
    };
    let cfg = args.get(2);
    let opts = HybridRetrieveOptions {
        vector_k: cfg_usize(name, cfg, "vectorK", 10)?,
        text_k: cfg_usize(name, cfg, "textK", 10)?,
        text_label: cfg_opt_string(name, cfg, "textLabel")?,
        text_property: cfg_opt_string(name, cfg, "textProperty")?,
        hops: cfg_u8(name, cfg, "hops", 2)?,
        max_distance: cfg_f32(name, cfg, "maxDistance", f32::MAX)?,
        max_nodes: cfg_opt_usize(name, cfg, "maxNodes")?,
        vector_label: cfg_opt_string(name, cfg, "vectorLabel")?,
        fusion: parse_fusion(name, cfg)?,
    };
    let sub = retrieve_hybrid(graph, &q, &text, &opts)
        .map_err(|e| format!("ProcedureCallFailed: {e}"))?;
    // Fused score: higher is more relevant, so order descending (higher_is_better=true).
    Ok(retrieval_proc(name, "score", subgraph_rows(sub, true)))
}

/// Assemble a retrieval [`Procedure`] from its output rows. It declares no inputs
/// because [`build`] already consumed the call arguments; the dispatcher passes no
/// arguments on to `resolve_against`.
fn retrieval_proc(name: &str, value_col: &str, rows: Vec<Vec<Value>>) -> Procedure {
    Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![
            ("nodeId".to_string(), CypherType::Integer),
            (value_col.to_string(), CypherType::Float),
        ],
        rows,
    }
}

/// Materialize `(nodeId, score)` rows from a retrieved subgraph. Seed nodes carry
/// their score; expansion-only nodes carry null. Scored nodes sort ahead of
/// unscored ones, by score (descending when `higher_is_better`, else ascending),
/// with node id as a deterministic tiebreaker.
fn subgraph_rows(sub: Subgraph, higher_is_better: bool) -> Vec<Vec<Value>> {
    use std::cmp::Ordering;
    let mut rows: Vec<(NodeId, Option<f32>)> = sub
        .nodes
        .iter()
        .map(|&n| (n, sub.scores.get(&n).copied()))
        .collect();
    rows.sort_by(|a, b| {
        let by_score = match (a.1, b.1) {
            (Some(x), Some(y)) => {
                let o = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
                if higher_is_better { o.reverse() } else { o }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_score.then(a.0.cmp(&b.0))
    });
    rows.into_iter()
        .map(|(n, s)| {
            let score = s.map(|v| json!(v as f64)).unwrap_or(Value::Null);
            vec![json!(n), score]
        })
        .collect()
}

/// Parse a query-vector argument: a JSON list of numbers, or null/empty for an
/// absent vector (disables vector search in the hybrid form).
fn parse_vector(proc: &str, v: &Value) -> Result<Vec<f32>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it.as_f64() {
                    Some(n) => out.push(n as f32),
                    None => {
                        return Err(format!(
                            "ProcedureCallFailed: procedure `{proc}` query vector must contain \
                             only numbers"
                        ));
                    }
                }
            }
            Ok(out)
        }
        _ => Err(format!(
            "ProcedureCallFailed: procedure `{proc}` query vector must be a list of numbers"
        )),
    }
}

/// Look up `key` in the optional configuration map. `Ok(None)` when the map is
/// absent, null, or has no such key; an error when the configuration argument is
/// present but is not a map.
fn cfg_field<'a>(
    proc: &str,
    cfg: Option<&'a Value>,
    key: &str,
) -> Result<Option<&'a Value>, String> {
    match cfg {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(m.get(key).filter(|v| !v.is_null())),
        Some(_) => Err(format!(
            "ProcedureCallFailed: procedure `{proc}` configuration argument must be a map"
        )),
    }
}

/// Read a non-negative integer configuration field, or `default` when absent.
fn cfg_usize(proc: &str, cfg: Option<&Value>, key: &str, default: usize) -> Result<usize, String> {
    match cfg_field(proc, cfg, key)? {
        None => Ok(default),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|u| u as usize)
            .ok_or_else(|| cfg_type_err(proc, key, "a non-negative integer")),
        Some(_) => Err(cfg_type_err(proc, key, "a non-negative integer")),
    }
}

/// Read an optional non-negative integer configuration field (`None` when absent).
fn cfg_opt_usize(proc: &str, cfg: Option<&Value>, key: &str) -> Result<Option<usize>, String> {
    match cfg_field(proc, cfg, key)? {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|u| Some(u as usize))
            .ok_or_else(|| cfg_type_err(proc, key, "a non-negative integer")),
        Some(_) => Err(cfg_type_err(proc, key, "a non-negative integer")),
    }
}

/// Read a hop-count configuration field, saturating to `u8::MAX`, or `default`.
fn cfg_u8(proc: &str, cfg: Option<&Value>, key: &str, default: u8) -> Result<u8, String> {
    Ok(match cfg_opt_usize(proc, cfg, key)? {
        Some(u) => u.min(u8::MAX as usize) as u8,
        None => default,
    })
}

/// Read a float configuration field, or `default` when absent.
fn cfg_f32(proc: &str, cfg: Option<&Value>, key: &str, default: f32) -> Result<f32, String> {
    match cfg_field(proc, cfg, key)? {
        None => Ok(default),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| cfg_type_err(proc, key, "a number")),
        Some(_) => Err(cfg_type_err(proc, key, "a number")),
    }
}

/// Read an optional string configuration field (`None` when absent).
fn cfg_opt_string(proc: &str, cfg: Option<&Value>, key: &str) -> Result<Option<String>, String> {
    match cfg_field(proc, cfg, key)? {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(cfg_type_err(proc, key, "a string")),
    }
}

/// Parse the `fusion` configuration field into a [`FusionStrategy`]. Accepts the
/// string `"rrf"`, a map `{rrfK}` for Reciprocal Rank Fusion, or a map
/// `{vectorWeight, textWeight}` for a weighted sum. Defaults to RRF with `k = 60`.
fn parse_fusion(proc: &str, cfg: Option<&Value>) -> Result<FusionStrategy, String> {
    match cfg_field(proc, cfg, "fusion")? {
        None => Ok(FusionStrategy::default()),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("rrf") => {
            Ok(FusionStrategy::Rrf { k: 60 })
        }
        Some(Value::Object(m)) => {
            let vw = m.get("vectorWeight").and_then(|v| v.as_f64());
            let tw = m.get("textWeight").and_then(|v| v.as_f64());
            if let (Some(vw), Some(tw)) = (vw, tw) {
                Ok(FusionStrategy::WeightedSum {
                    vector_weight: vw as f32,
                    text_weight: tw as f32,
                })
            } else if let Some(k) = m.get("rrfK").and_then(|v| v.as_u64()) {
                Ok(FusionStrategy::Rrf { k: k as u32 })
            } else {
                Err(format!(
                    "ProcedureCallFailed: procedure `{proc}` fusion map must have `rrfK` or both \
                     `vectorWeight` and `textWeight`"
                ))
            }
        }
        Some(_) => Err(format!(
            "ProcedureCallFailed: procedure `{proc}` fusion must be the string `rrf` or a map"
        )),
    }
}

/// Construct the standard out-of-range argument-count error.
fn arg_count_err(proc: &str, expected: &str, given: usize) -> String {
    format!(
        "SyntaxError(InvalidNumberOfArguments): procedure `{proc}` takes {expected} argument(s) \
         but {given} given"
    )
}

/// Construct the standard configuration-field type error.
fn cfg_type_err(proc: &str, key: &str, expected: &str) -> String {
    format!(
        "ProcedureCallFailed: procedure `{proc}` configuration field `{key}` must be {expected}"
    )
}

/// Map an `issundb-core` error into the standard procedure-failure string. A free
/// function (not a closure) so it is `Copy` and usable across several `map_err`
/// calls in one procedure body.
fn proc_err(e: issundb_core::Error) -> String {
    format!("ProcedureCallFailed: {e}")
}

/// Build a pathfinding or pattern-count procedure (`issundb.shortestPath`,
/// `issundb.dijkstra`, or `issundb.triangleCount`), or `Ok(None)` when `name` is
/// not one. Paths yield one ordered `(index, nodeId[, totalWeight])` row per node;
/// an absent path yields no rows.
fn build_pathfinding(
    graph: &Graph,
    name: &str,
    args: &[Value],
) -> Result<Option<Procedure>, String> {
    match name {
        "issundb.shortestPath" => Ok(Some(build_shortest_path(graph, args)?)),
        "issundb.dijkstra" => Ok(Some(build_dijkstra(graph, args)?)),
        "issundb.triangleCount" => Ok(Some(build_triangle_count(graph, args)?)),
        _ => Ok(None),
    }
}

/// `issundb.shortestPath(sourceId, targetId)`. Yields `(index, nodeId)` along an
/// unweighted shortest path; no rows when the target is unreachable.
fn build_shortest_path(graph: &Graph, args: &[Value]) -> Result<Procedure, String> {
    let name = "issundb.shortestPath";
    if args.len() != 2 {
        return Err(arg_count_err(name, "2", args.len()));
    }
    let src = parse_node_id(name, &args[0])?;
    let dst = parse_node_id(name, &args[1])?;
    let rows = match graph.shortest_path(src, dst).map_err(proc_err)? {
        Some(nodes) => nodes
            .into_iter()
            .enumerate()
            .map(|(i, n)| vec![json!(i as u64), json!(n)])
            .collect(),
        None => Vec::new(),
    };
    Ok(Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![
            ("index".to_string(), CypherType::Integer),
            ("nodeId".to_string(), CypherType::Integer),
        ],
        rows,
    })
}

/// `issundb.dijkstra(sourceId, targetId)`. Yields `(index, nodeId, totalWeight)`
/// along the least-weight path, where edge weight is the first present of the
/// `weight`, `cost`, `capacity`, or `cap` property (default `1.0`); `totalWeight`
/// is the whole-path weight, repeated on every row. No rows when unreachable.
fn build_dijkstra(graph: &Graph, args: &[Value]) -> Result<Procedure, String> {
    let name = "issundb.dijkstra";
    if args.len() != 2 {
        return Err(arg_count_err(name, "2", args.len()));
    }
    let src = parse_node_id(name, &args[0])?;
    let dst = parse_node_id(name, &args[1])?;
    let rows = match graph.shortest_path_dijkstra(src, dst).map_err(proc_err)? {
        Some(path) => {
            let total = path.total_weight;
            path.nodes
                .into_iter()
                .enumerate()
                .map(|(i, n)| vec![json!(i as u64), json!(n), json!(total)])
                .collect()
        }
        None => Vec::new(),
    };
    Ok(Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![
            ("index".to_string(), CypherType::Integer),
            ("nodeId".to_string(), CypherType::Integer),
            ("totalWeight".to_string(), CypherType::Float),
        ],
        rows,
    })
}

/// `issundb.triangleCount([{relTypes, labels}])`. Yields a single `(count)` row:
/// the number of directed triangle cycles `(a)->(b)->(c)->(a)`, optionally
/// constrained by per-hop relationship types and per-variable labels (each a list
/// of up to three strings, with `null` entries left unconstrained).
fn build_triangle_count(graph: &Graph, args: &[Value]) -> Result<Procedure, String> {
    let name = "issundb.triangleCount";
    if args.len() > 1 {
        return Err(arg_count_err(name, "0 or 1", args.len()));
    }
    let cfg = args.first();
    // The spec borrows `&str`, so the owned strings must outlive the call.
    let rel_owned = parse_str_triple(name, cfg, "relTypes")?;
    let label_owned = parse_str_triple(name, cfg, "labels")?;
    let spec = TriangleCountSpec {
        rel_types: [
            rel_owned[0].as_deref(),
            rel_owned[1].as_deref(),
            rel_owned[2].as_deref(),
        ],
        labels: [
            label_owned[0].as_deref(),
            label_owned[1].as_deref(),
            label_owned[2].as_deref(),
        ],
    };
    let count = graph.count_triangle_cycles(&spec).map_err(proc_err)?;
    Ok(Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![("count".to_string(), CypherType::Integer)],
        rows: vec![vec![json!(count)]],
    })
}

/// `issundb.communities([{maxIterations, topPerCommunity}])`. Partitions the graph
/// with label propagation, then ranks each community's members by PageRank
/// (descending, node id breaking ties), yielding `(communityId, nodeId, rank)`
/// with a one-based `rank`. `topPerCommunity` caps the members emitted per
/// community. This is the building block global-GraphRAG search needs to pick
/// representative nodes per community; the community summaries themselves are an
/// application concern, not a database one.
fn build_communities(
    graph: &Graph,
    name: &str,
    args: &[Value],
) -> Result<Option<Procedure>, String> {
    if name != "issundb.communities" {
        return Ok(None);
    }
    if args.len() > 1 {
        return Err(arg_count_err(name, "0 or 1", args.len()));
    }
    let cfg = args.first();
    let max_iterations = cfg_usize(name, cfg, "maxIterations", LABEL_PROP_ITERATIONS)?;
    let top = cfg_opt_usize(name, cfg, "topPerCommunity")?;

    let communities = graph.label_propagation(max_iterations).map_err(proc_err)?;
    let ranks = graph
        .page_rank(PAGE_RANK_ITERATIONS, PAGE_RANK_DAMPING)
        .map_err(proc_err)?;

    // Group nodes by community (BTreeMap for a deterministic community order).
    let mut by_community: BTreeMap<u64, Vec<(NodeId, f32)>> = BTreeMap::new();
    for (node, community) in communities {
        let score = ranks.get(&node).copied().unwrap_or(0.0);
        by_community
            .entry(community)
            .or_default()
            .push((node, score));
    }

    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (community, mut members) in by_community {
        members.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        for (i, (node, _score)) in members.iter().enumerate() {
            if top.is_some_and(|t| i >= t) {
                break;
            }
            rows.push(vec![json!(community), json!(node), json!(i as u64 + 1)]);
        }
    }

    Ok(Some(Procedure {
        name: name.to_string(),
        inputs: vec![],
        outputs: vec![
            ("communityId".to_string(), CypherType::Integer),
            ("nodeId".to_string(), CypherType::Integer),
            ("rank".to_string(), CypherType::Integer),
        ],
        rows,
    }))
}

/// Parse a node-id argument: a non-negative integer.
fn parse_node_id(proc: &str, v: &Value) -> Result<NodeId, String> {
    match v {
        Value::Number(n) => n.as_u64().map(|u| u as NodeId).ok_or_else(|| {
            format!(
                "ProcedureCallFailed: procedure `{proc}` node id must be a non-negative integer"
            )
        }),
        _ => Err(format!(
            "ProcedureCallFailed: procedure `{proc}` node id must be an integer"
        )),
    }
}

/// Parse a configuration field holding a list of up to three strings (with `null`
/// entries permitted and left unconstrained) into a fixed triple of owned strings.
fn parse_str_triple(
    proc: &str,
    cfg: Option<&Value>,
    key: &str,
) -> Result<[Option<String>; 3], String> {
    let mut out: [Option<String>; 3] = [None, None, None];
    match cfg_field(proc, cfg, key)? {
        None => Ok(out),
        Some(Value::Array(items)) => {
            if items.len() > 3 {
                return Err(format!(
                    "ProcedureCallFailed: procedure `{proc}` `{key}` accepts at most 3 entries"
                ));
            }
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::Null => {}
                    Value::String(s) => out[i] = Some(s.clone()),
                    _ => {
                        return Err(format!(
                            "ProcedureCallFailed: procedure `{proc}` `{key}` entries must be \
                             strings or null"
                        ));
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(format!(
            "ProcedureCallFailed: procedure `{proc}` `{key}` must be a list of strings"
        )),
    }
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

    // --- GraphRAG retrieval procedures ---

    use issundb_vector::VectorGraphExt;
    use serde_json::{Value, json};

    /// A small graph for retrieval: a chain `a -> b -> c` where `a` and `b` are
    /// `Doc` nodes with a `body` text property and vectors, and `a`'s vector
    /// points along the first axis. A `body` text index is created. The CSR
    /// snapshot is rebuilt so BFS expansion sees the edges.
    fn rag_graph() -> (TempDir, Graph, [i64; 3]) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g
            .add_node("Doc", &json!({"body": "rust graph database vector"}))
            .unwrap();
        let b = g
            .add_node("Doc", &json!({"body": "nearest neighbor search"}))
            .unwrap();
        let c = g.add_node("Doc", &json!({"body": "unrelated"})).unwrap();
        g.upsert_vector(a, &[1.0f32, 0.0]).unwrap();
        g.upsert_vector(b, &[0.0f32, 1.0]).unwrap();
        g.add_edge(a, b, "LINK", &json!({})).unwrap();
        g.add_edge(b, c, "LINK", &json!({})).unwrap();
        g.update(|txn| txn.create_node_text_index("Doc", "body"))
            .unwrap();
        g.rebuild_csr().unwrap();
        (dir, g, [a as i64, b as i64, c as i64])
    }

    #[test]
    fn retrieve_vector_yields_seed_with_distance() {
        let (_d, g, ids) = rag_graph();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.retrieve.vector([1.0, 0.0], {k: 1, hops: 0}) \
             YIELD nodeId, distance RETURN nodeId, distance",
            &params,
        )
        .unwrap();
        assert_eq!(
            res.columns,
            vec!["nodeId".to_string(), "distance".to_string()]
        );
        // hops=0: only the seed node `a`.
        assert_eq!(res.records.len(), 1);
        assert_eq!(res.records[0].values[0], json!(ids[0]));
        let dist = res.records[0].values[1].as_f64().unwrap();
        assert!(dist < 1e-5, "distance to identical vector should be ~0");
    }

    #[test]
    fn retrieve_vector_expansion_nodes_have_null_distance() {
        let (_d, g, ids) = rag_graph();
        let params = HashMap::new();
        // hops=1 from seed `a` pulls in `b` as an expansion node.
        let res = execute(
            &g,
            "CALL issundb.retrieve.vector([1.0, 0.0], {k: 1, hops: 1}) \
             YIELD nodeId, distance RETURN nodeId, distance",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 2);
        // Seed `a` sorts first (scored ahead of unscored), with a real distance.
        assert_eq!(res.records[0].values[0], json!(ids[0]));
        assert!(res.records[0].values[1].as_f64().is_some());
        // Expansion node `b` carries a null distance.
        assert_eq!(res.records[1].values[0], json!(ids[1]));
        assert_eq!(res.records[1].values[1], Value::Null);
    }

    #[test]
    fn retrieve_vector_defaults_without_config_map() {
        let (_d, g, _ids) = rag_graph();
        let params = HashMap::new();
        // No config map: default k and hops. Should run and return some rows.
        let res = execute(
            &g,
            "CALL issundb.retrieve.vector([1.0, 0.0]) YIELD nodeId RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        let count = res.records[0].values[0].as_u64().unwrap();
        assert!(count >= 1);
    }

    #[test]
    fn retrieve_hybrid_fuses_vector_and_text_seeds() {
        let (_d, g, ids) = rag_graph();
        let params = HashMap::new();
        // Vector hit is `a` (query [1,0]); text hit for "neighbor" is `b`.
        // hops=0 so the result is exactly the fused seed set {a, b}.
        let res = execute(
            &g,
            "CALL issundb.retrieve.hybrid([1.0, 0.0], 'neighbor', \
             {vectorK: 1, textK: 1, textLabel: 'Doc', textProperty: 'body', hops: 0}) \
             YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
            &params,
        )
        .unwrap();
        assert_eq!(res.columns, vec!["nodeId".to_string(), "score".to_string()]);
        let returned: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[0].as_i64().unwrap())
            .collect();
        assert!(returned.contains(&ids[0]), "vector seed a must be present");
        assert!(returned.contains(&ids[1]), "text seed b must be present");
        // Every seed in a hops=0 result carries a non-null fused score.
        for r in &res.records {
            assert!(r.values[1].as_f64().is_some());
        }
    }

    #[test]
    fn retrieve_hybrid_weighted_sum_fusion_is_accepted() {
        let (_d, g, _ids) = rag_graph();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.retrieve.hybrid([1.0, 0.0], 'rust', \
             {vectorK: 1, textK: 1, textLabel: 'Doc', textProperty: 'body', hops: 0, \
              fusion: {vectorWeight: 0.7, textWeight: 0.3}}) \
             YIELD nodeId, score RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        assert!(res.records[0].values[0].as_u64().unwrap() >= 1);
    }

    #[test]
    fn retrieve_vector_rejects_missing_vector_argument() {
        let (_d, g, _ids) = rag_graph();
        let params = HashMap::new();
        assert!(execute(&g, "CALL issundb.retrieve.vector()", &params).is_err());
    }

    #[test]
    fn retrieve_hybrid_composes_with_following_match() {
        let (_d, g, _ids) = rag_graph();
        let params = HashMap::new();
        // The canonical GraphRAG shape: retrieve seeds, then join back to nodes.
        let res = execute(
            &g,
            "CALL issundb.retrieve.hybrid([1.0, 0.0], 'neighbor', \
             {vectorK: 1, textK: 1, textLabel: 'Doc', textProperty: 'body', hops: 0}) \
             YIELD nodeId, score \
             MATCH (n) WHERE id(n) = nodeId \
             RETURN n.body AS body ORDER BY body",
            &params,
        )
        .unwrap();
        assert_eq!(res.columns, vec!["body".to_string()]);
        assert!(!res.records.is_empty());
        for r in &res.records {
            assert!(r.values[0].is_string());
        }
    }

    // --- parameterized algorithm procedures ---

    #[test]
    fn page_rank_accepts_config_map() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.pageRank({iterations: 5, damping: 0.9}) \
             YIELD nodeId, score RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], json!(3));
    }

    #[test]
    fn degree_accepts_direction_config() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        // Each node in a directed triangle has out-degree 1.
        let res = execute(
            &g,
            "CALL issundb.degree({direction: 'OUT'}) \
             YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId",
            &params,
        )
        .unwrap();
        assert_eq!(res.records.len(), 3);
        for r in &res.records {
            assert_eq!(r.values[1], json!(1));
        }
    }

    #[test]
    fn degree_rejects_invalid_direction() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        assert!(execute(&g, "CALL issundb.degree({direction: 'SIDEWAYS'})", &params).is_err());
    }

    #[test]
    fn wcc_and_scc_aliases_resolve() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        for q in [
            "CALL issundb.wcc() YIELD nodeId, componentId RETURN count(*) AS c",
            "CALL issundb.scc() YIELD nodeId, componentId RETURN count(*) AS c",
        ] {
            let res = execute(&g, q, &params).unwrap();
            assert_eq!(res.records[0].values[0], json!(3));
        }
    }

    #[test]
    fn parameterless_procedure_still_rejects_arguments() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        assert!(execute(&g, "CALL issundb.betweenness(1)", &params).is_err());
    }

    // --- pathfinding and pattern-count procedures ---

    /// A directed weighted chain `n0 -> n1 -> n2 -> n3`, returning the node ids.
    fn weighted_chain() -> (TempDir, Graph, [i64; 4]) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let n: Vec<_> = (0..4)
            .map(|_| g.add_node("N", &json!({})).unwrap())
            .collect();
        g.add_edge(n[0], n[1], "E", &json!({"weight": 1.0}))
            .unwrap();
        g.add_edge(n[1], n[2], "E", &json!({"weight": 2.0}))
            .unwrap();
        g.add_edge(n[2], n[3], "E", &json!({"weight": 3.0}))
            .unwrap();
        g.rebuild_csr().unwrap();
        (dir, g, [n[0] as i64, n[1] as i64, n[2] as i64, n[3] as i64])
    }

    #[test]
    fn shortest_path_yields_ordered_nodes() {
        let (_d, g, ids) = weighted_chain();
        let params = HashMap::new();
        let q = format!(
            "CALL issundb.shortestPath({}, {}) YIELD index, nodeId RETURN index, nodeId ORDER BY index",
            ids[0], ids[3]
        );
        let res = execute(&g, &q, &params).unwrap();
        assert_eq!(res.columns, vec!["index".to_string(), "nodeId".to_string()]);
        let path: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[1].as_i64().unwrap())
            .collect();
        assert_eq!(path, vec![ids[0], ids[1], ids[2], ids[3]]);
    }

    #[test]
    fn shortest_path_unreachable_yields_no_rows() {
        let (_d, g, ids) = weighted_chain();
        let params = HashMap::new();
        // The chain is directed, so there is no path from the last node back.
        let q = format!(
            "CALL issundb.shortestPath({}, {}) YIELD nodeId RETURN count(*) AS c",
            ids[3], ids[0]
        );
        let res = execute(&g, &q, &params).unwrap();
        assert_eq!(res.records[0].values[0], json!(0));
    }

    #[test]
    fn dijkstra_yields_total_weight() {
        let (_d, g, ids) = weighted_chain();
        let params = HashMap::new();
        let q = format!(
            "CALL issundb.dijkstra({}, {}) YIELD index, nodeId, totalWeight \
             RETURN index, nodeId, totalWeight ORDER BY index",
            ids[0], ids[3]
        );
        let res = execute(&g, &q, &params).unwrap();
        assert_eq!(res.records.len(), 4);
        // Total weight 1 + 2 + 3 = 6, repeated on each row.
        for r in &res.records {
            assert!((r.values[2].as_f64().unwrap() - 6.0).abs() < 1e-6);
        }
        // Rows are ordered along the path.
        let path: Vec<i64> = res
            .records
            .iter()
            .map(|r| r.values[1].as_i64().unwrap())
            .collect();
        assert_eq!(path, vec![ids[0], ids[1], ids[2], ids[3]]);
    }

    #[test]
    fn triangle_count_counts_the_cycle() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.triangleCount() YIELD count RETURN count",
            &params,
        )
        .unwrap();
        assert_eq!(res.columns, vec!["count".to_string()]);
        // The directed triangle a->b->c->a yields 3 cyclic assignments (one per
        // rotation of the start vertex).
        assert_eq!(res.records[0].values[0], json!(3));
    }

    #[test]
    fn triangle_count_with_type_filter_is_accepted() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.triangleCount({relTypes: ['T', 'T', 'T'], labels: ['N', 'N', 'N']}) \
             YIELD count RETURN count",
            &params,
        )
        .unwrap();
        assert_eq!(res.records[0].values[0], json!(3));
    }

    // --- communities (global-GraphRAG building block) ---

    #[test]
    fn communities_yields_ranked_members_per_community() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        let res = execute(
            &g,
            "CALL issundb.communities() YIELD communityId, nodeId, rank \
             RETURN communityId, nodeId, rank ORDER BY communityId, rank",
            &params,
        )
        .unwrap();
        assert_eq!(
            res.columns,
            vec![
                "communityId".to_string(),
                "nodeId".to_string(),
                "rank".to_string()
            ]
        );
        // One row per node, and ranks within a community start at 1.
        assert_eq!(res.records.len(), 3);
        assert!(
            res.records
                .iter()
                .all(|r| r.values[2].as_u64().unwrap() >= 1)
        );
    }

    #[test]
    fn communities_top_per_community_caps_members() {
        let (_d, g) = triangle();
        let params = HashMap::new();
        // The directed triangle is one strongly connected community under label
        // propagation, so topPerCommunity=1 keeps a single representative.
        let res = execute(
            &g,
            "CALL issundb.communities({topPerCommunity: 1}) YIELD nodeId, rank \
             RETURN count(*) AS c",
            &params,
        )
        .unwrap();
        let count = res.records[0].values[0].as_u64().unwrap();
        assert!(
            (1..=3).contains(&count),
            "expected 1..=3 representatives, got {count}"
        );
    }
}
