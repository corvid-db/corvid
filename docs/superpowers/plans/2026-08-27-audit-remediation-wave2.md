# Audit Remediation Wave 2 — Index-Creation Atomicity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make index creation crash-safe and race-safe: a persisted `Building{cursor} → Complete` state machine for all five durable index kinds, queries never serve from a `Building` index, and an interrupted backfill resumes on first use after reopen.

**Architecture:** One shared module (`index_build.rs`) owns the def-state codec and the atomic backfill driver — every page's index writes and the cursor advance commit in one transaction; the final transaction flips `Complete`. Each index kind (scalar, compound, geo, on-disk FTS, on-disk vector) wires its `create_*` through the driver, splits maintenance (all defs) from serviceability (complete defs only), and gains a lazy first-use resume. Legacy defs without state bytes decode as `Complete`.

**Tech Stack:** Rust 2024, redb 4.1, std sync primitives.

**Spec:** `docs/superpowers/specs/2026-08-27-audit-remediation-design.md` — Wave 2 section + Decisions 1 and 4. Read both before starting.

## Global Constraints

- `cargo test --workspace` green before every commit; `cargo clippy --all-targets --workspace -- -D warnings` clean; `cargo fmt --all` before commit; TDD (red first).
- Never panic on user input; typed errors.
- Coverage ≥ 90% held at wave exit (Task 7).
- **Dump format does NOT change**: `Db::load` replays indexes through the real `create_*` functions, so a load always materializes a complete index — creation state is engine-internal and never serialized (spec-interpretation ruling; strictly safer than resuming a stale cursor across dump/load).
- Def-state encoding is **front-marked**: new-format def values start with `0xFF` (never a valid legacy first byte), then state, then kind bytes. Malformed state decodes as `Building{cursor: []}` (backfill restarts — idempotent). Legacy values (no `0xFF`) decode as `Complete` with the whole value as kind bytes.
- Test failpoints are env-gated: `CORVID_TEST_ABORT_AFTER_PAGES=<n>` (process-abort after n committed backfill pages, `std::process::abort()`). Unset → no-op. Documented as test seams.
- Line anchors are from commit `cc6c917` and may drift; quoted code is authoritative.

---

### Task 1: `index_build` module — def-state codec + atomic backfill driver

**Files:**
- Create: `crates/corvid/src/index_build.rs`
- Modify: `crates/corvid/src/lib.rs` (add `pub mod index_build;` after `pub mod hnsw;`), `crates/corvid/src/db.rs` (add `index_resume: Mutex<()>` field + init in BOTH `open` and `open_in_memory`, plus accessor `pub(crate) fn index_resume(&self) -> &Mutex<()>`)

**Interfaces (Produces — later tasks consume exactly these):**
```rust
pub(crate) enum DefState { Complete, Building { cursor: Vec<u8> } }

// value layout: [0xFF] [tag:u8] ([cursor_len:u32 BE] [cursor])? [kind_bytes...]
// tag 0 = Complete, 1 = Building. Legacy (no leading 0xFF) = Complete, kind_bytes = whole value.
pub(crate) fn encode_def(kind_bytes: &[u8], state: &DefState) -> Vec<u8>
pub(crate) fn decode_def(value: &[u8]) -> (Vec<u8>, DefState)
// Returns (kind_bytes, state). Malformed 0xFF form → (rest, Building{cursor: vec![]}).

pub(crate) fn run_atomic_backfill(
    store: &Store,
    collection: &str,
    defs_ns: &str,
    def_key: &[u8],
    kind_bytes: &[u8],
    start_cursor: &[u8],
    index_page: &mut dyn FnMut(&mut crate::store::WriteBatch<'_>, &[(Vec<u8>, Vec<u8>)]) -> Result<()>,
) -> Result<()>
// Scans `collection` from start_cursor in 2048-row pages; per page ONE txn:
// index_page(tx, &page) then def row = Building{cursor: next_after(last)};
// when a scan returns no rows, ONE final txn writes def row = Complete.
// After each page commit, checks the abort failpoint.

pub(crate) fn read_building_cursor(store: &Store, defs_ns: &str, def_key: &[u8]) -> Result<Option<Vec<u8>>>
// Some(cursor) iff the def row exists and decodes Building.
```

