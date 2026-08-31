//! Reads (spec §4.9) — `corvid_get`, `corvid_scan`, `corvid_page`,
//! `corvid_len`.
//!
//! `get` uses the optional-value convention (absence is `CORVID_OK` +
//! `*out == NULL` — never a bare NULL return, spec §3). `scan` streams
//! every row through a C callback in key order (the engine's
//! constant-memory `for_each_doc`, not the materializing `scan`).
//! `page` is keyset pagination: an owned rows cursor (score 0.0 —
//! driven by `corvid_rows_next`/`corvid_rows_free`, which land with the
//! query family in Task 5) plus a `next_after` resume cursor that is an
//! ABI-owned buffer **freed with `corvid_free`** (born in
//! `crate::lifecycle::buffer_new`).

use std::ffi::c_int;
use std::ffi::c_void;

use corvid::ResultRow;

use crate::error::corvid_status;
use crate::error::guard;
use crate::error::record_argument;
use crate::handle::RowsHandle;
use crate::handle::borrow_coll;
use crate::handle::corvid_coll;
use crate::handle::corvid_rows;
use crate::handle::corvid_value;
use crate::handle::into_rows;
use crate::handle::reclaim_rows;
use crate::lifecycle::buffer_new;
use crate::value::borrowed_bytes;

/// `corvid_scan`'s row sink (spec §1.6): `ctx` is passed through
/// opaque; return 1 to continue, 0 to stop the scan — stopping is not
/// an error (any other return value also stops, defensively: a
/// misbehaving callback is not called again). `key` and `doc` are
/// BORROWED and valid only inside the callback — freeing the doc or
/// keeping the pointers past the return is UB; `corvid_value_clone` is
/// the sanctioned escape.
///
/// **Reentrancy (spec §1.6):** the callback runs on the caller's
/// thread between engine operations, inside the scan's read
/// transaction. It MUST NOT free or mutate the borrowed arguments, MUST
/// NOT issue writes to the same database, and SHOULD NOT make other
/// corvid calls at all — the portable contract is "no reentrant
/// corvid calls". Violating it is UB or a deadlock, not a checked
/// error.
#[allow(non_camel_case_types)] // C ABI name, emitted verbatim by cbindgen
pub type corvid_scan_fn = Option<
    extern "C" fn(
        ctx: *mut c_void,
        key: *const u8,
        key_len: usize,
        doc: *const corvid_value,
    ) -> c_int,
>;

/// The §7 NULL-checked coll borrow shared by every function in this
/// module (mutation.rs owns the identical twin — each family keeps its
/// own to keep the recorded fn names honest).
fn borrow_coll_checked<'a>(
    fn_name: &str,
    c: *mut corvid_coll,
) -> Option<&'a crate::handle::CollHandle> {
    if c.is_null() {
        record_argument(format!("{fn_name}: c is NULL").as_str());
        return None;
    }
    // SAFETY: c is non-NULL (checked) and contractually a live
    // corvid_collection product; the coll family is thread-safe (spec
    // §2), so a shared borrow is sound.
    unsafe { borrow_coll(c) }
}

