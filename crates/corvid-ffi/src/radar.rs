//! The C-surface radar (Phase-0 Task 7): the no-untested-export rule.
//! Test-only.
//!
//! Three set assertions, one source of truth:
//!
//! 1. **spec ↔ header** — the exported-symbol set of the GENERATED
//!    `corvid.h` equals `docs/FFI.md` Appendix A, parsed from the spec
//!    FILE at test time (never a transcribed `const` — the Task 6 review
//!    prepend closed that last drift link: a hand-copied list could rot
//!    against the very document it claims to pin). Combined with the
//!    byte drift gate (`crate::header`), the chain crate → header →
//!    spec has no silent-drift link left.
//! 2. **spec ↔ smoke (coverage)** — every Appendix A symbol is CALLED by
//!    the C smoke suite (`c/smoke.c`): no untested exports, 122/122.
//! 3. **smoke ↔ spec (phantom calls)** — every `corvid_*` call in
//!    `smoke.c` names a real Appendix A symbol (a typo'd call would fail
//!    at link time anyway; this fails at test time with a symbol name).
//!
//! # How "smoke.c calls X" is proven, and the method's limits
//!
//! The detector strips `/* */` and `//` comments (a commented-out call
//! must not count as coverage), then looks for the token `corvid_<name>`
//! immediately followed by `(` — the call shape. Known limits, accepted
//! and excluded by convention (documented at the top of `smoke.c`):
//!
//! - **Macro indirection would evade it** — a `#define CALL corvid_x`
//!   wrapper or an `#include`-generated call is invisible to the token
//!   scan. `smoke.c` therefore FORBIDS object-like/function-like macros
//!   that mention `corvid_` and `#include` of anything but system +
//!   `corvid.h`; the radar rejects any `#`-line containing `corvid_`
//!   (a cheap structural proxy for both evasions).
//! - **A call site is not a behavioral oracle** — it proves the symbol
//!   is linked and driven, not that every branch is exercised. The
//!   behavioral half is the golden suite (`crate::smoke`): the radar's
//!   job is exactly the "no untested EXPORT" bound, per the plan.
//!
//! The coverage counting is deliberately dumb and total: a symbol
//! appears once in the appendix, once in the header, at least once in
//! the smoke source. Nothing about call ORDER is implied.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The Appendix A heading, the anchor the spec-file parser looks for.
const APPENDIX_HEADING: &str = "## Appendix A";

/// Parse the spec itself (`docs/FFI.md`) for Appendix A's symbol list at
/// test time. The appendix is the fenced ``` block after the heading;
/// every line that is exactly a `corvid_*` name is a symbol. Fails
/// loudly (with the parse reason) when the spec's shape changes — a
/// renamed heading or removed fence must not silently empty the set
/// (an empty set would vacuously pass every downstream equality).
fn appendix_symbols() -> Vec<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let spec_path = manifest.join("..").join("..").join("docs").join("FFI.md");
    let spec = std::fs::read_to_string(&spec_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", spec_path.display()));

    let anchor = spec.find(APPENDIX_HEADING).unwrap_or_else(|| {
        panic!(
            "{}: the Appendix A heading {:?} is gone — the radar cannot \
             find the exported-symbol list; update the parser with the \
             spec",
            spec_path.display(),
            APPENDIX_HEADING
        )
    });
    let after = &spec[anchor..];
    let open = after.find("```").unwrap_or_else(|| {
        panic!(
            "{}: Appendix A has no fenced symbol block after the heading",
            spec_path.display()
        )
    });
    let body_start = open + 3;
    let close = after[body_start..].find("```").unwrap_or_else(|| {
        panic!(
            "{}: Appendix A's fenced symbol block is unterminated",
            spec_path.display()
        )
    });
    let body = &after[body_start..body_start + close];

    let names: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("corvid_"))
        .map(str::to_owned)
        .collect();
    assert!(
        !names.is_empty(),
        "{}: Appendix A's block parsed to ZERO symbols — refuse the \
         vacuous pass; check the block's contents",
        spec_path.display()
    );
    names
}

