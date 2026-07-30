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

## Matching the Documentation Theme

The playground is served under the MkDocs site, so it uses that theme's own colors rather
than an approximation of them. The custom properties at the top of `style.css` are Material
for MkDocs' tokens, copied from the built `palette.*.min.css` for this site's configuration:
deep purple primary (`#7e56c2`), amber accent (`#fa0`), hue 225, and Material's own
foreground and background ramps for the light and dark schemes. The scheme is carried on
`data-md-color-scheme` with the values `default` and `slate`, which are Material's names too,
so both halves of the site are switched by one vocabulary.

Changing `theme.palette` in `mkdocs.yml` therefore means updating that block. To get the new
values, run `make docs` and read them out of `site/assets/stylesheets/palette.*.min.css`
rather than guessing from the Material Design palette, since MkDocs derives its primary from
the named color rather than using it directly.

Inter and JetBrains Mono are named first in the font stacks, as the docs use them, but are
deliberately not fetched: the page loads nothing from a network, so a visitor without them
installed sees the system stack. That is the one respect in which the playground does not
match the documentation exactly.

## Layout

- `index.html`: the page. The inline script in `<head>` applies the stored scheme before
  first paint, so a dark-theme visitor never sees a white flash.
- `app.js`: everything the page does. The Cypher highlighter and the force-directed layout
  are written here rather than pulled from a library, so the page loads nothing it does not
  contain.
- `demos.js`: the demo catalog, checked by `make playground-check`. Each category carries a
  `docs` link into the surrounding documentation; those are relative to `/playground/`, so
  they break if a heading they anchor to is renamed.
- `style.css`: light and dark themes over one set of custom properties.
- `pkg/`: the generated module. A build artifact, not checked in.
