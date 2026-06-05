# API Reference

This page documents the principal structures, modules, and extension traits exposed through the public `issundb` facade.

## Core Graph Interface

The `Graph` struct is the main coordinator for all transactional graph storage, retrieval, and indexing operations.

### Lifecycle Methods

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>`  
  Opens an LMDB environment at the specified path with a maximum map size capacity.

### Node Management CRUD

- `add_node(label: &str, props: &impl Serialize) -> Result<NodeId, Error>`  
  Adds a new node to the database with a specific label and serializable properties.
- `get_node(id: NodeId) -> Result<Option<NodeRecord>, Error>`  
  Retrieves a node record by its unique identifier.
- `update_node(id: NodeId, props: &impl Serialize) -> Result<(), Error>`  
  Updates the properties of an existing node.
- `delete_node(id: NodeId) -> Result<(), Error>`  
  Removes a node and all of its associated edges from the graph.

### Edge and Adjacency CRUD

- `add_edge(src: NodeId, dst: NodeId, etype: &str, props: &impl Serialize) -> Result<EdgeId, Error>`  
  Adds a directed relationship between two nodes with specific properties.
- `get_edge(id: EdgeId) -> Result<Option<EdgeRecord>, Error>`  
  Retrieves an edge record by its unique identifier.
- `delete_edge(id: EdgeId) -> Result<(), Error>`  
  Deletes a relationship from the graph.
- `out_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`  
  Retrieves all outgoing relationships and target neighbors for a given node.
- `in_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`  
  Retrieves all incoming relationships and source neighbors for a given node.

---

## Vector Search Extensions

Import the `VectorGraphExt` trait to leverage embedding storage and vector similarity search.

- `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), Error>`  
  Associates a high-dimensional float vector embedding with a node.
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, Error>`  
  Retrieves the top-k nearest neighbor nodes matching the query vector.

---

## Full-Text Search Extensions

Import the `TextIndexExt` and `TextGraphExt` traits to configure and query text indexes.

- `TextIndexExt::create_text_index(label: &str, property: &str) -> Result<(), Error>`  
  Creates a full-text search index on a specific node property.
- `TextIndexExt::drop_text_index(label: &str, property: &str) -> Result<(), Error>`  
  Removes a full-text search index.
- `TextGraphExt::text_search(query: &str, opts: &TextSearchOptions) -> Result<Vec<TextHit>, TextError>`  
  Queries indexed text fields and ranks matching nodes using BM25 scoring.

---

## Cypher Query Extensions

Import the `GraphQueryExt` trait to run declarative graph queries.

- `query(cypher: &str) -> Result<QueryResult, String>`  
  Executes a raw Cypher query string against the database.
- `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, String>`  
  Executes a parameterized Cypher query against the database.

---

## Cypher DDL Reference

Schema statements run through the same `query` entry point as data statements. A DDL statement targets either nodes of a label, written `(n:Label)`, or relationships of a type, written `()-[r:TYPE]-()`.

### Index Statements

- `CREATE INDEX FOR (n:Label) ON (n.property)`  
  Creates a full-text search index on a node property. Node property equality and range lookups need no DDL because every node property is indexed automatically.
- `CREATE INDEX FOR ()-[r:TYPE]-() ON (r.property)`  
  Creates a relationship property index and backfills it from existing relationships. Relationship properties are indexed only while such an index exists; subsequent relationship creation and property updates keep it current.
- `DROP INDEX FOR (n:Label) ON (n.property)`  
  Removes the full-text search index on a node property.
- `DROP INDEX FOR ()-[r:TYPE]-() ON (r.property)`  
  Removes a relationship property index and its entries.

### Constraint Statements

- `CREATE CONSTRAINT ON (n:Label) ASSERT n.property IS UNIQUE`  
  Requires the property value to be unique across all nodes with the label.
- `CREATE CONSTRAINT ON (n:Label) ASSERT EXISTS(n.property)`  
  Requires the property to be present and non-null on every node with the label.
- `CREATE CONSTRAINT ON ()-[r:TYPE]-() ASSERT r.property IS UNIQUE`  
  Requires the property value to be unique across all relationships of the type.
- `CREATE CONSTRAINT ON ()-[r:TYPE]-() ASSERT EXISTS(r.property)`  
  Requires the property to be present and non-null on every relationship of the type.

Each `CREATE CONSTRAINT` form has a matching `DROP CONSTRAINT` form with the same target and assertion. Creating a constraint validates the existing data first and fails if any element already violates it. Once in place, a constraint is checked when an element is created and when its properties are updated; a violating write fails and leaves the database unchanged.
