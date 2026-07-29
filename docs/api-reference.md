# API Reference

This document lists the structures, modules, and extension traits exposed through the public `issundb` library crate.

## Core Graph Interface

The `Graph` struct coordinates all transactional graph storage, retrieval, and indexing operations.

### Lifecycle Methods

- `Graph::open(path: &Path, map_size_gb: usize) -> Result<Self, Error>`  
  Opens the LMDB database environment at the specified path with a maximum map size limit.
- `Graph::view<F, T>(&self, f: F) -> Result<T, Error>`  
  Executes a read-only transaction inside a closure.
- `Graph::update<F, T>(&self, f: F) -> Result<T, Error>`  
  Executes a read-write transaction inside a closure.
- `Graph::set_thread_count(&self, n: i32) -> Result<(), Error>`  
  Sets the thread count for the parallel read passes, overriding the `ISSUNDB_NUM_THREADS` environment variable. Set to `0` to restore default behavior.
- `Graph::backup(&self, destination: &Path) -> Result<(), Error>`  
  Writes a hot backup snapshot of the database environment to the destination file while the graph stays open.
- `Graph::backup_compact(&self, destination: &Path) -> Result<(), Error>`  
  Writes a compacted backup snapshot, reclaiming free pages during the copy.
- `Graph::restore(snapshot_file: &Path, dst_dir: &Path) -> Result<(), Error>`  
  Restores a backup snapshot into a new database directory. This is an associated function; call it before opening the restored graph.
- `Graph::rebuild_csr(&self) -> Result<(), Error>`  
  Rebuilds the in-memory CSR snapshot immediately instead of waiting for the on-demand refresh; useful before a burst of algorithm calls.

### Transactions and Concurrency

IssunDB uses a single-writer, multi-reader model.
Writes are serialized through an internal write lock and one LMDB write transaction at a time; every mutation method (and every write Cypher query) commits atomically. Reads never block writers or one another, so read-heavy workloads scale across threads over one shared `Graph`.

Reader isolation has a boundary worth knowing.
LMDB gives a read transaction a consistent snapshot, so everything inside one `Graph::view` closure observes a single point in time.
A read that does *not* go through `view`, which includes every Cypher query, is not one transaction: each accessor opens its own short read transaction, and the in-memory structures behind the hot read paths (the CSR snapshot and the property columns) are refreshed on their own schedule rather than being tied to a transaction.
A query running concurrently with a committing writer can therefore observe more than one commit point. Reads are always of committed data, never of a partial write, but a whole query is not evaluated against a single snapshot. Wrap the reads in one `Graph::view` closure when a sequence of reads has to agree with itself.

- `Graph::view(f)` runs the closure inside a read-only transaction (`ReadTxn`); every read inside it observes one consistent snapshot.
- `Graph::update(f)` runs the closure inside a read-write transaction (`WriteTxn`); the transaction commits when the closure returns `Ok` and aborts, leaving the database unchanged, when it returns `Err`.
- `ReadTxn` and `WriteTxn` expose the same node, edge, adjacency, and lookup methods as `Graph`, so multi-step logic can run atomically inside one closure.

The `map_size_gb` argument to `Graph::open` sets the maximum size of the LMDB memory map, which bounds the database size. The map is created sparsely, so a generous value costs no disk space up front; a write that would exceed the map fails with a storage error, and the database must be reopened with a larger value to grow it.

### Node Management CRUD

- `add_node(label: &str, props: &impl Serialize) -> Result<NodeId, Error>`  
  Adds a new node to the database with a specific label and serializable properties.
- `add_node_multi(labels: &[&str], props: &impl Serialize) -> Result<NodeId, Error>`  
  Adds a new node carrying zero or more labels.
- `get_node(id: NodeId) -> Result<Option<NodeRecord>, Error>`  
  Retrieves a node record by its unique identifier.
- `update_node(id: NodeId, props: &impl Serialize) -> Result<(), Error>`  
  Updates the properties of an existing node in the database.
- `delete_node(id: NodeId) -> Result<(), Error>`  
  Removes a node and all of its incident edges from the graph.

### Label Management

- `add_label(id: NodeId, label: &str) -> Result<(), Error>`  
  Adds a label to an existing node; this is a no-op if the node already carries it.
- `remove_label(id: NodeId, label: &str) -> Result<(), Error>`  
  Removes a label from a node; this is a no-op if the node does not carry it.
- `node_labels(id: NodeId) -> Result<Vec<String>, Error>`  
  Returns the list of label names that a node currently carries.

### Edge and Adjacency CRUD

