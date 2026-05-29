//! On-disk HNSW: a persistent approximate-nearest-neighbour graph stored as
//! redb entries instead of in RAM.
//!
//! The in-memory [`Hnsw`](crate::hnsw::Hnsw) is fast but holds the whole graph
//! (and rebuilds it on open), so it can't scale to billions of vectors on a
//! laptop. This index keeps every node — its vector and per-layer neighbour
//! lists — on disk, and insert/search load only the nodes they touch into a
//! small per-operation cache. Memory is therefore bounded by *nodes touched
//! per operation* (≈ `ef × M`), not by the collection size, and the graph
//! persists across reopen (no rebuild).
//!
//! Each index lives in a reserved collection. Keys are tagged:
//! `n‖id` → node, `k‖doc_key` → node id, `m` → metadata. A write performs the
//! whole insert (reads + neighbour updates) in one redb transaction, so the
//! graph is never left half-updated.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::distance::Metric;
use crate::error::Result;
use crate::pq::Pq;
use crate::quant::{Probe, Quantization, StoredVec};
use crate::store::Store;

const TAG_NODE: u8 = b'n';
const TAG_KEYMAP: u8 = b'k';
const TAG_META: u8 = b'm';
/// Key holding the persisted PQ codebook (only present for PQ indexes).
pub(crate) const TAG_PQ: u8 = b'q';

/// Build/search parameters for an on-disk index.
#[derive(Clone)]
pub(crate) struct DiskParams {
    pub metric: Metric,
    pub quant: Quantization,
    /// When set, vectors are stored as PQ codes and distances use this codebook
    /// (overrides `quant`).
    pub pq: Option<Arc<Pq>>,
    pub m: usize,
    pub m0: usize,
    pub ef_construction: usize,
    pub ml: f64,
}

impl DiskParams {
    pub fn with_quant(
        metric: Metric,
        quant: Quantization,
        m: usize,
        ef_construction: usize,
    ) -> Self {
        let m = m.max(2);
        Self {
            metric,
            quant,
            pq: None,
            m,
            m0: m * 2,
            ef_construction: ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
        }
    }

    /// Set the PQ codebook (storage becomes PQ codes).
    pub fn with_pq(mut self, pq: Arc<Pq>) -> Self {
        self.pq = Some(pq);
        self
    }

    fn neighbors_at(&self, layer: usize) -> usize {
        if layer == 0 { self.m0 } else { self.m }
    }

    /// Build the PQ probe for a query: under L2, precompute the asymmetric-
    /// distance table once (so each node is O(m) table lookups instead of an
    /// O(dim) reconstruct + distance); other metrics keep the query vector.
    fn pq_probe(pq: &Pq, metric: Metric, query: Vec<f32>) -> DProbe {
        match metric {
            Metric::L2 => DProbe::PqAdc(pq.l2_table(&query)),
            _ => DProbe::Pq(query),
        }
    }

    /// Encode a query vector into the form distances are computed against.
    fn make_probe(&self, query: &[f32]) -> DProbe {
        match &self.pq {
            Some(pq) => Self::pq_probe(pq, self.metric, query.to_vec()),
            None => DProbe::Q(self.quant.probe(query)),
        }
    }

    /// Build a probe from an already-stored vector (for neighbour pruning).
    fn probe_of(&self, stored: &StoredVec) -> DProbe {
        match (&self.pq, stored) {
            (Some(pq), StoredVec::Packed(code)) => Self::pq_probe(pq, self.metric, pq.decode(code)),
            (Some(_), StoredVec::Full(_)) => DProbe::Pq(Vec::new()),
            (None, _) => DProbe::Q(self.quant.probe_of(stored)),
        }
    }

    /// Distance from a probe to a stored vector.
    fn dist(&self, probe: &DProbe, stored: &StoredVec) -> f32 {
        match (probe, &self.pq, stored) {
            (DProbe::PqAdc(table), Some(pq), StoredVec::Packed(code)) => pq.adc_l2(table, code),
            (DProbe::Pq(q), Some(pq), StoredVec::Packed(code)) => pq.distance(self.metric, q, code),
            (DProbe::Q(p), None, _) => self.quant.dist(self.metric, p, stored),
            _ => f32::INFINITY,
        }
    }

