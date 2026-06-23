## Examples

| File                                                          | Description                                                                                                                                                            |
|---------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| [quickstart](quickstart.rs)                                   | A basic example showing how to open a database, insert nodes, add links, and execute a Cypher query                                                                    |
| [hybrid_retrieval_quickstart](hybrid_retrieval_quickstart.rs) | An end-to-end demo that shows how to create nodes and edges, build a full-text index, upsert vector data, run hybrid retrieval, and execute a Cypher query             |
| [load_ldbc](load_ldbc.rs)                                     | An example loading a social network graph and running a few graph analytics algorithms (including PageRank, connected components, betweenness centrality, and BFS)     |
| [neo4j_migration](neo4j_migration.rs)                         | An example showing how to migrate sample data from a Neo4j-style JSON export into IssunDB                                                                              |
| [graph_analytics](graph_analytics.rs)                         | A demo of using a few raph analytics algorithms in IssunDB, including PageRank, degree centrality, weighted shortest path, label propagation, and connected components |
| [concurrent_ops](concurrent_ops.rs)                           | A demo of concurrent reads and writes over a cloned `Graph` handle that shows transactional snapshot isolation for readers                                             |

### Running Examples

```sh
cargo run --example <name>
```

For instance:

```sh
cargo run --example hybrid_retrieval_quickstart
```
