//! Lifecycle conformance (Task 13): the admin surface through the public API
//! only — file-backed open/reopen (including the redb exclusive-lock pin),
//! in-memory isolation, backup restore, bulk durability scopes, compaction,
//! collection listing (user namespaces only), relaxed durability + flush,
//! the byte-level `Store` surface (transactions, snapshots, scans, batches),
//! the dump→load round-trip of EVERY Value variant and index family, the
//! semantic cache, the probabilistic sketches, plan identity and the plan
//! cache, and `Error::CorruptIndex` driven from real corrupted index bytes.
//!
//! The existing smoke test (`lifecycle_smoke_dump_load_roundtrips_documents`)
//! stays as the anchor; the matrix below is the Task 13 deliverable.

use std::collections::BTreeMap;

use corvid::{BloomFilter, Db, HyperLogLog, Metric, Quantization, Store, Value, field};

fn doc(name: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Map(m)
}

/// A doc carrying a vector under `field` (for the index round-trip corpus).
fn vecdoc(field: &str, v: Vec<f32>) -> Value {
    map(&[(field, Value::Vector(v))])
}

/// A doc with scalar/text/geo fields plus one named vector field.
fn rich_doc(field: &str, v: Vec<f32>, i: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("n".to_owned(), Value::Int(i));
    m.insert(
        "body".to_owned(),
        Value::Text(format!("item number {i} about vectors")),
    );
    m.insert(
        "pos".to_owned(),
        map(&[
            ("lat", Value::Float(10.0 + i as f64)),
            ("lon", Value::Float(20.0)),
        ]),
    );
    if !field.is_empty() {
        m.insert(field.to_owned(), Value::Vector(v));
    }
    Value::Map(m)
}

#[test]
fn lifecycle_smoke_dump_load_roundtrips_documents() {
    let source = Db::open_in_memory().unwrap();
    source
        .collection("docs")
        .insert(b"k", &doc("corvid", 8))
        .unwrap();
    source
        .collection("notes")
        .insert(b"n", &Value::Vector(vec![1.0, 2.0]))
        .unwrap();

    let mut buf = Vec::new();
    source.dump(&mut buf).unwrap();
    assert!(!buf.is_empty());

    let target = Db::open_in_memory().unwrap();
    target.load(buf.as_slice()).unwrap();
    assert_eq!(
        target.collection("docs").get(b"k").unwrap(),
        Some(doc("corvid", 8))
    );
    assert_eq!(
        target.collection("notes").get(b"n").unwrap(),
        Some(Value::Vector(vec![1.0, 2.0]))
    );
    let mut names = target.collections().unwrap();
    names.sort();
    assert_eq!(names, vec!["docs".to_owned(), "notes".to_owned()]);
}

// ===========================================================================
// Db::open — real files, persistence, failure modes
// ===========================================================================

/// A file-backed `Db::open` creates the file, and the data survives the
/// handle being dropped and the file reopened (create → insert → drop →
/// reopen → identical state). A path whose PARENT DIRECTORY does not exist
/// fails at the redb layer with the exact `Error::Database` variant (the io
/// error is transparently wrapped, never panics).
#[test]
fn lifecycle_db_open_real_file_persists_across_reopen_and_rejects_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc("persist", 9)).unwrap();
        c.insert_auto(&Value::Int(1)).unwrap();
        assert!(path.exists(), "open must create the backing file");
    }
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    assert_eq!(c.get(b"k").unwrap(), Some(doc("persist", 9)));
    assert_eq!(c.len().unwrap(), 2);
    assert_eq!(c.get(b"00000000000000000000").unwrap(), Some(Value::Int(1)));

    // Missing parent directory: the file cannot be created.
    let bad = dir.path().join("no/such/dir/corvid.db");
    let err = Db::open(&bad).map(|_| ());
    assert!(
        matches!(err, Err(corvid::Error::Database(_))),
        "opening under a nonexistent parent must be Error::Database, got {err:?}"
    );
}

/// Two live handles to the SAME file: redb locks database files exclusively,
/// so a second `Db::open` while the first handle is alive fails with
/// `Error::Database` — and succeeds again once the first handle is dropped.
#[test]
fn lifecycle_db_open_second_handle_to_same_file_hits_the_redb_exclusive_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    let db1 = Db::open(&path).unwrap();
    db1.collection("docs").insert(b"k", &Value::Int(1)).unwrap();

    let second = Db::open(&path).map(|_| ());
    assert!(
        matches!(second, Err(corvid::Error::Database(_))),
        "a second live handle must hit the file lock as Error::Database, got {second:?}"
    );

    drop(db1);
    let db2 = Db::open(&path).unwrap();
    assert_eq!(
        db2.collection("docs").get(b"k").unwrap(),
        Some(Value::Int(1))
    );
}

/// Independent `open_in_memory` databases are fully isolated: same collection
/// names, different data; writes to one never surface in the other; a fresh
/// in-memory db lists no collections.
#[test]
fn lifecycle_db_open_in_memory_instances_are_isolated() {
    let a = Db::open_in_memory().unwrap();
    let b = Db::open_in_memory().unwrap();
    assert!(
        Db::open_in_memory()
            .unwrap()
            .collections()
            .unwrap()
            .is_empty()
    );

    a.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
    b.collection("docs")
        .insert(b"k", &Value::Text("other".into()))
        .unwrap();
    assert_eq!(a.collection("docs").get(b"k").unwrap(), Some(Value::Int(1)));
    assert_eq!(
        b.collection("docs").get(b"k").unwrap(),
        Some(Value::Text("other".into()))
    );
    assert_eq!(a.collection("docs").len().unwrap(), 1);
    assert_eq!(b.collection("docs").len().unwrap(), 1);
}

// ===========================================================================
// Backup
// ===========================================================================

/// `Db::backup` writes a point-in-time copy that restores IDENTICAL state:
/// documents, index definitions (queryable after reopening the backup),
/// graph edges, and TTL entries. A backup into a nonexistent parent dir
/// fails with `Error::Database`; a backup onto an existing file is refused
/// with `Error::BackupTargetExists`; an empty db backs up to a valid,
/// openable, empty database.
#[test]
fn lifecycle_db_backup_restores_identical_state_and_pins_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let bak = dir.path().join("backup.db");
    {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..10i64 {
            c.insert(format!("k{i}").as_bytes(), &doc("d", i)).unwrap();
        }
        c.create_scalar_index("n").unwrap();
        c.create_text_index("name").unwrap();
        c.insert_with_ttl(b"exp", &doc("d", 99), 4242).unwrap();
        c.link_weighted(b"k0", "knows", b"k1", 0.5).unwrap();
        db.backup(&bak).unwrap();
    }
    // The backup is a complete, independent database; its indexes were copied
    // as definitions and reload — so they serve queries after reopen.
    let db = Db::open(&bak).unwrap();
    let c = db.collection("docs");
    assert_eq!(c.len().unwrap(), 11);
    let mut keys: Vec<Vec<u8>> = c
        .query()
        .filter(field("n").ge(Value::Int(8)))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    keys.sort();
    // k8, k9 (n = 8, 9) and the TTL doc (n = 99).
    assert_eq!(keys, vec![b"exp".to_vec(), b"k8".to_vec(), b"k9".to_vec()]);
    assert!(!c.text_search("name", "d", 3).unwrap().is_empty());
    assert_eq!(c.ttl(b"exp").unwrap(), Some(4242));
    assert_eq!(c.neighbors(b"k0", "knows").unwrap(), vec![b"k1".to_vec()]);
    assert_eq!(
        c.in_neighbors(b"k1", "knows").unwrap(),
        vec![b"k0".to_vec()]
    );

    // Bad path: the target's parent does not exist.
    let err = db.backup(dir.path().join("no/dir/bak.db"));
    assert!(
        matches!(err, Err(corvid::Error::Database(_))),
        "backup into a nonexistent parent must be Error::Database, got {err:?}"
    );
    // Existing target: refused (a merge would resurrect deleted records).
    let err = db.backup(&bak);
    assert!(
        matches!(&err, Err(corvid::Error::BackupTargetExists(p)) if p.ends_with("backup.db")),
        "backup onto an existing file must be Error::BackupTargetExists, got {err:?}"
    );

    // Empty db → a valid, empty backup.
    let empty_bak = dir.path().join("empty.db");
    Db::open_in_memory().unwrap().backup(&empty_bak).unwrap();
    let empty = Db::open(&empty_bak).unwrap();
    assert!(empty.collections().unwrap().is_empty());
}

// ===========================================================================
// Bulk
// ===========================================================================

/// `Db::bulk` is a DURABILITY scope, not an atomicity scope (pinning the
/// actual contract, which differs from the naive "Err → nothing applied"
/// reading): each write inside commits on its own, only the per-commit fsync
/// is skipped, and the closing flush makes everything durable EVEN WHEN the
/// closure returns Err — the writes performed before the failure persist and
/// the error propagates. Happy path: everything applied and durable across a
/// reopen, with indexes maintained under the load.
///
/// The underlying `BulkScope` is `!Send` by construction (it owns a
/// decrement of the creating thread's thread-local bulk depth), so a scope
/// cannot be moved to another thread — a compile-level guarantee this test
/// documents rather than re-proves at runtime.
#[test]
fn lifecycle_db_bulk_is_a_durability_scope_writes_before_err_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        c.create_scalar_index("n").unwrap();
        let err: corvid::Result<()> = db.bulk(|| {
            c.insert(b"kept1", &doc("x", 1))?;
            c.insert(b"kept2", &doc("x", 2))?;
            Err(corvid::Error::InvalidName("boom".into()))
        });
        assert!(
            matches!(err, Err(corvid::Error::InvalidName(_))),
            "the closure's error propagates, got {err:?}"
        );
        // The two committed writes persist (flush ran despite the error)...
        assert_eq!(c.len().unwrap(), 2);
        // ...and the scalar index was maintained under the relaxed load.
        assert_eq!(
            c.query()
                .filter(field("n").eq(Value::Int(2)))
                .run()
                .unwrap()
                .iter()
                .map(|r| r.key.clone())
                .collect::<Vec<_>>(),
            vec![b"kept2".to_vec()]
        );
    }
    // Durable after reopen.
    let db = Db::open(&path).unwrap();
    assert_eq!(db.collection("docs").len().unwrap(), 2);
    assert_eq!(
        db.collection("docs").get(b"kept1").unwrap(),
        Some(doc("x", 1))
    );
}

