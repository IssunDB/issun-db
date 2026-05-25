use issundb_core::{Graph, NodeId};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TextError {
    #[error("core storage error: {0}")]
    Core(#[from] issundb_core::Error),
}

/// A single ranked full-text search result.
#[derive(Debug, Clone)]
pub struct TextHit {
    pub node: NodeId,
    pub score: f32,
}

/// Options for text search.
#[derive(Clone, Debug)]
pub struct TextSearchOptions {
    pub label: Option<String>,
    pub property: Option<String>,
    pub limit: usize,
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self {
            label: None,
            property: None,
            limit: 10,
        }
    }
}

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

            let query_terms = issundb_core::storage::fts::tokenize(query, lang);
            if query_terms.is_empty() {
                continue;
            }

            for (term, &qtf) in &query_terms {
                let postings = self.fts_postings(&label, &property, term)?;
                if postings.is_empty() {
                    continue;
                }

                let n_term = postings.len() as f32;
                let n_docs_f = n_docs as f32;
                let idf = ((n_docs_f - n_term + 0.5) / (n_term + 0.5) + 1.0).ln();

                for (node_id, tf) in postings {
                    let doc_len = self.fts_doc_len(&label, &property, node_id)?.unwrap_or(0) as f32;

                    let dl_ratio = if avgdl > 0.0 { doc_len / avgdl } else { 1.0 };
                    let tf = tf as f32;
                    let k1 = 1.2_f32;
                    let b = 0.75_f32;

                    let numerator = tf * (k1 + 1.0);
                    let denominator = tf + k1 * (1.0 - b + b * dl_ratio);
                    let term_score = (qtf as f32) * idf * (numerator / denominator);

                    *scores.entry(node_id).or_insert(0.0) += term_score;
                }
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

        // 2. Search should return nothing or error out before index is created
        let opts = TextSearchOptions {
            label: Some("Movie".to_string()),
            property: Some("description".to_string()),
            limit: 5,
        };
        let hits = graph.text_search("space", &opts)?;
        assert!(
            hits.is_empty(),
            "Should return no hits when index is not created yet"
        );

        // 3. Create index and verify existence
        assert!(!graph.has_node_text_index("Movie", "description")?);
        graph.create_node_text_index("Movie", "description")?;
        assert!(graph.has_node_text_index("Movie", "description")?);

        // 4. Test searching with single terms (IDF and TF comparison)
        // "space" appears in n1 (1 time) and n2 (1 time).
        // Since n1 has description length 10 words, and n2 has description length 13 words:
        // n1 (shorter document) should have a slightly higher BM25 score than n2 due to length normalization!
        let hits_space = graph.text_search("space", &opts)?;
        assert_eq!(hits_space.len(), 2);
        assert_eq!(hits_space[0].node, n1);
        assert_eq!(hits_space[1].node, n2);
        assert!(
            hits_space[0].score > hits_space[1].score,
            "Shorter document should score higher"
        );

        // 5. Test multi-term search
        // "spaceship battle" should score highest for n2 because "battle" appears twice, "spaceship" once,
        // whereas in n1 only "spaceship" (or similar) might match.
        let hits_multi = graph.text_search("spaceship battle", &opts)?;
        assert_eq!(hits_multi[0].node, n2);

        // 6. Test update mutation behavior
        // Update n3 description to contain space terms
        graph.update_node(
            n3,
            "Movie",
            &json!({
                "title": "Titanic",
                "description": "A spaceship crashed in deep space instead of an iceberg"
            }),
        )?;

        let hits_updated = graph.text_search("spaceship", &opts)?;
        // Both n2 and n3 now have "spaceship"
        let found_n3 = hits_updated.iter().any(|h| h.node == n3);
        assert!(found_n3, "Updated node should be found in new search");

        // 7. Test delete mutation behavior
        graph.delete_node(n1)?;
        let hits_after_delete = graph.text_search("space", &opts)?;
        let found_n1 = hits_after_delete.iter().any(|h| h.node == n1);
        assert!(!found_n1, "Deleted node should not be found in search");

        // 8. Test search with no label/property constraints (global search across all active indexes)
        // Let's create another index on "title"
        graph.create_node_text_index("Movie", "title")?;
        let global_opts = TextSearchOptions {
            label: None,
            property: None,
            limit: 5,
        };
        let global_hits = graph.text_search("odyssey titanic", &global_opts)?;
        // should match title or description for titanic and odyssey
        assert!(!global_hits.is_empty());

        // 9. Drop index and verify cleanup
        graph.drop_node_text_index("Movie", "description")?;
        assert!(!graph.has_node_text_index("Movie", "description")?);
        let hits_after_drop = graph.text_search("spaceship", &opts)?;
        assert!(
            hits_after_drop.is_empty(),
            "Should not return hits after index drop"
        );

        Ok(())
    }
}
