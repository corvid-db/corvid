//! The header drift gate (spec §8: "the generated `corvid.h` is
//! committed and drift-gated"). Test-only.
//!
//! [`header_h_stays_generated_from_the_crate`] re-renders `corvid.h` from
//! the crate sources with cbindgen (exact-pinned in `Cargo.toml`, so the
//! output is byte-stable) and diffs it against the committed copy — the
//! SYNTAX.md pattern, so the crate, the committed header, and the spec
//! cannot disagree silently. With `CORVID_GEN_HEADER=1` in the
//! environment it (re)writes the file instead — the command is in the
//! header's own comment.

use std::path::PathBuf;

/// Render the canonical header text from this crate's sources.
fn generate_header() -> String {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = cbindgen::Config::from_file(manifest.join("cbindgen.toml")).expect(
        "crates/corvid-ffi/cbindgen.toml parses (check key names against cbindgen::Config)",
    );
    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&manifest)
        .generate()
        .expect("cbindgen parses the corvid-ffi sources");
    let mut out = Vec::<u8>::new();
    bindings.write(&mut out);
    String::from_utf8(out).expect("cbindgen emits UTF-8 for this crate's docs")
}

/// docs/FFI.md §8's drift gate, in the SYNTAX.md pattern: regenerate and
/// byte-diff on every run; `CORVID_GEN_HEADER=1` rewrites the committed
/// copy (the failure message and the header comment both carry the
/// command).
#[test]
fn header_h_stays_generated_from_the_crate() {
    let rendered = generate_header();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corvid.h");
    if std::env::var_os("CORVID_GEN_HEADER").is_some() {
        std::fs::write(&path, &rendered)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e} — generate it with CORVID_GEN_HEADER=1 cargo \
             test -p corvid-ffi header_h_stays_generated",
            path.display()
        )
    });
    assert_eq!(
        committed, rendered,
        "crates/corvid-ffi/corvid.h is out of sync with the crate sources \
         — regenerate with CORVID_GEN_HEADER=1 cargo test -p corvid-ffi \
         header_h_stays_generated"
    );
}

/// FFI.md Appendix A's exported-symbol set, verbatim (122 names, the
/// count per family 8 + 3 + 11 + 12 + 11 + 15 + 11 + 13 + 4 + 15 + 7 +
/// 7 + 5). Order follows the appendix.
const SPEC_APPENDIX: [&str; 122] = [
    "corvid_ffi_version",
    "corvid_open",
    "corvid_open_memory",
    "corvid_close",
    "corvid_last_error_code",
    "corvid_last_error_message",
    "corvid_free",
    "corvid_collections",
    "corvid_collection",
    "corvid_collection_free",
    "corvid_collection_name",
    "corvid_value_null",
    "corvid_value_bool",
    "corvid_value_int",
    "corvid_value_float",
    "corvid_value_text",
    "corvid_value_bytes",
    "corvid_value_vector",
    "corvid_value_array_new",
    "corvid_value_array_push",
    "corvid_value_map_new",
    "corvid_value_map_put",
    "corvid_value_type",
    "corvid_value_as_bool",
    "corvid_value_as_int",
    "corvid_value_as_float",
    "corvid_value_text_ref",
    "corvid_value_bytes_ref",
    "corvid_value_vector_ref",
    "corvid_value_array_get",
    "corvid_value_map_get",
    "corvid_value_len",
    "corvid_value_clone",
    "corvid_value_free",
    "corvid_pred_exists",
    "corvid_pred_compare",
    "corvid_pred_in",
    "corvid_pred_between",
    "corvid_pred_starts_with",
    "corvid_pred_contains",
    "corvid_pred_geo_within",
    "corvid_pred_and",
    "corvid_pred_or",
    "corvid_pred_not",
    "corvid_pred_free",
    "corvid_query_new",
    "corvid_query_filter",
    "corvid_query_vector",
    "corvid_query_text",
    "corvid_query_fuse_rrf",
    "corvid_query_rerank_mmr",
    "corvid_query_approx",
    "corvid_query_limit",
    "corvid_query_offset",
    "corvid_query_order_by",
    "corvid_query_select",
    "corvid_query_run",
    "corvid_query_free",
    "corvid_rows_next",
    "corvid_rows_free",
    "corvid_query_count",
    "corvid_query_count_distinct",
    "corvid_query_sum",
    "corvid_query_avg",
    "corvid_query_min",
    "corvid_query_max",
    "corvid_query_group_count",
    "corvid_query_group_sum",
    "corvid_query_group_avg",
    "corvid_groupiter_next",
    "corvid_groupiter_free",
    "corvid_insert",
    "corvid_put_many",
    "corvid_insert_auto",
    "corvid_update",
    "corvid_patch",
    "corvid_compare_and_set",
    "corvid_delete",
    "corvid_delete_where",
    "corvid_delete_batch",
    "corvid_insert_with_ttl",
    "corvid_set_ttl",
    "corvid_get_ttl",
    "corvid_purge_expired",
    "corvid_get",
    "corvid_scan",
    "corvid_page",
    "corvid_len",
    "corvid_create_scalar_index",
    "corvid_create_compound_index",
    "corvid_create_text_index",
    "corvid_create_text_index_ondisk",
    "corvid_create_geo_index",
    "corvid_create_vector_index",
    "corvid_create_vector_index_quantized",
    "corvid_create_vector_index_ondisk",
    "corvid_create_vector_index_ondisk_quantized",
    "corvid_create_vector_index_pq",
    "corvid_create_vector_index_ondisk_pq",
    "corvid_set_schema",
    "corvid_schema",
    "corvid_schemaiter_next",
    "corvid_schemaiter_free",
    "corvid_link",
    "corvid_link_weighted",
    "corvid_unlink",
    "corvid_neighbors",
    "corvid_in_neighbors",
    "corvid_neighbors_weighted",
    "corvid_traverse",
    "corvid_geo_within_radius",
    "corvid_geo_within_bbox",
    "corvid_geo_nearest",
    "corvid_geohits_next",
    "corvid_geohits_free",
    "corvid_strs_next",
    "corvid_strs_free",
    "corvid_dump_to_path",
    "corvid_load_from_path",
    "corvid_load_from_path_with_renames",
    "corvid_backup",
    "corvid_compact",
];

