# Getting Started

This guide provides instructions to help you get started with building, configuring, and querying IssunDB.

## Build from Source

Ensure you have Rust 1.85.0 or later installed on your system. The engine builds SuiteSparse:GraphBLAS from the vendored submodule, so a native
toolchain is also required:

- CMake and a C/C++ compiler (Clang or GCC) to build the GraphBLAS sources.
- `libclang`, which `bindgen` uses to generate the FFI bindings.
- An OpenMP runtime, resolved per platform: `libgomp` (bundled with GCC) on Linux, `libomp` on macOS (`brew install libomp`), and `vcomp`
  (part of the Visual C++ runtime) on Windows.

You can build the entire workspace, including the core storage library, query layer, and interactive CLI:

```bash
# Clone the repository and initialize submodules
git clone https://github.com/IssunDB/issun-db.git
cd issun-db
make submodules

# Build the release binaries
make build
```

## Basic CLI Usage

IssunDB comes with a Command Line Interface (CLI) that allows you to interact with your graph database using the Cypher query language:

```bash
# Launch the CLI with the default database location
make repl

# Or specify a custom database directory path
make repl REPL_PATH=/path/to/my-db
```

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
    // Open the graph database with a map size limit
    let graph = Graph::open(Path::new("./data"), 10)?;

    // Insert nodes and edges via the API
    let alice = graph.add_node("Person", &serde_json::json!({ "name": "Alice", "age": 30 }))?;
    let bob = graph.add_node("Person", &serde_json::json!({ "name": "Bob", "age": 25 }))?;
    graph.add_edge(alice, bob, "KNOWS", &serde_json::json!({ "since": 2020 }))?;

    println!("Graph created successfully.");
    Ok(())
}
```
