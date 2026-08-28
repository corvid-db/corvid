//! The MCP sidecar surface manifest and radar (conformance program, Task 2).
//!
//! [`MANIFEST`] is the single source of truth for everything a client can say
//! to `corvid-mcp`. The sidecar's "language" has two layers, and the manifest
//! carries both:
//!
//! - **Rust rows** (`corvid_mcp::…`): every public item of the crate —
//!   [`Server`](corvid_mcp::Server) and its public methods, the
//!   [`ToolError`](corvid_mcp::ToolError) variants, the `convert` functions,
//!   and the public items of `protocol`. `lib.rs` re-exports do not get their
//!   own rows; the row belongs to the defining item, canonicalized to its
//!   shortest public path. `main.rs` is a bin: `fn main` is not library
//!   surface.
//! - **Wire rows** (`mcp::tool::<name>`, `mcp::envelope::<kind>`): the parts
//!   of the language that are not Rust items at all — one row per MCP tool
//!   name and one row per JSON-RPC envelope kind. These are validated by
//!   dedicated completeness checks against runtime (`tools/list`) and the
//!   protocol sources, not by the Rust-item extractor.
//!
//! Every row carries its statement class from the conformance plan's taxonomy
//! table (for this crate, always "MCP wire") and the integration tests that
//! currently cover it.
//!
//! The radar tests fail on any drift, in both directions: a public item or
//! tool added without a manifest row fails, and a row whose item no longer
//! exists fails. A further check keeps `covering_tests` honest: every cited
//! name must be a `fn` in this `tests/` tree (never a unit test inside
//! `src/`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use corvid_mcp::protocol::handle_request;
use corvid_mcp::{Server, ToolError};
use serde_json::json;

/// One manifest row: a public construct, its statement class (one of
/// [`CLASSES`], spelled exactly as the plan's taxonomy table spells it), and
/// the integration tests that cover it.
pub struct Row {
    /// Fully qualified canonical path: `corvid_mcp::Server::handle` for Rust
    /// items, `mcp::tool::<name>` / `mcp::envelope::<kind>` for wire syntax.
    pub item: &'static str,
    /// The statement class from the plan's taxonomy.
    pub class: &'static str,
    /// Names of `fn`s in `crates/corvid-mcp/tests/` that cover this item.
    pub covering_tests: &'static [&'static str],
}

/// When `true`, a manifest row with empty `covering_tests` fails the radar.
/// `false` while the conformance waves populate the suite; Task 14 has now
/// filled every row — Task 15 deletes the flag entirely, making strict mode
/// permanent.
const STRICT_COVERING: bool = false;

/// The statement-class names, exactly as the conformance plan's taxonomy
/// table spells them. The sidecar's whole surface is one class: "MCP wire".
const CLASSES: &[&str] = &["MCP wire"];

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

