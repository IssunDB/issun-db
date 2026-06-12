# API Reference

This page documents the structures, modules, and extension traits exposed through the public `issundb` facade (the API exposed by `issundb` crate).

## Core Graph Interface

The `Graph` struct is the main coordinator for all transactional graph storage, retrieval, and indexing operations.

### Lifecycle Methods

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>`  
  Opens an LMDB environment at the specified path with a maximum map size capacity.

### Node Management CRUD

- `add_node(label: &str, props: &impl Serialize) -> Result<NodeId, Error>`  
  Adds a new node to the database with a specific label and serializable properties.
- `add_node_multi(labels: &[&str], props: &impl Serialize) -> Result<NodeId, Error>`  
  Adds a new node carrying zero or more labels.
- `get_node(id: NodeId) -> Result<Option<NodeRecord>, Error>`  
  Retrieves a node record by its unique identifier.
- `update_node(id: NodeId, props: &impl Serialize) -> Result<(), Error>`  
  Updates the properties of an existing node.
- `delete_node(id: NodeId) -> Result<(), Error>`  
  Removes a node and all of its associated edges from the graph.

### Label Management

- `add_label(id: NodeId, label: &str) -> Result<(), Error>`  
  Adds a label to an existing node; a no-op if the node already carries it.
- `remove_label(id: NodeId, label: &str) -> Result<(), Error>`  
  Removes a label from a node; a no-op if the node does not carry it.
- `node_labels(id: NodeId) -> Result<Vec<String>, Error>`  
  Returns the label names a node carries.

### Edge and Adjacency CRUD

- `add_edge(src: NodeId, dst: NodeId, etype: &str, props: &impl Serialize) -> Result<EdgeId, Error>`  
  Adds a directed relationship between two nodes with specific properties.
- `get_edge(id: EdgeId) -> Result<Option<EdgeRecord>, Error>`  
  Retrieves an edge record by its unique identifier.
- `update_edge(id: EdgeId, props: &impl Serialize) -> Result<(), Error>`  
  Updates the properties of an existing edge.
- `delete_edge(id: EdgeId) -> Result<(), Error>`  
  Deletes a relationship from the graph.
- `out_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`  
  Retrieves all outgoing relationships and target neighbors for a given node.
- `in_neighbors(node: NodeId) -> Result<Vec<NeighborEntry>, Error>`  
  Retrieves all incoming relationships and source neighbors for a given node.
- `node_has_relationships(node: NodeId) -> Result<bool, Error>`  
  Checks if a node has any incident (incoming or outgoing) relationships.

---

## GraphBLAS Algorithms

The following path-finding, network centrality, and connectivity algorithms are backed by Apache-2.0 SuiteSparse:GraphBLAS operations. They execute on
the in-memory CSR (Compressed Sparse Row) snapshot and the GraphBLAS matrices derived from it. Each algorithm refreshes that state on demand before
running, so committed mutations are always visible; an explicit `graph.rebuild_csr()?` call is never required for correctness.

### Traversal and Paths

- `bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`  
  Performs a Breadth-First Search traversal outward from the start node up to the specified depth.
- `dfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`  
  Performs a Depth-First Search traversal from the start node up to the specified depth.
- `shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`  
  Finds the shortest unweighted path between two nodes.
- `shortest_path_dijkstra(src: NodeId, dst: NodeId) -> Result<Option<WeightedPath>, Error>`  
  Finds the shortest weighted path between two nodes using Dijkstra's algorithm.
- `shortest_path_top_k(src: NodeId, dst: NodeId, k: usize, weight_property: &str) -> Result<Vec<WeightedPath>, Error>`  
  Finds the top-$k$ shortest weighted paths using Yen's algorithm.
- `all_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`  
  Returns all simple paths between the source and destination nodes.
- `all_shortest_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`  
  Returns all shortest unweighted paths between two nodes.
- `longest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`  
  Finds the longest simple path between two nodes.

### Analytics and Centralities

- `page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`  
  Computes PageRank centrality scores across all nodes.
- `degree_centrality(direction: DegreeDirection) -> Result<HashMap<NodeId, u64>, Error>`  
  Computes the degree centrality for each node based on incoming, outgoing, or combined edges.
- `betweenness_centrality() -> Result<HashMap<NodeId, f64>, Error>`  
  Computes the betweenness centrality score for all nodes.
- `harmonic_centrality() -> Result<HashMap<NodeId, f64>, Error>`  
  Computes the harmonic centrality score for all nodes.

### Connectivity and Flow

- `connected_components() -> Result<HashMap<NodeId, u64>, Error>`  
  Finds weakly connected components, returning a mapping of each Node ID to its component label.
- `strongly_connected_components() -> Result<HashMap<NodeId, u64>, Error>`  
  Finds strongly connected components in directed graphs.
- `spanning_forest(weight_property: &str, maximum: bool) -> Result<Vec<EdgeId>, Error>`  
  Computes the Minimum or Maximum Spanning Forest (MSF) of the graph.
- `maximum_flow(src: NodeId, dst: NodeId, capacity_property: &str) -> Result<f64, Error>`  
  Computes the maximum flow capacity between two nodes.
- `detect_cycle() -> Result<bool, Error>`  
  Detects if the graph contains any cycles.
- `count_triangle_cycles(spec: &TriangleCountSpec) -> Result<u64, Error>`  
  Counts the total number of triangles (cycles of length 3) in the graph.
- `label_propagation(max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error>`  
  Partitions the graph into communities using the Label Propagation Algorithm.

---

## Vector Search Extensions

Import the `VectorGraphExt` trait to leverage embedding storage and vector similarity search.

- `VectorGraphExt::configure_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`  
  Configures the metric and quantization for the graph's vector index.
- `VectorGraphExt::reindex_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`  
  Changes the metric and quantization and rebuilds the index from the persisted embeddings.
- `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), VectorError>`  
  Associates a float vector embedding with a node.
- `VectorGraphExt::remove_vector(n: NodeId) -> Result<(), VectorError>`  
  Removes the embedding for a node from the index and from storage.
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>`  
  Retrieves the top-$k$ nearest neighbor nodes matching the query vector.
