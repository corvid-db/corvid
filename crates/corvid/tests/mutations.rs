//! Mutation conformance (Task 3): every mutation construct through the
//! public API only, at the plan's case standard — happy/edge/error/corner
//! per construct, values asserted (never bare `is_ok`/`is_err`), every
//! error case pinned to the exact `corvid::Error` variant, batch
//! atomicity, index maintenance, and one event observation per mutation
//! kind (the full events matrix is Task 12).
//!
//! Contract notes (read from `src/db.rs` before asserting): `insert` puts
//! (overwrites); `insert_batch` is one transaction whose schema/unique
//! checks see earlier items — a violating item rolls the whole batch back,
//! while a repeated KEY inside a non-violating batch simply overwrites like
//! `insert` does; `insert_auto` reserves its id inside the insert
//! transaction, so a failed insert does not burn an id; `update` passes
//! `Option<Value>` (missing docs are not an error); `patch` merges
//! TOP-LEVEL map fields only (nested maps and arrays are replaced
//! wholesale); `compare_and_set` compares encoded bytes, so `NaN` equals
//! itself and `-0.0` is distinct from `0.0`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use corvid::schema::{Field, FieldType, Schema};
use corvid::{ChangeEvent, ChangeKind, Db, Error, Value, field};

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

#[test]
fn mutations_smoke_insert_roundtrips() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    assert!(c.is_empty().unwrap());

    // Insert then read back: the stored document equals what was written.
    c.insert(b"k1", &doc("corvid", 8)).unwrap();
    assert_eq!(c.get(b"k1").unwrap(), Some(doc("corvid", 8)));
    assert_eq!(c.len().unwrap(), 1);

    // Overwrite is visible on the next read.
    c.insert(b"k1", &doc("corvid", 9)).unwrap();
    assert_eq!(c.get(b"k1").unwrap(), Some(doc("corvid", 9)));

    // Auto keys are distinct and sort in insertion order.
    let a = c.insert_auto(&doc("auto", 1)).unwrap();
    let b = c.insert_auto(&doc("auto", 2)).unwrap();
    assert!(a < b);
    assert_eq!(c.get(&a).unwrap(), Some(doc("auto", 1)));

    // Delete removes exactly the keyed document.
    assert!(c.delete(b"k1").unwrap());
    assert_eq!(c.get(b"k1").unwrap(), None);
    assert!(!c.delete(b"k1").unwrap());
    assert_eq!(c.len().unwrap(), 2);
}

// ===========================================================================
// insert
// ===========================================================================

/// Every `Value` variant round-trips exactly through insert/get, including
/// the boundary members: `i64::MIN`/`MAX`, `±inf`, `-0.0` (bit-exact), NaN
/// (bit-exact — derived `PartialEq` cannot express it), empty text/bytes/
/// vector/array/map, unicode text and map keys, and deep map nesting.
#[test]
fn mutations_insert_roundtrips_every_value_variant() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    assert!(c.is_empty().unwrap());

    let cases: Vec<(&[u8], Value)> = vec![
        (b"null", Value::Null),
        (b"bool-t", Value::Bool(true)),
        (b"bool-f", Value::Bool(false)),
        (b"int-zero", Value::Int(0)),
        (b"int-neg", Value::Int(-1)),
        (b"int-min", Value::Int(i64::MIN)),
        (b"int-max", Value::Int(i64::MAX)),
        (b"float", Value::Float(2.5)),
        (b"float-inf", Value::Float(f64::INFINITY)),
        (b"float-neg-inf", Value::Float(f64::NEG_INFINITY)),
        (b"text-empty", Value::Text(String::new())),
        (b"text-unicode", Value::Text("héllo 🐦 数".to_owned())),
        (b"bytes-empty", Value::Bytes(Vec::new())),
        (b"bytes-binary", Value::Bytes(vec![0, 1, 2, 255])),
        (b"vec-empty", Value::Vector(Vec::new())),
        (b"vec", Value::Vector(vec![0.0, -1.5, 3.25])),
        (b"array-empty", Value::Array(Vec::new())),
        (
            b"array-nested",
            Value::Array(vec![
                Value::Int(1),
                Value::Array(vec![Value::Text("inner".into()), Value::Null]),
                Value::Bytes(vec![7, 0, 7]),
            ]),
        ),
        (b"map-empty", Value::Map(BTreeMap::new())),
        (b"map-unicode-key", map(&[("键", Value::Int(1))])),
        (
            b"map-nested-deep",
            map(&[(
                "l1",
                map(&[(
                    "l2",
                    map(&[(
                        "l3",
                        map(&[
                            ("vec", Value::Vector(vec![1.0, 2.0])),
                            (
                                "tags",
                                Value::Array(vec![Value::Text("a".into()), Value::Bool(false)]),
                            ),
                        ]),
                    )]),
                )]),
            )]),
        ),
    ];
    for (key, value) in &cases {
        c.insert(key, value).unwrap();
        assert_eq!(c.get(key).unwrap(), Some(value.clone()), "key {key:?}");
    }
    assert_eq!(c.len().unwrap(), cases.len());

    // NaN is its own derived-PartialEq exception: compare bit patterns.
    c.insert(b"float-nan", &Value::Float(f64::NAN)).unwrap();
    assert!(matches!(
        c.get(b"float-nan").unwrap(),
        Some(Value::Float(f)) if f.is_nan()
    ));
    // -0.0 == 0.0 under PartialEq, so pin the exact bits instead.
    c.insert(b"float-neg-zero", &Value::Float(-0.0)).unwrap();
    assert!(matches!(
        c.get(b"float-neg-zero").unwrap(),
        Some(Value::Float(f)) if f.to_bits() == (-0.0f64).to_bits()
    ));
    assert_eq!(c.len().unwrap(), cases.len() + 2);
}

