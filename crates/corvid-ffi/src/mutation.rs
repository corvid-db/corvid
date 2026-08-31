//! Mutations (spec §4.8) — the 13 write functions.
//!
//! All wrap `corvid::Collection` methods (db.rs / ttl.rs). `const
//! corvid_value*` document inputs are borrowed-read and handed to the
//! engine by reference — the ownership reading of §5's "CLONED into the
//! engine": the caller keeps its handle (and may free it immediately),
//! because the engine encodes its own copy inside the write
//! transaction. `corvid_update` crosses a C fn-ptr callback (spec
//! §1.6's no-reentrancy contract), CAS `expected`/`replacement` are
//! nullable with semantics, `delete_where` consumes its predicate, and
//! the TTL trio takes caller-supplied epochs (the engine keeps no
//! clock; expiry is `<= now` inclusive).
//!
//! # NULL discipline (spec §7)
//!
//! `coll`, keys, and documents are non-NULL (keys at any length — the
//! empty key is legal); failures return `CORVID_ERR` with
//! `CORVID_E_ARGUMENT` (or the mapped engine code) recorded. The pure
//! out-params (`existed_out`, `removed_out`, `purged_out`,
//! `applied_out`, `expires_at_out`, `has_ttl`, `key_len_out`) are
//! nullable — the call proceeds and simply writes nothing. On any
//! failure the out-params are left untouched.

use std::ffi::c_void;

use corvid::Value;

use crate::error::corvid_err;
use crate::error::corvid_status;
use crate::error::guard;
use crate::error::record;
use crate::error::record_argument;
use crate::handle::borrow_coll;
use crate::handle::corvid_coll;
use crate::handle::corvid_pred;
use crate::handle::corvid_value;
use crate::handle::reclaim_pred;
use crate::handle::reclaim_value;
use crate::lifecycle::buffer_new;
use crate::value::borrowed_bytes;
use crate::value::borrowed_value;

/// One `(key, value)` pair for bulk inserts (spec §1.2, POD): the input
/// shape of [`corvid_put_many`]. `key` is non-NULL (any length — the
/// empty key is legal); `val` is non-NULL and CLONED by the call, so
/// the caller keeps ownership of every value handle in the array.
#[repr(C)]
pub struct corvid_kv {
    /// The row's key, borrowed for the call.
    pub key: *const u8,
    /// Key bytes.
    pub key_len: usize,
    /// The document, borrowed-read and CLONED into the engine.
    pub val: *const corvid_value,
}

/// `corvid_update`'s read-modify-write closure (spec §1.6).
///
/// `current` is NULL when the key is absent (a missing document is not
/// an error); it is BORROWED and valid only inside the callback.
/// On success set `*out` to an OWNED `corvid_value*` (consumed by the
/// call) or leave it NULL to delete the key. Return `CORVID_OK` to
/// apply, any other value to abort (then `*out` must be NULL — nothing
/// is consumed).
///
/// **Reentrancy (spec §1.6):** the callback runs on the caller's
/// thread between engine operations. It MUST NOT issue further writes
/// to the same database, MUST NOT free or mutate the borrowed
/// arguments, and SHOULD NOT make other corvid calls at all — the
/// portable contract is "no reentrant corvid calls". Violating it
/// (notably calling into the same db handle from inside the callback)
/// is undefined behavior or a deadlock, not a checked error.
#[allow(non_camel_case_types)] // C ABI name, emitted verbatim by cbindgen
pub type corvid_update_fn = Option<
    extern "C" fn(
        ctx: *mut c_void,
        current: *const corvid_value,
        out: *mut *mut corvid_value,
    ) -> corvid_status,
>;