/// The complete language surface of the sidecar, classified.
static MANIFEST: &[Row] = &[
    // ===== server.rs — the tool layer (canonicalized through lib.rs) =====
    row(
        "corvid_mcp::Server",
        "MCP wire",
        &[
            "server_new_wraps_an_engine_db",
            "tools_smoke_in_process_wire_roundtrip",
        ],
    ),
    row(
        "corvid_mcp::Server::new",
        "MCP wire",
        &["server_new_wraps_an_engine_db"],
    ),
    row(
        "corvid_mcp::Server::open",
        "MCP wire",
        &[
            "backup_reopens_as_a_live_database",
            "open_server_memory_and_file_backed",
        ],
    ),
    row(
        "corvid_mcp::Server::open_in_memory",
        "MCP wire",
        &[
            "envelope_initialize_result_shape",
            "tools_smoke_in_process_wire_roundtrip",
        ],
    ),
    row(
        "corvid_mcp::Server::handle",
        "MCP wire",
        &[
            "envelope_error_taxonomy_three_surfaces",
            "store_then_get_roundtrips_and_overwrites",
        ],
    ),
    // ===== error.rs — the tool error vocabulary =====
    row(
        "corvid_mcp::ToolError",
        "MCP wire",
        &["envelope_error_taxonomy_three_surfaces"],
    ),
    row(
        "corvid_mcp::ToolError::UnknownTool",
        "MCP wire",
        &["envelope_error_taxonomy_three_surfaces"],
    ),
    row(
        "corvid_mcp::ToolError::BadParams",
        "MCP wire",
        &[
            "envelope_error_taxonomy_three_surfaces",
            "store_and_get_param_errors",
        ],
    ),
    row(
        "corvid_mcp::ToolError::Engine",
        "MCP wire",
        &[
            "envelope_error_taxonomy_three_surfaces",
            "store_engine_name_errors_surface",
        ],
    ),
    // ===== convert.rs — the JSON <-> engine value conventions =====
    row(
        "corvid_mcp::convert::json_to_value",
        "MCP wire",
        &[
            "vector_wrapper_roundtrips_through_the_wire",
            "convert_malformed_wrappers_fall_back_to_maps",
            "convert_int_float_distinction_survives",
            "convert_u64_beyond_i64_is_lossy_float",
        ],
    ),
    row(
        "corvid_mcp::convert::value_to_json",
        "MCP wire",
        &[
            "bytes_wrapper_roundtrips_through_the_wire",
            "convert_wrappers_nested_and_multi_key",
            "convert_vector_components_are_f32_precision",
            "convert_unicode_text_survives",
        ],
    ),
    // ===== protocol.rs — the transport =====
    row(
        "corvid_mcp::protocol::PROTOCOL_VERSION",
        "MCP wire",
        &[
            "envelope_initialize_result_shape",
            "tools_smoke_in_process_wire_roundtrip",
        ],
    ),
    row(
        "corvid_mcp::protocol::MAX_FRAME_SIZE",
        "MCP wire",
        &["frame_over_default_max_frame_size_is_refused"],
    ),
    row(
        "corvid_mcp::protocol::open_server",
        "MCP wire",
        &["open_server_memory_and_file_backed"],
    ),
    row(
        "corvid_mcp::protocol::run",
        "MCP wire",
        &[
            "envelope_session_multiple_requests_in_order",
            "frame_over_default_max_frame_size_is_refused",
            "tools_smoke_in_process_wire_roundtrip",
        ],
    ),
    row(
        "corvid_mcp::protocol::run_with_limit",
        "MCP wire",
        &["frame_size_boundary_exact_and_one_over"],
    ),
    row(
        "corvid_mcp::protocol::handle_request",
        "MCP wire",
        &[
            "envelope_initialize_result_shape",
            "envelope_notifications_produce_no_response",
        ],
    ),
    // ===== JSON-RPC envelope kinds (wire syntax, not Rust items) =====
    row(
        "mcp::envelope::initialize",
        "MCP wire",
        &[
            "envelope_initialize_result_shape",
            "tools_smoke_in_process_wire_roundtrip",
        ],
    ),
    row(
        "mcp::envelope::ping",
        "MCP wire",
        &[
            "envelope_ping_empty_result",
            "envelope_blank_and_crlf_frames_are_ignored",
        ],
    ),
    row(
        "mcp::envelope::tools/list",
        "MCP wire",
        &["envelope_tools_list_all_29_with_schemas"],
    ),
    row(
        "mcp::envelope::tools/call",
        "MCP wire",
        &[
            "envelope_tools_call_content_shape",
            "envelope_tools_call_malformed_request_is_invalid_params",
        ],
    ),
    row(
        "mcp::envelope::error_response",
        "MCP wire",
        &[
            "envelope_unknown_and_missing_method_codes",
            "envelope_malformed_line_is_parse_error_and_loop_survives",
        ],
    ),
    // ===== MCP tool names (wire syntax, not Rust items), in tools/list order =====
    row(
        "mcp::tool::store",
        "MCP wire",
        &[
            "store_then_get_roundtrips_and_overwrites",
            "store_accepts_every_json_document_kind",
            "store_and_get_param_errors",
            "store_engine_name_errors_surface",
        ],
    ),
    row(
        "mcp::tool::patch",
        "MCP wire",
        &["patch_merges_top_level_and_creates_missing"],
    ),
    row(
        "mcp::tool::compare_and_set",
        "MCP wire",
        &[
            "compare_and_set_absent_expected_and_mismatch",
            "compare_and_set_new_omitted_deletes",
        ],
    ),
    row(
        "mcp::tool::get",
        "MCP wire",
        &[
            "store_then_get_roundtrips_and_overwrites",
            "get_missing_key_and_unknown_collection_are_null",
        ],
    ),
    row(
        "mcp::tool::delete",
        "MCP wire",
        &["delete_reports_outcome_and_param_errors"],
    ),
    row(
        "mcp::tool::delete_where",
        "MCP wire",
        &["delete_where_counts_and_filter_errors"],
    ),
    row(
        "mcp::tool::search",
        "MCP wire",
        &[
            "search_vector_orders_by_similarity",
            "search_filter_op_matrix",
            "search_limit_validation_matrix",
            "search_engine_invalid_argument_mmr_and_rrf",
        ],
    ),
    row(
        "mcp::tool::create_index",
        "MCP wire",
        &[
            "create_index_variants_then_search",
            "create_index_param_and_training_errors",
        ],
    ),
    row(
        "mcp::tool::link",
        "MCP wire",
        &[
            "link_without_docs_and_duplicate_is_idempotent",
            "graph_param_errors",
        ],
    ),
    row(
        "mcp::tool::unlink",
        "MCP wire",
        &["unlink_reports_removed_true_then_false"],
    ),
    row(
        "mcp::tool::neighbors",
        "MCP wire",
        &[
            "neighbors_and_in_neighbors_directions",
            "list_tools_clamp_oversized_limit_and_reject_invalid",
        ],
    ),
    row(
        "mcp::tool::traverse",
        "MCP wire",
        &[
            "traverse_hops_cycles_and_empty_starts",
            "graph_param_errors",
        ],
    ),
    row(
        "mcp::tool::geo",
        "MCP wire",
        &["geo_radius_nearest_and_limit", "geo_param_errors"],
    ),
    row(
        "mcp::tool::join",
        "MCP wire",
        &[
            "join_left_outer_rows_and_missing_references",
            "join_int_foreign_key_matches_decimal_text_key",
            "list_tools_clamp_oversized_limit_and_reject_invalid",
        ],
    ),
    row(
        "mcp::tool::in_neighbors",
        "MCP wire",
        &[
            "neighbors_and_in_neighbors_directions",
            "list_tools_clamp_oversized_limit_and_reject_invalid",
        ],
    ),
    row(
        "mcp::tool::page",
        "MCP wire",
        &["page_cursor_walk_default_and_boundaries"],
    ),
    row(
        "mcp::tool::phrase_search",
        "MCP wire",
        &["phrase_search_ordered_tokens_and_k_bounds"],
    ),
    row(
        "mcp::tool::create_text_index",
        "MCP wire",
        &[
            "create_text_index_memory_and_ondisk",
            "index_tools_param_and_name_errors",
        ],
    ),
    row(
        "mcp::tool::create_scalar_index",
        "MCP wire",
        &[
            "create_scalar_index_exact_under_mutation",
            "index_tools_param_and_name_errors",
        ],
    ),
    row(
        "mcp::tool::create_geo_index",
        "MCP wire",
        &[
            "create_geo_index_then_radius_exact",
            "index_tools_param_and_name_errors",
        ],
    ),
    row(
        "mcp::tool::create_compound_index",
        "MCP wire",
        &[
            "create_compound_index_and_fields_errors",
            "index_tools_param_and_name_errors",
        ],
    ),
    row(
        "mcp::tool::backup",
        "MCP wire",
        &[
            "backup_reopens_as_a_live_database",
            "backup_existing_target_and_missing_path_errors",
        ],
    ),
    row(
        "mcp::tool::dump",
        "MCP wire",
        &[
            "dump_then_load_roundtrips_through_the_wire",
            "load_missing_and_garbage_file_errors",
        ],
    ),
    row(
        "mcp::tool::load",
        "MCP wire",
        &[
            "dump_then_load_roundtrips_through_the_wire",
            "load_missing_and_garbage_file_errors",
        ],
    ),
    row(
        "mcp::tool::list_collections",
        "MCP wire",
        &["list_collections_lists_user_names_exactly"],
    ),
    row(
        "mcp::tool::count",
        "MCP wire",
        &[
            "count_exact_with_filter_and_unknown_collection",
            "create_scalar_index_exact_under_mutation",
        ],
    ),
    row(
        "mcp::tool::insert_auto",
        "MCP wire",
        &[
            "insert_auto_keys_ordered_and_distinct",
            "dump_then_load_roundtrips_through_the_wire",
        ],
    ),
    row(
        "mcp::tool::set_schema",
        "MCP wire",
        &[
            "set_schema_then_get_schema_roundtrips",
            "set_schema_unique_enforced_on_stores",
            "set_schema_required_and_type_violations",
            "set_schema_param_and_name_errors",
            "dump_load_preserves_schema_constraints",
        ],
    ),
    row(
        "mcp::tool::get_schema",
        "MCP wire",
        &[
            "set_schema_then_get_schema_roundtrips",
            "set_schema_param_and_name_errors",
            "dump_load_preserves_schema_constraints",
        ],
    ),
];

