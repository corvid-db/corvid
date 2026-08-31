/* corvid.h — the typed C ABI of the `corvid` cdylib.
 *
 * GENERATED FILE — do not edit by hand. Source of truth: crates/corvid-ffi
 * (docs/FFI.md is the locked contract). Regenerate with:
 *   CORVID_GEN_HEADER=1 cargo test -p corvid-ffi header_h_stays_generated
 * Every ordinary `cargo test` run re-renders and byte-diffs this file, so
 * the crate, the header, and the spec cannot drift apart silently.
 */


#ifndef CORVID_H
#define CORVID_H

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
/* Opaque, single-pointer-sized handles (FFI.md §1.1). The definitions are
 * private to the library; each handle has exactly one destructor (_free,
 * or corvid_close for the db) and cross-family frees are undefined
 * behavior. */
typedef struct corvid_db    corvid_db;
typedef struct corvid_strs  corvid_strs;
typedef struct corvid_value corvid_value;


/**
 * Call outcome (FFI.md §1.3). Failure detail lives in the thread-local
 * last error — a CORVID_ERR return is always paired with a freshly
 * recorded code and message.
 */
enum corvid_status
#if __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  /**
   * Success.
   */
  CORVID_OK = 0,
  /**
   * Failure; detail in `corvid_last_error_code`/`_message`.
   */
  CORVID_ERR = 1,
};
#if __STDC_VERSION__ >= 202311L
typedef enum corvid_status corvid_status;
#else
typedef uint32_t corvid_status;
#endif // __STDC_VERSION__ >= 202311L

/**
 * Detailed codes returned by `corvid_last_error_code()` (FFI.md §1.3,
 * frozen per §8). Value 0 means "no error recorded on this thread";
 * 1–18 map 1:1 onto the engine's `corvid::Error` variants; 19 is
 * FFI-only.
 */
enum corvid_err
#if __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  /**
   * No error recorded on this thread.
   */
  CORVID_E_OK = 0,
  /**
   * `corvid::Error::Database` — opening/creating the file failed.
   */
  CORVID_E_DATABASE = 1,
  /**
   * `corvid::Error::Transaction` — beginning a read/write txn failed.
   */
  CORVID_E_TRANSACTION = 2,
  /**
   * `corvid::Error::Table` — opening a storage table failed.
   */
  CORVID_E_TABLE = 3,
  /**
   * `corvid::Error::Storage` — a storage read/write failed.
   */
  CORVID_E_STORAGE = 4,
  /**
   * `corvid::Error::Commit` — committing a write txn failed.
   */
  CORVID_E_COMMIT = 5,
  /**
   * `corvid::Error::SetDurability` — changing txn durability failed.
   */
  CORVID_E_SET_DURABILITY = 6,
  /**
   * `corvid::Error::Compaction` — compacting the file failed.
   */
  CORVID_E_COMPACTION = 7,
  /**
   * `corvid::Error::Decode` — stored bytes are not a decodable Value.
   */
  CORVID_E_DECODE = 8,
  /**
   * `corvid::Error::CorruptIndex` — persisted index state is corrupt.
   */
  CORVID_E_CORRUPT_INDEX = 9,
  /**
   * `corvid::Error::ReservedCollection` — name uses the `__` prefix.
   */
  CORVID_E_RESERVED_COLLECTION = 10,
  /**
   * `corvid::Error::InvalidName` — name has a NUL byte or interior `__`.
   */
  CORVID_E_INVALID_NAME = 11,
  /**
   * `corvid::Error::InvalidArgument` — argument outside its domain,
   * and the FFI's own NULL/UTF-8 discipline (spec §7).
   */
  CORVID_E_ARGUMENT = 12,
  /**
   * `corvid::Error::IncompatibleFormat` — foreign format version.
   */
  CORVID_E_INCOMPATIBLE_FORMAT = 13,
  /**
   * `corvid::Error::EmptyIndexTraining` — PQ create with no training
   * vectors.
   */
  CORVID_E_EMPTY_INDEX_TRAINING = 14,
  /**
   * `corvid::Error::SchemaViolation` — write violates declared schema.
   */
  CORVID_E_SCHEMA_VIOLATION = 15,
  /**
   * `corvid::Error::InvalidDump` — malformed / unknown-version dump.
   */
  CORVID_E_INVALID_DUMP = 16,
  /**
   * `corvid::Error::BackupTargetExists` — backup path already exists.
   */
  CORVID_E_BACKUP_TARGET_EXISTS = 17,
  /**
   * `corvid::Error::Io` — I/O error (dump/load paths, files).
   */
  CORVID_E_IO = 18,
  /**
   * FFI-only: `corvid_compact` while derived handles are still open
   * (spec §4.13). No engine variant.
   */
  CORVID_E_BUSY = 19,
};
#if __STDC_VERSION__ >= 202311L
typedef enum corvid_err corvid_err;
#else
typedef uint32_t corvid_err;
#endif // __STDC_VERSION__ >= 202311L

