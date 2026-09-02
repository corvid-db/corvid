//! The backend storage seam (OPFS program T2a; the contract is
//! corvid-js docs/OPFS-SPEC.md §2): `open_with_backend` /
//! `backup_with_backend` at both the `Store` and `Db` level, proven
//! against `InMemoryBackend` shared through an `Arc` and two
//! instrumented test backends. This is the seam the browser binding's
//! OPFS storage backend is built on — every guarantee it relies on
//! (custom backends genuinely dispatch, durable commits and
//! `Store::flush` reach `sync_data`, backend `close` fires exactly
//! once per open, a failing backend surfaces as a clean engine error)
//! is pinned here, engine-side, where it runs on every CI leg — not
//! only in the browser conformance suite.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use corvid::schema::{Field, FieldType, Schema};
use corvid::{Db, Store, Value};
use redb::StorageBackend;
use redb::backends::InMemoryBackend;

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = std::collections::BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Map(m)
}

/// An `InMemoryBackend` shared through an `Arc`: the backend value
/// itself moves into redb's `Database` (and drops with it), but the
/// data lives as long as the test's `Arc` — which is exactly what
/// makes drop-and-reopen observable without a real file, the way the
/// browser binding will observe it across Worker lifetimes.
#[derive(Clone, Debug)]
struct SharedBackend(Arc<InMemoryBackend>);

impl SharedBackend {
    fn pair() -> (Self, Self) {
        let arc = Arc::new(InMemoryBackend::new());
        (Self(arc.clone()), Self(arc))
    }
}

impl StorageBackend for SharedBackend {
    fn len(&self) -> io::Result<u64> {
        self.0.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.0.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write(offset, data)
    }
}

/// Per-call counters held OUTSIDE the backend value that moves into
/// redb, so the test can read them after the `Database` drops.
#[derive(Debug, Default)]
struct Counts {
    len_calls: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    set_lens: AtomicU64,
    syncs: AtomicU64,
    closes: AtomicU64,
}

/// A delegating backend that counts every trait call — the proof that
/// custom backends genuinely dispatch (the plan's core assumption).
#[derive(Debug)]
struct CountingBackend {
    inner: Arc<InMemoryBackend>,
    counts: Arc<Counts>,
}

impl CountingBackend {
    fn new() -> (Self, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        (
            Self {
                inner: Arc::new(InMemoryBackend::new()),
                counts: counts.clone(),
            },
            counts,
        )
    }
}

impl StorageBackend for CountingBackend {
    fn len(&self) -> io::Result<u64> {
        self.counts.len_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.counts.set_lens.fetch_add(1, Ordering::SeqCst);
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.counts.syncs.fetch_add(1, Ordering::SeqCst);
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.counts.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write(offset, data)
    }
    fn close(&self) -> io::Result<()> {
        self.counts.closes.fetch_add(1, Ordering::SeqCst);
        self.inner.close()
    }
}

/// A backend whose writes fail cleanly once a countdown is exhausted —
/// redb's own `FailingBackend` test pattern, brought to the engine's
/// seam: the Nth write (and every one after) returns an `io::Error`,
/// and the engine must surface it as an `Error`, never a panic.
#[derive(Debug)]
struct FailingBackend {
    inner: InMemoryBackend,
    remaining_writes: AtomicU64,
}

impl StorageBackend for FailingBackend {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.remaining_writes.load(Ordering::SeqCst) == 0 {
            return Err(io::Error::other("injected backend failure"));
        }
        self.remaining_writes.fetch_sub(1, Ordering::SeqCst);
        self.inner.write(offset, data)
    }
}