// ===========================================================================
// Radar.
// ===========================================================================

#[test]
fn manifest_rust_rows_match_extracted_public_surface() {
    let manifest: BTreeSet<&str> = MANIFEST.iter().map(|r| r.item).collect();
    // Every row is either a Rust item of this crate or a wire-syntax row —
    // a row with any other prefix is a typo the other checks would miss.
    for item in &manifest {
        assert!(
            item.starts_with("corvid_mcp::")
                || item.starts_with("mcp::tool::")
                || item.starts_with("mcp::envelope::"),
            "manifest row {item} carries neither the Rust-item prefix \
             (corvid_mcp::) nor a wire prefix (mcp::tool:: / mcp::envelope::)"
        );
    }
    // Wire rows are validated by their own completeness checks below; the
    // source-extraction equality applies to the Rust rows only.
    let rust_rows: BTreeSet<&str> = manifest
        .iter()
        .copied()
        .filter(|i| i.starts_with("corvid_mcp::"))
        .collect();
    let extracted = extract_public_surface();

    // In src/ but not in the manifest: an unclassified public construct.
    let missing_from_manifest: Vec<&str> = extracted.difference(&rust_rows).copied().collect();
    // In the manifest but not in src/: a stale row for a removed/renamed item.
    let removed_from_source: Vec<&str> = rust_rows.difference(&extracted).copied().collect();

    assert!(
        missing_from_manifest.is_empty() && removed_from_source.is_empty(),
        "surface drift between src/ and MANIFEST:\n  \
         missing_from_manifest (public in src/, no MANIFEST row — add a row \
         or make the item non-public): {missing_from_manifest:?}\n  \
         removed_from_source (MANIFEST row with no public item in src/ — \
         update or delete the row): {removed_from_source:?}"
    );
}

