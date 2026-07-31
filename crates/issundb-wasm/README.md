## issundb-wasm

Browser bindings for [IssunDB](https://github.com/IssunDB/issun-db).

This crate exposes one `Playground` type that owns a single `Graph` and forwards Cypher to it, plus the two capabilities Cypher cannot reach on its own
(full-text index creation and search, and vector upsert and search).
Every method returns a JSON string, so the boundary carries one type in both directions.

It is built for `wasm32-unknown-unknown` with `--no-default-features`, which selects the in-memory storage backend and the exact-scan vector index, since
LMDB needs a filesystem and `usearch` is C++.

The page that loads it lives in [`web/`](../../web); see [`web/README.md`](../../web/README.md) for the build and serve targets.
It is not published to crates.io.

### License

MIT or Apache-2.0.