/// SPEC §2.3.1 — the unit-level twin of `persist.txt`: documents, an
/// ondisk text index, an ondisk vector index, and a schema all survive
/// a full drop-and-reopen over the same shared backend.
#[test]
fn backend_db_open_with_backend_roundtrips_across_drop_and_reopen() {
    let (b_open, b_reopen) = SharedBackend::pair();

    {
        let db = Db::open_with_backend(b_open).unwrap();
        let c = db.collection("docs");
        c.insert(
            b"strong",
            &map(&[
                ("body", Value::Text("rust embedded database".into())),
                ("v", Value::Vector(vec![1.0, 0.0])),
            ]),
        )
        .unwrap();
        c.insert(
            b"weak",
            &map(&[
                ("body", Value::Text("python web frameworks".into())),
                ("v", Value::Vector(vec![0.0, 1.0])),
            ]),
        )
        .unwrap();
        c.create_text_index_ondisk("body").unwrap();
        c.create_vector_index_ondisk("v", corvid::Metric::Cosine)
            .unwrap();
        let schema = Schema::new().field(Field::new("body", FieldType::Text).required());
        c.set_schema(&schema).unwrap();
    } // full drop: Database drops, backend #1 with it; the Arc keeps the bytes

    let db = Db::open_with_backend(b_reopen).unwrap();
    assert_eq!(db.collections().unwrap(), vec!["docs".to_owned()]);
    let c = db.collection("docs");
    assert_eq!(c.len().unwrap(), 2);
    assert_eq!(
        c.get(b"strong").unwrap(),
        Some(map(&[
            ("body", Value::Text("rust embedded database".into())),
            ("v", Value::Vector(vec![1.0, 0.0])),
        ]))
    );
    let schema = c.schema().expect("schema survives the reopen");
    assert_eq!(schema.fields().len(), 1);
    assert_eq!(schema.fields()[0].name, "body");
    // The rebuilt ondisk text index still answers.
    let hits = c.text_search("body", "rust database", 2).unwrap();
    assert_eq!(hits.len(), 1, "ondisk text index answers after reopen");
    // The rebuilt ondisk vector index still answers (and ranks the
    // aligned vector first).
    let vhits = c
        .query()
        .vector("v", vec![1.0, 0.0], 2, corvid::Metric::Cosine)
        .run()
        .unwrap();
    assert_eq!(vhits.len(), 2, "ondisk vector index answers after reopen");
    assert_eq!(vhits[0].key, b"strong".to_vec());
}

/// SPEC §2.3.2 — trait dispatch, durability plumbing, and the
/// close-exactly-once contract, all observed through real workloads.
#[test]
fn backend_seam_dispatches_syncs_and_closes_exactly_once() {
    let (backend, counts) = CountingBackend::new();
    {
        let db = Db::open_with_backend(backend).unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &map(&[("n", Value::Int(1))])).unwrap();
        let got = c.get(b"a").unwrap();
        assert_eq!(got, Some(map(&[("n", Value::Int(1))])));
        // A durable engine commit must have reached sync_data.
        assert!(
            counts.syncs.load(Ordering::SeqCst) >= 1,
            "durable commit reached sync_data"
        );
        assert!(counts.writes.load(Ordering::SeqCst) > 0);
        assert!(counts.reads.load(Ordering::SeqCst) > 0);
        assert!(counts.len_calls.load(Ordering::SeqCst) > 0);
        assert!(counts.set_lens.load(Ordering::SeqCst) > 0);
    }
    // redb's trait contract: close fires exactly once, at Database drop.
    assert_eq!(counts.closes.load(Ordering::SeqCst), 1);

    // Store::flush is the explicit durable-commit path: it too must
    // land in sync_data (observed on a fresh Store over its own
    // counting backend).
    let (backend2, counts2) = CountingBackend::new();
    {
        let s = Store::open_with_backend(backend2).unwrap();
        s.transaction(|tx| {
            tx.put("docs", b"k", b"v")?;
            Ok(())
        })
        .unwrap();
        let before = counts2.syncs.load(Ordering::SeqCst);
        s.flush().unwrap();
        assert!(
            counts2.syncs.load(Ordering::SeqCst) > before,
            "Store::flush reached sync_data"
        );
    }
    assert_eq!(counts2.closes.load(Ordering::SeqCst), 1);
}