/// The bulk happy path: a load of N documents commits once-durable, and the
/// data (plus auto-id continuity) survives a reopen.
#[test]
fn lifecycle_db_bulk_happy_path_applies_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        db.bulk(|| {
            let c = db.collection("docs");
            for i in 0..50i64 {
                c.insert(format!("k{i}").as_bytes(), &doc("x", i))?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(db.collection("docs").len().unwrap(), 50);
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.collection("docs").len().unwrap(), 50);
    assert_eq!(
        db.collection("docs").get(b"k42").unwrap(),
        Some(doc("x", 42))
    );
}

/// `Store::begin_bulk` returns a `!Send` scope guard; scopes NEST (a depth
/// counter, no rejection — pinning the actual behavior) and unwinding or
/// dropping restores durable commits. Writes inside the scope become durable
/// via `Store::flush`.
#[test]
fn lifecycle_store_begin_bulk_scopes_nest_and_flush_makes_writes_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let s = Store::open(&path).unwrap();
        {
            let _outer = s.begin_bulk();
            s.put("docs", b"a", b"1").unwrap();
            {
                // Nested scope: permitted (depth 2), no error to assert.
                let _inner = s.begin_bulk();
                s.put("docs", b"b", b"2").unwrap();
            } // depth back to 1; still inside the outer scope
            s.put("docs", b"c", b"3").unwrap();
            s.flush().unwrap();
        } // outer scope dropped: durable commits restored
        s.put("docs", b"d", b"4").unwrap(); // ordinary durable write
    }
    let s = Store::open(&path).unwrap();
    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")] {
        assert_eq!(
            s.get("docs", k.as_bytes()).unwrap(),
            Some(v.as_bytes().to_vec()),
            "bulk + flushed writes must be durable across reopen"
        );
    }
}

// ===========================================================================
// Compact
// ===========================================================================

/// `Db::compact` (and `Store::compact`) reclaim space after heavy deletes and
/// return whether any data was moved; documents survive compaction intact,
/// a second compaction is harmless, and the file reopens cleanly. The bool's
/// exact value is redb-internal (both outcomes are legal for "did compaction
/// move data"); what is pinned is the Ok(bool) shape and data intactness.
#[test]
fn lifecycle_db_compact_keeps_data_intact_and_tolerates_double_compact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let mut db = Db::open(&path).unwrap();
        {
            let c = db.collection("docs");
            for i in 0..300u32 {
                c.insert(&i.to_le_bytes(), &doc("x", i as i64)).unwrap();
            }
            for i in 0..280u32 {
                c.delete(&i.to_le_bytes()).unwrap();
            }
        }
        let moved_a: bool = db.compact().unwrap();
        let _ = moved_a; // both values legal; shape pinned by the binding
        assert_eq!(db.collection("docs").len().unwrap(), 20);
        assert_eq!(
            db.collection("docs").get(&299u32.to_le_bytes()).unwrap(),
            Some(doc("x", 299))
        );
        let moved_b: bool = db.compact().unwrap();
        let _ = moved_b;
        assert_eq!(db.collection("docs").len().unwrap(), 20);
        assert_eq!(
            db.collection("docs").get(&280u32.to_le_bytes()).unwrap(),
            Some(doc("x", 280))
        );
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.collection("docs").len().unwrap(), 20);
    drop(db); // release the file lock for the byte-store handle below

    // The byte-store layer's own compaction (same contract, `&mut self`).
    let mut s = Store::open(&path).unwrap();
    let _moved: bool = s.compact().unwrap();
    assert_eq!(s.count("docs").unwrap(), 20);
}

// ===========================================================================
// collections — user namespaces only
// ===========================================================================

/// `Db::collections` hides every engine-internal namespace. Task 3 pinned
/// this with a graph edge present; this extends the pin to the TTL, edge
/// (forward AND reverse), index-definition, and on-disk index namespaces,
/// and — through the public `Store` API on the same file — proves those
/// reserved namespaces DO exist in the raw catalog, so the test pins the
/// FILTER, not the absence.
#[test]
fn lifecycle_db_collections_filters_graph_ttl_and_index_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("x", 1)).unwrap();
        db.collection("notes").insert(b"n", &Value::Int(1)).unwrap();
        c.create_scalar_index("n").unwrap();
        c.create_text_index("name").unwrap();
        c.create_vector_index_ondisk("v", Metric::L2).unwrap(); // __dann__ namespace
        c.link(b"a", "knows", b"b").unwrap(); // __edges__ + __redges__
        c.insert_with_ttl(b"exp", &doc("x", 2), 99).unwrap(); // __ttl__

        let mut names = db.collections().unwrap();
        names.sort();
        assert_eq!(names, vec!["docs".to_owned(), "notes".to_owned()]);
    }
    // Raw catalog through the public byte store: the reserved namespaces are
    // all there — __edges__, __redges__, __ttl__, __indexes__, __dann__.
    {
        let store = Store::open(&path).unwrap();
        let raw = store.collections().unwrap();
        for prefix in [
            "__edges__docs",
            "__redges__docs",
            "__ttl__docs",
            "__indexes__",
        ] {
            assert!(
                raw.iter().any(|n| n.starts_with(prefix)),
                "raw catalog must contain {prefix}, got {raw:?}"
            );
        }
        assert!(raw.contains(&"docs".to_owned()) && raw.contains(&"notes".to_owned()));
    }
    // Reopened Db keeps filtering.
    let db = Db::open(&path).unwrap();
    let mut names = db.collections().unwrap();
    names.sort();
    assert_eq!(names, vec!["docs".to_owned(), "notes".to_owned()]);
}

// ===========================================================================
// set_relaxed_durability + flush
// ===========================================================================

/// `Store::set_relaxed_durability` toggles the whole-store opt-in: writes
/// while relaxed are committed (consistent, immediately visible), and a
/// `flush` makes them durable across a reopen; toggling back restores the
/// default and subsequent operations are fine. Observability of the fsync
/// itself is limited without a crash rig — durability-after-flush is the
/// guaranteed contract pinned here.
#[test]
fn lifecycle_store_set_relaxed_durability_and_flush_keep_data_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let s = Store::open(&path).unwrap();
        s.set_relaxed_durability(true);
        s.put("docs", b"relaxed", b"v1").unwrap();
        s.flush().unwrap();
        s.set_relaxed_durability(false);
        s.put("docs", b"strict", b"v2").unwrap();
        s.set_relaxed_durability(true); // re-enable, then back off again
        s.put("docs", b"relaxed2", b"v3").unwrap();
        s.set_relaxed_durability(false);
        s.flush().unwrap();
    }
    let s = Store::open(&path).unwrap();
    for (k, v) in [("relaxed", "v1"), ("strict", "v2"), ("relaxed2", "v3")] {
        assert_eq!(
            s.get("docs", k.as_bytes()).unwrap(),
            Some(v.as_bytes().to_vec())
        );
    }
    // Subsequent operations are fine after all the toggling.
    assert_eq!(s.count("docs").unwrap(), 3);
    assert!(s.delete("docs", b"strict").unwrap());
    assert_eq!(s.count("docs").unwrap(), 2);
}

// ===========================================================================
// Store — transactions, snapshots, and the byte KV surface
// ===========================================================================

