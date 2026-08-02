//! Ground-truth tests for the built-in functions, closeness, the clustering
//! coefficient, and the triangle count.
//!
//! The corpus in `tests/fixtures/function_oracle.json` is generated offline by
//! `scripts/gen_function_oracle.py` (run via `make oracle-fixtures`) and
//! committed, the same arrangement `oracle.rs` uses: the Python and NetworkX
//! dependencies live in the generator and never in the test path.
//!
//! What this adds over the differential corpus in `issundb-cypher` is an
//! independent answer. That corpus compares IssunDB's fast paths against its own
//! row pipeline, which catches a fast-path defect and nothing else: a formula
//! that is simply wrong agrees with itself on both paths and passes. Every value
//! here was computed from the definition instead, and cross-checked against
//! NetworkX by the generator wherever NetworkX has an equivalent, so a mismatch
//! means IssunDB is wrong rather than merely inconsistent.
//!
//! Each assertion runs twice, once as the planner would normally answer it and
//! once with `ISSUNDB_ROW_PIPELINE_ONLY` forcing the general path, so a fast
//! path that diverges from ground truth is caught here as well.
//!
//! Deliberately out of scope, because a reference would have to invent a
//! convention rather than record one: eigenvector and Katz centrality (whose
//! scaling differs between implementations), and Louvain and label propagation
//! (whose partitions are not unique, and which `analytics.rs` pins by their own
//! invariants).

use std::collections::HashMap;

use issundb::{Graph, GraphQueryExt, NodeId};
use serde::Deserialize;
use serde_json::json;
use tempfile::TempDir;

const TOLERANCE: f64 = 1e-9;

/// `vector_dist` measures through the vector index, which computes in `f32`
/// rather than widening to `f64` as the `issundb.distance.*` family does, so its
/// answer differs from the reference around the seventh decimal. The corpus
/// already rounds its inputs to single precision, so what remains is the
/// accumulation, and this bound is that and nothing more: tightening it to the
/// `1e-9` the other functions meet fails.
const VECTOR_INDEX_TOLERANCE: f64 = 1e-6;

#[derive(Deserialize)]
struct Corpus {
    graphs: Vec<GraphCase>,
    values: ValueCases,
}

#[derive(Deserialize)]
struct GraphCase {
    id: String,
    n: usize,
    edges: Vec<[usize; 2]>,
    link: Vec<LinkRow>,
    closeness: Vec<f64>,
    clustering: Vec<f64>,
    #[serde(rename = "triangleAssignments")]
    triangle_assignments: u64,
}

#[derive(Deserialize)]
struct LinkRow {
    a: usize,
    b: usize,
    #[serde(rename = "commonNeighbors")]
    common_neighbors: f64,
    jaccard: f64,
    #[serde(rename = "adamicAdar")]
    adamic_adar: f64,
    #[serde(rename = "resourceAllocation")]
    resource_allocation: f64,
    #[serde(rename = "preferentialAttachment")]
    preferential_attachment: f64,
}

#[derive(Deserialize)]
struct ValueCases {
    similarity: Vec<SimilarityCase>,
    distance: Vec<DistanceCase>,
}

#[derive(Deserialize)]
struct SimilarityCase {
    a: Vec<i64>,
    b: Vec<i64>,
    jaccard: f64,
    overlap: f64,
}

#[derive(Deserialize)]
struct DistanceCase {
    a: Vec<f64>,
    b: Vec<f64>,
    #[serde(rename = "cosineDefined")]
    cosine_defined: bool,
    cosine: Option<f64>,
    euclidean: f64,
}

fn load_corpus() -> Corpus {
    let raw = include_str!("fixtures/function_oracle.json");
    serde_json::from_str(raw).expect("the function oracle corpus must parse")
}

/// Build a case's graph, returning the node ids in corpus order so a reference
/// index can be mapped onto the id the engine allocated.
fn build(case: &GraphCase) -> (TempDir, Graph, Vec<NodeId>) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    let ids: Vec<NodeId> = (0..case.n)
        .map(|i| g.add_node("N", &json!({ "idx": i as i64 })).unwrap())
        .collect();
    for [s, d] in &case.edges {
        g.add_edge(ids[*s], ids[*d], "R", &json!({})).unwrap();
    }
    (dir, g, ids)
}

