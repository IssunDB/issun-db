## Examples

| Example                       | Description                                                                                                                                                |
|-------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `quickstart`                  | A basic example showing how to open a database, insert nodes, add links, and execute a Cypher query                                                        |
| `hybrid_retrieval_quickstart` | An end-to-end demo that shows how to create nodes and edges, build a full-text index, upsert vector data, run hybrid retrieval, and execute a Cypher query |
| `load_ldbc`                   | Load a social netwrok graph and run a few graph analytics algorithms (including PageRank, BFS, and shortest path)                                          |
| `neo4j_migration`             | Migrate sample data from a Neo4j-style JSON export into IssunDB                                                                                            |

### Running Examples

```sh
cargo run --example <name>
```

For instance:

```sh
cargo run --example hybrid_retrieval_quickstart
```
