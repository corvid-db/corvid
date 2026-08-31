//! Value construction & reads (spec §4.3/§4.4) — the 23 value functions
//! over the ABI's document noun: 11 constructors (§4.3) and 12 readers
//! (§4.4).
//!
//! # Handle shapes: owned boxes and borrowed views
//!
//! A `corvid_value*` points at a `corvid::Value`. It has exactly two
//! provenances (spec §2/§5 rule 6):
//!
//! * **OWNED** — every constructor here, `corvid_value_clone`, and
//!   (Task 4) `corvid_get` / query min-max return box pointers;
//!   `corvid_value_free` is their only destructor.
//! * **BORROWED** — `corvid_value_array_get` / `corvid_value_map_get`
//!   return interior pointers straight into the parent value's `Vec` /
//!   `BTreeMap` storage: a lightweight borrowed-view handle, not a new
//!   allocation (a parent-held child registry was the alternative
//!   considered here — the plan itself only asked for "owned-vs-borrowed
//!   children", which the interior view satisfies as written; it won
//!   because it is zero-cost on the build-a-document hot path and its
//!   lifetime IS the spec's wording — the child "rides the parent").
//!
//! Consequences, bold per the plan's §4.6:
//!
//! * A borrowed child (or a `_ref` buffer) is valid until the parent's
//!   **next mutation** (`corvid_value_array_push` may reallocate the
//!   Vec; `corvid_value_map_put` drops a replaced child, and a new-key
//!   put can split a B-tree node that relocates existing entries — the
//!   conservative rule) **or free**. Using it after either is
//!   **undefined behavior**.
//! * **Calling `corvid_value_free` on a borrowed child is undefined
//!   behavior** — it is not a box pointer; the free corrupts the
//!   allocator. Passing a borrowed child where an owned value is
//!   consumed (`array_push` / `map_put`) is the same UB. Misuse looks
//!   like `corvid_value_free(corvid_value_array_get(a, 0))` or
//!   `corvid_value_free(corvid_value_map_get(m, k))` — both compile in C
//!   (the const casts away) and both are UB. The shape is undetectable
//!   at runtime (an interior pointer's bits are unremarkable), so the
//!   contract is this documentation plus the Task 7 ASan/LSan CI runs.
//!
//! # Input ownership (spec §5)
//!
//! Constructors CLONE their byte/text/vector buffers — the caller keeps
//! its memory and may reuse it immediately. `array_push` / `map_put`
//! CONSUME their value argument unconditionally (spec §8: even a failed
//! call has consumed it; do not free it afterwards). A duplicate
//! `map_put` key REPLACES the previous entry (engine `BTreeMap::insert`,
//! last write wins; the replaced child is dropped).
//!
//! # NULL discipline (spec §7)
//!
//! The status-returning `array_push` / `map_put` answer NULL or
//! misencoded input with `CORVID_ERR` + `CORVID_E_ARGUMENT`; the
//! non-status readers answer with their signature's inert value (0,
//! `*ok = 0`, NULL pointer) AND record `CORVID_E_ARGUMENT`. A
//! wrong-type read (`as_int` on Text, `vector_ref` on Bytes) is NOT an
//! error: `*ok = 0` / NULL pointer, nothing recorded — mirroring the
//! engine's `Option` accessors. These constructors' pointer inputs are
//! non-nullable at ANY length (spec §1.5): empty is a non-NULL pointer
//! with length 0.

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::ffi::c_int;

use corvid::Value;

use crate::error::corvid_status;
use crate::error::record_argument;
use crate::handle::borrow_value;
use crate::handle::borrow_value_mut;
use crate::handle::corvid_value;
use crate::handle::into_value;
use crate::handle::reclaim_value;

/// The value discriminant (FFI.md §1.4, frozen per §8): tags 0..=8,
/// identical to the engine value module's private encoding tags. The
/// engine's constants are not `pub`, so the correspondence is pinned by
/// the `type_tags_are_frozen...` test instead of a const reference.
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_value_type {
    /// `Value::Null` — absence of a value.
    CORVID_TYPE_NULL = 0,
    /// `Value::Bool` — 0/1.
    CORVID_TYPE_BOOL = 1,
    /// `Value::Int` — 64-bit signed; exact to 2^53 vs Float.
    CORVID_TYPE_INT = 2,
    /// `Value::Float` — 64-bit IEEE (NaN/±inf/-0.0 preserved).
    CORVID_TYPE_FLOAT = 3,
    /// `Value::Text` — UTF-8 bytes.
    CORVID_TYPE_TEXT = 4,
    /// `Value::Bytes` — opaque bytes.
    CORVID_TYPE_BYTES = 5,
    /// `Value::Array` — ordered list.
    CORVID_TYPE_ARRAY = 6,
    /// `Value::Map` — string-keyed map; documents are Maps.
    CORVID_TYPE_MAP = 7,
    /// `Value::Vector` — dense f32 embedding.
    CORVID_TYPE_VECTOR = 8,
}

/// The discriminant of `v` (spec §1.4).
fn tag_of(v: &Value) -> corvid_value_type {
    match v {
        Value::Null => corvid_value_type::CORVID_TYPE_NULL,
        Value::Bool(_) => corvid_value_type::CORVID_TYPE_BOOL,
        Value::Int(_) => corvid_value_type::CORVID_TYPE_INT,
        Value::Float(_) => corvid_value_type::CORVID_TYPE_FLOAT,
        Value::Text(_) => corvid_value_type::CORVID_TYPE_TEXT,
        Value::Bytes(_) => corvid_value_type::CORVID_TYPE_BYTES,
        Value::Array(_) => corvid_value_type::CORVID_TYPE_ARRAY,
        Value::Map(_) => corvid_value_type::CORVID_TYPE_MAP,
        Value::Vector(_) => corvid_value_type::CORVID_TYPE_VECTOR,
    }
}

/// Borrow `len` bytes at `ptr` for a non-nullable pointer parameter, or
/// `None` (having recorded `CORVID_E_ARGUMENT`) when it is NULL — at any
/// length, per spec §1.5's "empty is a non-NULL pointer with length 0".
///
/// The shared §1.5/§7 NULL-checked slice constructor for every family
/// that takes borrowed bytes (the Task 3 report's note: reuse, don't
/// re-derive).
pub(crate) fn borrowed_bytes<'a>(
    fn_name: &str,
    param: &str,
    ptr: *const u8,
    len: usize,
) -> Option<&'a [u8]> {
    if ptr.is_null() {
        record_argument(&format!(
            "{fn_name}: {param} is NULL (empty is a non-NULL pointer with \
             length 0, spec §1.5)"
        ));
        return None;
    }
    // SAFETY: ptr is non-NULL (checked) and the caller guarantees it is
    // valid for len reads — spec §1.5's borrowed-bytes contract.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Borrow `len` bytes at `ptr` as a `&str` for a non-nullable UTF-8
/// string parameter (spec §1.5 — engine strings are Rust `&str`/`String`),
/// or `None` (having recorded `CORVID_E_ARGUMENT`) when it is NULL or not
/// valid UTF-8. Shared by every family with a string parameter, alongside
/// [`borrowed_bytes`].
pub(crate) fn borrowed_utf8<'a>(
    fn_name: &str,
    param: &str,
    ptr: *const c_char,
    len: usize,
) -> Option<&'a str> {
    // borrowed_bytes performs the NULL check (§7) and the SAFETY-noted
    // raw-parts borrow (§1.5); from_utf8 is checked, never UB.
    let bytes = borrowed_bytes(fn_name, param, ptr as *const u8, len)?;
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(_) => {
            record_argument(&format!(
                "{fn_name}: {param} is not valid UTF-8 (spec §1.5)"
            ));
            None
        }
    }
}

