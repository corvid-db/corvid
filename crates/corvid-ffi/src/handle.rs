//! Opaque-handle infrastructure (spec §1.1, §2) and the FFI-owned
//! derived-handle counter (spec §4.13/§6).
//!
//! Every ABI handle is an opaque, single-pointer-sized, zero-sized
//! `#[repr(C)]` marker type (`corvid_db`, `corvid_strs`, …). The real
//! state lives in interior Rust structs (`DbHandle`, `StrsHandle`, …)
//! that never cross the boundary by value: constructors hand out
//! `Box::into_raw(body) as *mut Marker`, accessors borrow the body back,
//! and each family's `_free` runs `Box::from_raw` on the original body
//! pointer. C sees only the forward declaration in `corvid.h`;
//! cross-family frees are UB by contract (spec §2) because the marker
//! cast is only valid for the owning family.
//!
//! The one family that departs from the wrapper-struct shape is the
//! value family (spec §4.3/§4.4): a `corvid_value*` is a pointer at a
//! bare `corvid::Value`, boxed directly (`into_value`), because the
//! value has no cursor or counter state to carry. That family also has
//! the ABI's second handle provenance — borrowed children — documented
//! with its plumbing below.
//!
//! # The derived-handle counter
//!
//! `corvid_compact` needs exclusive engine access (`Db::compact` takes
//! `&mut self`), so spec §4.13 checks quiescence with an FFI-owned
//! `AtomicUsize` on the db: it counts live handles holding a **clone of
//! the engine `Arc`** — initialized to 1 (the db handle itself) at open,
//! incremented when a derived engine handle is created, decremented by
//! that handle's `_free`; `corvid_compact` (Task 6) requires exactly 1
//! and otherwise fails with the FFI-only `CORVID_E_BUSY`. The count is
//! deterministic because this layer is the only `Arc` cloner — bindings
//! never see the `Arc`.
//!
//! **Wiring:** the increment is added by each family that creates
//! engine-backed handles as those families land (collection handles with
//! Task 4, query handles with Task 5) — none exist yet, so
//! `DbHandle::retain_derived` currently has no production caller.
//! Cursors that own only materialized data and hold no engine reference
//! (`rows`, `strs`, `geohits`, `groupiter`, `schemaiter` — the spec §2
//! backing table) do **not** increment: they cannot keep the engine
//! working and so cannot block exclusive compaction.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use corvid::Db;

/// The opaque `corvid_db*` handle type (spec §1.1). Zero-sized and never
/// constructed — a pointer to it is a typed alias for the interior
/// `DbHandle` box (see the module docs for the provenance contract).
#[repr(C)]
pub struct corvid_db {
    _unused: [u8; 0],
}

/// The opaque `corvid_strs*` handle type (spec §1.1). Backed by
/// `StrsHandle`.
#[repr(C)]
pub struct corvid_strs {
    _unused: [u8; 0],
}

/// The opaque `corvid_value*` handle type (spec §1.1). Backed by a bare
/// boxed `corvid::Value` (`into_value`) — see the module docs for why
/// this family skips a wrapper struct, and the value plumbing below for
/// the owned-vs-borrowed provenance split.
#[repr(C)]
pub struct corvid_value {
    _unused: [u8; 0],
}

/// Interior state behind a `corvid_db*`: the engine handle (shared with
/// derived handles as `Arc` clones) and the derived-handle counter.
pub(crate) struct DbHandle {
    db: Arc<Db>,
    /// Live handles holding a clone of `db`, this handle included — see
    /// the module docs. `Relaxed` orderings would suffice (the count only
    /// gates a quiescence check, not data publication), but the
    /// acquire/release pair matches §6's cross-thread contract in the
    /// most conservative reading.
    derived: AtomicUsize,
}

