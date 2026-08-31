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
typedef struct corvid_db        corvid_db;
typedef struct corvid_coll      corvid_coll;
typedef struct corvid_strs      corvid_strs;
typedef struct corvid_value     corvid_value;
typedef struct corvid_pred      corvid_pred;
typedef struct corvid_rows      corvid_rows;
typedef struct corvid_query     corvid_query;
typedef struct corvid_groupiter corvid_groupiter;


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
 * The comparison operator (FFI.md §1.4, frozen per §8): mirrors
 * `corvid::CmpOp` (filter.rs).
 */
enum corvid_cmp
#if __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  /**
   * Equal (numeric Int/Float interop, else structural).
   */
  CORVID_CMP_EQ = 0,
  /**
   * Not equal.
   */
  CORVID_CMP_NE = 1,
  /**
   * Less than (numbers/text only).
   */
  CORVID_CMP_LT = 2,
  /**
   * Less or equal.
   */
  CORVID_CMP_LE = 3,
  /**
   * Greater than.
   */
  CORVID_CMP_GT = 4,
  /**
   * Greater or equal.
   */
  CORVID_CMP_GE = 5,
};
#if __STDC_VERSION__ >= 202311L
typedef enum corvid_cmp corvid_cmp;
#else
typedef uint32_t corvid_cmp;
#endif // __STDC_VERSION__ >= 202311L

/**
 * The distance metric (FFI.md §1.4, frozen per §8): mirrors
 * `corvid::Metric` (distance.rs).
 */
enum corvid_metric
#if __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  /**
   * Cosine distance `1 - cos_sim` in `[0,2]`; zero-norm = maximally
   * distant.
   */
  CORVID_METRIC_COSINE = 0,
  /**
   * Negated dot product (larger dot sorts first).
   */
  CORVID_METRIC_DOT = 1,
  /**
   * Squared Euclidean (monotonic with L2).
   */
  CORVID_METRIC_L2 = 2,
};
#if __STDC_VERSION__ >= 202311L
typedef enum corvid_metric corvid_metric;
#else
typedef uint32_t corvid_metric;
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
 * One `(key, value)` pair for bulk inserts (spec §1.2, POD): the input
 * shape of [`corvid_put_many`]. `key` is non-NULL (any length — the
 * empty key is legal); `val` is non-NULL and CLONED by the call, so
 * the caller keeps ownership of every value handle in the array.
 */
typedef struct corvid_kv {
  /**
   * The row's key, borrowed for the call.
   */
  const uint8_t *key;
  /**
   * Key bytes.
   */
  size_t key_len;
  /**
   * The document, borrowed-read and CLONED into the engine.
   */
  const corvid_value *val;
} corvid_kv;

/**
 * `corvid_update`'s read-modify-write closure (spec §1.6).
 *
 * `current` is NULL when the key is absent (a missing document is not
 * an error); it is BORROWED and valid only inside the callback.
 * On success set `*out` to an OWNED `corvid_value*` (consumed by the
 * call) or leave it NULL to delete the key. Return `CORVID_OK` to
 * apply, any other value to abort (then `*out` must be NULL — nothing
 * is consumed).
 *
 * **Reentrancy (spec §1.6):** the callback runs on the caller's
 * thread between engine operations. It MUST NOT issue further writes
 * to the same database, MUST NOT free or mutate the borrowed
 * arguments, and SHOULD NOT make other corvid calls at all — the
 * portable contract is "no reentrant corvid calls". Violating it
 * (notably calling into the same db handle from inside the callback)
 * is undefined behavior or a deadlock, not a checked error.
 */
typedef corvid_status (*corvid_update_fn)(void *ctx, const corvid_value *current, corvid_value **out);

/**
 * `corvid_scan`'s row sink (spec §1.6): `ctx` is passed through
 * opaque; return 1 to continue, 0 to stop the scan — stopping is not
 * an error (any other return value also stops, defensively: a
 * misbehaving callback is not called again). `key` and `doc` are
 * BORROWED and valid only inside the callback — freeing the doc or
 * keeping the pointers past the return is UB; `corvid_value_clone` is
 * the sanctioned escape.
 *
 * **Reentrancy (spec §1.6):** the callback runs on the caller's
 * thread between engine operations, inside the scan's read
 * transaction. It MUST NOT free or mutate the borrowed arguments, MUST
 * NOT issue writes to the same database, and SHOULD NOT make other
 * corvid calls at all — the portable contract is "no reentrant
 * corvid calls". Violating it is UB or a deadlock, not a checked
 * error.
 */
typedef int (*corvid_scan_fn)(void *ctx, const uint8_t *key, size_t key_len, const corvid_value *doc);

