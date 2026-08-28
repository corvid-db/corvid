//! Schema/index conformance (Task 11): creation and registration semantics
//! of every index family through the public API only — empty/populated
//! creation, duplicate re-creation (replacement), mixed-type corpora,
//! index-vs-scan result equivalence per family, index observability via
//! plan_shape/explain, the maintenance contract under every mutation kind
//! (including the compound trailing-field ruling: stale entries must never
//! surface), unique constraints (exact variant, NaN==NaN, containers, null),
//! index-level name validation, and dead-fraction compaction observed only
//! through public deletes.
//!
//! Contract notes pinned by these tests (read from `src/` first):
//! * every `create_*` index API is create-OR-REPLACE: a second call with the
//!   same or different parameters succeeds and installs a fresh index (no
//!   error, no stale entries from the previous definition);
//! * scalar indexes encode Bool/numbers/text into disjoint ordered lanes;
//!   bytes/vectors/containers/null are not encodable and fall back to scans;
//! * a compound index serves only queries whose constraints cover EVERY
//!   indexed field (leading equality prefix, optional range/eq tail);
//!   prefix-only queries decline to a scan — that gate is what keeps
//!   documents missing a trailing indexed field correct (W2 fix-round);
//! * text indexes skip documents whose indexed field is not `Text`;
//! * a vector index pins its dimension from the first vector it sees (in key
//!   order for a lazy backfill); other-dimension documents are excluded from
//!   the index and served by the exact fallback, matching an unindexed twin;
//! * unique equality is the engine-wide storage equality
//!   (`schema::unique_value_eq`, shared with `compare_and_set`): NaN==NaN,
//!   containers element-wise, but `Int(7)` and `Float(7.0)` stay DISTINCT
//!   values — with or without a scalar index on the field;
//! * `Null` at a unique field is exempt from the uniqueness check (many docs
//!   may hold `Null`), and is accepted for any declared type unless the
//!   field is `required`;
//! * index APIs take FIELD names, validated by the same rules as collection
//!   names: interior `__` / NUL byte → `Error::InvalidName`, a `__`-prefixed
//!   collection → `Error::ReservedCollection`. The EMPTY field name is
//!   accepted (it is a legal map key) and functional.

use std::collections::BTreeMap;

use corvid::schema::{Field, FieldType, Schema};
use corvid::{
    Collection, Db, Error, Hit, Metric, PlanShape, Predicate, Quantization, QueryBuilder,
    ResultRow, TextHit, Value, field,
};

// ===========================================================================
// helpers (twin harness per filters.rs conventions)
// ===========================================================================

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Map(m)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// Sorted result keys of a query — tests assert SETS, order-free.
fn keys(rows: &[ResultRow]) -> Vec<Vec<u8>> {
    let mut ks: Vec<Vec<u8>> = rows.iter().map(|r| r.key.clone()).collect();
    ks.sort();
    ks
}

/// Expected key sets by document name. Sorted HERE so call sites may list
/// names in any order — set assertions never depend on caller discipline.
fn ks(names: &[&str]) -> Vec<Vec<u8>> {
    let mut names: Vec<&str> = names.to_vec();
    names.sort_unstable();
    names.into_iter().map(|n| n.as_bytes().to_vec()).collect()
}

fn matching_keys(c: &Collection<'_>, p: &Predicate) -> Vec<Vec<u8>> {
    keys(&c.query().filter(p.clone()).run().unwrap())
}

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> Collection<'a> {
    let c = db.collection(name);
    for (k, d) in docs {
        c.insert(k, d).unwrap();
    }
    c
}

/// `doc` with one top-level field removed (drives update/patch/CAS shapes
/// that make a compound TRAILING field disappear).
fn without_field(doc: &Value, field: &str) -> Value {
    match doc {
        Value::Map(m) => {
            let mut m = m.clone();
            m.remove(field);
            Value::Map(m)
        }
        other => other.clone(),
    }
}

/// (key, distance) pairs of vector hits — the comparable projection across
/// the indexed (`approximate: true`) and exact (`approximate: false`) arms.
fn vhits(hits: &[Hit]) -> Vec<(Vec<u8>, f32)> {
    hits.iter().map(|h| (h.key.clone(), h.distance)).collect()
}

/// (key, score) pairs of text hits.
fn thits(hits: &[TextHit]) -> Vec<(Vec<u8>, f32)> {
    hits.iter().map(|h| (h.key.clone(), h.score)).collect()
}

fn scan_of(coll: &str) -> PlanShape {
    PlanShape::Scan {
        collection: coll.to_owned(),
    }
}

// ===========================================================================
// create_scalar_index
// ===========================================================================

#[test]
fn schema_scalar_index_empty_collection_creates_and_serves_later() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("empty_idx");
    assert_eq!(c.len().unwrap(), 0);
    // Creation over an empty collection succeeds (nothing to backfill).
    c.create_scalar_index("n").unwrap();
    // The index is registered and serviceable even before any document
    // exists: an equality window predicts the indexed arm.
    assert_eq!(
        c.query().filter(field("n").eq(Value::Int(1))).plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    // Documents inserted after creation are immediately queryable through it.
    c.insert(b"a", &map(&[("n", Value::Int(1))])).unwrap();
    c.insert(b"b", &map(&[("n", Value::Int(2))])).unwrap();
    assert_eq!(matching_keys(&c, &field("n").eq(Value::Int(1))), ks(&["a"]));
    assert_eq!(matching_keys(&c, &field("n").ge(Value::Int(2))), ks(&["b"]));
}

