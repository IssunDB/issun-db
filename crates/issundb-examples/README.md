# Examples

Standalone runnable examples for IssunDB. Each example is self-contained and
opens a temporary database, so no prior setup is required.

Run any example with:

```sh
cargo run --example <name>
```

## Available Examples

| Example | Description |
|---|---|
| `hybrid_retrieval_quickstart` | End-to-end demo: create nodes and edges, build a full-text index, upsert vectors, run hybrid retrieval, execute a Cypher query |
| `load_ldbc` | Load a hand-crafted LDBC Social Network Benchmark subset and run graph analytics (PageRank, BFS, shortest path) |
| `neo4j_migration` | Migrate sample data from a Neo4j-style JSON export into IssunDB |

## Adding a New Example

1. Add a `.rs` file in this directory.
2. Add a `[[example]]` entry in `examples/Cargo.toml`.
3. Keep the example self-contained: open a `TempDir`, build a graph, demonstrate
   the feature, then exit. Do not write to a persistent path without a CLI argument.
4. Add a one-line description to the table above.
