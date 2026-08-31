//! Lifecycle & errors (spec §4.1): the eight functions of the ABI's first
//! family — `corvid_ffi_version`, `corvid_open`, `corvid_open_memory`,
//! `corvid_close`, `corvid_last_error_code`,
//! `corvid_last_error_message`, `corvid_free`, `corvid_collections`.
//!
//! NULL discipline (spec §7): the status-returning `corvid_close` answers
//! an unexpected NULL with `CORVID_ERR` + `CORVID_E_ARGUMENT`; the
//! handle-returning constructors answer with NULL + a recorded error.
//! `corvid_free(NULL)` is a documented no-op.

use std::alloc::Layout;
use std::alloc::alloc;
use std::alloc::dealloc;
use std::ffi::c_char;
use std::ffi::c_void;

use crate::FFI_VERSION;
use crate::error::corvid_err;
use crate::error::corvid_status;
use crate::error::guard;
use crate::error::last_code;
use crate::error::last_message;
use crate::error::record_argument;
use crate::handle::DbHandle;
use crate::handle::StrsHandle;
use crate::handle::borrow_db;
use crate::handle::corvid_db;
use crate::handle::corvid_strs;
use crate::handle::into_db;
use crate::handle::into_strs;

/// The ABI version (spec §4.1/§8): `1`. Bindings verify this before
/// anything else. No engine counterpart — pure ABI versioning.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_ffi_version() -> u32 {
    FFI_VERSION
}

/// Open (creating if absent) a file-backed database. `path` is borrowed,
/// non-NULL, and must be valid UTF-8 (spec §1.5 — one encoding rule for
/// every ABI string). Wraps `corvid::Db::open`; returns the handle, or
/// NULL with `CORVID_E_DATABASE` / `CORVID_E_INCOMPATIBLE_FORMAT` /
/// `CORVID_E_IO` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_open(path: *const c_char, path_len: usize) -> *mut corvid_db {
    if path.is_null() {
        record_argument("corvid_open: path is NULL");
        return std::ptr::null_mut();
    }
    // SAFETY: path is non-NULL (checked above) and the caller guarantees
    // it is valid for path_len reads (spec §1.5's borrowed bytes).
    let bytes = unsafe { std::slice::from_raw_parts(path as *const u8, path_len) };
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            record_argument("corvid_open: path is not valid UTF-8 (spec §1.5)");
            return std::ptr::null_mut();
        }
    };
    match guard("corvid_open", || corvid::Db::open(text)) {
        Some(db) => into_db(DbHandle::new(db)),
        None => std::ptr::null_mut(),
    }
}

/// A purely in-memory database (no file). Wraps
/// `corvid::Db::open_in_memory`; fails only on engine-internal storage
/// errors (never in practice).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_open_memory() -> *mut corvid_db {
    match guard("corvid_open_memory", corvid::Db::open_in_memory) {
        Some(db) => into_db(DbHandle::new(db)),
        None => std::ptr::null_mut(),
    }
}

/// Release the handle's reference (spec §2/§4.1). Dropping the last
/// reference releases the `Db` and its file locks; derived handles keep
/// the engine alive independently. No engine counterpart — Rust drops
/// `Db`, and persistence is durable per-transaction.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_close(db: *mut corvid_db) -> corvid_status {
    // SAFETY: NULL falls through to the argument error (spec §7); any
    // non-NULL ptr is contractually a corvid_open/corvid_open_memory
    // product not yet closed — reclaim consumes it exactly once.
    match unsafe { crate::handle::reclaim_db(db) } {
        Some(_body) => corvid_status::CORVID_OK,
        None => {
            record_argument("corvid_close: db is NULL");
            corvid_status::CORVID_ERR
        }
    }
}

/// The thread-local last-error code (spec §3): one of the 19 codes,
/// `CORVID_E_OK` when nothing failed on this thread. Successful calls
/// never clear it.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_last_error_code() -> corvid_err {
    last_code()
}

/// The thread-local last-error message (spec §3/§4.1): NUL-terminated for
/// convenience, `*len_out` receives the byte length (`len_out` nullable).
/// Returns NULL when no error is recorded on this thread. The pointer is
/// valid until the next failing corvid call on this thread (or thread
/// exit) — copy it if you need it longer.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_last_error_message(len_out: *mut usize) -> *const c_char {
    match last_message() {
        None => {
            if !len_out.is_null() {
                // SAFETY: len_out is non-NULL (checked).
                unsafe { *len_out = 0 };
            }
            std::ptr::null()
        }
        Some((ptr, len)) => {
            if !len_out.is_null() {
                // SAFETY: len_out is non-NULL (checked).
                unsafe { *len_out = len };
            }
            ptr
        }
    }
}

