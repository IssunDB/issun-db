## IssunDB for Python

[![Python version](https://img.shields.io/badge/python-%3E=3.10-3776ab?style=flat&labelColor=282c34&logo=python)](https://github.com/IssunDB/issun-db)
[![PyPI version](https://img.shields.io/pypi/v/issundb?style=flat&labelColor=282c34&color=3776ab&logo=pypi)](https://pypi.org/project/issundb/)
[![Documentation](https://img.shields.io/badge/docs-read-3776ab?style=flat&labelColor=282c34&logo=readthedocs)](https://issundb.github.io/issun-db/)
[![Examples](https://img.shields.io/badge/examples-view-ffd343?style=flat&labelColor=282c34&logo=python)](https://github.com/IssunDB/issun-db/tree/main/crates/issundb-py/examples)
[![License: MIT](https://img.shields.io/badge/license-MIT-ffd343?style=flat&labelColor=282c34&logo=open-source-initiative)](https://github.com/IssunDB/issun-db/blob/main/LICENSE-MIT)

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

# Run a Cypher query and and print the results
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

The content of this directory are avaible under the [MIT License](https://github.com/IssunDB/issun-db/blob/main/LICENSE-MIT).
