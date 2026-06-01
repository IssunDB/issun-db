# IssunDB for Python

Python bindings for [IssunDB](../../README.md), an embedded graph database with vector search, full-text search, and Cypher query support, written in
Rust.

The bindings expose a single `IssunDB` class backed by the native extension.
Property maps and query results cross the boundary as JSON strings, so callers serialize with `json.dumps` on the way in and `json.loads` on the way
out.

## Installation

The package builds from source with [maturin](https://github.com/PyO3/maturin).
It requires a Rust toolchain (see the workspace MSRV) and a C compiler for the vendored GraphBLAS dependency.

For local development, build and install into the active environment:

```bash
pip install maturin
cd crates/issundb-py
maturin develop --features extension-module
```

Or from the repository root, via the Makefile target:

```bash
make develop-py
```

To build a release wheel:

```bash
make wheel-py
```

## Quickstart

```python
import json
from issundb import IssunDB

db = IssunDB("/tmp/my_graph")

# Create nodes; properties are passed as a JSON string.
alice = db.add_node("Person", json.dumps({"name": "Alice", "age": 30}))
bob = db.add_node("Person", json.dumps({"name": "Bob", "age": 25}))

# Connect them with a typed edge.
db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2020}))

# Run a Cypher query; results come back as a JSON string.
result = json.loads(db.query("MATCH (p:Person) RETURN p.name, p.age"))
print(result["columns"])  # ['p.name', 'p.age']
print(result["records"])  # [['Alice', 30], ['Bob', 25]]
```

## API Overview

`IssunDB` is opened against a filesystem directory that holds the LMDB environment. A single handle owns that environment for its lifetime; writes are
serialized internally, so one handle is safe to share.

| Area               | Methods                                               |
|--------------------|-------------------------------------------------------|
| Nodes              | `add_node`, `get_node`, `update_node`, `delete_node`  |
| Edges              | `add_edge`                                            |
| Query              | `query`, `explain`                                    |
| Vector search      | `upsert_vector`, `vector_search`                      |
| Full-text search   | `text_search`, `create_text_index`, `drop_text_index` |
| Backup and restore | `backup`, `backup_compact`, `restore`                 |

Node and edge IDs are non-negative integers. Property maps, Cypher results, and search hits are JSON strings; the result of `query` has the shape
`{"columns": [...], "records": [[...]]}`, and search results are JSON arrays of `{"node": int, "score": float}` or `{"node": int, "distance": float}`
objects.

### Vector and Full-Text Search

```python
import json
from issundb import IssunDB

db = IssunDB("/tmp/search_graph")

doc = db.add_node("Doc", json.dumps({"title": "Graph databases"}))

# Vector search over float32 embeddings.
db.upsert_vector(doc, [0.1, 0.2, 0.3])
hits = json.loads(db.vector_search([0.1, 0.2, 0.3], k=5))

# Full-text search over an indexed property.
db.create_text_index("Doc", "title")
matches = json.loads(db.text_search("graph", label="Doc", property="title", limit=10))
```

### Backup and Restore

```python
db.backup("/tmp/snapshot")  # hot backup
db.backup_compact("/tmp/snapshot")  # compacted hot backup

IssunDB.restore("/tmp/snapshot", "/tmp/restored")
restored = IssunDB("/tmp/restored")
```

## Type Stubs

The package ships `issundb.pyi` and a `py.typed` marker, so editors and type checkers see the full signatures and docstrings without importing the
native module.

## Testing

```bash
make test-py
```

This builds the extension into the active environment and runs the `pytest` suite under `tests/`.
