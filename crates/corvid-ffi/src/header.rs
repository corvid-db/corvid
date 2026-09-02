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
//!
//! The exported-SET half of the gate (header ↔ spec Appendix A) lives in
//! [`crate::radar`] — the C-surface radar, whose appendix source is
//! `docs/FFI.md` parsed at test time, never a transcribed const (the
//! Task 6 review prepend: that was the last drift link).
//!
//! # The portable-enum post-pass
//!
//! cbindgen keys its C emission off the Rust `#[repr(prim)]`: every
//! fieldless enum marked `#[repr(u32)]` (the §1.3/§1.4 frozen wire type
//! — kept, it is what pins the Rust-side discriminant size) is rendered
//! with a C23 fixed-underlying-type guard (`enum <tag>` + `#if
//! __STDC_VERSION__ >= 202311L` + `  : uint32_t` + `#endif`, plus a
//! guarded `typedef uint32_t <tag>;` fallback for pre-C23 C). cbindgen
//! 0.29 has NO configuration that suppresses this — `cpp_compat` only
//! re-shapes the guards. That shape is C-compiler-portable (the 3-OS
//! smoke legs compile it) but ILL-FORMED C++: `__STDC_VERSION__` is
//! undefined in C++ mode, so the fallback branch fires and `typedef
//! uint32_t corvid_status;` redeclares the enum tag — the same
//! namespace in C++ — as a different kind of entity (found by
//! corvid-cpp, which had to preprocessor-mask the guards in its ABI
//! TUs). [`portable_enum_form`] folds every guarded enum into the
//! plain `typedef enum <tag> { ... } <tag>;` that FFI.md §1.3/§1.4
//! have shown all along — simultaneously valid C (C11 and later,
//! including C23) and valid C++ (every standard). Nothing is lost: the
//! frozen VALUES are the contract and they are explicit; a plain C
//! enum whose enumerators are all in `0..=19` is `int`-sized (32-bit)
//! on every compiler the smoke matrix covers (gcc, clang, MSVC — the
//! legs compile the header as C on all three), matching the Rust
//! `#[repr(u32)]` wire type the same way the C23 `: uint32_t` spelling
//! did.

use std::path::PathBuf;

/// Render the canonical header text from this crate's sources. Shared
/// with the radar (the set gate reads the SAME render the drift gate
/// byte-compares, so the two can never disagree about what the header
/// says) — cbindgen's raw render run through [`portable_enum_form`].
pub(crate) fn generate_header() -> String {
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
    let rendered = String::from_utf8(out).expect("cbindgen emits UTF-8 for this crate's docs");
    portable_enum_form(rendered)
}

/// The head of cbindgen's per-enum trailing guard block — the text
/// between the enum's closing `};` and the `typedef enum <tag> <tag>;`
/// line inside the block. Unique enough to collect every guarded tag.
const TAIL_GUARD_HEAD: &str = "};\n#if __STDC_VERSION__ >= 202311L\ntypedef enum ";

/// Fold cbindgen's C23 fixed-underlying-type guards into the plain
/// `typedef enum <tag> { ... } <tag>;` (module docs: valid C11 AND C++).
/// Deterministic and total — it fails loudly (panics, so the drift test
/// that calls it fails) rather than passing a half-transformed header
/// through: a future cbindgen bump that changes the guarded shape must
/// re-audit this pass, not silently emit C23 again.
fn portable_enum_form(rendered: String) -> String {
    // Collect the guarded tags first (before any replacement can blur
    // the patterns): each trailing block reads
    //   };\n#if __STDC_VERSION__ >= 202311L\ntypedef enum <tag> <tag>;\n
    //   #else\ntypedef uint32_t <tag>;\n#endif // __STDC_VERSION__ >= 202311L
    let mut tags: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(rel) = rendered[from..].find(TAIL_GUARD_HEAD) {
        let tag_start = from + rel + TAIL_GUARD_HEAD.len();
        let tag_end = rendered[tag_start..]
            .find(' ')
            .unwrap_or_else(|| panic!("header post-pass: cbindgen guard tail at byte {tag_start} is not `typedef enum <tag> <tag>;`"));
        tags.push(rendered[tag_start..tag_start + tag_end].to_owned());
        from = tag_start + tag_end;
    }
    assert!(
        !tags.is_empty(),
        "header post-pass: found ZERO C23-guarded enums — cbindgen's guarded \
         emission is gone (bump?), so the plain-form fold is dead code; \
         remove or re-audit portable_enum_form"
    );

    let mut out = rendered;
    for tag in &tags {
        // The opening: `enum <tag>\n#if <guard>\n  : uint32_t\n#endif // <guard>\n {`
        // (the `uint32_t` is repr(u32) rendered — every frozen enum here is
        // u32; a non-u32 repr would miss this pattern and trip the final
        // no-guards-left assert below, which is the desired loud stop).
        let guarded_open = format!(
            "enum {tag}\n#if __STDC_VERSION__ >= 202311L\n  : uint32_t\n\
             #endif // __STDC_VERSION__ >= 202311L\n {{"
        );
        let plain_open = format!("typedef enum {tag} {{");
        assert_eq!(
            out.matches(&guarded_open).count(),
            1,
            "header post-pass: expected exactly one guarded opening for {tag}"
        );
        out = out.replace(&guarded_open, &plain_open);

        // The tail: the whole `#if/#else/#endif` typedef block collapses
        // into the tag-typedef close of the (now plain) enum above it.
        let guarded_tail = format!(
            "}};\n#if __STDC_VERSION__ >= 202311L\ntypedef enum {tag} {tag};\n\
             #else\ntypedef uint32_t {tag};\n#endif // __STDC_VERSION__ >= 202311L"
        );
        let plain_tail = format!("}} {tag};");
        assert_eq!(
            out.matches(&guarded_tail).count(),
            1,
            "header post-pass: expected exactly one guarded tail for {tag}"
        );
        out = out.replace(&guarded_tail, &plain_tail);
    }
    assert!(
        !out.contains("__STDC_VERSION__"),
        "header post-pass: a C23 guard survived the fold — the emitted \
         header would be ill-formed C++ again; audit portable_enum_form"
    );
    out
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
    if committed != rendered {
        // Diagnose before panicking: lengths + the first differing byte
        // window, so a Windows-checkout line-ending rewrite (the Task 8
        // CI round) is identifiable from the log alone instead of a
        // 90 KB undiffable assert dump.
        let (cl, rl) = (committed.len(), rendered.len());
        let idx = committed
            .bytes()
            .zip(rendered.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(cl.min(rl));
        let window = |s: &str| -> String {
            s[idx.saturating_sub(30)..(idx + 30).min(s.len())]
                .chars()
                .map(|c| {
                    if c.is_control() {
                        format!("\\x{:02x}", c as u32)
                    } else {
                        c.to_string()
                    }
                })
                .collect()
        };
        panic!(
            "crates/corvid-ffi/corvid.h is out of sync with the crate sources \
             — regenerate with CORVID_GEN_HEADER=1 cargo test -p corvid-ffi \
             header_h_stays_generated. Lengths {cl} vs {rl}; first difference \
             at byte {idx}: committed {:?} vs rendered {:?}",
            window(&committed),
            window(&rendered)
        );
    }
}