impl DbHandle {
    /// Wrap a freshly opened engine `Db`. The counter starts at 1: the db
    /// handle itself (spec §4.13's "exactly 1" for compaction).
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db: Arc::new(db),
            derived: AtomicUsize::new(1),
        }
    }

    /// The engine behind the handle, borrowed for an in-place call.
    pub(crate) fn engine(&self) -> &Db {
        &self.db
    }

    // The counter surface below has no production caller until the
    // derived-handle families land (collection: Task 4, query: Task 5,
    // compact's gate: Task 6); the unit tests pin its arithmetic now.
    #[allow(dead_code)] // wired by Tasks 4–6 (see the module docs)
    pub(crate) fn db(&self) -> Arc<Db> {
        self.db.clone()
    }

    /// Count a newly created derived engine handle (module docs).
    #[allow(dead_code)] // wired by Tasks 4–6 (see the module docs)
    pub(crate) fn retain_derived(&self) {
        self.derived.fetch_add(1, Ordering::Release);
    }

    /// Count a freed derived engine handle — called by the family's
    /// `_free`, never by `corvid_close` (which drops the counter with the
    /// handle).
    #[allow(dead_code)] // wired by Tasks 4–6 (see the module docs)
    pub(crate) fn release_derived(&self) {
        self.derived.fetch_sub(1, Ordering::Release);
    }

    /// `corvid_compact`'s quiescence check: exactly the db handle itself
    /// is live (spec §4.13). Task 6 wires this into `corvid_compact`.
    #[allow(dead_code)] // wired by Task 6 (corvid_compact)
    pub(crate) fn is_exclusive(&self) -> bool {
        self.derived.load(Ordering::Acquire) == 1
    }
}

/// Interior state behind a `corvid_strs*`: the materialized string list
/// and the read cursor (spec §2: "owned `Vec<String>` + cursor",
/// single-threaded use).
pub(crate) struct StrsHandle {
    items: Vec<String>,
    cursor: usize,
}

impl StrsHandle {
    pub(crate) fn new(items: Vec<String>) -> Self {
        Self { items, cursor: 0 }
    }

    /// Borrow the string at the cursor and advance it; `None` at
    /// exhaustion (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<&str> {
        let item = self.items.get(self.cursor).map(String::as_str);
        if item.is_some() {
            self.cursor += 1;
        }
        item
    }
}

// --- db handle plumbing ----------------------------------------------------

/// Box a db body and hand out its opaque ABI pointer.
pub(crate) fn into_db(body: DbHandle) -> *mut corvid_db {
    Box::into_raw(Box::new(body)) as *mut corvid_db
}

/// Shared borrow of a db body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_db`] and not yet reclaimed by
/// [`reclaim_db`]; the borrow honors `corvid_db`'s thread-safe contract
/// (spec §2) — concurrent shared borrows are fine, a concurrent
/// [`reclaim_db`] is not.
pub(crate) unsafe fn borrow_db<'a>(ptr: *mut corvid_db) -> Option<&'a DbHandle> {
    // SAFETY: caller guarantees provenance (doc comment above); the box
    // pointer round-trips through the zero-sized marker with the body's
    // original alignment, so the reference is valid.
    unsafe { (ptr as *mut DbHandle).as_ref() }
}

/// Take a db body back for `corvid_close`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_db`], exactly once, and not
/// yet reclaimed.
pub(crate) unsafe fn reclaim_db(ptr: *mut corvid_db) -> Option<Box<DbHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of a pointer
    // produced by into_db (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut DbHandle) })
}

// --- strs handle plumbing --------------------------------------------------

/// Box a strs body and hand out its opaque ABI pointer.
pub(crate) fn into_strs(body: StrsHandle) -> *mut corvid_strs {
    Box::into_raw(Box::new(body)) as *mut corvid_strs
}

/// Exclusive borrow of a strs body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_strs`] and not yet reclaimed
/// by [`reclaim_strs`]; cursors are single-threaded by contract (spec
/// §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_strs_mut<'a>(ptr: *mut corvid_strs) -> Option<&'a mut StrsHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut StrsHandle).as_mut() }
}

/// Take a strs body back for `corvid_strs_free`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_strs`], exactly once, and not
/// yet reclaimed.
pub(crate) unsafe fn reclaim_strs(ptr: *mut corvid_strs) -> Option<Box<StrsHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of a pointer
    // produced by into_strs (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut StrsHandle) })
}

