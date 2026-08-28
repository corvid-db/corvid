//! TTL conformance (Task 12): the full expiry matrix through the public API
//! only — insert_with_ttl/set_ttl/ttl roundtrips (i64 extremes included),
//! purge boundary semantics (`now == expires_at` is due, per the `<= now`
//! contract), purge idempotence and counting, expired-doc visibility
//! (purge-on-demand: an expired-but-unpurged record stays visible — the
//! engine has no clock), TTL + index maintenance across every index family,
//! set_ttl on missing docs, the plain-write clearing mechanism, and the
//! edge cascade (W3 ruling: a stranded TTL entry must not leave dangling
//! edges — same contract as delete).

use std::collections::BTreeMap;

use corvid::schema::{Field, FieldType, Schema};
use corvid::{Db, Error, Metric, Value, field};

fn doc(name: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    Value::Map(m)
}

#[test]
fn ttl_smoke_insert_with_ttl_purges_at_boundary() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    c.insert_with_ttl(b"k", &doc("ephemeral"), 100).unwrap();
    assert_eq!(c.ttl(b"k").unwrap(), Some(100));

    // Before the boundary the record is alive; the purge collects nothing.
    assert_eq!(c.purge_expired(99).unwrap(), 0);
    assert_eq!(c.get(b"k").unwrap(), Some(doc("ephemeral")));

    // At the boundary (now == expires_at) the record is due and purged,
    // through the normal delete path.
    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(c.get(b"k").unwrap(), None);

    // Purging again is a no-op.
    assert_eq!(c.purge_expired(100).unwrap(), 0);
}

// ===========================================================================
// Graph interaction — the W3 wave-review ruling (binding prepend)
// ===========================================================================

/// An expired document's purge cascades its graph edges exactly like a
/// plain delete — both namespaces, both directions.
#[test]
fn ttl_purge_cascades_edges_of_expired_document_both_namespaces() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert_with_ttl(b"doomed", &doc("ephemeral"), 100)
        .unwrap();
    c.insert(b"stays", &doc("permanent")).unwrap();
    c.link(b"doomed", "knows", b"stays").unwrap();
    c.link(b"stays", "knows", b"doomed").unwrap();
    c.link(b"stays", "likes", b"stays").unwrap();

    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(c.get(b"doomed").unwrap(), None);
    // Every edge that had `doomed` as an endpoint is gone, in both the
    // forward and the reverse namespace; `stays`'s self-loop survives.
    assert!(c.neighbors(b"doomed", "knows").unwrap().is_empty());
    assert!(c.neighbors(b"stays", "knows").unwrap().is_empty());
    assert!(c.in_neighbors(b"stays", "knows").unwrap().is_empty());
    assert_eq!(
        c.neighbors(b"stays", "likes").unwrap(),
        vec![b"stays".to_vec()]
    );
    assert_eq!(c.get(b"stays").unwrap(), Some(doc("permanent")));
}

/// The binding W3 ruling, RED first: `purge_due_key` must run the edge
/// cascade REGARDLESS of whether the document row existed — a stranded TTL
/// entry (expiry set on a key that never was a document, or whose document
/// was already deleted) carries the same delete contract as
/// [`corvid::Collection::delete`], which purges dangling edges even when it
/// returns `false`. `link` allows absent endpoints, so this state is
/// reachable through the public API alone: link against a never-inserted
/// key, then `set_ttl` on it.
#[test]
fn ttl_purge_cascades_edges_of_stranded_entry_without_document() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    // "ghost" never exists as a document; edges hang off it anyway.
    c.link(b"ghost", "knows", b"real").unwrap();
    c.link(b"real", "knows", b"ghost").unwrap();
    c.insert(b"real", &doc("real")).unwrap();
    // A stray expiry on the absent key — Ok, and stranded.
    c.set_ttl(b"ghost", 100).unwrap();
    assert_eq!(c.ttl(b"ghost").unwrap(), Some(100));
    assert_eq!(c.get(b"ghost").unwrap(), None);

    // The purge removes no document (count 0) but must still take the
    // ghost's edges with it, exactly like a delete of a missing key does.
    assert_eq!(c.purge_expired(200).unwrap(), 0);
    assert!(c.neighbors(b"ghost", "knows").unwrap().is_empty());
    assert!(c.in_neighbors(b"real", "knows").unwrap().is_empty());
    // The reverse edge (real -> ghost) is gone from the forward namespace
    // too — the cascade removes each edge from BOTH namespaces.
    assert!(c.neighbors(b"real", "knows").unwrap().is_empty());
    // The stranded TTL entries themselves are gone; the survivor keeps its
    // document and its unrelated state.
    assert_eq!(c.ttl(b"ghost").unwrap(), None);
    assert_eq!(c.get(b"real").unwrap(), Some(doc("real")));
}

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Map(m)
}

