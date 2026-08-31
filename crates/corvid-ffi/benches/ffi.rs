//! FFI crossing-cost bench (Phase-0 Task 8): the same four shapes done
//! through the C ABI (in a freshly compiled C consumer) and natively
//! in Rust, on identical deterministic corpora — "zero parsing,
//! bounded crossing cost" as a measurement instead of a claim.
//!
//! ```text
//! cargo build -p corvid-ffi --release    # the cdylib the child links
//! cargo bench -p corvid-ffi --bench ffi  # the full comparison
//! ```
//!
//! The build command is load-bearing: `cargo bench` does not produce
//! the cdylib artifact (nothing in the bench graph depends on the
//! cdylib-only lib target), and a freshness tripwire in
//! [`cdylib_artifact`] fails the bench if any `src/**/*.rs` is newer
//! than the artifact — a stale-ABI measurement can never run silently.
//!
//! # Why a C child, not Rust `extern` declarations
//!
//! The lib target is cdylib-only (no rlib), so this bench cannot link
//! the ABI as a Rust dependency — and hand-transcribed `extern "C"`
//! signatures would be a silent-UB drift hazard the radar cannot see.
//! Instead the driver compiles `c/bench.c` — which includes the
//! committed, drift-gated `corvid.h` (a signature change is a COMPILE
//! error) — exactly the smoke-suite mechanism (src/smoke.rs), with
//! `cc` as a dev-dependency. No new dependencies; nothing links into
//! the shipped cdylib.
//!
//! # Method
//!
//! The child does its own setup (corpus load) then N loop iterations;
//! the driver times the WHOLE child and subtracts a zero-iteration
//! baseline child (same setup, empty loop), so process spawn and
//! corpus load amortize away without any timing code in C. The native
//! twins run in-process on the same formulas (the engine-bench
//! deterministic-corpus convention: index arithmetic, no `rand`).
//! Medians of 5 rounds after a discarded warmup round, both sides;
//! single-machine numbers, compare relatively. The `put` shape
//! includes document CONSTRUCTION on both sides (a binding builds a
//! value per call), so the table prices the honest end-to-end path.
//! The scan and hybrid shapes are priced per full pass/query.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use corvid::{Db, Metric, Value};

/// Rounds per shape (medians reported); one warmup round is discarded.
const ROUNDS: usize = 5;
/// The corpus every get/scan/hybrid shape preloads (2k, the engine
/// benches' convention — e.g. `hnsw_build_2k_64d`).
const CORPUS: u32 = 2_000;
const DIM: usize = 64;

struct Shape {
    name: &'static str,
    /// Loop iterations the FFI child and the native twin each run.
    iters: u32,
}

const SHAPES: [Shape; 4] = [
    Shape {
        name: "put",
        iters: 10_000,
    },
    Shape {
        name: "get",
        iters: 100_000,
    },
    Shape {
        name: "scan",
        iters: 200,
    },
    Shape {
        name: "hybrid",
        iters: 500,
    },
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The release cdylib the documented invocation just built. `cargo
/// bench` does NOT produce it (the bench target does not depend on the
/// lib — cdylib-only, and cargo builds no harness from it under
/// `bench = false`), so the two-command invocation is load-bearing and
/// a FRESHNESS tripwire backs it: the newest `src/**/*.rs` mtime must
/// not be newer than the artifact's, so an edited-but-unbuilt crate
/// fails loudly instead of silently benchmarking the stale ABI (this
/// exact hazard: a T2-era 10-symbol dylib was found in target/release
/// while developing this bench).
fn cdylib_artifact() -> PathBuf {
    let mut dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let default = manifest_dir().join("..").join("..").join("target");
            default.canonicalize().unwrap_or(default)
        });
    dir.push("release");
    let artifact = ["libcorvid.dylib", "libcorvid.so", "corvid.dll"]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "the release cdylib is missing under {} — run `cargo build -p \
                 corvid-ffi --release` first (cargo bench does not build it)",
                dir.display()
            )
        });
    let built = artifact
        .metadata()
        .expect("the artifact exists; stat it")
        .modified()
        .expect("its mtime");
    let mut newest_src: Option<(std::time::SystemTime, PathBuf)> = None;
    let src = manifest_dir().join("src");
    let mut stack = vec![src];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .unwrap_or_else(|e| panic!("read {}: {e}", d.display()))
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(t) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let newer = match &newest_src {
                Some((prev, _)) => t > *prev,
                None => true,
            };
            if newer {
                newest_src = Some((t, p));
            }
        }
    }
    if let Some((t, p)) = newest_src
        && t > built
    {
        panic!(
            "{} is newer than the cdylib {} — the bench would measure the \
             STALE ABI; rerun `cargo build -p corvid-ffi --release`",
            p.display(),
            artifact.display()
        );
    }
    artifact
}

