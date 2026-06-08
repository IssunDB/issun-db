use issundb_core::{Graph, Language, NodeId};
use roaring::RoaringTreemap;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    sync::Arc,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TextError {
    #[error("core storage error: {0}")]
    Core(#[from] issundb_core::Error),

    #[error("index not found for label {label} and property {property}")]
    IndexNotFound { label: String, property: String },
}

/// A single ranked full-text search result.
#[derive(Debug, Clone)]
pub struct TextHit {
    pub node: NodeId,
    pub score: f32,
}

// ------------------------------------------------------------------
// Scoring traits and implementations
// ------------------------------------------------------------------

/// Relevance scoring strategy for full-text search.
///
/// Implement this trait to provide a custom scoring model. The standard
/// implementations are [`Bm25Scorer`] (default, recommended) and
/// [`TfIdfScorer`] (simpler, no length normalization).
pub trait Scorer: Send + Sync {
    /// Inverse document frequency component.
    ///
    /// `n_docs` is the total document count and `n_term` is the number of
    /// documents containing the term.
    fn idf(&self, n_docs: f32, n_term: f32) -> f32;

    /// Per-document term score given raw term frequency `tf`, document length
    /// `doc_len`, corpus average document length `avgdl`, pre-computed `idf`,
    /// and query term frequency `qtf`.
    fn score(&self, tf: f32, doc_len: f32, avgdl: f32, idf: f32, qtf: f32) -> f32;

    /// Loose upper bound on `score(tf, *, *, idf, 1.0)` for WAND pruning.
    ///
    /// Must be `>=` the maximum possible `score` value for a single term in
    /// any document. A tighter bound means more aggressive pruning.
    fn upper_bound(&self, idf: f32) -> f32;
}

/// BM25 (Okapi BM25) relevance scorer with default parameters k1 = 1.2, b = 0.75.
///
/// This is the default scorer used when none is supplied in [`TextSearchOptions`].
#[derive(Debug, Clone)]
pub struct Bm25Scorer {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25Scorer {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Scorer for Bm25Scorer {
    fn idf(&self, n_docs: f32, n_term: f32) -> f32 {
        ((n_docs - n_term + 0.5) / (n_term + 0.5) + 1.0).ln()
    }

    fn score(&self, tf: f32, doc_len: f32, avgdl: f32, idf: f32, qtf: f32) -> f32 {
        let dl_ratio = if avgdl > 0.0 { doc_len / avgdl } else { 1.0 };
        let numerator = tf * (self.k1 + 1.0);
        let denominator = tf + self.k1 * (1.0 - self.b + self.b * dl_ratio);
        qtf * idf * (numerator / denominator)
    }

    fn upper_bound(&self, idf: f32) -> f32 {
        // As tf → ∞ the BM25 saturation factor converges to (k1 + 1).
        // This is the tightest possible upper bound for a single query-term contribution.
        idf * (self.k1 + 1.0)
    }
}

/// TF-IDF relevance scorer. Lighter than BM25; no length normalization.
#[derive(Debug, Clone, Default)]
pub struct TfIdfScorer;

impl Scorer for TfIdfScorer {
    fn idf(&self, n_docs: f32, n_term: f32) -> f32 {
        ((n_docs / (n_term + 1.0)).ln()).max(0.0)
    }

    fn score(&self, tf: f32, _doc_len: f32, _avgdl: f32, idf: f32, qtf: f32) -> f32 {
        qtf * tf.sqrt() * idf
    }

    fn upper_bound(&self, idf: f32) -> f32 {
        // Loose bound: assumes at most 100 term occurrences per document.
        idf * (100.0_f32).sqrt()
    }
}

// ------------------------------------------------------------------
// Boolean mode
// ------------------------------------------------------------------

/// Boolean candidate-set filtering for multi-term queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BooleanMode {
    /// All query terms must appear in the document (set intersection).
    ///
    /// This is the strictest mode: documents missing any query term are
    /// excluded before scoring. Recommended for precision-oriented queries.
    #[default]
    And,
    /// At least one query term must appear (set union).
    ///
    /// Equivalent to no pre-filtering; WAND scoring naturally ranks higher
    /// those documents that match more terms.
    Or,
}

// ------------------------------------------------------------------
// TextSearchOptions
// ------------------------------------------------------------------

/// Options for full-text search.
#[derive(Clone)]
pub struct TextSearchOptions {
    /// Restrict results to nodes with this label. When `None`, all indexed labels are searched.
    pub label: Option<String>,
    /// Restrict results to nodes indexed on this property. When `None`, all indexed properties are searched.
    pub property: Option<String>,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Relevance scorer. When `None`, the default [`Bm25Scorer`] is used.
    pub scorer: Option<Arc<dyn Scorer>>,
    /// Boolean candidate filtering. When `None`, `BooleanMode::And` is used for
    /// multi-term queries, consistent with most full-text search engines.
    pub boolean_mode: Option<BooleanMode>,
}

impl std::fmt::Debug for TextSearchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSearchOptions")
            .field("label", &self.label)
            .field("property", &self.property)
            .field("limit", &self.limit)
            .field("boolean_mode", &self.boolean_mode)
            .finish()
    }
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self {
            label: None,
            property: None,
            limit: 10,
            scorer: None,
            boolean_mode: None,
        }
    }
}