/// Borrow the engine value behind a REQUIRED-non-NULL
/// `const corvid_value*` input, or `None` (having recorded
/// `CORVID_E_ARGUMENT`) when the pointer is NULL. The §5 rule-3 read
/// half: such inputs are cloned by their consumer when ownership enters
/// a tree or the engine; this helper only performs the §7 NULL check
/// and the SAFETY-noted borrow.
pub(crate) fn borrowed_value<'a>(
    fn_name: &str,
    param: &str,
    v: *const corvid_value,
) -> Option<&'a corvid::Value> {
    if v.is_null() {
        record_argument(format!("{fn_name}: {param} is NULL").as_str());
        return None;
    }
    // SAFETY: v is non-NULL (checked) and contractually an owned handle
    // or a live borrowed child (spec §2/§4.4) on this thread.
    unsafe { borrow_value(v) }
}

// --- §4.3 construction ------------------------------------------------------

/// `Value::Null` (spec §4.3). Infallible: allocation failure aborts like
/// any Rust allocation, matching the engine's `Value::Null` literal.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_null() -> *mut corvid_value {
    into_value(Value::Null)
}

/// `Value::Bool(v != 0)` (spec §4.3). Infallible; any non-zero `v` —
/// including negatives — is true.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_bool(v: c_int) -> *mut corvid_value {
    into_value(Value::Bool(v != 0))
}

/// `Value::Int` (spec §4.3). Infallible; `i64::MIN`/`MAX` cross exactly.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_int(v: i64) -> *mut corvid_value {
    into_value(Value::Int(v))
}

/// `Value::Float` (spec §4.3). Infallible; NaN payloads, ±inf, and -0.0
/// cross bit-exact (the engine stores the f64 as-is).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_float(v: f64) -> *mut corvid_value {
    into_value(Value::Float(v))
}

/// `Value::Text` (spec §4.3): the bytes are CLONED into the value — the
/// caller keeps its buffer. `s` must be valid UTF-8 (spec §1.5 — engine
/// strings are Rust `String`s) and non-NULL at any length. NULL or
/// invalid UTF-8 returns NULL with `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_text(s: *const c_char, len: usize) -> *mut corvid_value {
    let Some(bytes) = borrowed_bytes("corvid_value_text", "s", s as *const u8, len) else {
        return std::ptr::null_mut();
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => into_value(Value::Text(text.to_owned())),
        Err(_) => {
            record_argument("corvid_value_text: s is not valid UTF-8 (spec §1.5)");
            std::ptr::null_mut()
        }
    }
}

/// `Value::Bytes` (spec §4.3): CLONED, arbitrary bytes (spec §1.5 —
/// byte payloads are binary-safe). `b` non-NULL at any length; NULL
/// returns NULL + `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_bytes(b: *const u8, len: usize) -> *mut corvid_value {
    match borrowed_bytes("corvid_value_bytes", "b", b, len) {
        Some(bytes) => into_value(Value::Bytes(bytes.to_vec())),
        None => std::ptr::null_mut(),
    }
}

/// `Value::Vector` (spec §4.3): the floats are CLONED; `dim` 0 is legal
/// (an empty vector value — pass any non-NULL pointer with dim 0, spec
/// §1.5's empty shape). NULL `v` at any dim returns NULL +
/// `CORVID_E_ARGUMENT`. NaN/-0.0/±inf f32s cross bit-exact.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_vector(v: *const f32, dim: usize) -> *mut corvid_value {
    if v.is_null() {
        record_argument(
            "corvid_value_vector: v is NULL (dim 0 is legal with a \
             non-NULL pointer, spec §1.5)",
        );
        return std::ptr::null_mut();
    }
    // SAFETY: v is non-NULL (checked) and the caller guarantees it is
    // valid for dim reads (spec §1.5's borrowed buffer).
    let floats = unsafe { std::slice::from_raw_parts(v, dim) };
    into_value(Value::Vector(floats.to_vec()))
}

/// `Value::Array(vec![])` (spec §4.3) — the array builder root. Infallible.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_array_new() -> *mut corvid_value {
    into_value(Value::Array(Vec::new()))
}

/// Append `item` to `arr` (spec §4.3), **consuming** it: ownership moves
/// into the array — do not free or reuse `item` afterwards, whatever the
/// status (spec §8: consumption is unconditional; a failed push has
/// still dropped the item). `arr` must be an OWNED array value built by
/// `corvid_value_array_new` (or cloned from one) — any other value fails
/// with `CORVID_ERR` + `CORVID_E_ARGUMENT`. Pushing **invalidates every
/// child and `_ref` buffer previously borrowed from `arr`** (spec §5
/// rule 6: the Vec may reallocate) — using them after is UB. On the
/// self-insertion rejection path (`item == arr`) the shared handle has
/// already been consumed by the call — free neither pointer afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_array_push(
    arr: *mut corvid_value,
    item: *mut corvid_value,
) -> corvid_status {
    if item.is_null() {
        record_argument("corvid_value_array_push: item is NULL");
        return corvid_status::CORVID_ERR;
    }
    if std::ptr::eq(arr, item) {
        record_argument(
            "corvid_value_array_push: item aliases arr (a value cannot \
             contain itself)",
        );
        // SAFETY: item is non-NULL (checked) and contractually an OWNED
        // handle; reclaiming it is the unconditional consumption (§8) —
        // the alias is rejected without borrowing either.
        drop(unsafe { reclaim_value(item) });
        return corvid_status::CORVID_ERR;
    }
    // Consume FIRST so that every failure path below has already taken
    // the item (spec §8 — a failed push still consumes).
    // SAFETY: item is non-NULL (checked) and contractually an OWNED
    // handle (constructor/clone product); passing a borrowed child as a
    // consumed value is the documented UB of §4.4.
    let item = unsafe { reclaim_value(item) }.expect("non-NULL checked above");
    // SAFETY: arr is contractually an OWNED value handle not yet freed;
    // spec §2's single-thread contract makes the exclusive borrow sound.
    match unsafe { borrow_value_mut(arr) } {
        Some(Value::Array(items)) => {
            items.push(*item);
            corvid_status::CORVID_OK
        }
        Some(_) => {
            record_argument(
                "corvid_value_array_push: arr is not an array built by \
                 corvid_value_array_new",
            );
            corvid_status::CORVID_ERR
        }
        None => {
            record_argument("corvid_value_array_push: arr is NULL");
            corvid_status::CORVID_ERR
        }
    }
}

/// `Value::Map(BTreeMap::new())` (spec §4.3) — the map builder root.
/// Infallible. Map iteration order in the engine is sorted by key —
/// construction order does not matter for equality or encoding.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_map_new() -> *mut corvid_value {
    into_value(Value::Map(BTreeMap::new()))
}

/// Insert `val` under `key` (spec §4.3), **consuming** `val`
/// unconditionally (spec §8 — a failed put has still dropped it; do not
/// free it afterwards). `key` is borrowed, non-NULL at any length (the
/// empty key is legal), and must be valid UTF-8 (spec §1.5 — map keys
/// are Rust `String`s). A put **invalidates every child and `_ref`
/// buffer previously borrowed from `map`** (spec §5 rule 6), whatever
/// the key: a duplicate key REPLACES the previous entry (engine
/// `BTreeMap::insert`, last write wins — the replaced child is dropped),
/// and a NEW key can split a B-tree node, relocating even untouched
/// existing entries — the conservative rule, same as `array_push`'s.
/// Using a previously borrowed child after any put is UB. On the
/// self-insertion rejection path (`val == map`) the shared handle has
/// already been consumed by the call — free neither pointer afterwards.
/// `map` must be an OWNED map value built by `corvid_value_map_new`
/// (or cloned from one).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_map_put(
    map: *mut corvid_value,
    key: *const c_char,
    key_len: usize,
    val: *mut corvid_value,
) -> corvid_status {
    if val.is_null() {
        record_argument("corvid_value_map_put: val is NULL");
        return corvid_status::CORVID_ERR;
    }
    if std::ptr::eq(map, val) {
        record_argument(
            "corvid_value_map_put: val aliases map (a value cannot \
             contain itself)",
        );
        // SAFETY: val is non-NULL (checked) and contractually an OWNED
        // handle; reclaiming it is the unconditional consumption (§8).
        drop(unsafe { reclaim_value(val) });
        return corvid_status::CORVID_ERR;
    }
    // Consume FIRST so that every failure path below has already taken
    // the value (spec §8 — a failed put still consumes).
    // SAFETY: val is non-NULL (checked) and contractually an OWNED
    // handle (constructor/clone product); passing a borrowed child as a
    // consumed value is the documented UB of §4.4.
    let val = unsafe { reclaim_value(val) }.expect("non-NULL checked above");
    let Some(key_bytes) = borrowed_bytes("corvid_value_map_put", "key", key as *const u8, key_len)
    else {
        return corvid_status::CORVID_ERR;
    };
    let Ok(key) = std::str::from_utf8(key_bytes) else {
        record_argument("corvid_value_map_put: key is not valid UTF-8 (spec §1.5)");
        return corvid_status::CORVID_ERR;
    };
    // SAFETY: map is contractually an OWNED value handle not yet freed;
    // spec §2's single-thread contract makes the exclusive borrow sound.
    match unsafe { borrow_value_mut(map) } {
        Some(Value::Map(entries)) => {
            entries.insert(key.to_owned(), *val);
            corvid_status::CORVID_OK
        }
        Some(_) => {
            record_argument(
                "corvid_value_map_put: map is not a map built by \
                 corvid_value_map_new",
            );
            corvid_status::CORVID_ERR
        }
        None => {
            record_argument("corvid_value_map_put: map is NULL");
            corvid_status::CORVID_ERR
        }
    }
}

