//! Change-feed conformance (Task 12): the full events matrix through the
//! public API only — subscription identity (`SubscriptionId` distinctness,
//! `unsubscribe` true/false), multi-subscriber delivery, exact
//! `ChangeEvent` vectors for EVERY mutation path (insert, overwrite,
//! insert_auto, insert_batch, update both branches, patch both branches,
//! compare_and_set all branches, delete, delete_where, delete_batch, TTL
//! purge, link/unlink), dispatch timing (synchronous, post-commit: every
//! event has been delivered by the time the mutation call returns), and
//! cross-collection tagging.
//!
//! Dispatch model (read from `src/reactive.rs` before asserting):
//! notification is synchronous — callbacks run after the registry lock is
//! released and after the write has committed, in mutation order. A
//! callback therefore never observes a pre-commit state, and reentrancy is
//! the caller's business (a callback that mutates re-enters notify; no
//! engine-side reentrancy guard exists to pin). Subscriptions are db-wide:
//! `subscribe` takes no collection — each event carries its collection
//! name, so isolation is a subscriber-side filter.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use corvid::{ChangeEvent, ChangeKind, Db, SubscriptionId, Value, field};

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

type Log = Arc<Mutex<Vec<ChangeEvent>>>;

fn recorder() -> (Log, impl Fn(&ChangeEvent) + Send + Sync) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    (log, move |e: &ChangeEvent| {
        sink.lock().unwrap().push(e.clone())
    })
}

fn ev(collection: &str, key: &[u8], kind: ChangeKind) -> ChangeEvent {
    ChangeEvent {
        collection: collection.to_owned(),
        key: key.to_vec(),
        kind,
    }
}

#[test]
fn events_smoke_subscribe_records_insert_and_delete() {
    let db = Db::open_in_memory().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let id = db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    let c = db.collection("docs");
    c.insert(b"k", &Value::Int(1)).unwrap();
    c.delete(b"k").unwrap();

    // A delete of a missing key emits nothing.
    c.delete(b"absent").unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ChangeEvent {
                collection: "docs".to_owned(),
                key: b"k".to_vec(),
                kind: ChangeKind::Insert,
            },
            ChangeEvent {
                collection: "docs".to_owned(),
                key: b"k".to_vec(),
                kind: ChangeKind::Delete,
            },
        ]
    );

    // Unsubscribing stops delivery and reports false on repeat.
    assert!(db.unsubscribe(id));
    c.insert(b"k2", &Value::Int(2)).unwrap();
    assert_eq!(events.lock().unwrap().len(), 2);
    assert!(!db.unsubscribe(id));
}

// ===========================================================================
// Subscription identity and delivery
// ===========================================================================

/// `subscribe` returns a DISTINCT `SubscriptionId` per call; `unsubscribe`
/// reports whether the id existed (true once, false on repeat — and the
/// tuple field is private, so ids cannot be forged from outside); removing
/// one subscriber never disturbs the others.
#[test]
fn events_subscribe_returns_distinct_ids_and_unsubscribe_reports_existence() {
    let db = Db::open_in_memory().unwrap();
    let (log1, cb1) = recorder();
    let (log2, cb2) = recorder();
    let id1: SubscriptionId = db.subscribe(cb1);
    let id2: SubscriptionId = db.subscribe(cb2);
    assert_ne!(id1, id2); // distinct handles, comparable values

    let c = db.collection("docs");
    c.insert(b"k", &Value::Int(1)).unwrap();
    assert_eq!(log1.lock().unwrap().len(), 1);
    assert_eq!(log2.lock().unwrap().len(), 1);

    // Removing id1 stops ONLY subscriber 1; id2 keeps receiving.
    assert!(db.unsubscribe(id1));
    c.insert(b"k2", &Value::Int(2)).unwrap();
    assert_eq!(log1.lock().unwrap().len(), 1);
    assert_eq!(log2.lock().unwrap().len(), 2);

    // Repeat unsubscribe of id1 is false. (Ids cannot be forged: the tuple
    // field is private, so the only obtainable handles are subscribe's.)
    assert!(!db.unsubscribe(id1));

    // The surviving subscription is unaffected to the end.
    c.delete(b"k").unwrap();
    assert_eq!(
        *log2.lock().unwrap(),
        vec![
            ev("docs", b"k", ChangeKind::Insert),
            ev("docs", b"k2", ChangeKind::Insert),
            ev("docs", b"k", ChangeKind::Delete)
        ]
    );
}

