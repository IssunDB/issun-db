# Getting Started

This guide covers compiling, configuring, and querying IssunDB.
It explains prerequisites, building the engine from source, and using the command-line interface (CLI) to interact with the database.

## Prerequisites

Compiling IssunDB and its dependencies needs Rust 1.85.0 or later, along with the following system tools:

- Build tools: a C/C++ compiler (such as Clang or GCC), which compiles the bundled LMDB sources and the vector index.

There is nothing else to install: no CMake, no `libclang`, and no OpenMP runtime.

A `--no-default-features` build needs no C or C++ toolchain at all. It selects the in-memory storage backend instead of LMDB and the pure-Rust exact vector index instead of
HNSW, which is also the configuration that compiles for `wasm32-unknown-unknown`. That build does not persist to disk, so treat it as an embedded or browser target rather
than a database you reopen.

## Build from Source

To clone the repository and compile the workspace components (including the storage engine, query layer, and CLI), execute the following commands:

```bash
# Clone the repository (with Git submodules included)
git clone --recursive https://github.com/IssunDB/issun-db.git
cd issun-db

# Build release binaries (this can take a while the first time)
make build
```

## Basic CLI Usage

After building the binaries, run the interactive REPL to manage and query the database directly:

```bash
# Launch the CLI (with the default database location)
make repl

# Launch with a custom database directory
make repl REPL_PATH=/path/to/my-db
```

The built binary can also be invoked directly.
The database directory is a positional argument that falls back to the `ISSUNDB_DB_PATH` environment variable, `--map-size-gb` sets the LMDB map size (default 1), and `--script` (short form `-f`) executes a script file in batch mode instead of starting the prompt:

```bash
target/release/issundb-cli /path/to/my-db --map-size-gb 4
target/release/issundb-cli -f ./setup.cypher
```

### Interactive REPL Meta Commands

The REPL supports meta commands (prefixed with `:`) to manage the session, take backups, and import files:

| Command           | Usage                                           | Description                                                                                                               |
|-------------------|-------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------|
| `:open`           | `:open /path/to/db [map_size_gb]`               | Open or reopen a database at the specified directory; the map size defaults to the launch `--map-size-gb` value.          |
| `:close`          | `:close`                                        | Close the open database without exiting the CLI.                                                                          |
| `:run`            | `:run /path/to/script.cypher`                   | Execute a script file; meta and data commands are one line each, and a Cypher statement may span lines and ends with `;`. |
| `:!`              | `:! ls -la`                                     | Run a shell command from the REPL (alias `:shell`); rejected inside a script file.                                        |
| `:save`           | `:save /path/to/output.txt`                     | Direct the output of the next query to a file.                                                                            |
| `:timer`          | `:timer on`                                     | Report how long each Cypher statement takes, execution only; omit the argument to toggle. Also `--timer` at launch.        |
| `:params`         | `:params`                                       | List all current query parameters.                                                                                        |
| `:set`            | `:set limit 10`                                 | Set a query parameter value (JSON or string).                                                                             |
| `:unset`          | `:unset limit`                                  | Remove a query parameter.                                                                                                 |
| `:backup`         | `:backup /path/to/backup.db`                    | Write a hot backup snapshot of the database.                                                                              |
| `:backup-compact` | `:backup-compact /path/to/backup.db`            | Write a compacted backup snapshot.                                                                                        |
| `:restore`        | `:restore /path/to/snapshot.db /path/to/dst`    | Restore a snapshot into a new database directory.                                                                         |
| `:import-nodes`   | `:import-nodes /path/to/nodes.csv Label`        | Bulk-import nodes from a CSV or Parquet file whose columns become properties.                                             |
| `:import-edges`   | `:import-edges /path/to/edges.csv Src Dst Type` | Bulk-import edges from a two-column CSV or Parquet file of domain keys.                                                   |
| `:explain`        | `:explain MATCH (n) RETURN n`                   | Explain the physical plan of a Cypher query.                                                                              |
| `:threads`        | `:threads 4`                                    | Set the thread count for the parallel read passes, with 0 restoring the default.                                          |
| `:version`        | `:version`                                      | Show the IssunDB version.                                                                                                 |
| `help`            | `help`                                          | Show the built-in command list.                                                                                           |
| `quit`            | `quit`                                          | Exit the CLI (alias `exit`).                                                                                              |

### Graph Shell Commands

The REPL also supports direct operations and queries to manipulate nodes and edges, or execute graph algorithms:

