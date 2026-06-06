//! Characterization of LadybugDB's relationship-uniqueness semantics in the
//! pinned `lbug` build, using a two-node cycle (0 -> 1 and 1 -> 0): the only
//! three-hop walk must reuse its first edge, so it distinguishes trails from
//! walks.
//!
//! openCypher requires pairwise-distinct relationships within a MATCH
//! pattern. LadybugDB matches walks instead, in fixed-length chains and in
//! variable-length patterns alike, and the `recursive_pattern_semantic`
//! session setting registers but has no effect on results. The harness
//! compensates with the trail `Oracle` in `main.rs`. When this test starts
//! failing after an `lbug` upgrade, the build now honors trail semantics:
//! remove the oracle special-casing and this test together.

use lbug::{Connection, Database, SystemConfig};

fn scalar(conn: &Connection, q: &str) -> String {
    conn.query(q)
        .unwrap()
        .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>())
        .next()
        .unwrap()
        .join(",")
}

#[test]
fn walk_semantics_persist_despite_the_trail_setting() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::new(dir.path().join("db"), SystemConfig::default()).unwrap();
    let conn = Connection::new(&db).unwrap();
    conn.query("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id));")
        .unwrap();
    conn.query("CREATE REL TABLE KNOWS(FROM Person TO Person);")
        .unwrap();
    conn.query("CREATE (:Person {id: 0}), (:Person {id: 1});")
        .unwrap();
    conn.query(
        "MATCH (a:Person {id: 0}), (b:Person {id: 1}) \
         CREATE (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(a);",
    )
    .unwrap();

    // The setting registers.
    conn.query("CALL recursive_pattern_semantic = 'TRAIL';")
        .unwrap();
    let setting = scalar(
        &conn,
        "CALL current_setting('recursive_pattern_semantic') RETURN *;",
    );
    assert_eq!(setting, "TRAIL", "the setting must at least register");

    // Fixed three-hop chain: openCypher trail semantics expect 0 (the only
    // walk 0->1->0->1 reuses the edge 0->1); LadybugDB counts it.
    let fixed = scalar(
        &conn,
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person) \
         WHERE a.id = 0 RETURN count(d) AS n",
    );
    assert_eq!(
        fixed, "1",
        "fixed chains now enforce relationship uniqueness; drop the harness oracle"
    );

    // Variable-length *2..3: trail semantics expect 1 (only 0->1->0);
    // LadybugDB counts the edge-reusing three-hop walk as well.
    let var = scalar(
        &conn,
        "MATCH (a:Person)-[:KNOWS*2..3]->(c:Person) WHERE a.id = 0 RETURN count(c) AS n",
    );
    assert_eq!(
        var, "2",
        "the TRAIL setting now takes effect; drop the harness oracle"
    );
}
