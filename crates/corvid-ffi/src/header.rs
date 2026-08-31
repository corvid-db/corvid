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

use std::path::PathBuf;

/// Render the canonical header text from this crate's sources. Shared
/// with the radar (the set gate reads the SAME render the drift gate
/// byte-compares, so the two can never disagree about what the header
/// says).
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
