//! A semantic (vector-keyed) cache.
//!
//! Stores `(embedding, value)` entries and, on lookup, returns the cached value
//! when the nearest stored embedding is within a distance threshold of the
//! query. This is the classic LLM-response / RAG cache: cache by meaning, not
//! by exact key. It is a thin layer over [`Collection::vector_search`], so it
//! automatically benefits from an HNSW index created on the same field.

use crate::db::Collection;
use crate::distance::Metric;
use crate::error::Result;
use crate::value::Value;

/// A vector-keyed cache over a collection.
///
/// Entries are documents holding an embedding field and a value field. A
/// lookup returns the value of the nearest entry whose distance to the query
/// is at most `threshold` (interpret `threshold` in the chosen metric's units —
/// e.g. cosine distance in `[0, 2]`).
pub struct SemanticCache<'a> {
    collection: Collection<'a>,
    embedding_field: String,
    value_field: String,
    metric: Metric,
    threshold: f32,
}

impl<'a> Collection<'a> {
    /// View this collection as a [`SemanticCache`] keyed on `embedding_field`,
    /// storing payloads in `value_field`, matched under `metric` within
    /// `threshold`.
    pub fn semantic_cache(
        self,
        embedding_field: impl Into<String>,
        value_field: impl Into<String>,
        metric: Metric,
        threshold: f32,
    ) -> SemanticCache<'a> {
        SemanticCache {
            collection: self,
            embedding_field: embedding_field.into(),
            value_field: value_field.into(),
            metric,
            threshold,
        }
    }
}

impl SemanticCache<'_> {
    /// Insert or overwrite a cache entry at `key`.
    pub fn put(&self, key: &[u8], embedding: Vec<f32>, value: Value) -> Result<()> {
        let mut doc = std::collections::BTreeMap::new();
        doc.insert(self.embedding_field.clone(), Value::Vector(embedding));
        doc.insert(self.value_field.clone(), value);
        self.collection.insert(key, &Value::Map(doc))
    }

    /// Look up the cached value for `query`. Returns the value of the nearest
    /// entry within `threshold`, or `None` on a miss (no entry, nearest too
    /// far, or the entry lacks the value field).
    pub fn get(&self, query: &[f32]) -> Result<Option<Value>> {
        let hits = self
            .collection
            .vector_search(&self.embedding_field, query, 1, self.metric)?;
        match hits.into_iter().next() {
            Some(hit) if hit.distance <= self.threshold => {
                Ok(hit.document.get(&self.value_field).cloned())
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, Metric, Value};

    fn cache_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn exact_hit_returns_value() {
        let db = cache_db();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.05);
        cache
            .put(b"k1", vec![1.0, 0.0], Value::Text("answer".into()))
            .unwrap();
        assert_eq!(
            cache.get(&[1.0, 0.0]).unwrap(),
            Some(Value::Text("answer".into()))
        );
    }

    #[test]
    fn near_hit_within_threshold_returns_value() {
        let db = cache_db();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.05);
        cache
            .put(b"k1", vec![1.0, 0.0], Value::Text("answer".into()))
            .unwrap();
        // Slightly rotated query — cosine distance well under 0.05.
        assert_eq!(
            cache.get(&[0.999, 0.01]).unwrap(),
            Some(Value::Text("answer".into()))
        );
    }

    #[test]
    fn far_query_is_a_miss() {
        let db = cache_db();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.05);
        cache
            .put(b"k1", vec![1.0, 0.0], Value::Text("answer".into()))
            .unwrap();
        // Orthogonal → cosine distance 1.0 ≫ threshold.
        assert_eq!(cache.get(&[0.0, 1.0]).unwrap(), None);
    }

    #[test]
    fn empty_cache_is_a_miss() {
        let db = cache_db();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.5);
        assert_eq!(cache.get(&[1.0, 0.0]).unwrap(), None);
    }

    #[test]
    fn overwrite_updates_value() {
        let db = cache_db();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.05);
        cache
            .put(b"k", vec![1.0, 0.0], Value::Text("old".into()))
            .unwrap();
        cache
            .put(b"k", vec![1.0, 0.0], Value::Text("new".into()))
            .unwrap();
        assert_eq!(
            cache.get(&[1.0, 0.0]).unwrap(),
            Some(Value::Text("new".into()))
        );
    }

    #[test]
    fn entry_without_value_field_is_miss() {
        let db = cache_db();
        // Insert a doc with the embedding but no value field directly.
        let mut m = std::collections::BTreeMap::new();
        m.insert("q".to_owned(), Value::Vector(vec![1.0, 0.0]));
        db.collection("cache").insert(b"k", &Value::Map(m)).unwrap();
        let cache = db
            .collection("cache")
            .semantic_cache("q", "response", Metric::Cosine, 0.05);
        assert_eq!(cache.get(&[1.0, 0.0]).unwrap(), None);
    }

    #[test]
    fn works_with_an_ann_index() {
        let db = cache_db();
        let c = db.collection("cache");
        c.create_vector_index("q", Metric::Cosine);
        let cache = c.semantic_cache("q", "response", Metric::Cosine, 0.05);
        cache.put(b"k", vec![1.0, 0.0], Value::Int(42)).unwrap();
        assert_eq!(cache.get(&[1.0, 0.0]).unwrap(), Some(Value::Int(42)));
    }
}