/// Tools completeness: the `mcp::tool::*` rows must exactly match the tool
/// names the server advertises — queried at runtime through the public
/// in-process route (`handle_request` on a real `Server`), so the check reads
/// the same `tools/list` output a client sees. Adding or removing a tool
/// without touching the manifest fails in both directions. A second leg
/// asserts every advertised tool is actually dispatchable by
/// [`Server::handle`], so the advertisement and the dispatch table cannot
/// drift apart either.
#[test]
fn tool_rows_match_advertised_and_dispatchable_tools() {
    let server = Server::open_in_memory().expect("open in-memory server");
    let list = handle_request(
        &server,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .expect("tools/list must produce a response");
    let advertised: BTreeSet<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools/list result carries a tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool entry carries a name"))
        .collect();
    assert!(!advertised.is_empty(), "tools/list advertised no tools");

    let manifest_tools: BTreeSet<&str> = MANIFEST
        .iter()
        .map(|r| r.item)
        .filter_map(|i| i.strip_prefix("mcp::tool::"))
        .collect();

    let missing_from_manifest: Vec<&str> =
        advertised.difference(&manifest_tools).copied().collect();
    let removed_from_protocol: Vec<&str> =
        manifest_tools.difference(&advertised).copied().collect();
    assert!(
        missing_from_manifest.is_empty() && removed_from_protocol.is_empty(),
        "tool drift between tools/list and MANIFEST:\n  \
         missing_from_manifest (advertised by tools/list, no MANIFEST row — \
         add a row): {missing_from_manifest:?}\n  \
         removed_from_protocol (MANIFEST row for a tool tools/list no longer \
         advertises — update or delete the row): {removed_from_protocol:?}"
    );

    // Advertisement must be backed by dispatch: calling each advertised tool
    // (with empty arguments, so most fail with BadParams or an engine error —
    // that is fine) must never answer UnknownTool.
    for name in &advertised {
        if let Err(ToolError::UnknownTool(t)) = server.handle(name, &json!({})) {
            panic!(
                "tool `{t}` is advertised by tools/list but Server::handle does \
                 not dispatch it"
            );
        }
    }
}

/// Envelope completeness: the request-envelope rows must exactly match the
/// request methods `handle_request` dispatches on (its `Some("…")` match
/// arms). Notification methods (`notifications/…`) carry no id and produce no
/// response envelope, so they are excluded by naming convention. The
/// error-response envelope is produced by the error path rather than a match
/// arm, so its row is asserted to exist exactly once instead.
#[test]
fn envelope_rows_match_protocol_request_methods() {
    let arms = extract_method_arms();
    assert!(
        !arms.is_empty(),
        "no Some(\"…\") method arms found in protocol.rs"
    );

    let requests: BTreeSet<String> = arms
        .iter()
        .filter(|m| !m.starts_with("notifications/"))
        .cloned()
        .collect();
    let manifest_envelopes: BTreeSet<String> = MANIFEST
        .iter()
        .map(|r| r.item)
        .filter_map(|i| i.strip_prefix("mcp::envelope::"))
        .filter(|kind| *kind != "error_response")
        .map(str::to_owned)
        .collect();

    assert_eq!(
        requests, manifest_envelopes,
        "JSON-RPC envelope drift: the request methods handled in \
         protocol.rs ({requests:?}) and the manifest's request-envelope rows \
         ({manifest_envelopes:?}) must be the same set — every handled method \
         needs a row, every row a handled method"
    );

    let error_rows = MANIFEST
        .iter()
        .filter(|r| r.item == "mcp::envelope::error_response")
        .count();
    assert_eq!(
        error_rows, 1,
        "the manifest must carry exactly one mcp::envelope::error_response row"
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
// Wire extraction.
// ===========================================================================

/// The method names matched by `handle_request`'s `match method`: every
/// `Some("…")` arm, found by a raw-text scan of `protocol.rs`. The arm set is
/// small, fmt-stable, and the `Some("` byte sequence appears nowhere else in
/// the file (verified: it occurs only in the five match arms).
fn extract_method_arms() -> BTreeSet<String> {
    let src = fs::read_to_string(crate_dir().join("src").join("protocol.rs"))
        .expect("read src/protocol.rs");
    let mut out = BTreeSet::new();
    let mut rest = src.as_str();
    while let Some(pos) = rest.find("Some(\"") {
        let after = &rest[pos + "Some(\"".len()..];
        let Some(end) = after.find('"') else {
            break;
        };
        out.insert(after[..end].to_owned());
        rest = &after[end..];
    }
    out
}

// ===========================================================================
// Source extraction.
// ===========================================================================
//
// The tokenizer and item parser below are copied from the engine's radar
// (`crates/corvid/tests/surface/mod.rs`, conformance Task 1) and must stay
// behaviorally identical to it. Duplicating rather than sharing is a
// deliberate decision: integration tests cannot share code across crates
// without a dedicated tests-support crate, which the program has not (yet)
// warranted — see the Task 2 report. The only difference is the crate name
// used to canonicalize root paths.

/// The crate's own name, as item paths spell it.
const CRATE: &str = "corvid_mcp";

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
            format!("{CRATE}::{name}")
        } else {
            format!("{CRATE}::{module}::{name}")
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
/// `tests/` tree. Kept behaviorally identical to the engine radar's twin.
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