#[test]
fn schema_scalar_index_backfill_makes_populated_collection_immediately_queryable() {
    // A fixed corpus where the expected sets are enumerable by hand.
    let owned: Vec<(Vec<u8>, Value)> = (0..50i64)
        .map(|i| {
            (
                format!("k{i:02}").into_bytes(),
                map(&[("n", Value::Int(i))]),
            )
        })
        .collect();
    let docs: Vec<(&[u8], Value)> = owned
        .iter()
        .map(|(k, v)| (k.as_slice(), v.clone()))
        .collect();
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "docs", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "docs", &docs);
    // Created AFTER the corpus: the lazy backfill makes it immediately
    // queryable — no explicit "build" step exists in the public API.
    idx.create_scalar_index("n").unwrap();

    // Observability: the created index is USED (queries drive it).
    assert_eq!(
        idx.query()
            .filter(field("n").eq(Value::Int(7)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert!(
        idx.query()
            .filter(field("n").eq(Value::Int(7)))
            .explain()
            .starts_with("indexed-window(scalar)"),
        "explain must name the scalar family"
    );

    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        (field("n").eq(Value::Int(0)), vec!["k00"]),
        (field("n").eq(Value::Int(49)), vec!["k49"]),
        (field("n").eq(Value::Int(50)), vec![]),
        (
            field("n").lt(Value::Int(5)),
            vec!["k00", "k01", "k02", "k03", "k04"],
        ),
        (
            field("n").ge(Value::Int(45)),
            vec!["k45", "k46", "k47", "k48", "k49"],
        ),
        (
            field("n").between(Value::Int(10), Value::Int(12)),
            vec!["k10", "k11", "k12"],
        ),
        (field("n").eq(Value::Float(25.0)), vec!["k25"]),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

#[test]
fn schema_scalar_index_duplicate_creation_replaces_without_stale_entries() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("dup");
    c.insert(b"a", &map(&[("n", Value::Int(1))])).unwrap();
    c.insert(b"b", &map(&[("n", Value::Int(2))])).unwrap();
    c.create_scalar_index("n").unwrap();
    // Mutate a value, then RE-CREATE the index over the changed corpus.
    c.insert(b"a", &map(&[("n", Value::Int(9))])).unwrap();
    c.create_scalar_index("n").unwrap(); // duplicate creation: replace, Ok

    // No stale entry for the old value, the new one is found, and each
    // document surfaces exactly once (the forward map dedupes).
    assert_eq!(matching_keys(&c, &field("n").eq(Value::Int(1))), ks(&[]));
    assert_eq!(matching_keys(&c, &field("n").eq(Value::Int(9))), ks(&["a"]));
    assert_eq!(matching_keys(&c, &field("n").eq(Value::Int(2))), ks(&["b"]));
    assert_eq!(
        matching_keys(&c, &field("n").ge(Value::Int(0))),
        ks(&["a", "b"])
    );
    // A third creation with the corpus unchanged is a no-op replacement.
    c.create_scalar_index("n").unwrap();
    assert_eq!(
        matching_keys(&c, &field("n").ge(Value::Int(0))),
        ks(&["a", "b"])
    );
}

/// One document per `Value` kind under the SAME indexed field, plus a doc
/// missing it: the scalar index covers only the encodable lanes (bool,
/// numbers, text); the others decline to a scan — and every predicate still
/// returns exactly what the unindexed twin returns.
#[test]
fn schema_scalar_index_mixed_type_field_lanes_and_missing_docs_match_scan() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"k-int", map(&[("x", Value::Int(7))])),
        (b"k-float", map(&[("x", Value::Float(7.0))])),
        (b"k-negfloat", map(&[("x", Value::Float(2.5))])),
        (b"k-text", map(&[("x", text("7"))])),
        (b"k-text2", map(&[("x", text("eight"))])),
        (b"k-bool", map(&[("x", Value::Bool(true))])),
        (b"k-bytes", map(&[("x", Value::Bytes(vec![1, 2, 255]))])),
        (b"k-vec", map(&[("x", Value::Vector(vec![1.0, 2.0]))])),
        (
            b"k-arr",
            map(&[("x", Value::Array(vec![Value::Int(1), text("x")]))]),
        ),
        (b"k-null", map(&[("x", Value::Null)])),
        (b"k-missing", map(&[("other", Value::Int(0))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "mixed", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "mixed", &docs);
    idx.create_scalar_index("x").unwrap();

    // Index built over the encodable kinds only — but every query below is
    // twin-equivalent regardless of lane, fallback, or missing field.
    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        // Numeric equality crosses Int/Float: both 7s match.
        (field("x").eq(Value::Int(7)), vec!["k-int", "k-float"]),
        (field("x").eq(Value::Float(7.0)), vec!["k-int", "k-float"]),
        (field("x").eq(text("7")), vec!["k-text"]),
        (field("x").eq(Value::Bool(true)), vec!["k-bool"]),
        (
            field("x").eq(Value::Bytes(vec![1, 2, 255])),
            vec!["k-bytes"],
        ),
        (field("x").eq(Value::Vector(vec![1.0, 2.0])), vec!["k-vec"]),
        // Ordered comparisons: numbers only, text in byte order.
        (field("x").gt(Value::Int(5)), vec!["k-int", "k-float"]),
        (field("x").lt(Value::Float(7.0)), vec!["k-negfloat"]),
        (field("x").ge(text("7")), vec!["k-text", "k-text2"]),
        (field("x").starts_with("eig"), vec!["k-text2"]),
        // Missing field: never matches any comparison (Ne included) — and
        // k-float (Float 7.0) DROPS OUT of Ne(Int 7) because numeric
        // equality holds (7 == 7.0), while k-null stays in (Null != 7).
        (
            field("x").ne(Value::Int(7)),
            vec![
                "k-arr",
                "k-bool",
                "k-bytes",
                "k-negfloat",
                "k-null",
                "k-text",
                "k-text2",
                "k-vec",
            ],
        ),
        (
            field("x").exists(),
            vec![
                "k-int",
                "k-float",
                "k-negfloat",
                "k-text",
                "k-text2",
                "k-bool",
                "k-bytes",
                "k-vec",
                "k-arr",
                "k-null",
            ],
        ),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

/// The maintenance contract: after creation, insert / update / patch / CAS /
/// delete on the indexed side keep filter results identical to an unindexed
/// twin driven through the same mutations.
#[test]
fn schema_scalar_index_maintenance_contract_under_every_mutation_kind() {
    let start: Vec<(&[u8], Value)> = vec![
        (b"m1", map(&[("n", Value::Int(1))])),
        (b"m2", map(&[("n", Value::Int(5))])),
        (b"m3", map(&[("other", Value::Bool(true))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "maint", &start);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "maint", &start);
    idx.create_scalar_index("n").unwrap();

    let probes = |c: &Collection<'_>| -> Vec<Vec<Vec<u8>>> {
        vec![
            matching_keys(c, &field("n").eq(Value::Int(1))),
            matching_keys(c, &field("n").eq(Value::Int(5))),
            matching_keys(c, &field("n").eq(Value::Int(9))),
            matching_keys(c, &field("n").ge(Value::Int(0))),
        ]
    };
    assert_eq!(probes(&scan), probes(&idx), "baseline");

    // insert: a new value is immediately queryable through the index.
    idx.insert(b"m4", &map(&[("n", Value::Int(9))])).unwrap();
    scan.insert(b"m4", &map(&[("n", Value::Int(9))])).unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after insert");

    // update: changing the value retargets the index entry.
    idx.update(b"m1", |_| Some(map(&[("n", Value::Int(5))])))
        .unwrap();
    scan.update(b"m1", |_| Some(map(&[("n", Value::Int(5))])))
        .unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after value change");

    // update removing the indexed field: the entry must leave the index.
    idx.update(b"m2", |d| Some(without_field(&d.unwrap(), "n")))
        .unwrap();
    scan.update(b"m2", |d| Some(without_field(&d.unwrap(), "n")))
        .unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after field removal");
    assert_eq!(
        matching_keys(&idx, &field("n").eq(Value::Int(5))),
        ks(&["m1"])
    );

    // patch (creating the doc) adds the field back.
    idx.patch(b"m3", &map(&[("n", Value::Int(1))])).unwrap();
    scan.patch(b"m3", &map(&[("n", Value::Int(1))])).unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after patch adds field");

    // patch retargets an existing entry.
    idx.patch(b"m4", &map(&[("n", Value::Int(1))])).unwrap();
    scan.patch(b"m4", &map(&[("n", Value::Int(1))])).unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after patch changes value");

    // compare_and_set with a matching expectation rewrites the entry.
    let expected = idx.get(b"m1").unwrap().unwrap();
    assert!(
        idx.compare_and_set(b"m1", Some(&expected), Some(map(&[("n", Value::Int(9))])))
            .unwrap()
    );
    let expected = scan.get(b"m1").unwrap().unwrap();
    assert!(
        scan.compare_and_set(b"m1", Some(&expected), Some(map(&[("n", Value::Int(9))])))
            .unwrap()
    );
    assert_eq!(probes(&scan), probes(&idx), "after CAS");
    // CAS deleting the doc removes the entry.
    assert!(
        idx.compare_and_set(
            b"m3",
            Some(&map(&[("n", Value::Int(1)), ("other", Value::Bool(true))])),
            None
        )
        .unwrap()
    );
    assert!(
        scan.compare_and_set(
            b"m3",
            Some(&map(&[("n", Value::Int(1)), ("other", Value::Bool(true))])),
            None
        )
        .unwrap()
    );
    assert_eq!(probes(&scan), probes(&idx), "after CAS delete");

    // delete: the key must stop matching.
    idx.delete(b"m4").unwrap();
    scan.delete(b"m4").unwrap();
    assert_eq!(probes(&scan), probes(&idx), "after delete");
    assert_eq!(
        matching_keys(&idx, &field("n").ge(Value::Int(0))),
        ks(&["m1"])
    );
}

// ===========================================================================
// create_compound_index
// ===========================================================================

/// Field ORDER matters: (a,b) and (b,a) are different indexes with different
/// serviceable query shapes — pinned via plan_shape, with result parity
/// against an unindexed twin either way.
#[test]
fn schema_compound_index_field_order_determines_serviceability() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"d1", map(&[("a", text("x")), ("b", Value::Int(1))])),
        (b"d2", map(&[("a", text("x")), ("b", Value::Int(5))])),
        (b"d3", map(&[("a", text("y")), ("b", Value::Int(1))])),
        (b"d4", map(&[("a", text("y")), ("b", Value::Int(5))])),
        (b"d5", map(&[("a", text("x"))])),     // missing b
        (b"d6", map(&[("b", Value::Int(1))])), // missing a
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "ord", &docs);
    let ab_db = Db::open_in_memory().unwrap();
    let ab = seed(&ab_db, "ord", &docs);
    ab.create_compound_index(&["a", "b"]).unwrap();
    let ba_db = Db::open_in_memory().unwrap();
    let ba = seed(&ba_db, "ord", &docs);
    ba.create_compound_index(&["b", "a"]).unwrap();

    // eq on a + range on b fully covers (a,b) → compound window; on (b,a) the
    // leading field b carries no equality → the gate declines → scan.
    fn eqa_rangeb<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("x")))
            .filter(field("b").ge(Value::Int(1)))
    }
    assert_eq!(
        eqa_rangeb(&ab).plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(eqa_rangeb(&ba).plan_shape(), scan_of("ord"));
    assert_eq!(eqa_rangeb(&scan).plan_shape(), scan_of("ord"));
    assert!(
        eqa_rangeb(&ab)
            .explain()
            .starts_with("indexed-window(compound)")
    );

    // The mirror query fully covers (b,a) but not (a,b).
    fn eqb_rangea<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("b").eq(Value::Int(1)))
            .filter(field("a").ge(text("x")))
    }
    assert_eq!(
        eqb_rangea(&ba).plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(eqb_rangea(&ab).plan_shape(), scan_of("ord"));

    // Results identical on all three collections for both shapes.
    let want = keys(&eqa_rangeb(&scan).run().unwrap());
    assert_eq!(
        keys(&eqa_rangeb(&ab).run().unwrap()),
        want,
        "eqa+rangeb on (a,b)"
    );
    assert_eq!(
        keys(&eqa_rangeb(&ba).run().unwrap()),
        want,
        "eqa+rangeb on (b,a)"
    );
    let want = keys(&eqb_rangea(&scan).run().unwrap());
    assert_eq!(
        keys(&eqb_rangea(&ab).run().unwrap()),
        want,
        "eqb+rangea on (a,b)"
    );
    assert_eq!(
        keys(&eqb_rangea(&ba).run().unwrap()),
        want,
        "eqb+rangea on (b,a)"
    );
    // And the exact expected sets (d5/d6 drop out: their missing field fails
    // one conjunct).
    assert_eq!(keys(&eqa_rangeb(&scan).run().unwrap()), ks(&["d1", "d2"]));
    assert_eq!(keys(&eqb_rangea(&scan).run().unwrap()), ks(&["d1", "d3"]));
}