/// `Store::transaction` commits every write when the closure returns Ok, and
/// rolls EVERYTHING back (including the collection's creation and count)
/// when it returns Err. The `WriteBatch` sees its own uncommitted writes,
/// including through its paged `scan_from`, and its `next_auto_id`
/// reservation rolls back with the transaction (the in-txn twin of the
/// standalone counter).
#[test]
fn lifecycle_store_transaction_commit_rollback_and_write_batch_surface() {
    let s = Store::open_in_memory().unwrap();

    // Commit: all writes land, in and across collections.
    let n: u64 = s
        .transaction(|tx| {
            tx.put("docs", b"a", b"1")?;
            tx.put("docs", b"b", b"2")?;
            tx.put("notes", b"x", b"y")?;
            // The batch reads its own uncommitted writes...
            assert_eq!(tx.get("docs", b"a").unwrap(), Some(b"1".to_vec()));
            assert_eq!(tx.get("docs", b"missing").unwrap(), None);
            // ...sees them in its scan (key order)...
            assert_eq!(
                tx.scan("docs").unwrap(),
                vec![
                    (b"a".to_vec(), b"1".to_vec()),
                    (b"b".to_vec(), b"2".to_vec())
                ]
            );
            // ...and through its paged window (limit + start semantics).
            assert_eq!(
                tx.scan_from("docs", b"a", 1).unwrap(),
                vec![(b"a".to_vec(), b"1".to_vec())]
            );
            assert_eq!(
                tx.scan_from("docs", b"b", 5).unwrap(),
                vec![(b"b".to_vec(), b"2".to_vec())]
            );
            // Delete inside the batch reports removal and is visible.
            assert!(tx.delete("notes", b"x").unwrap());
            assert_eq!(tx.get("notes", b"x").unwrap(), None);
            // In-transaction auto-id reservation.
            let id = tx.next_auto_id("docs")?;
            assert_eq!(id, 0);
            Ok(id + 1)
        })
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(s.get("docs", b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.get("docs", b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(s.get("notes", b"x").unwrap(), None);
    assert_eq!(s.count("docs").unwrap(), 2);

    // Rollback: nothing from the aborted transaction survives — no rows, no
    // count, no burned auto-id (the reservation rolled back too).
    let err: corvid::Result<()> = s.transaction(|tx| {
        tx.put("docs", b"gone", b"z")?;
        assert_eq!(tx.next_auto_id("docs").unwrap(), 1);
        Err(corvid::Error::InvalidName("boom".into()))
    });
    assert!(matches!(err, Err(corvid::Error::InvalidName(_))));
    assert_eq!(s.get("docs", b"gone").unwrap(), None);
    assert_eq!(s.count("docs").unwrap(), 2);
    assert_eq!(
        s.next_auto_id("docs").unwrap(),
        1,
        "rolled-back reservation must not burn an id"
    );

    // Overwrite inside a transaction keeps the count honest (no double-add).
    s.transaction(|tx| tx.put("docs", b"a", b"1b")).unwrap();
    assert_eq!(s.count("docs").unwrap(), 2);
    assert_eq!(s.get("docs", b"a").unwrap(), Some(b"1b".to_vec()));
}

/// `Store::read` runs its closure against ONE consistent snapshot: a write
/// committed by the store itself MID-CLOSURE (the closure captures `&s`) is
/// invisible to the batch's subsequent reads, while a fresh read afterwards
/// sees it. The `ReadBatch` surface (collections, auto_ids, get, scan,
/// scan_from, scan_prefix, for_each) behaves like the standalone ops.
#[test]
fn lifecycle_store_read_batch_is_one_snapshot_and_mirrors_standalone_ops() {
    let s = Store::open_in_memory().unwrap();
    s.put("docs", b"a1", b"v1").unwrap();
    s.put("docs", b"a2", b"v2").unwrap();
    s.put("docs", b"b1", b"v3").unwrap();
    s.next_auto_id("docs").unwrap();
    s.next_auto_id("docs").unwrap();
    s.put("notes", b"z", b"n").unwrap();

    s.read(|r| {
        // Snapshot isolation: this write commits mid-closure...
        s.put("docs", b"mid", b"written-during-read").unwrap();
        // ...but the same batch never sees it.
        assert_eq!(r.get("docs", b"mid").unwrap(), None);
        assert_eq!(r.scan("docs").unwrap().len(), 3);
        assert_eq!(r.scan_from("docs", b"", 10).unwrap().len(), 3);

        // The full ReadBatch surface.
        assert_eq!(r.get("docs", b"a1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(r.get("ghost", b"k").unwrap(), None);
        assert_eq!(
            r.scan("docs").unwrap(),
            vec![
                (b"a1".to_vec(), b"v1".to_vec()),
                (b"a2".to_vec(), b"v2".to_vec()),
                (b"b1".to_vec(), b"v3".to_vec()),
            ]
        );
        assert_eq!(
            r.scan_prefix("docs", b"a").unwrap(),
            vec![
                (b"a1".to_vec(), b"v1".to_vec()),
                (b"a2".to_vec(), b"v2".to_vec())
            ]
        );
        assert_eq!(
            r.scan_prefix("docs", b"zz").unwrap(),
            Vec::<(Vec<u8>, Vec<u8>)>::new()
        );
        assert_eq!(
            r.collections().unwrap(),
            vec!["docs".to_owned(), "notes".to_owned()]
        );
        assert_eq!(r.auto_ids().unwrap(), vec![("docs".to_owned(), 2)]);
        let mut seen = Vec::new();
        r.for_each("docs", &mut |k, v| {
            seen.push((k.to_vec(), v.to_vec()));
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        // Early stop: false halts the stream after the first pair.
        let mut first = Vec::new();
        r.for_each("docs", &mut |k, v| {
            first.push((k.to_vec(), v.to_vec()));
            Ok(false)
        })
        .unwrap();
        assert_eq!(first, seen[..1]);
        Ok(())
    })
    .unwrap();
    // Outside the closure the mid-read commit is visible.
    assert_eq!(
        s.get("docs", b"mid").unwrap(),
        Some(b"written-during-read".to_vec())
    );
}

/// The standalone byte-KV surface: put/get/delete round-trips, scan in key
/// order, scan_from pagination via the documented resume convention (last
/// key + trailing 0 byte walks the collection exactly once), scan_prefix
/// bounds, the maintained count, for_each streaming with early stop,
/// per-collection monotonic next_auto_id, and the unknown-collection
/// contracts (get → None, scans → empty, for_each → no calls, count → 0,
/// delete → false). The `corvid::Result` alias is the error type throughout.
#[test]
fn lifecycle_store_kv_surface_roundtrips_and_unknown_collection_contracts() {
    let s = Store::open_in_memory().unwrap();

    // put/get/overwrite/delete.
    s.put("docs", b"a", b"alpha").unwrap();
    let got: corvid::Result<Option<Vec<u8>>> = s.get("docs", b"a");
    assert_eq!(got.unwrap(), Some(b"alpha".to_vec()));
    s.put("docs", b"a", b"beta").unwrap();
    assert_eq!(s.get("docs", b"a").unwrap(), Some(b"beta".to_vec()));
    assert!(s.delete("docs", b"a").unwrap());
    assert_eq!(s.get("docs", b"a").unwrap(), None);

    // Ordered scan + prefix bounds + the all-0xFF prefix edge.
    for (k, v) in [("ax", "1"), ("ay", "2"), ("bz", "3")] {
        s.put("docs", k.as_bytes(), v.as_bytes()).unwrap();
    }
    s.put("docs", &[0xff, 0xff], b"hi").unwrap();
    assert_eq!(
        s.scan("docs").unwrap(),
        vec![
            (b"ax".to_vec(), b"1".to_vec()),
            (b"ay".to_vec(), b"2".to_vec()),
            (b"bz".to_vec(), b"3".to_vec()),
            ([0xff, 0xff].to_vec(), b"hi".to_vec()),
        ]
    );
    assert_eq!(s.scan_prefix("docs", b"a").unwrap().len(), 2);
    assert_eq!(
        s.scan_prefix("docs", &[0xff, 0xff]).unwrap(),
        vec![([0xff, 0xff].to_vec(), b"hi".to_vec())]
    );
    assert_eq!(s.scan_prefix("docs", b"").unwrap().len(), 4);
    assert_eq!(s.count("docs").unwrap(), 4);

    // Cursor pagination: pages of 2 with last_key+0x00 resume cover all keys
    // exactly once, in order.
    let mut cursor = Vec::new();
    let mut paged = Vec::new();
    loop {
        let page = s.scan_from("docs", &cursor, 2).unwrap();
        let Some((last, _)) = page.last().cloned() else {
            break;
        };
        paged.extend(page);
        cursor = last;
        cursor.push(0);
    }
    assert_eq!(paged, s.scan("docs").unwrap());
    // Keys >= b"b": "bz" and [0xFF,0xFF] (which sorts after every ASCII key).
    assert_eq!(s.scan_from("docs", b"b", 5).unwrap().len(), 2);

    // for_each full walk + early stop.
    let mut all = Vec::new();
    s.for_each("docs", &mut |k: &[u8], v: &[u8]| {
        all.push((k.to_vec(), v.to_vec()));
        Ok(true)
    })
    .unwrap();
    assert_eq!(all.len(), 4);
    let mut stopped = Vec::new();
    s.for_each("docs", &mut |k: &[u8], v: &[u8]| {
        stopped.push((k.to_vec(), v.to_vec()));
        Ok(stopped.len() < 2)
    })
    .unwrap();
    assert_eq!(stopped, all[..2]);

    // Auto-ids: monotonic per collection, isolated across collections.
    assert_eq!(s.next_auto_id("docs").unwrap(), 0);
    assert_eq!(s.next_auto_id("docs").unwrap(), 1);
    assert_eq!(s.next_auto_id("notes").unwrap(), 0);
    assert_eq!(s.next_auto_id("docs").unwrap(), 2);

    // Unknown-collection contracts.
    assert_eq!(s.get("ghost", b"k").unwrap(), None);
    assert!(s.scan("ghost").unwrap().is_empty());
    assert!(s.scan_from("ghost", b"", 5).unwrap().is_empty());
    assert!(s.scan_prefix("ghost", b"x").unwrap().is_empty());
    assert_eq!(s.count("ghost").unwrap(), 0);
    let mut calls = 0;
    s.for_each("ghost", &mut |_: &[u8], _: &[u8]| {
        calls += 1;
        Ok(true)
    })
    .unwrap();
    assert_eq!(calls, 0);
    assert!(!s.delete("ghost", b"k").unwrap());
}

/// `Store::backup` writes a byte-store copy to a fresh file that reopens as
/// an independent store with identical records, counts, auto-ids, and
/// catalog; backing up onto an existing path is refused with
/// `Error::BackupTargetExists`.
#[test]
fn lifecycle_store_backup_copies_to_an_independent_openable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("src.db");
    let bak = dir.path().join("bak.db");
    {
        let s = Store::open(&path).unwrap();
        s.put("docs", b"a", b"1").unwrap();
        s.put("docs", b"b", b"2").unwrap();
        s.next_auto_id("docs").unwrap();
        s.backup(&bak).unwrap();
    }
    let b = Store::open(&bak).unwrap();
    assert_eq!(b.get("docs", b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(b.scan("docs").unwrap().len(), 2);
    assert_eq!(b.count("docs").unwrap(), 2);
    assert_eq!(b.collections().unwrap(), vec!["docs".to_owned()]);
    assert_eq!(
        b.read(|r| r.auto_ids()).unwrap(),
        vec![("docs".to_owned(), 1)]
    );
    // The source is unchanged, and a second backup onto the same target is
    // refused (redb would merge, resurrecting deleted records).
    let s = Store::open(&path).unwrap();
    assert_eq!(s.count("docs").unwrap(), 2);
    assert!(matches!(
        s.backup(&bak),
        Err(corvid::Error::BackupTargetExists(_))
    ));
}

// ===========================================================================
// dump → load: the everything round-trip
// ===========================================================================

/// Every `Value` variant survives a dump→load cycle BYTES-EXACTLY —
/// including the float payload corners (`NaN`, `+inf`, `-inf`, `-0.0` whose
/// bit patterns differ from `0.0`), i64 extremes, empty and unicode text,
/// binary bytes, empty/nested containers, and vectors. Comparison is on
/// `Value::encode()` output (semantic equality cannot see -0.0 vs 0.0 or
/// NaN payloads).
#[test]
fn lifecycle_dump_load_roundtrips_every_value_variant_bytes_exact() {
    let src = Db::open_in_memory().unwrap();
    let nan = Value::Float(f64::NAN);
    let neg_zero = Value::Float(-0.0f64);
    let docs: Vec<(&str, Value)> = vec![
        ("null", Value::Null),
        ("bool_t", Value::Bool(true)),
        ("bool_f", Value::Bool(false)),
        ("int_min", Value::Int(i64::MIN)),
        ("int_max", Value::Int(i64::MAX)),
        ("int_zero", Value::Int(0)),
        ("float_nan", nan.clone()),
        ("float_pinf", Value::Float(f64::INFINITY)),
        ("float_ninf", Value::Float(f64::NEG_INFINITY)),
        ("float_neg_zero", neg_zero.clone()),
        ("float_zero", Value::Float(0.0)),
        ("float_pi", Value::Float(std::f64::consts::PI)),
        ("text_empty", Value::Text(String::new())),
        ("text_unicode", Value::Text("héllo 世界 🐦‍⬛".into())),
        ("bytes_empty", Value::Bytes(Vec::new())),
        ("bytes_binary", Value::Bytes(vec![0x00, 0xFF, 0x7f, b'\n'])),
        ("array_empty", Value::Array(vec![])),
        (
            "array_mixed",
            Value::Array(vec![Value::Int(1), Value::Text("x".into()), Value::Null]),
        ),
        ("map_empty", Value::Map(BTreeMap::new())),
        (
            "map_nested",
            map(&[
                ("a", Value::Int(1)),
                (
                    "deep",
                    map(&[("inner", Value::Array(vec![Value::Bool(false)]))]),
                ),
                ("nan", nan),
            ]),
        ),
        ("vector_empty", Value::Vector(vec![])),
        (
            "vector",
            Value::Vector(vec![f32::NAN, f32::INFINITY, -0.0f32, 1.5, -2.25]),
        ),
        // Top-level documents of non-map kinds, too.
        ("top_int", Value::Int(-7)),
        ("top_text", Value::Text("top".into())),
    ];
    for (k, v) in &docs {
        src.collection("kinds").insert(k.as_bytes(), v).unwrap();
    }

    let mut buf = Vec::new();
    src.dump(&mut buf).unwrap();
    let dst = Db::open_in_memory().unwrap();
    dst.load(buf.as_slice()).unwrap();
    for (k, v) in &docs {
        let got = dst.collection("kinds").get(k.as_bytes()).unwrap().unwrap();
        assert_eq!(
            got.encode(),
            v.encode(),
            "{k}: encoded bytes must round-trip exactly"
        );
    }
    // -0.0 in particular: same bits after the trip — and DISTINCT from 0.0's
    // bytes (the encode is to_bits-based, so this pins the sign bit survived).
    let neg_zero_rt = dst
        .collection("kinds")
        .get(b"float_neg_zero")
        .unwrap()
        .unwrap();
    assert_eq!(neg_zero_rt.encode(), neg_zero.encode());
    assert_ne!(neg_zero_rt.encode(), Value::Float(0.0).encode());
    assert_eq!(dst.collection("kinds").len().unwrap(), docs.len() as usize);
}

/// The full end-to-end round-trip on one rich database: every index family —
/// vector indexes for EACH Metric × Quantization pair (the dump's metric and
/// quant codec bytes), an on-disk vector index, a PQ vector index, in-memory
/// and on-disk text, scalar, compound, geo — plus a schema with a unique
/// constraint, TTL entries, graph edges in BOTH namespaces, and auto-id
/// counters. Loading into a FRESH database reproduces identical, serviceable
/// results everywhere.
#[test]
fn lifecycle_dump_load_roundtrips_every_index_family_ttl_edges_schema_and_autoids() {
    use corvid::schema::{Field, FieldType, Schema};

    let src = Db::open_in_memory().unwrap();
    let c = src.collection("rich");
    // A fixed geometry corpus: 4 vectors shared by every metric×quant field.
    let vectors = [
        vec![1.0f32, 0.0],
        vec![0.9, 0.1],
        vec![0.0, 1.0],
        vec![-1.0, 0.0],
    ];
    let metrics = [
        ("cos", Metric::Cosine),
        ("dot", Metric::Dot),
        ("l2", Metric::L2),
    ];
    let quants = [
        ("none", Quantization::None),
        ("bin", Quantization::Binary),
        ("sca", Quantization::Scalar),
    ];
    // One doc per corpus vector carrying ALL nine vector fields at once.
    for (i, v) in vectors.iter().enumerate() {
        let mut d = rich_doc("", Vec::new(), i as i64);
        if let Value::Map(m) = &mut d {
            for (mname, _) in metrics {
                for (qname, _) in quants {
                    m.insert(format!("v_{mname}_{qname}"), Value::Vector(v.clone()));
                }
            }
        }
        c.insert(format!("k{i}").as_bytes(), &d).unwrap();
    }
    // Create one quantized in-memory vector index per Metric × Quantization.
    for (mname, metric) in metrics {
        for (qname, quant) in quants {
            c.create_vector_index_quantized(&format!("v_{mname}_{qname}"), metric, quant)
                .unwrap();
        }
    }
    // On-disk and PQ vector indexes (both kinds) on dedicated fields.
    for (i, v) in vectors.iter().enumerate() {
        let mut d = map(&[
            ("n2", Value::Int(i as i64)),
            ("v_disk", Value::Vector(v.clone())),
            (
                "v_pq",
                Value::Vector((0..8).map(|j| j as f32 + i as f32).collect()),
            ),
            (
                "v_ipq",
                Value::Vector((0..8).map(|j| j as f32 + 3.0 * i as f32).collect()),
            ),
        ]);
        // The scalar/text/geo lanes key off these docs too.
        if let Value::Map(m) = &mut d {
            m.insert("n".to_owned(), Value::Int(i as i64));
            m.insert(
                "body".to_owned(),
                Value::Text(format!("item number {i} about disks")),
            );
            m.insert(
                "pos".to_owned(),
                map(&[
                    ("lat", Value::Float(10.0 + i as f64)),
                    ("lon", Value::Float(20.0)),
                ]),
            );
        }
        c.insert(format!("d{i}").as_bytes(), &d).unwrap();
    }
    c.create_vector_index_ondisk("v_disk", Metric::Cosine)
        .unwrap();
    c.create_vector_index_ondisk_pq("v_pq", Metric::L2, 2, 4)
        .unwrap();
    // The in-memory PQ twin (W4 carry-in M1): every vector-index FAMILY —
    // quantized in-memory (Metric × Quantization), on-disk, on-disk PQ,
    // in-memory PQ — must round-trip the dump.
    c.create_vector_index_pq("v_ipq", Metric::L2, 2, 4).unwrap();
    // Text (both storage kinds), scalar, compound, geo.
    c.create_text_index("body").unwrap();
    c.create_text_index_ondisk("n2").unwrap(); // int field: empty postings, def still dumps
    c.create_scalar_index("n").unwrap();
    c.create_compound_index(&["n", "body"]).unwrap();
    c.create_geo_index("pos").unwrap();
    // Schema with required + unique constraints, on conforming docs.
    let schema = Schema::new()
        .field(Field::new("n", FieldType::Int).required())
        .field(Field::new("u", FieldType::Text).unique());
    for (i, v) in vectors.iter().enumerate() {
        let mut d = rich_doc("v_cos_none", v.clone(), i as i64);
        if let Value::Map(m) = &mut d {
            m.insert("u".to_owned(), Value::Text(format!("u{i}")));
        }
        c.insert(format!("u{i}").as_bytes(), &d).unwrap();
    }
    c.set_schema(&schema).unwrap();
    // TTL, edges (both namespaces get content), auto-ids (schema-conforming).
    c.insert_with_ttl(
        b"exp",
        &map(&[("n", Value::Int(99)), ("u", Value::Text("uexp".into()))]),
        424242,
    )
    .unwrap();
    c.link_weighted(b"k0", "knows", b"k1", 0.5).unwrap();
    c.link(b"k1", "knows", b"k2").unwrap();
    c.insert_auto(&map(&[("n", Value::Int(100))])).unwrap();
    c.insert_auto(&map(&[("n", Value::Int(101))])).unwrap();

    // A second user collection must survive too.
    src.collection("other")
        .insert(b"z", &Value::Text("hi".into()))
        .unwrap();

    let mut buf = Vec::new();
    src.dump(&mut buf).unwrap();
    let dst = Db::open_in_memory().unwrap();
    dst.load(buf.as_slice()).unwrap();
    let d = dst.collection("rich");

    // Documents: 4 metric-corpus + 4 disk/pq + 4 schema + 1 ttl + 2 auto.
    assert_eq!(d.len().unwrap(), 15);
    assert_eq!(
        dst.collection("other").get(b"z").unwrap(),
        Some(Value::Text("hi".into()))
    );

    // Scalar index recreated and serving (d2, k2, u2 all carry n = 2).
    assert_eq!(
        d.query()
            .filter(field("n").eq(Value::Int(2)))
            .run()
            .unwrap()
            .iter()
            .map(|r| r.key.clone())
            .collect::<Vec<_>>(),
        vec![b"d2".to_vec(), b"k2".to_vec(), b"u2".to_vec()]
    );
    // Compound index recreated (prefix window serviceable): n <= 1 matches
    // k0,k1,d0,d1,u0,u1 — six docs.
    assert_eq!(
        d.query()
            .filter(field("n").le(Value::Int(1)))
            .run()
            .unwrap()
            .len(),
        6
    );
    // Text indexes recreated (in-memory and on-disk arms).
    assert!(!d.text_search("body", "item", 3).unwrap().is_empty());
    // Geo index recreated and serving (every doc with a pos is near (10,20)).
    assert_eq!(
        d.geo_within_radius("pos", 10.0, 20.0, 500.0).unwrap().len(),
        12
    );
    // TTL restored and honored.
    assert_eq!(d.ttl(b"exp").unwrap(), Some(424242));
    assert_eq!(d.purge_expired(424242).unwrap(), 1);
    assert_eq!(d.get(b"exp").unwrap(), None);

    // Vector indexes: every Metric × Quantization pair serves identically in
    // the source and the loaded db (exact distances, deterministic order).
    for (mname, metric) in metrics {
        for (qname, _) in quants {
            let f = format!("v_{mname}_{qname}");
            let q = [1.0f32, 0.0];
            let from_src = src
                .collection("rich")
                .vector_search(&f, &q, 4, metric)
                .unwrap();
            let from_dst = d.vector_search(&f, &q, 4, metric).unwrap();
            assert_eq!(from_dst.len(), 4, "{f}: all corpus docs carry the field");
            assert_eq!(
                from_src.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
                from_dst.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
                "{f}: loaded ranking must equal source"
            );
            assert_eq!(from_dst[0].distance, from_src[0].distance);
        }
    }
    // On-disk and both PQ vector indexes serve after the round-trip.
    assert!(
        !d.vector_search("v_disk", &[1.0, 0.0], 2, Metric::Cosine)
            .unwrap()
            .is_empty()
    );
    assert!(
        !d.vector_search(
            "v_pq",
            &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
            2,
            Metric::L2
        )
        .unwrap()
        .is_empty()
    );
    // The in-memory PQ index serves too, and ranks identically to the
    // source db (dump vector mode 3 recreated via create_vector_index_pq).
    let ipq_query = [7.0f32, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0, 0.0];
    let ipq_src = src
        .collection("rich")
        .vector_search("v_ipq", &ipq_query, 4, Metric::L2)
        .unwrap();
    let ipq_dst = d.vector_search("v_ipq", &ipq_query, 4, Metric::L2).unwrap();
    assert_eq!(ipq_dst.len(), 4, "all corpus docs carry v_ipq");
    assert_eq!(
        ipq_src.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        ipq_dst.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        "loaded in-memory PQ ranking must equal source"
    );
    assert_eq!(ipq_dst[0].distance, ipq_src[0].distance);

    // Schema restored and enforced: duplicate unique value and missing
    // required field are both rejected with the exact variant.
    let dup = map(&[("n", Value::Int(5)), ("u", Value::Text("u0".into()))]);
    assert!(matches!(
        d.insert(b"dup", &dup),
        Err(corvid::Error::SchemaViolation(_))
    ));
    let missing = map(&[("u", Value::Text("unew".into()))]);
    assert!(matches!(
        d.insert(b"miss", &missing),
        Err(corvid::Error::SchemaViolation(_))
    ));

    // Edges restored in BOTH namespaces, weights intact.
    assert_eq!(d.neighbors(b"k0", "knows").unwrap(), vec![b"k1".to_vec()]);
    assert_eq!(
        d.in_neighbors(b"k2", "knows").unwrap(),
        vec![b"k1".to_vec()]
    );
    assert_eq!(
        d.neighbors_weighted(b"k0", "knows").unwrap(),
        vec![(b"k1".to_vec(), 0.5)]
    );
    // Auto-id counter continues past the dumped ids (no re-issue).
    assert_eq!(
        d.insert_auto(&map(&[("n", Value::Int(102))])).unwrap(),
        b"00000000000000000002".to_vec()
    );
}

/// Loading into a NON-EMPTY database MERGES (pins the actual contract):
/// records are upserted — a dump key overwrites the target's value, distinct
/// target keys survive — and the auto-id counter merges with max (never
/// backwards), so the next auto insert never re-issues a used id.
#[test]
fn lifecycle_dump_load_into_nonempty_db_merges_records_and_counters() {
    let mut buf = Vec::new();
    {
        let src = Db::open_in_memory().unwrap();
        src.collection("docs")
            .insert(b"collide", &Value::Int(99))
            .unwrap();
        src.collection("docs")
            .insert(b"from_dump", &Value::Text("yes".into()))
            .unwrap();
        src.collection("docs").insert_auto(&Value::Int(5)).unwrap();
        src.dump(&mut buf).unwrap();
    }
    let dst = Db::open_in_memory().unwrap();
    {
        let c = dst.collection("docs");
        c.insert(b"collide", &Value::Int(1)).unwrap(); // will be overwritten
        c.insert(b"only_local", &Value::Int(7)).unwrap(); // must survive
        c.insert_auto(&Value::Int(100)).unwrap(); // target counter: 1
    }
    dst.load(buf.as_slice()).unwrap();
    let c = dst.collection("docs");
    assert_eq!(
        c.get(b"collide").unwrap(),
        Some(Value::Int(99)),
        "dump record upserts"
    );
    assert_eq!(
        c.get(b"only_local").unwrap(),
        Some(Value::Int(7)),
        "distinct local key survives"
    );
    assert_eq!(
        c.get(b"from_dump").unwrap(),
        Some(Value::Text("yes".into()))
    );
    assert_eq!(c.len().unwrap(), 4);
    // Counter merged with max(dump=1, local=1) → next id is 1, not 0.
    assert_eq!(
        c.insert_auto(&Value::Int(-1)).unwrap(),
        b"00000000000000000001".to_vec()
    );
}

/// A dump naming an engine-reserved (`__`-prefixed) collection is rejected
/// on load with `Error::InvalidDump` (engine-level pin; corvid-mcp drives the
/// wire form) — replaying it would forge internal state. A truncated or
/// bad-magic stream is likewise `InvalidDump`. (These hand-built bytes are
/// format v1 — magic `CORVIDDUMPv1`, u32 LE length prefixes, u64 LE section
/// counts; the current dumper writes v2, which widens the prefixes to u64,
/// and the loader accepts both.)
#[test]
fn lifecycle_load_rejects_reserved_names_and_malformed_streams() {
    fn record(coll: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"CORVIDDUMPv1");
        b.extend_from_slice(&1u64.to_le_bytes()); // one record
        b.extend_from_slice(&(coll.len() as u32).to_le_bytes());
        b.extend_from_slice(coll.as_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(b'k');
        b.extend_from_slice(&0u32.to_le_bytes()); // empty value = Null
        b
    }
    for reserved in [
        "__edges__docs",
        "__ttl__docs",
        "__indexes__",
        "__dann__docs__v",
    ] {
        let dst = Db::open_in_memory().unwrap();
        let err = dst.load(record(reserved).as_slice());
        assert!(
            matches!(&err, Err(corvid::Error::InvalidDump(msg)) if msg.contains("reserved")),
            "record for reserved collection {reserved} must be InvalidDump, got {err:?}"
        );
        // Nothing was forged: no user data landed.
        assert!(dst.collections().unwrap().is_empty());
    }

    // Bad magic.
    let dst = Db::open_in_memory().unwrap();
    assert!(matches!(
        dst.load(b"not a corvid dump".as_slice()),
        Err(corvid::Error::InvalidDump(_))
    ));
    // Truncated after the magic (record count promises more than the bytes).
    let mut truncated = Vec::new();
    truncated.extend_from_slice(b"CORVIDDUMPv1");
    truncated.extend_from_slice(&2u64.to_le_bytes()); // two records...
    truncated.extend_from_slice(&4u32.to_le_bytes());
    truncated.extend_from_slice(b"docs"); // ...but the first is cut short
    assert!(matches!(
        dst.load(truncated.as_slice()),
        Err(corvid::Error::InvalidDump(_))
    ));
}

/// Dumping an empty database produces a valid (non-empty — magic + section
/// counts) stream that loads into an empty database; and I/O failures on the
/// sinks surface as the exact `Error::Io` variant in both directions.
#[test]
fn lifecycle_dump_of_empty_db_loads_empty_and_io_errors_surface() {
    let mut buf = Vec::new();
    Db::open_in_memory().unwrap().dump(&mut buf).unwrap();
    assert!(
        !buf.is_empty(),
        "an empty dump still carries magic + counts"
    );
    let dst = Db::open_in_memory().unwrap();
    dst.load(buf.as_slice()).unwrap();
    assert!(dst.collections().unwrap().is_empty());

    // A failing Write sink fails the dump with Error::Io.
    struct BrokenWrite;
    impl std::io::Write for BrokenWrite {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let err = Db::open_in_memory().unwrap().dump(BrokenWrite);
    assert!(
        matches!(err, Err(corvid::Error::Io(ref e)) if e.to_string().contains("disk on fire")),
        "a failing sink must surface Error::Io, got {err:?}"
    );

    // A failing Read source fails the load with Error::Io.
    struct BrokenRead;
    impl std::io::Read for BrokenRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("cable unplugged"))
        }
    }
    let err = Db::open_in_memory().unwrap().load(BrokenRead);
    assert!(
        matches!(err, Err(corvid::Error::Io(ref e)) if e.to_string().contains("cable unplugged")),
        "a failing source must surface Error::Io, got {err:?}"
    );
}

// ===========================================================================
// load_with_renames — the `a__b` migration (Task 8)
// ===========================================================================

/// Length-prefix helpers for the hand-built legacy fixture below (the dump
/// container format's v1: u32 LE length prefixes, u64 LE section counts —
/// v2 widens the prefixes to u64, and the loader accepts both, so these
/// bytes stay exactly loadable).
fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}
fn put_bytes(b: &mut Vec<u8>, s: &[u8]) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s);
}
fn put_u32(b: &mut Vec<u8>, n: usize) {
    b.extend_from_slice(&(n as u32).to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, n: u64) {
    b.extend_from_slice(&n.to_le_bytes());
}

/// A hand-built LEGACY dump: a `CORVIDDUMPv1` stream naming a collection
/// the current binary can no longer produce (`a__b` — name validation has
/// rejected interior `__` since audit-remediation wave 4). The dump
/// container format is stable and public, so building the bytes by hand is
/// the fixture strategy: a pre-wave-4 `dump` wrote exactly this shape
/// (`Db::dump` cannot, which is the bug being migrated). Every section
/// carries the legacy name, so the rename must move the documents AND
/// every definition together.
struct LegacyDump {
    buf: Vec<u8>,
}

impl LegacyDump {
    fn new(n_records: u64) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"CORVIDDUMPv1");
        put_u64(&mut buf, n_records);
        LegacyDump { buf }
    }
    fn record(mut self, coll: &str, key: &[u8], doc: &Value) -> Self {
        put_str(&mut self.buf, coll);
        put_bytes(&mut self.buf, key);
        put_bytes(&mut self.buf, &doc.encode());
        self
    }
    fn vector_index(mut self, coll: &str, field: &str) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_str(&mut self.buf, field);
        self.buf.push(2); // metric: L2
        self.buf.push(0); // quant: None
        self.buf.push(0); // mode: in-memory
        self
    }
    fn text_index(mut self, coll: &str, field: &str) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_str(&mut self.buf, field);
        self.buf.push(0); // in-memory
        self
    }
    fn scalar_index(mut self, coll: &str, field: &str) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_str(&mut self.buf, field);
        self
    }
    fn compound_index(mut self, coll: &str, fields: &[&str]) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_u32(&mut self.buf, fields.len());
        for f in fields {
            put_str(&mut self.buf, f);
        }
        self
    }
    fn geo_index(mut self, coll: &str, field: &str) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_str(&mut self.buf, field);
        self
    }
    fn schema(mut self, coll: &str) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_u32(&mut self.buf, 2); // n: Int required, body: Text
        self.buf.push(2);
        self.buf.push(1);
        self.buf.push(0);
        put_str(&mut self.buf, "n");
        self.buf.push(4);
        self.buf.push(0);
        self.buf.push(0);
        put_str(&mut self.buf, "body");
        self
    }
    fn ttl(mut self, coll: &str, key: &[u8], expiry: i64) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_bytes(&mut self.buf, key);
        self.buf.extend_from_slice(&expiry.to_le_bytes());
        self
    }
    fn auto_id(mut self, coll: &str, next: u64) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_u64(&mut self.buf, next);
        self
    }
    fn edge(mut self, coll: &str, rel: &str, from: &[u8], to: &[u8], w: f64) -> Self {
        put_u64(&mut self.buf, 1);
        put_str(&mut self.buf, coll);
        put_str(&mut self.buf, rel);
        put_bytes(&mut self.buf, from);
        put_bytes(&mut self.buf, to);
        self.buf.extend_from_slice(&w.to_bits().to_le_bytes());
        self
    }
    fn done(self) -> Vec<u8> {
        self.buf
    }
}

