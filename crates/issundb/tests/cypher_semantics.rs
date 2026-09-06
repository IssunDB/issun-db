//! Behavior tests for Cypher corners the default suite did not reach: scalar
//! function edge cases, null handling in list and map comparison and in the
//! quantifiers, partial date formats, aggregates inside `ORDER BY`
//! expressions, the aggregation scope validators, `DELETE` and `SET` on
//! values that are not plain bindings, `FOREACH` body substitution, hop
//! predicates over the factorized expansion, and the schema half of
//! `EXPORT DATABASE`.
//!
//! The openCypher TCK covers much of this, but it runs only under
//! `ISSUNDB_CONFORMANCE=1`; these tests keep the rules under `make test`.

use issundb::{Graph, GraphQueryExt, TextIndexExt};
use serde_json::{Value, json};
use tempfile::TempDir;

fn open_tmp() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    (dir, g)
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.query(q)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .records
        .into_iter()
        .map(|r| r.values)
        .collect()
}

fn row(g: &Graph, q: &str) -> Vec<Value> {
    let mut all = rows(g, q);
    assert_eq!(all.len(), 1, "{q}: expected one row, got {all:?}");
    all.remove(0)
}

fn err(g: &Graph, q: &str) -> String {
    match g.query(q) {
        Ok(res) => panic!("{q}: expected an error, got {:?}", res.records),
        Err(e) => e.to_string(),
    }
}

fn assert_close(actual: &Value, expected: f64) {
    let a = actual
        .as_f64()
        .unwrap_or_else(|| panic!("not a number: {actual}"));
    assert!((a - expected).abs() < 1e-9, "{a} != {expected}");
}

#[test]
fn string_and_math_functions_handle_values_nulls_and_bad_arguments() {
    let (_dir, g) = open_tmp();
    let r = row(
        &g,
        "RETURN left('hello', 2), right('hello', 3), degrees(3.141592653589793), \
         radians(180.0), haversin(0.0), coalesce(null, null, 3), toInteger('42'), \
         toInteger(' 7.9 '), toInteger('x'), toFloat('2.5'), toFloat('x'), \
         size('héllo'), range(5, 1, -2), abs(-2.5)",
    );
    assert_eq!(r[0], json!("he"));
    assert_eq!(r[1], json!("llo"));
    assert_close(&r[2], 180.0);
    assert_close(&r[3], std::f64::consts::PI);
    assert_close(&r[4], 0.0);
    assert_eq!(r[5], json!(3));
    assert_eq!(r[6], json!(42));
    assert_eq!(r[7], json!(7));
    assert_eq!(r[8], Value::Null);
    assert_eq!(r[9], json!(2.5));
    assert_eq!(r[10], Value::Null);
    assert_eq!(r[11], json!(5));
    assert_eq!(r[12], json!([5, 3, 1]));
    assert_eq!(r[13], json!(2.5));

    let nulls = row(
        &g,
        "RETURN left(null, 1), right('a', null), degrees(null), radians(null), \
         haversin(null), toInteger(null), coalesce(null, null), size(null)",
    );
    assert!(nulls.iter().all(|v| v.is_null()), "{nulls:?}");

    let ts = row(&g, "RETURN timestamp()");
    assert!(ts[0].as_i64().unwrap() > 0);

    assert!(err(&g, "RETURN left('a')").contains("exactly 2 arguments"));
    assert!(err(&g, "RETURN left(1, 2)").contains("must be a string"));
    assert!(err(&g, "RETURN right('a', 'b')").contains("must be an integer"));
    assert!(err(&g, "RETURN range(1, 5, 0)").contains("must not be zero"));
    assert!(err(&g, "RETURN degrees('a')").contains("must be a number"));
    assert!(err(&g, "RETURN size(1)").contains("list or string"));
    assert!(err(&g, "RETURN timestamp(1)").contains("exactly 0 arguments"));
}

