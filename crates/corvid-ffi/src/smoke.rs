//! The C smoke-suite driver (Phase-0 Task 7): compile `c/smoke.c`, link
//! it against the just-built cdylib, run it over the committed golden
//! fixtures, and prove every fixture line executed. Test-only.
//!
//! # Mechanism (designed inside T2's cdylib-only constraint)
//!
//! The lib target is `crate-type = ["cdylib"]` only — no rlib, so
//! nothing can link the FFI "as a Rust dependency", and an integration
//! `tests/` target is structurally impossible (the T2 note in
//! `src/lib.rs`). The smoke suite therefore does not extend the Rust
//! test binary at all: at TEST-RUN time this driver
//!
//! 1. locates the cdylib artifact `cargo test` just built/refreshed
//!    (`target/<debug|release>/libcorvid.dylib` / `.so` / `corvid.dll`
//!    — the profile from `cfg!(debug_assertions)`, the directory from
//!    `CARGO_TARGET_DIR` or the workspace root; if `cargo test` did
//!    NOT build it — plain `cargo test` skips the artifact for a
//!    cdylib-only crate, see [`cdylib_artifact`]'s self-heal note —
//!    the test builds it itself with a nested `cargo build`);
//! 2. compiles `c/smoke.c` with the **`cc` crate** (dev-dependency; it
//!    abstracts gcc/clang/cl selection and flag dialects) against the
//!    committed `corvid.h`, linking the cdylib BY PATH into a
//!    scratch-directory executable — nothing is linked into the cdylib,
//!    so the shipped artifact stays exactly the 122-symbol surface
//!    (on MSVC the link names the emitted import library
//!    `corvid.dll.lib` when present, the shape binding authors use;
//!    at run time the child finds `corvid.dll` via `PATH`);
//! 3. runs the executable via [`std::process::Command`] with the golden
//!    directory's fixtures (sorted, so a new `golden/*.txt` joins the
//!    suite automatically) and a fresh scratch workdir for its file
//!    db/dump/backup paths, asserting exit 0;
//! 4. parses the `SMOKE <file> lines=N executed=N` protocol and — the
//!    golden-coverage discipline — independently counts each fixture's
//!    executable lines (non-blank, non-`#`) in Rust, asserting the
//!    smoke's own count equals it: an OP whose handler silently skipped,
//!    or a fixture line the harness never dispatched, fails the test.
//!
//! The same test IS the standalone CI entry point: `cargo test -p
//! corvid-ffi smoke_suite` (or a filter on the test name) compiles and
//! runs the identical binary outside the workspace suite.
//!
//! # ASan/LSan variant
//!
//! Leak-check expectations: **zero leaks**, by construction — every
//! handle family's free path (`corvid_value_free`, `corvid_pred_free`,
//! `corvid_query_free`, `corvid_rows_free`, `corvid_strs_free`,
//! `corvid_geohits_free`, `corvid_groupiter_free`,
//! `corvid_schemaiter_free`, `corvid_collection_free`, `corvid_close`)
//! plus `corvid_free`'s buffer domain (`insert_auto` keys, `page`'s
//! `next_after`) runs inside the fixtures, so a leak in ANY of them
//! fails the sanitizer run.
//!
//! The mechanism is env-var driven (CI wires it in Task 8):
//!
//! ```text
//! RUSTFLAGS="-Zsanitizer=address" CORVID_SMOKE_ASAN=1 \
//!   ASAN_OPTIONS=detect_leaks=1 \
//!   cargo +nightly test -p corvid-ffi smoke_suite --lib -- --nocapture
//! ```
//!
//! The Rust cdylib must be rebuilt with the matching sanitizer (nightly
//! `-Zsanitizer=address` — mixing an instrumented executable with an
//! uninstrumented dylib is unsupported); `CORVID_SMOKE_ASAN=1` then adds
//! `-fsanitize=address,undefined` to BOTH the compile and the link of
//! smoke.c and points the child's `ASAN_OPTIONS` at leak detection —
//! `detect_leaks=1` everywhere EXCEPT macOS arm64, where the runtime
//! does not support LSan (x86_64 macOS and Linux both leak-check).
//! `CORVID_SMOKE_CC`, `CORVID_SMOKE_CFLAGS`, and
//! `CORVID_SMOKE_LDFLAGS` exist for the CI matrix's compiler/flag
//! overrides; values are shell-split and passed through verbatim.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The crate root (`CARGO_MANIFEST_DIR` at compile time).
fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Split an env-var string shell-style (whitespace — sufficient for the
/// flags CI passes).
fn shell_split(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

fn asan_mode() -> bool {
    std::env::var_os("CORVID_SMOKE_ASAN").is_some()
}

/// The cdylib artifact this run links: profile from the test binary's
/// own `debug_assertions`, directory from `CARGO_TARGET_DIR` or the
/// workspace default (`<ws>/target`). Two layouts are searched — the
/// host layout (`<target>/<profile>`, what a plain `cargo build` /
/// `cargo test` produces) and the explicit-target layout
/// (`<target>/<triple>/<profile>`, what `cargo test --target T` and
/// the sanitizer CI job produce).
///
/// **Self-heal (Task 8 discovery):** plain `cargo test` does NOT build
/// the cdylib for this cdylib-only crate — nothing else in the test
/// graph depends on the artifact, so cargo skips the normal lib build
/// (T7's green runs had unknowingly linked a leftover artifact from
/// manual builds; a clean-target CI run failed on exactly this). If
/// the artifact is absent in both layouts, the test builds it itself
/// — cargo releases the build lock before running tests, so a nested
/// `cargo build` cannot deadlock — with `--target <host triple>` and
/// the ambient `RUSTFLAGS`/`CARGO_TARGET_DIR` passed through, which
/// keeps the sanitizer job's instrumented flags on the rebuilt dylib
/// and the proc-macro/host split intact.
fn cdylib_artifact() -> PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // <crate>/../.. is the workspace root; its target/ is the
            // default shared directory.
            let default = manifest_dir().join("..").join("..").join("target");
            default.canonicalize().unwrap_or(default)
        });
    let layouts = [base.join(profile), base.join(host_triple()).join(profile)];
    let find = |layouts: &[PathBuf]| -> Option<PathBuf> {
        layouts.iter().find_map(|dir| {
            ["libcorvid.dylib", "libcorvid.so", "corvid.dll"]
                .iter()
                .map(|name| dir.join(name))
                .find(|p| p.exists())
        })
    };
    if let Some(found) = find(&layouts) {
        return found;
    }
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args([
        "build",
        "-p",
        env!("CARGO_PKG_NAME"),
        "--target",
        &host_triple(),
    ]);
    if !cfg!(debug_assertions) {
        cmd.arg("--release");
    }
    let out = cmd
        .current_dir(manifest_dir())
        .output()
        .unwrap_or_else(|e| panic!("run a nested `cargo build` for the cdylib: {e}"));
    assert!(
        out.status.success(),
        "building the cdylib artifact failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    find(&layouts).unwrap_or_else(|| {
        panic!(
            "the cdylib artifact is still missing under {} (or its \
             --target twin) after a nested `cargo build -p corvid-ffi` — \
             did CARGO_TARGET_DIR change mid-run?",
            base.display()
        )
    })
}