    /// Encode a vector for storage.
    fn encode(&self, v: &[f32]) -> StoredVec {
        match &self.pq {
            Some(pq) => StoredVec::Packed(pq.encode(v)),
            None => self.quant.encode(v),
        }
    }

    /// Decode stored bytes into a [`StoredVec`] (PQ codes stay packed).
    fn decode_stored(&self, bytes: &[u8]) -> StoredVec {
        match &self.pq {
            Some(_) => StoredVec::Packed(bytes.to_vec()),
            None => StoredVec::from_bytes(self.quant, bytes),
        }
    }
}

/// A query in the representation used for distance: a quantization probe, a
/// full vector for the PQ reconstruction path, or a precomputed PQ L2
/// asymmetric-distance table for the fast path.
enum DProbe {
    Q(Probe),
    Pq(Vec<f32>),
    PqAdc(Vec<f32>),
}

/// A graph node as stored on disk.
#[derive(Clone)]
struct Node {
    deleted: bool,
    doc_key: Vec<u8>,
    /// The vector in the index's storage form (full or quantized).
    vector: StoredVec,
    /// neighbour ids per layer (`layers[l]`).
    layers: Vec<Vec<u64>>,
}

/// Index metadata.
#[derive(Clone, Copy)]
struct Meta {
    entry: Option<u64>,
    count: u64,
}

// ---- key encoding ----

fn node_key(id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(TAG_NODE);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn keymap_key(doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + doc_key.len());
    k.push(TAG_KEYMAP);
    k.extend_from_slice(doc_key);
    k
}

// ---- node/meta codec ----

fn encode_node(node: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(node.deleted as u8);
    put_u32(&mut out, node.doc_key.len());
    out.extend_from_slice(&node.doc_key);
    let vec_bytes = node.vector.to_bytes();
    put_u32(&mut out, vec_bytes.len());
    out.extend_from_slice(&vec_bytes);
    put_u32(&mut out, node.layers.len());
    for layer in &node.layers {
        put_u32(&mut out, layer.len());
        for &id in layer {
            out.extend_from_slice(&id.to_be_bytes());
        }
    }
    out
}

fn decode_node(b: &[u8], p: &DiskParams) -> Option<Node> {
    let mut c = Cursor { b, pos: 0 };
    let deleted = c.u8()? != 0;
    let dk_len = c.u32()?;
    let doc_key = c.take(dk_len)?.to_vec();
    let vec_len = c.u32()?;
    let vector = p.decode_stored(c.take(vec_len)?);
    let n_layers = c.u32()?;
    let mut layers = Vec::with_capacity(n_layers);
    for _ in 0..n_layers {
        let cnt = c.u32()?;
        let mut layer = Vec::with_capacity(cnt);
        for _ in 0..cnt {
            layer.push(u64::from_be_bytes(c.take(8)?.try_into().ok()?));
        }
        layers.push(layer);
    }
    Some(Node {
        deleted,
        doc_key,
        vector,
        layers,
    })
}

fn encode_meta(m: Meta) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(m.entry.is_some() as u8);
    out.extend_from_slice(&m.entry.unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&m.count.to_be_bytes());
    out
}

fn decode_meta(b: &[u8]) -> Meta {
    if b.len() < 17 {
        return Meta {
            entry: None,
            count: 0,
        };
    }
    let has = b[0] != 0;
    let entry = u64::from_be_bytes(b[1..9].try_into().unwrap());
    let count = u64::from_be_bytes(b[9..17].try_into().unwrap());
    Meta {
        entry: has.then_some(entry),
        count,
    }
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.b.len())?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }
}

/// Deterministic level for a node id (no persisted RNG state needed).
fn level_for(id: u64, ml: f64) -> usize {
    // splitmix64 of the id → uniform in (0, 1].
    let mut z = id.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    let u = ((z >> 11) as f64 + 1.0) / ((1u64 << 53) as f64);
    (-u.ln() * ml).floor() as usize
}

