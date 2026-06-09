"""Tests for hybrid retrieval (GraphRAG) across the binding boundary.

A retrieval result crosses as a JSON object
``{"nodes": [int], "edges": [int], "scores": {str: float}}``; the scores map is
keyed by stringified node ID.
"""

import json

import pytest


def _seed(db):
    """Two connected Doc nodes, each with an embedding and a full-text index."""
    a = db.add_node("Doc", json.dumps({"body": "the quick brown fox"}))
    b = db.add_node("Doc", json.dumps({"body": "the lazy sleeping dog"}))
    db.upsert_vector(a, [1.0, 0.0, 0.0])
    db.upsert_vector(b, [0.0, 1.0, 0.0])
    db.create_text_index("Doc", "body")
    db.add_edge(a, b, "LINKS", json.dumps({}))
    return a, b


def test_retrieve_hybrid_returns_subgraph_shape(db):
    a, _ = _seed(db)
    result = json.loads(
        db.retrieve_hybrid(
            vector=[1.0, 0.0, 0.0],
            text_query="quick",
            text_label="Doc",
            text_property="body",
            hops=1,
        )
    )
    assert set(result.keys()) == {"nodes", "edges", "scores"}
    assert a in result["nodes"]
    # The seed node carries a fusion score, keyed by its stringified ID.
    assert str(a) in result["scores"]


def test_retrieve_hybrid_expands_over_hops(db):
    a, b = _seed(db)
    result = json.loads(
        db.retrieve_hybrid(
            vector=[1.0, 0.0, 0.0],
            text_query="quick",
            text_label="Doc",
            text_property="body",
            hops=1,
        )
    )
    # One hop from the vector and text seed reaches the linked neighbor.
    assert b in result["nodes"]


def test_retrieve_hybrid_weighted_sum_strategy(db):
    a, _ = _seed(db)
    result = json.loads(
        db.retrieve_hybrid(
            vector=[1.0, 0.0, 0.0],
            text_query="quick",
            text_label="Doc",
            text_property="body",
            hops=1,
            fusion_strategy="weighted_sum",
            vector_weight=0.7,
            text_weight=0.3,
        )
    )
    assert a in result["nodes"]


def test_retrieve_hybrid_rejects_unknown_strategy(db):
    _seed(db)
    with pytest.raises(ValueError):
        db.retrieve_hybrid(
            vector=[1.0, 0.0, 0.0],
            text_query="quick",
            text_label="Doc",
            text_property="body",
            fusion_strategy="harmonic",
        )