/// Strip `/* ... */` (multiline) and `// ...` comments from C source
/// text (newlines preserved so line-oriented diagnostics keep working).
fn strip_c_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/").map(|p| i + 2 + p + 2) {
                Some(end) => {
                    out.push_str(&"\n".repeat(src[i..end].matches('\n').count()));
                    i = end;
                }
                None => break, // unterminated: nothing after it anyway
            }
        } else if bytes[i..].starts_with(b"//") {
            match src[i..].find('\n').map(|p| i + p) {
                Some(nl) => {
                    out.push('\n');
                    i = nl;
                }
                None => break,
            }
        } else {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Extract every `corvid_*` FUNCTION name from the stripped header text:
/// in this header's grammar only a FUNCTION name is followed by `(`
/// (typedef, handle, and enum names are followed by a space, `;`, or
/// `)`), so that is the declaration detector. Deduplicated, in order.
fn header_function_names(stripped: &str) -> Vec<String> {
    symbol_names_followed_by_paren(stripped)
}

/// The shared token scan: every `corvid_<name>` whose next byte is `(`,
/// in order of first appearance (a name called twice counts once — the
/// radar is a set gate).
fn symbol_names_followed_by_paren(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = text[i..].find("corvid_") {
        let start = i + pos;
        let mut end = start + "corvid_".len();
        while end < bytes.len()
            && (bytes[end].is_ascii_lowercase()
                || bytes[end].is_ascii_digit()
                || bytes[end] == b'_')
        {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'(' {
            let name = text[start..end].to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        i = end;
    }
    names
}

/// Radar gate 1: the generated header exposes EXACTLY Appendix A — same
/// set both directions, same count. (The byte drift gate in
/// [`crate::header`] already binds header to crate; this binds header to
/// SPEC, completing the triangle crate = header = appendix.)
#[test]
fn header_exports_exactly_the_spec_appendix() {
    let appendix: BTreeSet<String> = appendix_symbols().into_iter().collect();
    let rendered = crate::header::generate_header();
    let header_names = header_function_names(&strip_c_comments(&rendered));
    let header_set: BTreeSet<String> = header_names.into_iter().collect();

    assert_eq!(
        appendix.len(),
        122,
        "Appendix A is the locked 122-symbol contract (spec §4); parsed {}",
        appendix.len()
    );
    let missing: Vec<String> = appendix.difference(&header_set).cloned().collect();
    assert!(
        missing.is_empty(),
        "Appendix A symbols absent from the generated corvid.h: {missing:?}"
    );
    let extra: Vec<String> = header_set.difference(&appendix).cloned().collect();
    assert!(
        extra.is_empty(),
        "corvid.h exports symbols outside Appendix A: {extra:?}"
    );
    assert_eq!(
        appendix.len(),
        header_set.len(),
        "set sizes diverged despite set equality (duplicate appendix lines?)"
    );
}

/// Radar gate 2 + 3: the C smoke suite drives EVERY Appendix A symbol,
/// and every `corvid_*` call it makes names an Appendix A symbol.
#[test]
fn smoke_suite_drives_every_appendix_symbol_and_no_phantoms() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let smoke_path = manifest.join("c").join("smoke.c");
    let smoke = std::fs::read_to_string(&smoke_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", smoke_path.display()));

    // The anti-evasion convention: smoke.c must not route corvid calls
    // through macros or includes (module docs) — reject any preprocessor
    // line that mentions a symbol.
    for (n, line) in smoke.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#')
            && trimmed.contains("corvid_")
            && !trimmed.starts_with("#include <")
            && !trimmed.contains("\"corvid.h\"")
        {
            panic!(
                "{}:{}: preprocessor line mentions corvid_ — the radar's \
                 call scan cannot see through macro indirection; call the \
                 ABI directly (see smoke.c's conventions block)",
                smoke_path.display(),
                n + 1
            );
        }
    }

    let appendix: BTreeSet<String> = appendix_symbols().into_iter().collect();
    let called: BTreeSet<String> = symbol_names_followed_by_paren(&strip_c_comments(&smoke))
        .into_iter()
        .collect();

    let untested: Vec<String> = appendix.difference(&called).cloned().collect();
    assert!(
        untested.is_empty(),
        "Appendix A symbols never called by c/smoke.c (no untested \
         exports): {untested:?}"
    );
    let phantoms: Vec<String> = called.difference(&appendix).cloned().collect();
    assert!(
        phantoms.is_empty(),
        "c/smoke.c calls corvid_* symbols outside Appendix A (typos or \
         private functions — the ABI has neither): {phantoms:?}"
    );
    assert_eq!(
        called.len(),
        appendix.len(),
        "smoke call set is not exactly the appendix ({}/{}); see the \
         sets above",
        called.len(),
        appendix.len()
    );
}