- [ ] **Step 1: Write failing unit tests** (in `index_build.rs` `mod tests`, using `crate::store::Store::open_in_memory`):

```rust
use super::*;

#[test]
fn legacy_values_decode_complete_with_kind_bytes_intact() {
    let (kb, st) = decode_def(b"");
    assert!(kb.is_empty());
    assert!(matches!(st, DefState::Complete));
    let (kb, st) = decode_def(&[1]); // legacy on-disk text kind byte
    assert_eq!(kb, vec![1]);
    assert!(matches!(st, DefState::Complete));
    let (kb, st) = decode_def(&[7, 0, 1]); // legacy vector def [metric, quant, kind]
    assert_eq!(kb, vec![7, 0, 1]);
    assert!(matches!(st, DefState::Complete));
}

#[test]
fn building_round_trips_with_cursor_and_kind_bytes() {
    for cursor in [vec![], vec![0u8], b"long-cursor-key-42".to_vec()] {
        let enc = encode_def(&[1, 2, 3], &DefState::Building { cursor: cursor.clone() });
        let (kb, st) = decode_def(&enc);
        assert_eq!(kb, vec![1, 2, 3]);
        match st {
            DefState::Building { cursor: c } => assert_eq!(c, cursor),
            DefState::Complete => panic!("expected Building"),
        }
    }
    let enc = encode_def(&[9], &DefState::Complete);
    let (kb, st) = decode_def(&enc);
    assert_eq!(kb, vec![9]);
    assert!(matches!(st, DefState::Complete));
}

#[test]
fn malformed_state_decodes_as_building_from_scratch() {
    // Truncated 0xFF form: tag says Building but cursor length overruns.
    let bad = [0xFFu8, 1, 0, 0, 0, 9, 1, 2];
    let (_, st) = decode_def(&bad);
    match st {
        DefState::Building { cursor } => assert!(cursor.is_empty()),
        DefState::Complete => panic!("malformed must not decode Complete"),
    }
}

#[test]
fn backfill_commits_pages_and_completes() {
    let s = crate::store::Store::open_in_memory().unwrap();
    for i in 0..10u8 {
        s.put("docs", &[i], &[i]).unwrap();
    }
    let mut pages: Vec<Vec<u8>> = Vec::new();
    run_atomic_backfill(&s, "docs", "__tdefs__", b"docs\x00f", &[5], b"", &mut |tx, page| {
        for (k, _) in page {
            tx.put("__tix__", k, b"x")?;
        }
        pages.push(page.iter().map(|(k, _)| k.clone()).collect());
        Ok(())
    })
    .unwrap();
    // One page (10 docs < 2048), then Complete.
    assert_eq!(pages.len(), 1);
    let (_, st) = decode_def(&s.get("__tdefs__", b"docs\x00f").unwrap().unwrap());
    assert!(matches!(st, DefState::Complete));
    assert_eq!(s.scan("__tix__").unwrap().len(), 10);
    // Resume from a mid-cursor only processes the remainder.
    let mut seen = 0usize;
    run_atomic_backfill(&s, "docs", "__tdefs__", b"docs\x00f", &[5], &[5, 0], &mut |_, page| {
        seen += page.len();
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, 5); // keys 5..=9
    assert!(matches!(decode_def(&s.get("__tdefs__", b"docs\x00f").unwrap().unwrap()).1, DefState::Complete));
}

#[test]
fn backfill_error_leaves_building_cursor_at_last_good_page() {
    let s = crate::store::Store::open_in_memory().unwrap();
    for i in 0..6u8 {
        s.put("docs", &[i], &[i]).unwrap();
    }
    let r = run_atomic_backfill(&s, "docs", "__tdefs__", b"docs\x00f", &[], b"", &mut |tx, page| {
        for (k, _) in page {
            tx.put("__tix__", k, b"x")?;
        }
        if page.iter().any(|(k, _)| k == &vec![3u8]) {
            return Err(crate::Error::Storage(redb::StorageError::Corrupted("boom".into())));
        }
        Ok(())
    });
    assert!(r.is_err());
    // Single page means the failed txn rolled back entirely: no def row yet.
    // With a fresh driver call the failpoint story is the crash test's job.
    let row = s.get("__tdefs__", b"docs\x00f").unwrap();
    assert!(row.is_none() || matches!(decode_def(&row.unwrap()).1, DefState::Building { .. }));
    assert_eq!(s.scan("__tix__").unwrap().len(), 0); // page txn rolled back
}
```

