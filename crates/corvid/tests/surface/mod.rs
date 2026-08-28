//! The engine surface manifest and radar (conformance program, Task 1).
//!
//! [`MANIFEST`] is the single source of truth for "everything a user can
//! write": one row per public construct of the `corvid` engine crate —
//! module-level functions/constants/type aliases, public types, public
//! methods (inherent impls, wherever the impl block lives), and every enum
//! variant. Each row carries its statement class from the conformance plan's
//! taxonomy table and the integration tests that currently cover it.
//!
//! The radar tests below parse the crate's own sources (from
//! `CARGO_MANIFEST_DIR`) and fail on any drift between the manifest and
//! reality, in both directions: a public item added to `src/` without a
//! manifest row fails, and a manifest row whose item no longer exists fails.
//! A third check keeps `covering_tests` honest: every cited name must be a
//! `fn` in this `tests/` tree (never a unit test inside `src/`).
//!
//! Naming convention for `item` paths: a type or module-level item takes its
//! shortest public path — the crate root when `lib.rs` re-exports it
//! (`corvid::Collection`), else its defining module (`corvid::schema::Field`);
//! methods and variants hang off that canonical type path
//! (`corvid::Collection::insert`, `corvid::Predicate::Between`). `lib.rs`
//! re-exports do not get separate rows; the row belongs to the defining item.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// One manifest row: a public construct, its statement class (one of
/// [`CLASSES`], spelled exactly as the plan's taxonomy table spells it), and
/// the integration tests that cover it.
pub struct Row {
    /// Fully qualified canonical path, e.g. `corvid::Collection::insert`.
    pub item: &'static str,
    /// The statement class from the plan's taxonomy.
    pub class: &'static str,
    /// Names of `fn`s in `crates/corvid/tests/` that cover this item.
    pub covering_tests: &'static [&'static str],
}

/// When `true`, a manifest row with empty `covering_tests` fails the radar.
/// `false` while the conformance waves (Tasks 3–13) fill the suite; Task 15
/// deletes the flag entirely, making strict mode permanent.
const STRICT_COVERING: bool = false;

/// The statement-class names, exactly as the conformance plan's taxonomy
/// table spells them. Every row's `class` must be one of these.
const CLASSES: &[&str] = &[
    "Mutations",
    "WHERE",
    "SELECT shaping",
    "Aggregations",
    "Vector search",
    "Text search",
    "Hybrid",
    "Geo",
    "Schema (ALTER)",
    "TTL",
    "Graph",
    "Joins",
    "Lifecycle",
];

/// Build a manifest row.
const fn row(
    item: &'static str,
    class: &'static str,
    covering_tests: &'static [&'static str],
) -> Row {
    Row {
        item,
        class,
        covering_tests,
    }
}

