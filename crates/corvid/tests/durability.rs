//! Crash-recovery / durability tests.
//!
//! redb provides the underlying guarantee (a commit fsyncs; a crash can only
//! lose work that was never committed). These tests pin that guarantee at the
//! engine's own API: committed data survives an abrupt reopen — including a
//! real process abort with no graceful shutdown — while an aborted transaction
//! leaves nothing behind, and derived indexes stay consistent on reopen.

use corvid::{Db, Metric, Store, Value, field};
use std::collections::BTreeMap;

fn rec(n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("n".to_owned(), Value::Int(n));
    m.insert("v".to_owned(), Value::Vector(vec![n as f32, 1.0]));
    Value::Map(m)
}

#[test]
fn committed_data_survives_abrupt_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        let docs: Vec<(Vec<u8>, Value)> = (0..200i64).map(|i| (vec![i as u8], rec(i))).collect();
        let refs: Vec<(&[u8], &Value)> = docs.iter().map(|(k, v)| (k.as_slice(), v)).collect();
        c.insert_batch(&refs).unwrap();
        // No close/flush call exists; just drop the handle (simulating a process
        // that ends right after a committed write).
    }
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    assert_eq!(c.len().unwrap(), 200);
    assert_eq!(c.get(&[42]).unwrap(), Some(rec(42)));
}

#[test]
fn aborted_transaction_leaves_no_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let store = Store::open(&path).unwrap();
        store.put("docs", b"committed", b"yes").unwrap();
        // A transaction that writes then fails must roll back entirely.
        let result: Result<(), _> = store.transaction(|tx| {
            tx.put("docs", b"partial", b"should-not-persist")?;
            Err(corvid::Error::ReservedCollection("boom".into()))
        });
        assert!(result.is_err());
        // Not visible even before reopen.
        assert_eq!(store.get("docs", b"partial").unwrap(), None);
    }
    // ...and not after reopen.
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.get("docs", b"committed").unwrap(),
        Some(b"yes".to_vec())
    );
    assert_eq!(store.get("docs", b"partial").unwrap(), None);
}

#[test]
fn only_committed_work_survives_a_failed_followup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let store = Store::open(&path).unwrap();
        store
            .transaction(|tx| {
                for i in 0..10u8 {
                    tx.put("docs", &[i], b"v1")?;
                }
                Ok(())
            })
            .unwrap();
        // A second transaction writes more, then fails → rolled back.
        let _ = store.transaction(|tx| -> Result<(), corvid::Error> {
            for i in 10..20u8 {
                tx.put("docs", &[i], b"v2")?;
            }
            Err(corvid::Error::ReservedCollection("boom".into()))
        });
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.count("docs").unwrap(), 10);
    assert_eq!(store.get("docs", &[15]).unwrap(), None);
}

#[test]
fn indexes_consistent_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        for i in 0..50i64 {
            c.insert(&[i as u8], &rec(i)).unwrap();
        }
        c.create_vector_index_ondisk("v", Metric::L2).unwrap();
        c.create_scalar_index("n").unwrap();
    }
    // Reopen: on-disk vector graph and scalar index reload from disk (no
    // rebuild) and answer correctly.
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    let near = c.vector_search("v", &[7.0, 1.0], 1, Metric::L2).unwrap();
    assert_eq!(near[0].key, vec![7u8]);
    let filtered = c
        .query()
        .filter(field("n").ge(Value::Int(48)))
        .run()
        .unwrap();
    let mut keys: Vec<_> = filtered.iter().map(|r| r.key.clone()).collect();
    keys.sort();
    assert_eq!(keys, vec![vec![48u8], vec![49u8]]);
}

/// A real crash: a child process commits a batch and then `abort()`s without
/// any graceful shutdown. The parent reopens the file and asserts the committed
/// data is intact — proving durability does not depend on clean teardown.
#[test]
fn committed_data_survives_process_abort() {
    const DB_ENV: &str = "CORVID_CRASH_DB";

    if let Ok(path) = std::env::var(DB_ENV) {
        // Child role: write a committed batch, then die hard.
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        let docs: Vec<(Vec<u8>, Value)> = (0..100i64).map(|i| (vec![i as u8], rec(i))).collect();
        let refs: Vec<(&[u8], &Value)> = docs.iter().map(|(k, v)| (k.as_slice(), v)).collect();
        c.insert_batch(&refs).unwrap();
        std::process::abort();
    }

    // Parent role: re-exec this very test in a child with the env var set.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "committed_data_survives_process_abort"])
        .env(DB_ENV, &path)
        .env("RUST_TEST_THREADS", "1")
        .status()
        .unwrap();
    // The child aborted, so it did not exit successfully.
    assert!(
        !status.success(),
        "child should have aborted, not exited cleanly"
    );

    // The committed batch is fully recoverable.
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    assert_eq!(c.len().unwrap(), 100);
    assert_eq!(c.get(&[73]).unwrap(), Some(rec(73)));
}
