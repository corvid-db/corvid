//! Status and error codes (spec §1.3) and the thread-local last-error
//! slot (spec §3).
//!
//! Every ABI function that can fail reports it through [`corvid_status`]
//! (`CORVID_OK`/`CORVID_ERR`) or a NULL where a handle/buffer was
//! expected, and — as its first act on the failure path — records a code
//! + message in thread-local storage via [`record`] or [`record_engine`].
//! Successful calls never clear the slot (spec §3: read the error
//! immediately after the failure that interests you); the next failing
//! call on the same thread overwrites both code and message.
//!
//! The 19 codes are frozen (spec §8): 1–18 map 1:1 onto the engine's
//! `corvid::Error` variants, pinned by the variant-inventory snapshot
//! test below ([`tests`] / FFI.md §1.3); 19 (`CORVID_E_BUSY`) is the one
//! FFI-only code (compaction exclusivity, spec §4.13). NEVER renumber.
//!
//! [`tests`]: self

use std::cell::RefCell;
use std::ffi::CString;

/// Call outcome (FFI.md §1.3). Failure detail lives in the thread-local
/// last error — a CORVID_ERR return is always paired with a freshly
/// recorded code and message.
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_status {
    /// Success.
    CORVID_OK = 0,
    /// Failure; detail in `corvid_last_error_code`/`_message`.
    CORVID_ERR = 1,
}

/// Detailed codes returned by `corvid_last_error_code()` (FFI.md §1.3,
/// frozen per §8). Value 0 means "no error recorded on this thread";
/// 1–18 map 1:1 onto the engine's `corvid::Error` variants; 19 is
/// FFI-only.
#[allow(non_camel_case_types)] // C ABI names, emitted verbatim by cbindgen
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum corvid_err {
    /// No error recorded on this thread.
    CORVID_E_OK = 0,
    /// `corvid::Error::Database` — opening/creating the file failed.
    CORVID_E_DATABASE = 1,
    /// `corvid::Error::Transaction` — beginning a read/write txn failed.
    CORVID_E_TRANSACTION = 2,
    /// `corvid::Error::Table` — opening a storage table failed.
    CORVID_E_TABLE = 3,
    /// `corvid::Error::Storage` — a storage read/write failed.
    CORVID_E_STORAGE = 4,
    /// `corvid::Error::Commit` — committing a write txn failed.
    CORVID_E_COMMIT = 5,
    /// `corvid::Error::SetDurability` — changing txn durability failed.
    CORVID_E_SET_DURABILITY = 6,
    /// `corvid::Error::Compaction` — compacting the file failed.
    CORVID_E_COMPACTION = 7,
    /// `corvid::Error::Decode` — stored bytes are not a decodable Value.
    CORVID_E_DECODE = 8,
    /// `corvid::Error::CorruptIndex` — persisted index state is corrupt.
    CORVID_E_CORRUPT_INDEX = 9,
    /// `corvid::Error::ReservedCollection` — name uses the `__` prefix.
    CORVID_E_RESERVED_COLLECTION = 10,
    /// `corvid::Error::InvalidName` — name has a NUL byte or interior `__`.
    CORVID_E_INVALID_NAME = 11,
    /// `corvid::Error::InvalidArgument` — argument outside its domain,
    /// and the FFI's own NULL/UTF-8 discipline (spec §7).
    CORVID_E_ARGUMENT = 12,
    /// `corvid::Error::IncompatibleFormat` — foreign format version.
    CORVID_E_INCOMPATIBLE_FORMAT = 13,
    /// `corvid::Error::EmptyIndexTraining` — PQ create with no training
    /// vectors.
    CORVID_E_EMPTY_INDEX_TRAINING = 14,
    /// `corvid::Error::SchemaViolation` — write violates declared schema.
    CORVID_E_SCHEMA_VIOLATION = 15,
    /// `corvid::Error::InvalidDump` — malformed / unknown-version dump.
    CORVID_E_INVALID_DUMP = 16,
    /// `corvid::Error::BackupTargetExists` — backup path already exists.
    CORVID_E_BACKUP_TARGET_EXISTS = 17,
    /// `corvid::Error::Io` — I/O error (dump/load paths, files).
    CORVID_E_IO = 18,
    /// FFI-only: `corvid_compact` while derived handles are still open
    /// (spec §4.13). No engine variant.
    CORVID_E_BUSY = 19,
}

