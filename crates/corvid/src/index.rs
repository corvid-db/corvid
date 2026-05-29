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
use crate::disk_hnsw::{self, DiskParams};
use crate::distance::Metric;
use crate::error::Result;
use crate::hnsw::{DEFAULT_EF_CONSTRUCTION, DEFAULT_M, Hnsw, Quantization};
use crate::store::Store;
use crate::value::Value;

/// Reserved collection holding persisted index definitions.
const INDEX_DEFS: &str = "__indexes__";

/// Ranked `(key, distance)` results, nearest first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// Where a vector index lives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexKind {
    /// HNSW graph held in RAM, rebuilt lazily on open.
    InMemory,
    /// HNSW graph stored on disk (redb); bounded memory, persists across open.
    OnDisk,
}

/// A registered vector index definition.
#[derive(Clone, Copy)]
struct VectorDef {
    metric: Metric,
    quant: Quantization,
    kind: IndexKind,
}

impl VectorDef {
    fn disk_params(&self) -> DiskParams {
        DiskParams::new(self.metric, DEFAULT_M, DEFAULT_EF_CONSTRUCTION)
    }
}

/// Per-database derived-index state, guarded by a mutex on the [`Db`].
#[derive(Default)]
pub(crate) struct IndexState {
    /// Registered index definitions (`(collection, field) -> def`).
    defs: HashMap<(String, String), VectorDef>,
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
    fn new(def: VectorDef) -> Self {
        Self {
            hnsw: Hnsw::with_quant(def.metric, def.quant, DEFAULT_M, DEFAULT_EF_CONSTRUCTION),
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
                // Quantization and kind bytes are optional (older defs lack them).
                let quant = value
                    .get(1)
                    .and_then(quant_from_byte)
                    .unwrap_or(Quantization::None);
                let kind = value
                    .get(2)
                    .and_then(kind_from_byte)
                    .unwrap_or(IndexKind::InMemory);
                state.defs.insert(
                    (coll, field),
                    VectorDef {
                        metric,
                        quant,
                        kind,
                    },
                );
            }
        }
        Ok(())
    }

    /// Register (or replace) an HNSW index on `field` for `collection`, with a
    /// storage quantization mode. The definition is persisted; the graph builds
    /// lazily on first use.
    pub(crate) fn register_vector_index(
        &self,
        collection: &str,
        field: &str,
        metric: Metric,
        quant: Quantization,
        kind: IndexKind,
    ) -> Result<()> {
        self.store().put(
            INDEX_DEFS,
            &def_key(collection, field),
            &[metric_byte(metric), quant_byte(quant), kind_byte(kind)],
        )?;
        let mut state = self.indexes().lock().expect("index lock");
        let key = (collection.to_owned(), field.to_owned());
        state.defs.insert(
            key.clone(),
            VectorDef {
                metric,
                quant,
                kind,
            },
        );
        // Drop any built in-memory graph so it rebuilds with the (possibly new) def.
        state.built.remove(&key);
        Ok(())
    }

    /// Maintain every index on `collection` after a document write.
    pub(crate) fn index_on_insert(&self, collection: &str, key: &[u8], doc: &Value) -> Result<()> {
        // Snapshot the relevant defs, then work without holding the lock for
        // on-disk I/O.
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), d)| (f.clone(), *d))
                .collect()
        };
        for (field, def) in defs {
            match def.kind {
                IndexKind::OnDisk => {
                    let ns = disk_hnsw::namespace(collection, &field);
                    match doc.get(&field).and_then(Value::as_vector) {
                        Some(v) => {
                            disk_hnsw::insert(self.store(), &ns, &def.disk_params(), key, v)?
                        }
                        None => {
                            disk_hnsw::delete(self.store(), &ns, key)?;
                        }
                    }
                }
                IndexKind::InMemory => {
                    let map_key = (collection.to_owned(), field.clone());
                    let mut state = self.indexes().lock().expect("index lock");
                    // Only maintain an already-built graph; an unbuilt one picks
                    // this up when it builds lazily.
                    if let Some(built) = state.built.get_mut(&map_key) {
                        match doc.get(&field).and_then(Value::as_vector) {
                            Some(v) => built.add(key, v.to_vec()),
                            None => built.tombstone(key),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Remove `key` from every index on `collection` after a delete.
    pub(crate) fn index_on_delete(&self, collection: &str, key: &[u8]) -> Result<()> {
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), d)| (f.clone(), *d))
                .collect()
        };
        for (field, def) in defs {
            match def.kind {
                IndexKind::OnDisk => {
                    let ns = disk_hnsw::namespace(collection, &field);
                    disk_hnsw::delete(self.store(), &ns, key)?;
                }
                IndexKind::InMemory => {
                    let map_key = (collection.to_owned(), field);
                    let mut state = self.indexes().lock().expect("index lock");
                    if let Some(built) = state.built.get_mut(&map_key) {
                        built.tombstone(key);
                    }
                }
            }
        }
        Ok(())
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
        let def = {
            let state = self.indexes().lock().expect("index lock");
            match state.defs.get(&map_key) {
                Some(d) if d.metric == metric => *d,
                _ => return Ok(None), // no index, or metric mismatch → exact
            }
        };

        // On-disk indexes are served directly from the store (bounded memory).
        if def.kind == IndexKind::OnDisk {
            let ns = disk_hnsw::namespace(collection, field);
            let ef = (k * 4).max(64);
            return Ok(Some(disk_hnsw::search(
                self.store(),
                &ns,
                &def.disk_params(),
                query,
                k,
                ef,
            )?));
        }

        let needs_build = !self
            .indexes()
            .lock()
            .expect("index lock")
            .built
            .contains_key(&map_key);

        if needs_build {
            let built = build_index(self.store(), collection, field, def)?;
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
            let built = build_index(self.store(), collection, field, def)?;
            let mut state = self.indexes().lock().expect("index lock");
            state.built.insert(map_key.clone(), built);
        }

        let state = self.indexes().lock().expect("index lock");
        Ok(state.built.get(&map_key).map(|b| b.search(query, k)))
    }
}