- `add_edge(src: NodeId, dst: NodeId, etype: &str, props: &impl Serialize) -> Result<EdgeId, Error>`  
  Creates a directed relationship between two nodes in the graph with specific properties.
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
  Checks if a node has any incident (incoming or outgoing) relationships in the graph.

### Metadata and Index Queries

- `all_nodes() -> Result<Vec<NodeId>, Error>`  
  Returns all node IDs in the graph in ascending order.
- `all_neighbors(node: NodeId) -> Result<Vec<DirectedNeighborEntry>, Error>`  
  Returns directed neighbor entries for all outgoing and incoming edges of a node.
- `nodes_by_label(label: &str) -> Result<Vec<NodeId>, Error>`  
  Returns all node IDs that carry the specified label.
- `edges_by_type(etype: &str) -> Result<Vec<EdgeId>, Error>`  
  Returns all edge IDs of the specified relationship type.
- `label_name(id: LabelId) -> Result<Option<String>, Error>`  
  Resolves a numeric Label ID back to its string name.
- `type_name(id: TypeId) -> Result<Option<String>, Error>`  
  Resolves a numeric Type ID back to its string name.
- `node_count_by_label(label: &str) -> Result<u64, Error>`  
  Returns the count of nodes carrying the specified label.
- `edge_count_by_type(etype: &str) -> Result<u64, Error>`  
  Returns the count of edges of the specified type.

### Property Lookups

Property values are represented by the `PropValue` enum with the variants `Bool`, `Int`, `Float`, and `Str`. Numeric lookups treat `30` and `30.0` as equal, and a range bound only matches values of its own type family (a string value never satisfies a numeric bound).

- `nodes_by_property(label: &str, property: &str, val: PropValue) -> Result<Vec<NodeId>, Error>`  
  Returns the nodes carrying the label whose property equals the value. Node properties are indexed automatically, so no DDL is required.
- `nodes_by_property_range(label: &str, property: &str, min_val: Option<PropValue>, min_inclusive: bool, max_val: Option<PropValue>, max_inclusive: bool) -> Result<Vec<NodeId>, Error>`  
  Returns the nodes whose property value falls inside the range; either bound may be absent.
- `edges_by_property(etype: &str, property: &str, val: PropValue) -> Result<Vec<EdgeId>, Error>`  
  Returns the edges of the type whose property equals the value. Edge properties are indexed only while a relationship property index exists, so create one first.
- `edges_by_property_range(etype: &str, property: &str, min_val: Option<PropValue>, max_val: Option<PropValue>) -> Result<Vec<EdgeId>, Error>`  
  Range form of the edge lookup; both bounds are inclusive.

### Index and Constraint Management

