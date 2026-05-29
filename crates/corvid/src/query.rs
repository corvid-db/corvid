//! Retrieval primitives over a collection: exact vector KNN and BM25 text
//! search, plus the shared ranking helpers the fluent builder composes.
//!
//! Both searches are exact (brute-force over a scan) in v0.1 — correct and
//! simple, the baseline an approximate index is later measured against. The
//! ranking helpers ([`ranked_vector`], [`ranked_bm25`]) operate on an
//! already-gathered candidate set so the builder can pre-filter once and then
//! rank, which keeps `filter` a true predicate rather than a post-filter.

use std::collections::{BinaryHeap, HashMap};

use crate::db::Collection;
use crate::distance::Metric;
use crate::error::Result;
use crate::text::{Bm25Params, idf, term_score, tokenize};
use crate::value::Value;

/// One result of a vector search: the document, its key, and its distance to
/// the query under the chosen metric (lower is nearer).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The document's key within the collection.
    pub key: Vec<u8>,
    /// Distance to the query vector. Lower is nearer.
    pub distance: f32,
    /// The full stored document.
    pub document: Value,
}

/// One result of a text search: the document, its key, and its BM25 score
/// against the query (higher is more relevant).
#[derive(Debug, Clone, PartialEq)]
pub struct TextHit {
    /// The document's key within the collection.
    pub key: Vec<u8>,
    /// BM25 relevance score. Higher is more relevant.
    pub score: f32,
    /// The full stored document.
    pub document: Value,
}

impl Collection<'_> {
    /// Return up to `k` documents ranked by BM25 relevance of their `field`
    /// text to `query`, most relevant first.
    ///
    /// Documents lacking the field or whose field is not [`Value::Text`] are
    /// not part of the corpus. Documents that contain none of the query terms
    /// are omitted. Ties break by key order.
    pub fn text_search(&self, field: &str, query: &str, k: usize) -> Result<Vec<TextHit>> {
        // Use an inverted index when one is registered; else exact over a scan.
        if let Some(ranked) = self.db().fts_search(self.name(), field, query, k)? {
            let mut out = Vec::with_capacity(ranked.len());
            for (key, score) in ranked {
                if let Some(document) = self.get(&key)? {
                    out.push(TextHit {
                        key,
                        score,
                        document,
                    });
                }
            }
            return Ok(out);
        }

        let cands = self.scan()?;
        let mut ranked = ranked_bm25(&cands, field, query);
        ranked.truncate(k);
        let mut docs = doc_map(cands);
        Ok(ranked
            .into_iter()
            .map(|(key, score)| {
                let document = docs.remove(&key).expect("ranked key came from cands");
                TextHit {
                    key,
                    score,
                    document,
                }
            })
            .collect())
    }

    /// Return the `k` documents whose embedding in field `field` is nearest to
    /// `query` under `metric`, nearest first.
    ///
    /// If a matching ANN index was created with
    /// [`Collection::create_vector_index`](crate::Collection::create_vector_index)
    /// it is used (approximate, faster); otherwise the search is exact
    /// (brute-force). Documents that lack the field, whose field is not a
    /// [`Value::Vector`], or whose dimension differs from `query` are skipped
    /// (schema-on-read). Ties break by key order.
    pub fn vector_search(
        &self,
        field: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Vec<Hit>> {
        // Use a registered ANN index when one matches; else fall back to exact.
        if let Some(ranked) = self.db().ann_search(self.name(), field, query, k, metric)? {
            let mut out = Vec::with_capacity(ranked.len());
            for (key, distance) in ranked {
                if let Some(document) = self.get(&key)? {
                    out.push(Hit {
                        key,
                        distance,
                        document,
                    });
                }
            }
            return Ok(out);
        }

        // Exact search streams the collection through a bounded top-k heap:
        // O(n) time but O(k) memory, so an unindexed search never materializes
        // the whole collection.
        let mut heap: BinaryHeap<NearCand> = BinaryHeap::new();
        self.for_each_doc(|key, doc| {
            if let Some(v) = doc.get(field).and_then(Value::as_vector)
                && v.len() == query.len()
            {
                let cand = NearCand {
                    dist: metric.distance(query, v),
                    key: key.to_vec(),
                    doc,
                };
                if heap.len() < k {
                    heap.push(cand);
                } else if heap.peek().is_some_and(|worst| cand < *worst) {
                    heap.pop();
                    heap.push(cand);
                }
            }
            Ok(true)
        })?;
        let mut out: Vec<NearCand> = heap.into_vec();
        out.sort_by(|a, b| a.dist.total_cmp(&b.dist).then_with(|| a.key.cmp(&b.key)));
        Ok(out
            .into_iter()
            .map(|c| Hit {
                key: c.key,
                distance: c.dist,
                document: c.doc,
            })
            .collect())
    }
}

