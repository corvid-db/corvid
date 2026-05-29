//! corvid — an embedded, multi-modal data store with a fluent builder API.
//!
//! This crate is the engine: strictly in-process, no networking. See
//! `DESIGN.md` at the repository root for the architecture and `CLAUDE.md`
//! for the rules this code is held to.
//!
//! The engine is built bottom-up. Today it exposes the L1 storage layer: a
//! transactional byte-oriented key/value store over redb ([`Store`]).

pub mod builder;
pub mod db;
pub mod distance;
pub mod error;
pub mod filter;
pub mod fts;
pub mod fusion;
pub mod geo;
pub mod graph;
pub mod hnsw;
pub mod index;
pub mod join;
pub mod query;
pub mod reactive;
pub mod semantic_cache;
pub mod sketch;
pub mod store;
pub mod text;
pub mod value;

pub use builder::{QueryBuilder, ResultRow};
pub use db::{Collection, Db};
pub use distance::Metric;
pub use error::{Error, Result};
pub use filter::{CmpOp, Predicate, field};
pub use fusion::{DEFAULT_RRF_K, mmr, reciprocal_rank_fusion};
pub use geo::{GeoHit, haversine_km};
pub use hnsw::Hnsw;
pub use join::JoinRow;
pub use query::{Hit, TextHit};
pub use reactive::{ChangeEvent, ChangeKind, SubscriptionId};
pub use semantic_cache::SemanticCache;
pub use sketch::{BloomFilter, HyperLogLog};
pub use store::Store;
pub use value::Value;
