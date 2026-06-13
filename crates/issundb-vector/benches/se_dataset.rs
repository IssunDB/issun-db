//! Loader for the Stack Exchange multi-vector search datasets.
//!
//! Reads `se_<dataset>_768.parquet` from the directory named by
//! `ISSUNDB_BENCH_SEARCH_DIR` (populated by
//! `scripts/download_search_datasets.sh`). Each row carries `id`, `title`,
//! `body`, and `tags` text plus an `embedding` column holding three
//! 768-dimensional vectors for [title, body, tags]; this loader projects one
//! of those vectors (selected by `ISSUNDB_BENCH_SEARCH_VEC`, default 1 = body)
//! down to `f32` for `upsert_vector`.
//!
//! This module is shared verbatim by the text, vector, and retrieval
//! benchmarks. It is included with `mod se_dataset;`, not as its own bench
//! target, so each crate disables `autobenches` and lists targets explicitly.

#![allow(dead_code)]

use std::{env, fs::File, path::Path, path::PathBuf};

use arrow_array::{
    cast::AsArray,
    types::{Float64Type, Int64Type},
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// Embedding dimensionality of each vector in the dataset.
pub const DIMS: usize = 768;

/// One Stack Exchange post: text fields plus one projected embedding vector.
pub struct Row {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub tags: String,
    pub vec: Vec<f32>,
}

/// The benchmark data directory, or `None` when the data is not present.
pub fn data_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env::var("ISSUNDB_BENCH_SEARCH_DIR").ok()?);
    dir.is_dir().then_some(dir)
}

fn dataset() -> String {
    env::var("ISSUNDB_BENCH_SEARCH_DATASET").unwrap_or_else(|_| "cs".into())
}

fn limit() -> usize {
    env::var("ISSUNDB_BENCH_SEARCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000)
}

fn vec_index() -> usize {
    // 0 = title, 1 = body, 2 = tags.
    env::var("ISSUNDB_BENCH_SEARCH_VEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// Load up to `ISSUNDB_BENCH_SEARCH_LIMIT` rows from the selected dataset.
pub fn load(dir: &Path) -> Vec<Row> {
    let ds = dataset();
    let path = dir.join(format!("se_{ds}_768.parquet"));
    let file = File::open(&path).unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("parquet reader builder")
        .with_batch_size(2048)
        .build()
        .expect("parquet reader build");

    let cap = limit();
    let vi = vec_index();
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.expect("read parquet batch");
        let ids = batch
            .column_by_name("id")
            .expect("id column")
            .as_primitive::<Int64Type>();
        let titles = batch
            .column_by_name("title")
            .expect("title column")
            .as_string::<i32>();
        let bodies = batch
            .column_by_name("body")
            .expect("body column")
            .as_string::<i32>();
        let tags = batch
            .column_by_name("tags")
            .expect("tags column")
            .as_string::<i32>();
        // `embedding` is List<List<Float64>>: one outer entry per row, holding
        // three inner vectors.
        let emb = batch
            .column_by_name("embedding")
            .expect("embedding column")
            .as_list::<i32>();

        for r in 0..batch.num_rows() {
            if rows.len() >= cap {
                return rows;
            }
            let inner = emb.value(r);
            let inner = inner.as_list::<i32>();
            let varr = inner.value(vi);
            let f = varr.as_primitive::<Float64Type>();
            let vec: Vec<f32> = f.values().iter().map(|&x| x as f32).collect();
            rows.push(Row {
                id: ids.value(r),
                title: titles.value(r).to_string(),
                body: bodies.value(r).to_string(),
                tags: tags.value(r).to_string(),
                vec,
            });
        }
    }
    rows
}