| Command               | Description                                                                                        |
|-----------------------|----------------------------------------------------------------------------------------------------|
| `query` (or `cypher`) | Execute a raw Cypher query string (e.g., `query MATCH (n) RETURN n`).                              |
| `add-node`            | Add a node with labels and properties (e.g., `add-node Person {"name": "Alice"}`).                 |
| `get-node`            | Retrieve a node record by its identifier (e.g., `get-node 1`).                                     |
| `update-node`         | Overwrite properties on a node (e.g., `update-node 1 {"name": "Bob"}`).                            |
| `delete-node`         | Delete a node and all associated edges (e.g., `delete-node 1`).                                    |
| `add-label`           | Add a label to an existing node (e.g., `add-label 1 Admin`).                                       |
| `remove-label`        | Remove a label from a node (e.g., `remove-label 1 Admin`).                                         |
| `add-edge`            | Create a directed relationship (e.g., `add-edge 1 2 KNOWS {"since": 2020}`).                       |
| `get-edge`            | Retrieve a relationship record by its identifier (e.g., `get-edge 5`).                             |
| `update-edge`         | Overwrite properties on a relationship (e.g., `update-edge 5 {"since": 2021}`).                    |
| `delete-edge`         | Delete a relationship (e.g., `delete-edge 5`).                                                     |
| `out`                 | Get all outgoing neighbors and relationships of a node (e.g., `out 1`).                            |
| `in`                  | Get all incoming neighbors and relationships of a node (e.g., `in 1`).                             |
| `label`               | Find nodes carrying a specific label (e.g., `label Person`).                                       |
| `etype`               | Find relationships of a specific type (e.g., `etype KNOWS`).                                       |
| `stats`               | Display node and relationship count statistics.                                                    |
| `bfs`                 | Run a breadth-first search traversal (e.g., `bfs 1 3`).                                            |
| `dfs`                 | Run a depth-first search traversal (e.g., `dfs 1 3`).                                              |
| `path`                | Find the shortest unweighted path between two nodes (e.g., `path 1 2`).                            |
| `wpath`               | Find the shortest weighted path between two nodes (e.g., `wpath 1 2`).                             |
| `pagerank`            | Compute PageRank centrality scores (e.g., `pagerank 20 0.85`).                                     |
| `components`          | Find weakly connected components in the graph.                                                     |
| `degree`              | Compute degree centrality (e.g., `degree out`).                                                    |
| `rebuild-csr`         | Rebuild the in-memory CSR snapshot cache.                                                          |
| `upsert-vec`          | Attach/upsert a vector embedding on a node (e.g., `upsert-vec 1 0.1 0.2 0.3`).                     |
| `remove-vec`          | Remove the vector embedding from a node (e.g., `remove-vec 1`).                                    |
| `vsearch`             | Query the vector index for k-nearest neighbors (e.g., `vsearch 5 0.1 0.2 0.3`).                    |
| `retrieve`            | Execute hybrid retrieval over vector and text indexes (e.g., `retrieve 5 2 0.1 0.2 --text query`). |
| `configure-vec`       | Configure vector index metric and quantization (e.g., `configure-vec cosine int8`).                |
| `text-index`          | Configure and manage full-text indexes (e.g., `text-index create Book title`).                     |
| `text-search`         | Query the BM25 full-text search index (e.g., `text-search "query" Book summary 5`).                |

---

## Embed in a Rust Project

To use IssunDB as an embedded database in a Rust project, add the `issundb` library and `serde_json` to the dependencies in `Cargo.toml`:

```toml
[dependencies]
issundb = "0.1.0-alpha.21"   # Match the version published on crates.io
serde_json = "1.0"           # Used to construct property maps
```

Alternatively, point to a local workspace path:

```toml
[dependencies]
issundb = { path = "../path/to/crates/issundb" }
```

The following example demonstrates opening the database environment, inserting nodes and edges, and handling errors:

```rust
use std::path::Path;
use issundb::Graph;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the graph database with a memory map size limit of 10 GB
    let graph = Graph::open(Path::new("./data"), 10)?;

    // Insert nodes and edges via the API
    let alice = graph.add_node("Person", &serde_json::json!({ "name": "Alice", "age": 30 }))?;
    let bob = graph.add_node("Person", &serde_json::json!({ "name": "Bob", "age": 25 }))?;
    graph.add_edge(alice, bob, "KNOWS", &serde_json::json!({ "since": 2020 }))?;

    println!("Graph created successfully.");
    Ok(())
}
```

Running this code opens the database environment, populates the graph, and prints a success message.