/**
 * Handle to a named collection (spec §4.2); the collection is created
 * lazily on first write. Wraps `corvid::Db::collection` (infallible in
 * Rust). `db` and `name` are non-NULL (`name` UTF-8, any length — the
 * empty name is legal); NULL or misencoded input returns NULL with
 * `CORVID_E_ARGUMENT` recorded. Reserved/invalid names are NOT checked
 * here — they fail at write time with
 * `CORVID_E_RESERVED_COLLECTION` / `CORVID_E_INVALID_NAME`, exactly as
 * the engine does. The handle increments the db's derived-handle
 * counter (spec §4.13) and holds an engine reference, so it keeps the
 * database alive after `corvid_close` (spec §2).
 */
corvid_coll *corvid_collection(corvid_db *db, const char *name, size_t name_len);

/**
 * Free a collection handle (spec §4.2). No engine counterpart (Rust
 * `Collection` is a copyable borrow); this releases the handle's engine
 * reference and its derived-handle count (spec §4.13). `corvid_close`
 * may have already run — the release is shaped to survive it (spec §2).
 * `corvid_collection_free(NULL)` is a no-op (spec §7).
 */
void corvid_collection_free(corvid_coll *coll);

/**
 * The collection's name (spec §4.2): NUL-terminated, `*len_out` set to
 * the byte length (`len_out` nullable). BORROWED from the handle: valid
 * until `corvid_collection_free`, and stable across calls (the buffer
 * never moves). A name that itself contains a NUL byte truncates only
 * the C view — `*len_out` still carries the exact length. A NULL `coll`
 * follows the non-status rule (§7): NULL return with
 * `CORVID_E_ARGUMENT` recorded. No direct engine counterpart (reads the
 * handle's stored name).
 */
const char *corvid_collection_name(corvid_coll *coll, size_t *len_out);

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
 * Insert or overwrite the document at `key` (spec §4.8; counterpart:
 * `Collection::insert`) — atomic with all index maintenance and
 * unique checks. `doc` is borrowed-read (the engine encodes its own
 * copy; the caller keeps the handle). Reserved/invalid collection
 * names fail here with `CORVID_E_RESERVED_COLLECTION` /
 * `CORVID_E_INVALID_NAME` (the write-time name gate).
 */
corvid_status corvid_insert(corvid_coll *c,
                            const uint8_t *key,
                            size_t key_len,
                            const corvid_value *doc);

/**
 * Single-transaction bulk load (spec §4.8; counterpart:
 * `Collection::insert_batch`): one commit instead of N; the whole
 * batch rolls back on a schema/unique violation; duplicate keys inside
 * one batch follow last-write-wins. `items` is an array of `count`
 * [`corvid_kv`] PODs (borrowed for the call; every `val` CLONED) —
 * NULL `items` is legal only with `count == 0` (an empty batch is a
 * successful no-op).
 */
corvid_status corvid_put_many(corvid_coll *c, const struct corvid_kv *items, size_t count);

/**
 * Insert under a fresh, monotonically increasing zero-padded 20-digit
 * key (spec §4.8; counterpart: `Collection::insert_auto ->
 * Vec<u8>`). Returns the key bytes — **free with `corvid_free`** —
 * with the length in `*key_len_out` (nullable, like §7's other
 * len_outs: the buffer's hidden header is what `corvid_free` needs,
 * so a NULL out is tolerable). NULL + error on failure; a failed
 * insert does not burn an id (the engine reserves the counter inside
 * the insert transaction, audit C9). `doc` as `corvid_insert`'s.
 */
uint8_t *corvid_insert_auto(corvid_coll *c, const corvid_value *doc, size_t *key_len_out);

/**
 * Read-modify-write `key` via callback (spec §4.8/§1.6; counterpart:
 * `Collection::update(key, f)` with `F: FnOnce(Option<Value>) ->
 * Option<Value>`). `fn` receives the current document (borrowed;
 * NULL when absent — not an error) and produces the replacement
 * (OWNED, consumed) or a deletion (`*out` left NULL). An aborting
 * callback (any non-`CORVID_OK` return) fails this call with
 * `CORVID_E_ARGUMENT` and a message noting the abort — nothing is
 * written, and a non-NULL `*out` on the abort path is left untouched
 * (the contract requires NULL there; a violating caller keeps
 * ownership of whatever it stored).
 *
 * The engine method is get-then-write; this wrapper inlines that same
 * shape (get, callback, insert-or-delete through the engine's own
 * methods) because the engine closure type has no abort channel — the
 * semantics are the engine's, with the abort leaving the store
 * untouched. One divergence, an honest boundary: the engine's
 * `update` runs `ensure_writable` (db.rs) BEFORE the closure, so on an
 * unwritable collection name its closure never runs, while this
 * wrapper reaches the check only at the write half — AFTER the
 * callback (the get half is a legal read on any name; through today's
 * ABI the final store state is identical, nothing written either
 * way). **Not linearizable** against concurrent writers (same as the
 * engine's); use `corvid_compare_and_set` when that matters.
 * **Reentrancy:** the callback MUST NOT call into the same database
 * (writes especially) — see [`corvid_update_fn`]; that is UB or a
 * deadlock, not a checked error.
 */
