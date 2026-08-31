# corvid FFI — the C ABI specification

Version: FFI_VERSION = 1 · Status: LOCKED contract (Phase 0, Task 1 of the
corvid-ffi plan, 2026-08-31) · Artifact: the `corvid` cdylib + generated
`corvid.h`.

This document is **the contract**: `crates/corvid-ffi` implements it
(Phase-0 Tasks 2–8), the C-surface radar asserts it (122/122 symbols), and
every binding repo (C, Node, JS, Go, JVM, Dart, PHP, Python) codes against
it. Every signature below was written against the real engine sources —
each function cites the Rust item it wraps. Where a C function has **no
direct engine counterpart**, that is stated explicitly.

## 0. Locked rulings (restated from the approved plan)

1. **No SQL, no JSON, no serialization anywhere in the runtime path.** The
   ABI is typed C function calls end to end. `corvid-mcp` keeps JSON only
   because JSON-RPC is the MCP spec; the FFI never touches it.
2. **Typed calls end to end.** Documents are built and read through
   `corvid_value` handles; there is no parse step, no string-formatted
   query, and no byte-blob document interface on the hot path.
3. **Bindings expose idiomatic OOP; FFI symbols never leak into a
   binding's public API.** Handles become native classes, iterators become
   the language's native iteration protocol, `CORVID_ERR` becomes native
   exceptions, handle destructors map to the language's dispose pattern.
   v1 bindings are synchronous (the engine is sync).

## 1. Types and calling conventions

All functions use the C ABI (`extern "C"`, `#[no_mangle]`), the platform's
default cdecl/System V convention, and are **synchronous**. All symbols are
prefixed `corvid_`.

### 1.1 Opaque handles (10 types)

Every handle is an opaque, single-pointer-sized forward-declared struct:

```c
typedef struct corvid_db        corvid_db;
typedef struct corvid_coll      corvid_coll;
typedef struct corvid_value     corvid_value;
typedef struct corvid_pred      corvid_pred;
typedef struct corvid_query     corvid_query;
typedef struct corvid_rows      corvid_rows;
typedef struct corvid_strs      corvid_strs;
typedef struct corvid_geohits   corvid_geohits;
typedef struct corvid_groupiter corvid_groupiter;
typedef struct corvid_schemaiter corvid_schemaiter;
```

### 1.2 POD structs

```c
/* One (key, value) pair for bulk inserts (corvid_put_many). */
typedef struct corvid_kv {
    const uint8_t    *key;     /* non-NULL; may point at empty (len 0) */
    size_t            key_len; /* bytes */
    const corvid_value *val;   /* non-NULL; CLONED by the call, caller keeps ownership */
} corvid_kv;

/* One declared schema field (corvid_set_schema input, corvid_schemaiter_next output). */
typedef struct corvid_field_def {
    const char       *name;    /* non-NULL for inputs; BORROWED when filled by schemaiter_next */
    corvid_field_type type;    /* see §1.4 */
    int               required; /* 0 or 1 */
    int               unique;   /* 0 or 1 */
} corvid_field_def;

/* One geospatial / weighted hit (corvid_geohits_next output). */
typedef struct corvid_geohit {
    const uint8_t *key;        /* BORROWED until the next geohits_next or geohits_free */
    size_t         key_len;
    double         distance_km; /* geo: km from the query point;
                                   neighbors_weighted: the edge weight;
                                   geo_within_bbox: 0.0 sentinel (no center). */
} corvid_geohit;
```

### 1.3 Status and error enums (explicit values — frozen, §8)

```c
typedef enum corvid_status {
    CORVID_OK  = 0,  /* success */
    CORVID_ERR = 1   /* failure; detail in corvid_last_error_code/message */
} corvid_status;

/* Detailed codes returned by corvid_last_error_code(). Value 0 means
   "no error recorded on this thread". Codes 1..18 map 1:1 onto the
   engine's corvid::Error variants (pinned by the variant-inventory
   snapshot test, §1.3); code 19 is FFI-only. NEVER renumber. */
typedef enum corvid_err {
    CORVID_E_OK                  = 0,  /* no error */
    CORVID_E_DATABASE            = 1,  /* corvid::Error::Database — opening/creating the file failed */
    CORVID_E_TRANSACTION         = 2,  /* corvid::Error::Transaction — beginning a read/write txn failed */
    CORVID_E_TABLE               = 3,  /* corvid::Error::Table — opening a storage table failed */
    CORVID_E_STORAGE             = 4,  /* corvid::Error::Storage — a storage read/write failed */
    CORVID_E_COMMIT              = 5,  /* corvid::Error::Commit — committing a write txn failed */
    CORVID_E_SET_DURABILITY      = 6,  /* corvid::Error::SetDurability — changing txn durability failed */
    CORVID_E_COMPACTION          = 7,  /* corvid::Error::Compaction — compacting the file failed */
    CORVID_E_DECODE              = 8,  /* corvid::Error::Decode — stored bytes are not a decodable Value */
    CORVID_E_CORRUPT_INDEX       = 9,  /* corvid::Error::CorruptIndex — persisted index state is corrupt */
    CORVID_E_RESERVED_COLLECTION = 10, /* corvid::Error::ReservedCollection — name uses the `__` prefix */
    CORVID_E_INVALID_NAME        = 11, /* corvid::Error::InvalidName — name has a NUL byte or interior `__` */
    CORVID_E_ARGUMENT            = 12, /* corvid::Error::InvalidArgument — argument outside its domain
                                          (RRF k, MMR lambda, geo bounds) AND the FFI's own NULL/UTF-8
                                          discipline (§7) */
    CORVID_E_INCOMPATIBLE_FORMAT = 13, /* corvid::Error::IncompatibleFormat — file is a foreign format version */
    CORVID_E_EMPTY_INDEX_TRAINING= 14, /* corvid::Error::EmptyIndexTraining — PQ create with no training vectors */
    CORVID_E_SCHEMA_VIOLATION    = 15, /* corvid::Error::SchemaViolation — write violates the declared schema */
    CORVID_E_INVALID_DUMP        = 16, /* corvid::Error::InvalidDump — malformed / unknown-version dump stream */
    CORVID_E_BACKUP_TARGET_EXISTS= 17, /* corvid::Error::BackupTargetExists — backup path already exists */
    CORVID_E_IO                  = 18, /* corvid::Error::Io — I/O error (dump/load paths, files) */
    CORVID_E_BUSY                = 19  /* FFI-ONLY: corvid_compact while derived handles are still
                                          open (engine Db::compact needs &mut self; see §4.13).
                                          No engine variant. */
} corvid_err;
```

The engine's `corvid::Error` (crates/corvid/src/error.rs) currently has
exactly 18 variants and is `#[non_exhaustive]` (the correct
published-crate posture — kept), so a downstream compile-time exhaustive
`match` is impossible by design. The mapping is instead pinned by a
**variant-inventory snapshot test** in `corvid-ffi`: a `const` array of
the 18 variant names, and a test that (i) matches every engine variant
with a wildcard arm asserting the variant is present in the inventory,
and (ii) asserts the inventory equals the mapping table above. Adding,
removing, or renaming an engine variant fails the FFI test suite until
the mapping is maintained — the same enforcement a plain exhaustive
match would give, without requiring the engine to drop
`#[non_exhaustive]`. `CORVID_E_BUSY` is the one code with no engine
source.

### 1.4 Domain enums (explicit values — frozen)

Values mirror the engine's on-disk/in-memory discriminants where they exist.

