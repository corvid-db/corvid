//! corvid — an embedded, multi-modal data store with a fluent builder API.
//!
//! This crate is the engine: strictly in-process, no networking. See
//! `DESIGN.md` at the repository root for the architecture and `CLAUDE.md`
//! for the rules this code is held to.
//!
//! The engine is built bottom-up. Today it exposes the L1 storage layer: a
//! transactional byte-oriented key/value store over redb ([`Store`]).

pub mod db;
pub mod error;
pub mod store;
pub mod value;

pub use db::{Collection, Db};
pub use error::{Error, Result};
pub use store::Store;
pub use value::Value;