/**
 * The value discriminant (FFI.md §1.4, frozen per §8): tags 0..=8,
 * identical to the engine value module's private encoding tags. The
 * engine's constants are not `pub`, so the correspondence is pinned by
 * the `type_tags_are_frozen...` test instead of a const reference.
 */
enum corvid_value_type
#if __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  /**
   * `Value::Null` — absence of a value.
   */
  CORVID_TYPE_NULL = 0,
  /**
   * `Value::Bool` — 0/1.
   */
  CORVID_TYPE_BOOL = 1,
  /**
   * `Value::Int` — 64-bit signed; exact to 2^53 vs Float.
   */
  CORVID_TYPE_INT = 2,
  /**
   * `Value::Float` — 64-bit IEEE (NaN/±inf/-0.0 preserved).
   */
  CORVID_TYPE_FLOAT = 3,
  /**
   * `Value::Text` — UTF-8 bytes.
   */
  CORVID_TYPE_TEXT = 4,
  /**
   * `Value::Bytes` — opaque bytes.
   */
  CORVID_TYPE_BYTES = 5,
  /**
   * `Value::Array` — ordered list.
   */
  CORVID_TYPE_ARRAY = 6,
  /**
   * `Value::Map` — string-keyed map; documents are Maps.
   */
  CORVID_TYPE_MAP = 7,
  /**
   * `Value::Vector` — dense f32 embedding.
   */
  CORVID_TYPE_VECTOR = 8,
};
#if __STDC_VERSION__ >= 202311L
typedef enum corvid_value_type corvid_value_type;
#else
typedef uint32_t corvid_value_type;
#endif // __STDC_VERSION__ >= 202311L

/**
 * The ABI version (spec §4.1/§8): `1`. Bindings verify this before
 * anything else. No engine counterpart — pure ABI versioning.
 */
uint32_t corvid_ffi_version(void);

/**
 * Open (creating if absent) a file-backed database. `path` is borrowed,
 * non-NULL, and must be valid UTF-8 (spec §1.5 — one encoding rule for
 * every ABI string). Wraps `corvid::Db::open`; returns the handle, or
 * NULL with `CORVID_E_DATABASE` / `CORVID_E_INCOMPATIBLE_FORMAT` /
 * `CORVID_E_IO` recorded.
 */
corvid_db *corvid_open(const char *path, size_t path_len);

/**
 * A purely in-memory database (no file). Wraps
 * `corvid::Db::open_in_memory`; fails only on engine-internal storage
 * errors (never in practice).
 */
corvid_db *corvid_open_memory(void);

/**
 * Release the handle's reference (spec §2/§4.1). Dropping the last
 * reference releases the `Db` and its file locks; derived handles keep
 * the engine alive independently. No engine counterpart — Rust drops
 * `Db`, and persistence is durable per-transaction.
 */
corvid_status corvid_close(corvid_db *db);

/**
 * The thread-local last-error code (spec §3): one of the 19 codes,
 * `CORVID_E_OK` when nothing failed on this thread. Successful calls
 * never clear it.
 */
corvid_err corvid_last_error_code(void);