```c
typedef enum corvid_cmp {
    CORVID_CMP_EQ = 0, /* equal (numeric Int/Float interop, else structural) */
    CORVID_CMP_NE = 1, /* not equal */
    CORVID_CMP_LT = 2, /* less than (numbers/text only) */
    CORVID_CMP_LE = 3, /* less or equal */
    CORVID_CMP_GT = 4, /* greater than */
    CORVID_CMP_GE = 5  /* greater or equal */
} corvid_cmp;           /* mirrors corvid::CmpOp (filter.rs) */

typedef enum corvid_metric {
    CORVID_METRIC_COSINE = 0, /* 1 - cos_sim, [0,2]; zero-norm = maximally distant */
    CORVID_METRIC_DOT    = 1, /* negated dot product (larger dot sorts first) */
    CORVID_METRIC_L2     = 2  /* squared Euclidean (monotonic with L2) */
} corvid_metric;               /* mirrors corvid::Metric (distance.rs) */

typedef enum corvid_quant {
    CORVID_QUANT_NONE   = 0, /* full f32 precision (dim*4 bytes/vector) */
    CORVID_QUANT_BINARY = 1, /* 1 bit/dim (sign), Hamming; ~32x smaller */
    CORVID_QUANT_SCALAR = 2  /* 8-bit per-vector min+scale; ~4x smaller */
} corvid_quant;                /* mirrors corvid::Quantization (quant.rs) */

typedef enum corvid_value_type {
    CORVID_TYPE_NULL   = 0, /* Value::Null  — absence of a value */
    CORVID_TYPE_BOOL   = 1, /* Value::Bool  — 0/1 */
    CORVID_TYPE_INT    = 2, /* Value::Int   — 64-bit signed; exact to 2^53 vs Float */
    CORVID_TYPE_FLOAT  = 3, /* Value::Float — 64-bit IEEE (NaN/±inf/-0.0 preserved) */
    CORVID_TYPE_TEXT   = 4, /* Value::Text  — UTF-8 bytes */
    CORVID_TYPE_BYTES  = 5, /* Value::Bytes — opaque bytes */
    CORVID_TYPE_ARRAY  = 6, /* Value::Array — ordered list */
    CORVID_TYPE_MAP    = 7, /* Value::Map   — string-keyed map; documents are Maps */
    CORVID_TYPE_VECTOR = 8  /* Value::Vector — dense f32 embedding */
} corvid_value_type;          /* tags 0..8, identical to value.rs's encoding tags */

typedef enum corvid_field_type {
    CORVID_FIELD_ANY    = 0, /* FieldType::Any — any value accepted */
    CORVID_FIELD_BOOL   = 1,
    CORVID_FIELD_INT    = 2,
    CORVID_FIELD_FLOAT  = 3,
    CORVID_FIELD_TEXT   = 4,
    CORVID_FIELD_BYTES  = 5,
    CORVID_FIELD_VECTOR = 6,
    CORVID_FIELD_ARRAY  = 7,
    CORVID_FIELD_MAP    = 8
} corvid_field_type;          /* mirrors schema.rs FieldType::to_byte (0..8) */
```

### 1.5 Strings, keys, and lengths

- Strings and keys cross the ABI as **pointer + length** pairs and are
  **binary-safe, NOT NUL-terminated** (keys may contain any byte except
  that names/paths must be UTF-8, see below). Empty is expressed as a
  non-NULL pointer with length 0.
- Engine string parameters (collection names, field paths, text values,
  relations, paths on disk) are Rust `&str`/`String`: the bytes must be
  **valid UTF-8**, or the call fails with `CORVID_E_ARGUMENT` (never UB).
  Keys and `Bytes` payloads may be arbitrary bytes.
- Name rules are the engine's (db.rs `validate_name` / `ensure_writable`):
  no NUL byte, no `__` sequence anywhere (`CORVID_E_INVALID_NAME`), and no
  leading `__` (`CORVID_E_RESERVED_COLLECTION`). The empty name is legal.
  Violations surface on the first write/definition call, exactly as in
  Rust — the engine does not validate on handle creation.

### 1.6 Callbacks

```c
/* corvid_scan row sink. ctx is passed through opaque.
   Return 1 to continue, 0 to stop the scan (stopping is not an error).
   key and doc are BORROWED and valid only inside the callback. */
typedef int (*corvid_scan_fn)(void *ctx,
                              const uint8_t *key, size_t key_len,
                              const corvid_value *doc);

/* corvid_update read-modify-write closure.
   current is NULL when the key is absent (a missing document is not an
   error); it is BORROWED and valid only inside the callback.
   On success set *out to an OWNED corvid_value* (consumed by the call) or
   leave it NULL to delete the key. Return CORVID_OK to apply, any other
   value to abort (then *out must be NULL — nothing is consumed). */
typedef corvid_status (*corvid_update_fn)(void *ctx,
                                          const corvid_value *current,
                                          corvid_value **out);
```

**Reentrancy (both callbacks):** callbacks run on the caller's thread
between engine operations. Reads through other handles are memory-safe,
but callbacks MUST NOT issue further writes to the same database, MUST NOT
free or mutate the borrowed arguments, and SHOULD NOT make other corvid
calls at all — bindings expose them as ordinary closures, and the portable
contract is "no reentrant corvid calls". `corvid_update` itself is
get-then-write and is documented (db.rs) as **not linearizable** against
concurrent writers to the same key; use `corvid_compare_and_set` when that
matters.

## 2. Handles: backing, lifecycle, thread contract

| Handle | Backed by (Rust) | Thread contract | Created by | Freed by |
|---|---|---|---|---|
| `corvid_db*` | `Arc<corvid::Db>` | **thread-safe**: concurrent reads from many threads; writes serialized by the engine | `corvid_open`, `corvid_open_memory` | `corvid_close` |
| `corvid_coll*` | `Arc<Db>` + collection name | **thread-safe** (shares the `Arc<Db>`) | `corvid_collection` | `corvid_collection_free` |
| `corvid_value*` | `corvid::Value` | builder handles are **single-threaded**; borrowed children ride the parent's lifetime | any `corvid_value_*` constructor, `corvid_get`, `corvid_query_min/max`, `corvid_value_clone` | `corvid_value_free` (owned values only) |
| `corvid_pred*` | `corvid::filter::Predicate` tree | **single-threaded** construction | the 10 `corvid_pred_*` constructors | `corvid_pred_free` (never-consumed roots only); consumed by `and/or/not/filter/delete_where` |
| `corvid_query*` | owned QueryBuilder state (`Arc<Db>` + name + filters + sources + knobs) | **single-threaded** build | `corvid_query_new` | `corvid_query_run` and every aggregate (CONSUME); `corvid_query_free` for abandoned builders |
| `corvid_rows*` | materialized `Vec<corvid::ResultRow>` + cursor | read-only cursor; **single-threaded** use | `corvid_query_run`, `corvid_page` | `corvid_rows_free` |
| `corvid_strs*` | owned `Vec<String>` + cursor | read-only cursor; **single-threaded** | `corvid_collections`, `corvid_neighbors`, `corvid_in_neighbors`, `corvid_traverse` | `corvid_strs_free` |
| `corvid_geohits*` | owned `Vec<corvid::GeoHit>` (or `(key, weight)` pairs) + cursor | read-only cursor; **single-threaded** | the 3 `corvid_geo_*` fns, `corvid_neighbors_weighted` | `corvid_geohits_free` |
| `corvid_groupiter*` | owned group list (sorted by group key) + cursor | read-only cursor; **single-threaded** | `corvid_query_group_count/sum/avg` (consume the query) | `corvid_groupiter_free` |
| `corvid_schemaiter*` | owned `Vec<schema::Field>` + cursor | read-only cursor; **single-threaded** | `corvid_schema` | `corvid_schemaiter_free` |

Lifecycle notes:

- `corvid_db` holds the only strong reference after open; every
  `corvid_coll` clones the `Arc`. `corvid_close` drops the handle's
  reference — the `Db` (and its file locks) are released when the last
  derived handle is gone. `corvid_compact` requires exclusivity, checked
  by the FFI-owned derived-handle counter (§4.13).
- A `corvid_coll` keeps its `corvid_db` alive; freeing the db handle while
  collection handles live is fine (the collection keeps the engine open).
- Collections are **created lazily on first write** (engine
  `Db::collection` is infallible); `corvid_collection` therefore never
  fails for name reasons — reserved/invalid names surface at write time,
  exactly as in Rust.
- **Cross-family frees are forbidden.** Each handle has exactly one
  destructor. Passing a handle to any function of another family is
  undefined behavior (the type system cannot stop it in C).

## 3. Error handling model

- Functions report success/failure with `corvid_status` (`CORVID_OK` /
  `CORVID_ERR`), or with a NULL return where a handle/buffer was expected.
- On `CORVID_ERR` (or a NULL that signals failure), the failing detail is
  available from **thread-local** storage:
  - `corvid_last_error_code()` — one of the 19 codes (`CORVID_E_OK`=0 when
    nothing failed on this thread);
  - `corvid_last_error_message()` — the engine's human-readable
    `Display` text.
- "Optional value" results (`corvid_get`, `corvid_schema`,
  `corvid_query_min`, `corvid_query_max`) use an **out-parameter plus
  status**: the call returns `CORVID_OK` and sets `*out` to the value, or
  returns `CORVID_OK` and sets `*out = NULL` when the answer is "no such
  value" (a missing document, an undeclared schema, no comparable value) —
  absence is a success, never an error. Because successful calls do not
  clear the last error (below), absence is NEVER signalled by a bare NULL
  return; only handles/buffers whose NULL is unambiguous (open, run,
  constructors, auto-key) return NULL for failure.
- Failure signals are always paired with a freshly recorded last error: a
  `CORVID_ERR` status or a NULL return (where NULL means failure) sets
  the thread-local code and message as its first act.
- Errors never leave partial side effects that Rust would not allow: the
  engine's transactions are atomic per call (insert, batch, CAS, index
  create) — a `CORVID_ERR` from `corvid_put_many` means the whole batch
  rolled back.
