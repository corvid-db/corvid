//! Indexes & schema (spec §4.10) — the 15 create/declare/inspect
//! functions.
//!
//! Every create is (or replace): re-creating an index rebuilds it, and
//! all definitions persist across reopen (the engine's registered defs).
//! The eleven creates map 1:1 onto `corvid::Collection::create_*`
//! methods (scalar.rs, fts.rs, geo_index.rs, index.rs) — the six HNSW
//! variants (in-memory / quantized / on-disk / on-disk-quantized /
//! product-quantized ×2) share query.rs's `metric_of` and the
//! `quant_of` twin below. PQ creates surface every
//! `Pq::train` domain failure — `m == 0`, `k` outside `2..=256`,
//! `dim % m != 0`, zero-dimensional or mixed-dimension training
//! vectors, and "no training vectors" — as the engine's single
//! `CORVID_E_EMPTY_INDEX_TRAINING` (spec §4.10's PQ clause).
//!
//! `set_schema` builds the engine `Schema` from a borrowed
//! `corvid_field_def` array (`Schema::new().field(Field::new(name,
//! ty).required().unique())` — schema.rs); `schema` answers absence as
//! success (`CORVID_OK` + `*out == NULL` for an undeclared collection,
//! spec §3) and otherwise materializes the fields into a
//! `corvid_schemaiter*` in declaration order.

use std::ffi::c_char;
use std::ffi::c_int;

use corvid::Quantization;
use corvid::schema::Field;
use corvid::schema::FieldType;
use corvid::schema::Schema;

use crate::error::corvid_status;
use crate::error::guard;
use crate::error::record_argument;
use crate::handle::SchemaIterHandle;
use crate::handle::borrow_coll;
use crate::handle::borrow_schemaiter_mut;
use crate::handle::corvid_coll;
use crate::handle::corvid_schemaiter;
use crate::handle::into_schemaiter;
use crate::handle::reclaim_schemaiter;
use crate::query::corvid_metric;
use crate::query::metric_of;
use crate::value::borrowed_utf8;

/// The declared type of a schema field (FFI.md §1.4, frozen per §8):
/// mirrors `corvid::schema::FieldType` (schema.rs `to_byte`, 0..8).
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_field_type {
    /// `FieldType::Any` — any value accepted.
    CORVID_FIELD_ANY = 0,
    /// `FieldType::Bool`.
    CORVID_FIELD_BOOL = 1,
    /// `FieldType::Int`.
    CORVID_FIELD_INT = 2,
    /// `FieldType::Float`.
    CORVID_FIELD_FLOAT = 3,
    /// `FieldType::Text`.
    CORVID_FIELD_TEXT = 4,
    /// `FieldType::Bytes`.
    CORVID_FIELD_BYTES = 5,
    /// `FieldType::Vector`.
    CORVID_FIELD_VECTOR = 6,
    /// `FieldType::Array`.
    CORVID_FIELD_ARRAY = 7,
    /// `FieldType::Map`.
    CORVID_FIELD_MAP = 8,
}

/// One declared schema field (spec §1.2, POD): the input shape of
/// [`corvid_set_schema`] and the output shape of
/// [`corvid_schemaiter_next`]. As an INPUT, `name` is non-NULL borrowed
/// UTF-8; as an OUTPUT it is BORROWED from the cursor — valid only
/// until the next `corvid_schemaiter_next` or
/// `corvid_schemaiter_free` on that handle.
#[repr(C)]
pub struct corvid_field_def {
    /// The field's name.
    pub name: *const c_char,
    /// Name bytes.
    pub name_len: usize,
    /// The accepted value type (§1.4).
    pub r#type: corvid_field_type,
    /// 0 or 1: the field must be present and non-null on every write.
    pub required: c_int,
    /// 0 or 1: no two documents may share this field's value.
    pub unique: c_int,
}

/// The stored-vector quantization mode (FFI.md §1.4, frozen per §8):
/// mirrors `corvid::Quantization` (quant.rs).
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_quant {
    /// Full `f32` precision (`dim * 4` bytes/vector).
    CORVID_QUANT_NONE = 0,
    /// One bit per dimension (sign), Hamming; ~32x smaller.
    CORVID_QUANT_BINARY = 1,
    /// 8-bit per-vector min+scale; ~4x smaller.
    CORVID_QUANT_SCALAR = 2,
}

/// Map an ABI quantization onto the engine's, or `None` (having recorded
/// `CORVID_E_ARGUMENT` under `context`) when the discriminant is outside
/// `NONE..=SCALAR` — the `metric_of` discipline (frozen enum, raw
/// discriminant checked; an out-of-domain integer from C is a checked
/// error, not an unspecified-match footgun).
fn quant_of(context: &str, q: u32) -> Option<Quantization> {
    match q {
        0 => Some(Quantization::None),
        1 => Some(Quantization::Binary),
        2 => Some(Quantization::Scalar),
        _ => {
            record_argument(&format!(
                "{context}: quant is outside \
                 CORVID_QUANT_NONE..=CORVID_QUANT_SCALAR"
            ));
            None
        }
    }
}

