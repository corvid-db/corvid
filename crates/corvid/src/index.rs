//! Derived ANN index cache.
//!
//! A collection can carry an in-memory HNSW index on a vector field, created
//! with [`Collection::create_vector_index`]. The index is *derived*: documents
//! remain the source of truth, and the index is (re)built from a collection
//! scan whenever a write to that collection invalidates it. A per-collection
//! write generation tracks invalidation; a query rebuilds the index if its
//! build generation is behind, so a query never observes a stale index. On
//! process restart the cache is empty and rebuilds lazily.
//!
//! This realizes the design's reconcile-on-open path: the contract
//! ([`Collection::vector_search`]) is unchanged, only the implementation
//! behind it accelerates.

use std::collections::HashMap;

use crate::db::{Collection, Db};
use crate::distance::Metric;
use crate::error::Result;
use crate::hnsw::Hnsw;
use crate::value::Value;

/// Per-database derived-index state, guarded by a mutex on the [`Db`].
#[derive(Default)]
pub(crate) struct IndexState {
    /// Write generation per collection; bumped on every mutating write.
    generation: HashMap<String, u64>,
    /// Registered indexes keyed by `(collection, field)`.
    indexes: HashMap<(String, String), CachedIndex>,
}

impl IndexState {
    /// Invalidate a collection's derived indexes by advancing its generation.
    pub(crate) fn bump(&mut self, collection: &str) {
        *self.generation.entry(collection.to_owned()).or_insert(0) += 1;
    }

    fn current_generation(&self, collection: &str) -> u64 {
        self.generation.get(collection).copied().unwrap_or(0)
    }
}

/// Ranked `(key, distance)` results, nearest first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// A built HNSW index plus the node-id → document-key mapping and the
/// generation it was built at.
struct CachedIndex {
    metric: Metric,
    built_at: Option<u64>,
    hnsw: Hnsw,
    keys: Vec<Vec<u8>>,
}

impl Db {
    /// Register (or replace) an HNSW index on `field` for `collection`. The
    /// index builds lazily on first use.
    pub(crate) fn register_vector_index(&self, collection: &str, field: &str, metric: Metric) {
        let mut state = self.indexes().lock().expect("index lock");
        state.indexes.insert(
            (collection.to_owned(), field.to_owned()),
            CachedIndex {
                metric,
                built_at: None,
                hnsw: Hnsw::new(metric),
                keys: Vec::new(),
            },
        );
    }

    /// If a matching index is registered, return the approximate nearest `k`
    /// keys with distances; otherwise `None` (the caller falls back to exact
    /// search). The index is rebuilt first if a write has invalidated it.
    pub(crate) fn ann_search(
        &self,
        collection: &str,
        field: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Option<RankedKeys>> {
        let mut state = self.indexes().lock().expect("index lock");
        let key = (collection.to_owned(), field.to_owned());

        match state.indexes.get(&key) {
            Some(ci) if ci.metric == metric => {}
            // No index, or one built for a different metric → caller does exact.
            _ => return Ok(None),
        }

        let current = state.current_generation(collection);
        let stale = state
            .indexes
            .get(&key)
            .is_none_or(|ci| ci.built_at != Some(current));
        if stale {
            let (hnsw, keys) = self.build_index(collection, field, metric)?;
            let ci = state.indexes.get_mut(&key).expect("registered above");
            ci.hnsw = hnsw;
            ci.keys = keys;
            ci.built_at = Some(current);
        }

        let ci = state.indexes.get(&key).expect("registered above");
        let ef = (k * 4).max(64);
        let out = ci
            .hnsw
            .search(query, k, ef)
            .into_iter()
            .map(|(id, dist)| (ci.keys[id].clone(), dist))
            .collect();
        Ok(Some(out))
    }

    /// Build an HNSW index for `field` by scanning `collection`, skipping
    /// documents without a matching-shaped vector. Returns the index and the
    /// node-id → key mapping.
    fn build_index(
        &self,
        collection: &str,
        field: &str,
        metric: Metric,
    ) -> Result<(Hnsw, Vec<Vec<u8>>)> {
        let mut hnsw = Hnsw::new(metric);
        let mut keys = Vec::new();
        for (key, bytes) in self.store().scan(collection)? {
            let doc = Value::decode(&bytes)?;
            if let Some(v) = doc.get(field).and_then(Value::as_vector) {
                hnsw.insert(v.to_vec());
                keys.push(key);
            }
        }
        Ok((hnsw, keys))
    }
}

impl Collection<'_> {
    /// Create (or replace) an in-memory HNSW index on `field` under `metric`.
    ///
    /// Once created, [`Collection::vector_search`] on the same `field` and
    /// `metric` uses it (approximate, faster); other fields/metrics and the
    /// filtered builder path stay exact. The index is derived from the
    /// documents and rebuilt automatically after writes.
    pub fn create_vector_index(&self, field: &str, metric: Metric) {
        self.db().register_vector_index(self.name(), field, metric);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(v: Vec<f32>) -> Value {
        let mut m = BTreeMap::new();
        m.insert("embedding".to_owned(), Value::Vector(v));
        Value::Map(m)
    }

    fn seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.insert(b"c", &doc(vec![-1.0, 0.0])).unwrap();
        db
    }

    #[test]
    fn indexed_search_matches_exact_on_small_corpus() {
        let db = seeded();
        let c = db.collection("docs");
        // Exact result first (no index registered yet).
        let exact = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();

        c.create_vector_index("embedding", Metric::L2);
        let indexed = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();

        let exact_keys: Vec<_> = exact.iter().map(|h| h.key.clone()).collect();
        let indexed_keys: Vec<_> = indexed.iter().map(|h| h.key.clone()).collect();
        // Small corpus + ample ef → ANN recovers the exact ordering.
        assert_eq!(exact_keys, indexed_keys);
    }

    #[test]
    fn index_rebuilds_after_insert() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2);
        // Prime the index.
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();

        // A uniquely-nearest new doc must appear after the write invalidates
        // and rebuilds the index.
        c.insert(b"exact", &doc(vec![5.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[5.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"exact".to_vec());
    }

    #[test]
    fn index_rebuilds_after_delete() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2);
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();

        c.delete(b"a").unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
    }

    #[test]
    fn metric_mismatch_falls_back_to_exact() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine);
        // Searching with a different metric ignores the index but still works.
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn unindexed_field_uses_exact() {
        let db = seeded();
        let c = db.collection("docs");
        // No index created → exact path, correct result.
        let hits = c
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }

    #[test]
    fn writes_to_other_collections_do_not_invalidate() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2);
        let first = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        // Writing elsewhere must not change this collection's results.
        db.collection("other")
            .insert(b"z", &doc(vec![9.0, 9.0]))
            .unwrap();
        let second = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert_eq!(
            first.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            second.iter().map(|h| h.key.clone()).collect::<Vec<_>>()
        );
    }
}
