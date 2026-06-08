"""Shared fixtures for the IssunDB Python binding tests.

Every test gets a fresh database in a pytest-managed temporary directory, so no
state leaks across tests and the directory is cleaned up automatically.
"""

import pytest

from issundb import IssunDB


@pytest.fixture
def db(tmp_path):
    """Open a fresh IssunDB graph under a per-test temporary directory."""
    return IssunDB(str(tmp_path / "graph"))