// --- §4.4 reads -------------------------------------------------------------

/// The value's discriminant (spec §4.4; counterpart: the `Value`
/// variant). A NULL `v` follows the non-status rule (§7): returns
/// `CORVID_TYPE_NULL` (0) AND records `CORVID_E_ARGUMENT` — the same
/// bits as a real Null value, which is the price of having no status
/// channel; distinguish by reading the recorded error.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_type(v: *const corvid_value) -> corvid_value_type {
    // SAFETY: NULL maps to the inert arm (§7); otherwise v is an owned
    // handle or a live borrowed child (the §4.4 contract) on this
    // thread.
    match unsafe { borrow_value(v) } {
        Some(value) => tag_of(value),
        None => {
            record_argument("corvid_value_type: v is NULL (§7 inert rule)");
            corvid_value_type::CORVID_TYPE_NULL
        }
    }
}

/// Typed read with an ok-flag (spec §4.4; counterpart:
/// `Value::as_bool -> Option<bool>`). A wrong type sets `*ok = 0` and
/// returns 0 — NOT an error, nothing recorded. A NULL `v` or NULL `ok`
/// follows the non-status rule (§7): `*ok = 0` (when `ok` is itself
/// readable), return 0, `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_as_bool(v: *const corvid_value, ok: *mut c_int) -> c_int {
    if ok.is_null() {
        record_argument("corvid_value_as_bool: ok is NULL (§7 inert rule)");
        return 0;
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_as_bool: v is NULL (§7 inert rule)");
        // SAFETY: ok is non-NULL (checked at the top).
        unsafe { *ok = 0 };
        return 0;
    };
    // SAFETY: ok is non-NULL (checked at the top).
    unsafe { *ok = c_int::from(value.as_bool().is_some()) };
    c_int::from(value.as_bool().unwrap_or(false))
}

/// Typed read with an ok-flag (spec §4.4; counterpart:
/// `Value::as_int -> Option<i64>`). A wrong type sets `*ok = 0` and
/// returns 0 — NOT an error, nothing recorded. A NULL `v` or NULL `ok`
/// follows the non-status rule (§7): `*ok = 0`, return 0,
/// `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_as_int(v: *const corvid_value, ok: *mut c_int) -> i64 {
    if ok.is_null() {
        record_argument("corvid_value_as_int: ok is NULL (§7 inert rule)");
        return 0;
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_as_int: v is NULL (§7 inert rule)");
        // SAFETY: ok is non-NULL (checked at the top).
        unsafe { *ok = 0 };
        return 0;
    };
    // SAFETY: ok is non-NULL (checked at the top).
    unsafe { *ok = c_int::from(value.as_int().is_some()) };
    value.as_int().unwrap_or(0)
}

/// Typed read with an ok-flag (spec §4.4; counterpart:
/// `Value::as_float -> Option<f64>`). A wrong type sets `*ok = 0` and
/// returns 0.0 — NOT an error, nothing recorded. A NULL `v` or NULL
/// `ok` follows the non-status rule (§7): `*ok = 0`, return 0.0,
/// `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_as_float(v: *const corvid_value, ok: *mut c_int) -> f64 {
    if ok.is_null() {
        record_argument("corvid_value_as_float: ok is NULL (§7 inert rule)");
        return 0.0;
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_as_float: v is NULL (§7 inert rule)");
        // SAFETY: ok is non-NULL (checked at the top).
        unsafe { *ok = 0 };
        return 0.0;
    };
    // SAFETY: ok is non-NULL (checked at the top).
    unsafe { *ok = c_int::from(value.as_float().is_some()) };
    value.as_float().unwrap_or(0.0)
}

/// Zero-copy BORROWED view of the text (spec §4.4; counterpart:
/// `Value::as_text`). NULL when the value is of a different type — not
/// an error; `*len_out` set to 0. NULL `v` or NULL `len_out` follows the
/// non-status rule (§7): NULL pointer (`*len_out = 0` when readable) and
/// `CORVID_E_ARGUMENT` recorded. The buffer points into the value's own
/// storage: valid until the parent value is freed or mutated, and
/// **writing through it is UB**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_text_ref(
    v: *const corvid_value,
    len_out: *mut usize,
) -> *const c_char {
    if len_out.is_null() {
        record_argument("corvid_value_text_ref: len_out is NULL (§7 inert rule)");
        return std::ptr::null();
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_text_ref: v is NULL (§7 inert rule)");
        // SAFETY: len_out is non-NULL (checked at the top).
        unsafe { *len_out = 0 };
        return std::ptr::null();
    };
    match value.as_text() {
        Some(text) => {
            // SAFETY: len_out is non-NULL (checked at the top); the
            // pointer + length pair is §1.5's binary-safe string shape,
            // borrowed from the value's storage (not NUL-terminated).
            unsafe { *len_out = text.len() };
            text.as_ptr() as *const c_char
        }
        None => {
            // Wrong type: inert NULL, NOT an error (spec §4.4).
            // SAFETY: len_out is non-NULL (checked at the top).
            unsafe { *len_out = 0 };
            std::ptr::null()
        }
    }
}

/// Zero-copy BORROWED view of the bytes (spec §4.4; counterpart:
/// `Value::as_bytes`). NULL on a different type — not an error;
/// `*len_out` set to 0. NULL `v` or NULL `len_out` follows §7's inert
/// rule (NULL pointer + `CORVID_E_ARGUMENT` recorded). Valid until the
/// parent value is freed or mutated; **writing through it is UB**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_bytes_ref(v: *const corvid_value, len_out: *mut usize) -> *const u8 {
    if len_out.is_null() {
        record_argument("corvid_value_bytes_ref: len_out is NULL (§7 inert rule)");
        return std::ptr::null();
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_bytes_ref: v is NULL (§7 inert rule)");
        // SAFETY: len_out is non-NULL (checked at the top).
        unsafe { *len_out = 0 };
        return std::ptr::null();
    };
    match value.as_bytes() {
        Some(bytes) => {
            // SAFETY: len_out is non-NULL (checked at the top); the
            // pointer + length pair is §1.5's borrowed-bytes shape.
            unsafe { *len_out = bytes.len() };
            bytes.as_ptr()
        }
        None => {
            // Wrong type: inert NULL, NOT an error (spec §4.4).
            // SAFETY: len_out is non-NULL (checked at the top).
            unsafe { *len_out = 0 };
            std::ptr::null()
        }
    }
}

