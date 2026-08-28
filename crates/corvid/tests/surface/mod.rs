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
use std::panic::{AssertUnwindSafe, catch_unwind};
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
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
        ],
    ),
    row(
        "corvid::Value::Null",
        "WHERE",
        &[
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
        ],
    ),
    row(
        "corvid::Value::Bool",
        "WHERE",
        &[
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_unordered_kinds_compare_false_for_ordered_ops",
        ],
    ),
    row(
        "corvid::Value::Int",
        "WHERE",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_int_float_precision_beyond_2_pow_53",
        ],
    ),
    row(
        "corvid::Value::Float",
        "WHERE",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_nan_comparisons_all_false_except_ne",
            "filters_int_float_precision_beyond_2_pow_53",
        ],
    ),
    row(
        "corvid::Value::Text",
        "WHERE",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_text_ordering_lexicographic_utf8",
        ],
    ),
    row(
        "corvid::Value::Bytes",
        "WHERE",
        &[
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
        ],
    ),
    row(
        "corvid::Value::Array",
        "WHERE",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_nested_dotted_paths_traverse_maps_only",
        ],
    ),
    row(
        "corvid::Value::Map",
        "WHERE",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_nested_dotted_paths_traverse_maps_only",
        ],
    ),
    row(
        "corvid::Value::Vector",
        "WHERE",
        &[
            "search_vector_smoke_ranks_nearest_first_exact",
            "mutations_insert_roundtrips_every_value_variant",
            "filters_compare_eq_matches_each_value_kind",
            "filters_unordered_kinds_compare_false_for_ordered_ops",
        ],
    ),
    row(
        "corvid::value::MAX_NESTING",
        "Lifecycle",
        &["lifecycle_value_decode_enforces_max_nesting_bound"],
    ),
    row(
        "corvid::Value::encode",
        "Lifecycle",
        &[
            "lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact",
            "lifecycle_value_decode_enforces_max_nesting_bound",
        ],
    ),
    row(
        "corvid::Value::decode",
        "Lifecycle",
        &[
            "lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact",
            "lifecycle_value_decode_enforces_max_nesting_bound",
        ],
    ),
    row(
        "corvid::Value::get",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::get_path",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_bool",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_int",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_float",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_text",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_bytes",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    row(
        "corvid::Value::as_vector",
        "WHERE",
        &["filters_value_accessors_read_stored_kinds"],
    ),
    // ===== filter.rs — predicates =====
    row(
        "corvid::CmpOp",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::CmpOp::Eq",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_update_maintains_scalar_index",
            "filters_compare_eq_matches_each_value_kind",
        ],
    ),
    row(
        "corvid::CmpOp::Ne",
        "WHERE",
        &[
            "filters_compare_ne_and_missing_path_semantics",
            "filters_nan_comparisons_all_false_except_ne",
        ],
    ),
    row(
        "corvid::CmpOp::Lt",
        "WHERE",
        &[
            "filters_ordered_comparisons_numbers_and_edges",
            "filters_text_ordering_lexicographic_utf8",
        ],
    ),
    row(
        "corvid::CmpOp::Le",
        "WHERE",
        &[
            "filters_ordered_comparisons_numbers_and_edges",
            "filters_between_inclusive_and_degenerate_bounds",
        ],
    ),
    row(
        "corvid::CmpOp::Gt",
        "WHERE",
        &[
            "filters_ordered_comparisons_numbers_and_edges",
            "filters_text_ordering_lexicographic_utf8",
        ],
    ),
    row(
        "corvid::CmpOp::Ge",
        "WHERE",
        &[
            "mutations_delete_where_counts_zero_partial_and_full",
            "filters_ordered_comparisons_numbers_and_edges",
            "filters_between_inclusive_and_degenerate_bounds",
        ],
    ),
    row(
        "corvid::Predicate",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_delete_where_counts_zero_partial_and_full",
            "filters_and_or_not_nesting_and_de_morgan",
        ],
    ),
    row(
        "corvid::Predicate::Compare",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "filters_compare_eq_matches_each_value_kind",
            "filters_ordered_comparisons_numbers_and_edges",
        ],
    ),
    row(
        "corvid::Predicate::Exists",
        "WHERE",
        &["filters_exists_presence_semantics"],
    ),
    row(
        "corvid::Predicate::In",
        "WHERE",
        &["filters_in_membership_matrix"],
    ),
    row(
        "corvid::Predicate::Between",
        "WHERE",
        &["filters_between_inclusive_and_degenerate_bounds"],
    ),
    row(
        "corvid::Predicate::StartsWith",
        "WHERE",
        &["filters_starts_with_prefix_semantics"],
    ),
    row(
        "corvid::Predicate::Contains",
        "WHERE",
        &["filters_contains_substring_semantics"],
    ),
    row(
        "corvid::Predicate::GeoWithin",
        "Geo",
        &[
            "filters_geo_within_point_formats_and_boundary",
            "filters_indexed_vs_scan_geo_window",
            "geo_predicate_deep_dateline_poles_invalid_centers",
        ],
    ),
    row(
        "corvid::Predicate::And",
        "WHERE",
        &[
            "filters_and_or_not_nesting_and_de_morgan",
            "filters_predicate_combinators_and_direct_construction",
        ],
    ),
    row(
        "corvid::Predicate::Or",
        "WHERE",
        &[
            "filters_and_or_not_nesting_and_de_morgan",
            "filters_indexed_vs_scan_or_union",
        ],
    ),
    row(
        "corvid::Predicate::Not",
        "WHERE",
        &[
            "filters_and_or_not_nesting_and_de_morgan",
            "filters_predicate_combinators_and_direct_construction",
        ],
    ),
    row(
        "corvid::Predicate::and",
        "WHERE",
        &[
            "filters_predicate_combinators_and_direct_construction",
            "filters_multiple_filter_calls_intersect_like_and",
        ],
    ),
    row(
        "corvid::Predicate::or",
        "WHERE",
        &[
            "filters_predicate_combinators_and_direct_construction",
            "filters_indexed_vs_scan_or_union",
        ],
    ),
    row(
        "corvid::Predicate::eval",
        "WHERE",
        &[
            "filters_compare_eq_matches_each_value_kind",
            "filters_ordered_comparisons_numbers_and_edges",
            "filters_starts_with_prefix_semantics",
        ],
    ),
    row(
        "corvid::field",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_delete_where_counts_zero_partial_and_full",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::eq",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_update_maintains_scalar_index",
            "filters_field_builders_produce_claimed_predicates",
            "filters_compare_eq_matches_each_value_kind",
        ],
    ),
    row(
        "corvid::filter::FieldRef::ne",
        "WHERE",
        &[
            "filters_field_builders_produce_claimed_predicates",
            "filters_compare_ne_and_missing_path_semantics",
        ],
    ),
    row(
        "corvid::filter::FieldRef::lt",
        "WHERE",
        &[
            "filters_field_builders_produce_claimed_predicates",
            "filters_ordered_comparisons_numbers_and_edges",
        ],
    ),
    row(
        "corvid::filter::FieldRef::le",
        "WHERE",
        &[
            "filters_field_builders_produce_claimed_predicates",
            "filters_ordered_comparisons_numbers_and_edges",
        ],
    ),
    row(
        "corvid::filter::FieldRef::gt",
        "WHERE",
        &[
            "filters_field_builders_produce_claimed_predicates",
            "filters_ordered_comparisons_numbers_and_edges",
        ],
    ),
    row(
        "corvid::filter::FieldRef::ge",
        "WHERE",
        &[
            "mutations_delete_where_counts_zero_partial_and_full",
            "filters_field_builders_produce_claimed_predicates",
            "filters_ordered_comparisons_numbers_and_edges",
        ],
    ),
    row(
        "corvid::filter::FieldRef::exists",
        "WHERE",
        &[
            "filters_exists_presence_semantics",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::is_in",
        "WHERE",
        &[
            "filters_in_membership_matrix",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::between",
        "WHERE",
        &[
            "filters_between_inclusive_and_degenerate_bounds",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::starts_with",
        "WHERE",
        &[
            "filters_starts_with_prefix_semantics",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::contains",
        "WHERE",
        &[
            "filters_contains_substring_semantics",
            "filters_field_builders_produce_claimed_predicates",
        ],
    ),
    row(
        "corvid::filter::FieldRef::within_km",
        "Geo",
        &[
            "filters_geo_within_point_formats_and_boundary",
            "filters_indexed_vs_scan_geo_window",
            "geo_predicate_deep_dateline_poles_invalid_centers",
        ],
    ),
    // ===== distance.rs — vector metrics =====
    row(
        "corvid::Metric",
        "Vector search",
        &[
            "search_vector_smoke_ranks_nearest_first_exact",
            "vector_metric_quantization_cross_orders_and_exact_distances",
        ],
    ),
    row(
        "corvid::Metric::Cosine",
        "Vector search",
        &[
            "search_vector_smoke_ranks_nearest_first_exact",
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_zero_norm_cosine_dot_l2_ranking",
        ],
    ),
    row(
        "corvid::Metric::Dot",
        "Vector search",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_exact_path_scores_match_hand_computed_formulas",
            "vector_zero_norm_cosine_dot_l2_ranking",
        ],
    ),
    row(
        "corvid::Metric::L2",
        "Vector search",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_exact_path_scores_match_hand_computed_formulas",
            "vector_hnsw_direct_api_extreme_params_and_determinism",
        ],
    ),
    row(
        "corvid::Metric::distance",
        "Vector search",
        &["vector_exact_path_scores_match_hand_computed_formulas"],
    ),
    row(
        "corvid::distance::dot",
        "Vector search",
        &["vector_exact_path_scores_match_hand_computed_formulas"],
    ),
    row(
        "corvid::distance::l2_squared",
        "Vector search",
        &["vector_exact_path_scores_match_hand_computed_formulas"],
    ),
    row(
        "corvid::distance::cosine_distance",
        "Vector search",
        &[
            "vector_exact_path_scores_match_hand_computed_formulas",
            "vector_zero_norm_cosine_dot_l2_ranking",
        ],
    ),
    // ===== quant.rs — vector quantization =====
    row(
        "corvid::Quantization",
        "Vector search",
        &["vector_metric_quantization_cross_orders_and_exact_distances"],
    ),
    row(
        "corvid::Quantization::None",
        "Vector search",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_indexed_none_matches_unindexed_twin_for_all_k",
        ],
    ),
    row(
        "corvid::Quantization::Binary",
        "Vector search",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_quantization_k1_binary_diverges_scalar_matches_exact",
            "vector_create_index_overloads_inmemory_ondisk_and_pq",
        ],
    ),
    row(
        "corvid::Quantization::Scalar",
        "Vector search",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_quantization_k1_binary_diverges_scalar_matches_exact",
        ],
    ),
    // ===== error.rs — error vocabulary =====
    row(
        "corvid::Result",
        "Lifecycle",
        // The alias is the error type of every engine call; named explicitly
        // by the Store surface test's bindings.
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Error",
        "Lifecycle",
        &[
            "schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing",
            "mutations_insert_rejects_reserved_and_invalid_collection_names",
        ],
    ),
    row(
        "corvid::Error::Database",
        "Lifecycle",
        // Shared variant (conformance convention 1): the redb file-creation
        // and file-lock failures wrap into it — driven by open under a
        // missing parent dir, the second-handle exclusive lock, and backup
        // into a bad path.
        &[
            "lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent",
            "lifecycle_db_open_second_handle_to_same_file_hits_the_redb_exclusive_lock",
            "lifecycle_db_backup_restores_identical_state_and_pins_error_paths",
        ],
    ),
    row("corvid::Error::Transaction", "Lifecycle", &[]),
    row("corvid::Error::Table", "Lifecycle", &[]),
    row("corvid::Error::Storage", "Lifecycle", &[]),
    row("corvid::Error::Commit", "Lifecycle", &[]),
    row("corvid::Error::SetDurability", "Lifecycle", &[]),
    row("corvid::Error::Compaction", "Lifecycle", &[]),
    row(
        "corvid::Error::Decode",
        "Lifecycle",
        // Task 13: driven through the public codec and through storage (a
        // too-deep raw record surfaces the same variant from Collection::get).
        &["lifecycle_value_decode_enforces_max_nesting_bound"],
    ),
    row(
        "corvid::Error::CorruptIndex",
        "Schema (ALTER)",
        // Task 11 routing (Task 13): corrupted on-disk index bytes in a real
        // db file → reopen → query → exact variant.
        &["lifecycle_corrupt_ondisk_index_bytes_on_disk_error_queries_with_exact_variant"],
    ),
    row(
        "corvid::Error::ReservedCollection",
        "Schema (ALTER)",
        // Shared variant (conformance convention 1): raised by every write
        // path's writability gate — the mutation tests cover the document
        // paths, the graph test covers link/link_weighted/unlink, and the
        // schema test covers every index-creation family.
        &[
            "mutations_insert_rejects_reserved_and_invalid_collection_names",
            "mutations_write_paths_reject_reserved_and_invalid_collection_names",
            "graph_link_unlink_reject_reserved_and_invalid_collection_names",
            "schema_index_creation_validates_names_across_families",
        ],
    ),
    row(
        "corvid::Error::InvalidName",
        "Schema (ALTER)",
        // Shared variant (conformance convention 1): document writes and
        // graph writes (link/unlink) raise it through the same
        // `ensure_writable` gate, and the schema test drives it through every
        // index-creation API's field-name validation (plus set_schema).
        &[
            "mutations_insert_rejects_reserved_and_invalid_collection_names",
            "mutations_write_paths_reject_reserved_and_invalid_collection_names",
            "graph_link_unlink_reject_reserved_and_invalid_collection_names",
            "schema_index_creation_validates_names_across_families",
        ],
    ),
    row(
        "corvid::Error::InvalidArgument",
        "Hybrid",
        // Shared variant (conformance convention 1): raised by the hybrid
        // ranking params (fuse_rrf k, rerank_mmr lambda) at every execution
        // entry point, by the text-side Bm25Params domain checks, by the geo
        // bbox bound validation (the only validating geo entry point), and
        // by the aggregate entry points, which validate the fluent args
        // before touching the store.
        &[
            "aggregations_validate_ranking_args_before_aggregating",
            "hybrid_fuse_rrf_rejects_invalid_k_at_run",
            "hybrid_rerank_mmr_rejects_out_of_range_and_nan_at_run",
            "text_bm25_params_new_and_validate_error_variants",
            "geo_bbox_validation_exact_error_variants",
        ],
    ),
    row("corvid::Error::IncompatibleFormat", "Lifecycle", &[]),
    row(
        "corvid::Error::EmptyIndexTraining",
        "Schema (ALTER)",
        &[
            "vector_create_index_overloads_inmemory_ondisk_and_pq",
            "schema_vector_pq_creation_training_error_variants_and_success",
        ],
    ),
    row(
        "corvid::Error::SchemaViolation",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing",
            "schema_unique_update_conflict_rejects_whole_write",
            "schema_unique_nan_equals_nan_rejects_second_document",
            "schema_unique_containers_bytes_text_vector_and_null_rule",
            "schema_unique_delete_then_reinsert_same_value_allowed",
            "schema_unique_batch_conflict_rolls_back_whole_batch",
            "schema_unique_with_scalar_index_stays_enforced_and_moves_with_values",
            "schema_unique_numeric_kind_equality_same_with_and_without_index",
            "mutations_insert_batch_schema_violation_rolls_back_whole_batch",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
            "mutations_insert_auto_failure_does_not_burn_an_id",
        ],
    ),
    row(
        "corvid::Error::InvalidDump",
        "Lifecycle",
        // Reserved names, bad magic, and truncated streams — the engine-level
        // pins (corvid-mcp drives the wire form in Task 14).
        &["lifecycle_load_rejects_reserved_names_and_malformed_streams"],
    ),
    row(
        "corvid::Error::BackupTargetExists",
        "Lifecycle",
        &[
            "lifecycle_db_backup_restores_identical_state_and_pins_error_paths",
            "lifecycle_store_backup_copies_to_an_independent_openable_file",
        ],
    ),
    row(
        "corvid::Error::Io",
        "Lifecycle",
        // Task 13: dump to a failing Write and load from a failing Read both
        // surface the exact variant.
        &["lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface"],
    ),
    // ===== builder.rs — the fluent query language =====
    row(
        "corvid::ResultRow",
        "SELECT shaping",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "queries_result_row_fields_per_query_shape",
            "queries_run_select_only_returns_all_in_key_order",
            "queries_select_preserves_rank_scores_and_filter_visibility",
        ],
    ),
    row(
        "corvid::PlanShape",
        "Lifecycle",
        &[
            "filters_indexed_vs_scan_scalar_predicates",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
        ],
    ),
    row(
        "corvid::PlanShape::AnnIndex",
        "Lifecycle",
        &["queries_plan_shape_ann_index_for_single_vector_source"],
    ),
    row(
        "corvid::PlanShape::TextIndex",
        "Lifecycle",
        &["queries_plan_shape_text_index_for_single_text_source"],
    ),
    row(
        "corvid::PlanShape::IndexedWindow",
        "Lifecycle",
        &[
            "filters_indexed_vs_scan_scalar_predicates",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
        ],
    ),
    row(
        "corvid::PlanShape::StreamingTopK",
        "Lifecycle",
        &["queries_plan_shape_streaming_topk_without_index"],
    ),
    row(
        "corvid::PlanShape::Scan",
        "Lifecycle",
        &[
            "filters_indexed_vs_scan_scalar_predicates",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
        ],
    ),
    row(
        "corvid::QueryBuilder",
        "SELECT shaping",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "queries_run_select_only_returns_all_in_key_order",
        ],
    ),
    row(
        "corvid::Collection::query",
        "SELECT shaping",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "queries_run_select_only_returns_all_in_key_order",
        ],
    ),
    row(
        "corvid::QueryBuilder::filter",
        "WHERE",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_update_maintains_scalar_index",
        ],
    ),
    row(
        "corvid::QueryBuilder::vector",
        "Vector search",
        &[
            "search_hybrid_smoke_rrf_fuses_vector_and_text",
            "vector_builder_approx_prefilters_exact_vs_postfilters_approx",
            "vector_k_boundaries_zero_one_n_and_beyond",
            "vector_empty_collection_single_doc_and_missing_field",
            "vector_builder_select_order_limit_offset_interplay",
        ],
    ),
    row(
        "corvid::QueryBuilder::text",
        "Text search",
        &[
            "search_hybrid_smoke_rrf_fuses_vector_and_text",
            "text_builder_text_k_bounds_empty_and_stopword_queries",
            "text_builder_text_ranking_scores_select_limit_and_missing_fields",
            "text_builder_text_index_arm_matches_scan_arm",
        ],
    ),
    row(
        "corvid::QueryBuilder::fuse_rrf",
        "Hybrid",
        &[
            "hybrid_fuse_rrf_rejects_invalid_k_at_run",
            "hybrid_fusion_rrf_boost_beats_single_source",
        ],
    ),
    row(
        "corvid::QueryBuilder::rerank_mmr",
        "Hybrid",
        &[
            "hybrid_rerank_mmr_rejects_out_of_range_and_nan_at_run",
            "hybrid_rerank_mmr_noop_without_vector_source",
            "hybrid_rerank_mmr_lambda_one_reorders_by_relevance",
            "hybrid_rerank_mmr_lambda_zero_diversifies",
            "hybrid_rerank_mmr_docs_without_embeddings_survive",
        ],
    ),
    row(
        "corvid::QueryBuilder::limit",
        "SELECT shaping",
        &[
            "queries_smoke_order_by_limit_select_shapes_rows",
            "queries_limit_zero_one_exact_and_over_match_count",
            "queries_order_by_limit_offset_window_after_ordering",
            "queries_limit_offset_on_empty_collection",
        ],
    ),
    row(
        "corvid::QueryBuilder::offset",
        "SELECT shaping",
        &[
            "filters_filter_then_limit_offset_pagination",
            "queries_offset_boundaries_and_full_range_pagination_loop",
            "queries_order_by_limit_offset_window_after_ordering",
            "queries_limit_offset_on_empty_collection",
        ],
    ),
    row(
        "corvid::QueryBuilder::order_by",
        "SELECT shaping",
        &[
            "queries_smoke_order_by_limit_select_shapes_rows",
            "queries_order_by_asc_desc_over_int_float_and_text",
            "queries_order_by_class_rule_incomparable_then_missing_last_both_directions",
            "queries_order_by_mixed_kind_field_groups_numbers_before_texts",
            "queries_order_by_limit_offset_window_after_ordering",
            "queries_order_by_with_filters_orders_only_matches",
            "queries_order_by_indexed_vs_scan_equivalent",
        ],
    ),
    row(
        "corvid::QueryBuilder::approx",
        "Vector search",
        &["vector_builder_approx_prefilters_exact_vs_postfilters_approx"],
    ),
    row(
        "corvid::QueryBuilder::select",
        "SELECT shaping",
        &[
            "queries_smoke_order_by_limit_select_shapes_rows",
            "queries_select_single_multiple_and_nested_dotted_paths",
            "queries_select_missing_fields_omitted_and_duplicates_collapse",
            "queries_select_empty_field_list_yields_empty_map_for_map_docs",
            "queries_select_non_map_documents_pass_through_unchanged",
            "queries_select_preserves_rank_scores_and_filter_visibility",
            "schema_select_empty_field_name_matches_get_path_semantics",
        ],
    ),
    row(
        "corvid::QueryBuilder::count",
        "Aggregations",
        &[
            "aggregations_smoke_sum_group_count_and_count",
            "aggregations_count_matrix_filter_empty_and_after_mutations",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
            "mutations_delete_removes_state_from_get_scan_and_count",
            "queries_count_with_filter_and_after_mutations",
        ],
    ),
    row(
        "corvid::QueryBuilder::group_count",
        "Aggregations",
        &[
            "aggregations_smoke_sum_group_count_and_count",
            "aggregations_count_distinct_and_groups_separate_types_by_tag",
            "aggregations_group_count_escapes_every_ambiguous_tag_prefix",
            "aggregations_group_count_zero_signed_floats_share_and_nan_groups",
            "aggregations_group_count_skips_missing_and_container_fields",
            "aggregations_group_count_respects_filters",
            "aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::sum",
        "Aggregations",
        &[
            "aggregations_smoke_sum_group_count_and_count",
            "aggregations_sum_int_float_mixed_and_negative_exact",
            "aggregations_sum_skips_missing_and_non_numeric_missing_all_is_zero",
            "aggregations_sum_nan_poisons_and_infinities_follow_ieee",
            "aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::avg",
        "Aggregations",
        &[
            "aggregations_avg_matrix_single_mixed_skipped_and_empty",
            "aggregations_avg_nan_member_poisons_the_mean",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::min",
        "Aggregations",
        &[
            "aggregations_min_max_numbers_interop_and_text_is_lexicographic",
            "aggregations_min_max_incomparable_kinds_yield_none",
            "aggregations_min_max_mixed_kinds_pin_first_comparable_kind_wins",
            "aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::max",
        "Aggregations",
        &[
            "aggregations_min_max_numbers_interop_and_text_is_lexicographic",
            "aggregations_min_max_incomparable_kinds_yield_none",
            "aggregations_min_max_mixed_kinds_pin_first_comparable_kind_wins",
            "aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::count_distinct",
        "Aggregations",
        &[
            "aggregations_count_distinct_scalars_duplicates_missing_and_empty",
            "aggregations_count_distinct_and_groups_separate_types_by_tag",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
            "aggregations_validate_ranking_args_before_aggregating",
        ],
    ),
    row(
        "corvid::QueryBuilder::group_sum",
        "Aggregations",
        &[
            "aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets",
            "aggregations_group_sum_avg_respect_filters",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
        ],
    ),
    row(
        "corvid::QueryBuilder::group_avg",
        "Aggregations",
        &[
            "aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets",
            "aggregations_group_sum_avg_respect_filters",
            "aggregations_ignore_limit_offset_select_order_by_and_sources",
            "aggregations_indexed_vs_scan_equivalent_for_every_aggregate",
        ],
    ),
    row(
        "corvid::QueryBuilder::plan",
        "Lifecycle",
        &["lifecycle_query_plan_key_is_canonical_for_identical_shapes"],
    ),
    row(
        "corvid::QueryBuilder::plan_shape",
        "Lifecycle",
        &[
            "filters_indexed_vs_scan_scalar_predicates",
            "queries_plan_shape_ann_index_for_single_vector_source",
            "queries_plan_shape_text_index_for_single_text_source",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
            "queries_plan_shape_streaming_topk_without_index",
            "queries_order_by_indexed_vs_scan_equivalent",
        ],
    ),
    row(
        "corvid::QueryBuilder::explain",
        "Lifecycle",
        &[
            "queries_plan_shape_ann_index_for_single_vector_source",
            "queries_plan_shape_text_index_for_single_text_source",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
            "queries_plan_shape_streaming_topk_without_index",
        ],
    ),
    row(
        "corvid::QueryBuilder::run",
        "SELECT shaping",
        &[
            "filters_smoke_field_eq_selects_matching_rows",
            "mutations_update_maintains_scalar_index",
            "queries_run_on_empty_collection_returns_empty_vec",
            "queries_run_select_only_returns_all_in_key_order",
        ],
    ),
    // ===== db.rs — database and collection handles =====
    row(
        "corvid::Db",
        "Lifecycle",
        &[
            "mutations_smoke_insert_roundtrips",
            "lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent",
        ],
    ),
    row(
        "corvid::Db::open",
        "Lifecycle",
        &[
            "lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent",
            "lifecycle_db_open_second_handle_to_same_file_hits_the_redb_exclusive_lock",
        ],
    ),
    row(
        "corvid::Db::open_in_memory",
        "Lifecycle",
        &[
            "mutations_smoke_insert_roundtrips",
            "lifecycle_db_open_in_memory_instances_are_isolated",
        ],
    ),
    row(
        "corvid::Db::collection",
        "Lifecycle",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Db::backup",
        "Lifecycle",
        &["lifecycle_db_backup_restores_identical_state_and_pins_error_paths"],
    ),
    row(
        "corvid::Db::bulk",
        "Lifecycle",
        &[
            "lifecycle_db_bulk_is_a_durability_scope_writes_before_err_persist",
            "lifecycle_db_bulk_happy_path_applies_and_survives_reopen",
        ],
    ),
    row(
        "corvid::Db::compact",
        "Lifecycle",
        &["lifecycle_db_compact_keeps_data_intact_and_tolerates_double_compact"],
    ),
    row(
        "corvid::Db::collections",
        "Lifecycle",
        &["lifecycle_db_collections_filters_graph_ttl_and_index_namespaces"],
    ),
    row(
        "corvid::Collection",
        "Mutations",
        &["mutations_smoke_insert_roundtrips"],
    ),
    row(
        "corvid::Collection::insert",
        "Mutations",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "mutations_insert_overwrites_and_accepts_empty_key_and_empty_map",
            "mutations_insert_rejects_reserved_and_invalid_collection_names",
        ],
    ),
    row(
        "corvid::Collection::update",
        "Mutations",
        &[
            "mutations_update_rewrites_document_to_every_value_kind",
            "mutations_update_on_missing_key_creates_or_stays_absent",
            "mutations_update_maintains_scalar_index",
        ],
    ),
    row(
        "corvid::Collection::patch",
        "Mutations",
        &["mutations_patch_merges_top_level_and_replaces_non_maps"],
    ),
    row(
        "corvid::Collection::compare_and_set",
        "Mutations",
        &[
            "mutations_compare_and_set_swap_noop_delete_and_semantic_float_equality",
            "mutations_compare_and_set_uses_semantic_value_equality",
            "mutations_compare_and_set_maintains_scalar_index",
        ],
    ),
    row(
        "corvid::Collection::for_each_doc",
        "SELECT shaping",
        &[
            "queries_for_each_doc_visits_key_order_and_early_stops_on_false",
            "queries_for_each_doc_on_empty_collection_visits_nothing",
        ],
    ),
    row(
        "corvid::Collection::len",
        "SELECT shaping",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "queries_len_and_is_empty_boundaries",
        ],
    ),
    row(
        "corvid::Collection::is_empty",
        "SELECT shaping",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
            "queries_len_and_is_empty_boundaries",
        ],
    ),
    row(
        "corvid::Collection::insert_batch",
        "Mutations",
        &[
            "mutations_insert_batch_happy_empty_overwrite_and_duplicates",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
            "mutations_insert_batch_schema_violation_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::Collection::insert_auto",
        "Mutations",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_auto_keys_are_unique_zero_padded_and_monotonic_per_collection",
            "mutations_insert_auto_failure_does_not_burn_an_id",
        ],
    ),
    row(
        "corvid::Collection::get",
        "SELECT shaping",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_insert_roundtrips_every_value_variant",
        ],
    ),
    row(
        "corvid::Collection::delete",
        "Mutations",
        &[
            "mutations_smoke_insert_roundtrips",
            "mutations_delete_removes_state_from_get_scan_and_count",
            // W3 wave-review citation: the delete path also cascades graph
            // edges (and does so even on an absent document row).
            "graph_delete_missing_document_still_purges_dangling_edges",
        ],
    ),
    row(
        "corvid::Collection::delete_where",
        "Mutations",
        &[
            "mutations_delete_where_counts_zero_partial_and_full",
            "mutations_delete_where_exact_with_scalar_index_present",
            // W3 wave-review citation: the cascade is shared with delete.
            "graph_delete_where_and_delete_batch_cascade_edges",
        ],
    ),
    row(
        "corvid::Collection::delete_batch",
        "Mutations",
        &[
            "mutations_delete_batch_counts_existing_only_and_accepts_empty",
            // W3 wave-review citation: the cascade is shared with delete.
            "graph_delete_where_and_delete_batch_cascade_edges",
        ],
    ),
    row(
        "corvid::Collection::scan",
        "SELECT shaping",
        &[
            "mutations_delete_removes_state_from_get_scan_and_count",
            "mutations_insert_overwrites_and_accepts_empty_key_and_empty_map",
            "queries_scan_returns_pairs_in_key_order",
        ],
    ),
    row(
        "corvid::Collection::page",
        "SELECT shaping",
        &[
            "queries_page_cursor_semantics",
            "queries_page_after_empty_bytes_skips_only_the_empty_key",
            "queries_page_over_empty_collection_yields_empty_page_with_no_cursor",
        ],
    ),
    row(
        "corvid::Collection::page_where",
        "SELECT shaping",
        &[
            "queries_page_where_predicate_and_full_walk",
            "queries_page_over_empty_collection_yields_empty_page_with_no_cursor",
        ],
    ),
    row(
        "corvid::db::Page",
        "SELECT shaping",
        &[
            "queries_page_cursor_semantics",
            "queries_page_where_predicate_and_full_walk",
        ],
    ),
    // ===== store.rs — the byte KV layer =====
    row(
        "corvid::Store",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::open",
        "Lifecycle",
        &[
            "lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable",
            "lifecycle_store_backup_copies_to_an_independent_openable_file",
        ],
    ),
    row(
        "corvid::Store::open_in_memory",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::Store::set_relaxed_durability",
        "Lifecycle",
        &["lifecycle_store_set_relaxed_durability_and_flush_keep_data_durable"],
    ),
    row(
        "corvid::Store::begin_bulk",
        "Lifecycle",
        &["lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable"],
    ),
    row(
        "corvid::Store::flush",
        "Lifecycle",
        &[
            "lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable",
            "lifecycle_store_set_relaxed_durability_and_flush_keep_data_durable",
        ],
    ),
    row(
        "corvid::Store::compact",
        "Lifecycle",
        &["lifecycle_db_compact_keeps_data_intact_and_tolerates_double_compact"],
    ),
    row(
        "corvid::Store::next_auto_id",
        "Lifecycle",
        &[
            "lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts",
            "lifecycle_store_transaction_commit_rollback_and_write_batch_surface",
        ],
    ),
    row(
        "corvid::Store::backup",
        "Lifecycle",
        &["lifecycle_store_backup_copies_to_an_independent_openable_file"],
    ),
    row(
        "corvid::Store::transaction",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::Store::read",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::Store::put",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::get",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::delete",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::scan",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::collections",
        "Lifecycle",
        &["lifecycle_db_collections_filters_graph_ttl_and_index_namespaces"],
    ),
    row(
        "corvid::Store::scan_from",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::count",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::for_each",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::Store::scan_prefix",
        "Lifecycle",
        &["lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts"],
    ),
    row(
        "corvid::store::BulkScope",
        "Lifecycle",
        &["lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable"],
    ),
    row(
        "corvid::store::WriteBatch",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::put",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::get",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::delete",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::scan",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::scan_from",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::WriteBatch::next_auto_id",
        "Lifecycle",
        &["lifecycle_store_transaction_commit_rollback_and_write_batch_surface"],
    ),
    row(
        "corvid::store::ReadBatch",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::collections",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::auto_ids",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::get",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::scan",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::scan_from",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::scan_prefix",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    row(
        "corvid::store::ReadBatch::for_each",
        "Lifecycle",
        &["lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops"],
    ),
    // ===== query.rs — retrieval primitives =====
    row(
        "corvid::Hit",
        "Vector search",
        &[
            "search_vector_smoke_ranks_nearest_first_exact",
            "vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback",
            "vector_metric_quantization_cross_orders_and_exact_distances",
        ],
    ),
    row(
        "corvid::TextHit",
        "Text search",
        &[
            "search_text_smoke_ranks_most_relevant_first",
            "text_search_bm25_ranking_tf_length_and_ties",
            "text_phrase_order_sensitive_match_and_scores",
        ],
    ),
    row(
        "corvid::Collection::text_search",
        "Text search",
        &[
            "search_text_smoke_ranks_most_relevant_first",
            "text_search_bm25_ranking_tf_length_and_ties",
            "text_search_rare_term_outscores_common_via_idf",
            "text_search_index_inmemory_ondisk_match_scan_twin",
            "text_search_k_boundaries_and_corpus_edges",
            "text_phrase_single_term_equals_term_search",
        ],
    ),
    row(
        "corvid::Collection::phrase_search",
        "Text search",
        &[
            "text_phrase_order_sensitive_match_and_scores",
            "text_phrase_repeated_terms_and_non_adjacent_non_match",
            "text_phrase_single_term_equals_term_search",
            "text_phrase_stopword_collapse_and_sentence_boundary",
            "text_phrase_k_boundaries_empty_phrase_and_index_arms",
        ],
    ),
    row(
        "corvid::Collection::vector_search",
        "Vector search",
        &[
            "search_vector_smoke_ranks_nearest_first_exact",
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "vector_exact_path_scores_match_hand_computed_formulas",
            "vector_indexed_none_matches_unindexed_twin_for_all_k",
            "vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback",
            "vector_k_boundaries_zero_one_n_and_beyond",
            "vector_dimension_mismatch_skips_docs_and_index_falls_back",
            "vector_zero_norm_cosine_dot_l2_ranking",
            "vector_empty_collection_single_doc_and_missing_field",
        ],
    ),
    // ===== text.rs — tokenization and BM25 =====
    row(
        "corvid::text::Bm25Params",
        "Text search",
        &[
            "text_bm25_params_new_and_validate_error_variants",
            "text_term_score_zero_saturation_length_and_b_zero",
        ],
    ),
    row(
        "corvid::text::Bm25Params::new",
        "Text search",
        &["text_bm25_params_new_and_validate_error_variants"],
    ),
    row(
        "corvid::text::Bm25Params::validate",
        "Text search",
        &["text_bm25_params_new_and_validate_error_variants"],
    ),
    row(
        "corvid::text::tokenize",
        "Text search",
        &["text_tokenize_case_punct_unicode_numbers_and_empties"],
    ),
    row(
        "corvid::text::s_stem",
        "Text search",
        &["text_s_stem_pins_conservative_plural_algorithm"],
    ),
    row(
        "corvid::text::Analyzer",
        "Text search",
        &["text_analyzer_default_raw_and_flag_combinations"],
    ),
    row(
        "corvid::text::Analyzer::raw",
        "Text search",
        &["text_analyzer_default_raw_and_flag_combinations"],
    ),
    row(
        "corvid::text::Analyzer::analyze",
        "Text search",
        &["text_analyzer_default_raw_and_flag_combinations"],
    ),
    row(
        "corvid::text::analyze",
        "Text search",
        &["text_analyzer_default_raw_and_flag_combinations"],
    ),
    row(
        "corvid::text::idf",
        "Text search",
        &["text_idf_values_monotonicity_and_nonnegativity"],
    ),
    row(
        "corvid::text::term_score",
        "Text search",
        &["text_term_score_zero_saturation_length_and_b_zero"],
    ),
    // ===== fusion.rs — rank fusion and diversification =====
    row(
        "corvid::DEFAULT_RRF_K",
        "Hybrid",
        &[
            "search_hybrid_smoke_rrf_fuses_vector_and_text",
            "hybrid_rrf_direct_formula_exact_scores_and_edges",
        ],
    ),
    row(
        "corvid::reciprocal_rank_fusion",
        "Hybrid",
        &[
            "search_hybrid_smoke_rrf_fuses_vector_and_text",
            "hybrid_rrf_direct_formula_exact_scores_and_edges",
        ],
    ),
    row(
        "corvid::mmr",
        "Hybrid",
        &["hybrid_mmr_direct_lambda_zero_one_diversity_and_k"],
    ),
    // ===== geo.rs — spatial queries =====
    row(
        "corvid::haversine_km",
        "Geo",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "geo_haversine_known_distances_symmetry_poles_antipodal",
        ],
    ),
    row(
        "corvid::GeoHit",
        "Geo",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "geo_nearest_k_zero_one_n_beyond_and_hit_fields",
        ],
    ),
    row(
        "corvid::Collection::geo_within_radius",
        "Geo",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "geo_within_radius_boundary_inclusive_and_ordering",
            "geo_within_radius_zero_tiny_and_full_globe_radii",
            "geo_within_radius_no_input_validation_mathematical_semantics",
        ],
    ),
    row(
        "corvid::Collection::geo_nearest",
        "Geo",
        &[
            "search_geo_smoke_within_radius_and_nearest",
            "geo_nearest_k_zero_one_n_beyond_and_hit_fields",
            "geo_nearest_equidistant_ties_break_by_key",
            "geo_nearest_skips_non_points_empty_and_finds_antipodal",
        ],
    ),
    row(
        "corvid::Collection::geo_within_bbox",
        "Geo",
        &[
            "geo_within_bbox_normal_inclusive_edges_and_key_order",
            "geo_within_bbox_degenerate_point_line_pole_and_globe",
            "geo_bbox_antimeridian_wrap_matches_both_sides",
            "geo_bbox_result_order_is_key_order_on_every_path",
            "geo_bbox_validation_exact_error_variants",
        ],
    ),
    // ===== geo_index.rs =====
    row(
        "corvid::Collection::create_geo_index",
        "Schema (ALTER)",
        &[
            "filters_indexed_vs_scan_geo_window",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
            "geo_index_twins_equivalence_and_live_mutations",
            "geo_index_plan_shape_serviceable_and_declined",
            "schema_geo_index_duplicate_creation_and_non_point_docs_skipped",
            "schema_geo_index_point_move_and_delete_maintained",
        ],
    ),
    // ===== graph.rs — edges =====
    row(
        "corvid::Collection::link",
        "Graph",
        &[
            "graph_smoke_link_neighbors_traverse_unlink",
            "graph_link_new_edge_resolves_neighbors_and_in_neighbors",
            "graph_link_duplicate_is_idempotent_and_reemits_insert_event",
            "graph_link_self_loop_lists_self_but_traverse_excludes_start",
            "graph_link_missing_endpoints_allowed_without_documents",
            "graph_link_relation_isolation_empty_unicode_and_byte_prefix",
            "graph_link_endpoint_keys_empty_and_unicode_in_byte_order",
            "graph_link_unlink_reject_reserved_and_invalid_collection_names",
            // Task 12 events completeness: linking to absent endpoints
            // still emits the Insert event keyed by `from`.
            "events_link_to_missing_endpoints_still_emits_insert_keyed_by_from",
        ],
    ),
    row(
        "corvid::Collection::link_weighted",
        "Graph",
        &[
            "graph_link_weighted_roundtrip_and_overwrite_semantics",
            "graph_link_weighted_float_extremes_round_trip",
        ],
    ),
    row(
        "corvid::Collection::neighbors_weighted",
        "Graph",
        &[
            "graph_link_weighted_roundtrip_and_overwrite_semantics",
            "graph_link_weighted_float_extremes_round_trip",
        ],
    ),
    row(
        "corvid::Collection::unlink",
        "Graph",
        &[
            "graph_smoke_link_neighbors_traverse_unlink",
            "graph_unlink_removes_edge_and_reverse_twin",
            "graph_unlink_is_directional_reverse_direction_edge_survives",
            "graph_unlink_missing_edge_is_silent_noop_returning_false",
            "graph_unlink_removes_only_the_named_relation",
            "graph_link_unlink_reject_reserved_and_invalid_collection_names",
        ],
    ),
    row(
        "corvid::Collection::neighbors",
        "Graph",
        &[
            "graph_smoke_link_neighbors_traverse_unlink",
            "graph_link_relation_isolation_empty_unicode_and_byte_prefix",
            "graph_link_endpoint_keys_empty_and_unicode_in_byte_order",
            "graph_neighbors_key_order_no_out_edges_and_missing_node",
        ],
    ),
    row(
        "corvid::Collection::in_neighbors",
        "Graph",
        &[
            "graph_link_new_edge_resolves_neighbors_and_in_neighbors",
            "graph_in_neighbors_mirror_target_only_and_mixed",
            "graph_delete_cascades_edges_in_both_namespaces",
        ],
    ),
    row(
        "corvid::Collection::traverse",
        "Graph",
        &[
            "graph_smoke_link_neighbors_traverse_unlink",
            "graph_traverse_depth_zero_empty_depth_one_equals_neighbors",
            "graph_traverse_multi_hop_bfs_order_exact",
            "graph_traverse_cycles_terminate_with_deduped_set",
            "graph_traverse_branching_and_diamond_convergence_order",
            "graph_traverse_relation_isolated_from_other_relations",
            "graph_traverse_missing_start_node_is_empty",
        ],
    ),
    // ===== join.rs =====
    row(
        "corvid::JoinRow",
        "Joins",
        &[
            "joins_smoke_left_outer_resolves_and_misses",
            "joins_happy_path_join_row_shape_is_exact",
            "joins_missing_fk_field_and_dangling_reference_retain_rows_with_none",
            "joins_foreign_key_kinds_text_bytes_int_and_unusable_shapes",
            "joins_self_join_references_within_one_collection",
            "joins_empty_left_empty_right_and_unknown_right_collection",
            "joins_rows_follow_left_collection_key_order",
            "joins_non_map_left_documents_retained_with_none",
        ],
    ),
    row(
        "corvid::Collection::join",
        "Joins",
        &[
            "joins_smoke_left_outer_resolves_and_misses",
            "joins_happy_path_join_row_shape_is_exact",
            "joins_dotted_foreign_key_path_resolves_nested_maps",
            "joins_missing_fk_field_and_dangling_reference_retain_rows_with_none",
            "joins_foreign_key_kinds_text_bytes_int_and_unusable_shapes",
            "joins_self_join_references_within_one_collection",
            "joins_empty_left_empty_right_and_unknown_right_collection",
            "joins_rows_follow_left_collection_key_order",
            "joins_track_right_and_left_side_mutations",
            "joins_non_map_left_documents_retained_with_none",
        ],
    ),
    // ===== hnsw.rs — in-memory ANN =====
    row(
        "corvid::hnsw::DEFAULT_M",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::hnsw::DEFAULT_EF_CONSTRUCTION",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::new",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::with_params",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::with_quant",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::len",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::is_empty",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::insert",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    row(
        "corvid::Hnsw::search",
        "Vector search",
        &["vector_hnsw_direct_api_extreme_params_and_determinism"],
    ),
    // ===== index.rs — vector index creation =====
    row(
        "corvid::Collection::create_vector_index",
        "Schema (ALTER)",
        &[
            "queries_plan_shape_ann_index_for_single_vector_source",
            "vector_indexed_none_matches_unindexed_twin_for_all_k",
            "vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback",
            "vector_dimension_mismatch_skips_docs_and_index_falls_back",
            "schema_vector_index_duplicate_creation_replaces_previous_params",
            "schema_vector_index_over_empty_field_then_insert_immediately_searchable",
            "schema_vector_index_mixed_dimensions_match_scan_twin",
            "schema_vector_dimension_change_on_update_leaves_index",
            "schema_vector_index_compaction_after_deletes_keeps_results_exact",
        ],
    ),
    row(
        "corvid::Collection::create_vector_index_quantized",
        "Schema (ALTER)",
        &[
            "vector_metric_quantization_cross_orders_and_exact_distances",
            "schema_vector_index_duplicate_creation_replaces_previous_params",
        ],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk",
        "Schema (ALTER)",
        &[
            "vector_create_index_overloads_inmemory_ondisk_and_pq",
            "schema_vector_index_duplicate_creation_replaces_previous_params",
            "schema_vector_index_over_empty_field_then_insert_immediately_searchable",
            "schema_vector_index_mixed_dimensions_match_scan_twin",
            "schema_vector_dimension_change_on_update_leaves_index",
            "schema_vector_index_compaction_after_deletes_keeps_results_exact",
        ],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk_quantized",
        "Schema (ALTER)",
        &[
            "vector_create_index_overloads_inmemory_ondisk_and_pq",
            "schema_vector_index_duplicate_creation_replaces_previous_params",
        ],
    ),
    row(
        "corvid::Collection::create_vector_index_ondisk_pq",
        "Schema (ALTER)",
        &[
            "vector_create_index_overloads_inmemory_ondisk_and_pq",
            "schema_vector_pq_creation_training_error_variants_and_success",
        ],
    ),
    // ===== fts.rs — text index creation =====
    row(
        "corvid::Collection::create_text_index",
        "Schema (ALTER)",
        &[
            "queries_plan_shape_text_index_for_single_text_source",
            "text_search_index_inmemory_ondisk_match_scan_twin",
            "text_builder_text_index_arm_matches_scan_arm",
            "text_phrase_k_boundaries_empty_phrase_and_index_arms",
            "schema_text_index_duplicate_and_non_text_values_excluded",
            "schema_text_index_mutations_keep_search_correct",
        ],
    ),
    row(
        "corvid::Collection::create_text_index_ondisk",
        "Schema (ALTER)",
        &[
            "text_search_index_inmemory_ondisk_match_scan_twin",
            "text_phrase_k_boundaries_empty_phrase_and_index_arms",
            "schema_text_index_mutations_keep_search_correct",
        ],
    ),
    // ===== scalar.rs — scalar/compound index creation =====
    row(
        "corvid::Collection::create_scalar_index",
        "Schema (ALTER)",
        &[
            "mutations_update_maintains_scalar_index",
            "mutations_compare_and_set_maintains_scalar_index",
            "mutations_delete_where_exact_with_scalar_index_present",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
            "queries_order_by_indexed_vs_scan_equivalent",
            "schema_scalar_index_empty_collection_creates_and_serves_later",
            "schema_scalar_index_backfill_makes_populated_collection_immediately_queryable",
            "schema_scalar_index_duplicate_creation_replaces_without_stale_entries",
            "schema_scalar_index_mixed_type_field_lanes_and_missing_docs_match_scan",
            "schema_scalar_index_maintenance_contract_under_every_mutation_kind",
        ],
    ),
    row(
        "corvid::Collection::create_compound_index",
        "Schema (ALTER)",
        &[
            "filters_indexed_vs_scan_compound_prefix",
            "queries_plan_shape_indexed_window_kinds_and_explain_families",
            "schema_compound_index_field_order_determines_serviceability",
            "schema_compound_index_single_and_three_field_arities",
            "schema_compound_index_duplicate_and_reverse_order_coexist",
            "schema_compound_trailing_field_mutations_never_surface_stale_entries",
        ],
    ),
    // ===== schema.rs — declared schemas =====
    row(
        "corvid::schema::FieldType",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::FieldType::Any",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::FieldType::Bool",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::FieldType::Int",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_schema_violation_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::FieldType::Float",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "schema_unique_nan_equals_nan_rejects_second_document",
        ],
    ),
    row(
        "corvid::schema::FieldType::Text",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::FieldType::Bytes",
        "Schema (ALTER)",
        &["schema_unique_containers_bytes_text_vector_and_null_rule"],
    ),
    row(
        "corvid::schema::FieldType::Vector",
        "Schema (ALTER)",
        &["schema_unique_containers_bytes_text_vector_and_null_rule"],
    ),
    row(
        "corvid::schema::FieldType::Array",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::FieldType::Map",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::Field",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Field::new",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Field::required",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::schema::Field::unique",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Schema",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Schema::new",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Schema::field",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::schema::Schema::fields",
        "Schema (ALTER)",
        &["schema_field_type_matrix_and_fields_accessor"],
    ),
    row(
        "corvid::Collection::set_schema",
        "Schema (ALTER)",
        &[
            "schema_field_type_matrix_and_fields_accessor",
            "mutations_insert_batch_schema_violation_rolls_back_whole_batch",
            "mutations_insert_batch_unique_conflict_rolls_back_whole_batch",
        ],
    ),
    row(
        "corvid::Collection::schema",
        "Schema (ALTER)",
        &["schema_getter_roundtrips_declared_fields"],
    ),
    // ===== reactive.rs — change feeds =====
    row(
        "corvid::ChangeKind",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "mutations_emit_change_events_per_mutation_kind",
            "events_insert_paths_emit_exact_insert_vectors_in_order",
            "events_update_and_patch_emit_exact_vectors_for_both_branches",
            "events_compare_and_set_emit_exact_vectors_per_branch",
            "events_delete_paths_emit_exact_delete_vectors_in_order",
        ],
    ),
    row(
        "corvid::ChangeKind::Insert",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "mutations_emit_change_events_per_mutation_kind",
            "events_insert_paths_emit_exact_insert_vectors_in_order",
            "events_update_and_patch_emit_exact_vectors_for_both_branches",
            "events_compare_and_set_emit_exact_vectors_per_branch",
            "events_link_to_missing_endpoints_still_emits_insert_keyed_by_from",
        ],
    ),
    row(
        "corvid::ChangeKind::Delete",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "mutations_emit_change_events_per_mutation_kind",
            "events_delete_paths_emit_exact_delete_vectors_in_order",
            "events_compare_and_set_emit_exact_vectors_per_branch",
            "events_ttl_purge_cascade_is_silent_and_stranded_purge_emits_nothing",
        ],
    ),
    row(
        "corvid::ChangeEvent",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "mutations_emit_change_events_per_mutation_kind",
            "events_multiple_subscribers_all_receive_identical_exact_vectors",
            "events_cross_collection_tagging_each_event_names_its_collection",
            "events_dispatch_is_synchronous_post_commit_and_in_mutation_order",
        ],
    ),
    row(
        "corvid::SubscriptionId",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence",
        ],
    ),
    row(
        "corvid::Db::subscribe",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "mutations_emit_change_events_per_mutation_kind",
            "events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence",
            "events_multiple_subscribers_all_receive_identical_exact_vectors",
            "events_cross_collection_tagging_each_event_names_its_collection",
        ],
    ),
    row(
        "corvid::Db::unsubscribe",
        "Lifecycle",
        &[
            "events_smoke_subscribe_records_insert_and_delete",
            "events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence",
        ],
    ),
    // ===== semantic_cache.rs =====
    row(
        "corvid::SemanticCache",
        "Lifecycle",
        &[
            "lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine",
            "lifecycle_semantic_cache_threshold_units_follow_the_metric_l2",
        ],
    ),
    row(
        "corvid::Collection::semantic_cache",
        "Lifecycle",
        &[
            "lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine",
            "lifecycle_semantic_cache_threshold_units_follow_the_metric_l2",
        ],
    ),
    row(
        "corvid::SemanticCache::put",
        "Lifecycle",
        &["lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine"],
    ),
    row(
        "corvid::SemanticCache::get",
        "Lifecycle",
        &[
            "lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine",
            "lifecycle_semantic_cache_threshold_units_follow_the_metric_l2",
        ],
    ),
    // ===== sketch.rs — probabilistic sketches =====
    row(
        "corvid::HyperLogLog",
        "Lifecycle",
        &["lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates"],
    ),
    row(
        "corvid::HyperLogLog::new",
        "Lifecycle",
        &["lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates"],
    ),
    row(
        "corvid::HyperLogLog::with_precision",
        "Lifecycle",
        &["lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates"],
    ),
    row(
        "corvid::HyperLogLog::add_bytes",
        "Lifecycle",
        &[
            "lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates",
            "lifecycle_hyperloglog_add_hash_is_the_precomputed_twin_of_add_bytes",
        ],
    ),
    row(
        "corvid::HyperLogLog::add_hash",
        "Lifecycle",
        &["lifecycle_hyperloglog_add_hash_is_the_precomputed_twin_of_add_bytes"],
    ),
    row(
        "corvid::HyperLogLog::estimate",
        "Lifecycle",
        &["lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates"],
    ),
    row(
        "corvid::BloomFilter",
        "Lifecycle",
        &["lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate"],
    ),
    row(
        "corvid::BloomFilter::new",
        "Lifecycle",
        &["lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate"],
    ),
    row(
        "corvid::BloomFilter::add_bytes",
        "Lifecycle",
        &["lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate"],
    ),
    row(
        "corvid::BloomFilter::contains_bytes",
        "Lifecycle",
        &["lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate"],
    ),
    row(
        "corvid::Collection::approx_distinct",
        "Aggregations",
        &[
            "aggregations_approx_distinct_exact_small_counts_and_duplicates",
            "aggregations_approx_distinct_distinguishes_encoded_kinds_and_skips_missing",
            "aggregations_approx_distinct_bounded_error_on_larger_corpus",
        ],
    ),
    // ===== ttl.rs — expiry =====
    row(
        "corvid::Collection::insert_with_ttl",
        "TTL",
        &[
            "ttl_smoke_insert_with_ttl_purges_at_boundary",
            "mutations_insert_with_ttl_sets_and_purges_expiry",
            "ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite",
            "ttl_timestamps_accept_i64_extremes_and_order_correctly",
            "ttl_plain_write_paths_clear_expiry",
            "ttl_purge_removes_doc_from_scalar_unique_vector_and_text_indexes",
            "ttl_purge_cascades_edges_of_expired_document_both_namespaces",
            "ttl_write_paths_reject_reserved_and_invalid_collection_names",
        ],
    ),
    row(
        "corvid::Collection::set_ttl",
        "TTL",
        &[
            "ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite",
            "ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting",
            "ttl_plain_write_paths_clear_expiry",
            "ttl_write_paths_reject_reserved_and_invalid_collection_names",
        ],
    ),
    row(
        "corvid::Collection::ttl",
        "TTL",
        &[
            "ttl_smoke_insert_with_ttl_purges_at_boundary",
            "mutations_insert_with_ttl_sets_and_purges_expiry",
            "ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite",
            "ttl_on_missing_doc_and_doc_without_expiry_both_none",
            "ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting",
        ],
    ),
    row(
        "corvid::Collection::purge_expired",
        "TTL",
        &[
            "ttl_smoke_insert_with_ttl_purges_at_boundary",
            "mutations_insert_with_ttl_sets_and_purges_expiry",
            "ttl_purge_boundary_one_before_exactly_at_one_after_and_idempotence",
            "ttl_expired_doc_visible_until_purged_hidden_from_all_reads_after",
            "ttl_timestamps_accept_i64_extremes_and_order_correctly",
            "ttl_purge_removes_doc_from_scalar_unique_vector_and_text_indexes",
            "ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting",
            // W3 wave-review ruling: the purge's edge cascade runs even for
            // a stranded entry (no document row) — same contract as delete.
            "ttl_purge_cascades_edges_of_expired_document_both_namespaces",
            "ttl_purge_cascades_edges_of_stranded_entry_without_document",
            "events_delete_paths_emit_exact_delete_vectors_in_order",
            "events_ttl_purge_cascade_is_silent_and_stranded_purge_emits_nothing",
        ],
    ),
    // ===== migrate.rs — dump/load =====
    row(
        "corvid::Db::dump",
        "Lifecycle",
        &[
            "lifecycle_smoke_dump_load_roundtrips_documents",
            "lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact",
            "lifecycle_dump_load_roundtrips_every_index_family_ttl_edges_schema_and_autoids",
            "lifecycle_dump_load_into_nonempty_db_merges_records_and_counters",
            "lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface",
        ],
    ),
    row(
        "corvid::Db::load",
        "Lifecycle",
        &[
            "lifecycle_smoke_dump_load_roundtrips_documents",
            "lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact",
            "lifecycle_dump_load_roundtrips_every_index_family_ttl_edges_schema_and_autoids",
            "lifecycle_dump_load_into_nonempty_db_merges_records_and_counters",
            "lifecycle_load_rejects_reserved_names_and_malformed_streams",
            "lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface",
        ],
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
    row(
        "corvid::QueryPlan",
        "Lifecycle",
        &["lifecycle_query_plan_key_is_canonical_for_identical_shapes"],
    ),
    row(
        "corvid::QueryPlan::key",
        "Lifecycle",
        &["lifecycle_query_plan_key_is_canonical_for_identical_shapes"],
    ),
    row(
        "corvid::PlanCache",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::new",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::get",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::insert",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::get_or_insert_with",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::len",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
    row(
        "corvid::PlanCache::is_empty",
        "Lifecycle",
        &["lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once"],
    ),
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
    let fns = citable_test_fns();
    assert!(
        !fns.is_empty(),
        "no citable test fns found under {}",
        tests_dir().display()
    );
    for r in MANIFEST {
        for name in r.covering_tests {
            assert!(
                fns.contains_key(*name),
                "manifest row {} cites covering test `{name}`, but no fn with \
                 that name exists under {} (rows must cite integration tests, \
                 never unit tests inside src/)",
                r.item,
                tests_dir().display()
            );
        }
    }
}

/// Conformance-plan convention: covering-test names are globally unique
/// across the `tests/` tree — citations are bare fn names, so one name
/// identifying two tests makes every citation of it ambiguous. A duplicate
/// anywhere (outside the radar's own excluded `surface/` directory) fails,
/// with both defining files named.
#[test]
fn cited_test_names_are_globally_unique() {
    let fns = citable_test_fns();
    let dups = duplicate_test_names(&fns);
    assert!(
        dups.is_empty(),
        "duplicate #[test] fn names under {} (each name must identify exactly \
         one test — rename one of them): {dups:#?}",
        tests_dir().display()
    );
}

/// The citable covering-test index: every `#[test]` fn under `tests/`,
/// EXCLUDING the radar's own `surface/` directory (conformance-plan
/// convention: radar self-tests are not citable). Maps each fn name to the
/// files defining it (more than one file is a duplicate; see
/// [`duplicate_test_names`]).
fn citable_test_fns() -> BTreeMap<&'static str, Vec<PathBuf>> {
    collect_test_fns(&tests_dir())
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
        parse_scope(
            &toks,
            &module,
            ScopeKind::Module,
            None,
            &mut raws,
            &mut aliases,
        );
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

/// The citable covering-test index over `dir`: every `#[test]` fn name,
/// mapped to the files defining it. The radar's own `surface/` subdirectory
/// is excluded (conformance-plan convention: radar self-tests are not
/// citable — a manifest row must cite a conformance test, never the radar
/// that checks it). Only fns carrying a `#[test]` attribute (optionally
/// among other attributes and behind visibility / qualifier prefixes) are
/// indexed — helper fns never qualify.
fn collect_test_fns(dir: &Path) -> BTreeMap<&'static str, Vec<PathBuf>> {
    let surface = dir.join("surface");
    let mut out: BTreeMap<&'static str, Vec<PathBuf>> = BTreeMap::new();
    for file in rust_files(dir) {
        if file.starts_with(&surface) {
            continue; // radar self-tests are not citable
        }
        let src =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for name in test_fn_names(&tokenize(&src)) {
            let leaked: &'static str = Box::leak(name.into_boxed_str());
            out.entry(leaked).or_default().push(file.clone());
        }
    }
    out
}

