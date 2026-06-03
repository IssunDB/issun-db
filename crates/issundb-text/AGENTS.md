# `issundb-text` Agent Guide


This file covers crate-specific guidance for contributors working inside `crates/issundb-text`. Read the root `AGENTS.md` first; the rules there apply
everywhere and are not repeated here.

## Tokenization Pipeline

The tokenizer currently lives in `crates/issundb-core/src/storage/fts.rs` (the `tokenize` function), pending the target decoupling that moves it into
this crate. Every string is processed through the following stages, applied in this exact order at both **index time** and **query time**:

1. **`fold_ascii`**: normalize diacritics and accented characters to their ASCII base forms (e.g., `é → e`, `ü → u`). This ensures that queries
   without diacritics match documents indexed with them.
2. **`unicode_words` segmentation**: split the folded string into word tokens on Unicode word boundaries.
3. **`to_lowercase`**: fold all tokens to lowercase.
4. **Stop-word filter**: discard tokens that appear in the stop-word list for the active language.
5. **Snowball stemmer**: reduce each surviving token to its stem using the language-appropriate Snowball algorithm.

Applying the stages in a different order at index time versus query time will produce mismatches and silent recall failures. If you change any stage,
change it in both paths simultaneously.

## WAND Correctness Contract

The WAND top-k retrieval algorithm prunes documents by comparing the sum of per-term upper bounds against the current k-th best score. The contract
for `Scorer::upper_bound` is:

```
scorer.upper_bound(idf) >= scorer.score(tf, doc_len, avgdl, idf, 1.0)
```

for **any** `tf`, `doc_len`, and `avgdl`. Violating this contract causes WAND to skip documents that would have been in the top-k results, producing
incorrect output without any compile-time or runtime error.

For `Bm25Scorer`, the tight upper bound is `idf * (k1 + 1)` because BM25's TF saturation factor converges to `(k1 + 1)` as `tf → ∞`.

## Adding a New `Scorer`

When implementing a custom scoring strategy:

1. Implement the `Scorer` trait (`idf`, `score`, `upper_bound`).
2. Write a unit test that asserts the `upper_bound` contract for representative inputs:

   ```rust
   let idf = scorer.idf(100.0, 10.0);
   let ub = scorer.upper_bound(idf);
   for &tf in &[1.0_f32, 2.0, 5.0, 10.0, 100.0] {
       for &(doc_len, avgdl) in &[(10.0, 50.0), (100.0, 50.0), (1.0, 50.0)] {
           let s = scorer.score(tf, doc_len, avgdl, idf, 1.0);
           assert!(s <= ub + 1e-4, "upper_bound violated: ub={ub}, score={s}");
       }
   }
   ```

3. Make the scorer available through `TextSearchOptions::scorer` as an `Arc<dyn Scorer>`.

Do not change the `Scorer` trait signature without updating both built-in implementations (`Bm25Scorer` and `TfIdfScorer`) and their upper-bound
tests.

## `PostingCursor` Invariant

`PostingCursor` wraps a `Vec<(NodeId, tf)>` that must be sorted in ascending `NodeId` order. This invariant is guaranteed by LMDB DUPSORT with
big-endian `u64` keys in `fts_postings`: LMDB stores duplicates in lexicographic byte order, which is ascending numeric order for big-endian integers.

Consequences:

- Never sort or deduplicate a posting list after loading it from `fts_postings`. If the order is wrong, the source write is the bug.
- `PostingCursor::skip_to` uses `partition_point` (binary search). It is only correct when the slice is sorted ascending. Inserting an unsorted entry
  into `fts_postings` will produce silently wrong WAND results.

## `BooleanMode::And` Pre-filtering

`BooleanMode` controls whether a candidate set is computed before building WAND cursors:

- `Some(BooleanMode::And)`: call `boolean_candidate_set` with `And` mode to intersect posting sets for all query terms. Filter each cursor's posting
  list to retain only NodeIds in the candidate set. This eliminates documents missing any query term before scoring begins.
- `Some(BooleanMode::Or)`: call `boolean_candidate_set` with `Or` mode if needed, or skip pre-filtering entirely. WAND naturally scores all documents
  that match at least one term.
- `None` (default): do not apply boolean pre-filtering. WAND handles multi-term relevance ranking with OR semantics: every document matching any query
  term is eligible, and higher multi-term overlap produces a higher BM25 score.

Never apply `And` pre-filtering when `boolean_mode` is `None`; that would silently change the default retrieval semantics.

## Batch Doc-Length Loading

Doc lengths are stored in `fts_docs` keyed by `(LabelId, PropKeyId, NodeId)`. Loading them one call per `(NodeId, term)` pair causes redundant LMDB
reads when multiple terms share candidate documents.

The correct pattern:

1. Collect the union of all candidate `NodeId` values across all cursors (sort and deduplicate the combined list).
2. Call `graph.fts_doc_len` once per unique `NodeId`.
3. Store results in a `HashMap<NodeId, u32>` and pass it to `wand_top_k`.

This pattern is already in place in `TextGraphExt::text_search`; do not regress it by adding per-term doc-length lookups inside the cursor loop.

## Language Addition Checklist

To add support for a new language (the tokenizer constants and selectors live in `crates/issundb-core/src/storage/fts.rs`):

1. **Stop-word list constant definition**: in `crates/issundb-core/src/storage/fts.rs` (e.g., `STOP_WORDS_DUTCH: &[&str]`).
2. **Stop-word retrieval mapping**: an arm for the new language in `get_stop_words` (same file).
3. **Stemmer algorithm mapping**: an arm for the new language in `map_algorithm` (same file; the Snowball stemmer language selector).
4. **Language enum variant**: the new variant in the `Language` enum in `crates/issundb-core/src/schema.rs`, keeping the `#[repr(u8)]` discriminant values contiguous.
5. **Unit test validation**: a unit test in `issundb-core` that round-trips the new `Language` variant through `to_u8` / `from_u8`.


All five steps are required; partial additions will compile but produce incorrect stemming or serialization.

## `RoaringTreemap` Vs. `RoaringBitmap`


`NodeId` is a `u64`. Use `roaring::RoaringTreemap` for all full-text search candidate sets; it covers the full u64 range. `roaring::RoaringBitmap`
covers only u32 and must not be used for NodeId sets. The existing code already uses `RoaringTreemap`; do not introduce `RoaringBitmap` for node sets.