// ===========================================================================
// insert_with_ttl / set_ttl / ttl roundtrips
// ===========================================================================

/// Expiry roundtrips exactly: `insert_with_ttl` sets it with the insert,
/// `set_ttl` sets it after a plain (TTL-less) insert, a second `set_ttl`
/// overwrites (never accumulates entries), and every read-back is the exact
/// i64 written — including negative timestamps and zero.
#[test]
fn ttl_roundtrip_set_on_insert_after_plain_insert_and_overwrite() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // Set with the insert; the row and its expiry commit together.
    c.insert_with_ttl(b"a", &doc("ephemeral"), 100).unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), Some(100));

    // Plain insert: no expiry at all.
    c.insert(b"b", &doc("plain")).unwrap();
    assert_eq!(c.ttl(b"b").unwrap(), None);

    // set_ttl arms the plain row without rewriting the document.
    c.set_ttl(b"b", -5).unwrap();
    assert_eq!(c.ttl(b"b").unwrap(), Some(-5));
    assert_eq!(c.get(b"b").unwrap(), Some(doc("plain")));

    // Overwrite replaces the previous expiry — the read-back is exactly the
    // last value written, and each row purges at its CURRENT expiry only:
    // at now -1, b (ts -5) is due but a (overwritten to ts 0) is not.
    c.set_ttl(b"a", 0).unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), Some(0));
    assert_eq!(c.purge_expired(-1).unwrap(), 1); // b's ts -5, not a's old 100
    assert_eq!(c.get(b"a").unwrap(), Some(doc("ephemeral")));
    assert_eq!(c.purge_expired(0).unwrap(), 1); // a's overwritten ts 0
    assert_eq!(c.len().unwrap(), 0);
}

/// Timestamps are unvalidated caller-epoch i64s: the extremes are accepted
/// and order correctly under the order-preserving encoding (no validation
/// error exists to pin — acceptance is the contract).
#[test]
fn ttl_timestamps_accept_i64_extremes_and_order_correctly() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert_with_ttl(b"min", &doc("bottom"), i64::MIN).unwrap();
    c.insert_with_ttl(b"zero", &doc("middle"), 0).unwrap();
    c.insert_with_ttl(b"max", &doc("top"), i64::MAX).unwrap();
    assert_eq!(c.ttl(b"min").unwrap(), Some(i64::MIN));
    assert_eq!(c.ttl(b"max").unwrap(), Some(i64::MAX));

    // The boundary holds at the very bottom: now == i64::MIN makes exactly
    // the i64::MIN entry due (inclusive `<= now`); nothing else is.
    assert_eq!(c.purge_expired(i64::MIN).unwrap(), 1);
    assert_eq!(c.get(b"min").unwrap(), None);
    assert_eq!(c.get(b"zero").unwrap(), Some(doc("middle")));

    // And at the very top: now == i64::MAX makes everything due.
    assert_eq!(c.purge_expired(i64::MAX).unwrap(), 2);
    assert_eq!(c.get(b"zero").unwrap(), None);
    assert_eq!(c.get(b"max").unwrap(), None);
    assert!(c.is_empty().unwrap());
}

// ===========================================================================
// purge_expired boundary matrix
// ===========================================================================