- [ ] **Step 2: Run** `cargo test -p corvid index_build` — expect compile failure (module missing).
- [ ] **Step 3: Implement** `index_build.rs` exactly:

```rust
//! Atomic index-creation state (audit A2): one codec and one backfill driver
//! shared by every persisted index kind.
//!
//! A persisted index definition carries a creation state so a crash or error
//! between registration and backfill completion can never leave a permanently
//! partial index that queries trust. Every backfill page commits its index
//! writes together with an advanced cursor in ONE transaction; completion is
//! its own final transaction. Writes maintain every index — building or
//! complete — inside the document transaction; index entries are keyed by
//! encoded value ‖ doc key, so backfill and maintenance overlap safely
//! (idempotent upserts). Queries never serve from a `Building` index.
//!
//! Legacy defs (no state marker) decode as `Complete`; a malformed state
//! decodes as `Building` with an empty cursor (backfill restarts — safe).

use crate::error::Result;
use crate::store::Store;

/// Marker byte starting every new-format def value. Never a valid legacy
/// first byte: legacy scalar/compound/geo defs are empty, text kind bytes are
/// 0/1, vector def bytes are small metric/quant/kind tags.
const NEW_FORMAT: u8 = 0xFF;
const TAG_COMPLETE: u8 = 0;
const TAG_BUILDING: u8 = 1;
/// Backfill page size (documents per transaction).
const PAGE: usize = 2048;

pub(crate) enum DefState {
    Complete,
    Building { cursor: Vec<u8> },
}

pub(crate) fn encode_def(kind_bytes: &[u8], state: &DefState) -> Vec<u8> {
    let mut out = Vec::with_capacity(kind_bytes.len() + 10);
    out.push(NEW_FORMAT);
    match state {
        DefState::Complete => out.push(TAG_COMPLETE),
        DefState::Building { cursor } => {
            out.push(TAG_BUILDING);
            out.extend_from_slice(&(cursor.len() as u32).to_be_bytes());
            out.extend_from_slice(cursor);
        }
    }
    out.extend_from_slice(kind_bytes);
    out
}

/// Decode a def-row value into `(kind_bytes, state)`. Empty and non-`0xFF`
/// values are legacy `Complete` with the whole value as kind bytes; a
/// malformed `0xFF` form decodes as `Building { cursor: vec![] }`.
pub(crate) fn decode_def(value: &[u8]) -> (Vec<u8>, DefState) {
    if value.first() != Some(&NEW_FORMAT) {
        return (value.to_vec(), DefState::Complete);
    }
    let rest = &value[1..];
    match rest.first() {
        Some(&TAG_COMPLETE) => (rest[1..].to_vec(), DefState::Complete),
        Some(&TAG_BUILDING) => {
            let body = &rest[1..];
            let len = u32::from_be_bytes(body.get(0..4).and_then(|b| b.try_into().ok()).unwrap_or(0)) as usize;
            match body.get(4..4 + len) {
                Some(cursor) => (
                    body.get(4 + len..).unwrap_or(&[]).to_vec(),
                    DefState::Building { cursor: cursor.to_vec() },
                ),
                // Truncated cursor: restart the backfill from the beginning.
                None => (Vec::new(), DefState::Building { cursor: Vec::new() }),
            }
        }
        _ => (Vec::new(), DefState::Building { cursor: Vec::new() }),
    }
}

/// The def row's current cursor iff it exists and is `Building`.
pub(crate) fn read_building_cursor(store: &Store, defs_ns: &str, def_key: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(match store.get(defs_ns, def_key)? {
        Some(v) => match decode_def(&v).1 {
            DefState::Building { cursor } => Some(cursor),
            DefState::Complete => None,
        },
        None => None,
    })
}

/// Test failpoint: `CORVID_TEST_ABORT_AFTER_PAGES=n` aborts the process after
/// `n` committed backfill pages (simulates a crash mid-creation). Unset → off.
fn abort_after_pages() -> Option<usize> {
    static N: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    (*N.get_or_init(|| {
        std::env::var("CORVID_TEST_ABORT_AFTER_PAGES")
            .ok()
            .and_then(|v| v.parse().ok())
    }))
    .clone()
}

/// Drive one atomic backfill over `collection` starting at `start_cursor`.
/// Each page's `index_page` writes and the cursor advance commit in ONE
/// transaction; a final transaction marks the def `Complete`.
#[allow(clippy::type_complexity)]
pub(crate) fn run_atomic_backfill(
    store: &Store,
    collection: &str,
    defs_ns: &str,
    def_key: &[u8],
    kind_bytes: &[u8],
    start_cursor: &[u8],
    index_page: &mut dyn FnMut(
        &mut crate::store::WriteBatch<'_>,
        &[(Vec<u8>, Vec<u8>)],
    ) -> Result<()>,
) -> Result<()> {
    let mut cursor = start_cursor.to_vec();
    let mut committed_pages = 0usize;
    loop {
        let page = store.scan_from(collection, &cursor, PAGE)?;
        let Some((last_key, _)) = page.last() else { break };
        let mut next = last_key.clone();
        next.push(0);
        let def_value = encode_def(kind_bytes, &DefState::Building { cursor: next.clone() });
        store.transaction(|tx| {
            index_page(tx, &page)?;
            tx.put(defs_ns, def_key, &def_value)?;
            Ok(())
        })?;
        cursor = next;
        committed_pages += 1;
        if let Some(n) = abort_after_pages()
            && committed_pages >= n
        {
            std::process::abort();
        }
    }
    let complete = encode_def(kind_bytes, &DefState::Complete);
    store.transaction(|tx| tx.put(defs_ns, def_key, &complete))?;
    Ok(())
}
```