/// Every current subscriber receives EVERY event, identically and exactly
/// (same order, same vectors — dispatch is not load-shed or round-robin).
#[test]
fn events_multiple_subscribers_all_receive_identical_exact_vectors() {
    let db = Db::open_in_memory().unwrap();
    let (log1, cb1) = recorder();
    let (log2, cb2) = recorder();
    let (log3, cb3) = recorder();
    db.subscribe(cb1);
    db.subscribe(cb2);
    db.subscribe(cb3);

    let c = db.collection("docs");
    c.insert(b"a", &Value::Int(1)).unwrap();
    c.delete(b"a").unwrap();
    c.insert(b"b", &Value::Int(2)).unwrap();

    let expected = vec![
        ev("docs", b"a", ChangeKind::Insert),
        ev("docs", b"a", ChangeKind::Delete),
        ev("docs", b"b", ChangeKind::Insert),
    ];
    assert_eq!(*log1.lock().unwrap(), expected);
    assert_eq!(*log2.lock().unwrap(), expected);
    assert_eq!(*log3.lock().unwrap(), expected.clone());
    // A late subscriber sees nothing retroactively.
    let (log4, cb4) = recorder();
    db.subscribe(cb4);
    c.delete(b"b").unwrap();
    assert_eq!(log4.lock().unwrap().len(), 1);
    assert_eq!(
        log4.lock().unwrap()[0],
        ev("docs", b"b", ChangeKind::Delete)
    );
}

// ===========================================================================
// Insert-path events
// ===========================================================================

/// The insert family emits exactly one Insert per applied row: a new-key
/// insert, an overwriting insert (idempotent data, non-skipped event), one
/// per insert_auto row keyed by the generated key, and one per BATCH ITEM
/// in item order — a repeated key inside a non-violating batch is last-
/// write-wins data but still notifies per item.
#[test]
fn events_insert_paths_emit_exact_insert_vectors_in_order() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    c.insert(b"i1", &doc("x", 1)).unwrap();
    c.insert(b"i1", &doc("x", 2)).unwrap(); // overwrite: still an Insert
    let auto = c.insert_auto(&doc("x", 3)).unwrap();
    c.insert_batch(&[
        (b"b2", &doc("x", 4)),
        (b"b1", &doc("x", 5)),
        (b"b2", &doc("x", 6)),
    ])
    .unwrap(); // duplicate b2 INSIDE the batch: per-item events, item order

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"i1", ChangeKind::Insert),
            ev("docs", b"i1", ChangeKind::Insert),
            ev("docs", &auto, ChangeKind::Insert),
            ev("docs", b"b2", ChangeKind::Insert),
            ev("docs", b"b1", ChangeKind::Insert),
            ev("docs", b"b2", ChangeKind::Insert),
        ]
    );
    // The empty batch is a no-op on events.
    let before = log.lock().unwrap().len();
    c.insert_batch(&[]).unwrap();
    assert_eq!(log.lock().unwrap().len(), before);
}

// ===========================================================================
// update / patch / compare_and_set events
// ===========================================================================

/// update and patch are Insert when they write (transform, create-on-
/// missing) and Delete when the transform returns None; patch creating a
/// missing document is an Insert. All exact vectors.
#[test]
fn events_update_and_patch_emit_exact_vectors_for_both_branches() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    // update transform: Insert with the new document in place.
    c.insert(b"u1", &doc("x", 1)).unwrap();
    c.update(b"u1", |cur| {
        let mut m = match cur {
            Some(Value::Map(m)) => m,
            _ => unreachable!("seeded"),
        };
        m.insert("n".to_owned(), Value::Int(9));
        Some(Value::Map(m))
    })
    .unwrap();

    // update delete branch: the transform returns None -> Delete.
    c.update(b"u1", |_| None).unwrap();

    // update on a missing key creating a document: Insert (missing docs are
    // not an error).
    c.update(b"u2", |_| Some(doc("born", 0))).unwrap();

    // patch on an existing map: top-level merge -> Insert.
    c.insert(b"p1", &doc("x", 1)).unwrap();
    c.patch(b"p1", &map(&[("extra", Value::Bool(true))]))
        .unwrap();

    // patch creating a missing document: Insert.
    c.patch(b"p2", &map(&[("extra", Value::Bool(true))]))
        .unwrap();

    // update's delete branch on a MISSING key: no event (delete of missing).
    c.update(b"absent", |_| None).unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"u1", ChangeKind::Insert), // the seed insert
            ev("docs", b"u1", ChangeKind::Insert), // update transform
            ev("docs", b"u1", ChangeKind::Delete), // update delete branch
            ev("docs", b"u2", ChangeKind::Insert), // update create-on-missing
            ev("docs", b"p1", ChangeKind::Insert), // the seed insert
            ev("docs", b"p1", ChangeKind::Insert), // patch merge
            ev("docs", b"p2", ChangeKind::Insert), // patch create-on-missing
        ]
    );
}

