//! Predicates (spec §4.5) — the 11 `corvid_pred_*` functions: ten
//! constructors over dotted field paths and `corvid_pred_free`.
//!
//! A `corvid_pred*` is a bare boxed `corvid::filter::Predicate` (the
//! handle plumbing lives in `crate::handle`). Rust counterparts are the
//! `corvid::field(path)` fluent builders (filter.rs `FieldRef`); paths
//! are dotted and traverse nested maps (`"meta.author"`), and an empty
//! path resolves nothing.
//!
//! # Ownership (spec §5 rule 4, §8)
//!
//! Path/value inputs at the constructors are borrowed-read and
//! **CLONED into the tree** — the caller keeps its value handle and may
//! free it immediately. The combinators `and`/`or`/`not` **consume
//! their argument(s) unconditionally** (the Task 3 discipline: even a
//! failed combine — NULL sibling, alias — has already taken the non-NULL
//! children), and `corvid_delete_where` (§4.8) plus Task 5's
//! `corvid_query_filter` consume their root. **Using or freeing a
//! consumed predicate is undefined behavior** (a double free);
//! `corvid_pred_free` is for never-consumed roots only.
//!
//! # NULL discipline (spec §7)
//!
//! The constructors return NULL + `CORVID_E_ARGUMENT` on a NULL path /
//! NULL value input, a NULL `values` array with `count > 0`, non-UTF-8
//! path or text argument, or an out-of-domain `corvid_cmp` opcode.
//! `corvid_pred_free(NULL)` is a no-op.

use std::ffi::c_char;

use corvid::CmpOp;
use corvid::filter::Predicate;

use crate::error::record_argument;
use crate::handle::corvid_pred;
use crate::handle::corvid_value;
use crate::handle::into_pred;
use crate::handle::reclaim_pred;
use crate::value::borrowed_utf8;
use crate::value::borrowed_value;

/// The comparison operator (FFI.md §1.4, frozen per §8): mirrors
/// `corvid::CmpOp` (filter.rs).
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_cmp {
    /// Equal (numeric Int/Float interop, else structural).
    CORVID_CMP_EQ = 0,
    /// Not equal.
    CORVID_CMP_NE = 1,
    /// Less than (numbers/text only).
    CORVID_CMP_LT = 2,
    /// Less or equal.
    CORVID_CMP_LE = 3,
    /// Greater than.
    CORVID_CMP_GT = 4,
    /// Greater or equal.
    CORVID_CMP_GE = 5,
}

/// Map an ABI opcode onto the engine operator, or `None` (having
/// recorded `CORVID_E_ARGUMENT`) when it is outside `EQ..=GE` — the
/// enum is frozen (§8), so an out-of-domain value is a caller bug, not
/// a future opcode. Validating the raw discriminant (not the enum)
/// keeps an out-of-domain integer from C a checked error instead of an
/// unspecified-match footgun: `repr(u32)` promises the bits, and a
/// wildcard on the integer is defined for every pattern (a Rust-side
/// invalid enum cannot even be constructed without UB, which is why
/// the test drives this by integer).
fn cmp_op(op: u32) -> Option<CmpOp> {
    match op {
        0 => Some(CmpOp::Eq),
        1 => Some(CmpOp::Ne),
        2 => Some(CmpOp::Lt),
        3 => Some(CmpOp::Le),
        4 => Some(CmpOp::Gt),
        5 => Some(CmpOp::Ge),
        _ => {
            record_argument("corvid_pred_compare: op is outside CORVID_CMP_EQ..=CORVID_CMP_GE");
            None
        }
    }
}

/// True when the path resolves to a present value (spec §4.5;
/// counterpart: `field(path).exists()` → `Predicate::Exists`). `path`
/// is borrowed, non-NULL at any length, valid UTF-8; the empty path
/// resolves nothing (a predicate that matches no document, not an
/// error). NULL or misencoded `path` returns NULL +
/// `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_exists(path: *const c_char, path_len: usize) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_exists", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::Exists(path.to_owned()))
}