/// Insert overwrites an existing key in place; the empty key is a legal key
/// (readable, counted, and sorted before every non-empty key in scans).
#[test]
fn mutations_insert_overwrites_and_accepts_empty_key_and_empty_map() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // New key.
    c.insert(b"k", &doc("first", 1)).unwrap();
    assert_eq!(c.get(b"k").unwrap(), Some(doc("first", 1)));

    // Overwrite: the new document replaces the old, length unchanged.
    c.insert(b"k", &doc("second", 2)).unwrap();
    assert_eq!(c.get(b"k").unwrap(), Some(doc("second", 2)));
    assert_eq!(c.len().unwrap(), 1);

    // The empty key is storable and readable, and sorts first.
    c.insert(b"", &doc("empty-key", 3)).unwrap();
    assert_eq!(c.get(b"").unwrap(), Some(doc("empty-key", 3)));
    let keys: Vec<Vec<u8>> = c.scan().unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![Vec::new(), b"k".to_vec()]);
    assert_eq!(c.len().unwrap(), 2);
}

/// Write-boundary name validation (audit C7): a leading `__` is
/// `Error::ReservedCollection`; an interior `__` or a NUL byte is
/// `Error::InvalidName`. Nothing is stored under any rejected name.
#[test]
fn mutations_insert_rejects_reserved_and_invalid_collection_names() {
    let db = Db::open_in_memory().unwrap();
    for (name, variant) in [
        ("__edges__docs", "reserved"),
        ("a__b", "interior __"),
        ("doc\0s", "NUL byte"),
    ] {
        let c = db.collection(name);
        let err = c.insert(b"k", &doc("x", 1));
        match variant {
            "reserved" => assert!(
                matches!(err, Err(Error::ReservedCollection(_))),
                "{name:?} must be ReservedCollection, got {err:?}"
            ),
            _ => assert!(
                matches!(err, Err(Error::InvalidName(_))),
                "{name:?} must be InvalidName, got {err:?}"
            ),
        }
        assert_eq!(c.get(b"k").unwrap(), None, "nothing stored under {name:?}");
    }
    // A single underscore stays legal.
    db.collection("a_b").insert(b"k", &doc("x", 1)).unwrap();
}

// ===========================================================================
// insert_batch
// ===========================================================================

/// The batch path: a happy batch stores every item; an empty slice is a
/// legal no-op; a key that already exists (before the batch, or earlier IN
/// the batch) is overwritten exactly like a plain `insert` — the documented
/// put-semantics, distinct from the violation rollback tested next.
#[test]
fn mutations_insert_batch_happy_empty_overwrite_and_duplicates() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // Happy batch.
    c.insert_batch(&[
        (b"a", &doc("x", 1)),
        (b"b", &doc("y", 2)),
        (b"c", &doc("z", 3)),
    ])
    .unwrap();
    assert_eq!(c.get(b"a").unwrap(), Some(doc("x", 1)));
    assert_eq!(c.get(b"c").unwrap(), Some(doc("z", 3)));
    assert_eq!(c.len().unwrap(), 3);

    // Empty slice: Ok, and no state change.
    c.insert_batch(&[]).unwrap();
    assert_eq!(c.len().unwrap(), 3);

    // Pre-existing key: the batch item overwrites it.
    c.insert_batch(&[(b"a", &doc("replaced", 9)), (b"d", &doc("new", 4))])
        .unwrap();
    assert_eq!(c.get(b"a").unwrap(), Some(doc("replaced", 9)));
    assert_eq!(c.get(b"d").unwrap(), Some(doc("new", 4)));
    assert_eq!(c.len().unwrap(), 4);

    // Duplicate key INSIDE a non-violating batch: last write wins, and the
    // final state has exactly one row per distinct key.
    c.insert_batch(&[(b"dup", &doc("first", 1)), (b"dup", &doc("second", 2))])
        .unwrap();
    assert_eq!(c.get(b"dup").unwrap(), Some(doc("second", 2)));
    assert_eq!(c.len().unwrap(), 5);
}

