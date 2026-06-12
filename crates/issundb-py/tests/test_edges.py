"""Tests for edge insertion and traversal across the binding boundary."""

import json

from conftest import rows


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
    assert ["Alice", "Bob", 2021] in rows(result)


def test_get_edge_round_trip(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    eid = db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))
    edge = json.loads(db.get_edge(eid))
    assert edge["src"] == alice
    assert edge["dst"] == bob
    assert edge["type"] == "KNOWS"
    assert edge["props"] == {"since": 2021}


def test_missing_edge_is_none(db):
    assert db.get_edge(999) is None


def test_delete_edge_removes_it(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    eid = db.add_edge(alice, bob, "KNOWS", json.dumps({}))
    db.delete_edge(eid)
    assert db.get_edge(eid) is None
    # The endpoints survive an edge deletion.
    assert db.get_node(alice) is not None
    assert db.get_node(bob) is not None


def test_delete_node_detaches_edges(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    db.add_edge(alice, bob, "KNOWS", json.dumps({}))
    db.delete_node(bob)
    result = json.loads(
        db.query("MATCH (:Person)-[r:KNOWS]->(:Person) RETURN count(r) AS c")
    )
    assert rows(result) == [[0]]


def test_update_edge(db):
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    eid = db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))
    db.update_edge(eid, json.dumps({"since": 2022, "source": "referral"}))
    edge = json.loads(db.get_edge(eid))
    assert edge["props"] == {"since": 2022, "source": "referral"}