/// compare_and_set notifies only when applied: a swap is Insert, the delete
/// branch (new = None, compare matched) is Delete, and a mismatched compare
/// emits nothing. CAS-create on an absent key (expected None) is Insert.
#[test]
fn events_compare_and_set_emit_exact_vectors_per_branch() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    // CAS-create: expected None on an absent key, applied -> Insert.
    assert!(
        c.compare_and_set(b"c1", None, Some(doc("created", 1)))
            .unwrap()
    );

    // CAS swap: compare matches the stored value -> Insert of the new doc.
    assert!(
        c.compare_and_set(b"c1", Some(&doc("created", 1)), Some(doc("swapped", 2)))
            .unwrap()
    );

    // CAS mismatch: nothing applied, nothing emitted.
    assert!(
        !c.compare_and_set(b"c1", Some(&doc("stale", 0)), Some(doc("no", 0)))
            .unwrap()
    );

    // CAS delete branch: compare matches, new is None -> Delete.
    assert!(
        c.compare_and_set(b"c1", Some(&doc("swapped", 2)), None)
            .unwrap()
    );

    // CAS delete branch against an absent key (expected None, new None):
    // compare matches, but no document existed -> not applied, no event.
    assert!(!c.compare_and_set(b"absent", None, None).unwrap());

    // CAS mismatch against a missing document (expected Some, absent):
    // no event.
    assert!(
        !c.compare_and_set(b"absent", Some(&doc("x", 0)), Some(doc("y", 0)))
            .unwrap()
    );

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"c1", ChangeKind::Insert),
            ev("docs", b"c1", ChangeKind::Insert),
            ev("docs", b"c1", ChangeKind::Delete),
        ]
    );
}

// ===========================================================================
// Delete-path events
// ===========================================================================

/// The delete family emits one Delete per document actually removed:
/// delete (existing yes / missing silent), delete_where in key order,
/// delete_batch in INPUT order (absent keys silent), and the TTL purge —
/// one Delete per purged document, in expiry order then key order.
#[test]
fn events_delete_paths_emit_exact_delete_vectors_in_order() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    c.insert(b"d1", &doc("x", 1)).unwrap();
    log.lock().unwrap().clear();

    // delete: existing -> Delete; missing -> silent.
    assert!(c.delete(b"d1").unwrap());
    assert!(!c.delete(b"d1").unwrap());

    // delete_batch: per EXISTING key, in input order (not key order).
    c.insert_batch(&[
        (b"z", &doc("x", 1)),
        (b"a", &doc("x", 5)),
        (b"m", &doc("x", 1)),
    ])
    .unwrap();
    log.lock().unwrap().clear();
    let removed = c.delete_batch(&[b"z", b"absent", b"a"]).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"z", ChangeKind::Delete),
            ev("docs", b"a", ChangeKind::Delete),
        ]
    );

    // delete_where: one Delete per match, in key order (the query runs in
    // key order and each match goes through the normal delete path). The
    // surviving "m" row (n = 1) also matches n <= 2 alongside the fresh
    // rows and the auto row, whose zero-padded key sorts first.
    c.insert_batch(&[
        (b"w2", &doc("x", 2)),
        (b"w1", &doc("x", 1)),
        (b"keep", &doc("x", 9)),
    ])
    .unwrap();
    let auto = c.insert_auto(&doc("x", 1)).unwrap(); // sorts before "m*"
    log.lock().unwrap().clear();
    let n = c.delete_where(field("n").le(Value::Int(2))).unwrap();
    assert_eq!(n, 4); // auto, m, w1, w2
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", &auto, ChangeKind::Delete),
            ev("docs", b"m", ChangeKind::Delete),
            ev("docs", b"w1", ChangeKind::Delete),
            ev("docs", b"w2", ChangeKind::Delete),
        ]
    );

    // delete_where with zero matches: no events.
    log.lock().unwrap().clear();
    let n = c.delete_where(field("n").le(Value::Int(2))).unwrap();
    assert_eq!(n, 0);
    assert!(log.lock().unwrap().is_empty());

    // TTL purge: one Delete per PURGED document. Due order is expiry order,
    // then key order within one timestamp (the TTL index is
    // (timestamp, key)-sorted).
    c.insert_with_ttl(b"t2", &doc("x", 2), 200).unwrap();
    c.insert_with_ttl(b"t1", &doc("x", 1), 200).unwrap();
    c.insert_with_ttl(b"t0", &doc("x", 0), 100).unwrap();
    c.insert(b"immortal", &doc("x", 9)).unwrap();
    log.lock().unwrap().clear();
    let purged = c.purge_expired(200).unwrap();
    assert_eq!(purged, 3);
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"t0", ChangeKind::Delete),
            ev("docs", b"t1", ChangeKind::Delete),
            ev("docs", b"t2", ChangeKind::Delete),
        ]
    );

    // A purge with nothing due emits nothing (the immortal row stays).
    log.lock().unwrap().clear();
    assert_eq!(c.purge_expired(i64::MAX - 1).unwrap(), 0);
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(c.get(b"immortal").unwrap(), Some(doc("x", 9)));
}

