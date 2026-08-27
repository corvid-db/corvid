//! Crash- and race-safety of index creation (audit A2).

use corvid::{Db, Value, field};
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
        .args([
            "--exact",
            "process_abort_mid_backfill_is_resumable_and_never_serves_partial",
        ])
        .env("CORVID_CRASH_DB", &path)
        .env(ABORT_ENV, "1")
        .env("RUST_TEST_THREADS", "1")
        .status()
        .unwrap();
    assert!(!status.success(), "child should have aborted");
    // Reopen: partial index must not produce partial results.
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    let rows = c
        .query()
        .filter(field("n").ge(Value::Int(4990)))
        .run()
        .unwrap();
    assert_eq!(rows.len(), 10, "resumed-or-fallback query must be exact");
    // And every doc is findable (full completeness after resume).
    for probe in [0i64, 2500, 4999] {
        let hit = c
            .query()
            .filter(field("n").eq(Value::Int(probe)))
            .run()
            .unwrap();
        assert_eq!(hit.len(), 1, "doc {probe} missing");
    }
}

/// The audit's registration race: a writer committing while a creation is in
/// flight must never be lost from the index.
#[test]
fn concurrent_writes_during_creation_are_never_lost() {
    // The writer thread needs its own handle, so the Db lives in an Arc from
    // the start (the main thread keeps using it through creation and probes).
    let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
    let c = db.collection("docs");
    for i in 0..3000i64 {
        c.insert(&(i as u32).to_le_bytes(), &rec(i)).unwrap();
    }
    let writer = {
        let db2 = std::sync::Arc::clone(&db);
        std::thread::spawn(move || {
            for i in 10_000..10_100i64 {
                db2.collection("docs")
                    .insert(&(i as u32).to_le_bytes(), &rec(i))
                    .unwrap();
            }
        })
    };
    db.collection("docs").create_scalar_index("n").unwrap();
    writer.join().unwrap();
    for probe in [0i64, 2999, 10_000, 10_099] {
        let hit = db
            .collection("docs")
            .query()
            .filter(field("n").eq(Value::Int(probe)))
            .run()
            .unwrap();
        assert_eq!(hit.len(), 1, "doc {probe} lost from index");
    }
}