These methods are the Rust equivalents of the Cypher DDL statements in the [Cypher DDL Reference](#cypher-ddl-reference); each creation method validates the existing data first and fails if any element already violates the constraint.

- `create_node_property_index(label: &str, property: &str) -> Result<(), Error>` and `drop_node_property_index(...)`  
  Declares (or removes) a node property index. Because every scalar node property is auto-indexed, the declaration mainly matters as the anchor for constraints.
- `create_node_unique_constraint(label: &str, property: &str) -> Result<(), Error>` and `drop_node_unique_constraint(...)`  
  Requires the property value to be unique across all nodes with the label; explicit nulls never conflict.
- `create_node_required_constraint(label: &str, property: &str) -> Result<(), Error>` and `drop_node_required_constraint(...)`  
  Requires the property to be present and non-null on every node with the label.
- `create_edge_property_index`, `create_edge_unique_constraint`, and `create_edge_required_constraint` (with matching `drop_*` methods)  
  The relationship counterparts, keyed by relationship type instead of label.
- `list_node_indexes_and_constraints() -> Result<Vec<(String, String, u8)>, Error>` and `list_edge_indexes_and_constraints() -> ...`  
  Lists the declared indexes and constraints as `(label_or_type, property, kind)` tuples, where the kind byte is `0x00` for an index, `0x01` for a unique constraint, and `0x02` for a required constraint.

---

## Graph Algorithms

Pathfinding, network centrality, and connectivity algorithms run over the in-memory CSR (Compressed Sparse Row) snapshot. Each algorithm automatically refreshes the snapshot cache on demand, making committed mutations immediately visible without manual calls to `rebuild_csr()`.

### Traversal and Paths

- `bfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`  
  Runs a breadth-first search traversal outward from the start node up to the specified depth.
- `dfs(start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error>`  
  Runs a depth-first search traversal from the start node up to the specified depth.
- `shortest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`  
  Finds the shortest unweighted path between two nodes in the graph.
- `shortest_path_dijkstra(src: NodeId, dst: NodeId) -> Result<Option<WeightedPath>, Error>`  
  Finds the shortest weighted path between two nodes using Dijkstra's algorithm.
- `shortest_path_top_k(src: NodeId, dst: NodeId, k: usize, weight_property: &str) -> Result<Vec<WeightedPath>, Error>`  
  Finds the top-k shortest weighted paths using Yen's algorithm.
- `all_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`  
  Returns all simple paths between the source and destination nodes.
- `all_shortest_paths(src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error>`  
  Returns all shortest unweighted paths between two nodes.
- `longest_path(src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error>`  
  Finds the longest simple path between two nodes.

### Analytics and Centralities

- `page_rank(iterations: u32, damping: f32) -> Result<HashMap<NodeId, f32>, Error>`  
  Computes PageRank centrality scores across all nodes in the graph.
- `degree_centrality(direction: DegreeDirection) -> Result<HashMap<NodeId, u64>, Error>`  
  Computes the degree centrality for each node based on incoming, outgoing, or combined edges.
- `betweenness_centrality() -> Result<HashMap<NodeId, f64>, Error>`  
  Computes the betweenness centrality score for all nodes.
- `harmonic_centrality() -> Result<HashMap<NodeId, f64>, Error>`  
  Computes the harmonic centrality score for all nodes.

### Connectivity and Flow

- `connected_components() -> Result<HashMap<NodeId, u64>, Error>`  
  Finds weakly connected components, mapping each Node ID to its component label.
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

The `VectorGraphExt` trait extends the graph with vector embedding storage and similarity search capability.

The index is configured through `VectorIndexOptions`, which holds a `VectorMetric` (`Cosine`, the default, `L2`, or `Dot`) and a `VectorQuantization` (`Float32`, the default, `Float16`, or `Int8`). The configuration is persisted inside the database, so reopening rebuilds the index with the same settings. Configure the index before the first upsert: changing the metric or quantization once vectors exist returns `VectorError::AlreadyConfigured`, and `reindex_vector_index` is the explicit way to change settings afterward.

`VectorSearchOptions` carries `k`, an optional exact-label filter (`label`), optional property equality filters (`properties`), and `rescore_factor`. On a quantized index a search fetches `k * rescore_factor` candidates (default factor 2) and re-ranks them by exact distance against the full-precision vectors in storage; pass `Some(1)` to disable the rescore. A `Float32` index never rescores by default.

- `VectorGraphExt::configure_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`  
  Configures the metric and quantization parameters for the graph's vector index.
- `VectorGraphExt::reindex_vector_index(opts: VectorIndexOptions) -> Result<(), VectorError>`  
  Changes the metric and quantization settings and rebuilds the index from the persisted embeddings.
- `VectorGraphExt::upsert_vector(n: NodeId, v: &[f32]) -> Result<(), VectorError>`  
  Associates a float vector embedding with a node.
- `VectorGraphExt::remove_vector(n: NodeId) -> Result<(), VectorError>`  
  Removes the embedding for a node from both the index and storage.
- `VectorGraphExt::vector_search(q: &[f32], k: usize) -> Result<Vec<Hit>, VectorError>`  
  Retrieves the top-k nearest neighbor nodes matching the query vector. Searching a graph with no stored embeddings returns
  `VectorError::EmptyIndex` rather than an empty hit list.
- `VectorGraphExt::vector_search_with(q: &[f32], opts: &VectorSearchOptions) -> Result<Vec<Hit>, VectorError>`  
  Retrieves the top-k nearest neighbor nodes satisfying label and property filters. Errors like `vector_search` on a graph with no embeddings.
- `VectorGraphExt::node_vector(n: NodeId) -> Result<Option<Vec<f32>>, VectorError>`  
  Returns the full-precision embedding stored for a node, or `None` if the node has no embedding. This performs an LMDB point lookup and does not build or consult the in-memory HNSW index.
- `VectorGraphExt::vector_distance(a: &[f32], b: &[f32]) -> Result<f32, VectorError>`  
  Computes the distance between two vectors under the graph's configured metric.

---

## Full-text Search Extensions

The `TextIndexExt` and `TextGraphExt` traits enable creating, configuring, and querying full-text search indexes on node properties.

`TextSearchOptions` controls a search: `label` and `property` narrow it to one index (when `None`, every active index is searched and per-node scores are summed), `limit` caps the result count (default 10), `scorer` replaces the default BM25 scorer, and `boolean_mode` selects candidate filtering. `BooleanMode::And` restricts results to documents containing every query term; `BooleanMode::Or` and the default `None` rank any document matching at least one term.

Stemming and stop words are language-aware. `create_text_index` uses English; `create_text_index_with_language` accepts a `Language` value: `English`, `Spanish`, `French`, `German`, `Italian`, or `Portuguese`.

- `TextIndexExt::create_text_index(label: &str, property: &str) -> Result<(), TextError>`  
  Creates a full-text search index on a specific node property.
- `TextIndexExt::create_text_index_with_language(label: &str, property: &str, lang: Language) -> Result<(), TextError>`  
  Creates a full-text search index for a specific language.
- `TextIndexExt::drop_text_index(label: &str, property: &str) -> Result<(), TextError>`  
  Removes a full-text search index.
- `TextIndexExt::has_text_index(label: &str, property: &str) -> Result<bool, TextError>`  
  Checks if a full-text search index exists for a label and property.
- `TextIndexExt::list_text_indexes() -> Result<Vec<(String, String, Language)>, TextError>`  
  Lists all active full-text search indexes in the database.
- `TextGraphExt::text_search(query: &str, opts: &TextSearchOptions) -> Result<Vec<TextHit>, TextError>`  
  Queries indexed text fields and ranks matching nodes using BM25 scoring. Each `TextHit` carries `node`, `score`, and the `label` and `property` of
  the text index that contributed the hit's largest partial score. A request that cannot match anything errors instead of returning an empty list:
  an empty query, a label or property filter naming no active index, or a graph with no text indexes at all.

---

## Hybrid Retrieval Extensions

Hybrid retrieval functions combine vector search and full-text keyword search with multi-source graph expansion:

- `retrieve(graph: &Graph, q: &[f32], k: usize, hops: u8) -> Result<Subgraph, RetrievalError>`  
  Runs a vector search to find `k` seed nodes, then performs a BFS traversal up to `hops` depth to build the result subgraph.
- `retrieve_with(graph: &Graph, q: &[f32], opts: &RetrieveOptions) -> Result<Subgraph, RetrievalError>`  
  Runs a vector search to find seeds with fine-grained control over distance and traversal limits.
- `retrieve_hybrid(graph: &Graph, q: &[f32], text_query: &str, opts: &HybridRetrieveOptions) -> Result<Subgraph, RetrievalError>`  
  Merges seed nodes from vector and full-text keyword searches, fuses their scores, and performs a multi-source expansion. When neither
  search would run (both inputs empty or both k values zero) it returns `RetrievalError::NoQuery`.

The returned `Subgraph` carries `nodes`, `edges`, `scores`, and `truncated`. The `truncated` flag is true when the `max_nodes` cap cut off seeds or
expansion, so missing edges in a capped result do not mean the returned nodes are unconnected.

---

## Cypher Query Extensions

The `GraphQueryExt` trait provides methods to execute Cypher queries against the database:

- `query(cypher: &str) -> Result<QueryResult, CypherError>`  
  Executes a raw Cypher query string against the database.
- `query_with_params(cypher: &str, params: &HashMap<String, serde_json::Value>) -> Result<QueryResult, CypherError>`  
  Executes a parameterized Cypher query against the database.
- `query_with_procedures(cypher: &str, params: &HashMap<String, serde_json::Value>, registry: &ProcedureRegistry) -> Result<QueryResult, CypherError>`  
  Executes a Cypher query resolving `CALL` clauses against a custom procedure registry.
- `explain(cypher: &str) -> Result<String, CypherError>`  
  Compiles and optimizes the physical query plan, returning it as an indented, human-readable tree.

The supported query language surface is documented on the [Cypher Support](cypher.md) page.

### Query Results and Errors

A query returns a `QueryResult` with `columns: Vec<String>` and `records: Vec<Record>`; each `Record` holds `values: Vec<serde_json::Value>` aligned row-major with the columns. Nodes and relationships project as JSON objects, and missing values project as JSON null. NaN and the two infinities have no JSON number representation, so a float result never collapses them to `null` (indistinguishable from a missing value); each projects as a tagged sentinel object instead: `{"__type__": "__NaN__"}`, `{"__type__": "__Infinity__"}`, or `{"__type__": "__-Infinity__"}`.

A query string may contain several semicolon-separated top-level statements in one call (for example `CREATE (n:Person {name: 'Ada'}) RETURN n.name; MATCH (m) WHERE id(m) = 0 RETURN m.name`), which is useful for a CREATE that a later statement in the same call needs to reference. Every statement runs, but `columns`/`records` reflect only the last one: `QueryResult` carries a `statement_count: usize` field (always 1 outside this case) so a caller can tell a multi-statement query apart from an ordinary single-statement one instead of silently reading the final statement's result as if it were the whole query.

Each layer has one error type, and all of them implement `std::error::Error`: `Error` for storage and domain failures (including the `NodeNotFound` and `EdgeNotFound` variants), `CypherError` for parse, plan, and execution failures, `VectorError` for vector index failures (including `AlreadyConfigured` and `DimensionMismatch`), `TextError` for full-text index failures, and `RetrievalError` for hybrid retrieval failures.

---

## Cypher Built-in Procedures

Graph data science procedures can be executed through the query interface using `CALL issundb.<name>(...)`. Results are bound to columns using `YIELD` and can be joined back to nodes using `id()`. Refer to `crates/issundb-examples/gds_cypher.rs` or [Graph Data Science in Cypher](examples.md#graph-data-science-in-cypher) for code examples.

### Analytics and Communities

- `CALL issundb.pageRank({iterations, damping})` yields `(nodeId, score)`. The configuration map is optional.
- `CALL issundb.betweenness()` and `CALL issundb.harmonic()` yield `(nodeId, score)`. Both take no arguments.
- `CALL issundb.degree({direction})` yields `(nodeId, score)`, where `direction` is `'IN'`, `'OUT'`, or `'BOTH'` (the default).
- `CALL issundb.connectedComponents()` (alias `issundb.wcc`) and `CALL issundb.stronglyConnectedComponents()` (alias `issundb.scc`) yield `(nodeId, componentId)`.
- `CALL issundb.labelPropagation({maxIterations})` yields `(nodeId, communityId)`.
- `CALL issundb.communities({maxIterations, topPerCommunity})` yields `(communityId, nodeId, rank)`, partitioning by label propagation and ranking each community by PageRank.

### Pathfinding

- `CALL issundb.shortestPath(srcId, dstId)` yields `(index, nodeId)` for the hop sequence, or no rows when the target is unreachable.
- `CALL issundb.dijkstra(srcId, dstId)` yields `(index, nodeId, totalWeight)`, using the first present of the `weight`, `cost`, `capacity`, or `cap` edge property as the edge weight.
- `CALL issundb.triangleCount({relTypes, labels})` yields a single `(count)` row for the directed triangle pattern. The configuration map and each of its fields are optional.

### GraphRAG Retrieval

- `CALL issundb.retrieve.vector(queryVector, {k, hops, maxDistance, maxNodes})` yields `(nodeId, distance)`. Seed nodes carry a distance; nodes reached only by expansion carry a null distance.
- `CALL issundb.retrieve.hybrid(queryVector, queryText, {vectorK, textK, hops, maxDistance, maxNodes, textLabel, textProperty, vectorLabel, fusion})` yields `(nodeId, score)`, fusing vector and full-text relevance before expansion. The `fusion` field is the string `'rrf'`, or a map `{rrfK}` or `{vectorWeight, textWeight}`.

For both retrieval procedures the leading vector and text arguments are required and the configuration map is optional.

---

## Cypher Functions

The following scalar functions are available inside any Cypher expression position:

- `vector_dist(node_or_vector, query_vector)`  
  Distance between a node's stored embedding (or a numeric vector) and a query vector, under the graph's configured vector index metric. An ascending `ORDER BY vector_dist(node, query)` with a `LIMIT` over a labeled scan is answered by a single HNSW index search.
- `issundb.distance.cosine(a, b)` and `issundb.distance.euclidean(a, b)`  
  Cosine distance (in `[0, 2]`) and Euclidean (L2) distance (in `[0, ∞)`) between two vectors. Each argument is a numeric list or a node, in which case its stored embedding is resolved.
- `issundb.similarity.jaccard(a, b)` and `issundb.similarity.overlap(a, b)`  
  Jaccard similarity and overlap coefficient (both in `[0, 1]`) between two lists treated as sets.

Each measure has a single canonical form, so the opposite direction is a short inline expression: cosine similarity is `1 - issundb.distance.cosine(a, b)`, Euclidean similarity is `1.0 / (1.0 + issundb.distance.euclidean(a, b))`, and a set distance is `1 - issundb.similarity.jaccard(a, b)`. A null operand, or a vector length mismatch, yields null.

---

## Cypher DDL Reference

Schema statements are executed through the query interface. A DDL statement targets either nodes with a specific label, written `(n:Label)`, or relationships of a specific type, written `()-[r:TYPE]-()`.

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