In `db.rs`: add field `index_resume: Mutex<()>` to `Db`, initialize `index_resume: Mutex::new(())` in both constructors, and add:

```rust
    /// Serialization point for lazy index-build resumes (try-lock only: a
    /// query arriving while another thread resumes proceeds on fallbacks).
    pub(crate) fn index_resume(&self) -> &Mutex<()> {
        &self.index_resume
    }
```

- [ ] **Step 4: Run** `cargo test -p corvid index_build` → PASS; then `cargo test -p corvid`, clippy, fmt; commit: `index_build: def-state codec + atomic backfill driver (audit A2 core)`

---

### Task 2: Scalar kind — state machine, serviceability gate, lazy resume

**Files:** Modify `crates/corvid/src/scalar.rs` (ScalarState type, register/load/create/has_scalar_index, resume), `crates/corvid/src/db.rs` (`try_resume_index_builds` entry), tests in scalar.rs + new `crates/corvid/tests/creation_atomicity.rs`.

**Interfaces (Produces):**
- `ScalarState.defs: HashMap<(String, String), bool>` — value is `building`.
- `pub(crate) fn try_resume_index_builds(&self, collection: &str) -> Result<()>` on `Db` (Task 3-6 extend its kind list).
- `has_scalar_index` returns `false` for building defs (unique checks + builder probes conservatively fall back).

- [ ] **Step 1: Failing tests.** In scalar.rs `mod tests` add:

