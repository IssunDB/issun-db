"""Tests for the deliberate statistics warm-ups.

Nothing in the engine builds either structure as a side effect of a query, so
these two calls are the only way a Python caller gets the optimizer's
expand-ratio estimates, its exact type-inference pruning, and its selectivity
estimates.

What is *not* tested here is that warming changes a plan. Both warm and cold
answers go through the same fallbacks by design (the fan-out estimate drops to
the global average, and the schema question to a bounded probe), so on a graph
this size the plans are identical either way and an assertion on the plan text
would pass without the warm-up. The paths are distinguished in the Rust unit
tests, which can reach the probe budget directly; here the contract is that the
warm-ups are safe to call and never change an answer.
"""

import json

from conftest import rows


def seed(db):
    """Two Person nodes that KNOW each other and one City nobody KNOWS.

    The schema realizes ``Person -KNOWS-> Person`` but never
    ``City -KNOWS-> Person``, which is the pattern type inference can prove
    empty.
    """
    a = db.add_node("Person", json.dumps({"name": "Ada", "age": 36}))
    b = db.add_node("Person", json.dumps({"name": "Grace", "age": 45}))
    db.add_node("City", json.dumps({"name": "London"}))
    db.add_edge(a, b, "KNOWS", json.dumps({}))
    return a, b


def test_materialize_edge_statistics_is_idempotent(db):
    seed(db)
    db.materialize_edge_statistics()
    # Cached against the write generation, so a second call is a no-op rather than
    # a second scan, and neither call disturbs the data.
    db.materialize_edge_statistics()
    result = json.loads(db.query("MATCH (n:Person) RETURN count(n) AS c"))
    assert rows(result) == [[2]]


def test_materialize_property_columns_is_idempotent(db):
    seed(db)
    db.materialize_property_columns()
    db.materialize_property_columns()
    # Property reads agree with the pre-warm answer: the columns are a cache,
    # never the source of truth.
    result = json.loads(
        db.query("MATCH (n:Person) WHERE n.age > 40 RETURN n.name AS name")
    )
    assert rows(result) == [["Grace"]]


def test_warm_ups_do_not_change_answers(db):
    seed(db)
    unsatisfiable = "MATCH (x:City)-[:KNOWS]->(y:Person) RETURN y.name AS name"
    satisfiable = "MATCH (x:Person)-[:KNOWS]->(y:Person) RETURN y.name AS name"
    before = (json.loads(db.query(unsatisfiable)), json.loads(db.query(satisfiable)))

    db.materialize_edge_statistics()
    db.materialize_property_columns()

    after = (json.loads(db.query(unsatisfiable)), json.loads(db.query(satisfiable)))
    assert rows(after[0]) == rows(before[0]) == []
    assert rows(after[1]) == rows(before[1]) == [["Grace"]]


def test_warm_ups_survive_a_later_write(db):
    seed(db)
    db.materialize_edge_statistics()
    db.materialize_property_columns()
    # A write invalidates neither answer: the statistics are advisory or re-probed,
    # so the query keeps returning the truth rather than the warmed snapshot.
    db.add_node("Person", json.dumps({"name": "Alan", "age": 41}))
    result = json.loads(
        db.query("MATCH (n:Person) WHERE n.age > 40 RETURN n.name AS name")
    )
    assert sorted(r[0] for r in rows(result)) == ["Alan", "Grace"]


def test_a_write_that_realizes_a_triple_is_visible_after_warming(db):
    """Warming cannot make the optimizer prune a hop the graph now realizes."""
    a, _ = seed(db)
    db.materialize_edge_statistics()
    city = db.add_node("City", json.dumps({"name": "Paris"}))
    # Now a City does KNOW a Person, in the opposite direction to the warmed
    # snapshot's schema.
    db.add_edge(city, a, "KNOWS", json.dumps({}))
    result = json.loads(
        db.query("MATCH (x:City)-[:KNOWS]->(y:Person) RETURN y.name AS name")
    )
    assert rows(result) == [["Ada"]]


def test_warm_ups_work_on_an_empty_graph(db):
    # No nodes, no edges, no registered labels: both must succeed rather than
    # error on nothing to scan.
    db.materialize_edge_statistics()
    db.materialize_property_columns()
