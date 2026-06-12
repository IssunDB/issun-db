"""Tests for Cypher query execution and plan explanation."""

import json

import pytest

from conftest import rows


def test_query_result_shape(db):
    db.add_node("Person", json.dumps({"name": "Grace"}))
    result = json.loads(db.query("MATCH (n:Person) RETURN n.name AS name"))
    assert set(result.keys()) == {"columns", "records"}
    assert result["columns"] == ["name"]
    assert rows(result) == [["Grace"]]


def test_query_create_then_match(db):
    db.query("CREATE (:City {name: 'Tokyo'})")
    result = json.loads(db.query("MATCH (c:City) RETURN c.name AS name"))
    assert ["Tokyo"] in rows(result)


def test_query_aggregation(db):
    db.add_node("Person", json.dumps({"name": "Ada"}))
    db.add_node("Person", json.dumps({"name": "Bob"}))
    result = json.loads(db.query("MATCH (n:Person) RETURN count(n) AS c"))
    assert rows(result) == [[2]]


def test_explain_returns_plan_text(db):
    plan = db.explain("MATCH (n:Person) RETURN n.name")
    assert isinstance(plan, str)
    assert plan.strip() != ""


def test_query_rejects_invalid_cypher(db):
    with pytest.raises(RuntimeError):
        db.query("MATCH (n RETURN n")


def test_query_with_params(db):
    db.add_node("Person", json.dumps({"name": "Ada"}))
    db.add_node("Person", json.dumps({"name": "Bob"}))
    result = json.loads(
        db.query(
            "MATCH (n:Person) WHERE n.name = $who RETURN n.name AS name",
            json.dumps({"who": "Ada"})
        )
    )
    assert rows(result) == [["Ada"]]
