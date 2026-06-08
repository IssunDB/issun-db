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


def rows(result):
    """Extract the row cell lists from a decoded query result.

    A query result is ``{"columns": [...], "records": [{"values": [...]}, ...]}``;
    this returns just the list of per-record ``values`` lists.
    """
    return [record["values"] for record in result["records"]]