/// The due boundary is inclusive (`expires_at <= now`): one-before is not
/// due, exactly-at is, one-after is; a mixed collection purges exactly the
/// due subset and reports its count; re-purging is idempotent (0).
#[test]
fn ttl_purge_boundary_one_before_exactly_at_one_after_and_idempotence() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // One before the boundary: not due, nothing purged.
    c.insert_with_ttl(b"k", &doc("x"), 100).unwrap();
    assert_eq!(c.purge_expired(99).unwrap(), 0);
    assert_eq!(c.get(b"k").unwrap(), Some(doc("x")));

    // Exactly at the boundary: due (inclusive contract).
    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(c.get(b"k").unwrap(), None);

    // One after the boundary of a later expiry: due as well.
    c.insert_with_ttl(b"k2", &doc("y"), 500).unwrap();
    assert_eq!(c.purge_expired(501).unwrap(), 1);
    assert_eq!(c.get(b"k2").unwrap(), None);

    // A mixed collection purges exactly the due subset, in one call.
    c.insert_with_ttl(b"early", &doc("e"), 50).unwrap();
    c.insert_with_ttl(b"mid", &doc("m"), 100).unwrap();
    c.insert_with_ttl(b"late", &doc("l"), 150).unwrap();
    c.insert(b"never", &doc("n")).unwrap(); // no TTL: never expires
    assert_eq!(c.purge_expired(100).unwrap(), 2); // early + mid
    assert_eq!(c.get(b"early").unwrap(), None);
    assert_eq!(c.get(b"mid").unwrap(), None);
    assert_eq!(c.get(b"late").unwrap(), Some(doc("l")));
    assert_eq!(c.get(b"never").unwrap(), Some(doc("n")));

    // Idempotent: a re-purge at the same (or any earlier) now finds 0.
    assert_eq!(c.purge_expired(100).unwrap(), 0);
    assert_eq!(c.purge_expired(149).unwrap(), 0);
    // The survivor is still purgeable at its own boundary.
    assert_eq!(c.purge_expired(150).unwrap(), 1);
    assert_eq!(c.len().unwrap(), 1); // only the TTL-less row remains
}

// ===========================================================================
// Visibility — purge-on-demand semantics
// ===========================================================================

/// TTL is purge-on-demand: an expired-but-unpurged record stays fully
/// visible (the engine has no clock to compare against on a read); after
/// the purge it is gone from every read path — get, scan, len, filtered
/// query.
#[test]
fn ttl_expired_doc_visible_until_purged_hidden_from_all_reads_after() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert_with_ttl(b"old", &doc("stale"), 10).unwrap();
    c.insert_with_ttl(b"new", &doc("fresh"), 10_000).unwrap();

    // Past expiry (now would be 11) but NOT yet purged: fully visible.
    assert_eq!(c.get(b"old").unwrap(), Some(doc("stale")));
    assert_eq!(c.ttl(b"old").unwrap(), Some(10));
    assert_eq!(c.len().unwrap(), 2);
    assert_eq!(c.scan().unwrap().len(), 2);
    let names: Vec<Vec<u8>> = c
        .query()
        .filter(field("name").eq(Value::Text("stale".to_owned())))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(names, vec![b"old".to_vec()]);

    // Purge at 11 removes only the expired one...
    assert_eq!(c.purge_expired(11).unwrap(), 1);
    // ...and every read path excludes it afterwards.
    assert_eq!(c.get(b"old").unwrap(), None);
    assert_eq!(c.ttl(b"old").unwrap(), None);
    assert_eq!(c.len().unwrap(), 1);
    let scanned = c.scan().unwrap();
    assert_eq!(scanned, vec![(b"new".to_vec(), doc("fresh"))]);
    let names: Vec<Vec<u8>> = c
        .query()
        .filter(field("name").eq(Value::Text("stale".to_owned())))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(names.is_empty());
}

// ===========================================================================
// TTL + index maintenance
// ===========================================================================

/// A purged document leaves EVERY index family: the scalar index stops
/// matching it, the unique constraint frees its value (a fresh insert may
/// reuse it, while the kept row's value still conflicts with the exact
/// variant), the vector index no longer returns it, and the text index
/// drops its postings.
#[test]
fn ttl_purge_removes_doc_from_scalar_unique_vector_and_text_indexes() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("idx");
    c.set_schema(&Schema::new().field(Field::new("email", FieldType::Text).unique()))
        .unwrap();
    c.create_scalar_index("tag").unwrap();
    c.create_vector_index("v", Metric::Cosine).unwrap();
    c.create_text_index("body").unwrap();

    let gone = map(&[
        ("tag", Value::Text("temp".to_owned())),
        ("email", Value::Text("gone@x.com".to_owned())),
        ("v", Value::Vector(vec![1.0, 0.0])),
        ("body", Value::Text("corvid corvid temp".to_owned())),
    ]);
    let kept = map(&[
        ("tag", Value::Text("perm".to_owned())),
        ("email", Value::Text("kept@x.com".to_owned())),
        ("v", Value::Vector(vec![0.0, 1.0])),
        ("body", Value::Text("corvid keeper".to_owned())),
    ]);
    c.insert_with_ttl(b"gone", &gone, 100).unwrap();
    c.insert(b"kept", &kept).unwrap();

    // Sanity: before the purge both docs are served by every index.
    assert_eq!(
        c.vector_search("v", &[1.0, 0.0], 2, Metric::Cosine)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(c.text_search("body", "corvid", 2).unwrap().len(), 2);

    assert_eq!(c.purge_expired(100).unwrap(), 1);

    // Scalar index: the purged tag matches nothing; the kept one does.
    let tags: Vec<Vec<u8>> = c
        .query()
        .filter(field("tag").eq(Value::Text("temp".to_owned())))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(tags.is_empty());
    let tags: Vec<Vec<u8>> = c
        .query()
        .filter(field("tag").eq(Value::Text("perm".to_owned())))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(tags, vec![b"kept".to_vec()]);

    // Unique: the purged value is free again; the kept one still conflicts.
    c.insert(
        b"reborn",
        &map(&[("email", Value::Text("gone@x.com".to_owned()))]),
    )
    .unwrap();
    let err = c
        .insert(
            b"clash",
            &map(&[("email", Value::Text("kept@x.com".to_owned()))]),
        )
        .unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)));

    // Vector index: only the kept (and unindexed reborn) rows remain — the
    // purged key never surfaces, at any k.
    let hits = c
        .vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
        .unwrap();
    let keys: Vec<&[u8]> = hits.iter().map(|h| h.key.as_slice()).collect();
    assert_eq!(keys, vec![&b"kept"[..]]);

    // Text index: the purged doc's postings are gone; "temp" matches
    // nothing, "corvid" matches only the kept doc.
    assert!(c.text_search("body", "temp", 2).unwrap().is_empty());
    let th = c.text_search("body", "corvid", 2).unwrap();
    assert_eq!(th.len(), 1);
    assert_eq!(th[0].key, b"kept".to_vec());
    c.delete(b"reborn").unwrap(); // keep the final state minimal
    assert_eq!(c.len().unwrap(), 1);
}

