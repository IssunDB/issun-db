"""Tests for edge insertion and traversal across the binding boundary."""

import json


def test_add_edge_returns_id(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    eid = db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))
    assert isinstance(eid, int)


def test_edge_is_traversable_with_cypher(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))
    result = json.loads(
        db.query(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) "
            "RETURN a.name AS src, b.name AS dst, r.since AS since"
        )
    )
    assert result["columns"] == ["src", "dst", "since"]
    assert ["Alice", "Bob", 2021] in result["records"]


def test_delete_node_detaches_edges(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    db.add_edge(alice, bob, "KNOWS", json.dumps({}))
    db.delete_node(bob)
    result = json.loads(
        db.query("MATCH (:Person)-[r:KNOWS]->(:Person) RETURN count(r) AS c")
    )
    assert result["records"] == [[0]]
