# IssunDB Playground

A single-page app that runs the whole IssunDB engine in a browser tab, compiled to
WebAssembly. There is no server component, and nothing you type, run, or create leaves the tab: the
database lives in the tab's memory, so every query in it is executed by the same Rust code an
embedded application links against. The page's only external request is for its two web fonts, which
is the same request the documentation around it already makes.

It exists to make the engine's surface visible. The example catalog has seven categories:
openCypher queries, the ten graph algorithms, what the optimizer does with a query, full-text
search, vector search, and two aimed at what the engine is built for. **GraphRAG** covers ranking a
corpus by BM25, reaching it by embedding instead of by wording, fusing both scores and expanding the
result in one procedure call, and assembling the context a language model would be handed.
**Knowledge graph** covers entities with typed relations, a three-hop question with a group-by, two
researchers connected by what they write about rather than by an edge, and reach measured in hops.
Each example carries a short note on why the result looks the way it does.

Selecting an example or a sample graph puts it in the editor and stops there; nothing runs until
Execute Query or Explain is pressed. Running a `CREATE` the moment it was clicked wrote to the
database before the reader had seen the statement, and clicking the same example twice quietly
added a second copy of its data. The two examples with a step Cypher cannot express, full-text
search and vector search, keep that step: it is held against the loaded example and runs after the
statement it depends on.

## Sample Graphs

The Setup panel offers five, each small enough to read at once and shaped so that one part of the
engine has something to say about it:

| Sample             | What it is                                                                                        | Examples that query it                                         |
|--------------------|---------------------------------------------------------------------------------------------------|----------------------------------------------------------------|
| Social network     | Weighted acquaintances. Seeded on load.                                                           | Cypher basics, Graph algorithms, Query planning, Vector search |
| Article corpus     | Documents, topics, and citations.                                                                 | Full-text search, GraphRAG                                     |
| Knowledge graph    | Researchers, labs, papers, and the concepts those mention.                                        | Knowledge graph                                                |
| Org chart          | A reporting tree, so variable-length hops, shortest path, and a numeric range scan.               |                                                                |
| Transport network  | Routes carrying a weight, a cost, and a capacity, so a weighted path differs from a shortest one. |                                                                |
| Retail co-purchase | Customers and products, so grouped counts, a price range scan, and a co-purchase join.            |                                                                |

This is the only place a dataset comes from. Every example queries whatever graph is loaded rather
than creating one, which is what keeps the two panels from being two lists of datasets: an earlier
version had the GraphRAG and knowledge-graph examples build their own corpora, one of which
duplicated the Article corpus sample outright.

`Load Graph` puts the selected sample's `CREATE` in the editor. `Reset Graph` discards everything and
re-seeds with it. Each category names the graph it queries, so choosing a category points the picker
at that graph, and an example whose graph is not in the database says which one to load rather than
returning an empty table. The one example that still writes is the Cypher basics lesson on `CREATE`,
and it makes two nodes rather than a dataset.

None of the six carries a comment, and the sidebar carries no prose describing them: the table above
is where that belongs. `make playground-check` seeds each category's own graph before running its
examples, runs all six samples, and fails one that parses but creates no nodes.

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

`make playground-check` runs every sample graph, demo, procedure, and function snippet in
`demos.js` through the compiled module and fails on an error. All three are Cypher inside a JavaScript file, which no
Rust test can see, so this is what keeps a button, a preset, or a reference entry from silently
breaking. The procedure half also rejects `ProcedureNotFound` specifically, which is the failure
a rename produces.

## What This Build Is

The module is built with `--no-default-features`, which selects the two pure-Rust backends:

- Storage is the in-memory backend, because LMDB needs a filesystem and memory mapping
  that `wasm32-unknown-unknown` does not have. Nothing survives a reload, which the Setup panel
  says rather than leaving a visitor to discover it.
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

Inter and JetBrains Mono are fetched from Google Fonts by the link tags in `index.html`. That is the
page's one external request, and it is the same one Material for MkDocs already makes for the same
two families, since `theme.font` in `mkdocs.yml` names them: the playground would otherwise fall back
to the system stack and read in a different typeface from the page a visitor arrived from.

The loaded weights are 300 to 700 for Inter and 400 to 600 for JetBrains Mono. Styling outside those
makes the browser synthesize the difference, which is why the result table's header row is 600 rather
than 700 now that the table is set in the code face.

