# Python Integration

This guide covers installation, quickstart examples, vector and text index configuration, and query execution using the `issundb` Python bindings.

---

## Installation

Install the official Python package from PyPI using `pip`:

```bash
pip install issundb
```

### Build from Source

To compile the bindings locally from the repository root, build and install the package using `uv` and `maturin`:

```bash
# Maturin compiles the native extension and installs it in the environment
pip install maturin
maturin develop --manifest-path crates/issundb-py/Cargo.toml
```

---

## Quickstart

The following example demonstrates opening a database, populating it with nodes and edges, and executing a Cypher query:

Properties are passed across the bindings as JSON strings, so we serialize them using `json.dumps` when writing data and deserialize them using `json.loads` when reading results:

```python
import json
from issundb import IssunDB

# 1. Open or create the database at a local directory
db = IssunDB("./data", map_size_gb=10)

# 2. Add some nodes with labels and JSON-encoded properties
alice_props = json.dumps({"name": "Alice", "age": 30})
alice_id = db.add_node("Person", alice_props)

bob_props = json.dumps({"name": "Bob", "age": 25})
bob_id = db.add_node("Person", bob_props)

# 3. Connect the nodes with a directed relationship
edge_props = json.dumps({"since": 2021})
edge_id = db.add_edge(alice_id, bob_id, "KNOWS", edge_props)

# 4. Execute a Cypher query and inspect the results
query = "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since"
result_str = db.query(query)
result = json.loads(result_str)

print("Columns:", result["columns"])
for record in result["records"]:
    values = record["values"]
    print(f"{values[0]} knows {values[1]} since {values[2]}")
```

---

## Vector Search

Vector embeddings can be managed to perform nearest-neighbor similarity searches.
Configure the metric and quantization settings before upserting embeddings:

```python
import json
from issundb import IssunDB

db = IssunDB("./data")

# 1. Configure the vector index metric and quantization
# Supported metrics: "cosine", "l2", "ip"
# Supported quantization: "f32", "f16", "i8"
db.configure_vector_index(metric="cosine", quantization="f32")

# 2. Add a document node
doc_props = json.dumps({"title": "Rust Guide", "content": "Database concepts."})
doc_id = db.add_node("Document", doc_props)

# 3. Upsert a 3-dimensional vector embedding for the node
db.upsert_vector(doc_id, [0.1, 0.9, 0.4])

# 4. Query the vector index to find nearest neighbors
query_vector = [0.15, 0.85, 0.35]
results_str = db.vector_search(query_vector, k=5)
results = json.loads(results_str)

for hit in results:
    node_id = hit["node"]
    distance = hit["distance"]
    print(f"Match Node ID: {node_id}, Cosine Distance: {distance:.4f}")
```

---

## Full-Text Search

BM25 full-text indexes can be configured and queried to search unstructured text fields:

```python
import json
from issundb import IssunDB

db = IssunDB("./data")

# 1. Create a full-text search index on the 'description' property of 'Movie' nodes
db.create_text_index("Movie", "description")

# 2. Add movie nodes
inception_props = json.dumps({
    "title": "Inception",
    "description": "A thief who steals corporate secrets through dream-sharing technology."
})
inception_id = db.add_node("Movie", inception_props)

# 3. Query our full-text index using keywords
results_str = db.text_search("secrets dream", label="Movie", property="description", limit=5)
results = json.loads(results_str)

for hit in results:
    node_id = hit["node"]
    score = hit["score"]
    print(f"Match Node ID: {node_id}, BM25 Score: {score:.4f}")
```

---

## Python API Reference

Here is a quick reference of the methods available on the `IssunDB` class:

### Connection and Settings

* `IssunDB(path: str, map_size_gb: Optional[int] = None)`: Opens or creates a database at `path`.
* `set_thread_count(n: int) -> None`: Sets the thread count for parallel GraphBLAS operations (set `0` for default).

### Node and Edge CRUD

* `add_node(labels: Union[str, List[str]], props: str) -> int`: Adds a node and returns its unique ID.
* `get_node(id: int) -> Optional[str]`: Retrieves JSON-encoded node properties, or `None`.
* `update_node(id: int, props: str) -> None`: Overwrites node properties.
* `delete_node(id: int) -> None`: Deletes a node and its incident edges.
* `add_label(id: int, label: str) -> None`: Adds a label to a node.
* `remove_label(id: int, label: str) -> None`: Removes a label from a node.
* `add_edge(src: int, dst: int, etype: str, props: str) -> int`: Adds an edge and returns its unique ID.
* `get_edge(id: int) -> Optional[str]`: Retrieves JSON-encoded edge properties, or `None`.
* `update_edge(id: int, props: str) -> None`: Overwrites edge properties.
* `delete_edge(id: int) -> None`: Deletes an edge.

### Querying and Search

* `query(cypher: str) -> str`: Executes a Cypher query and returns the results as a JSON-encoded string.
* `explain(cypher: str) -> str`: Returns the indented execution plan tree of a Cypher query.
* `configure_vector_index(metric: str, quantization: str) -> None`: Sets vector index configurations.
* `reindex_vector_index(metric: str, quantization: str) -> None`: Rebuilds the vector index under a new configuration.
* `upsert_vector(id: int, vector: List[float]) -> None`: Associates an embedding vector with a node.
* `remove_vector(id: int) -> None`: Removes the embedding for a node.
* `vector_search(vector: List[float], k: int, label: Optional[str] = None, properties: Optional[str] = None) -> str`: Performs nearest-neighbor vector search.
* `create_text_index(label: str, property: str, language: Optional[str] = None) -> None`: Creates a full-text search index.
* `drop_text_index(label: str, property: str) -> None`: Removes a full-text search index.
* `list_text_indexes() -> str`: Returns a JSON list of active full-text search indexes.
* `text_search(query: str, label: Optional[str] = None, property: Optional[str] = None, limit: int = 10) -> str`: Performs keyword text search.
* `retrieve_hybrid(vector: List[float], text_query: str, options: str) -> str`: Runs a hybrid search and neighborhood expansion, returning a JSON-encoded Subgraph.

### Maintenance and Backups

* `backup(path: str) -> None`: Takes a hot backup snapshot of the database environment.
* `backup_compact(path: str) -> None`: Takes a compacted backup snapshot.
* `restore(snapshot_path: str, destination_path: str) -> None`: Restores a snapshot database to a new directory.