/// Build a fresh index for `field` by scanning `collection`.
fn build_index(store: &Store, collection: &str, field: &str, def: VectorDef) -> Result<BuiltIndex> {
    let mut built = BuiltIndex::new(def);
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

fn quant_byte(q: Quantization) -> u8 {
    match q {
        Quantization::None => 0,
        Quantization::Binary => 1,
        Quantization::Scalar => 2,
    }
}

fn quant_from_byte(b: &u8) -> Option<Quantization> {
    match b {
        0 => Some(Quantization::None),
        1 => Some(Quantization::Binary),
        2 => Some(Quantization::Scalar),
        _ => None,
    }
}

fn kind_byte(k: IndexKind) -> u8 {
    match k {
        IndexKind::InMemory => 0,
        IndexKind::OnDisk => 1,
    }
}

fn kind_from_byte(b: &u8) -> Option<IndexKind> {
    match b {
        0 => Some(IndexKind::InMemory),
        1 => Some(IndexKind::OnDisk),
        _ => None,
    }
}

impl Collection<'_> {
    /// Create (or replace) a full-precision in-memory HNSW index on `field`.
    ///
    /// The definition persists across reopen; the graph builds lazily and is
    /// then maintained incrementally. [`Collection::vector_search`] on the same
    /// `field`/`metric` uses it; other fields/metrics stay exact.
    pub fn create_vector_index(&self, field: &str, metric: Metric) -> Result<()> {
        self.db().register_vector_index(
            self.name(),
            field,
            metric,
            Quantization::None,
            IndexKind::InMemory,
        )
    }

    /// Like [`Collection::create_vector_index`] but storing vectors with a
    /// [`Quantization`] mode to cut index memory (binary ≈ 32×, scalar ≈ 4×) at
    /// some recall cost.
    pub fn create_vector_index_quantized(
        &self,
        field: &str,
        metric: Metric,
        quant: Quantization,
    ) -> Result<()> {
        self.db()
            .register_vector_index(self.name(), field, metric, quant, IndexKind::InMemory)
    }

    /// Create an **on-disk** HNSW index on `field`. The graph is stored in the
    /// database (not RAM) and persists across reopen, so search memory is
    /// bounded by nodes touched per query rather than by collection size —
    /// suitable for very large collections. Existing documents are backfilled.
    pub fn create_vector_index_ondisk(&self, field: &str, metric: Metric) -> Result<()> {
        self.db().register_vector_index(
            self.name(),
            field,
            metric,
            Quantization::None,
            IndexKind::OnDisk,
        )?;
        // Backfill existing documents, streaming (bounded memory).
        let ns = disk_hnsw::namespace(self.name(), field);
        let params = DiskParams::new(metric, DEFAULT_M, DEFAULT_EF_CONSTRUCTION);
        let store = self.db().store();
        self.for_each_doc(|key, doc| {
            if let Some(v) = doc.get(field).and_then(Value::as_vector) {
                disk_hnsw::insert(store, &ns, &params, key, v)?;
            }
            Ok(true)
        })?;
        Ok(())
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
    fn quantized_index_is_used_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
            c.create_vector_index_quantized("embedding", Metric::Cosine, Quantization::Scalar)
                .unwrap();
            let hits = c
                .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
                .unwrap();
            assert_eq!(hits[0].key, b"a".to_vec());
        }
        // The quantized definition reloads on reopen and is still used.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn binary_quantized_index_via_collection() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"pos", &doc(vec![1.0, 1.0])).unwrap();
        c.insert(b"neg", &doc(vec![-1.0, -1.0])).unwrap();
        c.create_vector_index_quantized("embedding", Metric::Cosine, Quantization::Binary)
            .unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 1.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"pos".to_vec());
    }

    #[test]
    fn ondisk_index_searches_persists_and_backfills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            // Insert BEFORE creating the index → exercises backfill.
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
            c.create_vector_index_ondisk("embedding", Metric::L2)
                .unwrap();
            // Insert AFTER → exercises incremental on-disk maintenance.
            c.insert(b"c", &doc(vec![0.9, 0.1])).unwrap();
            let hits = c
                .vector_search("embedding", &[1.0, 0.0], 2, Metric::L2)
                .unwrap();
            assert_eq!(hits[0].key, b"a".to_vec());
            assert_eq!(hits[1].key, b"c".to_vec());
        }
        // Reopen: on-disk graph is used directly — no rebuild.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }

    #[test]
    fn ondisk_index_reflects_delete() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.delete(b"a").unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 5, Metric::L2)
            .unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
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