/// Run `cypher` on both execution paths, returning each path's first-column
/// values keyed by the row's first column. Both must succeed.
fn both_paths(g: &Graph, cypher: &str) -> [Vec<Vec<serde_json::Value>>; 2] {
    let fast = g
        .query(cypher)
        .unwrap_or_else(|e| panic!("fast path failed for {cypher}: {e}"));
    let slow = {
        // The switch is read per query from the environment, so setting it here
        // covers the second execution only. It is restored immediately after.
        unsafe { std::env::set_var("ISSUNDB_ROW_PIPELINE_ONLY", "1") };
        let out = g.query(cypher);
        unsafe { std::env::remove_var("ISSUNDB_ROW_PIPELINE_ONLY") };
        out.unwrap_or_else(|e| panic!("row pipeline failed for {cypher}: {e}"))
    };
    [
        fast.records.into_iter().map(|r| r.values).collect(),
        slow.records.into_iter().map(|r| r.values).collect(),
    ]
}

fn approx(got: f64, want: f64, what: &str) {
    approx_within(got, want, TOLERANCE, what);
}

fn approx_within(got: f64, want: f64, tolerance: f64, what: &str) {
    assert!(
        (got - want).abs() < tolerance,
        "{what}: expected {want}, got {got} (tolerance {tolerance})",
    );
}

/// The five neighborhood link-prediction functions against values computed from
/// their definitions and confirmed against NetworkX.
#[test]
fn link_prediction_functions_match_ground_truth() {
    let corpus = load_corpus();
    for case in &corpus.graphs {
        let (_dir, g, ids) = build(case);
        for row in &case.link {
            let cypher = format!(
                "MATCH (a:N), (b:N) WHERE id(a) = {} AND id(b) = {} \
                 RETURN issundb.link.commonNeighbors(a, b) AS cn, \
                 issundb.link.jaccard(a, b) AS jac, \
                 issundb.link.adamicAdar(a, b) AS aa, \
                 issundb.link.resourceAllocation(a, b) AS ra, \
                 issundb.link.preferentialAttachment(a, b) AS pa",
                ids[row.a], ids[row.b],
            );
            for (path, rows) in both_paths(&g, &cypher).iter().enumerate() {
                let values = &rows[0];
                let where_ = format!("case {} pair ({}, {}) path {path}", case.id, row.a, row.b);
                let num = |i: usize| values[i].as_f64().expect("a number");
                approx(
                    num(0),
                    row.common_neighbors,
                    &format!("{where_} commonNeighbors"),
                );
                approx(num(1), row.jaccard, &format!("{where_} jaccard"));
                approx(num(2), row.adamic_adar, &format!("{where_} adamicAdar"));
                approx(
                    num(3),
                    row.resource_allocation,
                    &format!("{where_} resourceAllocation"),
                );
                approx(
                    num(4),
                    row.preferential_attachment,
                    &format!("{where_} preferentialAttachment"),
                );
            }
        }
    }
}

/// Wasserman-Faust closeness over out-distances.
#[test]
fn closeness_matches_ground_truth() {
    let corpus = load_corpus();
    for case in &corpus.graphs {
        let (_dir, g, ids) = build(case);
        let index_of: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        let cypher = "CALL issundb.closeness() YIELD nodeId, score \
                      RETURN nodeId, score ORDER BY nodeId";
        for (path, rows) in both_paths(&g, cypher).iter().enumerate() {
            for values in rows {
                let node = values[0].as_u64().expect("a node id");
                let idx = index_of[&node];
                approx(
                    values[1].as_f64().expect("a score"),
                    case.closeness[idx],
                    &format!("case {} closeness node {idx} path {path}", case.id),
                );
            }
        }
    }
}

