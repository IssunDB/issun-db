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