/// Map an ABI field-type discriminant onto the engine `FieldType`, or
/// `None` (having recorded `CORVID_E_ARGUMENT` under `context`) when it
/// is outside `ANY..=MAP` — the `metric_of` discipline again; the
/// spec's §4.10 error for an invalid field type value.
fn field_type_of(context: &str, t: u32) -> Option<FieldType> {
    match t {
        0 => Some(FieldType::Any),
        1 => Some(FieldType::Bool),
        2 => Some(FieldType::Int),
        3 => Some(FieldType::Float),
        4 => Some(FieldType::Text),
        5 => Some(FieldType::Bytes),
        6 => Some(FieldType::Vector),
        7 => Some(FieldType::Array),
        8 => Some(FieldType::Map),
        _ => {
            record_argument(&format!(
                "{context}: field type is outside \
                 CORVID_FIELD_ANY..=CORVID_FIELD_MAP"
            ));
            None
        }
    }
}

/// The engine `FieldType` back onto the frozen ABI discriminant
/// ([`corvid_schemaiter_next`]'s output half of the mapping — identical
/// to schema.rs `to_byte`, spelled out here because that mapping is
/// `pub(crate)` in the engine).
fn field_type_tag(ty: FieldType) -> corvid_field_type {
    match ty {
        FieldType::Any => corvid_field_type::CORVID_FIELD_ANY,
        FieldType::Bool => corvid_field_type::CORVID_FIELD_BOOL,
        FieldType::Int => corvid_field_type::CORVID_FIELD_INT,
        FieldType::Float => corvid_field_type::CORVID_FIELD_FLOAT,
        FieldType::Text => corvid_field_type::CORVID_FIELD_TEXT,
        FieldType::Bytes => corvid_field_type::CORVID_FIELD_BYTES,
        FieldType::Vector => corvid_field_type::CORVID_FIELD_VECTOR,
        FieldType::Array => corvid_field_type::CORVID_FIELD_ARRAY,
        FieldType::Map => corvid_field_type::CORVID_FIELD_MAP,
    }
}

/// The §7 NULL-checked shared coll borrow (the read.rs/mutation.rs twin,
/// local to this module like its siblings).
fn borrow_coll_checked<'a>(
    fn_name: &str,
    c: *mut corvid_coll,
) -> Option<&'a crate::handle::CollHandle> {
    if c.is_null() {
        record_argument(&format!("{fn_name}: c is NULL"));
        return None;
    }
    // SAFETY: c is non-NULL (checked) with corvid_collection provenance,
    // not yet freed; the coll family is thread-safe (spec §2), so a
    // shared borrow is fine.
    unsafe { borrow_coll(c) }
}