/**
 * The thread-local last-error message (spec §3/§4.1): NUL-terminated for
 * convenience, `*len_out` receives the byte length (`len_out` nullable).
 * Returns NULL when no error is recorded on this thread. The pointer is
 * valid until the next failing corvid call on this thread (or thread
 * exit) — copy it if you need it longer.
 */
const char *corvid_last_error_message(size_t *len_out);

/**
 * The ONLY buffer deallocator in the ABI (spec §4.1/§5 rule 1): frees
 * any buffer the ABI returned by value — `corvid_insert_auto` keys,
 * `corvid_page`'s `next_after` cursor. Does NOT free handles (each has
 * its own `_free`) or values. The domain is exactly those ABI-returned
 * buffers (spec §4.1): freeing a pointer the ABI did not return, or
 * freeing one twice, is undefined behavior — the same class of misuse
 * as C `free()`. `corvid_free(NULL)` is a no-op.
 */
void corvid_free(void *ptr);

/**
 * User collection names (engine-internal `__` namespaces excluded), in
 * name order, as a string cursor driven by `corvid_strs_next` /
 * `corvid_strs_free` (spec §4.12). Wraps `corvid::Db::collections`.
 * Listing does not create anything — an empty-but-never-written
 * collection may not appear. Returns NULL + error on failure.
 */
corvid_strs *corvid_collections(corvid_db *db);

/**
 * Advance the cursor (spec §4.12): returns 1 and fills `*str_out` /
 * `*len_out` for the next string, 0 at exhaustion — out-params
 * untouched at 0; never errors (the list is materialized). The string
 * bytes are BORROWED until the next `corvid_strs_next` or
 * `corvid_strs_free` on this handle — using or freeing them after is UB.
 *
 * NULL handle or NULL out-parameter follows the non-status rule (spec
 * §7): defined inert value (0 = exhausted) AND `CORVID_E_ARGUMENT`
 * recorded — never UB, and never a status return (there is none).
 */
int corvid_strs_next(corvid_strs *s, const char **str_out, size_t *len_out);

/**
 * Free the cursor (spec §4.12). `corvid_strs_free(NULL)` is a no-op
 * (spec §7). Cross-family frees are UB (spec §2).
 */
void corvid_strs_free(corvid_strs *s);

/**
 * `Value::Null` (spec §4.3). Infallible: allocation failure aborts like
 * any Rust allocation, matching the engine's `Value::Null` literal.
 */
corvid_value *corvid_value_null(void);

/**
 * `Value::Bool(v != 0)` (spec §4.3). Infallible; any non-zero `v` —
 * including negatives — is true.
 */
corvid_value *corvid_value_bool(int v);

/**
 * `Value::Int` (spec §4.3). Infallible; `i64::MIN`/`MAX` cross exactly.
 */
corvid_value *corvid_value_int(int64_t v);

/**
 * `Value::Float` (spec §4.3). Infallible; NaN payloads, ±inf, and -0.0
 * cross bit-exact (the engine stores the f64 as-is).
 */
corvid_value *corvid_value_float(double v);

/**
 * `Value::Text` (spec §4.3): the bytes are CLONED into the value — the
 * caller keeps its buffer. `s` must be valid UTF-8 (spec §1.5 — engine
 * strings are Rust `String`s) and non-NULL at any length. NULL or
 * invalid UTF-8 returns NULL with `CORVID_E_ARGUMENT` recorded.
 */
corvid_value *corvid_value_text(const char *s, size_t len);

/**
 * `Value::Bytes` (spec §4.3): CLONED, arbitrary bytes (spec §1.5 —
 * byte payloads are binary-safe). `b` non-NULL at any length; NULL
 * returns NULL + `CORVID_E_ARGUMENT`.
 */
corvid_value *corvid_value_bytes(const uint8_t *b, size_t len);

/**
 * `Value::Vector` (spec §4.3): the floats are CLONED; `dim` 0 is legal
 * (an empty vector value — pass any non-NULL pointer with dim 0, spec
 * §1.5's empty shape). NULL `v` at any dim returns NULL +
 * `CORVID_E_ARGUMENT`. NaN/-0.0/±inf f32s cross bit-exact.
 */
