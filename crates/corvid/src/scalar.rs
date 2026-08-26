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
use crate::store::Store;
use crate::value::Value;

/// Reserved collection holding persisted scalar-index definitions.
const SCALAR_DEFS: &str = "__scalar_indexes__";

const FWD_TAG: u8 = 0x00;
const LANE_BOOL: u8 = 0x01;
const LANE_NUM: u8 = 0x02;
const LANE_TEXT: u8 = 0x03;

/// Reserved collection holding persisted compound-index definitions.
const COMPOUND_DEFS: &str = "__cscalar_indexes__";

/// Per-database scalar-index registry.
#[derive(Default)]
pub(crate) struct ScalarState {
    /// Single-field indexes: `(collection, field)`.
    defs: std::collections::HashSet<(String, String)>,
    /// Compound indexes: `(collection, ordered field list)`.
    compound: Vec<(String, Vec<String>)>,
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

/// Index many `(doc_key, value)` pairs in one transaction (bulk load).
pub(crate) fn insert_many(store: &Store, ns: &str, items: &[(Vec<u8>, Value)]) -> Result<()> {
    store.transaction(|tx| {
        for (doc_key, value) in items {
            remove_in_txn(tx, ns, doc_key)?;
            if let Some(enc) = encode_value(value) {
                let mut idx_key = enc.clone();
                idx_key.extend_from_slice(doc_key);
                tx.put(ns, &idx_key, &[])?;
                tx.put(ns, &fwd_key(doc_key), &enc)?;
            }
        }
        Ok(())
    })
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

/// Scan the `[lower, upper]` window within `lane`, returning candidate doc keys
/// (a verified superset). Stops and returns `None` if the candidate count would
/// exceed `cap` — the caller then falls back to a bounded scan, so a
/// low-selectivity filter never materialises an unbounded set in memory.
fn window_candidates(
    store: &Store,
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
        let page = store.scan_from(ns, &cursor, PAGE)?;
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

/// Scan a compound index: a fixed equality `prefix` over the leading fields,
/// then an optional range `window` over the next field. Returns a verified
/// superset of doc keys, or `None` if it would exceed `cap`. `n_fields` is the
/// index arity (to locate the doc key past all encoded values).
fn compound_candidates(
    store: &Store,
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
        let page = store.scan_from(ns, &cursor, PAGE)?;
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
    /// Load persisted scalar-index definitions. Called once on open.
    pub(crate) fn load_scalar_defs(&self) -> Result<()> {
        let mut state = self.scalar().lock().expect("scalar lock");
        for (key, _) in self.store().scan(SCALAR_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                state.defs.insert(def);
            }
        }
        Ok(())
    }

    /// Register (or replace) a scalar index on `field` for `collection`.
    pub(crate) fn register_scalar_index(&self, collection: &str, field: &str) -> Result<()> {
        self.store()
            .put(SCALAR_DEFS, &def_key(collection, field), b"")?;
        let mut state = self.scalar().lock().expect("scalar lock");
        state.defs.insert((collection.to_owned(), field.to_owned()));
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

    fn scalar_fields(&self, collection: &str) -> Vec<String> {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .iter()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Whether `field` of `collection` has a scalar index.
    pub(crate) fn has_scalar_index(&self, collection: &str, field: &str) -> bool {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .defs
            .contains(&(collection.to_owned(), field.to_owned()))
    }

    /// If `field` has a scalar index, return a *superset* of doc keys matching
    /// every `constraint` (the caller must verify with the exact predicate).
    ///
    /// `None` when the field is not indexed, the constraints aren't
    /// range-serviceable, or the candidate set would exceed `cap` (in which
    /// case a full scan is the better — and bounded — plan).
    pub(crate) fn scalar_candidates(
        &self,
        collection: &str,
        field: &str,
        constraints: &[Constraint<'_>],
        cap: usize,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_scalar_index(collection, field) {
            return Ok(None);
        }
        let Some((lane, lower, upper)) = window(constraints) else {
            return Ok(None);
        };
        let ns = namespace(collection, field);
        window_candidates(self.store(), &ns, lane, &lower, upper.as_deref(), cap)
    }

    /// Doc keys whose indexed text value at `field` starts with `prefix` (a
    /// verified superset). `None` if not indexed or over `cap`.
    pub(crate) fn scalar_prefix_candidates(
        &self,
        collection: &str,
        field: &str,
        prefix: &str,
        cap: usize,
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
            let page = self.store().scan_from(&ns, &cursor, PAGE)?;
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

    /// Load persisted compound-index definitions. Called once on open.
    pub(crate) fn load_compound_defs(&self) -> Result<()> {
        let mut state = self.scalar().lock().expect("scalar lock");
        for (key, _) in self.store().scan(COMPOUND_DEFS)? {
            if let Some(def) = split_compound_def_key(&key) {
                state.compound.push(def);
            }
        }
        Ok(())
    }

    /// Register (or replace) a compound index over `fields` for `collection`.
    pub(crate) fn register_compound_index(
        &self,
        collection: &str,
        fields: &[String],
    ) -> Result<()> {
        self.store()
            .put(COMPOUND_DEFS, &compound_def_key(collection, fields), b"")?;
        let mut state = self.scalar().lock().expect("scalar lock");
        let entry = (collection.to_owned(), fields.to_vec());
        if !state.compound.contains(&entry) {
            state.compound.push(entry);
        }
        Ok(())
    }

    /// All single-field scalar index definitions (for dump/migrate).
    pub(crate) fn scalar_specs(&self) -> Vec<(String, String)> {
        let state = self.scalar().lock().expect("scalar lock");
        state.defs.iter().cloned().collect()
    }

    /// All compound index definitions (for dump/migrate).
    pub(crate) fn compound_specs(&self) -> Vec<(String, Vec<String>)> {
        let state = self.scalar().lock().expect("scalar lock");
        state.compound.clone()
    }

    /// Compound indexes registered on `collection` (ordered field lists).
    pub(crate) fn compound_indexes(&self, collection: &str) -> Vec<Vec<String>> {
        let state = self.scalar().lock().expect("scalar lock");
        state
            .compound
            .iter()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Maintain every compound index on `collection` after a document write.
    /// Maintain every compound index on `collection` inside the caller's
    /// write transaction.
    pub(crate) fn compound_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        for fields in self.compound_indexes(collection) {
            let ns = compound_namespace(collection, &fields);
            let values: Option<Vec<&Value>> = fields.iter().map(|f| doc.get_path(f)).collect();
            match &values {
                Some(vs) => compound_insert_in_txn(tx, &ns, key, vs)?,
                None => compound_remove_in_txn(tx, &ns, key)?,
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
        for fields in self.compound_indexes(collection) {
            compound_remove_in_txn(tx, &compound_namespace(collection, &fields), key)?;
        }
        Ok(())
    }

    /// A verified superset of doc keys for a compound index `fields`: equality
    /// `eq_prefix` over the leading fields, then optional range `tail`
    /// constraints over the next field. `None` if no such index, the prefix is
    /// empty with no tail, or the candidate set exceeds `cap`.
    pub(crate) fn compound_candidates(
        &self,
        collection: &str,
        fields: &[String],
        eq_prefix: &[&Value],
        tail: &[Constraint<'_>],
        cap: usize,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        let known = {
            let state = self.scalar().lock().expect("scalar lock");
            state
                .compound
                .iter()
                .any(|(c, f)| c == collection && f == fields)
        };
        if !known || (eq_prefix.is_empty() && tail.is_empty()) {
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
        compound_candidates(self.store(), &ns, &prefix, tail_window, fields.len(), cap)
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
    pub fn create_scalar_index(&self, field: &str) -> Result<()> {
        self.db().register_scalar_index(self.name(), field)?;
        let ns = namespace(self.name(), field);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = self.db().store().scan_from(self.name(), &cursor, 2048)?;
            if page.is_empty() {
                break;
            }
            let mut batch: Vec<(Vec<u8>, Value)> = Vec::new();
            for (key, bytes) in &page {
                let doc = Value::decode(bytes)?;
                if let Some(value) = doc.get_path(field) {
                    batch.push((key.clone(), value.clone()));
                }
            }
            if !batch.is_empty() {
                insert_many(self.db().store(), &ns, &batch)?;
            }
            cursor = next_after(&page.last().unwrap().0);
        }
        Ok(())
    }

    /// Create (or replace) a compound index over an ordered list of `fields`,
    /// backfilling existing documents. A query with equality on a leading
    /// prefix of the fields (optionally plus a range on the next field) then
    /// uses it. A document missing any of the fields is not indexed. On disk,
    /// persists.
    pub fn create_compound_index(&self, fields: &[&str]) -> Result<()> {
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
        self.db().register_compound_index(self.name(), &fields)?;
        let ns = compound_namespace(self.name(), &fields);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = self.db().store().scan_from(self.name(), &cursor, 2048)?;
            if page.is_empty() {
                break;
            }
            self.db().store().transaction(|tx| {
                for (key, bytes) in &page {
                    let doc = Value::decode(bytes)?;
                    let values: Option<Vec<&Value>> =
                        fields.iter().map(|f| doc.get_path(f)).collect();
                    if let Some(vs) = &values {
                        compound_insert_in_txn(tx, &ns, key, vs)?;
                    }
                }
                Ok(())
            })?;
            cursor = next_after(&page.last().unwrap().0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let count = |p: crate::Predicate| {
            c.query().filter(p).run().unwrap().len()
        };
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
            .scalar_candidates("docs", "meta.score", &one(CmpOp::Eq, &Value::Int(7)), 1000)
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
            .compound_candidates("docs", &fields, &[&a0], &[], 1000)
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), 7); // i in {0,3,6,9,12,15,18}
        // Unregistered field list → None.
        let other = vec!["a".to_owned()];
        assert!(
            db.compound_candidates("docs", &other, &[&a0], &[], 1000)
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
            .compound_candidates("docs", &fields, &[&a, &b], &[], 1000)
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
            .scalar_candidates("docs", "n", &one(CmpOp::Eq, &Value::Int(3)), 1000)
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
            .scalar_candidates("docs", "n", &one(CmpOp::Gt, &Value::Int(1)), 1000)
            .unwrap()
            .unwrap();
        for k in [b"a".as_slice(), b"c", b"d"] {
            assert!(gt.iter().any(|g| g == k), "gt missing {k:?}");
        }

        // `<=` must contain n=1 (b) and n=2 (c), never the larger values.
        let le = db
            .scalar_candidates("docs", "n", &one(CmpOp::Le, &Value::Int(2)), 1000)
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
            .scalar_candidates("docs", "n", &one(CmpOp::Ge, &Value::Int(0)), 2)
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
            db.scalar_candidates("docs", "n", &constraints, 1000)
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
            .scalar_candidates("docs", "x", &constraints, 1000)
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
            )
            .unwrap()
            .unwrap();
        assert_eq!(s, vec![b"t".to_vec()]);
        let f = db
            .scalar_candidates("docs", "flag", &one(CmpOp::Eq, &Value::Bool(true)), 100)
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
            .scalar_candidates("docs", "arr", &one(CmpOp::Eq, &Value::Int(1)), 100)
            .unwrap();
        // Int query value pins a numeric lane with no entries → empty superset.
        assert_eq!(got, Some(vec![]));
    }

    #[test]
    fn unindexed_field_returns_none() {
        let db = db_with_index();
        assert!(
            db.scalar_candidates("docs", "other", &one(CmpOp::Eq, &Value::Int(1)), 1000)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ne_is_not_serviceable() {
        let db = db_with_index();
        assert!(
            db.scalar_candidates("docs", "n", &one(CmpOp::Ne, &Value::Int(1)), 1000)
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
            .scalar_candidates("docs", "n", &one(CmpOp::Eq, &Value::Int(3)), 1000)
            .unwrap()
            .unwrap();
        three.sort();
        assert_eq!(three, vec![b"d".to_vec()]);
        // Delete d: gone from the index.
        c.delete(b"d").unwrap();
        assert!(
            db.scalar_candidates("docs", "n", &one(CmpOp::Eq, &Value::Int(3)), 1000)
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
            .scalar_candidates("docs", "n", &one(CmpOp::Eq, &Value::Int(2)), 1000)
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
}