- **Message lifetime rule:** the message pointer is valid until the *next
  failing corvid call on the same thread* (or thread exit). Copy it if you
  need it longer. The code is likewise overwritten by the next failure.
  Successful calls do NOT clear the last error — read it immediately
  after the failure that interests you.
- The engine never panics on user input; FFI wrappers additionally catch
  any residual panic and convert it to `CORVID_ERR` + message (defensive
  only — not part of the contract).

## 4. Function reference — all 122 symbols

Conventions used in every signature below (not repeated per function):

- `corvid_status` return unless stated otherwise.
- `(const char* s, size_t len)` = borrowed, binary-safe, must be UTF-8
  where the engine takes `&str`; `NULL` pointer with `len > 0` is
  `CORVID_E_ARGUMENT`; `NULL` with `len == 0` is treated as empty only
  where the parameter is marked *nullable*.
- `(const uint8_t* k, size_t klen)` = borrowed raw key bytes; empty legal.
- `const corvid_value*` inputs are **CLONED** — the caller keeps ownership
  (§5 rule 3).
- `size_t` everywhere for lengths/counts; `int` for booleans (0/1);
  `int64_t` for engine `i64`; `double` for engine `f64`; `float` for
  engine `f32`.

### 4.1 Lifecycle & errors (8)

```c
uint32_t corvid_ffi_version(void);
```
Returns `1` (FFI_VERSION). No engine counterpart — pure ABI versioning.
Bindings verify this before anything else.

```c
corvid_db* corvid_open(const char *path, size_t path_len);
```
Open (creating if absent) a file-backed database. `path` non-NULL,
filesystem path in platform encoding. Wraps `corvid::Db::open`.
Returns the handle, or NULL + `CORVID_E_DATABASE` /
`CORVID_E_INCOMPATIBLE_FORMAT` / `CORVID_E_IO`.
*(Erratum, 2026-08-28, Task 2 implementation: "platform encoding" above
is superseded by §1.5's universal UTF-8 rule — `corvid_open` enforces
UTF-8 and answers non-UTF-8 bytes with `CORVID_E_ARGUMENT`, exactly as
§9's non-UTF-8-path exclusion row records. Signature and error set are
unchanged.)*

```c
corvid_db* corvid_open_memory(void);
```
A purely in-memory database (no file). Wraps `corvid::Db::open_in_memory`.
Fails only on engine-internal storage errors (never in practice).

```c
corvid_status corvid_close(corvid_db *db);
```
Releases the handle's reference. No direct engine counterpart — Rust drops
`Db`; persistence is durable per-transaction, so there is no explicit
close/flush in the engine either. Freeing the db while rows/iterators from
it are live is fine (those own their data); collection handles keep the
engine alive independently.

```c
corvid_err corvid_last_error_code(void);
```
Thread-local last-error code (`CORVID_E_OK` when none). No engine
counterpart — FFI error plumbing (§3).

```c
const char* corvid_last_error_message(size_t *len_out);
```
Thread-local last-error message; NUL-terminated for convenience, `*len_out`
receives the byte length (`len_out` itself nullable). Returns **NULL when
no error is recorded on this thread**. Lifetime: until the next failing
call on this thread (§3). No engine counterpart.

```c
void corvid_free(void *ptr);
```
**The ONLY buffer deallocator in the ABI.** Frees any buffer the ABI
returned by value: `corvid_insert_auto` keys, `corvid_page`'s
`next_after` cursor. Does NOT free handles (each has its own `_free`) or
values. `corvid_free(NULL)` is a no-op. No engine counterpart.

```c
corvid_strs* corvid_collections(corvid_db *db);
```
User collection names (engine-internal `__` namespaces excluded), in name
order, as a string cursor. Wraps `corvid::Db::collections`. NULL + error
on failure. NOTE: listing does not create anything — like the engine, an
empty-but-never-written collection may not appear (collections are created
on first write).

### 4.2 Collection handles (3)

```c
corvid_coll* corvid_collection(corvid_db *db, const char *name, size_t name_len);
```
Handle to a named collection; the collection is created lazily on first
write. Wraps `corvid::Db::collection` (infallible in Rust). Returns NULL
only on NULL arguments (`CORVID_E_ARGUMENT`); reserved/invalid names are
NOT checked here — they fail at write time with
`CORVID_E_RESERVED_COLLECTION` / `CORVID_E_INVALID_NAME`, exactly as the
engine does.

```c
void corvid_collection_free(corvid_coll *coll);
```
No engine counterpart (Rust `Collection` is a copyable borrow).