/// The `a__b` migration recipe, end to end: a legacy dump (every section
/// naming `a__b`) loads through `load_with_renames{a__b → a_b}` with
/// documents, EVERY index family (rebuilt from the renamed documents —
/// serviceable, not just present), the schema, TTLs, graph edges, and the
/// auto-id counter all landing under the new name; the old name is gone.
/// Plain `load` on the same dump fails at index replay with `InvalidName`
/// (the AUDIT row this closes).
#[test]
fn lifecycle_load_with_renames_migrates_a_legacy_pre_wave4_dump() {
    let docs: Vec<Value> = (0..3i64)
        .map(|i| rich_doc("v", vec![i as f32, 1.0], i))
        .collect();
    let dump = LegacyDump::new(3)
        .record("a__b", b"\x00", &docs[0])
        .record("a__b", b"\x01", &docs[1])
        .record("a__b", b"\x02", &docs[2])
        .vector_index("a__b", "v")
        .text_index("a__b", "body")
        .scalar_index("a__b", "n")
        .compound_index("a__b", &["n", "body"])
        .geo_index("a__b", "pos")
        .schema("a__b")
        .ttl("a__b", b"\x02", 424242)
        .auto_id("a__b", 5)
        .edge("a__b", "knows", b"a", b"b", 0.5)
        .done();

    // The unmigrated failure mode, pinned first: plain load rejects the
    // legacy def name at index replay.
    let plain = Db::open_in_memory().unwrap();
    assert!(
        matches!(plain.load(dump.as_slice()), Err(corvid::Error::InvalidName(n)) if n == "a__b"),
        "plain load must fail the legacy dump with InvalidName naming it"
    );

    let mut renames = BTreeMap::new();
    renames.insert("a__b".to_owned(), "a_b".to_owned());
    let dst = Db::open_in_memory().unwrap();
    dst.load_with_renames(dump.as_slice(), &renames).unwrap();

    // The old name is gone; only the target exists.
    assert_eq!(dst.collections().unwrap(), vec!["a_b".to_owned()]);
    let c = dst.collection("a_b");
    assert_eq!(c.len().unwrap(), 3);
    assert_eq!(c.get(b"\x01").unwrap(), Some(docs[1].clone()));

    // Scalar index: recreated AND serviceable under the new name.
    let hits: Vec<_> = c
        .query()
        .filter(field("n").eq(Value::Int(1)))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(hits, vec![b"\x01".to_vec()]);

    // An index def landed and services: the filter resolves through an
    // index window, not a scan. (The compound def's replay is forced by
    // the same load success — an unmapped `a__b` in ANY def section fails
    // the whole load with InvalidName, pinned above; attribution prefers
    // the scalar probe, so the compound window is not separately shown.)
    assert_eq!(
        c.query().filter(field("n").eq(Value::Int(1))).plan_shape(),
        corvid::PlanShape::IndexedWindow { kind: "scalar" }
    );

    // Text index: search finds the corpus.
    assert_eq!(c.text_search("body", "item", 3).unwrap().len(), 3);

    // Vector index: nearest-first under the renamed field.
    let vhits = c
        .query()
        .vector("v", vec![2.0, 1.0], 2, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(vhits.len(), 2);
    assert_eq!(vhits[0].key, b"\x02".to_vec());

    // Geo index: the radius query resolves (docs sit at lat 10..12, lon 20).
    assert_eq!(
        c.geo_within_radius("pos", 11.0, 20.0, 200.0).unwrap().len(),
        3
    );

    // Schema: restored and enforced on new writes under the new name.
    assert!(matches!(
        c.insert(b"new", &map(&[("body", Value::Text("no n".into()))])),
        Err(corvid::Error::SchemaViolation(_))
    ));

    // TTL restored.
    assert_eq!(c.ttl(b"\x02").unwrap(), Some(424242));

    // Edges restored, both directions, weight intact.
    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.in_neighbors(b"b", "knows").unwrap(), vec![b"a".to_vec()]);
    assert_eq!(
        c.neighbors_weighted(b"a", "knows").unwrap(),
        vec![(b"b".to_vec(), 0.5)]
    );

    // The auto-id counter moved with the collection: next id is 5.
    assert_eq!(
        c.insert_auto(&map(&[("n", Value::Int(99))])).unwrap(),
        b"00000000000000000005".to_vec()
    );
}

