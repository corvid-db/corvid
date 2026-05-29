//! Derived ANN index maintenance.
//!
//! A collection can carry an HNSW index on a vector field, created with
//! [`Collection::create_vector_index`]. Index *definitions* are persisted (in a
//! reserved `__indexes__` collection) so they survive a reopen; the HNSW graph
//! itself is in-memory and built lazily on first use from a collection scan.
//!
//! After the initial build the graph is maintained **incrementally**: each
//! insert adds one node and each delete tombstones one, both O(log n) — there
//! is no full rebuild per write (which would be quadratic for a write-then-read
//! loop). Overwrites tombstone the old node and add a new one. When tombstones
//! exceed half the graph, it is compacted by a one-off rebuild from the store.
//! Documents remain the source of truth, so a query never observes a stale
//! index.

use std::collections::HashMap;

use crate::db::{Collection, Db};
use crate::distance::Metric;
use crate::error::Result;
use crate::hnsw::Hnsw;
use crate::store::Store;
use crate::value::Value;

/// Reserved collection holding persisted index definitions.
const INDEX_DEFS: &str = "__indexes__";

/// Ranked `(key, distance)` results, nearest first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// Per-database derived-index state, guarded by a mutex on the [`Db`].
#[derive(Default)]
pub(crate) struct IndexState {
    /// Registered index definitions (`(collection, field) -> metric`).
    defs: HashMap<(String, String), Metric>,
    /// Built in-memory indexes, populated lazily.
    built: HashMap<(String, String), BuiltIndex>,
}

/// A built HNSW graph plus the bookkeeping to map nodes to live keys.
struct BuiltIndex {
    hnsw: Hnsw,
    /// node id -> key, or `None` if the node was tombstoned.
    node_to_key: Vec<Option<Vec<u8>>>,
    /// live key -> current node id.
    key_to_node: HashMap<Vec<u8>, usize>,
}

impl BuiltIndex {
    fn new(metric: Metric) -> Self {
        Self {
            hnsw: Hnsw::new(metric),
            node_to_key: Vec::new(),
            key_to_node: HashMap::new(),
        }
    }

    fn dead(&self) -> usize {
        self.node_to_key.len() - self.key_to_node.len()
    }

    /// Add (or replace) `key`'s vector. An existing node for `key` is tombstoned.
    fn add(&mut self, key: &[u8], vector: Vec<f32>) {
        self.tombstone(key);
        let id = self.hnsw.insert(vector);
        debug_assert_eq!(id, self.node_to_key.len(), "hnsw ids are dense");
        self.node_to_key.push(Some(key.to_vec()));
        self.key_to_node.insert(key.to_vec(), id);
    }

    /// Tombstone `key`'s node if present.
    fn tombstone(&mut self, key: &[u8]) {
        if let Some(old) = self.key_to_node.remove(key) {
            self.node_to_key[old] = None;
        }
    }

    /// Search for the nearest `k` live keys. Over-fetches by the tombstone
    /// count so that, even if every dead node ranks ahead, `k` live nodes
    /// remain.
    fn search(&self, query: &[f32], k: usize) -> RankedKeys {
        if k == 0 {
            return Vec::new();
        }
        let want = k + self.dead();
        let ef = (want * 4).max(64);
        self.hnsw
            .search(query, want, ef)
            .into_iter()
            .filter_map(|(id, dist)| self.node_to_key[id].clone().map(|key| (key, dist)))
            .take(k)
            .collect()
    }
}

impl Db {
    /// Load persisted index definitions. Called once on open.
    pub(crate) fn load_index_defs(&self) -> Result<()> {
        let mut state = self.indexes().lock().expect("index lock");
        for (key, value) in self.store().scan(INDEX_DEFS)? {
            if let (Some((coll, field)), Some(metric)) = (
                split_def_key(&key),
                value.first().and_then(metric_from_byte),
            ) {
                state.defs.insert((coll, field), metric);
            }
        }
        Ok(())
    }

    /// Register (or replace) an HNSW index on `field` for `collection`. The
    /// definition is persisted; the graph builds lazily on first use.
    pub(crate) fn register_vector_index(
        &self,
        collection: &str,
        field: &str,
        metric: Metric,
    ) -> Result<()> {
        self.store().put(
            INDEX_DEFS,
            &def_key(collection, field),
            &[metric_byte(metric)],
        )?;
        let mut state = self.indexes().lock().expect("index lock");
        let key = (collection.to_owned(), field.to_owned());
        state.defs.insert(key.clone(), metric);
        // Drop any built graph so it rebuilds with the (possibly new) metric.
        state.built.remove(&key);
        Ok(())
    }

    /// Maintain every index on `collection` after a document write.
    pub(crate) fn index_on_insert(&self, collection: &str, key: &[u8], doc: &Value) {
        let mut state = self.indexes().lock().expect("index lock");
        let fields: Vec<String> = state
            .defs
            .keys()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect();
        for field in fields {
            let map_key = (collection.to_owned(), field.clone());
            // Only maintain an already-built graph; an unbuilt one will pick
            // this write up when it builds lazily from a scan.
            if let Some(built) = state.built.get_mut(&map_key) {
                match doc.get(&field).and_then(Value::as_vector) {
                    Some(v) => built.add(key, v.to_vec()),
                    None => built.tombstone(key),
                }
            }
        }
    }