/// A `(distance, id)` candidate, ordered by distance then id.
#[derive(Clone, Copy, PartialEq)]
struct Cand {
    dist: f32,
    id: u64,
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.dist.total_cmp(&o.dist).then(self.id.cmp(&o.id))
    }
}

/// Loads nodes from a reader on demand and caches them for the operation,
/// tracking which are dirtied so they can be flushed in one pass.
struct Cache {
    /// Cached nodes behind `Rc` so loading one into the search frontier is a
    /// refcount bump, not a deep copy of its vector + neighbour lists.
    nodes: HashMap<u64, Rc<Node>>,
    dirty: std::collections::HashSet<u64>,
}

impl Cache {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            dirty: std::collections::HashSet::new(),
        }
    }
}

// ---- public operations ----

/// Insert (or overwrite) `doc_key`'s `vector` into the on-disk index `ns`.
pub(crate) fn insert(
    store: &Store,
    ns: &str,
    p: &DiskParams,
    doc_key: &[u8],
    vector: &[f32],
) -> Result<()> {
    store.transaction(|tx| {
        let mut meta = read_meta(tx, ns)?;
        let mut cache = Cache::new();
        insert_in_txn(tx, ns, p, &mut cache, &mut meta, doc_key, vector)?;
        flush_dirty(tx, ns, &mut cache)?;
        tx.put(ns, &[TAG_META], &encode_meta(meta))?;
        Ok(())
    })
}

/// Cap on the shared node cache during a bulk insert: keeps hot nodes (entry
/// point, upper layers) cached across inserts without unbounded growth. Dirty
/// nodes are already flushed, so clearing is safe (reloads from the txn).
const BULK_CACHE_CAP: usize = 50_000;

/// Insert many vectors in a single transaction (one commit, one fsync). Each
/// insert uses a fresh per-node cache and flushes its touched nodes before the
/// next, so within-batch graph connectivity works via read-your-writes while
/// memory stays bounded per insert. Far faster than repeated [`insert`] for
/// bulk loads.
pub(crate) fn insert_many(
    store: &Store,
    ns: &str,
    p: &DiskParams,
    items: &[(Vec<u8>, Vec<f32>)],
) -> Result<()> {
    store.transaction(|tx| {
        let mut meta = read_meta(tx, ns)?;
        let mut cache = Cache::new();
        for (doc_key, vector) in items {
            insert_in_txn(tx, ns, p, &mut cache, &mut meta, doc_key, vector)?;
            if cache.nodes.len() > BULK_CACHE_CAP {
                // Persist dirty nodes before evicting, then drop the resident
                // set (it reloads on demand).
                flush_dirty(tx, ns, &mut cache)?;
                cache.nodes.clear();
            }
        }
        flush_dirty(tx, ns, &mut cache)?;
        tx.put(ns, &[TAG_META], &encode_meta(meta))?;
        Ok(())
    })
}

fn read_meta(tx: &crate::store::WriteBatch<'_>, ns: &str) -> Result<Meta> {
    Ok(match tx.get(ns, &[TAG_META])? {
        Some(b) => decode_meta(&b),
        None => Meta {
            entry: None,
            count: 0,
        },
    })
}