/// Fetch and decode the document at `key` (spec §4.9; counterpart:
/// `Collection::get -> Option<Value>`): `*out` receives an OWNED value
/// — free it with `corvid_value_free`. **Absence is a success**:
/// `CORVID_OK` + `*out == NULL` when the key holds no document.
/// `CORVID_ERR` on failure. `out` is required (spec §4.9: "out
/// non-NULL" — the one read whose out-param is marked so); `key` as
/// everywhere (non-NULL, any length).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_get(
    c: *mut corvid_coll,
    key: *const u8,
    key_len: usize,
    out: *mut *mut corvid_value,
) -> corvid_status {
    if out.is_null() {
        record_argument("corvid_get: out is NULL (required — spec §4.9)");
        return corvid_status::CORVID_ERR;
    }
    // SAFETY: out is non-NULL (checked); define the absent/failed shape
    // up front so no path leaves it dangling.
    unsafe { *out = std::ptr::null_mut() };
    let (Some(coll), Some(key)) = (
        borrow_coll_checked("corvid_get", c),
        borrowed_bytes("corvid_get", "key", key, key_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_get", || coll.collection().get(key)) {
        Some(Some(doc)) => {
            // SAFETY: out is non-NULL (checked at the top).
            unsafe { *out = crate::handle::into_value(doc) };
            corvid_status::CORVID_OK
        }
        Some(None) => corvid_status::CORVID_OK, // absence is a success
        None => corvid_status::CORVID_ERR,
    }
}

/// Stream every `(key, document)` in the collection to `fn`, in key
/// order (spec §4.9; counterpart: `Collection::for_each_doc(FnMut(&[u8],
/// Value) -> Result<bool>)` — the callback-shaped engine twin of the
/// materializing `Collection::scan`). Constant memory regardless of
/// collection size. The callback returns 1 to continue, 0 to stop
/// (stopping is not an error — `CORVID_OK` either way); `key`/`doc`
/// are BORROWED for the callback's duration only (see
/// [`corvid_scan_fn`]'s reentrancy contract).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_scan(
    c: *mut corvid_coll,
    fn_: corvid_scan_fn,
    ctx: *mut c_void,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_scan", c) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(fn_) = fn_ else {
        record_argument("corvid_scan: fn is NULL");
        return corvid_status::CORVID_ERR;
    };
    // A panicking callback must not unwind across the C boundary or the
    // engine's for_each frame (spec §3's defensive rule): catch, stop
    // the scan, report.
    let panicked = std::cell::Cell::new(false);
    let walked = guard("corvid_scan", || {
        coll.collection().for_each_doc(|key, doc| {
            // SAFETY: fn_ is non-NULL (checked) and contractually a
            // valid callback; key borrows the engine's bytes and doc is
            // our decoded local — both valid for the call per §1.6's
            // callback-scoped borrow.
            let ret = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fn_(
                    ctx,
                    key.as_ptr(),
                    key.len(),
                    &doc as *const corvid::Value as *const corvid_value,
                )
            }));
            match ret {
                Ok(1) => Ok(true),
                Ok(_) => Ok(false),
                Err(_) => {
                    panicked.set(true);
                    Ok(false)
                }
            }
        })
    });
    if panicked.get() {
        crate::error::record(
            crate::error::corvid_err::CORVID_E_DATABASE,
            "corvid_scan: callback panicked",
        );
        return corvid_status::CORVID_ERR;
    }
    match walked {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Keyset pagination (spec §4.9; counterpart:
/// `Collection::page(after: Option<&[u8]>, limit) -> Page { rows, next }`):
/// up to `limit` documents in key order strictly after `after`, from
/// one MVCC snapshot. `after == NULL || after_len == 0` starts at the
/// beginning; `limit == 0` returns empty rows and no cursor.
///
/// `*rows_out` (required) receives an OWNED rows cursor holding the
/// page's materialized rows with score 0.0 — walk it with
/// `corvid_rows_next` / free it with `corvid_rows_free` (Task 5's
/// cursor family; the handle itself is produced here).
///
/// `*next_after_out` (nullable, as is `next_after_len_out`) receives
/// the resume cursor — an ABI-owned byte buffer, **free it with
/// `corvid_free`** — or NULL with `*next_after_len_out == 0` at the end
/// of the collection. The buffer is allocated only when
/// `next_after_out` is non-NULL; a caller that ignores pagination may
/// pass NULL for the pair.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_page(
    c: *mut corvid_coll,
    after: *const u8,
    after_len: usize,
    limit: usize,
    rows_out: *mut *mut corvid_rows,
    next_after_out: *mut *mut u8,
    next_after_len_out: *mut usize,
) -> corvid_status {
    if rows_out.is_null() {
        record_argument("corvid_page: rows_out is NULL (required)");
        return corvid_status::CORVID_ERR;
    }
    // SAFETY: rows_out is non-NULL (checked); define the failed shape
    // up front.
    unsafe { *rows_out = std::ptr::null_mut() };
    if !next_after_out.is_null() {
        // SAFETY: next_after_out is non-NULL (checked).
        unsafe { *next_after_out = std::ptr::null_mut() };
    }
    if !next_after_len_out.is_null() {
        // SAFETY: next_after_len_out is non-NULL (checked).
        unsafe { *next_after_len_out = 0 };
    }

    let Some(coll) = borrow_coll_checked("corvid_page", c) else {
        return corvid_status::CORVID_ERR;
    };
    // after is nullable-with-semantics (§7): NULL or length 0 starts at
    // the beginning; a NULL pointer with nonzero length is the
    // unexpected-NULL shape instead.
    let after = if after.is_null() || after_len == 0 {
        if after_len > 0 {
            record_argument(
                "corvid_page: after is NULL with after_len > 0 (§7's \
                 NULL-with-nonzero-length shape)",
            );
            return corvid_status::CORVID_ERR;
        }
        None
    } else {
        match borrowed_bytes("corvid_page", "after", after, after_len) {
            Some(bytes) => Some(bytes),
            None => return corvid_status::CORVID_ERR,
        }
    };

    let Some(page) = guard("corvid_page", || coll.collection().page(after, limit)) else {
        return corvid_status::CORVID_ERR;
    };
    // Page rows become ResultRows with score 0.0 (spec §4.9): the same
    // cursor shape corvid_query_run will hand out, one walker for both.
    let rows = page
        .rows
        .into_iter()
        .map(|(key, document)| ResultRow {
            key,
            score: 0.0,
            document,
        })
        .collect();
    let rows_ptr = into_rows(RowsHandle::new(rows));

    match page.next {
        Some(next) if !next_after_out.is_null() => {
            let buffer = buffer_new(&next);
            if buffer.is_null() {
                // Allocation failure (theoretic): fail cleanly — no
                // rows handle, no cursor, nothing to free on the
                // caller's side.
                // SAFETY: rows_ptr is a fresh into_rows product not yet
                // handed out; this is its single reclaim.
                drop(unsafe { reclaim_rows(rows_ptr) });
                crate::error::record(
                    crate::error::corvid_err::CORVID_E_DATABASE,
                    "corvid_page: allocating the next_after buffer failed",
                );
                return corvid_status::CORVID_ERR;
            }
            // SAFETY: both out-params were NULL-checked at the top.
            unsafe {
                *next_after_out = buffer;
                if !next_after_len_out.is_null() {
                    *next_after_len_out = next.len();
                }
            }
        }
        Some(next) => {
            // No cursor wanted: still report its length if asked (0 —
            // the buffer side of the pair was declined).
            let _ = next;
        }
        None => {
            // End of collection: NULL cursor with length 0 — already
            // the initialized shape.
        }
    }

    // SAFETY: rows_out is non-NULL (checked at the top).
    unsafe { *rows_out = rows_ptr };
    corvid_status::CORVID_OK
}

/// The document count (spec §4.9; counterpart: `Collection::len ->
/// usize`) — O(1) maintained counter. `out` is nullable (§7's optional
/// out-params: the call still succeeds and writes nothing).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_len(c: *mut corvid_coll, out: *mut usize) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_len", c) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_len", || coll.collection().len()) {
        Some(len) => {
            if !out.is_null() {
                // SAFETY: out is non-NULL (checked).
                unsafe { *out = len };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_err;
    use crate::error::corvid_status::CORVID_ERR;
    use crate::error::corvid_status::CORVID_OK;
    use crate::error::last_code;
    use crate::handle::borrow_rows_mut;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_free;
    use crate::lifecycle::corvid_open_memory;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;

    type Coll = *mut corvid_coll;

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let coll = corvid_collection(db, b"docs".as_ptr() as *const std::ffi::c_char, 4);
        assert!(!coll.is_null());
        (db, coll)
    }

    fn seed(coll: Coll, keys: &[&[u8]]) {
        for (i, key) in keys.iter().enumerate() {
            let v = corvid_value_int(i as i64);
            assert_eq!(
                crate::mutation::corvid_insert(coll, key.as_ptr(), key.len(), v),
                CORVID_OK
            );
            corvid_value_free(v);
        }
    }

    /// Walk a rows cursor through the in-crate plumbing — exactly the
    /// walk `corvid_rows_next` (Task 5) will expose — collecting
    /// (key, score, int-doc). Reclaims the handle so tests leak
    /// nothing while the ABI-side destructor is still Task 5's.
    fn walk_rows(rows: *mut corvid_rows) -> Vec<(Vec<u8>, f32, i64)> {
        let mut out = Vec::new();
        // SAFETY: rows is a live into_rows product (corvid_page's, in
        // these tests); single-threaded exclusive borrow.
        let handle = unsafe { borrow_rows_mut(rows) }.expect("non-NULL handle");
        while let Some(row) = handle.next() {
            let mut ok = 0;
            let n = crate::value::corvid_value_as_int(
                &row.document as *const corvid::Value as *const corvid_value,
                &mut ok,
            );
            assert_eq!(ok, 1, "these tests store int documents");
            out.push((row.key.clone(), row.score, n));
        }
        // SAFETY: the single reclaim of the into_rows product.
        drop(unsafe { reclaim_rows(rows) });
        out
    }

    // --- get -----------------------------------------------------------------

    #[test]
    fn get_pins_present_absent_and_owned_out() {
        let (db, coll) = fresh();
        seed(coll, &[b"a", b"b"]);

        let mut out: *mut corvid_value = std::ptr::null_mut();
        assert_eq!(corvid_get(coll, b"b".as_ptr(), 1, &mut out), CORVID_OK);
        assert!(!out.is_null(), "presence hands out an owned handle");
        let mut ok = 0;
        assert_eq!(crate::value::corvid_value_as_int(out, &mut ok), 1);
        assert_eq!(ok, 1);
        // OWNED: the caller's to free, and no aliasing with the store —
        // free it, re-get, still correct.
        corvid_value_free(out);
        assert_eq!(corvid_get(coll, b"b".as_ptr(), 1, &mut out), CORVID_OK);
        corvid_value_free(out);

        // Absence: CORVID_OK + *out == NULL (never a bare NULL return).
        let mut absent = std::ptr::dangling_mut::<corvid_value>(); // garbage in
        assert_eq!(corvid_get(coll, b"zz".as_ptr(), 2, &mut absent), CORVID_OK);
        assert!(absent.is_null(), "absence is a success with *out NULL");

        // The empty key is a legal lookup.
        assert_eq!(corvid_get(coll, b"".as_ptr(), 0, &mut out), CORVID_OK);
        assert!(out.is_null());

        // §7: NULL out (required here), NULL coll, NULL key.
        assert_eq!(
            corvid_get(coll, b"a".as_ptr(), 1, std::ptr::null_mut()),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_get(std::ptr::null_mut(), b"a".as_ptr(), 1, &mut out),
            CORVID_ERR
        );
        assert_eq!(corvid_get(coll, std::ptr::null(), 0, &mut out), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- scan ------------------------------------------------------------------

    /// The scan test's collector: appends (key, doc-int) pairs, with a
    /// stop-after-N knob (0 = never stop).
    struct ScanCtx {
        seen: Vec<(Vec<u8>, i64)>,
        stop_after: usize,
    }

    extern "C" fn scan_collect(
        ctx: *mut c_void,
        key: *const u8,
        key_len: usize,
        doc: *const corvid_value,
    ) -> c_int {
        // SAFETY: ctx is the test's own ScanCtx; key/doc are the
        // callback-scoped borrows the contract guarantees.
        let ctx = unsafe { &mut *(ctx as *mut ScanCtx) };
        // SAFETY: key is readable for key_len bytes (§1.5), doc is a
        // live borrowed Value for the callback's duration.
        let key = unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec();
        let mut ok = 0;
        let n = crate::value::corvid_value_as_int(doc, &mut ok);
        assert_eq!(ok, 1, "the borrowed doc reads through the value fns");
        ctx.seen.push((key, n));
        c_int::from(ctx.stop_after == 0 || ctx.seen.len() < ctx.stop_after)
    }

    #[test]
    fn scan_streams_in_key_order_and_stops_cleanly() {
        let (db, coll) = fresh();
        // Insert out of key order; the visit order must still be key
        // order (engine for_each).
        seed(coll, &[b"m", b"a", b"z", b"b"]);

        let mut ctx = ScanCtx {
            seen: Vec::new(),
            stop_after: 0,
        };
        assert_eq!(
            corvid_scan(
                coll,
                Some(scan_collect),
                &mut ctx as *mut ScanCtx as *mut c_void
            ),
            CORVID_OK
        );
        assert_eq!(
            ctx.seen.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"b".to_vec(), b"m".to_vec(), b"z".to_vec()],
            "visit order is key order"
        );
        assert_eq!(
            ctx.seen.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            vec![1, 3, 0, 2],
            "each visited doc is the right one"
        );

        // Early stop: 0 return stops, and the status is still OK.
        let mut ctx = ScanCtx {
            seen: Vec::new(),
            stop_after: 2,
        };
        assert_eq!(
            corvid_scan(
                coll,
                Some(scan_collect),
                &mut ctx as *mut ScanCtx as *mut c_void
            ),
            CORVID_OK,
            "stopping is not an error"
        );
        assert_eq!(ctx.seen.len(), 2);

        // Empty collection: zero visits, OK.
        let (db2, empty) = fresh();
        let mut ctx = ScanCtx {
            seen: Vec::new(),
            stop_after: 0,
        };
        assert_eq!(
            corvid_scan(
                empty,
                Some(scan_collect),
                &mut ctx as *mut ScanCtx as *mut c_void
            ),
            CORVID_OK
        );
        assert!(ctx.seen.is_empty());

        // §7: NULL fn, NULL coll.
        assert_eq!(corvid_scan(coll, None, std::ptr::null_mut()), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_scan(
                std::ptr::null_mut(),
                Some(scan_collect),
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );

        corvid_collection_free(coll);
        corvid_collection_free(empty);
        assert_eq!(corvid_close(db), CORVID_OK);
        assert_eq!(corvid_close(db2), CORVID_OK);
    }

    // --- page -------------------------------------------------------------------

    #[test]
    fn page_walks_the_full_collection_through_the_cursor_chain() {
        let (db, coll) = fresh();
        let keys: Vec<Vec<u8>> = (0..25u8).map(|i| vec![i]).collect();
        let slices: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        seed(coll, &slices);

        // Walk in pages of 10 via the next_after chain: after=NULL
        // (start), then each returned cursor, until the end.
        let mut seen: Vec<u8> = Vec::new();
        let mut after: *mut u8 = std::ptr::null_mut();
        let mut after_len = 0usize;
        let mut first = true;
        let mut pages = 0;
        loop {
            let after_arg = if first { std::ptr::null() } else { after };
            let after_len_arg = if first { 0 } else { after_len };
            first = false;

            let mut rows: *mut corvid_rows = std::ptr::null_mut();
            let mut next: *mut u8 = std::ptr::null_mut();
            let mut next_len = 0usize;
            assert_eq!(
                corvid_page(
                    coll,
                    after_arg,
                    after_len_arg,
                    10,
                    &mut rows,
                    &mut next,
                    &mut next_len,
                ),
                CORVID_OK
            );
            assert!(!rows.is_null(), "even an empty page returns a cursor");

            let walked = walk_rows(rows);
            pages += 1;
            for (key, score, _) in &walked {
                assert_eq!(*score, 0.0, "page rows carry score 0.0 (spec §4.9)");
                seen.push(key[0]);
            }

            if next.is_null() {
                assert_eq!(next_len, 0, "end of collection: NULL cursor, zero length");
                break;
            }
            // The resume cursor is exactly the last key of this page
            // (keyset semantics) — and it is corvid_free-able.
            // SAFETY: next is a buffer_new product with next_len bytes.
            assert_eq!(
                unsafe { std::slice::from_raw_parts(next, next_len) },
                walked.last().unwrap().0
            );
            if !after.is_null() {
                corvid_free(after as *mut c_void); // the PREVIOUS page's cursor
            }
            after = next;
            after_len = next_len;
        }
        corvid_free(after as *mut c_void);

        assert_eq!(pages, 3, "25 rows in pages of 10: 10 + 10 + 5");
        assert_eq!(
            seen,
            (0..25u8).collect::<Vec<u8>>(),
            "every row once, in order"
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    #[test]
    fn page_pins_the_edge_shapes() {
        let (db, coll) = fresh();
        seed(coll, &[b"a", b"b", b"c"]);

        // limit 0: empty rows, no cursor.
        let mut rows: *mut corvid_rows = std::ptr::null_mut();
        let mut next: *mut u8 = std::ptr::null_mut();
        let mut next_len = usize::MAX;
        assert_eq!(
            corvid_page(
                coll,
                std::ptr::null(),
                0,
                0,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_OK
        );
        assert!(walk_rows(rows).is_empty());
        assert!(next.is_null() && next_len == 0);

        // after with length 0 (non-NULL): the beginning, like NULL.
        assert_eq!(
            corvid_page(
                coll,
                b"".as_ptr(),
                0,
                2,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_OK
        );
        let walked = walk_rows(rows);
        assert_eq!(walked.len(), 2);
        // SAFETY: next is a buffer_new product with next_len bytes.
        assert_eq!(unsafe { std::slice::from_raw_parts(next, next_len) }, b"b");
        corvid_free(next as *mut c_void);

        // after past the end: empty rows, no cursor.
        assert_eq!(
            corvid_page(
                coll,
                b"zz".as_ptr(),
                2,
                2,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_OK
        );
        assert!(walk_rows(rows).is_empty());
        assert!(next.is_null() && next_len == 0);

        // limit greater than the remainder: a short page ends the walk.
        assert_eq!(
            corvid_page(
                coll,
                b"b".as_ptr(),
                1,
                10,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_OK
        );
        assert_eq!(walk_rows(rows).len(), 1, "only c remains after b");
        assert!(next.is_null(), "a short page means the end");

        // The cursor pair is optional: NULL next_after_out succeeds and
        // reports length 0.
        assert_eq!(
            corvid_page(
                coll,
                std::ptr::null(),
                0,
                2,
                &mut rows,
                std::ptr::null_mut(),
                &mut next_len
            ),
            CORVID_OK
        );
        assert_eq!(walk_rows(rows).len(), 2);
        assert_eq!(next_len, 0, "no buffer wanted: the length side reads 0");

        // §7 shapes: NULL rows_out (required); NULL after with nonzero
        // length.
        assert_eq!(
            corvid_page(
                coll,
                std::ptr::null(),
                0,
                1,
                std::ptr::null_mut(),
                &mut next,
                &mut next_len
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_page(
                coll,
                std::ptr::null(),
                5,
                1,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_ERR,
            "NULL after with after_len > 0 is the §7 unexpected-NULL shape"
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_page(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                1,
                &mut rows,
                &mut next,
                &mut next_len
            ),
            CORVID_ERR
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- len ----------------------------------------------------------------------

    #[test]
    fn len_is_the_maintained_count() {
        let (db, coll) = fresh();
        let mut n = usize::MAX;
        assert_eq!(corvid_len(coll, &mut n), CORVID_OK);
        assert_eq!(n, 0, "a never-written collection is empty (lazy creation)");
        seed(coll, &[b"a", b"b"]);
        assert_eq!(corvid_len(coll, &mut n), CORVID_OK);
        assert_eq!(n, 2);

        // out is nullable (§7's optional out-params): still a success.
        assert_eq!(corvid_len(coll, std::ptr::null_mut()), CORVID_OK);

        assert_eq!(corvid_len(std::ptr::null_mut(), &mut n), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }
}