// ===========================================================================
// set_ttl / ttl edge cases
// ===========================================================================

/// `set_ttl` on a key with no document is NOT an error: it is Ok and arms a
/// stranded entry, which the purge later drops WITHOUT counting it as a
/// purged document (only actual document removals count).
#[test]
fn ttl_set_ttl_on_missing_doc_is_ok_and_purges_without_counting() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    // No document exists at "ghost"; set_ttl is still Ok.
    c.set_ttl(b"ghost", 100).unwrap();
    assert_eq!(c.ttl(b"ghost").unwrap(), Some(100));
    assert_eq!(c.get(b"ghost").unwrap(), None);

    // The stranded entry is due and cleaned, but the count stays 0 — a
    // phantom row was never a document.
    assert_eq!(c.purge_expired(200).unwrap(), 0);
    assert_eq!(c.ttl(b"ghost").unwrap(), None);
    assert_eq!(c.purge_expired(300).unwrap(), 0);

    // A real doc purged in the same sweep as a stranded entry: only the
    // real one counts.
    c.set_ttl(b"phantom", 100).unwrap();
    c.insert_with_ttl(b"real", &doc("real"), 100).unwrap();
    assert_eq!(c.purge_expired(200).unwrap(), 1);
    assert_eq!(c.get(b"real").unwrap(), None);
    assert_eq!(c.ttl(b"phantom").unwrap(), None);
    assert_eq!(c.len().unwrap(), 0);
}

/// `ttl()` cannot distinguish "no such document" from "document without an
/// expiry": both are `None` (the TTL index is a per-key forward entry, not
/// a document attribute).
#[test]
fn ttl_on_missing_doc_and_doc_without_expiry_both_none() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    assert_eq!(c.ttl(b"never-touched").unwrap(), None);
    c.insert(b"plain", &doc("plain")).unwrap();
    assert_eq!(c.ttl(b"plain").unwrap(), None);
    // ...and an armed key reads back exactly its timestamp (the positive
    // control for the distinction the two Nones cannot make).
    c.set_ttl(b"plain", 42).unwrap();
    assert_eq!(c.ttl(b"plain").unwrap(), Some(42));
}

// ===========================================================================
// The clearing mechanism — plain writes own expiry state
// ===========================================================================

