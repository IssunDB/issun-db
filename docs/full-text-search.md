# Full-Text Search

IssunDB's FTS engine is implemented in `crates/issundb-text/` and stores its
inverted index in the `fts_postings` and `fts_docs` LMDB sub-databases owned by
`crates/issundb-core/`.

## Tokenization Pipeline

Both index time and query time apply the same five-stage pipeline in order:

```
Input text
  │
  ▼  fold_ascii          ("café" → "cafe", "über" → "uber")
  ▼  unicode_words       (Unicode word-boundary segmentation)
  ▼  to_lowercase
  ▼  stop-word filter    (language-specific list)
  ▼  Snowball stemmer    (language-specific: English, Spanish, French, German, Italian, Portuguese)
  │
  └─ HashMap<stem, term_frequency>
```

ASCII folding ensures that accented and unaccented spellings of the same word
produce the same index terms. Both query terms and indexed document terms are
folded, so queries without diacritics match documents with them and vice versa.

## Scoring

### BM25 (default)

```
IDF(t) = ln((N - df(t) + 0.5) / (df(t) + 0.5) + 1)

score(d, t) = IDF(t) × tf(t,d) × (k1 + 1)
                       ─────────────────────────────
                       tf(t,d) + k1 × (1 - b + b × |d| / avgdl)
```

Default parameters: k1 = 1.2, b = 0.75 (industry standard for web search).

### TF-IDF (alternative)

```
IDF(t) = ln(N / (df(t) + 1))
score(d, t) = IDF(t) × sqrt(tf(t,d))
```

Lighter than BM25; does not normalize by document length.

### Custom Scorer

Implement the `Scorer` trait and pass it via `TextSearchOptions::scorer`.
The `upper_bound(idf)` method must return a value `>=` the maximum achievable
`score(tf, *, *, idf, 1.0)` for any tf and document length; this bound is used
by WAND for pruning — a looser bound is safe but reduces pruning efficiency.

## WAND Top-K Retrieval

Instead of scoring every candidate document exhaustively, IssunDB uses the
**Weak-AND (WAND)** algorithm to find the top-k results with far fewer full
score evaluations.

Key steps:
1. For each query term, create a `PostingCursor` over its sorted posting list
   (LMDB DUPSORT with big-endian NodeId keys guarantees ascending order; no
   re-sort is needed).
2. Pre-compute `idf` and `upper_bound(idf)` per term cursor.
3. Maintain a min-heap of size k tracking the current k-th best score
   (the pruning threshold).
4. On each iteration:
   - Sort cursors by current document ID.
   - Find the first cursor index where the prefix sum of upper bounds exceeds
     the threshold (the "pivot").
   - If cursor[0] already points to the pivot document: compute the full BM25
     score; advance all cursors at that document.
   - Otherwise: skip all cursors before the pivot to the pivot document via
     binary search.
5. Stop when no pivot can be found (no remaining document can beat the threshold).

WAND is exact: it returns the same top-k results as exhaustive scoring.

## Boolean Pre-Filtering

Set `TextSearchOptions::boolean_mode = Some(BooleanMode::And)` to pre-filter
candidates to only documents containing all query terms before WAND scoring.
This uses `roaring::RoaringTreemap` (required because NodeId is u64) for fast
bitmap intersection and reduces the WAND candidate set for precise multi-term
queries.

`BooleanMode::Or` and the default (`None`) produce the same result: all documents
matching any query term are candidates, with multi-term matches ranked higher by
score accumulation.

## Index Management

```rust
// Create an index (indexes all existing nodes with this label+property).
graph.create_node_text_index("Article", "body")?;

// Drop an index and remove all stored postings.
graph.drop_node_text_index("Article", "body")?;

// Check whether an index exists.
graph.has_node_text_index("Article", "body")?;

// List all active indexes.
graph.active_text_indexes()?;  // Vec<(label, property, Language)>
```

Indexes are maintained automatically: `Graph::update_node` removes old postings
and writes new ones; `Graph::delete_node` removes the node's postings from all
active indexes.