/// A unique-constraint conflict — against a pre-existing row or between two
/// items of the same batch (the in-transaction check sees earlier items) —
/// rejects the WHOLE batch with `Error::SchemaViolation`: no partial state.
#[test]
fn mutations_insert_batch_unique_conflict_rolls_back_whole_batch() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.set_schema(
        &Schema::new()
            .field(Field::new("n", FieldType::Int))
            .field(Field::new("u", FieldType::Text).unique()),
    )
    .unwrap();
    c.create_scalar_index("u").unwrap();

    let udoc = |n: i64, u: &str| map(&[("n", Value::Int(n)), ("u", Value::Text(u.to_owned()))]);

    // Seed one row holding the unique value "taken".
    c.insert(b"seed", &udoc(0, "taken")).unwrap();

    // Conflict with the SEEDED row: the batch dies whole.
    let items: &[(&[u8], &Value)] = &[(b"ok1", &udoc(1, "fresh")), (b"clash", &udoc(2, "taken"))];
    let err = c.insert_batch(items).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "got {err:?}");
    assert_eq!(c.get(b"ok1").unwrap(), None, "no partial state: ok1 absent");
    assert_eq!(c.get(b"clash").unwrap(), None);
    assert_eq!(c.len().unwrap(), 1, "only the seed survives");

    // Conflict BETWEEN two items of the same batch: also whole-batch death.
    let err = c
        .insert_batch(&[(b"k1", &udoc(3, "twice")), (b"k2", &udoc(4, "twice"))])
        .unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "got {err:?}");
    assert_eq!(c.get(b"k1").unwrap(), None);
    assert_eq!(c.get(b"k2").unwrap(), None);
    assert_eq!(c.len().unwrap(), 1);
}

/// A schema violation anywhere in the batch rolls the whole batch back
/// (one transaction): items before the violator leave no partial state.
#[test]
fn mutations_insert_batch_schema_violation_rolls_back_whole_batch() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.set_schema(
        &Schema::new()
            .field(Field::new("n", FieldType::Int).required())
            .field(Field::new("tag", FieldType::Text)),
    )
    .unwrap();

    let bad = map(&[("n", Value::Text("not an int".into()))]);
    let items: &[(&[u8], &Value)] = &[
        (b"first", &doc("x", 1)),
        (b"bad", &bad),
        (b"third", &doc("z", 3)),
    ];
    let err = c.insert_batch(items).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "got {err:?}");
    // NO partial state: neither the items before the violator nor after.
    assert_eq!(c.get(b"first").unwrap(), None);
    assert_eq!(c.get(b"bad").unwrap(), None);
    assert_eq!(c.get(b"third").unwrap(), None);
    assert_eq!(c.len().unwrap(), 0);
    assert!(c.is_empty().unwrap());
}

// ===========================================================================
// insert_auto
// ===========================================================================

/// Auto keys are unique, zero-padded 20-digit decimals that sort in
/// allocation order, and each collection has an independent sequence.
#[test]
fn mutations_insert_auto_keys_are_unique_zero_padded_and_monotonic_per_collection() {
    let db = Db::open_in_memory().unwrap();
    let a = db.collection("a");
    let b = db.collection("b");

    let k0 = a.insert_auto(&doc("x", 0)).unwrap();
    let k1 = a.insert_auto(&doc("x", 1)).unwrap();
    let k2 = a.insert_auto(&doc("x", 2)).unwrap();
    assert_eq!(k0, b"00000000000000000000".to_vec());
    assert_eq!(k1, b"00000000000000000001".to_vec());
    assert_eq!(k2, b"00000000000000000002".to_vec());
    // Unique and monotonically increasing.
    assert!(k0 < k1 && k1 < k2);
    // Each document is readable at exactly its returned key.
    assert_eq!(a.get(&k1).unwrap(), Some(doc("x", 1)));
    assert_eq!(a.len().unwrap(), 3);

    // Independent sequence: collection b starts from id 0 again.
    let kb = b.insert_auto(&doc("y", 9)).unwrap();
    assert_eq!(kb, b"00000000000000000000".to_vec());
    assert_eq!(b.len().unwrap(), 1);
    assert_eq!(a.len().unwrap(), 3);
}