/// Insert or overwrite the document at `key` (spec §4.8; counterpart:
/// `Collection::insert`) — atomic with all index maintenance and
/// unique checks. `doc` is borrowed-read (the engine encodes its own
/// copy; the caller keeps the handle). Reserved/invalid collection
/// names fail here with `CORVID_E_RESERVED_COLLECTION` /
/// `CORVID_E_INVALID_NAME` (the write-time name gate).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_insert(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    doc: *const corvid_value,
) -> corvid_status {
    let (Some(coll), Some(key), Some(doc)) = (
        borrow_coll_checked("corvid_insert", "c", c),
        borrowed_bytes("corvid_insert", "key", key, key_len),
        borrowed_value("corvid_insert", "doc", doc),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_insert", || coll.collection().insert(key, doc)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Single-transaction bulk load (spec §4.8; counterpart:
/// `Collection::insert_batch`): one commit instead of N; the whole
/// batch rolls back on a schema/unique violation; duplicate keys inside
/// one batch follow last-write-wins. `items` is an array of `count`
/// [`corvid_kv`] PODs (borrowed for the call; every `val` CLONED) —
/// NULL `items` is legal only with `count == 0` (an empty batch is a
/// successful no-op).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_put_many(
    c: *mut corvid_coll,
    items: *const corvid_kv,
    count: usize,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_put_many", "c", c) else {
        return corvid_status::CORVID_ERR;
    };
    // Validate the whole array before touching the engine: every key and
    // value must be present (a NULL element fails the batch before any
    // transaction opens — the engine would roll it back anyway, but the
    // argument error is the ABI's to report).
    let mut pairs = Vec::with_capacity(count);
    if count > 0 {
        if items.is_null() {
            record_argument("corvid_put_many: items is NULL with count > 0");
            return corvid_status::CORVID_ERR;
        }
        for i in 0..count {
            // SAFETY: items is non-NULL (checked) and the caller
            // guarantees count readable corvid_kv structs (§1.2's POD
            // array contract).
            let kv = unsafe { &*items.add(i) };
            let (Some(key), Some(val)) = (
                borrowed_bytes("corvid_put_many", "items[i].key", kv.key, kv.key_len),
                borrowed_value("corvid_put_many", "items[i].val", kv.val),
            ) else {
                return corvid_status::CORVID_ERR;
            };
            pairs.push((key, val));
        }
    }
    match guard("corvid_put_many", || coll.collection().insert_batch(&pairs)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Insert under a fresh, monotonically increasing zero-padded 20-digit
/// key (spec §4.8; counterpart: `Collection::insert_auto ->
/// Vec<u8>`). Returns the key bytes — **free with `corvid_free`** —
/// with the length in `*key_len_out` (nullable, like §7's other
/// len_outs: the buffer's hidden header is what `corvid_free` needs,
/// so a NULL out is tolerable). NULL + error on failure; a failed
/// insert does not burn an id (the engine reserves the counter inside
/// the insert transaction, audit C9). `doc` as `corvid_insert`'s.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_insert_auto(
    c: *mut corvid_coll,
    doc: *const corvid_value,
    key_len_out: *mut usize,
) -> *mut u8 {
    let (Some(coll), Some(doc)) = (
        borrow_coll_checked("corvid_insert_auto", "c", c),
        borrowed_value("corvid_insert_auto", "doc", doc),
    ) else {
        return std::ptr::null_mut();
    };
    let Some(key) = guard("corvid_insert_auto", || coll.collection().insert_auto(doc)) else {
        return std::ptr::null_mut();
    };
    let buffer = buffer_new(&key);
    if buffer.is_null() {
        // Allocation failure (theoretic): the write already committed,
        // but the caller cannot learn the key — report the failure.
        record(
            corvid_err::CORVID_E_DATABASE,
            "corvid_insert_auto: allocating the key buffer failed",
        );
        return std::ptr::null_mut();
    }
    if !key_len_out.is_null() {
        // SAFETY: key_len_out is non-NULL (checked).
        unsafe { *key_len_out = key.len() };
    }
    buffer
}

/// Read-modify-write `key` via callback (spec §4.8/§1.6; counterpart:
/// `Collection::update(key, f)` with `F: FnOnce(Option<Value>) ->
/// Option<Value>`). `fn` receives the current document (borrowed;
/// NULL when absent — not an error) and produces the replacement
/// (OWNED, consumed) or a deletion (`*out` left NULL). An aborting
/// callback (any non-`CORVID_OK` return) fails this call with
/// `CORVID_E_ARGUMENT` and a message noting the abort — nothing is
/// written, and a non-NULL `*out` on the abort path is left untouched
/// (the contract requires NULL there; a violating caller keeps
/// ownership of whatever it stored).
///
/// The engine method is get-then-write; this wrapper inlines that same
/// shape (get, callback, insert-or-delete through the engine's own
/// methods) because the engine closure type has no abort channel — the
/// semantics are the engine's, with the abort leaving the store
/// untouched. One divergence, an honest boundary: the engine's
/// `update` runs `ensure_writable` (db.rs) BEFORE the closure, so on an
/// unwritable collection name its closure never runs, while this
/// wrapper reaches the check only at the write half — AFTER the
/// callback (the get half is a legal read on any name; through today's
/// ABI the final store state is identical, nothing written either
/// way). **Not linearizable** against concurrent writers (same as the
/// engine's); use `corvid_compare_and_set` when that matters.
/// **Reentrancy:** the callback MUST NOT call into the same database
/// (writes especially) — see [`corvid_update_fn`]; that is UB or a
/// deadlock, not a checked error.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_update(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    fn_: corvid_update_fn,
    ctx: *mut c_void,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_update", "c", c) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(key) = borrowed_bytes("corvid_update", "key", key, key_len) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(fn_) = fn_ else {
        record_argument("corvid_update: fn is NULL");
        return corvid_status::CORVID_ERR;
    };

    // The get half of the engine's update.
    let Some(current) = guard("corvid_update", || coll.collection().get(key)) else {
        return corvid_status::CORVID_ERR;
    };
    let current_ptr = current.as_ref().map_or(std::ptr::null(), |v| {
        v as *const Value as *const corvid_value
    });
    let mut out: *mut corvid_value = std::ptr::null_mut();
    // SAFETY: fn_ is non-NULL (checked) and contractually a valid
    // callback; current_ptr borrows our local `current` (valid for the
    // call, per §1.6's callback-scoped borrow) and out is a valid,
    // writable out-param. catch_unwind keeps a panicking callback from
    // unwinding across the C boundary (spec §3's defensive rule).
    let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fn_(ctx, current_ptr, &mut out)
    }));
    let status = match status {
        Ok(status) => status,
        Err(payload) => {
            record(
                corvid_err::CORVID_E_DATABASE,
                format!(
                    "corvid_update: callback panicked: {}",
                    crate::error::panic_text(&payload)
                ),
            );
            return corvid_status::CORVID_ERR;
        }
    };
    if status != corvid_status::CORVID_OK {
        record(
            corvid_err::CORVID_E_ARGUMENT,
            format!(
                "corvid_update: callback aborted (returned {status:?}); \
                 nothing was written"
            ),
        );
        return corvid_status::CORVID_ERR;
    }

    // The write half: NULL out deletes, non-NULL is consumed as the
    // replacement.
    if out.is_null() {
        match guard("corvid_update", || coll.collection().delete(key)) {
            Some(_) => corvid_status::CORVID_OK,
            None => corvid_status::CORVID_ERR,
        }
    } else {
        // SAFETY: out is non-NULL (checked) and contractually an OWNED
        // handle produced by the callback for this call — reclaiming it
        // is the documented consumption.
        let doc = *unsafe { reclaim_value(out) }.expect("non-NULL checked above");
        match guard("corvid_update", || coll.collection().insert(key, &doc)) {
            Some(()) => corvid_status::CORVID_OK,
            None => corvid_status::CORVID_ERR,
        }
    }
}

