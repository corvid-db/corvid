//! Secondary scalar index: sub-linear equality and range filters.
//!
//! Without an index, a filtered query scans the whole collection and evaluates
//! the predicate on every document — O(N). A scalar index instead stores an
//! order-preserving encoding of one field's value as redb keys, so an equality
//! or range filter scans only the matching key range (plus the documents it
//! actually returns). The index is always on disk (memory bounded by the result
//! set) and persists across reopen.
//!
//! ## Encoding
//!
//! Each indexed value becomes a self-delimiting, order-preserving byte string:
//! a *lane* byte (so bools, numbers, and text occupy disjoint, ordered ranges)
//! then an order-preserving payload, then a terminator. Numbers (int and float)
//! share one lane keyed by the IEEE-754 total order of their `f64` value, which
//! matches the query layer's numeric comparison (`value_order` casts ints to
//! `f64`); because that cast is monotonic, a range scan never *excludes* a true
//! match — at worst it includes a few ties, which the caller re-checks against
//! the exact predicate.
//!
//! Layout in a reserved collection (`__scalar__<coll>__<field>`):
//! - `0x00 ‖ doc_key` → `encoded_value` (forward map, to remove the old entry).
//! - `encoded_value ‖ doc_key` → `[]` (the index entry; `encoded_value` starts
//!   with a lane byte ≥ 1, so it never collides with the forward map).

use crate::db::{Collection, Db};
use crate::error::Result;
use crate::filter::CmpOp;
use crate::store::SnapshotReader;
use crate::value::Value;

/// Reserved collection holding persisted scalar-index definitions.
pub(crate) const SCALAR_DEFS: &str = "__scalar_indexes__";

const FWD_TAG: u8 = 0x00;
const LANE_BOOL: u8 = 0x01;
const LANE_NUM: u8 = 0x02;
const LANE_TEXT: u8 = 0x03;

/// Reserved collection holding persisted compound-index definitions.
const COMPOUND_DEFS: &str = "__cscalar_indexes__";

/// Reserved collection holding compound-index **miss markers**: one row per
/// compound def that has ever observed (in backfill or maintenance) a
/// document leaving an indexed field missing/non-encodable. Presence means
/// `all_docs_indexed` must be false for that def — see [`CompoundDef`].
/// A separate namespace (not a def-key suffix) so the def scans never see
/// phantom definitions.
const COMPOUND_MISS: &str = "__cscalar_misses__";

/// Compound def kind bytes (the opaque bytes after the creation-state codec,
/// `index_build::encode_def`). Single byte:
/// - `Complete` + `[1]`: every document in the collection has ALL the
///   index's fields present and encodable (`all_docs_indexed`) — the
///   soundness precondition for prefix-only windows.
/// - `Complete` + anything else (`[0]`, `[2]`, empty, legacy): not all
///   indexed. `[2]` is the crash-window value: the backfill driver's own
///   final transaction writes it before the flag-aware completion rewrites
///   `[1]`/`[0]` below, so a crash between the two conservatively decodes
///   false.
/// - `Building` + `[2]`: this build cycle is **flag-aware** (registered by
///   the current code; miss markers are authoritative for the whole
///   corpus). A `Building` row with other kind bytes is a pre-flag legacy
///   cycle: its early pages counted no misses, so completion must not set
///   the flag.
const KIND_ALL_INDEXED: u8 = 1;
const KIND_FLAG_AWARE: u8 = 2;

/// The per-def compound state kept in memory.
#[derive(Default)]
pub(crate) struct CompoundDef {
    /// Whether the creation backfill is still in flight (an interrupted
    /// creation). A building index is never served; the first probe (or a
    /// re-creation) resumes the build.
    pub(crate) building: bool,
    /// Whether EVERY document in the collection has all this index's fields
    /// present and encodable — set at backfill completion iff no miss was
    /// ever observed (backfill page or maintenance write), and flipped
    /// false permanently on any miss write. `false` for building defs and
    /// all legacy defs. Recomputed by re-registration (a fresh cycle clears
    /// the miss markers and re-walks the corpus).
    pub(crate) all_docs_indexed: bool,
}

/// Per-database scalar-index registry.
#[derive(Default)]
pub(crate) struct ScalarState {
    /// Single-field indexes: `(collection, field)` → whether the index is
    /// still **building** (an interrupted creation). Maintenance iterates all
    /// defs; serviceability requires `building == false`.
    defs: std::collections::HashMap<(String, String), bool>,
    /// Compound indexes: `(collection, ordered field list)` → per-def state
    /// (building / all-docs-indexed). Maintenance iterates all defs;
    /// serviceability requires `building == false`.
    compound: std::collections::HashMap<(String, Vec<String>), CompoundDef>,
}

pub(crate) fn new_state() -> std::sync::Mutex<ScalarState> {
    std::sync::Mutex::new(ScalarState::default())
}

/// The reserved collection backing a single-field scalar index.
pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__scalar__{collection}__{field}")
}

/// The reserved collection backing a compound index over `fields`.
pub(crate) fn compound_namespace(collection: &str, fields: &[String]) -> String {
    format!("__cscalar__{collection}__{}", fields.join("\u{1}"))
}

// ---- order-preserving encoding ----

/// Encode a scalar value into an order-preserving, self-delimiting key payload.
/// Returns `None` for values that are not indexable scalars (null, containers,
/// vectors, bytes).
pub(crate) fn encode_value(v: &Value) -> Option<Vec<u8>> {
    let (lane, payload): (u8, Vec<u8>) = match v {
        Value::Bool(b) => (LANE_BOOL, vec![*b as u8]),
        Value::Int(i) => (LANE_NUM, num_payload(*i as f64)),
        Value::Float(f) => (LANE_NUM, num_payload(*f)),
        Value::Text(s) => (LANE_TEXT, s.as_bytes().to_vec()),
        _ => return None,
    };
    let mut out = Vec::with_capacity(1 + payload.len() + 2);
    out.push(lane);
    escape_into(&payload, &mut out);
    out.extend_from_slice(&[0x00, 0x00]); // terminator: sorts before escaped bytes
    Some(out)
}

/// IEEE-754 total-order encoding of an `f64` as 8 big-endian bytes.
///
/// `-0.0` is canonicalized to `+0.0`: the predicate layer treats the two as
/// equal (`-0.0 == 0.0`, and `partial_cmp` says `Equal`), so they must share
/// one index key — otherwise an equality/range window anchored at either
/// zero would silently exclude documents storing the other.
fn num_payload(f: f64) -> Vec<u8> {
    let f = if f == 0.0 { 0.0 } else { f };
    let bits = f.to_bits();
    // Flip all bits for negatives, just the sign bit for non-negatives, so the
    // unsigned big-endian order matches numeric order.
    let ordered = if bits & (1 << 63) != 0 {
        !bits
    } else {
        bits | (1 << 63)
    };
    ordered.to_be_bytes().to_vec()
}

/// Escape `0x00` as `0x00 0x01` so a `0x00 0x00` terminator is unambiguous and
/// lexicographic order is preserved.
fn escape_into(payload: &[u8], out: &mut Vec<u8>) {
    for &b in payload {
        if b == 0x00 {
            out.extend_from_slice(&[0x00, 0x01]);
        } else {
            out.push(b);
        }
    }
}

/// The lane byte for a value, for range scans within one type.
fn lane_of(v: &Value) -> Option<u8> {
    match v {
        Value::Bool(_) => Some(LANE_BOOL),
        Value::Int(_) | Value::Float(_) => Some(LANE_NUM),
        Value::Text(_) => Some(LANE_TEXT),
        _ => None,
    }
}

fn fwd_key(doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + doc_key.len());
    k.push(FWD_TAG);
    k.extend_from_slice(doc_key);
    k
}

// ---- maintenance ----

/// Index (or re-index) `doc_key`'s `value` inside a caller's transaction, so
/// the index entry commits atomically with the document that produced it.
pub(crate) fn insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
    value: &Value,
) -> Result<()> {
    remove_in_txn(tx, ns, doc_key)?;
    if let Some(enc) = encode_value(value) {
        let mut idx_key = enc.clone();
        idx_key.extend_from_slice(doc_key);
        tx.put(ns, &idx_key, &[])?;
        tx.put(ns, &fwd_key(doc_key), &enc)?;
    }
    Ok(())
}

fn remove_in_txn(tx: &mut crate::store::WriteBatch<'_>, ns: &str, doc_key: &[u8]) -> Result<()> {
    if let Some(enc) = tx.get(ns, &fwd_key(doc_key))? {
        let mut idx_key = enc;
        idx_key.extend_from_slice(doc_key);
        tx.delete(ns, &idx_key)?;
        tx.delete(ns, &fwd_key(doc_key))?;
    }
    Ok(())
}

// ---- candidate scans ----

const PAGE: usize = 4096;

/// A single comparison constraint on the indexed field.
pub(crate) struct Constraint<'v> {
    pub op: CmpOp,
    pub value: &'v Value,
}