/// The id reservation commits with the row (audit C9): a schema-violating
/// document and a unique-constraint conflict both fail without burning the
/// id they would have taken — the next successful insert reuses it.
#[test]
fn mutations_insert_auto_failure_does_not_burn_an_id() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("events");
    c.set_schema(
        &Schema::new()
            .field(Field::new("n", FieldType::Int).required())
            .field(Field::new("u", FieldType::Text).unique()),
    )
    .unwrap();
    c.create_scalar_index("u").unwrap();
    let ev = |n: i64, u: &str| map(&[("n", Value::Int(n)), ("u", Value::Text(u.to_owned()))]);

    // Schema violation (rejected inside the transaction): no id burned.
    let bad = map(&[
        ("n", Value::Text("not an int".into())),
        ("u", Value::Text("?".into())),
    ]);
    let err = c.insert_auto(&bad).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "got {err:?}");
    assert_eq!(
        c.insert_auto(&ev(0, "a")).unwrap(),
        b"00000000000000000000".to_vec(),
        "id 0 must be reissued"
    );

    // Unique conflict against the committed row: no id burned either.
    let err = c.insert_auto(&ev(1, "a")).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "got {err:?}");
    assert_eq!(
        c.insert_auto(&ev(2, "b")).unwrap(),
        b"00000000000000000001".to_vec(),
        "id 1 must be reissued"
    );
}

// ===========================================================================
// update
// ===========================================================================

/// The transform's output becomes the stored document, whatever `Value`
/// kind it is; returning `None` deletes. There is no missing-document
/// error — see the next test for that contract.
#[test]
fn mutations_update_rewrites_document_to_every_value_kind() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"k", &doc("seed", 1)).unwrap();

    let rewrites: Vec<Value> = vec![
        Value::Null,
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::Float(-2.5),
        Value::Text("rewritten".to_owned()),
        Value::Bytes(vec![9, 0]),
        Value::Array(vec![Value::Int(1), Value::Null]),
        Value::Vector(vec![1.0, 2.0]),
        map(&[("fresh", Value::Bool(true))]),
    ];
    for new in &rewrites {
        c.update(b"k", |cur| {
            assert!(cur.is_some(), "the seeded document must be passed in");
            Some(new.clone())
        })
        .unwrap();
        assert_eq!(c.get(b"k").unwrap(), Some(new.clone()));
    }
    assert_eq!(c.len().unwrap(), 1);

    // Returning None deletes the document (and the delete is observable).
    c.update(b"k", |_| None).unwrap();
    assert_eq!(c.get(b"k").unwrap(), None);
    assert_eq!(c.len().unwrap(), 0);
}

/// `update` on a missing key is NOT an error: the transform receives `None`
/// and decides — `Some` creates the document at that key, `None` leaves the
/// collection untouched. (No missing-doc error variant exists on this API;
/// this pins the actual contract.)
#[test]
fn mutations_update_on_missing_key_creates_or_stays_absent() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    let mut saw_none = false;
    c.update(b"absent", |cur| {
        saw_none = cur.is_none();
        Some(doc("created-by-update", 7))
    })
    .unwrap();
    assert!(
        saw_none,
        "the transform must receive None for a missing doc"
    );
    assert_eq!(c.get(b"absent").unwrap(), Some(doc("created-by-update", 7)));
    assert_eq!(c.len().unwrap(), 1);

    // None from the transform: still Ok, still absent, nothing created.
    c.update(b"never-exists", |_| None).unwrap();
    assert_eq!(c.get(b"never-exists").unwrap(), None);
    assert_eq!(c.len().unwrap(), 1);
}

/// A scalar index stays exact across update-to-value and update-to-delete:
/// the old value stops matching, the new one starts, a deleted row matches
/// nothing.
#[test]
fn mutations_update_maintains_scalar_index() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.create_scalar_index("n").unwrap();
    c.insert(b"a", &doc("x", 1)).unwrap();
    c.insert(b"b", &doc("y", 2)).unwrap();

    let keys_eq = |n: i64| -> Vec<Vec<u8>> {
        let mut ks: Vec<Vec<u8>> = c
            .query()
            .filter(field("n").eq(Value::Int(n)))
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        ks.sort();
        ks
    };

    c.update(b"a", |cur| {
        let mut m = match cur.unwrap() {
            Value::Map(m) => m,
            _ => unreachable!("seeded a map"),
        };
        m.insert("n".to_owned(), Value::Int(9));
        Some(Value::Map(m))
    })
    .unwrap();
    assert_eq!(
        keys_eq(1),
        Vec::<Vec<u8>>::new(),
        "old value no longer matches"
    );
    assert_eq!(keys_eq(9), vec![b"a".to_vec()], "new value matches");
    assert_eq!(keys_eq(2), vec![b"b".to_vec()], "untouched row unaffected");

    // Update-to-delete removes the row from the index too.
    c.update(b"b", |_| None).unwrap();
    assert_eq!(keys_eq(2), Vec::<Vec<u8>>::new());
    assert_eq!(c.len().unwrap(), 1);
}

// ===========================================================================
// patch
// ===========================================================================

