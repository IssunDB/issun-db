# IssunDB Playground

A single-page app that runs the whole IssunDB engine in a browser tab, compiled to
WebAssembly. There is no server component and no network call after the page loads: the
database lives in the tab's memory, so every query in it is executed by the same Rust code
an embedded application links against.

It exists to make the engine's surface visible. The demo catalog covers openCypher
queries, the ten graph algorithms, what the optimizer does with a query, full-text search,
and vector search, each with a short note on why the result looks the way it does.

The published copy is at <https://issundb.github.io/issun-db/playground/>. The docs workflow
builds it and copies this directory into `site/playground/` after the MkDocs build, so the
playground and the documentation share the one GitHub Pages deployment rather than competing
for it.

## Running It

```bash
make playground-deps    # once: the wasm target and the matching wasm-bindgen CLI
make playground-build   # compile the module into web/pkg/
make playground-serve   # http://localhost:8000
```

A module cannot be loaded over `file://`, so the page has to be served over HTTP. Any
static server works; `make playground-serve` uses Python's.

`make playground-check` runs every demo in `demos.js` through the compiled module and
fails on an error. The catalog is Cypher inside a JavaScript file, which no Rust test can
see, so this is what keeps a button from silently breaking.

## What This Build Is

The module is built with `--no-default-features`, which selects the two pure-Rust backends:

- Storage is the in-memory backend, because LMDB needs a filesystem and memory mapping
  that `wasm32-unknown-unknown` does not have. Nothing survives a reload, which the page
  states in the header rather than leaving a visitor to discover.
- The vector index is the exact-scan backend, since the `hnsw` feature selects `usearch`,
  which is C++ and fails the wasm build in `cxx`. Results are the true nearest neighbors
  rather than approximate ones, so the demo is honest, just not sublinear.

Everything else is the ordinary engine: the same parser, planner, optimizer, executor,
counting kernels, CSR snapshot, and BM25 index.

Three further consequences worth knowing:

- There is one thread. `threads::resolve` reports 1 on a wasm target, so the parallel
  reductions run serially.
- The stack is 16 MB, set by a link argument in `.cargo/config.toml`. The default is 1 MB,
  which is also the engine's inline-execution budget for a nested query, leaving no
  margin.
- `backup` and `restore` are absent, as they are file operations.

## Layout

- `index.html`: the page.
- `app.js`: everything the page does. The Cypher highlighter and the force-directed layout
  are written here rather than pulled from a library, so the page loads nothing it does not
  contain.
- `demos.js`: the demo catalog, checked by `make playground-check`.
- `style.css`: light and dark themes over one set of custom properties.
- `pkg/`: the generated module. A build artifact, not checked in.
