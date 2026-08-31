//! Query builder, rows cursor, aggregations (spec §4.6/§4.7) — the 26
//! functions of the ABI's retrieval families: 15 query/rows (§4.6) and
//! 11 aggregations (§4.7).
//!
//! A `corvid_query*` holds owned QueryBuilder state (spec §2's backing
//! table: engine `Arc` + name + filters + sources + knobs) — see the
//! `QueryHandle` docs in `crate::handle` for why the parts, not a live
//! borrowed `QueryBuilder<'c>`, are what lives behind the opaque
//! pointer, and how execution materializes the real engine builder
//! exactly once. `corvid_query_run` and every aggregate CONSUME the
//! handle (spec §5 rule 5, mirroring the engine's by-value `self`) —
//! unconditionally, per spec §8: a failed run has still consumed it;
//! `corvid_query_free` is for builders abandoned without executing, and
//! after either path the pointer MUST NOT be touched again (double
//! free / use-after-free = UB, the documented discipline this family
//! shares with the predicate combinators).
//!
//! # Validation timing (spec §4.6, engine audit C6)
//!
//! The engine's fluent builder stores caller arguments as given and
//! validates the ranking parameters at execution: non-finite or
//! non-positive `fuse_rrf` k, or a `rerank_mmr` lambda outside `[0,1]`
//! (NaN included), fail `run`/every aggregate with
//! `CORVID_E_ARGUMENT`. The ABI mirrors that exactly: the setters
//! always succeed (`CORVID_OK` for any float), and the error surfaces
//! at the executing call. The ABI's own §7 discipline (NULL/misencoded
//! pointers) is checked at the setter as usual.
//!
//! # The rows cursor (spec §4.6)
//!
//! `corvid_query_run` materializes the engine's `Vec<ResultRow>` into a
//! `corvid_rows*` — the same handle `corvid_page` produces (score 0.0
//! there), one walker for both. `corvid_rows_next` hands out each
//! row's key and document as **BORROWED views into the cursor** — the
//! same interior-pointer shape as the value family's borrowed children
//! (value.rs module docs): valid only until the next
//! `corvid_rows_next` or `corvid_rows_free`; using or freeing them
//! after either is UB. `score` is the fused RRF score by value, `0.0`
//! for pure filter/order queries and page rows.

use std::ffi::c_char;
use std::ffi::c_int;

use corvid::Metric;

use crate::error::corvid_status;
use crate::error::record_argument;
use crate::handle::GroupIterHandle;
use crate::handle::RowsHandle;
use crate::handle::borrow_coll;
use crate::handle::borrow_groupiter_mut;
use crate::handle::borrow_query_mut;
use crate::handle::borrow_rows_mut;
use crate::handle::corvid_coll;
use crate::handle::corvid_groupiter;
use crate::handle::corvid_query;
use crate::handle::corvid_rows;
use crate::handle::corvid_value;
use crate::handle::into_groupiter;
use crate::handle::into_query;
use crate::handle::into_rows;
use crate::handle::into_value;
use crate::handle::reclaim_groupiter;
use crate::handle::reclaim_query;
use crate::handle::reclaim_rows;
use crate::value::borrowed_utf8;

/// The distance metric (FFI.md §1.4, frozen per §8): mirrors
/// `corvid::Metric` (distance.rs).
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_metric {
    /// Cosine distance `1 - cos_sim` in `[0,2]`; zero-norm = maximally
    /// distant.
    CORVID_METRIC_COSINE = 0,
    /// Negated dot product (larger dot sorts first).
    CORVID_METRIC_DOT = 1,
    /// Squared Euclidean (monotonic with L2).
    CORVID_METRIC_L2 = 2,
}

/// Map an ABI metric onto the engine metric, or `None` (having recorded
/// `CORVID_E_ARGUMENT` under the calling function's name) when it is
/// outside `COSINE..=L2` — the enum is frozen (§8), so an out-of-domain
/// value is a caller bug, not a future opcode. Validating the raw
/// discriminant (not the enum) keeps an out-of-domain integer from C a
/// checked error instead of an unspecified-match footgun, exactly like
/// `corvid_cmp` (pred.rs). Shared by `corvid_query_vector` and the six
/// `corvid_create_vector_index*` creates (index.rs) — hence the
/// parameterized context (the Task 5 review prepend: the rejection must
/// name the function that rejected).
pub(crate) fn metric_of(context: &str, m: u32) -> Option<Metric> {
    match m {
        0 => Some(Metric::Cosine),
        1 => Some(Metric::Dot),
        2 => Some(Metric::L2),
        _ => {
            record_argument(&format!(
                "{context}: metric is outside \
                 CORVID_METRIC_COSINE..=CORVID_METRIC_L2"
            ));
            None
        }
    }
}

/// The §7 NULL-checked exclusive query borrow shared by every setter
/// in this module (read.rs owns the coll-shaped twin).
fn borrow_query_checked<'a>(
    fn_name: &str,
    q: *mut corvid_query,
) -> Option<&'a mut crate::handle::QueryHandle> {
    if q.is_null() {
        record_argument(format!("{fn_name}: q is NULL").as_str());
        return None;
    }
    // SAFETY: q is non-NULL (checked) and contractually a live
    // corvid_query_new product not yet consumed; the query family is
    // single-threaded by contract (spec §2), so the exclusive borrow is
    // sound.
    unsafe { borrow_query_mut(q) }
}

/// Begin a query over `coll` (spec §4.6; counterpart:
/// `Collection::query() -> QueryBuilder`). Returns NULL only on NULL
/// `coll` (with `CORVID_E_ARGUMENT` recorded). The handle holds an
/// engine reference (it keeps the db alive after `corvid_close`,
/// spec §2) and increments the db's derived-handle counter (spec
/// §4.13) — released by `corvid_query_run`/any aggregate (which
/// consume the handle) or by `corvid_query_free`, exactly one of the
/// two.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_new(coll: *mut corvid_coll) -> *mut corvid_query {
    // SAFETY: NULL maps to None (spec §7); a non-NULL handle has
    // corvid_collection provenance and the coll family is thread-safe
    // (spec §2), so a shared borrow is fine.
    let Some(handle) = (unsafe { borrow_coll(coll) }) else {
        record_argument("corvid_query_new: coll is NULL");
        return std::ptr::null_mut();
    };
    // The count and the handle are born together (spec §4.13), as in
    // corvid_collection — spawn_query increments before constructing.
    into_query(handle.spawn_query())
}

/// Add a filter — **CONSUMES `pred`** (spec §4.6; counterpart:
/// `QueryBuilder::filter(predicate)`, by value). Multiple calls AND
/// together. `pred` is consumed unconditionally when non-NULL (spec
/// §8): a failed call (NULL `q`) has still taken it — free nothing
/// afterwards. NULL `pred` fails with `CORVID_E_ARGUMENT` and consumes
/// nothing.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_filter(
    q: *mut corvid_query,
    pred: *mut crate::handle::corvid_pred,
) -> corvid_status {
    if pred.is_null() {
        record_argument("corvid_query_filter: pred is NULL");
        return corvid_status::CORVID_ERR;
    }
    // Consume FIRST (spec §8's unconditional-consumption discipline —
    // the delete_where precedent): every failure path below has already
    // taken the predicate.
    // SAFETY: pred is non-NULL (checked) and contractually an unconsumed
    // into_pred product; this call is its single consumption.
    let pred = *unsafe { crate::handle::reclaim_pred(pred) }.expect("non-NULL checked above");
    let Some(query) = borrow_query_checked("corvid_query_filter", q) else {
        return corvid_status::CORVID_ERR;
    };
    query.push_filter(pred);
    corvid_status::CORVID_OK
}

/// Add a vector-search source (spec §4.6; counterpart:
/// `QueryBuilder::vector(field, query, k, metric)`). The query vector
/// is CLONED — the caller keeps its buffer. `field` is borrowed UTF-8,
/// non-NULL at any length; `query` is non-NULL at any `dim` (dim 0
/// legal, spec §1.5); `k` is any `size_t` (the engine truncates each
/// source's ranking to `k`). A `metric` outside
/// `CORVID_METRIC_COSINE..=L2`, a NULL pointer, or invalid UTF-8 fails
/// with `CORVID_E_ARGUMENT` and leaves the query untouched.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_vector(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    query: *const f32,
    dim: usize,
    k: usize,
    metric: corvid_metric,
) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_vector", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_vector", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    if query.is_null() {
        record_argument(
            "corvid_query_vector: query is NULL (empty is a non-NULL \
             pointer with dim 0, spec §1.5)",
        );
        return corvid_status::CORVID_ERR;
    }
    let Some(metric) = metric_of("corvid_query_vector", metric as u32) else {
        return corvid_status::CORVID_ERR;
    };
    // SAFETY: query is non-NULL (checked) and the caller guarantees it
    // is readable for dim f32s (spec §1.5's borrowed-buffer contract).
    let query_vec = unsafe { std::slice::from_raw_parts(query, dim) }.to_vec();
    handle.push_vector(field.to_owned(), query_vec, k, metric);
    corvid_status::CORVID_OK
}

/// Add a BM25 text-search source (spec §4.6; counterpart:
/// `QueryBuilder::text(field, query, k)`). `s` is CLONED into the
/// source; both strings are borrowed UTF-8, non-NULL at any length.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_text(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    s: *const c_char,
    s_len: usize,
    k: usize,
) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_text", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_text", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(text) = borrowed_utf8("corvid_query_text", "s", s, s_len) else {
        return corvid_status::CORVID_ERR;
    };
    handle.push_text(field.to_owned(), text.to_owned(), k);
    corvid_status::CORVID_OK
}

/// Set the Reciprocal Rank Fusion constant (spec §4.6; counterpart:
/// `QueryBuilder::fuse_rrf(k)`; engine default `corvid::DEFAULT_RRF_K`
/// = 60). **This setter always succeeds** — the engine validates at
/// execution (audit C6): a non-finite or non-positive `k` fails
/// `corvid_query_run`/aggregates with `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_fuse_rrf(q: *mut corvid_query, k: f32) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_fuse_rrf", q) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_rrf_k(k);
    corvid_status::CORVID_OK
}

