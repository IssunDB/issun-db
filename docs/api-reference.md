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
