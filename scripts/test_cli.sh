#!/usr/bin/env bash
#
# Test script for the IssunDB CLI.
# This script compiles the CLI binary and runs all help/example commands
# on a temporary database to verify end-to-end functionality.
#
# Exit immediately if a command exits with a non-zero status.
set -e

# Get repository root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

echo "Building IssunDB CLI..."
cargo build -p issundb-cli

# Setup temporary directory for test environment
TEMP_DIR=$(mktemp -d -t issundb-cli-test-XXXXXX)
# Ensure cleanup on exit
cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

DB_PATH="$TEMP_DIR/db"
IMPORT_CYPHER="$TEMP_DIR/import.cypher"
NODES_JSONL="$TEMP_DIR/nodes.jsonl"
NODES_CSV="$TEMP_DIR/nodes.csv"
OUTPUT_TXT="$TEMP_DIR/output.txt"
BACKUP_DB="$TEMP_DIR/backup.db"
COMPACT_DB="$TEMP_DIR/compact.db"

# Create mock data files
echo "MATCH (n) RETURN n" > "$IMPORT_CYPHER"
echo '{"label": "Person", "props": {"name": "Eve"}}' > "$NODES_JSONL"
cat <<EOF > "$NODES_CSV"
label,name
Person,Grace
EOF

echo "Running end-to-end CLI integration test..."

# Run the CLI binary with all example commands
./target/debug/issundb-cli <<EOF
:open $DB_PATH
:threads 4
:threads 0
add-node Person {"name": "Alice"}
add-node Person {"name": "Bob"}
get-node 0
update-node 0 {"name": "Charlie"}
add-edge 0 1 KNOWS {"since": 2020, "cost": 1.5, "capacity": 10.0}
get-edge 0
out 0
in 1
label Person
etype KNOWS
stats
bfs 0 2
dfs 0 2
path 0 1
wpath 0 1
pagerank 20 0.85
components
degree out
degree in
degree both
upsert-vec 0 0.1 0.2 0.3
upsert-vec 1 0.4 0.5 0.6
vsearch 5 0.1 0.2 0.3
retrieve 5 2 0.1 0.2 0.3
text-index create Person name
text-index list
text-search "Charlie" Person name 5
text-index drop Person name
:explain MATCH (n) RETURN n
query MATCH (n) RETURN n
:set limit 10
:set person {"name": "Alice"}
:params
:unset limit
:save $OUTPUT_TXT
query MATCH (n) RETURN n
:run $IMPORT_CYPHER
:backup $BACKUP_DB
:backup-compact $COMPACT_DB
EXPORT DATABASE '$TEMP_DIR/db_export' WITH {format: 'parquet'}
IMPORT DATABASE '$TEMP_DIR/db_export'
:import-jsonl $NODES_JSONL
:import-csv $NODES_CSV
rebuild-csr
delete-edge 0
delete-node 0
:version
help
quit
EOF

echo "CLI Integration Test Passed Successfully!"
