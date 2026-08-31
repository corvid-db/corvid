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
typedef struct corvid_db   corvid_db;
typedef struct corvid_strs corvid_strs;


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

#endif  /* CORVID_H */