#[test]
fn list_and_map_equality_is_three_valued() {
    let (_dir, g) = open_tmp();
    let r = row(
        &g,
        "RETURN [1, null] = [1, null], [1, 2] = [1, 3], [1] = [1, 2], \
         {a: 1, b: null} = {a: 1, b: null}, {a: 1} = {b: 1}, {a: 1, b: 2} = {a: 1, b: 3}, \
         [1, 2] = [1, 2], {a: [1, null]} = {a: [1, 2]}",
    );
    assert_eq!(
        r,
        vec![
            Value::Null,
            json!(false),
            json!(false),
            Value::Null,
            json!(false),
            json!(false),
            json!(true),
            Value::Null,
        ]
    );
}

#[test]
fn quantifiers_follow_three_valued_logic() {
    let (_dir, g) = open_tmp();
    let r = row(
        &g,
        "RETURN any(x IN [null, false] WHERE x), none(x IN [null, false] WHERE x), \
         none(x IN [false] WHERE x), none(x IN [true, null] WHERE x), \
         single(x IN [true, null] WHERE x), single(x IN [true, true] WHERE x), \
         single(x IN [true, false] WHERE x), single(x IN [false] WHERE x), \
         all(x IN [true, null] WHERE x), all(x IN [true, false] WHERE x), \
         all(x IN [] WHERE x), any(x IN [] WHERE x)",
    );
    assert_eq!(
        r,
        vec![
            Value::Null,
            Value::Null,
            json!(true),
            json!(false),
            Value::Null,
            json!(false),
            json!(true),
            json!(false),
            Value::Null,
            json!(false),
            json!(true),
            json!(false),
        ]
    );
}

#[test]
fn slices_clamp_and_subscripts_type_check() {
    let (_dir, g) = open_tmp();
    let r = row(
        &g,
        "RETURN [1, 2, 3, 4][-2..], [1, 2, 3, 4][..-1], [1, 2, 3, 4][1..null], \
         'hello'[1..3], 'hello'[-2..], [1, 2, 3][5..], [1, 2, 3][2..1], \
         null[0..1], [1, 2][null], {a: 1}['a'], {a: 1}['b']",
    );
    assert_eq!(
        r,
        vec![
            json!([3, 4]),
            json!([1, 2, 3]),
            Value::Null,
            json!("el"),
            json!("lo"),
            json!([]),
            json!([]),
            Value::Null,
            Value::Null,
            json!(1),
            Value::Null,
        ]
    );
    assert!(err(&g, "RETURN [1, 2][true]").contains("got Boolean"));
    assert!(err(&g, "RETURN [1, 2][1.5]").contains("got Float"));
    assert!(err(&g, "RETURN [1, 2]['a']").contains("got String"));
    assert!(err(&g, "RETURN [1, 2][[0]]").contains("got List"));
    assert!(err(&g, "RETURN [1, 2][{a: 1}]").contains("got Map"));
    assert!(err(&g, "RETURN true[0]").contains("cannot index into Boolean"));
    assert!(err(&g, "RETURN 'ab'[0]").contains("cannot index into String"));
}

#[test]
fn partial_date_strings_and_week_truncation() {
    let (_dir, g) = open_tmp();
    let r = row(
        &g,
        "RETURN toString(date('2024')), toString(date('202403')), toString(date('2024-03')), \
         toString(date('2024W10')), toString(date('2024-W10')), toString(date('2024W103')), \
         toString(date('2024-W10-3')), toString(date('20240306'))",
    );
    assert_eq!(
        r,
        vec![
            json!("2024-01-01"),
            json!("2024-03-01"),
            json!("2024-03-01"),
            json!("2024-03-04"),
            json!("2024-03-04"),
            json!("2024-03-06"),
            json!("2024-03-06"),
            json!("2024-03-06"),
        ]
    );
    let t = row(
        &g,
        "RETURN toString(date.truncate('week', date('2024-03-07'))), \
         toString(date.truncate('week', date('2024-03-07'), {dayOfWeek: 3})), \
         toString(date.truncate('month', date('2024-03-07'), {day: 5}))",
    );
    assert_eq!(
        t,
        vec![
            json!("2024-03-04"),
            json!("2024-03-06"),
            json!("2024-03-05")
        ]
    );
    assert!(err(&g, "RETURN date('2024-13')").contains("cannot parse date"));
}

fn grouped_fixture() -> (TempDir, Graph) {
    let (dir, g) = open_tmp();
    g.query(
        "CREATE (:T {k: 'A', v: 1}), (:T {k: 'A', v: 2}), (:T {k: 'A', v: 3}), \
         (:T {k: 'B', v: 4}), (:T {k: 'C', v: 5}), (:T {k: 'C', v: 6})",
    )
    .unwrap();
    (dir, g)
}