- `VectorGraphExt::vector_search_with(q: &[f32], opts: &VectorSearchOptions) -> Result<Vec<Hit>, VectorError>`  
  Retrieves the top-$k$ nearest neighbor nodes satisfying label and property filters.

---

## Full-Text Search Extensions

Import the `TextIndexExt` and `TextGraphExt` traits to configure and query text indexes.

- `TextIndexExt::create_text_index(label: &str, property: &str) -> Result<(), TextError>`  
  Creates a full-text search index on a specific node property.
- `TextIndexExt::create_text_index_with_language(label: &str, property: &str, lang: Language) -> Result<(), TextError>`  
  Creates a full-text search index for a specific language.
- `TextIndexExt::drop_text_index(label: &str, property: &str) -> Result<(), TextError>`  
  Removes a full-text search index.
- `TextIndexExt::has_text_index(label: &str, property: &str) -> Result<bool, TextError>`  
  Checks if a full-text search index exists for a label and property.
- `TextIndexExt::list_text_indexes() -> Result<Vec<(String, String, Language)>, TextError>`  
  Lists all active full-text search indexes.
- `TextGraphExt::text_search(query: &str, opts: &TextSearchOptions) -> Result<Vec<TextHit>, TextError>`  
  Queries indexed text fields and ranks matching nodes using BM25 scoring.

---

## Cypher Query Extensions

Import the `GraphQueryExt` trait to run declarative graph queries.

- `query(cypher: &str) -> Result<QueryResult, CypherError>`  
  Executes a raw Cypher query string against the database.
- `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, CypherError>`  
  Executes a parameterized Cypher query against the database.

---

## Cypher DDL Reference

Schema statements run through the same `query` entry point as data statements. A DDL statement targets either nodes of a label, written `(n:Label)`,
or relationships of a type, written `()-[r:TYPE]-()`.

### Index Statements

- `CREATE INDEX FOR (n:Label) ON (n.property)`  
  Creates a full-text search index on a node property. Node property equality and range lookups need no DDL because every node property is indexed
  automatically.
- `CREATE INDEX FOR ()-[r:TYPE]-() ON (r.property)`  
  Creates a relationship property index and backfills it from existing relationships. Relationship properties are indexed only while such an index
  exists; subsequent relationship creation and property updates keep it current.
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

Each `CREATE CONSTRAINT` form has a matching `DROP CONSTRAINT` form with the same target and assertion. Creating a constraint validates the existing
data first and fails if any element already violates it. Once in place, a constraint is checked when an element is created and when its properties are
updated; a violating write fails and leaves the database unchanged.