corvid_status corvid_update(corvid_coll *c,
                            const uint8_t *key,
                            size_t key_len,
                            corvid_update_fn fn_,
                            void *ctx);

/**
 * Merge `patch`'s top-level fields into the map at `key` (creating it
 * if absent); a non-map on either side replaces the document with
 * `patch` (spec §4.8; counterpart: `Collection::patch`). `patch` is
 * borrowed-read as everywhere.
 */
corvid_status corvid_patch(corvid_coll *c,
                           const uint8_t *key,
                           size_t key_len,
                           const corvid_value *patch);

/**
 * Atomic conditional write (spec §4.8; counterpart:
 * `Collection::compare_and_set(key, Option<&Value>, Option<Value>) ->
 * bool`). **Both value parameters are nullable, and nullability is
 * semantic**: `expected == NULL` means "must be absent";
 * `replacement == NULL` means "delete if it matches". `*applied_out`
 * (nullable) is 1 when applied, 0 when the compare failed — which is
 * `CORVID_OK`, NOT an error. Equality is the engine's semantic value
 * equality (`schema::unique_value_eq`): `NaN == NaN` regardless of
 * payload, `-0.0 == 0.0`, containers element-wise. The `replacement`
 * is cloned for the engine (the caller keeps its handle); `expected`
 * is borrowed-read.
 */
corvid_status corvid_compare_and_set(corvid_coll *c,
                                     const uint8_t *key,
                                     size_t key_len,
                                     const corvid_value *expected,
                                     const corvid_value *replacement,
                                     int32_t *applied_out);

/**
 * Remove the document at `key` (spec §4.8; counterpart:
 * `Collection::delete -> bool`): `*existed_out` (nullable) is 1 when a
 * document was removed, 0 when the key held none. Deleting cascades
 * the key's graph edges in the same transaction — including edges
 * dangling on a key that never existed as a document (the engine's
 * delete-of-absent still cleans edges).
 */
corvid_status corvid_delete(corvid_coll *c,
                            const uint8_t *key,
                            size_t key_len,
                            int32_t *existed_out);

/**
 * Delete every document matching `pred` (spec §4.8; counterpart:
 * `Collection::delete_where(Predicate) -> usize`) — **CONSUMES `pred`**
 * (index-accelerated matching through the engine's query path). `pred`
 * is required; it is consumed unconditionally, whatever the status
 * (spec §8) — using or freeing it afterwards is UB. `*removed_out`
 * (nullable) receives the number removed.
 */
corvid_status corvid_delete_where(corvid_coll *c, corvid_pred *pred, size_t *removed_out);

/**
 * Delete each of `keys` (spec §4.8; counterpart:
 * `Collection::delete_batch(&[&[u8]]) -> usize`); `*removed_out`
 * (nullable) counts how many existed. `keys`/`key_lens` are parallel
 * borrowed arrays, non-NULL when `count > 0` (`count == 0` with NULL
 * arrays is a successful no-op). Each delete cascades that key's graph
 * edges, as `corvid_delete`'s.
 */
corvid_status corvid_delete_batch(corvid_coll *c,
                                  const uint8_t *const *keys,
                                  const size_t *key_lens,
                                  size_t count,
                                  size_t *removed_out);

/**
 * Insert `doc` at `key` with expiry `expires_at` (spec §4.8;
 * counterpart: `Collection::insert_with_ttl`) — the row and its expiry
 * commit atomically. `expires_at` is in the caller's epoch (the engine
 * keeps no clock); the record behaves normally until purged.
 */
corvid_status corvid_insert_with_ttl(corvid_coll *c,
                                     const uint8_t *key,
                                     size_t key_len,
                                     const corvid_value *doc,
                                     int64_t expires_at);

/**
 * Set (or replace) `key`'s expiry without rewriting the document
 * (spec §4.8; counterpart: `Collection::set_ttl`). Setting an expiry
 * on an absent key records the expiry anyway (the engine's TTL index
 * is key-addressed); the purge's compare-expiry re-verification keeps
 * it harmless.
 */
corvid_status corvid_set_ttl(corvid_coll *c,
                             const uint8_t *key,
                             size_t key_len,
                             int64_t expires_at);