/// Arity 1 and arity 3: a single-field compound serves equality AND range
/// windows on that field; a three-field index serves only when every field
/// is constrained (eq prefix + optional tail).
#[test]
fn schema_compound_index_single_and_three_field_arities() {
    // --- arity one ---
    let docs: Vec<(&[u8], Value)> = vec![
        (b"s1", map(&[("n", Value::Int(3))])),
        (b"s2", map(&[("n", Value::Int(7))])),
        (b"s3", map(&[("n", Value::Int(9))])),
        (b"s4", map(&[("other", Value::Bool(true))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "one", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "one", &docs);
    idx.create_compound_index(&["n"]).unwrap();
    // Equality fully covers the single field...
    assert_eq!(
        idx.query()
            .filter(field("n").eq(Value::Int(7)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    // ...and so does a range (prefix length 0 + tail on the only field).
    assert_eq!(
        idx.query()
            .filter(field("n").ge(Value::Int(5)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    for (p, want) in [
        (field("n").eq(Value::Int(7)), vec!["s2"]),
        (field("n").ge(Value::Int(5)), vec!["s2", "s3"]),
        (field("n").lt(Value::Int(4)), vec!["s1"]),
    ] {
        let want_keys = ks(&want);
        assert_eq!(matching_keys(&scan, &p), want_keys, "scan {p:?}");
        assert_eq!(matching_keys(&idx, &p), want_keys, "indexed {p:?}");
    }

    // --- arity three ---
    let docs3: Vec<(&[u8], Value)> = vec![
        (
            b"t1",
            map(&[
                ("a", text("p")),
                ("b", Value::Int(1)),
                ("c", Value::Int(10)),
            ]),
        ),
        (
            b"t2",
            map(&[
                ("a", text("p")),
                ("b", Value::Int(2)),
                ("c", Value::Int(20)),
            ]),
        ),
        (
            b"t3",
            map(&[
                ("a", text("q")),
                ("b", Value::Int(1)),
                ("c", Value::Int(30)),
            ]),
        ),
        (b"t4", map(&[("a", text("p")), ("b", Value::Int(2))])), // missing c
    ];
    let scan3_db = Db::open_in_memory().unwrap();
    let scan3 = seed(&scan3_db, "three", &docs3);
    let idx3_db = Db::open_in_memory().unwrap();
    let idx3 = seed(&idx3_db, "three", &docs3);
    idx3.create_compound_index(&["a", "b", "c"]).unwrap();

    fn all_eq<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("p")))
            .filter(field("b").eq(Value::Int(2)))
            .filter(field("c").eq(Value::Int(20)))
    }
    fn eq_eq_range<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("p")))
            .filter(field("b").eq(Value::Int(2)))
            .filter(field("c").ge(Value::Int(15)))
    }
    // Full coverage (every indexed field constrained) serves.
    assert_eq!(
        all_eq(&idx3).plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(
        eq_eq_range(&idx3).plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(keys(&all_eq(&idx3).run().unwrap()), ks(&["t2"]));
    assert_eq!(keys(&eq_eq_range(&idx3).run().unwrap()), ks(&["t2"]));

    // Prefix-only shapes (c unconstrained) DECLINE to a scan — the W2
    // full-coverage soundness gate, pinned from the public API: t4 matches
    // these filters but is not in the index (missing c), so a window over
    // the index would omit it.
    fn eq_eq<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("p")))
            .filter(field("b").eq(Value::Int(2)))
    }
    fn eq_range<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("p")))
            .filter(field("b").ge(Value::Int(2)))
    }
    assert_eq!(
        eq_eq(&idx3).plan_shape(),
        scan_of("three"),
        "eq+eq must decline"
    );
    assert_eq!(
        keys(&eq_eq(&idx3).run().unwrap()),
        keys(&eq_eq(&scan3).run().unwrap()),
        "eq+eq"
    );
    assert_eq!(
        eq_range(&idx3).plan_shape(),
        scan_of("three"),
        "eq+range must decline"
    );
    assert_eq!(
        keys(&eq_range(&idx3).run().unwrap()),
        keys(&eq_range(&scan3).run().unwrap()),
        "eq+range"
    );
    // t4 (missing c) IS a match of the prefix-only filters via the scan.
    assert_eq!(keys(&eq_eq(&scan3).run().unwrap()), ks(&["t2", "t4"]));
    assert_eq!(keys(&eq_range(&scan3).run().unwrap()), ks(&["t2", "t4"]));
}

/// Duplicate creation replaces; (a,b) and (b,a) coexist as distinct indexes
/// on one collection, and both stay correct.
#[test]
fn schema_compound_index_duplicate_and_reverse_order_coexist() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"k0", map(&[("a", text("g")), ("b", Value::Int(0))])),
        (b"k1", map(&[("a", text("g")), ("b", Value::Int(1))])),
        (b"k2", map(&[("a", text("g")), ("b", Value::Int(2))])),
        (b"k4", map(&[("a", text("g")), ("b", Value::Int(4))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "coex", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "coex", &docs);
    idx.create_compound_index(&["a", "b"]).unwrap();
    // Mutate a value, then duplicate creation: replace, no stale entries.
    idx.insert(b"k3", &map(&[("a", text("g")), ("b", Value::Int(3))]))
        .unwrap();
    scan.insert(b"k3", &map(&[("a", text("g")), ("b", Value::Int(3))]))
        .unwrap();
    idx.insert(b"k4", &map(&[("a", text("g")), ("b", Value::Int(93))]))
        .unwrap();
    scan.insert(b"k4", &map(&[("a", text("g")), ("b", Value::Int(93))]))
        .unwrap();
    idx.create_compound_index(&["a", "b"]).unwrap();
    // The reverse order registers as a SECOND, independent index.
    idx.create_compound_index(&["b", "a"]).unwrap();

    // (a,b) window over the re-created index: a eq + b range — the stale
    // (g,4) entry from before the overwrite must never surface.
    fn ab_query<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("a").eq(text("g")))
            .filter(field("b").ge(Value::Int(90)))
    }
    assert_eq!(
        keys(&ab_query(&idx).run().unwrap()),
        keys(&ab_query(&scan).run().unwrap()),
        "stale (g,4) entry must never surface after re-creation"
    );
    assert_eq!(keys(&ab_query(&idx).run().unwrap()), ks(&["k4"]));
    // (b,a) window over the independently registered reverse index.
    fn ba_query<'a>(c: &Collection<'a>) -> QueryBuilder<'a> {
        c.query()
            .filter(field("b").eq(Value::Int(93)))
            .filter(field("a").ge(text("a")))
    }
    assert_eq!(
        keys(&ba_query(&idx).run().unwrap()),
        keys(&ba_query(&scan).run().unwrap())
    );
    assert_eq!(keys(&ba_query(&idx).run().unwrap()), ks(&["k4"]));
    // Both arms still report the compound kind.
    assert_eq!(
        idx.query()
            .filter(field("a").eq(text("g")))
            .filter(field("b").ge(Value::Int(0)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(
        idx.query()
            .filter(field("b").eq(Value::Int(93)))
            .filter(field("a").ge(text("a")))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
}

/// The W2-routed deep case: update/patch/CAS that removes or ADDS a trailing
/// indexed field, then a prefix-only query — the scan path reflects the
/// removal (the doc still matches on its leading field), and full-coverage
/// queries never surface the stale entry.
#[test]
fn schema_compound_trailing_field_mutations_never_surface_stale_entries() {
    let start: Vec<(&[u8], Value)> = vec![
        (b"p1", map(&[("cat", text("red")), ("n", Value::Int(1))])),
        (b"p2", map(&[("cat", text("red")), ("n", Value::Int(2))])),
        (b"p3", map(&[("cat", text("blue")), ("n", Value::Int(1))])),
        (b"p4", map(&[("cat", text("red"))])), // missing trailing n
        (b"p5", map(&[("cat", text("blue")), ("n", Value::Int(5))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "trail", &start);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "trail", &start);
    idx.create_compound_index(&["cat", "n"]).unwrap();

    let prefix_red = field("cat").eq(text("red"));
    let full = |cat: &str, p: Predicate| field("cat").eq(text(cat)).and(p);

    // Baseline: prefix-only matches p4 (missing n) via the scan; the gate
    // keeps that query OFF the index.
    assert_eq!(
        idx.query().filter(prefix_red.clone()).plan_shape(),
        scan_of("trail")
    );
    assert_eq!(matching_keys(&idx, &prefix_red), ks(&["p1", "p2", "p4"]));
    assert_eq!(matching_keys(&scan, &prefix_red), ks(&["p1", "p2", "p4"]));

    // --- update REMOVES the trailing field from p2 ---
    idx.update(b"p2", |d| Some(without_field(&d.unwrap(), "n")))
        .unwrap();
    scan.update(b"p2", |d| Some(without_field(&d.unwrap(), "n")))
        .unwrap();
    // Prefix-only: p2 still matches (its leading field is intact).
    assert_eq!(matching_keys(&idx, &prefix_red), ks(&["p1", "p2", "p4"]));
    // Full coverage: the stale (red, 2) entry must NOT resurrect p2...
    assert_eq!(
        matching_keys(&idx, &full("red", field("n").eq(Value::Int(2)))),
        ks(&[]),
        "stale compound entry surfaced"
    );
    // ...and other n-constrained queries stay exact on both twins.
    for p in [
        field("n").eq(Value::Int(1)),
        field("n").ge(Value::Int(1)),
        field("n").lt(Value::Int(9)),
    ] {
        let q = full("red", p);
        assert_eq!(matching_keys(&idx, &q), matching_keys(&scan, &q), "{q:?}");
    }
    assert_eq!(
        matching_keys(&idx, &full("red", field("n").eq(Value::Int(1)))),
        ks(&["p1"])
    );

    // --- patch ADDS a trailing field to p4 (which lacked it) ---
    idx.patch(b"p4", &map(&[("n", Value::Int(9))])).unwrap();
    scan.patch(b"p4", &map(&[("n", Value::Int(9))])).unwrap();
    assert_eq!(
        matching_keys(&idx, &full("red", field("n").eq(Value::Int(9)))),
        ks(&["p4"]),
        "added trailing field must be findable through the index"
    );
    assert_eq!(matching_keys(&idx, &prefix_red), ks(&["p1", "p2", "p4"]));

    // --- CAS REMOVES the trailing field from p5 ---
    let expected = idx.get(b"p5").unwrap().unwrap();
    assert!(
        idx.compare_and_set(b"p5", Some(&expected), Some(without_field(&expected, "n")))
            .unwrap()
    );
    let expected = scan.get(b"p5").unwrap().unwrap();
    assert!(
        scan.compare_and_set(b"p5", Some(&expected), Some(without_field(&expected, "n")))
            .unwrap()
    );
    assert_eq!(
        matching_keys(&idx, &full("blue", field("n").eq(Value::Int(5)))),
        ks(&[]),
        "CAS removal must leave no stale (blue, 5) entry"
    );
    assert_eq!(
        matching_keys(&idx, &field("cat").eq(text("blue"))),
        ks(&["p3", "p5"]),
        "prefix-only still reflects the still-present leading field"
    );
    assert_eq!(
        matching_keys(&idx, &full("blue", field("n").eq(Value::Int(1)))),
        ks(&["p3"])
    );

    // --- CAS ADDS a trailing field to p2 (back) with a fresh value ---
    let expected = idx.get(b"p2").unwrap().unwrap();
    assert!(
        idx.compare_and_set(
            b"p2",
            Some(&expected),
            Some(map(&[("cat", text("red")), ("n", Value::Int(7))]))
        )
        .unwrap()
    );
    let expected = scan.get(b"p2").unwrap().unwrap();
    assert!(
        scan.compare_and_set(
            b"p2",
            Some(&expected),
            Some(map(&[("cat", text("red")), ("n", Value::Int(7))]))
        )
        .unwrap()
    );
    assert_eq!(
        matching_keys(&idx, &full("red", field("n").eq(Value::Int(7)))),
        ks(&["p2"])
    );
    assert_eq!(
        matching_keys(&idx, &full("red", field("n").eq(Value::Int(2)))),
        ks(&[])
    );

    // Final full-coverage parity sweep after all four mutations.
    for cat in ["red", "blue"] {
        for p in [
            field("n").eq(Value::Int(1)),
            field("n").eq(Value::Int(7)),
            field("n").eq(Value::Int(9)),
            field("n").ge(Value::Int(0)),
        ] {
            let q = full(cat, p);
            assert_eq!(matching_keys(&idx, &q), matching_keys(&scan, &q), "{q:?}");
        }
    }
}

// ===========================================================================
// create_text_index
// ===========================================================================

#[test]
fn schema_text_index_duplicate_and_non_text_values_excluded() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"t1", map(&[("body", text("the quick brown fox"))])),
        (b"t2", map(&[("body", text("the lazy dog"))])),
        (b"t3", map(&[("body", text("fox news daily"))])),
        (b"t4", map(&[("body", Value::Int(42))])), // non-text: not in corpus
        (b"t5", map(&[("other", text("fox here"))])), // different field
        (b"t6", map(&[("body", text(""))])),       // empty text IS corpus
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "txt", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "txt", &docs);
    idx.create_text_index("body").unwrap();
    // Duplicate creation: replace (re-register drops the built postings).
    idx.create_text_index("body").unwrap();

    // Observability: the text arm drives the created index.
    assert_eq!(
        idx.query().text("body", "fox", 10).plan_shape(),
        PlanShape::TextIndex {
            field: "body".to_owned()
        }
    );
    assert_eq!(
        scan.query().text("body", "fox", 10).plan_shape(),
        scan_of("txt")
    );

    // Only text values of `body` are searchable; scores match the scan arm.
    for q in ["fox", "the", "quick", "daily", "news"] {
        assert_eq!(
            thits(&idx.text_search("body", q, 10).unwrap()),
            thits(&scan.text_search("body", q, 10).unwrap()),
            "query {q:?}"
        );
    }
    let hits = idx.text_search("body", "fox", 10).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        ks(&["t1", "t3"])
    );
    // A non-text value is never a hit.
    assert!(hits.iter().all(|h| h.key != b"t4".to_vec()));
}

#[test]
fn schema_text_index_mutations_keep_search_correct() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("body", text("alpha beta gamma"))])),
        (b"b", map(&[("body", text("beta delta"))])),
    ];
    // In-memory index...
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "tmut", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "tmut", &docs);
    idx.create_text_index("body").unwrap();
    // (build the postings before mutating so maintenance is what's tested)
    assert_eq!(idx.text_search("body", "beta", 5).unwrap().len(), 2);

    // Insert a new doc with a fresh term → immediately findable.
    idx.insert(b"c", &map(&[("body", text("zeta omega"))]))
        .unwrap();
    scan.insert(b"c", &map(&[("body", text("zeta omega"))]))
        .unwrap();
    assert_eq!(
        thits(&idx.text_search("body", "zeta", 5).unwrap()),
        thits(&scan.text_search("body", "zeta", 5).unwrap())
    );
    assert_eq!(idx.text_search("body", "zeta", 5).unwrap().len(), 1);

    // Delete it → gone from the index arm.
    idx.delete(b"c").unwrap();
    scan.delete(b"c").unwrap();
    assert!(idx.text_search("body", "zeta", 5).unwrap().is_empty());

    // Clear the field on update → no longer findable.
    idx.update(b"b", |d| Some(without_field(&d.unwrap(), "body")))
        .unwrap();
    scan.update(b"b", |d| Some(without_field(&d.unwrap(), "body")))
        .unwrap();
    assert_eq!(idx.text_search("body", "delta", 5).unwrap().len(), 0);
    assert_eq!(
        thits(&idx.text_search("body", "beta", 5).unwrap()),
        thits(&scan.text_search("body", "beta", 5).unwrap())
    );

    // Patch rewrites the text → the old term stops matching, the new one hits.
    idx.patch(b"b", &map(&[("body", text("epsilon zeta"))]))
        .unwrap();
    scan.patch(b"b", &map(&[("body", text("epsilon zeta"))]))
        .unwrap();
    for q in ["beta", "epsilon", "zeta", "alpha"] {
        assert_eq!(
            thits(&idx.text_search("body", q, 5).unwrap()),
            thits(&scan.text_search("body", q, 5).unwrap()),
            "query {q:?}"
        );
    }
    // Phrase search rides the same maintained postings.
    assert_eq!(
        thits(&idx.phrase_search("body", "epsilon zeta", 5).unwrap()),
        thits(&scan.phrase_search("body", "epsilon zeta", 5).unwrap())
    );

    // ...and the same contract holds for the ON-DISK text index.
    let odb = Db::open_in_memory().unwrap();
    let oc = seed(&odb, "tmut", &docs);
    oc.create_text_index_ondisk("body").unwrap();
    oc.create_text_index_ondisk("body").unwrap(); // duplicate: replace, Ok
    assert_eq!(oc.text_search("body", "beta", 5).unwrap().len(), 2);
    oc.insert(b"c", &map(&[("body", text("zeta omega"))]))
        .unwrap();
    assert_eq!(oc.text_search("body", "zeta", 5).unwrap().len(), 1);
    oc.delete(b"c").unwrap();
    assert!(oc.text_search("body", "zeta", 5).unwrap().is_empty());
    oc.update(b"b", |d| Some(without_field(&d.unwrap(), "body")))
        .unwrap();
    assert_eq!(oc.text_search("body", "delta", 5).unwrap().len(), 0);
    assert_eq!(oc.text_search("body", "beta", 5).unwrap().len(), 1);
}