```c
const char* corvid_collection_name(corvid_coll *coll, size_t *len_out);
```
The collection's name, NUL-terminated, `*len_out` set. BORROWED from the
handle: valid until `corvid_collection_free`. No direct engine
counterpart (reads the handle's stored name).

### 4.3 Value construction (11)

All constructors return an OWNED `corvid_value*` or NULL +
`CORVID_E_ARGUMENT` (NULL/misencoded input). Byte/text/vector inputs are
**CLONED** into the value — the caller retains its buffer.

```c
corvid_value* corvid_value_null(void);                      /* Value::Null */
corvid_value* corvid_value_bool(int v);                     /* Value::Bool(v != 0) */
corvid_value* corvid_value_int(int64_t v);                  /* Value::Int */
corvid_value* corvid_value_float(double v);                 /* Value::Float — NaN/±inf/-0.0 preserved bit-exact */
corvid_value* corvid_value_text(const char *s, size_t len); /* Value::Text — bytes CLONED; must be valid UTF-8 */
corvid_value* corvid_value_bytes(const uint8_t *b, size_t len); /* Value::Bytes — CLONED, arbitrary bytes */
corvid_value* corvid_value_vector(const float *v, size_t dim);  /* Value::Vector — CLONED; dim 0 legal */
```
Engine counterparts: the `corvid::Value` enum variants (value.rs).

```c
corvid_value* corvid_value_array_new(void);                 /* Value::Array(vec![]) */
corvid_status corvid_value_array_push(corvid_value *arr, corvid_value *item);
```
Append `item`, **consuming** it (ownership moves into the array; do not
free it afterwards). `arr` must be an array built by `corvid_value_array_new`
(single-threaded mutation). Engine counterpart: `Vec::push` on the
variant's payload — no named engine method; the fluent Rust API builds
literals inline. Pushing invalidates previously borrowed children of
`arr` (§5 rule 6).

```c
corvid_value* corvid_value_map_new(void);                   /* Value::Map(empty) */
corvid_status corvid_value_map_put(corvid_value *map, const char *key, size_t key_len, corvid_value *val);
```
Insert `val` under `key` (UTF-8), **consuming** `val`. A duplicate key
REPLACES the previous entry (engine `BTreeMap::insert`, last write wins;
the replaced child is dropped). Same invalidation rule as `array_push`.
Map iteration order in the engine is sorted by key — construction order
does not matter for equality or encoding.

### 4.4 Value reads (12)

```c
corvid_value_type corvid_value_type(const corvid_value *v);
```
The value's discriminant. Counterpart: the `Value` variant
(`std::mem::discriminant`). A NULL `v` follows the non-status rule (§7):
returns `CORVID_TYPE_NULL` (0) and records `CORVID_E_ARGUMENT`.

```c
int      corvid_value_as_bool(const corvid_value *v, int *ok);
int64_t  corvid_value_as_int(const corvid_value *v, int *ok);
double   corvid_value_as_float(const corvid_value *v, int *ok);
```
Typed read with an ok-flag. A wrong type sets `*ok = 0` and returns 0 —
**not an error**, mirroring `Value::as_bool/as_int/as_float` returning
`Option`. A NULL `v` or NULL `ok` follows the non-status rule (§7):
`*ok = 0`, return 0, `CORVID_E_ARGUMENT` recorded.

```c
const char*   corvid_value_text_ref(const corvid_value *v, size_t *len_out);
const uint8_t* corvid_value_bytes_ref(const corvid_value *v, size_t *len_out);
const float*  corvid_value_vector_ref(const corvid_value *v, size_t *dim_out);
```
Zero-copy BORROWED views (counterparts: `Value::as_text` / `as_bytes` /
`as_vector`). NULL when the value is of a different type — not an error;
NULL with `CORVID_E_ARGUMENT` recorded when `v` or `len_out` is NULL
(§7). The buffer is valid until the parent value is freed or mutated;
**freeing or writing through these pointers is UB.**

```c
const corvid_value* corvid_value_array_get(const corvid_value *arr, size_t index);
const corvid_value* corvid_value_map_get(const corvid_value *map, const char *key, size_t key_len);
```
BORROWED children (counterparts: `Vec` indexing and `Value::get`).
NULL when out of range / absent / parent is not that container — not an
error. Child lifetime rides the parent (§5 rule 6): **calling
`corvid_value_free` on a borrowed child is UB** (bold per plan §4.6).

```c
size_t corvid_value_len(const corvid_value *v);
```
Array items / map entries / vector dimensions / text bytes / bytes bytes;
0 for null/bool/int/float. A NULL `v` returns 0 with
`CORVID_E_ARGUMENT` recorded (§7). No single engine method — the
collection lengths (`Vec::len`, `BTreeMap::len`, `String::len`) it
reports.

```c
corvid_value* corvid_value_clone(const corvid_value *v);
```
Deep copy returning an OWNED value (counterpart: `Value::clone`,
`#[derive(Clone)]`). This is the sanctioned way to keep data observed
through a borrowed pointer (e.g. a `rows` document) beyond the parent's
lifetime.

```c
void corvid_value_free(corvid_value *v);
```
Frees an OWNED value only. **Calling it on a borrowed child (from
`_ref`, `array_get`, `map_get`, `rows_next`, `geohits_next`, callbacks,
or `corvid_value_array_push`/`map_put` inputs already consumed) is
undefined behavior.** Counterpart: Rust `Drop`.

### 4.5 Predicates (11)

Ten constructors return an OWNED `corvid_pred*` (NULL +
`CORVID_E_ARGUMENT` on bad input); the combinators **consume** their
children (§5 rule 4). Rust counterparts are the `corvid::field(path)`
fluent builders (filter.rs `FieldRef`), listed per function. Paths are
dotted and traverse nested maps (`"meta.author"`); an empty path resolves
nothing.

```c
corvid_pred* corvid_pred_exists(const char *path, size_t path_len);
```
True when the path resolves to a present value. Counterpart:
`field(path).exists()` → `Predicate::Exists`.

```c
corvid_pred* corvid_pred_compare(const char *path, size_t path_len,
                                 corvid_cmp op, const corvid_value *value);
```
Compare the path's value against a constant (CLONED). Counterpart:
`field(path).eq/ne/lt/le/gt/ge(v)` → `Predicate::Compare`. Semantics:
missing path ⇒ false; unordered kinds under ordered ops ⇒ false;
`Int`/`Float` compare numerically across kinds (exact to 2^53); NaN
compares false against everything except `NE`.

```c
corvid_pred* corvid_pred_in(const char *path, size_t path_len,
                            const corvid_value *const *values, size_t count);
```
True when the value equals any element (each CLONED). Counterpart:
`field(path).is_in([...])` → `Predicate::In`. `values` non-NULL when
`count > 0`; `count == 0` matches nothing.

```c
corvid_pred* corvid_pred_between(const char *path, size_t path_len,
                                 const corvid_value *low, const corvid_value *high);
```
Inclusive `[low, high]`. Counterpart: `field(path).between(lo, hi)` →
`Predicate::Between`. Both bounds non-NULL, CLONED.

```c
corvid_pred* corvid_pred_starts_with(const char *path, size_t path_len,
                                     const char *prefix, size_t prefix_len);
corvid_pred* corvid_pred_contains(const char *path, size_t path_len,
                                  const char *substr, size_t substr_len);
```
Text predicates; false on non-text values and missing paths.
Counterparts: `field(path).starts_with(p)` / `.contains(s)` →
`Predicate::StartsWith` / `Predicate::Contains`.

```c
corvid_pred* corvid_pred_geo_within(const char *path, size_t path_len,
                                    double lat, double lon, double radius_km);
```
Path holds a point (`[lat, lon]` array or `lat`/`lon` map) within
`radius_km` (inclusive, haversine). Counterpart:
`field(path).within_km(lat, lon, r)` → `Predicate::GeoWithin`.

```c
corvid_pred* corvid_pred_and(corvid_pred *a, corvid_pred *b);
corvid_pred* corvid_pred_or(corvid_pred *a, corvid_pred *b);
corvid_pred* corvid_pred_not(corvid_pred *a);
```
Logical combinators — **CONSUME their argument(s)** and return a new root.
Counterparts: `Predicate::and` / `Predicate::or` / `std::ops::Not`
(`Predicate::And/Or/Not`). After a combine, the children belong to the
tree.

```c
void corvid_pred_free(corvid_pred *p);
```
Frees a **never-consumed root** only. Predicates handed to
`corvid_pred_and/or/not`, `corvid_query_filter`, or
`corvid_delete_where` are consumed by that call and MUST NOT be freed
(double free = UB). Counterpart: Rust `Drop` of the tree.

### 4.6 Query builder & rows (15)

A query is built on a `corvid_query*` (single-threaded) and executed by
`corvid_query_run` or any aggregate, **either of which consumes it**
(mirroring the engine's `QueryBuilder` taking `self`). Counterpart for
the whole family: `corvid::Collection::query()` →
`corvid::QueryBuilder` (builder.rs) and its fluent methods.

```c
corvid_query* corvid_query_new(corvid_coll *coll);
```
Counterpart: `Collection::query()`. NULL only on NULL `coll`.

```c
corvid_status corvid_query_filter(corvid_query *q, corvid_pred *pred);
```
Add a filter — **CONSUMES `pred`**. Multiple calls AND together.
Counterpart: `QueryBuilder::filter(predicate)` (takes `Predicate` by
value).

```c
corvid_status corvid_query_vector(corvid_query *q, const char *field, size_t field_len,
                                  const float *query, size_t dim, size_t k,
                                  corvid_metric metric);
```
Add a vector-search source (query vector CLONED). Counterpart:
`QueryBuilder::vector(field, vec![...], k, metric)` — arity verified:
`(field, query, k, metric)`.

```c
corvid_status corvid_query_text(corvid_query *q, const char *field, size_t field_len,
                                const char *s, size_t s_len, size_t k);
```
Add a BM25 text-search source. Counterpart:
`QueryBuilder::text(field, query, k)`.

```c
corvid_status corvid_query_fuse_rrf(corvid_query *q, float k);
corvid_status corvid_query_rerank_mmr(corvid_query *q, float lambda);
```
RRF constant (engine default `corvid::DEFAULT_RRF_K` = 60) and MMR lambda
in `[0,1]`. Counterparts: `QueryBuilder::fuse_rrf(k)` /
`QueryBuilder::rerank_mmr(lambda)`. The engine validates at execution
(audit C6): non-positive/NaN `k`, or lambda outside `[0,1]`, fail
`corvid_query_run`/aggregates with `CORVID_E_ARGUMENT` — these setters
always succeed.

```c
corvid_status corvid_query_approx(corvid_query *q);
corvid_status corvid_query_limit(corvid_query *q, size_t n);
corvid_status corvid_query_offset(corvid_query *q, size_t n);
corvid_status corvid_query_order_by(corvid_query *q, const char *field, size_t field_len,
                                    int descending);
```
Counterparts: `QueryBuilder::approx` / `::limit` / `::offset` /
`::order_by(field, descending)`. `limit 0` yields an empty result;
`offset` applies after ordering, before limit. Ordering contract (audit
C4): comparable values (numbers numerically — numbers before texts across
kinds — texts lexically) first in value order; incomparable values
(bools, containers, NaN) after them; rows missing the field last; ties by
key. `descending` reverses within-class order only.

```c
corvid_status corvid_query_select(corvid_query *q, const char *const *fields,
                                  const size_t *field_lens, size_t count);
```
Project result documents to these top-level fields (missing fields are
absent; non-map documents pass through unchanged; ranking still sees the
full document). Counterpart: `QueryBuilder::select(fields)`.

```c
corvid_rows* corvid_query_run(corvid_query *q);
```
Execute — **CONSUMES `q`** (counterpart: `QueryBuilder::run(self)`).
Returns a rows cursor even for an empty result (distinguish failure by
`CORVID_ERR`); NULL + error on failure. One MVCC snapshot covers the
whole query; ranking parameters are validated here (audit C6). The
handle owns the materialized `Vec<ResultRow>`.

```c
void corvid_query_free(corvid_query *q);
```
For builders abandoned without running. **Not** for use after
`corvid_query_run`/aggregates (consumed). No engine counterpart
(Rust drops the builder).

```c
int corvid_rows_next(corvid_rows *rows,
                     const uint8_t **key_out, size_t *key_len_out,
                     const corvid_value **doc_out, float *score_out);
```
Advance: returns 1 and fills the out-params for the next row, 0 at
exhaustion (out-params untouched at 0; never errors — the result is
materialized). NULL-handle / NULL-out-param behavior follows the
non-status rule (§7): return 0 with `CORVID_E_ARGUMENT` recorded. The
key and the document are **BORROWED from the cursor: valid only until
the next `corvid_rows_next` or `corvid_rows_free` — using or freeing
them after is UB.** `score` is the fused RRF score (`f32`), `0.0` for
pure filter/order queries and for `corvid_page` rows. No direct engine
counterpart — the cursor walks the `Vec<ResultRow>` that
`QueryBuilder::run` returned.

```c
void corvid_rows_free(corvid_rows *rows);
```
Counterpart: dropping the `Vec<ResultRow>`.

### 4.7 Aggregations (11)

Every aggregate **consumes the query** (engine methods take `self`) and
executes on one read snapshot, over the filtered set — retrieval sources,
ranking, limit/offset/select are ignored. Counterparts:
`QueryBuilder::count` etc. (builder.rs).

```c
corvid_status corvid_query_count(corvid_query *q, size_t *out);
```
`QueryBuilder::count() -> usize`. O(1) when unfiltered (maintained
counter).

```c
corvid_status corvid_query_count_distinct(corvid_query *q,
                                          const char *field, size_t field_len,
                                          size_t *out);
```
Distinct values at `field` by the canonical group key (text bare;
int/float/bool type-tagged so distinct kinds stay distinct; missing and
container values ignored). `QueryBuilder::count_distinct(field)`.

```c
corvid_status corvid_query_sum(corvid_query *q, const char *field, size_t field_len,
                               double *out);
corvid_status corvid_query_avg(corvid_query *q, const char *field, size_t field_len,
                               double *out, int *has_value);
```
Sum / mean of numeric (`int`/`float`) values; missing or non-numeric are
skipped. `avg` sets `*has_value = 0` when there were no numeric values
(counterpart returns `Option<f64>`). `QueryBuilder::sum` /
`QueryBuilder::avg`.

```c
corvid_status corvid_query_min(corvid_query *q, const char *field, size_t field_len,
                               corvid_value **out);
corvid_status corvid_query_max(corvid_query *q, const char *field, size_t field_len,
                               corvid_value **out);
```
Minimum / maximum comparable (numeric or text) value at `field`, as an
OWNED value handle in `*out`. Absence is a success:
`CORVID_OK` + `*out == NULL` when the filtered set holds no comparable
value (`Option<Value>::None`); `CORVID_ERR` on failure. `out` non-NULL.
`QueryBuilder::min` / `QueryBuilder::max`.

```c
corvid_groupiter* corvid_query_group_count(corvid_query *q,
                                           const char *field, size_t field_len);
corvid_groupiter* corvid_query_group_sum(corvid_query *q,
                                        const char *group_field, size_t group_field_len,
                                        const char *value_field, size_t value_field_len);
corvid_groupiter* corvid_query_group_avg(corvid_query *q,
                                        const char *group_field, size_t group_field_len,
                                        const char *value_field, size_t value_field_len);
```
Grouped aggregates over the filtered set, as a cursor of `(group key,
value)` pairs in ascending group-key (byte) order — the engine's
`BTreeMap` iteration order. Counterparts: `QueryBuilder::group_count` /
`group_sum` / `group_avg`. Group keys use the canonical tagged form
(text bare; `i:`/`f:`/`b:` tags; `t:` escaping for ambiguous texts).

```c
int corvid_groupiter_next(corvid_groupiter *it,
                          const char **key_out, size_t *key_len_out,
                          double *value_out);
```
Next `(key, value)`: 1 fetched, 0 exhausted. The key is BORROWED until
the next call or `corvid_groupiter_free`. NULL-handle / NULL-out-param
behavior follows the non-status rule (§7). The value is a `double` for
`group_sum`/`group_avg`; `group_count` yields a count that is exact in a
`double` up to 2^53 (beyond any realistic group cardinality — noted, not
an engine limit). No direct engine counterpart (cursor over the
`BTreeMap` the aggregates return).

```c
void corvid_groupiter_free(corvid_groupiter *it);
```

### 4.8 Mutations (13)

All wrap `corvid::Collection` methods (db.rs). `const corvid_value*`
document inputs are CLONED — the caller keeps its handle.

```c
corvid_status corvid_insert(corvid_coll *c, const uint8_t *key, size_t key_len,
                            const corvid_value *doc);
```
Insert or overwrite at `key`, atomically with all index maintenance and
unique checks. Counterpart: `Collection::insert`.

```c
corvid_status corvid_put_many(corvid_coll *c, const corvid_kv *items, size_t count);
```
Single-transaction bulk load — the fast path (one commit instead of N;
whole batch rolls back on schema/unique violation; duplicates inside a
batch follow last-write-wins). Counterpart:
`Collection::insert_batch(&[(&[u8], &Value)])`.

```c
uint8_t* corvid_insert_auto(corvid_coll *c, const corvid_value *doc,
                            size_t *key_len_out);
```
Insert under a fresh, monotonically increasing zero-padded 20-digit key;
returns the key bytes (length in `*key_len_out`) — **free with
`corvid_free`**. NULL + error on failure (a failed insert does not burn
an id). Counterpart: `Collection::insert_auto -> Vec<u8>`.

```c
corvid_status corvid_update(corvid_coll *c, const uint8_t *key, size_t key_len,
                            corvid_update_fn fn, void *ctx);
```
Read-modify-write via callback (§1.6): `fn` receives the current document
(borrowed; NULL when absent — not an error) and produces the replacement
(owned, consumed) or a deletion. An aborting callback (non-`CORVID_OK`
return) fails `corvid_update` with `CORVID_E_ARGUMENT` and a message
noting that the callback aborted — nothing is written. Counterpart:
`Collection::update(key, f)` with
`F: FnOnce(Option<Value>) -> Option<Value>`. **Not linearizable**
against concurrent writers (get-then-write); use `corvid_compare_and_set`
when that matters. Indexes stay consistent either way.

```c
corvid_status corvid_patch(corvid_coll *c, const uint8_t *key, size_t key_len,
                           const corvid_value *patch);
```
Merge `patch`'s top-level fields into the map at `key` (creating it if
absent); non-map either side replaces the document with `patch`.
Counterpart: `Collection::patch`.

```c
corvid_status corvid_compare_and_set(corvid_coll *c, const uint8_t *key, size_t key_len,
                                     const corvid_value *expected,
                                     const corvid_value *replacement,
                                     int *applied_out);
```
Atomic conditional write. **Both value parameters are nullable**, and
nullability is semantic: `expected == NULL` means "must be absent";
`replacement == NULL` means "delete if it matches". `*applied_out` is 1
when applied, 0 when the compare failed (which is NOT an error status).
Equality is the engine's semantic value equality (`schema::unique_value_eq`):
`NaN == NaN` regardless of payload, `-0.0 == 0.0`, containers
element-wise. Counterpart:
`Collection::compare_and_set(key, Option<&Value>, Option<Value>) -> bool`
— arity and optionality verified against db.rs.

```c
corvid_status corvid_delete(corvid_coll *c, const uint8_t *key, size_t key_len,
                            int *existed_out);
corvid_status corvid_delete_where(corvid_coll *c, corvid_pred *pred,
                                  size_t *removed_out);
corvid_status corvid_delete_batch(corvid_coll *c, const uint8_t *const *keys,
                                  const size_t *key_lens, size_t count,
                                  size_t *removed_out);
```
Counterparts: `Collection::delete -> bool` (`*existed_out`, nullable
out-param); `Collection::delete_where(Predicate) -> usize`
(**consumes `pred`**; index-accelerated matching); 
`Collection::delete_batch(&[&[u8]]) -> usize`. Deleting a key cascades
its graph edges in the same transaction (including edges dangling on a
key that never existed as a document); out-params are nullable.

```c
corvid_status corvid_insert_with_ttl(corvid_coll *c, const uint8_t *key, size_t key_len,
                                     const corvid_value *doc, int64_t expires_at);
corvid_status corvid_set_ttl(corvid_coll *c, const uint8_t *key, size_t key_len,
                             int64_t expires_at);
corvid_status corvid_get_ttl(corvid_coll *c, const uint8_t *key, size_t key_len,
                             int64_t *expires_at_out, int *has_ttl);
corvid_status corvid_purge_expired(corvid_coll *c, int64_t now, size_t *purged_out);
```
Counterparts (ttl.rs): `Collection::insert_with_ttl` (row + expiry in one
commit), `Collection::set_ttl` (set/replace without rewriting the doc),
`Collection::ttl -> Option<i64>` (`*has_ttl` = 0 when unset — not an
error), `Collection::purge_expired(now) -> usize` (the engine keeps no
clock; `now` is the caller's epoch). Expiry is `<= now` inclusive.

### 4.9 Reads (4)

```c
corvid_status corvid_get(corvid_coll *c, const uint8_t *key, size_t key_len,
                         corvid_value **out);
```
Fetch and decode — `*out` receives an OWNED value. Absence is a success:
`CORVID_OK` + `*out == NULL` when the key holds no document (counterpart
`Collection::get -> Option<Value>`); `CORVID_ERR` on failure. `out`
non-NULL.

```c
corvid_status corvid_scan(corvid_coll *c, corvid_scan_fn fn, void *ctx);
```
Stream every `(key, document)` in key order to the callback — constant
memory regardless of collection size; the callback returns 0 to stop
(stopping is not an error). Counterpart:
`Collection::for_each_doc(FnMut(&[u8], Value) -> Result<bool>)` (the
callback-shaped engine twin of `Collection::scan`, which materializes a
`Vec` — the ABI exposes the streaming form).

```c
corvid_status corvid_page(corvid_coll *c, const uint8_t *after, size_t after_len,
                          size_t limit, corvid_rows **rows_out,
                          uint8_t **next_after_out, size_t *next_after_len_out);
```
Keyset pagination: up to `limit` documents in key order strictly after
`after` (`after == NULL || after_len == 0` starts at the beginning),
from one MVCC snapshot. `*rows_out` is an owned rows cursor (score 0.0;
`corvid_rows_next/free` drive it). `*next_after_out` is the resume
cursor — **free it with `corvid_free`** — or NULL with
`*next_after_len_out == 0` at the end of the collection. `limit == 0`
returns empty rows and no cursor. Counterpart:
`Collection::page(after: Option<&[u8]>, limit) -> Page { rows, next }`
— the buffer's ownership (caller frees via `corvid_free`) is the ABI
addition. (Filtered pagination `Collection::page_where` is not exposed in
v1 — §9.)

```c
corvid_status corvid_len(corvid_coll *c, size_t *out);
```
Document count, O(1) maintained counter. Counterpart:
`Collection::len -> usize`.

### 4.10 Indexes & schema (15)

All wrap `corvid::Collection::create_*` methods. Every create is (or
replace): re-creating an index rebuilds it. All validate the collection
and field names (`CORVID_E_RESERVED_COLLECTION` / `CORVID_E_INVALID_NAME`)
and persist across reopen.

```c
corvid_status corvid_create_scalar_index(corvid_coll *c, const char *field, size_t field_len);
```
Scalar secondary index (equality + range acceleration; on disk).
Counterpart: `Collection::create_scalar_index` (scalar.rs).

```c
corvid_status corvid_create_compound_index(corvid_coll *c,
                                           const char *const *fields,
                                           const size_t *field_lens, size_t count);
```
Compound index over an ordered field list (equality prefix + optional
range on the next field). Counterpart:
`Collection::create_compound_index(&[&str])` (scalar.rs).

```c
corvid_status corvid_create_text_index(corvid_coll *c, const char *field, size_t field_len);
corvid_status corvid_create_text_index_ondisk(corvid_coll *c, const char *field, size_t field_len);
```
Inverted text index, in-memory or on-disk. Counterparts:
`Collection::create_text_index` / `create_text_index_ondisk` (fts.rs).

```c
corvid_status corvid_create_geo_index(corvid_coll *c, const char *field, size_t field_len);
```
Spatial index for radius/bbox windows. Counterpart:
`Collection::create_geo_index` (geo_index.rs).

```c
corvid_status corvid_create_vector_index(corvid_coll *c, const char *field, size_t field_len,
                                         corvid_metric metric);
corvid_status corvid_create_vector_index_quantized(corvid_coll *c, const char *field, size_t field_len,
                                                   corvid_metric metric, corvid_quant quant);
corvid_status corvid_create_vector_index_ondisk(corvid_coll *c, const char *field, size_t field_len,
                                                corvid_metric metric);
corvid_status corvid_create_vector_index_ondisk_quantized(corvid_coll *c, const char *field, size_t field_len,
                                                          corvid_metric metric, corvid_quant quant);
corvid_status corvid_create_vector_index_pq(corvid_coll *c, const char *field, size_t field_len,
                                            corvid_metric metric, size_t m, size_t k);
corvid_status corvid_create_vector_index_ondisk_pq(corvid_coll *c, const char *field, size_t field_len,
                                                   corvid_metric metric, size_t m, size_t k);
```
HNSW variants, 1:1 with the engine (index.rs): in-memory full precision
(`create_vector_index`), in-memory quantized
(`create_vector_index_quantized`), on-disk full
(`create_vector_index_ondisk`), on-disk quantized
(`create_vector_index_ondisk_quantized`), in-memory product-quantized
(`create_vector_index_pq` — **arity verified: `(field, metric, m, k)`**,
`m` subspaces × `k` centroids, `dim % m == 0`), on-disk product-quantized
(`create_vector_index_ondisk_pq`, same arity). PQ creates fail with
`CORVID_E_EMPTY_INDEX_TRAINING` when there are no usable training
vectors, and — because `Pq::train`'s domain checks fold into the same
error at index.rs — also for `m == 0`, `k` outside `2..=256`,
`dim % m != 0`, zero-dimensional or mixed-dimension training vectors
(pq.rs `train_inner`).

```c
corvid_status corvid_set_schema(corvid_coll *c, const corvid_field_def *fields, size_t count);
```
Declare (or replace) the collection's schema; enforced on subsequent
writes only (existing documents are not retroactively validated).
Counterpart: `Collection::set_schema(&Schema)` (schema.rs) — the C side
passes the field array the Rust side builds with
`Schema::new().field(Field::new(name, ty).required().unique())`.

```c
corvid_status corvid_schema(corvid_coll *c, corvid_schemaiter **out);
```
The declared schema as a field cursor. Absence is a success:
`CORVID_OK` + `*out == NULL` when no schema is declared (counterpart:
`Collection::schema() -> Option<Schema>`); `CORVID_ERR` on failure.
`out` non-NULL.

```c
int corvid_schemaiter_next(corvid_schemaiter *it, corvid_field_def *out);
```
Next field: 1 fetched, 0 exhausted. Fields arrive in declaration order;
`out->name` is BORROWED until the next call or
`corvid_schemaiter_free`. NULL-handle / NULL-out-param behavior follows
the non-status rule (§7). No direct engine counterpart (cursor over
`Schema::fields()`).

```c
void corvid_schemaiter_free(corvid_schemaiter *it);
```

### 4.11 Graph (7)

Directed property graph over document keys, in the same database
(indexed by relation). All wrap `corvid::Collection` methods (graph.rs).
Endpoints need not exist as documents.

```c
corvid_status corvid_link(corvid_coll *c, const uint8_t *from, size_t from_len,
                          const char *relation, size_t rel_len,
                          const uint8_t *to, size_t to_len);
```
Idempotent directed edge with default weight 1.0 (a plain link
overwrites a prior weighted edge's weight). Counterpart:
`Collection::link`.

```c
corvid_status corvid_link_weighted(corvid_coll *c, const uint8_t *from, size_t from_len,
                                   const char *relation, size_t rel_len,
                                   const uint8_t *to, size_t to_len, double weight);
```
Counterpart: `Collection::link_weighted`.

```c
corvid_status corvid_unlink(corvid_coll *c, const uint8_t *from, size_t from_len,
                            const char *relation, size_t rel_len,
                            const uint8_t *to, size_t to_len, int *removed_out);
```
Remove the edge (and its reverse) atomically; `*removed_out` (nullable)
reports whether the forward edge existed — false is not an error.
Counterpart: `Collection::unlink -> bool`.

```c
corvid_strs* corvid_neighbors(corvid_coll *c, const uint8_t *from, size_t from_len,
                              const char *relation, size_t rel_len);
corvid_strs* corvid_in_neighbors(corvid_coll *c, const uint8_t *to, size_t to_len,
                                 const char *relation, size_t rel_len);
```
Out-/in-edge endpoints in key order, as a strs cursor. Counterparts:
`Collection::neighbors` / `Collection::in_neighbors` (both
`-> Vec<Vec<u8>>`).

```c
corvid_geohits* corvid_neighbors_weighted(corvid_coll *c, const uint8_t *from, size_t from_len,
                                          const char *relation, size_t rel_len);
```
`(target, weight)` pairs — the `(key, double)` shape reuses the geohits
cursor: `distance_km` carries the edge weight (1.0 for unweighted edges).
Counterpart: `Collection::neighbors_weighted -> Vec<(Vec<u8>, f64)>`.

```c
corvid_strs* corvid_traverse(corvid_coll *c, const uint8_t *start, size_t start_len,
                             const char *relation, size_t rel_len, size_t hops);
```
BFS up to `hops` hops following `relation`; reachable nodes excluding
`start`, each once, in BFS order; `hops == 0` yields nothing; cycles
terminate. One read snapshot covers the walk. Counterpart:
`Collection::traverse`.

### 4.12 Geo & shared string iterators (7)

The three geo queries wrap `corvid::Collection` methods (geo.rs) and
return a geohits cursor (nearest-first for radius/nearest; key order for
bbox). A location field holds `[lat, lon]` (array) or a `lat`/`lon` map;
documents without a valid point are skipped. Distances are haversine
kilometres (spherical Earth).

```c
corvid_geohits* corvid_geo_within_radius(corvid_coll *c, const char *field, size_t field_len,
                                         double lat, double lon, double radius_km);
```
Within `radius_km` (inclusive) of the point, nearest first, ties by key.
Counterpart: `Collection::geo_within_radius`.

```c
corvid_geohits* corvid_geo_within_bbox(corvid_coll *c, const char *field, size_t field_len,
                                       double min_lat, double min_lon,
                                       double max_lat, double max_lon);
```
Inside the box, in key order. Bounds are validated at entry — latitude
`[-90, 90]`, longitude `[-180, 180]`, NaN rejected, inverted latitude
rejected — with `CORVID_E_ARGUMENT`. `min_lon > max_lon` wraps the
antimeridian (matches both ranges; exact, unaccelerated).
`distance_km` in the hits is the **0.0 sentinel** (the box query has no
center; the engine returns no distance — documented ABI behavior).
Counterpart: `Collection::geo_within_bbox`.

```c
corvid_geohits* corvid_geo_nearest(corvid_coll *c, const char *field, size_t field_len,
                                   double lat, double lon, size_t k);
```
The true `k` nearest (expanding radius; exact), nearest first; fewer
than `k` only when fewer valid points exist; `k == 0` yields nothing.
Counterpart: `Collection::geo_nearest`.

```c
int corvid_geohits_next(corvid_geohits *h, corvid_geohit *out,
                        const corvid_value **doc_out);
```
Next hit: 1 fetched, 0 exhausted. `out->key` is BORROWED until the next
call or `corvid_geohits_free`; `*doc_out` (nullable pointer) is the
likewise-borrowed full document for this hit. **Cursors from
`corvid_neighbors_weighted` set `*doc_out = NULL`** — the engine returns
`(key, weight)` pairs with no document — and `corvid_geohits_next`
still returns 1 for them. NULL-handle / NULL-out-param behavior follows
the non-status rule (§7). No direct engine counterpart (cursor over the
`Vec<GeoHit>`).

```c
void corvid_geohits_free(corvid_geohits *h);
```

```c
int corvid_strs_next(corvid_strs *s, const char **str_out, size_t *len_out);
```
Next string: 1 fetched, 0 exhausted. BORROWED until the next call or
`corvid_strs_free`. NULL-handle / NULL-out-param behavior follows the
non-status rule (§7). No direct engine counterpart (cursor over
`Vec<String>`).

```c
void corvid_strs_free(corvid_strs *s);
```

### 4.13 Admin (5)

Path-based administrative operations. All wrap `corvid::Db` methods;
the FFI opens the files itself (`std::fs::File`) and hands them to the
engine's generic `Read`/`Write` methods.

```c
corvid_status corvid_dump_to_path(corvid_db *db, const char *path, size_t path_len);
```
Write a logical, version-stamped dump of the whole database (documents,
index/schema/TTL definitions, graph edges, auto-id counters) to `path`,
from one read snapshot. Counterpart: `Db::dump<W: Write>` (migrate.rs) —
the engine writes to a `Writer`; the FFI supplies a `File`.

```c
corvid_status corvid_load_from_path(corvid_db *db, const char *path, size_t path_len);
```
Replay a dump into this database. Counterpart: `Db::load<R: Read>`
(equivalent to `load_with_renames` with an empty map).

```c
corvid_status corvid_load_from_path_with_renames(corvid_db *db,
                                                 const char *path, size_t path_len,
                                                 const char *const *old_names,
                                                 const char *const *new_names,
                                                 const size_t *old_lens,
                                                 const size_t *new_lens,
                                                 size_t count);
```
Dump replay with a collection-rename map (the migration path for legacy
`__`-containing names). Counterpart:
`Db::load_with_renames(r, &BTreeMap<String, String>)` — same contract:
invalid targets fail with `CORVID_E_INVALID_NAME` before reading;
two-sources-one-target collisions fail with `CORVID_E_ARGUMENT`.

```c
corvid_status corvid_backup(corvid_db *db, const char *path, size_t path_len);
```
Consistent point-in-time physical backup to a FRESH file (an existing
target fails with `CORVID_E_BACKUP_TARGET_EXISTS`); safe while writers
are active. Physical means feature-configuration-dependent — use
dump/load to move between feature builds. Counterpart: `Db::backup`.

```c
corvid_status corvid_compact(corvid_db *db, int *moved_out);
```
Reclaim file space after heavy deletes (offline maintenance).
`*moved_out` (nullable) reports whether any data moved. Counterpart:
`Db::compact(&mut self) -> Result<bool>` — **the engine requires
exclusive access**, so this call requires quiescence: every handle
derived from this `db` (collections, queries, and anything else holding
an engine reference) must already be freed. Exclusivity is checked with
an **FFI-owned derived-handle counter** — an `AtomicUsize` incremented
when a handle is created from the db and decremented when that handle is
freed; `corvid_compact` requires the count to be exactly 1 (the db
handle itself) and otherwise fails with the FFI-only `CORVID_E_BUSY`.
The counter is deterministic because the FFI layer is the only `Arc`
cloner — bindings never see the `Arc`. This is the one FFI-only error
code.

## 5. Ownership & transfer rules

The plan's §4, verbatim:

> 1. ABI-returned buffers (strings, next_after, auto-keys) →
>    corvid_free(ptr) only. 2. Handles → their own _free, never
>    cross-family. 3. const corvid_value* inputs are CLONED — caller
>    keeps ownership. 4. Predicates consumed by and/or/not/filter/
>    delete_where. 5. run and aggregations CONSUME the query. 6.
>    Owned-vs-borrowed outputs documented per signature (rows doc +
>    value children are borrowed; freeing them is UB, documented in
>    bold). 7. NULL discipline per parameter; unexpected NULL →
>    CORVID_E_ARGUMENT, never UB.

Per-family transfer table (inputs: C=cloned, K=consumed, B=borrowed-read;
outputs: O=owned-by-caller, B=borrowed):

| Family | Inputs | Outputs |
|---|---|---|
| Lifecycle & errors | path B | db handle O; error message B (thread-local); strs handle O |
| Collection | name B | coll handle O; name B (until free) |
| Value construction | text/bytes/vector C; `array_push`/`map_put` item K | value O |
| Value reads | parent B | `_ref` buffers B; children B; `as_*` by value; `clone` O |
| Predicates | path/value C; combinators' children K | pred O |
| Query builder | filter pred K; vector/text/select/fields B | query O; `run` → rows O (query K) |
| Aggregations | query K; field names B | scalars by value; min/max O; groupiter O |
| Mutations | keys/docs B (docs C into the engine); update callback's `*out` K; CAS/pred per rule 4 | auto-key buffer O (corvid_free); counters by value |
| Reads | key/after B | `get` value O; scan rows B (callback-scoped); page rows O + next_after O (corvid_free) |
| Indexes & schema | field(s) B; field_defs B | schemaiter O; iterated names B |
| Graph | keys/relations B | strs/geohits O |
| Geo & iterators | field/coords by value | geohits O; hit keys/docs B |
| Admin | paths B | by value |

## 6. Thread-safety contract

- `corvid_db` / `corvid_coll`: **thread-safe**. Concurrent reads from any
  number of threads; writes are serialized by the engine (redb's single
  writer); queries are MVCC point-in-time. The engine's `Db` is `Sync`.
- Value builders, predicates, queries, and every cursor: **single-threaded
  construction and use.** Concurrent calls on the same handle from two
  threads are **undefined behavior** — documented, not detected. Bindings
  enforce this by confining each object to one thread/queue (the plan's
  per-language idiom maps do this naturally; PHP ZTS note: one handle per
  request/thread).
- Different handles (even derived from one db) may be used concurrently:
  a query on thread A and an insert on thread B are fine — each sees a
  consistent snapshot/commit as documented in the engine.
- `corvid_last_error_code/message` are **thread-local**: each thread sees
  its own last failure; no locking is needed or provided.
- Freeing a handle while another thread is calling into it is UB. Free
  after joining/quiescing.
- **Quiescence consequence of `corvid_compact`:** compact needs exclusive
  engine access, checked by the FFI-owned derived-handle counter (§4.13).
  Concurrent use of other handles is unaffected until the compact call —
  but a binding that wants to compact must reach a quiescent point (all
  collection/query handles freed) first; threads still holding handles
  keep `CORVID_E_BUSY` as the deterministic answer, never a hang or UB.

## 7. NULL discipline

- Every pointer parameter is documented nullable or not (above, per
  signature). An unexpected NULL — including a NULL handle, a NULL
  out-param marked required, or a NULL data pointer with nonzero length —
  returns `CORVID_ERR` with `CORVID_E_ARGUMENT`. **Never UB.**
- **Non-status functions** (functions that do not return
  `corvid_status`: `corvid_value_type`, `corvid_value_as_bool/as_int/
  as_float`, `corvid_value_len`, the `_ref` trio, and all five `_next`
  cursors) follow the same discipline without a status channel: a NULL
  handle or a NULL required out-parameter yields a **defined inert
  value** — `0` / `*ok = 0` / `NULL` pointer / `0` (= exhausted) for
  `_next` — AND records `CORVID_E_ARGUMENT` in the thread-local
  last-error. Never UB, and never a status return (these functions have
  none). The per-signature notes in §4.4, §4.6, and §4.12 defer to this
  rule.
- Nullable-by-contract pointers carry semantics: `corvid_compare_and_set`'s
  `expected`/`replacement` (absent / delete), `corvid_page`'s `after`
  (start), `corvid_update`'s `current`/`*out` (absent / delete), optional
  out-params (`existed_out`, `removed_out`, `moved_out`, `len_out`s,
  `doc_out`).
- Empty (pointer, 0-length) is distinct from NULL and legal for keys,
  names, text, bytes, and vectors — the engine accepts empty keys and
  documents.
- `corvid_free(NULL)` and every `_free(NULL)` are no-ops.
- UTF-8-requiring strings with invalid encoding: `CORVID_E_ARGUMENT`
  (checked, copied, never UB).

## 8. Naming conventions & stability policy

Naming:

- Every symbol is prefixed `corvid_`.
- Constructors return handles and end in a noun or `_new`
  (`corvid_value_int`, `corvid_value_array_new`, `corvid_query_new`,
  `corvid_pred_*`).
- Destructors end in `_free` — exactly one per handle type; never
  cross-family. `corvid_free` (no suffix) is reserved for plain buffers.
- Cursor advance ends in `_next` and returns `int` (1 row, 0 exhausted).
- Zero-copy borrows end in `_ref`.
- Fluent query setters are `corvid_query_<knob>` (they mutate the builder
  handle; the Rust chain `.filter(...).vector(...)` becomes a sequence of
  calls).
- A function that **consumes** a handle or value says so in this spec and
  consumes it unconditionally — even when it later fails (a failed
  `corvid_query_run` has still consumed the query; a failed
  `corvid_pred_and` has still consumed both children). Callers must not
  free consumed handles afterwards. (Mirrors Rust by-value semantics.)

Stability:

- `corvid_ffi_version()` returns `FFI_VERSION = 1`.
- **Enum values are frozen.** `corvid_status`, `corvid_err` (1–19),
  `corvid_cmp`, `corvid_metric`, `corvid_quant`, `corvid_value_type`,
  `corvid_field_type` are never renumbered and never reordered; new
  values may only be appended (a new engine `Error` variant appends code
  20+, never fills a gap — and the variant-inventory snapshot test of
  §1.3 fails until it is mapped).
- **Pre-1.0 break policy:** while the engine is pre-1.0, breaking ABI
  changes are allowed but must be loud — bump `FFI_VERSION`, change the
  SONAME-less artifact names, and record the break in CHANGELOG and
  DESIGN.md's decision log. Bindings pin exact engine tags, so a break is
  a coordinated bump PR per binding repo, never a surprise.
- **Post-1.0 soname discipline:** the cdylib is `libcorvid.so.1` /
  `libcorvid.1.dylib` / `corvid.dll` with import-lib versioning;
  additive changes (new functions, appended enum values) keep soname `.1`
  and `FFI_VERSION = 1`; any breaking change (signature change, symbol
  removal, renumber) bumps `FFI_VERSION` to 2 and the soname to `.2`, and
  ships alongside a migration note. Struct layouts in `corvid.h` are
  append-only (new fields go at the end, with size checks in the header).
- The generated `corvid.h` is committed and drift-gated: a test
  regenerates it from the crate and diffs (the SYNTAX.md pattern), so
  spec, header, and radar can never disagree silently.

## 9. v1 exclusions and reopen triggers

Deliberately absent from this ABI (from plan §3, plus two spec-level
notes), so binding authors know what is missing on purpose:

| Exclusion | Why | Reopen trigger |
|---|---|---|
| Events / subscriptions (`reactive.rs`, `Subscribe`) | reentrancy across languages | demonstrated v2 need (a binding shipping a portable event loop story) |
| Direct `vector_search` / `text_search` / `phrase_search` fns | the query builder covers them (`.vector`/`.text` sources) | a workload proving per-call overhead of the builder matters (FFI bench) |
| Sketches (`BloomFilter`, `CuckooFilter`, `HyperLogLog`, `LshIndex`, `MinHash`, `TDigest`) | not core to the typed-document story | binding-user demand |
| Semantic cache (`SemanticCache`) | young API | engine-side stabilization |
| `PlanCache` / `explain` / `plan_shape` | advisory/diagnostic, no runtime contract | a binding asks for query introspection |
| `Db::bulk` (begin_bulk relaxed durability) | `corvid_put_many` covers the bulk fast path | a dump-ingest bench showing per-commit fsync cost matters |
| `Collection::page_where` (spec note) | filtered keyset pagination composes from `query().filter()` + `offset/limit`; cursor semantics across a moving filter set are subtle | a binding needing constant-memory filtered pagination |
| `Store`-level byte API (spec note) | the ABI is typed-document only by ruling 1 | none foreseen |
| Non-UTF-8 filesystem paths (spec note) | the engine's `Db::open` accepts any `AsRef<Path>` (including non-UTF-8 OS paths); the ABI takes `(const char*, len)` and requires UTF-8 (§1.5) — a deliberate narrowing so one encoding rule covers every string | a binding on a platform where UTF-8 paths are insufficient (then: a wide-char or OS-native path entry point, additive) |

## Appendix A — exported symbols (122, pinned)

The C-surface radar (Task 7) asserts the header exposes exactly these
122 symbols and the smoke suite drives every one. Grouped as in §4; the
count per family: 8 + 3 + 11 + 12 + 11 + 15 + 11 + 13 + 4 + 15 + 7 + 7 +
5 = **122**.

```
corvid_ffi_version
corvid_open
corvid_open_memory
corvid_close
corvid_last_error_code
corvid_last_error_message
corvid_free
corvid_collections
corvid_collection
corvid_collection_free
corvid_collection_name
corvid_value_null
corvid_value_bool
corvid_value_int
corvid_value_float
corvid_value_text
corvid_value_bytes
corvid_value_vector
corvid_value_array_new
corvid_value_array_push
corvid_value_map_new
corvid_value_map_put
corvid_value_type
corvid_value_as_bool
corvid_value_as_int
corvid_value_as_float
corvid_value_text_ref
corvid_value_bytes_ref
corvid_value_vector_ref
corvid_value_array_get
corvid_value_map_get
corvid_value_len
corvid_value_clone
corvid_value_free
corvid_pred_exists
corvid_pred_compare
corvid_pred_in
corvid_pred_between
corvid_pred_starts_with
corvid_pred_contains
corvid_pred_geo_within
corvid_pred_and
corvid_pred_or
corvid_pred_not
corvid_pred_free
corvid_query_new
corvid_query_filter
corvid_query_vector
corvid_query_text
corvid_query_fuse_rrf
corvid_query_rerank_mmr
corvid_query_approx
corvid_query_limit
corvid_query_offset
corvid_query_order_by
corvid_query_select
corvid_query_run
corvid_query_free
corvid_rows_next
corvid_rows_free
corvid_query_count
corvid_query_count_distinct
corvid_query_sum
corvid_query_avg
corvid_query_min
corvid_query_max
corvid_query_group_count
corvid_query_group_sum
corvid_query_group_avg
corvid_groupiter_next
corvid_groupiter_free
corvid_insert
corvid_put_many
corvid_insert_auto
corvid_update
corvid_patch
corvid_compare_and_set
corvid_delete
corvid_delete_where
corvid_delete_batch
corvid_insert_with_ttl
corvid_set_ttl
corvid_get_ttl
corvid_purge_expired
corvid_get
corvid_scan
corvid_page
corvid_len
corvid_create_scalar_index
corvid_create_compound_index
corvid_create_text_index
corvid_create_text_index_ondisk
corvid_create_geo_index
corvid_create_vector_index
corvid_create_vector_index_quantized
corvid_create_vector_index_ondisk
corvid_create_vector_index_ondisk_quantized
corvid_create_vector_index_pq
corvid_create_vector_index_ondisk_pq
corvid_set_schema
corvid_schema
corvid_schemaiter_next
corvid_schemaiter_free
corvid_link
corvid_link_weighted
corvid_unlink
corvid_neighbors
corvid_in_neighbors
corvid_neighbors_weighted
corvid_traverse
corvid_geo_within_radius
corvid_geo_within_bbox
corvid_geo_nearest
corvid_geohits_next
corvid_geohits_free
corvid_strs_next
corvid_strs_free
corvid_dump_to_path
corvid_load_from_path
corvid_load_from_path_with_renames
corvid_backup
corvid_compact
```
