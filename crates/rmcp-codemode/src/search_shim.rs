//! A minimal in-core search index over tool paths and descriptions.
//!
//! BM25 (Okapi, k1 = 1.2, b = 0.75) with a path-field weight boost, matching
//! pi's ranking intent (names rank above descriptions) and OpenCode's
//! tokenizer (camelCase and separator boundaries split into lowercase word
//! tokens). In-tree: no dependencies.

use std::collections::HashMap;

/// Okapi BM25 parameters.
const K1: f32 = 1.2;
const B: f32 = 0.75;
/// Score boost for terms matched in the path field: a tool whose *name*
/// matches ranks above one whose description merely mentions the query.
const PATH_WEIGHT: f32 = 2.0;

/// Split text into lowercase word tokens: separators and camelCase
/// boundaries both split (OpenCode's tokenizer), `*` is dropped as a
/// wildcard marker (pi's rule).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let push = |current: &mut String, terms: &mut Vec<String>| {
        if !current.is_empty() {
            terms.push(std::mem::take(current));
        }
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase()
                && current
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_lowercase() || c.is_ascii_digit())
            {
                push(&mut current, &mut terms);
            }
            current.extend(ch.to_lowercase());
        } else {
            push(&mut current, &mut terms);
        }
    }
    push(&mut current, &mut terms);
    terms.into_iter().filter(|t| t != "*").collect()
}

/// One search hit: the tool's canonical path, description, and score.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// The tool's canonical path.
    pub path: String,
    /// The tool's description as indexed.
    pub description: String,
    /// BM25 relevance score; higher is better.
    pub score: f32,
}

struct Document {
    /// Term frequencies per field: 0 = path, 1 = description, 2 = extra text.
    fields: [HashMap<String, u32>; 3],
    length: u32,
}

/// A BM25 index over tool paths and descriptions.
#[derive(Default)]
pub struct SearchIndex {
    documents: Vec<Document>,
    hits: Vec<SearchHit>,
    /// Document frequency per term.
    df: HashMap<String, u32>,
    avg_len: f32,
    total_len: u32,
}

impl SearchIndex {
    /// Add one document: the canonical path, its description, and extra
    /// searchable text (namespace descriptions and instructions).
    pub fn add(&mut self, path: &str, description: Option<&str>, extra: &[String]) {
        let path_terms = tokenize(path);
        let description = description.unwrap_or_default();
        let desc_terms = tokenize(description);
        let mut extra_terms = Vec::new();
        for text in extra {
            extra_terms.extend(tokenize(text));
        }
        let mut fields: [HashMap<String, u32>; 3] = Default::default();
        for (field, terms) in fields
            .iter_mut()
            .zip([&path_terms, &desc_terms, &extra_terms])
        {
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
                *self.df.entry(term.clone()).or_insert(0) += 1;
            }
        }
        self.total_len += length;
        self.documents.push(Document { fields, length });
        self.avg_len = self.total_len as f32 / self.documents.len() as f32;
        self.hits.push(SearchHit {
            path: path.to_owned(),
            description: description.to_owned(),
            score: 0.0,
        });
    }
}

impl SearchIndex {
    /// The number of indexed tools.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Rank the index against `query`. An empty query returns everything in
    /// path order (so scripts can browse a namespace); otherwise only
    /// positive-scoring tools, by descending score.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if self.documents.is_empty() {
            return Vec::new();
        }
        let terms = tokenize(query);
        if terms.is_empty() {
            let mut order: Vec<usize> = (0..self.hits.len()).collect();
            order.sort_by(|&a, &b| self.hits[a].path.cmp(&self.hits[b].path));
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
                for (field_index, weight) in [PATH_WEIGHT, 1.0, 1.0].iter().enumerate() {
                    for term in &terms {
                        let Some(tf) = doc.fields[field_index].get(term) else {
                            continue;
                        };
                        let Some(df) = self.df.get(term) else {
                            continue;
                        };
                        let idf = ((n - *df as f32 + 0.5) / (*df as f32 + 0.5)).max(1e-6).ln();
                        let field_len = doc.length as f32;
                        score += weight * idf * (*tf as f32 * (K1 + 1.0))
                            / (*tf as f32 + K1 * (1.0 - B + B * field_len / self.avg_len));
                    }
                }
                (i, score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.hits[a.0].path.cmp(&self.hits[b.0].path))
        });
        scored
            .into_iter()
            .take(limit)
            .map(|(i, score)| SearchHit {
                score,
                path: self.hits[i].path.clone(),
                description: self.hits[i].description.clone(),
            })
            .collect()
    }
}