// ===========================================================================
// create_vector_index variants — creation/registration semantics
// ===========================================================================

/// Duplicate creation on the same field with DIFFERENT parameters replaces
/// the index wholesale (no error, no stale nodes under the old params).
#[test]
fn schema_vector_index_duplicate_creation_replaces_previous_params() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("v", Value::Vector(vec![1.0, 0.0]))])),
        (b"b", map(&[("v", Value::Vector(vec![0.0, 1.0]))])),
        (b"c", map(&[("v", Value::Vector(vec![-1.0, 0.0]))])),
    ];
    let twin_db = Db::open_in_memory().unwrap();
    let twin = seed(&twin_db, "vr", &docs);

    // In-memory: create with L2, serve; re-create with Cosine, replace.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "vr", &docs);
    c.create_vector_index("v", Metric::L2).unwrap();
    c.create_vector_index("v", Metric::L2).unwrap(); // same params: replace, Ok
    assert!(
        c.vector_search("v", &[1.0, 0.0], 3, Metric::L2)
            .unwrap()
            .iter()
            .all(|h| h.approximate)
    );
    c.create_vector_index("v", Metric::Cosine).unwrap(); // different params
    // The new metric drives the index; the old one no longer matches the def
    // and falls back to the exact path.
    assert!(
        c.vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
            .unwrap()
            .iter()
            .all(|h| h.approximate)
    );
    assert!(
        c.vector_search("v", &[1.0, 0.0], 3, Metric::L2)
            .unwrap()
            .iter()
            .all(|h| !h.approximate)
    );
    for m in [Metric::Cosine, Metric::L2] {
        assert_eq!(
            vhits(&c.vector_search("v", &[1.0, 0.0], 3, m).unwrap()),
            vhits(&twin.vector_search("v", &[1.0, 0.0], 3, m).unwrap()),
            "metric {m:?}"
        );
    }

    // Quantized in-memory replace (None → Binary) keeps results correct.
    c.create_vector_index_quantized("v", Metric::Cosine, Quantization::Binary)
        .unwrap();
    assert_eq!(
        vhits(
            &c.vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
                .unwrap()
        ),
        vhits(
            &twin
                .vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
                .unwrap()
        )
    );

    // On-disk replace with a different metric: the namespace resets in the
    // registration transaction and the backfill rebuilds from scratch.
    let odb = Db::open_in_memory().unwrap();
    let oc = seed(&odb, "vr", &docs);
    oc.create_vector_index_ondisk("v", Metric::L2).unwrap();
    assert!(
        oc.vector_search("v", &[1.0, 0.0], 3, Metric::L2)
            .unwrap()
            .iter()
            .all(|h| h.approximate)
    );
    oc.create_vector_index_ondisk_quantized("v", Metric::Dot, Quantization::Scalar)
        .unwrap();
    assert!(
        oc.vector_search("v", &[1.0, 0.0], 3, Metric::Dot)
            .unwrap()
            .iter()
            .all(|h| h.approximate),
        "the replacement on-disk index must serve its own metric"
    );
    for m in [Metric::Dot, Metric::Cosine] {
        assert_eq!(
            vhits(&oc.vector_search("v", &[1.0, 0.0], 3, m).unwrap()),
            vhits(&twin.vector_search("v", &[1.0, 0.0], 3, m).unwrap()),
            "on-disk metric {m:?}"
        );
    }
}

