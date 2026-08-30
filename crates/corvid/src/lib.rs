//! corvid — an embedded, multi-modal data store with a fluent builder API.
//!
//! This crate is the engine: strictly in-process, no networking. See
//! `DESIGN.md` at the repository root for the architecture and `CLAUDE.md`
//! for the rules this code is held to.
//!
//! Open a [`Db`], take a [`Collection`], and compose vector search, full-text
//! search ([`Collection::text_search`], [`Collection::phrase_search`]), metadata
//! filtering ([`field`]), rank fusion (RRF), and MMR reranking into one
//! transactionally consistent [`QueryBuilder`] call. Secondary indexes (HNSW —
//! in-memory and on-disk, with quantization and product quantization — inverted
//! text, scalar/compound, and geo) are derived from the documents and kept
//! consistent on every write, so a query never sees a stale index. Also here: a
//! directed property graph, geospatial queries, joins, an optional declared
//! schema, per-record TTL, reactive change feeds, a semantic cache, and
//! probabilistic sketches. The API is synchronous; a [`Store`] is the
//! byte-oriented KV layer underneath.

#![forbid(unsafe_code)]

pub mod builder;
pub mod db;
pub mod disk_fts;
pub mod disk_hnsw;
pub mod distance;
pub mod error;
pub mod filter;
pub mod fts;
pub mod fusion;
pub mod geo;
pub mod geo_index;
pub mod graph;
pub mod hnsw;
pub mod index;
pub mod index_build;
pub mod join;
pub mod migrate;
pub mod plan;
pub mod pq;
pub mod quant;
pub mod query;
pub mod reactive;
pub mod scalar;
pub mod schema;
pub mod semantic_cache;
pub mod sketch;
pub mod store;
/// Scoped worker-team fork/join for the deterministic parallel paths
/// (PQ training) — engine-private; see the module docs.
pub(crate) mod team;
/// Feature-gated instrumentation shim — engine-private; see the module docs
/// and the `tracing` feature in `Cargo.toml`.
pub(crate) mod telemetry;
pub mod text;
pub mod ttl;
pub mod value;

pub use builder::{PlanShape, QueryBuilder, ResultRow};
pub use db::{Collection, Db};
pub use distance::Metric;
pub use error::{Error, Result};
pub use filter::{CmpOp, Predicate, field};
pub use fusion::{DEFAULT_RRF_K, mmr, reciprocal_rank_fusion};
pub use geo::{GeoHit, haversine_km};
pub use hnsw::Hnsw;
pub use join::JoinRow;
pub use plan::{PlanCache, QueryPlan};
pub use quant::Quantization;
pub use query::{Hit, TextHit};
pub use reactive::{ChangeEvent, ChangeKind, SubscriptionId};
pub use semantic_cache::SemanticCache;
pub use sketch::{BloomFilter, CuckooFilter, HyperLogLog, LshIndex, MinHash, TDigest};
pub use store::Store;
pub use value::Value;