/// The entries of the covering-test index whose name is defined in more
/// than one file — the ambiguous citations [`cited_test_names_are_globally_unique`]
/// rejects. Each duplicate is `(name, files)` so the failure can name every
/// location.
fn duplicate_test_names<'a>(
    fns: &'a BTreeMap<&'static str, Vec<PathBuf>>,
) -> Vec<(&'static str, &'a [PathBuf])> {
    fns.iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(name, files)| (*name, files.as_slice()))
        .collect()
}

/// The names of the `fn`s in `toks` that directly carry a `#[test]`
/// attribute.
fn test_fn_names(toks: &[Tok]) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = Cursor::new(toks);
    while c.peek().is_some() {
        if c.at_punct('#') && matches!(c.toks.get(c.pos + 1), Some(Tok::Punct('['))) {
            let attr_start = c.pos;
            c.pos += 1; // now on '['
            let end = c.group_end('[', ']');
            let body = &c.toks[attr_start + 2..end - 1];
            if matches!(body, [Tok::Ident(name)] if name == "test")
                && let Some(name) = fn_name_after_test_attr(toks, end)
            {
                out.push(name);
            }
            c.pos = end;
        } else {
            c.pos += 1;
        }
    }
    out
}

/// The name of the fn a `#[test]` attribute is attached to, if the tokens
/// after the attribute (at `from`) are attribute/visibility/qualifier
/// prefixes followed by `fn NAME`.
fn fn_name_after_test_attr(toks: &[Tok], from: usize) -> Option<String> {
    let mut c = Cursor { toks, pos: from };
    loop {
        if let Some(Tok::Ident(k)) = c.peek() {
            match k.as_str() {
                "fn" => {
                    c.pos += 1;
                    return next_ident(&mut c);
                }
                "pub" | "crate" | "async" | "unsafe" | "extern" | "const" => c.pos += 1,
                _ => return None,
            }
        } else if matches!(
            c.peek(),
            Some(Tok::Punct('(')) | Some(Tok::Punct(')')) | Some(Tok::Literal)
        ) {
            c.pos += 1;
        } else if c.at_punct('#') && matches!(c.toks.get(c.pos + 1), Some(Tok::Punct('['))) {
            // A further attribute on the same fn (`#[test]
            // #[should_panic]`).
            c.pos += 1; // now on '['
            c.pos = c.group_end('[', ']');
        } else {
            return None;
        }
    }
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

/// The kind of scope being parsed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    /// A module body (a file or an inline `pub mod`).
    Module,
    /// An inherent impl body: `pub fn`s are the type's public methods.
    InherentImpl,
    /// A `pub trait` body: every fn/type/const item is the trait's public
    /// surface (trait items carry no visibility qualifier of their own).
    PubTrait,
}

