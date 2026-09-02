# The corvid language — syntax reference

<!-- GENERATED FILE — do not edit by hand. Source of truth: the surface
manifests (crates/corvid/tests/surface/mod.rs and
crates/corvid-mcp/tests/surface/mod.rs), which the radar tests keep
honest against the sources. Regenerate with:
CORVID_GEN_SYNTAX=1 cargo test -p corvid-db --test surface syntax_md
Every ordinary `cargo test` run re-renders and diffs this file, so the
committed copy cannot drift from the manifests. -->

This is the complete writable surface of `corvid` and `corvid-mcp`: every
public construct, grouped by statement class (the SQL analogue is a
guide, not a promise), each with the integration tests that pin its
happy/edge/error/corner behavior. Construct paths are canonical Rust
paths; `mcp::tool::<name>` / `mcp::envelope::<kind>` are wire syntax.


## Mutations — 10 construct(s)

- `corvid::Collection` — `mutations_smoke_insert_roundtrips`
- `corvid::Collection::insert` — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `mutations_insert_overwrites_and_accepts_empty_key_and_empty_map`, `mutations_insert_rejects_reserved_and_invalid_collection_names`
- `corvid::Collection::update` — `mutations_update_rewrites_document_to_every_value_kind`, `mutations_update_on_missing_key_creates_or_stays_absent`, `mutations_update_maintains_scalar_index`
- `corvid::Collection::patch` — `mutations_patch_merges_top_level_and_replaces_non_maps`
- `corvid::Collection::compare_and_set` — `mutations_compare_and_set_swap_noop_delete_and_semantic_float_equality`, `mutations_compare_and_set_uses_semantic_value_equality`, `mutations_compare_and_set_maintains_scalar_index`
- `corvid::Collection::insert_batch` — `mutations_insert_batch_happy_empty_overwrite_and_duplicates`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`, `mutations_insert_batch_schema_violation_rolls_back_whole_batch`
- `corvid::Collection::insert_auto` — `mutations_smoke_insert_roundtrips`, `mutations_insert_auto_keys_are_unique_zero_padded_and_monotonic_per_collection`, `mutations_insert_auto_failure_does_not_burn_an_id`
- `corvid::Collection::delete` *(shared across classes: Mutations, Graph)* — `mutations_smoke_insert_roundtrips`, `mutations_delete_removes_state_from_get_scan_and_count`, `graph_delete_missing_document_still_purges_dangling_edges`
- `corvid::Collection::delete_where` *(shared across classes: Mutations, Graph)* — `mutations_delete_where_counts_zero_partial_and_full`, `mutations_delete_where_exact_with_scalar_index_present`, `graph_delete_where_and_delete_batch_cascade_edges`
- `corvid::Collection::delete_batch` *(shared across classes: Mutations, Graph)* — `mutations_delete_batch_counts_existing_only_and_accepts_empty`, `graph_delete_where_and_delete_batch_cascade_edges`

## WHERE — 52 construct(s)

- `corvid::Value` *(shared across classes: Mutations, WHERE)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`
- `corvid::Value::Null` *(shared across classes: Mutations, WHERE)* — `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`
- `corvid::Value::Bool` *(shared across classes: Mutations, WHERE)* — `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_unordered_kinds_compare_false_for_ordered_ops`
- `corvid::Value::Int` *(shared across classes: Mutations, WHERE)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_int_float_precision_beyond_2_pow_53`
- `corvid::Value::Float` *(shared across classes: Geo, Mutations, WHERE)* — `search_geo_smoke_within_radius_and_nearest`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_nan_comparisons_all_false_except_ne`, `filters_int_float_precision_beyond_2_pow_53`
- `corvid::Value::Text` *(shared across classes: Mutations, WHERE)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_text_ordering_lexicographic_utf8`
- `corvid::Value::Bytes` *(shared across classes: Mutations, WHERE)* — `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`
- `corvid::Value::Array` *(shared across classes: Geo, Mutations, WHERE)* — `search_geo_smoke_within_radius_and_nearest`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_nested_dotted_paths_traverse_maps_only`
- `corvid::Value::Map` *(shared across classes: Mutations, WHERE)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_nested_dotted_paths_traverse_maps_only`
- `corvid::Value::Vector` *(shared across classes: Vector search, Mutations, WHERE)* — `search_vector_smoke_ranks_nearest_first_exact`, `mutations_insert_roundtrips_every_value_variant`, `filters_compare_eq_matches_each_value_kind`, `filters_unordered_kinds_compare_false_for_ordered_ops`
- `corvid::Value::get` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::get_path` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_bool` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_int` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_float` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_text` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_bytes` — `filters_value_accessors_read_stored_kinds`
- `corvid::Value::as_vector` — `filters_value_accessors_read_stored_kinds`
- `corvid::CmpOp` — `filters_smoke_field_eq_selects_matching_rows`, `filters_field_builders_produce_claimed_predicates`
- `corvid::CmpOp::Eq` *(shared across classes: WHERE, Mutations)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_update_maintains_scalar_index`, `filters_compare_eq_matches_each_value_kind`
- `corvid::CmpOp::Ne` — `filters_compare_ne_and_missing_path_semantics`, `filters_nan_comparisons_all_false_except_ne`
- `corvid::CmpOp::Lt` — `filters_ordered_comparisons_numbers_and_edges`, `filters_text_ordering_lexicographic_utf8`
- `corvid::CmpOp::Le` — `filters_ordered_comparisons_numbers_and_edges`, `filters_between_inclusive_and_degenerate_bounds`
- `corvid::CmpOp::Gt` — `filters_ordered_comparisons_numbers_and_edges`, `filters_text_ordering_lexicographic_utf8`
- `corvid::CmpOp::Ge` *(shared across classes: Mutations, WHERE)* — `mutations_delete_where_counts_zero_partial_and_full`, `filters_ordered_comparisons_numbers_and_edges`, `filters_between_inclusive_and_degenerate_bounds`
- `corvid::Predicate` *(shared across classes: WHERE, Mutations)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_delete_where_counts_zero_partial_and_full`, `filters_and_or_not_nesting_and_de_morgan`
- `corvid::Predicate::Compare` — `filters_smoke_field_eq_selects_matching_rows`, `filters_compare_eq_matches_each_value_kind`, `filters_ordered_comparisons_numbers_and_edges`
- `corvid::Predicate::Exists` — `filters_exists_presence_semantics`
- `corvid::Predicate::In` — `filters_in_membership_matrix`
- `corvid::Predicate::Between` — `filters_between_inclusive_and_degenerate_bounds`
- `corvid::Predicate::StartsWith` — `filters_starts_with_prefix_semantics`
- `corvid::Predicate::Contains` — `filters_contains_substring_semantics`
- `corvid::Predicate::And` — `filters_and_or_not_nesting_and_de_morgan`, `filters_predicate_combinators_and_direct_construction`
- `corvid::Predicate::Or` — `filters_and_or_not_nesting_and_de_morgan`, `filters_indexed_vs_scan_or_union`
- `corvid::Predicate::Not` — `filters_and_or_not_nesting_and_de_morgan`, `filters_predicate_combinators_and_direct_construction`
- `corvid::Predicate::and` — `filters_predicate_combinators_and_direct_construction`, `filters_multiple_filter_calls_intersect_like_and`
- `corvid::Predicate::or` — `filters_predicate_combinators_and_direct_construction`, `filters_indexed_vs_scan_or_union`
- `corvid::Predicate::eval` — `filters_compare_eq_matches_each_value_kind`, `filters_ordered_comparisons_numbers_and_edges`, `filters_starts_with_prefix_semantics`
- `corvid::field` *(shared across classes: WHERE, Mutations)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_delete_where_counts_zero_partial_and_full`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef` — `filters_smoke_field_eq_selects_matching_rows`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef::eq` *(shared across classes: WHERE, Mutations)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_update_maintains_scalar_index`, `filters_field_builders_produce_claimed_predicates`, `filters_compare_eq_matches_each_value_kind`
- `corvid::filter::FieldRef::ne` — `filters_field_builders_produce_claimed_predicates`, `filters_compare_ne_and_missing_path_semantics`
- `corvid::filter::FieldRef::lt` — `filters_field_builders_produce_claimed_predicates`, `filters_ordered_comparisons_numbers_and_edges`
- `corvid::filter::FieldRef::le` — `filters_field_builders_produce_claimed_predicates`, `filters_ordered_comparisons_numbers_and_edges`
- `corvid::filter::FieldRef::gt` — `filters_field_builders_produce_claimed_predicates`, `filters_ordered_comparisons_numbers_and_edges`
- `corvid::filter::FieldRef::ge` *(shared across classes: Mutations, WHERE)* — `mutations_delete_where_counts_zero_partial_and_full`, `filters_field_builders_produce_claimed_predicates`, `filters_ordered_comparisons_numbers_and_edges`
- `corvid::filter::FieldRef::exists` — `filters_exists_presence_semantics`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef::is_in` — `filters_in_membership_matrix`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef::between` — `filters_between_inclusive_and_degenerate_bounds`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef::starts_with` — `filters_starts_with_prefix_semantics`, `filters_field_builders_produce_claimed_predicates`
- `corvid::filter::FieldRef::contains` — `filters_contains_substring_semantics`, `filters_field_builders_produce_claimed_predicates`
- `corvid::QueryBuilder::filter` *(shared across classes: WHERE, Mutations)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_update_maintains_scalar_index`

## SELECT shaping — 16 construct(s)

- `corvid::ResultRow` *(shared across classes: WHERE, SELECT shaping)* — `filters_smoke_field_eq_selects_matching_rows`, `queries_result_row_fields_per_query_shape`, `queries_run_select_only_returns_all_in_key_order`, `queries_select_preserves_rank_scores_and_filter_visibility`
- `corvid::QueryBuilder` *(shared across classes: WHERE, SELECT shaping)* — `filters_smoke_field_eq_selects_matching_rows`, `queries_run_select_only_returns_all_in_key_order`
- `corvid::Collection::query` *(shared across classes: WHERE, SELECT shaping)* — `filters_smoke_field_eq_selects_matching_rows`, `queries_run_select_only_returns_all_in_key_order`
- `corvid::QueryBuilder::limit` — `queries_smoke_order_by_limit_select_shapes_rows`, `queries_limit_zero_one_exact_and_over_match_count`, `queries_order_by_limit_offset_window_after_ordering`, `queries_limit_offset_on_empty_collection`
- `corvid::QueryBuilder::offset` *(shared across classes: WHERE, SELECT shaping)* — `filters_filter_then_limit_offset_pagination`, `queries_offset_boundaries_and_full_range_pagination_loop`, `queries_order_by_limit_offset_window_after_ordering`, `queries_limit_offset_on_empty_collection`
- `corvid::QueryBuilder::order_by` — `queries_smoke_order_by_limit_select_shapes_rows`, `queries_order_by_asc_desc_over_int_float_and_text`, `queries_order_by_class_rule_incomparable_then_missing_last_both_directions`, `queries_order_by_mixed_kind_field_groups_numbers_before_texts`, `queries_order_by_limit_offset_window_after_ordering`, `queries_order_by_with_filters_orders_only_matches`, `queries_order_by_indexed_vs_scan_equivalent`
- `corvid::QueryBuilder::select` *(shared across classes: SELECT shaping, Schema (ALTER))* — `queries_smoke_order_by_limit_select_shapes_rows`, `queries_select_single_multiple_and_nested_dotted_paths`, `queries_select_missing_fields_omitted_and_duplicates_collapse`, `queries_select_empty_field_list_yields_empty_map_for_map_docs`, `queries_select_non_map_documents_pass_through_unchanged`, `queries_select_preserves_rank_scores_and_filter_visibility`, `schema_select_empty_field_name_matches_get_path_semantics`
- `corvid::QueryBuilder::run` *(shared across classes: WHERE, Mutations, SELECT shaping)* — `filters_smoke_field_eq_selects_matching_rows`, `mutations_update_maintains_scalar_index`, `queries_run_on_empty_collection_returns_empty_vec`, `queries_run_select_only_returns_all_in_key_order`
- `corvid::Collection::for_each_doc` — `queries_for_each_doc_visits_key_order_and_early_stops_on_false`, `queries_for_each_doc_on_empty_collection_visits_nothing`
- `corvid::Collection::len` *(shared across classes: Mutations, SELECT shaping)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `queries_len_and_is_empty_boundaries`
- `corvid::Collection::is_empty` *(shared across classes: Mutations, SELECT shaping)* — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`, `queries_len_and_is_empty_boundaries`
- `corvid::Collection::get` — `mutations_smoke_insert_roundtrips`, `mutations_insert_roundtrips_every_value_variant`
- `corvid::Collection::scan` *(shared across classes: Mutations, SELECT shaping)* — `mutations_delete_removes_state_from_get_scan_and_count`, `mutations_insert_overwrites_and_accepts_empty_key_and_empty_map`, `queries_scan_returns_pairs_in_key_order`
- `corvid::Collection::page` — `queries_page_cursor_semantics`, `queries_page_after_empty_bytes_skips_only_the_empty_key`, `queries_page_over_empty_collection_yields_empty_page_with_no_cursor`
- `corvid::Collection::page_where` — `queries_page_where_predicate_and_full_walk`, `queries_page_over_empty_collection_yields_empty_page_with_no_cursor`
- `corvid::db::Page` — `queries_page_cursor_semantics`, `queries_page_where_predicate_and_full_walk`