/// The actual patch contract (src/db.rs): TOP-LEVEL map fields merge
/// (patch fields overwrite same-named ones, everything else survives);
/// nested maps and arrays are REPLACED wholesale, never deep-merged; a
/// non-map on either side replaces the document; a missing key is created
/// with the patch as its document.
#[test]
fn mutations_patch_merges_top_level_and_replaces_non_maps() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // Top-level merge: overwrite "n", add "extra", keep "name".
    c.insert(
        b"m",
        &map(&[
            ("name", Value::Text("corvid".into())),
            ("n", Value::Int(1)),
            ("keep", Value::Bool(true)),
        ]),
    )
    .unwrap();
    c.patch(b"m", &map(&[("n", Value::Int(2)), ("extra", Value::Null)]))
        .unwrap();
    assert_eq!(
        c.get(b"m").unwrap(),
        Some(map(&[
            ("name", Value::Text("corvid".into())),
            ("n", Value::Int(2)),
            ("keep", Value::Bool(true)),
            ("extra", Value::Null),
        ]))
    );

    // A nested map in the patch REPLACES the whole nested map — the old
    // inner keys are gone, not merged.
    c.insert(
        b"nested",
        &map(&[(
            "cfg",
            map(&[("depth", Value::Int(3)), ("old", Value::Bool(true))]),
        )]),
    )
    .unwrap();
    c.patch(
        b"nested",
        &map(&[("cfg", map(&[("depth", Value::Int(4))]))]),
    )
    .unwrap();
    assert_eq!(
        c.get(b"nested").unwrap(),
        Some(map(&[("cfg", map(&[("depth", Value::Int(4))]))])),
        "nested maps replace wholesale (no deep merge)"
    );

    // Arrays replace wholesale too.
    c.insert(b"arr", &map(&[("tags", Value::Array(vec![Value::Int(1)]))]))
        .unwrap();
    c.patch(
        b"arr",
        &map(&[(
            "tags",
            Value::Array(vec![Value::Text("x".into()), Value::Null]),
        )]),
    )
    .unwrap();
    assert_eq!(
        c.get(b"arr").unwrap(),
        Some(map(&[(
            "tags",
            Value::Array(vec![Value::Text("x".into()), Value::Null])
        )]))
    );

    // Missing key: created with the patch as its document.
    c.patch(b"created", &doc("from-patch", 5)).unwrap();
    assert_eq!(c.get(b"created").unwrap(), Some(doc("from-patch", 5)));

    // Non-map current document: replaced by the (map) patch.
    c.insert(b"scalar", &Value::Int(41)).unwrap();
    c.patch(b"scalar", &map(&[("now", Value::Bool(true))]))
        .unwrap();
    assert_eq!(
        c.get(b"scalar").unwrap(),
        Some(map(&[("now", Value::Bool(true))]))
    );

    // Non-map patch: replaces the whole document.
    c.patch(b"m", &Value::Text("not a map".into())).unwrap();
    assert_eq!(c.get(b"m").unwrap(), Some(Value::Text("not a map".into())));

    // Empty-map patch: no fields change an existing map…
    c.patch(b"arr", &Value::Map(BTreeMap::new())).unwrap();
    assert_eq!(
        c.get(b"arr").unwrap(),
        Some(map(&[(
            "tags",
            Value::Array(vec![Value::Text("x".into()), Value::Null])
        )]))
    );
    // …and on a missing key it creates an empty map document.
    c.patch(b"empty-created", &Value::Map(BTreeMap::new()))
        .unwrap();
    assert_eq!(
        c.get(b"empty-created").unwrap(),
        Some(Value::Map(BTreeMap::new()))
    );
}

// ===========================================================================
// compare_and_set
// ===========================================================================