/// Zero-copy BORROWED view of the vector (spec §4.4; counterpart:
/// `Value::as_vector`). NULL on a different type — not an error;
/// `*dim_out` set to 0. NULL `v` or NULL `dim_out` follows §7's inert
/// rule (NULL pointer + `CORVID_E_ARGUMENT` recorded). Valid until the
/// parent value is freed or mutated; **writing through it is UB**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_vector_ref(
    v: *const corvid_value,
    dim_out: *mut usize,
) -> *const f32 {
    if dim_out.is_null() {
        record_argument("corvid_value_vector_ref: dim_out is NULL (§7 inert rule)");
        return std::ptr::null();
    }
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_vector_ref: v is NULL (§7 inert rule)");
        // SAFETY: dim_out is non-NULL (checked at the top).
        unsafe { *dim_out = 0 };
        return std::ptr::null();
    };
    match value.as_vector() {
        Some(vector) => {
            // SAFETY: dim_out is non-NULL (checked at the top); the
            // pointer + dim pair is §1.5's borrowed-buffer shape. An
            // empty vector yields a non-NULL dangling-but-aligned
            // pointer with dim 0 (§1.5's empty shape).
            unsafe { *dim_out = vector.len() };
            vector.as_ptr()
        }
        None => {
            // Wrong type: inert NULL, NOT an error (spec §4.4).
            // SAFETY: dim_out is non-NULL (checked at the top).
            unsafe { *dim_out = 0 };
            std::ptr::null()
        }
    }
}

/// BORROWED child at `index` (spec §4.4; counterpart: `Vec` indexing).
/// NULL when `arr` is not an array or `index` is out of range — not an
/// error, nothing recorded. A NULL `arr` follows §7's inert rule (NULL +
/// `CORVID_E_ARGUMENT` recorded). The child is an interior view into the
/// parent's storage: valid until the parent's next mutation or free
/// (spec §5 rule 6), and **calling `corvid_value_free` on it is UB**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_array_get(
    arr: *const corvid_value,
    index: usize,
) -> *const corvid_value {
    let Some(value) = (unsafe { borrow_value(arr) }) else {
        record_argument("corvid_value_array_get: arr is NULL (§7 inert rule)");
        return std::ptr::null();
    };
    match value {
        Value::Array(items) => items.get(index).map_or(std::ptr::null(), |child| {
            child as *const Value as *const corvid_value
        }),
        // Wrong container kind: inert NULL, not an error (spec §4.4).
        _ => std::ptr::null(),
    }
}

/// BORROWED child under `key` (spec §4.4; counterpart: `Value::get`).
/// NULL when `map` is not a map or the key is absent — not an error,
/// nothing recorded. A NULL `map` or NULL `key` follows §7's inert rule
/// (NULL + `CORVID_E_ARGUMENT` recorded); a non-UTF-8 `key` likewise
/// (spec §1.5 — map keys are Rust `String`s). The child is an interior
/// view into the parent's storage: valid until the parent's next
/// mutation or free (spec §5 rule 6), and **calling
/// `corvid_value_free` on it is UB**.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_map_get(
    map: *const corvid_value,
    key: *const c_char,
    key_len: usize,
) -> *const corvid_value {
    let Some(value) = (unsafe { borrow_value(map) }) else {
        record_argument("corvid_value_map_get: map is NULL (§7 inert rule)");
        return std::ptr::null();
    };
    let Some(key_bytes) = borrowed_bytes("corvid_value_map_get", "key", key as *const u8, key_len)
    else {
        return std::ptr::null();
    };
    let Ok(key) = std::str::from_utf8(key_bytes) else {
        record_argument("corvid_value_map_get: key is not valid UTF-8 (spec §1.5)");
        return std::ptr::null();
    };
    value.get(key).map_or(std::ptr::null(), |child| {
        child as *const Value as *const corvid_value
    })
}

/// The value's length (spec §4.4): array items / map entries / vector
/// dimensions / text bytes / bytes bytes; 0 for null, bool, int, float.
/// A NULL `v` returns 0 with `CORVID_E_ARGUMENT` recorded (§7). No
/// single engine method — the collection lengths (`Vec::len`,
/// `BTreeMap::len`, `String::len`) it reports.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_len(v: *const corvid_value) -> usize {
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_len: v is NULL (§7 inert rule)");
        return 0;
    };
    match value {
        Value::Text(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Array(items) => items.len(),
        Value::Map(entries) => entries.len(),
        Value::Vector(v) => v.len(),
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => 0,
    }
}

/// Deep copy returning an OWNED value (spec §4.4; counterpart:
/// `Value::clone` via `#[derive(Clone)]`). This is the sanctioned way to
/// keep data observed through a borrowed child or `_ref` buffer beyond
/// the parent's lifetime (e.g. a `rows` document). A NULL `v` returns
/// NULL + `CORVID_E_ARGUMENT` (§7 — the handle-returning failure shape).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_clone(v: *const corvid_value) -> *mut corvid_value {
    let Some(value) = (unsafe { borrow_value(v) }) else {
        record_argument("corvid_value_clone: v is NULL");
        return std::ptr::null_mut();
    };
    into_value(value.clone())
}