/// The host triple (cc wants TARGET/HOST; the harness always compiles
/// AND runs natively — mirrors src/smoke.rs).
fn host_triple() -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        panic!("unsupported bench host arch")
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
        panic!("unsupported bench host OS")
    }
}

/// What the link line names on MSVC (the import lib `corvid.dll.lib`
/// when present — src/smoke.rs's rule) — the cdylib file elsewhere.
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

/// Compile `c/bench.c` into `workdir` against the release cdylib.
fn build_bench(workdir: &Path) -> PathBuf {
    let dylib = cdylib_artifact();
    let triple = host_triple();
    let mut build = cc::Build::new();
    build
        .file(manifest_dir().join("c").join("bench.c"))
        .include(manifest_dir()) // corvid.h lives at the crate root
        .opt_level(2) // a release-mode consumer, like the shipped artifact
        .debug(false)
        .target(&triple)
        .host(&triple);
    let compiler = build.get_compiler();

    let exe = workdir.join(if cfg!(target_os = "windows") {
        "corvid_ffi_bench.exe"
    } else {
        "corvid_ffi_bench"
    });
    // `to_command` (not `Command::new(path)`): on MSVC the tool carries
    // the detected INCLUDE/LIB SDK environment (src/smoke.rs's rule).
    let mut cmd = compiler.to_command();
    cmd.arg(manifest_dir().join("c").join("bench.c"))
        .arg(link_target(&dylib))
        .arg("-o")
        .arg(&exe)
        .arg("-I")
        .arg(manifest_dir());
    for arg in compiler.args() {
        cmd.arg(arg);
    }
    if cfg!(target_os = "macos")
        && let Some(dir) = dylib.parent()
    {
        cmd.arg(format!("-Wl,-rpath,{}", dir.display()));
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("run the C compiler {:?}: {e}", compiler.path()));
    assert!(
        out.status.success(),
        "compiling c/bench.c failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    exe
}

/// Run the harness once; returns the wall-clock duration of the whole
/// child (spawn + setup + loop — callers subtract the iters-0 twin).
fn run_child(exe: &Path, mode: &str, iters: u32, corpus: u32, workdir: &Path) -> Duration {
    let dylib_dir = cdylib_artifact()
        .parent()
        .map(Path::to_path_buf)
        .expect("the artifact always has a parent");
    let start = Instant::now();
    let mut cmd = Command::new(exe);
    cmd.arg(mode).arg(iters.to_string()).arg(corpus.to_string());
    // Loader help (the smoke suite's belt-and-braces rule); Windows
    // resolves corvid.dll through PATH.
    let var = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "linux") {
        "LD_LIBRARY_PATH"
    } else {
        "PATH"
    };
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
    let out = cmd
        .current_dir(workdir)
        .output()
        .unwrap_or_else(|e| panic!("run the bench harness {}: {e}", exe.display()));
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "the FFI bench child failed ({mode}/{iters}):\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    elapsed
}

// ---- The native twins (identical formulas to c/bench.c) ------------

fn key_of(i: u32) -> String {
    format!("k{i:06}")
}

fn doc_value(i: u32) -> Value {
    let txt = format!(
        "w{} w{} w{} w{}",
        i % 50,
        (i + 13) % 50,
        (i + 29) % 50,
        (i + 41) % 50
    );
    let vec: Vec<f32> = (0..DIM)
        .map(|j| ((i * 37 + j as u32 * 11) % 2000) as f32 / 1000.0 - 1.0)
        .collect();
    let mut m = BTreeMap::new();
    m.insert("i".to_owned(), Value::Int(i as i64));
    m.insert("txt".to_owned(), Value::Text(txt));
    m.insert("vec".to_owned(), Value::Vector(vec));
    Value::Map(m)
}