```rust
/// A building scalar index is never served: filtered queries fall back to a
/// scan and stay correct; the first such query resumes the build.
#[test]
fn building_scalar_index_falls_back_then_resumes() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    for i in 0..50i64 {
        c.insert(&[i as u8], &rec(i)).unwrap();
    }
    // Forge a Building def exactly as an interrupted creation would leave it.
    db.register_scalar_index("docs", "n").unwrap(); // registers Building
    assert!(!db.has_scalar_index("docs", "n"), "building def must not be serviceable");
    // Before resume: a filtered query must still be correct (scan fallback).
    let rows = c.query().filter(crate::field("n").ge(Value::Int(40))).run().unwrap();
    assert_eq!(rows.len(), 10); // resumed by the query itself, then correct
    // After the resume the def is complete and serviceable.
    assert!(db.has_scalar_index("docs", "n"));
}

#[test]
fn legacy_stateless_scalar_def_is_complete() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"k", &rec(1)).unwrap();
    db.register_scalar_index("docs", "n").unwrap();
    // Overwrite the def row with the legacy empty form.
    db.store().put(crate::scalar::SCALAR_DEFS, b"docs\x00n", b"").unwrap();
    let fresh = Db::open_in_memory().unwrap(); // loaders on a fresh db
    let _ = fresh; // state decode is covered below via reopen of a file db
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = Db::open(&path).unwrap();
        db.collection("docs").insert(b"k", &rec(1)).unwrap();
        db.register_scalar_index("docs", "n").unwrap();
        db.store().put(crate::scalar::SCALAR_DEFS, b"docs\x00n", b"").unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert!(db.has_scalar_index("docs", "n")); // legacy → Complete → serviceable
}
```

And `crates/corvid/tests/creation_atomicity.rs`:

```rust
//! Crash- and race-safety of index creation (audit A2).

use corvid::{Db, Metric, Value, field};
use std::collections::BTreeMap;

fn rec(i: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("n".to_owned(), Value::Int(i));
    m.insert("v".to_owned(), Value::Vector(vec![i as f32, 1.0]));
    m.insert("body".to_owned(), Value::Text(format!("doc number {i}")));
    Value::Map(m)
}

const ABORT_ENV: &str = "CORVID_TEST_ABORT_AFTER_PAGES";

/// A real crash mid-backfill (child process aborts after page 1). The parent
/// reopens: the def must be Building (never trusted), the first query resumes
/// it, and results equal the unindexed truth.
#[test]
fn process_abort_mid_backfill_is_resumable_and_never_serves_partial() {
    if let Ok(path) = std::env::var("CORVID_CRASH_DB") {
        let db = Db::open(&path).unwrap();
        db.collection("docs").create_scalar_index("n").unwrap();
        return; // unreachable when the failpoint fires
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        for i in 0..5000i64 {
            c.insert(&(i as u32).to_le_bytes(), &rec(i)).unwrap();
        }
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_abort_mid_backfill_is_resumable_and_never_serves_partial"])
        .env("CORVID_CRASH_DB", &path)
        .env(ABORT_ENV, "1")
        .env("RUST_TEST_THREADS", "1")
        .status()
        .unwrap();
    assert!(!status.success(), "child should have aborted");
    // Reopen: partial index must not produce partial results.
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    let rows = c.query().filter(field("n").ge(Value::Int(4990))).run().unwrap();
    assert_eq!(rows.len(), 10, "resumed-or-fallback query must be exact");
    // And every doc is findable (full completeness after resume).
    for probe in [0i64, 2500, 4999] {
        let hit = c.query().filter(field("n").eq(Value::Int(probe))).run().unwrap();
        assert_eq!(hit.len(), 1, "doc {probe} missing");
    }
}

/// The audit's registration race: a writer committing while a creation is in
/// flight must never be lost from the index.
#[test]
fn concurrent_writes_during_creation_are_never_lost() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    for i in 0..3000i64 {
        c.insert(&(i as u32).to_le_bytes(), &rec(i)).unwrap();
    }
    let writer = {
        let db = std::sync::Arc::new(db);
        let db2 = std::sync::Arc::clone(&db);
        std::thread::spawn(move || {
            for i in 10_000..10_100i64 {
                db2.collection("docs").insert(&(i as u32).to_le_bytes(), &rec(i)).unwrap();
            }
        })
    };
    db.collection("docs").create_scalar_index("n").unwrap();
    writer.join().unwrap();
    for probe in [0i64, 2999, 10_000, 10_099] {
        let hit = db.collection("docs")
            .query()
            .filter(field("n").eq(Value::Int(probe)))
            .run()
            .unwrap();
        assert_eq!(hit.len(), 1, "doc {probe} lost from index");
    }
}
```