/// Compare the path's value against a constant (spec §4.5; counterpart:
/// `field(path).eq/ne/lt/le/gt/ge(v)` → `Predicate::Compare`). `value`
/// is borrowed-read and **CLONED** into the tree — the caller keeps its
/// handle. Semantics (filter.rs): a missing path ⇒ false; unordered
/// kinds under ordered ops ⇒ false; `Int`/`Float` compare numerically
/// across kinds (exact to 2^53); NaN compares false against everything
/// except `NE`. NULL `value`, or an `op` outside `CORVID_CMP_EQ..=GE`,
/// returns NULL + `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_compare(
    path: *const c_char,
    path_len: usize,
    op: corvid_cmp,
    value: *const corvid_value,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_compare", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    let Some(op) = cmp_op(op as u32) else {
        return std::ptr::null_mut();
    };
    let Some(value) = borrowed_value("corvid_pred_compare", "value", value) else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::Compare {
        path: path.to_owned(),
        op,
        value: value.clone(),
    })
}

/// True when the value equals any element of `values` (spec §4.5;
/// counterpart: `field(path).is_in([...])` → `Predicate::In`). Each
/// element is borrowed-read and **CLONED**. `values` may be NULL only
/// when `count == 0` — an empty membership matches nothing (not an
/// error). A NULL element, or a NULL array with `count > 0`, returns
/// NULL + `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_in(
    path: *const c_char,
    path_len: usize,
    values: *const *const corvid_value,
    count: usize,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_in", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    let mut members = Vec::with_capacity(count);
    if count > 0 {
        if values.is_null() {
            record_argument("corvid_pred_in: values is NULL with count > 0");
            return std::ptr::null_mut();
        }
        for i in 0..count {
            // SAFETY: values is non-NULL (checked) and the caller
            // guarantees count readable pointers (spec §1.5's
            // array-input contract).
            let handle = unsafe { *values.add(i) };
            let Some(value) = borrowed_value("corvid_pred_in", "values[i]", handle) else {
                return std::ptr::null_mut();
            };
            members.push(value.clone());
        }
    }
    into_pred(Predicate::In {
        path: path.to_owned(),
        values: members,
    })
}

/// Inclusive `[low, high]` range (spec §4.5; counterpart:
/// `field(path).between(lo, hi)` → `Predicate::Between`). Both bounds
/// are required, borrowed-read, and **CLONED**. A NULL bound returns
/// NULL + `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_between(
    path: *const c_char,
    path_len: usize,
    low: *const corvid_value,
    high: *const corvid_value,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_between", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    let Some(low) = borrowed_value("corvid_pred_between", "low", low) else {
        return std::ptr::null_mut();
    };
    let Some(high) = borrowed_value("corvid_pred_between", "high", high) else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::Between {
        path: path.to_owned(),
        low: low.clone(),
        high: high.clone(),
    })
}

/// The text at `path` starts with `prefix` (spec §4.5; counterpart:
/// `field(path).starts_with(p)` → `Predicate::StartsWith`). False on
/// non-text values and missing paths. `prefix` is borrowed, non-NULL at
/// any length, valid UTF-8; NULL or misencoded returns NULL +
/// `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_starts_with(
    path: *const c_char,
    path_len: usize,
    prefix: *const c_char,
    prefix_len: usize,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_starts_with", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    let Some(prefix) = borrowed_utf8("corvid_pred_starts_with", "prefix", prefix, prefix_len)
    else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::StartsWith {
        path: path.to_owned(),
        prefix: prefix.to_owned(),
    })
}

