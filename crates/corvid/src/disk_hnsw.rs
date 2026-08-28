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
use crate::error::{Error, Result};
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
    /// If the table cannot be built (dimension mismatch), keep the query
    /// vector: `search` declines dimension-mismatched queries before any
    /// probe is built, so this arm is unreachable defense in depth — not a
    /// correct fallback for mismatched dimensions.
    fn pq_probe(pq: &Pq, metric: Metric, query: Vec<f32>) -> DProbe {
        match metric {
            Metric::L2 => match pq.l2_table(&query) {
                Some(table) => DProbe::PqAdc(table),
                None => DProbe::Pq(query),
            },
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
#[derive(Clone, Copy, PartialEq, Debug)]
struct Meta {
    entry: Option<u64>,
    count: u64,
    /// Tombstoned node count (audit B5): drives dead-fraction compaction
    /// (`dead * 2 > live`, checked on the write path in `index.rs`). Invariant:
    /// `live = count - dead` is the number of live nodes, because every path
    /// that tombstones an already-live node (delete, overwrite, dimension-
    /// mismatch re-insert) increments `dead`, and every path that assigns a
    /// node id also grows `count` (an overwrite does both, leaving `live`
    /// unchanged). Reset to 0 with the namespace (a fresh build).
    dead: u32,
    /// Dimension of indexed vectors, fixed by the first inserted vector.
    /// Inserts and queries of any other dimension are rejected/skipped so a
    /// mismatch can never silently produce truncated-distance garbage.
    dim: Option<u32>,
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

/// A `CorruptIndex` error naming the namespace and what was wrong with it
/// (audit C1: corrupt state errors loudly instead of silently degrading).
fn corrupt(ns: &str, what: &str) -> Error {
    Error::CorruptIndex {
        context: format!("{what} in index namespace '{ns}'"),
    }
}

/// Decode a keymap value into its node id. Only full 8-byte values were
/// ever written; anything else is corrupt and returns `None` so callers
/// skip the entry instead of decoding zeros and tombstoning node 0 —
/// whoever owns it (audit C13).
fn decode_keymap(b: &[u8]) -> Option<u64> {
    let arr: [u8; 8] = b.try_into().ok()?;
    Some(u64::from_be_bytes(arr))
}

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
    // Capacity clamps (audit C1): the counts are untrusted input, so each
    // reserve is bounded by the bytes actually remaining (a layer costs at
    // least its 4-byte count, a neighbour id exactly 8). A corrupt huge
    // count then fails the `take` below fast, instead of attempting a giant
    // allocation first; valid input never exceeds these bounds, so the
    // clamp only ever under-reserves for bytes that can't be there.
    let mut layers = Vec::with_capacity(n_layers.min(c.remaining() / 4));
    for _ in 0..n_layers {
        let cnt = c.u32()?;
        let mut layer = Vec::with_capacity(cnt.min(c.remaining() / 8));
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
    let mut out = Vec::with_capacity(26);
    out.push(m.entry.is_some() as u8);
    out.extend_from_slice(&m.entry.unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&m.count.to_be_bytes());
    // dim: 0x00 = unset, 0x01 + u32 = set.
    match m.dim {
        None => out.push(0),
        Some(d) => {
            out.push(1);
            out.extend_from_slice(&d.to_be_bytes());
        }
    }
    // dead (audit B5): 4 BE bytes appended after the legacy section.
    out.extend_from_slice(&m.dead.to_be_bytes());
    out
}

/// Decode a meta row. `None` iff the row is a shape no writer version ever
/// produced (audit C1: a present-but-malformed row must error loudly at the
/// callers, not decode as an empty index and silently serve no results
/// forever). Every documented legacy shape — 17 (pre-dim), 18 (dim unset),
/// 22 (legacy dim-set, or new dim-unset + dead) and 26 (dim-set + dead)
/// bytes — still decodes exactly as before.
fn decode_meta(b: &[u8]) -> Option<Meta> {
    // Every version wrote at least the 17-byte legacy section (flag +
    // entry + count); a present row shorter than that is corrupt, not
    // legacy.
    if b.len() < 17 {
        return None;
    }
    let has = b[0] != 0;
    let entry = u64::from_be_bytes(b[1..9].try_into().ok()?);
    let count = u64::from_be_bytes(b[9..17].try_into().ok()?);
    // Older namespaces (pre-dim) decode as unset, which preserves their
    // accept-all behavior until the first fresh write pins the dimension.
    let dim = match b.get(17) {
        Some(1) if b.len() >= 22 => Some(u32::from_be_bytes(b[18..22].try_into().ok()?)),
        _ => None,
    };
    // dead (audit B5) is length-disambiguated: legacy rows are exactly 17
    // (pre-dim), 18 (dim unset) or 22 (dim set) bytes and never carry it;
    // new rows append exactly 4 bytes after the dim section. The one length
    // collision — 22 — splits cleanly on byte 17: legacy dim-set rows hold
    // the dim flag `0x01` there, new dim-unset rows hold `0x00` (the unset
    // flag) with dead at 18..22.
    let dim_end = match b.get(17) {
        Some(1) if b.len() >= 22 => 22,
        // The flag promises a 4-byte dim that isn't there: never written.
        Some(&1) => return None,
        Some(_) => 18,
        None => 17,
    };
    // The only lengths any encoder ever produced are `dim_end` (legacy, no
    // dead) and `dim_end + 4` (current, with dead). Anything else was never
    // written — corrupt, not a legacy shape.
    if b.len() != dim_end && b.len() != dim_end + 4 {
        return None;
    }
    let dead = if b.len() == dim_end + 4 {
        u32::from_be_bytes(b[dim_end..dim_end + 4].try_into().ok()?)
    } else {
        0
    };
    Some(Meta {
        entry: has.then_some(entry),
        count,
        dead,
        dim,
    })
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
    /// Bytes not yet consumed (capacity clamps bound reserves against this).
    fn remaining(&self) -> usize {
        self.b.len() - self.pos
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

/// Insert (or overwrite) `doc_key`'s `vector` into the on-disk index `ns`
/// inside the caller's transaction, so graph state commits atomically with
/// the document that produced it (or with the backfill page's cursor
/// advance, when the atomic-creation driver calls it).
pub(crate) fn insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    doc_key: &[u8],
    vector: &[f32],
) -> Result<()> {
    let mut meta = read_meta(tx, ns)?;
    let mut cache = Cache::new();
    insert_node_in_txn(tx, ns, p, &mut cache, &mut meta, doc_key, vector)?;
    flush_dirty(tx, ns, &mut cache)?;
    tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    Ok(())
}

/// Insert a page of (doc_key, vector) pairs inside the caller's transaction
/// with ONE node cache and ONE meta read-modify-write for the whole page —
/// the atomicity of per-tx inserts at (near-)bulk speed.
pub(crate) fn insert_page_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    page: &[(Vec<u8>, Vec<f32>)],
) -> Result<()> {
    let mut meta = read_meta(tx, ns)?;
    let mut cache = Cache::new();
    for (doc_key, vector) in page {
        insert_node_in_txn(tx, ns, p, &mut cache, &mut meta, doc_key, vector)?;
    }
    flush_dirty(tx, ns, &mut cache)?;
    tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    Ok(())
}

/// Test seam: insert one vector in its own transaction.
#[cfg(test)]
pub(crate) fn insert(
    store: &Store,
    ns: &str,
    p: &DiskParams,
    doc_key: &[u8],
    vector: &[f32],
) -> Result<()> {
    store.transaction(|tx| insert_in_txn(tx, ns, p, doc_key, vector))
}

/// Test seam: tombstone one key in its own transaction.
#[cfg(test)]
pub(crate) fn delete(store: &Store, ns: &str, p: &DiskParams, doc_key: &[u8]) -> Result<bool> {
    store.transaction(|tx| delete_in_txn(tx, ns, p, doc_key))
}

fn read_meta(tx: &crate::store::WriteBatch<'_>, ns: &str) -> Result<Meta> {
    Ok(match tx.get(ns, &[TAG_META])? {
        Some(b) => decode_meta(&b).ok_or_else(|| corrupt(ns, "malformed meta row"))?,
        None => Meta {
            entry: None,
            count: 0,
            dead: 0,
            dim: None,
        },
    })
}

/// Insert one vector within an open transaction, flushing its touched nodes
/// (so a following insert in the same transaction sees them) and updating
/// `meta` in place. The caller persists `meta` and commits.
fn insert_node_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    cache: &mut Cache,
    meta: &mut Meta,
    doc_key: &[u8],
    vector: &[f32],
) -> Result<()> {
    // Dimension gate: the first vector pins the index's dimension; any other
    // dimension is not indexed (matching the exact-search paths, which skip
    // mismatched documents). If an existing key's vector changes shape, its
    // old node is tombstoned so nothing stale competes in searches.
    match meta.dim {
        None => meta.dim = Some(vector.len() as u32),
        Some(d) if d as usize != vector.len() => {
            delete_in_txn(tx, ns, p, doc_key)?;
            // delete_in_txn just did its own meta read-modify-write (dead
            // increment); re-sync so the caller's final meta put doesn't
            // clobber it with this stale copy.
            *meta = read_meta(tx, ns)?;
            return Ok(());
        }
        _ => {}
    }
    {
        // Overwrite: tombstone the previous node for this key. A keymap
        // value that isn't a full node id is corrupt (never written): skip
        // it entirely — decoding it as id 0 would tombstone whoever owns
        // node 0 (audit C13). The fresh keymap put below overwrites the
        // garbage row.
        if let Some(old_bytes) = tx.get(ns, &keymap_key(doc_key))?
            && let Some(old_id) = decode_keymap(&old_bytes)
        {
            if load(tx, ns, cache, p, old_id)?.is_some()
                && let Some(node) = cache.nodes.get_mut(&old_id)
            {
                Rc::make_mut(node).deleted = true;
                cache.dirty.insert(old_id);
            }
            // Audit B5: the old id stops being live (tombstoned above, or
            // its node row is already absent) — count it dead. The insert
            // below also grows `count`, so `live = count - dead` is
            // unchanged by the overwrite, as it must be.
            meta.dead = meta.dead.saturating_add(1);
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

/// Tombstone `doc_key` inside a caller's transaction. Increments the meta
/// dead counter (audit B5) so the write-path compaction trigger observes the
/// tombstone; callers that hold their own `Meta` copy across this call must
/// re-read it (see `insert_node_in_txn`'s dimension-mismatch arm).
pub(crate) fn delete_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    doc_key: &[u8],
) -> Result<bool> {
    let Some(id_bytes) = tx.get(ns, &keymap_key(doc_key))? else {
        return Ok(false);
    };
    // A keymap value that isn't a full node id is corrupt (never written):
    // drop the garbage row, touch no node — decoding it as id 0 would
    // tombstone whoever owns node 0 (audit C13) — and count nothing dead.
    let Some(id) = decode_keymap(&id_bytes) else {
        tx.delete(ns, &keymap_key(doc_key))?;
        return Ok(false);
    };
    if let Some(bytes) = tx.get(ns, &node_key(id))?
        && let Some(mut node) = decode_node(&bytes, p)
    {
        node.deleted = true;
        tx.put(ns, &node_key(id), &encode_node(&node))?;
    }
    tx.delete(ns, &keymap_key(doc_key))?;
    // The id stops being live whether or not its node row was readable, so
    // `dead` increments on every keymap hit (keeping `live = count - dead`
    // honest even against corrupt node rows). Saturating: `dead` can never
    // legitimately exceed `count` (u64), so a saturated counter only
    // over-triggers compaction, never under-reports.
    let mut meta = read_meta(tx, ns)?;
    meta.dead = meta.dead.saturating_add(1);
    tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    Ok(true)
}

/// Search for the `k` nearest live document keys to `query`, reading meta and
/// nodes from `reader`'s snapshot (audit B3: the candidate keys and the
/// caller's document fetches share one point in time). Returns `None`
/// when the index cannot honestly serve the query — a dimension mismatch —
/// so the caller falls back to an exact scan instead of getting garbage.
pub(crate) fn search(
    reader: &dyn crate::store::SnapshotReader,
    ns: &str,
    p: &DiskParams,
    query: &[f32],
    k: usize,
    ef_search: usize,
) -> Result<Option<DiskRanked>> {
    if k == 0 {
        return Ok(Some(Vec::new()));
    }
    let meta = match reader.get(ns, &[TAG_META])? {
        Some(b) => decode_meta(&b).ok_or_else(|| corrupt(ns, "malformed meta row"))?,
        None => return Ok(Some(Vec::new())),
    };
    // Dimension gate (unset on legacy namespaces → accept-all).
    if let Some(d) = meta.dim
        && d as usize != query.len()
    {
        return Ok(None);
    }
    // A PQ index can only serve queries matching its codebook dimension
    // (covers legacy namespaces whose meta.dim is unset).
    if let Some(pq) = &p.pq
        && pq.dim() != query.len()
    {
        return Ok(None);
    }
    let Some(entry) = meta.entry else {
        return Ok(Some(Vec::new()));
    };
    let mut cache = Cache::new();
    let top = load_r(reader, ns, &mut cache, p, entry)?
        .map(|n| n.layers.len() - 1)
        .unwrap_or(0);

    let mut cur = entry;
    for layer in (1..=top).rev() {
        let w = search_layer_r(reader, ns, p, &mut cache, query, &[cur], 1, layer)?;
        if let Some(c) = w.first() {
            cur = c.id;
        }
    }
    // Over-fetch so tombstoned hits don't crowd out live ones: DEAD-SCALED
    // (audit B5), mirroring the in-memory rule (`k + dead`). The frontier
    // holds `ef_search.max(k) + dead` entries, so even if every dead node
    // ranks ahead of the live ones, `k` live nodes remain among the
    // candidates — recall no longer decays as tombstones accumulate (the
    // compaction trigger instead bounds the cost this width adds).
    // Saturating: a pathological `dead` counter can never panic the search
    // (wave-5 deferred guard).
    let want = ef_search.max(k).saturating_add(meta.dead as usize);
    let w = search_layer_r(reader, ns, p, &mut cache, query, &[cur], want, 0)?;

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
    Ok(Some(out))
}

/// Ranked `(doc_key, distance)` results, nearest first.
pub(crate) type DiskRanked = Vec<(Vec<u8>, f32)>;

/// `(dead, live)` node counts for the on-disk index at `ns`, read from its
/// meta row (audit B5 compaction trigger input); `None` when the namespace
/// has no meta (never built, or freshly reset by a registration/compaction).
/// One point-get — cheap enough to call after every applied write.
pub(crate) fn dead_fraction(store: &Store, ns: &str) -> Result<Option<(u32, u64)>> {
    match store.get(ns, &[TAG_META])? {
        Some(b) => {
            let m = decode_meta(&b).ok_or_else(|| corrupt(ns, "malformed meta row"))?;
            Ok(Some((m.dead, m.count.saturating_sub(m.dead as u64))))
        }
        None => Ok(None),
    }
}

/// The reserved collection name holding an on-disk index's graph.
pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__dann__{collection}__{field}")
}

/// Persist a PQ codebook in the index namespace inside the caller's
/// transaction (audit A5: it must commit atomically with the def row and the
/// namespace reset that precedes it).
pub(crate) fn store_codebook_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    pq: &Pq,
) -> Result<()> {
    tx.put(ns, &[TAG_PQ], &pq.to_bytes())
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
            // Present but undecodable: corrupt state errors loudly (audit
            // C1) — treating it as an absent node silently hollowed
            // searches out (empty results forever).
            None => Err(corrupt(ns, &format!("malformed node row for id {id}"))),
        },
        None => Ok(None),
    }
}

