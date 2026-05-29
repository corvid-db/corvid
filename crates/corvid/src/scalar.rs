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

/// Per-database scalar-index registry: the set of `(collection, field)` indexed.
#[derive(Default)]
pub(crate) struct ScalarState {
    defs: std::collections::HashSet<(String, String)>,
}

pub(crate) fn new_state() -> std::sync::Mutex<ScalarState> {
    std::sync::Mutex::new(ScalarState::default())
}

/// The reserved collection backing a scalar index.
pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__scalar__{collection}__{field}")
}

// ---- order-preserving encoding ----

/// Encode a scalar value into an order-preserving, self-delimiting key payload.
/// Returns `None` for values that are not indexable scalars (null, containers,
/// vectors, bytes).
fn encode_value(v: &Value) -> Option<Vec<u8>> {
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
fn num_payload(f: f64) -> Vec<u8> {
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

/// Index (or re-index) `doc_key`'s `value` in one transaction.
pub(crate) fn insert(store: &Store, ns: &str, doc_key: &[u8], value: &Value) -> Result<()> {
    store.transaction(|tx| {
        remove_in_txn(tx, ns, doc_key)?;
        if let Some(enc) = encode_value(value) {
            let mut idx_key = enc.clone();
            idx_key.extend_from_slice(doc_key);
            tx.put(ns, &idx_key, &[])?;
            tx.put(ns, &fwd_key(doc_key), &enc)?;
        }
        Ok(())
    })
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

/// Remove `doc_key` from the index.
pub(crate) fn delete(store: &Store, ns: &str, doc_key: &[u8]) -> Result<()> {
    store.transaction(|tx| remove_in_txn(tx, ns, doc_key))
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
    pub(crate) fn scalar_on_insert(&self, collection: &str, key: &[u8], doc: &Value) -> Result<()> {
        let fields = self.scalar_fields(collection);
        for field in fields {
            let ns = namespace(collection, &field);
            match doc.get(&field) {
                Some(value) => insert(self.store(), &ns, key, value)?,
                None => delete(self.store(), &ns, key)?,
            }
        }
        Ok(())
    }

    /// Remove `key` from every scalar index on `collection` after a delete.
    pub(crate) fn scalar_on_delete(&self, collection: &str, key: &[u8]) -> Result<()> {
        for field in self.scalar_fields(collection) {
            delete(self.store(), &namespace(collection, &field), key)?;
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
                if let Some(value) = doc.get(field) {
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
        insert(&store, "ix", b"doc1", &v).unwrap();
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
