//! Query operations over a collection (engine layer L4, in progress).
//!
//! Today this provides exact (brute-force) k-nearest-neighbour vector search.
//! It is correct and simple — the baseline every approximate index is measured
//! against. An HNSW index can replace the scan behind [`Collection::vector_search`]
//! later without changing the result contract.

use crate::db::Collection;
use crate::distance::Metric;
use crate::error::Result;
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

impl Collection<'_> {
    /// Return the `k` documents whose embedding in field `field` is nearest to
    /// `query` under `metric`, nearest first.
    ///
    /// Documents that lack the field, whose field is not a [`Value::Vector`],
    /// or whose dimension differs from `query` are skipped — a missing or
    /// mismatched embedding is not an error under schema-on-read. Ties are
    /// broken by key order for determinism.
    pub fn vector_search(
        &self,
        field: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Vec<Hit>> {
        let mut hits: Vec<Hit> = Vec::new();
        for (key, document) in self.scan()? {
            let Some(vector) = document.get(field).and_then(Value::as_vector) else {
                continue;
            };
            if vector.len() != query.len() {
                continue;
            }
            let distance = metric.distance(query, vector);
            hits.push(Hit {
                key,
                distance,
                document,
            });
        }

        // Sort by distance ascending; break ties by key for a stable result.
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.key.cmp(&b.key))
        });
        hits.truncate(k);
        Ok(hits)
    }
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
        // Two identical vectors -> equal distance -> key order decides.
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
}