/// ABI-owned byte buffers carry a hidden length header — one `usize`
/// word before the returned pointer, allocation aligned to `usize` —
/// because `corvid_free` receives only the pointer and Rust's global
/// allocator deallocates with the exact [`Layout`]. The header is what a
/// C `malloc` implementation keeps in its own chunk metadata; keeping it
/// on our side keeps the crate std-only and the free side sound for
/// every buffer shape. Returned pointer, never the header.
const BUFFER_HEADER: usize = core::mem::size_of::<usize>();

/// Allocate an ABI-owned buffer holding `bytes` (spec §4.1: the buffer
/// shape behind `corvid_insert_auto` keys and `corvid_page`'s
/// `next_after`; those producers land with Task 4). Returns NULL only on
/// allocation failure — callers translate that to their signature's
/// failure shape.
#[allow(dead_code)] // first producer lands with Task 4 (insert_auto/page)
pub(crate) fn buffer_new(bytes: &[u8]) -> *mut u8 {
    // A slice's length is bounded well below isize::MAX, so the layout
    // arithmetic cannot overflow.
    let layout = Layout::from_size_align(BUFFER_HEADER + bytes.len(), BUFFER_HEADER)
        .expect("slice length keeps the buffer layout in range");
    // SAFETY: the layout has non-zero size (>= BUFFER_HEADER) and a valid
    // power-of-two alignment.
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: raw is a live allocation of BUFFER_HEADER + bytes.len()
    // bytes, so the header word and the payload fit; the returned pointer
    // skips the hidden header (the caller's bytes start there).
    unsafe {
        (raw as *mut usize).write_unaligned(bytes.len());
        raw.add(BUFFER_HEADER)
            .copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
        raw.add(BUFFER_HEADER)
    }
}

/// Reclaim a [`buffer_new`] buffer. Provenance is the contract: the only
/// ABI-owned buffers are born in [`buffer_new`] (see its doc), and
/// `corvid_free` is their only deallocator.
///
/// # Safety
///
/// `ptr` is NULL or was returned by [`buffer_new`] and not yet freed.
unsafe fn buffer_drop(ptr: *mut u8) {
    // SAFETY: the header sits BUFFER_HEADER bytes before the returned
    // pointer, written unaligned by buffer_new.
    let raw = unsafe { ptr.sub(BUFFER_HEADER) };
    let len = unsafe { (raw as *const usize).read_unaligned() };
    let layout = Layout::from_size_align(BUFFER_HEADER + len, BUFFER_HEADER)
        .expect("header was written by buffer_new, whose layout is in range");
    // SAFETY: raw came from alloc with exactly this layout (buffer_new).
    unsafe { dealloc(raw, layout) };
}

/// The ONLY buffer deallocator in the ABI (spec §4.1/§5 rule 1): frees
/// any buffer the ABI returned by value — `corvid_insert_auto` keys,
/// `corvid_page`'s `next_after` cursor. Does NOT free handles (each has
/// its own `_free`) or values. The domain is exactly those ABI-returned
/// buffers (spec §4.1): freeing a pointer the ABI did not return, or
/// freeing one twice, is undefined behavior — the same class of misuse
/// as C `free()`. `corvid_free(NULL)` is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: every ABI-owned buffer is produced by buffer_new (the sole
    // producer — see its doc), which lays out the hidden header that
    // buffer_drop reads back to reconstruct the allocation layout.
    unsafe { buffer_drop(ptr as *mut u8) }
}

