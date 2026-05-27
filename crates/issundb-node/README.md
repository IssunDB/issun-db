# issundb (Node.js)

Node.js bindings for [IssunDB](../../README.md), an embedded graph database with vector and full-text search.

## Installation

```bash
npm install @napi-rs/cli
npm run build
```

## Quick Start

```js
const { IssunDB } = require('./index')

const db = new IssunDB('/tmp/my_graph')
const nodeId = db.addNode('Person', JSON.stringify({ name: 'Alice', age: 30 }))
const result = db.query('MATCH (p:Person) RETURN p.name')
console.log(JSON.parse(result))
```