/// The value-portion bound of an index key (`lane ‖ escaped_payload`, no
/// terminator) — what range comparisons are made against.
fn bound_bytes(value: &Value) -> Option<Vec<u8>> {
    let enc = encode_value(value)?;
    Some(enc[..enc.len() - 2].to_vec()) // strip the 0x00 0x00 terminator
}

/// Combine the constraints into a lane and an inclusive `[lower, upper]`
/// value-bound window (bucket-level; the caller verifies exact strictness).
/// Returns `None` if the constraints don't pin a single lane or aren't
/// range-serviceable.
fn window(constraints: &[Constraint<'_>]) -> Option<(u8, Vec<u8>, Option<Vec<u8>>)> {
    let mut lane: Option<u8> = None;
    let mut lower: Option<Vec<u8>> = None;
    let mut upper: Option<Vec<u8>> = None;

    for c in constraints {
        let l = lane_of(c.value)?;
        match lane {
            Some(existing) if existing != l => return None, // mixed types
            _ => lane = Some(l),
        }
        let b = bound_bytes(c.value)?;
        match c.op {
            CmpOp::Eq => {
                lower = Some(max_opt(lower, b.clone()));
                upper = Some(min_opt(upper, b));
            }
            CmpOp::Ge | CmpOp::Gt => lower = Some(max_opt(lower, b)),
            CmpOp::Le | CmpOp::Lt => upper = Some(min_opt(upper, b)),
            CmpOp::Ne => return None,
        }
    }
    let lane = lane?;
    // Lower defaults to the start of the lane.
    let lower = lower.unwrap_or_else(|| vec![lane]);
    Some((lane, lower, upper))
}

fn max_opt(cur: Option<Vec<u8>>, b: Vec<u8>) -> Vec<u8> {
    match cur {
        Some(c) if c >= b => c,
        _ => b,
    }
}

fn min_opt(cur: Option<Vec<u8>>, b: Vec<u8>) -> Vec<u8> {
    match cur {
        Some(c) if c <= b => c,
        _ => b,
    }
}

/// Scan the `[lower, upper]` window within `lane` on `reader`'s snapshot,
/// returning candidate doc keys (a verified superset). Stops and returns
/// `None` if the candidate count would exceed `cap` — the caller then falls
/// back to a bounded scan, so a low-selectivity filter never materialises an
/// unbounded set in memory.
fn window_candidates(
    reader: &dyn SnapshotReader,
    ns: &str,
    lane: u8,
    lower: &[u8],
    upper: Option<&[u8]>,
    cap: usize,
) -> Result<Option<Vec<Vec<u8>>>> {
    let lane_end = [lane + 1];
    let mut out = Vec::new();
    let mut cursor = lower.to_vec();
    loop {
        let page = reader.scan_from(ns, &cursor, PAGE)?;
        if page.is_empty() {
            break;
        }
        let mut stop = false;
        for (key, _) in &page {
            if key.as_slice() >= lane_end.as_slice() || key[0] != lane {
                stop = true;
                break;
            }
            if let Some(up) = upper
                && entry_exceeds(key, up)
            {
                stop = true;
                break;
            }
            if let Some(doc_key) = doc_key_of(key) {
                out.push(doc_key);
                if out.len() > cap {
                    return Ok(None); // not selective enough — fall back to scan
                }
            }
        }
        if stop {
            break;
        }
        cursor = next_after(&page.last().unwrap().0);
    }
    Ok(Some(out))
}

/// Whether an index `key`'s value portion sorts strictly after `bound`
/// (lane+escaped payload, no terminator).
fn entry_exceeds(key: &[u8], bound: &[u8]) -> bool {
    match terminator_pos(key) {
        Some(end) => key[..end] > *bound,
        None => false,
    }
}

/// Index keys are `lane ‖ escaped_value ‖ 0x00 0x00 ‖ doc_key`. Find the start
/// of the `0x00 0x00` terminator.
fn terminator_pos(key: &[u8]) -> Option<usize> {
    let mut i = 1; // skip lane byte
    while i + 1 < key.len() {
        if key[i] == 0x00 {
            if key[i + 1] == 0x00 {
                return Some(i);
            }
            i += 2; // escaped 0x00 0x01
        } else {
            i += 1;
        }
    }
    None
}

fn doc_key_of(key: &[u8]) -> Option<Vec<u8>> {
    terminator_pos(key).map(|t| key[t + 2..].to_vec())
}

/// Decode a numeric-lane index key's value portion back to its `f64` (the
/// inverse of `num_payload`'s order transform, unescaping applied). `None`
/// on a malformed key.
fn num_value_of(key: &[u8]) -> Option<f64> {
    let t = terminator_pos(key)?;
    let mut payload: Vec<u8> = Vec::with_capacity(8);
    let mut i = 1; // skip the lane byte
    while i < t {
        if key[i] == 0x00 {
            // Inside the value portion a 0x00 is always an escaped zero
            // (0x00 0x01) — the raw terminator ends the portion at `t`.
            i += 2;
            payload.push(0x00);
        } else {
            payload.push(key[i]);
            i += 1;
        }
    }
    let bits = u64::from_be_bytes(payload.try_into().ok()?);
    let orig = if bits & (1 << 63) != 0 {
        bits & !(1 << 63) // was non-negative: only the sign bit was set
    } else {
        !bits // was negative: all bits were flipped
    };
    Some(f64::from_bits(orig))
}

/// Stream the index's COMPARABLE entries — every document whose field value
/// is `Int`/`Float` (numeric lane, IEEE-754 total order, `-0.0` and `+0.0`
/// sharing one key) or `Text` (text lane, lexical) — in ascending value
/// order: numeric lane first, then text lane (the ordering contract's
/// class-0 order: numbers before texts, each lane in value order, ties by
/// doc key — exactly what the `(lane ‖ payload ‖ terminator ‖ doc_key)` key
/// layout sorts to). `f` receives each entry's `(value_bytes, doc_key)`
/// (value bytes = the key up to the terminator); returning `Ok(false)`
/// stops the walk. Entries whose numeric payload decodes to NaN are
/// SKIPPED: NaN is indexed (it is a float) but incomparable, so it belongs
/// to the post-comparable tail of the order, not the walk. The forward map
/// (`0x00`-prefixed) and the bool lane sort below the numeric lane and are
/// never visited. A key with no value terminator is malformed (unreachable
/// via the encoder) and errors [`crate::Error::CorruptIndex`] — the
/// corruption philosophy is to fail loudly, never silently skip rows the
/// walk's consumer (the order-index path) would then mis-order.
pub(crate) fn comparable_entries(
    reader: &dyn SnapshotReader,
    ns: &str,
    mut f: impl FnMut(&[u8], &[u8]) -> Result<bool>,
) -> Result<()> {
    let mut cursor = vec![LANE_NUM];
    loop {
        let page = reader.scan_from(ns, &cursor, PAGE)?;
        if page.is_empty() {
            break;
        }
        let mut stop = false;
        for (key, _) in &page {
            if key[0] > LANE_TEXT {
                stop = true; // past the text lane: nothing comparable remains
                break;
            }
            let Some(t) = terminator_pos(key) else {
                return Err(crate::Error::CorruptIndex {
                    context: format!(
                        "index key without a value terminator in index namespace '{ns}': {key:02x?}"
                    ),
                });
            };
            if key[0] == LANE_NUM && num_value_of(key).is_some_and(f64::is_nan) {
                continue; // NaN: incomparable (class 1) — tail, not walk
            }
            if !f(&key[..t], &key[t + 2..])? {
                return Ok(());
            }
        }
        if stop {
            break;
        }
        cursor = next_after(&page.last().unwrap().0);
    }
    Ok(())
}

/// The smallest key strictly greater than `key`.
fn next_after(key: &[u8]) -> Vec<u8> {
    let mut k = key.to_vec();
    k.push(0);
    k
}

// ---- compound (multi-field) index ----

/// Encode an ordered tuple of values into one composite key prefix. Each value
/// uses the same self-delimiting encoding, so concatenation stays
/// order-preserving and parseable. `None` if any value is non-indexable.
fn encode_tuple(values: &[&Value]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        out.extend_from_slice(&encode_value(v)?);
    }
    Some(out)
}

/// Skip `n` self-delimiting value encodings from the start of `key`; returns the
/// offset where the next field (or the doc key) begins.
fn skip_values(key: &[u8], n: usize) -> Option<usize> {
    let mut pos = 0;
    for _ in 0..n {
        let rel = terminator_pos(&key[pos..])?;
        pos += rel + 2; // value bytes + 0x00 0x00 terminator
    }
    Some(pos)
}

/// Index `doc_key`'s tuple of field values (composite). Missing/non-indexable
/// any field → the document is simply not in this index.
fn compound_insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
    values: &[&Value],
) -> Result<()> {
    compound_remove_in_txn(tx, ns, doc_key)?;
    if let Some(enc) = encode_tuple(values) {
        let mut idx_key = enc.clone();
        idx_key.extend_from_slice(doc_key);
        tx.put(ns, &idx_key, &[])?;
        tx.put(ns, &fwd_key(doc_key), &enc)?;
    }
    Ok(())
}

