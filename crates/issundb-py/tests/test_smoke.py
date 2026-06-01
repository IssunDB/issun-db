"""Smoke tests for the IssunDB Python bindings.

Each test opens a fresh database in a temporary directory and exercises one
round-trip across the binding boundary.
"""

import json
import tempfile

from issundb import IssunDB


def test_node_round_trip():
    with tempfile.TemporaryDirectory() as path:
        db = IssunDB(path)
        nid = db.add_node("Person", json.dumps({"name": "Ada"}))
        props = json.loads(db.get_node(nid))
        assert props["name"] == "Ada"


def test_missing_node_is_none():
    with tempfile.TemporaryDirectory() as path:
        db = IssunDB(path)
        assert db.get_node(999) is None


def test_cypher_query():
    with tempfile.TemporaryDirectory() as path:
        db = IssunDB(path)
        db.add_node("Person", json.dumps({"name": "Grace"}))
        result = json.loads(db.query("MATCH (n:Person) RETURN n.name AS name"))
        assert result["columns"] == ["name"]
        assert ["Grace"] in result["records"]