/// The exported-symbol gate (spec §2/Appendix A, reached in full with
/// Task 6): the header exposes EXACTLY these 122 `corvid_*` functions —
/// the same set the crate's `#[no_mangle]` surface produces (the
/// byte-drift test above holds header and crate together). Comments are
/// stripped first so doc mentions do not count; in this header's
/// grammar only a FUNCTION name is followed by `(` (typedef, handle,
/// and enum names are followed by a space, `;`, or `)`), so that is the
/// declaration detector.
#[test]
fn exported_symbols_match_the_spec_appendix() {
    let rendered = generate_header();
    // Strip /* ... */ comments (the doc blocks) across lines.
    let mut stripped = String::with_capacity(rendered.len());
    let bytes = rendered.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            match rendered[i + 2..].find("*/").map(|p| i + 2 + p + 2) {
                Some(end) => {
                    i = end;
                    stripped.push('\n');
                }
                None => break, // unterminated: nothing after it anyway
            }
        } else {
            let ch = rendered[i..].chars().next().unwrap();
            stripped.push(ch);
            i += ch.len_utf8();
        }
    }

    // Every `corvid_<name>` token whose NEXT BYTE is '(' — deduplicated,
    // in header order. cbindgen never spaces a function name from its
    // parameter list, while a TYPE before a parenthesized declarator
    // always is (`typedef corvid_status (*corvid_update_fn)(...)`), which
    // is exactly the false positive the immediate-paren rule excludes.
    let stripped_bytes = stripped.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = stripped[i..].find("corvid_") {
        let start = i + pos;
        let mut end = start + "corvid_".len();
        while end < stripped_bytes.len()
            && (stripped_bytes[end].is_ascii_lowercase()
                || stripped_bytes[end].is_ascii_digit()
                || stripped_bytes[end] == b'_')
        {
            end += 1;
        }
        if end < stripped_bytes.len() && stripped_bytes[end] == b'(' {
            let name = stripped[start..end].to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        i = end;
    }

    assert_eq!(
        names.len(),
        SPEC_APPENDIX.len(),
        "the header must expose exactly the spec's {} symbols, found {}: {:?}",
        SPEC_APPENDIX.len(),
        names.len(),
        names
    );
    for want in SPEC_APPENDIX {
        assert!(
            names.contains(&want.to_owned()),
            "spec appendix symbol {want} missing from corvid.h"
        );
    }
}