/// Match applies the swap and returns true; mismatch returns false with the
/// stored value untouched; `expected = None` means insert-if-absent; `new =
/// None` is a conditional delete. Comparison is byte-level on the encoded
/// value, so `NaN` matches itself and `-0.0` does NOT match `0.0`.
#[test]
fn mutations_compare_and_set_swap_noop_delete_and_bitwise_float_equality() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    // Mismatch: not applied, nothing written, false returned.
    c.insert(b"k", &doc("a", 1)).unwrap();
    assert!(
        !c.compare_and_set(b"k", Some(&doc("a", 999)), Some(doc("a", 2)))
            .unwrap()
    );
    assert_eq!(c.get(b"k").unwrap(), Some(doc("a", 1)));

    // Match: applied, value swapped.
    assert!(
        c.compare_and_set(b"k", Some(&doc("a", 1)), Some(doc("a", 2)))
            .unwrap()
    );
    assert_eq!(c.get(b"k").unwrap(), Some(doc("a", 2)));

    // Insert-if-absent: expected None fails on an existing key…
    assert!(!c.compare_and_set(b"k", None, Some(doc("a", 3))).unwrap());
    assert_eq!(c.get(b"k").unwrap(), Some(doc("a", 2)));
    // …and succeeds on an absent one.
    assert!(
        c.compare_and_set(b"fresh", None, Some(doc("z", 7)))
            .unwrap()
    );
    assert_eq!(c.get(b"fresh").unwrap(), Some(doc("z", 7)));

    // Conditional delete: mismatch leaves the row, match removes it.
    assert!(!c.compare_and_set(b"k", Some(&doc("a", 1)), None).unwrap());
    assert_eq!(c.get(b"k").unwrap(), Some(doc("a", 2)));
    assert!(c.compare_and_set(b"k", Some(&doc("a", 2)), None).unwrap());
    assert_eq!(c.get(b"k").unwrap(), None);

    // Byte-level equality corners. Stored NaN: an expected NaN matches.
    c.insert(b"nan", &Value::Float(f64::NAN)).unwrap();
    assert!(
        c.compare_and_set(b"nan", Some(&Value::Float(f64::NAN)), Some(Value::Int(1)))
            .unwrap()
    );
    assert_eq!(c.get(b"nan").unwrap(), Some(Value::Int(1)));

    // Stored -0.0: expected +0.0 does NOT match (distinct encodings)…
    c.insert(b"negz", &Value::Float(-0.0)).unwrap();
    assert!(
        !c.compare_and_set(b"negz", Some(&Value::Float(0.0)), Some(Value::Int(2)))
            .unwrap()
    );
    assert!(matches!(
        c.get(b"negz").unwrap(),
        Some(Value::Float(f)) if f.to_bits() == (-0.0f64).to_bits()
    ));
    // …while the exact -0.0 expectation does.
    assert!(
        c.compare_and_set(b"negz", Some(&Value::Float(-0.0)), Some(Value::Int(2)))
            .unwrap()
    );
    assert_eq!(c.get(b"negz").unwrap(), Some(Value::Int(2)));
}

/// A scalar index follows a successful swap (old value stops matching, new
/// value starts) and a conditional delete.
#[test]
fn mutations_compare_and_set_maintains_scalar_index() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.create_scalar_index("n").unwrap();
    c.insert(b"k", &doc("a", 1)).unwrap();

    // Failed swap changes nothing.
    assert!(
        !c.compare_and_set(b"k", Some(&doc("a", 999)), Some(doc("a", 2)))
            .unwrap()
    );
    let keys = |n: i64| {
        c.query()
            .filter(field("n").eq(Value::Int(n)))
            .run()
            .unwrap()
            .len()
    };
    assert_eq!(keys(1), 1);

    // Successful swap moves the index entry.
    assert!(
        c.compare_and_set(b"k", Some(&doc("a", 1)), Some(doc("a", 2)))
            .unwrap()
    );
    assert_eq!(keys(1), 0);
    assert_eq!(keys(2), 1);

    // Conditional delete clears it.
    assert!(c.compare_and_set(b"k", Some(&doc("a", 2)), None).unwrap());
    assert_eq!(keys(2), 0);
    assert!(c.is_empty().unwrap());
}

// ===========================================================================
// delete / delete_batch / delete_where
// ===========================================================================

/// Delete reports true for an existing row and false for a missing one, and
/// the state is gone from every observable: get, scan, len, count.
#[test]
fn mutations_delete_removes_state_from_get_scan_and_count() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc("x", 1)).unwrap();
    c.insert(b"b", &doc("y", 2)).unwrap();

    assert!(c.delete(b"a").unwrap());
    assert!(
        !c.delete(b"a").unwrap(),
        "second delete of the same key: false"
    );
    assert_eq!(c.get(b"a").unwrap(), None);
    assert_eq!(
        c.scan().unwrap(),
        vec![(b"b".to_vec(), doc("y", 2))],
        "scan excludes the deleted row"
    );
    assert_eq!(c.len().unwrap(), 1);
    assert_eq!(c.query().count().unwrap(), 1);
}

/// delete_batch returns how many keys existed (missing keys are skipped),
/// and an empty slice is a legal no-op returning 0.
#[test]
fn mutations_delete_batch_counts_existing_only_and_accepts_empty() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc("x", 1)).unwrap();
    c.insert(b"b", &doc("y", 2)).unwrap();
    c.insert(b"c", &doc("z", 3)).unwrap();

    let keys: &[&[u8]] = &[b"a", b"missing", b"c"];
    let removed = c.delete_batch(keys).unwrap();
    assert_eq!(removed, 2, "only the two existing keys count");
    assert_eq!(c.get(b"a").unwrap(), None);
    assert_eq!(c.get(b"c").unwrap(), None);
    assert_eq!(c.get(b"b").unwrap(), Some(doc("y", 2)));
    assert_eq!(c.len().unwrap(), 1);

    assert_eq!(c.delete_batch(&[]).unwrap(), 0);
    assert_eq!(c.len().unwrap(), 1);
}