/// Insert one vector within an open transaction, flushing its touched nodes
/// (so a following insert in the same transaction sees them) and updating
/// `meta` in place. The caller persists `meta` and commits.
fn insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    cache: &mut Cache,
    meta: &mut Meta,
    doc_key: &[u8],
    vector: &[f32],
) -> Result<()> {
    {
        // Overwrite: tombstone the previous node for this key.
        if let Some(old_bytes) = tx.get(ns, &keymap_key(doc_key))? {
            let old_id = u64::from_be_bytes(old_bytes.as_slice().try_into().unwrap_or([0; 8]));
            if load(tx, ns, cache, p, old_id)?.is_some()
                && let Some(node) = cache.nodes.get_mut(&old_id)
            {
                Rc::make_mut(node).deleted = true;
                cache.dirty.insert(old_id);
            }
        }

        let id = meta.count;
        meta.count += 1;
        let level = level_for(id, p.ml);
        cache.nodes.insert(
            id,
            Rc::new(Node {
                deleted: false,
                doc_key: doc_key.to_vec(),
                vector: p.encode(vector),
                layers: vec![Vec::new(); level + 1],
            }),
        );
        cache.dirty.insert(id);
        tx.put(ns, &keymap_key(doc_key), &id.to_be_bytes())?;

        if let Some(entry) = meta.entry {
            let top = load(tx, ns, cache, p, entry)?
                .map(|n| n.layers.len() - 1)
                .unwrap_or(0);

            // Greedy descent above the new node's level.
            let mut cur = entry;
            for layer in ((level + 1)..=top).rev() {
                let w = search_layer(tx, ns, p, cache, vector, &[cur], 1, layer)?;
                if let Some(c) = w.first() {
                    cur = c.id;
                }
            }

            // Connect on each layer at/below the new node's level.
            let start = level.min(top);
            for layer in (0..=start).rev() {
                let w = search_layer(tx, ns, p, cache, vector, &[cur], p.ef_construction, layer)?;
                let m_layer = p.neighbors_at(layer);
                let neighbors: Vec<u64> = w.iter().take(m_layer).map(|c| c.id).collect();

                if let Some(node) = cache.nodes.get_mut(&id) {
                    Rc::make_mut(node).layers[layer] = neighbors.clone();
                }
                for &nb in &neighbors {
                    if let Some(node) = cache.nodes.get_mut(&nb) {
                        let node = Rc::make_mut(node);
                        node.layers[layer].push(id);
                        let overflow = node.layers[layer].len() > m_layer;
                        cache.dirty.insert(nb);
                        if overflow {
                            prune(tx, ns, p, cache, nb, layer, m_layer)?;
                        }
                    }
                }
                if let Some(c) = w.first() {
                    cur = c.id;
                }
            }

            if level > top {
                meta.entry = Some(id);
            }
        } else {
            meta.entry = Some(id);
        }

        // Touched nodes stay dirty in the cache; the cache provides read-your-
        // writes for the rest of this transaction. They are flushed to the store
        // once at the end of the batch (or before the cache is evicted over its
        // cap) — so a hub node touched by many inserts is written once, not once
        // per insert.
    }
    Ok(())
}

/// Write every dirty node to the store and clear the dirty set (nodes stay
/// cached). Call before evicting the cache and at the end of a batch.
fn flush_dirty(tx: &mut crate::store::WriteBatch<'_>, ns: &str, cache: &mut Cache) -> Result<()> {
    let dirty: Vec<u64> = cache.dirty.drain().collect();
    for nid in dirty {
        if let Some(node) = cache.nodes.get(&nid) {
            tx.put(ns, &node_key(nid), &encode_node(node))?;
        }
    }
    Ok(())
}

/// Delete `doc_key` from the index (tombstone). Returns whether it existed.
pub(crate) fn delete(store: &Store, ns: &str, p: &DiskParams, doc_key: &[u8]) -> Result<bool> {
    store.transaction(|tx| {
        let Some(id_bytes) = tx.get(ns, &keymap_key(doc_key))? else {
            return Ok(false);
        };
        let id = u64::from_be_bytes(id_bytes.as_slice().try_into().unwrap_or([0; 8]));
        if let Some(bytes) = tx.get(ns, &node_key(id))?
            && let Some(mut node) = decode_node(&bytes, p)
        {
            node.deleted = true;
            tx.put(ns, &node_key(id), &encode_node(&node))?;
        }
        tx.delete(ns, &keymap_key(doc_key))?;
        Ok(true)
    })
}