/**
 * `key`'s expiry, if one is set (spec §4.8; counterpart:
 * `Collection::ttl -> Option<i64>`). `*has_ttl` (nullable) is 1/0 —
 * unset is NOT an error; `*expires_at_out` (nullable) carries the
 * timestamp when set and 0 when not. A plain (non-TTL) write clears a
 * previously set expiry — the engine clears it in the write
 * transaction.
 */
corvid_status corvid_get_ttl(corvid_coll *c,
                             const uint8_t *key,
                             size_t key_len,
                             int64_t *expires_at_out,
                             int32_t *has_ttl);

/**
 * Delete every record whose expiry is `<= now` — **inclusive** (spec
 * §4.8; counterpart: `Collection::purge_expired(now) -> usize`);
 * `*purged_out` (nullable) receives the count. `now` is the caller's
 * epoch. Records are removed through the normal delete path (indexes
 * stay consistent); each candidate's expiry is re-verified inside the
 * delete transaction, so a rewritten record is skipped, never purged.
 */
corvid_status corvid_purge_expired(corvid_coll *c, int64_t now, size_t *purged_out);

/**
 * True when the path resolves to a present value (spec §4.5;
 * counterpart: `field(path).exists()` → `Predicate::Exists`). `path`
 * is borrowed, non-NULL at any length, valid UTF-8; the empty path
 * resolves nothing (a predicate that matches no document, not an
 * error). NULL or misencoded `path` returns NULL +
 * `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_exists(const char *path, size_t path_len);

/**
 * Compare the path's value against a constant (spec §4.5; counterpart:
 * `field(path).eq/ne/lt/le/gt/ge(v)` → `Predicate::Compare`). `value`
 * is borrowed-read and **CLONED** into the tree — the caller keeps its
 * handle. Semantics (filter.rs): a missing path ⇒ false; unordered
 * kinds under ordered ops ⇒ false; `Int`/`Float` compare numerically
 * across kinds (exact to 2^53); NaN compares false against everything
 * except `NE`. NULL `value`, or an `op` outside `CORVID_CMP_EQ..=GE`,
 * returns NULL + `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_compare(const char *path,
                                 size_t path_len,
                                 corvid_cmp op,
                                 const corvid_value *value);

/**
 * True when the value equals any element of `values` (spec §4.5;
 * counterpart: `field(path).is_in([...])` → `Predicate::In`). Each
 * element is borrowed-read and **CLONED**. `values` may be NULL only
 * when `count == 0` — an empty membership matches nothing (not an
 * error). A NULL element, or a NULL array with `count > 0`, returns
 * NULL + `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_in(const char *path,
                            size_t path_len,
                            const corvid_value *const *values,
                            size_t count);

/**
 * Inclusive `[low, high]` range (spec §4.5; counterpart:
 * `field(path).between(lo, hi)` → `Predicate::Between`). Both bounds
 * are required, borrowed-read, and **CLONED**. A NULL bound returns
 * NULL + `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_between(const char *path,
                                 size_t path_len,
                                 const corvid_value *low,
                                 const corvid_value *high);

/**
 * The text at `path` starts with `prefix` (spec §4.5; counterpart:
 * `field(path).starts_with(p)` → `Predicate::StartsWith`). False on
 * non-text values and missing paths. `prefix` is borrowed, non-NULL at
 * any length, valid UTF-8; NULL or misencoded returns NULL +
 * `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_starts_with(const char *path,
                                     size_t path_len,
                                     const char *prefix,
                                     size_t prefix_len);

/**
 * The text at `path` contains `substr` (spec §4.5; counterpart:
 * `field(path).contains(s)` → `Predicate::Contains`). False on
 * non-text values and missing paths. `substr` is borrowed, non-NULL at
 * any length, valid UTF-8; NULL or misencoded returns NULL +
 * `CORVID_E_ARGUMENT`.
 */
corvid_pred *corvid_pred_contains(const char *path,
                                  size_t path_len,
                                  const char *substr,
                                  size_t substr_len);

/**
 * The path holds a point (`[lat, lon]` array or `lat`/`lon` map)
 * within `radius_km` of `(lat, lon)` — inclusive, haversine (spec
 * §4.5; counterpart: `field(path).within_km(lat, lon, r)` →
 * `Predicate::GeoWithin`). False on non-point values and missing
 * paths. `path` as everywhere; the coordinates and radius cross by
 * value (no validation — a negative radius simply matches nothing, as
 * in the engine).
 */
corvid_pred *corvid_pred_geo_within(const char *path,
                                    size_t path_len,
                                    double lat,
                                    double lon,
                                    double radius_km);

