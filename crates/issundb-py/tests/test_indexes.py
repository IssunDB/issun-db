"""Tests for index administration and filtered search across the binding boundary.

Covers vector index configuration and reindexing, vector search filters, the
full-text index language argument and listing, and GraphBLAS thread control.
"""

import json

import pytest


def test_configure_vector_index_before_upsert(db):
    # Configuration must precede the first upsert; an l2 index then resolves
    # nearest neighbors over the upserted vectors.
    db.configure_vector_index("l2")
    a = db.add_node("Doc", json.dumps({"title": "a"}))
    db.add_node("Doc", json.dumps({"title": "b"}))
    db.upsert_vector(a, [1.0, 0.0, 0.0])
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 1))
    assert hits[0]["node"] == a


def test_configure_vector_index_rejects_unknown_metric(db):
    with pytest.raises(ValueError):
        db.configure_vector_index("manhattan")


def test_reindex_vector_index_on_populated_graph(db):
    a = db.add_node("Doc", json.dumps({"title": "a"}))
    b = db.add_node("Doc", json.dumps({"title": "b"}))
    db.upsert_vector(a, [1.0, 0.0, 0.0])
    db.upsert_vector(b, [0.0, 1.0, 0.0])
    # Reindex rebuilds from the stored vectors under the new metric.
    db.configure_vector_index("l2", reindex=True)
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 1))
    assert hits[0]["node"] == a


def test_vector_search_label_filter(db):
    doc = db.add_node("Doc", json.dumps({"title": "a"}))
    note = db.add_node("Note", json.dumps({"title": "b"}))
    # Both vectors are close to the query; only the label filter separates them.
    db.upsert_vector(doc, [1.0, 0.0, 0.0])
    db.upsert_vector(note, [1.0, 0.1, 0.0])
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 5, "Doc"))
    nodes = [h["node"] for h in hits]
    assert doc in nodes
    assert note not in nodes


def test_vector_search_property_filter(db):
    en = db.add_node("Doc", json.dumps({"lang": "en"}))
    fr = db.add_node("Doc", json.dumps({"lang": "fr"}))
    db.upsert_vector(en, [1.0, 0.0, 0.0])
    db.upsert_vector(fr, [1.0, 0.1, 0.0])
    hits = json.loads(
        db.vector_search([1.0, 0.0, 0.0], 5, None, json.dumps({"lang": "en"}))
    )
    nodes = [h["node"] for h in hits]
    assert en in nodes
    assert fr not in nodes


def test_vector_search_rejects_non_object_properties(db):
    with pytest.raises(ValueError):
        db.vector_search([1.0, 0.0, 0.0], 1, None, "[1, 2, 3]")


def test_create_text_index_with_language(db):
    nid = db.add_node("Article", json.dumps({"body": "les chats dormaient"}))
    db.create_text_index("Article", "body", "french")
    hits = json.loads(db.text_search("chat", "Article", "body", 10))
    # French stemming maps the search term to the indexed plural form.
    assert any(h["node"] == nid for h in hits)


def test_create_text_index_rejects_unknown_language(db):
    db.add_node("Article", json.dumps({"body": "text"}))
    with pytest.raises(ValueError):
        db.create_text_index("Article", "body", "klingon")


def test_list_text_indexes_reports_created_index(db):
    db.create_text_index("Article", "body", "english")
    indexes = json.loads(db.list_text_indexes())
    assert {
        "label": "Article",
        "property": "body",
        "language": "english",
    } in indexes


def test_list_text_indexes_empty_by_default(db):
    assert json.loads(db.list_text_indexes()) == []


def test_set_thread_count_is_accepted(db):
    # A bounded count and the 0 sentinel (restore default) both succeed.
    db.set_thread_count(1)
    db.set_thread_count(0)


def test_label_management(db):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    db.add_label(nid, "Admin")
    # Verify via Cypher
    res = json.loads(db.query("MATCH (n:Admin) RETURN n.name AS name"))
    assert ["Ada"] in [r["values"] for r in res["records"]]

    db.remove_label(nid, "Admin")
    res = json.loads(db.query("MATCH (n:Admin) RETURN n.name AS name"))
    assert len(res["records"]) == 0


def test_has_text_index(db):
    db.create_text_index("Person", "bio")
    assert db.has_text_index("Person", "bio") is True
    assert db.has_text_index("Person", "missing") is False


def test_remove_vector(db):
    db.configure_vector_index("l2")
    nid = db.add_node("Doc", json.dumps({}))
    db.upsert_vector(nid, [1.0, 0.0, 0.0])
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 5))
    assert any(h["node"] == nid for h in hits)

    db.remove_vector(nid)
    hits = json.loads(db.vector_search([1.0, 0.0, 0.0], 5))
    assert not any(h["node"] == nid for h in hits)

    # A repeat removal of the same vector is a no-op.
    db.remove_vector(nid)
