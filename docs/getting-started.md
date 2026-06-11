# Getting Started

This guide provides instructions to help you build, configure, and query IssunDB.

## Prerequisites

To compile the database and its native dependencies, ensure you have Rust 1.85.0 or later installed on your system, along with the following:

- **Build Tools**: CMake and a C/C++ compiler (Clang or GCC) to build the SuiteSparse:GraphBLAS static library.
- **FFI Bindings**: `libclang`, which `bindgen` uses to build raw GraphBLAS wrappers.
- **OpenMP Runtime**: Resolves to `libgomp` (bundled with GCC) on Linux, `libomp` on macOS (`brew install libomp`), and `vcomp` (part of the MSVC runtime) on Windows.

## Build from Source

You can build the workspace (including the core storage engine, query layer, and interactive CLI) by running:

```bash
# Clone the repository and initialize submodules
git clone https://github.com/IssunDB/issun-db.git
cd issun-db
git submodule update --init --recursive

# Build release binaries
make build
```

## Basic CLI Usage

Launch the interactive REPL binary to manage and query your database manually:

```bash
# Launch with the default database location
make repl

# Launch with a custom database directory
make repl REPL_PATH=/path/to/my-db
```

### Interactive REPL Meta Commands

These commands manage database sessions, backups, parameters, and bulk imports:

| Command | Usage | Description |
|---|---|---|
| `:open` | `:open /path/to/db` | Open or reopen a database at the specified directory. |
| `:run` | `:run /path/to/script.cypher` | Execute a file line by line. |
| `:save` | `:save /path/to/output.txt` | Direct the output of the next query to a file. |
| `:params` | `:params` | List all current query parameters. |
| `:set` | `:set limit 10` | Set a query parameter value (JSON or string). |
| `:unset` | `:unset limit` | Remove a query parameter. |
| `:backup` | `:backup /path/to/backup.db` | Write a hot backup snapshot of the database. |
| `:backup-compact` | `:backup-compact /path/to/backup.db` | Write a compacted backup snapshot. |
| `:restore` | `:restore /path/to/snapshot.db /path/to/dst` | Restore a snapshot into a new database directory. |
| `:import-jsonl` | `:import-jsonl /path/to/data.jsonl` | Import nodes from a JSONL file. |
| `:import-csv` | `:import-csv /path/to/data.csv` | Import nodes from a CSV file. |
| `:explain` | `:explain MATCH (n) RETURN n` | Explain the physical plan of a Cypher query. |

### Graph Shell Commands

These subcommands perform direct graph operations, algorithm executions, vector search configuration, and full-text search indexing:

| Command | Description |
|---|---|
| `query` (or `cypher`) | Execute a raw Cypher query string (e.g., `query MATCH (n) RETURN n`). |
| `add-node` | Add a node with labels and properties (e.g., `add-node Person {"name": "Alice"}`). |
| `get-node` | Retrieve a node record by its identifier (e.g., `get-node 1`). |
| `update-node` | Overwrite properties on a node (e.g., `update-node 1 {"name": "Bob"}`). |
| `delete-node` | Delete a node and all associated edges (e.g., `delete-node 1`). |
| `add-edge` | Create a directed relationship (e.g., `add-edge 1 2 KNOWS {"since": 2020}`). |
| `get-edge` | Retrieve a relationship record by its identifier (e.g., `get-edge 5`). |
| `delete-edge` | Delete a relationship (e.g., `delete-edge 5`). |
| `out` | Get all outgoing neighbors and relationships of a node (e.g., `out 1`). |
| `in` | Get all incoming neighbors and relationships of a node (e.g., `in 1`). |
| `label` | Find nodes carrying a specific label (e.g., `label Person`). |
| `etype` | Find relationships of a specific type (e.g., `etype KNOWS`). |
| `stats` | Display node and relationship count statistics. |
| `bfs` | Run a Breadth-First Search traversal (e.g., `bfs 1 3`). |
| `dfs` | Run a Depth-First Search traversal (e.g., `dfs 1 3`). |
| `path` | Find the shortest unweighted path between two nodes (e.g., `path 1 2`). |
| `wpath` | Find the shortest weighted path between two nodes (e.g., `wpath 1 2`). |
| `pagerank` | Compute PageRank centrality scores (e.g., `pagerank 20 0.85`). |
| `components` | Find weakly connected components in the graph. |
| `degree` | Compute degree centrality (e.g., `degree out`). |
| `upsert-vec` | Attach/upsert a vector embedding on a node (e.g., `upsert-vec 1 0.1 0.2 0.3`). |
| `vsearch` | Query the vector index for $k$-nearest neighbors (e.g., `vsearch 5 0.1 0.2 0.3`). |
| `retrieve` | Execute hybrid retrieval over vector and text indexes (e.g., `retrieve 5 2 0.1 0.2 --text query`). |
| `configure-vec` | Configure vector index metric and quantization (e.g., `configure-vec cosine int8`). |
| `text-index` | Configure and manage full-text indexes (e.g., `text-index create Book title`). |

---

## Embed in a Rust Project

To use the database in your own Rust application, add the `issundb` facade dependency to your `Cargo.toml` file:

```toml
[dependencies]
issundb = { path = "../path/to/crates/issundb" }
```

You can then open a database environment, execute transactions, and run queries programmatically:

```rust
use std::path::Path;
use issundb::Graph;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the graph database with a map size limit (in GB)
    let graph = Graph::open(Path::new("./data"), 10)?;

    // Insert nodes and edges via the API
    let alice = graph.add_node("Person", &serde_json::json!({ "name": "Alice", "age": 30 }))?;
    let bob = graph.add_node("Person", &serde_json::json!({ "name": "Bob", "age": 25 }))?;
    graph.add_edge(alice, bob, "KNOWS", &serde_json::json!({ "since": 2020 }))?;

    println!("Graph created successfully.");
    Ok(())
}
```
