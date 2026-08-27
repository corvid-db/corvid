//! Error types for the corvid engine.
//!
//! Each layer surfaces a typed error. At the storage layer we wrap redb's
//! distinct error types transparently so `?` composes across redb calls
//! without losing the underlying cause.

use thiserror::Error;

/// The result type used throughout the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors the engine can produce.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Opening or creating the database file failed.
    #[error("database: {0}")]
    Database(#[from] redb::DatabaseError),

    /// Beginning a read or write transaction failed.
    #[error("transaction: {0}")]
    Transaction(#[from] redb::TransactionError),

    /// Opening a table within a transaction failed.
    #[error("table: {0}")]
    Table(#[from] redb::TableError),

    /// A read or write against storage failed.
    #[error("storage: {0}")]
    Storage(#[from] redb::StorageError),

    /// Committing a write transaction failed.
    #[error("commit: {0}")]
    Commit(#[from] redb::CommitError),

    /// Changing a transaction's durability failed.
    #[error("durability: {0}")]
    SetDurability(#[from] redb::SetDurabilityError),

    /// Compacting the database file failed.
    #[error("compaction: {0}")]
    Compaction(#[from] redb::CompactionError),

    /// Stored bytes could not be decoded into a [`crate::Value`].
    #[error("decode: {0}")]
    Decode(String),

    /// Persisted index state is corrupt (truncated or malformed bytes).
    /// Surfaced loudly rather than silently degrading to empty results or
    /// decoded-as-zero ids (audit C1/C13): a corrupt index must error so it
    /// can be rebuilt, never be trusted.
    #[error("corrupt index state: {context}")]
    CorruptIndex { context: String },

    /// A collection name used the engine-reserved `__` prefix.
    #[error("reserved collection name: {0}")]
    ReservedCollection(String),

    /// The on-disk file was written by an incompatible format version. Per
    /// project policy there is no migration: the old file must be reimported.
    #[error("incompatible format: file is v{found}, engine expects v{expected}")]
    IncompatibleFormat {
        /// The version found in the file.
        found: u64,
        /// The version this engine writes.
        expected: u64,
    },

    /// A product-quantized index was created with no vectors to train its
    /// codebook on (or with parameters the sample can't satisfy).
    #[error("cannot train a PQ codebook: no usable training vectors")]
    EmptyIndexTraining,

    /// A write violated the collection's declared schema.
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// A dump stream was malformed or from an unknown dump version.
    #[error("invalid dump: {0}")]
    InvalidDump(String),

    /// The backup target path already exists. Backing up would silently
    /// merge into the old file (redb opens-or-creates), so an existing target
    /// is refused instead.
    #[error("backup target already exists: {0}")]
    BackupTargetExists(String),

    /// An I/O error reading or writing a dump stream.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