/// The rename-map error contract: an invalid target fails upfront with that
/// target's `InvalidName` (nothing loaded); two sources sharing a target,
/// and a target colliding with an unmapped dump collection, are
/// `InvalidArgument` (one output keyspace per dump name — merging two
/// collections would silently overwrite documents); a reserved dump name
/// cannot be laundered by a rename; a map entry whose source never occurs
/// in the dump is a documented no-op.
#[test]
fn lifecycle_load_with_renames_error_contract_invalid_target_collisions_and_noops() {
    fn two_records() -> Vec<u8> {
        LegacyDump::new(2)
            .record("a__b", b"x", &Value::Int(1))
            .record("a_b", b"y", &Value::Int(2))
            .done()
    }
    fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    // Invalid target: the offending rename's own InvalidName, before the
    // stream is read (nothing loads).
    let dst = Db::open_in_memory().unwrap();
    let err = dst.load_with_renames(two_records().as_slice(), &map_of(&[("a__b", "x__y")]));
    assert!(
        matches!(&err, Err(corvid::Error::InvalidName(n)) if n == "x__y"),
        "invalid target must be InvalidName naming it, got {err:?}"
    );
    assert!(dst.collections().unwrap().is_empty());

    // Two sources sharing one target: rejected upfront, both named.
    let dst = Db::open_in_memory().unwrap();
    let err = dst.load_with_renames(
        two_records().as_slice(),
        &map_of(&[("a__b", "z"), ("c__d", "z")]),
    );
    assert!(
        matches!(&err, Err(corvid::Error::InvalidArgument(m)) if m.contains("a__b") && m.contains("c__d")),
        "a shared target must be InvalidArgument naming both sources, got {err:?}"
    );
    assert!(dst.collections().unwrap().is_empty());

    // A target colliding with an UNMAPPED dump collection is the same
    // keyspace merge, caught mid-stream (records already replayed stay —
    // the streaming posture of every mid-dump failure).
    let err = Db::open_in_memory()
        .unwrap()
        .load_with_renames(two_records().as_slice(), &map_of(&[("a__b", "a_b")]));
    assert!(
        matches!(&err, Err(corvid::Error::InvalidArgument(m)) if m.contains("a_b")),
        "a dump-vs-map collision must be InvalidArgument, got {err:?}"
    );

    // A reserved dump name is rejected before mapping — no laundering.
    let reserved = LegacyDump::new(1)
        .record("__edges__docs", b"k", &Value::Int(1))
        .done();
    let err = Db::open_in_memory()
        .unwrap()
        .load_with_renames(reserved.as_slice(), &map_of(&[("__edges__docs", "ok")]));
    assert!(
        matches!(&err, Err(corvid::Error::InvalidDump(m)) if m.contains("reserved")),
        "a rename must not launder a reserved dump name, got {err:?}"
    );

    // A map entry whose source never occurs in the dump is a no-op: the
    // dump loads unchanged under its own names.
    let mut dump = two_records();
    put_u64(&mut dump, 0); // vectors
    put_u64(&mut dump, 0); // texts
    put_u64(&mut dump, 0); // scalars
    put_u64(&mut dump, 0); // compounds
    put_u64(&mut dump, 0); // geos
    put_u64(&mut dump, 0); // schemas
    put_u64(&mut dump, 0); // ttls
    put_u64(&mut dump, 0); // autos
    put_u64(&mut dump, 0); // edges
    let dst = Db::open_in_memory().unwrap();
    dst.load_with_renames(dump.as_slice(), &map_of(&[("ghost__old", "zz")]))
        .unwrap();
    assert_eq!(
        dst.collections().unwrap(),
        vec!["a__b".to_owned(), "a_b".to_owned()]
    );
}