// --- value handle plumbing --------------------------------------------------
//
// A `corvid_value*` has TWO provenances (spec §2/§4.4/§5 rule 6), both
// pointing at a `corvid::Value`:
//
// * OWNED — `into_value` boxes the Value; the value constructors,
//   `corvid_value_clone`, and (later tasks) `corvid_get` / query
//   min-max return these. `reclaim_value` (via `corvid_value_free`) is
//   their only destructor.
// * BORROWED — `corvid_value_array_get` / `corvid_value_map_get` return
//   interior pointers straight into a live parent Value's `Vec` /
//   `BTreeMap` storage: a lightweight borrowed-view handle, valid until
//   the parent's next mutation (`array_push` / `map_put`) or free. A
//   parent-held child registry was the alternative design (the plan
//   allowed either); it lost because it would tax every child read on
//   the document hot path with bookkeeping for a guarantee C cannot
//   check anyway — the interior view is zero-cost and its lifetime is
//   exactly the spec's "rides the parent" wording.
//
// The two are indistinguishable by pointer VALUE (an interior pointer's
// bits are unremarkable), so misuse — freeing a borrowed child, or
// passing one where an owned value is consumed — is undetectable at
// runtime: documented UB, bold in the spec, netted by the Task 7
// ASan/LSan CI runs.

/// Box a value and hand out its opaque ABI pointer: an OWNED
/// `corvid_value*` (spec §4.3 — the constructors' return shape).
pub(crate) fn into_value(body: corvid::Value) -> *mut corvid_value {
    Box::into_raw(Box::new(body)) as *mut corvid_value
}

/// Shared borrow of a value, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL, an owned handle produced by [`into_value`] and not yet
/// reclaimed, or a borrowed child whose parent has not been mutated
/// (`array_push`/`map_put`) or freed since the child was obtained; the
/// §2 contract confines a value handle to one thread.
pub(crate) unsafe fn borrow_value<'a>(ptr: *const corvid_value) -> Option<&'a corvid::Value> {
    // SAFETY: caller guarantees provenance (doc comment above); both
    // handle shapes point at a live `corvid::Value`, and the box pointer
    // round-trips through the zero-sized marker with its alignment.
    unsafe { (ptr as *const corvid::Value).as_ref() }
}

/// Exclusive borrow of a value for mutation, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or an OWNED handle produced by [`into_value`], not yet
/// reclaimed. The mutation sites (`corvid_value_array_push` /
/// `corvid_value_map_put`) take `corvid_value*` non-const, so a borrowed
/// child cannot reach one without a deliberate const-cast — which is the
/// documented UB of spec §4.4.
pub(crate) unsafe fn borrow_value_mut<'a>(ptr: *mut corvid_value) -> Option<&'a mut corvid::Value> {
    // SAFETY: caller guarantees owned provenance (doc comment above);
    // the §2 single-thread contract makes this the only borrow.
    unsafe { (ptr as *mut corvid::Value).as_mut() }
}

