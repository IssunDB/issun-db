"""Tests for hot backup, compaction, and restore across the binding boundary."""

import json

from issundb import IssunDB


def test_backup_then_restore_round_trip(db, tmp_path):
    nid = db.add_node("Person", json.dumps({"name": "Ada"}))
    snapshot = tmp_path / "snapshot"
    db.backup(str(snapshot))

    dst = tmp_path / "restored"
    IssunDB.restore(str(snapshot), str(dst))

    restored = IssunDB(str(dst))
    props = json.loads(restored.get_node(nid))
    assert props == {"name": "Ada"}


def test_backup_compact_then_restore_round_trip(db, tmp_path):
    nid = db.add_node("Person", json.dumps({"name": "Bob"}))
    snapshot = tmp_path / "snapshot-compact"
    db.backup_compact(str(snapshot))

    dst = tmp_path / "restored-compact"
    IssunDB.restore(str(snapshot), str(dst))

    restored = IssunDB(str(dst))
    props = json.loads(restored.get_node(nid))
    assert props == {"name": "Bob"}