/// User collection names (engine-internal `__` namespaces excluded), in
/// name order, as a string cursor driven by `corvid_strs_next` /
/// `corvid_strs_free` (spec §4.12). Wraps `corvid::Db::collections`.
/// Listing does not create anything — an empty-but-never-written
/// collection may not appear. Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_collections(db: *mut corvid_db) -> *mut corvid_strs {
    // SAFETY: NULL maps to None (spec §7); a non-NULL handle has
    // corvid_open/open_memory provenance and `corvid_db` is the
    // thread-safe family (spec §2) — a shared borrow is fine.
    let Some(handle) = (unsafe { borrow_db(db) }) else {
        record_argument("corvid_collections: db is NULL");
        return std::ptr::null_mut();
    };
    match guard("corvid_collections", || handle.engine().collections()) {
        Some(names) => into_strs(StrsHandle::new(names)),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_version_is_one() {
        assert_eq!(corvid_ffi_version(), 1);
    }

    #[test]
    fn open_requires_a_non_null_utf8_path() {
        // NULL path: §7's unexpected-NULL rule for handle constructors.
        assert!(corvid_open(std::ptr::null(), 0).is_null());
        assert_eq!(
            crate::error::last_code(),
            corvid_err::CORVID_E_ARGUMENT,
            "NULL path records E_ARGUMENT"
        );

        // Non-UTF-8 path bytes: §1.5's encoding rule, checked never UB.
        let bad = [0xFF_u8, 0xFE];
        assert!(corvid_open(bad.as_ptr() as *const c_char, bad.len()).is_null(),);
        assert_eq!(crate::error::last_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn open_failure_records_the_engine_code_and_display_text() {
        // A path under a missing parent directory fails inside redb's
        // open — the spec §4.1 error set for open (E_DATABASE et al.).
        let path = format!(
            "{}/corvid-ffi-no-such-dir/db.corvid",
            std::env::temp_dir().display()
        );
        let path_bytes = path.as_bytes();
        assert!(corvid_open(path_bytes.as_ptr() as *const c_char, path_bytes.len()).is_null());
        let code = crate::error::last_code();
        assert!(
            code == corvid_err::CORVID_E_DATABASE
                || code == corvid_err::CORVID_E_IO
                || code == corvid_err::CORVID_E_INCOMPATIBLE_FORMAT,
            "open failure outside the spec §4.1 error set: {code:?}"
        );
        let mut len = usize::MAX;
        let msg = corvid_last_error_message(&mut len);
        assert!(!msg.is_null(), "failure always pairs a message (spec §3)");
        assert!(len > 0);
    }

    #[test]
    fn open_close_round_trip_on_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.corvid");
        let path_bytes = path.to_str().unwrap().as_bytes();
        let db = corvid_open(path_bytes.as_ptr() as *const c_char, path_bytes.len());
        assert!(!db.is_null());
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
        // Reopen succeeds: the file was created and cleanly released.
        let db = corvid_open(path_bytes.as_ptr() as *const c_char, path_bytes.len());
        assert!(!db.is_null());
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn close_null_is_an_argument_error() {
        // Status functions answer NULL with CORVID_ERR + E_ARGUMENT
        // (spec §7); the _free no-op exception is for _free-named
        // destructors, and corvid_close is not one.
        assert_eq!(
            corvid_close(std::ptr::null_mut()),
            corvid_status::CORVID_ERR
        );
        assert_eq!(crate::error::last_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn last_error_message_is_null_without_an_error() {
        // This test thread has recorded nothing (each test runs on its
        // own thread, so the slot starts clean).
        let mut len = usize::MAX;
        assert!(corvid_last_error_message(&mut len).is_null());
        assert_eq!(len, 0);
        assert_eq!(corvid_last_error_code(), corvid_err::CORVID_E_OK);
        // len_out itself is nullable (spec §4.1).
        assert!(corvid_last_error_message(std::ptr::null_mut()).is_null());
    }

    #[test]
    fn successes_do_not_clear_the_last_error() {
        // §3: read the error immediately after the failure that interests
        // you — a later success must not erase it.
        let bad = [0xFF_u8];
        assert!(corvid_open(bad.as_ptr() as *const c_char, 1).is_null());
        assert_eq!(crate::error::last_code(), corvid_err::CORVID_E_ARGUMENT);

        let db = corvid_open_memory();
        assert!(!db.is_null());
        assert_eq!(
            corvid_last_error_code(),
            corvid_err::CORVID_E_ARGUMENT,
            "a successful call never clears the slot"
        );
        // The message survives too, with its byte length.
        let mut len = 0;
        let msg = corvid_last_error_message(&mut len);
        assert!(!msg.is_null() && len > 0);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
        // A closing success does not clear it either.
        assert_eq!(corvid_last_error_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn collections_on_an_empty_db_yields_an_empty_cursor() {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let strs = corvid_collections(db);
        assert!(!strs.is_null(), "empty db, not failure (NULL means error)");

        let mut str_ptr: *const c_char = std::ptr::null();
        let mut str_len = usize::MAX;
        assert_eq!(
            crate::strs::corvid_strs_next(strs, &mut str_ptr, &mut str_len),
            0,
            "empty db: the cursor is immediately exhausted"
        );
        assert_eq!(
            str_len,
            usize::MAX,
            "exhaustion leaves out-params untouched"
        );
        crate::strs::corvid_strs_free(strs);
        assert_eq!(corvid_close(db), corvid_status::CORVID_OK);
    }

    #[test]
    fn collections_null_db_is_an_argument_error() {
        assert!(corvid_collections(std::ptr::null_mut()).is_null());
        assert_eq!(crate::error::last_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn free_null_is_a_no_op() {
        // §7: corvid_free(NULL) and every _free(NULL) are no-ops — the
        // proof this does not dereference is that it returns.
        corvid_free(std::ptr::null_mut());
        crate::strs::corvid_strs_free(std::ptr::null_mut());
    }

    #[test]
    fn buffers_round_trip_through_the_hidden_length_header() {
        // The buffer machinery behind corvid_free (producers land with
        // Task 4): byte-exact payload, arbitrary lengths including empty.
        for payload in [&b"hello corvid"[..], b"", &[0_u8; 64][..]] {
            let ptr = buffer_new(payload);
            assert!(!ptr.is_null());
            // SAFETY: ptr is a buffer_new product; the header places
            // payload.len() readable bytes at ptr.
            let back = unsafe { std::slice::from_raw_parts(ptr, payload.len()) };
            assert_eq!(back, payload);
            // Free through the ABI entry point, not just buffer_drop.
            corvid_free(ptr as *mut c_void);
        }
    }
}
