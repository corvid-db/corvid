//! The string-cursor family plumbing (spec §4.12): `corvid_strs_next` and
//! `corvid_strs_free`.
//!
//! The cursor is the shared read shape for every ABI call that returns
//! `Vec<String>` (spec §2: `corvid_collections` — landed — plus the graph
//! family's `corvid_neighbors` / `corvid_in_neighbors` /
//! `corvid_traverse`, Task 6). Single-threaded use by contract; the
//! strings are binary-safe `(pointer, length)` pairs, NOT NUL-terminated
//! (spec §1.5), borrowed until the next call or `_free`.

use std::ffi::c_char;
use std::ffi::c_int;

use crate::error::record_argument;
use crate::handle::borrow_strs_mut;
use crate::handle::corvid_strs;
use crate::handle::reclaim_strs;

/// Advance the cursor (spec §4.12): returns 1 and fills `*str_out` /
/// `*len_out` for the next string, 0 at exhaustion — out-params
/// untouched at 0; never errors (the list is materialized). The string
/// bytes are BORROWED until the next `corvid_strs_next` or
/// `corvid_strs_free` on this handle — using or freeing them after is UB.
///
/// NULL handle or NULL out-parameter follows the non-status rule (spec
/// §7): defined inert value (0 = exhausted) AND `CORVID_E_ARGUMENT`
/// recorded — never UB, and never a status return (there is none).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_strs_next(
    s: *mut corvid_strs,
    str_out: *mut *const c_char,
    len_out: *mut usize,
) -> c_int {
    if s.is_null() || str_out.is_null() || len_out.is_null() {
        record_argument("corvid_strs_next: NULL handle or out-param (§7 inert rule)");
        return 0;
    }
    // SAFETY: handle non-NULL (checked) with corvid_collections (and
    // later graph) provenance, not yet freed; the §2 contract confines a
    // cursor to one thread, so the exclusive borrow is sound.
    let cursor = unsafe { borrow_strs_mut(s) }.expect("non-NULL checked above");
    match cursor.next() {
        Some(text) => {
            // SAFETY: both out-params are non-NULL (checked); the pointer
            // + length pair is §1.5's binary-safe string shape.
            unsafe {
                *str_out = text.as_ptr() as *const c_char;
                *len_out = text.len();
            }
            1
        }
        None => 0,
    }
}

/// Free the cursor (spec §4.12). `corvid_strs_free(NULL)` is a no-op
/// (spec §7). Cross-family frees are UB (spec §2).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_strs_free(s: *mut corvid_strs) {
    // SAFETY: NULL is the documented no-op; otherwise s is a
    // corvid_collections/graph product, reclaimed exactly once here.
    drop(unsafe { reclaim_strs(s) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::StrsHandle;
    use crate::handle::into_strs;

    #[test]
    fn next_walks_every_string_then_sticks() {
        let cursor = into_strs(StrsHandle::new(vec!["alpha".into(), "beta".into()]));
        let mut str_ptr: *const c_char = std::ptr::null();
        let mut len = 0;

        assert_eq!(corvid_strs_next(cursor, &mut str_ptr, &mut len), 1);
        // SAFETY: str_ptr borrows the cursor's current string (valid
        // until the next call, which we make only after reading).
        assert_eq!(
            unsafe { std::slice::from_raw_parts(str_ptr as *const u8, len) },
            b"alpha"
        );

        assert_eq!(corvid_strs_next(cursor, &mut str_ptr, &mut len), 1);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(str_ptr as *const u8, len) },
            b"beta"
        );

        // Exhaustion: 0, out-params untouched.
        str_ptr = std::ptr::null();
        len = usize::MAX;
        assert_eq!(corvid_strs_next(cursor, &mut str_ptr, &mut len), 0);
        assert!(str_ptr.is_null());
        assert_eq!(len, usize::MAX);

        corvid_strs_free(cursor);
    }

    #[test]
    fn null_discipline_yields_the_inert_value() {
        // Each NULL shape: 0 returned, E_ARGUMENT recorded (§7).
        let mut str_ptr: *const c_char = std::ptr::null();
        let mut len = 0;

        assert_eq!(
            corvid_strs_next(std::ptr::null_mut(), &mut str_ptr, &mut len),
            0
        );
        assert_eq!(
            crate::error::last_code(),
            crate::error::corvid_err::CORVID_E_ARGUMENT
        );

        let cursor = into_strs(StrsHandle::new(vec!["x".into()]));
        assert_eq!(corvid_strs_next(cursor, std::ptr::null_mut(), &mut len), 0);
        assert_eq!(
            crate::error::last_code(),
            crate::error::corvid_err::CORVID_E_ARGUMENT
        );
        assert_eq!(
            corvid_strs_next(cursor, &mut str_ptr, std::ptr::null_mut()),
            0
        );
        assert_eq!(
            crate::error::last_code(),
            crate::error::corvid_err::CORVID_E_ARGUMENT
        );
        corvid_strs_free(cursor);
    }

    #[test]
    fn empty_strings_carry_a_non_null_pointer_with_len_zero() {
        // §1.5: empty is a non-NULL pointer with length 0.
        let cursor = into_strs(StrsHandle::new(vec![String::new()]));
        let mut str_ptr: *const c_char = std::ptr::null();
        let mut len = usize::MAX;
        assert_eq!(corvid_strs_next(cursor, &mut str_ptr, &mut len), 1);
        assert!(!str_ptr.is_null());
        assert_eq!(len, 0);
        corvid_strs_free(cursor);
    }
}
