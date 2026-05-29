#!/usr/bin/env bash

# You need to have Graphviz installed to run this script
# On Debian-based OSes, you can install it using: sudo apt-get install graphviz

# Directory containing .dot files. Defaults to the script's own directory so the
# script works regardless of the caller's working directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ASSET_DIR=${1:-"${SCRIPT_DIR}"}

# Make figures from .dot files
for f in "${ASSET_DIR}"/*.dot; do
    dot -Tsvg "$f" -o "${f%.dot}.svg"
done