fn compound_remove_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
) -> Result<()> {
    if let Some(enc) = tx.get(ns, &fwd_key(doc_key))? {
        let mut idx_key = enc;
        idx_key.extend_from_slice(doc_key);
        tx.delete(ns, &idx_key)?;
        tx.delete(ns, &fwd_key(doc_key))?;
    }
    Ok(())
}

/// Scan a compound index on `reader`'s snapshot: a fixed equality `prefix`
/// over the leading fields, then an optional range `window` over the next
/// field. Returns a verified superset of doc keys, or `None` if it would
/// exceed `cap`. `n_fields` is the index arity (to locate the doc key past
/// all encoded values).
fn compound_candidates(
    reader: &dyn SnapshotReader,
    ns: &str,
    prefix: &[u8],
    tail: Option<(Vec<u8>, Option<Vec<u8>>)>,
    n_fields: usize,
    cap: usize,
) -> Result<Option<Vec<Vec<u8>>>> {
    // Start at prefix (+ tail lower bound); a missing lower bound starts right
    // after the prefix.
    let mut start = prefix.to_vec();
    let upper = match &tail {
        Some((lower, upper)) => {
            start.extend_from_slice(lower);
            upper.clone()
        }
        None => None,
    };

    let mut out = Vec::new();
    let mut cursor = start;
    loop {
        let page = reader.scan_from(ns, &cursor, PAGE)?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for (key, _) in &page {
            if !key.starts_with(prefix) {
                advanced = false;
                break;
            }
            // Enforce the tail upper bound on the next field's value portion.
            if let Some(up) = &upper {
                let rest = &key[prefix.len()..];
                match terminator_pos(rest) {
                    Some(end) if &rest[..end] > up.as_slice() => {
                        advanced = false;
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(doc_start) = skip_values(key, n_fields) {
                out.push(key[doc_start..].to_vec());
                if out.len() > cap {
                    return Ok(None);
                }
            }
            advanced = true;
        }
        if !advanced {
            break;
        }
        cursor = next_after(&page.last().unwrap().0);
    }
    Ok(Some(out))
}

impl Db {
    /// Load persisted scalar-index definitions. Called once on open. Legacy
    /// rows without state bytes decode as `Complete`; a `Building` row marks
    /// the index for lazy resume on first use.
    pub(crate) fn load_scalar_defs(&self) -> Result<()> {
        let mut state = self.scalar().lock().expect("scalar lock");
        for (key, value) in self.store().scan(SCALAR_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                // Kind bytes are unused for scalar defs (empty).
                let (_, st) = crate::index_build::decode_def(&value);
                state.defs.insert(
                    def,
                    matches!(st, crate::index_build::DefState::Building { .. }),
                );
            }
        }
        Ok(())
    }

    /// Register (or replace) a scalar index on `field` for `collection`: the
    /// def row becomes `Building` (empty cursor) so a crash between
    /// registration and backfill completion leaves a never-served,
    /// resumable state. An in-flight `Building` row keeps its cursor, so a
    /// re-registration resumes the interrupted backfill instead of rescanning.
    pub(crate) fn register_scalar_index(&self, collection: &str, field: &str) -> Result<()> {
        let key = def_key(collection, field);
        let in_flight =
            crate::index_build::read_building_cursor(self.store(), SCALAR_DEFS, &key)?.is_some();
        if !in_flight {
            self.store().put(
                SCALAR_DEFS,
                &key,
                &crate::index_build::encode_def(
                    &[],
                    &crate::index_build::DefState::Building { cursor: vec![] },
                ),
            )?;
        }
        let mut state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .insert((collection.to_owned(), field.to_owned()), true);
        Ok(())
    }

    /// Maintain every scalar index on `collection` after a document write.
    /// Maintain every scalar index on `collection` inside the caller's write
    /// transaction, so index entries commit atomically with the document.
    pub(crate) fn scalar_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        let fields = self.scalar_fields(collection);
        for field in fields {
            let ns = namespace(collection, &field);
            match doc.get_path(&field) {
                Some(value) => insert_in_txn(tx, &ns, key, value)?,
                None => remove_in_txn(tx, &ns, key)?,
            }
        }
        Ok(())
    }

    /// Remove `key` from every scalar index on `collection` inside the
    /// caller's write transaction.
    pub(crate) fn scalar_on_delete_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        for field in self.scalar_fields(collection) {
            remove_in_txn(tx, &namespace(collection, &field), key)?;
        }
        Ok(())
    }

    /// Every field of `collection` with a scalar index, building or complete —
    /// maintenance must keep all of them current so a resumed backfill and
    /// concurrent writes overlap safely (idempotent upserts).
    fn scalar_fields(&self, collection: &str) -> Vec<String> {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .keys()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Whether `field` of `collection` has a **complete** scalar index. A
    /// building index is never serviceable: unique checks and query probes
    /// conservatively fall back (the first probe resumes the build).
    pub(crate) fn has_scalar_index(&self, collection: &str, field: &str) -> bool {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .get(&(collection.to_owned(), field.to_owned()))
            .is_some_and(|building| !*building)
    }

    /// Whether `field` of `collection` has a scalar-index DEFINITION at all
    /// (complete or building) — the static "this query may consult the
    /// index, so `run` must offer the build a resume first" gate for the
    /// order-index arm (builder.rs), which unlike filtered probes has no
    /// filter to trigger the existing resume condition.
    pub(crate) fn has_scalar_index_def(&self, collection: &str, field: &str) -> bool {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .contains_key(&(collection.to_owned(), field.to_owned()))
    }

    /// Flip a scalar index's in-memory def to complete after its backfill
    /// committed `Complete` on disk.
    pub(crate) fn mark_scalar_complete(&self, collection: &str, field: &str) {
        let mut state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .insert((collection.to_owned(), field.to_owned()), false);
    }

    /// Building scalar defs of `collection` as `(field, cursor)` jobs, read
    /// from the def rows (disk is the resume truth after a crash).
    pub(crate) fn collect_building_scalar(
        &self,
        collection: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut jobs = Vec::new();
        for (key, value) in self.store().scan(SCALAR_DEFS)? {
            let Some((coll, field)) = split_def_key(&key) else {
                continue;
            };
            if coll != collection {
                continue;
            }
            if let crate::index_build::DefState::Building { cursor } =
                crate::index_build::decode_def(&value).1
            {
                jobs.push((field, cursor));
            }
        }
        Ok(jobs)
    }

    /// (Re-)run the atomic backfill for one scalar index from `cursor`, then
    /// mark it complete — the exact driver invocation `create_scalar_index`
    /// uses, shared with lazy resumes.
    pub(crate) fn resume_scalar(&self, collection: &str, field: &str, cursor: &[u8]) -> Result<()> {
        let ns = namespace(collection, field);
        let kb: Vec<u8> = Vec::new();
        crate::index_build::run_atomic_backfill(
            self.store(),
            collection,
            SCALAR_DEFS,
            &def_key(collection, field),
            &kb,
            cursor,
            &mut |tx, page| {
                for (key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    if let Some(value) = doc.get_path(field) {
                        insert_in_txn(tx, &ns, key, value)?;
                    }
                }
                Ok(())
            },
        )?;
        self.mark_scalar_complete(collection, field);
        Ok(())
    }

    /// If `field` has a scalar index, return a *superset* of doc keys matching
    /// every `constraint` (the caller must verify with the exact predicate),
    /// reading the index window from `reader` — one snapshot for the candidate
    /// set and the verification that follows (audit B3).
    ///
    /// `None` when the field is not indexed, the constraints aren't
    /// range-serviceable, or the candidate set would exceed `cap` (in which
    /// case a full scan is the better — and bounded — plan). Interrupted
    /// builds are NOT resumed here: resuming writes, so it must happen before
    /// the caller's snapshot opens (the query entry points do).
    pub(crate) fn scalar_candidates(
        &self,
        collection: &str,
        field: &str,
        constraints: &[Constraint<'_>],
        cap: usize,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_scalar_index(collection, field) {
            return Ok(None);
        }
        let Some((lane, lower, upper)) = window(constraints) else {
            return Ok(None);
        };
        let ns = namespace(collection, field);
        window_candidates(reader, &ns, lane, &lower, upper.as_deref(), cap)
    }

    /// Doc keys whose indexed text value at `field` starts with `prefix` (a
    /// verified superset), read from `reader`'s snapshot (audit B3). `None` if
    /// not indexed or over `cap`. Resume discipline as
    /// [`Db::scalar_candidates`]: the caller resumes before its snapshot.
    pub(crate) fn scalar_prefix_candidates(
        &self,
        collection: &str,
        field: &str,
        prefix: &str,
        cap: usize,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_scalar_index(collection, field) {
            return Ok(None);
        }
        let mut pbytes = vec![LANE_TEXT];
        escape_into(prefix.as_bytes(), &mut pbytes);
        let ns = namespace(collection, field);
        let mut out = Vec::new();
        let mut cursor = pbytes.clone();
        loop {
            let page = reader.scan_from(&ns, &cursor, PAGE)?;
            if page.is_empty() {
                break;
            }
            let mut advanced = false;
            for (key, _) in &page {
                if !key.starts_with(&pbytes) {
                    advanced = false;
                    break;
                }
                if let Some(dk) = doc_key_of(key) {
                    out.push(dk);
                    if out.len() > cap {
                        return Ok(None);
                    }
                }
                advanced = true;
            }
            if !advanced {
                break;
            }
            cursor = next_after(&page.last().unwrap().0);
        }
        Ok(Some(out))
    }

    /// Load persisted compound-index definitions. Called once on open. Legacy
    /// rows without state bytes decode as `Complete`; a `Building` row marks
    /// the index for lazy resume on first use. The `all_docs_indexed` flag
    /// decodes from the def row's kind byte — legacy/absent bytes read as
    /// false (backward compatible: a pre-flag database declines prefix-only
    /// windows until its indexes are re-created).
    pub(crate) fn load_compound_defs(&self) -> Result<()> {
        let mut state = self.scalar().lock().expect("scalar lock");
        for (key, value) in self.store().scan(COMPOUND_DEFS)? {
            if let Some(def) = split_compound_def_key(&key) {
                let (kind, st) = crate::index_build::decode_def(&value);
                state.compound.insert(
                    def,
                    CompoundDef {
                        building: matches!(st, crate::index_build::DefState::Building { .. }),
                        all_docs_indexed: matches!(st, crate::index_build::DefState::Complete)
                            && kind.first() == Some(&KIND_ALL_INDEXED),
                    },
                );
            }
        }
        Ok(())
    }

    /// Register (or replace) a compound index over `fields` for `collection`:
    /// the def row becomes `Building` (empty cursor, kind `KIND_FLAG_AWARE`
    /// — a flag-aware cycle) so a crash between registration and backfill
    /// completion leaves a never-served, resumable state. An in-flight
    /// `Building` row keeps its cursor, so a re-registration resumes the
    /// interrupted backfill instead of rescanning. A FRESH cycle (complete
    /// def or none) also clears the def's miss marker in the same
    /// transaction: a re-registration recomputes `all_docs_indexed` from
    /// scratch, so miss events from previous cycles must not outlive it.
    pub(crate) fn register_compound_index(
        &self,
        collection: &str,
        fields: &[String],
    ) -> Result<()> {
        let key = compound_def_key(collection, fields);
        let in_flight =
            crate::index_build::read_building_cursor(self.store(), COMPOUND_DEFS, &key)?.is_some();
        if !in_flight {
            self.store().transaction(|tx| {
                tx.put(
                    COMPOUND_DEFS,
                    &key,
                    &crate::index_build::encode_def(
                        &[KIND_FLAG_AWARE],
                        &crate::index_build::DefState::Building { cursor: vec![] },
                    ),
                )?;
                tx.delete(COMPOUND_MISS, &key)?;
                Ok(())
            })?;
        }
        let mut state = self.scalar().lock().expect("scalar lock");
        state.compound.insert(
            (collection.to_owned(), fields.to_vec()),
            CompoundDef {
                building: true,
                all_docs_indexed: false,
            },
        );
        Ok(())
    }

    /// All single-field scalar index definitions (for dump/migrate). State is
    /// intentionally dropped: dump/load replays creation, materializing each
    /// def as `Complete`.
    pub(crate) fn scalar_specs(&self) -> Vec<(String, String)> {
        let state = self.scalar().lock().expect("scalar lock");
        state.defs.keys().cloned().collect()
    }

    /// All compound index definitions (for dump/migrate). State is
    /// intentionally dropped: dump/load replays creation, materializing each
    /// def as `Complete`.
    pub(crate) fn compound_specs(&self) -> Vec<(String, Vec<String>)> {
        let state = self.scalar().lock().expect("scalar lock");
        state.compound.keys().cloned().collect()
    }

    /// Compound indexes registered and **complete** on `collection` (ordered
    /// field lists). A building index is never serviceable: query probes
    /// conservatively fall back (the first probe resumes the build).
    pub(crate) fn compound_indexes(&self, collection: &str) -> Vec<Vec<String>> {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .compound
            .iter()
            .filter(|((c, _), d)| c == collection && !d.building)
            .map(|((_, f), _)| f.clone())
            .collect()
    }

    /// Every compound index of `collection`, building or complete —
    /// maintenance must keep all of them current so a resumed backfill and
    /// concurrent writes overlap safely (idempotent upserts).
    fn compound_fields(&self, collection: &str) -> Vec<Vec<String>> {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .compound
            .iter()
            .filter(|((c, _), _)| c == collection)
            .map(|((_, f), _)| f.clone())
            .collect()
    }

    /// Whether `fields` of `collection` has a **complete** compound index.
    pub(crate) fn has_compound_index(&self, collection: &str, fields: &[String]) -> bool {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .compound
            .get(&(collection.to_owned(), fields.to_vec()))
            .is_some_and(|d| !d.building)
    }

    /// Whether `fields` of `collection` has a complete compound index whose
    /// `all_docs_indexed` flag is true — the soundness precondition for
    /// prefix-only windows (see [`CompoundDef`]).
    pub(crate) fn compound_all_docs_indexed(&self, collection: &str, fields: &[String]) -> bool {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .compound
            .get(&(collection.to_owned(), fields.to_vec()))
            .is_some_and(|d| !d.building && d.all_docs_indexed)
    }

    /// Flip a compound index's in-memory def to complete after its backfill
    /// committed `Complete` on disk, with the computed `all_docs_indexed`
    /// flag.
    pub(crate) fn mark_compound_complete(
        &self,
        collection: &str,
        fields: &[String],
        all_indexed: bool,
    ) {
        let mut state = self.scalar().lock().expect("scalar lock");
        state.compound.insert(
            (collection.to_owned(), fields.to_vec()),
            CompoundDef {
                building: false,
                all_docs_indexed: all_indexed,
            },
        );
    }

    /// Flip a compound index's in-memory `all_docs_indexed` to false — a
    /// document write just left an indexed field missing/non-encodable, so
    /// the def is not in the index's coverage from now on (permanent until
    /// re-registration recomputes it).
    fn compound_miss_in_memory(&self, collection: &str, fields: &[String]) {
        let mut state = self.scalar().lock().expect("scalar lock");
        if let Some(d) = state
            .compound
            .get_mut(&(collection.to_owned(), fields.to_vec()))
        {
            d.all_docs_indexed = false;
        }
    }

    /// Building compound defs of `collection` as `(fields, cursor)` jobs,
    /// read from the def rows (disk is the resume truth after a crash).
    pub(crate) fn collect_building_compound(
        &self,
        collection: &str,
    ) -> Result<Vec<(Vec<String>, Vec<u8>)>> {
        let mut jobs = Vec::new();
        for (key, value) in self.store().scan(COMPOUND_DEFS)? {
            let Some((coll, fields)) = split_compound_def_key(&key) else {
                continue;
            };
            if coll != collection {
                continue;
            }
            if let crate::index_build::DefState::Building { cursor } =
                crate::index_build::decode_def(&value).1
            {
                jobs.push((fields, cursor));
            }
        }
        Ok(jobs)
    }

    /// (Re-)run the atomic backfill for one compound index from `cursor`,
    /// then complete the def — the exact driver invocation
    /// `create_compound_index` uses, shared with lazy resumes.
    ///
    /// `all_docs_indexed` completion: every page that declines to index a
    /// document (a field missing/non-encodable) commits a miss marker for
    /// this def in the SAME transaction as the page, so a crash mid-backfill
    /// never loses a miss. After the driver commits its `Complete` row
    /// (kind `[KIND_FLAG_AWARE]`, which conservatively decodes as
    /// not-all-indexed), one final transaction computes the flag —
    /// `flag-aware cycle AND no miss marker` — reading the marker and
    /// writing the def in ONE transaction: a concurrent maintenance miss
    /// either commits before it (marker seen → false) or after it (flips
    /// the now-complete def's flag to false itself). A crash between the
    /// driver's completion and this rewrite leaves the conservative `false`
    /// on disk. A legacy (pre-flag) `Building` row was never marked aware,
    /// so its flag can only complete false — re-create the index to earn
    /// the flag.
    pub(crate) fn resume_compound(
        &self,
        collection: &str,
        fields: &[String],
        cursor: &[u8],
    ) -> Result<()> {
        let ns = compound_namespace(collection, fields);
        let key = compound_def_key(collection, fields);
        // A resumed cycle keeps the awareness its registration wrote; a
        // legacy cycle (empty kind bytes) stays unaware.
        let aware = match self.store().get(COMPOUND_DEFS, &key)? {
            Some(row) => {
                let (kind, st) = crate::index_build::decode_def(&row);
                matches!(st, crate::index_build::DefState::Building { .. })
                    && kind.first() == Some(&KIND_FLAG_AWARE)
            }
            None => false,
        };
        let kb = vec![KIND_FLAG_AWARE];
        crate::index_build::run_atomic_backfill(
            self.store(),
            collection,
            COMPOUND_DEFS,
            &key,
            &kb,
            cursor,
            &mut |tx, page| {
                for (doc_key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    let values: Option<Vec<&Value>> =
                        fields.iter().map(|f| doc.get_path(f)).collect();
                    match &values {
                        Some(vs) => compound_insert_in_txn(tx, &ns, doc_key, vs)?,
                        None => tx.put(COMPOUND_MISS, &key, &[])?,
                    }
                }
                Ok(())
            },
        )?;
        let all_indexed = aware
            && self.store().transaction(|tx| {
                let clean = tx.get(COMPOUND_MISS, &key)?.is_none();
                if clean {
                    tx.put(
                        COMPOUND_DEFS,
                        &key,
                        &crate::index_build::encode_def(
                            &[KIND_ALL_INDEXED],
                            &crate::index_build::DefState::Complete,
                        ),
                    )?;
                }
                Ok(clean)
            })?;
        self.mark_compound_complete(collection, fields, all_indexed);
        Ok(())
    }

    /// Maintain every compound index on `collection` after a document write,
    /// inside the caller's write transaction. A document leaving any indexed
    /// field missing/non-encodable is not indexed — and permanently clears
    /// that def's `all_docs_indexed`: the miss is persisted in the SAME
    /// transaction (a miss marker while the def is still `Building`, a
    /// def-rewrite to flag=false once `Complete`), so a crash can never
    /// leave a served prefix-only window over an index with unindexed
    /// matching documents. The flag is never re-flipped true without a
    /// rebuild — re-registration recomputes it.
    pub(crate) fn compound_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        for fields in self.compound_fields(collection) {
            let ns = compound_namespace(collection, &fields);
            let values: Option<Vec<&Value>> = fields.iter().map(|f| doc.get_path(f)).collect();
            match &values {
                Some(vs) => compound_insert_in_txn(tx, &ns, key, vs)?,
                None => {
                    compound_remove_in_txn(tx, &ns, key)?;
                    // Persist the miss: disk state decides which shape to
                    // write (the in-memory `building` bit may lag a
                    // concurrent re-registration).
                    let dk = compound_def_key(collection, &fields);
                    if let Some(row) = tx.get(COMPOUND_DEFS, &dk)? {
                        let (_, st) = crate::index_build::decode_def(&row);
                        match st {
                            crate::index_build::DefState::Complete => {
                                tx.put(
                                    COMPOUND_DEFS,
                                    &dk,
                                    &crate::index_build::encode_def(
                                        &[0],
                                        &crate::index_build::DefState::Complete,
                                    ),
                                )?;
                            }
                            crate::index_build::DefState::Building { .. } => {
                                tx.put(COMPOUND_MISS, &dk, &[])?;
                            }
                        }
                    }
                    self.compound_miss_in_memory(collection, &fields);
                }
            }
        }
        Ok(())
    }

    /// Remove `key` from every compound index on `collection` inside the
    /// caller's write transaction.
    pub(crate) fn compound_on_delete_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        for fields in self.compound_fields(collection) {
            compound_remove_in_txn(tx, &compound_namespace(collection, &fields), key)?;
        }
        Ok(())
    }

    /// A verified superset of doc keys for a compound index `fields`: equality
    /// `eq_prefix` over the leading fields, then optional range `tail`
    /// constraints over the next field, read from `reader`'s snapshot (audit
    /// B3). `None` if no such index, the prefix is empty with no tail, or the
    /// candidate set exceeds `cap`. Resume discipline as
    /// [`Db::scalar_candidates`]: the caller resumes before its snapshot.
    ///
    /// Soundness gate: a query leaving trailing fields unconstrained
    /// (prefix-only) is served ONLY when the def's `all_docs_indexed` flag is
    /// true. When the flag is false, the index may skip documents the filter
    /// still matches (a missing/non-encodable trailing field), so the window
    /// would not be a superset — decline. When the flag is true, every
    /// document in the collection is in the index, so every match is in the
    /// window.
    pub(crate) fn compound_candidates(
        &self,
        collection: &str,
        fields: &[String],
        eq_prefix: &[&Value],
        tail: &[Constraint<'_>],
        cap: usize,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_compound_index(collection, fields) || (eq_prefix.is_empty() && tail.is_empty())
        {
            return Ok(None);
        }
        let covered = eq_prefix.len() + usize::from(!tail.is_empty());
        if covered < fields.len() && !self.compound_all_docs_indexed(collection, fields) {
            return Ok(None);
        }
        let Some(prefix) = encode_tuple(eq_prefix) else {
            return Ok(None);
        };
        let tail_window = if tail.is_empty() {
            None
        } else {
            match window(tail) {
                Some((_lane, lower, upper)) => Some((lower, upper)),
                None => return Ok(None),
            }
        };
        let ns = compound_namespace(collection, fields);
        compound_candidates(reader, &ns, &prefix, tail_window, fields.len(), cap)
    }
}

fn compound_def_key(collection: &str, fields: &[String]) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(collection.as_bytes());
    for f in fields {
        k.push(0);
        k.extend_from_slice(f.as_bytes());
    }
    k
}

