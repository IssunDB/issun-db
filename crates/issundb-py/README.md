## IssunDB for Python

[![Python version](https://img.shields.io/badge/python-%3E=3.10-3776ab?style=flat&labelColor=282c34&logo=python)](https://github.com/IssunDB/issun-db)
[![PyPI version](https://img.shields.io/pypi/v/issundb?style=flat&labelColor=282c34&color=fc8d62&logo=pypi)](https://pypi.org/project/issundb/)
[![Documentation](https://img.shields.io/badge/docs-read-007ec6?style=flat&labelColor=282c34&logo=readthedocs)](https://issundb.github.io/issun-db/)
[![Examples](https://img.shields.io/badge/examples-view-66c2a5?style=flat&labelColor=282c34&logo=python)](https://github.com/IssunDB/issun-db/tree/main/crates/issundb-py/examples)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-007ec6?style=flat&labelColor=282c34&logo=open-source-initiative)](https://github.com/IssunDB/issun-db)

The Python bindings for [IssunDB](https://github.com/IssunDB/issun-db).

### Installation

```bash
pip install issundb
```

### Quickstart

```python
import json

from issundb import IssunDB

# Open or create a database
db = IssunDB("./issundb-data")

# Add two nodes (with properties)
alice = db.add_node("Person", json.dumps({"name": "Alice", "age": 30}))
bob = db.add_node("Person", json.dumps({"name": "Bob", "age": 28}))

# Add a directed edge between the nodes
db.add_edge(alice, bob, "KNOWS", json.dumps({"since": 2021}))

# Run a Cypher query and print the results
result = json.loads(
    db.query("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since")
)

print(result["columns"])
for record in result["records"]:
    print(record["values"])
```

```
# Output:
['a.name', 'b.name', 'r.since']
['Alice', 'Bob', 2021]
```

### Documentation

Visit [IssunDB's documentation](https://IssunDB.github.io/issun-db/) for detailed information including examples and API references.

### License

The contents of this directory are available under either of these licenses:

* MIT License ([LICENSE-MIT](https://github.com/IssunDB/issun-db/blob/main/LICENSE-MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/IssunDB/issun-db/blob/main/LICENSE-APACHE))