/// Task 8 review minor (a), the compound-def rename pin made DIRECTLY
/// observable: a legacy dump's compound index def is rebuilt under the
/// renamed collection and SERVES — `plan_shape` attributes
/// `IndexedWindow{kind:"compound"}`. The corpus is shaped for attribution:
/// `plan_shape` prefers any serviceable scalar window (the twin checks the
/// scalar probe first, mirroring `keep_smaller`'s probe order), so the
/// fixture carries ONLY the compound def; selectivity-wise the same corpus
/// makes a scalar `n` window unselective (n repeats across half the docs)
/// where the compound full-equality tuple is exact — with a scalar def
/// present the compound candidate set would win `keep_smaller` at runtime
/// while attribution still said "scalar", which is why the def is absent.
/// The query constrains BOTH compound fields with Eq — a full-equality
/// prefix, serviceable regardless of `all_docs_indexed`.
#[test]
fn lifecycle_load_with_renames_compound_index_serves_under_the_new_name() {
    // 6 docs: n alternates 0/1 (unselective alone), body is unique per doc.
    let mut dump = LegacyDump::new(6).buf;
    for i in 0..6i64 {
        let doc = map(&[
            ("n", Value::Int(i % 2)),
            ("body", Value::Text(format!("item {i}"))),
        ]);
        put_str(&mut dump, "a__b");
        put_bytes(&mut dump, &[i as u8]);
        put_bytes(&mut dump, &doc.encode());
    }
    put_u64(&mut dump, 0); // vectors
    put_u64(&mut dump, 0); // texts
    put_u64(&mut dump, 0); // scalars — deliberately none (see doc comment)
    put_u64(&mut dump, 1); // one compound def on (n, body)
    put_str(&mut dump, "a__b");
    put_u32(&mut dump, 2);
    put_str(&mut dump, "n");
    put_str(&mut dump, "body");
    put_u64(&mut dump, 0); // geos
    put_u64(&mut dump, 0); // schemas
    put_u64(&mut dump, 0); // ttls
    put_u64(&mut dump, 0); // autos
    put_u64(&mut dump, 0); // edges

    let mut renames = BTreeMap::new();
    renames.insert("a__b".to_owned(), "a_b".to_owned());
    let dst = Db::open_in_memory().unwrap();
    dst.load_with_renames(dump.as_slice(), &renames).unwrap();
    let c = dst.collection("a_b");
    assert_eq!(c.len().unwrap(), 6);

    // The renamed compound def is the query's index window — attributed
    // "compound", not a scan and not the (absent) scalar probe.
    assert_eq!(
        c.query()
            .filter(field("n").eq(Value::Int(1)))
            .filter(field("body").eq(Value::Text("item 3".into())))
            .plan_shape(),
        corvid::PlanShape::IndexedWindow { kind: "compound" }
    );

    // ...and it serves the exact document through the renamed index.
    let hits: Vec<_> = c
        .query()
        .filter(field("n").eq(Value::Int(1)))
        .filter(field("body").eq(Value::Text("item 3".into())))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(hits, vec![b"\x03".to_vec()]);
}

/// Task 8 review minor (b): `load_with_renames` into a NON-empty database
/// MERGES with pre-existing collections by design (same contract as
/// `load`) — including a rename target that already exists in the
/// destination. The collision guard constrains intra-dump consistency only
/// (two dump names into one output keyspace); a dump name landing on a
/// pre-existing collection is the documented merge, not an error.
#[test]
fn lifecycle_load_with_renames_into_non_empty_db_merges_with_existing_collections() {
    let mut dump = LegacyDump::new(1).buf;
    put_str(&mut dump, "a__b");
    put_bytes(&mut dump, b"d1");
    put_bytes(&mut dump, &Value::Int(1).encode());
    for _ in 0..9 {
        put_u64(&mut dump, 0); // vectors…edges all empty
    }

    // Destination already holds an unrelated collection AND a collection
    // named exactly the rename target.
    let dst = Db::open_in_memory().unwrap();
    dst.collection("keep")
        .insert(b"k1", &Value::Int(42))
        .unwrap();
    dst.collection("a_b")
        .insert(b"local", &Value::Text("mine".into()))
        .unwrap();

    let mut renames = BTreeMap::new();
    renames.insert("a__b".to_owned(), "a_b".to_owned());
    dst.load_with_renames(dump.as_slice(), &renames).unwrap();

    // The unrelated collection is untouched.
    assert_eq!(
        dst.collection("keep").get(b"k1").unwrap(),
        Some(Value::Int(42))
    );
    // The target collection now holds BOTH its local document and the
    // dump's record — merged, not rejected, not overwritten.
    let c = dst.collection("a_b");
    assert_eq!(c.get(b"local").unwrap(), Some(Value::Text("mine".into())));
    assert_eq!(c.get(b"d1").unwrap(), Some(Value::Int(1)));
    assert_eq!(c.len().unwrap(), 2);
    assert_eq!(
        dst.collections().unwrap(),
        vec!["a_b".to_owned(), "keep".to_owned()]
    );
}

// ===========================================================================
// Error::CorruptIndex — corrupted on-disk index bytes (Task 11 routing)
// ===========================================================================