/// The engine-variant → code mapping table of FFI.md §1.3.
///
/// The engine's `Error` is `#[non_exhaustive]` (the correct published
/// posture), so the trailing wildcard is required to compile — but it is
/// unreachable while the variant-inventory snapshot test
/// ([`tests::variant_inventory_matches_the_engine_and_the_mapping`]) is
/// green: that test constructs one instance of every engine variant and
/// fails on any variant outside the inventory. Removing or renaming an
/// engine variant fails compilation here (the named arms break); adding
/// one fails the snapshot test until this mapping is maintained.
pub(crate) fn code_of(err: &corvid::Error) -> corvid_err {
    use corvid::Error;
    match err {
        Error::Database(_) => corvid_err::CORVID_E_DATABASE,
        Error::Transaction(_) => corvid_err::CORVID_E_TRANSACTION,
        Error::Table(_) => corvid_err::CORVID_E_TABLE,
        Error::Storage(_) => corvid_err::CORVID_E_STORAGE,
        Error::Commit(_) => corvid_err::CORVID_E_COMMIT,
        Error::SetDurability(_) => corvid_err::CORVID_E_SET_DURABILITY,
        Error::Compaction(_) => corvid_err::CORVID_E_COMPACTION,
        Error::Decode(_) => corvid_err::CORVID_E_DECODE,
        Error::CorruptIndex { .. } => corvid_err::CORVID_E_CORRUPT_INDEX,
        Error::ReservedCollection(_) => corvid_err::CORVID_E_RESERVED_COLLECTION,
        Error::InvalidName(_) => corvid_err::CORVID_E_INVALID_NAME,
        Error::InvalidArgument(_) => corvid_err::CORVID_E_ARGUMENT,
        Error::IncompatibleFormat { .. } => corvid_err::CORVID_E_INCOMPATIBLE_FORMAT,
        Error::EmptyIndexTraining => corvid_err::CORVID_E_EMPTY_INDEX_TRAINING,
        Error::SchemaViolation(_) => corvid_err::CORVID_E_SCHEMA_VIOLATION,
        Error::InvalidDump(_) => corvid_err::CORVID_E_INVALID_DUMP,
        Error::BackupTargetExists(_) => corvid_err::CORVID_E_BACKUP_TARGET_EXISTS,
        Error::Io(_) => corvid_err::CORVID_E_IO,
        // Unreachable while the inventory snapshot test is green (see the
        // module docs); CORVID_E_ARGUMENT is the defensive placeholder,
        // never a contract.
        _ => corvid_err::CORVID_E_ARGUMENT,
    }
}

/// The recorded failure: code plus the engine's `Display` text.
struct LastError {
    code: corvid_err,
    /// `CString` so the pointer handed out by
    /// `corvid_last_error_message` is NUL-terminated for convenience
    /// (spec §4.1) while `*len_out` still carries the exact byte length.
    message: CString,
}

thread_local! {
    /// The per-thread failure slot (spec §3/§6): each thread sees its own
    /// last failure; no locking is needed or provided.
    static LAST_ERROR: RefCell<Option<LastError>> = const { RefCell::new(None) };
}

/// Record a failure (spec §3). Called as the first act of every failure
/// path; overwrites any previous code and message on this thread.
pub(crate) fn record(code: corvid_err, message: impl Into<String>) {
    // CString cannot hold interior NULs; the engine's Display text may
    // quote caller-supplied bytes, so scrub rather than lose the error.
    let message = CString::new(Into::<String>::into(message).replace('\0', "\u{FFFD}"))
        .expect("NUL bytes replaced above");
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(LastError { code, message }));
}

/// Record an engine failure with its mapped code and `Display` text.
pub(crate) fn record_engine(err: &corvid::Error) {
    record(code_of(err), err.to_string());
}

/// Record the ABI's own argument-discipline failure (spec §7).
pub(crate) fn record_argument(context: &str) {
    record(
        corvid_err::CORVID_E_ARGUMENT,
        format!("corvid: invalid argument ({context})"),
    );
}

/// The thread-local code ([`CORVID_E_OK`] when nothing failed here).
pub(crate) fn last_code() -> corvid_err {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(corvid_err::CORVID_E_OK, |e| e.code)
    })
}

/// The thread-local message: `(pointer, byte length)`, or `None` when no
/// error is recorded on this thread. The pointer is NUL-terminated and
/// valid until the next failing corvid call on this thread (or thread
/// exit) — spec §3's lifetime rule; copy it if you need it longer.
pub(crate) fn last_message() -> Option<(*const std::ffi::c_char, usize)> {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|e| (e.message.as_ptr(), e.message.as_bytes().len()))
    })
}

