//! Collection handles (spec §4.2) — the three functions of the ABI's
//! second family: `corvid_collection`, `corvid_collection_free`,
//! `corvid_collection_name`.
//!
//! A `corvid_coll*` is the derived engine handle: it holds a clone of the
//! db's `Arc<Db>` plus the stored name (spec §2), so it is thread-safe
//! like its parent and **keeps the engine alive after `corvid_close`**
//! (freeing the db handle while collection handles live is fine). Its
//! creation increments the FFI-owned derived-handle counter and its
//! `_free` decrements it (spec §4.13) — the gate `corvid_compact` (Task
//! 6) will require to read exactly 1.
//!
//! Names are NOT validated here (spec §4.2): the engine creates
//! collections lazily on first write (`Db::collection` is infallible), so
//! reserved (`__`-prefixed) and invalid (interior `__`, NUL byte) names
//! are accepted by the handle and fail at write time with
//! `CORVID_E_RESERVED_COLLECTION` / `CORVID_E_INVALID_NAME`, exactly as
//! in Rust. The name must still be valid UTF-8 (spec §1.5) — that is the
//! ABI's one encoding rule, checked here.

use std::ffi::c_char;

use crate::error::record_argument;
use crate::handle::CollHandle;
use crate::handle::borrow_coll;
use crate::handle::borrow_db;
use crate::handle::corvid_coll;
use crate::handle::corvid_db;
use crate::handle::into_coll;
use crate::handle::reclaim_coll;
use crate::value::borrowed_utf8;

/// Handle to a named collection (spec §4.2); the collection is created
/// lazily on first write. Wraps `corvid::Db::collection` (infallible in
/// Rust). `db` and `name` are non-NULL (`name` UTF-8, any length — the
/// empty name is legal); NULL or misencoded input returns NULL with
/// `CORVID_E_ARGUMENT` recorded. Reserved/invalid names are NOT checked
/// here — they fail at write time with
/// `CORVID_E_RESERVED_COLLECTION` / `CORVID_E_INVALID_NAME`, exactly as
/// the engine does. The handle increments the db's derived-handle
/// counter (spec §4.13) and holds an engine reference, so it keeps the
/// database alive after `corvid_close` (spec §2).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_collection(
    db: *mut corvid_db,
    name: *const c_char,
    name_len: usize,
) -> *mut corvid_coll {
    // SAFETY: NULL maps to None (spec §7); a non-NULL handle has
    // corvid_open/open_memory provenance and `corvid_db` is the
    // thread-safe family (spec §2) — a shared borrow is fine.
    let Some(handle) = (unsafe { borrow_db(db) }) else {
        record_argument("corvid_collection: db is NULL");
        return std::ptr::null_mut();
    };
    let Some(name) = borrowed_utf8("corvid_collection", "name", name, name_len) else {
        return std::ptr::null_mut();
    };
    // The count and the handle are born together (spec §4.13): the
    // increment happens before the engine Arc moves into the coll body.
    handle.retain_derived();
    into_coll(CollHandle::new(
        handle.db(),
        name.to_owned(),
        handle.counter(),
    ))
}

/// Free a collection handle (spec §4.2). No engine counterpart (Rust
/// `Collection` is a copyable borrow); this releases the handle's engine
/// reference and its derived-handle count (spec §4.13). `corvid_close`
/// may have already run — the release is shaped to survive it (spec §2).
/// `corvid_collection_free(NULL)` is a no-op (spec §7).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_collection_free(coll: *mut corvid_coll) {
    // SAFETY: NULL is the documented no-op; otherwise coll is
    // contractually a corvid_collection product reclaimed exactly once
    // here. Cross-family frees are UB (spec §2).
    if let Some(body) = unsafe { reclaim_coll(coll) } {
        body.release_derived();
    }
}

