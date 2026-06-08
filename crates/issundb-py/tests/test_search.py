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