/// The TTL purge's edge cascade is silent: purging a document that has
/// edges emits exactly ONE event — the document's own Delete — never
/// per-edge events; and a purge that only drops a stranded entry (no
/// document) emits nothing at all.
#[test]
fn events_ttl_purge_cascade_is_silent_and_stranded_purge_emits_nothing() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    c.insert_with_ttl(b"doomed", &doc("x", 1), 100).unwrap();
    c.insert(b"stays", &doc("y", 2)).unwrap();
    c.link(b"doomed", "knows", b"stays").unwrap();
    c.link(b"stays", "knows", b"doomed").unwrap();
    log.lock().unwrap().clear();

    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(
        *log.lock().unwrap(),
        vec![ev("docs", b"doomed", ChangeKind::Delete)]
    );
    // The cascade really happened (edges gone) without any edge event.
    assert!(c.neighbors(b"stays", "knows").unwrap().is_empty());

    // A stranded expiry entry (set_ttl on a never-inserted key with edges
    // hanging off it): the purge drops the entry and the edges but counts
    // no document — and emits nothing, like a delete of a missing key.
    c.link(b"ghost", "knows", b"stays").unwrap();
    c.set_ttl(b"ghost", 100).unwrap();
    log.lock().unwrap().clear();
    assert_eq!(c.purge_expired(200).unwrap(), 0);
    assert!(log.lock().unwrap().is_empty());
    assert!(c.in_neighbors(b"stays", "knows").unwrap().is_empty());
}

// ===========================================================================
// Cross-collection tagging (subscriptions are db-wide)
// ===========================================================================

/// `subscribe` takes no collection: every event is delivered to every
/// subscriber, TAGGED with the collection that actually mutated. A
/// subscriber that wants one collection filters on `event.collection` —
/// the engine guarantees the tag always names the mutated collection, so
/// the filter can never leak another collection's events.
#[test]
fn events_cross_collection_tagging_each_event_names_its_collection() {
    let db = Db::open_in_memory().unwrap();
    let (all_log, all_cb) = recorder();
    db.subscribe(all_cb);
    // A subscriber-side filter: keep only "alpha" events.
    let (alpha_log, alpha_cb): (Log, _) = {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let cb = move |e: &ChangeEvent| {
            if e.collection == "alpha" {
                sink.lock().unwrap().push(e.clone());
            }
        };
        (log, cb)
    };
    db.subscribe(alpha_cb);

    let alpha = db.collection("alpha");
    let beta = db.collection("beta");
    alpha.insert(b"a1", &doc("x", 1)).unwrap();
    beta.insert(b"b1", &doc("y", 1)).unwrap();
    alpha.delete(b"a1").unwrap();
    beta.delete_where(field("n").eq(Value::Int(1))).unwrap();

    // Every event names exactly the collection that mutated — never the
    // edge/TTL namespaces or the other collection.
    assert_eq!(
        *all_log.lock().unwrap(),
        vec![
            ev("alpha", b"a1", ChangeKind::Insert),
            ev("beta", b"b1", ChangeKind::Insert),
            ev("alpha", b"a1", ChangeKind::Delete),
            ev("beta", b"b1", ChangeKind::Delete),
        ]
    );
    // The filtered subscriber saw only alpha's two events, exactly.
    assert_eq!(
        *alpha_log.lock().unwrap(),
        vec![
            ev("alpha", b"a1", ChangeKind::Insert),
            ev("alpha", b"a1", ChangeKind::Delete),
        ]
    );
}