/// The PQ overload needs a trainable corpus: `Error::EmptyIndexTraining`
/// (exact variant) when there is nothing to train on or the parameters
/// cannot be satisfied; a valid corpus creates a serving index.
#[test]
fn schema_vector_pq_creation_training_error_variants_and_success() {
    // No documents at all.
    let db = Db::open_in_memory().unwrap();
    let err = db
        .collection("pq_empty")
        .create_vector_index_ondisk_pq("v", Metric::L2, 4, 16);
    assert!(matches!(err, Err(Error::EmptyIndexTraining)));

    // Documents exist but NONE has a usable vector at the field.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("pq_novec");
    c.insert(b"a", &map(&[("body", text("no vector"))]))
        .unwrap();
    c.insert(b"b", &map(&[("v", Value::Int(5))])).unwrap();
    let err = c.create_vector_index_ondisk_pq("v", Metric::L2, 4, 16);
    assert!(matches!(err, Err(Error::EmptyIndexTraining)));

    // Dimension not divisible by m — the sample cannot satisfy m subspaces.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("pq_dim");
    c.insert(b"a", &map(&[("v", Value::Vector(vec![1.0; 8]))]))
        .unwrap();
    let err = c.create_vector_index_ondisk_pq("v", Metric::L2, 3, 16);
    assert!(matches!(err, Err(Error::EmptyIndexTraining)));
    // Degenerate hyperparameters (m = 0, k out of 2..=256) likewise.
    for (m, k) in [(0usize, 16usize), (4, 1), (4, 257)] {
        let err = c.create_vector_index_ondisk_pq("v", Metric::L2, m, k);
        assert!(
            matches!(err, Err(Error::EmptyIndexTraining)),
            "m={m} k={k} must be untrainable"
        );
    }

    // Success: dim 8, m=4, k=16, plus deterministic winners (five bitwise
    // copies of the query and eight axis vectors — nothing can outrank a
    // distance-0 copy).
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("pq_ok");
    let query = vec![3.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    for n in 0..5u8 {
        c.insert(
            format!("w{n}").as_bytes(),
            &map(&[("v", Value::Vector(query.clone()))]),
        )
        .unwrap();
    }
    for i in 0..8usize {
        let mut v = vec![0.0f32; 8];
        v[i] = 2.0;
        c.insert(format!("x{i}").as_bytes(), &map(&[("v", Value::Vector(v))]))
            .unwrap();
    }
    c.create_vector_index_ondisk_pq("v", Metric::L2, 4, 16)
        .unwrap();
    let hits = c.vector_search("v", &query, 5, Metric::L2).unwrap();
    assert_eq!(hits[0].key, b"w0".to_vec());
    assert_eq!(hits[0].distance, 0.0);
    assert!(hits.iter().all(|h| h.approximate));

    // Re-creating with DIFFERENT (m, k) over the same field replaces the
    // trained codebook wholesale and stays correct.
    c.create_vector_index_ondisk_pq("v", Metric::L2, 8, 32)
        .unwrap();
    let hits = c.vector_search("v", &query, 5, Metric::L2).unwrap();
    assert_eq!(hits[0].key, b"w0".to_vec());
    assert_eq!(hits[0].distance, 0.0);

    // Insert after creation → encoded with the existing codebook, findable.
    c.insert(b"new", &map(&[("v", Value::Vector(query.clone()))]))
        .unwrap();
    let hits = c.vector_search("v", &query, 10, Metric::L2).unwrap();
    assert!(hits.iter().any(|h| h.key == b"new".to_vec()));
    // Delete → never again a hit.
    c.delete(b"new").unwrap();
    let hits = c.vector_search("v", &query, 10, Metric::L2).unwrap();
    assert!(hits.iter().all(|h| h.key != b"new".to_vec()));
}

/// Creation over a field with NO vectors at all succeeds (a usable, empty
/// index); a matching-dimension document inserted afterwards is immediately
/// searchable — the lazy-resume/maintenance contract.
#[test]
fn schema_vector_index_over_empty_field_then_insert_immediately_searchable() {
    let v = vec![1.0f32, 2.0, 3.0];
    // In-memory kind.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nov");
    c.insert(b"doc-only", &map(&[("body", text("no vector here"))]))
        .unwrap();
    c.create_vector_index("v", Metric::L2).unwrap();
    // Observability: the index exists and is consultable even while empty.
    assert_eq!(
        c.query().vector("v", v.clone(), 5, Metric::L2).plan_shape(),
        PlanShape::AnnIndex {
            field: "v".to_owned()
        }
    );
    assert!(c.vector_search("v", &v, 5, Metric::L2).unwrap().is_empty());
    c.insert(b"w", &map(&[("v", Value::Vector(v.clone()))]))
        .unwrap();
    let hits = c.vector_search("v", &v, 5, Metric::L2).unwrap();
    assert_eq!(vhits(&hits), vec![(b"w".to_vec(), 0.0)]);
    assert!(hits.iter().all(|h| h.approximate));

    // On-disk kind (durable backfill over an empty corpus → Complete).
    let odb = Db::open_in_memory().unwrap();
    let oc = odb.collection("nov");
    oc.create_vector_index_ondisk("v", Metric::Cosine).unwrap();
    assert!(
        oc.vector_search("v", &v, 5, Metric::Cosine)
            .unwrap()
            .is_empty()
    );
    oc.insert(b"w", &map(&[("v", Value::Vector(v.clone()))]))
        .unwrap();
    let hits = oc.vector_search("v", &v, 5, Metric::Cosine).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, b"w".to_vec());
    // Cosine of a vector with itself is ~0 but not exactly 0 in f32 (the
    // norm product rounds); pin the tolerance, not the bit pattern.
    assert!(hits[0].distance.abs() < 1e-6, "got {}", hits[0].distance);
    assert!(hits.iter().all(|h| h.approximate));
}

/// A vector index pins ONE dimension (from the first vector a build sees —
/// key order for a lazy backfill). Documents of other dimensions are not in
/// the index but stay queryable via the exact fallback: both dimensions
/// return exactly what an unindexed twin returns.
#[test]
fn schema_vector_index_mixed_dimensions_match_scan_twin() {
    // Keys order the corpus: dim-2 docs sort before dim-3 docs, so the
    // build's first vector pins dim 2 deterministically.
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a1", map(&[("v", Value::Vector(vec![1.0, 0.0]))])),
        (b"a2", map(&[("v", Value::Vector(vec![0.0, 1.0]))])),
        (b"a3", map(&[("v", Value::Vector(vec![1.0, 1.0]))])),
        (b"z1", map(&[("v", Value::Vector(vec![1.0, 0.0, 0.0]))])),
        (b"z2", map(&[("v", Value::Vector(vec![0.0, 1.0, 0.0]))])),
        (b"m0", map(&[("body", text("no vector"))])),
    ];
    let q2 = vec![0.9f32, 0.1];
    let q3 = vec![0.9f32, 0.1, 0.0];
    for kind in ["in-memory", "on-disk"] {
        let twin_db = Db::open_in_memory().unwrap();
        let twin = seed(&twin_db, "mixdim", &docs);
        let idx_db = Db::open_in_memory().unwrap();
        let idx = seed(&idx_db, "mixdim", &docs);
        if kind == "in-memory" {
            idx.create_vector_index("v", Metric::L2).unwrap();
        } else {
            idx.create_vector_index_ondisk("v", Metric::L2).unwrap();
        }
        // The pinned dimension serves via the graph (approximate)...
        let hits2 = idx.vector_search("v", &q2, 6, Metric::L2).unwrap();
        assert!(
            hits2.iter().all(|h| h.approximate),
            "{kind}: dim-2 query must use the index"
        );
        // ...the other dimension falls back to exact and still returns its
        // documents (and only those).
        let hits3 = idx.vector_search("v", &q3, 6, Metric::L2).unwrap();
        assert!(
            hits3.iter().all(|h| !h.approximate),
            "{kind}: dim-3 query must take the exact fallback"
        );
        // Twin parity per dimension, keys AND exact distances.
        assert_eq!(
            vhits(&idx.vector_search("v", &q2, 6, Metric::L2).unwrap()),
            vhits(&twin.vector_search("v", &q2, 6, Metric::L2).unwrap()),
            "{kind}: dim-2 parity"
        );
        assert_eq!(
            vhits(&idx.vector_search("v", &q3, 6, Metric::L2).unwrap()),
            vhits(&twin.vector_search("v", &q3, 6, Metric::L2).unwrap()),
            "{kind}: dim-3 parity"
        );
        let mut got2: Vec<Vec<u8>> = hits2.iter().map(|h| h.key.clone()).collect();
        got2.sort();
        assert_eq!(
            got2,
            ks(&["a1", "a2", "a3"]),
            "{kind}: only dim-2 docs on the dim-2 query"
        );
        let mut got3: Vec<Vec<u8>> = hits3.iter().map(|h| h.key.clone()).collect();
        got3.sort();
        assert_eq!(
            got3,
            ks(&["z1", "z2"]),
            "{kind}: only dim-3 docs on the dim-3 query"
        );
    }
}

