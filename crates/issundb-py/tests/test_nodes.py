"""Tests for node CRUD across the binding boundary.

Property maps cross as JSON strings, so each test serializes with ``json.dumps``
on the way in and ``json.loads`` on the way out.
"""

import json

import pytest


def test_add_node_returns_id(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    assert isinstance(nid, int)


def test_node_ids_are_distinct(db):
    first = db.add_node("Person", json.dumps({"name": "Ada"}))
    second = db.add_node("Person", json.dumps({"name": "Bob"}))
    assert first != second


def test_get_node_round_trip(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada", "age": 30}))
    props = json.loads(db.get_node(nid))
    assert props == {"name": "Ada", "age": 30}


def test_missing_node_is_none(db):
    assert db.get_node(999) is None


def test_update_node_replaces_props(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    db.update_node(nid, json.dumps({"name": "Charlie"}))
    props = json.loads(db.get_node(nid))
    assert props == {"name": "Charlie"}


def test_delete_node_removes_it(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    db.delete_node(nid)
    assert db.get_node(nid) is None


def test_add_node_rejects_invalid_json(db):
    with pytest.raises(ValueError):
        db.add_node("Person", "not json")


def test_update_node_rejects_invalid_json(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    with pytest.raises(ValueError):
        db.update_node(nid, "{")


def test_add_node_with_multiple_labels(db):
    nid = db.add_node(["Person", "Admin"], json.dumps({"name": "Ada"}))
    assert isinstance(nid, int)
    # A multi-label node matches a pattern that requires both of its labels.
    result = json.loads(db.query("MATCH (n:Person:Admin) RETURN n.name AS name"))
    assert result["columns"] == ["name"]
    assert ["Ada"] in [record["values"] for record in result["records"]]


def test_add_node_rejects_non_string_labels(db):
    with pytest.raises(ValueError):
        db.add_node(42, json.dumps({"name": "Ada"}))


def test_add_nodes_returns_ids_in_order(db):
    ids = db.add_nodes(
        [
            ("Person", json.dumps({"name": "Ada"})),
            ("Person", json.dumps({"name": "Bob"})),
        ]
    )
    assert len(ids) == 2
    assert len(set(ids)) == 2
    assert json.loads(db.get_node(ids[0]))["name"] == "Ada"
    assert json.loads(db.get_node(ids[1]))["name"] == "Bob"


def test_add_nodes_accepts_multi_label_items(db):
    ids = db.add_nodes([(["Person", "Employee"], json.dumps({"name": "Ada"}))])
    result = json.loads(db.query("MATCH (n:Employee) RETURN n.name"))
    assert [record["values"] for record in result["records"]] == [["Ada"]]
    assert len(ids) == 1


def test_add_nodes_accepts_a_generator(db):
    ids = db.add_nodes((("Person", json.dumps({"i": i})) for i in range(5)))
    assert len(ids) == 5


def test_add_nodes_empty_batch_is_a_noop(db):
    assert db.add_nodes([]) == []


def test_add_nodes_rejects_a_malformed_item(db):
    with pytest.raises(ValueError):
        db.add_nodes([("Person",)])


def test_add_nodes_rejects_malformed_props_json(db):
    with pytest.raises(ValueError):
        db.add_nodes([("Person", "{not json")])


def test_add_nodes_rolls_back_the_whole_batch_on_failure(db):
    db.query("CREATE CONSTRAINT ON (n:User) ASSERT n.email IS UNIQUE")
    db.add_node("User", json.dumps({"email": "a@b.c"}))

    # The second item duplicates the constrained value, so the batch must fail
    # and leave neither of its nodes behind.
    with pytest.raises(RuntimeError):
        db.add_nodes(
            [
                ("User", json.dumps({"email": "fresh@b.c"})),
                ("User", json.dumps({"email": "a@b.c"})),
            ]
        )

    result = json.loads(db.query("MATCH (n:User) RETURN count(n)"))
    assert [record["values"] for record in result["records"]] == [[1]]