/// Count a fixture's executable lines: non-blank, not starting with '#'
/// (the same rule `smoke.c`'s driver applies — if the two ever disagree,
/// the count assertion below fails loudly rather than passing on a
/// hidden convention change).
fn executable_lines(path: &Path) -> usize {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
}

/// The host triple this test binary runs on (cc wants a TARGET/HOST
/// triple but the test process has no cargo build-script env; the smoke
/// always compiles AND runs natively, so host == target and this
/// cfg-derived triple is exactly the machine cargo just built for).
fn host_triple() -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "x86") {
        "i686"
    } else {
        panic!("unsupported smoke-suite host arch")
    };
    if cfg!(target_os = "macos") {
        format!("{arch}-apple-darwin")
    } else if cfg!(target_os = "linux") {
        format!("{arch}-unknown-linux-gnu")
    } else if cfg!(target_os = "windows") {
        let env = if cfg!(target_env = "msvc") {
            "msvc"
        } else {
            "gnu"
        };
        format!("{arch}-pc-windows-{env}")
    } else {
        panic!("unsupported smoke-suite host OS")
    }
}

/// What the LINK line names: on MSVC, the cdylib build also emits an
/// import library next to `corvid.dll` (`corvid.dll.lib`) and linking
/// against THAT is the documented path (link.exe reads a bare `.dll`'s
/// exports in simple cases, but the import lib is the contract the
/// release artifacts ship); elsewhere the cdylib file itself. The
/// import-lib preference is Task 8's handling of the T7-noted
/// Windows/MSVC risk — verified by the windows CI smoke leg.
fn link_target(dylib: &Path) -> PathBuf {
    if cfg!(target_os = "windows")
        && let Some(imp) = dylib
            .parent()
            .map(|d| d.join("corvid.dll.lib"))
            .filter(|p| p.exists())
    {
        return imp;
    }
    dylib.to_path_buf()
}