/// Changing a document's vector dimension on update removes it from the
/// index (its old node is tombstoned, the new shape is not indexable): the
/// old-dimension search excludes it, the new-dimension search finds it via
/// the exact fallback — twin parity throughout.
#[test]
fn schema_vector_dimension_change_on_update_leaves_index() {
    let q2 = vec![1.0f32, 0.0];
    let q3 = vec![1.0f32, 0.0, 0.0];
    for kind in ["in-memory", "on-disk"] {
        let twin_db = Db::open_in_memory().unwrap();
        let twin = seed(
            &twin_db,
            "dimchg",
            &[
                (b"a", map(&[("v", Value::Vector(vec![1.0, 0.0]))])),
                (b"b", map(&[("v", Value::Vector(vec![0.0, 1.0]))])),
            ],
        );
        let idx_db = Db::open_in_memory().unwrap();
        let idx = seed(
            &idx_db,
            "dimchg",
            &[
                (b"a", map(&[("v", Value::Vector(vec![1.0, 0.0]))])),
                (b"b", map(&[("v", Value::Vector(vec![0.0, 1.0]))])),
            ],
        );
        if kind == "in-memory" {
            idx.create_vector_index("v", Metric::L2).unwrap();
        } else {
            idx.create_vector_index_ondisk("v", Metric::L2).unwrap();
        }
        // Build/complete the index before the mutation.
        assert_eq!(idx.vector_search("v", &q2, 5, Metric::L2).unwrap().len(), 2);

        // Update a's vector to dim 3 on BOTH sides.
        let new_doc = map(&[("v", Value::Vector(q3.clone()))]);
        idx.update(b"a", |_| Some(new_doc.clone())).unwrap();
        twin.update(b"a", |_| Some(new_doc.clone())).unwrap();

        for q in [&q2, &q3] {
            assert_eq!(
                vhits(&idx.vector_search("v", q, 5, Metric::L2).unwrap()),
                vhits(&twin.vector_search("v", q, 5, Metric::L2).unwrap()),
                "{kind}: query dim {} parity",
                q.len()
            );
        }
        let hits2 = idx.vector_search("v", &q2, 5, Metric::L2).unwrap();
        assert!(hits2.iter().all(|h| h.key != b"a".to_vec()));
        let mut got2: Vec<Vec<u8>> = hits2.iter().map(|h| h.key.clone()).collect();
        got2.sort();
        assert_eq!(got2, ks(&["b"]));
        let hits3 = idx.vector_search("v", &q3, 5, Metric::L2).unwrap();
        let mut got3: Vec<Vec<u8>> = hits3.iter().map(|h| h.key.clone()).collect();
        got3.sort();
        assert_eq!(
            got3,
            ks(&["a"]),
            "{kind}: the re-shaped doc stays queryable via the fallback"
        );
    }
}

/// Dead-fraction compaction (index.rs): after enough public deletes the
/// index compacts automatically — observable only as continued correctness,
/// never as a stale or missing result. No internal counters are pinned.
#[test]
fn schema_vector_index_compaction_after_deletes_keeps_results_exact() {
    // On-disk trigger: dead * 2 > live (dead > 1/3 of nodes), evaluated on
    // the write path after each applied delete.
    let odb = Db::open_in_memory().unwrap();
    let oc = odb.collection("compact");
    for i in 0..12i64 {
        let mut v = vec![0.0f32; 4];
        v[0] = i as f32;
        v[1] = 1.0;
        oc.insert(
            format!("k{i:02}").as_bytes(),
            &map(&[("v", Value::Vector(v))]),
        )
        .unwrap();
    }
    oc.create_vector_index_ondisk("v", Metric::L2).unwrap();
    assert_eq!(
        oc.vector_search("v", &[0.0, 1.0, 0.0, 0.0], 12, Metric::L2)
            .unwrap()
            .len(),
        12
    );
    // Cross the threshold: 5 of 12 deleted → dead*2 = 10 > live = 7.
    for i in 0..5i64 {
        oc.delete(format!("k{i:02}").as_bytes()).unwrap();
    }
    let hits = oc
        .vector_search("v", &[0.0, 1.0, 0.0, 0.0], 12, Metric::L2)
        .unwrap();
    let mut got: Vec<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
    got.sort();
    assert_eq!(
        got,
        (5..12i64)
            .map(|i| format!("k{i:02}").into_bytes())
            .collect::<Vec<_>>(),
        "post-compaction search returns exactly the live docs"
    );
    assert!(hits.iter().all(|h| h.approximate));
    // And further maintenance on the compacted index stays correct.
    oc.insert(
        b"new",
        &map(&[("v", Value::Vector(vec![0.0, 1.0, 0.0, 0.0]))]),
    )
    .unwrap();
    let hits = oc
        .vector_search("v", &[0.0, 1.0, 0.0, 0.0], 1, Metric::L2)
        .unwrap();
    assert_eq!(hits[0].key, b"new".to_vec());
    assert_eq!(hits[0].distance, 0.0);

    // In-memory trigger: dead-majority (dead * 2 > total nodes), evaluated
    // on the search path after the graph has been built.
    let mdb = Db::open_in_memory().unwrap();
    let mc = mdb.collection("compact");
    for i in 0..10i64 {
        let mut v = vec![0.0f32; 4];
        v[0] = i as f32;
        v[1] = 1.0;
        mc.insert(
            format!("k{i:02}").as_bytes(),
            &map(&[("v", Value::Vector(v))]),
        )
        .unwrap();
    }
    mc.create_vector_index("v", Metric::L2).unwrap();
    assert_eq!(
        mc.vector_search("v", &[9.0, 1.0, 0.0, 0.0], 1, Metric::L2)
            .unwrap()[0]
            .key,
        *b"k09"
    );
    for i in 0..6i64 {
        mc.delete(format!("k{i:02}").as_bytes()).unwrap();
    }
    let hits = mc
        .vector_search("v", &[0.0, 1.0, 0.0, 0.0], 10, Metric::L2)
        .unwrap();
    let mut got: Vec<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
    got.sort();
    assert_eq!(
        got,
        (6..10i64)
            .map(|i| format!("k{i:02}").into_bytes())
            .collect::<Vec<_>>(),
        "in-memory compaction keeps results exact"
    );
    assert!(hits.iter().all(|h| h.approximate));
}

// ===========================================================================
// create_geo_index
// ===========================================================================

#[test]
fn schema_geo_index_duplicate_creation_and_non_point_docs_skipped() {
    // Corpus includes valid array/map points and NON-point values at the
    // indexed field (scalars, malformed arrays, text-lat maps) plus a doc
    // missing the field entirely.
    let docs: Vec<(&[u8], Value)> = vec![
        (
            b"london",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(51.5074), Value::Float(-0.1278)]),
            )]),
        ),
        (
            b"paris",
            map(&[(
                "loc",
                map(&[
                    ("lat", Value::Float(48.8566)),
                    ("lon", Value::Float(2.3522)),
                ]),
            )]),
        ),
        (b"int-doc", map(&[("loc", Value::Int(7))])),
        (
            b"short-arr",
            map(&[("loc", Value::Array(vec![Value::Float(1.0)]))]),
        ),
        (
            b"text-lat",
            map(&[(
                "loc",
                map(&[("lat", text("x")), ("lon", Value::Float(0.0))]),
            )]),
        ),
        (b"no-field", map(&[("body", text("nothing"))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "gplace", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "gplace", &docs);
    idx.create_geo_index("loc").unwrap();
    // Duplicate creation: replace, Ok.
    idx.create_geo_index("loc").unwrap();

    // Observability: the created geo index drives the builder's window.
    assert_eq!(
        idx.query()
            .filter(field("loc").within_km(51.5, -0.13, 50.0))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "geo" }
    );
    assert_eq!(
        scan.query()
            .filter(field("loc").within_km(51.5, -0.13, 50.0))
            .plan_shape(),
        scan_of("gplace")
    );

    // Radius and bbox queries: exact GeoHit parity (keys, distances, docs —
    // malformed points skipped identically on both sides).
    for (lat, lon, r) in [
        (51.5f64, -0.13f64, 400.0),
        (48.85, 2.35, 50.0),
        (0.0, 0.0, 20016.0),
    ] {
        assert_eq!(
            idx.geo_within_radius("loc", lat, lon, r).unwrap(),
            scan.geo_within_radius("loc", lat, lon, r).unwrap(),
            "radius {lat},{lon},{r}"
        );
    }
    assert_eq!(
        idx.geo_within_bbox("loc", 50.0, -1.0, 52.0, 3.0).unwrap(),
        scan.geo_within_bbox("loc", 50.0, -1.0, 52.0, 3.0).unwrap()
    );
    let hits = idx.geo_within_radius("loc", 51.5, -0.13, 400.0).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        ks(&["london", "paris"]),
        "Paris (~344 km from London) is inside a 400 km radius"
    );
}

#[test]
fn schema_geo_index_point_move_and_delete_maintained() {
    let start: Vec<(&[u8], Value)> = vec![
        (
            b"near",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(10.0), Value::Float(10.0)]),
            )]),
        ),
        (
            b"mid",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(20.0), Value::Float(20.0)]),
            )]),
        ),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "gmove", &start);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "gmove", &start);
    idx.create_geo_index("loc").unwrap();
    // Exercise the index before mutating.
    assert_eq!(
        idx.geo_within_radius("loc", 10.0, 10.0, 200.0)
            .unwrap()
            .len(),
        1
    );

    // MOVE a point (overwrite): the old cell must stop matching.
    idx.insert(
        b"near",
        &map(&[(
            "loc",
            Value::Array(vec![Value::Float(-30.0), Value::Float(-30.0)]),
        )]),
    )
    .unwrap();
    scan.insert(
        b"near",
        &map(&[(
            "loc",
            Value::Array(vec![Value::Float(-30.0), Value::Float(-30.0)]),
        )]),
    )
    .unwrap();
    for (lat, lon) in [(10.0f64, 10.0f64), (-30.0, -30.0)] {
        assert_eq!(
            idx.geo_within_radius("loc", lat, lon, 1600.0).unwrap(),
            scan.geo_within_radius("loc", lat, lon, 1600.0).unwrap(),
            "after move, center {lat},{lon}"
        );
    }
    // mid (20,20) is ~1545 km from (10,10): the only doc left in the old
    // neighborhood — the moved doc must not linger in its old cell.
    assert_eq!(
        idx.geo_within_radius("loc", 10.0, 10.0, 1600.0)
            .unwrap()
            .iter()
            .map(|h| h.key.clone())
            .collect::<Vec<_>>(),
        ks(&["mid"]),
        "the moved doc must not linger in its old cell"
    );

    // DELETE: gone from both the index and the twin.
    idx.delete(b"mid").unwrap();
    scan.delete(b"mid").unwrap();
    assert_eq!(
        idx.geo_within_radius("loc", 0.0, 0.0, 20016.0).unwrap(),
        scan.geo_within_radius("loc", 0.0, 0.0, 20016.0).unwrap()
    );
    assert_eq!(
        idx.geo_within_radius("loc", 0.0, 0.0, 20016.0)
            .unwrap()
            .iter()
            .map(|h| h.key.clone())
            .collect::<Vec<_>>(),
        ks(&["near"])
    );
}