- [ ] **Step 2: Run** `cargo test -p corvid building_scalar_index legacy_stateless process_abort concurrent_writes --test creation_atomicity` variants → observe failures (register currently writes a Complete-shaped empty def; `SCALAR_DEFS` may need `pub(crate)` visibility for the test — expose it if private via `pub(crate) const SCALAR_DEFS`).
- [ ] **Step 3: Implement.** In scalar.rs:
  - `ScalarState { defs: HashMap<(String, String), bool> }` (value = building). Mechanically adapt iteration sites: `scalar_fields` keeps returning ALL fields (maintenance); `has_scalar_index` becomes `state.defs.get(&(c,f)).is_some_and(|b| !*b)`.
  - `register_scalar_index`: put `index_build::encode_def(&[], &DefState::Building { cursor: vec![] })`, insert `true`.
  - `load_scalar_defs`: `let (kb, st) = index_build::decode_def(&value); state.defs.insert(def, matches!(st, DefState::Building { .. }));` (kind bytes unused for scalar).
  - `create_scalar_index`: after register, read cursor via `index_build::read_building_cursor`, then:

```rust
        let ns = namespace(self.name(), field);
        let kb: Vec<u8> = Vec::new();
        index_build::run_atomic_backfill(
            self.db().store(),
            self.name(),
            SCALAR_DEFS,
            &def_key(self.name(), field),
            &kb,
            &cursor.unwrap_or_default(),
            &mut |tx, page| {
                for (key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    if let Some(value) = doc.get_path(field) {
                        insert_in_txn(tx, &ns, key, value)?;
                    }
                }
                Ok(())
            },
        )?;
        self.db().mark_scalar_complete(self.name(), field);
        Ok(())
```

  - Add `pub(crate) fn mark_scalar_complete(&self, collection: &str, field: &str)` on `Db` (scalar.rs): brief lock, `defs.insert((c,f), false)`.
  - Add to db.rs (Db impl):

```rust
    /// Resume any interrupted index builds for `collection` before its
    /// indexes are consulted. Try-lock: if another thread is already
    /// resuming, return and let callers run on their fallbacks.
    pub(crate) fn try_resume_index_builds(&self, collection: &str) -> Result<()> {
        let jobs = self.collect_building_scalar(collection)?; // Task 2: scalar only
        if jobs.is_empty() {
            return Ok(());
        }
        if self.index_resume().try_lock().is_err() {
            return Ok(());
        }
        for (field, cursor) in jobs {
            self.resume_scalar(collection, &field, &cursor)?;
        }
        Ok(())
    }
```

  with `collect_building_scalar` / `resume_scalar` on Db in scalar.rs: collect returns `Vec<(String, Vec<u8>)>` of building fields with cursors read from def rows; `resume_scalar` re-runs the same `run_atomic_backfill` closure as creation, then `mark_scalar_complete`.
  - First lines of `scalar_candidates` AND `scalar_prefix_candidates`: `self.try_resume_index_builds(collection)?;`
  - Make `SCALAR_DEFS` `pub(crate)` if not already.

- [ ] **Step 4: Green**, full gates, commit: `scalar: atomic crash-safe creation with lazy resume (audit A2)`

---

### Task 3: Compound kind

Identical shape to Task 2 for `create_compound_index` / `register_compound_index` / `load_compound_defs` (scalar.rs, `COMPOUND_DEFS`, `compound_def_key`): register writes `Building{cursor:[]}` with empty kind bytes; create runs the driver with the closure collecting `Option<Vec<&Value>>` and calling `compound_insert_in_txn` (skip docs missing any field, matching current behavior); `compound_indexes(coll)` filters `!building`; maintenance iteration sites mechanically adapted to the new defs type; add `collect_building_compound`/`resume_compound` jobs to `try_resume_index_builds` and a `mark_compound_complete`; `try_resume_index_builds(collection)?;` as the first line of `compound_candidates`. Tests: forge-Building compound def → prefix query correct and complete after resume; legacy `b""` def row → serviceable. Commit: `compound: atomic crash-safe creation with lazy resume (audit A2)`

---

### Task 4: Geo kind