/// delete_where returns the exact match count for 0, partial, and full
/// predicates (no index involved), leaving the complement intact.
#[test]
fn mutations_delete_where_counts_zero_partial_and_full() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert_batch(&[
        (b"a", &doc("x", 1)),
        (b"b", &doc("y", 2)),
        (b"c", &doc("z", 3)),
    ])
    .unwrap();

    // Zero matches: 0 removed, nothing else disturbed.
    assert_eq!(c.delete_where(field("n").eq(Value::Int(99))).unwrap(), 0);
    assert_eq!(c.len().unwrap(), 3);

    // Partial: n >= 2 removes exactly b and c.
    assert_eq!(c.delete_where(field("n").ge(Value::Int(2))).unwrap(), 2);
    assert_eq!(c.get(b"b").unwrap(), None);
    assert_eq!(c.get(b"c").unwrap(), None);
    assert_eq!(c.get(b"a").unwrap(), Some(doc("x", 1)));
    assert_eq!(c.len().unwrap(), 1);

    // Full: a predicate covering the remaining set empties the collection.
    assert_eq!(
        c.delete_where(field("n").ge(Value::Int(i64::MIN))).unwrap(),
        1
    );
    assert!(c.is_empty().unwrap());
}

/// With a scalar index on the filtered field, delete_where stays exact: the
/// index serves the predicate, the deletes maintain it, and a post-delete
/// filtered query returns precisely the survivors.
#[test]
fn mutations_delete_where_exact_with_scalar_index_present() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.create_scalar_index("n").unwrap();
    c.insert_batch(&[
        (b"a", &doc("x", 1)),
        (b"b", &doc("y", 2)),
        (b"c", &doc("z", 3)),
        (b"d", &doc("w", 4)),
    ])
    .unwrap();

    assert_eq!(c.delete_where(field("n").ge(Value::Int(3))).unwrap(), 2);

    let mut survivors: Vec<Vec<u8>> = c
        .query()
        .filter(field("n").ge(Value::Int(1)))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    survivors.sort();
    assert_eq!(survivors, vec![b"a".to_vec(), b"b".to_vec()]);
    // The deleted values no longer answer an index-served query.
    assert_eq!(
        c.query()
            .filter(field("n").ge(Value::Int(3)))
            .run()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(c.len().unwrap(), 2);
}

// ===========================================================================
// insert_with_ttl (basics; the deep TTL matrix is Task 12)
// ===========================================================================

/// The TTL basics through the mutation surface: the expiry is observable
/// via `ttl`, the row behaves normally until a purge, the boundary
/// `now == expires_at` is due, and a plain overwrite clears the expiry.
#[test]
fn mutations_insert_with_ttl_sets_and_purges_expiry() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    c.insert_with_ttl(b"k", &doc("ephemeral", 1), 100).unwrap();
    assert_eq!(c.get(b"k").unwrap(), Some(doc("ephemeral", 1)));
    assert_eq!(c.ttl(b"k").unwrap(), Some(100));

    // Just before the boundary: not due.
    assert_eq!(c.purge_expired(99).unwrap(), 0);
    assert_eq!(c.get(b"k").unwrap(), Some(doc("ephemeral", 1)));
    // At the boundary: due, and gone through the normal delete path.
    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(c.get(b"k").unwrap(), None);
    assert_eq!(c.ttl(b"k").unwrap(), None);

    // A plain overwrite clears the expiry: the row becomes immortal.
    c.insert_with_ttl(b"k2", &doc("short-lived", 2), 50)
        .unwrap();
    c.insert(b"k2", &doc("rewritten", 3)).unwrap();
    assert_eq!(c.ttl(b"k2").unwrap(), None);
    assert_eq!(c.purge_expired(1000).unwrap(), 0);
    assert_eq!(c.get(b"k2").unwrap(), Some(doc("rewritten", 3)));
}

// ===========================================================================
// Events, one observation per mutation kind (the full matrix is Task 12)
// ===========================================================================