/// Diversify results with Maximal Marginal Relevance (spec §4.6;
/// counterpart: `QueryBuilder::rerank_mmr(lambda)`). **This setter
/// always succeeds** — `lambda` outside `[0,1]` (NaN included) fails
/// `corvid_query_run`/aggregates with `CORVID_E_ARGUMENT` at execution
/// (audit C6). The rerank anchors on the first vector source; without
/// one it is a no-op (engine documented).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_rerank_mmr(q: *mut corvid_query, lambda: f32) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_rerank_mmr", q) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_mmr_lambda(lambda);
    corvid_status::CORVID_OK
}

/// Allow approximate execution (spec §4.6; counterpart:
/// `QueryBuilder::approx`): a filtered single-vector-source query may
/// use its ANN index with over-fetch-then-filter. A knob, not data —
/// it cannot fail beyond the NULL discipline.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_approx(q: *mut corvid_query) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_approx", q) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_approx();
    corvid_status::CORVID_OK
}

/// Cap the result at `n` rows (spec §4.6; counterpart:
/// `QueryBuilder::limit`). `limit 0` yields an empty result (the
/// engine truncates to zero), applied after `offset`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_limit(q: *mut corvid_query, n: usize) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_limit", q) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_limit(n);
    corvid_status::CORVID_OK
}

/// Skip the first `n` rows (spec §4.6; counterpart:
/// `QueryBuilder::offset`) — applied after ordering, before `limit`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_offset(q: *mut corvid_query, n: usize) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_offset", q) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_offset(n);
    corvid_status::CORVID_OK
}

/// Order results by a scalar field instead of by rank (spec §4.6;
/// counterpart: `QueryBuilder::order_by(field, descending)`).
/// `descending` is any non-zero `int`. The engine's ordering contract
/// (audit C4): comparable values (numbers numerically — numbers before
/// texts across kinds — texts lexically) first in value order;
/// incomparable values (bools, containers, NaN) after them; rows
/// missing the field last; ties by key; `descending` reverses
/// within-class order only.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_order_by(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    descending: c_int,
) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_order_by", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_order_by", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    handle.set_order_by(field.to_owned(), descending != 0);
    corvid_status::CORVID_OK
}

/// Project result documents to these top-level fields (spec §4.6;
/// counterpart: `QueryBuilder::select(fields)`): missing fields are
/// absent, non-map documents pass through unchanged, and ranking still
/// sees the full document. `fields`/`field_lens` are parallel borrowed
/// arrays, non-NULL when `count > 0` (`count == 0` — arrays may be
/// NULL — is the engine-faithful empty projection: map documents
/// project to an empty map, exactly `select(vec![])` in Rust). A NULL
/// array (or array element) with `count > 0`, or a non-UTF-8 field,
/// fails with `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_select(
    q: *mut corvid_query,
    fields: *const *const c_char,
    field_lens: *const usize,
    count: usize,
) -> corvid_status {
    let Some(handle) = borrow_query_checked("corvid_query_select", q) else {
        return corvid_status::CORVID_ERR;
    };
    let mut names = Vec::with_capacity(count);
    if count > 0 {
        if fields.is_null() || field_lens.is_null() {
            record_argument("corvid_query_select: fields/field_lens is NULL with count > 0");
            return corvid_status::CORVID_ERR;
        }
        for i in 0..count {
            // SAFETY: fields is non-NULL (checked) and the caller
            // guarantees count readable pointers (spec §1.5's
            // array-input contract).
            let field = unsafe { *fields.add(i) };
            // SAFETY: field_lens is non-NULL (checked) and readable for
            // count usizes, parallel to fields.
            let len = unsafe { *field_lens.add(i) };
            match borrowed_utf8("corvid_query_select", "fields[i]", field, len) {
                Some(name) => names.push(name.to_owned()),
                None => return corvid_status::CORVID_ERR,
            }
        }
    }
    handle.set_projection(names);
    corvid_status::CORVID_OK
}

/// Execute — **CONSUMES `q`** (spec §4.6; counterpart:
/// `QueryBuilder::run(self)`). Returns a rows cursor even for an empty
/// result; NULL + error on failure (distinguish failure by the NULL,
/// never by an empty cursor). One MVCC snapshot covers the whole query;
/// the ranking parameters are validated HERE (audit C6 — a bad
/// `fuse_rrf`/`rerank_mmr` value fails with `CORVID_E_ARGUMENT` after
/// having consumed the query, per spec §8). The handle's derived count
/// is released by this consumption whichever way it goes.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_run(q: *mut corvid_query) -> *mut corvid_rows {
    // Consume FIRST (spec §8): every failure path below has already
    // taken the query — the caller frees nothing afterwards.
    let Some(body) = consume_query("corvid_query_run", q) else {
        return std::ptr::null_mut();
    };
    match body.execute("corvid_query_run", |b| b.run()) {
        Some(rows) => into_rows(RowsHandle::new(rows)),
        None => std::ptr::null_mut(),
    }
}

/// Free a builder abandoned without executing (spec §4.6). **NOT** for
/// use after `corvid_query_run`/aggregates — they consumed the handle,
/// and this free would be the documented double-free UB (spec §8). No
/// engine counterpart (Rust drops the builder). Releases the handle's
/// derived count (spec §4.13). `corvid_query_free(NULL)` is a no-op
/// (spec §7).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_free(q: *mut corvid_query) {
    // SAFETY: NULL is the documented no-op; otherwise q is
    // contractually a never-executed corvid_query_new product,
    // reclaimed exactly once here.
    if let Some(body) = unsafe { reclaim_query(q) } {
        body.release_derived();
    }
}

/// Advance the rows cursor (spec §4.6): returns 1 and fills the
/// out-params for the next row, 0 at exhaustion — out-params untouched
/// at 0; never errors (the result is materialized). The key and the
/// document are **BORROWED from the cursor: valid only until the next
/// `corvid_rows_next` or `corvid_rows_free` — using or freeing them
/// after is UB** (the value family's borrowed-child rule, value.rs
/// module docs; `corvid_value_clone` is the sanctioned escape).
/// `score` is the fused RRF score (`f32`), `0.0` for pure filter/order
/// queries and `corvid_page` rows. NULL handle or NULL out-parameter
/// follows the non-status rule (spec §7): return 0 with
/// `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_rows_next(
    rows: *mut corvid_rows,
    key_out: *mut *const u8,
    key_len_out: *mut usize,
    doc_out: *mut *const corvid_value,
    score_out: *mut f32,
) -> c_int {
    if rows.is_null()
        || key_out.is_null()
        || key_len_out.is_null()
        || doc_out.is_null()
        || score_out.is_null()
    {
        record_argument("corvid_rows_next: NULL handle or out-param (§7 inert rule)");
        return 0;
    }
    // SAFETY: rows is non-NULL (checked) with corvid_query_run /
    // corvid_page provenance, not yet freed; the §2 contract confines a
    // cursor to one thread, so the exclusive borrow is sound.
    let cursor = unsafe { borrow_rows_mut(rows) }.expect("non-NULL checked above");
    match cursor.next() {
        Some(row) => {
            // SAFETY: all out-params are non-NULL (checked); the key
            // pointer and the doc view borrow the cursor's current row
            // (valid until the next call or free, per the contract
            // above), and the (pointer, length) pair is §1.5's
            // binary-safe shape.
            unsafe {
                *key_out = row.key.as_ptr();
                *key_len_out = row.key.len();
                *doc_out = &row.document as *const corvid::Value as *const corvid_value;
                *score_out = row.score;
            }
            1
        }
        None => 0,
    }
}

/// Free the rows cursor (spec §4.6; counterpart: dropping the
/// `Vec<ResultRow>`). `corvid_rows_free(NULL)` is a no-op (spec §7);
/// the last row's borrowed key/doc die with it.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_rows_free(rows: *mut corvid_rows) {
    // SAFETY: NULL is the documented no-op; otherwise rows is a
    // corvid_query_run / corvid_page product, reclaimed exactly once
    // here.
    drop(unsafe { reclaim_rows(rows) });
}

// --- §4.7 aggregations -------------------------------------------------------
//
// Every aggregate CONSUMES the query (engine methods take `self`) and
// executes on one read snapshot over the FILTERED set — retrieval
// sources, ranking, limit/offset/select are ignored (spec §4.7). The
// ranking knobs are still VALIDATED (audit C6): a bad fuse_rrf/mmr
// value fails with CORVID_E_ARGUMENT, having consumed the query.

