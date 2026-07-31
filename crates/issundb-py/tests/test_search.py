"""Tests for vector search and full-text search across the binding boundary.

Vector hits cross as a JSON array of ``{"node": int, "distance": float}`` and
text hits as a JSON array of ``{"node": int, "score": float}``.
"""

import json

import pytest


def test_vector_search_finds_nearest(db):
    a = db.add_node("Doc", json.dumps({"title": "a"}))
    b = db.add_node("Doc", json.dumps({"title": "b"}))
    db.upsert_vector(a, [1.0, 0.0, 0.0])
    db.upsert_vector(b, [0.0, 1.0, 0.0])
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 1))
    assert len(hits) == 1
    assert hits[0]["node"] == a
    assert "distance" in hits[0]


def test_vector_search_respects_k(db):
    # Non-zero vectors and a non-zero query so the default cosine metric is
    # well defined for every comparison.
    vectors = [[1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [1.0, 2.0, 0.0]]
    for i, vec in enumerate(vectors):
        nid = db.add_node("Doc", json.dumps({"i": i}))
        db.upsert_vector(nid, vec)
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 2))
    assert len(hits) == 2


def test_text_search_finds_indexed_node(db):
    nid = db.add_node("Article", json.dumps({"body": "the quick brown fox"}))
    db.create_text_index("Article", "body")
    hits = json.loads(db.text_search("quick", "Article", "body", 10))
    assert any(h["node"] == nid for h in hits)
    assert all("score" in h for h in hits)


def test_search_dropped_index_raises(db):
    db.add_node("Article", json.dumps({"body": "the quick brown fox"}))
    db.create_text_index("Article", "body")
    db.drop_text_index("Article", "body")
    # Searching a named (label, property) index that no longer exists surfaces
    # IndexNotFound, which crosses the boundary as a RuntimeError.
    with pytest.raises(RuntimeError):
        db.text_search("quick", "Article", "body", 10)


def test_upsert_vector_rejects_a_node_that_does_not_exist(db):
    """An embedding may only be given to a node that exists.

    Regression: it used to be accepted. Node ids are handed out monotonically, so
    a vector written ahead of its node was inherited by the next node created with
    that id, which then answered a search at distance zero having never been
    embedded. Nothing downstream could detect it, because a stored vector carries
    no evidence of who it was meant for.
    """
    alice = db.add_node("Person", json.dumps({"name": "Alice"}))
    db.upsert_vector(alice, [1.0, 0.0])

    future = alice + 1
    with pytest.raises(RuntimeError, match=f"node {future} does not exist"):
        db.upsert_vector(future, [0.0, 1.0])

    # Bob takes that id and must own no embedding.
    bob = db.add_node("Person", json.dumps({"name": "Bob"}))
    assert bob == future
    hits = json.loads(db.vector_search([0.0, 1.0], 5))
    assert all(h["node"] != bob for h in hits), (
        f"a node that was never embedded must not appear in a vector search: {hits}"
    )


def test_remove_vector_for_a_deleted_node_is_allowed(db):
    """Removal stays permissive, so a vector whose node is gone can still be cleaned up."""
    node = db.add_node("Doc", json.dumps({"title": "x"}))
    db.upsert_vector(node, [1.0, 0.0])
    db.delete_node(node)
    db.remove_vector(node)