// ===========================================================================
// unique constraints
// ===========================================================================

fn unique_email_schema() -> Schema {
    Schema::new().field(Field::new("email", FieldType::Text).unique())
}

#[test]
fn schema_unique_insert_conflict_rejects_with_exact_variant_and_stores_nothing() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uq");
    c.set_schema(&unique_email_schema()).unwrap();
    c.insert(b"u1", &map(&[("email", text("dup@x.com"))]))
        .unwrap();
    // Same value, different key → the EXACT variant, with the documented
    // message.
    let err = c
        .insert(b"u2", &map(&[("email", text("dup@x.com"))]))
        .unwrap_err();
    let Error::SchemaViolation(msg) = err else {
        panic!("expected SchemaViolation, got {err:?}");
    };
    assert_eq!(msg, "field 'email' must be unique; value already exists");
    // Nothing was stored.
    assert_eq!(c.get(b"u2").unwrap(), None);
    assert_eq!(c.len().unwrap(), 1);
    // Overwriting the SAME key with its own value is exempt.
    c.insert(b"u1", &map(&[("email", text("dup@x.com"))]))
        .unwrap();
    // A different value is fine.
    c.insert(b"u3", &map(&[("email", text("other@x.com"))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 2);
}

#[test]
fn schema_unique_update_conflict_rejects_whole_write() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqu");
    c.set_schema(&unique_email_schema()).unwrap();
    c.insert(b"u1", &map(&[("email", text("a@x.com"))]))
        .unwrap();
    let original = map(&[("email", text("b@x.com"))]);
    c.insert(b"u2", &original).unwrap();
    // An update that would collide with u1's value is rejected...
    let err = c
        .update(b"u2", |_| Some(map(&[("email", text("a@x.com"))])))
        .unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)));
    // ...and the WHOLE write is rejected: u2 keeps its original document.
    assert_eq!(c.get(b"u2").unwrap(), Some(original));
    // An update to a fresh value succeeds and frees the old one.
    c.update(b"u2", |_| Some(map(&[("email", text("c@x.com"))])))
        .unwrap();
    c.insert(b"u3", &map(&[("email", text("b@x.com"))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 3);
}

/// NaN is the same STORED value as NaN for uniqueness (the engine-wide
/// storage equality), regardless of payload — with and without a scalar
/// index on the field (NaN's encoded bucket key is not walkable, so the
/// index path must fall back to the scan comparison).
#[test]
fn schema_unique_nan_equals_nan_rejects_second_document() {
    for with_index in [false, true] {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection(if with_index { "uqnan_idx" } else { "uqnan" });
        c.set_schema(&Schema::new().field(Field::new("x", FieldType::Float).unique()))
            .unwrap();
        if with_index {
            c.create_scalar_index("x").unwrap();
        }
        c.insert(b"a", &map(&[("x", Value::Float(f64::NAN))]))
            .unwrap();
        let err = c.insert(b"b", &map(&[("x", Value::Float(f64::NAN))]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "with_index={with_index}: duplicate NaN must be rejected"
        );
        assert_eq!(c.get(b"b").unwrap(), None);
        // A finite value is still fine alongside the NaN.
        c.insert(b"c", &map(&[("x", Value::Float(1.0))])).unwrap();
        assert_eq!(c.len().unwrap(), 2);
    }
}

/// Bytes/Text/Vector uniqueness (Vector equality is element-wise with the
/// NaN rule), and the NULL exemption: `Null` values never conflict — many
/// documents may hold Null at a unique field.
#[test]
fn schema_unique_containers_bytes_text_vector_and_null_rule() {
    // Bytes.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqbytes");
    c.set_schema(&Schema::new().field(Field::new("b", FieldType::Bytes).unique()))
        .unwrap();
    c.insert(b"a", &map(&[("b", Value::Bytes(vec![1, 2, 3]))]))
        .unwrap();
    let err = c.insert(b"c", &map(&[("b", Value::Bytes(vec![1, 2, 3]))]));
    assert!(matches!(err, Err(Error::SchemaViolation(_))));
    c.insert(b"d", &map(&[("b", Value::Bytes(vec![1, 2]))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 2);

    // Vector: byte-identical vectors conflict, including with NaN elements.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqvec");
    c.set_schema(&Schema::new().field(Field::new("v", FieldType::Vector).unique()))
        .unwrap();
    c.insert(b"a", &map(&[("v", Value::Vector(vec![f32::NAN, 1.0]))]))
        .unwrap();
    let err = c.insert(b"b", &map(&[("v", Value::Vector(vec![f32::NAN, 1.0]))]));
    assert!(
        matches!(err, Err(Error::SchemaViolation(_))),
        "equal vectors containing NaN must conflict"
    );
    // A different vector (even NaN in the other slot) does not.
    c.insert(b"c", &map(&[("v", Value::Vector(vec![1.0, f32::NAN]))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 2);

    // Null: exempt from the uniqueness check — pinned as ALLOWED.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqnull");
    c.set_schema(&Schema::new().field(Field::new("x", FieldType::Float).unique()))
        .unwrap();
    c.insert(b"a", &map(&[("x", Value::Null)])).unwrap();
    c.insert(b"b", &map(&[("x", Value::Null)])).unwrap();
    assert_eq!(
        c.len().unwrap(),
        2,
        "two nulls at a unique field are allowed"
    );
    // Null alongside a real value is fine too, and the real value still
    // conflicts with itself only.
    c.insert(b"c", &map(&[("x", Value::Float(1.0))])).unwrap();
    let err = c.insert(b"d", &map(&[("x", Value::Float(1.0))]));
    assert!(matches!(err, Err(Error::SchemaViolation(_))));
    // Documents MISSING the unique field entirely are unconstrained.
    c.insert(b"e", &map(&[("other", Value::Int(1))])).unwrap();
    c.insert(b"f", &map(&[("other", Value::Int(2))])).unwrap();
    assert_eq!(c.len().unwrap(), 5);
}

#[test]
fn schema_unique_delete_then_reinsert_same_value_allowed() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqre");
    c.set_schema(&unique_email_schema()).unwrap();
    c.insert(b"u1", &map(&[("email", text("x@x.com"))]))
        .unwrap();
    assert!(c.delete(b"u1").unwrap());
    // The value died with its document: a new doc may take it.
    c.insert(b"u2", &map(&[("email", text("x@x.com"))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 1);
    // And the conflict is live again afterwards.
    let err = c.insert(b"u3", &map(&[("email", text("x@x.com"))]));
    assert!(matches!(err, Err(Error::SchemaViolation(_))));
}

#[test]
fn schema_unique_batch_conflict_rolls_back_whole_batch() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqbatch");
    c.set_schema(&unique_email_schema()).unwrap();
    let items = [
        (b"b1".as_slice(), &map(&[("email", text("one@x.com"))])),
        (b"b2".as_slice(), &map(&[("email", text("two@x.com"))])),
        (b"b3".as_slice(), &map(&[("email", text("one@x.com"))])),
    ];
    let err = c.insert_batch(&items).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)));
    // The whole batch rolled back — including the non-conflicting items.
    assert_eq!(c.len().unwrap(), 0);
    assert_eq!(c.get(b"b2").unwrap(), None);
    // A clean batch commits.
    let items = [
        (b"b1".as_slice(), &map(&[("email", text("one@x.com"))])),
        (b"b2".as_slice(), &map(&[("email", text("two@x.com"))])),
    ];
    c.insert_batch(&items).unwrap();
    assert_eq!(c.len().unwrap(), 2);
}

/// A scalar index on the unique field routes the check through the index
/// bucket walk: still enforced, and value movement stays consistent.
#[test]
fn schema_unique_with_scalar_index_stays_enforced_and_moves_with_values() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("uqidx");
    c.set_schema(&unique_email_schema()).unwrap();
    c.create_scalar_index("email").unwrap();
    c.insert(b"u1", &map(&[("email", text("a@x.com"))]))
        .unwrap();
    // Enforcement through the bucket path.
    let err = c.insert(b"u2", &map(&[("email", text("a@x.com"))]));
    assert!(matches!(err, Err(Error::SchemaViolation(_))));
    // Moving the value: u1 vacates, u2 may take it.
    c.update(b"u1", |_| Some(map(&[("email", text("z@x.com"))])))
        .unwrap();
    c.insert(b"u2", &map(&[("email", text("a@x.com"))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 2);
    // And the index-driven queries stay correct alongside.
    assert_eq!(
        matching_keys(&c, &field("email").eq(text("z@x.com"))),
        ks(&["u1"])
    );
    assert_eq!(
        matching_keys(&c, &field("email").eq(text("a@x.com"))),
        ks(&["u2"])
    );
}

/// RED pin (Task 11): uniqueness is the engine-wide STORAGE equality
/// (`schema::unique_value_eq`, shared with compare_and_set) — `Int(7)` and
/// `Float(7.0)` are distinct stored values, so both documents must be
/// accepted. The scalar-index bucket walk keyed conflict detection on the
/// order-preserving f64 encoding, which collapses the two kinds into one
/// bucket and rejected the second document ONLY when an index existed —
/// diverging from the index-free path and from CAS's notion of equality.
#[test]
fn schema_unique_numeric_kind_equality_same_with_and_without_index() {
    for with_index in [false, true] {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection(if with_index { "uqnum_idx" } else { "uqnum" });
        c.set_schema(&Schema::new().field(Field::new("x", FieldType::Any).unique()))
            .unwrap();
        if with_index {
            c.create_scalar_index("x").unwrap();
        }
        c.insert(b"a", &map(&[("x", Value::Int(7))])).unwrap();
        c.insert(b"b", &map(&[("x", Value::Float(7.0))]))
            .expect("Int(7) and Float(7.0) are distinct stored values");
        assert_eq!(c.len().unwrap(), 2);
        // Structurally equal values still conflict on both paths.
        let err = c.insert(b"c", &map(&[("x", Value::Int(7))]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "with_index={with_index}: a true duplicate must still be rejected"
        );
        assert_eq!(c.len().unwrap(), 2);
    }
}

// ===========================================================================
// name validation on the index APIs
// ===========================================================================

/// Index APIs take FIELD names, validated like collection names: interior
/// `__` and NUL are `Error::InvalidName`, a `__`-prefixed collection is
/// `Error::ReservedCollection`, an interior-`__` collection is
/// `Error::InvalidName`. The empty field name is ACCEPTED (a legal map key)
/// and functional.
#[test]
fn schema_index_creation_validates_names_across_families() {
    let db = Db::open_in_memory().unwrap();
    // Every family rejects `__` and NUL in the FIELD name with the exact
    // variant.
    let bad_fields = ["a__b", "x\u{0}y"];
    for f in bad_fields {
        let c = db.collection("names");
        let err = c.create_scalar_index(f);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "scalar field {f:?}"
        );
        let err = c.create_compound_index(&["ok", f]);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "compound field {f:?}"
        );
        let err = c.create_text_index(f);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "text field {f:?}"
        );
        let err = c.create_text_index_ondisk(f);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "text-ondisk field {f:?}"
        );
        let err = c.create_vector_index(f, Metric::L2);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "vector field {f:?}"
        );
        let err = c.create_vector_index_quantized(f, Metric::L2, Quantization::Binary);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "vector-quant field {f:?}"
        );
        let err = c.create_vector_index_ondisk(f, Metric::L2);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "vector-ondisk field {f:?}"
        );
        let err = c.create_vector_index_ondisk_quantized(f, Metric::L2, Quantization::Scalar);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "vector-ondisk-quant field {f:?}"
        );
        let err = c.create_vector_index_ondisk_pq(f, Metric::L2, 2, 8);
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "vector-pq field {f:?}"
        );
        let err = c.create_geo_index(f);
        assert!(matches!(err, Err(Error::InvalidName(_))), "geo field {f:?}");
        // Schemas validate their FIELD names through the same rule.
        let err = c.set_schema(&Schema::new().field(Field::new(f, FieldType::Any)));
        assert!(
            matches!(err, Err(Error::InvalidName(_))),
            "schema field {f:?}"
        );
    }

    // Reserved / invalid COLLECTION names are refused by every family.
    for coll in ["__indexes__", "__schemas__"] {
        let err = db.collection(coll).create_scalar_index("f");
        assert!(
            matches!(err, Err(Error::ReservedCollection(_))),
            "collection {coll:?}"
        );
        let err = db.collection(coll).create_geo_index("f");
        assert!(
            matches!(err, Err(Error::ReservedCollection(_))),
            "geo on {coll:?}"
        );
    }
    let err = db.collection("x__y").create_scalar_index("f");
    assert!(matches!(err, Err(Error::InvalidName(_))));

    // The EMPTY field name is accepted by the creation API (a legal map
    // key), but it resolves no field — `get_path("")` is `None` — so
    // nothing is indexed and `field("")` predicates match nothing, with or
    // without an index. RED pin (Task 11): the predicate layer's private
    // path resolver DID resolve `""` to a top-level `""` key, so an
    // UNINDEXED twin matched a document holding it while the indexed
    // collection served an empty window — an index-vs-scan divergence.
    let c = db.collection("empty_field");
    let mut m = BTreeMap::new();
    m.insert(String::new(), Value::Int(5));
    c.insert(b"k", &Value::Map(m)).unwrap();
    c.create_scalar_index("").unwrap();
    let bare = db.collection("empty_field_bare");
    let mut m2 = BTreeMap::new();
    m2.insert(String::new(), Value::Int(5));
    bare.insert(b"k", &Value::Map(m2)).unwrap();
    for coll in [&c, &bare] {
        assert_eq!(matching_keys(coll, &field("").eq(Value::Int(5))), ks(&[]));
        assert_eq!(matching_keys(coll, &field("").exists()), ks(&[]));
    }
}