/// Every applied mutation emits exactly one `ChangeEvent` (Insert for
/// writes, Delete for removals) with the collection, key, and kind;
/// non-applied operations (failed CAS, delete of a missing key) emit
/// nothing.
#[test]
fn mutations_emit_change_events_per_mutation_kind() {
    let db = Db::open_in_memory().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));
    let c = db.collection("docs");

    let ev = |key: &[u8], kind: ChangeKind| ChangeEvent {
        collection: "docs".to_owned(),
        key: key.to_vec(),
        kind,
    };
    let mut expected: Vec<ChangeEvent> = Vec::new();

    c.insert(b"i1", &doc("x", 1)).unwrap();
    expected.push(ev(b"i1", ChangeKind::Insert));

    c.insert_batch(&[(b"b1", &doc("x", 1)), (b"b2", &doc("x", 2))])
        .unwrap();
    expected.push(ev(b"b1", ChangeKind::Insert));
    expected.push(ev(b"b2", ChangeKind::Insert));

    let auto = c.insert_auto(&doc("x", 3)).unwrap();
    expected.push(ev(&auto, ChangeKind::Insert));

    c.update(b"i1", |cur| {
        let mut m = match cur.unwrap() {
            Value::Map(m) => m,
            _ => unreachable!("seeded a map"),
        };
        m.insert("n".to_owned(), Value::Int(9));
        Some(Value::Map(m))
    })
    .unwrap();
    expected.push(ev(b"i1", ChangeKind::Insert));

    c.patch(b"i1", &map(&[("extra", Value::Bool(true))]))
        .unwrap();
    expected.push(ev(b"i1", ChangeKind::Insert));

    // Post-patch document: {extra: true, name: "x", n: 9}.
    let patched = map(&[
        ("extra", Value::Bool(true)),
        ("name", Value::Text("x".into())),
        ("n", Value::Int(9)),
    ]);
    assert!(
        c.compare_and_set(b"i1", Some(&patched), Some(doc("swapped", 9)))
            .unwrap()
    );
    expected.push(ev(b"i1", ChangeKind::Insert));

    // A non-applied CAS emits nothing.
    assert!(
        !c.compare_and_set(b"i1", Some(&doc("stale", 0)), Some(doc("no", 0)))
            .unwrap()
    );

    c.delete(b"i1").unwrap();
    expected.push(ev(b"i1", ChangeKind::Delete));

    // Delete of a missing key emits nothing.
    c.delete(b"missing").unwrap();

    let keys: &[&[u8]] = &[b"b1", b"absent", b"b2"];
    let removed = c.delete_batch(keys).unwrap();
    assert_eq!(removed, 2);
    expected.push(ev(b"b1", ChangeKind::Delete));
    expected.push(ev(b"b2", ChangeKind::Delete));

    c.insert(b"w1", &doc("x", 1)).unwrap();
    expected.push(ev(b"w1", ChangeKind::Insert));
    c.insert(b"w2", &doc("x", 2)).unwrap();
    expected.push(ev(b"w2", ChangeKind::Insert));
    let n = c.delete_where(field("n").ge(Value::Int(1))).unwrap();
    assert_eq!(n, 3); // w1, w2, and the insert_auto row
    // Deletes run in key order: the zero-padded auto key sorts before "w*".
    expected.push(ev(&auto, ChangeKind::Delete));
    expected.push(ev(b"w1", ChangeKind::Delete));
    expected.push(ev(b"w2", ChangeKind::Delete));

    assert_eq!(*log.lock().unwrap(), expected);
}

// ===========================================================================
// Write-boundary name validation across every mutation construct
// ===========================================================================

/// Every mutation path rejects engine-reserved names (leading `__` →
/// `Error::ReservedCollection`) and names that could forge engine
/// namespaces (interior `__` / NUL → `Error::InvalidName`) before any
/// storage is touched (audit C7).
#[test]
fn mutations_write_paths_reject_reserved_and_invalid_collection_names() {
    let db = Db::open_in_memory().unwrap();
    let reserved = db.collection("__edges__docs");
    let forged = db.collection("a__b");
    let nul = db.collection("doc\0s");
    let predicate = field("n").eq(Value::Int(1));

    for err in [
        reserved.insert(b"k", &doc("x", 1)).err(),
        reserved.insert_batch(&[(b"k", &doc("x", 1))]).err(),
        reserved.insert_auto(&doc("x", 1)).err(),
        reserved.insert_with_ttl(b"k", &doc("x", 1), 100).err(),
        reserved.update(b"k", |_| None).err(),
        reserved.patch(b"k", &doc("x", 1)).err(),
        reserved
            .compare_and_set(b"k", None, Some(doc("x", 1)))
            .err(),
        reserved.delete(b"k").err(),
        reserved.delete_batch(&[b"k"]).err(),
        reserved.delete_where(predicate).err(),
        reserved.set_ttl(b"k", 100).err(),
    ] {
        assert!(
            matches!(err, Some(Error::ReservedCollection(_))),
            "reserved name must be ReservedCollection, got {err:?}"
        );
    }

    for err in [
        forged.insert(b"k", &doc("x", 1)).err(),
        forged.delete(b"k").err(),
        forged.insert_auto(&doc("x", 1)).err(),
        nul.insert(b"k", &doc("x", 1)).err(),
    ] {
        assert!(
            matches!(err, Some(Error::InvalidName(_))),
            "invalid name must be InvalidName, got {err:?}"
        );
    }

    // No rejected write created a user-visible collection.
    let names = db.collections().unwrap();
    assert_eq!(names, Vec::<String>::new());
}