fn query_vec() -> Vec<f32> {
    (0..DIM)
        .map(|j| ((7 * 37 + j as u32 * 11) % 2000) as f32 / 1000.0 - 1.0)
        .collect()
}

fn native_setup(db: &Db, corpus: u32) {
    let coll = db.collection("bench");
    for i in 0..corpus {
        coll.insert(key_of(i).as_bytes(), &doc_value(i))
            .expect("native setup insert");
    }
}

/// One native round: setup (corpus load) runs OUTSIDE the timed
/// region, mirroring the FFI child's zero-iteration subtraction — both
/// sides time the loop and only the loop.
fn native_round(mode: &str, iters: u32) -> Duration {
    let db = Db::open_in_memory().expect("open");
    if mode != "put" {
        native_setup(&db, CORPUS);
    }
    let coll = db.collection("bench");
    let start = Instant::now();
    match mode {
        "put" => {
            for i in 0..iters {
                coll.insert(key_of(CORPUS + i).as_bytes(), &doc_value(CORPUS + i))
                    .expect("native put");
            }
        }
        "get" => {
            for i in 0..iters {
                let idx = i % CORPUS;
                let doc = coll
                    .get(key_of(idx).as_bytes())
                    .expect("native get")
                    .expect("present");
                match doc {
                    Value::Map(ref m) if m["i"] == Value::Int(idx as i64) => {}
                    other => panic!("native get: wrong doc {other:?}"),
                }
            }
        }
        "scan" => {
            for _ in 0..iters {
                let mut n = 0u32;
                coll.for_each_doc(|_k, _v| {
                    n += 1;
                    Ok(true)
                })
                .expect("native scan");
                assert_eq!(n, CORPUS, "native scan row count");
            }
        }
        "hybrid" => {
            let q = query_vec();
            for _ in 0..iters {
                let rows = coll
                    .query()
                    .vector("vec", q.clone(), 10, Metric::Cosine)
                    .text("txt", "w3 w17", 10)
                    .run()
                    .expect("native hybrid");
                assert!(!rows.is_empty(), "native hybrid result");
            }
        }
        other => panic!("unknown native shape {other}"),
    }
    start.elapsed()
}

/// Median of a non-empty slice.
fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn fmt_per_op(d: Duration, iters: u32) -> String {
    let ns = d.as_nanos() as f64 / f64::from(iters);
    if ns >= 10_000.0 {
        format!("{:>12.1} us/op", ns / 1000.0)
    } else {
        format!("{ns:>12.1} ns/op")
    }
}

fn main() {
    let workdir = tempfile::tempdir().expect("a scratch workdir");
    let exe = build_bench(workdir.path());

    println!("corvid FFI crossing-cost bench (corpus {CORPUS}, docs {{i,txt,vec[64]}})");
    println!(
        "medians of {ROUNDS} rounds; FFI = C child through the ABI, native = in-process Rust\n"
    );
    println!(
        "{:<8} {:>16} {:>16} {:>7}",
        "shape", "FFI (ABI)", "native Rust", "ratio"
    );

    for shape in SHAPES {
        // FFI side: per-op = (T(n) - T(0)) / n, spawn+setup cancelled.
        let mut ffi_rounds = Vec::with_capacity(ROUNDS);
        for _ in 0..=ROUNDS {
            // index 0 is the discarded warmup
            let zero = run_child(&exe, shape.name, 0, CORPUS, workdir.path());
            let full = run_child(&exe, shape.name, shape.iters, CORPUS, workdir.path());
            ffi_rounds.push(full.saturating_sub(zero));
        }
        let ffi = median(&mut ffi_rounds[1..]);

        // Native twin: same loop in-process.
        let mut native_rounds = Vec::with_capacity(ROUNDS);
        for _ in 0..=ROUNDS {
            native_rounds.push(native_round(shape.name, shape.iters));
        }
        let native = median(&mut native_rounds[1..]);

        let ratio = ffi.as_secs_f64() / native.as_secs_f64();
        println!(
            "{:<8} {} {} {:>6.2}x",
            shape.name,
            fmt_per_op(ffi, shape.iters),
            fmt_per_op(native, shape.iters),
            ratio
        );
    }
    println!(
        "\nput prices construction + insert; scan is per full {CORPUS}-row pass; \
         hybrid is per vector+text RRF query (k=10 per source, results drained)."
    );
}