    /// Tombstone `key` in every built index on `collection` after a delete.
    pub(crate) fn index_on_delete(&self, collection: &str, key: &[u8]) {
        let mut state = self.indexes().lock().expect("index lock");
        let map_keys: Vec<(String, String)> = state
            .built
            .keys()
            .filter(|(c, _)| c == collection)
            .cloned()
            .collect();
        for mk in map_keys {
            if let Some(built) = state.built.get_mut(&mk) {
                built.tombstone(key);
            }
        }
    }

    /// If a matching index is registered, return the approximate nearest `k`
    /// keys with distances; otherwise `None` (the caller falls back to exact).
    pub(crate) fn ann_search(
        &self,
        collection: &str,
        field: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Option<RankedKeys>> {
        let map_key = (collection.to_owned(), field.to_owned());

        // Decide what work to do under a short lock; build/scan unlocked.
        let needs_build = {
            let state = self.indexes().lock().expect("index lock");
            match state.defs.get(&map_key) {
                Some(m) if *m == metric => !state.built.contains_key(&map_key),
                _ => return Ok(None), // no index, or metric mismatch → exact
            }
        };

        if needs_build {
            let built = build_index(self.store(), collection, field, metric)?;
            let mut state = self.indexes().lock().expect("index lock");
            state.built.entry(map_key.clone()).or_insert(built);
        }

        // Compact if more than half the graph is tombstoned.
        let needs_compact = {
            let state = self.indexes().lock().expect("index lock");
            state
                .built
                .get(&map_key)
                .is_some_and(|b| !b.node_to_key.is_empty() && b.dead() * 2 > b.node_to_key.len())
        };
        if needs_compact {
            let built = build_index(self.store(), collection, field, metric)?;
            let mut state = self.indexes().lock().expect("index lock");
            state.built.insert(map_key.clone(), built);
        }

        let state = self.indexes().lock().expect("index lock");
        Ok(state.built.get(&map_key).map(|b| b.search(query, k)))
    }
}

/// Build a fresh index for `field` by scanning `collection`.
fn build_index(store: &Store, collection: &str, field: &str, metric: Metric) -> Result<BuiltIndex> {
    let mut built = BuiltIndex::new(metric);
    for (key, bytes) in store.scan(collection)? {
        let doc = Value::decode(&bytes)?;
        if let Some(v) = doc.get(field).and_then(Value::as_vector) {
            built.add(&key, v.to_vec());
        }
    }
    Ok(built)
}

fn def_key(collection: &str, field: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(collection.len() + 1 + field.len());
    k.extend_from_slice(collection.as_bytes());
    k.push(0);
    k.extend_from_slice(field.as_bytes());
    k
}

fn split_def_key(key: &[u8]) -> Option<(String, String)> {
    let pos = key.iter().position(|&b| b == 0)?;
    let coll = std::str::from_utf8(&key[..pos]).ok()?.to_owned();
    let field = std::str::from_utf8(&key[pos + 1..]).ok()?.to_owned();
    Some((coll, field))
}

fn metric_byte(m: Metric) -> u8 {
    match m {
        Metric::Cosine => 0,
        Metric::Dot => 1,
        Metric::L2 => 2,
    }
}

fn metric_from_byte(b: &u8) -> Option<Metric> {
    match b {
        0 => Some(Metric::Cosine),
        1 => Some(Metric::Dot),
        2 => Some(Metric::L2),
        _ => None,
    }
}

impl Collection<'_> {
    /// Create (or replace) an in-memory HNSW index on `field` under `metric`.
    ///
    /// The definition persists across reopen; the graph builds lazily and is
    /// then maintained incrementally. [`Collection::vector_search`] on the same
    /// `field`/`metric` uses it; other fields/metrics stay exact.
    pub fn create_vector_index(&self, field: &str, metric: Metric) -> Result<()> {
        self.db().register_vector_index(self.name(), field, metric)
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
        let exact = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        c.create_vector_index("embedding", Metric::L2).unwrap();
        let indexed = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert_eq!(
            exact.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            indexed.iter().map(|h| h.key.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn incremental_insert_is_reflected_without_full_rebuild() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        // Build the graph.
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        // New uniquely-nearest doc — maintained incrementally.
        c.insert(b"exact", &doc(vec![5.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[5.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"exact".to_vec());
    }

    #[test]
    fn delete_tombstones_from_index() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
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
    fn overwrite_updates_indexed_vector() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        // Move "a" far away; it should no longer be the nearest to (1,0).
        c.insert(b"a", &doc(vec![9.0, 9.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_ne!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn many_overwrites_then_query_stays_correct_after_compaction() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        c.insert(b"k", &doc(vec![0.0, 0.0])).unwrap();
        let _ = c
            .vector_search("embedding", &[0.0, 0.0], 1, Metric::L2)
            .unwrap();
        // Overwrite the same key many times → many tombstones → triggers compaction.
        for i in 0..20 {
            c.insert(b"k", &doc(vec![i as f32, 0.0])).unwrap();
        }
        c.insert(b"target", &doc(vec![100.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[100.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"target".to_vec());
    }

    #[test]
    fn index_definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.create_vector_index("embedding", Metric::Cosine).unwrap();
        }
        // Reopen: the index definition should be reloaded and used.
        let db = Db::open(&path).unwrap();
        db.collection("docs")
            .insert(b"b", &doc(vec![0.0, 1.0]))
            .unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn metric_mismatch_falls_back_to_exact() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn unindexed_field_uses_exact() {
        let db = seeded();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }
}
