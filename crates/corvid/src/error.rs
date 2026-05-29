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

    /// Stored bytes could not be decoded into a [`crate::Value`].
    #[error("decode: {0}")]
    Decode(String),
}