/// Free an OWNED value (spec §4.4; counterpart: Rust `Drop`).
/// `corvid_value_free(NULL)` is a no-op (§7). **Calling it on a borrowed
/// child — from `_ref`, `array_get`, `map_get`, `rows_next`,
/// `geohits_next`, callbacks, or a value already consumed by
/// `array_push`/`map_put` — is undefined behavior** (spec §4.4, bold):
/// those pointers are interior views or already-dead boxes, not this
/// destructor's domain.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_value_free(v: *mut corvid_value) {
    // SAFETY: NULL is the documented no-op; otherwise v is contractually
    // an OWNED handle (constructor/clone/corvid_get product) reclaimed
    // exactly once here. Freeing a borrowed child is the documented UB
    // above — undetectable at runtime (an interior pointer's bits are
    // unremarkable); the Task 7 ASan/LSan CI surface is the net.
    drop(unsafe { reclaim_value(v) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::corvid_err;
    use crate::error::last_code;

    // --- test helpers ------------------------------------------------------

    /// Build a text value from a &str (panic-free shortcut for the happy
    /// path; the unhappy paths construct pointers by hand).
    fn text(s: &str) -> *mut corvid_value {
        corvid_value_text(s.as_ptr() as *const c_char, s.len())
    }

    fn bytes(b: &[u8]) -> *mut corvid_value {
        corvid_value_bytes(b.as_ptr(), b.len())
    }

    fn vector(v: &[f32]) -> *mut corvid_value {
        corvid_value_vector(v.as_ptr(), v.len())
    }

    /// Push asserting success (the consumed item is gone either way).
    fn push(arr: *mut corvid_value, item: *mut corvid_value) {
        assert_eq!(corvid_value_array_push(arr, item), corvid_status::CORVID_OK);
    }

    /// Put asserting success (the consumed value is gone either way).
    fn put(map: *mut corvid_value, key: &str, val: *mut corvid_value) {
        assert_eq!(
            corvid_value_map_put(map, key.as_ptr() as *const c_char, key.len(), val),
            corvid_status::CORVID_OK
        );
    }

    /// The mutations.rs oracle adapted to the ABI: does the value behind
    /// `actual` equal `expected`, bit-exactly for floats (derived
    /// `PartialEq` cannot see NaN payloads or -0.0)? Maps are walked by
    /// the EXPECTED keys (the ABI has no key-enumeration surface by
    /// design — §4.4 reads by key), with the length pinned so extra
    /// entries cannot hide.
    fn matches(expected: &Value, actual: *const corvid_value) -> bool {
        if actual.is_null() || corvid_value_type(actual) != tag_of(expected) {
            return false;
        }
        let mut ok: c_int = 0;
        match expected {
            Value::Null => true,
            Value::Bool(b) => corvid_value_as_bool(actual, &mut ok) == c_int::from(*b) && ok == 1,
            Value::Int(n) => corvid_value_as_int(actual, &mut ok) == *n && ok == 1,
            Value::Float(f) => {
                corvid_value_as_float(actual, &mut ok).to_bits() == f.to_bits() && ok == 1
            }
            Value::Text(s) => {
                let mut len = 0;
                let p = corvid_value_text_ref(actual, &mut len);
                !p.is_null()
                    && len == s.len()
                    // SAFETY: p is the borrowed view of a live parent, and
                    // len bytes are readable there (§1.5 shape).
                    && unsafe { std::slice::from_raw_parts(p as *const u8, len) } == s.as_bytes()
            }
            Value::Bytes(b) => {
                let mut len = 0;
                let p = corvid_value_bytes_ref(actual, &mut len);
                !p.is_null()
                    && len == b.len()
                    // SAFETY: p is the borrowed view, len bytes readable.
                    && unsafe { std::slice::from_raw_parts(p, len) } == b.as_slice()
            }
            Value::Vector(v) => {
                let mut dim = 0;
                let p = corvid_value_vector_ref(actual, &mut dim);
                !p.is_null()
                    && dim == v.len()
                    && v.iter().enumerate().all(|(i, f)| {
                        // SAFETY: p is the borrowed view; dim was checked
                        // against the expected length above.
                        unsafe { *p.add(i) }.to_bits() == f.to_bits()
                    })
            }
            Value::Array(items) => {
                corvid_value_len(actual) == items.len()
                    && items
                        .iter()
                        .enumerate()
                        .all(|(i, item)| matches(item, corvid_value_array_get(actual, i)))
            }
            Value::Map(entries) => {
                corvid_value_len(actual) == entries.len()
                    && entries.iter().all(|(k, val)| {
                        matches(
                            val,
                            corvid_value_map_get(actual, k.as_ptr() as *const c_char, k.len()),
                        )
                    })
            }
        }
    }

    /// The expected map behind a builder, mirroring mutations.rs's `map`.
    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    // --- §1.4 the discriminant ---------------------------------------------

    #[test]
    fn type_tags_are_frozen_zero_through_eight() {
        // §1.4/§8: explicit values, never renumbered; #[repr(u32)] is the
        // representation that crosses the boundary.
        let tags = [
            (corvid_value_type::CORVID_TYPE_NULL, 0u32),
            (corvid_value_type::CORVID_TYPE_BOOL, 1),
            (corvid_value_type::CORVID_TYPE_INT, 2),
            (corvid_value_type::CORVID_TYPE_FLOAT, 3),
            (corvid_value_type::CORVID_TYPE_TEXT, 4),
            (corvid_value_type::CORVID_TYPE_BYTES, 5),
            (corvid_value_type::CORVID_TYPE_ARRAY, 6),
            (corvid_value_type::CORVID_TYPE_MAP, 7),
            (corvid_value_type::CORVID_TYPE_VECTOR, 8),
        ];
        for (tag, n) in tags {
            assert_eq!(tag as u32, n);
        }
        // tag_of agrees with the engine's variant set one-for-one (the
        // encoding-tag correspondence the spec pins).
        assert_eq!(tag_of(&Value::Null), corvid_value_type::CORVID_TYPE_NULL);
        assert_eq!(
            tag_of(&Value::Bool(true)),
            corvid_value_type::CORVID_TYPE_BOOL
        );
        assert_eq!(tag_of(&Value::Int(0)), corvid_value_type::CORVID_TYPE_INT);
        assert_eq!(
            tag_of(&Value::Float(0.0)),
            corvid_value_type::CORVID_TYPE_FLOAT
        );
        assert_eq!(
            tag_of(&Value::Text(String::new())),
            corvid_value_type::CORVID_TYPE_TEXT
        );
        assert_eq!(
            tag_of(&Value::Bytes(Vec::new())),
            corvid_value_type::CORVID_TYPE_BYTES
        );
        assert_eq!(
            tag_of(&Value::Array(Vec::new())),
            corvid_value_type::CORVID_TYPE_ARRAY
        );
        assert_eq!(
            tag_of(&Value::Map(BTreeMap::new())),
            corvid_value_type::CORVID_TYPE_MAP
        );
        assert_eq!(
            tag_of(&Value::Vector(Vec::new())),
            corvid_value_type::CORVID_TYPE_VECTOR
        );
    }

    // --- §4.3 construction ---------------------------------------------------

    #[test]
    fn scalars_round_trip_bit_exact() {
        let mut ok: c_int = 9;

        // bool: 0/1 and any non-zero (incl. negative) is true.
        for (raw, want) in [(0, false), (1, true), (-5, true), (256, true)] {
            let h = corvid_value_bool(raw);
            assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_BOOL);
            assert_eq!(corvid_value_as_bool(h, &mut ok), c_int::from(want));
            assert_eq!(ok, 1);
            corvid_value_free(h);
        }

        // int: the exact 64-bit domain, both extremes included.
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            let h = corvid_value_int(v);
            assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_INT);
            assert_eq!(corvid_value_as_int(h, &mut ok), v);
            assert_eq!(ok, 1);
            corvid_value_free(h);
        }

        // float: bit-exact across the IEEE boundary members — NaN (with
        // and without a payload), -0.0, ±inf — the derived PartialEq
        // blind spots the oracle pins by bits.
        let floats = [
            2.5f64,
            f64::NAN,
            f64::from_bits(0x7FF8_ABCD_0000_0001), // NaN, custom payload
            f64::from_bits(0xFFF8_0000_0000_0002), // NaN, negative quiet
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN,
            f64::MAX,
        ];
        for v in floats {
            let h = corvid_value_float(v);
            assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_FLOAT);
            assert_eq!(corvid_value_as_float(h, &mut ok).to_bits(), v.to_bits());
            assert_eq!(ok, 1);
            corvid_value_free(h);
        }
    }

    #[test]
    fn text_bytes_vector_round_trip_binary_safe() {
        // text: unicode (multi-byte), empty, and an interior NUL (valid
        // UTF-8 — binary-safe pointer+len, §1.5, never NUL-terminated).
        for s in ["héllo 🐦 数", "", "a\0b"] {
            let h = text(s);
            assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_TEXT);
            assert!(matches(&Value::Text(s.to_owned()), h));
            corvid_value_free(h);
        }
        // non-UTF-8 text: checked, never UB (§1.5).
        let bad = [0xFF_u8, 0xFE];
        assert!(corvid_value_text(bad.as_ptr() as *const c_char, bad.len()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // bytes: arbitrary binary including 0x00 and 0xFF; empty.
        for b in [&b""[..], &[0_u8, 1, 2, 255][..]] {
            let h = bytes(b);
            assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_BYTES);
            assert!(matches(&Value::Bytes(b.to_vec()), h));
            corvid_value_free(h);
        }

        // vector: f32 boundary members bit-exact; empty (dim 0) legal.
        let floats = [
            0.0f32,
            -0.0,
            f32::NAN,
            f32::from_bits(0x7FC0_0001), // NaN payload
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.5,
            3.25,
        ];
        let h = vector(&floats);
        assert_eq!(corvid_value_type(h), corvid_value_type::CORVID_TYPE_VECTOR);
        assert!(matches(&Value::Vector(floats.to_vec()), h));
        corvid_value_free(h);

        let empty: [f32; 0] = [];
        let h = corvid_value_vector(empty.as_ptr(), 0);
        let mut dim = usize::MAX;
        let p = corvid_value_vector_ref(h, &mut dim);
        assert!(!p.is_null(), "§1.5: empty is a non-NULL pointer, len 0");
        assert_eq!(dim, 0);
        corvid_value_free(h);

        // NULL buffers are argument errors at ANY length (§1.5).
        assert!(corvid_value_text(std::ptr::null(), 0).is_null());
        assert!(corvid_value_bytes(std::ptr::null(), 0).is_null());
        assert!(corvid_value_vector(std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
    }

    #[test]
    fn constructors_clone_their_buffers() {
        // Text: caller keeps its bytes and may reuse them immediately
        // (only the first 6 bytes cross: "corvid" out of the longer
        // buffer).
        let mut buf = *b"corvid-value-buffer!!";
        let len = 6; // "corvid"
        let h = corvid_value_text(buf.as_ptr() as *const c_char, len);
        buf[0] = b'X';
        let mut out = 0;
        let p = corvid_value_text_ref(h, &mut out);
        // SAFETY: p is the borrowed view of the live value; out bytes
        // readable (§1.5 shape).
        assert_eq!(
            unsafe { std::slice::from_raw_parts(p as *const u8, out) },
            b"corvid"
        );
        assert_eq!(out, 6);
        corvid_value_free(h);

        // Bytes: same contract.
        let mut raw = [1_u8, 2, 3];
        let h = bytes(&raw);
        raw[0] = 99;
        let mut out = 0;
        let p = corvid_value_bytes_ref(h, &mut out);
        // SAFETY: borrowed view of the live value.
        assert_eq!(unsafe { std::slice::from_raw_parts(p, out) }, [1, 2, 3]);
        corvid_value_free(h);

        // Vector: same contract.
        let mut floats = [1.0f32, 2.0, 3.0];
        let h = vector(&floats);
        floats[0] = 99.0;
        let mut dim = 0;
        let p = corvid_value_vector_ref(h, &mut dim);
        // SAFETY: borrowed view of the live value.
        assert_eq!(unsafe { *p }.to_bits(), 1.0f32.to_bits());
        corvid_value_free(h);
    }

    #[test]
    fn arrays_build_read_and_nest() {
        let arr = corvid_value_array_new();
        assert_eq!(corvid_value_type(arr), corvid_value_type::CORVID_TYPE_ARRAY);
        assert_eq!(corvid_value_len(arr), 0);
        assert!(corvid_value_array_get(arr, 0).is_null()); // out of range: not an error

        push(arr, corvid_value_int(1));
        push(arr, text("two"));
        push(arr, corvid_value_bool(1));
        assert_eq!(corvid_value_len(arr), 3);

        let mut ok = 0;
        let first = corvid_value_array_get(arr, 0);
        assert!(!first.is_null());
        assert_eq!(corvid_value_as_int(first, &mut ok), 1);
        assert_eq!(ok, 1);
        assert!(matches(
            &Value::Array(vec![
                Value::Int(1),
                Value::Text("two".into()),
                Value::Bool(true)
            ]),
            arr
        ));
        assert!(corvid_value_array_get(arr, 3).is_null(), "one past the end");

        // Depth 3+ of nested arrays, read through chained borrows.
        let innermost = corvid_value_array_new();
        push(innermost, corvid_value_int(42));
        let inner = corvid_value_array_new();
        push(inner, innermost); // consumed
        let outer = corvid_value_array_new();
        push(outer, inner); // consumed
        let expected = Value::Array(vec![Value::Array(vec![Value::Array(vec![Value::Int(42)])])]);
        assert!(matches(&expected, outer));
        let d1 = corvid_value_array_get(outer, 0);
        let d2 = corvid_value_array_get(d1, 0);
        let d3 = corvid_value_array_get(d2, 0);
        assert!(!d3.is_null() && corvid_value_as_int(d3, &mut ok) == 42);
        corvid_value_free(outer);

        // Pushing into a non-array: ERR + E_ARGUMENT; the item is
        // consumed either way (§8) — freed inside, never after.
        let not_array = corvid_value_int(5);
        assert_eq!(
            corvid_value_array_push(not_array, corvid_value_int(1)),
            corvid_status::CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_value_free(not_array);

        // NULL shapes: both fail; the non-NULL item is consumed on the
        // NULL-arr path (do not free it afterwards).
        assert_eq!(
            corvid_value_array_push(arr, std::ptr::null_mut()),
            corvid_status::CORVID_ERR
        );
        assert_eq!(
            corvid_value_array_push(std::ptr::null_mut(), corvid_value_int(7)),
            corvid_status::CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // Self-push is rejected — and consumes the handle (§8): a2 is
        // gone after this call, so it is deliberately NOT freed below.
        let a2 = corvid_value_array_new();
        assert_eq!(corvid_value_array_push(a2, a2), corvid_status::CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_value_free(arr);
    }

    #[test]
    fn maps_build_replace_and_nest() {
        let m = corvid_value_map_new();
        assert_eq!(corvid_value_type(m), corvid_value_type::CORVID_TYPE_MAP);
        assert_eq!(corvid_value_len(m), 0);
        assert!(corvid_value_map_get(m, b"a".as_ptr() as *const c_char, 1).is_null());

        put(m, "a", corvid_value_int(1));
        put(m, "键", text("unicode key"));
        put(m, "", corvid_value_bool(0)); // the empty key is legal
        assert_eq!(corvid_value_len(m), 3);

        let mut ok = 0;
        let a = corvid_value_map_get(m, b"a".as_ptr() as *const c_char, 1);
        assert!(!a.is_null());
        assert_eq!(corvid_value_as_int(a, &mut ok), 1);

        // Duplicate key REPLACES: last write wins, length unchanged, the
        // replaced child is dropped (its borrows die — §5 rule 6).
        put(m, "a", corvid_value_int(99));
        assert_eq!(corvid_value_len(m), 3);
        assert_eq!(
            corvid_value_as_int(
                corvid_value_map_get(m, b"a".as_ptr() as *const c_char, 1),
                &mut ok
            ),
            99
        );

        // Depth 3+ of nested maps.
        let l3 = corvid_value_map_new();
        put(l3, "deep", corvid_value_float(-0.0));
        let l2 = corvid_value_map_new();
        put(l2, "l3", l3); // consumed
        let l1 = corvid_value_map_new();
        put(l1, "l2", l2); // consumed
        let expected = map(&[("l2", map(&[("l3", map(&[("deep", Value::Float(-0.0))]))]))]);
        assert!(matches(&expected, l1));
        corvid_value_free(l1);

        // Error paths: non-UTF-8 key, NULL key, non-map target — ERR +
        // E_ARGUMENT, the value consumed on every one (§8).
        let bad = [0xFF_u8];
        assert_eq!(
            corvid_value_map_put(m, bad.as_ptr() as *const c_char, 1, corvid_value_int(2)),
            corvid_status::CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_value_map_put(m, std::ptr::null(), 0, corvid_value_int(3)),
            corvid_status::CORVID_ERR
        );
        let not_map = text("scalar");
        assert_eq!(
            corvid_value_map_put(
                not_map,
                b"k".as_ptr() as *const c_char,
                1,
                corvid_value_int(4)
            ),
            corvid_status::CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_value_free(not_map);

        // Self-put is rejected — and consumes the handle (§8).
        let m2 = corvid_value_map_new();
        assert_eq!(
            corvid_value_map_put(m2, b"k".as_ptr() as *const c_char, 1, m2),
            corvid_status::CORVID_ERR
        );

        corvid_value_free(m);
    }

    // --- §4.4 reads ----------------------------------------------------------

    #[test]
    fn every_variant_round_trips_through_the_abi() {
        // The mutations.rs oracle list, ABI-shaped: each case is built
        // through the constructors (containers via the builders) and
        // read back with `matches` — bit-exact for floats — both from
        // the original handle and from a clone.
        let nested_map = map(&[(
            "l1",
            map(&[(
                "l2",
                map(&[(
                    "l3",
                    map(&[
                        ("vec", Value::Vector(vec![1.0, 2.0])),
                        (
                            "tags",
                            Value::Array(vec![
                                Value::Text("a".into()),
                                Value::Bool(false),
                                Value::Bytes(vec![7, 0, 7]),
                            ]),
                        ),
                    ]),
                )]),
            )]),
        )]);

        let cases: Vec<Value> = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Float(f64::NAN),
            Value::Float(-0.0),
            Value::Float(f64::NEG_INFINITY),
            Value::Text("héllo 🐦 数".into()),
            Value::Text(String::new()),
            Value::Bytes(vec![0, 1, 2, 255]),
            Value::Bytes(Vec::new()),
            Value::Vector(vec![0.0, -1.5, 3.25]),
            Value::Vector(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(vec![
                Value::Int(1),
                Value::Array(vec![Value::Text("inner".into()), Value::Null]),
                Value::Bytes(vec![7, 0, 7]),
            ]),
            Value::Map(BTreeMap::new()),
            map(&[("键", Value::Int(1))]),
            nested_map,
        ];

        for expected in &cases {
            let h = build(expected);
            assert!(matches(expected, h), "round trip failed for {expected:?}");
            let clone = corvid_value_clone(h);
            assert!(
                matches(expected, clone),
                "clone round trip failed for {expected:?}"
            );
            corvid_value_free(clone);
            corvid_value_free(h);
        }
    }

    /// Build `v` through the ABI constructors (the inverse of `matches`,
    /// sharing its expected-driven walk).
    fn build(v: &Value) -> *mut corvid_value {
        match v {
            Value::Null => corvid_value_null(),
            Value::Bool(b) => corvid_value_bool(c_int::from(*b)),
            Value::Int(n) => corvid_value_int(*n),
            Value::Float(f) => corvid_value_float(*f),
            Value::Text(s) => text(s),
            Value::Bytes(b) => bytes(b),
            Value::Vector(v) => vector(v),
            Value::Array(items) => {
                let arr = corvid_value_array_new();
                for item in items {
                    push(arr, build(item));
                }
                arr
            }
            Value::Map(entries) => {
                let m = corvid_value_map_new();
                for (k, val) in entries {
                    put(m, k, build(val));
                }
                m
            }
        }
    }

    #[test]
    fn wrong_type_reads_are_inert_not_errors() {
        // This test thread's slot starts clean (each test runs on its own
        // thread): the wrong-type reads below must NOT record anything.
        assert_eq!(last_code(), corvid_err::CORVID_E_OK);

        let t = text("x");
        let mut ok: c_int = 9;

        // as_* on the wrong type: *ok = 0, inert return, no record.
        assert_eq!(corvid_value_as_int(t, &mut ok), 0);
        assert_eq!(ok, 0);
        assert_eq!(corvid_value_as_bool(t, &mut ok), 0);
        assert_eq!(ok, 0);
        assert_eq!(corvid_value_as_float(t, &mut ok), 0.0);
        assert_eq!(ok, 0);

        // _ref on the wrong type: NULL pointer, length 0, no record.
        let mut len = usize::MAX;
        assert!(corvid_value_bytes_ref(t, &mut len).is_null());
        assert_eq!(len, 0);
        assert!(corvid_value_vector_ref(t, &mut len).is_null());
        assert_eq!(len, 0);
        let v = vector(&[1.0]);
        assert!(corvid_value_text_ref(v, &mut len).is_null());
        assert_eq!(len, 0);

        // _get on the wrong container kind: NULL, no record.
        assert!(corvid_value_array_get(t, 0).is_null());
        assert!(corvid_value_map_get(v, b"k".as_ptr() as *const c_char, 1).is_null());

        // Scalars and containers read inertly too.
        let n = corvid_value_int(3);
        assert_eq!(corvid_value_as_bool(n, &mut ok), 0);
        assert_eq!(ok, 0);
        let arr = corvid_value_array_new();
        push(arr, corvid_value_int(1));
        assert_eq!(corvid_value_as_int(arr, &mut ok), 0);
        assert_eq!(ok, 0);
        assert_eq!(corvid_value_text_ref(arr, &mut len), std::ptr::null());
        assert_eq!(len, 0);

        // Nothing above recorded an error: the slot is still clean.
        assert_eq!(
            last_code(),
            corvid_err::CORVID_E_OK,
            "wrong-type reads are inert, not errors (§4.4)"
        );

        corvid_value_free(t);
        corvid_value_free(v);
        corvid_value_free(n);
        corvid_value_free(arr);
    }

    #[test]
    fn null_discipline_yields_inert_values_and_records() {
        let mut ok: c_int = 5;
        let mut len = usize::MAX;
        let live = text("live");

        // type(NULL): tag 0 (same bits as a real Null — §7's price) +
        // E_ARGUMENT recorded.
        assert_eq!(
            corvid_value_type(std::ptr::null()),
            corvid_value_type::CORVID_TYPE_NULL
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // as_*: NULL v and NULL ok, both inert + recorded.
        assert_eq!(corvid_value_as_bool(std::ptr::null(), &mut ok), 0);
        assert_eq!(ok, 0);
        assert_eq!(corvid_value_as_int(std::ptr::null(), &mut ok), 0);
        assert_eq!(corvid_value_as_float(std::ptr::null(), &mut ok), 0.0);
        assert_eq!(
            corvid_value_as_bool(live, std::ptr::null_mut()),
            0,
            "NULL ok is itself the §7 error"
        );
        assert_eq!(
            corvid_value_as_int(live, std::ptr::null_mut()),
            0,
            "as_int on Text is inert, but NULL ok records"
        );
        assert_eq!(corvid_value_as_float(live, std::ptr::null_mut()), 0.0);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // _ref trio: NULL v and NULL len_out/dim_out.
        assert!(corvid_value_text_ref(std::ptr::null(), &mut len).is_null());
        assert_eq!(len, 0);
        assert!(corvid_value_bytes_ref(std::ptr::null(), &mut len).is_null());
        assert!(corvid_value_vector_ref(std::ptr::null(), &mut len).is_null());
        assert!(corvid_value_text_ref(live, std::ptr::null_mut()).is_null());
        assert!(corvid_value_bytes_ref(live, std::ptr::null_mut()).is_null());
        assert!(corvid_value_vector_ref(live, std::ptr::null_mut()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // _get: NULL parent, NULL key (map_get), and non-UTF-8 key.
        assert!(corvid_value_array_get(std::ptr::null(), 0).is_null());
        assert!(
            corvid_value_map_get(std::ptr::null(), b"k".as_ptr() as *const c_char, 1).is_null()
        );
        let m = corvid_value_map_new();
        assert!(corvid_value_map_get(m, std::ptr::null(), 0).is_null());
        let bad = [0xFF_u8];
        assert!(corvid_value_map_get(m, bad.as_ptr() as *const c_char, 1).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_value_free(m);

        // len(NULL): 0 + recorded.
        assert_eq!(corvid_value_len(std::ptr::null()), 0);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // clone(NULL): NULL + recorded.
        assert!(corvid_value_clone(std::ptr::null()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // free(NULL): a no-op, nothing recorded (nothing fails).
        corvid_value_free(std::ptr::null_mut());

        corvid_value_free(live);
    }

    #[test]
    fn ref_accessors_are_zero_copy_into_the_handle_storage() {
        let t = text("streaming-borrow");
        let mut l1 = 0;
        let mut l2 = 0;
        let p1 = corvid_value_text_ref(t, &mut l1);
        let p2 = corvid_value_text_ref(t, &mut l2);
        assert!(
            std::ptr::eq(p1, p2),
            "repeated refs borrow the same storage"
        );
        assert_eq!(l1, "streaming-borrow".len());
        assert_eq!(l2, l1);

        // A clone owns separate storage: deep copy, different pointer.
        let c = corvid_value_clone(t);
        let p3 = corvid_value_text_ref(c, &mut l2);
        assert!(!std::ptr::eq(p1, p3));
        assert_eq!(l2, l1);

        let v = vector(&[1.0, 2.0]);
        let q1 = corvid_value_vector_ref(v, &mut l1);
        let q2 = corvid_value_vector_ref(v, &mut l2);
        assert!(std::ptr::eq(q1, q2));

        corvid_value_free(t);
        corvid_value_free(c);
        corvid_value_free(v);
    }

    #[test]
    fn clone_is_a_deep_copy_not_an_alias() {
        // Array: clone, then keep building the parent — the clone is
        // untouched (no aliasing through the consumed children).
        let arr = corvid_value_array_new();
        push(arr, corvid_value_int(1));
        let snapshot = corvid_value_clone(arr);
        push(arr, corvid_value_int(2));
        put_array_extra(arr);
        assert_eq!(corvid_value_len(arr), 3, "parent kept growing");
        assert_eq!(
            corvid_value_len(snapshot),
            1,
            "the clone is frozen at copy time"
        );
        let mut ok = 0;
        assert_eq!(
            corvid_value_as_int(corvid_value_array_get(snapshot, 0), &mut ok),
            1
        );

        // Map: same proof.
        let m = corvid_value_map_new();
        put(m, "a", corvid_value_int(1));
        let snap = corvid_value_clone(m);
        put(m, "b", corvid_value_int(2));
        put(m, "a", corvid_value_int(9)); // replace, not grow
        assert_eq!(corvid_value_len(m), 2);
        assert_eq!(corvid_value_len(snap), 1);
        assert_eq!(
            corvid_value_as_int(
                corvid_value_map_get(snap, b"a".as_ptr() as *const c_char, 1),
                &mut ok
            ),
            1,
            "the clone never saw the replace"
        );

        corvid_value_free(snapshot);
        corvid_value_free(snap);
        corvid_value_free(arr);
        corvid_value_free(m);
    }

    /// One more parent-side push for the clone test (kept separate so the
    /// consumed handle of each push is obvious at the call site).
    fn put_array_extra(arr: *mut corvid_value) {
        push(arr, corvid_value_int(3));
    }

    #[test]
    fn borrowed_children_ride_the_parent_lifetime() {
        // Build a parent map holding every shape, take children and refs,
        // churn the allocator with unrelated values, then read through
        // the borrows: their lifetime tracks the PARENT, not the churn.
        let parent = corvid_value_map_new();
        put(parent, "num", corvid_value_int(42));
        put(parent, "name", text("corvid"));
        let tags = corvid_value_array_new();
        push(tags, text("a"));
        push(tags, bytes(&[9, 9]));
        push(tags, vector(&[0.5]));
        put(parent, "tags", tags); // consumed into the parent
        put(parent, "nan", corvid_value_float(f64::NAN));

        let num = corvid_value_map_get(parent, b"num".as_ptr() as *const c_char, 3);
        let name = corvid_value_map_get(parent, b"name".as_ptr() as *const c_char, 4);
        let tags = corvid_value_map_get(parent, b"tags".as_ptr() as *const c_char, 4);
        let nan = corvid_value_map_get(parent, b"nan".as_ptr() as *const c_char, 3);
        assert!(!num.is_null() && !name.is_null() && !tags.is_null() && !nan.is_null());

        // Churn: values unconnected to the parent come and go.
        for i in 0..128i64 {
            let junk = corvid_value_int(i);
            let junk2 = corvid_value_clone(junk);
            corvid_value_free(junk);
            corvid_value_free(junk2);
        }

        // The borrowed children still read correctly.
        let mut ok: c_int = 0;
        let mut len = 0;
        assert_eq!(corvid_value_as_int(num, &mut ok), 42);
        let name_p = corvid_value_text_ref(name, &mut len);
        // SAFETY: name is a borrowed child of the live parent, taken
        // after the churn; len bytes are readable (§1.5 shape).
        assert_eq!(
            unsafe { std::slice::from_raw_parts(name_p as *const u8, len) },
            b"corvid"
        );
        assert_eq!(corvid_value_len(tags), 3);
        // Nested borrows through the child (array-of-everything).
        let second = corvid_value_array_get(tags, 1);
        let bytes_p = corvid_value_bytes_ref(second, &mut len);
        // SAFETY: second borrows the live parent (grandchild).
        assert_eq!(unsafe { std::slice::from_raw_parts(bytes_p, len) }, [9, 9]);
        let third = corvid_value_array_get(tags, 2);
        let mut dim = 0;
        let vec_p = corvid_value_vector_ref(third, &mut dim);
        assert_eq!(dim, 1);
        // SAFETY: third borrows the live parent (grandchild).
        assert_eq!(unsafe { *vec_p }.to_bits(), 0.5f32.to_bits());
        assert_eq!(
            corvid_value_as_float(nan, &mut ok).to_bits(),
            f64::NAN.to_bits()
        );

        // Freeing the parent is the LAST act: the children die with it
        // (using them after is the documented UB, untestable by design).
        corvid_value_free(parent);
    }

    #[test]
    fn mutation_invalidates_borrowed_children_by_contract() {
        // The contract's testable half: after a push the parent reads
        // coherently (len grew, children fetched AFTER the push are
        // valid). The other half — that children fetched BEFORE are
        // invalid — is the UB boundary (Vec realloc); it is deliberately
        // NOT exercised: reading a stale child is UB, not an assertable
        // outcome. Which half of the contract a given push falls on is
        // allocator-dependent, exactly why the spec words it as it does.
        let arr = corvid_value_array_new();
        push(arr, corvid_value_int(1));
        assert_eq!(corvid_value_len(arr), 1);
        push(arr, corvid_value_int(2));
        push(arr, corvid_value_int(3));
        assert_eq!(corvid_value_len(arr), 3);
        let mut ok = 0;
        for (i, want) in [(0usize, 1i64), (1, 2), (2, 3)] {
            assert_eq!(
                corvid_value_as_int(corvid_value_array_get(arr, i), &mut ok),
                want,
                "post-mutation children are fresh borrows"
            );
        }
        corvid_value_free(arr);

        // The map side: a replaced child's borrows die with it; the new
        // child under the same key is a fresh, valid borrow.
        let m = corvid_value_map_new();
        put(m, "k", text("first"));
        put(m, "k", text("second")); // replaces: old child dropped
        let mut len = 0;
        let p = corvid_value_text_ref(
            corvid_value_map_get(m, b"k".as_ptr() as *const c_char, 1),
            &mut len,
        );
        // SAFETY: fresh child borrow of the live parent.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(p as *const u8, len) },
            b"second"
        );
        assert_eq!(corvid_value_len(m), 1);
        corvid_value_free(m);
    }

    #[test]
    fn len_reports_each_shape() {
        assert_eq!(corvid_value_len(corvid_value_null()), 0);
        assert_eq!(corvid_value_len(corvid_value_bool(1)), 0);
        assert_eq!(corvid_value_len(corvid_value_int(i64::MAX)), 0);
        assert_eq!(corvid_value_len(corvid_value_float(f64::NAN)), 0);
        // h(1) é(2) l l o (3) space(1) 🐦(4) = 11 bytes, 8 chars.
        assert_eq!(corvid_value_len(text("héllo 🐦")), 11, "bytes, not chars");
        assert_eq!(corvid_value_len(bytes(&[0, 1, 2, 255])), 4);
        assert_eq!(corvid_value_len(vector(&[0.0, -1.5, 3.25])), 3);
        let arr = corvid_value_array_new();
        push(arr, corvid_value_null());
        push(arr, corvid_value_null());
        assert_eq!(corvid_value_len(arr), 2);
        let m = corvid_value_map_new();
        put(m, "a", corvid_value_null());
        put(m, "b", corvid_value_null());
        put(m, "a", corvid_value_null()); // replace, not grow
        assert_eq!(corvid_value_len(m), 2);
        corvid_value_free(arr);
        corvid_value_free(m);
    }

    #[test]
    fn owned_values_free_their_trees() {
        // Own → free across every shape (the destructor side of the
        // owned-vs-borrowed split; the borrowed-child side is the bold
        // UB of the module docs and of corvid_value_free's doc, not an
        // executable path). Deep trees exercise recursive drop of
        // consumed children.
        let deep = corvid_value_map_new();
        let arr = corvid_value_array_new();
        push(arr, text("x"));
        let inner_map = corvid_value_map_new();
        put(inner_map, "deep", vector(&[f32::NAN, -0.0]));
        push(arr, inner_map);
        put(deep, "arr", arr);
        put(deep, "self", corvid_value_clone(deep)); // a copy, not a cycle
        corvid_value_free(deep);

        for v in [
            corvid_value_null(),
            corvid_value_bool(0),
            corvid_value_int(0),
            corvid_value_float(0.0),
            text(""),
            bytes(&[]),
            vector(&[]),
            corvid_value_array_new(),
            corvid_value_map_new(),
        ] {
            corvid_value_free(v);
        }
    }
}