## Aggregations — 10 construct(s)

- `corvid::QueryBuilder::count` *(shared across classes: Aggregations, Mutations, SELECT shaping)* — `aggregations_smoke_sum_group_count_and_count`, `aggregations_count_matrix_filter_empty_and_after_mutations`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`, `mutations_delete_removes_state_from_get_scan_and_count`, `queries_count_with_filter_and_after_mutations`
- `corvid::QueryBuilder::group_count` — `aggregations_smoke_sum_group_count_and_count`, `aggregations_count_distinct_and_groups_separate_types_by_tag`, `aggregations_group_count_escapes_every_ambiguous_tag_prefix`, `aggregations_group_count_zero_signed_floats_share_and_nan_groups`, `aggregations_group_count_skips_missing_and_container_fields`, `aggregations_group_count_respects_filters`, `aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::sum` — `aggregations_smoke_sum_group_count_and_count`, `aggregations_sum_int_float_mixed_and_negative_exact`, `aggregations_sum_skips_missing_and_non_numeric_missing_all_is_zero`, `aggregations_sum_nan_poisons_and_infinities_follow_ieee`, `aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::avg` — `aggregations_avg_matrix_single_mixed_skipped_and_empty`, `aggregations_avg_nan_member_poisons_the_mean`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::min` — `aggregations_min_max_numbers_interop_and_text_is_lexicographic`, `aggregations_min_max_incomparable_kinds_yield_none`, `aggregations_min_max_mixed_kinds_pin_first_comparable_kind_wins`, `aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::max` — `aggregations_min_max_numbers_interop_and_text_is_lexicographic`, `aggregations_min_max_incomparable_kinds_yield_none`, `aggregations_min_max_mixed_kinds_pin_first_comparable_kind_wins`, `aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::count_distinct` — `aggregations_count_distinct_scalars_duplicates_missing_and_empty`, `aggregations_count_distinct_and_groups_separate_types_by_tag`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`, `aggregations_validate_ranking_args_before_aggregating`
- `corvid::QueryBuilder::group_sum` — `aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets`, `aggregations_group_sum_avg_respect_filters`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`
- `corvid::QueryBuilder::group_avg` — `aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets`, `aggregations_group_sum_avg_respect_filters`, `aggregations_ignore_limit_offset_select_order_by_and_sources`, `aggregations_indexed_vs_scan_equivalent_for_every_aggregate`
- `corvid::Collection::approx_distinct` — `aggregations_approx_distinct_exact_small_counts_and_duplicates`, `aggregations_approx_distinct_distinguishes_encoded_kinds_and_skips_missing`, `aggregations_approx_distinct_bounded_error_on_larger_corpus`

## Vector search — 39 construct(s)

