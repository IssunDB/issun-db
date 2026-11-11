# Text Agent Guidance

Read the root `AGENTS.md` first.

## Tokenization and Posting Invariants

- Use core's tokenizer for both indexing and queries. Changes must keep the same normalization, segmentation,
  case folding, stop-word filtering, and stemming order on both paths, or matching silently loses recall.
- Posting cursors require ascending node-ID order for binary-search skips. Fix ordering at the write source;
  do not hide a source defect by sorting or deduplicating loaded postings.
- Keep default relevance retrieval eligible for any matching term. Do not apply AND pre-filtering unless requested.
- Load document lengths once per unique candidate node, not once per node and term.
- Use `RoaringTreemap` for node candidate sets. A `RoaringBitmap` cannot represent the full `u64` node-ID range.

## Scorer Correctness

For every valid document length and average length, and every frequency up to `max_tf`, preserve:

```text
scorer.upper_bound(idf, max_tf) >= scorer.score(tf, doc_len, avgdl, idf, 1.0)
```

A bound that is too low silently prunes top-k results. Saturating scorers may use their limit; a score that grows
with frequency must account for `max_tf`. New scorers need upper-bound tests across frequencies and document lengths.
Update built-in implementations and their tests when changing the trait contract.

## Adding Languages

Update stop words, stop-word selection, the Snowball mapping, and the persisted `Language` encoding together.
Keep discriminants contiguous and test `to_u8`/`from_u8` round trips. A partial addition can compile while producing
incorrect stemming or serialization; do not change existing persisted language IDs.
