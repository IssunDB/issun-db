## issundb-py

Python bindings for [IssunDB](../../README.md).

### Installation

```bash
pip install maturin
maturin develop --features extension-module
```

### Quickstart

```python
from issundb import IssunDB

db = IssunDB("/tmp/my_graph")
node_id = db.add_node("Person", '{"name": "Alice", "age": 30}')
db.query("MATCH (p:Person) RETURN p.name")
```