/// The scalar-index order walk's corruption twin: a hand-corrupted KEY (no
/// value terminator — unreachable via the encoder, exactly what bit-rot
/// produces) in a persisted scalar index namespace makes the `order_by`
/// walk error `Error::CorruptIndex` naming the namespace, never a silently
/// degraded or short result.
#[test]
fn lifecycle_corrupt_scalar_index_key_errors_order_walk_with_exact_variant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        for (k, n) in [(b"a", 3i64), (b"b", 1), (b"c", 2)] {
            c.insert(k, &doc(std::str::from_utf8(k).unwrap(), n))
                .unwrap();
        }
        c.create_scalar_index("n").unwrap();
        // Serving correctly before corruption.
        let rows = c.query().order_by("n", false).run().unwrap();
        assert_eq!(rows.len(), 3);
    }
    // Corrupt the persisted index: insert a malformed key row (numeric-lane
    // byte 0x02 + payload with NO 0x00 0x00 terminator) into the raw
    // namespace through the public byte-store API.
    let ns = {
        let store = Store::open(&path).unwrap();
        let ns = store
            .collections()
            .unwrap()
            .into_iter()
            .find(|n| n.starts_with("__scalar__docs__n"))
            .expect("the scalar index namespace must exist in the raw catalog");
        store.put(&ns, &[0x02, 0x05, 0x01], &[]).unwrap();
        ns
    };
    let db = Db::open(&path).unwrap();
    let err = db.collection("docs").query().order_by("n", false).run();
    match &err {
        Err(corvid::Error::CorruptIndex { context }) => {
            assert!(
                context.contains("__scalar__docs__n") && context.contains(&ns),
                "the error must name the corrupt namespace {ns}, got {context:?}"
            );
        }
        other => panic!("corrupt index key must error CorruptIndex, got {other:?}"),
    }
    // The documents themselves are untouched — a filtered query still works
    // (the window scan tolerates rows without a doc key), so the failure is
    // attributable to the order walk alone.
    assert_eq!(
        db.collection("docs")
            .query()
            .filter(field("n").ge(Value::Int(1)))
            .run()
            .unwrap()
            .len(),
        3
    );
    drop(db);
    // Removing the single corrupt row through the raw store restores service.
    {
        let store = Store::open(&path).unwrap();
        store.delete(&ns, &[0x02, 0x05, 0x01]).unwrap();
    }
    let db = Db::open(&path).unwrap();
    let rows = db
        .collection("docs")
        .query()
        .order_by("n", false)
        .run()
        .unwrap();
    assert_eq!(rows.len(), 3);
}

/// `Error::CorruptIndex` driven end-to-end from a REAL database file: build
/// an on-disk vector index, close, corrupt the index bytes on disk (the
/// reserved `__dann__*` namespace's row values are garbled through the
/// public byte-store API — keys untouched, exactly what bit-rot looks like),
/// reopen, and query: the exact `Error::CorruptIndex` variant (naming the
/// namespace), never a silently degraded or empty result.
#[test]
fn lifecycle_corrupt_ondisk_index_bytes_on_disk_error_queries_with_exact_variant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &vecdoc("v", vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &vecdoc("v", vec![0.0, 1.0])).unwrap();
        c.create_vector_index_ondisk("v", Metric::L2).unwrap();
        // Serving correctly before corruption.
        let hits = c.vector_search("v", &[1.0, 0.0], 1, Metric::L2).unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }
    // Corrupt the persisted index bytes: locate the on-disk ANN namespace in
    // the raw catalog and overwrite every row's VALUE with garbage.
    let ns = {
        let store = Store::open(&path).unwrap();
        let ns = store
            .collections()
            .unwrap()
            .into_iter()
            .find(|n| n.starts_with("__dann__"))
            .expect("the on-disk ANN namespace must exist in the raw catalog");
        let rows = store.scan(&ns).unwrap();
        assert!(!rows.is_empty());
        for (k, _) in &rows {
            store.put(&ns, k, &[0xFF, 0xFF, 0xFF]).unwrap();
        }
        ns
    };
    let db = Db::open(&path).unwrap();
    let err = db
        .collection("docs")
        .vector_search("v", &[1.0, 0.0], 1, Metric::L2);
    match &err {
        Err(corvid::Error::CorruptIndex { context }) => {
            assert!(
                context.contains("__dann__") && context.contains(&ns),
                "the error must name the corrupt namespace {ns}, got {context:?}"
            );
        }
        other => panic!("corrupt index bytes must error CorruptIndex, got {other:?}"),
    }
    // The documents themselves are untouched — a plain get still works, so
    // the failure is attributable to the index, and rebuilding it (re-create
    // replaces the namespace wholesale) restores service.
    assert_eq!(
        db.collection("docs").get(b"a").unwrap(),
        Some(vecdoc("v", vec![1.0, 0.0]))
    );
    db.collection("docs")
        .create_vector_index_ondisk("v", Metric::L2)
        .unwrap();
    let hits = db
        .collection("docs")
        .vector_search("v", &[1.0, 0.0], 1, Metric::L2)
        .unwrap();
    assert_eq!(hits[0].key, b"a".to_vec());
}

// ===========================================================================
// SemanticCache
// ===========================================================================

/// The semantic cache's similarity contract with known vectors under
/// cosine: the nearest entry within `threshold` wins (distance is exact
/// cosine distance in [0, 2]); a query whose nearest entry is beyond the
/// threshold is a miss; an empty cache is a miss; the nearest of MULTIPLE
/// entries is chosen; overwriting an entry updates the served value; and an
/// entry lacking the value field is a miss.
#[test]
fn lifecycle_semantic_cache_threshold_and_nearest_entry_semantics_cosine() {
    let db = Db::open_in_memory().unwrap();
    let cache = db
        .collection("cache")
        .semantic_cache("q", "response", Metric::Cosine, 0.05);
    cache
        .put(b"a", vec![1.0, 0.0], Value::Text("answer-a".into()))
        .unwrap();
    cache
        .put(b"b", vec![0.0, 1.0], Value::Text("answer-b".into()))
        .unwrap();

    // Exact hit and a near hit well inside the threshold.
    assert_eq!(
        cache.get(&[1.0, 0.0]).unwrap(),
        Some(Value::Text("answer-a".into()))
    );
    assert_eq!(
        cache.get(&[0.999, 0.01]).unwrap(),
        Some(Value::Text("answer-a".into()))
    );
    // The nearest of multiple entries wins: query rotated toward b.
    assert_eq!(
        cache.get(&[0.01, 0.999]).unwrap(),
        Some(Value::Text("answer-b".into()))
    );
    // Far queries miss: orthogonal → cosine distance 1.0 ≫ 0.05.
    assert_eq!(cache.get(&[0.0, -1.0]).unwrap(), None);
    // A tight threshold turns a near hit into a miss: [0.98, 0.196] sits
    // ~0.02 from [1,0] (well inside 0.05, far outside 0.005).
    let tight = db
        .collection("cache")
        .semantic_cache("q", "response", Metric::Cosine, 0.005);
    assert_eq!(tight.get(&[0.98, 0.196]).unwrap(), None);
    assert_eq!(
        cache.get(&[0.98, 0.196]).unwrap(),
        Some(Value::Text("answer-a".into()))
    );

    // Overwrite updates the served value.
    cache
        .put(b"a", vec![1.0, 0.0], Value::Text("answer-a2".into()))
        .unwrap();
    assert_eq!(
        cache.get(&[1.0, 0.0]).unwrap(),
        Some(Value::Text("answer-a2".into()))
    );

    // An entry lacking the value field is a miss (embedding-only doc).
    db.collection("cache")
        .insert(b"c", &map(&[("q", Value::Vector(vec![1.0, 1.0]))]))
        .unwrap();
    assert_eq!(cache.get(&[1.0, 1.0]).unwrap(), None);

    // Empty cache is a miss.
    let empty = db
        .collection("other")
        .semantic_cache("q", "response", Metric::Cosine, 1.0);
    assert_eq!(empty.get(&[1.0, 0.0]).unwrap(), None);
}

/// The same contract under L2 (thresholds are in the chosen metric's units —
/// and the L2 metric is SQUARED Euclidean): nearest-by-distance wins; a
/// squared distance just past the threshold misses while one inside hits.
#[test]
fn lifecycle_semantic_cache_threshold_units_follow_the_metric_l2() {
    let db = Db::open_in_memory().unwrap();
    let cache = db
        .collection("cache")
        .semantic_cache("q", "response", Metric::L2, 2.0);
    cache.put(b"origin", vec![0.0, 0.0], Value::Int(0)).unwrap();
    cache.put(b"far", vec![10.0, 0.0], Value::Int(10)).unwrap();
    // Query at [1,0]: squared L2 distance 1 to origin (inside 2.0) vs 81 to
    // far — nearest wins.
    assert_eq!(cache.get(&[1.0, 0.0]).unwrap(), Some(Value::Int(0)));
    // Query at [3,0]: squared distance 9 to origin > 2.0 → miss.
    assert_eq!(cache.get(&[3.0, 0.0]).unwrap(), None);
    // Widening the threshold past the squared distance turns it into a hit.
    let wide = db
        .collection("cache")
        .semantic_cache("q", "response", Metric::L2, 9.5);
    assert_eq!(wide.get(&[3.0, 0.0]).unwrap(), Some(Value::Int(0)));
}

// ===========================================================================
// HyperLogLog
// ===========================================================================

/// HyperLogLog: `new` starts at zero; small cardinalities are exact under
/// linear counting (deterministic DefaultHasher); duplicates are ignored;
/// `with_precision` clamps its exponent (p < 4 and p > 16 both yield working
/// sketches — the clamp is the validation, no error path exists); a larger
/// corpus stays within the documented error bound. There is no union
/// operation in the public surface (pinning the surface: merge is the
/// caller's job).
///
/// Bound fragility: `add_bytes` hashes through std's `DefaultHasher`
/// (SipHash-1-3 with fixed keys today). A std upgrade that swaps
/// `DefaultHasher` changes which buckets each string lands in, which can
/// move an estimate across a window edge even though the sketch math is
/// unchanged. The asserted windows are sized to the documented HLL error
/// for these corpus sizes under any reasonable hasher — they are safe
/// per-toolchain, not hasher-independent: if a std change ever moves an
/// estimate out, re-derive the window for the new toolchain rather than
/// widening it blindly.
#[test]
fn lifecycle_hyperloglog_precision_clamps_estimates_and_ignores_duplicates() {
    assert_eq!(HyperLogLog::new().estimate(), 0);

    let mut small = HyperLogLog::new();
    for i in 0..50u32 {
        small.add_bytes(format!("v{i}").as_bytes());
    }
    let est = small.estimate();
    assert!((48..=52).contains(&est), "50 distinct → ~50, got {est}");

    let mut dupes = HyperLogLog::new();
    for _ in 0..1000 {
        dupes.add_bytes(b"same");
    }
    assert_eq!(dupes.estimate(), 1);

    // Clamped precisions still work: p=2 clamps to 4 (coarse but sane),
    // p=99 clamps to 16 (fine). Bounds chosen per the 1.04/sqrt(2^p) error.
    let mut coarse = HyperLogLog::with_precision(2);
    for i in 0..10_000u32 {
        coarse.add_bytes(format!("item-{i}").as_bytes());
    }
    let coarse_est = coarse.estimate() as f64;
    assert!(
        (coarse_est - 10_000.0).abs() / 10_000.0 < 0.30,
        "clamped-to-p4 error too high: {coarse_est}"
    );
    let mut fine = HyperLogLog::with_precision(99);
    for i in 0..10_000u32 {
        fine.add_bytes(format!("item-{i}").as_bytes());
    }
    let fine_est = fine.estimate() as f64;
    assert!(
        (fine_est - 10_000.0).abs() / 10_000.0 < 0.03,
        "clamped-to-p16 error too high: {fine_est}"
    );
    let mut def = HyperLogLog::new(); // default p = 14, same corpus
    for i in 0..10_000u32 {
        def.add_bytes(format!("item-{i}").as_bytes());
    }
    let def_est = def.estimate() as f64;
    assert!(
        (def_est - 10_000.0).abs() / 10_000.0 < 0.03,
        "default error too high: {def_est}"
    );
}