/**
 * Logical conjunction — **CONSUMES `a` and `b`** (spec §4.5/§5 rule 4;
 * counterpart: `Predicate::and` → `Predicate::And`). After the call the
 * children belong to the tree: freeing them, passing them again, or
 * otherwise using them is **undefined behavior** (a double free). A
 * NULL child fails the combine (NULL + `CORVID_E_ARGUMENT`) after
 * consuming the non-NULL sibling (spec §8's unconditional-consumption
 * discipline); `a == b` (aliasing one handle into both arms) is
 * rejected the same way, consuming the shared handle once.
 */
corvid_pred *corvid_pred_and(corvid_pred *a, corvid_pred *b);

/**
 * Logical disjunction — **CONSUMES `a` and `b`** (spec §4.5;
 * counterpart: `Predicate::or` → `Predicate::Or`). The consumption,
 * NULL-child, and aliasing contracts are `corvid_pred_and`'s.
 */
corvid_pred *corvid_pred_or(corvid_pred *a, corvid_pred *b);

/**
 * Logical negation — **CONSUMES `a`** (spec §4.5; counterpart:
 * `std::ops::Not` → `Predicate::Not`). A NULL `a` fails (NULL +
 * `CORVID_E_ARGUMENT`); after the call the child belongs to the tree
 * (using or freeing it is UB).
 */
corvid_pred *corvid_pred_not(corvid_pred *a);

/**
 * Free a **never-consumed root** (spec §4.5; counterpart: Rust `Drop`
 * of the tree). `corvid_pred_free(NULL)` is a no-op (§7). Predicates
 * handed to `corvid_pred_and/or/not` or `corvid_delete_where` (and,
 * from Task 5 on, `corvid_query_filter`) were consumed by that call —
 * freeing them too is a double free, **undefined behavior**.
 */
void corvid_pred_free(corvid_pred *p);

/**
 * Begin a query over `coll` (spec §4.6; counterpart:
 * `Collection::query() -> QueryBuilder`). Returns NULL only on NULL
 * `coll` (with `CORVID_E_ARGUMENT` recorded). The handle holds an
 * engine reference (it keeps the db alive after `corvid_close`,
 * spec §2) and increments the db's derived-handle counter (spec
 * §4.13) — released by `corvid_query_run`/any aggregate (which
 * consume the handle) or by `corvid_query_free`, exactly one of the
 * two.
 */
corvid_query *corvid_query_new(corvid_coll *coll);

/**
 * Add a filter — **CONSUMES `pred`** (spec §4.6; counterpart:
 * `QueryBuilder::filter(predicate)`, by value). Multiple calls AND
 * together. `pred` is consumed unconditionally when non-NULL (spec
 * §8): a failed call (NULL `q`) has still taken it — free nothing
 * afterwards. NULL `pred` fails with `CORVID_E_ARGUMENT` and consumes
 * nothing.
 */
corvid_status corvid_query_filter(corvid_query *q, corvid_pred *pred);

/**
 * Add a vector-search source (spec §4.6; counterpart:
 * `QueryBuilder::vector(field, query, k, metric)`). The query vector
 * is CLONED — the caller keeps its buffer. `field` is borrowed UTF-8,
 * non-NULL at any length; `query` is non-NULL at any `dim` (dim 0
 * legal, spec §1.5); `k` is any `size_t` (the engine truncates each
 * source's ranking to `k`). A `metric` outside
 * `CORVID_METRIC_COSINE..=L2`, a NULL pointer, or invalid UTF-8 fails
 * with `CORVID_E_ARGUMENT` and leaves the query untouched.
 */
corvid_status corvid_query_vector(corvid_query *q,
                                  const char *field,
                                  size_t field_len,
                                  const float *query,
                                  size_t dim,
                                  size_t k,
                                  corvid_metric metric);

/**
 * Add a BM25 text-search source (spec §4.6; counterpart:
 * `QueryBuilder::text(field, query, k)`). `s` is CLONED into the
 * source; both strings are borrowed UTF-8, non-NULL at any length.
 */
corvid_status corvid_query_text(corvid_query *q,
                                const char *field,
                                size_t field_len,
                                const char *s,
                                size_t s_len,
                                size_t k);

/**
 * Set the Reciprocal Rank Fusion constant (spec §4.6; counterpart:
 * `QueryBuilder::fuse_rrf(k)`; engine default `corvid::DEFAULT_RRF_K`
 * = 60). **This setter always succeeds** — the engine validates at
 * execution (audit C6): a non-finite or non-positive `k` fails
 * `corvid_query_run`/aggregates with `CORVID_E_ARGUMENT`.
 */
corvid_status corvid_query_fuse_rrf(corvid_query *q, float k);