/// RED pin (Task 11 review round 1): `select`'s projection carried a third
/// hand-rolled dotted-path resolver that resolved `""` to a top-level `""`
/// map key — while `field("")` predicates never match and
/// `Value::get_path("")` is `None`. `select("")` must agree with that
/// single path semantics: the name resolves no field, so the projection
/// omits it and yields an empty map. Nested dotted projection is pinned
/// alongside (it must be identical before and after the delegation to
/// `get_path`).
#[test]
fn schema_select_empty_field_name_matches_get_path_semantics() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("proj");
    let mut m = BTreeMap::new();
    m.insert(String::new(), Value::Int(5));
    m.insert(
        "meta".to_owned(),
        map(&[("author", map(&[("name", text("ada"))]))]),
    );
    c.insert(b"k", &Value::Map(m)).unwrap();

    // The empty field NAME resolves no field — same answer `field("")`
    // predicates give (no match) and `get_path("")` gives (`None`): the
    // projected document is an EMPTY map, not `{"": 5}`.
    let rows = c.query().select([""]).run().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].document, map(&[]));

    // Nested dotted paths project exactly as before the delegation.
    let rows = c.query().select(["meta.author"]).run().unwrap();
    assert_eq!(
        rows[0].document,
        map(&[("meta", map(&[("author", map(&[("name", text("ada"))]))]))])
    );
}

// ===========================================================================
// declared schemas — FieldType matrix and the Schema surface
// ===========================================================================

/// Every `FieldType` variant accepts its own kind and rejects a wrong kind
/// through a real write; `Any` accepts everything; `fields()` exposes the
/// declaration in order.
#[test]
fn schema_field_type_matrix_and_fields_accessor() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("types");

    // The declared schema is observable through `fields()`, in order, with
    // the builder flags materialized on `Field`.
    let schema = Schema::new()
        .field(Field::new("b", FieldType::Bool))
        .field(Field::new("i", FieldType::Int))
        .field(Field::new("f", FieldType::Float))
        .field(Field::new("t", FieldType::Text))
        .field(Field::new("by", FieldType::Bytes))
        .field(Field::new("v", FieldType::Vector))
        .field(Field::new("a", FieldType::Array))
        .field(Field::new("m", FieldType::Map))
        .field(Field::new("any", FieldType::Any))
        .field(Field::new("req", FieldType::Int).required())
        .field(Field::new("uniq", FieldType::Text).unique());
    assert_eq!(schema.fields().len(), 11);
    assert_eq!(schema.fields()[0].name, "b");
    assert_eq!(schema.fields()[0].ty, FieldType::Bool);
    assert!(!schema.fields()[0].required && !schema.fields()[0].unique);
    let req = &schema.fields()[9];
    assert_eq!(
        (req.name.as_str(), req.ty, req.required, req.unique),
        ("req", FieldType::Int, true, false)
    );
    let uniq = &schema.fields()[10];
    assert_eq!(
        (uniq.name.as_str(), uniq.ty, uniq.required, uniq.unique),
        ("uniq", FieldType::Text, false, true)
    );
    c.set_schema(&schema).unwrap();

    // One conforming document: every field carries its own kind.
    let good = map(&[
        ("b", Value::Bool(false)),
        ("i", Value::Int(1)),
        ("f", Value::Float(2.5)),
        ("t", text("x")),
        ("by", Value::Bytes(vec![9])),
        ("v", Value::Vector(vec![1.0])),
        ("a", Value::Array(vec![Value::Int(1)])),
        ("m", map(&[("k", Value::Null)])),
        ("any", Value::Bool(true)), // Any accepts any kind...
        ("req", Value::Int(3)),
        ("uniq", text("u1")),
    ]);
    c.insert(b"good", &good).unwrap();
    assert_eq!(c.get(b"good").unwrap(), Some(good.clone()));

    // Each typed field rejects a value of a DIFFERENT kind with the exact
    // variant (Null is exempt — it reads as absent unless required).
    let wrong = [
        ("b", Value::Int(1)),
        ("i", Value::Float(1.0)),
        ("f", Value::Int(1)),
        ("t", Value::Int(1)),
        ("by", text("bytes")),
        ("v", Value::Array(vec![Value::Int(1)])),
        ("a", Value::Map(BTreeMap::new())),
        ("m", Value::Array(vec![])),
    ];
    for (f, v) in wrong {
        let mut doc = good.clone();
        if let Value::Map(m) = &mut doc {
            m.insert(f.to_owned(), v);
        }
        let err = c.insert(b"bad", &doc).unwrap_err();
        assert!(
            matches!(err, Error::SchemaViolation(_)),
            "field {f} must reject the wrong kind"
        );
        assert_eq!(c.get(b"bad").unwrap(), None);
    }
    // `required` rejects both a missing field and an explicit Null.
    let mut no_req = good.clone();
    if let Value::Map(m) = &mut no_req {
        m.remove("req");
    }
    assert!(matches!(
        c.insert(b"bad", &no_req),
        Err(Error::SchemaViolation(_))
    ));
    let mut null_req = good.clone();
    if let Value::Map(m) = &mut null_req {
        m.insert("req".to_owned(), Value::Null);
    }
    assert!(matches!(
        c.insert(b"bad", &null_req),
        Err(Error::SchemaViolation(_))
    ));
    // A unique violation is the same variant with the unique message.
    let mut dup_uniq = good.clone();
    if let Value::Map(m) = &mut dup_uniq {
        m.insert("uniq".to_owned(), text("u1"));
    }
    let err = c.insert(b"dup", &dup_uniq).unwrap_err();
    let Error::SchemaViolation(msg) = err else {
        panic!("expected SchemaViolation, got {err:?}");
    };
    assert_eq!(msg, "field 'uniq' must be unique; value already exists");
}