/// Create (or replace) a scalar secondary index on `field` (spec §4.10;
/// counterpart: `Collection::create_scalar_index`, scalar.rs) — equality
/// and range filters on `field` then use it; on disk, persists.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_scalar_index(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
) -> corvid_status {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_create_scalar_index", c),
        borrowed_utf8("corvid_create_scalar_index", "field", field, field_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_scalar_index", || {
        coll.collection().create_scalar_index(field)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) a compound index over an ordered field list (spec
/// §4.10; counterpart: `Collection::create_compound_index(&[&str])`,
/// scalar.rs) — equality on a leading prefix plus an optional range on
/// the next field use it. `fields` (and `field_lens`) may be NULL only
/// when `count == 0` (the `pred_in` array rule); each name is borrowed
/// UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_compound_index(
    c: *mut corvid_coll,
    fields: *const *const c_char,
    field_lens: *const usize,
    count: usize,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_create_compound_index", c) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(names) = utf8_array(
        "corvid_create_compound_index",
        "fields",
        fields,
        field_lens,
        count,
    ) else {
        return corvid_status::CORVID_ERR;
    };
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    match guard("corvid_create_compound_index", || {
        coll.collection().create_compound_index(&refs)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) an in-memory inverted text index on `field`
/// (spec §4.10; counterpart: `Collection::create_text_index`, fts.rs).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_text_index(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
) -> corvid_status {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_create_text_index", c),
        borrowed_utf8("corvid_create_text_index", "field", field, field_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_text_index", || {
        coll.collection().create_text_index(field)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) an **on-disk** inverted text index on `field`
/// (spec §4.10; counterpart: `Collection::create_text_index_ondisk`,
/// fts.rs) — postings stored as redb entries; existing documents
/// backfill synchronously.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_text_index_ondisk(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
) -> corvid_status {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_create_text_index_ondisk", c),
        borrowed_utf8("corvid_create_text_index_ondisk", "field", field, field_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_text_index_ondisk", || {
        coll.collection().create_text_index_ondisk(field)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) a spatial index on `field` (spec §4.10;
/// counterpart: `Collection::create_geo_index`, geo_index.rs) — serves
/// the radius/bbox windows of §4.12.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_geo_index(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
) -> corvid_status {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_create_geo_index", c),
        borrowed_utf8("corvid_create_geo_index", "field", field, field_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_geo_index", || {
        coll.collection().create_geo_index(field)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) a full-precision in-memory HNSW index on `field`
/// (spec §4.10; counterpart: `Collection::create_vector_index`,
/// index.rs).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric)) = (
        borrow_coll_checked("corvid_create_vector_index", c),
        borrowed_utf8("corvid_create_vector_index", "field", field, field_len),
        metric_of("corvid_create_vector_index", metric as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index", || {
        coll.collection().create_vector_index(field, metric)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Like [`corvid_create_vector_index`] but storing vectors quantized
/// (spec §4.10; counterpart: `Collection::create_vector_index_quantized`)
/// — binary ≈32x / scalar ≈4x smaller at some recall cost.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index_quantized(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
    quant: corvid_quant,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric), Some(quant)) = (
        borrow_coll_checked("corvid_create_vector_index_quantized", c),
        borrowed_utf8(
            "corvid_create_vector_index_quantized",
            "field",
            field,
            field_len,
        ),
        metric_of("corvid_create_vector_index_quantized", metric as u32),
        quant_of("corvid_create_vector_index_quantized", quant as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index_quantized", || {
        coll.collection()
            .create_vector_index_quantized(field, metric, quant)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) an on-disk HNSW index on `field` (spec §4.10;
/// counterpart: `Collection::create_vector_index_ondisk`) — the graph
/// lives in the database file; existing documents backfill
/// synchronously.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index_ondisk(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric)) = (
        borrow_coll_checked("corvid_create_vector_index_ondisk", c),
        borrowed_utf8(
            "corvid_create_vector_index_ondisk",
            "field",
            field,
            field_len,
        ),
        metric_of("corvid_create_vector_index_ondisk", metric as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index_ondisk", || {
        coll.collection().create_vector_index_ondisk(field, metric)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Like [`corvid_create_vector_index_ondisk`] but storing each vector
/// quantized (spec §4.10; counterpart:
/// `Collection::create_vector_index_ondisk_quantized`).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index_ondisk_quantized(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
    quant: corvid_quant,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric), Some(quant)) = (
        borrow_coll_checked("corvid_create_vector_index_ondisk_quantized", c),
        borrowed_utf8(
            "corvid_create_vector_index_ondisk_quantized",
            "field",
            field,
            field_len,
        ),
        metric_of("corvid_create_vector_index_ondisk_quantized", metric as u32),
        quant_of("corvid_create_vector_index_ondisk_quantized", quant as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index_ondisk_quantized", || {
        coll.collection()
            .create_vector_index_ondisk_quantized(field, metric, quant)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) an in-memory HNSW index storing
/// **product-quantized** vectors (spec §4.10; counterpart:
/// `Collection::create_vector_index_pq(field, metric, m, k)`, index.rs —
/// arity verified): a codebook of `m` subspaces × `k` centroids trains
/// deterministically from existing vectors; `dim % m == 0` required.
/// Every `Pq::train` domain failure — `m == 0`, `k` outside `2..=256`,
/// `dim % m != 0`, zero-dimensional or mixed-dimension training
/// vectors, or no usable training vectors — surfaces as the engine's
/// single `CORVID_E_EMPTY_INDEX_TRAINING`.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index_pq(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
    m: usize,
    k: usize,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric)) = (
        borrow_coll_checked("corvid_create_vector_index_pq", c),
        borrowed_utf8("corvid_create_vector_index_pq", "field", field, field_len),
        metric_of("corvid_create_vector_index_pq", metric as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index_pq", || {
        coll.collection()
            .create_vector_index_pq(field, metric, m, k)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Create (or replace) an on-disk HNSW index storing product-quantized
/// vectors (spec §4.10; counterpart:
/// `Collection::create_vector_index_ondisk_pq(field, metric, m, k)`,
/// index.rs — same arity and the same `EmptyIndexTraining` domain
/// contract as [`corvid_create_vector_index_pq`]).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_create_vector_index_ondisk_pq(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    metric: corvid_metric,
    m: usize,
    k: usize,
) -> corvid_status {
    let (Some(coll), Some(field), Some(metric)) = (
        borrow_coll_checked("corvid_create_vector_index_ondisk_pq", c),
        borrowed_utf8(
            "corvid_create_vector_index_ondisk_pq",
            "field",
            field,
            field_len,
        ),
        metric_of("corvid_create_vector_index_ondisk_pq", metric as u32),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_create_vector_index_ondisk_pq", || {
        coll.collection()
            .create_vector_index_ondisk_pq(field, metric, m, k)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Declare (or replace) the collection's schema (spec §4.10;
/// counterpart: `Collection::set_schema(&Schema)`, schema.rs): enforced
/// on subsequent writes only — existing documents are not retroactively
/// validated. The engine `Schema` is built with
/// `Schema::new().field(Field::new(name, ty).required().unique())`;
/// field names are validated UTF-8 and an out-of-domain field-type
/// discriminant fails with `CORVID_E_ARGUMENT`. `fields` may be NULL
/// only when `count == 0` (the `pred_in` array rule) — an empty array
/// declares an empty schema, which accepts any map document and
/// REPLACES any previously declared one.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_set_schema(
    c: *mut corvid_coll,
    fields: *const corvid_field_def,
    count: usize,
) -> corvid_status {
    let Some(coll) = borrow_coll_checked("corvid_set_schema", c) else {
        return corvid_status::CORVID_ERR;
    };
    if fields.is_null() && count > 0 {
        record_argument(
            "corvid_set_schema: fields is NULL with count > 0 \
             (the pred_in array rule: NULL only at count 0)",
        );
        return corvid_status::CORVID_ERR;
    }
    // Validate and collect the whole array BEFORE the engine call — a
    // bad entry anywhere rejects the schema, and nothing is partially
    // declared (the engine's own transactional discipline).
    let mut schema = Schema::new();
    for i in 0..count {
        // SAFETY: fields is non-NULL (checked above) and the caller
        // guarantees count readable corvid_field_def structs (§1.2's
        // borrowed-POD contract, the corvid_kv precedent).
        let def = unsafe { &*fields.add(i) };
        let Some(name) = borrowed_utf8("corvid_set_schema", "name", def.name, def.name_len) else {
            return corvid_status::CORVID_ERR;
        };
        // SAFETY: the type field's 4 bytes read as a raw u32 — never
        // materialized as the enum — so any bit pattern a C caller may
        // have written is a checked value, not an invalid enum (the
        // fieldless #[repr(u32)] layout guarantees the view).
        let ty_bits = unsafe { (&raw const def.r#type).cast::<u32>().read() };
        let Some(ty) = field_type_of("corvid_set_schema", ty_bits) else {
            return corvid_status::CORVID_ERR;
        };
        let mut field = Field::new(name, ty);
        if def.required != 0 {
            field = field.required();
        }
        if def.unique != 0 {
            field = field.unique();
        }
        schema = schema.field(field);
    }
    match guard("corvid_set_schema", || {
        coll.collection().set_schema(&schema)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// The declared schema as a field cursor (spec §4.10; counterpart:
/// `Collection::schema() -> Option<Schema>`, infallible in Rust).
/// Absence is a success: `CORVID_OK` + `*out == NULL` when no schema is
/// declared; `CORVID_ERR` only on NULL arguments. `out` non-NULL.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_schema(
    c: *mut corvid_coll,
    out: *mut *mut corvid_schemaiter,
) -> corvid_status {
    let (Some(coll), true) = (borrow_coll_checked("corvid_schema", c), !out.is_null()) else {
        record_argument(if out.is_null() {
            "corvid_schema: out is NULL (a required out-param, §7)"
        } else {
            "corvid_schema: c is NULL"
        });
        return corvid_status::CORVID_ERR;
    };
    // Infallible engine-side (Option, not Result) — no guard needed.
    match coll.collection().schema() {
        Some(schema) => {
            // SAFETY: out is non-NULL (checked); the store happens on the
            // success path only, exactly as §4.10 documents.
            unsafe { *out = into_schemaiter(SchemaIterHandle::new(schema.fields().to_vec())) };
            corvid_status::CORVID_OK
        }
        None => {
            // Absence is a success: *out = NULL (spec §3) — and it is
            // written, so a stale caller pointer cannot masquerade as a
            // declared schema.
            // SAFETY: out is non-NULL (checked).
            unsafe { *out = std::ptr::null_mut() };
            corvid_status::CORVID_OK
        }
    }
}

/// Advance the schema cursor (spec §4.10): returns 1 and fills `*out`
/// for the next field (declaration order), 0 at exhaustion — out-params
/// untouched at 0; never errors (the list is materialized). `out->name`
/// is BORROWED until the next `corvid_schemaiter_next` or
/// `corvid_schemaiter_free` on this handle — using or freeing it after
/// either is UB.
///
/// NULL handle or NULL out-parameter follows the non-status rule (spec
/// §7): defined inert value (0 = exhausted) AND `CORVID_E_ARGUMENT`
/// recorded — never UB, and never a status return (there is none).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_schemaiter_next(
    it: *mut corvid_schemaiter,
    out: *mut corvid_field_def,
) -> c_int {
    if it.is_null() || out.is_null() {
        record_argument("corvid_schemaiter_next: NULL handle or out-param (§7 inert rule)");
        return 0;
    }
    // SAFETY: handle non-NULL (checked) with corvid_schema provenance,
    // not yet freed; the §2 contract confines a cursor to one thread, so
    // the exclusive borrow is sound.
    let cursor = unsafe { borrow_schemaiter_mut(it) }.expect("non-NULL checked above");
    match cursor.next() {
        Some(field) => {
            // SAFETY: out is non-NULL (checked); the name pointer + the
            // by-value flags fill the POD in place (§1.2).
            unsafe {
                *out = corvid_field_def {
                    name: field.name.as_ptr() as *const c_char,
                    name_len: field.name.len(),
                    r#type: field_type_tag(field.ty),
                    required: field.required as c_int,
                    unique: field.unique as c_int,
                };
            }
            1
        }
        None => 0,
    }
}

/// Free the cursor (spec §4.10). `corvid_schemaiter_free(NULL)` is a
/// no-op (spec §7). Cross-family frees are UB (spec §2).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_schemaiter_free(it: *mut corvid_schemaiter) {
    // SAFETY: NULL is the documented no-op; otherwise it is a
    // corvid_schema product, reclaimed exactly once here.
    drop(unsafe { reclaim_schemaiter(it) });
}

/// Read a `(ptr, len)` string array (shared by the compound-index field
/// list here and the admin rename pairs in admin.rs): the array pointers
/// may be NULL only at `count == 0` (the `pred_in` array rule); every
/// element is non-NULL UTF-8 (§1.5) or the whole call has failed with
/// `CORVID_E_ARGUMENT` — collected up front, nothing partially consumed.
pub(crate) fn utf8_array(
    fn_name: &str,
    param: &str,
    ptr: *const *const c_char,
    lens: *const usize,
    count: usize,
) -> Option<Vec<String>> {
    if ptr.is_null() || lens.is_null() {
        if count == 0 {
            return Some(Vec::new());
        }
        record_argument(&format!(
            "{fn_name}: {param} array is NULL with count > 0 \
             (the pred_in array rule: NULL only at count 0)"
        ));
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: ptr and lens are non-NULL (checked) and the caller
        // guarantees count readable elements in each (§1.5's
        // borrowed-array contract, the select precedent in query.rs).
        let (element, len) = unsafe { (*ptr.add(i), *lens.add(i)) };
        let text = borrowed_utf8(fn_name, param, element, len)?;
        out.push(text.to_owned());
    }
    Some(out)
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
    use crate::mutation::corvid_insert;
    use crate::pred::corvid_cmp::CORVID_CMP_EQ;
    use crate::pred::corvid_pred_compare;
    use crate::query::corvid_metric::CORVID_METRIC_COSINE;
    use crate::query::corvid_query_approx;
    use crate::query::corvid_query_limit;
    use crate::query::corvid_query_new;
    use crate::query::corvid_query_run;
    use crate::query::corvid_query_text;
    use crate::query::corvid_query_vector;
    use crate::query::corvid_rows_free;
    use crate::query::corvid_rows_next;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;
    use crate::value::corvid_value_text;
    use crate::value::corvid_value_vector;

    type Coll = *mut corvid_coll;

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
    fn doc(pairs: &[(&str, *mut crate::handle::corvid_value)]) -> *mut crate::handle::corvid_value {
        let map = corvid_value_map_new();
        for (key, item) in pairs {
            let (key, key_len) = s(key);
            assert_eq!(corvid_value_map_put(map, key, key_len, *item), CORVID_OK);
        }
        map
    }

    fn text_value(text: &str) -> *mut crate::handle::corvid_value {
        let (ptr, len) = s(text);
        corvid_value_text(ptr, len)
    }

    fn vector_value(v: &[f32]) -> *mut crate::handle::corvid_value {
        corvid_value_vector(v.as_ptr(), v.len())
    }

    fn insert(coll: Coll, key: &[u8], document: *mut crate::handle::corvid_value) {
        assert_eq!(
            corvid_insert(coll, key.as_ptr(), key.len(), document),
            CORVID_OK
        );
        corvid_value_free(document);
    }

    /// A tolerated insert (failure expected by the caller).
    fn try_insert(
        coll: Coll,
        key: &[u8],
        document: *mut crate::handle::corvid_value,
    ) -> corvid_status {
        let status = corvid_insert(coll, key.as_ptr(), key.len(), document);
        corvid_value_free(document);
        status
    }

    /// Walk a rows cursor, collecting keys in arrival order.
    fn keys_of(rows: *mut crate::handle::corvid_rows) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut key: *const u8 = std::ptr::null();
            let mut key_len = 0usize;
            let mut d: *const crate::handle::corvid_value = std::ptr::null();
            let mut score = 0f32;
            if corvid_rows_next(rows, &mut key, &mut key_len, &mut d, &mut score) != 1 {
                return out;
            }
            // SAFETY: key borrows the cursor's current row, valid until
            // the next corvid_rows_next (which this loop makes next).
            out.push(unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec());
        }
    }

    /// One input corvid_field_def.
    fn field_def(
        name: &str,
        ty: corvid_field_type,
        required: bool,
        unique: bool,
    ) -> corvid_field_def {
        let (ptr, len) = s(name);
        corvid_field_def {
            name: ptr,
            name_len: len,
            r#type: ty,
            required: required as c_int,
            unique: unique as c_int,
        }
    }

    // --- §4.10: creates ---------------------------------------------------------

    /// Scalar + compound creates succeed (and re-create = replace), and
    /// a scalar-indexed equality filter answers through the query ABI.
    /// Also the §7 NULL/array discipline for both signatures.
    #[test]
    fn scalar_and_compound_creates_drive_filtered_queries() {
        let (db, coll) = fresh();
        for (key, n) in [("a", 1), ("b", 5), ("c", 9)] {
            insert(
                coll,
                key.as_bytes(),
                doc(&[("n", corvid_value_int(n)), ("tag", text_value("x"))]),
            );
        }

        let (n, n_len) = s("n");
        assert_eq!(corvid_create_scalar_index(coll, n, n_len), CORVID_OK);
        assert_eq!(
            corvid_create_scalar_index(coll, n, n_len),
            CORVID_OK,
            "re-create replaces (spec §4.10)"
        );

        // The indexed equality filter answers through the query ABI.
        let q = corvid_query_new(coll);
        let (path, path_len) = s("n");
        let pred = corvid_pred_compare(path, path_len, CORVID_CMP_EQ, corvid_value_int(5));
        assert_eq!(crate::query::corvid_query_filter(q, pred), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_eq!(keys_of(rows), vec![b"b".to_vec()]);
        corvid_rows_free(rows);

        // Compound: an ordered field array; NULL arrays are fine at
        // count 0, an argument error above it.
        let (n2, n2_len) = s("tag");
        let names = [n, n2];
        let lens = [n_len, n2_len];
        assert_eq!(
            corvid_create_compound_index(coll, names.as_ptr(), lens.as_ptr(), 2),
            CORVID_OK
        );
        assert_eq!(
            corvid_create_compound_index(coll, std::ptr::null(), std::ptr::null(), 0),
            CORVID_OK,
            "NULL arrays at count 0 are the pred_in array rule"
        );
        assert_eq!(
            corvid_create_compound_index(coll, std::ptr::null(), std::ptr::null(), 1),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // A compound-indexed prefix query answers too (equality on the
        // leading n field).
        let q = corvid_query_new(coll);
        let pred = corvid_pred_compare(path, path_len, CORVID_CMP_EQ, corvid_value_int(9));
        assert_eq!(crate::query::corvid_query_filter(q, pred), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_eq!(keys_of(rows), vec![b"c".to_vec()]);
        corvid_rows_free(rows);

        // §7: NULL coll / NULL / non-UTF-8 field.
        assert_eq!(
            corvid_create_scalar_index(std::ptr::null_mut(), n, n_len),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_create_scalar_index(coll, std::ptr::null(), 0),
            CORVID_ERR
        );
        let bad = [0xFF_u8, 0xFE];
        assert_eq!(
            corvid_create_scalar_index(coll, bad.as_ptr() as *const c_char, bad.len()),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        // A field name with `__` is the engine's write-time name gate.
        let (dunder, dunder_len) = s("a__b");
        assert_eq!(
            corvid_create_scalar_index(coll, dunder, dunder_len),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_INVALID_NAME);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// Text indexes (both kinds) serve BM25 text queries through the
    /// query ABI.
    #[test]
    fn text_indexes_serve_text_queries() {
        let (db, coll) = fresh();
        insert(
            coll,
            b"rust",
            doc(&[("body", text_value("the rust engine compiles rust code"))]),
        );
        insert(
            coll,
            b"other",
            doc(&[("body", text_value("an unrelated body of text"))]),
        );

        for ondisk in [false, true] {
            let (field, field_len) = s("body");
            let created = if ondisk {
                corvid_create_text_index_ondisk(coll, field, field_len)
            } else {
                corvid_create_text_index(coll, field, field_len)
            };
            assert_eq!(created, CORVID_OK, "{ondisk} create");
            let q = corvid_query_new(coll);
            let (needle, needle_len) = s("rust");
            assert_eq!(
                corvid_query_text(q, field, field_len, needle, needle_len, 1),
                CORVID_OK
            );
            assert_eq!(corvid_query_approx(q), CORVID_OK); // inert for text
            let rows = corvid_query_run(q);
            assert_eq!(
                keys_of(rows),
                vec![b"rust".to_vec()],
                "{ondisk}: the rust doc tops the BM25 ranking"
            );
            corvid_rows_free(rows);
        }

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The geo index create succeeds and a windowed query still answers
    /// bit-identically (the acceleration is invisible by contract).
    #[test]
    fn geo_index_create_keeps_queries_exact() {
        use crate::geo::corvid_geo_within_radius;
        use crate::value::corvid_value_array_new;
        use crate::value::corvid_value_array_push;
        use crate::value::corvid_value_float;

        let (db, coll) = fresh();
        // A `[lat, lon]` array point (the engine's extract_point shape).
        let point = corvid_value_array_new();
        assert_eq!(
            corvid_value_array_push(point, corvid_value_float(52.5)),
            CORVID_OK
        );
        assert_eq!(
            corvid_value_array_push(point, corvid_value_float(13.4)),
            CORVID_OK
        );
        insert(coll, b"home", doc(&[("loc", point)]));
        let (field, field_len) = s("loc");
        assert_eq!(corvid_create_geo_index(coll, field, field_len), CORVID_OK);

        let hits = corvid_geo_within_radius(coll, field, field_len, 52.5, 13.4, 1.0);
        assert!(!hits.is_null(), "the indexed window still answers");
        crate::geo::corvid_geohits_free(hits);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The four non-PQ HNSW variants create OK and the vector+approx
    /// query path answers through the query ABI (the T5 fns).
    #[test]
    fn vector_index_variants_drive_approx_queries() {
        let (db, coll) = fresh();
        insert(coll, b"a", doc(&[("v", vector_value(&[1.0, 0.0]))]));
        insert(coll, b"b", doc(&[("v", vector_value(&[0.0, 1.0]))]));
        insert(coll, b"c", doc(&[("v", vector_value(&[-1.0, 0.0]))]));

        let (field, field_len) = s("v");
        assert_eq!(
            corvid_create_vector_index(coll, field, field_len, CORVID_METRIC_COSINE),
            CORVID_OK
        );
        assert_eq!(
            corvid_create_vector_index_quantized(
                coll,
                field,
                field_len,
                CORVID_METRIC_COSINE,
                corvid_quant::CORVID_QUANT_BINARY
            ),
            CORVID_OK,
            "a later create replaces the earlier def"
        );
        assert_eq!(
            corvid_create_vector_index_ondisk(coll, field, field_len, CORVID_METRIC_COSINE),
            CORVID_OK
        );
        assert_eq!(
            corvid_create_vector_index_ondisk_quantized(
                coll,
                field,
                field_len,
                CORVID_METRIC_COSINE,
                corvid_quant::CORVID_QUANT_SCALAR
            ),
            CORVID_OK
        );

        // Vector source + approx + limit through the query ABI: cosine
        // ranks [1,0] first, [-1,0] (the opposite) not in the top 2.
        let q = corvid_query_new(coll);
        assert_eq!(
            corvid_query_vector(
                q,
                field,
                field_len,
                [1.0f32, 0.0].as_ptr(),
                2,
                2,
                CORVID_METRIC_COSINE
            ),
            CORVID_OK
        );
        assert_eq!(corvid_query_approx(q), CORVID_OK);
        assert_eq!(corvid_query_limit(q, 2), CORVID_OK);
        let rows = corvid_query_run(q);
        assert_eq!(keys_of(rows), vec![b"a".to_vec(), b"b".to_vec()]);
        corvid_rows_free(rows);

        // Out-of-domain metric and quant are checked E_ARGUMENTs naming
        // the rejecter.
        assert!(metric_of("corvid_create_vector_index", 7).is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert!(quant_of("corvid_create_vector_index_quantized", 3).is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            quant_of("corvid_create_vector_index_quantized", 0),
            Some(Quantization::None)
        );
        assert_eq!(
            quant_of("corvid_create_vector_index_quantized", 1),
            Some(Quantization::Binary)
        );
        assert_eq!(
            quant_of("corvid_create_vector_index_quantized", 2),
            Some(Quantization::Scalar)
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The PQ clause (spec §4.10): every `Pq::train` domain failure and
    /// the no-training-vectors case surface as the engine's single
    /// `CORVID_E_EMPTY_INDEX_TRAINING`; a well-formed create succeeds.
    #[test]
    fn pq_domain_errors_fold_into_empty_index_training() {
        let (db, coll) = fresh();
        for (i, v) in [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.9, 0.1, 0.0, 0.0],
            [0.1, 0.9, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.5, 0.5, 0.5, 0.5],
            [1.0, 1.0, 1.0, 1.0],
        ]
        .into_iter()
        .enumerate()
        {
            insert(
                coll,
                format!("v{i}").as_bytes(),
                doc(&[("e", vector_value(&v))]),
            );
        }
        let (field, field_len) = s("e");

        // No training vectors at all (a different, empty field).
        let (absent, absent_len) = s("nope");
        assert_eq!(
            corvid_create_vector_index_pq(coll, absent, absent_len, CORVID_METRIC_COSINE, 2, 2),
            CORVID_ERR
        );
        assert_eq!(
            last_code(),
            corvid_err::CORVID_E_EMPTY_INDEX_TRAINING,
            "no usable training vectors"
        );

        // m == 0.
        assert_eq!(
            corvid_create_vector_index_pq(coll, field, field_len, CORVID_METRIC_COSINE, 0, 2),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_EMPTY_INDEX_TRAINING);

        // k below 2 (and above 256).
        for k in [1usize, 257] {
            assert_eq!(
                corvid_create_vector_index_pq(coll, field, field_len, CORVID_METRIC_COSINE, 2, k),
                CORVID_ERR,
                "k = {k}"
            );
            assert_eq!(last_code(), corvid_err::CORVID_E_EMPTY_INDEX_TRAINING);
        }

        // dim % m != 0 (dim 4, m 3).
        assert_eq!(
            corvid_create_vector_index_pq(coll, field, field_len, CORVID_METRIC_COSINE, 3, 2),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_EMPTY_INDEX_TRAINING);

        // Well-formed: dim 4, m 2, k 2 (and the on-disk twin).
        assert_eq!(
            corvid_create_vector_index_pq(coll, field, field_len, CORVID_METRIC_COSINE, 2, 2),
            CORVID_OK
        );
        assert_eq!(
            corvid_create_vector_index_ondisk_pq(
                coll,
                field,
                field_len,
                CORVID_METRIC_COSINE,
                2,
                2
            ),
            CORVID_OK
        );

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    // --- §4.10: schema ----------------------------------------------------------

    /// The schema round-trip: set → schema → schemaiter walks the field
    /// defs equal, in declaration order; absence is *out = NULL; an
    /// empty array REPLACES with an empty schema.
    #[test]
    fn schema_round_trips_through_the_iterator() {
        let (db, coll) = fresh();

        // Undeclared: OK + NULL (spec §3 absence-as-success).
        let mut it: *mut corvid_schemaiter = std::ptr::null_mut();
        assert_eq!(corvid_schema(coll, &mut it), CORVID_OK);
        assert!(it.is_null(), "no schema declared → *out = NULL");

        // Declare three fields with distinct shapes.
        let defs = [
            field_def("name", corvid_field_type::CORVID_FIELD_TEXT, true, false),
            field_def("age", corvid_field_type::CORVID_FIELD_INT, false, false),
            field_def("email", corvid_field_type::CORVID_FIELD_TEXT, false, true),
        ];
        assert_eq!(corvid_set_schema(coll, defs.as_ptr(), 3), CORVID_OK);

        assert_eq!(corvid_schema(coll, &mut it), CORVID_OK);
        assert!(!it.is_null());
        let mut out = corvid_field_def {
            name: std::ptr::null(),
            name_len: usize::MAX,
            r#type: corvid_field_type::CORVID_FIELD_ANY,
            required: -1,
            unique: -1,
        };
        for (i, want) in defs.iter().enumerate() {
            assert_eq!(corvid_schemaiter_next(it, &mut out), 1, "field {i}");
            // SAFETY: out.name borrows the cursor's current field, read
            // before the next next() invalidates it.
            let name = unsafe { std::slice::from_raw_parts(out.name as *const u8, out.name_len) };
            assert_eq!(name, unsafe {
                std::slice::from_raw_parts(want.name as *const u8, want.name_len)
            });
            assert_eq!(out.r#type as u32, want.r#type as u32);
            assert_eq!(out.required, want.required);
            assert_eq!(out.unique, want.unique);
        }
        // Exhaustion: 0, out-params untouched.
        out.name_len = usize::MAX;
        assert_eq!(corvid_schemaiter_next(it, &mut out), 0);
        assert_eq!(out.name_len, usize::MAX, "exhaustion leaves out untouched");
        corvid_schemaiter_free(it);

        // Every ABI discriminant maps to the engine FieldType and back
        // (the §1.4 frozen table).
        for (bits, _) in [
            (0u32, ()),
            (1, ()),
            (2, ()),
            (3, ()),
            (4, ()),
            (5, ()),
            (6, ()),
            (7, ()),
            (8, ()),
        ] {
            let ty = field_type_of("test", bits).expect("0..=8 all valid");
            assert_eq!(field_type_tag(ty) as u32, bits);
        }
        assert!(field_type_of("test", 9).is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // An empty array replaces the schema with an empty one: the
        // cursor exists and is immediately exhausted (NOT absence).
        assert_eq!(corvid_set_schema(coll, std::ptr::null(), 0), CORVID_OK);
        assert_eq!(corvid_schema(coll, &mut it), CORVID_OK);
        assert!(!it.is_null(), "an empty schema is declared, not absent");
        assert_eq!(corvid_schemaiter_next(it, &mut out), 0);
        corvid_schemaiter_free(it);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// set_schema enforcement through the ABI: a unique field rejects a
    /// duplicate with `CORVID_E_SCHEMA_VIOLATION` (and nothing is
    /// stored); a required field rejects absence; the input discipline
    /// (NULL name, out-of-domain type, `__` name) is `CORVID_E_ARGUMENT`
    /// / `CORVID_E_INVALID_NAME`.
    #[test]
    fn schema_enforcement_pins_unique_and_required() {
        let (db, coll) = fresh();
        let defs = [
            field_def("name", corvid_field_type::CORVID_FIELD_TEXT, true, false),
            field_def("email", corvid_field_type::CORVID_FIELD_TEXT, false, true),
        ];
        assert_eq!(corvid_set_schema(coll, defs.as_ptr(), 2), CORVID_OK);

        // A conforming doc.
        assert_eq!(
            try_insert(
                coll,
                b"u1",
                doc(&[("name", text_value("rocky")), ("email", text_value("a@x"))])
            ),
            CORVID_OK
        );

        // Unique violation: same email under another key.
        assert_eq!(
            try_insert(
                coll,
                b"u2",
                doc(&[("name", text_value("zoe")), ("email", text_value("a@x"))])
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_SCHEMA_VIOLATION);
        let mut n = 0usize;
        assert_eq!(crate::read::corvid_len(coll, &mut n), CORVID_OK);
        assert_eq!(n, 1, "the violating write stored nothing");

        // A distinct email is fine.
        assert_eq!(
            try_insert(
                coll,
                b"u2",
                doc(&[("name", text_value("zoe")), ("email", text_value("b@x"))])
            ),
            CORVID_OK
        );

        // Required violation: name absent.
        assert_eq!(
            try_insert(coll, b"u3", doc(&[("email", text_value("c@x"))])),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_SCHEMA_VIOLATION);

        // Input discipline.
        let mut bad_null_name = field_def("ok", corvid_field_type::CORVID_FIELD_ANY, false, false);
        bad_null_name.name = std::ptr::null();
        assert_eq!(corvid_set_schema(coll, &bad_null_name, 1), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        let mut bad_type = field_def("ok", corvid_field_type::CORVID_FIELD_ANY, false, false);
        // SAFETY: the fieldless #[repr(u32)] enum's raw-discriminant
        // write mirrors what a C caller can pass; the FFI reads it as a
        // u32 (never as the enum), so the invalid value is checked, not
        // materialized.
        unsafe { (&raw mut bad_type.r#type).cast::<u32>().write(9) };
        assert_eq!(corvid_set_schema(coll, &bad_type, 1), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        let dunder = field_def("a__b", corvid_field_type::CORVID_FIELD_ANY, false, false);
        assert_eq!(corvid_set_schema(coll, &dunder, 1), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_INVALID_NAME);

        assert_eq!(
            corvid_set_schema(std::ptr::null_mut(), defs.as_ptr(), 2),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The cursor family's §7 non-status rule and no-op frees.
    #[test]
    fn schema_family_null_discipline() {
        let (db, coll) = fresh();
        let defs = [field_def(
            "n",
            corvid_field_type::CORVID_FIELD_INT,
            false,
            false,
        )];
        assert_eq!(corvid_set_schema(coll, defs.as_ptr(), 1), CORVID_OK);
        let mut probe = field_def("probe", corvid_field_type::CORVID_FIELD_ANY, false, false);

        // corvid_schema: NULL coll, NULL out.
        let mut it: *mut corvid_schemaiter = std::ptr::null_mut();
        assert_eq!(corvid_schema(std::ptr::null_mut(), &mut it), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(
            corvid_schema(coll, std::ptr::null_mut()),
            CORVID_ERR,
            "out is a required out-param"
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // schemaiter_next: NULL handle / NULL out → 0 + E_ARGUMENT.
        assert_eq!(corvid_schemaiter_next(std::ptr::null_mut(), &mut probe), 0);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(corvid_schema(coll, &mut it), CORVID_OK);
        assert!(!it.is_null());
        let mut out = field_def("n", corvid_field_type::CORVID_FIELD_INT, false, false);
        assert_eq!(corvid_schemaiter_next(it, std::ptr::null_mut()), 0);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(corvid_schemaiter_next(it, &mut out), 1);
        corvid_schemaiter_free(it);
        corvid_schemaiter_free(std::ptr::null_mut()); // §7 no-op

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }
}
