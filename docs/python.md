# Python Integration

This guide covers installation, quickstart examples, vector and text index configuration, and query execution using the `issundb` Python bindings.

---

## Installation

Install the official Python package from PyPI using `pip`:

```bash
pip install issundb
```

### Build from Source

To compile the bindings locally from the repository root, build and install the package using `maturin`:

```bash
# Maturin compiles the native extension and installs it in the environment
pip install maturin
maturin develop --manifest-path crates/issundb-py/Cargo.toml
```

---

## Quickstart

The following example demonstrates opening a database, populating it with nodes and edges, and executing a Cypher query:

Properties are passed across the bindings as JSON strings. Serialize them using `json.dumps` when writing data, and deserialize them using `json.loads` when reading results:

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
# Supported metrics: "cosine", "l2", "dot" (alias "ip")
# Supported quantization: "float32", "float16", "int8"
db.configure_vector_index(metric="cosine", quantization="float32")

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

## Full-text Search

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

# 3. Query the full-text index using keywords
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

* `IssunDB(path: str, map_size_gb: Optional[int] = None)`: Opens or creates a database at `path`. `map_size_gb` defaults to 1 and is a hard ceiling on
  how large the database may grow, not an allocation. There is no resize path: once the data exceeds it every write fails until the database is
  reopened with a larger value, which is safe and keeps the existing data. Size it for the eventual database rather than the current one.
* `set_thread_count(n: int) -> None`: Sets the thread count for the parallel read passes (set `0` for default).

### Node and Edge CRUD

> [!NOTE]
> Every `props` argument must be a JSON object. Valid JSON that is not an object (an array, a number, a string) raises `ValueError`: a property bag
> is a map, and a non-object would be stored but no property read or predicate could ever see it.


* `add_node(labels: Union[str, List[str]], props: str) -> int`: Adds a node and returns its unique ID.
* `add_nodes(items: Iterable[Tuple[Union[str, List[str]], str]]) -> List[int]`: Adds many nodes in one transaction and returns their IDs in order.
* `get_node(id: int) -> Optional[str]`: Retrieves JSON-encoded node properties, or `None`. Properties only; use `node_labels` for the labels.
* `node_labels(id: int) -> List[str]`: Returns a node's labels in insertion order. An unlabeled node and a missing node both give an empty list.
* `update_node(id: int, props: str) -> None`: Overwrites node properties.
* `delete_node(id: int) -> None`: Deletes a node and its incident edges.
* `add_label(id: int, label: str) -> None`: Adds a label to a node.
* `remove_label(id: int, label: str) -> None`: Removes a label from a node.
* `add_edge(src: int, dst: int, etype: str, props: str) -> int`: Adds an edge and returns its unique ID.
* `add_edges(items: Iterable[Tuple[int, int, str, str]]) -> List[int]`: Adds many edges in one transaction and returns their IDs in order.
* `get_edge(id: int) -> Optional[str]`: Retrieves the edge as a JSON-encoded object with `src`, `dst`, `type`, and `props`, or `None`. Unlike `get_node`, this returns the whole record rather than the properties alone.
* `update_edge(id: int, props: str) -> None`: Overwrites edge properties.
* `delete_edge(id: int) -> None`: Deletes an edge.

A single-record insert is one durable LMDB commit, so a Python loop over `add_node` or `add_edge` is bound by commit latency rather than by the work.
Use `add_nodes` and `add_edges` to load data: they write the whole batch under one transaction. Both are all-or-nothing, so any failure rolls back
every record in the batch.

### Querying and Search

* `query(cypher: str, params: Optional[str] = None) -> str`: Executes a Cypher query, optionally with JSON-encoded parameters, and returns the results as a JSON-encoded string with `columns`, `records`, and `statement_count`. A semicolon-separated query runs every statement, but `columns`/`records` reflect only the last one; `statement_count` above 1 signals that the earlier statements' own results were not returned.
* `explain(cypher: str) -> str`: Returns the indented execution plan tree of a Cypher query.
* `configure_vector_index(metric: str, quantization: str = "float32", reindex: bool = False) -> None`: Sets the vector index metric and quantization; pass `reindex=True` to rebuild a populated index under the new configuration.
* `upsert_vector(id: int, vector: List[float]) -> None`: Associates an embedding vector with a node.
* `remove_vector(id: int) -> None`: Removes the embedding for a node.
* `vector_search(vector: List[float], k: int, label: Optional[str] = None, properties: Optional[str] = None, rescore_factor: Optional[int] = None) -> str`: Performs nearest-neighbor vector search with optional label and property filters.
* `create_text_index(label: str, property: str, language: Optional[str] = None) -> None`: Creates a full-text search index. The language controls stemming and stop words and accepts `"english"` (the default), `"spanish"`, `"french"`, `"german"`, `"italian"`, or `"portuguese"`, case-insensitively; an unknown language raises an error.
* `drop_text_index(label: str, property: str) -> None`: Removes a full-text search index.
* `has_text_index(label: str, property: str) -> bool`: Checks whether a full-text search index exists for a label and property.
* `list_text_indexes() -> str`: Returns a JSON list of active full-text search indexes.
* `text_search(query: str, label: Optional[str] = None, property: Optional[str] = None, limit: int = 10) -> str`: Performs keyword text search. Each JSON hit carries `node`, `score`, and the `label` and `property` of the text index that matched it.
* `retrieve_hybrid(vector: Optional[List[float]] = None, text_query: Optional[str] = None, vector_k: int = 10, text_k: int = 10, text_label: Optional[str] = None, text_property: Optional[str] = None, vector_label: Optional[str] = None, hops: int = 2, max_distance: Optional[float] = None, max_nodes: Optional[int] = None, fusion_strategy: str = "rrf", rrf_k: int = 60, vector_weight: float = 0.5, text_weight: float = 0.5) -> str`: Runs a hybrid search and neighborhood expansion, returning a JSON-encoded subgraph with `nodes`, `edges`, `scores`, and a `truncated` flag that is true when `max_nodes` cut off seeds or expansion. Raises `ValueError` when neither `vector` nor `text_query` is given.

### Maintenance and Backups

* `backup(path: str) -> None`: Takes a hot backup snapshot of the database environment.
* `backup_compact(path: str) -> None`: Takes a compacted backup snapshot.
* `restore(snapshot_path: str, destination_path: str) -> None`: Restores a snapshot database to a new directory.