// ===========================================================================
// Graph events — completeness over Task 7's pins
// ===========================================================================

/// `link` never checks endpoint existence: linking to/from keys that are
/// not (and never become) documents still commits and still emits the
/// Insert event keyed by `from`. (unlink's silent noop and the delete
/// cascade's single doc event are pinned in graph.rs.)
#[test]
fn events_link_to_missing_endpoints_still_emits_insert_keyed_by_from() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("nodes");

    // Neither endpoint exists as a document.
    c.link(b"ghost", "knows", b"phantom").unwrap();
    // Only the target is absent.
    c.insert(b"real", &Value::Int(1)).unwrap();
    log.lock().unwrap().clear();
    c.link(b"real", "knows", b"phantom").unwrap();
    // Only the source is absent.
    c.link(b"ghost", "likes", b"real").unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("nodes", b"real", ChangeKind::Insert),
            ev("nodes", b"ghost", ChangeKind::Insert),
        ]
    );
    // The events named the USER collection, and reads confirm the edges.
    assert_eq!(
        c.neighbors(b"ghost", "knows").unwrap(),
        vec![b"phantom".to_vec()]
    );
}

// ===========================================================================
// Dispatch timing — synchronous, post-commit
// ===========================================================================

/// Notification is synchronous and post-commit: by the time a mutation
/// call RETURNS, its event has already been delivered (a background/deferred
/// feed could not guarantee this), and events arrive in exact mutation
/// order. The callback list is snapshotted before dispatch, so a callback
/// unsubscribing another subscription mid-dispatch cannot affect the event
/// in flight (already-cloned callbacks still run; the removal applies to
/// the NEXT event).
#[test]
fn events_dispatch_is_synchronous_post_commit_and_in_mutation_order() {
    let db = Db::open_in_memory().unwrap();
    let (log, cb) = recorder();
    db.subscribe(cb);
    let c = db.collection("docs");

    c.insert(b"k", &Value::Int(1)).unwrap();
    // Delivered already — no pump, no poll, no delay.
    assert_eq!(
        *log.lock().unwrap(),
        vec![ev("docs", b"k", ChangeKind::Insert)]
    );

    c.delete(b"k").unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ev("docs", b"k", ChangeKind::Insert),
            ev("docs", b"k", ChangeKind::Delete),
        ]
    );

    // Mid-dispatch unsubscribe (second db: the unsubscribing callback needs a
    // 'static handle on it): the callback list is cloned before dispatch, so
    // the first subscriber unsubscribing the second MID-EVENT cannot stop the
    // second's already-cloned callback for the event in flight — the removal
    // applies to the NEXT event.
    let db2 = Arc::new(Db::open_in_memory().unwrap());
    let (log2, cb2) = recorder();
    let slot = Arc::new(Mutex::new(None::<SubscriptionId>));
    {
        let handle = Arc::clone(&db2);
        let sink = Arc::clone(&slot);
        db2.subscribe(move |_: &ChangeEvent| {
            if let Some(id) = *sink.lock().unwrap() {
                handle.unsubscribe(id);
            }
        });
    }
    let id2 = db2.subscribe(cb2);
    *slot.lock().unwrap() = Some(id2);
    let c2 = db2.collection("docs");
    c2.insert(b"k", &Value::Int(1)).unwrap();
    assert_eq!(log2.lock().unwrap().len(), 1);
    // The next event no longer reaches the unsubscribed callback.
    c2.insert(b"k2", &Value::Int(2)).unwrap();
    assert_eq!(log2.lock().unwrap().len(), 1);
}