/// The text at `path` contains `substr` (spec §4.5; counterpart:
/// `field(path).contains(s)` → `Predicate::Contains`). False on
/// non-text values and missing paths. `substr` is borrowed, non-NULL at
/// any length, valid UTF-8; NULL or misencoded returns NULL +
/// `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_contains(
    path: *const c_char,
    path_len: usize,
    substr: *const c_char,
    substr_len: usize,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_contains", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    let Some(substr) = borrowed_utf8("corvid_pred_contains", "substr", substr, substr_len) else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::Contains {
        path: path.to_owned(),
        substr: substr.to_owned(),
    })
}

/// The path holds a point (`[lat, lon]` array or `lat`/`lon` map)
/// within `radius_km` of `(lat, lon)` — inclusive, haversine (spec
/// §4.5; counterpart: `field(path).within_km(lat, lon, r)` →
/// `Predicate::GeoWithin`). False on non-point values and missing
/// paths. `path` as everywhere; the coordinates and radius cross by
/// value (no validation — a negative radius simply matches nothing, as
/// in the engine).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_geo_within(
    path: *const c_char,
    path_len: usize,
    lat: f64,
    lon: f64,
    radius_km: f64,
) -> *mut corvid_pred {
    let Some(path) = borrowed_utf8("corvid_pred_geo_within", "path", path, path_len) else {
        return std::ptr::null_mut();
    };
    into_pred(Predicate::GeoWithin {
        path: path.to_owned(),
        lat,
        lon,
        radius_km,
    })
}

/// Logical conjunction — **CONSUMES `a` and `b`** (spec §4.5/§5 rule 4;
/// counterpart: `Predicate::and` → `Predicate::And`). After the call the
/// children belong to the tree: freeing them, passing them again, or
/// otherwise using them is **undefined behavior** (a double free). A
/// NULL child fails the combine (NULL + `CORVID_E_ARGUMENT`) after
/// consuming the non-NULL sibling (spec §8's unconditional-consumption
/// discipline); `a == b` (aliasing one handle into both arms) is
/// rejected the same way, consuming the shared handle once.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_and(a: *mut corvid_pred, b: *mut corvid_pred) -> *mut corvid_pred {
    combine("corvid_pred_and", a, b, |a, b| {
        Predicate::And(Box::new(a), Box::new(b))
    })
}

/// Logical disjunction — **CONSUMES `a` and `b`** (spec §4.5;
/// counterpart: `Predicate::or` → `Predicate::Or`). The consumption,
/// NULL-child, and aliasing contracts are `corvid_pred_and`'s.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_or(a: *mut corvid_pred, b: *mut corvid_pred) -> *mut corvid_pred {
    combine("corvid_pred_or", a, b, |a, b| {
        Predicate::Or(Box::new(a), Box::new(b))
    })
}

/// Logical negation — **CONSUMES `a`** (spec §4.5; counterpart:
/// `std::ops::Not` → `Predicate::Not`). A NULL `a` fails (NULL +
/// `CORVID_E_ARGUMENT`); after the call the child belongs to the tree
/// (using or freeing it is UB).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_not(a: *mut corvid_pred) -> *mut corvid_pred {
    if a.is_null() {
        record_argument("corvid_pred_not: a is NULL");
        return std::ptr::null_mut();
    }
    // SAFETY: a is non-NULL (checked) and contractually an unconsumed
    // into_pred product; this call is its single consumption.
    let a = *unsafe { reclaim_pred(a) }.expect("non-NULL checked above");
    into_pred(Predicate::Not(Box::new(a)))
}

/// Free a **never-consumed root** (spec §4.5; counterpart: Rust `Drop`
/// of the tree). `corvid_pred_free(NULL)` is a no-op (§7). Predicates
/// handed to `corvid_pred_and/or/not` or `corvid_delete_where` (and,
/// from Task 5 on, `corvid_query_filter`) were consumed by that call —
/// freeing them too is a double free, **undefined behavior**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_pred_free(p: *mut corvid_pred) {
    // SAFETY: NULL is the documented no-op; otherwise p is contractually
    // an unconsumed into_pred product reclaimed exactly once here.
    drop(unsafe { reclaim_pred(p) });
}