/// The clustering coefficient over distinct undirected neighbors.
#[test]
fn clustering_coefficient_matches_ground_truth() {
    let corpus = load_corpus();
    for case in &corpus.graphs {
        let (_dir, g, ids) = build(case);
        let index_of: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        let cypher = "CALL issundb.clusteringCoefficient() YIELD nodeId, score \
                      RETURN nodeId, score ORDER BY nodeId";
        for (path, rows) in both_paths(&g, cypher).iter().enumerate() {
            for values in rows {
                let node = values[0].as_u64().expect("a node id");
                let idx = index_of[&node];
                approx(
                    values[1].as_f64().expect("a score"),
                    case.clustering[idx],
                    &format!("case {} clustering node {idx} path {path}", case.id),
                );
            }
        }
    }
}

/// The triangle count, against a brute-force enumeration of the directed
/// pattern's assignments. The corpus carries parallel edges and self-loops, so
/// this pins the multiplication and the relationship-uniqueness rule rather than
/// only the simple-graph case.
#[test]
fn triangle_count_matches_ground_truth() {
    let corpus = load_corpus();
    for case in &corpus.graphs {
        let (_dir, g, _ids) = build(case);
        let cypher = "CALL issundb.triangleCount() YIELD count RETURN count";
        for (path, rows) in both_paths(&g, cypher).iter().enumerate() {
            let got = rows[0][0].as_u64().expect("a count");
            assert_eq!(
                got, case.triangle_assignments,
                "case {} triangle count path {path}",
                case.id,
            );
        }
    }
}

/// The set-similarity and vector-distance functions, which read no graph, so
/// their answer is a closed form rather than a convention.
#[test]
fn value_functions_match_ground_truth() {
    let corpus = load_corpus();
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();

    let list = |v: &[i64]| {
        let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        format!("[{}]", parts.join(", "))
    };
    let flist = |v: &[f64]| {
        let parts: Vec<String> = v.iter().map(|x| format!("{x:?}")).collect();
        format!("[{}]", parts.join(", "))
    };

    for case in &corpus.values.similarity {
        let cypher = format!(
            "RETURN issundb.similarity.jaccard({a}, {b}) AS jac, \
             issundb.similarity.overlap({a}, {b}) AS ovl",
            a = list(&case.a),
            b = list(&case.b),
        );
        for (path, rows) in both_paths(&g, &cypher).iter().enumerate() {
            approx(
                rows[0][0].as_f64().expect("a number"),
                case.jaccard,
                &format!("similarity.jaccard {:?} {:?} path {path}", case.a, case.b),
            );
            approx(
                rows[0][1].as_f64().expect("a number"),
                case.overlap,
                &format!("similarity.overlap {:?} {:?} path {path}", case.a, case.b),
            );
        }
    }

    for case in &corpus.values.distance {
        let cypher = format!(
            "RETURN issundb.distance.cosine({a}, {b}) AS cos, \
             issundb.distance.euclidean({a}, {b}) AS euc, \
             vector_dist({a}, {b}) AS vd",
            a = flist(&case.a),
            b = flist(&case.b),
        );
        for (path, rows) in both_paths(&g, &cypher).iter().enumerate() {
            approx(
                rows[0][1].as_f64().expect("a number"),
                case.euclidean,
                &format!("distance.euclidean {:?} {:?} path {path}", case.a, case.b),
            );
            // Cosine divides by each vector's norm, so a zero vector has no
            // answer to check against. The corpus marks those pairs and this
            // asserts only the euclidean value for them; the engine's two cosine
            // entry points disagree there, which is recorded in the generator
            // rather than settled by inventing a convention here.
            if !case.cosine_defined {
                continue;
            }
            let want = case.cosine.expect("a defined cosine carries its value");
            approx(
                rows[0][0].as_f64().expect("a number"),
                want,
                &format!("distance.cosine {:?} {:?} path {path}", case.a, case.b),
            );
            // `vector_dist` measures with the vector index's configured metric,
            // which defaults to cosine, so on a freshly opened graph it owes the
            // same answer as the fixed-metric function above.
            approx_within(
                rows[0][2].as_f64().expect("a number"),
                want,
                VECTOR_INDEX_TOLERANCE,
                &format!("vector_dist {:?} {:?} path {path}", case.a, case.b),
            );
        }
    }
}