/// The collection's name (spec §4.2): NUL-terminated, `*len_out` set to
/// the byte length (`len_out` nullable). BORROWED from the handle: valid
/// until `corvid_collection_free`, and stable across calls (the buffer
/// never moves). A name that itself contains a NUL byte truncates only
/// the C view — `*len_out` still carries the exact length. A NULL `coll`
/// follows the non-status rule (§7): NULL return with
/// `CORVID_E_ARGUMENT` recorded. No direct engine counterpart (reads the
/// handle's stored name).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_collection_name(
    coll: *mut corvid_coll,
    len_out: *mut usize,
) -> *const c_char {
    // SAFETY: NULL maps to the inert arm (§7); otherwise coll is a live
    // corvid_collection product on this thread (spec §2).
    match unsafe { borrow_coll(coll) } {
        Some(handle) => {
            if !len_out.is_null() {
                // SAFETY: len_out is non-NULL (checked).
                unsafe { *len_out = handle.name_len() };
            }
            handle.name_ptr()
        }
        None => {
            record_argument("corvid_collection_name: coll is NULL (§7 inert rule)");
            if !len_out.is_null() {
                // SAFETY: len_out is non-NULL (checked).
                unsafe { *len_out = 0 };
            }
            std::ptr::null()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::corvid_err;
    use crate::error::corvid_status;
    use crate::error::last_code;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open_memory;

    fn db_handle() -> *mut corvid_db {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        db
    }

    fn collection(db: *mut corvid_db, name: &str) -> *mut corvid_coll {
        let coll = corvid_collection(db, name.as_ptr() as *const c_char, name.len());
        assert!(!coll.is_null(), "a legal name never fails here (spec §4.2)");
        coll
    }

    /// The db's exclusive-compaction gate through the ABI surface: a
    /// live collection handle blocks it, freeing the handle restores it
    /// (spec §4.13 — the counter wiring this family owns).
    #[test]
    fn collection_handles_gate_exclusive_compaction() {
        // SAFETY: the is_exclusive check is crate-internal plumbing for
        // Task 6's corvid_compact; borrow under the test's single thread.
        fn exclusive(db: *mut corvid_db) -> bool {
            // SAFETY: db is a live corvid_open_memory product.
            unsafe { crate::handle::borrow_db(db) }
                .expect("non-NULL")
                .is_exclusive()
        }

        let db = db_handle();
        assert!(exclusive(db), "fresh db: count is exactly 1");

        let a = collection(db, "docs");
        assert!(!exclusive(db), "one derived handle blocks compact");
        let b = collection(db, "docs");
        assert!(!exclusive(db));

        corvid_collection_free(a);
        assert!(!exclusive(db), "one coll still live");
        corvid_collection_free(b);
        assert!(exclusive(db), "back to exactly 1");

        corvid_collection_free(std::ptr::null_mut()); // §7 no-op
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    /// Spec §2's pin: a collection handle keeps the engine alive after
    /// `corvid_close` drops the db handle — writes through the orphaned
    /// coll work, and its `_free` (the counter release) still runs
    /// cleanly afterwards.
    #[test]
    fn a_live_collection_keeps_the_engine_alive_after_close() {
        let db = db_handle();
        let coll = collection(db, "docs");
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);

        // The engine reference is alive: a write through the orphaned
        // handle lands (observed via corvid_len — Task 4's read family).
        let doc = crate::value::corvid_value_int(7);
        assert_eq!(
            crate::mutation::corvid_insert(coll, b"k".as_ptr(), 1, doc),
            corvid_status::CORVID_OK
        );
        crate::value::corvid_value_free(doc);
        let mut n = usize::MAX;
        assert_eq!(
            crate::read::corvid_len(coll, &mut n),
            corvid_status::CORVID_OK
        );
        assert_eq!(n, 1, "the write survived the db handle's close");

        corvid_collection_free(coll); // the counter release after close
    }

    #[test]
    fn names_are_borrowed_stable_and_exact() {
        let db = db_handle();
        let coll = collection(db, "user-events");
        let mut len = usize::MAX;
        let view = corvid_collection_name(coll, &mut len);
        assert!(!view.is_null());
        assert_eq!(len, "user-events".len());
        // SAFETY: view is the handle's own NUL-terminated buffer, valid
        // until free; the C-string length matches the exact length (no
        // interior NUL in this name).
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(view) }.to_bytes(),
            b"user-events"
        );
        // Borrowed, not copied: repeated calls hand out the same pointer.
        let mut len2 = 0;
        assert_eq!(corvid_collection_name(coll, &mut len2), view);
        assert_eq!(len2, len);
        // len_out is nullable (spec §7's optional out-params).
        assert_eq!(corvid_collection_name(coll, std::ptr::null_mut()), view);

        // The empty name is legal (spec §1.5).
        let empty = collection(db, "");
        let mut len3 = usize::MAX;
        let view3 = corvid_collection_name(empty, &mut len3);
        assert_eq!(len3, 0);
        // SAFETY: same provenance as above.
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(view3) }.to_bytes(), b"");

        corvid_collection_free(coll);
        corvid_collection_free(empty);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn null_and_misencoded_arguments_are_argument_errors() {
        let db = db_handle();

        assert!(
            corvid_collection(std::ptr::null_mut(), b"x".as_ptr() as *const c_char, 1).is_null()
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        assert!(corvid_collection(db, std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        let bad = [0xFF_u8, 0xFE];
        assert!(corvid_collection(db, bad.as_ptr() as *const c_char, bad.len()).is_null());
        assert_eq!(
            last_code(),
            corvid_err::CORVID_E_ARGUMENT,
            "non-UTF-8 name: the ABI's one encoding rule (§1.5)"
        );

        // The name reader's §7 inert shape.
        let mut len = usize::MAX;
        assert!(corvid_collection_name(std::ptr::null_mut(), &mut len).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(len, 0);

        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    /// Reserved and invalid names are accepted by the handle (lazy, spec
    /// §4.2) and rejected at write time with their dedicated codes —
    /// pinned here through the mutation family's `corvid_insert`.
    #[test]
    fn reserved_and_invalid_names_fail_at_write_not_at_handle_time() {
        let db = db_handle();
        for name in ["__edges__docs", "a__b", "doc\0s"] {
            let coll = collection(db, name);
            let mut len = usize::MAX;
            // SAFETY: view is the handle's buffer; read before free.
            let view = corvid_collection_name(coll, &mut len);
            assert!(!view.is_null(), "the handle itself never fails (§4.2)");
            assert_eq!(len, name.len());

            let doc = crate::value::corvid_value_int(1);
            assert_eq!(
                crate::mutation::corvid_insert(coll, b"k".as_ptr(), 1, doc),
                corvid_status::CORVID_ERR,
                "{name:?} must fail at write time"
            );
            crate::value::corvid_value_free(doc);
            let code = last_code();
            assert!(
                code == corvid_err::CORVID_E_RESERVED_COLLECTION
                    || code == corvid_err::CORVID_E_INVALID_NAME,
                "{name:?}: write-time code {code:?} outside the spec §4.2 pair"
            );
            corvid_collection_free(coll);
        }
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }
}