- `corvid::Metric` — `search_vector_smoke_ranks_nearest_first_exact`, `vector_metric_quantization_cross_orders_and_exact_distances`
- `corvid::Metric::Cosine` — `search_vector_smoke_ranks_nearest_first_exact`, `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_zero_norm_cosine_dot_l2_ranking`
- `corvid::Metric::Dot` — `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_exact_path_scores_match_hand_computed_formulas`, `vector_zero_norm_cosine_dot_l2_ranking`
- `corvid::Metric::L2` — `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_exact_path_scores_match_hand_computed_formulas`, `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Metric::distance` — `vector_exact_path_scores_match_hand_computed_formulas`
- `corvid::distance::dot` — `vector_exact_path_scores_match_hand_computed_formulas`
- `corvid::distance::l2_squared` — `vector_exact_path_scores_match_hand_computed_formulas`
- `corvid::distance::cosine_distance` — `vector_exact_path_scores_match_hand_computed_formulas`, `vector_zero_norm_cosine_dot_l2_ranking`
- `corvid::Quantization` — `vector_metric_quantization_cross_orders_and_exact_distances`
- `corvid::Quantization::None` — `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_indexed_none_matches_unindexed_twin_for_all_k`
- `corvid::Quantization::Binary` — `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_quantization_k1_binary_diverges_scalar_matches_exact`, `vector_create_index_overloads_inmemory_ondisk_and_pq`
- `corvid::Quantization::Scalar` — `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_quantization_k1_binary_diverges_scalar_matches_exact`
- `corvid::QueryBuilder::vector` *(shared across classes: Hybrid, Vector search)* — `search_hybrid_smoke_rrf_fuses_vector_and_text`, `vector_builder_approx_prefilters_exact_vs_postfilters_approx`, `vector_k_boundaries_zero_one_n_and_beyond`, `vector_empty_collection_single_doc_and_missing_field`, `vector_builder_select_order_limit_offset_interplay`
- `corvid::QueryBuilder::approx` — `vector_builder_approx_prefilters_exact_vs_postfilters_approx`
- `corvid::Hit` — `search_vector_smoke_ranks_nearest_first_exact`, `vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback`, `vector_metric_quantization_cross_orders_and_exact_distances`
- `corvid::Collection::vector_search` — `search_vector_smoke_ranks_nearest_first_exact`, `vector_metric_quantization_cross_orders_and_exact_distances`, `vector_exact_path_scores_match_hand_computed_formulas`, `vector_indexed_none_matches_unindexed_twin_for_all_k`, `vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback`, `vector_k_boundaries_zero_one_n_and_beyond`, `vector_dimension_mismatch_skips_docs_and_index_falls_back`, `vector_zero_norm_cosine_dot_l2_ranking`, `vector_empty_collection_single_doc_and_missing_field`
- `corvid::hnsw::DEFAULT_M` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::hnsw::DEFAULT_EF_CONSTRUCTION` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::new` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::with_params` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::with_quant` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::with_pq` — `vector_hnsw_direct_pq_adc_and_reconstruction_paths`
- `corvid::Hnsw::len` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::is_empty` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::insert` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::Hnsw::search` — `vector_hnsw_direct_api_extreme_params_and_determinism`
- `corvid::pq::Pq` — `pq_train_rejects_unusable_params_and_sample`, `pq_train_is_deterministic_and_codebook_size_math_holds`, `pq_adc_recall_bound_on_fixed_corpus`
- `corvid::pq::Pq::code_len` — `pq_train_is_deterministic_and_codebook_size_math_holds`, `pq_encode_is_deterministic_compact_and_dimension_guarded`
- `corvid::pq::Pq::dim` — `pq_train_is_deterministic_and_codebook_size_math_holds`, `pq_decode_reconstructs_and_guards_malformed_codes`
- `corvid::pq::Pq::params` — `pq_train_is_deterministic_and_codebook_size_math_holds`, `pq_adc_l2_fast_path_matches_reconstruction_and_guards`
- `corvid::pq::Pq::train` — `pq_train_rejects_unusable_params_and_sample`, `pq_train_is_deterministic_and_codebook_size_math_holds`
- `corvid::pq::Pq::encode` — `pq_encode_is_deterministic_compact_and_dimension_guarded`
- `corvid::pq::Pq::decode` — `pq_decode_reconstructs_and_guards_malformed_codes`
- `corvid::pq::Pq::distance` — `pq_distance_is_reconstruction_distance_for_every_metric`
- `corvid::pq::Pq::l2_table` — `pq_adc_l2_fast_path_matches_reconstruction_and_guards`
- `corvid::pq::Pq::adc_l2` — `pq_adc_l2_fast_path_matches_reconstruction_and_guards`, `pq_adc_recall_bound_on_fixed_corpus`
- `corvid::pq::Pq::to_bytes` — `pq_train_is_deterministic_and_codebook_size_math_holds`, `pq_codebook_roundtrips_bytes_and_rejects_malformed`
- `corvid::pq::Pq::from_bytes` — `pq_codebook_roundtrips_bytes_and_rejects_malformed`

## Text search — 15 construct(s)

- `corvid::QueryBuilder::text` *(shared across classes: Hybrid, Text search)* — `search_hybrid_smoke_rrf_fuses_vector_and_text`, `text_builder_text_k_bounds_empty_and_stopword_queries`, `text_builder_text_ranking_scores_select_limit_and_missing_fields`, `text_builder_text_index_arm_matches_scan_arm`
- `corvid::TextHit` — `search_text_smoke_ranks_most_relevant_first`, `text_search_bm25_ranking_tf_length_and_ties`, `text_phrase_order_sensitive_match_and_scores`
- `corvid::Collection::text_search` — `search_text_smoke_ranks_most_relevant_first`, `text_search_bm25_ranking_tf_length_and_ties`, `text_search_rare_term_outscores_common_via_idf`, `text_search_index_inmemory_ondisk_match_scan_twin`, `text_search_k_boundaries_and_corpus_edges`, `text_phrase_single_term_equals_term_search`
- `corvid::Collection::phrase_search` — `text_phrase_order_sensitive_match_and_scores`, `text_phrase_repeated_terms_and_non_adjacent_non_match`, `text_phrase_single_term_equals_term_search`, `text_phrase_stopword_collapse_and_sentence_boundary`, `text_phrase_k_boundaries_empty_phrase_and_index_arms`
- `corvid::text::Bm25Params` — `text_bm25_params_new_and_validate_error_variants`, `text_term_score_zero_saturation_length_and_b_zero`
- `corvid::text::Bm25Params::new` — `text_bm25_params_new_and_validate_error_variants`
- `corvid::text::Bm25Params::validate` — `text_bm25_params_new_and_validate_error_variants`
- `corvid::text::tokenize` — `text_tokenize_case_punct_unicode_numbers_and_empties`, `text_tokenize_cjk_bigrams_boundary_and_mixed_strings`
- `corvid::text::s_stem` — `text_s_stem_pins_conservative_plural_algorithm`
- `corvid::text::Analyzer` — `text_analyzer_default_raw_and_flag_combinations`
- `corvid::text::Analyzer::raw` — `text_analyzer_default_raw_and_flag_combinations`
- `corvid::text::Analyzer::analyze` — `text_analyzer_default_raw_and_flag_combinations`, `text_analyze_cjk_no_stopwords_no_stemming`
- `corvid::text::analyze` — `text_analyzer_default_raw_and_flag_combinations`, `text_analyze_cjk_no_stopwords_no_stemming`
- `corvid::text::idf` — `text_idf_values_monotonicity_and_nonnegativity`
- `corvid::text::term_score` — `text_term_score_zero_saturation_length_and_b_zero`

## Hybrid — 6 construct(s)

- `corvid::Error::InvalidArgument` *(shared across classes: Aggregations, Hybrid, Text search, Geo, Lifecycle)* — `aggregations_validate_ranking_args_before_aggregating`, `hybrid_fuse_rrf_rejects_invalid_k_at_run`, `hybrid_rerank_mmr_rejects_out_of_range_and_nan_at_run`, `text_bm25_params_new_and_validate_error_variants`, `geo_bbox_validation_exact_error_variants`, `lifecycle_load_with_renames_error_contract_invalid_target_collisions_and_noops`
- `corvid::QueryBuilder::fuse_rrf` — `hybrid_fuse_rrf_rejects_invalid_k_at_run`, `hybrid_fusion_rrf_boost_beats_single_source`
- `corvid::QueryBuilder::rerank_mmr` — `hybrid_rerank_mmr_rejects_out_of_range_and_nan_at_run`, `hybrid_rerank_mmr_noop_without_vector_source`, `hybrid_rerank_mmr_lambda_one_reorders_by_relevance`, `hybrid_rerank_mmr_lambda_zero_diversifies`, `hybrid_rerank_mmr_docs_without_embeddings_survive`
- `corvid::DEFAULT_RRF_K` — `search_hybrid_smoke_rrf_fuses_vector_and_text`, `hybrid_rrf_direct_formula_exact_scores_and_edges`
- `corvid::reciprocal_rank_fusion` — `search_hybrid_smoke_rrf_fuses_vector_and_text`, `hybrid_rrf_direct_formula_exact_scores_and_edges`
- `corvid::mmr` — `hybrid_mmr_direct_lambda_zero_one_diversity_and_k`

