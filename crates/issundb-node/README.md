# IssunDB for Node.js

Node.js bindings for [IssunDB](../../README.md), an embedded graph database with vector search, full-text search, and Cypher query support, written in
Rust.

The bindings expose a single `IssunDB` class backed by a native NAPI addon. Property maps and query results cross the boundary as JSON strings, so
callers serialize with `JSON.stringify` on the way in and `JSON.parse` on the way out.

## Installation

The addon builds from source with the [NAPI-RS](https://napi.rs) CLI. It requires a Rust toolchain (see the workspace MSRV) and a C compiler for the
vendored GraphBLAS dependency.

```bash
cd crates/issundb-node
npm install
npm run build
```

Or from the repository root, via the Makefile target:

```bash
make build-node
```

## Quickstart

```js
const {IssunDB} = require('.')

const db = new IssunDB('/tmp/my_graph')

// Create nodes; properties are passed as a JSON string.
const alice = db.addNode('Person', JSON.stringify({name: 'Alice', age: 30}))
const bob = db.addNode('Person', JSON.stringify({name: 'Bob', age: 25}))

// Connect them with a typed edge.
db.addEdge(alice, bob, 'KNOWS', JSON.stringify({since: 2020}))

// Run a Cypher query; results come back as a JSON string.
const result = JSON.parse(db.query('MATCH (p:Person) RETURN p.name, p.age'))
console.log(result.columns) // ['p.name', 'p.age']
console.log(result.records) // [['Alice', 30], ['Bob', 25]]
```

## Node and Edge IDs

Node and edge IDs are 64-bit unsigned integers, which exceed the safe integer range of a JavaScript `number`. They cross the boundary as `BigInt`:
`addNode` and `addEdge` return `BigInt`, and every method that accepts an ID accepts a `BigInt`. Pass IDs through unchanged, or write them as `BigInt`
literals when constructing one by hand:

```js
const id = db.addNode('Person', JSON.stringify({name: 'Carol'}))
db.getNode(id)    // id is already a BigInt
db.getNode(42n)   // BigInt literal
```

A non-integer or negative value passed where an ID is expected raises an error rather than performing a truncated lookup.

## API Overview

`IssunDB` is opened against a filesystem directory that holds the LMDB environment. A single handle owns that environment for its lifetime; writes are
serialized internally, so one handle is safe to share. NAPI camelCases method names, so the Rust `add_node` is `addNode` in JavaScript.

| Area               | Methods                                          |
|--------------------|--------------------------------------------------|
| Nodes              | `addNode`, `getNode`, `updateNode`, `deleteNode` |
| Edges              | `addEdge`                                        |
| Query              | `query`, `explain`                               |
| Vector search      | `upsertVector`, `vectorSearch`                   |
| Full-text search   | `textSearch`, `createTextIndex`, `dropTextIndex` |
| Backup and restore | `backup`, `backupCompact`, `restore`             |

Property maps, Cypher results, and search hits are JSON strings; the result of `query` has the shape `{"columns": [...], "records": [[...]]}`, and
search results are JSON arrays of `{"node": number, "score": number}` or `{"node": number, "distance": number}` objects.

### Vector and Full-Text Search

```js
const {IssunDB} = require('.')

const db = new IssunDB('/tmp/search_graph')
const doc = db.addNode('Doc', JSON.stringify({title: 'Graph databases'}))

// Vector search over float32 embeddings.
db.upsertVector(doc, [0.1, 0.2, 0.3])
const hits = JSON.parse(db.vectorSearch([0.1, 0.2, 0.3], 5))

// Full-text search over an indexed property.
db.createTextIndex('Doc', 'title')
const matches = JSON.parse(db.textSearch('graph', 'Doc', 'title', 10))
```

### Backup and Restore

```js
db.backup('/tmp/snapshot')         // hot backup
db.backupCompact('/tmp/snapshot')  // compacted hot backup

IssunDB.restore('/tmp/snapshot', '/tmp/restored')
const restored = new IssunDB('/tmp/restored')
```

## Testing

```bash
make test-node
```

This builds the addon and runs the `node:test` suite under `test/`.