// ------------------------------------------------------------------
// WAND internals
// ------------------------------------------------------------------

/// A posting-list cursor for one query term, used by WAND top-k retrieval.
///
/// Wraps a `Vec<(NodeId, tf)>` that is already sorted in ascending NodeId order
/// (LMDB DUPSORT with big-endian u64 keys guarantees this). Supports O(log n)
/// skipping via `skip_to` for WAND-style pivot advancement.
struct PostingCursor {
    postings: Vec<(NodeId, u32)>,
    pos: usize,
    /// Pre-computed IDF for this term.
    pub idf: f32,
    /// Per-term WAND upper bound for a single document contribution.
    pub upper_bound: f32,
    /// Query term frequency (how many times the term appears in the query).
    pub qtf: f32,
}

impl PostingCursor {
    fn new(postings: Vec<(NodeId, u32)>, idf: f32, upper_bound: f32, qtf: f32) -> Self {
        Self {
            postings,
            pos: 0,
            idf,
            upper_bound,
            qtf,
        }
    }

    #[inline]
    fn current(&self) -> Option<NodeId> {
        self.postings.get(self.pos).map(|&(id, _)| id)
    }

    #[inline]
    fn current_tf(&self) -> u32 {
        self.postings.get(self.pos).map(|&(_, tf)| tf).unwrap_or(0)
    }

    #[inline]
    fn advance(&mut self) {
        self.pos += 1;
    }

    /// Advance the cursor to the first entry with `NodeId >= target`.
    fn skip_to(&mut self, target: NodeId) {
        let start = self.pos;
        let len = self.postings.len();
        let offset = self.postings[start..len].partition_point(|&(id, _)| id < target);
        self.pos = start + offset;
    }
}

/// `f32` wrapper that implements `Ord` for use in `BinaryHeap`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrderedF32(f32);

impl Eq for OrderedF32 {}