/// The complete public surface of the engine, classified. Enum variants get
/// their own rows (user-facing vocabularies: predicates, operators, metrics,
/// quantizations, change kinds, error variants, plan shapes, value kinds,
/// field types); nothing is "too trivial".
static MANIFEST: &[Row] = &[
    // ===== value.rs — the typed value model =====
    row(
        "corvid::Value",
        "WHERE",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Value::Null", "WHERE", &[]),
    row("corvid::Value::Bool", "WHERE", &[]),
    row(
        "corvid::Value::Int",
        "WHERE",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Value::Float",
        "WHERE",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row(
        "corvid::Value::Text",
        "WHERE",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Value::Bytes", "WHERE", &[]),
    row(
        "corvid::Value::Array",
        "WHERE",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row(
        "corvid::Value::Map",
        "WHERE",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Value::Vector",
        "WHERE",
        &["search_vector_smoke_ranks_nearest_first_exact"],
    ),
    row("corvid::value::MAX_NESTING", "Lifecycle", &[]),
    row("corvid::Value::encode", "Lifecycle", &[]),
    row("corvid::Value::decode", "Lifecycle", &[]),
    row("corvid::Value::get", "WHERE", &[]),
    row("corvid::Value::get_path", "WHERE", &[]),
    row("corvid::Value::as_bool", "WHERE", &[]),
    row("corvid::Value::as_int", "WHERE", &[]),
    row("corvid::Value::as_float", "WHERE", &[]),
    row("corvid::Value::as_text", "WHERE", &[]),
    row("corvid::Value::as_bytes", "WHERE", &[]),
    row("corvid::Value::as_vector", "WHERE", &[]),
    // ===== filter.rs — predicates =====
    row(
        "corvid::CmpOp",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::CmpOp::Eq",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row("corvid::CmpOp::Ne", "WHERE", &[]),
    row("corvid::CmpOp::Lt", "WHERE", &[]),
    row("corvid::CmpOp::Le", "WHERE", &[]),
    row("corvid::CmpOp::Gt", "WHERE", &[]),
    row("corvid::CmpOp::Ge", "WHERE", &[]),
    row(
        "corvid::Predicate",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::Predicate::Compare",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row("corvid::Predicate::Exists", "WHERE", &[]),
    row("corvid::Predicate::In", "WHERE", &[]),
    row("corvid::Predicate::Between", "WHERE", &[]),
    row("corvid::Predicate::StartsWith", "WHERE", &[]),
    row("corvid::Predicate::Contains", "WHERE", &[]),
    row("corvid::Predicate::GeoWithin", "Geo", &[]),
    row("corvid::Predicate::And", "WHERE", &[]),
    row("corvid::Predicate::Or", "WHERE", &[]),
    row("corvid::Predicate::Not", "WHERE", &[]),
    row("corvid::Predicate::and", "WHERE", &[]),
    row("corvid::Predicate::or", "WHERE", &[]),
    row("corvid::Predicate::eval", "WHERE", &[]),
    row(
        "corvid::field",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::filter::FieldRef",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::filter::FieldRef::eq",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row("corvid::filter::FieldRef::ne", "WHERE", &[]),
    row("corvid::filter::FieldRef::lt", "WHERE", &[]),
    row("corvid::filter::FieldRef::le", "WHERE", &[]),
    row("corvid::filter::FieldRef::gt", "WHERE", &[]),
    row("corvid::filter::FieldRef::ge", "WHERE", &[]),
    row("corvid::filter::FieldRef::exists", "WHERE", &[]),
    row("corvid::filter::FieldRef::is_in", "WHERE", &[]),
    row("corvid::filter::FieldRef::between", "WHERE", &[]),
    row("corvid::filter::FieldRef::starts_with", "WHERE", &[]),
    row("corvid::filter::FieldRef::contains", "WHERE", &[]),
    row("corvid::filter::FieldRef::within_km", "Geo", &[]),
    // ===== distance.rs — vector metrics =====
    row(
        "corvid::Metric",
        "Vector search",
        &["search_vector_smoke_ranks_nearest_first_exact"],
    ),
    row(
        "corvid::Metric::Cosine",
        "Vector search",
        &["search_vector_smoke_ranks_nearest_first_exact"],
    ),
    row("corvid::Metric::Dot", "Vector search", &[]),
    row("corvid::Metric::L2", "Vector search", &[]),
    row("corvid::Metric::distance", "Vector search", &[]),
    row("corvid::distance::dot", "Vector search", &[]),
    row("corvid::distance::l2_squared", "Vector search", &[]),
    row("corvid::distance::cosine_distance", "Vector search", &[]),
    // ===== quant.rs — vector quantization =====
    row("corvid::Quantization", "Vector search", &[]),
    row("corvid::Quantization::None", "Vector search", &[]),
    row("corvid::Quantization::Binary", "Vector search", &[]),
    row("corvid::Quantization::Scalar", "Vector search", &[]),
    // ===== error.rs — error vocabulary =====
    row("corvid::Result", "Lifecycle", &[]),
    row(
        "corvid::Error",
        "Lifecycle",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::Error::Database", "Lifecycle", &[]),
    row("corvid::Error::Transaction", "Lifecycle", &[]),
    row("corvid::Error::Table", "Lifecycle", &[]),
    row("corvid::Error::Storage", "Lifecycle", &[]),
    row("corvid::Error::Commit", "Lifecycle", &[]),
    row("corvid::Error::SetDurability", "Lifecycle", &[]),
    row("corvid::Error::Compaction", "Lifecycle", &[]),
    row("corvid::Error::Decode", "Lifecycle", &[]),
    row("corvid::Error::CorruptIndex", "Schema (ALTER)", &[]),
    row("corvid::Error::ReservedCollection", "Schema (ALTER)", &[]),
    row("corvid::Error::InvalidName", "Schema (ALTER)", &[]),
    row("corvid::Error::InvalidArgument", "Hybrid", &[]),
    row("corvid::Error::IncompatibleFormat", "Lifecycle", &[]),
    row("corvid::Error::EmptyIndexTraining", "Schema (ALTER)", &[]),
    row(
        "corvid::Error::SchemaViolation",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::Error::InvalidDump", "Lifecycle", &[]),
    row("corvid::Error::BackupTargetExists", "Lifecycle", &[]),
    row("corvid::Error::Io", "Lifecycle", &[]),
    // ===== builder.rs — the fluent query language =====
    row(
        "corvid::ResultRow",
        "SELECT shaping",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row("corvid::PlanShape", "Lifecycle", &[]),
    row("corvid::PlanShape::AnnIndex", "Lifecycle", &[]),
    row("corvid::PlanShape::TextIndex", "Lifecycle", &[]),
    row("corvid::PlanShape::IndexedWindow", "Lifecycle", &[]),
    row("corvid::PlanShape::StreamingTopK", "Lifecycle", &[]),
    row("corvid::PlanShape::Scan", "Lifecycle", &[]),
    row(
        "corvid::QueryBuilder",
        "SELECT shaping",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::Collection::query",
        "SELECT shaping",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::QueryBuilder::filter",
        "WHERE",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    row(
        "corvid::QueryBuilder::vector",
        "Vector search",
        &["search_hybrid_smoke_rrf_fuses_vector_and_text"],
    ),
    row(
        "corvid::QueryBuilder::text",
        "Text search",
        &["search_hybrid_smoke_rrf_fuses_vector_and_text"],
    ),
    row("corvid::QueryBuilder::fuse_rrf", "Hybrid", &[]),
    row("corvid::QueryBuilder::rerank_mmr", "Hybrid", &[]),
    row(
        "corvid::QueryBuilder::limit",
        "SELECT shaping",
        &["queries_smoke_order_by_limit_select_shapes_rows"],
    ),
    row("corvid::QueryBuilder::offset", "SELECT shaping", &[]),
    row(
        "corvid::QueryBuilder::order_by",
        "SELECT shaping",
        &["queries_smoke_order_by_limit_select_shapes_rows"],
    ),
    row("corvid::QueryBuilder::approx", "Vector search", &[]),
    row(
        "corvid::QueryBuilder::select",
        "SELECT shaping",
        &["queries_smoke_order_by_limit_select_shapes_rows"],
    ),
    row(
        "corvid::QueryBuilder::count",
        "Aggregations",
        &["aggregations_smoke_sum_group_count_and_count"],
    ),
    row(
        "corvid::QueryBuilder::group_count",
        "Aggregations",
        &["aggregations_smoke_sum_group_count_and_count"],
    ),
    row(
        "corvid::QueryBuilder::sum",
        "Aggregations",
        &["aggregations_smoke_sum_group_count_and_count"],
    ),
    row("corvid::QueryBuilder::avg", "Aggregations", &[]),
    row("corvid::QueryBuilder::min", "Aggregations", &[]),
    row("corvid::QueryBuilder::max", "Aggregations", &[]),
    row("corvid::QueryBuilder::count_distinct", "Aggregations", &[]),
    row("corvid::QueryBuilder::group_sum", "Aggregations", &[]),
    row("corvid::QueryBuilder::group_avg", "Aggregations", &[]),
    row("corvid::QueryBuilder::plan", "Lifecycle", &[]),
    row("corvid::QueryBuilder::plan_shape", "Lifecycle", &[]),
    row("corvid::QueryBuilder::explain", "Lifecycle", &[]),
    row(
        "corvid::QueryBuilder::run",
        "SELECT shaping",
        &["filters_smoke_field_eq_selects_matching_rows"],
    ),
    // ===== db.rs — database and collection handles =====
    row(
        "corvid::Db",
        "Lifecycle",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Db::open", "Lifecycle", &[]),
    row(
        "corvid::Db::open_in_memory",
        "Lifecycle",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Db::collection",
        "Lifecycle",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Db::backup", "Lifecycle", &[]),
    row("corvid::Db::bulk", "Lifecycle", &[]),
    row("corvid::Db::compact", "Lifecycle", &[]),
    row("corvid::Db::collections", "Lifecycle", &[]),
    row(
        "corvid::Collection",
        "Mutations",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Collection::insert",
        "Mutations",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Collection::update", "Mutations", &[]),
    row("corvid::Collection::patch", "Mutations", &[]),
    row("corvid::Collection::compare_and_set", "Mutations", &[]),
    row("corvid::Collection::for_each_doc", "SELECT shaping", &[]),
    row(
        "corvid::Collection::len",
        "SELECT shaping",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Collection::is_empty",
        "SELECT shaping",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Collection::insert_batch", "Mutations", &[]),
    row(
        "corvid::Collection::insert_auto",
        "Mutations",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Collection::get",
        "SELECT shaping",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Collection::delete",
        "Mutations",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row("corvid::Collection::delete_where", "Mutations", &[]),
    row("corvid::Collection::delete_batch", "Mutations", &[]),
    row("corvid::Collection::scan", "SELECT shaping", &[]),
    row("corvid::Collection::page", "SELECT shaping", &[]),
    row("corvid::Collection::page_where", "SELECT shaping", &[]),
    row("corvid::db::Page", "SELECT shaping", &[]),
    // ===== store.rs — the byte KV layer =====
    row("corvid::Store", "Lifecycle", &[]),
    row("corvid::Store::open", "Lifecycle", &[]),
    row("corvid::Store::open_in_memory", "Lifecycle", &[]),
    row("corvid::Store::set_relaxed_durability", "Lifecycle", &[]),
    row("corvid::Store::begin_bulk", "Lifecycle", &[]),
    row("corvid::Store::flush", "Lifecycle", &[]),
    row("corvid::Store::compact", "Lifecycle", &[]),
    row("corvid::Store::next_auto_id", "Lifecycle", &[]),
    row("corvid::Store::backup", "Lifecycle", &[]),
    row("corvid::Store::transaction", "Lifecycle", &[]),
    row("corvid::Store::read", "Lifecycle", &[]),
    row("corvid::Store::put", "Lifecycle", &[]),
    row("corvid::Store::get", "Lifecycle", &[]),
    row("corvid::Store::delete", "Lifecycle", &[]),
    row("corvid::Store::scan", "Lifecycle", &[]),
    row("corvid::Store::collections", "Lifecycle", &[]),
    row("corvid::Store::scan_from", "Lifecycle", &[]),
    row("corvid::Store::count", "Lifecycle", &[]),
    row("corvid::Store::for_each", "Lifecycle", &[]),
    row("corvid::Store::scan_prefix", "Lifecycle", &[]),
    row("corvid::store::BulkScope", "Lifecycle", &[]),
    row("corvid::store::WriteBatch", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::put", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::get", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::delete", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::scan", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::scan_from", "Lifecycle", &[]),
    row("corvid::store::WriteBatch::next_auto_id", "Lifecycle", &[]),
    row("corvid::store::ReadBatch", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::collections", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::auto_ids", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::get", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::scan", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::scan_from", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::scan_prefix", "Lifecycle", &[]),
    row("corvid::store::ReadBatch::for_each", "Lifecycle", &[]),
    // ===== query.rs — retrieval primitives =====
    row(
        "corvid::Hit",
        "Vector search",
        &["search_vector_smoke_ranks_nearest_first_exact"],
    ),
    row(
        "corvid::TextHit",
        "Text search",
        &["search_text_smoke_ranks_most_relevant_first"],
    ),
    row(
        "corvid::Collection::text_search",
        "Text search",
        &["search_text_smoke_ranks_most_relevant_first"],
    ),
    row("corvid::Collection::phrase_search", "Text search", &[]),
    row(
        "corvid::Collection::vector_search",
        "Vector search",
        &["search_vector_smoke_ranks_nearest_first_exact"],
    ),
    // ===== text.rs — tokenization and BM25 =====
    row("corvid::text::Bm25Params", "Text search", &[]),
    row("corvid::text::Bm25Params::new", "Text search", &[]),
    row("corvid::text::Bm25Params::validate", "Text search", &[]),
    row("corvid::text::tokenize", "Text search", &[]),
    row("corvid::text::s_stem", "Text search", &[]),
    row("corvid::text::Analyzer", "Text search", &[]),
    row("corvid::text::Analyzer::raw", "Text search", &[]),
    row("corvid::text::Analyzer::analyze", "Text search", &[]),
    row("corvid::text::analyze", "Text search", &[]),
    row("corvid::text::idf", "Text search", &[]),
    row("corvid::text::term_score", "Text search", &[]),
    // ===== fusion.rs — rank fusion and diversification =====
    row(
        "corvid::DEFAULT_RRF_K",
        "Hybrid",
        &["search_hybrid_smoke_rrf_fuses_vector_and_text"],
    ),
    row(
        "corvid::reciprocal_rank_fusion",
        "Hybrid",
        &["search_hybrid_smoke_rrf_fuses_vector_and_text"],
    ),
    row("corvid::mmr", "Hybrid", &[]),
    // ===== geo.rs — spatial queries =====
    row(
        "corvid::haversine_km",
        "Geo",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row(
        "corvid::GeoHit",
        "Geo",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row(
        "corvid::Collection::geo_within_radius",
        "Geo",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row(
        "corvid::Collection::geo_nearest",
        "Geo",
        &["search_geo_smoke_within_radius_and_nearest"],
    ),
    row("corvid::Collection::geo_within_bbox", "Geo", &[]),
    // ===== geo_index.rs =====
    row(
        "corvid::Collection::create_geo_index",
        "Schema (ALTER)",
        &[],
    ),
    // ===== graph.rs — edges =====
    row(
        "corvid::Collection::link",
        "Graph",
        &["graph_smoke_link_neighbors_traverse_unlink"],
    ),
    row("corvid::Collection::link_weighted", "Graph", &[]),
    row("corvid::Collection::neighbors_weighted", "Graph", &[]),
    row(
        "corvid::Collection::unlink",
        "Graph",
        &["graph_smoke_link_neighbors_traverse_unlink"],
    ),
    row(
        "corvid::Collection::neighbors",
        "Graph",
        &["graph_smoke_link_neighbors_traverse_unlink"],
    ),
    row("corvid::Collection::in_neighbors", "Graph", &[]),
    row(
        "corvid::Collection::traverse",
        "Graph",
        &["graph_smoke_link_neighbors_traverse_unlink"],
    ),
    // ===== join.rs =====
    row(
        "corvid::JoinRow",
        "Joins",
        &["joins_smoke_left_outer_resolves_and_misses"],
    ),
    row(
        "corvid::Collection::join",
        "Joins",
        &["joins_smoke_left_outer_resolves_and_misses"],
    ),
    // ===== hnsw.rs — in-memory ANN =====
    row("corvid::hnsw::DEFAULT_M", "Vector search", &[]),
    row(
        "corvid::hnsw::DEFAULT_EF_CONSTRUCTION",
        "Vector search",
        &[],
    ),
    row("corvid::Hnsw", "Vector search", &[]),
    row("corvid::Hnsw::new", "Vector search", &[]),
    row("corvid::Hnsw::with_params", "Vector search", &[]),
    row("corvid::Hnsw::with_quant", "Vector search", &[]),
    row("corvid::Hnsw::len", "Vector search", &[]),
    row("corvid::Hnsw::is_empty", "Vector search", &[]),
    row("corvid::Hnsw::insert", "Vector search", &[]),
    row("corvid::Hnsw::search", "Vector search", &[]),
    // ===== index.rs — vector index creation =====
    row(
        "corvid::Collection::create_vector_index",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_vector_index_quantized",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk_quantized",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk_pq",
        "Schema (ALTER)",
        &[],
    ),
    // ===== fts.rs — text index creation =====
    row(
        "corvid::Collection::create_text_index",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_text_index_ondisk",
        "Schema (ALTER)",
        &[],
    ),
    // ===== scalar.rs — scalar/compound index creation =====
    row(
        "corvid::Collection::create_scalar_index",
        "Schema (ALTER)",
        &[],
    ),
    row(
        "corvid::Collection::create_compound_index",
        "Schema (ALTER)",
        &[],
    ),
    // ===== schema.rs — declared schemas =====
    row(
        "corvid::schema::FieldType",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::schema::FieldType::Any", "Schema (ALTER)", &[]),
    row("corvid::schema::FieldType::Bool", "Schema (ALTER)", &[]),
    row(
        "corvid::schema::FieldType::Int",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::schema::FieldType::Float", "Schema (ALTER)", &[]),
    row(
        "corvid::schema::FieldType::Text",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::schema::FieldType::Bytes", "Schema (ALTER)", &[]),
    row("corvid::schema::FieldType::Vector", "Schema (ALTER)", &[]),
    row("corvid::schema::FieldType::Array", "Schema (ALTER)", &[]),
    row("corvid::schema::FieldType::Map", "Schema (ALTER)", &[]),
    row(
        "corvid::schema::Field",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row(
        "corvid::schema::Field::new",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row(
        "corvid::schema::Field::required",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::schema::Field::unique", "Schema (ALTER)", &[]),
    row(
        "corvid::schema::Schema",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row(
        "corvid::schema::Schema::new",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row(
        "corvid::schema::Schema::field",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    row("corvid::schema::Schema::fields", "Schema (ALTER)", &[]),
    row(
        "corvid::Collection::set_schema",
        "Schema (ALTER)",
        &["schema_smoke_set_schema_rejects_bad_documents"],
    ),
    // ===== reactive.rs — change feeds =====
    row(
        "corvid::ChangeKind",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::ChangeKind::Insert",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::ChangeKind::Delete",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::ChangeEvent",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::SubscriptionId",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::Db::subscribe",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    row(
        "corvid::Db::unsubscribe",
        "Lifecycle",
        &["events_smoke_subscribe_records_insert_and_delete"],
    ),
    // ===== semantic_cache.rs =====
    row("corvid::SemanticCache", "Lifecycle", &[]),
    row("corvid::Collection::semantic_cache", "Lifecycle", &[]),
    row("corvid::SemanticCache::put", "Lifecycle", &[]),
    row("corvid::SemanticCache::get", "Lifecycle", &[]),
    // ===== sketch.rs — probabilistic sketches =====
    row("corvid::HyperLogLog", "Lifecycle", &[]),
    row("corvid::HyperLogLog::new", "Lifecycle", &[]),
    row("corvid::HyperLogLog::with_precision", "Lifecycle", &[]),
    row("corvid::HyperLogLog::add_bytes", "Lifecycle", &[]),
    row("corvid::HyperLogLog::add_hash", "Lifecycle", &[]),
    row("corvid::HyperLogLog::estimate", "Lifecycle", &[]),
    row("corvid::BloomFilter", "Lifecycle", &[]),
    row("corvid::BloomFilter::new", "Lifecycle", &[]),
    row("corvid::BloomFilter::add_bytes", "Lifecycle", &[]),
    row("corvid::BloomFilter::contains_bytes", "Lifecycle", &[]),
    row("corvid::Collection::approx_distinct", "Aggregations", &[]),
    // ===== ttl.rs — expiry =====
    row(
        "corvid::Collection::insert_with_ttl",
        "TTL",
        &["ttl_smoke_insert_with_ttl_purges_at_boundary"],
    ),
    row("corvid::Collection::set_ttl", "TTL", &[]),
    row(
        "corvid::Collection::ttl",
        "TTL",
        &["ttl_smoke_insert_with_ttl_purges_at_boundary"],
    ),
    row(
        "corvid::Collection::purge_expired",
        "TTL",
        &["ttl_smoke_insert_with_ttl_purges_at_boundary"],
    ),
    // ===== migrate.rs — dump/load =====
    row(
        "corvid::Db::dump",
        "Lifecycle",
        &["lifecycle_smoke_dump_load_roundtrips_documents"],
    ),
    row(
        "corvid::Db::load",
        "Lifecycle",
        &["lifecycle_smoke_dump_load_roundtrips_documents"],
    ),
    // ===== pq.rs — product quantization =====
    row("corvid::pq::Pq", "Vector search", &[]),
    row("corvid::pq::Pq::code_len", "Vector search", &[]),
    row("corvid::pq::Pq::dim", "Vector search", &[]),
    row("corvid::pq::Pq::params", "Vector search", &[]),
    row("corvid::pq::Pq::train", "Vector search", &[]),
    row("corvid::pq::Pq::encode", "Vector search", &[]),
    row("corvid::pq::Pq::decode", "Vector search", &[]),
    row("corvid::pq::Pq::distance", "Vector search", &[]),
    row("corvid::pq::Pq::l2_table", "Vector search", &[]),
    row("corvid::pq::Pq::adc_l2", "Vector search", &[]),
    row("corvid::pq::Pq::to_bytes", "Vector search", &[]),
    row("corvid::pq::Pq::from_bytes", "Vector search", &[]),
    // ===== plan.rs — plan identity and cache =====
    row("corvid::QueryPlan", "Lifecycle", &[]),
    row("corvid::QueryPlan::key", "Lifecycle", &[]),
    row("corvid::PlanCache", "Lifecycle", &[]),
    row("corvid::PlanCache::new", "Lifecycle", &[]),
    row("corvid::PlanCache::get", "Lifecycle", &[]),
    row("corvid::PlanCache::insert", "Lifecycle", &[]),
    row("corvid::PlanCache::get_or_insert_with", "Lifecycle", &[]),
    row("corvid::PlanCache::len", "Lifecycle", &[]),
    row("corvid::PlanCache::is_empty", "Lifecycle", &[]),
];

// ===========================================================================
// Radar.
// ===========================================================================

#[test]
fn manifest_matches_extracted_public_surface() {
    let manifest: BTreeSet<&str> = MANIFEST.iter().map(|r| r.item).collect();
    let extracted = extract_public_surface();

    // In src/ but not in the manifest: an unclassified public construct.
    let missing_from_manifest: Vec<&str> = extracted.difference(&manifest).copied().collect();
    // In the manifest but not in src/: a stale row for a removed/renamed item.
    let removed_from_source: Vec<&str> = manifest.difference(&extracted).copied().collect();

    assert!(
        missing_from_manifest.is_empty() && removed_from_source.is_empty(),
        "surface drift between src/ and MANIFEST:\n  \
         missing_from_manifest (public in src/, no MANIFEST row — add a row \
         or make the item non-public): {missing_from_manifest:?}\n  \
         removed_from_source (MANIFEST row with no public item in src/ — \
         update or delete the row): {removed_from_source:?}"
    );
}

#[test]
fn covering_tests_name_existing_integration_tests() {
    let fns = collect_test_fns(&tests_dir());
    assert!(
        !fns.is_empty(),
        "no test fns found under {}",
        tests_dir().display()
    );
    for r in MANIFEST {
        for name in r.covering_tests {
            assert!(
                fns.contains(*name),
                "manifest row {} cites covering test `{name}`, but no fn with \
                 that name exists under {} (rows must cite integration tests, \
                 never unit tests inside src/)",
                r.item,
                tests_dir().display()
            );
        }
    }
}

#[test]
fn manifest_rows_are_classified_unique_and_strict_covered() {
    let mut seen = BTreeSet::new();
    for r in MANIFEST {
        assert!(
            CLASSES.contains(&r.class),
            "manifest row {} has class {:?}, which is not one of the plan's \
             taxonomy classes {CLASSES:?}",
            r.item,
            r.class
        );
        assert!(seen.insert(r.item), "duplicate manifest row for {}", r.item);
        if STRICT_COVERING {
            assert!(
                !r.covering_tests.is_empty(),
                "STRICT_COVERING is on and manifest row {} has no covering tests",
                r.item
            );
        }
    }
}

// ===========================================================================
// Source extraction.
// ===========================================================================

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tests_dir() -> PathBuf {
    crate_dir().join("tests")
}

/// A public construct discovered in the sources, before canonicalization.
#[derive(Debug)]
enum Raw {
    /// A `pub` item at a module's top level: a fn, const, static, type
    /// alias, or a type declaration (whose variants/methods are separate).
    ModuleItem { module: String, name: String },
    /// A variant of a `pub enum`.
    Variant {
        module: String,
        ty: String,
        variant: String,
    },
    /// A `pub fn` in an inherent impl (self-type name as written).
    Method { ty: String, name: String },
}

/// Walk `src/`, extract every public construct, and canonicalize it to the
/// manifest's naming convention: the shortest public path — the crate root
/// when `lib.rs` re-exports the name, else the defining module.
fn extract_public_surface() -> BTreeSet<&'static str> {
    let mut raws = Vec::new();
    let mut aliases = BTreeSet::new();
    for file in rust_files(&crate_dir().join("src")) {
        let module = module_of(&file);
        let src =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let toks = tokenize(&src);
        parse_scope(&toks, &module, None, &mut raws, &mut aliases);
    }

    // Map each pub type name to its defining module, so impl blocks (which
    // name their self type as written, wherever the impl lives) resolve.
    let mut type_modules: BTreeMap<String, &str> = BTreeMap::new();
    for raw in &raws {
        if let Raw::ModuleItem { module, name } = raw
            && let Some(prev) = type_modules.insert(name.clone(), module.as_str())
        {
            assert_eq!(
                prev, module,
                "pub type {name} is declared in two modules ({prev} and \
                 {module}); the canonical-naming scheme assumes unique \
                 type names across the crate"
            );
        }
    }

    // The canonical path of a module-level name: the crate root when lib.rs
    // re-exports it, else its defining module.
    let canon = |module: &str, name: &str| -> &'static str {
        let path = if module.is_empty() || aliases.contains(name) {
            format!("corvid::{name}")
        } else {
            format!("corvid::{module}::{name}")
        };
        Box::leak(path.into_boxed_str())
    };

    let mut out: BTreeSet<&'static str> = BTreeSet::new();
    for raw in &raws {
        match raw {
            Raw::ModuleItem { module, name } => {
                out.insert(canon(module, name));
            }
            Raw::Variant {
                module,
                ty,
                variant,
            } => {
                let type_module = type_modules.get(ty).copied().unwrap_or(module.as_str());
                let type_path = canon(type_module, ty);
                out.insert(Box::leak(
                    format!("{type_path}::{variant}").into_boxed_str(),
                ));
            }
            Raw::Method { ty, name } => {
                // Only pub fns on pub types are public surface: a `pub fn` in
                // an impl of a `pub(crate)` type is crate-internal.
                if let Some(module) = type_modules.get(ty) {
                    let type_path = canon(module, ty);
                    out.insert(Box::leak(format!("{type_path}::{name}").into_boxed_str()));
                }
            }
        }
    }
    out
}

/// All `fn` names defined anywhere under `dir` (for the test-existence radar).
fn collect_test_fns(dir: &Path) -> BTreeSet<&'static str> {
    let mut out = BTreeSet::new();
    for file in rust_files(dir) {
        let src =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let toks = tokenize(&src);
        for pair in toks.windows(2) {
            if matches!(&pair[0], Tok::Ident(kw) if kw == "fn")
                && let Tok::Ident(name) = &pair[1]
            {
                let leaked: &'static str = Box::leak(name.as_str().to_owned().into_boxed_str());
                out.insert(leaked);
            }
        }
    }
    out
}

/// Every `.rs` file under `dir` (recursively), sorted for determinism.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The module path of a source file: `src/lib.rs` is the crate root; a file
/// `src/a/b.rs` (or `src/a/b/mod.rs`) is module `a::b`.
fn module_of(file: &Path) -> String {
    let rel = file
        .strip_prefix(crate_dir().join("src"))
        .expect("src file outside src/");
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.pop().unwrap_or_default();
    if let Some(stem) = last.strip_suffix(".rs")
        && last != "lib.rs"
        && last != "mod.rs"
        && !stem.is_empty()
    {
        parts.push(stem.to_owned());
    }
    parts.join("::")
}

// ===========================================================================
// Tokenizer: comments stripped, literals elided, punctuation single-char.
// ===========================================================================

#[derive(Clone, PartialEq, Eq, Debug)]
enum Tok {
    Ident(String),
    Punct(char),
    /// A string / raw string / char / byte / number literal, elided.
    Literal,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn char_at(s: &[char], i: usize) -> Option<char> {
    s.get(i).copied()
}

fn tokenize(src: &str) -> Vec<Tok> {
    let s: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && char_at(&s, i + 1) == Some('/') {
            i = skip_while(&s, i, |c| c != '\n');
        } else if c == '/' && char_at(&s, i + 1) == Some('*') {
            i = skip_block_comment(&s, i);
        } else if c == '"' {
            i = skip_string(&s, i);
            toks.push(Tok::Literal);
        } else if (c == 'b' || c == 'r') && literal_starts_at(&s, i).is_some() {
            i = literal_starts_at(&s, i).unwrap();
            toks.push(Tok::Literal);
        } else if c == '\'' {
            if let Some(end) = char_literal_end(&s, i) {
                i = end;
                toks.push(Tok::Literal);
            } else {
                // A lifetime: consume the `'` and its identifier so it can
                // never be mistaken for a type name.
                i = skip_while(&s, i + 1, is_ident_char);
                toks.push(Tok::Punct('\''));
            }
        } else if is_ident_start(c) {
            let end = skip_while(&s, i, is_ident_char);
            toks.push(Tok::Ident(s[i..end].iter().collect()));
            i = end;
        } else if c.is_ascii_digit() {
            i = skip_while(&s, i, |c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
            toks.push(Tok::Literal);
        } else {
            toks.push(Tok::Punct(c));
            i += 1;
        }
    }
    toks
}

fn skip_while(s: &[char], i: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut i = i;
    while i < s.len() && pred(s[i]) {
        i += 1;
    }
    i
}

fn skip_block_comment(s: &[char], i: usize) -> usize {
    let mut depth = 1;
    let mut i = i + 2;
    while i < s.len() && depth > 0 {
        if s[i] == '/' && char_at(s, i + 1) == Some('*') {
            depth += 1;
            i += 2;
        } else if s[i] == '*' && char_at(s, i + 1) == Some('/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_string(s: &[char], i: usize) -> usize {
    let mut i = i + 1;
    while i < s.len() {
        match s[i] {
            '\\' => i += 2,
            '"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// If `i` starts a `b"…"`, `b'…'`, or raw string `r"…"` / `r#"…"#`, the index
/// just past it; else `None`. (Raw identifiers `r#ident` do not occur in this
/// codebase and are not supported by the extractor.)
fn literal_starts_at(s: &[char], i: usize) -> Option<usize> {
    match s[i] {
        'b' => match char_at(s, i + 1) {
            Some('"') => Some(skip_string(s, i + 1)),
            Some('\'') => char_literal_end(s, i + 1),
            _ => None,
        },
        'r' => {
            let mut j = i + 1;
            let mut hashes = 0;
            while char_at(s, j) == Some('#') {
                hashes += 1;
                j += 1;
            }
            if char_at(s, j) != Some('"') {
                return None;
            }
            let mut k = j + 1;
            while k < s.len() {
                if s[k] == '"' && (0..hashes).all(|h| char_at(s, k + 1 + h) == Some('#')) {
                    return Some(k + 1 + hashes);
                }
                k += 1;
            }
            None
        }
        _ => None,
    }
}

/// End of a char literal whose opening `'` is at `i`, if it is one (as
/// opposed to a lifetime like `'a` or `'_`). Scans to the line's end at most:
/// a char literal cannot span lines.
fn char_literal_end(s: &[char], i: usize) -> Option<usize> {
    match char_at(s, i + 1) {
        Some('\\') => {
            let mut j = i + 2;
            while j < s.len() {
                match s[j] {
                    '\n' => return None,
                    '\\' => j += 2,
                    '\'' => return Some(j + 1),
                    _ => j += 1,
                }
            }
            None
        }
        Some(c) if c != '\'' && char_at(s, i + 2) == Some('\'') => Some(i + 3),
        _ => None,
    }
}

// ===========================================================================
// Item parser.
// ===========================================================================

/// A position within a token slice.
struct Cursor<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(toks: &'a [Tok]) -> Self {
        Cursor { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn at_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(p)) if *p == c)
    }

    fn at_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(i)) if i == s)
    }

    /// Skip a balanced group whose opening token is at the cursor.
    fn skip_group(&mut self, open: char, close: char) {
        let mut depth = 0usize;
        while let Some(t) = self.peek() {
            if let Tok::Punct(p) = t {
                if *p == open {
                    depth += 1;
                } else if *p == close {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return;
                    }
                }
            }
            self.pos += 1;
        }
    }

    /// The token index just past the balanced group opening at the cursor.
    fn group_end(&self, open: char, close: char) -> usize {
        let mut probe = Cursor {
            toks: self.toks,
            pos: self.pos,
        };
        probe.skip_group(open, close);
        probe.pos
    }

    /// Skip the remainder of an item: through to `;`, or through its balanced
    /// `{...}` body (plus an optional trailing `;`).
    fn skip_item(&mut self) {
        while let Some(t) = self.peek() {
            match t {
                Tok::Punct(';') => {
                    self.pos += 1;
                    return;
                }
                Tok::Punct('{') => {
                    self.skip_group('{', '}');
                    if self.at_punct(';') {
                        self.pos += 1;
                    }
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }
}

/// The visibility of an item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vis {
    Public,
    Other,
}

/// Parse one scope's items — a file body, a `pub mod` body, or an inherent
/// impl body (with `impl_ty` naming the self type) — recording public
/// constructs into `raws` and crate-root re-exports into `aliases`.
fn parse_scope(
    toks: &[Tok],
    module: &str,
    impl_ty: Option<&str>,
    raws: &mut Vec<Raw>,
    aliases: &mut BTreeSet<String>,
) {
    let mut c = Cursor::new(toks);
    while c.peek().is_some() {
        // Attributes preceding the item. `#[cfg(test)]` hides the item from
        // the public surface; inner attributes (`#![...]`) are just skipped.
        let mut cfg_test = false;
        while c.at_punct('#') {
            c.pos += 1;
            if c.at_punct('!') {
                c.pos += 1;
            }
            if c.at_punct('[') {
                let end = c.group_end('[', ']');
                if is_cfg_test(&c.toks[c.pos..end]) {
                    cfg_test = true;
                }
                c.pos = end;
            }
        }
        if c.peek().is_none() {
            break;
        }
        let vis = parse_vis(&mut c);
        let Some(Tok::Ident(kw)) = c.peek().cloned() else {
            c.skip_item();
            continue;
        };
        c.pos += 1;
        let public = vis == Vis::Public && !cfg_test;
        match kw.as_str() {
            "use" => {
                if public && module.is_empty() && impl_ty.is_none() {
                    parse_root_use(&mut c, aliases);
                }
                c.skip_item();
            }
            "mod" => {
                let Some(name) = next_ident(&mut c) else {
                    c.skip_item();
                    continue;
                };
                if public && impl_ty.is_none() && c.at_punct('{') {
                    let end = c.group_end('{', '}');
                    let inner = if module.is_empty() {
                        name
                    } else {
                        format!("{module}::{name}")
                    };
                    parse_scope(&c.toks[c.pos + 1..end - 1], &inner, None, raws, aliases);
                    c.pos = end;
                } else {
                    // Private or test-only module (`mod tests`): its contents
                    // are not public surface. `pub mod x;` file modules are
                    // walked as files instead.
                    c.skip_item();
                }
            }
            "fn" => {
                if public && let Some(name) = next_ident(&mut c) {
                    record(impl_ty, module, name, raws);
                }
                c.skip_item();
            }
            "const" | "static" => {
                if public {
                    if c.at_ident("mut") {
                        c.pos += 1;
                    }
                    if let Some(name) = next_ident(&mut c) {
                        record(impl_ty, module, name, raws);
                    }
                }
                c.skip_item();
            }
            "type" | "struct" | "trait" => {
                if public && let Some(name) = next_ident(&mut c) {
                    record(impl_ty, module, name, raws);
                }
                c.skip_item();
            }
            "enum" => {
                if public && let Some(name) = next_ident(&mut c) {
                    record(impl_ty, module, name.clone(), raws);
                    if c.at_punct('<') {
                        c.skip_group('<', '>');
                    }
                    if c.at_punct('{') {
                        parse_enum_body(&mut c, module, &name, raws);
                        continue;
                    }
                }
                c.skip_item();
            }
            "impl" => {
                if c.at_punct('<') {
                    c.skip_group('<', '>');
                }
                let mut header = Vec::new();
                while !c.at_punct('{') && c.peek().is_some() {
                    header.push(c.peek().unwrap().clone());
                    c.pos += 1;
                }
                // A trait impl (`impl Trait for Type`) has no inherent items
                // — visibility qualifiers are illegal in trait impls — so its
                // body is skipped wholesale.
                let is_trait_impl = header
                    .iter()
                    .any(|t| matches!(t, Tok::Ident(k) if k == "for"));
                if !is_trait_impl && let Some(ty) = first_type_ident(&header) {
                    let end = c.group_end('{', '}');
                    parse_scope(
                        &c.toks[c.pos + 1..end - 1],
                        module,
                        Some(&ty),
                        raws,
                        aliases,
                    );
                    c.pos = end;
                } else {
                    c.skip_item();
                }
            }
            _ => c.skip_item(),
        }
    }
}

fn record(impl_ty: Option<&str>, module: &str, name: String, raws: &mut Vec<Raw>) {
    match impl_ty {
        Some(ty) => raws.push(Raw::Method {
            ty: ty.to_owned(),
            name,
        }),
        None => raws.push(Raw::ModuleItem {
            module: module.to_owned(),
            name,
        }),
    }
}

/// Consume a visibility prefix, reporting whether it was plain `pub`
/// (`pub(crate)`, `pub(super)`, `pub(in …)` are not public surface).
fn parse_vis(c: &mut Cursor) -> Vis {
    if !c.at_ident("pub") {
        return Vis::Other;
    }
    c.pos += 1;
    if c.at_punct('(') {
        c.skip_group('(', ')');
        Vis::Other
    } else {
        Vis::Public
    }
}

/// Parse a crate-root `pub use` into re-export aliases: `pub use m::{A, B};`
/// contributes `A` and `B`; `pub use m::item;` contributes `item`. Renames
/// (`as`) contribute the alias.
fn parse_root_use(c: &mut Cursor, aliases: &mut BTreeSet<String>) {
    let mut last: Option<String> = None;
    while let Some(t) = c.peek().cloned() {
        match t {
            Tok::Punct('{') => {
                let end = c.group_end('{', '}');
                let mut i = c.pos + 1;
                while i < end - 1 {
                    if let Some(Tok::Ident(name)) = c.toks.get(i) {
                        if name == "as" {
                            if let Some(Tok::Ident(alias)) = c.toks.get(i + 1) {
                                aliases.insert(alias.clone());
                            }
                        } else {
                            aliases.insert(name.clone());
                        }
                    }
                    i += 1;
                }
                c.pos = end;
                return;
            }
            Tok::Punct(';') => {
                if let Some(name) = last {
                    aliases.insert(name);
                }
                return;
            }
            Tok::Ident(name) => {
                last = Some(name);
                c.pos += 1;
            }
            _ => {
                c.pos += 1;
            }
        }
    }
}

/// Parse an enum body (cursor on `{`), recording each variant name.
fn parse_enum_body(c: &mut Cursor, module: &str, ty: &str, raws: &mut Vec<Raw>) {
    c.pos += 1; // consume '{'
    loop {
        while c.at_punct('#') {
            c.pos += 1;
            if c.at_punct('[') {
                c.pos = c.group_end('[', ']');
            }
        }
        if c.at_punct('}') {
            c.pos += 1;
            return;
        }
        let Some(Tok::Ident(variant)) = c.peek().cloned() else {
            c.pos += 1;
            continue;
        };
        c.pos += 1;
        if c.at_punct('(') {
            c.pos = c.group_end('(', ')');
        } else if c.at_punct('{') {
            c.pos = c.group_end('{', '}');
        }
        // Anything else before the separator (e.g. a discriminant `= expr`).
        while let Some(t) = c.peek() {
            match t {
                Tok::Punct(',') => {
                    c.pos += 1;
                    break;
                }
                Tok::Punct('}') => {
                    c.pos += 1;
                    raws.push(Raw::Variant {
                        module: module.to_owned(),
                        ty: ty.to_owned(),
                        variant,
                    });
                    return;
                }
                _ => c.pos += 1,
            }
        }
        raws.push(Raw::Variant {
            module: module.to_owned(),
            ty: ty.to_owned(),
            variant,
        });
    }
}

/// The identifier after the cursor, if one is there (advancing past it).
fn next_ident(c: &mut Cursor) -> Option<String> {
    if let Some(Tok::Ident(name)) = c.peek() {
        let name = name.clone();
        c.pos += 1;
        Some(name)
    } else {
        None
    }
}

/// The base name of the first type in an impl header's token tail: its last
/// path segment (generics and lifetimes already elided by the tokenizer).
fn first_type_ident(header: &[Tok]) -> Option<String> {
    let mut i = 0;
    while i < header.len() {
        if let Tok::Ident(name) = &header[i] {
            if matches!(name.as_str(), "where" | "dyn") {
                return None;
            }
            let mut out = name.clone();
            let mut j = i + 1;
            while matches!(header.get(j), Some(Tok::Punct(':')))
                && matches!(header.get(j + 1), Some(Tok::Punct(':')))
                && let Some(Tok::Ident(next)) = header.get(j + 2)
            {
                out = next.clone();
                j += 3;
            }
            return Some(out);
        }
        i += 1;
    }
    None
}

/// Whether an attribute's bracket body is exactly `cfg(test)`.
fn is_cfg_test(body: &[Tok]) -> bool {
    matches!(
        body,
        [Tok::Ident(a), Tok::Punct('('), Tok::Ident(b), Tok::Punct(')')]
            if a == "cfg" && b == "test"
    )
}