/**
 * Diversify results with Maximal Marginal Relevance (spec §4.6;
 * counterpart: `QueryBuilder::rerank_mmr(lambda)`). **This setter
 * always succeeds** — `lambda` outside `[0,1]` (NaN included) fails
 * `corvid_query_run`/aggregates with `CORVID_E_ARGUMENT` at execution
 * (audit C6). The rerank anchors on the first vector source; without
 * one it is a no-op (engine documented).
 */
corvid_status corvid_query_rerank_mmr(corvid_query *q, float lambda);

/**
 * Allow approximate execution (spec §4.6; counterpart:
 * `QueryBuilder::approx`): a filtered single-vector-source query may
 * use its ANN index with over-fetch-then-filter. A knob, not data —
 * it cannot fail beyond the NULL discipline.
 */
corvid_status corvid_query_approx(corvid_query *q);

/**
 * Cap the result at `n` rows (spec §4.6; counterpart:
 * `QueryBuilder::limit`). `limit 0` yields an empty result (the
 * engine truncates to zero), applied after `offset`.
 */
corvid_status corvid_query_limit(corvid_query *q, size_t n);

/**
 * Skip the first `n` rows (spec §4.6; counterpart:
 * `QueryBuilder::offset`) — applied after ordering, before `limit`.
 */
corvid_status corvid_query_offset(corvid_query *q, size_t n);

/**
 * Order results by a scalar field instead of by rank (spec §4.6;
 * counterpart: `QueryBuilder::order_by(field, descending)`).
 * `descending` is any non-zero `int`. The engine's ordering contract
 * (audit C4): comparable values (numbers numerically — numbers before
 * texts across kinds — texts lexically) first in value order;
 * incomparable values (bools, containers, NaN) after them; rows
 * missing the field last; ties by key; `descending` reverses
 * within-class order only.
 */
corvid_status corvid_query_order_by(corvid_query *q,
                                    const char *field,
                                    size_t field_len,
                                    int descending);

/**
 * Project result documents to these top-level fields (spec §4.6;
 * counterpart: `QueryBuilder::select(fields)`): missing fields are
 * absent, non-map documents pass through unchanged, and ranking still
 * sees the full document. `fields`/`field_lens` are parallel borrowed
 * arrays, non-NULL when `count > 0` (`count == 0` — arrays may be
 * NULL — is the engine-faithful empty projection: map documents
 * project to an empty map, exactly `select(vec![])` in Rust). A NULL
 * array (or array element) with `count > 0`, or a non-UTF-8 field,
 * fails with `CORVID_E_ARGUMENT`.
 */
corvid_status corvid_query_select(corvid_query *q,
                                  const char *const *fields,
                                  const size_t *field_lens,
                                  size_t count);

/**
 * Execute — **CONSUMES `q`** (spec §4.6; counterpart:
 * `QueryBuilder::run(self)`). Returns a rows cursor even for an empty
 * result; NULL + error on failure (distinguish failure by the NULL,
 * never by an empty cursor). One MVCC snapshot covers the whole query;
 * the ranking parameters are validated HERE (audit C6 — a bad
 * `fuse_rrf`/`rerank_mmr` value fails with `CORVID_E_ARGUMENT` after
 * having consumed the query, per spec §8). The handle's derived count
 * is released by this consumption whichever way it goes.
 */
corvid_rows *corvid_query_run(corvid_query *q);

/**
 * Free a builder abandoned without executing (spec §4.6). **NOT** for
 * use after `corvid_query_run`/aggregates — they consumed the handle,
 * and this free would be the documented double-free UB (spec §8). No
 * engine counterpart (Rust drops the builder). Releases the handle's
 * derived count (spec §4.13). `corvid_query_free(NULL)` is a no-op
 * (spec §7).
 */
void corvid_query_free(corvid_query *q);

/**
 * Advance the rows cursor (spec §4.6): returns 1 and fills the
 * out-params for the next row, 0 at exhaustion — out-params untouched
 * at 0; never errors (the result is materialized). The key and the
 * document are **BORROWED from the cursor: valid only until the next
 * `corvid_rows_next` or `corvid_rows_free` — using or freeing them
 * after is UB** (the value family's borrowed-child rule, value.rs
 * module docs; `corvid_value_clone` is the sanctioned escape).
 * `score` is the fused RRF score (`f32`), `0.0` for pure filter/order
 * queries and `corvid_page` rows. NULL handle or NULL out-parameter
 * follows the non-status rule (spec §7): return 0 with
 * `CORVID_E_ARGUMENT` recorded.
 */
int corvid_rows_next(corvid_rows *rows,
                     const uint8_t **key_out,
                     size_t *key_len_out,
                     const corvid_value **doc_out,
                     float *score_out);