## Layout

The page is one centered column: a sidebar of four cards (Setup, Examples, Procedures, and Query
History) beside a main column of three (the Cypher editor, a status banner, and the results). It is
deliberately modelled on the Onager playground, which shares this project's Material palette, so the
two read as the same family of tool. Two structural details follow from that model. The page scrolls
as a document rather than the panels scrolling inside a fixed viewport, so the results card carries a
fixed-height pane instead of stretching, which also gives the graph view a defined box to lay out
in. And `color-scheme` is declared per palette, because without it the browser draws its scrollbars
and native controls from the light palette on both schemes.

- `index.html`: the page. The inline script in `<head>` applies the stored scheme before
  first paint, so a dark-theme visitor never sees a white flash.
- `app.js`: everything the page does except run the engine. The Cypher highlighter, the
  autocomplete, the plan tree, and the force-directed layout are written here rather than pulled
  from a library, so the page loads nothing it does not contain.
- `worker.js`: the engine. It owns the wasm module and the graph, and the page reaches it only by
  message, so a query never blocks the tab. See "Cancelling a Query" for what that costs.
- `demos.js`: the demo catalog and the procedure and function references, all checked by
  `make playground-check`. Each demo category carries a `docs` link into the surrounding
  documentation; those are relative to `/playground/`, so they break if a heading they anchor to
  is renamed. Both reference lists are written out by hand because the engine cannot enumerate its
  own procedures, which is why the check runs each snippet.
- `style.css`: light and dark themes over one set of custom properties.
- `logo.svg`: the header logo and the favicon, copied from `docs/assets/logo.svg` by
  `make playground-build` rather than duplicated here, so the documentation stays the one place it
  is edited. Gitignored, like `pkg/`. It is drawn with dark strokes, so the header puts it on a
  white tile instead of recoloring it.
- `pkg/`: the generated module. A build artifact, not checked in.

## The Graph View

The pane is a fixed 580px, because the page scrolls as a document and the force layout reads the
element's size when it starts, so a share of the viewport would make the layout depend on how far down
the page happened to be.

Zoom is the `−` and `+` buttons or the wheel, and dragging the background pans. All three move the same
view box that `Fit` sets and that pointer positions are mapped through, so a vertex drag lands where the
pointer is at any zoom. The wheel holds the point under the cursor still, and its listener is
non-passive: a wheel over the canvas that both zoomed and scrolled the page would be unusable. Zoom is
bounded to a factor of eight in and four out, or a few scrolls leave an empty canvas with no clue which
way the graph went. A redraw returns the view to the whole canvas, so a query is never answered into a
frame computed for different data.

## Cancelling a Query

The engine runs in `worker.js`, so a long query leaves the page responsive and a Cancel button
appears beside Execute. Pressing it terminates the worker, which is the only way to stop a
WebAssembly call: there is no interruption point for a message to be handled at, so nothing short of
killing the thread will do.

The graph lives in the worker and therefore dies with it. Cancelling is still recoverable rather
than destructive, because the page immediately starts a fresh worker, re-seeds the sample, and
replays every statement it recorded as setup, which is the same log a share link carries. A query
that only read loses nothing. One that had already written is reapplied from that log, so anything
typed straight into the editor and run is restored, while a write made by a statement the page never
saw is not.

## What the Footer Reports

The left end reads `This playground app is powered by IssunDB (0.1.0-alpha.20; develop@1f938); ...`.
The version is the crate's, and `develop@1f938` is the branch and short commit the module was built
from. The right end reads `| 6 nodes and 9 edges | 32 KB in use | 19.9 MB heap |`.

`in use` is live bytes the engine has allocated and not freed, counted by a `GlobalAlloc` wrapper the
`issundb-wasm` crate installs and read through `Playground.memoryBytes()`. `heap` is the WebAssembly
linear memory the browser has committed, from `memory.buffer.byteLength` on the module's exports. The
two differ because the wasm heap only ever grows: it settles at the high-water mark and stays there,
so it reads well above what is in use after anything large has been loaded and reset.

The tooltip adds what an empty database accounts for, which is a few kilobytes. That figure is why
the footer shows two numbers rather than three: an earlier version split the live total into engine
and graph, and since an empty database costs almost nothing, it was the same number printed twice.