/// Search for the `k` nearest live document keys to `query`.
pub(crate) fn search(
    store: &Store,
    ns: &str,
    p: &DiskParams,
    query: &[f32],
    k: usize,
    ef_search: usize,
) -> Result<Vec<(Vec<u8>, f32)>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    store.read(|r| {
        let meta = match r.get(ns, &[TAG_META])? {
            Some(b) => decode_meta(&b),
            None => return Ok(Vec::new()),
        };
        let Some(entry) = meta.entry else {
            return Ok(Vec::new());
        };
        let mut cache = Cache::new();
        let top = load_r(r, ns, &mut cache, p, entry)?
            .map(|n| n.layers.len() - 1)
            .unwrap_or(0);

        let mut cur = entry;
        for layer in (1..=top).rev() {
            let w = search_layer_r(r, ns, p, &mut cache, query, &[cur], 1, layer)?;
            if let Some(c) = w.first() {
                cur = c.id;
            }
        }
        // Over-fetch so tombstoned hits don't crowd out live ones.
        let want = ef_search.max(k) * 2;
        let w = search_layer_r(r, ns, p, &mut cache, query, &[cur], want, 0)?;

        let mut out = Vec::new();
        for c in w {
            if let Some(node) = cache.nodes.get(&c.id)
                && !node.deleted
            {
                out.push((node.doc_key.clone(), c.dist));
                if out.len() == k {
                    break;
                }
            }
        }
        Ok(out)
    })
}

/// The reserved collection name holding an on-disk index's graph.
pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__dann__{collection}__{field}")
}

/// Persist a PQ codebook in the index namespace.
pub(crate) fn store_codebook(store: &Store, ns: &str, pq: &Pq) -> Result<()> {
    store.put(ns, &[TAG_PQ], &pq.to_bytes())
}

/// Load a persisted PQ codebook from the index namespace, if present.
pub(crate) fn load_codebook(store: &Store, ns: &str) -> Result<Option<Pq>> {
    Ok(store.get(ns, &[TAG_PQ])?.and_then(|b| Pq::from_bytes(&b)))
}

// ---- internal graph algorithm (generic over read source) ----
//
// Two thin wrappers (write-txn `tx` and read-txn `r`) feed a shared core via a
// node-fetch closure, so the algorithm isn't duplicated.