/**
 * Free the rows cursor (spec §4.6; counterpart: dropping the
 * `Vec<ResultRow>`). `corvid_rows_free(NULL)` is a no-op (spec §7);
 * the last row's borrowed key/doc die with it.
 */
void corvid_rows_free(corvid_rows *rows);

/**
 * Count the matching documents (spec §4.7; counterpart:
 * `QueryBuilder::count() -> usize`; O(1) when unfiltered via the
 * engine's maintained counter). `out` is nullable (§7's optional
 * out-params: the call still executes and writes nothing).
 */
corvid_status corvid_query_count(corvid_query *q, size_t *out);

/**
 * Distinct values at `field` (spec §4.7; counterpart:
 * `QueryBuilder::count_distinct(field)`) — by the canonical group key
 * (text bare; int/float/bool type-tagged so distinct kinds stay
 * distinct; missing and container values ignored). `field` is
 * borrowed UTF-8, non-NULL at any length; `out` is nullable (§7).
 */
corvid_status corvid_query_count_distinct(corvid_query *q,
                                          const char *field,
                                          size_t field_len,
                                          size_t *out);

/**
 * Sum the numeric (`int`/`float`) values at `field` (spec §4.7;
 * counterpart: `QueryBuilder::sum(field) -> f64`); missing or
 * non-numeric values are skipped (an all-skipped field sums to `0.0`).
 * `out` is nullable (§7).
 */
corvid_status corvid_query_sum(corvid_query *q, const char *field, size_t field_len, double *out);

/**
 * Mean of the numeric values at `field` (spec §4.7; counterpart:
 * `QueryBuilder::avg(field) -> Option<f64>`). Absence is a success:
 * when no numeric value exists, `*has_value = 0` and `*out` (if
 * non-NULL) is set to `0.0` for a defined shape; otherwise
 * `*has_value = 1` and `*out` carries the mean. Both out-params are
 * nullable (§7).
 */
corvid_status corvid_query_avg(corvid_query *q,
                               const char *field,
                               size_t field_len,
                               double *out,
                               int *has_value);

/**
 * The minimum comparable (numeric or text) value at `field` (spec
 * §4.7; counterpart: `QueryBuilder::min(field) -> Option<Value>`), as
 * an OWNED value handle in `*out` — free it with
 * `corvid_value_free`. Absence is a success: `CORVID_OK` + `*out ==
 * NULL` when the filtered set holds no comparable value (the §3
 * optional-value convention). `out` is REQUIRED (spec §4.7: "out
 * non-NULL"); a NULL `field` is the usual `CORVID_E_ARGUMENT`.
 */
corvid_status corvid_query_min(corvid_query *q,
                               const char *field,
                               size_t field_len,
                               corvid_value **out);

/**
 * The maximum comparable value at `field` — [`corvid_query_min`]'s
 * twin (spec §4.7; counterpart: `QueryBuilder::max(field)`), same
 * owned-out/absence-is-success/required-`out` contract.
 */
corvid_status corvid_query_max(corvid_query *q,
                               const char *field,
                               size_t field_len,
                               corvid_value **out);

/**
 * Count matching documents grouped by the value at `field` (spec
 * §4.7; counterpart: `QueryBuilder::group_count(field)`), as a
 * `(group key, count)` cursor in ascending group-key (byte) order —
 * the engine's `BTreeMap` iteration order. Group keys use the
 * canonical tagged form (text bare; `i:`/`f:`/`b:` tags; `t:`
 * escaping for ambiguous texts). NULL + error on failure (the query
 * is consumed either way).
 */
corvid_groupiter *corvid_query_group_count(corvid_query *q, const char *field, size_t field_len);

/**
 * Sum `value_field` grouped by `group_field` (spec §4.7; counterpart:
 * `QueryBuilder::group_sum`), as a `(group key, sum)` cursor in
 * ascending group-key order; non-numeric or missing values are
 * skipped per row (a group with none never materializes). NULL +
 * error on failure (the query is consumed either way).
 */
corvid_groupiter *corvid_query_group_sum(corvid_query *q,
                                         const char *group_field,
                                         size_t group_field_len,
                                         const char *value_field,
                                         size_t value_field_len);

/**
 * Mean of `value_field` grouped by `group_field` (spec §4.7;
 * counterpart: `QueryBuilder::group_avg`), as a `(group key, mean)`
 * cursor in ascending group-key order. NULL + error on failure (the
 * query is consumed either way).
 */
corvid_groupiter *corvid_query_group_avg(corvid_query *q,
                                         const char *group_field,
                                         size_t group_field_len,
                                         const char *value_field,
                                         size_t value_field_len);