corvid_value *corvid_value_vector(const float *v, size_t dim);

/**
 * `Value::Array(vec![])` (spec §4.3) — the array builder root. Infallible.
 */
corvid_value *corvid_value_array_new(void);

/**
 * Append `item` to `arr` (spec §4.3), **consuming** it: ownership moves
 * into the array — do not free or reuse `item` afterwards, whatever the
 * status (spec §8: consumption is unconditional; a failed push has
 * still dropped the item). `arr` must be an OWNED array value built by
 * `corvid_value_array_new` (or cloned from one) — any other value fails
 * with `CORVID_ERR` + `CORVID_E_ARGUMENT`. Pushing **invalidates every
 * child and `_ref` buffer previously borrowed from `arr`** (spec §5
 * rule 6: the Vec may reallocate) — using them after is UB.
 */
corvid_status corvid_value_array_push(corvid_value *arr, corvid_value *item);

/**
 * `Value::Map(BTreeMap::new())` (spec §4.3) — the map builder root.
 * Infallible. Map iteration order in the engine is sorted by key —
 * construction order does not matter for equality or encoding.
 */
corvid_value *corvid_value_map_new(void);

/**
 * Insert `val` under `key` (spec §4.3), **consuming** `val`
 * unconditionally (spec §8 — a failed put has still dropped it; do not
 * free it afterwards). `key` is borrowed, non-NULL at any length (the
 * empty key is legal), and must be valid UTF-8 (spec §1.5 — map keys
 * are Rust `String`s). A duplicate key REPLACES the previous entry
 * (engine `BTreeMap::insert`, last write wins), **invalidating children
 * and `_ref` buffers borrowed from the replaced child** (spec §5 rule
 * 6). `map` must be an OWNED map value built by `corvid_value_map_new`
 * (or cloned from one).
 */
corvid_status corvid_value_map_put(corvid_value *map,
                                   const char *key,
                                   size_t key_len,
                                   corvid_value *val);

/**
 * The value's discriminant (spec §4.4; counterpart: the `Value`
 * variant). A NULL `v` follows the non-status rule (§7): returns
 * `CORVID_TYPE_NULL` (0) AND records `CORVID_E_ARGUMENT` — the same
 * bits as a real Null value, which is the price of having no status
 * channel; distinguish by reading the recorded error.
 */
corvid_value_type corvid_value_type(const corvid_value *v);

/**
 * Typed read with an ok-flag (spec §4.4; counterpart:
 * `Value::as_bool -> Option<bool>`). A wrong type sets `*ok = 0` and
 * returns 0 — NOT an error, nothing recorded. A NULL `v` or NULL `ok`
 * follows the non-status rule (§7): `*ok = 0` (when `ok` is itself
 * readable), return 0, `CORVID_E_ARGUMENT` recorded.
 */
int corvid_value_as_bool(const corvid_value *v, int *ok);

/**
 * Typed read with an ok-flag (spec §4.4; counterpart:
 * `Value::as_int -> Option<i64>`). A wrong type sets `*ok = 0` and
 * returns 0 — NOT an error, nothing recorded. A NULL `v` or NULL `ok`
 * follows the non-status rule (§7): `*ok = 0`, return 0,
 * `CORVID_E_ARGUMENT` recorded.
 */
int64_t corvid_value_as_int(const corvid_value *v, int *ok);

/**
 * Typed read with an ok-flag (spec §4.4; counterpart:
 * `Value::as_float -> Option<f64>`). A wrong type sets `*ok = 0` and
 * returns 0.0 — NOT an error, nothing recorded. A NULL `v` or NULL
 * `ok` follows the non-status rule (§7): `*ok = 0`, return 0.0,
 * `CORVID_E_ARGUMENT` recorded.
 */
double corvid_value_as_float(const corvid_value *v, int *ok);