/// The shared body of `and`/`or`: validate, consume unconditionally,
/// combine.
fn combine(
    fn_name: &str,
    a: *mut corvid_pred,
    b: *mut corvid_pred,
    mk: impl FnOnce(Predicate, Predicate) -> Predicate,
) -> *mut corvid_pred {
    if std::ptr::eq(a, b) {
        if a.is_null() {
            // Both NULL: nothing to consume; record once.
            record_argument(format!("{fn_name}: a and b are NULL").as_str());
            return std::ptr::null_mut();
        }
        record_argument(
            format!("{fn_name}: a and b alias (one handle cannot fill both arms)").as_str(),
        );
        // SAFETY: a is non-NULL (checked) and contractually an unconsumed
        // product; the aliased handle is consumed exactly once — through
        // a — so the rejection path cannot double-free.
        drop(unsafe { reclaim_pred(a) });
        return std::ptr::null_mut();
    }
    if a.is_null() || b.is_null() {
        let which = if a.is_null() { "a" } else { "b" };
        record_argument(format!("{fn_name}: {which} is NULL").as_str());
        // Consume the non-NULL sibling unconditionally (spec §8): the
        // failed combine has still taken it — free nothing afterwards.
        // SAFETY: reclaim_pred maps NULL to None, so this drops exactly
        // whichever sibling was non-NULL, once.
        drop(unsafe { reclaim_pred(a) });
        drop(unsafe { reclaim_pred(b) });
        return std::ptr::null_mut();
    }
    // SAFETY: both are non-NULL (checked) and contractually unconsumed
    // into_pred products; each is consumed exactly once here.
    let a = *unsafe { reclaim_pred(a) }.expect("non-NULL checked above");
    let b = *unsafe { reclaim_pred(b) }.expect("non-NULL checked above");
    into_pred(mk(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_err;
    use crate::error::corvid_status;
    use crate::error::last_code;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open_memory;
    use crate::mutation::corvid_delete_where;
    use crate::mutation::corvid_insert;
    use crate::read::corvid_len;
    use crate::value::corvid_value_array_new;
    use crate::value::corvid_value_array_push;
    use crate::value::corvid_value_bool;
    use crate::value::corvid_value_float;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;
    use crate::value::corvid_value_text;

    type Coll = *mut crate::handle::corvid_coll;

    // --- test helpers ------------------------------------------------------

    /// The observation channel for predicate semantics in Task 5's
    /// absence (the task brief's own recipe): seed `docs` through the
    /// ABI, `corvid_delete_where` the pred under test, and read the
    /// removal count — a real filtered query through the engine's
    /// index-aware delete path.
    fn removed_by(coll: Coll, pred: *mut corvid_pred) -> usize {
        let mut removed = usize::MAX;
        assert_eq!(
            corvid_delete_where(coll, pred, &mut removed),
            corvid_status::CORVID_OK
        );
        removed
    }

    /// A map document `{f0: v0, ...}` built through the ABI value
    /// builders (the engine's document shape).
    fn map_doc(fields: &[(&str, *mut corvid_value)]) -> *mut corvid_value {
        let map = corvid_value_map_new();
        for (name, v) in fields {
            assert_eq!(
                corvid_value_map_put(map, name.as_ptr() as *const c_char, name.len(), *v),
                corvid_status::CORVID_OK
            );
        }
        map
    }

    fn insert_doc(coll: Coll, key: &[u8], doc: *mut corvid_value) {
        assert_eq!(
            corvid_insert(coll, key.as_ptr(), key.len(), doc),
            corvid_status::CORVID_OK
        );
        corvid_value_free(doc); // insert CLONED it — the handle is spent
    }

    /// Seed one doc per (key, int field) — the numeric cases.
    fn seed_ints(coll: Coll, docs: &[(&[u8], i64)]) {
        for (key, n) in docs {
            let v = corvid_value_int(*n);
            insert_doc(coll, key, map_doc(&[("n", v)]));
        }
    }

    fn len_of(coll: Coll) -> usize {
        let mut n = usize::MAX;
        assert_eq!(corvid_len(coll, &mut n), corvid_status::CORVID_OK);
        n
    }

    fn pred_path(path: &str) -> (*const c_char, usize) {
        (path.as_ptr() as *const c_char, path.len())
    }

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let coll = corvid_collection(db, b"docs".as_ptr() as *const c_char, 4);
        assert!(!coll.is_null());
        (db, coll)
    }

    // --- §4.5 semantics through the ABI -------------------------------------

    #[test]
    fn exists_matches_present_fields_and_dotted_paths() {
        let (db, coll) = fresh();
        // {n: 1} and {meta: {author: "rocky"}}.
        seed_ints(coll, &[(b"a", 1)]);
        let author = corvid_value_text(b"rocky".as_ptr() as *const c_char, 5);
        let inner = map_doc(&[("author", author)]);
        let outer = map_doc(&[("meta", inner)]);
        insert_doc(coll, b"b", outer);

        // exists("n") removes the first only.
        let (p, pl) = pred_path("n");
        assert_eq!(removed_by(coll, corvid_pred_exists(p, pl)), 1);

        // A dotted path resolves the nested map.
        let (mp, mpl) = pred_path("meta.author");
        assert_eq!(removed_by(coll, corvid_pred_exists(mp, mpl)), 1);
        assert_eq!(len_of(coll), 0);

        // The empty path resolves nothing (filter.rs's rule).
        seed_ints(coll, &[(b"a", 1)]);
        let (ep, epl) = pred_path("");
        assert_eq!(removed_by(coll, corvid_pred_exists(ep, epl)), 0);
        // A path through a non-map descends nowhere.
        let (dp, dpl) = pred_path("n.deeper");
        assert_eq!(removed_by(coll, corvid_pred_exists(dp, dpl)), 0);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn every_compare_op_pins_filter_semantics() {
        let (db, coll) = fresh();
        let (p, pl) = pred_path("n");

        let reseed = |coll: Coll| seed_ints(coll, &[(b"a", 1), (b"b", 2), (b"c", 3)]);
        let case = |coll: Coll, op: corvid_cmp, rhs: i64, expect: usize| {
            let v = corvid_value_int(rhs);
            assert_eq!(
                removed_by(coll, corvid_pred_compare(p, pl, op, v)),
                expect,
                "{op:?} vs {rhs}"
            );
            corvid_value_free(v);
            reseed(coll);
            assert_eq!(len_of(coll), 3);
        };

        reseed(coll);
        case(coll, corvid_cmp::CORVID_CMP_EQ, 2, 1);
        case(coll, corvid_cmp::CORVID_CMP_NE, 2, 2);
        case(coll, corvid_cmp::CORVID_CMP_LT, 2, 1);
        case(coll, corvid_cmp::CORVID_CMP_LE, 2, 2);
        case(coll, corvid_cmp::CORVID_CMP_GT, 2, 1);
        case(coll, corvid_cmp::CORVID_CMP_GE, 2, 2);

        // Numeric interop: Int field vs Float bound (filter.rs).
        let f = corvid_value_float(1.5);
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_compare(p, pl, corvid_cmp::CORVID_CMP_GT, f)
            ),
            2,
            "2 and 3 are > 1.5"
        );
        corvid_value_free(f);
        reseed(coll);

        // Missing path: every op is false — even NE (filter.rs pins it).
        let (np, npl) = pred_path("absent");
        let one = corvid_value_int(1);
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_compare(np, npl, corvid_cmp::CORVID_CMP_NE, one)
            ),
            0
        );
        corvid_value_free(one);

        // Unordered kinds under an ordered op: false (bool > bool). The
        // doc's flag was consumed by map_doc; the pred gets its own.
        let flag = corvid_value_bool(1);
        insert_doc(coll, b"flag", map_doc(&[("f", flag)]));
        let (fp, fpl) = pred_path("f");
        let true_again = corvid_value_bool(1);
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_compare(fp, fpl, corvid_cmp::CORVID_CMP_GT, true_again)
            ),
            0
        );
        corvid_value_free(true_again);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn nan_compares_false_except_ne() {
        let (db, coll) = fresh();
        let nan_v = corvid_value_float(f64::NAN);
        insert_doc(coll, b"a", map_doc(&[("x", nan_v)])); // consumed by map_doc
        let (p, pl) = pred_path("x");

        let nan = corvid_value_float(f64::NAN);
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_compare(p, pl, corvid_cmp::CORVID_CMP_EQ, nan)
            ),
            0,
            "NaN eq NaN is false in filter.rs eval"
        );
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_compare(p, pl, corvid_cmp::CORVID_CMP_NE, nan)
            ),
            1,
            "NaN ne NaN is true"
        );
        corvid_value_free(nan);
        assert_eq!(len_of(coll), 0);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn in_between_text_and_geo_constructors() {
        let (db, coll) = fresh();
        let (p, pl) = pred_path("t");
        for (key, t) in [
            (b"a".as_slice(), "alpha"),
            (b"b".as_slice(), "beta"),
            (b"c".as_slice(), "gamma"),
        ] {
            // map_doc's map_put consumes v — no caller-side free.
            let v = corvid_value_text(t.as_ptr() as *const c_char, t.len());
            insert_doc(coll, key, map_doc(&[("t", v)]));
        }

        // in {alpha, gamma}: 2 removed; the CLONED members are the
        // caller's to free afterwards.
        let alpha = corvid_value_text(b"alpha".as_ptr() as *const c_char, 5);
        let gamma = corvid_value_text(b"gamma".as_ptr() as *const c_char, 5);
        let members: [*const corvid_value; 2] = [alpha, gamma];
        assert_eq!(
            removed_by(coll, corvid_pred_in(p, pl, members.as_ptr(), members.len())),
            2
        );
        corvid_value_free(alpha);
        corvid_value_free(gamma);
        assert_eq!(len_of(coll), 1, "beta survives");

        // count == 0 matches nothing; values may be NULL then.
        assert_eq!(
            removed_by(coll, corvid_pred_in(p, pl, std::ptr::null(), 0)),
            0
        );

        // starts_with on the survivor ("beta").
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_starts_with(p, pl, b"be".as_ptr() as *const c_char, 2)
            ),
            1
        );
        // contains on re-seeded text docs.
        for (key, t) in [(b"a".as_slice(), "alpha"), (b"b".as_slice(), "beta")] {
            let v = corvid_value_text(t.as_ptr() as *const c_char, t.len());
            insert_doc(coll, key, map_doc(&[("t", v)]));
        }
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_contains(p, pl, b"et".as_ptr() as *const c_char, 2)
            ),
            1,
            "only beta contains 'et'"
        );

        // Non-text field under a text pred: false.
        let seven = corvid_value_int(7);
        insert_doc(coll, b"n", map_doc(&[("num", seven)]));
        let (np, npl) = pred_path("num");
        assert_eq!(
            removed_by(
                coll,
                corvid_pred_starts_with(np, npl, b"7".as_ptr() as *const c_char, 1)
            ),
            0
        );

        // between, inclusive at both ends.
        seed_ints(coll, &[(b"a", 1), (b"b", 2), (b"c", 3)]);
        let (bp, bpl) = pred_path("n");
        let lo = corvid_value_int(2);
        let hi = corvid_value_int(2);
        assert_eq!(
            removed_by(coll, corvid_pred_between(bp, bpl, lo, hi)),
            1,
            "[2,2] removes exactly the 2 (both bounds inclusive)"
        );
        corvid_value_free(lo);
        corvid_value_free(hi);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn geo_within_matches_points_in_radius() {
        let (db, coll) = fresh();
        // loc: [lat, lon] arrays — London and NYC.
        for (key, lat, lon) in [
            (b"a".as_slice(), 51.5074, -0.1278),
            (b"b".as_slice(), 40.7128, -74.0060),
        ] {
            let loc = corvid_value_array_new();
            assert_eq!(
                corvid_value_array_push(loc, corvid_value_float(lat)),
                corvid_status::CORVID_OK
            );
            assert_eq!(
                corvid_value_array_push(loc, corvid_value_float(lon)),
                corvid_status::CORVID_OK
            );
            insert_doc(coll, key, map_doc(&[("loc", loc)]));
        }

        let (p, pl) = pred_path("loc");
        // Within 50 km of central London: a only.
        assert_eq!(
            removed_by(coll, corvid_pred_geo_within(p, pl, 51.5, -0.13, 50.0)),
            1
        );
        assert_eq!(len_of(coll), 1);
        // The survivor (NYC) is not within 1 km of (0,0).
        assert_eq!(
            removed_by(coll, corvid_pred_geo_within(p, pl, 0.0, 0.0, 1.0)),
            0
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn combinators_consume_and_evaluate() {
        let (db, coll) = fresh();
        // map_doc's map_put CONSUMES each value handle — nothing to
        // free on the caller's side afterwards.
        let one = corvid_value_int(1);
        let x = corvid_value_text(b"x".as_ptr() as *const c_char, 1);
        insert_doc(coll, b"a", map_doc(&[("n", one), ("t", x)]));
        let five = corvid_value_int(5);
        let y = corvid_value_text(b"y".as_ptr() as *const c_char, 1);
        insert_doc(coll, b"b", map_doc(&[("n", five), ("t", y)]));
        let nine = corvid_value_int(9);
        insert_doc(coll, b"c", map_doc(&[("n", nine)]));

        // and(n >= 2, exists t): only b.
        let (np, npl) = pred_path("n");
        let two = corvid_value_int(2);
        let left = corvid_pred_compare(np, npl, corvid_cmp::CORVID_CMP_GE, two);
        corvid_value_free(two);
        let (tp, tpl) = pred_path("t");
        let right = corvid_pred_exists(tp, tpl);
        let both = corvid_pred_and(left, right);
        assert!(!both.is_null());
        assert_eq!(removed_by(coll, both), 1);
        assert_eq!(len_of(coll), 2);

        // or(n == 1, n == 9): both survivors.
        let one = corvid_value_int(1);
        let l = corvid_pred_compare(np, npl, corvid_cmp::CORVID_CMP_EQ, one);
        corvid_value_free(one);
        let nine = corvid_value_int(9);
        let r = corvid_pred_compare(np, npl, corvid_cmp::CORVID_CMP_EQ, nine);
        corvid_value_free(nine);
        let either = corvid_pred_or(l, r);
        assert_eq!(removed_by(coll, either), 2);
        assert_eq!(len_of(coll), 0);

        // not: only the doc WITHOUT n goes.
        let one = corvid_value_int(1);
        insert_doc(coll, b"a", map_doc(&[("n", one)]));
        let z = corvid_value_text(b"z".as_ptr() as *const c_char, 1);
        insert_doc(coll, b"b", map_doc(&[("t", z)]));
        let inner = corvid_pred_exists(tp, tpl);
        assert_eq!(removed_by(coll, corvid_pred_not(inner)), 1);
        assert_eq!(len_of(coll), 1);

        // pred_free on a never-consumed root (the destructor's own path).
        let spare = corvid_pred_exists(tp, tpl);
        corvid_pred_free(spare);
        corvid_pred_free(std::ptr::null_mut()); // §7 no-op

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    // --- §7 discipline -------------------------------------------------------

    #[test]
    fn constructor_argument_errors() {
        let bad = [0xFF_u8, 0xFE];

        assert!(corvid_pred_exists(std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        assert!(
            corvid_pred_exists(bad.as_ptr() as *const c_char, bad.len()).is_null(),
            "non-UTF-8 path"
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // compare: NULL value; out-of-domain op.
        let (p, pl) = pred_path("n");
        let one = corvid_value_int(1);
        assert!(corvid_pred_compare(p, pl, corvid_cmp::CORVID_CMP_EQ, std::ptr::null()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        // An out-of-domain opcode: C passes any integer; the validation
        // runs on the raw discriminant (constructing an invalid enum
        // value in Rust would itself be UB — see cmp_op). Drive the
        // mapper by integer, then pin that the ABI site consults it.
        assert!(cmp_op(corvid_cmp::CORVID_CMP_GE as u32 + 7).is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        // The in-domain mapping itself, one opcode per arm.
        assert_eq!(cmp_op(0), Some(CmpOp::Eq));
        assert_eq!(cmp_op(1), Some(CmpOp::Ne));
        assert_eq!(cmp_op(2), Some(CmpOp::Lt));
        assert_eq!(cmp_op(3), Some(CmpOp::Le));
        assert_eq!(cmp_op(4), Some(CmpOp::Gt));
        assert_eq!(cmp_op(5), Some(CmpOp::Ge));
        assert_eq!(cmp_op(6), None);
        corvid_value_free(one);

        // in: NULL values with count > 0; NULL element.
        let arr: [*const corvid_value; 1] = [std::ptr::null()];
        assert!(corvid_pred_in(p, pl, std::ptr::null(), 2).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_pred_in(p, pl, arr.as_ptr(), 1).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // between: NULL bounds.
        let lo = corvid_value_int(0);
        assert!(corvid_pred_between(p, pl, std::ptr::null(), lo).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_pred_between(p, pl, lo, std::ptr::null()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_value_free(lo);

        // Text preds: NULL / non-UTF-8 needles.
        assert!(corvid_pred_starts_with(p, pl, std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_pred_contains(p, pl, bad.as_ptr() as *const c_char, 2).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn combinator_null_and_alias_shapes() {
        // NULL child: the non-NULL sibling is consumed unconditionally.
        let (p, pl) = pred_path("n");
        let a = corvid_pred_exists(p, pl);
        assert!(corvid_pred_and(std::ptr::null_mut(), a).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        let b = corvid_pred_exists(p, pl);
        assert!(corvid_pred_or(b, std::ptr::null_mut()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        assert!(corvid_pred_not(std::ptr::null_mut()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // Both NULL.
        assert!(corvid_pred_and(std::ptr::null_mut(), std::ptr::null_mut()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // Aliased arms: the shared handle is consumed once, rejected.
        let shared = corvid_pred_exists(p, pl);
        assert!(corvid_pred_and(shared, shared).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        // (Using `shared` again would be the documented double-free UB;
        // nothing here touches it afterwards.)

        // Value inputs are CLONED at construction: freeing the value
        // right after building leaves a working pred.
        let (db, coll) = fresh();
        seed_ints(coll, &[(b"a", 4), (b"b", 6)]);
        let five = corvid_value_int(5);
        let pred = corvid_pred_compare(p, pl, corvid_cmp::CORVID_CMP_GT, five);
        corvid_value_free(five); // gone before the pred is ever used
        assert_eq!(removed_by(coll, pred), 1);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    /// The §1.3-frozen opcode values, as the header carries them.
    #[test]
    fn cmp_values_are_frozen_to_the_spec() {
        assert_eq!(corvid_cmp::CORVID_CMP_EQ as u32, 0);
        assert_eq!(corvid_cmp::CORVID_CMP_NE as u32, 1);
        assert_eq!(corvid_cmp::CORVID_CMP_LT as u32, 2);
        assert_eq!(corvid_cmp::CORVID_CMP_LE as u32, 3);
        assert_eq!(corvid_cmp::CORVID_CMP_GT as u32, 4);
        assert_eq!(corvid_cmp::CORVID_CMP_GE as u32, 5);
    }
}