/// Compile + link the smoke executable into `workdir`; returns its path.
fn build_smoke(workdir: &Path) -> PathBuf {
    let dylib = cdylib_artifact();
    let triple = host_triple();

    let mut build = cc::Build::new();
    build
        .file(manifest_dir().join("c").join("smoke.c"))
        .include(manifest_dir()) // corvid.h lives at the crate root
        .opt_level(0)
        .debug(true)
        // cc assumes a build-script environment (TARGET/HOST env vars);
        // at test runtime supply the same knowledge explicitly — the
        // native triple, matching the cdylib cargo just built.
        .target(&triple)
        .host(&triple);
    if let Some(cc_env) = std::env::var_os("CORVID_SMOKE_CC") {
        build.compiler(cc_env);
    }
    for flag in shell_split(&std::env::var("CORVID_SMOKE_CFLAGS").unwrap_or_default()) {
        build.flag(&flag);
    }
    if asan_mode() {
        build.flag("-fsanitize=address,undefined");
    }
    let compiler = build.get_compiler();

    let smoke_c = manifest_dir().join("c").join("smoke.c");
    let exe = workdir.join(if cfg!(target_os = "windows") {
        "corvid_smoke.exe"
    } else {
        "corvid_smoke"
    });
    // `to_command` (not `Command::new(compiler.path())`): on MSVC the
    // tool carries the detected INCLUDE/LIB environment of the
    // installed SDK — without it cl cannot find even <math.h>
    // (the Windows CI leg caught exactly that).
    let mut cmd = compiler.to_command();
    cmd.arg(&smoke_c)
        .arg(link_target(&dylib))
        .arg("-o")
        .arg(&exe)
        .arg("-I")
        .arg(manifest_dir());
    for arg in compiler.args() {
        cmd.arg(arg);
    }
    // macOS records the dylib's install name (@rpath/...); bake the
    // artifact's directory so the child resolves it without env help.
    if cfg!(target_os = "macos")
        && let Some(dir) = dylib.parent()
    {
        cmd.arg(format!("-Wl,-rpath,{}", dir.display()));
    }
    for flag in shell_split(&std::env::var("CORVID_SMOKE_LDFLAGS").unwrap_or_default()) {
        cmd.arg(&flag);
    }
    if asan_mode() {
        cmd.arg("-fsanitize=address,undefined");
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("run the C compiler {:?}: {e}", compiler.path()));
    assert!(
        out.status.success(),
        "compiling c/smoke.c failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    exe
}

/// The sorted golden fixture list (a new `golden/*.txt` joins
/// automatically).
fn fixtures() -> Vec<PathBuf> {
    let golden = manifest_dir().join("golden");
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&golden)
        .unwrap_or_else(|e| panic!("read {}: {e}", golden.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    fixtures.sort();
    assert!(
        !fixtures.is_empty(),
        "the golden directory {} holds no fixtures — the smoke suite \
         cannot run vacuously",
        golden.display()
    );
    fixtures
}

/// Compile, run, and verify the suite (shared by the plain and the ASan
/// entry points below).
fn run_smoke_suite() {
    let fixtures = fixtures();
    let workdir = tempfile::tempdir().expect("a scratch workdir");
    let exe = build_smoke(workdir.path());

    let dylib_dir = cdylib_artifact()
        .parent()
        .map(Path::to_path_buf)
        .expect("the artifact always has a parent");
    let mut cmd = Command::new(&exe);
    cmd.arg(workdir.path());
    for f in &fixtures {
        cmd.arg(f);
    }
    if asan_mode() {
        let opts = std::env::var("ASAN_OPTIONS").unwrap_or_else(|_| {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                // LSan is unsupported on macOS arm64 (the runtime aborts
                // if detect_leaks is forced there); x86_64 macOS and
                // Linux both support it and get the leak gate. The arm64
                // leak half runs in the Linux CI job (Task 8).
                "halt_on_error=1".to_owned()
            } else {
                "detect_leaks=1:halt_on_error=1".to_owned()
            }
        });
        cmd.env("ASAN_OPTIONS", opts);
    }
    // Help the loader find the cdylib when the platform needs it (the
    // baked rpath covers the common case; this is belt-and-braces for
    // CI runners with stripped rpaths). Windows resolves corvid.dll at
    // load time through PATH (case-insensitive), so the artifact's
    // directory joins it there too.
    let var = if cfg!(target_os = "macos") {
        Some("DYLD_LIBRARY_PATH")
    } else if cfg!(target_os = "linux") {
        Some("LD_LIBRARY_PATH")
    } else if cfg!(target_os = "windows") {
        Some("PATH")
    } else {
        None
    };
    if let Some(var) = var {
        let sep = if cfg!(target_os = "windows") {
            ";"
        } else {
            ":"
        };
        let prev = std::env::var(var).unwrap_or_default();
        let value = if prev.is_empty() {
            dylib_dir.display().to_string()
        } else {
            format!("{}{}{}", dylib_dir.display(), sep, prev)
        };
        cmd.env(var, value);
    }

    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("run the smoke binary {}: {e}", exe.display()));
    assert!(
        out.status.success(),
        "the C smoke suite failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Coverage: each fixture reports its own dispatch count; the Rust
    // side recounts independently — the two must agree.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut reported: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("SMOKE ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let Some(file) = parts.next() else { continue };
        let parse = |kv: Option<&str>, k: &str| -> Option<i64> {
            kv.and_then(|kv| {
                kv.split_once('=')
                    .filter(|(key, _)| *key == k)
                    .and_then(|(_, v)| v.parse().ok())
            })
        };
        let lines = parse(parts.next(), "lines").unwrap_or(-1);
        let executed = parse(parts.next(), "executed").unwrap_or(-1);
        reported.insert(file.to_owned(), (lines, executed));
    }
    for f in &fixtures {
        let key = f.to_string_lossy().to_string();
        let &(lines, executed) = reported
            .get(&key)
            .unwrap_or_else(|| panic!("the smoke run never reported {key}; stdout was:\n{stdout}"));
        let rust_count = executable_lines(f) as i64;
        assert_eq!(
            lines, rust_count,
            "{key}: smoke counted {lines} executable lines, Rust counts \
             {rust_count} — the two line rules disagree (a grammar change \
             on one side only)"
        );
        assert_eq!(
            executed, rust_count,
            "{key}: only {executed}/{rust_count} fixture lines executed — \
             the golden suite must run every line (see stderr above)"
        );
    }
    assert_eq!(
        reported.len(),
        fixtures.len(),
        "the smoke run reported {} files for {} fixtures",
        reported.len(),
        fixtures.len()
    );
}

