//! BM25 tool search.
//!
//! The discovery primitive for deferred tools, following pi's `searchTools`
//! semantics: rank callable tools by relevance to a query, returning
//! `{ name, description }` hits. BM25 (Okapi) runs over tokenized tool paths,
//! descriptions, and input property names.

use std::collections::{BTreeMap, HashMap};

/// Tokenize text into lowercase search terms.
///
/// Splits on non-alphanumeric characters and camelCase boundaries, so
/// `search_tools` and `searchTools` both produce `["search", "tools"]`. Empty
/// terms and lone `*` wildcards are dropped.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let push_current = |current: &mut String, terms: &mut Vec<String>| {
        if !current.is_empty() {
            terms.push(std::mem::take(current));
        }
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // camelCase boundary: lowercase/digit followed by uppercase.
            if ch.is_uppercase()
                && current
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_lowercase() || c.is_ascii_digit())
            {
                push_current(&mut current, &mut terms);
            }
            current.extend(ch.to_lowercase());
        } else {
            push_current(&mut current, &mut terms);
        }
    }
    push_current(&mut current, &mut terms);
    terms.into_iter().filter(|t| t != "*").collect()
}

/// A document in the search index: a tool's searchable text, split by field.
#[derive(Clone, Debug)]
struct Document {
    /// Token counts per field. Field order: path (weight 2), description (1),
    /// extra text such as input property names (1).
    fields: [HashMap<String, u32>; 3],
    /// Total number of terms across all fields.
    length: u32,
}

impl Document {
    fn term_freq(&self, term: &str) -> u32 {
        self.fields
            .iter()
            .map(|f| f.get(term).copied().unwrap_or(0))
            .sum()
    }
}

/// Okapi BM25 parameters.
const K1: f32 = 1.2;
const B: f32 = 0.75;
/// Score bonus weight for the path field, so a tool whose *name* matches ranks
/// above one whose description merely mentions the query.
const PATH_WEIGHT: f32 = 2.0;

/// A search result: the tool's identifier and description.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct SearchHit {
    /// The tool's full path in the registry, e.g. `github.create_pull_request`.
    pub name: String,
    /// The tool's description, as shown in discovery results.
    pub description: String,
    /// The BM25 relevance score; higher is better.
    pub score: f32,
}

/// An BM25 index over tool descriptions.
///
/// Build once per registry snapshot with [`SearchIndex::build`]; queries are
/// read-only and allocation-light.
#[derive(Clone, Debug, Default)]
pub struct SearchIndex {
    documents: Vec<Document>,
    hits: Vec<SearchHit>,
    /// Document frequency per term.
    df: BTreeMap<String, u32>,
    /// Average document length across the index.
    avg_len: f32,
}

impl SearchIndex {
    /// Build an index from `(name, description, extra searchable text)` triples.
    ///
    /// `name` is the tool's registry path (e.g. `github.list_issues`); it is
    /// tokenized as-is, and the `.` separators act as term boundaries.
    pub fn build<I>(tools: I) -> Self
    where
        I: IntoIterator<Item = (String, String, Vec<String>)>,
    {
        let mut documents = Vec::new();
        let mut hits = Vec::new();
        let mut df: BTreeMap<String, u32> = BTreeMap::new();
        let mut total_len = 0u32;

        for (name, description, extra) in tools {
            let path_terms = tokenize(&name);
            let desc_terms = tokenize(&description);
            let mut extra_terms = Vec::new();
            for text in extra {
                extra_terms.extend(tokenize(&text));
            }

            let mut fields: [HashMap<String, u32>; 3] =
                [HashMap::new(), HashMap::new(), HashMap::new()];
            let term_groups: [&[String]; 3] = [&path_terms, &desc_terms, &extra_terms];
            for (index, terms) in term_groups.iter().enumerate() {
                let field = &mut fields[index];
                for term in terms.iter() {
                    *field.entry(term.clone()).or_insert(0) += 1;
                }
            }

            let length = (path_terms.len() + desc_terms.len() + extra_terms.len()) as u32;
            let mut seen = std::collections::HashSet::new();
            for term in path_terms
                .iter()
                .chain(desc_terms.iter())
                .chain(extra_terms.iter())
            {
                if seen.insert(term.clone()) {
                    *df.entry(term.clone()).or_insert(0) += 1;
                }
            }

            total_len += length;
            documents.push(Document { fields, length });
            hits.push(SearchHit {
                name,
                description,
                score: 0.0,
            });
        }

        let avg_len = if documents.is_empty() {
            0.0
        } else {
            total_len as f32 / documents.len() as f32
        };
        Self {
            documents,
            hits,
            df,
            avg_len,
        }
    }