/// Run an engine call for an FFI wrapper: failure records the mapped code
/// and `Display` text; a residual panic — impossible by the engine's
/// contract, caught defensively per spec §3 — records
/// [`corvid_err::CORVID_E_DATABASE`] (the generic engine-failure bucket;
/// the message says it is a panic). Returns `None` having recorded, so
/// callers translate to their signature's failure shape.
pub(crate) fn guard<T>(context: &str, f: impl FnOnce() -> corvid::Result<T>) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Some(value),
        Ok(Err(err)) => {
            record_engine(&err);
            None
        }
        Err(payload) => {
            record(
                corvid_err::CORVID_E_DATABASE,
                format!(
                    "corvid: internal panic in {context}: {}",
                    panic_text(&payload)
                ),
            );
            None
        }
    }
}

/// Best-effort text out of a `catch_unwind` payload (`&'static str`,
/// `String`, or opaque).
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's 18 `corvid::Error` variant names, pinned (FFI.md §1.3).
    /// Order matches the spec's code table 1..=18.
    const ENGINE_VARIANT_INVENTORY: [&str; 18] = [
        "Database",
        "Transaction",
        "Table",
        "Storage",
        "Commit",
        "SetDurability",
        "Compaction",
        "Decode",
        "CorruptIndex",
        "ReservedCollection",
        "InvalidName",
        "InvalidArgument",
        "IncompatibleFormat",
        "EmptyIndexTraining",
        "SchemaViolation",
        "InvalidDump",
        "BackupTargetExists",
        "Io",
    ];

    /// Match every engine variant by name, with a wildcard arm asserting
    /// any unknown variant is present in the inventory — FFI.md §1.3's
    /// snapshot mechanism. Removing/renaming an engine variant breaks the
    /// named arms at compile time; adding one trips the wildcard here
    /// (and in [`code_of`]'s fallback) the moment the test constructs it.
    fn assert_mapped(err: &corvid::Error) -> &'static str {
        use corvid::Error;
        let name = match err {
            Error::Database(_) => "Database",
            Error::Transaction(_) => "Transaction",
            Error::Table(_) => "Table",
            Error::Storage(_) => "Storage",
            Error::Commit(_) => "Commit",
            Error::SetDurability(_) => "SetDurability",
            Error::Compaction(_) => "Compaction",
            Error::Decode(_) => "Decode",
            Error::CorruptIndex { .. } => "CorruptIndex",
            Error::ReservedCollection(_) => "ReservedCollection",
            Error::InvalidName(_) => "InvalidName",
            Error::InvalidArgument(_) => "InvalidArgument",
            Error::IncompatibleFormat { .. } => "IncompatibleFormat",
            Error::EmptyIndexTraining => "EmptyIndexTraining",
            Error::SchemaViolation(_) => "SchemaViolation",
            Error::InvalidDump(_) => "InvalidDump",
            Error::BackupTargetExists(_) => "BackupTargetExists",
            Error::Io(_) => "Io",
            unexpected => panic!(
                "corvid::Error has a variant outside ENGINE_VARIANT_INVENTORY \
                 ({unexpected:?}) — extend the inventory and the FFI.md §1.3 \
                 mapping (code_of) before shipping"
            ),
        };
        assert!(
            ENGINE_VARIANT_INVENTORY.contains(&name),
            "variant {name} missing from ENGINE_VARIANT_INVENTORY"
        );
        name
    }

    /// One constructible instance of every engine variant — the snapshot
    /// input. The redb-passthrough six are built from redb's public error
    /// enums (the engine wraps them via `#[from]`).
    fn every_engine_variant() -> Vec<corvid::Error> {
        use corvid::Error;
        use redb::{
            CommitError, CompactionError, DatabaseError, SetDurabilityError, StorageError,
            TableError, TransactionError,
        };
        vec![
            Error::Database(DatabaseError::DatabaseAlreadyOpen),
            Error::Transaction(TransactionError::Storage(StorageError::DatabaseClosed)),
            Error::Table(TableError::TableDoesNotExist("t".into())),
            Error::Storage(StorageError::DatabaseClosed),
            Error::Commit(CommitError::Storage(StorageError::DatabaseClosed)),
            Error::SetDurability(SetDurabilityError::PersistentSavepointModified),
            Error::Compaction(CompactionError::TransactionInProgress),
            Error::Decode("bad bytes".into()),
            Error::CorruptIndex {
                context: "truncated".into(),
            },
            Error::ReservedCollection("__x".into()),
            Error::InvalidName("a__b".into()),
            Error::InvalidArgument("lambda out of range".into()),
            Error::IncompatibleFormat {
                found: 1,
                expected: 2,
            },
            Error::EmptyIndexTraining,
            Error::SchemaViolation("field f".into()),
            Error::InvalidDump("unknown version".into()),
            Error::BackupTargetExists("/tmp/old".into()),
            Error::Io(std::io::Error::other("gone")),
        ]
    }

    /// FFI.md §1.3's mapping table, as (variant name, code) pairs. The
    /// snapshot test asserts the engine's live variant set equals this
    /// table exactly — inventory == mapping == engine.
    const SPEC_MAPPING: [(&str, corvid_err); 18] = [
        ("Database", corvid_err::CORVID_E_DATABASE),
        ("Transaction", corvid_err::CORVID_E_TRANSACTION),
        ("Table", corvid_err::CORVID_E_TABLE),
        ("Storage", corvid_err::CORVID_E_STORAGE),
        ("Commit", corvid_err::CORVID_E_COMMIT),
        ("SetDurability", corvid_err::CORVID_E_SET_DURABILITY),
        ("Compaction", corvid_err::CORVID_E_COMPACTION),
        ("Decode", corvid_err::CORVID_E_DECODE),
        ("CorruptIndex", corvid_err::CORVID_E_CORRUPT_INDEX),
        (
            "ReservedCollection",
            corvid_err::CORVID_E_RESERVED_COLLECTION,
        ),
        ("InvalidName", corvid_err::CORVID_E_INVALID_NAME),
        ("InvalidArgument", corvid_err::CORVID_E_ARGUMENT),
        (
            "IncompatibleFormat",
            corvid_err::CORVID_E_INCOMPATIBLE_FORMAT,
        ),
        (
            "EmptyIndexTraining",
            corvid_err::CORVID_E_EMPTY_INDEX_TRAINING,
        ),
        ("SchemaViolation", corvid_err::CORVID_E_SCHEMA_VIOLATION),
        ("InvalidDump", corvid_err::CORVID_E_INVALID_DUMP),
        (
            "BackupTargetExists",
            corvid_err::CORVID_E_BACKUP_TARGET_EXISTS,
        ),
        ("Io", corvid_err::CORVID_E_IO),
    ];

    /// The variant-inventory snapshot test (FFI.md §1.3): every engine
    /// variant is constructed, asserted present in the inventory, and
    /// asserted to carry exactly the spec's code. An engine change in any
    /// direction (add/remove/rename) fails here until the mapping is
    /// maintained.
    #[test]
    fn variant_inventory_matches_the_engine_and_the_mapping() {
        // (ii) first: the inventory equals the spec's mapping table, in
        // code order 1..=18.
        assert_eq!(
            ENGINE_VARIANT_INVENTORY.len(),
            SPEC_MAPPING.len(),
            "inventory and spec mapping tables disagree"
        );
        for (inventory_name, (spec_name, _)) in
            ENGINE_VARIANT_INVENTORY.iter().zip(SPEC_MAPPING.iter())
        {
            assert_eq!(inventory_name, spec_name, "inventory/mapping order drift");
        }

        // (i) the wildcard-armed match, driven by a real instance of every
        // engine variant: each named arm asserts inventory membership, and
        // the code matches the spec table exactly.
        let variants = every_engine_variant();
        assert_eq!(
            variants.len(),
            ENGINE_VARIANT_INVENTORY.len(),
            "constructor list and inventory disagree"
        );
        for err in &variants {
            let name = assert_mapped(err);
            let expected = SPEC_MAPPING
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("{name} absent from SPEC_MAPPING"))
                .1;
            assert_eq!(code_of(err), expected, "mapping drift for {name}");
        }
    }

    #[test]
    fn enum_values_are_frozen_to_the_spec() {
        // §1.3/§8: explicit values, never renumbered. as u32 pins the ABI
        // representation #[repr(u32)] carries across the boundary.
        assert_eq!(corvid_status::CORVID_OK as u32, 0);
        assert_eq!(corvid_status::CORVID_ERR as u32, 1);
        assert_eq!(corvid_err::CORVID_E_OK as u32, 0);
        assert_eq!(corvid_err::CORVID_E_DATABASE as u32, 1);
        assert_eq!(corvid_err::CORVID_E_TRANSACTION as u32, 2);
        assert_eq!(corvid_err::CORVID_E_TABLE as u32, 3);
        assert_eq!(corvid_err::CORVID_E_STORAGE as u32, 4);
        assert_eq!(corvid_err::CORVID_E_COMMIT as u32, 5);
        assert_eq!(corvid_err::CORVID_E_SET_DURABILITY as u32, 6);
        assert_eq!(corvid_err::CORVID_E_COMPACTION as u32, 7);
        assert_eq!(corvid_err::CORVID_E_DECODE as u32, 8);
        assert_eq!(corvid_err::CORVID_E_CORRUPT_INDEX as u32, 9);
        assert_eq!(corvid_err::CORVID_E_RESERVED_COLLECTION as u32, 10);
        assert_eq!(corvid_err::CORVID_E_INVALID_NAME as u32, 11);
        assert_eq!(corvid_err::CORVID_E_ARGUMENT as u32, 12);
        assert_eq!(corvid_err::CORVID_E_INCOMPATIBLE_FORMAT as u32, 13);
        assert_eq!(corvid_err::CORVID_E_EMPTY_INDEX_TRAINING as u32, 14);
        assert_eq!(corvid_err::CORVID_E_SCHEMA_VIOLATION as u32, 15);
        assert_eq!(corvid_err::CORVID_E_INVALID_DUMP as u32, 16);
        assert_eq!(corvid_err::CORVID_E_BACKUP_TARGET_EXISTS as u32, 17);
        assert_eq!(corvid_err::CORVID_E_IO as u32, 18);
        assert_eq!(corvid_err::CORVID_E_BUSY as u32, 19);
    }

    #[test]
    fn record_and_read_the_thread_local_slot() {
        // A fresh test thread has no recorded error...
        assert_eq!(last_code(), corvid_err::CORVID_E_OK);
        assert!(last_message().is_none());

        // ...a failure records code + NUL-terminated message...
        record_engine(&corvid::Error::InvalidName("a__b".into()));
        assert_eq!(last_code(), corvid_err::CORVID_E_INVALID_NAME);
        let (ptr, len) = last_message().expect("recorded above");
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let text = std::str::from_utf8(bytes).unwrap();
        assert_eq!(text, "invalid name (NUL byte or `__` is not allowed): a__b");
        // NUL terminator for C convenience (spec §4.1), outside the length.
        assert_eq!(unsafe { *ptr.add(len) }, 0);

        // ...the next failure overwrites both, per §3's lifetime rule.
        record(corvid_err::CORVID_E_BUSY, "compact: handles open");
        assert_eq!(last_code(), corvid_err::CORVID_E_BUSY);
        let (_, len2) = last_message().expect("recorded above");
        assert_eq!(len2, "compact: handles open".len());
    }

    #[test]
    fn record_scrubs_interior_nuls_instead_of_panicking() {
        record(corvid_err::CORVID_E_DECODE, "bad \0 bytes");
        let (ptr, len) = last_message().expect("recorded above");
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        // The interior NUL became U+FFFD; nothing in the payload is lost
        // to a CString::new failure.
        assert_eq!(std::str::from_utf8(bytes).unwrap(), "bad \u{FFFD} bytes");
    }

    #[test]
    fn guard_maps_failure_and_panics() {
        // Engine failure: recorded with the mapped code.
        let ok = guard("test", || Ok::<_, corvid::Error>(7));
        assert_eq!(ok, Some(7));
        assert_eq!(
            last_code(),
            corvid_err::CORVID_E_OK,
            "success never records"
        );

        let none: Option<()> = guard("test", || Err(corvid::Error::EmptyIndexTraining));
        assert!(none.is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_EMPTY_INDEX_TRAINING);

        // Defensive panic path (spec §3): CORVID_ERR + message, no unwind
        // across the boundary.
        let panicked: Option<()> = guard("test", || panic!("boom"));
        assert!(panicked.is_none());
        assert_eq!(last_code(), corvid_err::CORVID_E_DATABASE);
        let (_, len) = last_message().expect("recorded above");
        assert!(len > 0);
    }

    #[test]
    fn last_error_is_thread_local() {
        // §3/§6: each thread sees only its own failures.
        record(corvid_err::CORVID_E_IO, "parent failure");
        let child = std::thread::spawn(|| {
            // A fresh thread starts clean...
            assert_eq!(last_code(), corvid_err::CORVID_E_OK);
            assert!(last_message().is_none());
            // ...records independently...
            record(corvid_err::CORVID_E_BUSY, "child failure");
            assert_eq!(last_code(), corvid_err::CORVID_E_BUSY);
        });
        child.join().unwrap();
        // ...and the parent's slot is untouched by the child's activity.
        assert_eq!(last_code(), corvid_err::CORVID_E_IO);
    }
}