fn split_compound_def_key(key: &[u8]) -> Option<(String, Vec<String>)> {
    let mut parts = key.split(|&b| b == 0);
    let coll = std::str::from_utf8(parts.next()?).ok()?.to_owned();
    let fields: Option<Vec<String>> = parts
        .map(|p| std::str::from_utf8(p).ok().map(|s| s.to_owned()))
        .collect();
    let fields = fields?;
    if fields.is_empty() {
        return None;
    }
    Some((coll, fields))
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

impl Collection<'_> {
    /// Create (or replace) a scalar index on `field`, backfilling existing
    /// documents. Equality and range filters on `field` then use it instead of
    /// scanning the whole collection. The index is on disk and persists.
    ///
    /// Atomic and crash-safe (audit A2): the def is registered `Building`
    /// before any backfill work; every page's index writes and cursor advance
    /// commit in one transaction; completion is its own final transaction. A
    /// crash or error leaves a resumable `Building` def that queries never
    /// serve — the first filtered query (or a re-creation) resumes it.
    pub fn create_scalar_index(&self, field: &str) -> Result<()> {
        self.ensure_writable()?;
        crate::db::validate_name(field)?;
        self.db().register_scalar_index(self.name(), field)?;
        // A def still Building from an interrupted creation resumes from its
        // saved cursor; a Complete (or fresh) def backfills from the start.
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            SCALAR_DEFS,
            &def_key(self.name(), field),
        )?;
        self.db()
            .resume_scalar(self.name(), field, &cursor.unwrap_or_default())
    }

    /// Create (or replace) a compound index over an ordered list of `fields`,
    /// backfilling existing documents. A query with equality on a leading
    /// prefix of the fields (optionally plus a range on the next field) then
    /// uses it. A document missing any of the fields is not indexed. On disk,
    /// persists.
    ///
    /// Atomic and crash-safe (audit A2): the def is registered `Building`
    /// before any backfill work; every page's index writes and cursor advance
    /// commit in one transaction; completion is its own final transaction. A
    /// crash or error leaves a resumable `Building` def that queries never
    /// serve — the first compound query (or a re-creation) resumes it.
    pub fn create_compound_index(&self, fields: &[&str]) -> Result<()> {
        self.ensure_writable()?;
        for f in fields {
            crate::db::validate_name(f)?;
        }
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
        self.db().register_compound_index(self.name(), &fields)?;
        // A def still Building from an interrupted creation resumes from its
        // saved cursor; a Complete (or fresh) def backfills from the start.
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            COMPOUND_DEFS,
            &compound_def_key(self.name(), &fields),
        )?;
        self.db()
            .resume_compound(self.name(), &fields, &cursor.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::{Db, field};
    use std::collections::BTreeMap;

    fn rec(n: i64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(n));
        Value::Map(m)
    }

    fn float_doc(x: f64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("x".to_owned(), Value::Float(x));
        Value::Map(m)
    }

    /// Regression: `-0.0` and `+0.0` are equal to the predicate layer, so the
    /// index must not split them into two keys (a window anchored at either
    /// zero used to exclude documents storing the other).
    #[test]
    fn negative_zero_is_visible_to_zero_anchored_windows() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"neg", &float_doc(-0.0)).unwrap();
        c.insert(b"pos", &float_doc(0.0)).unwrap();
        c.create_scalar_index("x").unwrap();

        let count = |p: crate::Predicate| c.query().filter(p).run().unwrap().len();
        assert_eq!(count(field("x").eq(Value::Float(0.0))), 2);
        assert_eq!(count(field("x").eq(Value::Float(-0.0))), 2);
        assert_eq!(count(field("x").ge(Value::Int(0))), 2);
        assert_eq!(count(field("x").ge(Value::Float(-0.0))), 2);
        assert_eq!(count(field("x").le(Value::Float(-0.0))), 2);
        // And a scan-only database agrees.
        let plain = Db::open_in_memory().unwrap();
        let pc = plain.collection("docs");
        pc.insert(b"neg", &float_doc(-0.0)).unwrap();
        pc.insert(b"pos", &float_doc(0.0)).unwrap();
        assert_eq!(
            pc.query()
                .filter(field("x").eq(Value::Float(0.0)))
                .run()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn between_in_prefix_use_index_matching_scan() {
        use crate::field;
        fn fill(c: &crate::Collection) {
            for i in 0..60i64 {
                let mut m = BTreeMap::new();
                m.insert("n".to_owned(), Value::Int(i));
                m.insert("name".to_owned(), Value::Text(format!("item{:02}", i)));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        fill(&ic);
        ic.create_scalar_index("n").unwrap();
        ic.create_scalar_index("name").unwrap();

        let keys = |rows: Vec<crate::ResultRow>| {
            let mut k: Vec<_> = rows.into_iter().map(|r| r.key).collect();
            k.sort();
            k
        };
        // between
        let q = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("n").between(Value::Int(10), Value::Int(15)))
                .run()
                .unwrap()
        };
        assert_eq!(keys(q(&plain)), keys(q(&indexed)));
        // in
        let q = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("n").is_in([Value::Int(3), Value::Int(50), Value::Int(59)]))
                .run()
                .unwrap()
        };
        assert_eq!(keys(q(&plain)), keys(q(&indexed)));
        assert_eq!(keys(q(&indexed)).len(), 3);
        // starts_with (text prefix)
        let q = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("name").starts_with("item1"))
                .run()
                .unwrap()
        };
        assert_eq!(keys(q(&plain)), keys(q(&indexed)));
        // item10..item19 → 10 docs
        assert_eq!(keys(q(&indexed)).len(), 10);
    }

    #[test]
    fn scalar_index_on_nested_field() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..10i64 {
            let mut meta = BTreeMap::new();
            meta.insert("score".to_owned(), Value::Int(i));
            let mut m = BTreeMap::new();
            m.insert("meta".to_owned(), Value::Map(meta));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // Index a nested/dotted field; a filter on it uses the index and
        // returns the right docs (parity with dotted-path filter semantics).
        c.create_scalar_index("meta.score").unwrap();
        let got = db
            .scalar_candidates(
                "docs",
                "meta.score",
                &one(CmpOp::Eq, &Value::Int(7)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![vec![7u8]]);
        let rows = c
            .query()
            .filter(field("meta.score").ge(Value::Int(8)))
            .run()
            .unwrap();
        let mut keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![vec![8u8], vec![9u8]]);
    }

    #[test]
    fn compound_index_matches_full_scan() {
        use crate::field;
        fn fill(c: &crate::Collection) {
            for i in 0..120i64 {
                let mut m = BTreeMap::new();
                m.insert("a".to_owned(), Value::Text(format!("g{}", i % 4)));
                m.insert("b".to_owned(), Value::Int(i % 10));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        fill(&ic);
        ic.create_compound_index(&["a", "b"]).unwrap();

        let run = |db: &Db| {
            // Equality on the leading field + range on the next: a compound win.
            db.collection("docs")
                .query()
                .filter(field("a").eq(Value::Text("g2".into())))
                .filter(field("b").ge(Value::Int(5)))
                .order_by("b", false)
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&plain), run(&indexed));
        assert!(!run(&indexed).is_empty());
    }

    #[test]
    fn compound_candidates_prefix_eq_only() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..20i64 {
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Int(i % 3));
            m.insert("b".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        c.create_compound_index(&["a", "b"]).unwrap();
        let fields = vec!["a".to_owned(), "b".to_owned()];
        let a0 = Value::Int(0);
        // Eq on the leading field only: all docs with a == 0.
        let got = db
            .compound_candidates("docs", &fields, &[&a0], &[], 1000, db.store())
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), 7); // i in {0,3,6,9,12,15,18}
        // Unregistered field list → None.
        let other = vec!["a".to_owned()];
        assert!(
            db.compound_candidates("docs", &other, &[&a0], &[], 1000, db.store())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn compound_definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Int(1));
            m.insert("b".to_owned(), Value::Int(2));
            c.insert(b"k", &Value::Map(m)).unwrap();
            c.create_compound_index(&["a", "b"]).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let fields = vec!["a".to_owned(), "b".to_owned()];
        let (a, b) = (Value::Int(1), Value::Int(2));
        let got = db
            .compound_candidates("docs", &fields, &[&a, &b], &[], 1000, db.store())
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"k".to_vec()]);
    }

    #[test]
    fn encoding_preserves_numeric_order() {
        let vals = [-1000i64, -1, 0, 1, 2, 1000, i64::MAX];
        let encs: Vec<Vec<u8>> = vals
            .iter()
            .map(|&n| encode_value(&Value::Int(n)).unwrap())
            .collect();
        let mut sorted = encs.clone();
        sorted.sort();
        assert_eq!(encs, sorted, "encoding must sort in numeric order");
    }

    #[test]
    fn encoding_preserves_text_order() {
        let words = ["a", "ab", "abc", "b", "z"];
        let encs: Vec<Vec<u8>> = words
            .iter()
            .map(|w| encode_value(&Value::Text((*w).into())).unwrap())
            .collect();
        let mut sorted = encs.clone();
        sorted.sort();
        assert_eq!(encs, sorted);
    }

    /// The order walk's two per-entry facts, pinned on the raw encoding:
    /// `num_value_of` inverts `num_payload` exactly (NaN payloads decode
    /// to NaN, everything else round-trips), and `comparable_entries`
    /// streams numeric lane then text lane in value order while skipping
    /// NaN entries and never touching the forward map or the bool lane —
    /// the class-0 order `order_index_rows` (builder.rs) serves from.
    #[test]
    fn comparable_entries_walks_class0_order_and_skips_nan() {
        // Payload decode: round-trips non-NaN (including ±0.0 and the
        // extremes), and flags both NaN signs.
        for f in [
            0.0,
            -0.0,
            -1.5,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::MIN,
            f64::MAX,
        ] {
            let key = encode_value(&Value::Float(f)).unwrap();
            assert_eq!(num_value_of(&key), Some(f), "round-trip {f}");
        }
        for nan in [f64::NAN, -f64::NAN] {
            let key = encode_value(&Value::Float(nan)).unwrap();
            assert!(num_value_of(&key).is_some_and(f64::is_nan));
        }

        // The walk: numbers in value order, then texts, NaN skipped, bool
        // and forward-map rows never visited. Keys deliberately oppose
        // value order.
        let store = Store::open_in_memory().unwrap();
        let rows: Vec<(Value, &str)> = vec![
            (Value::Int(5), "z"),
            (Value::Float(-1.0), "y"),
            (Value::Float(2.0), "x"),
            (Value::Int(2), "w"), // same f64 as 2.0: one bucket, key order
            (Value::Float(f64::NAN), "v"), // skipped: incomparable
            (Value::Bool(true), "u"), // bool lane: never visited
            (Value::Text("b".into()), "t"),
            (Value::Text("a".into()), "s"),
        ];
        store
            .transaction(|tx| {
                for (v, doc) in &rows {
                    insert_in_txn(tx, "ix", doc.as_bytes(), v)?;
                }
                Ok(())
            })
            .unwrap();
        let mut got: Vec<String> = Vec::new();
        comparable_entries(&store, "ix", |_value, doc| {
            got.push(String::from_utf8_lossy(doc).into_owned());
            Ok(true)
        })
        .unwrap();
        // Numeric lane by value (Float(-1.0) first, then the 2/2.0 bucket —
        // one encoding, doc-key order inside it; the EXACT within-bucket
        // order is the builder's job), then the text lane; NaN and bool
        // rows absent.
        assert_eq!(got, vec!["y", "w", "x", "z", "s", "t"]);
    }

    /// A malformed index key (no `0x00 0x00` value terminator — unreachable
    /// via the encoder, the shape bit-rot or a hand-corrupted row produces)
    /// makes `comparable_entries` error `Error::CorruptIndex` instead of
    /// silently skipping the row: the order walk that consumes it would
    /// otherwise mis-order the result without any signal (audit C1's
    /// fail-loudly philosophy; the end-to-end twin lives in lifecycle.rs).
    #[test]
    fn comparable_entries_errors_on_terminator_less_key() {
        let store = Store::open_in_memory().unwrap();
        store
            .transaction(|tx| {
                insert_in_txn(tx, "ix", b"good", &Value::Int(1))?;
                // Hand-corrupted row: numeric-lane key with payload but NO
                // terminator, so `terminator_pos` finds no doc-key boundary.
                tx.put("ix", &[LANE_NUM, 0x05, 0x01], &[])
            })
            .unwrap();
        let err = comparable_entries(&store, "ix", |_, _| Ok(true)).unwrap_err();
        match err {
            crate::Error::CorruptIndex { context } => {
                assert!(
                    context.contains("ix"),
                    "the error must name the corrupt namespace, got {context:?}"
                );
            }
            other => panic!("malformed key must error CorruptIndex, got {other:?}"),
        }
    }

    #[test]
    fn text_with_zero_byte_round_trips_delimiter() {
        // A value containing 0x00 must not break the terminator/doc-key split.
        let store = Store::open_in_memory().unwrap();
        let v = Value::Text("a\u{0}b".into());
        store
            .transaction(|tx| insert_in_txn(tx, "ix", b"doc1", &v))
            .unwrap();
        let (lane, lower, upper) = window(&[Constraint {
            op: CmpOp::Eq,
            value: &v,
        }])
        .unwrap();
        let got = window_candidates(&store, "ix", lane, &lower, upper.as_deref(), 1000)
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"doc1".to_vec()]);
    }

    fn one(op: CmpOp, value: &Value) -> Vec<Constraint<'_>> {
        vec![Constraint { op, value }]
    }

    fn db_with_index() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for (k, n) in [(b"a", 3i64), (b"b", 1), (b"c", 2), (b"d", 3)] {
            c.insert(k, &rec(n)).unwrap();
        }
        c.create_scalar_index("n").unwrap();
        db
    }

    #[test]
    fn eq_returns_matching_keys() {
        let db = db_with_index();
        let mut got = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(3)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        got.sort();
        assert_eq!(got, vec![b"a".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn range_returns_superset_on_each_side() {
        let db = db_with_index();
        // `>` includes the boundary bucket (superset for the caller to verify):
        // it must contain every true match (n=3,2,3) and exclude the far side.
        let gt = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Gt, &Value::Int(1)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        for k in [b"a".as_slice(), b"c", b"d"] {
            assert!(gt.iter().any(|g| g == k), "gt missing {k:?}");
        }

        // `<=` must contain n=1 (b) and n=2 (c), never the larger values.
        let le = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Le, &Value::Int(2)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        for k in [b"b".as_slice(), b"c"] {
            assert!(le.iter().any(|g| g == k), "le missing {k:?}");
        }
        assert!(!le.iter().any(|g| g == b"a"), "le must exclude n=3");
    }

    #[test]
    fn cap_exceeded_falls_back_to_none() {
        let db = db_with_index(); // four docs, n in {1,2,3}
        // A range covering everything, with a cap below the match count, must
        // signal fall-back (None) rather than materialise the set.
        let got = db
            .scalar_candidates("docs", "n", &one(CmpOp::Ge, &Value::Int(0)), 2, db.store())
            .unwrap();
        assert!(got.is_none(), "over-cap range must fall back");
    }

    #[test]
    fn mixed_type_constraints_are_not_serviceable() {
        let db = db_with_index();
        let (lo, hi) = (Value::Int(1), Value::Text("z".into()));
        let constraints = vec![
            Constraint {
                op: CmpOp::Ge,
                value: &lo,
            },
            Constraint {
                op: CmpOp::Le,
                value: &hi,
            },
        ];
        assert!(
            db.scalar_candidates("docs", "n", &constraints, 1000, db.store())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn float_and_int_share_a_numeric_window() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let mut m = BTreeMap::new();
        m.insert("x".to_owned(), Value::Float(2.5));
        c.insert(b"f", &Value::Map(m)).unwrap();
        c.insert(b"i", &rec_field("x", Value::Int(2))).unwrap();
        c.create_scalar_index("x").unwrap();
        // Range [2.0, 3.0] over the shared numeric lane finds both.
        let (lo, hi) = (Value::Float(2.0), Value::Float(3.0));
        let constraints = vec![
            Constraint {
                op: CmpOp::Ge,
                value: &lo,
            },
            Constraint {
                op: CmpOp::Le,
                value: &hi,
            },
        ];
        let mut got = db
            .scalar_candidates("docs", "x", &constraints, 1000, db.store())
            .unwrap()
            .unwrap();
        got.sort();
        assert_eq!(got, vec![b"f".to_vec(), b"i".to_vec()]);
    }

    #[test]
    fn text_and_bool_eq_lookups() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"t", &rec_field("s", Value::Text("hello".into())))
            .unwrap();
        c.insert(b"b", &rec_field("flag", Value::Bool(true)))
            .unwrap();
        c.create_scalar_index("s").unwrap();
        c.create_scalar_index("flag").unwrap();
        let s = db
            .scalar_candidates(
                "docs",
                "s",
                &one(CmpOp::Eq, &Value::Text("hello".into())),
                100,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(s, vec![b"t".to_vec()]);
        let f = db
            .scalar_candidates(
                "docs",
                "flag",
                &one(CmpOp::Eq, &Value::Bool(true)),
                100,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(f, vec![b"b".to_vec()]);
    }

    fn rec_field(field: &str, value: Value) -> Value {
        let mut m = BTreeMap::new();
        m.insert(field.to_owned(), value);
        Value::Map(m)
    }

    #[test]
    fn non_indexable_value_is_skipped() {
        // A container value can't be indexed: the field is simply absent from
        // the index, and a query on it returns no candidates (empty, not error).
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"v", &rec_field("arr", Value::Array(vec![Value::Int(1)])))
            .unwrap();
        c.create_scalar_index("arr").unwrap();
        let got = db
            .scalar_candidates(
                "docs",
                "arr",
                &one(CmpOp::Eq, &Value::Int(1)),
                100,
                db.store(),
            )
            .unwrap();
        // Int query value pins a numeric lane with no entries → empty superset.
        assert_eq!(got, Some(vec![]));
    }

    #[test]
    fn unindexed_field_returns_none() {
        let db = db_with_index();
        assert!(
            db.scalar_candidates(
                "docs",
                "other",
                &one(CmpOp::Eq, &Value::Int(1)),
                1000,
                db.store()
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn ne_is_not_serviceable() {
        let db = db_with_index();
        assert!(
            db.scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Ne, &Value::Int(1)),
                1000,
                db.store()
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn maintained_on_insert_overwrite_and_delete() {
        let db = db_with_index();
        let c = db.collection("docs");
        // Overwrite a (3 -> 9): no longer an eq-3 candidate.
        c.insert(b"a", &rec(9)).unwrap();
        let mut three = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(3)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        three.sort();
        assert_eq!(three, vec![b"d".to_vec()]);
        // Delete d: gone from the index.
        c.delete(b"d").unwrap();
        assert!(
            db.scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(3)),
                1000,
                db.store()
            )
            .unwrap()
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            for (k, n) in [(b"a", 1i64), (b"b", 2)] {
                c.insert(k, &rec(n)).unwrap();
            }
            c.create_scalar_index("n").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let got = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(2)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"b".to_vec()]);
    }

    #[test]
    fn query_filter_uses_index_and_is_correct() {
        let db = db_with_index();
        let rows = db
            .collection("docs")
            .query()
            .filter(field("n").eq(Value::Int(3)))
            .run()
            .unwrap();
        let mut keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![b"a".to_vec(), b"d".to_vec()]);
    }

    /// A building scalar index is never served: filtered queries fall back to a
    /// scan and stay correct; the first such query resumes the build.
    #[test]
    fn building_scalar_index_falls_back_then_resumes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..50i64 {
            c.insert(&[i as u8], &rec(i)).unwrap();
        }
        // Forge a Building def exactly as an interrupted creation would leave it.
        db.register_scalar_index("docs", "n").unwrap(); // registers Building
        assert!(
            !db.has_scalar_index("docs", "n"),
            "building def must not be serviceable"
        );
        // Before resume: a filtered query must still be correct (scan fallback).
        let rows = c
            .query()
            .filter(crate::field("n").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 10); // resumed by the query itself, then correct
        // After the resume the def is complete and serviceable.
        assert!(db.has_scalar_index("docs", "n"));
    }

    /// Contention: while another thread holds the resume lock mid-backfill, a
    /// filtered query must not serve the building scalar index — it falls
    /// back to an exact scan and stays correct, and the def stays building;
    /// once the lock is free, the next query resumes the build and serves.
    #[test]
    fn building_scalar_index_with_resume_lock_held_falls_back() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..50i64 {
            c.insert(&[i as u8], &rec(i)).unwrap();
        }
        // Forge a Building def exactly as an interrupted creation would leave it.
        db.register_scalar_index("docs", "n").unwrap();
        // With the resume lock held (another thread resuming), the building
        // def must not be served: scalar_candidates reports "no usable
        // index"...
        let _guard = db.index_resume().lock().unwrap();
        assert!(
            db.scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(3)),
                1000,
                db.store()
            )
            .unwrap()
            .is_none(),
            "a building scalar index must not be served"
        );
        // ...so the filtered query falls back to an exact scan and stays
        // correct, while the contended resume never runs (def still building).
        let rows = c
            .query()
            .filter(crate::field("n").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 10);
        assert!(!db.has_scalar_index("docs", "n"));
        assert_eq!(db.collect_building_scalar("docs").unwrap().len(), 1);
        drop(_guard);
        // Once the resume lock is free, the next query resumes the backfill
        // and the completed index serves.
        let rows = c
            .query()
            .filter(crate::field("n").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 10);
        assert!(db.has_scalar_index("docs", "n"));
        let got = db
            .scalar_candidates(
                "docs",
                "n",
                &one(CmpOp::Eq, &Value::Int(3)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![vec![3u8]], "a completed scalar index must serve");
    }

    /// A building compound index is never served: filtered queries fall back
    /// to a scan and stay correct; the first such query resumes the build.
    #[test]
    fn building_compound_index_falls_back_then_resumes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..50i64 {
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Text(format!("g{}", i % 2)));
            m.insert("b".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // Forge a Building def exactly as an interrupted creation would leave
        // it: registered, but with no backfill pages committed (empty index
        // namespace).
        let fields = vec!["a".to_owned(), "b".to_owned()];
        db.register_compound_index("docs", &fields).unwrap();
        assert!(
            !db.has_compound_index("docs", &fields),
            "building def must not be serviceable"
        );
        // Before resume: a prefix+range query must still be correct (scan
        // fallback; the query itself resumes the build).
        let rows = c
            .query()
            .filter(crate::field("a").eq(Value::Text("g1".into())))
            .filter(crate::field("b").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 5); // i in {41,43,45,47,49}
        // After the resume the def is complete and serviceable.
        assert!(db.has_compound_index("docs", &fields));
        let g1 = Value::Text("g1".into());
        let got = db
            .compound_candidates(
                "docs",
                &fields,
                &[&g1],
                &one(CmpOp::Ge, &Value::Int(40)),
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), 5);
    }

    /// Contention: while another thread holds the resume lock mid-backfill, a
    /// compound query must not serve the building index — it falls back to an
    /// exact scan and stays correct, and the def stays building; once the
    /// lock is free, the next query resumes the build and serves.
    #[test]
    fn building_compound_index_with_resume_lock_held_falls_back() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..50i64 {
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Text(format!("g{}", i % 2)));
            m.insert("b".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // Forge a Building def exactly as an interrupted creation would leave it.
        let fields = vec!["a".to_owned(), "b".to_owned()];
        db.register_compound_index("docs", &fields).unwrap();
        // With the resume lock held (another thread resuming), the building
        // def must not be served: compound_candidates reports "no usable
        // index"...
        let _guard = db.index_resume().lock().unwrap();
        let g1 = Value::Text("g1".into());
        let tail = one(CmpOp::Ge, &Value::Int(40));
        assert!(
            db.compound_candidates("docs", &fields, &[&g1], &tail, 1000, db.store())
                .unwrap()
                .is_none(),
            "a building compound index must not be served"
        );
        // ...so the prefix+range query falls back to an exact scan and stays
        // correct, while the contended resume never runs (def still building).
        let rows = c
            .query()
            .filter(crate::field("a").eq(g1.clone()))
            .filter(crate::field("b").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 5); // i in {41,43,45,47,49}
        assert!(!db.has_compound_index("docs", &fields));
        assert_eq!(db.collect_building_compound("docs").unwrap().len(), 1);
        drop(_guard);
        // Once the resume lock is free, the next query resumes the backfill
        // and the completed index serves.
        let rows = c
            .query()
            .filter(crate::field("a").eq(g1.clone()))
            .filter(crate::field("b").ge(Value::Int(40)))
            .run()
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert!(db.has_compound_index("docs", &fields));
        let mut got = db
            .compound_candidates("docs", &fields, &[&g1], &tail, 1000, db.store())
            .unwrap()
            .unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![vec![41u8], vec![43u8], vec![45u8], vec![47u8], vec![49u8]],
            "a completed compound index must serve"
        );
    }

    /// A miss observed while the def is still BUILDING must survive the
    /// backfill completion: a document inserted mid-build (before the walk's
    /// cursor) is never seen by any backfill page, so the maintenance path
    /// persists a miss marker in the document's own transaction — the
    /// completion reads it and completes with `all_docs_indexed` = false.
    /// Forged deterministically (no threads): register a Building def
    /// exactly as an interrupted creation would, write the corpus through
    /// the maintenance path, then let the first query resume the build.
    #[test]
    fn compound_miss_during_building_survives_completion() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let fields = vec!["a".to_owned(), "b".to_owned()];
        // Register Building (the interrupted-creation state) BEFORE any
        // document exists: every insert below takes the maintenance path.
        db.register_compound_index("docs", &fields).unwrap();
        for i in 0..20i64 {
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Text(format!("g{}", i % 2)));
            m.insert("b".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // The miss: matches a g-equality prefix-only filter, missing `b`,
        // inserted while the def is Building (before the walk's cursor).
        let mut m = BTreeMap::new();
        m.insert("a".to_owned(), Value::Text("g0".into()));
        c.insert(b"z", &Value::Map(m)).unwrap();
        // A prefix-only query resumes the build and must NOT be served the
        // window afterwards (the miss marker persisted through completion),
        // yet stay CORRECT via the scan fallback — including the z doc.
        let rows = c
            .query()
            .filter(field("a").eq(Value::Text("g0".into())))
            .run()
            .unwrap();
        let mut got: Vec<Vec<u8>> = rows.into_iter().map(|r| r.key).collect();
        got.sort();
        let mut want: Vec<Vec<u8>> = (0..20i64)
            .filter(|i| i % 2 == 0)
            .map(|i| vec![i as u8])
            .chain(std::iter::once(b"z".to_vec()))
            .collect();
        want.sort();
        assert_eq!(got, want);
        assert!(
            !db.compound_all_docs_indexed("docs", &fields),
            "the mid-build miss must keep the completed flag false"
        );
        // The prefix-only probe declines (the planner-visible consequence).
        assert!(
            db.compound_candidates(
                "docs",
                &fields,
                &[&Value::Text("g0".into())],
                &[],
                1000,
                db.store()
            )
            .unwrap()
            .is_none(),
            "prefix-only must decline while the flag is false"
        );
    }

    /// The flag-aware completion happy path through the internal probe: an
    /// all-present corpus (docs inserted BEFORE the index is created, so
    /// the backfill pages see them) earns the flag and the prefix-only
    /// probe serves.
    #[test]
    fn compound_backfill_computes_all_docs_indexed_flag() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..20i64 {
            let mut m = BTreeMap::new();
            m.insert("a".to_owned(), Value::Text(format!("g{}", i % 2)));
            m.insert("b".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        c.create_compound_index(&["a", "b"]).unwrap();
        let fields = vec!["a".to_owned(), "b".to_owned()];
        assert!(db.compound_all_docs_indexed("docs", &fields));
        let got = db
            .compound_candidates(
                "docs",
                &fields,
                &[&Value::Text("g1".into())],
                &[],
                1000,
                db.store(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), 10);
    }

    #[test]
    fn legacy_stateless_compound_def_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            for i in 0..20i64 {
                let mut m = BTreeMap::new();
                m.insert("a".to_owned(), Value::Text(format!("g{}", i % 2)));
                m.insert("b".to_owned(), Value::Int(i));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
            c.create_compound_index(&["a", "b"]).unwrap();
            // Overwrite the def row with the legacy empty form.
            db.store()
                .put(COMPOUND_DEFS, b"docs\x00a\x00b", b"")
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let fields = vec!["a".to_owned(), "b".to_owned()];
        assert!(db.has_compound_index("docs", &fields)); // legacy → Complete → serviceable
        assert!(db.collect_building_compound("docs").unwrap().is_empty());
        // End-to-end (mirrors the fts/vector legacy tests): a real prefix+
        // range query served through the legacy def returns exactly the
        // right rows.
        let rows = db
            .collection("docs")
            .query()
            .filter(field("a").eq(Value::Text("g1".into())))
            .filter(field("b").ge(Value::Int(10)))
            .run()
            .unwrap();
        let mut keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![vec![11u8], vec![13u8], vec![15u8], vec![17u8], vec![19u8]]
        );
    }

    #[test]
    fn legacy_stateless_scalar_def_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            for i in 0..20i64 {
                c.insert(&[i as u8], &rec(i)).unwrap();
            }
            c.create_scalar_index("n").unwrap();
            // Overwrite the def row with the legacy empty form.
            db.store()
                .put(crate::scalar::SCALAR_DEFS, b"docs\x00n", b"")
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.has_scalar_index("docs", "n")); // legacy → Complete → serviceable
        assert!(db.collect_building_scalar("docs").unwrap().is_empty());
        // End-to-end (mirrors the fts/vector legacy tests): a real filtered
        // query served through the legacy def returns exactly the right rows.
        let rows = db
            .collection("docs")
            .query()
            .filter(field("n").ge(Value::Int(15)))
            .run()
            .unwrap();
        let mut keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![vec![15u8], vec![16u8], vec![17u8], vec![18u8], vec![19u8]]
        );
    }
}