Same for `create_geo_index` / `register_geo_index` / `load_geo_defs` (geo_index.rs, `GEO_DEFS`, `def_key`): register Building; driver closure `if let Some(value) = doc.get_path(field) { insert_in_txn(tx, &ns, key, value)? }` using geo's own `insert_in_txn`; `has_geo_index` gates on `!building`; `GeoState.defs` becomes `HashMap<(String, String), bool>` (geo_specs iterates keys — unchanged output); `try_resume_index_builds` gains geo jobs; first line of `geo_candidates` resumes. Tests: forged Building geo def → `within_km`/radius query correct via fallback then complete after resume; legacy empty def → serviceable. Commit: `geo: atomic crash-safe creation with lazy resume (audit A2)`

---

### Task 5: On-disk FTS kind

`FtsState.defs: HashMap<(String, String), (TextKind, bool)>` (bool = building; InMemory always `false`). `register_text_index(collection, field, TextKind::OnDisk)` writes `encode_def(&[kind_byte(kind)], Building{cursor:[]})`; `TextKind::InMemory` writes `encode_def(&[kind_byte(kind)], Complete)` (no durable backfill exists). `load_text_defs` decodes via `decode_def` and `kind_from(kind_bytes.first())`. `create_text_index_ondisk` drives the backfill with the closure `if let Some(text) = doc.get_path(field).and_then(Value::as_text) { disk_fts::insert_in_txn(tx, &ns, key, t)? }` then marks complete. `fts_search`/`fts_phrase_search`: FIRST line (before any lock) `self.try_resume_index_builds(collection)?;`, and the `Some(TextKind::OnDisk)` arm serves only when `!building` — a building OnDisk index returns `Ok(None)` (caller falls back to exact scan). `text_specs` iterates kinds, unchanged (dump never carries state). `try_resume_index_builds` gains fts jobs. Tests: forged Building on-disk text def → `text_search` correct (fallback) and index serviceable after first call; InMemory def unaffected. Commit: `fts: atomic crash-safe on-disk creation with lazy resume (audit A2)`

---

### Task 6: On-disk vector kinds

`VectorDef` gains `pub(crate) building: bool` (set at every construction site: `register_vector_index_inner` writes `Building` iff `kind.is_on_disk()` — def value `encode_def(&[metric_byte, quant_byte, kind_byte], …)` — and inserts `building` accordingly; `load_index_defs` decodes kind bytes from `decode_def(..).0` and state from `.1`; `vector_specs` ignores building). Make `disk_hnsw::insert_in_txn` `pub(crate)`. `create_vector_index_ondisk_quantized` and `create_vector_index_ondisk_pq` run their existing page loops through the driver (closure: `if let Some(v) = doc.get_path(field).and_then(Value::as_vector) { disk_hnsw::insert_in_txn(tx, &ns, &params, key, v)? }`; PQ keeps its pre-registration training sample unchanged). `ann_search`: first line `self.try_resume_index_builds(collection)?;` then the on-disk branch returns `Ok(None)` when `def.building` (exact fallback — this also fixes the audit's "crash before first chunk → silent empty" worst case, since an absent/empty namespace under a Building def never serves). In-memory kinds stay `Complete` (lazy build unchanged). Tests: forged Building on-disk def → `vector_search` equals exact results; after first call the index serves; PQ variant smoke test; legacy def bytes `[m,q,k]` decode Complete. Commit: `vector: atomic crash-safe on-disk creation with lazy resume (audit A2)`

---

### Task 7: Wave exit — gates, decision log, AUDIT status

1. Full gates: `cargo fmt --all --check`, clippy `-D warnings`, `cargo test --workspace`, `cargo llvm-cov --workspace --fail-under-lines 90` (dips → report, no padding).
2. DESIGN.md decision log — append:

```markdown
| 2026-08-27 | Index creation = persisted Building{cursor}→Complete watermark with lazy first-use resume; queries never serve Building defs; dump/load materializes Complete via create_* replay | Closes audit A2: register-then-backfill left permanently partial indexes after crash/error/race; single-txn backfill would hold redb's write lock for the whole corpus (the availability cliff), staging-swap doubles maintenance paths |
```

3. AUDIT.md status: mark A2 fixed with the wave-2 commit range. Commit: `AUDIT/DESIGN: wave 2 — index-creation atomicity landed (A2 fixed)`