fn load_r(
    r: &dyn crate::store::SnapshotReader,
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
            // See `load`: corrupt state errors, never reads as absent.
            None => Err(corrupt(ns, &format!("malformed node row for id {id}"))),
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
search_layer_impl!(search_layer_r, &dyn crate::store::SnapshotReader, load_r);

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
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn single_then_found() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"a", &[1.0, 2.0, 3.0]).unwrap();
        let got = search(&store, "ix", &p, &[1.0, 2.0, 3.0], 1, 50)
            .unwrap()
            .unwrap();
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
        let got = search(&store, "ix", &p, &data[20], 1, 50).unwrap().unwrap();
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
        let got = search(&store, "ix", &p, &data[20], 5, 80).unwrap().unwrap();
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
        let got = search(&store, "ix", &p, &data[20], 1, 50).unwrap().unwrap();
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
        let got = search(&store, "ix", &p, &[10.0, 10.0], 2, 50)
            .unwrap()
            .unwrap();
        let keys: HashSet<Vec<u8>> = got.into_iter().map(|(k, _)| k).collect();
        assert!(keys.contains(b"x".as_slice()));
        assert!(keys.contains(b"far".as_slice()));
    }

    #[test]
    fn meta_dead_round_trips_and_legacy_decodes_zero() {
        // New-format rows round-trip dead with and without a pinned dim.
        for m in [
            Meta {
                entry: Some(7),
                count: 9,
                dead: 4,
                dim: Some(3),
            },
            Meta {
                entry: Some(1),
                count: 5,
                dead: 2,
                dim: None,
            },
            Meta {
                entry: None,
                count: 0,
                dead: 0,
                dim: None,
            },
        ] {
            assert_eq!(decode_meta(&encode_meta(m)), Some(m));
        }
        // Legacy rows (pre-dead) decode dead = 0 with their fields intact.
        let mut legacy = Vec::new();
        legacy.push(1u8);
        legacy.extend_from_slice(&7u64.to_be_bytes());
        legacy.extend_from_slice(&9u64.to_be_bytes());
        let pre_dim = decode_meta(&legacy).unwrap(); // 17 bytes: no dim flag at all
        assert_eq!(
            pre_dim,
            Meta {
                entry: Some(7),
                count: 9,
                dead: 0,
                dim: None,
            }
        );
        legacy.push(1u8); // dim set
        legacy.extend_from_slice(&3u32.to_be_bytes());
        let dim_set = decode_meta(&legacy).unwrap(); // 22 bytes: legacy dim-set
        assert_eq!(dim_set.dim, Some(3));
        assert_eq!(dim_set.dead, 0);
        assert_eq!(dim_set.count, 9);
        // The 22-byte collision: a new dim-unset + dead row must not be
        // mistaken for a legacy dim-set row (byte 17 splits them).
        let mut new_unset = Vec::new();
        new_unset.push(1u8);
        new_unset.extend_from_slice(&1u64.to_be_bytes());
        new_unset.extend_from_slice(&6u64.to_be_bytes());
        new_unset.push(0u8); // dim unset flag
        new_unset.extend_from_slice(&5u32.to_be_bytes()); // dead
        let m = decode_meta(&new_unset).unwrap();
        assert_eq!(m.dim, None);
        assert_eq!(m.dead, 5);
        assert_eq!(m.count, 6);
    }

    /// Audit C1: `decode_meta` refuses rows no writer version ever produced;
    /// every form any encoder did write still decodes.
    #[test]
    fn decode_meta_rejects_never_written_shapes() {
        // Corrupt lengths (audit C1 targets these erroring, not degrading).
        assert!(decode_meta(&[0u8; 16]).is_none()); // truncated header
        assert!(decode_meta(&[1u8, 2, 3]).is_none());
        assert!(decode_meta(&[0u8; 19]).is_none()); // 18 + one stray byte
        assert!(decode_meta(&[0u8; 20]).is_none());
        assert!(decode_meta(&[0u8; 21]).is_none()); // dead without a dim flag
        assert!(decode_meta(&[0u8; 23]).is_none());
        assert!(decode_meta(&[0u8; 25]).is_none());
        assert!(decode_meta(&[0u8; 27]).is_none());
        // Flag byte promises a dim that isn't there.
        let mut dangling = vec![1u8];
        dangling.extend_from_slice(&7u64.to_be_bytes());
        dangling.extend_from_slice(&9u64.to_be_bytes());
        dangling.push(1);
        assert!(decode_meta(&dangling).is_none()); // 18 bytes, flag = 1
        // Every written form decodes: 17 (pre-dim), 18 (dim unset),
        // 22 (legacy dim-set or new dim-unset + dead), 26 (dim-set + dead).
        let base = |flag: Option<u8>| {
            let mut v = vec![1u8];
            v.extend_from_slice(&7u64.to_be_bytes());
            v.extend_from_slice(&9u64.to_be_bytes());
            if let Some(f) = flag {
                v.push(f);
            }
            v
        };
        assert!(decode_meta(&base(None)).is_some()); // 17
        assert!(decode_meta(&base(Some(0))).is_some()); // 18
        assert!(decode_meta(&base(Some(1))).is_none()); // 18 + flag 1: corrupt
        let mut v = base(Some(0));
        v.extend_from_slice(&5u32.to_be_bytes()); // dead → 22, new dim-unset
        assert!(decode_meta(&v).is_some());
        let mut v = base(Some(1));
        v.extend_from_slice(&3u32.to_be_bytes()); // dim → 22, legacy dim-set
        assert!(decode_meta(&v).is_some());
        v.extend_from_slice(&5u32.to_be_bytes()); // dead → 26, dim-set + dead
        assert!(decode_meta(&v).is_some());
    }

    /// A page insert produces the same graph state as per-doc inserts: build the
    /// same corpus into two namespaces — one via insert_page_in_txn, one via
    /// repeated insert_in_txn in a single transaction — and require identical
    /// search results (top-5 keys, both k-ordered).
    #[test]
    fn page_insert_matches_per_doc_state() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 8, 64);
        let items: Vec<(Vec<u8>, Vec<f32>)> = (0..50u8)
            .map(|i| (vec![i], vec![i as f32, 1.0, 0.0, 0.0]))
            .collect();
        store
            .transaction(|tx| insert_page_in_txn(tx, "ix", &p, &items))
            .unwrap();
        store
            .transaction(|tx| {
                for (k, v) in &items {
                    insert_in_txn(tx, "ix2", &p, k, v)?;
                }
                Ok(())
            })
            .unwrap();
        let a = search(&store, "ix", &p, &[0.0, 1.0, 0.0, 0.0], 5, 64)
            .unwrap()
            .unwrap();
        let b = search(&store, "ix2", &p, &[0.0, 1.0, 0.0, 0.0], 5, 64)
            .unwrap()
            .unwrap();
        let keys = |r: &[(Vec<u8>, f32)]| r.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&a), keys(&b));
    }

    /// Audit C1: a present-but-malformed node row must make `search` error
    /// loudly — previously the loader read the failed decode as an absent
    /// node and the search silently returned empty results forever.
    #[test]
    fn corrupt_node_row_errors_search() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"a", &[1.0, 2.0, 3.0]).unwrap();
        // Forge a truncated node row for id 0 (the first insert's node and
        // the graph's entry point).
        store.put("ix", &node_key(0), &[0u8, 0xFF, 0xFF]).unwrap();
        let res = search(&store, "ix", &p, &[1.0, 2.0, 3.0], 1, 50);
        assert!(
            matches!(res, Err(crate::Error::CorruptIndex { .. })),
            "corrupt node row must error loudly, got {res:?}"
        );
    }

    /// Audit C1: a present-but-malformed meta row must error the search
    /// instead of decoding as an empty index — the silent-empty failure
    /// mode. No writer version ever produced rows of these shapes.
    #[test]
    fn corrupt_meta_row_errors_search() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"a", &[1.0, 2.0, 3.0]).unwrap();
        // Truncated header (3 bytes).
        store.put("ix", &[TAG_META], &[1u8, 2, 3]).unwrap();
        let res = search(&store, "ix", &p, &[1.0, 2.0, 3.0], 1, 50);
        assert!(
            matches!(res, Err(crate::Error::CorruptIndex { .. })),
            "truncated meta row must error loudly, got {res:?}"
        );
        // 18 valid bytes plus one stray byte: never written by any encoder.
        store.put("ix", &[TAG_META], &[0u8; 19]).unwrap();
        let res = search(&store, "ix", &p, &[1.0, 2.0, 3.0], 1, 50);
        assert!(
            matches!(res, Err(crate::Error::CorruptIndex { .. })),
            "never-written meta length must error loudly, got {res:?}"
        );
    }

    /// Audit C1: forged huge counts in a node row must not drive a huge
    /// allocation — capacity is clamped against the bytes remaining, so the
    /// decode fails fast (and the loader reports corrupt state) instead of
    /// attempting a multi-gigabyte reserve first.
    #[test]
    fn decode_node_clamps_forged_huge_counts() {
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        let forged = |layers: u32, cnt: u32| {
            let mut b = vec![0u8, 1, 0, 0, 0, b'k']; // deleted, dk_len=1, "k"
            b.extend_from_slice(&0u32.to_le_bytes()); // vec_len = 0
            b.extend_from_slice(&layers.to_le_bytes()); // n_layers
            b.extend_from_slice(&cnt.to_le_bytes()); // first layer cnt
            b
        };
        assert!(decode_node(&forged(u32::MAX, u32::MAX), &p).is_none());
        assert!(decode_node(&forged(1, u32::MAX), &p).is_none());
    }

    /// Audit C13: a keymap value shorter than a node id is corrupt (never
    /// written). Deleting its key must no-op on the graph — not decode the
    /// short value as id 0 and tombstone whoever owns node 0.
    #[test]
    fn short_keymap_value_delete_does_not_tombstone_node_zero() {
        let store = Store::open_in_memory().unwrap();
        let p = DiskParams::with_quant(Metric::L2, Quantization::None, 16, 128);
        insert(&store, "ix", &p, b"a", &[1.0, 0.0]).unwrap(); // node 0
        insert(&store, "ix", &p, b"b", &[0.0, 1.0]).unwrap();
        // Forge a corrupt short keymap for a key that owns no node.
        store.put("ix", &keymap_key(b"zzz"), &[9, 9, 9]).unwrap();
        assert!(!delete(&store, "ix", &p, b"zzz").unwrap());
        // Node 0 ("a") was not tombstoned: still live and searchable.
        let got = search(&store, "ix", &p, &[1.0, 0.0], 1, 10)
            .unwrap()
            .unwrap();
        assert_eq!(got[0].0, b"a".to_vec());
        // The garbage keymap row itself is gone.
        assert!(store.get("ix", &keymap_key(b"zzz")).unwrap().is_none());
    }
}