/**
 * Advance the group cursor (spec §4.7): returns 1 and fills the
 * out-params for the next `(key, value)` pair, 0 at exhaustion —
 * out-params untouched at 0; never errors (the list is materialized).
 * The key bytes are BORROWED until the next call or
 * `corvid_groupiter_free` — the strs-cursor rule (strs.rs); using
 * them after is UB. The value is a `double` (`group_sum`/`group_avg`
 * means and sums; `group_count` counts, exact in a `double` to 2^53).
 * NULL handle or NULL out-parameter follows the non-status rule
 * (spec §7): return 0 with `CORVID_E_ARGUMENT` recorded.
 */
int corvid_groupiter_next(corvid_groupiter *it,
                          const char **key_out,
                          size_t *key_len_out,
                          double *value_out);

/**
 * Free the group cursor (spec §4.7). `corvid_groupiter_free(NULL)` is
 * a no-op (spec §7); the last key's borrowed bytes die with it.
 */
void corvid_groupiter_free(corvid_groupiter *it);

/**
 * Fetch and decode the document at `key` (spec §4.9; counterpart:
 * `Collection::get -> Option<Value>`): `*out` receives an OWNED value
 * — free it with `corvid_value_free`. **Absence is a success**:
 * `CORVID_OK` + `*out == NULL` when the key holds no document.
 * `CORVID_ERR` on failure. `out` is required (spec §4.9: "out
 * non-NULL" — the one read whose out-param is marked so); `key` as
 * everywhere (non-NULL, any length).
 */
corvid_status corvid_get(corvid_coll *c, const uint8_t *key, size_t key_len, corvid_value **out);

/**
 * Stream every `(key, document)` in the collection to `fn`, in key
 * order (spec §4.9; counterpart: `Collection::for_each_doc(FnMut(&[u8],
 * Value) -> Result<bool>)` — the callback-shaped engine twin of the
 * materializing `Collection::scan`). Constant memory regardless of
 * collection size. The callback returns 1 to continue, 0 to stop
 * (stopping is not an error — `CORVID_OK` either way); `key`/`doc`
 * are BORROWED for the callback's duration only (see
 * [`corvid_scan_fn`]'s reentrancy contract).
 */
corvid_status corvid_scan(corvid_coll *c, corvid_scan_fn fn_, void *ctx);

/**
 * Keyset pagination (spec §4.9; counterpart:
 * `Collection::page(after: Option<&[u8]>, limit) -> Page { rows, next }`):
 * up to `limit` documents in key order strictly after `after`, from
 * one MVCC snapshot. `after == NULL || after_len == 0` starts at the
 * beginning; `limit == 0` returns empty rows and no cursor.
 *
 * `*rows_out` (required) receives an OWNED rows cursor holding the
 * page's materialized rows with score 0.0 — walk it with
 * `corvid_rows_next` / free it with `corvid_rows_free` (Task 5's
 * cursor family; the handle itself is produced here).
 *
 * `*next_after_out` (nullable, as is `next_after_len_out`) receives
 * the resume cursor — an ABI-owned byte buffer, **free it with
 * `corvid_free`** — or NULL with `*next_after_len_out == 0` at the end
 * of the collection. The buffer is allocated only when
 * `next_after_out` is non-NULL; a caller that ignores pagination may
 * pass NULL for the pair.
 */
corvid_status corvid_page(corvid_coll *c,
                          const uint8_t *after,
                          size_t after_len,
                          size_t limit,
                          corvid_rows **rows_out,
                          uint8_t **next_after_out,
                          size_t *next_after_len_out);

/**
 * The document count (spec §4.9; counterpart: `Collection::len ->
 * usize`) — O(1) maintained counter. `out` is nullable (§7's optional
 * out-params: the call still succeeds and writes nothing).
 */
corvid_status corvid_len(corvid_coll *c, size_t *out);

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
 * rule 6: the Vec may reallocate) — using them after is UB. On the
 * self-insertion rejection path (`item == arr`) the shared handle has
 * already been consumed by the call — free neither pointer afterwards.
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
 * are Rust `String`s). A put **invalidates every child and `_ref`
 * buffer previously borrowed from `map`** (spec §5 rule 6), whatever
 * the key: a duplicate key REPLACES the previous entry (engine
 * `BTreeMap::insert`, last write wins — the replaced child is dropped),
 * and a NEW key can split a B-tree node, relocating even untouched
 * existing entries — the conservative rule, same as `array_push`'s.
 * Using a previously borrowed child after any put is UB. On the
 * self-insertion rejection path (`val == map`) the shared handle has
 * already been consumed by the call — free neither pointer afterwards.
 * `map` must be an OWNED map value built by `corvid_value_map_new`
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