/// Take a value body back for `corvid_value_free`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or an OWNED handle produced by [`into_value`], exactly
/// once, and not yet reclaimed. Reclaiming a borrowed child (an interior
/// pointer from `array_get`/`map_get`) is UB — spec §4.4, in bold.
pub(crate) unsafe fn reclaim_value(ptr: *mut corvid_value) -> Option<Box<corvid::Value>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of an
    // into_value product (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut corvid::Value) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter's lifecycle: 1 at open, +1 per derived handle, back to
    /// 1 at release; `is_exclusive` is the compaction gate (spec §4.13).
    /// The production retain/release call sites land with the collection
    /// (Task 4) and query (Task 5) families; this pins the arithmetic.
    #[test]
    fn derived_counter_gates_exclusive_compaction() {
        let db = DbHandle::new(corvid::Db::open_in_memory().unwrap());
        assert!(
            db.is_exclusive(),
            "a fresh db handle is alone: count starts at 1"
        );

        db.retain_derived();
        assert!(!db.is_exclusive(), "one derived handle blocks compact");

        db.retain_derived();
        db.release_derived();
        assert!(!db.is_exclusive(), "still one derived handle live");

        db.release_derived();
        assert!(db.is_exclusive(), "last release returns to exactly 1");
    }

    /// The engine `Arc` the counter guards: a derived clone keeps the
    /// engine alive after `corvid_close` drops the db handle (spec §2's
    /// "a corvid_coll keeps its corvid_db alive").
    #[test]
    fn derived_arc_clone_outlives_the_db_handle() {
        let db = DbHandle::new(corvid::Db::open_in_memory().unwrap());
        let engine = db.db();
        assert!(std::ptr::eq(Arc::as_ptr(&engine), db.engine()));
        drop(db);
        // The engine reference still resolves collections — nothing
        // touched the file lock or state.
        assert!(engine.collections().unwrap().is_empty());
    }

    #[test]
    fn strs_cursor_walks_and_sticks_at_exhaustion() {
        let mut strs = StrsHandle::new(vec!["alpha".into(), "beta".into()]);
        assert_eq!(strs.next(), Some("alpha"));
        assert_eq!(strs.next(), Some("beta"));
        assert_eq!(strs.next(), None);
        assert_eq!(strs.next(), None, "exhaustion is sticky");

        let mut empty = StrsHandle::new(Vec::new());
        assert_eq!(empty.next(), None);
    }

    #[test]
    fn opaque_pointers_round_trip_through_the_marker() {
        let ptr = into_db(DbHandle::new(corvid::Db::open_in_memory().unwrap()));
        assert!(!ptr.is_null());
        // SAFETY: ptr was produced by into_db just above and not yet
        // reclaimed; single-threaded test.
        assert!(unsafe { borrow_db(ptr) }.is_some());
        // SAFETY: same provenance, first and only reclaim.
        drop(unsafe { reclaim_db(ptr) });

        let null: *mut corvid_db = std::ptr::null_mut();
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_db(null) }.is_none());
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { reclaim_db(null) }.is_none());

        let strs = into_strs(StrsHandle::new(Vec::new()));
        // SAFETY: fresh into_strs product, exclusive test-thread borrow.
        assert!(unsafe { borrow_strs_mut(strs) }.is_some());
        // SAFETY: first and only reclaim.
        drop(unsafe { reclaim_strs(strs) });
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_strs_mut(std::ptr::null_mut()) }.is_none());
    }

    #[test]
    fn value_pointers_round_trip_and_children_live_inside_the_parent() {
        use crate::handle::borrow_value;
        use crate::handle::into_value;
        use crate::handle::reclaim_value;

        let owned = into_value(corvid::Value::Array(vec![corvid::Value::Int(7)]));
        // The borrowed-child shape of array_get: an interior pointer into
        // the parent's Vec. Scoped so the shared borrow provably ends
        // before the reclaim below (raw-pointer provenance is a SAFETY
        // contract, not something borrowck checks).
        let child: *const corvid_value = {
            // SAFETY: fresh into_value product on this thread.
            let parent = unsafe { borrow_value(owned) }.expect("non-NULL handle");
            match parent {
                corvid::Value::Array(items) => {
                    &items[0] as *const corvid::Value as *const corvid_value
                }
                _ => unreachable!("constructed as an array above"),
            }
        };
        // SAFETY: the child borrows the still-live parent; read before
        // the reclaim below.
        assert_eq!(
            unsafe { borrow_value(child) }.and_then(corvid::Value::as_int),
            Some(7)
        );
        // SAFETY: first and only reclaim of the owned product; the child
        // borrow ended with the statement above.
        drop(unsafe { reclaim_value(owned) });

        let null: *const corvid_value = std::ptr::null();
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_value(null) }.is_none());
        // SAFETY: NULL is the documented no-op reclaim.
        assert!(unsafe { reclaim_value(std::ptr::null_mut()) }.is_none());
    }
}
