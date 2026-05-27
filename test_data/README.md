# Test Data

This directory holds versioned LMDB database snapshots used by backwards-compatibility
tests. Each subdirectory corresponds to one IssunDB release and contains:

- `README.md`: what nodes, edges, and indexes are in the snapshot, and which
  version of IssunDB generated it.
- `db/`: the LMDB environment directory (`data.mdb`, `lock.mdb`). These binary
  files are checked into git.

## Regenerating a Snapshot

The `gen_testdata` binary (in `crates/issundb-testing`) generates the current
version's snapshot. Run:

```sh
make testdata
```

This overwrites `test_data/v<current_version>/db/`. Commit the result to record
the current storage format.

## Adding a New Version

1. Run `make testdata` immediately after tagging a release.
2. Commit `test_data/v<new_version>/` to record the format at that version.
3. Write a backwards-compatibility test in `crates/issundb/tests/conformance.rs`
   that opens the old snapshot and verifies queries still return correct results.

## Using Snapshots in Tests

```rust
use std::path::PathBuf;

fn snapshot_path(version: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test_data")
        .join(version)
        .join("db")
}

#[test]
fn can_read_v0_1_0_alpha_1_snapshot() {
    let path = snapshot_path("v0.1.0-alpha.1");
    let g = issundb_core::Graph::open(&path, 1).unwrap();
    // ... assertions
}
```