/// Parse one scope's items — a file body, a `pub mod` body, an inherent impl
/// body, or a `pub trait` body (`owner` names the self/trait type for the
/// latter two) — recording public constructs into `raws` and crate-root
/// re-exports into `aliases`.
fn parse_scope(
    toks: &[Tok],
    module: &str,
    scope: ScopeKind,
    owner: Option<&str>,
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
        skip_fn_qualifiers(&mut c);
        let Some(Tok::Ident(kw)) = c.peek().cloned() else {
            c.skip_item();
            continue;
        };
        c.pos += 1;
        let public = vis == Vis::Public && !cfg_test;
        // Inside a pub trait every item is public surface (visibility
        // qualifiers are illegal on trait items), so recording ignores `vis`.
        let recordable = public || scope == ScopeKind::PubTrait;
        match kw.as_str() {
            "use" => {
                if public && module.is_empty() && scope == ScopeKind::Module {
                    parse_root_use(&mut c, aliases);
                }
                c.skip_item();
            }
            "mod" => {
                let Some(name) = next_ident(&mut c) else {
                    c.skip_item();
                    continue;
                };
                if public && scope == ScopeKind::Module && c.at_punct('{') {
                    let end = c.group_end('{', '}');
                    let inner = if module.is_empty() {
                        name
                    } else {
                        format!("{module}::{name}")
                    };
                    parse_scope(
                        &c.toks[c.pos + 1..end - 1],
                        &inner,
                        ScopeKind::Module,
                        None,
                        raws,
                        aliases,
                    );
                    c.pos = end;
                } else {
                    // Private or test-only module (`mod tests`): its contents
                    // are not public surface. `pub mod x;` file modules are
                    // walked as files instead.
                    c.skip_item();
                }
            }
            "fn" => {
                if recordable && let Some(name) = next_ident(&mut c) {
                    record(scope, owner, module, name, raws);
                }
                c.skip_item();
            }
            "const" | "static" => {
                if recordable {
                    if c.at_ident("mut") {
                        c.pos += 1;
                    }
                    if let Some(name) = next_ident(&mut c) {
                        record(scope, owner, module, name, raws);
                    }
                }
                c.skip_item();
            }
            "type" | "struct" | "trait" => {
                if recordable && let Some(name) = next_ident(&mut c) {
                    record(scope, owner, module, name.clone(), raws);
                    if kw == "trait" && scope == ScopeKind::Module {
                        parse_trait_body(&mut c, module, &name, raws, aliases);
                        continue;
                    }
                }
                c.skip_item();
            }
            "enum" => {
                if recordable && let Some(name) = next_ident(&mut c) {
                    record(scope, owner, module, name.clone(), raws);
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
                        ScopeKind::InherentImpl,
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

/// Walk a `pub trait` body (the cursor is just past the trait name),
/// recording every fn/type/const item as a member of the trait. Supertrait
/// bounds, generics, and where clauses between the name and the body are
/// scanned through (they cannot contain braces).
fn parse_trait_body(
    c: &mut Cursor,
    module: &str,
    ty: &str,
    raws: &mut Vec<Raw>,
    aliases: &mut BTreeSet<String>,
) {
    if c.at_punct('<') {
        c.skip_group('<', '>');
    }
    while !c.at_punct('{') && c.peek().is_some() {
        if c.at_punct(';') {
            // A trait alias (`pub trait X = Y;`) has no body items.
            c.pos += 1;
            return;
        }
        c.pos += 1;
    }
    if c.peek().is_none() {
        return;
    }
    let end = c.group_end('{', '}');
    parse_scope(
        &c.toks[c.pos + 1..end - 1],
        module,
        ScopeKind::PubTrait,
        Some(ty),
        raws,
        aliases,
    );
    c.pos = end;
}

fn record(scope: ScopeKind, owner: Option<&str>, module: &str, name: String, raws: &mut Vec<Raw>) {
    match owner.filter(|_| scope != ScopeKind::Module) {
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

/// Skip item qualifiers that may sit between a visibility and the item
/// keyword: `pub async fn`, `pub unsafe fn`, `pub const fn`,
/// `pub unsafe extern "C" fn`, `pub unsafe trait`, … (`const` only counts as
/// a qualifier when an `fn` — possibly through further qualifiers —
/// follows). The ABI string after `extern` is consumed too.
fn skip_fn_qualifiers(c: &mut Cursor) {
    loop {
        let is_qualifier = match c.peek() {
            Some(Tok::Ident(k)) => match k.as_str() {
                "async" | "unsafe" | "extern" => true,
                "const" => matches!(
                    c.toks.get(c.pos + 1),
                    Some(Tok::Ident(n)) if matches!(n.as_str(), "fn" | "async" | "unsafe" | "extern" | "const")
                ),
                _ => false,
            },
            _ => false,
        };
        if !is_qualifier {
            return;
        }
        c.pos += 1;
        if matches!(c.peek(), Some(Tok::Literal)) {
            c.pos += 1; // the ABI string of `extern "C"`
        }
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
/// (`as`) contribute the alias. Globs and nested brace groups are rejected
/// loudly — silently mishandling them would corrupt alias resolution and
/// misname every row for the re-exported items.
fn parse_root_use(c: &mut Cursor, aliases: &mut BTreeSet<String>) {
    const GLOB_MSG: &str = "glob re-export (`pub use …::*`) in lib.rs: the radar cannot derive \
         which names it exports; list the names explicitly or teach \
         parse_root_use about globs";
    const NESTED_MSG: &str = "nested brace group in a `pub use` in lib.rs: the radar only reads \
         one-level `pub use m::{A, B};` lists; flatten the re-export or \
         teach parse_root_use about nesting";
    let mut last: Option<String> = None;
    while let Some(t) = c.peek().cloned() {
        match t {
            Tok::Punct('*') => panic!("{GLOB_MSG}"),
            Tok::Punct('{') => {
                let end = c.group_end('{', '}');
                for t in &c.toks[c.pos + 1..end - 1] {
                    if matches!(t, Tok::Punct('*')) {
                        panic!("{GLOB_MSG}");
                    }
                    if matches!(t, Tok::Punct('{')) {
                        panic!("{NESTED_MSG}");
                    }
                }
                let mut i = c.pos + 1;
                let mut prev_name: Option<String> = None;
                while i < end - 1 {
                    if let Some(Tok::Ident(name)) = c.toks.get(i) {
                        if name == "as" {
                            if let Some(Tok::Ident(alias)) = c.toks.get(i + 1) {
                                aliases.insert(alias.clone());
                            }
                            // `A as C` exports C, not A.
                            if let Some(prev) = prev_name.take() {
                                aliases.remove(&prev);
                            }
                        } else {
                            prev_name = Some(name.clone());
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

// ===========================================================================
// Extractor unit tests (synthetic sources, no filesystem involved).
// ===========================================================================

/// Qualified fns (`async`/`unsafe`/`const`/`extern "C"`) are recorded like
/// any pub fn, and a pub trait contributes its own name plus one row per
/// member (fn, default fn, associated type, associated const).
#[test]
fn radar_extracts_qualified_fns_and_pub_trait_members() {
    let src = r#"
        pub async fn afn() {}
        pub unsafe fn ufn() {}
        pub const fn cfn() -> u8 { 7 }
        pub unsafe extern "C" fn efn() {}
        fn private_helper() {}
        pub trait Shape: Send {
            fn area(&self) -> f64;
            fn default_impl(&self) -> u8 { 0 }
            type Output;
            const CAP: usize;
        }
    "#;
    let mut raws = Vec::new();
    let mut aliases = BTreeSet::new();
    parse_scope(
        &tokenize(src),
        "m",
        ScopeKind::Module,
        None,
        &mut raws,
        &mut aliases,
    );
    let names: BTreeSet<String> = raws
        .iter()
        .map(|r| match r {
            Raw::ModuleItem { module, name } => format!("{module}::{name}"),
            Raw::Variant { ty, variant, .. } => format!("{ty}::{variant}"),
            Raw::Method { ty, name } => format!("{ty}::{name}"),
        })
        .collect();
    for expected in [
        "m::afn",
        "m::ufn",
        "m::cfn",
        "m::efn",
        "m::Shape",
        "Shape::area",
        "Shape::default_impl",
        "Shape::Output",
        "Shape::CAP",
    ] {
        assert!(names.contains(expected), "missing {expected} in {names:?}");
    }
    assert!(!names.contains("m::private_helper"));
}

/// Only fns directly carrying `#[test]` are indexed by the existence radar —
/// plain helpers, `const fn`s, and other attributes never qualify.
#[test]
fn radar_indexes_only_test_attributed_fns() {
    let src = r#"
        fn plain_helper() {}
        const fn const_helper() {}
        #[test]
        fn real_test() {}
        #[test] #[should_panic]
        fn attrs_after_test() {}
        #[ignore]
        #[test]
        pub async fn qualified_test() {}
        #[cfg(test)]
        fn cfg_attr_is_not_a_test_attr() {}
    "#;
    let names = test_fn_names(&tokenize(src));
    assert_eq!(
        names,
        vec![
            "real_test".to_owned(),
            "attrs_after_test".to_owned(),
            "qualified_test".to_owned(),
        ]
    );
}

/// Glob and nested-brace re-exports are rejected loudly instead of silently
/// corrupting the alias set; the plain and renamed forms keep working.
#[test]
fn radar_rejects_glob_and_nested_reexports() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let run_use = |src: &str| {
        catch_unwind(AssertUnwindSafe(|| {
            let toks = tokenize(src);
            let mut c = Cursor::new(&toks);
            c.pos = 2; // past `pub use`
            let mut aliases = BTreeSet::new();
            parse_root_use(&mut c, &mut aliases);
            aliases
        }))
    };

    let glob = run_use("pub use m::*;");
    let nested = run_use("pub use m::{a, n::{b}};");
    std::panic::set_hook(prev_hook);
    assert!(glob.is_err(), "a glob re-export must be rejected loudly");
    assert!(
        nested.is_err(),
        "a nested brace group must be rejected loudly"
    );

    // Plain and renamed forms still resolve (and `A as C` exports only C).
    let toks = tokenize("pub use m::{A, B as C};");
    let mut c = Cursor::new(&toks);
    c.pos = 2;
    let mut aliases = BTreeSet::new();
    parse_root_use(&mut c, &mut aliases);
    assert_eq!(aliases, BTreeSet::from(["A".to_owned(), "C".to_owned()]));
}

/// The covering-test index excludes the radar's own `surface/` directory and
/// maps each name to its defining file(s), so duplicates anywhere in the
/// tree are detectable with both locations named (conformance-plan
/// conventions 2 and 3; Task 3 housekeeping). Verified against a synthetic
/// `tests/` tree.
#[test]
fn radar_index_excludes_surface_and_tracks_duplicate_locations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tests");
    fs::create_dir_all(root.join("surface")).unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(
        root.join("a.rs"),
        "#[test] fn dup() {}\n#[test] fn solo() {}\nfn helper() {}\n",
    )
    .unwrap();
    fs::write(root.join("sub").join("b.rs"), "#[test] fn dup() {}\n").unwrap();
    fs::write(
        root.join("surface").join("mod.rs"),
        "#[test] fn dup() {}\n#[test] fn radar_only() {}\n",
    )
    .unwrap();

    let fns = collect_test_fns(&root);

    // The radar's own tests are not citable: neither of surface/mod.rs's
    // names is indexed, even the one duplicated elsewhere.
    assert!(!fns.contains_key("radar_only"));
    assert_eq!(
        fns.get("dup").map(|v| v.as_slice()),
        Some(vec![root.join("a.rs"), root.join("sub").join("b.rs")].as_slice()),
        "dup's locations must be both non-surface files, in walk order"
    );
    // A name defined once carries exactly one location; helpers are absent.
    assert_eq!(
        fns.get("solo").map(|v| v.as_slice()),
        Some(vec![root.join("a.rs")].as_slice())
    );
    assert!(!fns.contains_key("helper"));

    // The uniqueness check reports the duplicate with both locations, and
    // nothing else.
    let dups = duplicate_test_names(&fns);
    assert_eq!(dups.len(), 1);
    assert_eq!(dups[0].0, "dup");
    assert_eq!(
        dups[0].1,
        &vec![root.join("a.rs"), root.join("sub").join("b.rs")]
    );

    // A tree with no surface/ directory at all indexes normally.
    let bare = tempfile::tempdir().unwrap();
    fs::create_dir_all(bare.path()).unwrap();
    fs::write(bare.path().join("only.rs"), "#[test] fn one() {}\n").unwrap();
    let fns = collect_test_fns(bare.path());
    assert_eq!(
        fns.get("one").map(|v| v.as_slice()),
        Some(vec![bare.path().join("only.rs")].as_slice())
    );
    assert!(duplicate_test_names(&fns).is_empty());
}