fn first_column(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

#[test]
fn order_by_accepts_aggregates_inside_expressions() {
    let (_dir, g) = grouped_fixture();
    let by_case = first_column(rows(
        &g,
        "MATCH (n:T) RETURN n.k AS k, count(*) AS c \
         ORDER BY CASE WHEN count(*) > 2 THEN 0 ELSE 1 END, k",
    ));
    assert_eq!(by_case, vec![json!("A"), json!("B"), json!("C")]);

    let by_function = first_column(rows(
        &g,
        "MATCH (n:T) WITH n.k AS k, count(*) AS c \
         ORDER BY toInteger(toString(count(*))) DESC RETURN k",
    ));
    assert_eq!(by_function, vec![json!("A"), json!("C"), json!("B")]);

    let by_collect_size = first_column(rows(
        &g,
        "MATCH (n:T) RETURN n.k AS k, count(*) AS c ORDER BY size(collect(n.v)) DESC, -min(n.v)",
    ));
    assert_eq!(by_collect_size, vec![json!("A"), json!("C"), json!("B")]);

    let by_quantifier = first_column(rows(
        &g,
        "MATCH (n:T) RETURN n.k AS k, count(*) AS c ORDER BY all(x IN collect(n.v) WHERE x > 3), k",
    ));
    assert_eq!(by_quantifier, vec![json!("A"), json!("B"), json!("C")]);

    let by_list_comprehension = first_column(rows(
        &g,
        "MATCH (n:T) WITH n.k AS k, collect(n.v) AS vs \
         ORDER BY size([x IN vs WHERE x > 2]) DESC, k RETURN k",
    ));
    assert_eq!(
        by_list_comprehension,
        vec![json!("C"), json!("A"), json!("B")]
    );
}

#[test]
fn aggregation_scope_validators_reject_non_grouping_references() {
    let (_dir, g) = grouped_fixture();
    for q in [
        "MATCH (n:T) RETURN count(*) + n.v",
        "MATCH (n:T) RETURN count(*) + CASE WHEN n.k = 'A' THEN 1 ELSE 0 END",
        "MATCH (n:T) RETURN count(*) + size([x IN [1] WHERE x = n.v])",
        "MATCH (n:T) RETURN count(*) + CASE WHEN n:T THEN 1 ELSE 0 END",
        "MATCH (n:T) RETURN collect(n.v)[n.v]",
    ] {
        let e = err(&g, q);
        assert!(
            e.contains("AmbiguousAggregationExpression"),
            "{q}: unexpected error {e}"
        );
    }
    assert!(
        err(
            &g,
            "MATCH (n:T) WITH n.k AS k, count(*) AS c ORDER BY n.v RETURN k"
        )
        .contains("not a grouping key"),
    );
    assert!(
        err(
            &g,
            "MATCH (n:T) WITH n.k AS k, count(*) AS c ORDER BY toString(n.v) RETURN k"
        )
        .contains("not a grouping key"),
    );

    // A grouping expression or its alias is allowed anywhere in the item.
    let ok = rows(
        &g,
        "MATCH (n:T) RETURN n.k AS k, count(*) + size(n.k) AS c ORDER BY k",
    );
    assert_eq!(
        ok,
        vec![
            vec![json!("A"), json!(4)],
            vec![json!("B"), json!(2)],
            vec![json!("C"), json!(3)],
        ]
    );
    let ok = first_column(rows(
        &g,
        "MATCH (n:T) WITH n.k AS k, count(*) AS c ORDER BY size(k) + c DESC, k RETURN k",
    ));
    assert_eq!(ok, vec![json!("A"), json!("C"), json!("B")]);
}

#[test]
fn structural_functions_reject_the_wrong_entity_kind() {
    let (_dir, g) = open_tmp();
    g.query("CREATE (:P {name: 'a'})-[:R]->(:P {name: 'b'})")
        .unwrap();
    assert!(err(&g, "MATCH p = (a)-->(b) RETURN size(p)").contains("path variable"));
    assert!(err(&g, "MATCH (n) RETURN length(n)").contains("length() cannot be applied"));
    assert!(err(&g, "MATCH ()-[r]->() RETURN length(r)").contains("length() cannot be applied"));
    assert!(err(&g, "MATCH (n) RETURN type(n)").contains("type() requires a relationship"));
    assert!(err(&g, "MATCH p = ()-->() RETURN type(p)").contains("type() requires a relationship"));
    assert_eq!(
        row(
            &g,
            "MATCH p = (a)-[r]->(b) RETURN length(p), type(r), size([a, b])"
        ),
        vec![json!(1), json!("R"), json!(2)]
    );
}

#[test]
fn delete_accepts_paths_lists_and_null_and_rejects_scalars() {
    let (_dir, g) = open_tmp();
    g.query("CREATE (:D {n: 1})-[:R]->(:D {n: 2}), (:D {n: 3}), (:D {n: 4}), (:K {n: 5})")
        .unwrap();

    g.query("MATCH p = (:D)-[:R]->(:D) DELETE p").unwrap();
    assert_eq!(row(&g, "MATCH (d:D) RETURN count(d)"), vec![json!(2)]);
    assert_eq!(row(&g, "MATCH ()-[r]->() RETURN count(r)"), vec![json!(0)]);

    g.query("MATCH (d:D) WITH collect(d) AS ds DELETE ds")
        .unwrap();
    assert_eq!(row(&g, "MATCH (d:D) RETURN count(d)"), vec![json!(0)]);

    g.query("WITH null AS x DELETE x").unwrap();
    g.query("MATCH (k:K) WITH [k, null] AS l DELETE l").unwrap();
    assert_eq!(row(&g, "MATCH (n) RETURN count(n)"), vec![json!(0)]);

    assert!(err(&g, "WITH 1 AS x DELETE x").contains("DELETE expects"));
    assert!(err(&g, "WITH {a: 1} AS m DELETE m").contains("DELETE expects"));
    assert!(err(&g, "WITH ['a'] AS l DELETE l").contains("DELETE expects"));
}

#[test]
fn set_and_labels_resolve_wrapped_and_missing_entities() {
    let (_dir, g) = open_tmp();
    g.query("CREATE (:S {n: 1})-[:REL]->(:S {n: 2})-[:REL]->(:S {n: 3})")
        .unwrap();

    // OPTIONAL MATCH binds null; SET on it is a no-op, not an error.
    assert_eq!(
        row(&g, "OPTIONAL MATCH (n:Nope) SET n.x = 1 RETURN n"),
        vec![Value::Null]
    );

    // A node or relationship that travelled through a list is still a target.
    assert_eq!(
        row(
            &g,
            "MATCH (a:S {n: 1}) UNWIND [a] AS m SET m.x = 7, m:Extra \
             RETURN m.x, m:Extra, m:S, m:Nope"
        ),
        vec![json!(7), json!(true), json!(true), json!(false)]
    );
    assert_eq!(
        row(
            &g,
            "MATCH (:S {n: 1})-[r]->() UNWIND [r] AS s SET s.w = 2 \
             RETURN s.w, s:REL, s:OTHER"
        ),
        vec![json!(2), json!(true), json!(false)]
    );
    assert_eq!(
        row(&g, "MATCH (a:S {n: 1}) RETURN a.x, labels(a)"),
        vec![json!(7), json!(["S", "Extra"])]
    );
    assert_eq!(
        row(
            &g,
            "MATCH (a:S {n: 1}) UNWIND [a] AS m REMOVE m.x, m:Extra RETURN m.x, labels(m)"
        ),
        vec![Value::Null, json!(["S"])]
    );

    assert!(err(&g, "WITH 5 AS s SET s.x = 1").contains("scalar variable"));
    assert!(err(&g, "MATCH (:S)-[rs*1..2]->() SET rs.x = 1").contains("list of relationships"));
}

#[test]
fn foreach_substitutes_the_loop_variable_through_every_expression_kind() {
    let (_dir, g) = open_tmp();
    g.query(
        "FOREACH (x IN [{k: 1}, {k: 3}] | CREATE (:F { \
            k: x.k, \
            big: CASE WHEN x.k > 2 THEN true ELSE false END, \
            cnt: size([y IN [1, 2, 3] WHERE y <= x.k]), \
            allpos: all(y IN [x.k] WHERE y > 0), \
            shadow: all(x IN [-1] WHERE x > 0), \
            first: [x.k, 9][0], \
            tail: [x.k, 9][1..], \
            s: toString(x.k), \
            neg: -x.k, \
            nn: x.k IS NOT NULL, \
            red: reduce(acc = 0, y IN [x.k, 1] | acc + y), \
            whole: x \
         }))",
    )
    .unwrap();
    let created = rows(
        &g,
        "MATCH (f:F) RETURN f.k, f.big, f.cnt, f.allpos, f.shadow, f.first, f.tail, \
         f.s, f.neg, f.nn, f.red, f.whole ORDER BY f.k",
    );
    assert_eq!(
        created,
        vec![
            vec![
                json!(1),
                json!(false),
                json!(1),
                json!(true),
                json!(false),
                json!(1),
                json!([9]),
                json!("1"),
                json!(-1),
                json!(true),
                json!(2),
                json!({"k": 1}),
            ],
            vec![
                json!(3),
                json!(true),
                json!(3),
                json!(true),
                json!(false),
                json!(3),
                json!([9]),
                json!("3"),
                json!(-3),
                json!(true),
                json!(4),
                json!({"k": 3}),
            ],
        ]
    );

    // Nested FOREACH: the inner loop variable is substituted inside the outer body.
    g.query("FOREACH (x IN [10, 20] | FOREACH (y IN [1, 2] | CREATE (:G {v: x + y})))")
        .unwrap();
    assert_eq!(
        first_column(rows(&g, "MATCH (g:G) RETURN g.v ORDER BY g.v")),
        vec![json!(11), json!(12), json!(21), json!(22)]
    );
}

#[test]
fn hop_predicates_over_shared_and_expansion_variables_agree() {
    let (_dir, g) = open_tmp();
    g.query(
        "CREATE (a:P {name: 'a', tags: ['x', 'y']}), (b:P {name: 'b', tags: ['q']}), \
                (c:P {name: 'c', tags: ['z']}), (d:P {name: 'd', tags: ['z']}), \
                (a)-[:R]->(b), (a)-[:R]->(c), (d)-[:R]->(b)",
    )
    .unwrap();
    g.rebuild_csr().unwrap();

    let pairs = |q: &str| -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = rows(&g, q)
            .into_iter()
            .map(|r| {
                (
                    r[0].as_str().unwrap().to_string(),
                    r[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        out.sort();
        out
    };
    let ab = vec![("a".to_string(), "b".to_string())];

    // Source-only predicate: evaluated once per source and applied to every hop.
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE all(t IN a.tags WHERE t <> 'z') \
             RETURN a.name, b.name"
        ),
        vec![
            ("a".to_string(), "b".to_string()),
            ("a".to_string(), "c".to_string()),
        ]
    );
    // Destination predicates in each expression shape.
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE all(t IN a.tags WHERE t <> 'z') \
             AND size([t IN b.tags WHERE t = 'q']) > 0 RETURN a.name, b.name"
        ),
        ab
    );
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE none(t IN b.tags WHERE t = 'z') \
             AND a.name <> 'd' RETURN a.name, b.name"
        ),
        ab
    );
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE reduce(s = '', t IN b.tags | s + t) = 'q' \
             AND a.tags[0] = 'x' RETURN a.name, b.name"
        ),
        ab
    );
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE CASE b.name WHEN 'b' THEN true ELSE false END \
             AND EXISTS { (b)<-[:R]-(o:P) WHERE o.name <> a.name } \
             RETURN a.name, b.name"
        ),
        vec![
            ("a".to_string(), "b".to_string()),
            ("d".to_string(), "b".to_string()),
        ]
    );
    assert_eq!(
        pairs(
            "MATCH (a:P)-[:R]->(b:P) WHERE (b)<-[:R]-(:P {name: 'd'}) AND a.name = 'a' \
             RETURN a.name, b.name"
        ),
        ab
    );
    assert_eq!(
        pairs(
            "MATCH (a:P)-[r:R]->(b:P) WHERE size([(b)<-[:R]-(o) | o.name]) = 2 \
             AND r IS NOT NULL RETURN a.name, b.name"
        ),
        vec![
            ("a".to_string(), "b".to_string()),
            ("d".to_string(), "b".to_string()),
        ]
    );
}