    /// The number of tools in the index.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Rank all tools against `query`, returning the best hits.
    ///
    /// With an empty query, all tools are returned in path order with zero
    /// scores. Otherwise only tools with a positive BM25 score are returned,
    /// ordered by descending score. `limit` caps the result size; results
    /// beyond the limit are not computed.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let terms = tokenize(query);
        if self.documents.is_empty() {
            return Vec::new();
        }

        if terms.is_empty() {
            let mut order: Vec<usize> = (0..self.hits.len()).collect();
            order.sort_by(|&a, &b| self.hits[a].name.cmp(&self.hits[b].name));
            return order
                .into_iter()
                .take(limit)
                .map(|i| self.hits[i].clone())
                .collect();
        }

        let n = self.documents.len() as f32;
        let mut scored: Vec<(usize, f32)> = self
            .documents
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let mut score = 0.0;
                for term in &terms {
                    let Some(&doc_freq) = self.df.get(term) else {
                        continue;
                    };
                    // IDF, BM25+ style floor to stay positive on common terms.
                    let idf = ((n - doc_freq as f32 + 0.5) / (doc_freq as f32 + 0.5) + 1.0).ln();
                    let tf = doc.term_freq(term) as f32;
                    if tf == 0.0 {
                        continue;
                    }
                    let norm = 1.0 - B + B * (doc.length as f32 / self.avg_len.max(1.0));
                    let tf_component = (tf * (K1 + 1.0)) / (tf + K1 * norm);
                    // Weight the path field double: a name match outranks a
                    // description match for the same term.
                    let path_tf = doc.fields[0].get(term).copied().unwrap_or(0) as f32;
                    let path_component = if path_tf > 0.0 {
                        (path_tf * (K1 + 1.0)) / (path_tf + K1 * norm) * (PATH_WEIGHT - 1.0)
                    } else {
                        0.0
                    };
                    score += idf * (tf_component + path_component);
                }
                (i, score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();

        scored.sort_by(|&(ai, ascore), &(bi, bscore)| {
            bscore
                .partial_cmp(&ascore)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.hits[ai].name.cmp(&self.hits[bi].name))
        });
        scored.truncate(limit);
        scored
            .into_iter()
            .map(|(i, score)| SearchHit {
                score,
                ..self.hits[i].clone()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> SearchIndex {
        SearchIndex::build([
            (
                "github.list_issues".into(),
                "List issues in a repository".into(),
                vec![],
            ),
            (
                "github.create_pull_request".into(),
                "Create a pull request".into(),
                vec![],
            ),
            (
                "github.search_code".into(),
                "Search code in repositories".into(),
                vec![],
            ),
            (
                "ghidra.decompile".into(),
                "Decompile a function to pseudo-C".into(),
                vec![],
            ),
            (
                "ghidra.search_tools".into(),
                "Search available ghidra tools".into(),
                vec![],
            ),
            (
                "fs.read_file".into(),
                "Read a file from disk".into(),
                vec![],
            ),
        ])
    }

    #[test]
    fn tokenize_splits_camel_case_and_separators() {
        assert_eq!(tokenize("searchTools"), vec!["search", "tools"]);
        assert_eq!(tokenize("search_tools"), vec!["search", "tools"]);
        assert_eq!(
            tokenize("github.list_issues"),
            vec!["github", "list", "issues"]
        );
        assert_eq!(tokenize("aBc12d"), vec!["a", "bc12d"]);
    }

    #[test]
    fn exact_name_match_ranks_first() {
        let index = index();
        let hits = index.search("search_tools", 5);
        assert_eq!(hits[0].name, "ghidra.search_tools");
    }

    #[test]
    fn description_matches_rank() {
        let index = index();
        let hits = index.search("pull request", 5);
        assert_eq!(hits[0].name, "github.create_pull_request");
    }

    #[test]
    fn empty_query_returns_all_in_path_order() {
        let index = index();
        let hits = index.search("", 10);
        assert_eq!(hits.len(), 6);
        assert_eq!(hits[0].name, "fs.read_file");
    }

    #[test]
    fn limit_is_respected() {
        let index = index();
        assert_eq!(index.search("", 2).len(), 2);
        assert_eq!(index.search("search", 1).len(), 1);
    }

    #[test]
    fn no_match_is_empty() {
        let index = index();
        assert!(index.search("zzz_nonexistent", 5).is_empty());
    }

    #[test]
    fn extra_text_is_searchable() {
        let index = SearchIndex::build([(
            "tool.x".into(),
            "Does a thing".into(),
            vec!["width".into(), "height".into()],
        )]);
        let hits = index.search("height", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "tool.x");
    }
}
