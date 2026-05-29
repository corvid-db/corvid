//! corvid — an embedded, multi-modal data store with a fluent builder API.
//!
//! This crate is the engine: strictly in-process, no networking. See
//! `DESIGN.md` at the repository root for the architecture and `CLAUDE.md`
//! for the rules this code is held to.
//!
//! The engine is built bottom-up. Today it exposes the L1 storage layer: a
//! transactional byte-oriented key/value store over redb ([`Store`]).

pub mod db;
pub mod distance;
pub mod error;
pub mod fusion;
pub mod query;
pub mod store;
pub mod text;
pub mod value;

pub use db::{Collection, Db};
pub use distance::Metric;
pub use error::{Error, Result};
pub use fusion::{DEFAULT_RRF_K, mmr, reciprocal_rank_fusion};
pub use query::{Hit, TextHit};
pub use store::Store;
pub use value::Value;
