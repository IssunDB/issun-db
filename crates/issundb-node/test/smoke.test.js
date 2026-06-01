// Smoke tests for the IssunDB Node.js bindings.
//
// Each test opens a fresh database in a temporary directory and exercises one
// round-trip across the binding boundary.

const test = require('node:test')
const assert = require('node:assert')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const {IssunDB} = require('..')

function tmpDb() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'issundb-'))
}

test('node round trip', () => {
  const db = new IssunDB(tmpDb())
  const id = db.addNode('Person', JSON.stringify({name: 'Ada'}))
  const props = JSON.parse(db.getNode(id))
  assert.strictEqual(props.name, 'Ada')
})

test('missing node is null', () => {
  const db = new IssunDB(tmpDb())
  // Node IDs cross the boundary as u64, which napi maps to BigInt.
  assert.strictEqual(db.getNode(999n), null)
})

test('cypher query', () => {
  const db = new IssunDB(tmpDb())
  db.addNode('Person', JSON.stringify({name: 'Grace'}))
  const result = JSON.parse(db.query('MATCH (n:Person) RETURN n.name AS name'))
  assert.deepStrictEqual(result.columns, ['name'])
})