## Geo — 7 construct(s)

- `corvid::Predicate::GeoWithin` *(shared across classes: WHERE, Geo)* — `filters_geo_within_point_formats_and_boundary`, `filters_indexed_vs_scan_geo_window`, `geo_predicate_deep_dateline_poles_invalid_centers`
- `corvid::filter::FieldRef::within_km` *(shared across classes: WHERE, Geo)* — `filters_geo_within_point_formats_and_boundary`, `filters_indexed_vs_scan_geo_window`, `geo_predicate_deep_dateline_poles_invalid_centers`
- `corvid::haversine_km` — `search_geo_smoke_within_radius_and_nearest`, `geo_haversine_known_distances_symmetry_poles_antipodal`
- `corvid::GeoHit` — `search_geo_smoke_within_radius_and_nearest`, `geo_nearest_k_zero_one_n_beyond_and_hit_fields`
- `corvid::Collection::geo_within_radius` — `search_geo_smoke_within_radius_and_nearest`, `geo_within_radius_boundary_inclusive_and_ordering`, `geo_within_radius_zero_tiny_and_full_globe_radii`, `geo_within_radius_no_input_validation_mathematical_semantics`
- `corvid::Collection::geo_nearest` — `search_geo_smoke_within_radius_and_nearest`, `geo_nearest_k_zero_one_n_beyond_and_hit_fields`, `geo_nearest_equidistant_ties_break_by_key`, `geo_nearest_skips_non_points_empty_and_finds_antipodal`
- `corvid::Collection::geo_within_bbox` — `geo_within_bbox_normal_inclusive_edges_and_key_order`, `geo_within_bbox_degenerate_point_line_pole_and_globe`, `geo_bbox_antimeridian_wrap_matches_both_sides`, `geo_bbox_result_order_is_key_order_on_every_path`, `geo_bbox_validation_exact_error_variants`

## Schema (ALTER) — 36 construct(s)