/**
 * Zero-copy BORROWED view of the text (spec §4.4; counterpart:
 * `Value::as_text`). NULL when the value is of a different type — not
 * an error; `*len_out` set to 0. NULL `v` or NULL `len_out` follows the
 * non-status rule (§7): NULL pointer (`*len_out = 0` when readable) and
 * `CORVID_E_ARGUMENT` recorded. The buffer points into the value's own
 * storage: valid until the parent value is freed or mutated, and
 * **writing through it is UB**.
 */
const char *corvid_value_text_ref(const corvid_value *v, size_t *len_out);

/**
 * Zero-copy BORROWED view of the bytes (spec §4.4; counterpart:
 * `Value::as_bytes`). NULL on a different type — not an error;
 * `*len_out` set to 0. NULL `v` or NULL `len_out` follows §7's inert
 * rule (NULL pointer + `CORVID_E_ARGUMENT` recorded). Valid until the
 * parent value is freed or mutated; **writing through it is UB**.
 */
const uint8_t *corvid_value_bytes_ref(const corvid_value *v, size_t *len_out);

/**
 * Zero-copy BORROWED view of the vector (spec §4.4; counterpart:
 * `Value::as_vector`). NULL on a different type — not an error;
 * `*dim_out` set to 0. NULL `v` or NULL `dim_out` follows §7's inert
 * rule (NULL pointer + `CORVID_E_ARGUMENT` recorded). Valid until the
 * parent value is freed or mutated; **writing through it is UB**.
 */
const float *corvid_value_vector_ref(const corvid_value *v, size_t *dim_out);

/**
 * BORROWED child at `index` (spec §4.4; counterpart: `Vec` indexing).
 * NULL when `arr` is not an array or `index` is out of range — not an
 * error, nothing recorded. A NULL `arr` follows §7's inert rule (NULL +
 * `CORVID_E_ARGUMENT` recorded). The child is an interior view into the
 * parent's storage: valid until the parent's next mutation or free
 * (spec §5 rule 6), and **calling `corvid_value_free` on it is UB**.
 */
const corvid_value *corvid_value_array_get(const corvid_value *arr, size_t index);

/**
 * BORROWED child under `key` (spec §4.4; counterpart: `Value::get`).
 * NULL when `map` is not a map or the key is absent — not an error,
 * nothing recorded. A NULL `map` or NULL `key` follows §7's inert rule
 * (NULL + `CORVID_E_ARGUMENT` recorded); a non-UTF-8 `key` likewise
 * (spec §1.5 — map keys are Rust `String`s). The child is an interior
 * view into the parent's storage: valid until the parent's next
 * mutation or free (spec §5 rule 6), and **calling
 * `corvid_value_free` on it is UB**.
 */
const corvid_value *corvid_value_map_get(const corvid_value *map, const char *key, size_t key_len);

/**
 * The value's length (spec §4.4): array items / map entries / vector
 * dimensions / text bytes / bytes bytes; 0 for null, bool, int, float.
 * A NULL `v` returns 0 with `CORVID_E_ARGUMENT` recorded (§7). No
 * single engine method — the collection lengths (`Vec::len`,
 * `BTreeMap::len`, `String::len`) it reports.
 */
size_t corvid_value_len(const corvid_value *v);

/**
 * Deep copy returning an OWNED value (spec §4.4; counterpart:
 * `Value::clone` via `#[derive(Clone)]`). This is the sanctioned way to
 * keep data observed through a borrowed child or `_ref` buffer beyond
 * the parent's lifetime (e.g. a `rows` document). A NULL `v` returns
 * NULL + `CORVID_E_ARGUMENT` (§7 — the handle-returning failure shape).
 */
corvid_value *corvid_value_clone(const corvid_value *v);

/**
 * Free an OWNED value (spec §4.4; counterpart: Rust `Drop`).
 * `corvid_value_free(NULL)` is a no-op (§7). **Calling it on a borrowed
 * child — from `_ref`, `array_get`, `map_get`, `rows_next`,
 * `geohits_next`, callbacks, or a value already consumed by
 * `array_push`/`map_put` — is undefined behavior** (spec §4.4, bold):
 * those pointers are interior views or already-dead boxes, not this
 * destructor's domain.
 */
void corvid_value_free(corvid_value *v);

#endif  /* CORVID_H */