/// Merge `patch`'s top-level fields into the map at `key` (creating it
/// if absent); a non-map on either side replaces the document with
/// `patch` (spec §4.8; counterpart: `Collection::patch`). `patch` is
/// borrowed-read as everywhere.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_patch(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    patch: *const corvid_value,
) -> corvid_status {
    let (Some(coll), Some(key), Some(patch)) = (
        borrow_coll_checked("corvid_patch", "c", c),
        borrowed_bytes("corvid_patch", "key", key, key_len),
        borrowed_value("corvid_patch", "patch", patch),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_patch", || coll.collection().patch(key, patch)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Atomic conditional write (spec §4.8; counterpart:
/// `Collection::compare_and_set(key, Option<&Value>, Option<Value>) ->
/// bool`). **Both value parameters are nullable, and nullability is
/// semantic**: `expected == NULL` means "must be absent";
/// `replacement == NULL` means "delete if it matches". `*applied_out`
/// (nullable) is 1 when applied, 0 when the compare failed — which is
/// `CORVID_OK`, NOT an error. Equality is the engine's semantic value
/// equality (`schema::unique_value_eq`): `NaN == NaN` regardless of
/// payload, `-0.0 == 0.0`, containers element-wise. The `replacement`
/// is cloned for the engine (the caller keeps its handle); `expected`
/// is borrowed-read.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_compare_and_set(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    expected: *const corvid_value,
    replacement: *const corvid_value,
    applied_out: *mut i32,
) -> corvid_status {
    let (Some(coll), Some(key)) = (
        borrow_coll_checked("corvid_compare_and_set", "c", c),
        borrowed_bytes("corvid_compare_and_set", "key", key, key_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    // Nullable-with-semantics (§7): NULL expected = must-be-absent; the
    // pointer, when non-NULL, must still be a valid value handle.
    let expected = if expected.is_null() {
        None
    } else {
        match borrowed_value("corvid_compare_and_set", "expected", expected) {
            Some(v) => Some(v),
            None => return corvid_status::CORVID_ERR,
        }
    };
    let replacement = if replacement.is_null() {
        None
    } else {
        match borrowed_value("corvid_compare_and_set", "replacement", replacement) {
            Some(v) => Some(v.clone()), // §5 rule 3: the engine consumes its own copy
            None => return corvid_status::CORVID_ERR,
        }
    };
    match guard("corvid_compare_and_set", || {
        coll.collection()
            .compare_and_set(key, expected, replacement)
    }) {
        Some(applied) => {
            if !applied_out.is_null() {
                // SAFETY: applied_out is non-NULL (checked).
                unsafe { *applied_out = i32::from(applied) };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Remove the document at `key` (spec §4.8; counterpart:
/// `Collection::delete -> bool`): `*existed_out` (nullable) is 1 when a
/// document was removed, 0 when the key held none. Deleting cascades
/// the key's graph edges in the same transaction — including edges
/// dangling on a key that never existed as a document (the engine's
/// delete-of-absent still cleans edges).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_delete(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    existed_out: *mut i32,
) -> corvid_status {
    let (Some(coll), Some(key)) = (
        borrow_coll_checked("corvid_delete", "c", c),
        borrowed_bytes("corvid_delete", "key", key, key_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_delete", || coll.collection().delete(key)) {
        Some(existed) => {
            if !existed_out.is_null() {
                // SAFETY: existed_out is non-NULL (checked).
                unsafe { *existed_out = i32::from(existed) };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Delete every document matching `pred` (spec §4.8; counterpart:
/// `Collection::delete_where(Predicate) -> usize`) — **CONSUMES `pred`**
/// (index-accelerated matching through the engine's query path). `pred`
/// is required; it is consumed unconditionally, whatever the status
/// (spec §8) — using or freeing it afterwards is UB. `*removed_out`
/// (nullable) receives the number removed.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_delete_where(
    c: *mut corvid_coll,
    pred: *mut corvid_pred,
    removed_out: *mut usize,
) -> corvid_status {
    if pred.is_null() {
        record_argument("corvid_delete_where: pred is NULL");
        return corvid_status::CORVID_ERR;
    }
    // Consume FIRST (spec §8's unconditional-consumption discipline —
    // the Task 3 precedent): every failure path below has already taken
    // the predicate.
    // SAFETY: pred is non-NULL (checked) and contractually an unconsumed
    // into_pred product; this call is its single consumption.
    let pred = *unsafe { reclaim_pred(pred) }.expect("non-NULL checked above");
    let Some(coll) = borrow_coll_checked("corvid_delete_where", "c", c) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_delete_where", || {
        coll.collection().delete_where(pred)
    }) {
        Some(removed) => {
            if !removed_out.is_null() {
                // SAFETY: removed_out is non-NULL (checked).
                unsafe { *removed_out = removed };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Delete each of `keys` (spec §4.8; counterpart:
/// `Collection::delete_batch(&[&[u8]]) -> usize`); `*removed_out`
/// (nullable) counts how many existed. `keys`/`key_lens` are parallel
/// borrowed arrays, non-NULL when `count > 0` (`count == 0` with NULL
/// arrays is a successful no-op). Each delete cascades that key's graph
/// edges, as `corvid_delete`'s.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_delete_batch(
    c: *mut corvid_coll,
    keys: *const *const u8,
    key_lens: *const usize,
    count: usize,
    removed_out: *mut usize,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_delete_batch", "c", c) else {
        return corvid_status::CORVID_ERR;
    };
    let mut slices = Vec::with_capacity(count);
    if count > 0 {
        if keys.is_null() || key_lens.is_null() {
            record_argument("corvid_delete_batch: keys/key_lens is NULL with count > 0");
            return corvid_status::CORVID_ERR;
        }
        for i in 0..count {
            // SAFETY: keys/key_lens are non-NULL (checked) and the caller
            // guarantees count readable elements in each (§1.5's parallel
            // array contract).
            let (key, len) = unsafe { (*keys.add(i), *key_lens.add(i)) };
            let Some(key) = borrowed_bytes("corvid_delete_batch", "keys[i]", key, len) else {
                return corvid_status::CORVID_ERR;
            };
            slices.push(key);
        }
    }
    match guard("corvid_delete_batch", || {
        coll.collection().delete_batch(&slices)
    }) {
        Some(removed) => {
            if !removed_out.is_null() {
                // SAFETY: removed_out is non-NULL (checked).
                unsafe { *removed_out = removed };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Insert `doc` at `key` with expiry `expires_at` (spec §4.8;
/// counterpart: `Collection::insert_with_ttl`) — the row and its expiry
/// commit atomically. `expires_at` is in the caller's epoch (the engine
/// keeps no clock); the record behaves normally until purged.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_insert_with_ttl(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    doc: *const corvid_value,
    expires_at: i64,
) -> corvid_status {
    let (Some(coll), Some(key), Some(doc)) = (
        borrow_coll_checked("corvid_insert_with_ttl", "c", c),
        borrowed_bytes("corvid_insert_with_ttl", "key", key, key_len),
        borrowed_value("corvid_insert_with_ttl", "doc", doc),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_insert_with_ttl", || {
        coll.collection().insert_with_ttl(key, doc, expires_at)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Set (or replace) `key`'s expiry without rewriting the document
/// (spec §4.8; counterpart: `Collection::set_ttl`). Setting an expiry
/// on an absent key records the expiry anyway (the engine's TTL index
/// is key-addressed); the purge's compare-expiry re-verification keeps
/// it harmless.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_set_ttl(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    expires_at: i64,
) -> corvid_status {
    let (Some(coll), Some(key)) = (
        borrow_coll_checked("corvid_set_ttl", "c", c),
        borrowed_bytes("corvid_set_ttl", "key", key, key_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_set_ttl", || {
        coll.collection().set_ttl(key, expires_at)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// `key`'s expiry, if one is set (spec §4.8; counterpart:
/// `Collection::ttl -> Option<i64>`). `*has_ttl` (nullable) is 1/0 —
/// unset is NOT an error; `*expires_at_out` (nullable) carries the
/// timestamp when set and 0 when not. A plain (non-TTL) write clears a
/// previously set expiry — the engine clears it in the write
/// transaction.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_get_ttl(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    expires_at_out: *mut i64,
    has_ttl: *mut i32,
) -> corvid_status {
    let (Some(coll), Some(key)) = (
        borrow_coll_checked("corvid_get_ttl", "c", c),
        borrowed_bytes("corvid_get_ttl", "key", key, key_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_get_ttl", || coll.collection().ttl(key)) {
        Some(ttl) => {
            let (has, at) = match ttl {
                Some(ts) => (1, ts),
                None => (0, 0),
            };
            if !has_ttl.is_null() {
                // SAFETY: has_ttl is non-NULL (checked).
                unsafe { *has_ttl = has };
            }
            if !expires_at_out.is_null() {
                // SAFETY: expires_at_out is non-NULL (checked).
                unsafe { *expires_at_out = at };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Delete every record whose expiry is `<= now` — **inclusive** (spec
/// §4.8; counterpart: `Collection::purge_expired(now) -> usize`);
/// `*purged_out` (nullable) receives the count. `now` is the caller's
/// epoch. Records are removed through the normal delete path (indexes
/// stay consistent); each candidate's expiry is re-verified inside the
/// delete transaction, so a rewritten record is skipped, never purged.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_purge_expired(
    c: *mut corvid_coll,
    now: i64,
    purged_out: *mut usize,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_purge_expired", "c", c) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_purge_expired", || {
        coll.collection().purge_expired(now)
    }) {
        Some(purged) => {
            if !purged_out.is_null() {
                // SAFETY: purged_out is non-NULL (checked).
                unsafe { *purged_out = purged };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// The §7 NULL-checked coll borrow shared by every function above:
/// `None` (having recorded `CORVID_E_ARGUMENT`) on a NULL handle.
// SAFETY-free wrapper: borrow_coll is the unsafe half, documented in
// handle.rs; this adds only the §7 check.
fn borrow_coll_checked<'a>(
    fn_name: &str,
    param: &str,
    c: *mut corvid_coll,
) -> Option<&'a crate::handle::CollHandle> {
    if c.is_null() {
        record_argument(format!("{fn_name}: {param} is NULL").as_str());
        return None;
    }
    // SAFETY: c is non-NULL (checked) and contractually a live
    // corvid_collection product; the coll family is thread-safe (spec
    // §2), so a shared borrow is sound.
    unsafe { borrow_coll(c) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_status::CORVID_ERR;
    use crate::error::corvid_status::CORVID_OK;
    use crate::error::last_code;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_free;
    use crate::lifecycle::corvid_open_memory;
    use crate::read::corvid_get;
    use crate::read::corvid_len;
    use crate::value::corvid_value_float;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;
    use crate::value::corvid_value_text;

    type Coll = *mut corvid_coll;

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let coll = corvid_collection(db, b"docs".as_ptr() as *const std::ffi::c_char, 4);
        assert!(!coll.is_null());
        (db, coll)
    }

    fn int_doc(coll: Coll, key: &[u8], n: i64) {
        let v = corvid_value_int(n);
        assert_eq!(corvid_insert(coll, key.as_ptr(), key.len(), v), CORVID_OK);
        corvid_value_free(v);
    }

    fn get_int(coll: Coll, key: &[u8]) -> Option<i64> {
        let mut out: *mut corvid_value = std::ptr::null_mut();
        assert_eq!(
            corvid_get(coll, key.as_ptr(), key.len(), &mut out),
            CORVID_OK
        );
        if out.is_null() {
            return None;
        }
        let mut ok = 0;
        let n = crate::value::corvid_value_as_int(out, &mut ok);
        corvid_value_free(out);
        (ok == 1).then_some(n)
    }

    fn len_of(coll: Coll) -> usize {
        let mut n = usize::MAX;
        assert_eq!(corvid_len(coll, &mut n), CORVID_OK);
        n
    }

    // A callback that increments (current seen?) and returns a fixed
    // replacement — the shapes the update tests need.
    struct UpdateCtx {
        saw_current: bool,
        make: fn() -> *mut corvid_value,
    }

    extern "C" fn update_replace(
        ctx: *mut c_void,
        current: *const corvid_value,
        out: *mut *mut corvid_value,
    ) -> corvid_status {
        // SAFETY: ctx is the test's own UpdateCtx box, current is the
        // borrowed current doc (NULL when absent), out is our out-param.
        let ctx = unsafe { &mut *(ctx as *mut UpdateCtx) };
        ctx.saw_current = !current.is_null();
        // SAFETY: out is a valid out-param per the callback contract.
        unsafe { *out = (ctx.make)() };
        CORVID_OK
    }

    extern "C" fn update_delete(
        _ctx: *mut c_void,
        _current: *const corvid_value,
        out: *mut *mut corvid_value,
    ) -> corvid_status {
        // SAFETY: out is a valid out-param; leaving it NULL deletes.
        unsafe { *out = std::ptr::null_mut() };
        CORVID_OK
    }

    extern "C" fn update_abort(
        _ctx: *mut c_void,
        _current: *const corvid_value,
        _out: *mut *mut corvid_value,
    ) -> corvid_status {
        CORVID_ERR
    }

    fn make_five() -> *mut corvid_value {
        corvid_value_int(5)
    }

    // --- insert / names / put_many ------------------------------------------

    #[test]
    fn insert_get_and_overwrite_round_trip() {
        let (db, coll) = fresh();
        int_doc(coll, b"k", 1);
        assert_eq!(get_int(coll, b"k"), Some(1));
        int_doc(coll, b"k", 2); // overwrite
        assert_eq!(get_int(coll, b"k"), Some(2));

        // The empty key is legal (§1.5).
        int_doc(coll, b"", 7);
        assert_eq!(get_int(coll, b""), Some(7));
        assert_eq!(len_of(coll), 2);

        // NULL discipline.
        assert_eq!(
            corvid_insert(std::ptr::null_mut(), b"k".as_ptr(), 1, corvid_value_int(1)),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_insert(coll, std::ptr::null(), 0, corvid_value_int(1)),
            CORVID_ERR
        );
        assert_eq!(
            corvid_insert(coll, b"k".as_ptr(), 1, std::ptr::null()),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    #[test]
    fn put_many_is_atomic_last_write_wins_and_fast() {
        let (db, coll) = fresh();
        let v1 = corvid_value_int(1);
        let v2 = corvid_value_int(2);
        let v3 = corvid_value_int(3);
        let v2b = corvid_value_int(22);
        let kvs: [corvid_kv; 4] = [
            corvid_kv {
                key: b"a".as_ptr(),
                key_len: 1,
                val: v1,
            },
            corvid_kv {
                key: b"b".as_ptr(),
                key_len: 1,
                val: v2,
            },
            corvid_kv {
                key: b"c".as_ptr(),
                key_len: 1,
                val: v3,
            },
            // Duplicate key inside one batch: last write wins.
            corvid_kv {
                key: b"b".as_ptr(),
                key_len: 1,
                val: v2b,
            },
        ];
        assert_eq!(
            corvid_put_many(coll, kvs.as_ptr(), kvs.len()),
            CORVID_OK,
            "one transaction, four rows, three distinct keys"
        );
        for v in [&v1, &v2, &v3, &v2b] {
            corvid_value_free(*v); // the batch CLONED them
        }
        assert_eq!(len_of(coll), 3);
        assert_eq!(
            get_int(coll, b"b"),
            Some(22),
            "duplicate key: last write wins"
        );

        // Empty batch (NULL items, count 0): a successful no-op.
        assert_eq!(corvid_put_many(coll, std::ptr::null(), 0), CORVID_OK);
        assert_eq!(len_of(coll), 3);
        // NULL items with count > 0: the argument error.
        assert_eq!(corvid_put_many(coll, std::ptr::null(), 2), CORVID_ERR);
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        // A NULL val inside the array fails the batch up front.
        let bad = [corvid_kv {
            key: b"z".as_ptr(),
            key_len: 1,
            val: std::ptr::null(),
        }];
        assert_eq!(corvid_put_many(coll, bad.as_ptr(), 1), CORVID_ERR);
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(len_of(coll), 3, "the failed batches wrote nothing");

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// Whole-batch rollback on a mid-batch violation, pinned through the
    /// ABI. The schema itself is declared through the ENGINE API in the
    /// test setup (its ABI surface — `corvid_set_schema` — lands with
    /// Task 6); the violation and the rollback are observed entirely
    /// through `corvid_put_many` / `corvid_len`.
    #[test]
    fn put_many_rolls_back_whole_batch_on_violation() {
        let (db, coll) = fresh();
        // Engine-side setup: unique field u.
        {
            // SAFETY: db is a live open_memory product; single-threaded
            // test borrow.
            let handle = unsafe { crate::handle::borrow_db(db) }.unwrap();
            let schema = corvid::schema::Schema::new()
                .field(corvid::schema::Field::new("u", corvid::schema::FieldType::Int).unique());
            handle
                .engine()
                .collection("docs")
                .set_schema(&schema)
                .unwrap();
        }

        let good = corvid_value_map_new();
        let u1 = corvid_value_int(1);
        assert_eq!(
            corvid_value_map_put(good, b"u".as_ptr() as *const std::ffi::c_char, 1, u1),
            CORVID_OK
        );
        let dup_a = corvid_value_clone_abi(good);
        let dup_b = corvid_value_clone_abi(good);
        // The batch: two distinct keys but the same unique value.
        let kvs: [corvid_kv; 2] = [
            corvid_kv {
                key: b"a".as_ptr(),
                key_len: 1,
                val: dup_a,
            },
            corvid_kv {
                key: b"b".as_ptr(),
                key_len: 1,
                val: dup_b,
            },
        ];
        assert_eq!(corvid_put_many(coll, kvs.as_ptr(), kvs.len()), CORVID_ERR);
        assert_eq!(
            last_code(),
            crate::error::corvid_err::CORVID_E_SCHEMA_VIOLATION,
            "unique violations surface as the schema-violation code"
        );
        assert_eq!(len_of(coll), 0, "the WHOLE batch rolled back");
        corvid_value_free(dup_a); // the failed batch cloned, never consumed
        corvid_value_free(dup_b);

        // A clean single-item batch lands; then the same value again
        // (fresh key) violates.
        let kvs: [corvid_kv; 1] = [corvid_kv {
            key: b"a".as_ptr(),
            key_len: 1,
            val: good,
        }];
        assert_eq!(corvid_put_many(coll, kvs.as_ptr(), kvs.len()), CORVID_OK);
        assert_eq!(len_of(coll), 1);

        let dup = corvid_value_map_new();
        let u1b = corvid_value_int(1);
        assert_eq!(
            corvid_value_map_put(dup, b"u".as_ptr() as *const std::ffi::c_char, 1, u1b),
            CORVID_OK
        );
        let other = corvid_value_map_new();
        let u9 = corvid_value_int(9);
        assert_eq!(
            corvid_value_map_put(other, b"u".as_ptr() as *const std::ffi::c_char, 1, u9),
            CORVID_OK
        );
        // Mid-batch: a fine row, then the violating row.
        let kvs: [corvid_kv; 2] = [
            corvid_kv {
                key: b"z".as_ptr(),
                key_len: 1,
                val: other,
            },
            corvid_kv {
                key: b"b".as_ptr(),
                key_len: 1,
                val: dup,
            },
        ];
        assert_eq!(corvid_put_many(coll, kvs.as_ptr(), kvs.len()), CORVID_ERR);
        assert_eq!(len_of(coll), 1, "the earlier fine row rolled back too");
        assert_eq!(get_int(coll, b"z"), None);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    #[test]
    fn insert_auto_returns_zero_padded_monotonic_freeable_keys() {
        let (db, coll) = fresh();
        let doc = corvid_value_int(10);
        let mut len = usize::MAX;
        let k0 = corvid_insert_auto(coll, doc, &mut len);
        assert!(!k0.is_null());
        assert_eq!(len, 20, "zero-padded 20-digit key");
        // SAFETY: k0 is a buffer_new product with len readable bytes.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(k0, len) },
            b"00000000000000000000"
        );
        corvid_free(k0 as *mut c_void); // the ABI buffer destructor

        let k1 = corvid_insert_auto(coll, doc, std::ptr::null_mut()); // len_out nullable
        assert!(!k1.is_null());
        // SAFETY: same provenance; 20-byte keys sort in id order.
        let k1_bytes = unsafe { std::slice::from_raw_parts(k1, 20) };
        assert_eq!(k1_bytes, b"00000000000000000001", "monotonic");
        corvid_free(k1 as *mut c_void);
        corvid_value_free(doc);

        // A failed insert does not burn an id (engine audit C9): a
        // schema violation (declared engine-side — Task 6 owns the ABI)
        // leaves the counter untouched.
        {
            // SAFETY: live db handle, single-threaded test borrow.
            let handle = unsafe { crate::handle::borrow_db(db) }.unwrap();
            let schema = corvid::schema::Schema::new()
                .field(corvid::schema::Field::new("n", corvid::schema::FieldType::Int).required());
            handle
                .engine()
                .collection("docs")
                .set_schema(&schema)
                .unwrap();
        }
        let bad = corvid_value_text(b"not an int".as_ptr() as *const std::ffi::c_char, 10);
        assert!(corvid_insert_auto(coll, bad, std::ptr::null_mut()).is_null());
        assert_eq!(
            last_code(),
            crate::error::corvid_err::CORVID_E_SCHEMA_VIOLATION
        );
        corvid_value_free(bad);
        let good = corvid_value_map_new();
        let n = corvid_value_int(1);
        assert_eq!(
            corvid_value_map_put(good, b"n".as_ptr() as *const std::ffi::c_char, 1, n),
            CORVID_OK
        );
        let k2 = corvid_insert_auto(coll, good, std::ptr::null_mut());
        // SAFETY: same provenance.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(k2, 20) },
            b"00000000000000000002",
            "the failed insert burned no id"
        );
        corvid_free(k2 as *mut c_void);
        corvid_value_free(good);

        // NULL doc / NULL coll.
        assert!(corvid_insert_auto(coll, std::ptr::null(), std::ptr::null_mut()).is_null());
        assert!(
            corvid_insert_auto(
                std::ptr::null_mut(),
                corvid_value_int(1),
                std::ptr::null_mut()
            )
            .is_null()
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- update ---------------------------------------------------------------

    #[test]
    fn update_shapes_absent_create_modify_and_delete() {
        let (db, coll) = fresh();

        // Absent key: current is NULL, the replacement creates the row.
        let mut ctx = UpdateCtx {
            saw_current: true,
            make: make_five,
        };
        assert_eq!(
            corvid_update(
                coll,
                b"fresh".as_ptr(),
                5,
                Some(update_replace),
                &mut ctx as *mut UpdateCtx as *mut c_void,
            ),
            CORVID_OK
        );
        assert!(
            !ctx.saw_current,
            "absent key: current is NULL in the callback"
        );
        assert_eq!(get_int(coll, b"fresh"), Some(5));

        // Existing key: current is non-NULL and readable inside the
        // callback (the replace fn only observes presence; read the
        // value through a dedicated callback below).
        extern "C" fn read_current(
            ctx: *mut c_void,
            current: *const corvid_value,
            out: *mut *mut corvid_value,
        ) -> corvid_status {
            // SAFETY: test-owned ctx and contract-valid pointers.
            let seen = unsafe { &mut *(ctx as *mut Option<i64>) };
            *seen = if current.is_null() {
                None
            } else {
                let mut ok = 0;
                let n = crate::value::corvid_value_as_int(current, &mut ok);
                (ok == 1).then_some(n)
            };
            // SAFETY: out is a valid out-param; NULL deletes — not this
            // callback's path, so hand back the CURRENT doc cloned to
            // keep the row.
            unsafe { *out = crate::value::corvid_value_clone(current) };
            CORVID_OK
        }
        let mut seen: Option<i64> = None;
        assert_eq!(
            corvid_update(
                coll,
                b"fresh".as_ptr(),
                5,
                Some(read_current),
                &mut seen as *mut Option<i64> as *mut c_void,
            ),
            CORVID_OK
        );
        assert_eq!(seen, Some(5), "the callback saw the borrowed current doc");
        assert_eq!(
            get_int(coll, b"fresh"),
            Some(5),
            "clone-of-current keeps the row"
        );

        // NULL out: delete.
        assert_eq!(
            corvid_update(
                coll,
                b"fresh".as_ptr(),
                5,
                Some(update_delete),
                std::ptr::null_mut()
            ),
            CORVID_OK
        );
        assert_eq!(get_int(coll, b"fresh"), None);

        // Aborting callback: nothing written, E_ARGUMENT recorded, the
        // message notes the abort.
        int_doc(coll, b"keep", 9);
        assert_eq!(
            corvid_update(
                coll,
                b"keep".as_ptr(),
                4,
                Some(update_abort),
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        let mut mlen = 0;
        let msg = crate::lifecycle::corvid_last_error_message(&mut mlen);
        // SAFETY: the message is NUL-terminated; read as bytes.
        let text = std::ffi::CStr::from_bytes_with_nul(unsafe {
            std::slice::from_raw_parts(msg as *const u8, mlen + 1)
        })
        .unwrap()
        .to_str()
        .unwrap();
        assert!(
            text.contains("abort"),
            "the message notes the abort: {text}"
        );
        assert_eq!(get_int(coll, b"keep"), Some(9), "the abort wrote nothing");

        // NULL fn / NULL coll / NULL key.
        assert_eq!(
            corvid_update(coll, b"k".as_ptr(), 1, None, std::ptr::null_mut()),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_update(
                std::ptr::null_mut(),
                b"k".as_ptr(),
                1,
                Some(update_delete),
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );
        assert_eq!(
            corvid_update(
                coll,
                std::ptr::null(),
                0,
                Some(update_delete),
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- patch / CAS -----------------------------------------------------------

    #[test]
    fn patch_merges_creates_and_replaces() {
        let (db, coll) = fresh();
        // {name: "corvid", n: 1}
        let base = corvid_value_map_new();
        let name = corvid_value_text(b"corvid".as_ptr() as *const std::ffi::c_char, 6);
        assert_eq!(
            corvid_value_map_put(base, b"name".as_ptr() as *const std::ffi::c_char, 4, name),
            CORVID_OK
        );
        let one = corvid_value_int(1);
        assert_eq!(
            corvid_value_map_put(base, b"n".as_ptr() as *const std::ffi::c_char, 1, one),
            CORVID_OK
        );
        assert_eq!(corvid_insert(coll, b"k".as_ptr(), 1, base), CORVID_OK);
        corvid_value_free(base);

        // Patch {n: 2, extra: true}: merge.
        let patch = corvid_value_map_new();
        let two = corvid_value_int(2);
        assert_eq!(
            corvid_value_map_put(patch, b"n".as_ptr() as *const std::ffi::c_char, 1, two),
            CORVID_OK
        );
        let yes = crate::value::corvid_value_bool(1);
        assert_eq!(
            corvid_value_map_put(patch, b"extra".as_ptr() as *const std::ffi::c_char, 5, yes),
            CORVID_OK
        );
        assert_eq!(corvid_patch(coll, b"k".as_ptr(), 1, patch), CORVID_OK);
        corvid_value_free(patch);

        let got = get_value(coll, b"k");
        let mut ok = 0;
        // n merged to 2; name survived; extra added.
        // SAFETY: got is an owned handle from corvid_get.
        let n_child =
            crate::value::corvid_value_map_get(got, b"n".as_ptr() as *const std::ffi::c_char, 1);
        assert_eq!(crate::value::corvid_value_as_int(n_child, &mut ok), 2);
        assert_eq!(ok, 1);
        let name_child =
            crate::value::corvid_value_map_get(got, b"name".as_ptr() as *const std::ffi::c_char, 4);
        let mut tlen = 0;
        let t = crate::value::corvid_value_text_ref(name_child, &mut tlen);
        // SAFETY: t is the child's borrowed buffer with tlen bytes.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(t as *const u8, tlen) },
            b"corvid"
        );
        assert!(
            !crate::value::corvid_value_map_get(
                got,
                b"extra".as_ptr() as *const std::ffi::c_char,
                5
            )
            .is_null()
        );
        corvid_value_free(got);

        // Patch on an absent key creates it (engine patch -> update with
        // None current). Patch with a non-map replaces.
        let scalar_patch = corvid_value_int(42);
        assert_eq!(
            corvid_patch(coll, b"k".as_ptr(), 1, scalar_patch),
            CORVID_OK
        );
        corvid_value_free(scalar_patch);
        assert_eq!(
            get_int(coll, b"k"),
            Some(42),
            "non-map patch replaces the doc"
        );

        assert_eq!(
            corvid_patch(coll, b"new".as_ptr(), 3, corvid_value_int(7)),
            CORVID_OK
        );
        assert_eq!(get_int(coll, b"new"), Some(7));

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    #[test]
    fn compare_and_set_pins_swap_mismatch_absent_and_delete_branches() {
        let (db, coll) = fresh();
        int_doc(coll, b"k", 1);

        // Mismatch: applied == 0 with CORVID_OK (NOT an error).
        let mut applied = -1;
        let wrong = corvid_value_int(999);
        let two = corvid_value_int(2);
        assert_eq!(
            corvid_compare_and_set(coll, b"k".as_ptr(), 1, wrong, two, &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 0, "compare failure is a success status");
        assert_eq!(get_int(coll, b"k"), Some(1), "mismatch wrote nothing");
        corvid_value_free(wrong);
        corvid_value_free(two);

        // Swap: matching expected applies.
        let one = corvid_value_int(1);
        let two = corvid_value_int(2);
        assert_eq!(
            corvid_compare_and_set(coll, b"k".as_ptr(), 1, one, two, &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 1);
        assert_eq!(get_int(coll, b"k"), Some(2));
        corvid_value_free(one);
        corvid_value_free(two);

        // expected == NULL means "must be absent": on an existing key it
        // fails to apply.
        let three = corvid_value_int(3);
        assert_eq!(
            corvid_compare_and_set(
                coll,
                b"k".as_ptr(),
                1,
                std::ptr::null(),
                three,
                &mut applied
            ),
            CORVID_OK
        );
        assert_eq!(applied, 0);
        assert_eq!(get_int(coll, b"k"), Some(2));
        // ...and on an absent key it inserts.
        assert_eq!(
            corvid_compare_and_set(
                coll,
                b"fresh".as_ptr(),
                5,
                std::ptr::null(),
                three,
                &mut applied
            ),
            CORVID_OK
        );
        assert_eq!(applied, 1);
        assert_eq!(get_int(coll, b"fresh"), Some(3));
        corvid_value_free(three);

        // replacement == NULL means "delete if it matches".
        let two = corvid_value_int(2);
        assert_eq!(
            corvid_compare_and_set(coll, b"k".as_ptr(), 1, two, std::ptr::null(), &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 1);
        assert_eq!(
            get_int(coll, b"k"),
            None,
            "the delete branch removed the row"
        );
        corvid_value_free(two);
        // Delete-branch against a mismatch (or absent row): applied 0.
        let five = corvid_value_int(5);
        assert_eq!(
            corvid_compare_and_set(coll, b"k".as_ptr(), 1, five, std::ptr::null(), &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 0);
        corvid_value_free(five);

        // Semantic equality (schema::unique_value_eq): NaN == NaN
        // regardless of payload; -0.0 == 0.0.
        let nan_a = corvid_value_map_new();
        let x_a = corvid_value_float(f64::NAN);
        assert_eq!(
            corvid_value_map_put(nan_a, b"x".as_ptr() as *const std::ffi::c_char, 1, x_a),
            CORVID_OK
        );
        assert_eq!(corvid_insert(coll, b"nan".as_ptr(), 3, nan_a), CORVID_OK);
        corvid_value_free(nan_a);
        let nan_b = corvid_value_map_new();
        // A DIFFERENT NaN payload (0x7FF8... vs the quiet bit set).
        let x_b = corvid_value_float(f64::from_bits(0x7FF8_0000_0000_0001));
        assert_eq!(
            corvid_value_map_put(nan_b, b"x".as_ptr() as *const std::ffi::c_char, 1, x_b),
            CORVID_OK
        );
        let zero = corvid_value_int(0);
        assert_eq!(
            corvid_compare_and_set(coll, b"nan".as_ptr(), 3, nan_b, zero, &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 1, "NaN == NaN regardless of payload");
        assert_eq!(get_int(coll, b"nan"), Some(0));
        corvid_value_free(nan_b);
        corvid_value_free(zero);

        // The -0.0 == 0.0 half of the same rule (the T4 report claimed
        // it without asserting it — the Task 5 prepend's pin): a stored
        // +0.0 matches an expected -0.0, bitwise-distinct floats that
        // IEEE (and the engine's semantic equality) call equal.
        let stored = corvid_value_map_new();
        let x_plus = corvid_value_float(0.0);
        assert_eq!(
            corvid_value_map_put(stored, b"x".as_ptr() as *const std::ffi::c_char, 1, x_plus),
            CORVID_OK
        );
        assert_eq!(corvid_insert(coll, b"negz".as_ptr(), 4, stored), CORVID_OK);
        corvid_value_free(stored);
        let expected = corvid_value_map_new();
        let x_minus = corvid_value_float(-0.0);
        assert_eq!(
            corvid_value_map_put(
                expected,
                b"x".as_ptr() as *const std::ffi::c_char,
                1,
                x_minus
            ),
            CORVID_OK
        );
        let replacement = corvid_value_map_new();
        let x_one = corvid_value_float(1.0);
        assert_eq!(
            corvid_value_map_put(
                replacement,
                b"x".as_ptr() as *const std::ffi::c_char,
                1,
                x_one
            ),
            CORVID_OK
        );
        assert_eq!(
            corvid_compare_and_set(
                coll,
                b"negz".as_ptr(),
                4,
                expected,
                replacement,
                &mut applied
            ),
            CORVID_OK
        );
        assert_eq!(applied, 1, "-0.0 == 0.0 under semantic equality");
        // And the reflected order: stored -0.0 matches expected +0.0.
        let stored2 = corvid_value_map_new();
        let x_minus2 = corvid_value_float(-0.0);
        assert_eq!(
            corvid_value_map_put(
                stored2,
                b"x".as_ptr() as *const std::ffi::c_char,
                1,
                x_minus2
            ),
            CORVID_OK
        );
        assert_eq!(
            corvid_insert(coll, b"negz2".as_ptr(), 5, stored2),
            CORVID_OK
        );
        corvid_value_free(stored2);
        let expected2 = corvid_value_map_new();
        let x_plus2 = corvid_value_float(0.0);
        assert_eq!(
            corvid_value_map_put(
                expected2,
                b"x".as_ptr() as *const std::ffi::c_char,
                1,
                x_plus2
            ),
            CORVID_OK
        );
        let two = corvid_value_int(2);
        assert_eq!(
            corvid_compare_and_set(coll, b"negz2".as_ptr(), 5, expected2, two, &mut applied),
            CORVID_OK
        );
        assert_eq!(applied, 1);
        assert_eq!(get_int(coll, b"negz2"), Some(2));
        corvid_value_free(expected2);
        corvid_value_free(two);

        // applied_out is nullable (§7's optional out-params).
        let one = corvid_value_int(0);
        let two = corvid_value_int(9);
        assert_eq!(
            corvid_compare_and_set(coll, b"nan".as_ptr(), 3, one, two, std::ptr::null_mut()),
            CORVID_OK
        );
        corvid_value_free(one);
        corvid_value_free(two);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- deletes ---------------------------------------------------------------

    #[test]
    fn delete_reports_existed_and_delete_batch_counts() {
        let (db, coll) = fresh();
        int_doc(coll, b"a", 1);
        int_doc(coll, b"b", 2);
        int_doc(coll, b"c", 3);

        let mut existed = -1;
        assert_eq!(
            corvid_delete(coll, b"a".as_ptr(), 1, &mut existed),
            CORVID_OK
        );
        assert_eq!(existed, 1);
        assert_eq!(
            corvid_delete(coll, b"a".as_ptr(), 1, &mut existed),
            CORVID_OK
        );
        assert_eq!(existed, 0, "second delete: nothing was there");
        // existed_out nullable.
        assert_eq!(
            corvid_delete(coll, b"b".as_ptr(), 1, std::ptr::null_mut()),
            CORVID_OK
        );

        int_doc(coll, b"d", 4);
        int_doc(coll, b"e", 5);
        // Mixed existing/absent keys.
        let keys: [*const u8; 3] = [b"c".as_ptr(), b"e".as_ptr(), b"zz".as_ptr()];
        let lens: [usize; 3] = [1, 1, 2];
        let mut removed = usize::MAX;
        assert_eq!(
            corvid_delete_batch(coll, keys.as_ptr(), lens.as_ptr(), 3, &mut removed),
            CORVID_OK
        );
        assert_eq!(removed, 2, "c and e existed; zz did not");
        assert_eq!(len_of(coll), 1, "only d remains");

        // count == 0 with NULL arrays: a successful no-op.
        assert_eq!(
            corvid_delete_batch(coll, std::ptr::null(), std::ptr::null(), 0, &mut removed),
            CORVID_OK
        );
        assert_eq!(removed, 0);
        // NULL arrays with count > 0: the argument error.
        assert_eq!(
            corvid_delete_batch(coll, std::ptr::null(), std::ptr::null(), 1, &mut removed),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    #[test]
    fn delete_where_consumes_and_counts() {
        let (db, coll) = fresh();
        // Map documents with an n field (the predicate's path).
        for n in 0..10i64 {
            // map_put CONSUMES v into the doc; only the doc handle
            // stays ours to free after insert clones it.
            let v = corvid_value_int(n);
            let doc = corvid_value_map_new();
            assert_eq!(
                corvid_value_map_put(doc, b"n".as_ptr() as *const std::ffi::c_char, 1, v),
                CORVID_OK
            );
            let key = format!("k{n}");
            assert_eq!(corvid_insert(coll, key.as_ptr(), key.len(), doc), CORVID_OK);
            corvid_value_free(doc);
        }
        let pred = crate::pred::corvid_pred_compare(
            b"n".as_ptr() as *const std::ffi::c_char,
            1,
            crate::pred::corvid_cmp::CORVID_CMP_GE,
            corvid_value_int(7),
        );
        let mut removed = usize::MAX;
        assert_eq!(corvid_delete_where(coll, pred, &mut removed), CORVID_OK);
        assert_eq!(removed, 3);
        assert_eq!(len_of(coll), 7);

        // NULL pred: the argument error (and nothing consumed).
        assert_eq!(
            corvid_delete_where(coll, std::ptr::null_mut(), &mut removed),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);
        // NULL coll: pred consumed first (spec §8), then the error.
        let pred = crate::pred::corvid_pred_exists(b"n".as_ptr() as *const std::ffi::c_char, 1);
        assert_eq!(
            corvid_delete_where(std::ptr::null_mut(), pred, std::ptr::null_mut()),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- TTL ---------------------------------------------------------------------

    #[test]
    fn ttl_trio_and_purge_pin_the_inclusive_boundary() {
        let (db, coll) = fresh();

        // insert_with_ttl: row + expiry in one commit.
        let v = corvid_value_int(1);
        assert_eq!(
            corvid_insert_with_ttl(coll, b"a".as_ptr(), 1, v, 100),
            CORVID_OK
        );
        corvid_value_free(v);
        let (mut at, mut has) = (i64::MIN, -1);
        assert_eq!(
            corvid_get_ttl(coll, b"a".as_ptr(), 1, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!((has, at), (1, 100));

        // set_ttl replaces without rewriting the doc.
        assert_eq!(corvid_set_ttl(coll, b"a".as_ptr(), 1, 200), CORVID_OK);
        assert_eq!(
            corvid_get_ttl(coll, b"a".as_ptr(), 1, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!((has, at), (1, 200));
        assert_eq!(get_int(coll, b"a"), Some(1), "set_ttl left the doc alone");

        // An untouched key: has_ttl == 0 — not an error.
        int_doc(coll, b"plain", 2);
        assert_eq!(
            corvid_get_ttl(coll, b"plain".as_ptr(), 5, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!(has, 0);
        assert_eq!(at, 0, "expires_at_out reads 0 when unset");
        // An absent key: same shape.
        assert_eq!(
            corvid_get_ttl(coll, b"absent".as_ptr(), 6, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!(has, 0);

        // Purge: expiry <= now is INCLUSIVE.
        let mut purged = usize::MAX;
        assert_eq!(corvid_purge_expired(coll, 199, &mut purged), CORVID_OK);
        assert_eq!(purged, 0, "200 > 199: not due yet");
        assert_eq!(corvid_purge_expired(coll, 200, &mut purged), CORVID_OK);
        assert_eq!(purged, 1, "expiry == now: due (inclusive boundary)");
        assert_eq!(get_int(coll, b"a"), None);
        assert_eq!(len_of(coll), 1, "the plain record never expires");
        assert_eq!(
            corvid_get_ttl(coll, b"a".as_ptr(), 1, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!(has, 0, "the purge cleared the expiry entry too");

        // Multiple due records, in one purge; only the due ones.
        for (key, expires) in [
            (b"x".as_slice(), 10i64),
            (b"y".as_slice(), 20i64),
            (b"z".as_slice(), 30i64),
        ] {
            let v = corvid_value_int(1);
            assert_eq!(
                corvid_insert_with_ttl(coll, key.as_ptr(), key.len(), v, expires),
                CORVID_OK
            );
            corvid_value_free(v);
        }
        assert_eq!(corvid_purge_expired(coll, 20, &mut purged), CORVID_OK);
        assert_eq!(purged, 2, "x (10) and y (20) due; z (30) not");
        assert_eq!(len_of(coll), 2, "plain + z");
        assert_eq!(get_int(coll, b"z"), Some(1));

        // A plain write CLEARS a previously set expiry (the engine's
        // in-transaction ttl_clear on non-TTL writes).
        int_doc(coll, b"z", 9);
        assert_eq!(
            corvid_get_ttl(coll, b"z".as_ptr(), 1, &mut at, &mut has),
            CORVID_OK
        );
        assert_eq!(has, 0, "plain rewrite cleared the expiry");

        // Out-params are nullable; NULL coll/key are argument errors.
        assert_eq!(
            corvid_purge_expired(coll, 0, std::ptr::null_mut()),
            CORVID_OK
        );
        assert_eq!(
            corvid_set_ttl(std::ptr::null_mut(), b"k".as_ptr(), 1, 5),
            CORVID_ERR
        );
        assert_eq!(corvid_set_ttl(coll, std::ptr::null(), 0, 5), CORVID_ERR);
        assert_eq!(
            corvid_insert_with_ttl(coll, b"k".as_ptr(), 1, std::ptr::null(), 5),
            CORVID_ERR
        );
        assert_eq!(
            corvid_get_ttl(
                std::ptr::null_mut(),
                b"k".as_ptr(),
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), crate::error::corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // helpers for the patch test
    fn get_value(coll: Coll, key: &[u8]) -> *mut corvid_value {
        let mut out: *mut corvid_value = std::ptr::null_mut();
        assert_eq!(
            corvid_get(coll, key.as_ptr(), key.len(), &mut out),
            CORVID_OK
        );
        assert!(!out.is_null());
        out
    }

    fn corvid_value_clone_abi(v: *const corvid_value) -> *mut corvid_value {
        crate::value::corvid_value_clone(v)
    }
}