/// SPEC §2.3.3 — injected backend write failures surface as clean
/// engine `Error`s, never panics, at BOTH failure points: during open
/// (the format-version write) and mid-life (the first post-open
/// commit).
#[test]
fn backend_failing_write_surfaces_clean_error_not_panic() {
    // Failure point 1: every write fails — the open itself must return
    // a clean storage-class error. Exercised through the STORE form (the
    // Db form's failure path is the same delegation; Store is not Debug,
    // so no unwrap_err).
    let err = match Store::open_with_backend(FailingBackend {
        inner: InMemoryBackend::new(),
        remaining_writes: AtomicU64::new(0),
    }) {
        Err(e) => e,
        Ok(_) => panic!("open with an all-failing backend must fail"),
    };
    assert!(
        matches!(err, corvid::Error::Database(_) | corvid::Error::Storage(_)),
        "open-time backend failure surfaced cleanly, got: {err:?}"
    );

    // Failure point 2: open succeeds, the first post-open write fails.
    // The countdown is placed by probing how many writes a successful
    // open performs with a counting backend (deterministic for a given
    // binary: same operations, same sequence). The count is read while
    // the probe is still open — a drop can flush deferred pages, which
    // the live database under test hasn't done yet.
    let (probe, probe_counts) = CountingBackend::new();
    let probe_db = Db::open_with_backend(probe).unwrap();
    let open_writes = probe_counts.writes.load(Ordering::SeqCst);
    drop(probe_db);
    assert!(
        open_writes > 0,
        "a fresh open writes at least the format version"
    );

    let db = Db::open_with_backend(FailingBackend {
        inner: InMemoryBackend::new(),
        remaining_writes: AtomicU64::new(open_writes),
    })
    .unwrap();
    let c = db.collection("docs");
    let err = c
        .insert(b"a", &map(&[("n", Value::Int(1))]))
        .expect_err("countdown-exhausted write must fail");
    assert!(
        matches!(
            err,
            corvid::Error::Commit(_) | corvid::Error::Storage(_) | corvid::Error::Io(_)
        ),
        "mid-life backend failure surfaced as a commit/storage/io error, got: {err:?}"
    );
    // The failed transaction rolled back: the document is absent.
    assert_eq!(c.get(b"a").unwrap(), None);
}

/// SPEC §2.3.4 — both backup twins copy into a genuinely independent,
/// reopenable backend; contents match the source. The Store-level and
/// Db-level forms are exercised side by side.
#[test]
fn backend_backup_with_backend_copies_to_independent_reopenable_backend() {
    // Db-level.
    let (src, src_keepalive) = SharedBackend::pair();
    let (dst, dst_reopen) = SharedBackend::pair();
    let db = Db::open_with_backend(src).unwrap();
    let c = db.collection("docs");
    c.insert(
        b"a",
        &map(&[
            ("n", Value::Int(1)),
            ("body", Value::Text("alpha beta".into())),
        ]),
    )
    .unwrap();
    c.insert(
        b"b",
        &map(&[
            ("n", Value::Int(2)),
            ("body", Value::Text("gamma delta".into())),
        ]),
    )
    .unwrap();
    c.create_text_index_ondisk("body").unwrap();
    let schema = Schema::new().field(Field::new("n", FieldType::Int).required());
    c.set_schema(&schema).unwrap();
    db.backup_with_backend(dst).unwrap();

    let restored = Db::open_with_backend(dst_reopen).unwrap();
    let rc = restored.collection("docs");
    assert_eq!(rc.len().unwrap(), 2);
    assert_eq!(
        rc.get(b"a").unwrap(),
        Some(map(&[
            ("n", Value::Int(1)),
            ("body", Value::Text("alpha beta".into())),
        ]))
    );
    assert_eq!(restored.collections().unwrap(), vec!["docs".to_owned()]);
    assert!(rc.schema().is_some(), "schema survives the physical copy");
    // The copied ondisk text index (defs AND postings) answers.
    let hits = rc.text_search("body", "alpha", 2).unwrap();
    assert_eq!(hits.len(), 1, "ondisk text index answers after the copy");
    assert_eq!(hits[0].key, b"a".to_vec());
    drop(restored);

    // Store-level, over a fresh source with different content.
    let (s_src, s_src_keepalive) = SharedBackend::pair();
    let (s_dst, s_dst_reopen) = SharedBackend::pair();
    let store = Store::open_with_backend(s_src).unwrap();
    store
        .transaction(|tx| {
            tx.put("notes", b"x", b"1")?;
            Ok(())
        })
        .unwrap();
    store.backup_with_backend(s_dst).unwrap();
    drop(store);

    let reopened = Store::open_with_backend(s_dst_reopen).unwrap();
    assert_eq!(reopened.get("notes", b"x").unwrap(), Some(b"1".to_vec()));
    // The keepalive halves hold the source Arcs until here — after the
    // restores are proven, deliberately last.
    drop((src_keepalive, s_src_keepalive));
}
