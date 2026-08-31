//! corvid-ffi — the typed C ABI over the corvid engine.
//!
//! The contract is `docs/FFI.md` (LOCKED, Phase-0 Task 1): 122 `corvid_`
//! prefixed `extern "C"` symbols across 13 families, opaque handles, the
//! 19-code error enum, and the ownership rules of its §4/§5. This crate
//! implements that contract family by family; [`FFI_VERSION`] is 1 and
//! bindings verify it via `corvid_ffi_version()` before anything else.
//!
//! Layout: one module per spec §4 family. Landed so far (the rest are
//! stubs that name their landing task):
//!
//! | module | spec §4 family | status |
//! |---|---|---|
//! | [`error`] | §1.3/§3 status + error codes, thread-local last error | landed |
//! | [`handle`] | §1.1/§2/§4.13 opaque handles, derived-handle counter | landed |
//! | [`lifecycle`] | §4.1 lifecycle & errors (8 fns) | landed |
//! | [`strs`] | §4.12 string-cursor plumbing (`strs_next`/`strs_free`) | landed |
//! | [`value`] | §4.3/§4.4 value construction & reads (23 fns) | landed |
//! | [`pred`] | §4.5 predicates | Task 4 |
//! | [`mutation`] | §4.8 mutations | Task 4 |
//! | [`read`] | §4.9 reads | Task 4 |
//! | [`query`] | §4.6/§4.7 query builder, rows, aggregations | Task 5 |
//! | [`index`] | §4.10 indexes & schema | Task 6 |
//! | [`graph`] | §4.11 graph | Task 6 |
//! | [`geo`] | §4.12 geo queries & geohits cursor | Task 6 |
//! | [`admin`] | §4.13 dump/load/backup/compact | Task 6 |
//!
//! Safety regime: the engine is `#![forbid(unsafe_code)]`; this crate IS
//! the unsafe boundary. `#![deny(unsafe_op_in_unsafe_fn)]` is on at the
//! crate root, every `unsafe` block carries a `SAFETY:` comment, and the
//! NULL discipline of spec §7 is enforced before any dereference — an
//! unexpected NULL records `CORVID_E_ARGUMENT` and returns the inert
//! value for the signature's shape, never UB.
//!
//! Error model (spec §3): every failure signal — a `CORVID_ERR` status or
//! a NULL where a handle/buffer was expected — records a thread-local
//! code + message as its first act. Successful calls never clear it; read
//! it immediately after the failure that interests you.
//!
//! Testing: everything is a unit test in `src/` (`#[cfg(test)] mod tests`
//! per module, plus the header drift gate in `src/header.rs`). An
//! integration `tests/` directory is structurally impossible here: the lib
//! target is named `corvid` (so the cdylib artifacts are `libcorvid.*`),
//! which would make `corvid` resolve ambiguously against the engine
//! dependency in an integration test's extern prelude.

#![deny(unsafe_op_in_unsafe_fn)]
// The ABI shims are safe-marker `extern "C"` fns that dereference caller
// pointers inside `unsafe` blocks, each with a `SAFETY:` comment: pointer
// validity beyond the NULL discipline (spec §7) is the C caller's
// documented contract — borrowed bytes valid for their length (§1.5),
// handle provenance per family (§2) — none of which Rust's signature can
// express. Marking the shims `unsafe` would push that burden to BINDINGS
// (wrongly: the contract is exactly what §7 checks), so the lint's
// suggestion is declined crate-wide, deliberately.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

/// Admin family (spec §4.13): `dump_to_path`, `load_from_path`,
/// `load_from_path_with_renames`, `backup`, `compact`. Lands with Task 6.
pub mod admin;
/// Status and error codes (spec §1.3), the engine-variant mapping, and the
/// thread-local last-error slot (spec §3).
pub mod error;
/// Geo queries and the geohits cursor (spec §4.12). Lands with Task 6.
pub mod geo;
/// Graph family (spec §4.11). Lands with Task 4 (link/unlink) and Task 6.
pub mod graph;
/// Opaque-handle infrastructure (spec §1.1/§2) and the FFI-owned
/// derived-handle counter (spec §4.13/§6).
pub mod handle;
/// Indexes & schema (spec §4.10). Lands with Task 6.
pub mod index;
/// Lifecycle & errors (spec §4.1): `ffi_version`, `open`, `open_memory`,
/// `close`, `last_error_code`, `last_error_message`, `free`,
/// `collections`.
pub mod lifecycle;
/// Mutations (spec §4.8). Lands with Task 4.
pub mod mutation;
/// Predicates (spec §4.5). Lands with Task 4.
pub mod pred;
/// Queries, rows cursor, aggregations (spec §4.6/§4.7). Task 5.
pub mod query;
/// Reads (spec §4.9). Lands with Task 4.
pub mod read;
/// The string-cursor family plumbing (spec §4.12): `strs_next`,
/// `strs_free` — shared by `corvid_collections` (landed) and the graph
/// cursors (Task 6).
pub mod strs;
/// Value construction & reads (spec §4.3/§4.4): the 23 value functions
/// — constructors that CLONE their buffers, `_ref` zero-copy borrows,
/// `array_get`/`map_get` borrowed children, and `corvid_value_free` for
/// OWNED values only.
pub mod value;

/// The header drift gate (test-only): regenerates `corvid.h` from this
/// crate via cbindgen and asserts byte-equality with the committed copy —
/// spec, header, and radar cannot disagree silently (spec §8).
#[cfg(test)]
mod header;

/// ABI version of the `corvid` cdylib (spec §0/§8). `1` — a breaking
/// change bumps it (and the soname) loudly, per the pre-1.0 break policy.
pub const FFI_VERSION: u32 = 1;