/// A nearest-neighbour candidate for the bounded top-k heap. Ordered by
/// `(distance, key)` so the max-heap's top is the current worst kept result.
/// The document is carried along but excluded from ordering.
struct NearCand {
    dist: f32,
    key: Vec<u8>,
    doc: Value,
}

impl PartialEq for NearCand {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.key == other.key
    }
}
impl Eq for NearCand {}
impl Ord for NearCand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.dist
            .total_cmp(&other.dist)
            .then_with(|| self.key.cmp(&other.key))
    }
}
impl PartialOrd for NearCand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Rank candidates by vector distance ascending (nearest first), keeping only
/// those whose `field` holds a [`Value::Vector`] of matching dimension. Ties
/// break by key.
pub(crate) fn ranked_vector(
    cands: &[(Vec<u8>, Value)],
    field: &str,
    query: &[f32],
    metric: Metric,
) -> Vec<(Vec<u8>, f32)> {
    let mut ranked: Vec<(Vec<u8>, f32)> = cands
        .iter()
        .filter_map(|(key, doc)| {
            let v = doc.get(field).and_then(Value::as_vector)?;
            (v.len() == query.len()).then(|| (key.clone(), metric.distance(query, v)))
        })
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Rank candidates by BM25 score descending over `field`, keeping only those
/// with a positive score (i.e. containing at least one query term). Ties break
/// by key.
pub(crate) fn ranked_bm25(
    cands: &[(Vec<u8>, Value)],
    field: &str,
    query: &str,
) -> Vec<(Vec<u8>, f32)> {
    let params = Bm25Params::default();

    let mut query_terms = tokenize(query);
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Vec::new();
    }

    struct DocStats<'a> {
        key: &'a [u8],
        term_freq: HashMap<String, u32>,
        len: usize,
    }

    let mut docs: Vec<DocStats> = Vec::new();
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    let mut total_len = 0usize;
    for (key, doc) in cands {
        let Some(text) = doc.get(field).and_then(Value::as_text) else {
            continue;
        };
        let mut term_freq: HashMap<String, u32> = HashMap::new();
        let mut len = 0usize;
        for token in tokenize(text) {
            *term_freq.entry(token).or_insert(0) += 1;
            len += 1;
        }
        for term in term_freq.keys() {
            *doc_freq.entry(term.clone()).or_insert(0) += 1;
        }
        total_len += len;
        docs.push(DocStats {
            key,
            term_freq,
            len,
        });
    }

    let n = docs.len();
    if n == 0 {
        return Vec::new();
    }
    // Guard the all-empty-text corpus: avg_len 0 would divide by zero; query
    // term frequencies are 0 there anyway so scores stay 0.
    let avg_len = match total_len as f32 / n as f32 {
        0.0 => 1.0,
        v => v,
    };

    let mut ranked: Vec<(Vec<u8>, f32)> = Vec::new();
    for doc in &docs {
        let mut score = 0.0;
        for term in &query_terms {
            let tf = doc.term_freq.get(term).copied().unwrap_or(0);
            if tf == 0 {
                continue;
            }
            let df = doc_freq.get(term).copied().unwrap_or(0);
            score += term_score(tf, doc.len, avg_len, idf(n, df), params);
        }
        if score > 0.0 {
            ranked.push((doc.key.to_vec(), score));
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Build a key → document lookup from a scanned candidate set.
pub(crate) fn doc_map(cands: Vec<(Vec<u8>, Value)>) -> HashMap<Vec<u8>, Value> {
    cands.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use std::collections::BTreeMap;

    fn doc_with_vec(label: &str, v: Vec<f32>) -> Value {
        let mut m = BTreeMap::new();
        m.insert("label".to_owned(), Value::Text(label.to_owned()));
        m.insert("embedding".to_owned(), Value::Vector(v));
        Value::Map(m)
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc_with_vec("a", vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc_with_vec("b", vec![0.0, 1.0])).unwrap();
        c.insert(b"c", &doc_with_vec("c", vec![-1.0, 0.0])).unwrap();
        db
    }

    #[test]
    fn returns_k_nearest_in_order() {
        let db = seed();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 2, Metric::L2)
            .unwrap();
        let keys: Vec<_> = hits.iter().map(|h| h.key.clone()).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(hits[0].distance, 0.0);
    }

    #[test]
    fn k_larger_than_corpus_returns_all() {
        let db = seed();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 100, Metric::Cosine)
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn k_zero_returns_empty() {
        let db = seed();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 0, Metric::L2)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn documents_without_the_field_are_skipped() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"has", &doc_with_vec("has", vec![1.0, 0.0]))
            .unwrap();
        c.insert(b"none", &Value::Text("no vector here".into()))
            .unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 10, Metric::L2)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, b"has".to_vec());
    }

    #[test]
    fn wrong_dimension_vectors_are_skipped() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"ok", &doc_with_vec("ok", vec![1.0, 0.0]))
            .unwrap();
        c.insert(b"bad", &doc_with_vec("bad", vec![1.0, 0.0, 0.0]))
            .unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 10, Metric::L2)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, b"ok".to_vec());
    }

    #[test]
    fn field_that_is_not_a_vector_is_skipped() {
        let db = Db::open_in_memory().unwrap();
        let mut m = BTreeMap::new();
        m.insert("embedding".to_owned(), Value::Text("not a vector".into()));
        db.collection("docs").insert(b"x", &Value::Map(m)).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 10, Metric::L2)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn ties_break_by_key_order() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"z", &doc_with_vec("z", vec![1.0, 0.0])).unwrap();
        c.insert(b"a", &doc_with_vec("a", vec![1.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 2, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
        assert_eq!(hits[1].key, b"z".to_vec());
    }

    #[test]
    fn empty_collection_returns_empty() {
        let db = Db::open_in_memory().unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 5, Metric::L2)
            .unwrap();
        assert!(hits.is_empty());
    }

    fn doc_with_text(body: &str) -> Value {
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), Value::Text(body.to_owned()));
        Value::Map(m)
    }

    fn seed_text() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc_with_text("the quick brown fox"))
            .unwrap();
        c.insert(b"b", &doc_with_text("the lazy dog sleeps"))
            .unwrap();
        c.insert(b"c", &doc_with_text("a fox and a dog play"))
            .unwrap();
        db
    }

    #[test]
    fn text_search_ranks_by_relevance() {
        let db = seed_text();
        let hits = db
            .collection("docs")
            .text_search("body", "fox", 10)
            .unwrap();
        let keys: Vec<_> = hits.iter().map(|h| h.key.clone()).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&b"a".to_vec()));
        assert!(keys.contains(&b"c".to_vec()));
    }

    #[test]
    fn text_search_omits_docs_without_query_terms() {
        let db = seed_text();
        let hits = db
            .collection("docs")
            .text_search("body", "fox", 10)
            .unwrap();
        assert!(!hits.iter().any(|h| h.key == b"b".to_vec()));
    }

    #[test]
    fn text_search_multi_term_scores_higher_with_more_matches() {
        let db = seed_text();
        let hits = db
            .collection("docs")
            .text_search("body", "fox dog", 10)
            .unwrap();
        assert_eq!(hits[0].key, b"c".to_vec());
    }

    #[test]
    fn text_search_empty_query_returns_empty() {
        let db = seed_text();
        let hits = db
            .collection("docs")
            .text_search("body", "  ,. ", 10)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn text_search_respects_k() {
        let db = seed_text();
        let hits = db.collection("docs").text_search("body", "the", 1).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn text_search_skips_missing_and_non_text_fields() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"text", &doc_with_text("hello world")).unwrap();
        c.insert(b"novec", &Value::Int(5)).unwrap();
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), Value::Int(99));
        c.insert(b"wrongtype", &Value::Map(m)).unwrap();
        let hits = c.text_search("body", "hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, b"text".to_vec());
    }

    #[test]
    fn text_search_empty_corpus_returns_empty() {
        let db = Db::open_in_memory().unwrap();
        let hits = db
            .collection("docs")
            .text_search("body", "anything", 5)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn text_search_all_empty_text_returns_empty() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc_with_text("")).unwrap();
        c.insert(b"b", &doc_with_text("")).unwrap();
        let hits = c.text_search("body", "word", 5).unwrap();
        assert!(hits.is_empty());
    }
}
