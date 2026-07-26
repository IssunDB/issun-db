"""Tests for edge insertion and traversal across the binding boundary."""

import json

import pytest
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


def test_add_edges_returns_ids_in_order(db):
    ids = db.add_nodes([("N", "{}"), ("N", "{}"), ("N", "{}")])
    edge_ids = db.add_edges(
        [
            (ids[0], ids[1], "R", json.dumps({"w": 1})),
            (ids[1], ids[2], "R", json.dumps({"w": 2})),
        ]
    )
    assert len(edge_ids) == 2
    first = json.loads(db.get_edge(edge_ids[0]))
    assert first["src"] == ids[0]
    assert first["dst"] == ids[1]
    assert first["props"] == {"w": 1}


def test_add_edges_empty_batch_is_a_noop(db):
    assert db.add_edges([]) == []


def test_add_edges_rejects_a_malformed_item(db):
    with pytest.raises(ValueError):
        db.add_edges([(1, 2, "R")])


def test_add_edges_rolls_back_the_whole_batch_on_failure(db):
    ids = db.add_nodes([("N", "{}"), ("N", "{}")])

    # The second edge names a node that does not exist, so neither edge commits.
    with pytest.raises(RuntimeError):
        db.add_edges(
            [
                (ids[0], ids[1], "R", "{}"),
                (ids[0], 999999, "R", "{}"),
            ]
        )

    assert rows(json.loads(db.query("MATCH ()-[r:R]->() RETURN count(r)"))) == [[0]]