#[test]
fn export_database_writes_the_schema_and_import_replays_it() {
    let (dir, g) = open_tmp();
    g.query(
        "CREATE (:User {email: 'a@b.c', name: 'Ann', age: 30})-[:ROAD {code: 'r1', cost: 4, len: 9}]->\
                (:User {email: 'b@b.c', name: 'Bob', age: 41})",
    )
    .unwrap();
    // A node CREATE INDEX provisions the full-text index. A declared node
    // property index exists only through the Graph API and has no Cypher
    // statement, so the export leaves it out rather than turning it into a
    // text index on import.
    g.query("CREATE INDEX FOR (n:User) ON (n.name)").unwrap();
    g.create_node_property_index("User", "city").unwrap();
    g.query("CREATE CONSTRAINT ON (n:User) ASSERT n.email IS UNIQUE")
        .unwrap();
    g.query("CREATE CONSTRAINT ON (n:User) ASSERT EXISTS(n.age)")
        .unwrap();
    g.query("CREATE INDEX FOR ()-[r:ROAD]-() ON (r.cost)")
        .unwrap();
    g.query("CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT r.code IS UNIQUE")
        .unwrap();
    g.query("CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT EXISTS(r.len)")
        .unwrap();

    let export_dir = dir.path().join("export");
    g.query(&format!(
        "EXPORT DATABASE '{}' WITH {{format: 'jsonl'}}",
        export_dir.display()
    ))
    .unwrap();
    let schema = std::fs::read_to_string(export_dir.join("schema.cypher")).unwrap();
    for line in [
        "CREATE CONSTRAINT ON (n:User) ASSERT n.email IS UNIQUE;",
        "CREATE CONSTRAINT ON (n:User) ASSERT EXISTS(n.age);",
        "CREATE INDEX FOR ()-[r:ROAD]-() ON (r.cost);",
        "CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT r.code IS UNIQUE;",
        "CREATE CONSTRAINT ON ()-[r:ROAD]-() ASSERT EXISTS(r.len);",
    ] {
        assert!(
            schema.contains(line),
            "schema.cypher lacks {line}:\n{schema}"
        );
    }
    let index = std::fs::read_to_string(export_dir.join("index.cypher")).unwrap();
    assert!(
        index.contains("CREATE INDEX FOR (n:User) ON (n.name);"),
        "{index}"
    );

    let (_dir2, g2) = open_tmp();
    g2.query(&format!("IMPORT DATABASE '{}'", export_dir.display()))
        .unwrap();

    let mut node_schema = g2.list_node_indexes_and_constraints().unwrap();
    node_schema.sort();
    assert_eq!(
        node_schema,
        vec![
            ("User".to_string(), "age".to_string(), 0x02),
            ("User".to_string(), "email".to_string(), 0x01),
        ]
    );
    let mut edge_schema = g2.list_edge_indexes_and_constraints().unwrap();
    edge_schema.sort();
    assert_eq!(
        edge_schema,
        vec![
            ("ROAD".to_string(), "code".to_string(), 0x01),
            ("ROAD".to_string(), "cost".to_string(), 0x00),
            ("ROAD".to_string(), "len".to_string(), 0x02),
        ]
    );
    assert!(g2.has_text_index("User", "name").unwrap());
    assert!(!g2.has_text_index("User", "city").unwrap());
    assert!(!g2.has_node_property_index("User", "city").unwrap());

    // The replayed constraints enforce.
    assert!(err(&g2, "CREATE (:User {email: 'a@b.c', age: 1})").contains("nique"),);
    assert!(err(&g2, "CREATE (:User {email: 'z@b.c'})").contains("equired"));
    assert_eq!(
        row(
            &g2,
            "MATCH (a:User)-[r:ROAD]->(b:User) RETURN a.name, r.code, b.name"
        ),
        vec![json!("Ann"), json!("r1"), json!("Bob")]
    );

    let missing = dir.path().join("missing");
    assert!(
        err(&g2, &format!("IMPORT DATABASE '{}'", missing.display()))
            .contains("is not a directory")
    );
}