The build stamp is compiled into the module, read from `ISSUNDB_BUILD_REF` through `option_env!`, rather
than fetched as a sidecar JSON file, so it cannot disagree with the module it describes and the
deployed tree has one fewer file to keep in step. `make playground-build` fills it in from `git`; `docs.yml` sets
it from the workflow's refs instead, since `actions/checkout` leaves a detached HEAD where
`git rev-parse --abbrev-ref HEAD` answers `HEAD` rather than the branch. A build with the variable
unset, outside a git checkout, names the version alone. The crate's `build.rs` exists only to
declare `rerun-if-env-changed` for it, without which cargo would reuse a cached artifact and keep
reporting an earlier build's commit.

## Taking a Query Out and Bringing One Back

`Download` saves the editor as `issundb-query.cypher`, and `Load` replaces it from a `.cypher`, `.cql`,
`.cyp`, or `.txt` file. A loaded file is put in the editor and nothing more, like every other way text
arrives there.

A file over 512 KB is refused with its size in the message rather than loaded. The cap is not about
the engine, which would take far more: it is the highlighter, which rebuilds the whole document's
markup on every change, so a file that was never meant to be edited would stall the tab.

## Sharing a Query

The Share button copies a link carrying the query in the fragment, so it reaches no server even
though the page is hosted on one. It also carries the write statements run this session, under
`s`, and the recipient's page replays them over the seeded sample graph before running the query.
Without that a link over data the sender had created returned nothing for whoever opened it.
Replaying the statements rather than serializing the graph is what makes it exact: a graph
snapshot is capped at 300 nodes and carries no relationship properties.

A link can also be written by hand or generated, using `#cypher=` with percent-encoded plain
text. A generator has to encode a plus as `%2B`, since a fragment read as a query string turns a
literal one into a space. Both forms are applied on a fragment change as well as on load, so a
link followed while the playground is already open is not ignored; one carrying `s` reloads, so
its setup lands on a freshly seeded database rather than on top of the current one.

## Formatting a Query

`Format` in the editor footer, or Shift+Alt+F, rewrites the query's line breaks and keyword casing.
It does two things and no more: one clause per line, with a break after each comma in a `CREATE` or
`MERGE` pattern list, and uppercase for clause keywords and operators. Spacing inside a line is left
as written apart from collapsing runs of whitespace, because re-spacing would have to know that the
`-` in `-[:KNOWS]->` and the `*` in `[r*1..3]` are not binary operators.

The casing rule is narrower than the highlighter's keyword set, and deliberately so. Uppercasing
everything that set contains rewrote `issundb.shortestPath` to `issundb.SHORTESTPATH` and the yield
fields `index` and `count` to `INDEX` and `COUNT`, which are case-sensitive names rather than syntax.
A word is uppercased only if the clause-phrase scan recognized it or it is in a short list of
operators, and never when it follows a `.`, a `:`, or an `AS`. A property that happens to spell a
clause is left where it is for the same reason: `RETURN n.set` is one line, not two.

Because the pass cannot change what a query means, that is testable, and `make playground-check`
tests it: every Cypher string in the catalog is run on two fresh databases, once as written and once
formatted, and the column and row sets compared. Reintroducing any of the three casing bugs above
fails it. The formatter lives in `format.js` rather than `app.js` so the checker can import it,
since `app.js` touches the DOM at import time and a Node process has none.

## Links from the Documentation

`docs/hooks/playground_links.py` is a MkDocs hook that puts a "Run in the playground" link under a
Cypher block marked with `<!-- playground -->` on the line before its fence. The link carries the
block as `q` and every earlier marked block on the same page as `s`, so an example that builds on
one above it still works.

The marker is opt-in because most Cypher in the documentation cannot run here: it binds a query
parameter, it is a CLI script rather than Cypher, or it needs stored embeddings the seeded sample
graph does not have. Of the five Cypher blocks in `docs/examples.md`, one runs. A link landing on an
error is worse than no link, so marking a block is a claim that it runs, and
`make playground-check` holds you to it: every marked block is executed against the seeded graph and
has to return at least one row.

## What the Result Views Cap

A result is rendered as one `innerHTML` assignment, so the table and the JSON pane show at most
the first 1000 rows and the table clips a cell past 200 characters. The row counter reports the
true total, and the CSV and JSON downloads contain every row: the caps are on the view, not on
the result. The graph view is capped separately, at 300 nodes, in the Rust `graphSnapshot`.