/// `add_hash` is the precomputed-hash twin of `add_bytes`: feeding the hash
/// the byte path would compute (same std DefaultHasher, deterministic within
/// a binary) is a duplicate — the estimate does not move — while a distinct
/// hash is a new observation.
#[test]
fn lifecycle_hyperloglog_add_hash_is_the_precomputed_twin_of_add_bytes() {
    fn std_hash(bytes: &[u8]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    }
    let mut hll = HyperLogLog::new();
    hll.add_bytes(b"x");
    assert_eq!(hll.estimate(), 1);
    // The same item via its precomputed hash: a duplicate.
    hll.add_hash(std_hash(b"x"));
    assert_eq!(hll.estimate(), 1);
    // Distinct hashes are distinct observations.
    hll.add_hash(0xDEAD_BEEF);
    hll.add_hash(0x0000_0001);
    let est = hll.estimate();
    assert!((2..=3).contains(&est), "3 distinct total → ~3, got {est}");
}

// ===========================================================================
// BloomFilter
// ===========================================================================

/// BloomFilter: no false negatives ever (every added item is found);
/// the false-positive rate on absent items honors a generous bound around
/// the configured rate; out-of-range fp rates (0.0, ≥ 0.5) and
/// `expected_items = 0` are clamped, not rejected — the filter still works
/// and never loses an added item.
#[test]
fn lifecycle_bloom_filter_no_false_negatives_and_bounded_fp_rate() {
    // Nominal configuration.
    let mut bloom = BloomFilter::new(1000, 0.01);
    for i in 0..1000u32 {
        bloom.add_bytes(format!("k{i}").as_bytes());
    }
    for i in 0..1000u32 {
        assert!(
            bloom.contains_bytes(format!("k{i}").as_bytes()),
            "no false negatives: k{i} was added"
        );
    }
    let mut fp = 0;
    const TRIALS: u32 = 10_000;
    for i in 0..TRIALS {
        if bloom.contains_bytes(format!("absent-{i}").as_bytes()) {
            fp += 1;
        }
    }
    let rate = fp as f64 / TRIALS as f64;
    assert!(
        rate < 0.05,
        "configured 1% must stay under 5% observed, got {rate}"
    );

    // Out-of-range fp (>= 0.5): clamped to 0.5 — still functional, still no
    // false negatives, FP rate merely coarser.
    let mut loose = BloomFilter::new(1000, 0.99);
    for i in 0..1000u32 {
        loose.add_bytes(format!("k{i}").as_bytes());
    }
    for i in 0..1000u32 {
        assert!(loose.contains_bytes(format!("k{i}").as_bytes()));
    }
    let mut loose_fp = 0;
    for i in 0..TRIALS {
        if loose.contains_bytes(format!("absent-{i}").as_bytes()) {
            loose_fp += 1;
        }
    }
    let loose_rate = loose_fp as f64 / TRIALS as f64;
    assert!(
        loose_rate < 0.75,
        "fp clamped to 0.5 must stay well under 1, got {loose_rate}"
    );

    // fp = 0.0 clamps to the minimum positive → oversized, very exact filter.
    let mut exact = BloomFilter::new(100, 0.0);
    for i in 0..100u32 {
        exact.add_bytes(format!("k{i}").as_bytes());
    }
    for i in 0..100u32 {
        assert!(exact.contains_bytes(format!("k{i}").as_bytes()));
    }
    let mut exact_fp = 0;
    for i in 0..TRIALS {
        if exact.contains_bytes(format!("absent-{i}").as_bytes()) {
            exact_fp += 1;
        }
    }
    assert!(
        (exact_fp as f64 / TRIALS as f64) < 0.05,
        "fp=0 must clamp to a near-exact filter"
    );

    // expected_items = 0 clamps to 1: a tiny but working filter.
    let mut tiny = BloomFilter::new(0, 0.01);
    tiny.add_bytes(b"only");
    assert!(tiny.contains_bytes(b"only"));
    assert!(!tiny.contains_bytes(b"absent"));
}

// ===========================================================================
// PlanCache / QueryPlan
// ===========================================================================

/// `QueryBuilder::plan` produces a canonical, hashable identity:
/// identically-shaped builders produce EQUAL plans with EQUAL `key()`
/// strings; any shape difference (filter value, limit, vector params,
/// metric) produces a different key.
#[test]
fn lifecycle_query_plan_key_is_canonical_for_identical_shapes() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    let a = c
        .query()
        .filter(field("n").ge(Value::Int(2)))
        .vector("v", vec![1.0, 0.0], 5, Metric::L2)
        .limit(3)
        .plan();
    let b = c
        .query()
        .filter(field("n").ge(Value::Int(2)))
        .vector("v", vec![1.0, 0.0], 5, Metric::L2)
        .limit(3)
        .plan();
    assert_eq!(a, b);
    assert_eq!(a.key(), b.key(), "identical shapes share one canonical key");
    assert!(!a.key().is_empty());
    // Each shape axis changes the key.
    let base = c
        .query()
        .filter(field("n").ge(Value::Int(2)))
        .limit(3)
        .plan();
    assert_ne!(base.key(), a.key()); // vector source added
    assert_ne!(
        base.key(),
        c.query()
            .filter(field("n").ge(Value::Int(3)))
            .limit(3)
            .plan()
            .key()
    );
    assert_ne!(
        base.key(),
        c.query()
            .filter(field("n").ge(Value::Int(2)))
            .limit(4)
            .plan()
            .key()
    );
    assert_ne!(
        c.query()
            .vector("v", vec![1.0, 0.0], 5, Metric::L2)
            .plan()
            .key(),
        c.query()
            .vector("v", vec![0.0, 1.0], 5, Metric::L2)
            .plan()
            .key()
    );
}

/// `PlanCache`: new is empty; `get` misses then hits after `insert`;
/// `insert` REPLACES an existing entry (len stays 1);
/// `get_or_insert_with` invokes its closure exactly once per plan (the
/// second lookup reuses the cached value); `len` counts distinct plans and
/// `is_empty` tracks it.
#[test]
fn lifecycle_plan_cache_miss_hit_insert_replace_and_closure_runs_once() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    let mut cache = corvid::PlanCache::new();
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);

    let p1 = c.query().filter(field("n").ge(Value::Int(2))).plan();
    let p2 = c.query().filter(field("n").ge(Value::Int(3))).plan();
    assert_eq!(cache.get(&p1), None, "miss on a fresh cache");

    cache.insert(p1.clone(), 42);
    assert_eq!(cache.get(&p1), Some(&42));
    assert_eq!(cache.len(), 1);
    assert!(!cache.is_empty());

    // Insert replaces, len unchanged.
    cache.insert(p1.clone(), 43);
    assert_eq!(cache.get(&p1), Some(&43));
    assert_eq!(cache.len(), 1);

    // Distinct plans coexist; a differently-shaped builder still misses.
    cache.insert(p2.clone(), 7);
    assert_eq!(cache.get(&p2), Some(&7));
    assert_eq!(cache.len(), 2);
    assert_eq!(
        cache.get(&c.query().filter(field("n").ge(Value::Int(9))).plan()),
        None
    );

    // get_or_insert_with: the closure runs once per plan; the cached value is
    // returned on both the first and second lookups (fresh plan p3).
    let p3 = c.query().filter(field("n").ge(Value::Int(5))).plan();
    let mut calls = 0;
    let v = *cache.get_or_insert_with(p3.clone(), || {
        calls += 1;
        100
    });
    assert_eq!(v, 100);
    assert_eq!(calls, 1);
    let v2 = *cache.get_or_insert_with(p3, || {
        calls += 1;
        999 // must not run
    });
    assert_eq!(v2, 100, "the replacement closure must not run on a hit");
    assert_eq!(calls, 1);
    assert_eq!(cache.len(), 3);
}

// ===========================================================================
// MAX_NESTING — decode bounds (Lifecycle rows for value::MAX_NESTING /
// Value::encode / Value::decode)
// ===========================================================================

/// `Value::encode` accepts arbitrarily deep containers, but `Value::decode`
/// enforces `MAX_NESTING` (128): a 128-deep encoding decodes (the boundary
/// is inclusive), a 129-deep one is rejected with `Error::Decode` — and a
/// too-deep value stored as raw bytes under a collection key (through the
/// public byte store, on a real file) surfaces the same decode error from
/// `Collection::get`.
#[test]
fn lifecycle_value_decode_enforces_max_nesting_bound() {
    fn nested(depth: usize) -> Value {
        let mut v = Value::Int(0);
        for _ in 0..depth {
            v = Value::Array(vec![v]);
        }
        v
    }
    // 128 deep: decodes (the boundary is inclusive).
    let ok = nested(corvid::value::MAX_NESTING);
    assert_eq!(Value::decode(&ok.encode()).unwrap(), ok);
    // 129 deep: rejected with the exact variant.
    let too_deep = nested(corvid::value::MAX_NESTING + 1);
    assert!(matches!(
        Value::decode(&too_deep.encode()),
        Err(corvid::Error::Decode(_))
    ));
    // Through storage: write the too-deep bytes as a raw record under a
    // collection key via the public byte store, reopen as a Db — the
    // document get fails at decode, loudly.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        db.collection("docs")
            .insert(b"fine", &Value::Int(1))
            .unwrap();
    }
    Store::open(&path)
        .unwrap()
        .put("docs", b"deep", &too_deep.encode())
        .unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.collection("docs").get(b"fine").unwrap(),
        Some(Value::Int(1))
    );
    assert!(matches!(
        db.collection("docs").get(b"deep"),
        Err(corvid::Error::Decode(_))
    ));
}