fn load(
    tx: &crate::store::WriteBatch<'_>,
    ns: &str,
    cache: &mut Cache,
    p: &DiskParams,
    id: u64,
) -> Result<Option<Rc<Node>>> {
    if let Some(n) = cache.nodes.get(&id) {
        return Ok(Some(n.clone()));
    }
    match tx.get(ns, &node_key(id))? {
        Some(b) => match decode_node(&b, p) {
            Some(node) => {
                let rc = Rc::new(node);
                cache.nodes.insert(id, rc.clone());
                Ok(Some(rc))
            }
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn load_r(
    r: &crate::store::ReadBatch<'_>,
    ns: &str,
    cache: &mut Cache,
    p: &DiskParams,
    id: u64,
) -> Result<Option<Rc<Node>>> {
    if let Some(n) = cache.nodes.get(&id) {
        return Ok(Some(n.clone()));
    }
    match r.get(ns, &node_key(id))? {
        Some(b) => match decode_node(&b, p) {
            Some(node) => {
                let rc = Rc::new(node);
                cache.nodes.insert(id, rc.clone());
                Ok(Some(rc))
            }
            None => Ok(None),
        },
        None => Ok(None),
    }
}

macro_rules! search_layer_impl {
    ($name:ident, $src:ty, $loader:ident) => {
        #[allow(clippy::too_many_arguments)]
        fn $name(
            src: $src,
            ns: &str,
            p: &DiskParams,
            cache: &mut Cache,
            query: &[f32],
            entries: &[u64],
            ef: usize,
            layer: usize,
        ) -> Result<Vec<Cand>> {
            use std::cmp::Reverse;
            use std::collections::{BinaryHeap, HashSet};
            let probe = p.make_probe(query);
            let mut visited: HashSet<u64> = HashSet::new();
            let mut candidates: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
            let mut results: BinaryHeap<Cand> = BinaryHeap::new();

            for &ep in entries {
                if let Some(node) = $loader(src, ns, cache, p, ep)? {
                    let d = p.dist(&probe, &node.vector);
                    candidates.push(Reverse(Cand { dist: d, id: ep }));
                    results.push(Cand { dist: d, id: ep });
                    visited.insert(ep);
                }
            }

            while let Some(Reverse(c)) = candidates.pop() {
                if results.len() >= ef
                    && c.dist > results.peek().map(|w| w.dist).unwrap_or(f32::INFINITY)
                {
                    break;
                }
                let neighbors: Vec<u64> = match cache.nodes.get(&c.id) {
                    Some(n) if layer < n.layers.len() => n.layers[layer].clone(),
                    _ => Vec::new(),
                };
                for nb in neighbors {
                    if visited.insert(nb) {
                        if let Some(node) = $loader(src, ns, cache, p, nb)? {
                            let d = p.dist(&probe, &node.vector);
                            let worst = results.peek().map(|w| w.dist).unwrap_or(f32::INFINITY);
                            if results.len() < ef || d < worst {
                                candidates.push(Reverse(Cand { dist: d, id: nb }));
                                results.push(Cand { dist: d, id: nb });
                                if results.len() > ef {
                                    results.pop();
                                }
                            }
                        }
                    }
                }
            }

            let mut out: Vec<Cand> = results.into_vec();
            out.sort_unstable();
            Ok(out)
        }
    };
}

search_layer_impl!(search_layer, &crate::store::WriteBatch<'_>, load);
search_layer_impl!(search_layer_r, &crate::store::ReadBatch<'_>, load_r);

/// Keep only the `m` nearest neighbours of `node` on `layer` (in the cache).
fn prune(
    tx: &crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    cache: &mut Cache,
    node: u64,
    layer: usize,
    m: usize,
) -> Result<()> {
    let base = match cache.nodes.get(&node) {
        Some(n) => n.vector.clone(),
        None => return Ok(()),
    };
    let probe = p.probe_of(&base);
    let ns_ids: Vec<u64> = cache
        .nodes
        .get(&node)
        .map(|n| n.layers[layer].clone())
        .unwrap_or_default();
    let mut scored: Vec<(f32, u64)> = Vec::with_capacity(ns_ids.len());
    for nb in ns_ids {
        if let Some(other) = load(tx, ns, cache, p, nb)? {
            scored.push((p.dist(&probe, &other.vector), nb));
        }
    }
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.truncate(m);
    if let Some(n) = cache.nodes.get_mut(&node) {
        Rc::make_mut(n).layers[layer] = scored.into_iter().map(|(_, id)| id).collect();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn corpus(n: usize, dim: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0xDEAD_BEEF_CAFE_1234;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
        (0..n).map(|_| (0..dim).map(|_| next()).collect()).collect()
    }

    fn exact(data: &[Vec<f32>], q: &[f32], k: usize) -> Vec<usize> {
        let mut s: Vec<(usize, f32)> = data
            .iter()
            .enumerate()
            .map(|(i, v)| (i, Metric::L2.distance(q, v)))
            .collect();
        s.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        s.into_iter().take(k).map(|(i, _)| i).collect()
    }

    #[test]
    fn empty_search_is_empty() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        assert!(
            search(&store, "ix", &p, &[1.0, 2.0], 5, 50)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn single_then_found() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"a", &[1.0, 2.0, 3.0]).unwrap();
        let got = search(&store, "ix", &p, &[1.0, 2.0, 3.0], 1, 50).unwrap();
        assert_eq!(got[0].0, b"a".to_vec());
        assert_eq!(got[0].1, 0.0);
    }

    #[test]
    fn recall_against_exact() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        let data = corpus(400, 16);
        for (i, v) in data.iter().enumerate() {
            insert(&store, "ix", &p, format!("k{i}").as_bytes(), v).unwrap();
        }
        let k = 10;
        let mut total = 0.0;
        let queries = corpus(15, 16);
        for q in &queries {
            let approx: HashSet<usize> = search(&store, "ix", &p, q, k, 100)
                .unwrap()
                .into_iter()
                .map(|(key, _)| String::from_utf8(key).unwrap()[1..].parse().unwrap())
                .collect();
            let want: HashSet<usize> = exact(&data, q, k).into_iter().collect();
            total += approx.intersection(&want).count() as f64 / k as f64;
        }
        let recall = total / queries.len() as f64;
        assert!(recall >= 0.85, "on-disk recall {recall} too low");
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.db");
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        let data = corpus(150, 8);
        {
            let store = Store::open(&path).unwrap();
            for (i, v) in data.iter().enumerate() {
                insert(&store, "ix", &p, format!("k{i}").as_bytes(), v).unwrap();
            }
        }
        // Reopen: no rebuild, search uses the persisted graph.
        let store = Store::open(&path).unwrap();
        let got = search(&store, "ix", &p, &data[20], 1, 50).unwrap();
        assert_eq!(got[0].0, b"k20".to_vec());
    }

    #[test]
    fn delete_then_absent() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        let data = corpus(100, 8);
        for (i, v) in data.iter().enumerate() {
            insert(&store, "ix", &p, format!("k{i}").as_bytes(), v).unwrap();
        }
        assert!(delete(&store, "ix", &p, b"k20").unwrap());
        let got = search(&store, "ix", &p, &data[20], 5, 80).unwrap();
        assert!(!got.iter().any(|(key, _)| key == b"k20"));
    }

    /// Center a corpus around zero so sign bits vary (binary quantization is
    /// degenerate on all-positive data — every bit would be 1).
    fn centered(mut data: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        for v in &mut data {
            for x in v {
                *x -= 0.5;
            }
        }
        data
    }

    fn quantized_recall(quant: Quantization, metric: Metric, center: bool, floor: f64) {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(metric, quant, 16, 128);
        let mut data = corpus(400, 16);
        let mut queries = corpus(15, 16);
        if center {
            data = centered(data);
            queries = centered(queries);
        }
        for (i, v) in data.iter().enumerate() {
            insert(&store, "ix", &p, format!("k{i}").as_bytes(), v).unwrap();
        }
        let k = 10;
        let mut total = 0.0;
        for q in &queries {
            let approx: HashSet<usize> = search(&store, "ix", &p, q, k, 100)
                .unwrap()
                .into_iter()
                .map(|(key, _)| String::from_utf8(key).unwrap()[1..].parse().unwrap())
                .collect();
            let want: HashSet<usize> = exact(&data, q, k).into_iter().collect();
            total += approx.intersection(&want).count() as f64 / k as f64;
        }
        let recall = total / queries.len() as f64;
        assert!(
            recall >= floor,
            "on-disk {quant:?} recall {recall} < {floor}"
        );
    }

    #[test]
    fn scalar_quantized_ondisk_recall() {
        // Scalar quantization is near-lossless: recall stays high.
        quantized_recall(Quantization::Scalar, Metric::L2, false, 0.80);
    }

    #[test]
    fn binary_quantized_ondisk_finds_neighbours() {
        // Binary is lossy but on sign-varied data it still retrieves a
        // meaningful fraction of the true neighbours under cosine.
        quantized_recall(Quantization::Binary, Metric::Cosine, true, 0.30);
    }

    #[test]
    fn quantized_persists_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.db");
        let p = DiskParams::with_quant(Metric::L2, Quantization::Scalar, 16, 128);
        let data = corpus(120, 8);
        {
            let store = Store::open(&path).unwrap();
            for (i, v) in data.iter().enumerate() {
                insert(&store, "ix", &p, format!("k{i}").as_bytes(), v).unwrap();
            }
        }
        // Reopen: the quantized graph decodes with the same mode, no rebuild.
        let store = Store::open(&path).unwrap();
        let got = search(&store, "ix", &p, &data[20], 1, 50).unwrap();
        assert_eq!(got[0].0, b"k20".to_vec());
    }

    #[test]
    fn overwrite_updates_vector() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"x", &[0.0, 0.0]).unwrap();
        insert(&store, "ix", &p, b"far", &[10.0, 10.0]).unwrap();
        // Move x next to far; querying near far should surface both.
        insert(&store, "ix", &p, b"x", &[10.0, 10.0]).unwrap();
        let got = search(&store, "ix", &p, &[10.0, 10.0], 2, 50).unwrap();
        let keys: HashSet<Vec<u8>> = got.into_iter().map(|(k, _)| k).collect();
        assert!(keys.contains(b"x".as_slice()));
        assert!(keys.contains(b"far".as_slice()));
    }
}