/// The suite entry point: compile `c/smoke.c` against the cdylib, run it
/// over every committed fixture, assert success AND that every
/// executable fixture line was dispatched (the golden-coverage
/// discipline paired with the radar's symbol coverage in
/// [`crate::radar`]). Also the standalone CI command: `cargo test -p
/// corvid-ffi smoke_suite`.
#[test]
fn smoke_suite_runs_and_covers_every_fixture_line() {
    run_smoke_suite();
}

/// The ASan/LSan entry point (module docs): the identical suite under
/// `-fsanitize=address,undefined` with leak detection on. Runs ONLY in
/// the sanitizer CI job — the plain suite must not silently stand in
/// for it. The assertion order matters: in ASan mode this test RUNS the
/// suite (failures surface as test failures); outside it, the test
/// asserts the mode is off and passes trivially, so `cargo test
/// --workspace` stays green without the toolchain.
#[test]
fn smoke_asan_leak_check_runs_only_in_sanitizer_mode() {
    if asan_mode() {
        // A mis-wired job that set CORVID_SMOKE_ASAN but fights its own
        // link flags must not silently run unsanitized.
        assert!(
            std::env::var("CORVID_SMOKE_LDFLAGS")
                .map(|f| !f.contains("sanitize"))
                .unwrap_or(true),
            "CORVID_SMOKE_ASAN adds its own -fsanitize link flag; \
             CORVID_SMOKE_LDFLAGS must not fight it"
        );
        run_smoke_suite();
    }
    // Outside ASan mode this test is a no-op: the plain entry point
    // above already ran the same suite unsanitized.
}
