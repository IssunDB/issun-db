## IssunDB for Python

[![Python version](https://img.shields.io/badge/python-%3E=3.10-3776ab?style=flat&labelColor=282c34&logo=python)](https://github.com/IssunDB/issun-db)
[![PyPI version](https://img.shields.io/pypi/v/issundb?style=flat&labelColor=282c34&color=3775a9&logo=pypi)](https://pypi.org/project/issundb/)
[![Documentation](https://img.shields.io/badge/docs-read-00acc1?style=flat&labelColor=282c34&logo=readthedocs)](https://IssunDB.github.io/issun-db/)
[![License: MIT](https://img.shields.io/badge/license-MIT-0288d1?style=flat&labelColor=282c34&logo=open-source-initiative)](../../LICENSE-MIT)

This directory contains the Python bindings for [IssunDB](https://github.com/IssunDB/issun-db).

### Installation

```bash
pip install issundb
```

### Quickstart

Property maps and query results cross the boundary as JSON strings, so callers serialize with `json.dumps` on the way in and `json.loads` on the
way out.

```python
import json

from issundb import IssunDB

# Open (or create) a graph database directory
db = IssunDB("./issundb-data")

# Add two nodes with properties
alice = db.add_node("Person", json.dumps({"name": "Alice", "age": 30}))
bob = db.add_node("Person", json.dumps({"name": "Bob", "age": 28}))

# Add a directed edge between the nodes
db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))

# Run a Cypher query and decode the JSON result
result = json.loads(
    db.query("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since")
)

print(result["columns"])
for record in result["records"]:
    print(record)
```

```
# Output:
['a.name', 'b.name', 'r.since']
['Alice', 'Bob', 2021]
```

### API

The extension exports a single `IssunDB` class.
See [`issundb.pyi`](issundb.pyi) for the full typed surface, including:

- Node and edge CRUD: `add_node`, `get_node`, `update_node`, `delete_node`, and `add_edge`.
- Cypher: `query` and `explain`.
- Vector search: `upsert_vector` and `vector_search`.
- Full-text search: `text_search`, `create_text_index`, and `drop_text_index`.
- Backup and restore: `backup`, `backup_compact`, and `restore`.

### Documentation

Visit IssunDB's [documentation page](https://IssunDB.github.io/issun-db/) for detailed information including examples and API references.

### License

The content in this directory are licensed under the [MIT License](../../LICENSE-MIT).
