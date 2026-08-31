//! corvid-ffi — the typed C ABI over the corvid engine.
//!
//! The contract is `docs/FFI.md` (LOCKED, Phase-0 Task 1): 122 `corvid_`
//! prefixed `extern "C"` symbols across 13 families, opaque handles, the
//! 19-code error enum, and the ownership rules of its §4/§5. This crate
//! implements that contract family by family; [`FFI_VERSION`] is 1 and
//! bindings verify it via `corvid_ffi_version()` before anything else.
//!
//! Layout: one module per spec §4 family, declared in spec order (the
//! header's emission order follows, so `corvid.h` reads like the spec's
//! function reference). Landed (the full 122-symbol surface — spec
//! Appendix A):
//!
//! | module | spec §4 family | status |
//! |---|---|---|
//! | [`error`] | §1.3/§3 status + error codes, thread-local last error | landed |
//! | [`handle`] | §1.1/§2/§4.13 opaque handles, derived-handle counter, compact gate | landed |
//! | [`lifecycle`] | §4.1 lifecycle & errors (8 fns) | landed |
//! | [`collection`] | §4.2 collection handles (3 fns) | landed |
//! | [`value`] | §4.3/§4.4 value construction & reads (23 fns) | landed |
//! | [`pred`] | §4.5 predicates (11 fns) | landed |
//! | [`query`] | §4.6/§4.7 query builder, rows cursor, aggregations (26 fns) | landed |
//! | [`mutation`] | §4.8 mutations (13 fns) | landed |
//! | [`read`] | §4.9 reads (4 fns) | landed |
//! | [`strs`] | §4.12 string-cursor plumbing (`strs_next`/`strs_free`) | landed |
//! | [`index`] | §4.10 indexes & schema (15 fns) | landed |
//! | [`graph`] | §4.11 graph (7 fns) | landed |
//! | [`geo`] | §4.12 geo queries & geohits cursor (5 fns) | landed |
//! | [`admin`] | §4.13 dump/load/backup/compact (5 fns) | landed |
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
//! per module, plus the header drift gate in `src/header.rs`, the
//! C-surface radar in `src/radar.rs`, and the C smoke-suite driver in
//! `src/smoke.rs` — which compiles `c/smoke.c` against the just-built
//! cdylib at TEST time and runs it over the committed golden fixtures,
//! so `cargo test --workspace` enforces the C surface end to end). An
//! integration `tests/` directory is structurally impossible here: the
//! lib target is named `corvid` (so the cdylib artifacts are
//! `libcorvid.*`), which would make `corvid` resolve ambiguously against
//! the engine dependency in an integration test's extern prelude.

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
/// `load_from_path_with_renames`, `backup`, and the exclusivity-gated
/// `compact` (the FFI-only `CORVID_E_BUSY` call site).
pub mod admin;
/// Collection handles (spec §4.2): `collection`, `collection_free`,
/// `collection_name` — the derived engine handle that keeps the `Db`
/// alive past `corvid_close` and drives the derived-handle counter.
pub mod collection;
/// Status and error codes (spec §1.3), the engine-variant mapping, and the
/// thread-local last-error slot (spec §3).
pub mod error;
/// Geo queries and the geohits cursor (spec §4.12):
/// `geo_within_radius` / `geo_within_bbox` / `geo_nearest` plus
/// `geohits_next` / `geohits_free` — the cursor shape shared with
/// `corvid_neighbors_weighted`.
pub mod geo;
/// Graph family (spec §4.11): the 7 edge/neighborhood functions —
/// `link`, `link_weighted`, `unlink`, `neighbors`, `in_neighbors`,
/// `neighbors_weighted` (the geohits reuse), `traverse`.
pub mod graph;
/// Opaque-handle infrastructure (spec §1.1/§2) and the FFI-owned
/// derived-handle counter (spec §4.13/§6).
pub mod handle;
/// Indexes & schema (spec §4.10): the 11 `corvid_create_*_index`
/// variants (scalar, compound, text x2, geo, and the six HNSW forms
/// incl. quantized and product-quantized) plus `set_schema` /
/// `schema` / `schemaiter_next` / `schemaiter_free`.
pub mod index;
/// Lifecycle & errors (spec §4.1): `ffi_version`, `open`, `open_memory`,
/// `close`, `last_error_code`, `last_error_message`, `free`,
/// `collections`.
pub mod lifecycle;
/// Mutations (spec §4.8): the 13 write functions — insert, put_many,
/// insert_auto, update (C callback), patch, compare_and_set, delete,
/// delete_where, delete_batch, and the TTL trio.
pub mod mutation;
/// Predicates (spec §4.5): the 11 `corvid_pred_*` functions — ten
/// constructors over dotted field paths (values CLONED into the tree)
/// and `pred_free` for never-consumed roots.
pub mod pred;
/// Queries, rows cursor, aggregations (spec §4.6/§4.7): the 15
/// query-family functions — the builder setters (filter consuming a
/// predicate, vector/text sources, RRF/MMR knobs, limit/offset/
/// order_by/select), `run` CONSUMING the query into a rows cursor,
/// `rows_next`'s borrowed key/doc walk — plus the 11 aggregations
/// (all consuming; the `groupiter` cursor).
pub mod query;
/// Reads (spec §4.9): `get` (owned-out), `scan` (callback), `page`
/// (keyset pagination + the `next_after` buffer), `len`.
pub mod read;
/// The string-cursor family plumbing (spec §4.12): `strs_next`,
/// `strs_free` — shared by `corvid_collections` and the graph-family
/// cursors (`neighbors` / `in_neighbors` / `traverse`).
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

/// The C-surface radar (test-only, Task 7): no untested exports — the
/// generated header's symbol set equals spec Appendix A (parsed from
/// `docs/FFI.md` at test time), and the C smoke suite (`c/smoke.c`)
/// calls every one of the 122 symbols.
#[cfg(test)]
mod radar;

/// The C smoke suite driver (test-only, Task 7): compiles `c/smoke.c`
/// against the just-built cdylib with the `cc` crate at TEST time, runs
/// it over the committed golden fixtures (`golden/`), and asserts every
/// fixture line executed — the golden-coverage half of the radar.
#[cfg(test)]
mod smoke;

/// ABI version of the `corvid` cdylib (spec §0/§8). `1` — a breaking
/// change bumps it (and the soname) loudly, per the pre-1.0 break policy.
pub const FFI_VERSION: u32 = 1;