/// Count the matching documents (spec §4.7; counterpart:
/// `QueryBuilder::count() -> usize`; O(1) when unfiltered via the
/// engine's maintained counter). `out` is nullable (§7's optional
/// out-params: the call still executes and writes nothing).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_count(q: *mut corvid_query, out: *mut usize) -> corvid_status {
    // Consume FIRST (spec §8) — every aggregate's preamble.
    let Some(body) = consume_query("corvid_query_count", q) else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_count", |b| b.count()) {
        Some(count) => {
            if !out.is_null() {
                // SAFETY: out is non-NULL (checked).
                unsafe { *out = count };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Distinct values at `field` (spec §4.7; counterpart:
/// `QueryBuilder::count_distinct(field)`) — by the canonical group key
/// (text bare; int/float/bool type-tagged so distinct kinds stay
/// distinct; missing and container values ignored). `field` is
/// borrowed UTF-8, non-NULL at any length; `out` is nullable (§7).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_count_distinct(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    out: *mut usize,
) -> corvid_status {
    let Some(body) = consume_query("corvid_query_count_distinct", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_count_distinct", "field", field, field_len)
    else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_count_distinct", |b| b.count_distinct(field)) {
        Some(count) => {
            if !out.is_null() {
                // SAFETY: out is non-NULL (checked).
                unsafe { *out = count };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Sum the numeric (`int`/`float`) values at `field` (spec §4.7;
/// counterpart: `QueryBuilder::sum(field) -> f64`); missing or
/// non-numeric values are skipped (an all-skipped field sums to `0.0`).
/// `out` is nullable (§7).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_sum(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    out: *mut f64,
) -> corvid_status {
    let Some(body) = consume_query("corvid_query_sum", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_sum", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_sum", |b| b.sum(field)) {
        Some(total) => {
            if !out.is_null() {
                // SAFETY: out is non-NULL (checked).
                unsafe { *out = total };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// Mean of the numeric values at `field` (spec §4.7; counterpart:
/// `QueryBuilder::avg(field) -> Option<f64>`). Absence is a success:
/// when no numeric value exists, `*has_value = 0` and `*out` (if
/// non-NULL) is set to `0.0` for a defined shape; otherwise
/// `*has_value = 1` and `*out` carries the mean. Both out-params are
/// nullable (§7).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_avg(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    out: *mut f64,
    has_value: *mut c_int,
) -> corvid_status {
    let Some(body) = consume_query("corvid_query_avg", q) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(field) = borrowed_utf8("corvid_query_avg", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_avg", |b| b.avg(field)) {
        Some(mean) => {
            if !has_value.is_null() {
                // SAFETY: has_value is non-NULL (checked).
                unsafe { *has_value = c_int::from(mean.is_some()) };
            }
            if !out.is_null() {
                // SAFETY: out is non-NULL (checked).
                unsafe { *out = mean.unwrap_or(0.0) };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// The minimum comparable (numeric or text) value at `field` (spec
/// §4.7; counterpart: `QueryBuilder::min(field) -> Option<Value>`), as
/// an OWNED value handle in `*out` — free it with
/// `corvid_value_free`. Absence is a success: `CORVID_OK` + `*out ==
/// NULL` when the filtered set holds no comparable value (the §3
/// optional-value convention). `out` is REQUIRED (spec §4.7: "out
/// non-NULL"); a NULL `field` is the usual `CORVID_E_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_min(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    out: *mut *mut corvid_value,
) -> corvid_status {
    let Some(body) = consume_query("corvid_query_min", q) else {
        return corvid_status::CORVID_ERR;
    };
    if out.is_null() {
        record_argument("corvid_query_min: out is NULL (required — spec §4.7)");
        return corvid_status::CORVID_ERR; // q consumed either way (§8)
    }
    // SAFETY: out is non-NULL (checked); define the absent/failed shape
    // up front so no path leaves it dangling.
    unsafe { *out = std::ptr::null_mut() };
    let Some(field) = borrowed_utf8("corvid_query_min", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_min", |b| b.min(field)) {
        Some(Some(value)) => {
            // SAFETY: out is non-NULL (checked at the top).
            unsafe { *out = into_value(value) };
            corvid_status::CORVID_OK
        }
        Some(None) => corvid_status::CORVID_OK, // absence is a success
        None => corvid_status::CORVID_ERR,
    }
}

/// The maximum comparable value at `field` — [`corvid_query_min`]'s
/// twin (spec §4.7; counterpart: `QueryBuilder::max(field)`), same
/// owned-out/absence-is-success/required-`out` contract.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_max(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
    out: *mut *mut corvid_value,
) -> corvid_status {
    let Some(body) = consume_query("corvid_query_max", q) else {
        return corvid_status::CORVID_ERR;
    };
    if out.is_null() {
        record_argument("corvid_query_max: out is NULL (required — spec §4.7)");
        return corvid_status::CORVID_ERR; // q consumed either way (§8)
    }
    // SAFETY: out is non-NULL (checked); define the absent/failed shape
    // up front so no path leaves it dangling.
    unsafe { *out = std::ptr::null_mut() };
    let Some(field) = borrowed_utf8("corvid_query_max", "field", field, field_len) else {
        return corvid_status::CORVID_ERR;
    };
    match body.execute("corvid_query_max", |b| b.max(field)) {
        Some(Some(value)) => {
            // SAFETY: out is non-NULL (checked at the top).
            unsafe { *out = into_value(value) };
            corvid_status::CORVID_OK
        }
        Some(None) => corvid_status::CORVID_OK, // absence is a success
        None => corvid_status::CORVID_ERR,
    }
}

/// The groupiter constructors' shared tail: box an engine `BTreeMap`
/// aggregate as a group cursor — ascending group-key order (the map's
/// iteration order), values widened to `f64` (`usize` counts are exact
/// to 2^53, spec §4.7's note).
fn into_group_cursor<T>(
    groups: std::collections::BTreeMap<String, T>,
    widen: impl Fn(T) -> f64,
) -> *mut corvid_groupiter {
    let list = groups
        .into_iter()
        .map(|(key, value)| (key, widen(value)))
        .collect();
    into_groupiter(GroupIterHandle::new(list))
}

/// The groupiter constructors' shared consumption preamble (spec §5
/// rule 5 / §8): NULL-check `q`, consume it unconditionally, and hand
/// the body back — or `None` having recorded `CORVID_E_ARGUMENT`.
// SAFETY-free wrapper: the unsafe reclaim sits inside, with its
// contract note.
fn consume_query(fn_name: &str, q: *mut corvid_query) -> Option<Box<crate::handle::QueryHandle>> {
    if q.is_null() {
        record_argument(format!("{fn_name}: q is NULL").as_str());
        return None;
    }
    // SAFETY: q is non-NULL (checked) and contractually an unconsumed
    // corvid_query_new product; this call is its single consumption.
    unsafe { reclaim_query(q) }
}

/// Count matching documents grouped by the value at `field` (spec
/// §4.7; counterpart: `QueryBuilder::group_count(field)`), as a
/// `(group key, count)` cursor in ascending group-key (byte) order —
/// the engine's `BTreeMap` iteration order. Group keys use the
/// canonical tagged form (text bare; `i:`/`f:`/`b:` tags; `t:`
/// escaping for ambiguous texts). NULL + error on failure (the query
/// is consumed either way).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_group_count(
    q: *mut corvid_query,
    field: *const c_char,
    field_len: usize,
) -> *mut corvid_groupiter {
    let Some(body) = consume_query("corvid_query_group_count", q) else {
        return std::ptr::null_mut();
    };
    let Some(field) = borrowed_utf8("corvid_query_group_count", "field", field, field_len) else {
        return std::ptr::null_mut();
    };
    match body.execute("corvid_query_group_count", |b| b.group_count(field)) {
        Some(groups) => into_group_cursor(groups, |n| n as f64),
        None => std::ptr::null_mut(),
    }
}

/// Sum `value_field` grouped by `group_field` (spec §4.7; counterpart:
/// `QueryBuilder::group_sum`), as a `(group key, sum)` cursor in
/// ascending group-key order; non-numeric or missing values are
/// skipped per row (a group with none never materializes). NULL +
/// error on failure (the query is consumed either way).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_group_sum(
    q: *mut corvid_query,
    group_field: *const c_char,
    group_field_len: usize,
    value_field: *const c_char,
    value_field_len: usize,
) -> *mut corvid_groupiter {
    let Some(body) = consume_query("corvid_query_group_sum", q) else {
        return std::ptr::null_mut();
    };
    let Some(group_field) = borrowed_utf8(
        "corvid_query_group_sum",
        "group_field",
        group_field,
        group_field_len,
    ) else {
        return std::ptr::null_mut();
    };
    let Some(value_field) = borrowed_utf8(
        "corvid_query_group_sum",
        "value_field",
        value_field,
        value_field_len,
    ) else {
        return std::ptr::null_mut();
    };
    match body.execute("corvid_query_group_sum", |b| {
        b.group_sum(group_field, value_field)
    }) {
        Some(groups) => into_group_cursor(groups, f64::from),
        None => std::ptr::null_mut(),
    }
}

/// Mean of `value_field` grouped by `group_field` (spec §4.7;
/// counterpart: `QueryBuilder::group_avg`), as a `(group key, mean)`
/// cursor in ascending group-key order. NULL + error on failure (the
/// query is consumed either way).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_query_group_avg(
    q: *mut corvid_query,
    group_field: *const c_char,
    group_field_len: usize,
    value_field: *const c_char,
    value_field_len: usize,
) -> *mut corvid_groupiter {
    let Some(body) = consume_query("corvid_query_group_avg", q) else {
        return std::ptr::null_mut();
    };
    let Some(group_field) = borrowed_utf8(
        "corvid_query_group_avg",
        "group_field",
        group_field,
        group_field_len,
    ) else {
        return std::ptr::null_mut();
    };
    let Some(value_field) = borrowed_utf8(
        "corvid_query_group_avg",
        "value_field",
        value_field,
        value_field_len,
    ) else {
        return std::ptr::null_mut();
    };
    match body.execute("corvid_query_group_avg", |b| {
        b.group_avg(group_field, value_field)
    }) {
        Some(groups) => into_group_cursor(groups, f64::from),
        None => std::ptr::null_mut(),
    }
}

/// Advance the group cursor (spec §4.7): returns 1 and fills the
/// out-params for the next `(key, value)` pair, 0 at exhaustion —
/// out-params untouched at 0; never errors (the list is materialized).
/// The key bytes are BORROWED until the next call or
/// `corvid_groupiter_free` — the strs-cursor rule (strs.rs); using
/// them after is UB. The value is a `double` (`group_sum`/`group_avg`
/// means and sums; `group_count` counts, exact in a `double` to 2^53).
/// NULL handle or NULL out-parameter follows the non-status rule
/// (spec §7): return 0 with `CORVID_E_ARGUMENT` recorded.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_groupiter_next(
    it: *mut corvid_groupiter,
    key_out: *mut *const c_char,
    key_len_out: *mut usize,
    value_out: *mut f64,
) -> c_int {
    if it.is_null() || key_out.is_null() || key_len_out.is_null() || value_out.is_null() {
        record_argument("corvid_groupiter_next: NULL handle or out-param (§7 inert rule)");
        return 0;
    }
    // SAFETY: it is non-NULL (checked) with a corvid_query_group_*
    // provenance, not yet freed; the §2 contract confines a cursor to
    // one thread, so the exclusive borrow is sound.
    let cursor = unsafe { borrow_groupiter_mut(it) }.expect("non-NULL checked above");
    match cursor.next() {
        Some((key, value)) => {
            // SAFETY: all out-params are non-NULL (checked); the key
            // pointer + length pair is §1.5's binary-safe string shape,
            // borrowing the cursor's current group key.
            unsafe {
                *key_out = key.as_ptr() as *const c_char;
                *key_len_out = key.len();
                *value_out = value;
            }
            1
        }
        None => 0,
    }
}

/// Free the group cursor (spec §4.7). `corvid_groupiter_free(NULL)` is
/// a no-op (spec §7); the last key's borrowed bytes die with it.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_groupiter_free(it: *mut corvid_groupiter) {
    // SAFETY: NULL is the documented no-op; otherwise it is a
    // corvid_query_group_* product, reclaimed exactly once here.
    drop(unsafe { reclaim_groupiter(it) });
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
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open_memory;
    use crate::pred::corvid_cmp::CORVID_CMP_EQ;
    use crate::pred::corvid_cmp::CORVID_CMP_GT;
    use crate::pred::corvid_pred_compare;
    use crate::value::corvid_value_as_float;
    use crate::value::corvid_value_as_int;
    use crate::value::corvid_value_float;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_len;
    use crate::value::corvid_value_map_get;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;
    use crate::value::corvid_value_text;
    use crate::value::corvid_value_type;
    use crate::value::corvid_value_type::CORVID_TYPE_FLOAT;
    use crate::value::corvid_value_type::CORVID_TYPE_INT;
    use crate::value::corvid_value_type::CORVID_TYPE_MAP;
    use crate::value::corvid_value_vector;

    type Coll = *mut corvid_coll;
    type Query = *mut corvid_query;

    /// (pointer, length) for a borrowed UTF-8 parameter (§1.5).
    fn s(text: &str) -> (*const c_char, usize) {
        (text.as_ptr() as *const c_char, text.len())
    }

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let (name, len) = s("docs");
        let coll = corvid_collection(db, name, len);
        assert!(!coll.is_null());
        (db, coll)
    }

    /// Build a map document through the value ABI; consumes the item
    /// handles (map_put's §8 discipline).
    fn doc(pairs: &[(&str, *mut corvid_value)]) -> *mut corvid_value {
        let map = corvid_value_map_new();
        for (key, item) in pairs {
            let (key, key_len) = s(key);
            assert_eq!(corvid_value_map_put(map, key, key_len, *item), CORVID_OK);
        }
        map
    }

    fn text_value(text: &str) -> *mut corvid_value {
        let (ptr, len) = s(text);
        corvid_value_text(ptr, len)
    }

    fn vector_value(v: &[f32]) -> *mut corvid_value {
        corvid_value_vector(v.as_ptr(), v.len())
    }

    fn insert(coll: Coll, key: &[u8], document: *mut corvid_value) {
        assert_eq!(
            crate::mutation::corvid_insert(coll, key.as_ptr(), key.len(), document),
            CORVID_OK
        );
        corvid_value_free(document);
    }

    fn query(coll: Coll) -> Query {
        let q = corvid_query_new(coll);
        assert!(!q.is_null(), "a live coll never fails here (§4.6)");
        q
    }

    /// tag-eq filter, consumed by the call (never freed in tests).
    fn tag_eq(tag: &str) -> *mut crate::handle::corvid_pred {
        let (path, path_len) = s("tag");
        corvid_pred_compare(path, path_len, CORVID_CMP_EQ, text_value(tag))
    }

    /// kind-eq filter — the hybrid corpus's shared field (matches both
    /// docs, so rankings are the unfiltered ones).
    fn kind_eq() -> *mut crate::handle::corvid_pred {
        let (path, path_len) = s("kind");
        corvid_pred_compare(path, path_len, CORVID_CMP_EQ, text_value("doc"))
    }

    fn n_gt(n: i64) -> *mut crate::handle::corvid_pred {
        let (path, path_len) = s("n");
        corvid_pred_compare(path, path_len, CORVID_CMP_GT, corvid_value_int(n))
    }

    /// Walk a rows cursor through the ABI, collecting (key, score); the
    /// doc is inspected per-row by the caller's `inspect` INSIDE the
    /// walk (borrowed only until the next next — value.rs's rule).
    fn walk<F>(rows: *mut corvid_rows, mut inspect: F) -> Vec<(Vec<u8>, f32)>
    where
        F: FnMut(&[u8], *const corvid_value),
    {
        let mut out = Vec::new();
        loop {
            let mut key: *const u8 = std::ptr::null();
            let mut key_len = 0usize;
            let mut doc: *const corvid_value = std::ptr::null();
            let mut score = f32::NAN;
            if corvid_rows_next(rows, &mut key, &mut key_len, &mut doc, &mut score) != 1 {
                return out; // exhaustion: finish() pins the sticky shape
            }
            // SAFETY: key/doc borrow the cursor's current row, valid
            // until the next corvid_rows_next (which this loop makes
            // only after inspect returns).
            let key_bytes = unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec();
            inspect(&key_bytes, doc);
            out.push((key_bytes, score));
        }
    }

    /// Walk to exhaustion (after `walk`'s rows), pinning the sticky-0
    /// and untouched-out-params shape, then free the cursor.
    fn finish(rows: *mut corvid_rows) {
        let mut key: *const u8 = std::ptr::dangling();
        let mut key_len = usize::MAX;
        let mut doc: *const corvid_value = std::ptr::dangling();
        let mut score = f32::NAN;
        assert_eq!(
            corvid_rows_next(rows, &mut key, &mut key_len, &mut doc, &mut score),
            0
        );
        assert_eq!(key_len, usize::MAX, "exhaustion leaves out-params alone");
        assert!(score.is_nan(), "the sentinel survived untouched");
        corvid_rows_free(rows);
    }

    /// `corvid_rows_next` expecting immediate exhaustion (0), with
    /// throwaway out-params.
    fn assert_exhausted(rows: *mut corvid_rows) {
        let mut key: *const u8 = std::ptr::null();
        let mut key_len = 0usize;
        let mut doc: *const corvid_value = std::ptr::null();
        let mut score = 0f32;
        assert_eq!(
            corvid_rows_next(rows, &mut key, &mut key_len, &mut doc, &mut score),
            0
        );
    }

    /// The db's exclusivity gate (§4.13) — the counter observable the
    /// query family's consumption wiring moves.
    fn exclusive(db: *mut crate::handle::corvid_db) -> bool {
        // SAFETY: db is a live corvid_open_memory product in these
        // tests (never closed at the call site).
        unsafe { crate::handle::borrow_db(db) }
            .expect("non-NULL")
            .is_exclusive()
    }

    /// The hybrid corpus: `strong` matches both modalities, `weak`
    /// only the vector one.
    fn hybrid_corpus(coll: Coll) {
        insert(
            coll,
            b"strong".as_ref(),
            doc(&[
                ("tag", text_value("s")),
                ("kind", text_value("doc")),
                ("body", text_value("rust embedded database")),
                ("v", vector_value(&[1.0, 0.0])),
            ]),
        );
        insert(
            coll,
            b"weak".as_ref(),
            doc(&[
                ("tag", text_value("w")),
                ("kind", text_value("doc")),
                ("body", text_value("python web frameworks")),
                ("v", vector_value(&[0.0, 1.0])),
            ]),
        );
    }

    // --- §4.6: the hybrid query with exact RRF scores -----------------------

    /// The brief's centerpiece: filter + vector + text + rerank_mmr +
    /// limit through the ABI, scores asserted bitwise against the RRF
    /// `1/(k+rank)` arithmetic (SYNTAX.md / fusion.rs): `strong` is
    /// rank 1 of BOTH sources (`1/61 + 1/61`), `weak` is rank 2 of the
    /// vector source and ABSENT from the text ranking (BM25 keeps only
    /// positive scores) so it fuses to `1/62`; MMR keeps fused scores,
    /// and lambda 1.0 (pure relevance) preserves the order here.
    #[test]
    fn hybrid_query_pins_exact_rrf_scores() {
        let (db, coll) = fresh();
        hybrid_corpus(coll);

        let q = query(coll);
        assert_eq!(corvid_query_filter(q, kind_eq()), CORVID_OK);
        let (field, field_len) = s("v");
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                [1.0f32, 0.0].as_ptr(),
                2,
                2,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_OK
        );
        let (body, body_len) = s("body");
        let (text, text_len) = s("rust database");
        assert_eq!(
            corvid_query_text(q, body, body_len, text, text_len, 2),
            CORVID_OK
        );
        assert_eq!(corvid_query_rerank_mmr(q, 1.0), CORVID_OK);
        assert_eq!(corvid_query_limit(q, 2), CORVID_OK);

        let rows = corvid_query_run(q);
        assert!(!rows.is_null());
        let walked = walk(rows, |_, _| {});
        assert_eq!(walked.len(), 2);
        assert_eq!(walked[0].0, b"strong".to_vec());
        assert_eq!(walked[1].0, b"weak".to_vec());
        assert_eq!(walked[0].1, 1.0f32 / 61.0 + 1.0f32 / 61.0);
        assert_eq!(walked[1].1, 1.0f32 / 62.0);
        assert!(walked[0].1 > walked[1].1);
        finish(rows);

        // The filter is a true PRE-ranking predicate: a tag filter that
        // excludes `weak` leaves `strong` rank 1 of both (2-source)
        // rankings over the FILTERED set — 2/61 again, one row.
        let q = query(coll);
        assert_eq!(corvid_query_filter(q, tag_eq("s")), CORVID_OK);
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                [1.0f32, 0.0].as_ptr(),
                2,
                2,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_OK
        );
        assert_eq!(
            corvid_query_text(q, body, body_len, text, text_len, 2),
            CORVID_OK
        );
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert_eq!(
            walked,
            vec![(b"strong".to_vec(), 1.0f32 / 61.0 + 1.0f32 / 61.0)]
        );
        finish(rows);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// Score presence by source shape (spec §4.6): pure filter and pure
    /// order_by queries score 0.0; a lone vector source scores
    /// 1/(60+rank) per row.
    #[test]
    fn score_presence_follows_the_source_shape() {
        let (db, coll) = fresh();
        hybrid_corpus(coll);

        // Pure filter: 0.0 everywhere.
        let q = query(coll);
        assert_eq!(corvid_query_filter(q, kind_eq()), CORVID_OK);
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert_eq!(walked.len(), 2);
        assert!(walked.iter().all(|&(_, score)| score == 0.0));
        finish(rows);

        // Pure order_by: rank order replaced, scores 0.0.
        let q = query(coll);
        let (field, field_len) = s("tag");
        assert_eq!(corvid_query_order_by(q, field, field_len, 0), CORVID_OK);
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert!(walked.iter().all(|&(_, score)| score == 0.0));
        finish(rows);

        // A lone vector source: RRF of one ranking.
        let q = query(coll);
        let (v, v_len) = s("v");
        assert_eq!(
            corvid_query_vector(
                q,
                v,
                v_len,
                [1.0f32, 0.0].as_ptr(),
                2,
                2,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_OK
        );
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert_eq!(walked[0].1, 1.0f32 / 61.0);
        assert_eq!(walked[1].1, 1.0f32 / 62.0);
        finish(rows);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- §4.6: the rows cursor ------------------------------------------------

    /// The borrowed-doc contract's testable half: each row's doc reads
    /// through the value ABI inside the walk, differs per row, and the
    /// sanctioned escape (clone) survives past exhaustion.
    #[test]
    fn rows_cursor_walks_borrowed_docs() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("n", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("n", corvid_value_int(2))]));

        let q = query(coll);
        let (path, path_len) = s("n");
        assert_eq!(corvid_query_filter(q, n_gt(0)), CORVID_OK);
        let rows = corvid_query_run(q);

        let mut seen: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut cloned: Vec<(*mut corvid_value, i64)> = Vec::new();
        walk(rows, |key, doc_ptr| {
            let child = corvid_value_map_get(doc_ptr, path, path_len);
            assert!(!child.is_null(), "every walked doc carries n");
            let mut ok = 0;
            let n = corvid_value_as_int(child, &mut ok);
            assert_eq!(ok, 1, "the borrowed doc reads through the ABI");
            seen.push((key.to_vec(), n));
            // The sanctioned escape (value.rs): clone outlives the row.
            cloned.push((crate::value::corvid_value_clone(doc_ptr), n));
        });
        assert_eq!(
            seen,
            vec![(b"a".to_vec(), 1), (b"b".to_vec(), 2)],
            "key order, per-row docs"
        );
        // Exhaustion is sticky, out-params untouched; the CLONES still
        // read after the cursor moved past their rows.
        let mut key: *const u8 = std::ptr::null();
        let mut key_len = usize::MAX;
        let mut doc: *const corvid_value = std::ptr::null();
        let mut score = f32::NAN;
        assert_eq!(
            corvid_rows_next(rows, &mut key, &mut key_len, &mut doc, &mut score),
            0
        );
        assert_eq!(key_len, usize::MAX);
        for (handle, n) in cloned {
            let mut ok = 0;
            assert_eq!(
                corvid_value_as_int(corvid_value_map_get(handle, path, path_len), &mut ok),
                n
            );
            assert_eq!(ok, 1, "the clone outlived the walk");
            corvid_value_free(handle);
        }
        // The stale-doc half (using `doc` after the next call) is the
        // documented UB boundary — deliberately not exercised (the T3
        // precedent for borrowed children).
        corvid_rows_free(rows);

        // An empty result is a CURSOR, not a failure: non-NULL, next 0.
        let q = query(coll);
        assert_eq!(corvid_query_filter(q, n_gt(99)), CORVID_OK);
        let empty = corvid_query_run(q);
        assert!(!empty.is_null(), "empty result still returns a cursor");
        assert_eq!(
            corvid_rows_next(empty, &mut key, &mut key_len, &mut doc, &mut score),
            0
        );
        corvid_rows_free(empty);

        // §7's inert rule: NULL handle and each NULL out-param.
        assert_eq!(
            corvid_rows_next(
                std::ptr::null_mut(),
                &mut key,
                &mut key_len,
                &mut doc,
                &mut score
            ),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        let rows = corvid_query_run(query(coll));
        assert_eq!(
            corvid_rows_next(
                rows,
                std::ptr::null_mut(),
                &mut key_len,
                &mut doc,
                &mut score
            ),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_rows_next(rows, &mut key, std::ptr::null_mut(), &mut doc, &mut score),
            0
        );
        assert_eq!(
            corvid_rows_next(
                rows,
                &mut key,
                &mut key_len,
                std::ptr::null_mut(),
                &mut score
            ),
            0
        );
        assert_eq!(
            corvid_rows_next(rows, &mut key, &mut key_len, &mut doc, std::ptr::null_mut()),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_rows_free(rows);
        corvid_rows_free(std::ptr::null_mut()); // §7 no-op

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- §4.6: validate-at-run (audit C6) --------------------------------------

    /// Bad `fuse_rrf` k / `rerank_mmr` lambda values are stored by the
    /// setters (`CORVID_OK` — the fluent rule) and rejected by `run`
    /// AND the aggregates with `CORVID_E_ARGUMENT`; the boundary
    /// values 0.0/1.0 run. The failed run has still CONSUMED the query
    /// (§8) — observed here through the derived counter dropping back.
    #[test]
    fn ranking_args_validate_at_execution_not_at_set_time() {
        let (db, coll) = fresh();
        hybrid_corpus(coll);

        for bad_k in [0.0f32, -60.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let q = query(coll);
            assert_eq!(corvid_query_fuse_rrf(q, bad_k), CORVID_OK, "setter stores");
            assert!(corvid_query_run(q).is_null(), "run rejects {bad_k}");
            assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        }
        for bad_lambda in [-0.5f32, 1.5, f32::NAN] {
            let q = query(coll);
            assert_eq!(corvid_query_rerank_mmr(q, bad_lambda), CORVID_OK);
            assert!(corvid_query_run(q).is_null(), "run rejects {bad_lambda}");
            assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        }
        // The aggregates validate too (spec §4.6 wording): a bad k
        // fails count, not just run.
        let q = query(coll);
        assert_eq!(corvid_query_fuse_rrf(q, 0.0), CORVID_OK);
        let mut n = usize::MAX;
        assert_eq!(corvid_query_count(q, &mut n), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(n, usize::MAX, "failed count writes nothing");

        // Boundaries are legal (engine [0,1] inclusive; k positive).
        for good_lambda in [0.0f32, 1.0] {
            let q = query(coll);
            assert_eq!(corvid_query_rerank_mmr(q, good_lambda), CORVID_OK);
            assert!(!corvid_query_run(q).is_null(), "lambda {good_lambda} runs");
        }

        // Consumption-on-failure, observed through the §4.13 counter:
        // free the coll first so the failed run's release is the one
        // that restores exclusivity.
        corvid_collection_free(coll);
        let coll2 = {
            let (name, len) = s("docs2");
            corvid_collection(db, name, len)
        };
        hybrid_corpus(coll2);
        let q = query(coll2);
        assert_eq!(corvid_query_fuse_rrf(q, f32::NAN), CORVID_OK);
        corvid_collection_free(coll2);
        assert!(!exclusive(db), "the live query still blocks compaction");
        assert!(corvid_query_run(q).is_null(), "the NaN k run fails");
        assert!(
            exclusive(db),
            "the FAILED run consumed (and released) the query"
        );

        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- §4.6: consumption + counter wiring -------------------------------------

    /// `corvid_query_run`/aggregates and `corvid_query_free` are each
    /// the query's single consumption: the derived count moves exactly
    /// once per handle (§4.13), and a query keeps the engine working
    /// after `corvid_close` (§2). Running/freeing twice is the
    /// documented UB — not exercised.
    #[test]
    fn query_consumption_wires_the_counter_and_outlives_close() {
        let (db, coll) = fresh();

        // query_free on a NEVER-RUN builder releases the count.
        let q = query(coll);
        corvid_query_free(q);
        corvid_query_free(std::ptr::null_mut()); // §7 no-op

        // A run consumes; an aggregate consumes; the count follows.
        let q = query(coll);
        let rows = corvid_query_run(q);
        assert!(!rows.is_null());
        corvid_rows_free(rows);
        let q = query(coll);
        let mut n = 0usize;
        assert_eq!(corvid_query_count(q, &mut n), CORVID_OK);
        assert_eq!(n, 0, "the collection is empty");

        // The §2 pin: close the db, the query still runs.
        let q = query(coll);
        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
        let (path, path_len) = s("v");
        assert_eq!(
            corvid_query_vector(
                q,
                path,
                path_len,
                [0.0f32, 1.0].as_ptr(),
                2,
                1,
                corvid_metric::CORVID_METRIC_DOT
            ),
            CORVID_OK
        );
        let rows = corvid_query_run(q);
        assert!(!rows.is_null(), "the orphaned query executes (§2)");
        assert_exhausted(rows);
        corvid_rows_free(rows);
        // (query_free after run would be the double-free UB — skipped.)

        // Arithmetic-only tail: with the coll gone, one live query is
        // exactly the +1 over the db handle.
        let db2 = corvid_open_memory();
        assert!(!db2.is_null());
        let (name, len) = s("d");
        let coll2 = corvid_collection(db2, name, len);
        assert!(!exclusive(db2), "coll handle is +1");
        let q = query(coll2);
        corvid_collection_free(coll2);
        assert!(!exclusive(db2), "query handle is +1");
        corvid_query_free(q);
        assert!(exclusive(db2), "the free path releases");
        assert_eq!(corvid_close(db2), CORVID_OK);
    }

    /// The rows cursor owns its materialized rows: a query run BEFORE
    /// `corvid_close` still walks AFTER it (spec §2/§4.1: "freeing the db
    /// while rows/iterators from it are live is fine (those own their
    /// data)"). The page-family twin of this shape landed in Task 4
    /// (read.rs); this is the query-family pin (the Task 5 review
    /// prepend's cosmetic gap).
    #[test]
    fn rows_survive_close() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("v", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("v", corvid_value_int(2))]));

        let rows = {
            let q = query(coll);
            corvid_query_run(q) // consumes q
        };
        assert!(!rows.is_null());
        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);

        let walked = walk(rows, |_, _| {});
        assert_eq!(walked.len(), 2, "the materialized rows outlive the db");
        assert_eq!(walked[0].0, b"a".to_vec());
        assert_eq!(walked[1].0, b"b".to_vec());
        finish(rows);
    }

    // --- §4.6: filters AND; limit/offset; order_by; select ----------------------

    /// Multiple `corvid_query_filter` calls AND together; a NULL pred
    /// is an argument error that consumes nothing.
    #[test]
    fn filters_and_together_and_null_pred_is_an_argument_error() {
        let (db, coll) = fresh();
        for (key, tag, n) in [("a", "x", 1), ("b", "x", 5), ("c", "y", 9)] {
            insert(
                coll,
                key.as_bytes(),
                doc(&[("tag", text_value(tag)), ("n", corvid_value_int(n))]),
            );
        }

        let q = query(coll);
        assert_eq!(corvid_query_filter(q, tag_eq("x")), CORVID_OK);
        assert_eq!(corvid_query_filter(q, n_gt(2)), CORVID_OK);
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert_eq!(walked, vec![(b"b".to_vec(), 0.0)], "tag==x AND n>2");
        finish(rows);

        // NULL pred: E_ARGUMENT, the query itself unaffected (a pred it
        // never took is not its input to consume).
        let q = query(coll);
        assert_eq!(corvid_query_filter(q, std::ptr::null_mut()), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        let rows = corvid_query_run(q);
        assert_eq!(
            walk(rows, |_, _| {}).len(),
            3,
            "the failed filter added nothing"
        );
        finish(rows);

        // NULL q: E_ARGUMENT (the pred is consumed anyway per §8, so it
        // is handed a fresh one and never touched again).
        assert_eq!(
            corvid_query_filter(std::ptr::null_mut(), tag_eq("x")),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// `limit`/`offset` boundaries (spec §4.6): limit 0 is empty,
    /// offset applies after ordering before limit, offset at/past the
    /// end is empty, an over-long limit returns everything.
    #[test]
    fn limit_offset_boundaries_window_after_ordering() {
        let (db, coll) = fresh();
        for (key, n) in [("a", 4), ("b", 2), ("c", 1), ("d", 3)] {
            insert(coll, key.as_bytes(), doc(&[("n", corvid_value_int(n))]));
        }

        // limit 0 → empty.
        let q = query(coll);
        assert_eq!(corvid_query_limit(q, 0), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_exhausted(rows);
        corvid_rows_free(rows);

        // offset == len and offset beyond → empty.
        for offset in [4usize, 10] {
            let q = query(coll);
            assert_eq!(corvid_query_offset(q, offset), CORVID_OK);
            let rows = corvid_query_run(q);
            assert_exhausted(rows);
            corvid_rows_free(rows);
        }

        // Window after ordering: n asc = c(1), b(2), d(3), a(4); offset
        // 1 limit 2 → the middle two.
        let (field, field_len) = s("n");
        let q = query(coll);
        assert_eq!(corvid_query_order_by(q, field, field_len, 0), CORVID_OK);
        assert_eq!(corvid_query_offset(q, 1), CORVID_OK);
        assert_eq!(corvid_query_limit(q, 2), CORVID_OK);
        let rows = corvid_query_run(q);
        let walked = walk(rows, |_, _| {});
        assert_eq!(
            walked.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![b"b".to_vec(), b"d".to_vec()],
            "offset after ordering, before limit"
        );
        finish(rows);

        // Over-long limit returns all four in n order.
        let q = query(coll);
        assert_eq!(corvid_query_order_by(q, field, field_len, 0), CORVID_OK);
        assert_eq!(corvid_query_limit(q, 99), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_eq!(walk(rows, |_, _| {}).len(), 4);
        finish(rows);

        // Offset without limit: the tail.
        let q = query(coll);
        assert_eq!(corvid_query_offset(q, 3), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_eq!(walk(rows, |_, _| {}).len(), 1);
        finish(rows);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The ordering contract through the ABI (audit C4): comparable
    /// values first in value order, rows missing the field last, ties
    /// by key; `descending` (any non-zero int) reverses within-class
    /// order only.
    #[test]
    fn order_by_sorts_comparables_first_missing_last_both_directions() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("n", corvid_value_int(3))]));
        insert(coll, b"b", doc(&[("n", corvid_value_int(1))]));
        insert(coll, b"c", doc(&[("n", corvid_value_int(2))]));
        insert(coll, b"d", doc(&[("t", text_value("unrelated"))]));

        let (field, field_len) = s("n");
        let keys_of = |descending: c_int| {
            let q = query(coll);
            assert_eq!(
                corvid_query_order_by(q, field, field_len, descending),
                CORVID_OK
            );
            let rows = corvid_query_run(q);
            let keys = walk(rows, |_, _| {})
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>();
            finish(rows);
            keys
        };
        assert_eq!(
            keys_of(0),
            vec![b"b", b"c", b"a", b"d"],
            "asc: value order, missing last"
        );
        assert_eq!(
            keys_of(1),
            vec![b"a", b"c", b"b", b"d"],
            "desc reverses the class, not the missing-last rule"
        );
        assert_eq!(
            keys_of(-1),
            vec![b"a", b"c", b"b", b"d"],
            "any non-zero descending"
        );

        // NULL field is the usual argument error.
        let q = query(coll);
        assert_eq!(corvid_query_order_by(q, std::ptr::null(), 0, 0), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_query_free(q);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// `select` projects map documents to the named top-level fields
    /// (missing fields absent), passes non-map documents through
    /// unchanged, and count 0 is the engine-faithful EMPTY projection
    /// (map documents become empty maps — `select(vec![])` in Rust).
    /// Ranking still sees the full document (the filter matched on the
    /// projected-away field).
    #[test]
    fn select_projects_maps_and_passes_non_maps_through() {
        let (db, coll) = fresh();
        insert(
            coll,
            b"a",
            doc(&[
                ("n", corvid_value_int(1)),
                ("t", text_value("keep me")),
                ("v", vector_value(&[1.0, 2.0])),
            ]),
        );
        insert(coll, b"b", corvid_value_int(42)); // a non-map document

        let (n_field, n_len) = s("n");
        let (t_field, t_len) = s("t");
        let (v_field, v_len) = s("v");

        // Two fields: exactly n and t survive; v is absent.
        let q = query(coll);
        let fields: [*const c_char; 2] = [n_field, t_field];
        let lens: [usize; 2] = [n_len, t_len];
        assert_eq!(
            corvid_query_select(q, fields.as_ptr(), lens.as_ptr(), 2),
            CORVID_OK
        );
        let rows = corvid_query_run(q);
        let mut shapes: Vec<(Vec<u8>, u32, usize, bool)> = Vec::new();
        walk(rows, |key, doc_ptr| {
            shapes.push((
                key.to_vec(),
                corvid_value_type(doc_ptr) as u32,
                corvid_value_len(doc_ptr),
                !corvid_value_map_get(doc_ptr, v_field, v_len).is_null(),
            ));
        });
        finish(rows);
        assert_eq!(shapes[0], (b"a".to_vec(), CORVID_TYPE_MAP as u32, 2, false));
        assert_eq!(
            shapes[1],
            (b"b".to_vec(), CORVID_TYPE_INT as u32, 0, false),
            "the non-map doc passes through unchanged (len 0: a scalar)"
        );

        // Ranking still sees the FULL document (spec §4.6): a filter on
        // n matches although the projection drops n and keeps only t.
        let q = query(coll);
        let fields: [*const c_char; 1] = [t_field];
        let lens: [usize; 1] = [t_len];
        assert_eq!(
            corvid_query_select(q, fields.as_ptr(), lens.as_ptr(), 1),
            CORVID_OK
        );
        assert_eq!(corvid_query_filter(q, n_gt(0)), CORVID_OK);
        let rows = corvid_query_run(q);
        let mut shapes: Vec<(Vec<u8>, usize, bool)> = Vec::new();
        walk(rows, |key, doc_ptr| {
            shapes.push((
                key.to_vec(),
                corvid_value_len(doc_ptr),
                corvid_value_map_get(doc_ptr, n_field, n_len).is_null(),
            ));
        });
        finish(rows);
        assert_eq!(
            shapes,
            vec![(b"a".to_vec(), 1, true)],
            "the filter matched the projected-away n; only t remains"
        );

        // count 0 with NULL arrays: the empty projection.
        let q = query(coll);
        assert_eq!(
            corvid_query_select(q, std::ptr::null(), std::ptr::null(), 0),
            CORVID_OK
        );
        let rows = corvid_query_run(q);
        let mut projected: Vec<(Vec<u8>, u32, usize)> = Vec::new();
        walk(rows, |key, doc_ptr| {
            projected.push((
                key.to_vec(),
                corvid_value_type(doc_ptr) as u32,
                corvid_value_len(doc_ptr),
            ));
        });
        finish(rows);
        assert_eq!(
            projected[0],
            (b"a".to_vec(), CORVID_TYPE_MAP as u32, 0),
            "empty map"
        );
        assert_eq!(
            projected[1],
            (b"b".to_vec(), CORVID_TYPE_INT as u32, 0),
            "the non-map doc is still untouched"
        );

        // NULL arrays with count > 0, and a NULL element, are E_ARGUMENT.
        let q = query(coll);
        assert_eq!(
            corvid_query_select(q, std::ptr::null(), std::ptr::null(), 1),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        let fields: [*const c_char; 2] = [n_field, std::ptr::null()];
        assert_eq!(
            corvid_query_select(q, fields.as_ptr(), lens.as_ptr(), 2),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        // A non-UTF-8 field name too.
        let bad = [0xFF_u8, 0xFE];
        let fields: [*const c_char; 1] = [bad.as_ptr() as *const c_char];
        let lens: [usize; 1] = [bad.len()];
        assert_eq!(
            corvid_query_select(q, fields.as_ptr(), lens.as_ptr(), 1),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_query_free(q);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The setters' argument discipline (§1.5/§1.4/§7): NULL or
    /// misencoded pointers and out-of-domain metrics are
    /// `CORVID_E_ARGUMENT` and leave the query runnable; NULL q is the
    /// inert error for every setter.
    #[test]
    fn setter_argument_discipline_leaves_the_query_usable() {
        let (db, coll) = fresh();
        hybrid_corpus(coll);

        let (field, field_len) = s("v");
        let bad = [0xFF_u8, 0xFE];

        // vector: NULL field, non-UTF-8 field, NULL query buffer, and a
        // metric outside the frozen set.
        let q = query(coll);
        assert_eq!(
            corvid_query_vector(
                q,
                std::ptr::null(),
                0,
                [1.0f32, 0.0].as_ptr(),
                2,
                1,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_query_vector(
                q,
                bad.as_ptr() as *const c_char,
                bad.len(),
                [1.0f32, 0.0].as_ptr(),
                2,
                1,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_ERR
        );
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                std::ptr::null(),
                2,
                1,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_ERR,
            "NULL query with dim > 0 is the §1.5 unexpected-NULL shape"
        );
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                std::ptr::null(),
                0,
                1,
                corvid_metric::CORVID_METRIC_COSINE
            ),
            CORVID_ERR,
            "the buffer pointer is non-nullable at dim 0 too (the value-family rule)"
        );
        // The engine's L2/DOT arms run through the ABI.
        for metric in [
            corvid_metric::CORVID_METRIC_COSINE,
            corvid_metric::CORVID_METRIC_DOT,
            corvid_metric::CORVID_METRIC_L2,
        ] {
            let q = query(coll);
            assert_eq!(
                corvid_query_vector(q, field, field_len, [1.0f32, 0.0].as_ptr(), 2, 2, metric),
                CORVID_OK
            );
            let rows = corvid_query_run(q);
            assert_eq!(walk(rows, |_, _| {}).len(), 2, "{metric:?} ranks both docs");
            finish(rows);
        }
        // The rejected setter left the earlier query runnable: it holds
        // no sources, so it is a pure (empty-filter) scan.
        let rows = corvid_query_run(q);
        assert_eq!(
            walk(rows, |_, _| {}).len(),
            2,
            "no partial state from the rejected setters"
        );
        finish(rows);

        // text: NULL field and NULL s.
        let q = query(coll);
        let (body, body_len) = s("body");
        assert_eq!(
            corvid_query_text(q, std::ptr::null(), 0, body, body_len, 1),
            CORVID_ERR
        );
        assert_eq!(
            corvid_query_text(q, body, body_len, std::ptr::null(), 0, 1),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        corvid_query_free(q);

        // Every setter answers NULL q with E_ARGUMENT.
        assert_eq!(
            corvid_query_fuse_rrf(std::ptr::null_mut(), 60.0),
            CORVID_ERR
        );
        assert_eq!(
            corvid_query_rerank_mmr(std::ptr::null_mut(), 1.0),
            CORVID_ERR
        );
        assert_eq!(corvid_query_approx(std::ptr::null_mut()), CORVID_ERR);
        assert_eq!(corvid_query_limit(std::ptr::null_mut(), 1), CORVID_ERR);
        assert_eq!(corvid_query_offset(std::ptr::null_mut(), 1), CORVID_ERR);
        assert_eq!(
            corvid_query_order_by(std::ptr::null_mut(), body, body_len, 0),
            CORVID_ERR
        );
        assert_eq!(
            corvid_query_select(std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), 0),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_query_new(std::ptr::null_mut()).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // The out-of-domain metric rejection, at the mapper level (an
        // invalid enum cannot be constructed in Rust without UB — the
        // cmp_op precedent). The context parameter names the rejecter.
        assert!(metric_of("corvid_query_vector", 3).is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        let (_, msg_len) = crate::error::last_message().expect("recorded above");
        assert!(msg_len > 0);
        assert_eq!(metric_of("corvid_query_vector", 0), Some(Metric::Cosine));
        assert_eq!(metric_of("corvid_query_vector", 1), Some(Metric::Dot));
        assert_eq!(metric_of("corvid_query_vector", 2), Some(Metric::L2));

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- §4.7: aggregation oracles ---------------------------------------------

    /// `count` (filtered and the O(1) unfiltered path) and
    /// `count_distinct`'s typed-key rule: 1, 1.0, "1", true, and
    /// "i:1" are FIVE distinct values (tags i:/f:/b: plus bare text,
    /// with the colliding text t:-escaped); missing and containers
    /// are ignored. Aggregates ignore retrieval sources and
    /// limit/offset/select (spec §4.7).
    #[test]
    fn count_and_count_distinct_pin_typed_distinctness() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("v", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("v", corvid_value_float(1.0))]));
        insert(coll, b"c", doc(&[("v", text_value("1"))]));
        insert(
            coll,
            b"d",
            doc(&[("v", crate::value::corvid_value_bool(1))]),
        );
        insert(coll, b"e", doc(&[("v", text_value("i:1"))]));
        insert(coll, b"f", doc(&[("v", corvid_value_int(1))])); // duplicate of a
        insert(
            coll,
            b"g",
            doc(&[("v", crate::value::corvid_value_array_new())]),
        ); // container: ignored
        insert(coll, b"h", doc(&[("other", corvid_value_int(9))])); // missing: ignored

        let (field, field_len) = s("v");
        let mut n = usize::MAX;
        assert_eq!(corvid_query_count(query(coll), &mut n), CORVID_OK);
        assert_eq!(n, 8, "unfiltered: the O(1) maintained counter");

        let q = query(coll);
        assert_eq!(corvid_query_filter(q, n_gt(0)), CORVID_OK); // matches nothing
        let mut n2 = usize::MAX;
        assert_eq!(corvid_query_count(q, &mut n2), CORVID_OK);
        assert_eq!(n2, 0);

        let mut distinct = usize::MAX;
        assert_eq!(
            corvid_query_count_distinct(query(coll), field, field_len, &mut distinct),
            CORVID_OK
        );
        assert_eq!(
            distinct, 5,
            "1, 1.0, \"1\", true, \"i:1\" — five typed keys"
        );

        // Sources/limit/select are ignored by aggregates.
        let q = query(coll);
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                [0.0f32].as_ptr(),
                1,
                1,
                corvid_metric::CORVID_METRIC_L2
            ),
            CORVID_OK
        );
        assert_eq!(corvid_query_limit(q, 1), CORVID_OK);
        assert_eq!(corvid_query_offset(q, 1), CORVID_OK);
        let mut n3 = usize::MAX;
        assert_eq!(corvid_query_count(q, &mut n3), CORVID_OK);
        assert_eq!(n3, 8, "the aggregate saw the full filtered set");

        // out-params are nullable (§7): the call still executes.
        assert_eq!(
            corvid_query_count(query(coll), std::ptr::null_mut()),
            CORVID_OK
        );
        assert_eq!(
            corvid_query_count_distinct(query(coll), field, field_len, std::ptr::null_mut()),
            CORVID_OK
        );

        // NULL field / NULL q.
        assert_eq!(
            corvid_query_count_distinct(query(coll), std::ptr::null(), 0, &mut distinct),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_query_count(std::ptr::null_mut(), &mut distinct),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// `sum`/`avg` exact-value oracles: int/float mix, skips (missing,
    /// non-numeric), all-skipped sum 0.0 / avg has_value 0, and the
    /// nullable out-params.
    #[test]
    fn sum_and_avg_skip_non_numeric_and_pin_exact_values() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("n", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("n", corvid_value_float(2.5))]));
        insert(coll, b"c", doc(&[("n", corvid_value_int(10))]));
        insert(coll, b"d", doc(&[("n", text_value("skip me"))]));
        insert(coll, b"e", doc(&[("other", corvid_value_int(100))])); // missing n

        let (field, field_len) = s("n");
        let mut total = f64::NAN;
        assert_eq!(
            corvid_query_sum(query(coll), field, field_len, &mut total),
            CORVID_OK
        );
        assert_eq!(total, 13.5, "1 + 2.5 + 10; text and missing skipped");

        let mut mean = f64::NAN;
        let mut has_value = -1;
        assert_eq!(
            corvid_query_avg(query(coll), field, field_len, &mut mean, &mut has_value),
            CORVID_OK
        );
        assert_eq!(mean, 4.5, "13.5 / 3");
        assert_eq!(has_value, 1);

        // All-skipped: sum 0.0, avg has_value 0 with the defined 0.0.
        let (other, other_len) = s("other");
        assert_eq!(
            corvid_query_sum(query(coll), other, other_len, &mut total),
            CORVID_OK
        );
        assert_eq!(total, 100.0); // sanity: the field exists
        let (absent, absent_len) = s("nope");
        assert_eq!(
            corvid_query_sum(query(coll), absent, absent_len, &mut total),
            CORVID_OK
        );
        assert_eq!(total, 0.0, "an all-skipped field sums to 0.0");
        has_value = -1;
        mean = f64::NAN;
        assert_eq!(
            corvid_query_avg(query(coll), absent, absent_len, &mut mean, &mut has_value),
            CORVID_OK
        );
        assert_eq!(has_value, 0, "no numeric value: absence is a success");
        assert_eq!(mean, 0.0, "the defined no-value shape");

        // A filter narrows the aggregate.
        let q = query(coll);
        assert_eq!(corvid_query_filter(q, n_gt(2)), CORVID_OK);
        assert_eq!(corvid_query_sum(q, field, field_len, &mut total), CORVID_OK);
        assert_eq!(total, 12.5, "2.5 + 10");

        // Nullable outs; then NULL field/q.
        assert_eq!(
            corvid_query_sum(query(coll), field, field_len, std::ptr::null_mut()),
            CORVID_OK
        );
        assert_eq!(
            corvid_query_avg(
                query(coll),
                field,
                field_len,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            CORVID_OK
        );
        assert_eq!(
            corvid_query_sum(query(coll), std::ptr::null(), 0, &mut total),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_query_avg(
                std::ptr::null_mut(),
                field,
                field_len,
                &mut mean,
                &mut has_value
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// `min`/`max` oracles: exact i64 ordering, Int/Float interop with
    /// the ORIGINAL kind returned, lexicographic text, ±inf extrema,
    /// incomparable-only fields yielding the absence shape
    /// (`CORVID_OK` + `*out == NULL`), OWNED outs, and the required
    /// out-param.
    #[test]
    fn min_max_are_typed_owned_and_absence_is_success() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("n", corvid_value_int(3))]));
        insert(coll, b"b", doc(&[("n", corvid_value_int(1))]));
        insert(coll, b"c", doc(&[("n", corvid_value_int(2))]));

        let (field, field_len) = s("n");
        let mut out: *mut corvid_value = std::ptr::dangling_mut();
        assert_eq!(
            corvid_query_min(query(coll), field, field_len, &mut out),
            CORVID_OK
        );
        let mut ok = 0;
        assert_eq!(corvid_value_as_int(out, &mut ok), 1);
        assert_eq!(ok, 1);
        corvid_value_free(out); // OWNED: the caller's to free
        assert_eq!(
            corvid_query_max(query(coll), field, field_len, &mut out),
            CORVID_OK
        );
        assert_eq!(corvid_value_as_int(out, &mut ok), 3);
        corvid_value_free(out);

        // Int/Float interop: min carries the ORIGINAL Float kind, max
        // the Int.
        let (db2, coll2) = fresh();
        insert(coll2, b"a", doc(&[("n", corvid_value_int(3))]));
        insert(coll2, b"b", doc(&[("n", corvid_value_float(2.5))]));
        insert(coll2, b"c", doc(&[("n", corvid_value_int(10))]));
        assert_eq!(
            corvid_query_min(query(coll2), field, field_len, &mut out),
            CORVID_OK
        );
        assert_eq!(corvid_value_type(out), CORVID_TYPE_FLOAT);
        assert_eq!(corvid_value_as_float(out, &mut ok), 2.5);
        assert_eq!(ok, 1);
        corvid_value_free(out);
        assert_eq!(
            corvid_query_max(query(coll2), field, field_len, &mut out),
            CORVID_OK
        );
        assert_eq!(corvid_value_type(out), CORVID_TYPE_INT);
        corvid_value_free(out);

        // Text is byte-lexicographic: "Apple" < "banana" < "cherry".
        let (db3, coll3) = fresh();
        insert(coll3, b"a", doc(&[("t", text_value("banana"))]));
        insert(coll3, b"b", doc(&[("t", text_value("cherry"))]));
        insert(coll3, b"c", doc(&[("t", text_value("Apple"))]));
        let (t, t_len) = s("t");
        let mut text_len = 0usize;
        assert_eq!(
            corvid_query_min(query(coll3), t, t_len, &mut out),
            CORVID_OK
        );
        // SAFETY: text_ref borrows the owned min value; read before free.
        let min_ptr = crate::value::corvid_value_text_ref(out, &mut text_len);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(min_ptr as *const u8, text_len) },
            b"Apple"
        );
        corvid_value_free(out);
        assert_eq!(
            corvid_query_max(query(coll3), t, t_len, &mut out),
            CORVID_OK
        );
        // SAFETY: same provenance for the max.
        let max_ptr = crate::value::corvid_value_text_ref(out, &mut text_len);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(max_ptr as *const u8, text_len) },
            b"cherry"
        );
        corvid_value_free(out);

        // ±inf are comparable extrema.
        insert(
            coll3,
            b"i1",
            doc(&[("x", corvid_value_float(f64::INFINITY))]),
        );
        insert(
            coll3,
            b"i2",
            doc(&[("x", corvid_value_float(f64::NEG_INFINITY))]),
        );
        let (x, x_len) = s("x");
        assert_eq!(
            corvid_query_min(query(coll3), x, x_len, &mut out),
            CORVID_OK
        );
        assert_eq!(corvid_value_as_float(out, &mut ok), f64::NEG_INFINITY);
        corvid_value_free(out);

        // Incomparable-only field (bools): absence, a SUCCESS with NULL.
        insert(
            coll3,
            b"b1",
            doc(&[("flag", crate::value::corvid_value_bool(1))]),
        );
        let (flag, flag_len) = s("flag");
        out = std::ptr::dangling_mut();
        assert_eq!(
            corvid_query_min(query(coll3), flag, flag_len, &mut out),
            CORVID_OK
        );
        assert!(
            out.is_null(),
            "no comparable value: *out == NULL (§3 optional-value)"
        );
        assert_eq!(
            corvid_query_max(query(coll3), flag, flag_len, &mut out),
            CORVID_OK
        );
        assert!(out.is_null());
        // A truly absent field: same shape.
        let (nope, nope_len) = s("nope");
        assert_eq!(
            corvid_query_min(query(coll3), nope, nope_len, &mut out),
            CORVID_OK
        );
        assert!(out.is_null());

        // The out-param is REQUIRED (spec §4.7), and NULL field/q.
        assert_eq!(
            corvid_query_min(query(coll3), t, t_len, std::ptr::null_mut()),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_query_max(query(coll3), std::ptr::null(), 0, &mut out),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_query_min(std::ptr::null_mut(), t, t_len, &mut out),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        corvid_collection_free(coll2);
        corvid_collection_free(coll3);
        assert_eq!(corvid_close(db), CORVID_OK);
        assert_eq!(corvid_close(db2), CORVID_OK);
        assert_eq!(corvid_close(db3), CORVID_OK);
    }

    /// The grouped aggregates' oracles: typed canonical keys in
    /// ascending byte order, exact sums/means per bucket, buckets with
    /// no numeric value absent, filters respected, and the groupiter
    /// cursor contract (borrowed keys, sticky exhaustion, §7 inert
    /// rule, empty-group shape).
    #[test]
    fn group_aggregates_walk_typed_keys_in_ascending_order() {
        let (db, coll) = fresh();
        // The typed-key corpus (engine pinned): five buckets.
        insert(coll, b"a", doc(&[("v", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("v", corvid_value_float(1.0))]));
        insert(coll, b"c", doc(&[("v", text_value("1"))]));
        insert(
            coll,
            b"d",
            doc(&[("v", crate::value::corvid_value_bool(1))]),
        );
        insert(coll, b"e", doc(&[("v", text_value("i:1"))]));
        insert(coll, b"f", doc(&[("v", corvid_value_int(1))]));

        let (field, field_len) = s("v");
        let iter = corvid_query_group_count(query(coll), field, field_len);
        assert!(!iter.is_null());
        let mut key: *const c_char = std::ptr::null();
        let mut key_len = 0usize;
        let mut value = f64::NAN;
        let mut walked: Vec<(Vec<u8>, f64)> = Vec::new();
        while {
            let r = corvid_groupiter_next(iter, &mut key, &mut key_len, &mut value);
            if r == 1 {
                // SAFETY: key borrows the cursor's current group, valid
                // until the next call (which the loop makes only after
                // the push).
                walked.push((
                    unsafe { std::slice::from_raw_parts(key as *const u8, key_len) }.to_vec(),
                    value,
                ));
            }
            r == 1
        } {}
        assert_eq!(
            walked,
            vec![
                (b"1".to_vec(), 1.0),
                (b"b:true".to_vec(), 1.0),
                (b"f:1".to_vec(), 1.0),
                (b"i:1".to_vec(), 2.0),
                (b"t:i:1".to_vec(), 1.0),
            ],
            "ascending group-key bytes; i:1 counted twice; the colliding text t:-escaped"
        );
        // Sticky exhaustion, out-params untouched.
        key_len = usize::MAX;
        value = f64::NAN;
        assert_eq!(
            corvid_groupiter_next(iter, &mut key, &mut key_len, &mut value),
            0
        );
        assert_eq!(key_len, usize::MAX);
        corvid_groupiter_free(iter);

        // group_sum / group_avg: exact per-bucket oracles, skips and
        // absent buckets.
        let (db2, coll2) = fresh();
        insert(
            coll2,
            b"a",
            doc(&[("g", text_value("x")), ("n", corvid_value_int(1))]),
        );
        insert(
            coll2,
            b"b",
            doc(&[("g", text_value("x")), ("n", corvid_value_float(2.5))]),
        );
        insert(
            coll2,
            b"c",
            doc(&[("g", text_value("y")), ("n", corvid_value_int(10))]),
        );
        insert(
            coll2,
            b"d",
            doc(&[("g", text_value("y")), ("n", text_value("skip"))]),
        );
        insert(coll2, b"e", doc(&[("g", text_value("z"))])); // no n at all

        let (g, g_len) = s("g");
        let (n, n_len) = s("n");
        let walk_groups = |iter: *mut corvid_groupiter| {
            let mut out = Vec::new();
            let mut key: *const c_char = std::ptr::null();
            let mut key_len = 0usize;
            let mut value = f64::NAN;
            while corvid_groupiter_next(iter, &mut key, &mut key_len, &mut value) == 1 {
                // SAFETY: borrowed until the next call, which the loop
                // makes after the push.
                out.push((
                    unsafe { std::slice::from_raw_parts(key as *const u8, key_len) }.to_vec(),
                    value,
                ));
            }
            corvid_groupiter_free(iter);
            out
        };
        let summed = walk_groups(corvid_query_group_sum(query(coll2), g, g_len, n, n_len));
        assert_eq!(
            summed,
            vec![(b"x".to_vec(), 3.5), (b"y".to_vec(), 10.0)],
            "z never materializes (no numeric n)"
        );
        let averaged = walk_groups(corvid_query_group_avg(query(coll2), g, g_len, n, n_len));
        assert_eq!(averaged, vec![(b"x".to_vec(), 1.75), (b"y".to_vec(), 10.0)]);

        // Filters narrow groups: n > 2 keeps {b: x/2.5, c: y/10}.
        let q = query(coll2);
        assert_eq!(corvid_query_filter(q, n_gt(2)), CORVID_OK);
        assert_eq!(
            walk_groups(corvid_query_group_sum(q, g, g_len, n, n_len)),
            vec![(b"x".to_vec(), 2.5), (b"y".to_vec(), 10.0)]
        );

        // An empty group set is a cursor, not a failure.
        let (absent, absent_len) = s("nope");
        let empty = corvid_query_group_count(query(coll2), absent, absent_len);
        assert!(!empty.is_null());
        let mut k2: *const c_char = std::ptr::null();
        assert_eq!(
            corvid_groupiter_next(empty, &mut k2, &mut key_len, &mut value),
            0
        );
        corvid_groupiter_free(empty);

        // §7's inert rule and the no-op free; NULL q and NULL fields.
        assert_eq!(
            corvid_groupiter_next(std::ptr::null_mut(), &mut k2, &mut key_len, &mut value),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        let iter = corvid_query_group_count(query(coll2), g, g_len);
        assert_eq!(
            corvid_groupiter_next(iter, std::ptr::null_mut(), &mut key_len, &mut value),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_groupiter_next(iter, &mut k2, std::ptr::null_mut(), &mut value),
            0
        );
        assert_eq!(
            corvid_groupiter_next(iter, &mut k2, &mut key_len, std::ptr::null_mut()),
            0
        );
        corvid_groupiter_free(iter);
        corvid_groupiter_free(std::ptr::null_mut()); // §7 no-op

        assert!(corvid_query_group_count(std::ptr::null_mut(), g, g_len).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_query_group_count(query(coll2), std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(corvid_query_group_sum(std::ptr::null_mut(), g, g_len, n, n_len).is_null());
        assert!(corvid_query_group_avg(std::ptr::null_mut(), g, g_len, n, n_len).is_null());
        assert!(corvid_query_group_avg(query(coll2), g, g_len, std::ptr::null(), 0).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        corvid_collection_free(coll2);
        assert_eq!(corvid_close(db), CORVID_OK);
        assert_eq!(corvid_close(db2), CORVID_OK);
    }
}
