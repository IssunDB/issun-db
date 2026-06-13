#!/usr/bin/env bash
#
# Download the Stack Exchange multi-vector search datasets used by the text,
# vector, and hybrid retrieval benchmarks into a git-ignored directory.
#
# Source: https://huggingface.co/datasets/habedi/multi-vector-search-datasets
#
# Each row carries `id` (int64), `title`, `body`, and `tags` (text), plus an
# `embedding` column holding three 768-dimensional vectors for [title, body,
# tags] (generated with all-mpnet-base-v2). This gives one corpus that drives
# full-text search (title/body/tags), vector search (one embedding vector), and
# hybrid retrieval (both on the same nodes).
#
# Requires the Hugging Face CLI (`hf`, or the older `huggingface-cli`):
#   pip install "huggingface_hub[cli]"
#
# Usage:
#   scripts/download_search_datasets.sh                 # Stack Exchange parquet files
#   scripts/download_search_datasets.sh --with-flickr   # also fetch the Flickr8k file
#   ISSUNDB_BENCH_SEARCH_DIR=/some/dir scripts/download_search_datasets.sh
#
# After downloading, point the benchmarks at the directory:
#   export ISSUNDB_BENCH_SEARCH_DIR="$(pwd)/data/multi-vector-search"
#   make bench-text bench-vector bench-retrieval

set -euo pipefail

HF_REPO="habedi/multi-vector-search-datasets"
DEFAULT_DIR="$(git rev-parse --show-toplevel 2>/dev/null || pwd)/data/multi-vector-search"
DATA_DIR="${ISSUNDB_BENCH_SEARCH_DIR:-$DEFAULT_DIR}"

INCLUDE=("--include" "se_cs_768.parquet" "--include" "se_ds_768.parquet" "--include" "se_p_768.parquet")
for arg in "$@"; do
    case "$arg" in
    --with-flickr) INCLUDE+=("--include" "flickr8k_768.parquet") ;;
    -h | --help)
        sed -n '2,28p' "$0"
        exit 0
        ;;
    *)
        echo "Unknown argument: $arg" >&2
        exit 1
        ;;
    esac
done

# The CLI was renamed from `huggingface-cli` to `hf`; prefer the new name.
if command -v hf >/dev/null 2>&1; then
    HF_CLI=hf
elif command -v huggingface-cli >/dev/null 2>&1; then
    HF_CLI=huggingface-cli
else
    echo "Error: Hugging Face CLI not found. Install it with: pip install \"huggingface_hub[cli]\"" >&2
    exit 1
fi

echo "Downloading $HF_REPO into $DATA_DIR"
mkdir -p "$DATA_DIR"
"$HF_CLI" download "$HF_REPO" --repo-type dataset "${INCLUDE[@]}" --local-dir "$DATA_DIR"

echo
echo "Download complete. To run the benchmarks against this data:"
echo "  export ISSUNDB_BENCH_SEARCH_DIR=\"$DATA_DIR\""
echo "  make bench-text bench-vector bench-retrieval"