- `corvid::Error::CorruptIndex` — `lifecycle_corrupt_ondisk_index_bytes_on_disk_error_queries_with_exact_variant`
- `corvid::Error::ReservedCollection` *(shared across classes: Mutations, Graph, Schema (ALTER))* — `mutations_insert_rejects_reserved_and_invalid_collection_names`, `mutations_write_paths_reject_reserved_and_invalid_collection_names`, `graph_link_unlink_reject_reserved_and_invalid_collection_names`, `schema_index_creation_validates_names_across_families`
- `corvid::Error::InvalidName` *(shared across classes: Mutations, Graph, Schema (ALTER), Lifecycle)* — `mutations_insert_rejects_reserved_and_invalid_collection_names`, `mutations_write_paths_reject_reserved_and_invalid_collection_names`, `graph_link_unlink_reject_reserved_and_invalid_collection_names`, `schema_index_creation_validates_names_across_families`, `lifecycle_load_with_renames_error_contract_invalid_target_collisions_and_noops`
- `corvid::Error::EmptyIndexTraining` *(shared across classes: Vector search, Schema (ALTER))* — `vector_create_index_overloads_inmemory_ondisk_and_pq`, `schema_vector_pq_creation_training_error_variants_and_success`
- `corvid::Error::SchemaViolation` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing`, `schema_unique_update_conflict_rejects_whole_write`, `schema_unique_nan_equals_nan_rejects_second_document`, `schema_unique_containers_bytes_text_vector_and_null_rule`, `schema_unique_delete_then_reinsert_same_value_allowed`, `schema_unique_batch_conflict_rolls_back_whole_batch`, `schema_unique_with_scalar_index_stays_enforced_and_moves_with_values`, `schema_unique_numeric_kind_equality_same_with_and_without_index`, `mutations_insert_batch_schema_violation_rolls_back_whole_batch`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`, `mutations_insert_auto_failure_does_not_burn_an_id`
- `corvid::Collection::create_geo_index` *(shared across classes: WHERE, SELECT shaping, Geo, Schema (ALTER))* — `filters_indexed_vs_scan_geo_window`, `queries_plan_shape_indexed_window_kinds_and_explain_families`, `geo_index_twins_equivalence_and_live_mutations`, `geo_index_plan_shape_serviceable_and_declined`, `schema_geo_index_duplicate_creation_and_non_point_docs_skipped`, `schema_geo_index_point_move_and_delete_maintained`
- `corvid::Collection::create_vector_index` *(shared across classes: SELECT shaping, Vector search, Schema (ALTER))* — `queries_plan_shape_ann_index_for_single_vector_source`, `vector_indexed_none_matches_unindexed_twin_for_all_k`, `vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback`, `vector_dimension_mismatch_skips_docs_and_index_falls_back`, `schema_vector_index_duplicate_creation_replaces_previous_params`, `schema_vector_index_over_empty_field_then_insert_immediately_searchable`, `schema_vector_index_mixed_dimensions_match_scan_twin`, `schema_vector_dimension_change_on_update_leaves_index`, `schema_vector_index_compaction_after_deletes_keeps_results_exact`
- `corvid::Collection::create_vector_index_quantized` *(shared across classes: Vector search, Schema (ALTER))* — `vector_metric_quantization_cross_orders_and_exact_distances`, `schema_vector_index_duplicate_creation_replaces_previous_params`
- `corvid::Collection::create_vector_index_ondisk` *(shared across classes: Vector search, Schema (ALTER))* — `vector_create_index_overloads_inmemory_ondisk_and_pq`, `schema_vector_index_duplicate_creation_replaces_previous_params`, `schema_vector_index_over_empty_field_then_insert_immediately_searchable`, `schema_vector_index_mixed_dimensions_match_scan_twin`, `schema_vector_dimension_change_on_update_leaves_index`, `schema_vector_index_compaction_after_deletes_keeps_results_exact`
- `corvid::Collection::create_vector_index_ondisk_quantized` *(shared across classes: Vector search, Schema (ALTER))* — `vector_create_index_overloads_inmemory_ondisk_and_pq`, `schema_vector_index_duplicate_creation_replaces_previous_params`
- `corvid::Collection::create_vector_index_ondisk_pq` *(shared across classes: Vector search, Schema (ALTER))* — `vector_create_index_overloads_inmemory_ondisk_and_pq`, `schema_vector_pq_creation_training_error_variants_and_success`
- `corvid::Collection::create_vector_index_pq` — `vector_inmemory_pq_cross_metrics_orders_and_exact_distances`, `vector_inmemory_pq_recall_determinism_and_reopen`, `vector_inmemory_pq_creation_requires_training_documents`
- `corvid::Collection::create_text_index` *(shared across classes: SELECT shaping, Text search, Schema (ALTER))* — `queries_plan_shape_text_index_for_single_text_source`, `text_search_index_inmemory_ondisk_match_scan_twin`, `text_builder_text_index_arm_matches_scan_arm`, `text_phrase_k_boundaries_empty_phrase_and_index_arms`, `schema_text_index_duplicate_and_non_text_values_excluded`, `schema_text_index_mutations_keep_search_correct`
- `corvid::Collection::create_text_index_ondisk` *(shared across classes: Text search, Schema (ALTER))* — `text_search_index_inmemory_ondisk_match_scan_twin`, `text_phrase_k_boundaries_empty_phrase_and_index_arms`, `schema_text_index_mutations_keep_search_correct`
- `corvid::Collection::create_scalar_index` *(shared across classes: Mutations, SELECT shaping, Schema (ALTER))* — `mutations_update_maintains_scalar_index`, `mutations_compare_and_set_maintains_scalar_index`, `mutations_delete_where_exact_with_scalar_index_present`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`, `queries_order_by_indexed_vs_scan_equivalent`, `schema_scalar_index_empty_collection_creates_and_serves_later`, `schema_scalar_index_backfill_makes_populated_collection_immediately_queryable`, `schema_scalar_index_duplicate_creation_replaces_without_stale_entries`, `schema_scalar_index_mixed_type_field_lanes_and_missing_docs_match_scan`, `schema_scalar_index_maintenance_contract_under_every_mutation_kind`
- `corvid::Collection::create_compound_index` *(shared across classes: WHERE, SELECT shaping, Schema (ALTER))* — `filters_indexed_vs_scan_compound_prefix`, `queries_plan_shape_indexed_window_kinds_and_explain_families`, `schema_compound_index_field_order_determines_serviceability`, `schema_compound_index_single_and_three_field_arities`, `schema_compound_index_duplicate_and_reverse_order_coexist`, `schema_compound_trailing_field_mutations_never_surface_stale_entries`
- `corvid::schema::FieldType` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::FieldType::Any` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::FieldType::Bool` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::FieldType::Int` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_schema_violation_rolls_back_whole_batch`
- `corvid::schema::FieldType::Float` — `schema_field_type_matrix_and_fields_accessor`, `schema_unique_nan_equals_nan_rejects_second_document`
- `corvid::schema::FieldType::Text` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::FieldType::Bytes` — `schema_unique_containers_bytes_text_vector_and_null_rule`
- `corvid::schema::FieldType::Vector` — `schema_unique_containers_bytes_text_vector_and_null_rule`
- `corvid::schema::FieldType::Array` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::FieldType::Map` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::Field` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Field::new` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Field::required` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::schema::Field::unique` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Schema` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Schema::new` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Schema::field` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::schema::Schema::fields` — `schema_field_type_matrix_and_fields_accessor`
- `corvid::Collection::set_schema` *(shared across classes: Schema (ALTER), Mutations)* — `schema_field_type_matrix_and_fields_accessor`, `mutations_insert_batch_schema_violation_rolls_back_whole_batch`, `mutations_insert_batch_unique_conflict_rolls_back_whole_batch`
- `corvid::Collection::schema` — `schema_getter_roundtrips_declared_fields`

## TTL — 4 construct(s)

- `corvid::Collection::insert_with_ttl` *(shared across classes: TTL, Mutations)* — `ttl_smoke_insert_with_ttl_purges_at_boundary`, `mutations_insert_with_ttl_sets_and_purges_expiry`, `ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite`, `ttl_timestamps_accept_i64_extremes_and_order_correctly`, `ttl_plain_write_paths_clear_expiry`, `ttl_purge_removes_doc_from_scalar_unique_vector_and_text_indexes`, `ttl_purge_cascades_edges_of_expired_document_both_namespaces`, `ttl_write_paths_reject_reserved_and_invalid_collection_names`
- `corvid::Collection::set_ttl` — `ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite`, `ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting`, `ttl_plain_write_paths_clear_expiry`, `ttl_write_paths_reject_reserved_and_invalid_collection_names`
- `corvid::Collection::ttl` *(shared across classes: TTL, Mutations)* — `ttl_smoke_insert_with_ttl_purges_at_boundary`, `mutations_insert_with_ttl_sets_and_purges_expiry`, `ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite`, `ttl_on_missing_doc_and_doc_without_expiry_both_none`, `ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting`
- `corvid::Collection::purge_expired` *(shared across classes: TTL, Mutations, Lifecycle)* — `ttl_smoke_insert_with_ttl_purges_at_boundary`, `mutations_insert_with_ttl_sets_and_purges_expiry`, `ttl_purge_boundary_one_before_exactly_at_one_after_and_idempotence`, `ttl_expired_doc_visible_until_purged_hidden_from_all_reads_after`, `ttl_timestamps_accept_i64_extremes_and_order_correctly`, `ttl_purge_removes_doc_from_scalar_unique_vector_and_text_indexes`, `ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting`, `ttl_purge_cascades_edges_of_expired_document_both_namespaces`, `ttl_purge_cascades_edges_of_stranded_entry_without_document`, `events_delete_paths_emit_exact_delete_vectors_in_order`, `events_ttl_purge_cascade_is_silent_and_stranded_purge_emits_nothing`

## Graph — 7 construct(s)

- `corvid::Collection::link` *(shared across classes: Graph, Lifecycle)* — `graph_smoke_link_neighbors_traverse_unlink`, `graph_link_new_edge_resolves_neighbors_and_in_neighbors`, `graph_link_duplicate_is_idempotent_and_reemits_insert_event`, `graph_link_self_loop_lists_self_but_traverse_excludes_start`, `graph_link_missing_endpoints_allowed_without_documents`, `graph_link_relation_isolation_empty_unicode_and_byte_prefix`, `graph_link_endpoint_keys_empty_and_unicode_in_byte_order`, `graph_link_unlink_reject_reserved_and_invalid_collection_names`, `events_link_to_missing_endpoints_still_emits_insert_keyed_by_from`
- `corvid::Collection::link_weighted` — `graph_link_weighted_roundtrip_and_overwrite_semantics`, `graph_link_weighted_float_extremes_round_trip`
- `corvid::Collection::neighbors_weighted` — `graph_link_weighted_roundtrip_and_overwrite_semantics`, `graph_link_weighted_float_extremes_round_trip`
- `corvid::Collection::unlink` — `graph_smoke_link_neighbors_traverse_unlink`, `graph_unlink_removes_edge_and_reverse_twin`, `graph_unlink_is_directional_reverse_direction_edge_survives`, `graph_unlink_missing_edge_is_silent_noop_returning_false`, `graph_unlink_removes_only_the_named_relation`, `graph_link_unlink_reject_reserved_and_invalid_collection_names`
- `corvid::Collection::neighbors` — `graph_smoke_link_neighbors_traverse_unlink`, `graph_link_relation_isolation_empty_unicode_and_byte_prefix`, `graph_link_endpoint_keys_empty_and_unicode_in_byte_order`, `graph_neighbors_key_order_no_out_edges_and_missing_node`
- `corvid::Collection::in_neighbors` — `graph_link_new_edge_resolves_neighbors_and_in_neighbors`, `graph_in_neighbors_mirror_target_only_and_mixed`, `graph_delete_cascades_edges_in_both_namespaces`
- `corvid::Collection::traverse` — `graph_smoke_link_neighbors_traverse_unlink`, `graph_traverse_depth_zero_empty_depth_one_equals_neighbors`, `graph_traverse_multi_hop_bfs_order_exact`, `graph_traverse_cycles_terminate_with_deduped_set`, `graph_traverse_branching_and_diamond_convergence_order`, `graph_traverse_relation_isolated_from_other_relations`, `graph_traverse_missing_start_node_is_empty`

## Joins — 2 construct(s)

- `corvid::JoinRow` — `joins_smoke_left_outer_resolves_and_misses`, `joins_happy_path_join_row_shape_is_exact`, `joins_missing_fk_field_and_dangling_reference_retain_rows_with_none`, `joins_foreign_key_kinds_text_bytes_int_and_unusable_shapes`, `joins_self_join_references_within_one_collection`, `joins_empty_left_empty_right_and_unknown_right_collection`, `joins_rows_follow_left_collection_key_order`, `joins_non_map_left_documents_retained_with_none`
- `corvid::Collection::join` — `joins_smoke_left_outer_resolves_and_misses`, `joins_happy_path_join_row_shape_is_exact`, `joins_dotted_foreign_key_path_resolves_nested_maps`, `joins_missing_fk_field_and_dangling_reference_retain_rows_with_none`, `joins_foreign_key_kinds_text_bytes_int_and_unusable_shapes`, `joins_self_join_references_within_one_collection`, `joins_empty_left_empty_right_and_unknown_right_collection`, `joins_rows_follow_left_collection_key_order`, `joins_track_right_and_left_side_mutations`, `joins_non_map_left_documents_retained_with_none`

## Lifecycle — 127 construct(s)

- `corvid::value::MAX_NESTING` — `lifecycle_value_decode_enforces_max_nesting_bound`
- `corvid::Value::encode` — `lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact`, `lifecycle_value_decode_enforces_max_nesting_bound`
- `corvid::Value::decode` — `lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact`, `lifecycle_value_decode_enforces_max_nesting_bound`
- `corvid::Result` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Error` *(shared across classes: Schema (ALTER), Mutations)* — `schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing`, `mutations_insert_rejects_reserved_and_invalid_collection_names`
- `corvid::Error::Database` — `lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent`, `lifecycle_db_open_second_handle_to_same_file_hits_the_redb_exclusive_lock`, `lifecycle_db_backup_restores_identical_state_and_pins_error_paths`
- `corvid::Error::Transaction` — *exempt from strict coverage: redb passthrough: transaction failure requires a redb-internal fault; no public-API path reaches it (fault-injection hooks are forbidden, Ruling 3)*
- `corvid::Error::Table` — *exempt from strict coverage: redb passthrough: table-open failure requires corruption inside redb's private table format, which public engine calls cannot produce*
- `corvid::Error::Storage` — *exempt from strict coverage: redb passthrough: storage-level I/O errors surface only from redb internals (disk fault mid-operation), not from any public call*
- `corvid::Error::Commit` — *exempt from strict coverage: redb passthrough: commit failure is a redb-internal disk fault mid-commit; unreachable without fault injection (Ruling 3)*
- `corvid::Error::SetDurability` — *exempt from strict coverage: redb passthrough: the durability-mode switch fails only inside redb's own set_durability, on conditions public inputs cannot produce*
- `corvid::Error::Compaction` — *exempt from strict coverage: redb passthrough: compaction failure needs an I/O fault during redb's compaction pass inside `Db::compact` internals*
- `corvid::Error::Decode` — `lifecycle_value_decode_enforces_max_nesting_bound`
- `corvid::Error::IncompatibleFormat` — *exempt from strict coverage: redb's format-version marker lives in its META pages, not the public byte layer the engine writes; an engine-level version mismatch is refused earlier by the engine's own on-disk format marker*
- `corvid::Error::InvalidDump` — `lifecycle_load_rejects_reserved_names_and_malformed_streams`
- `corvid::Error::BackupTargetExists` — `lifecycle_db_backup_restores_identical_state_and_pins_error_paths`, `lifecycle_store_backup_copies_to_an_independent_openable_file`
- `corvid::Error::Io` — `lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface`
- `corvid::PlanShape` *(shared across classes: WHERE, SELECT shaping)* — `filters_indexed_vs_scan_scalar_predicates`, `queries_plan_shape_indexed_window_kinds_and_explain_families`
- `corvid::PlanShape::AnnIndex` — `queries_plan_shape_ann_index_for_single_vector_source`
- `corvid::PlanShape::TextIndex` — `queries_plan_shape_text_index_for_single_text_source`
- `corvid::PlanShape::IndexedWindow` *(shared across classes: WHERE, SELECT shaping)* — `filters_indexed_vs_scan_scalar_predicates`, `queries_plan_shape_indexed_window_kinds_and_explain_families`
- `corvid::PlanShape::SortIndex` — `queries_plan_shape_sort_index_for_order_by_on_indexed_field`, `queries_order_by_index_walk_parity_across_kind_lattice`
- `corvid::PlanShape::StreamingTopK` — `queries_plan_shape_streaming_topk_without_index`
- `corvid::PlanShape::Scan` *(shared across classes: WHERE, SELECT shaping)* — `filters_indexed_vs_scan_scalar_predicates`, `queries_plan_shape_indexed_window_kinds_and_explain_families`
- `corvid::QueryBuilder::plan` — `lifecycle_query_plan_key_is_canonical_for_identical_shapes`
- `corvid::QueryBuilder::plan_shape` *(shared across classes: WHERE, SELECT shaping)* — `filters_indexed_vs_scan_scalar_predicates`, `queries_plan_shape_ann_index_for_single_vector_source`, `queries_plan_shape_text_index_for_single_text_source`, `queries_plan_shape_indexed_window_kinds_and_explain_families`, `queries_plan_shape_streaming_topk_without_index`, `queries_order_by_indexed_vs_scan_equivalent`
- `corvid::QueryBuilder::explain` — `queries_plan_shape_ann_index_for_single_vector_source`, `queries_plan_shape_text_index_for_single_text_source`, `queries_plan_shape_indexed_window_kinds_and_explain_families`, `queries_plan_shape_streaming_topk_without_index`
- `corvid::Db` *(shared across classes: Mutations, Lifecycle)* — `mutations_smoke_insert_roundtrips`, `lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent`
- `corvid::Db::open` — `lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent`, `lifecycle_db_open_second_handle_to_same_file_hits_the_redb_exclusive_lock`
- `corvid::Db::open_in_memory` *(shared across classes: Mutations, Lifecycle)* — `mutations_smoke_insert_roundtrips`, `lifecycle_db_open_in_memory_instances_are_isolated`
- `corvid::Db::open_with_backend` — `backend_db_open_with_backend_roundtrips_across_drop_and_reopen`, `backend_failing_write_surfaces_clean_error_not_panic`
- `corvid::Db::collection` — `mutations_smoke_insert_roundtrips`
- `corvid::Db::backup` — `lifecycle_db_backup_restores_identical_state_and_pins_error_paths`
- `corvid::Db::backup_with_backend` — `backend_backup_with_backend_copies_to_independent_reopenable_backend`
- `corvid::Db::bulk` — `lifecycle_db_bulk_is_a_durability_scope_writes_before_err_persist`, `lifecycle_db_bulk_happy_path_applies_and_survives_reopen`
- `corvid::Db::compact` — `lifecycle_db_compact_keeps_data_intact_and_tolerates_double_compact`
- `corvid::Db::collections` — `lifecycle_db_collections_filters_graph_ttl_and_index_namespaces`
- `corvid::Store` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::open` — `lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable`, `lifecycle_store_backup_copies_to_an_independent_openable_file`
- `corvid::Store::open_in_memory` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::Store::open_with_backend` — `backend_seam_dispatches_syncs_and_closes_exactly_once`, `backend_failing_write_surfaces_clean_error_not_panic`
- `corvid::Store::set_relaxed_durability` — `lifecycle_store_set_relaxed_durability_and_flush_keep_data_durable`
- `corvid::Store::begin_bulk` — `lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable`
- `corvid::Store::flush` — `lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable`, `lifecycle_store_set_relaxed_durability_and_flush_keep_data_durable`
- `corvid::Store::compact` — `lifecycle_db_compact_keeps_data_intact_and_tolerates_double_compact`
- `corvid::Store::next_auto_id` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`, `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::Store::backup` — `lifecycle_store_backup_copies_to_an_independent_openable_file`
- `corvid::Store::backup_with_backend` — `backend_backup_with_backend_copies_to_independent_reopenable_backend`
- `corvid::Store::transaction` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::Store::read` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::Store::put` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::get` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::delete` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::scan` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::collections` — `lifecycle_db_collections_filters_graph_ttl_and_index_namespaces`
- `corvid::Store::scan_from` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::count` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::for_each` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::Store::scan_prefix` — `lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts`
- `corvid::store::BulkScope` — `lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable`
- `corvid::store::WriteBatch` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::put` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::get` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::delete` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::scan` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::scan_from` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::WriteBatch::next_auto_id` — `lifecycle_store_transaction_commit_rollback_and_write_batch_surface`
- `corvid::store::ReadBatch` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::collections` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::auto_ids` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::get` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::scan` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::scan_from` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::scan_prefix` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::store::ReadBatch::for_each` — `lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops`
- `corvid::ChangeKind` *(shared across classes: Lifecycle, Mutations)* — `events_smoke_subscribe_records_insert_and_delete`, `mutations_emit_change_events_per_mutation_kind`, `events_insert_paths_emit_exact_insert_vectors_in_order`, `events_update_and_patch_emit_exact_vectors_for_both_branches`, `events_compare_and_set_emit_exact_vectors_per_branch`, `events_delete_paths_emit_exact_delete_vectors_in_order`
- `corvid::ChangeKind::Insert` *(shared across classes: Lifecycle, Mutations)* — `events_smoke_subscribe_records_insert_and_delete`, `mutations_emit_change_events_per_mutation_kind`, `events_insert_paths_emit_exact_insert_vectors_in_order`, `events_update_and_patch_emit_exact_vectors_for_both_branches`, `events_compare_and_set_emit_exact_vectors_per_branch`, `events_link_to_missing_endpoints_still_emits_insert_keyed_by_from`
- `corvid::ChangeKind::Delete` *(shared across classes: Lifecycle, Mutations)* — `events_smoke_subscribe_records_insert_and_delete`, `mutations_emit_change_events_per_mutation_kind`, `events_delete_paths_emit_exact_delete_vectors_in_order`, `events_compare_and_set_emit_exact_vectors_per_branch`, `events_ttl_purge_cascade_is_silent_and_stranded_purge_emits_nothing`
- `corvid::ChangeEvent` *(shared across classes: Lifecycle, Mutations)* — `events_smoke_subscribe_records_insert_and_delete`, `mutations_emit_change_events_per_mutation_kind`, `events_multiple_subscribers_all_receive_identical_exact_vectors`, `events_cross_collection_tagging_each_event_names_its_collection`, `events_dispatch_is_synchronous_post_commit_and_in_mutation_order`
- `corvid::SubscriptionId` — `events_smoke_subscribe_records_insert_and_delete`, `events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence`
- `corvid::Db::subscribe` *(shared across classes: Lifecycle, Mutations)* — `events_smoke_subscribe_records_insert_and_delete`, `mutations_emit_change_events_per_mutation_kind`, `events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence`, `events_multiple_subscribers_all_receive_identical_exact_vectors`, `events_cross_collection_tagging_each_event_names_its_collection`
- `corvid::Db::unsubscribe` — `events_smoke_subscribe_records_insert_and_delete`, `events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence`
- `corvid::SemanticCache` — `lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine`, `lifecycle_semantic_cache_threshold_units_follow_the_metric_l2`
- `corvid::Collection::semantic_cache` — `lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine`, `lifecycle_semantic_cache_threshold_units_follow_the_metric_l2`
- `corvid::SemanticCache::put` — `lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine`
- `corvid::SemanticCache::get` — `lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine`, `lifecycle_semantic_cache_threshold_units_follow_the_metric_l2`
- `corvid::HyperLogLog` — `lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates`
- `corvid::HyperLogLog::new` — `lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates`
- `corvid::HyperLogLog::with_precision` — `lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates`
- `corvid::HyperLogLog::add_bytes` — `lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates`, `lifecycle_hyperloglog_add_hash_is_the_precomputed_twin_of_add_bytes`
- `corvid::HyperLogLog::add_hash` — `lifecycle_hyperloglog_add_hash_is_the_precomputed_twin_of_add_bytes`
- `corvid::HyperLogLog::estimate` — `lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates`
- `corvid::BloomFilter` — `lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate`
- `corvid::BloomFilter::new` — `lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate`
- `corvid::BloomFilter::add_bytes` — `lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate`
- `corvid::BloomFilter::contains_bytes` — `lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate`
- `corvid::CuckooFilter` — `lifecycle_cuckoo_filter_membership_delete_and_bounded_fp`, `lifecycle_cuckoo_filter_overflow_rejects_and_rollback_preserves_admitted`
- `corvid::CuckooFilter::new` — `lifecycle_cuckoo_filter_membership_delete_and_bounded_fp`, `lifecycle_cuckoo_filter_overflow_rejects_and_rollback_preserves_admitted`
- `corvid::CuckooFilter::add_bytes` — `lifecycle_cuckoo_filter_membership_delete_and_bounded_fp`, `lifecycle_cuckoo_filter_overflow_rejects_and_rollback_preserves_admitted`
- `corvid::CuckooFilter::contains_bytes` — `lifecycle_cuckoo_filter_membership_delete_and_bounded_fp`, `lifecycle_cuckoo_filter_overflow_rejects_and_rollback_preserves_admitted`
- `corvid::CuckooFilter::delete_bytes` — `lifecycle_cuckoo_filter_membership_delete_and_bounded_fp`
- `corvid::TDigest` — `lifecycle_tdigest_exact_boundaries_nan_and_monotone_cdf`, `lifecycle_tdigest_merge_algebra_and_bounded_error`
- `corvid::TDigest::new` — `lifecycle_tdigest_exact_boundaries_nan_and_monotone_cdf`, `lifecycle_tdigest_merge_algebra_and_bounded_error`
- `corvid::TDigest::add` — `lifecycle_tdigest_exact_boundaries_nan_and_monotone_cdf`, `lifecycle_tdigest_merge_algebra_and_bounded_error`
- `corvid::TDigest::merge` — `lifecycle_tdigest_merge_algebra_and_bounded_error`
- `corvid::TDigest::quantile` — `lifecycle_tdigest_exact_boundaries_nan_and_monotone_cdf`, `lifecycle_tdigest_merge_algebra_and_bounded_error`
- `corvid::TDigest::cdf` — `lifecycle_tdigest_exact_boundaries_nan_and_monotone_cdf`
- `corvid::MinHash` — `lifecycle_minhash_signature_invariance_and_jaccard_bounds`
- `corvid::MinHash::new` — `lifecycle_minhash_signature_invariance_and_jaccard_bounds`
- `corvid::MinHash::signature` — `lifecycle_minhash_signature_invariance_and_jaccard_bounds`
- `corvid::MinHash::jaccard_estimate` — `lifecycle_minhash_signature_invariance_and_jaccard_bounds`
- `corvid::LshIndex` — `lifecycle_lsh_banding_recall_and_skew_fixed_corpus`
- `corvid::LshIndex::new` — `lifecycle_lsh_banding_recall_and_skew_fixed_corpus`
- `corvid::LshIndex::insert` — `lifecycle_lsh_banding_recall_and_skew_fixed_corpus`
- `corvid::LshIndex::candidates` — `lifecycle_lsh_banding_recall_and_skew_fixed_corpus`
- `corvid::Db::dump` — `lifecycle_smoke_dump_load_roundtrips_documents`, `lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact`, `lifecycle_dump_load_roundtrips_every_index_family_ttl_edges_schema_and_autoids`, `lifecycle_dump_load_into_nonempty_db_merges_records_and_counters`, `lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface`
- `corvid::Db::load` — `lifecycle_smoke_dump_load_roundtrips_documents`, `lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact`, `lifecycle_dump_load_roundtrips_every_index_family_ttl_edges_schema_and_autoids`, `lifecycle_dump_load_into_nonempty_db_merges_records_and_counters`, `lifecycle_load_rejects_reserved_names_and_malformed_streams`, `lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface`
- `corvid::Db::load_with_renames` — `lifecycle_load_with_renames_migrates_a_legacy_pre_wave4_dump`, `lifecycle_load_with_renames_error_contract_invalid_target_collisions_and_noops`
- `corvid::QueryPlan` — `lifecycle_query_plan_key_is_canonical_for_identical_shapes`
- `corvid::QueryPlan::key` — `lifecycle_query_plan_key_is_canonical_for_identical_shapes`
- `corvid::PlanCache` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::new` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::get` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::insert` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::get_or_insert_with` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::len` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`
- `corvid::PlanCache::is_empty` — `lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once`

## MCP wire — 51 construct(s)

The sidecar's whole surface is one class: the JSON-RPC envelopes, every
tool name, and the Rust items a client's bytes flow through. Covered by
the in-process duplex-I/O suite in `crates/corvid-mcp/tests/tools/`.

- `corvid_mcp::Server` — `server_new_wraps_an_engine_db`, `tools_smoke_in_process_wire_roundtrip`
- `corvid_mcp::Server::new` — `server_new_wraps_an_engine_db`
- `corvid_mcp::Server::open` — `backup_reopens_as_a_live_database`, `open_server_memory_and_file_backed`
- `corvid_mcp::Server::open_in_memory` — `envelope_initialize_result_shape`, `tools_smoke_in_process_wire_roundtrip`
- `corvid_mcp::Server::handle` — `envelope_error_taxonomy_three_surfaces`, `store_then_get_roundtrips_and_overwrites`
- `corvid_mcp::ToolError` — `envelope_error_taxonomy_three_surfaces`
- `corvid_mcp::ToolError::UnknownTool` — `envelope_error_taxonomy_three_surfaces`
- `corvid_mcp::ToolError::BadParams` — `envelope_error_taxonomy_three_surfaces`, `store_and_get_param_errors`
- `corvid_mcp::ToolError::Engine` — `envelope_error_taxonomy_three_surfaces`, `store_engine_name_errors_surface`
- `corvid_mcp::convert::json_to_value` — `vector_wrapper_roundtrips_through_the_wire`, `convert_malformed_wrappers_fall_back_to_maps`, `convert_int_float_distinction_survives`, `convert_u64_beyond_i64_is_lossy_float`
- `corvid_mcp::convert::value_to_json` — `bytes_wrapper_roundtrips_through_the_wire`, `convert_wrappers_nested_and_multi_key`, `convert_vector_components_are_f32_precision`, `convert_unicode_text_survives`
- `corvid_mcp::protocol::PROTOCOL_VERSION` — `envelope_initialize_result_shape`, `tools_smoke_in_process_wire_roundtrip`
- `corvid_mcp::protocol::MAX_FRAME_SIZE` — `frame_over_default_max_frame_size_is_refused`
- `corvid_mcp::protocol::open_server` — `open_server_memory_and_file_backed`
- `corvid_mcp::protocol::run` — `envelope_session_multiple_requests_in_order`, `frame_over_default_max_frame_size_is_refused`, `tools_smoke_in_process_wire_roundtrip`
- `corvid_mcp::protocol::run_with_limit` — `frame_size_boundary_exact_and_one_over`
- `corvid_mcp::protocol::handle_request` — `envelope_initialize_result_shape`, `envelope_notifications_produce_no_response`
- `mcp::envelope::initialize` — `envelope_initialize_result_shape`, `tools_smoke_in_process_wire_roundtrip`
- `mcp::envelope::ping` — `envelope_ping_empty_result`, `envelope_blank_and_crlf_frames_are_ignored`
- `mcp::envelope::tools/list` — `envelope_tools_list_all_29_with_schemas`
- `mcp::envelope::tools/call` — `envelope_tools_call_content_shape`, `envelope_tools_call_malformed_request_is_invalid_params`
- `mcp::envelope::error_response` — `envelope_unknown_and_missing_method_codes`, `envelope_malformed_line_is_parse_error_and_loop_survives`
- `mcp::tool::store` — `store_then_get_roundtrips_and_overwrites`, `store_accepts_every_json_document_kind`, `store_and_get_param_errors`, `store_engine_name_errors_surface`
- `mcp::tool::patch` — `patch_merges_top_level_and_creates_missing`
- `mcp::tool::compare_and_set` — `compare_and_set_absent_expected_and_mismatch`, `compare_and_set_new_omitted_deletes`
- `mcp::tool::get` — `store_then_get_roundtrips_and_overwrites`, `get_missing_key_and_unknown_collection_are_null`
- `mcp::tool::delete` — `delete_reports_outcome_and_param_errors`
- `mcp::tool::delete_where` — `delete_where_counts_and_filter_errors`
- `mcp::tool::search` — `search_vector_orders_by_similarity`, `search_filter_op_matrix`, `search_limit_validation_matrix`, `search_engine_invalid_argument_mmr_and_rrf`
- `mcp::tool::create_index` — `create_index_variants_then_search`, `create_index_param_and_training_errors`, `index_tools_on_disk_flag_type_errors`
- `mcp::tool::link` — `link_without_docs_and_duplicate_is_idempotent`, `graph_param_errors`
- `mcp::tool::unlink` — `unlink_reports_removed_true_then_false`
- `mcp::tool::neighbors` — `neighbors_and_in_neighbors_directions`, `list_tools_clamp_oversized_limit_and_reject_invalid`
- `mcp::tool::traverse` — `traverse_hops_cycles_and_empty_starts`, `graph_param_errors`
- `mcp::tool::geo` — `geo_radius_nearest_and_limit`, `geo_param_errors`
- `mcp::tool::join` — `join_left_outer_rows_and_missing_references`, `join_int_foreign_key_matches_decimal_text_key`, `list_tools_clamp_oversized_limit_and_reject_invalid`
- `mcp::tool::in_neighbors` — `neighbors_and_in_neighbors_directions`, `list_tools_clamp_oversized_limit_and_reject_invalid`
- `mcp::tool::page` — `page_cursor_walk_default_and_boundaries`
- `mcp::tool::phrase_search` — `phrase_search_ordered_tokens_and_k_bounds`
- `mcp::tool::create_text_index` — `create_text_index_memory_and_ondisk`, `index_tools_param_and_name_errors`, `index_tools_on_disk_flag_type_errors`
- `mcp::tool::create_scalar_index` — `create_scalar_index_exact_under_mutation`, `index_tools_param_and_name_errors`
- `mcp::tool::create_geo_index` — `create_geo_index_then_radius_exact`, `index_tools_param_and_name_errors`
- `mcp::tool::create_compound_index` — `create_compound_index_and_fields_errors`, `index_tools_param_and_name_errors`
- `mcp::tool::backup` — `backup_reopens_as_a_live_database`, `backup_existing_target_and_missing_path_errors`
- `mcp::tool::dump` — `dump_then_load_roundtrips_through_the_wire`, `load_missing_and_garbage_file_errors`
- `mcp::tool::load` — `dump_then_load_roundtrips_through_the_wire`, `load_missing_and_garbage_file_errors`, `load_rename_param_migrates_collections_through_the_wire`
- `mcp::tool::list_collections` — `list_collections_lists_user_names_exactly`
- `mcp::tool::count` — `count_exact_with_filter_and_unknown_collection`, `create_scalar_index_exact_under_mutation`
- `mcp::tool::insert_auto` — `insert_auto_keys_ordered_and_distinct`, `dump_then_load_roundtrips_through_the_wire`
- `mcp::tool::set_schema` — `set_schema_then_get_schema_roundtrips`, `set_schema_unique_enforced_on_stores`, `set_schema_required_and_type_violations`, `set_schema_param_and_name_errors`, `dump_load_preserves_schema_constraints`, `set_schema_flag_type_errors`, `set_schema_declared_empty_vs_undeclared_fields`
- `mcp::tool::get_schema` — `set_schema_then_get_schema_roundtrips`, `set_schema_param_and_name_errors`, `dump_load_preserves_schema_constraints`, `set_schema_declared_empty_vs_undeclared_fields`

331 engine construct(s) across 13 statement classes, 51 wire construct(s),
309 distinct covering tests (existence and uniqueness enforced by the
radars; the 7 exempt row(s) above are the only uncovered ones, each
with its justification).

## Semantics notes

Cross-class contracts the conformance program pins; each note names the
suite that owns it.

### Pre-ranking predicates and BM25 (Text search)

A builder text query with a filter ranks the *filtered* candidate set:
the predicate runs first (index window or scan), and the BM25
statistics — document frequencies, average document length — are
computed over exactly those candidates, not the whole collection.
The same query without a filter scores against full-corpus stats, so
a score always means "relevance within the candidate set the filter
admits". Owned by `tests/search_text.rs`.

### geo_within_bbox returns key order, portably (Geo)

`geo_within_bbox` materializes its result sorted by key on every path:
the indexed window and the scan are byte-identical, documents
included. Key order is the contract — never cell or insertion order.
Owned by `tests/search_geo.rs`.

### NaN duality: comparisons vs storage equality

Two rules coexist by design:

- *Predicate comparisons* (`eq`/`ne`, every ordered operator, `is_in`,
`between`): NaN matches nothing, not even NaN — a NaN filter value
selects an empty set (`ne` selects everything else).
- *Storage equality* (`compare_and_set` expected values, unique
constraints): NaN equals NaN regardless of payload, and `-0.0` equals
`0.0` — the shared semantic rule (owned by `tests/mutations.rs` and
`tests/schema.rs`).

### Equality is per-construct

| Construct | Equality rule |
|---|---|
| `compare_and_set` expected value | Semantic value equality: NaN==NaN across payloads, -0.0==0.0, containers element-wise |
| Predicates (`eq`/`ne`, ordered ops) | Typed total-order comparison: NaN never equals anything; `Int(2)` equals `Float(2.0)` numerically (mixed comparisons convert the integer through f64, exact up to 2^53) |
| Unique constraints | Storage-level semantic equality (NaN==NaN), enforced per field value on write |
| Joins | An `Int` foreign key matches a `Text` key via its decimal-string encoding: `Int(7)` joins to the key `"7"` |
| Group keys (`group_count`/`group_sum`/`group_avg`, `count_distinct`) | Type-tagged canonical keys: bare for text (`blog`), `i:`/`f:`/`b:` tags for non-text, `t:` escape for text that would look tagged — distinct types stay distinct |