impl PartialOrd for OrderedF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Run Weak-AND (WAND) top-k retrieval over multiple sorted posting lists.
///
/// Returns up to `k` `(NodeId, score)` pairs, sorted by descending score.
///
/// WAND skips documents whose maximum possible score (sum of per-term upper
/// bounds for terms that still have the document in their future) cannot
/// exceed the current k-th best score threshold. This typically eliminates
/// 90–99 % of full BM25 evaluations compared to exhaustive scoring.
fn wand_top_k(
    mut cursors: Vec<PostingCursor>,
    k: usize,
    scorer: &dyn Scorer,
    doc_lengths: &HashMap<NodeId, u32>,
    avgdl: f32,
) -> Vec<(NodeId, f32)> {
    if k == 0 || cursors.is_empty() {
        return Vec::new();
    }

    // Min-heap tracking the k best (score, node_id) pairs seen so far.
    // Wrapped in Reverse so the heap root is the *minimum* of the top-k,
    // giving us the current pruning threshold efficiently.
    //
    // The heap is grown lazily rather than reserved from `k` up front: it is
    // bounded to `k` entries by the pop-on-overflow below, so its peak size is
    // `min(k, matching_docs)`, and amortized doubling makes the growth cost
    // negligible next to the heap operations. Sizing the reservation from the
    // caller-supplied `k` would let a large `k` drive an allocation before any
    // result exists to fill it.
    let mut heap: BinaryHeap<Reverse<(OrderedF32, NodeId)>> = BinaryHeap::new();
    let mut threshold = 0.0_f32;

    loop {
        // Drop exhausted cursors.
        cursors.retain(|c| c.current().is_some());
        if cursors.is_empty() {
            break;
        }

        // Sort by current doc ID (in-place; nearly sorted after each step).
        cursors.sort_unstable_by_key(|c| c.current().unwrap_or(u64::MAX));

        // Find the pivot: the first index i such that the prefix sum of upper
        // bounds for cursors[0..=i] exceeds the current threshold.
        let mut prefix_ub = 0.0_f32;
        let mut pivot_idx = cursors.len(); // sentinel
        for (i, cursor) in cursors.iter().enumerate() {
            prefix_ub += cursor.upper_bound;
            if prefix_ub > threshold {
                pivot_idx = i;
                break;
            }
        }

        // No remaining term combination can beat the threshold: done.
        if pivot_idx == cursors.len() {
            break;
        }

        let Some(pivot_doc) = cursors[pivot_idx].current() else {
            break;
        };

        // If the first cursor already points to pivot_doc, all cursors with
        // index < pivot_idx must also point to pivot_doc (sorted invariant).
        // Full evaluation: compute the exact BM25 score for this document.
        if cursors[0].current() == Some(pivot_doc) {
            let doc_len = doc_lengths.get(&pivot_doc).copied().unwrap_or(0) as f32;
            let mut total_score = 0.0_f32;

            for cursor in &cursors {
                match cursor.current() {
                    Some(id) if id == pivot_doc => {
                        let tf = cursor.current_tf() as f32;
                        total_score += scorer.score(tf, doc_len, avgdl, cursor.idf, cursor.qtf);
                    }
                    Some(id) if id > pivot_doc => break, // sorted: no more matches
                    _ => break,
                }
            }

            if total_score > threshold {
                heap.push(Reverse((OrderedF32(total_score), pivot_doc)));
                if heap.len() > k {
                    heap.pop();
                }
                if heap.len() == k {
                    threshold = heap.peek().map(|Reverse((s, _))| s.0).unwrap_or(0.0);
                }
            }

            // Advance all cursors that were pointing to pivot_doc.
            for cursor in &mut cursors {
                if cursor.current() == Some(pivot_doc) {
                    cursor.advance();
                }
            }
        } else {
            // Some cursors before pivot_idx lag behind pivot_doc.
            // Advance them via binary search so the next iteration can
            // attempt a full evaluation of pivot_doc.
            for cursor in cursors[..pivot_idx].iter_mut() {
                if cursor.current() < Some(pivot_doc) {
                    cursor.skip_to(pivot_doc);
                }
            }
        }
    }

    // Drain heap into a vector sorted by descending score.
    let mut results: Vec<(NodeId, f32)> =
        heap.into_iter().map(|Reverse((s, id))| (id, s.0)).collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

// ------------------------------------------------------------------
// Boolean pre-filtering
// ------------------------------------------------------------------

/// Build a candidate document set for a boolean query using [`RoaringTreemap`].
///
/// For `And` mode, returns the intersection of posting sets for all query terms.
/// For `Or` mode, returns the union. When no terms are present, returns an
/// empty bitmap.
fn boolean_candidate_set(
    graph: &Graph,
    label: &str,
    property: &str,
    query_terms: &HashMap<String, u32>,
    mode: BooleanMode,
) -> Result<RoaringTreemap, issundb_core::Error> {
    let mut result: Option<RoaringTreemap> = None;

    for term in query_terms.keys() {
        let postings = graph.fts_postings(label, property, term)?;
        let term_set: RoaringTreemap = postings.iter().map(|&(id, _)| id).collect();

        result = Some(match result {
            None => term_set,
            Some(existing) => match mode {
                BooleanMode::And => existing & term_set,
                BooleanMode::Or => existing | term_set,
            },
        });
    }

    Ok(result.unwrap_or_default())
}

// ------------------------------------------------------------------
// Public trait
// ------------------------------------------------------------------

/// Full-text search operations for `Graph`.
pub trait TextGraphExt {
    fn text_search(&self, query: &str, opts: &TextSearchOptions)
    -> Result<Vec<TextHit>, TextError>;
}

impl TextGraphExt for Graph {
    fn text_search(
        &self,
        query: &str,
        opts: &TextSearchOptions,
    ) -> Result<Vec<TextHit>, TextError> {
        let bm25_default = Bm25Scorer::default();
        let scorer: &dyn Scorer = opts
            .scorer
            .as_ref()
            .map(|s| s.as_ref() as &dyn Scorer)
            .unwrap_or(&bm25_default);

        // Discover which (label, property, lang) indices to search.
        let mut active_indices = Vec::new();
        if let (Some(label), Some(property)) = (&opts.label, &opts.property) {
            if self.has_node_text_index(label, property)? {
                let all_active = self.active_text_indexes()?;
                if let Some((_, _, lang)) = all_active
                    .iter()
                    .find(|(l, p, _)| l == label && p == property)
                {
                    active_indices.push((label.clone(), property.clone(), *lang));
                }
            } else {
                return Err(TextError::IndexNotFound {
                    label: label.clone(),
                    property: property.clone(),
                });
            }
        } else {
            let all_active = self.active_text_indexes()?;
            for (label, property, lang) in all_active {
                let matches_label = opts.label.as_ref().map(|l| l == &label).unwrap_or(true);
                let matches_prop = opts
                    .property
                    .as_ref()
                    .map(|p| p == &property)
                    .unwrap_or(true);
                if matches_label && matches_prop {
                    active_indices.push((label, property, lang));
                }
            }
        }

        let mut scores: HashMap<NodeId, f32> = HashMap::new();

        for (label, property, lang) in active_indices {
            let stats = match self.fts_stats(&label, &property)? {
                Some(s) => s,
                None => continue,
            };
            let (n_docs, sum_dl) = stats;
            if n_docs == 0 {
                continue;
            }
            let avgdl = (sum_dl as f32) / (n_docs as f32);

            let query_terms = self.tokenize_text(query, lang);
            if query_terms.is_empty() {
                continue;
            }

            // When boolean AND is explicitly requested, pre-filter to documents
            // that contain all query terms before building cursors. When
            // boolean_mode is None (the default) WAND scoring handles relevance
            // ranking without hard-filtering on term presence.
            let candidate_filter: Option<RoaringTreemap> =
                if opts.boolean_mode == Some(BooleanMode::And) && query_terms.len() > 1 {
                    Some(boolean_candidate_set(
                        self,
                        &label,
                        &property,
                        &query_terms,
                        BooleanMode::And, // already guarded by Some(BooleanMode::And) above
                    )?)
                } else {
                    None
                };

            // Build WAND cursors for each query term.
            let n_docs_f = n_docs as f32;
            let mut cursors: Vec<PostingCursor> = Vec::with_capacity(query_terms.len());

            for (term, &qtf) in &query_terms {
                let mut postings = self.fts_postings(&label, &property, term)?;
                if postings.is_empty() {
                    continue;
                }
                // Apply boolean pre-filter if requested.
                if let Some(ref filter) = candidate_filter {
                    postings.retain(|(id, _)| filter.contains(*id));
                    if postings.is_empty() {
                        continue;
                    }
                }
                let n_term = postings.len() as f32;
                let idf = scorer.idf(n_docs_f, n_term);
                let ub = scorer.upper_bound(idf) * qtf as f32;
                cursors.push(PostingCursor::new(postings, idf, ub, qtf as f32));
            }

            if cursors.is_empty() {
                continue;
            }

            // Collect the union of all candidate doc IDs and batch-load their lengths.
            // This is more efficient than calling fts_doc_len once per (doc, term) pair.
            let candidate_ids: Vec<NodeId> = {
                let mut ids: Vec<NodeId> = cursors
                    .iter()
                    .flat_map(|c| c.postings.iter().map(|&(id, _)| id))
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                ids
            };

            let mut doc_lengths: HashMap<NodeId, u32> = HashMap::with_capacity(candidate_ids.len());
            for id in candidate_ids {
                if let Some(len) = self.fts_doc_len(&label, &property, id)? {
                    doc_lengths.insert(id, len);
                }
            }

            // Run WAND top-k.
            let top_k = wand_top_k(cursors, opts.limit, scorer, &doc_lengths, avgdl);
            for (node_id, score) in top_k {
                *scores.entry(node_id).or_insert(0.0) += score;
            }
        }

        let mut hits: Vec<TextHit> = scores
            .into_iter()
            .map(|(node, score)| TextHit { node, score })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if hits.len() > opts.limit {
            hits.truncate(opts.limit);
        }

        Ok(hits)
    }
}

/// Text index lifecycle operations for `Graph`.
///
/// Implement text index creation, removal, and discovery through this trait
/// rather than calling the corresponding storage methods on `Graph` directly.
pub trait TextIndexExt {
    fn create_text_index(&self, label: &str, property: &str) -> Result<(), TextError>;
    fn create_text_index_with_language(
        &self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), TextError>;
    fn drop_text_index(&self, label: &str, property: &str) -> Result<(), TextError>;
    fn has_text_index(&self, label: &str, property: &str) -> Result<bool, TextError>;
    fn list_text_indexes(&self) -> Result<Vec<(String, String, Language)>, TextError>;
}

impl TextIndexExt for Graph {
    fn create_text_index(&self, label: &str, property: &str) -> Result<(), TextError> {
        self.create_node_text_index(label, property)
            .map_err(Into::into)
    }

    fn create_text_index_with_language(
        &self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), TextError> {
        self.create_node_text_index_with_language(label, property, lang)
            .map_err(Into::into)
    }

    fn drop_text_index(&self, label: &str, property: &str) -> Result<(), TextError> {
        self.drop_node_text_index(label, property)
            .map_err(Into::into)
    }

    fn has_text_index(&self, label: &str, property: &str) -> Result<bool, TextError> {
        self.has_node_text_index(label, property)
            .map_err(Into::into)
    }

    fn list_text_indexes(&self) -> Result<Vec<(String, String, Language)>, TextError> {
        self.active_text_indexes().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn test_fts_e2e_indexing_and_scoring() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let graph = Graph::open(temp.path(), 1)?;

        // 1. Insert documents before index is created
        let n1 = graph.add_node(
            "Movie",
            &json!({
                "title": "A Space Odyssey",
                "description": "A beautiful sci-fi odyssey in space with spaceships and stars"
            }),
        )?;
        let n2 = graph.add_node("Movie", &json!({
            "title": "Star Wars",
            "description": "A battle in deep space with stars and lasers and a big battle spaceship"
        }))?;
        let n3 = graph.add_node("Movie", &json!({
            "title": "Titanic",
            "description": "A tragic love story on a ship that hits an iceberg and sinks in the ocean"
        }))?;

        // 2. Search should return IndexNotFound before index is created
        let opts = TextSearchOptions {
            label: Some("Movie".to_string()),
            property: Some("description".to_string()),
            limit: 5,
            ..Default::default()
        };
        let err = graph.text_search("space", &opts).unwrap_err();
        assert!(
            matches!(err, TextError::IndexNotFound { .. }),
            "Expected IndexNotFound error when index is not created yet"
        );

        // 3. Create index and verify existence
        assert!(!graph.has_node_text_index("Movie", "description")?);
        graph.create_node_text_index("Movie", "description")?;
        assert!(graph.has_node_text_index("Movie", "description")?);

        // 4. Single-term search: BM25 length normalization makes shorter doc rank higher.
        // "space" appears in n1 (1 time) and n2 (1 time).
        // n1 is shorter (9 words) so it has a higher BM25 score than n2 (12 words).
        let hits_space = graph.text_search("space", &opts)?;
        assert_eq!(hits_space.len(), 2);
        assert_eq!(hits_space[0].node, n1);
        assert_eq!(hits_space[1].node, n2);
        assert!(
            hits_space[0].score > hits_space[1].score,
            "Shorter document should score higher"
        );

        // 5. Multi-term search: "spaceship battle" should score n2 highest because
        //    "battle" appears twice in n2, while n1 has neither "battle" nor more "spaceship".
        let hits_multi = graph.text_search("spaceship battle", &opts)?;
        assert_eq!(hits_multi[0].node, n2);

        // 6. Update mutation: update n3 to include space terms.
        graph.update_node(
            n3,
            &json!({
                "title": "Titanic",
                "description": "A spaceship crashed in deep space instead of an iceberg"
            }),
        )?;

        let hits_updated = graph.text_search("spaceship", &opts)?;
        let found_n3 = hits_updated.iter().any(|h| h.node == n3);
        assert!(found_n3, "Updated node should be found in new search");

        // 7. Delete mutation: n1 should disappear from results.
        graph.delete_node(n1)?;
        let hits_after_delete = graph.text_search("space", &opts)?;
        let found_n1 = hits_after_delete.iter().any(|h| h.node == n1);
        assert!(!found_n1, "Deleted node should not be found in search");

        // 8. Global search across all active indexes.
        graph.create_node_text_index("Movie", "title")?;
        let global_opts = TextSearchOptions {
            label: None,
            property: None,
            limit: 5,
            ..Default::default()
        };
        let global_hits = graph.text_search("odyssey titanic", &global_opts)?;
        assert!(!global_hits.is_empty());

        // 9. Drop index and verify cleanup.
        graph.drop_node_text_index("Movie", "description")?;
        assert!(!graph.has_node_text_index("Movie", "description")?);
        let err_after_drop = graph.text_search("spaceship", &opts).unwrap_err();
        assert!(
            matches!(err_after_drop, TextError::IndexNotFound { .. }),
            "Should return IndexNotFound error after index drop"
        );

        Ok(())
    }

    #[test]
    fn test_bm25_scorer_upper_bound_is_tight() {
        let scorer = Bm25Scorer::default();
        let idf = scorer.idf(100.0, 10.0);
        let ub = scorer.upper_bound(idf);
        // Score with very high tf and very low doc_len should approach the upper bound.
        let high_tf_score = scorer.score(1000.0, 1.0, 100.0, idf, 1.0);
        assert!(
            high_tf_score <= ub + 1e-4,
            "upper_bound must be >= any achievable score: ub={ub}, score={high_tf_score}"
        );
    }

    #[test]
    fn test_boolean_and_mode_excludes_partial_matches() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let graph = Graph::open(temp.path(), 1)?;

        let n1 = graph.add_node("Doc", &json!({ "body": "the quick brown fox jumps" }))?;
        let n2 = graph.add_node("Doc", &json!({ "body": "the quick brown lazy dog" }))?;
        let n3 = graph.add_node("Doc", &json!({ "body": "only fox here" }))?;

        graph.create_node_text_index("Doc", "body")?;

        // AND mode: "quick fox" must appear in both terms.
        // n1 has both "quick" and "fox"; n2 has "quick" but not "fox"; n3 has "fox" but not "quick".
        let and_opts = TextSearchOptions {
            label: Some("Doc".to_string()),
            property: Some("body".to_string()),
            limit: 10,
            boolean_mode: Some(BooleanMode::And),
            ..Default::default()
        };
        let hits = graph.text_search("quick fox", &and_opts)?;
        // Only n1 matches both terms (after stemming, "quick" and "fox" should both appear).
        assert!(
            hits.iter().all(|h| h.node == n1),
            "AND mode should return only n1; got {:?}",
            hits.iter().map(|h| h.node).collect::<Vec<_>>()
        );

        // OR mode: any term match is sufficient.
        let or_opts = TextSearchOptions {
            label: Some("Doc".to_string()),
            property: Some("body".to_string()),
            limit: 10,
            boolean_mode: Some(BooleanMode::Or),
            ..Default::default()
        };
        let or_hits = graph.text_search("quick fox", &or_opts)?;
        let hit_nodes: Vec<NodeId> = or_hits.iter().map(|h| h.node).collect();
        assert!(
            hit_nodes.contains(&n1) && hit_nodes.contains(&n2) && hit_nodes.contains(&n3),
            "OR mode should return n1, n2, and n3"
        );

        Ok(())
    }

    #[test]
    fn test_ascii_folded_query_matches_indexed_diacritics() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = TempDir::new()?;
        let graph = Graph::open(temp.path(), 1)?;

        // Index a document with diacritics.
        let n = graph.add_node("Doc", &json!({ "text": "café résumé über" }))?;
        graph.create_node_text_index("Doc", "text")?;

        // Query without diacritics should still find the document because
        // fold_ascii normalizes both at index time and at query time.
        let opts = TextSearchOptions {
            label: Some("Doc".to_string()),
            property: Some("text".to_string()),
            limit: 5,
            ..Default::default()
        };
        let hits = graph.text_search("cafe resume uber", &opts)?;
        assert!(
            hits.iter().any(|h| h.node == n),
            "ASCII-folded query should match document indexed with diacritics"
        );

        Ok(())
    }

    #[test]
    fn test_tfidf_scorer_via_options() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let graph = Graph::open(temp.path(), 1)?;

        // Use 3 documents so that IDF for terms appearing in only 1 doc is
        // ln(3 / 2) ≈ 0.405 > 0. TfIdfScorer produces zero scores when IDF = 0
        // (term in half the corpus), so we need at least 3 docs where the
        // discriminating terms appear in only 1.
        let n1 = graph.add_node(
            "Doc",
            &json!({ "body": "rust programming language systems" }),
        )?;
        let _n2 = graph.add_node("Doc", &json!({ "body": "python programming scripting" }))?;
        let _n3 = graph.add_node("Doc", &json!({ "body": "java enterprise programming" }))?;
        graph.create_node_text_index("Doc", "body")?;

        let opts = TextSearchOptions {
            label: Some("Doc".to_string()),
            property: Some("body".to_string()),
            limit: 5,
            scorer: Some(Arc::new(TfIdfScorer)),
            ..Default::default()
        };
        let hits = graph.text_search("rust systems", &opts)?;
        assert!(
            !hits.is_empty(),
            "TfIdfScorer should return results for matching query"
        );
        assert_eq!(
            hits[0].node, n1,
            "n1 contains both terms and should rank first"
        );

        Ok(())
    }

    #[test]
    fn test_text_index_ext_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let graph = Graph::open(temp.path(), 1)?;

        // Ensure initially we have no text indexes
        assert!(!graph.has_text_index("Doc", "body")?);
        assert!(graph.list_text_indexes()?.is_empty());

        // Create an index
        graph.create_text_index("Doc", "body")?;
        assert!(graph.has_text_index("Doc", "body")?);

        // List indexes and check
        let list = graph.list_text_indexes()?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "Doc");
        assert_eq!(list[0].1, "body");
        assert_eq!(list[0].2, Language::English); // Default language

        // Create another index with a specific language
        graph.create_text_index_with_language("Doc", "title", Language::German)?;
        assert!(graph.has_text_index("Doc", "title")?);

        let list = graph.list_text_indexes()?;
        assert_eq!(list.len(), 2);
        let title_idx = list.iter().find(|(_, p, _)| p == "title").unwrap();
        assert_eq!(title_idx.2, Language::German);

        // Drop the indexes
        graph.drop_text_index("Doc", "body")?;
        assert!(!graph.has_text_index("Doc", "body")?);

        graph.drop_text_index("Doc", "title")?;
        assert!(!graph.has_text_index("Doc", "title")?);

        assert!(graph.list_text_indexes()?.is_empty());

        Ok(())
    }
}