/// Every plain write path CLEARS an armed expiry (there is no clear_ttl
/// API): insert overwrite, update, patch, compare_and_set, insert_batch,
/// insert_auto, and delete all leave `ttl() == None`, and a purge at the
/// old timestamp then finds nothing due. Rewriting via `insert_with_ttl`
/// re-arms it; `set_ttl(i64::MAX)` is the "practically immortal" idiom but
/// is still just an expiry (due at `now == i64::MAX`).
#[test]
fn ttl_plain_write_paths_clear_expiry() {
    // insert overwrite clears (also pinned from the mutations side).
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert_with_ttl(b"a", &doc("a"), 100).unwrap();
    c.insert(b"a", &doc("a2")).unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), None);
    assert_eq!(c.purge_expired(i64::MAX).unwrap(), 0);
    assert_eq!(c.get(b"a").unwrap(), Some(doc("a2")));

    // update (transform) clears.
    c.set_ttl(b"a", 100).unwrap();
    c.update(b"a", |cur| {
        let mut m = match cur {
            Some(Value::Map(m)) => m,
            _ => unreachable!("seeded"),
        };
        m.insert("name".to_owned(), Value::Text("a3".to_owned()));
        Some(Value::Map(m))
    })
    .unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), None);

    // patch clears.
    c.set_ttl(b"a", 100).unwrap();
    c.patch(b"a", &doc("a4")).unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), None);

    // compare_and_set (swap branch) clears.
    c.set_ttl(b"a", 100).unwrap();
    assert!(
        c.compare_and_set(b"a", Some(&doc("a4")), Some(doc("a5")))
            .unwrap()
    );
    assert_eq!(c.ttl(b"a").unwrap(), None);

    // insert_batch on the same key clears.
    c.set_ttl(b"a", 100).unwrap();
    c.insert_batch(&[(b"a", &doc("a6"))]).unwrap();
    assert_eq!(c.ttl(b"a").unwrap(), None);

    // insert_auto writes through its own path but lands on a FRESH key —
    // which a caller could nevertheless have armed beforehand, since auto
    // keys are publicly observable zero-padded sequential ids. Arm the exact
    // key the next insert_auto will allocate; the write must clear it (a
    // fresh immortal document must never inherit a stale expiry).
    let cur_auto = c.insert_auto(&doc("auto")).unwrap();
    let id: u128 = std::str::from_utf8(&cur_auto).unwrap().parse().unwrap();
    let next_auto = format!("{:020}", id + 1).into_bytes();
    c.set_ttl(&next_auto, 100).unwrap();
    assert_eq!(c.ttl(&next_auto).unwrap(), Some(100));
    let landed = c.insert_auto(&doc("auto2")).unwrap();
    assert_eq!(landed, next_auto); // the prediction held
    assert_eq!(c.ttl(&landed).unwrap(), None); // the stale expiry was cleared
    assert_eq!(c.purge_expired(i64::MAX).unwrap(), 0);
    assert_eq!(c.get(&landed).unwrap(), Some(doc("auto2")));

    // delete removes both the row and its TTL entries.
    c.set_ttl(b"a", 100).unwrap();
    assert!(c.delete(b"a").unwrap());
    assert_eq!(c.ttl(b"a").unwrap(), None);
    assert_eq!(c.purge_expired(i64::MAX).unwrap(), 0);

    // The immortal idiom is still an expiry, due at now == i64::MAX.
    c.insert_with_ttl(b"forever", &doc("f"), i64::MAX).unwrap();
    assert_eq!(c.ttl(b"forever").unwrap(), Some(i64::MAX));
    assert_eq!(c.purge_expired(i64::MAX - 1).unwrap(), 0);
    assert_eq!(c.purge_expired(i64::MAX).unwrap(), 1);
    assert_eq!(c.get(b"forever").unwrap(), None);

    // Rewriting through insert_with_ttl re-arms (the only re-arm paths are
    // insert_with_ttl and set_ttl).
    c.insert(b"b", &doc("b1")).unwrap();
    c.insert_with_ttl(b"b", &doc("b2"), 7).unwrap();
    assert_eq!(c.ttl(b"b").unwrap(), Some(7));
    assert_eq!(c.purge_expired(7).unwrap(), 1);
}

// ===========================================================================
// Write-boundary name validation (TTL paths)
// ===========================================================================

/// The TTL write paths reject engine-reserved names (`ReservedCollection`)
/// and names that could forge engine namespaces (interior `__` →
/// `InvalidName`) — the same boundary as every other write (audit C7).
#[test]
fn ttl_write_paths_reject_reserved_and_invalid_collection_names() {
    let db = Db::open_in_memory().unwrap();
    assert!(matches!(
        db.collection("__ttl__docs").purge_expired(100).err(),
        Some(Error::ReservedCollection(_))
    ));
    assert!(matches!(
        db.collection("a__b").set_ttl(b"k", 100).err(),
        Some(Error::InvalidName(_))
    ));
    assert!(matches!(
        db.collection("__edges__docs").purge_expired(100).err(),
        Some(Error::ReservedCollection(_))
    ));
    // No rejected write created a user-visible collection.
    assert!(db.collections().unwrap().is_empty());
}
