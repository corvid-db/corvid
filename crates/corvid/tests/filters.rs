//! WHERE/filter conformance (Task 4): the full predicate lattice through the
//! public API only — every `Predicate` form × `CmpOp` × `Value` kind, missing
//! and nested paths, NaN/±inf/`-0.0` semantics, indexed-vs-scan equivalence
//! for every index-serviceable predicate form, and filter+pagination play.
//!
//! Contract notes pinned by these tests (read from `src/filter.rs` first):
//! * a comparison whose path is MISSING is `false` for every operator —
//!   including `Ne`, so `Not(compare-on-missing)` matches every document;
//! * ordered comparisons (`Lt/Le/Gt/Ge`, `Between`) order only numbers
//!   (`Int`/`Float` interop through `f64`) and text (UTF-8 byte order);
//!   bools, bytes, nulls, vectors, arrays, maps, and mixed kinds are
//!   unordered → `false` (bools are NOT `false < true`);
//! * `Eq`/`Ne`/`In` compare numbers across `Int`/`Float` numerically and
//!   everything else structurally; NaN never equals anything, so `Ne` on a
//!   NaN field is `true` against every constant (NaN included);
//! * `Exists` counts a present `Null` or container as present;
//! * dotted paths traverse nested MAPS only — `items.0` does not index into
//!   an array, and descending through a scalar misses;
//! * `StartsWith`/`Contains` match only `Text` fields: the empty prefix or
//!   substring matches every text (empty string included) and nothing else;
//! * `GeoWithin` performs no coordinate validation — out-of-range lat/lon
//!   are evaluated mathematically; the radius test is inclusive (`<=`).

use std::collections::BTreeMap;

use corvid::{
    CmpOp, Collection, Db, PlanShape, Predicate, ResultRow, Store, Value, field, haversine_km,
};

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

fn matching_keys(c: &Collection<'_>, p: &Predicate) -> Vec<Vec<u8>> {
    keys(&c.query().filter(p.clone()).run().unwrap())
}

/// Expected key sets by document name. Sorted HERE so call sites may list
/// names in any order — set assertions never depend on caller discipline.
fn ks(names: &[&str]) -> Vec<Vec<u8>> {
    let mut names: Vec<&str> = names.to_vec();
    names.sort_unstable();
    names.into_iter().map(|n| n.as_bytes().to_vec()).collect()
}

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> Collection<'a> {
    let c = db.collection(name);
    for (k, d) in docs {
        c.insert(k, d).unwrap();
    }
    c
}

/// A document holding one field of every `Value` kind under a distinct name,
/// plus nested structure shared with the dotted-path tests.
fn kinds_doc() -> Value {
    map(&[
        ("i", Value::Int(7)),
        ("f", Value::Float(2.5)),
        ("t", Value::Text("héllo 🐦".to_owned())),
        ("b", Value::Bool(true)),
        ("by", Value::Bytes(vec![1, 2, 255])),
        ("v", Value::Vector(vec![1.0, 2.0])),
        ("a", Value::Array(vec![Value::Int(1), text("x")])),
        ("m", map(&[("k", Value::Int(1))])),
        ("z", Value::Null),
        ("meta", map(&[("author", map(&[("name", text("ada"))]))])),
    ])
}

/// One document per `Value` kind under the SAME field `x` (plus a doc missing
/// `x`), for set-level lattice assertions.
fn kinds_corpus() -> Vec<(&'static [u8], Value)> {
    vec![
        (b"k-int", map(&[("x", Value::Int(7))])),
        (b"k-float", map(&[("x", Value::Float(7.0))])),
        (b"k-text", map(&[("x", text("7"))])),
        (b"k-bool", map(&[("x", Value::Bool(true))])),
        (b"k-bytes", map(&[("x", Value::Bytes(vec![1, 2, 255]))])),
        (b"k-vec", map(&[("x", Value::Vector(vec![1.0, 2.0]))])),
        (
            b"k-arr",
            map(&[("x", Value::Array(vec![Value::Int(1), text("x")]))]),
        ),
        (b"k-map", map(&[("x", map(&[("k", Value::Int(1))]))])),
        (b"k-null", map(&[("x", Value::Null)])),
        (b"k-missing", map(&[("other", Value::Int(0))])),
    ]
}

#[test]
fn filters_smoke_field_eq_selects_matching_rows() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(
        b"a",
        &map(&[("category", text("blog")), ("n", Value::Int(1))]),
    )
    .unwrap();
    c.insert(
        b"b",
        &map(&[("category", text("news")), ("n", Value::Int(2))]),
    )
    .unwrap();
    c.insert(
        b"c",
        &map(&[("category", text("blog")), ("n", Value::Int(3))]),
    )
    .unwrap();

    let rows = c
        .query()
        .filter(field("category").eq(text("blog")))
        .run()
        .unwrap();

    let mut keys: Vec<&[u8]> = rows.iter().map(|r| r.key.as_slice()).collect();
    keys.sort();
    assert_eq!(keys, vec![&b"a"[..], &b"c"[..]]);
    for r in &rows {
        assert_eq!(r.document.get("category"), Some(&text("blog")));
        assert_eq!(r.score, 0.0); // a pure filter query carries no rank score
    }
}

// ===========================================================================
// Compare: Eq across the whole Value lattice
// ===========================================================================

#[test]
fn filters_compare_eq_matches_each_value_kind() {
    let d = kinds_doc();
    // Int/Float interop: numerically equal across the two number kinds.
    assert!(field("i").eq(Value::Int(7)).eval(&d));
    assert!(field("i").eq(Value::Float(7.0)).eval(&d));
    assert!(field("f").eq(Value::Float(2.5)).eval(&d));
    assert!(!field("f").eq(Value::Int(2)).eval(&d));
    assert!(!field("i").eq(text("7")).eval(&d)); // no number/text mixing
    // Text, Bool, Bytes: structural equality.
    assert!(field("t").eq(text("héllo 🐦")).eval(&d));
    assert!(field("b").eq(Value::Bool(true)).eval(&d));
    assert!(!field("b").eq(Value::Int(1)).eval(&d));
    assert!(field("by").eq(Value::Bytes(vec![1, 2, 255])).eval(&d));
    assert!(!field("by").eq(Value::Bytes(vec![1, 2, 254])).eval(&d));
    // Containers: exact structural equality, recursively.
    assert!(field("v").eq(Value::Vector(vec![1.0, 2.0])).eval(&d));
    assert!(!field("v").eq(Value::Vector(vec![1.0, 2.0, 3.0])).eval(&d));
    assert!(
        field("a")
            .eq(Value::Array(vec![Value::Int(1), text("x")]))
            .eval(&d)
    );
    assert!(!field("a").eq(Value::Array(vec![Value::Int(1)])).eval(&d));
    assert!(field("m").eq(map(&[("k", Value::Int(1))])).eval(&d));
    assert!(!field("m").eq(map(&[("k", Value::Int(2))])).eval(&d));
    // A PRESENT Null equals Null; a MISSING path never does.
    assert!(field("z").eq(Value::Null).eval(&d));
    assert!(!field("nope").eq(Value::Null).eval(&d));

    // Set level: same lattice over a corpus with one kind per document.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kinds", &kinds_corpus());
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Int(7))),
        ks(&["k-float", "k-int"]) // the Float(7.0) doc matches the Int literal
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Float(7.0))),
        ks(&["k-float", "k-int"]) // and vice versa
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(text("7"))),
        ks(&["k-text"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Bool(true))),
        ks(&["k-bool"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Bytes(vec![1, 2, 255]))),
        ks(&["k-bytes"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Vector(vec![1.0, 2.0]))),
        ks(&["k-vec"])
    );
    assert_eq!(
        matching_keys(
            &c,
            &field("x").eq(Value::Array(vec![Value::Int(1), text("x")]))
        ),
        ks(&["k-arr"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(map(&[("k", Value::Int(1))]))),
        ks(&["k-map"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").eq(Value::Null)),
        ks(&["k-null"])
    );
}

// ===========================================================================
// Compare: Ne + the missing-path rule
// ===========================================================================

#[test]
fn filters_compare_ne_and_missing_path_semantics() {
    let d = kinds_doc();
    assert!(field("i").ne(Value::Int(8)).eval(&d));
    assert!(!field("i").ne(Value::Int(7)).eval(&d));
    assert!(!field("i").ne(Value::Float(7.0)).eval(&d)); // numeric interop
    assert!(field("t").ne(Value::Int(7)).eval(&d)); // different kind → not equal
    assert!(!field("z").ne(Value::Null).eval(&d)); // present Null IS Null

    // The surprising pin: Ne on a MISSING path is FALSE — a missing field
    // survives no comparison, negated or not. (Not(missing-compare) is true.)
    for op in [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ] {
        assert!(
            !Predicate::Compare {
                path: "nope".to_owned(),
                op,
                value: Value::Int(1),
            }
            .eval(&d),
            "missing path must compare false for {op:?}"
        );
    }

    // Set level: ne keeps docs whose x is PRESENT and not numerically equal —
    // the missing-field doc drops out even under Ne.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kinds", &kinds_corpus());
    assert_eq!(
        matching_keys(&c, &field("x").ne(Value::Int(7))),
        ks(&[
            "k-arr", "k-bool", "k-bytes", "k-map", "k-null", "k-text", "k-vec"
        ])
    );
    assert_eq!(
        matching_keys(&c, &field("x").ne(Value::Null)),
        ks(&[
            "k-arr", "k-bool", "k-bytes", "k-float", "k-int", "k-map", "k-text", "k-vec"
        ])
    );
}

// ===========================================================================
// Compare: ordered operators over numbers, with the float edges
// ===========================================================================

#[test]
fn filters_ordered_comparisons_numbers_and_edges() {
    let d = map(&[("n", Value::Int(7)), ("f", Value::Float(2.5))]);
    assert!(field("n").lt(Value::Int(8)).eval(&d));
    assert!(!field("n").lt(Value::Int(7)).eval(&d));
    assert!(field("n").le(Value::Int(7)).eval(&d));
    assert!(field("n").gt(Value::Int(6)).eval(&d));
    assert!(!field("n").gt(Value::Int(7)).eval(&d));
    assert!(field("n").ge(Value::Int(7)).eval(&d));
    // Cross-type: int field vs float constant, and vice versa.
    assert!(field("n").gt(Value::Float(6.5)).eval(&d));
    assert!(!field("n").gt(Value::Float(7.0)).eval(&d));
    assert!(field("n").ge(Value::Float(7.0)).eval(&d));
    assert!(field("f").lt(Value::Int(3)).eval(&d));
    assert!(!field("f").lt(Value::Int(2)).eval(&d));
    assert!(field("f").ge(Value::Int(2)).eval(&d));

    // Float extremes and the integer extremes.
    let e = map(&[
        ("inf", Value::Float(f64::INFINITY)),
        ("ninf", Value::Float(f64::NEG_INFINITY)),
        ("negz", Value::Float(-0.0)),
        ("posz", Value::Float(0.0)),
        ("max", Value::Float(f64::MAX)),
        ("imin", Value::Int(i64::MIN)),
        ("imax", Value::Int(i64::MAX)),
    ]);
    assert!(field("inf").gt(Value::Float(f64::MAX)).eval(&e));
    assert!(field("inf").ge(Value::Float(f64::INFINITY)).eval(&e));
    assert!(!field("inf").gt(Value::Float(f64::INFINITY)).eval(&e));
    assert!(field("ninf").lt(Value::Float(-f64::MAX)).eval(&e));
    assert!(field("max").lt(Value::Float(f64::INFINITY)).eval(&e));
    // -0.0 and 0.0 are EQUAL: strict orders disagree, non-strict agree.
    assert!(field("negz").eq(Value::Float(0.0)).eval(&e));
    assert!(field("negz").le(Value::Float(0.0)).eval(&e));
    assert!(field("negz").ge(Value::Float(0.0)).eval(&e));
    assert!(!field("negz").lt(Value::Float(0.0)).eval(&e));
    assert!(!field("negz").gt(Value::Float(0.0)).eval(&e));
    assert!(!field("negz").ne(Value::Float(0.0)).eval(&e));
    // Same-type integer extremes compare exactly.
    assert!(field("imin").lt(Value::Int(i64::MAX)).eval(&e));
    assert!(field("imax").le(Value::Int(i64::MAX)).eval(&e));
    assert!(field("imax").gt(Value::Int(i64::MIN)).eval(&e));
    // Mixed int/float near the top: i64::MAX as f64 still exceeds 1e18.
    assert!(field("imax").gt(Value::Float(1e18)).eval(&e));

    // Set level over a mixed int/float corpus.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "nums",
        &[
            (b"n1", map(&[("n", Value::Int(1))])),
            (b"n2", map(&[("n", Value::Int(5))])),
            (b"n3", map(&[("n", Value::Int(9))])),
            (b"n4", map(&[("n", Value::Float(5.0))])),
            (b"n5", map(&[("n", Value::Float(9.5))])),
            (b"n6", map(&[("n", Value::Int(-3))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("n").gt(Value::Int(5))),
        ks(&["n3", "n5"])
    );
    assert_eq!(
        matching_keys(&c, &field("n").gt(Value::Float(4.5))),
        ks(&["n2", "n3", "n4", "n5"]) // the Int(5) doc crosses into a float range
    );
    assert_eq!(
        matching_keys(&c, &field("n").le(Value::Int(5))),
        ks(&["n1", "n2", "n4", "n6"]) // 1 <= 5 too
    );
    assert_eq!(
        matching_keys(&c, &field("n").lt(Value::Int(0))),
        ks(&["n6"])
    );
    assert_eq!(
        matching_keys(&c, &field("n").ge(Value::Int(5))),
        ks(&["n2", "n3", "n4", "n5"])
    );
}

// ===========================================================================
// NaN semantics
// ===========================================================================

#[test]
fn filters_nan_comparisons_all_false_except_ne() {
    let d = map(&[("x", Value::Float(f64::NAN))]);
    // Against a NaN field every comparison is false — except Ne, which is the
    // negation of "NaN equals it" (it never does), so it is TRUE for every
    // constant, NaN included.
    assert!(!field("x").eq(Value::Float(f64::NAN)).eval(&d));
    assert!(!field("x").eq(Value::Float(1.0)).eval(&d));
    assert!(field("x").ne(Value::Float(f64::NAN)).eval(&d));
    assert!(field("x").ne(Value::Int(0)).eval(&d));
    assert!(!field("x").lt(Value::Float(f64::INFINITY)).eval(&d));
    assert!(!field("x").le(Value::Float(f64::NEG_INFINITY)).eval(&d));
    assert!(!field("x").gt(Value::Float(f64::NEG_INFINITY)).eval(&d));
    assert!(!field("x").ge(Value::Float(f64::INFINITY)).eval(&d));
    // NaN as the CONSTANT: the same lattice from the other side.
    assert!(!field("x").lt(Value::Float(f64::NAN)).eval(&d));
    let d2 = map(&[("y", Value::Int(5))]);
    assert!(!field("y").gt(Value::Float(f64::NAN)).eval(&d2));
    assert!(field("y").ne(Value::Float(f64::NAN)).eval(&d2));
    // Between is Ge && Le: a NaN bound on either side matches nothing.
    assert!(
        !field("y")
            .between(Value::Float(f64::NAN), Value::Int(10))
            .eval(&d2)
    );
    assert!(
        !field("y")
            .between(Value::Int(0), Value::Float(f64::NAN))
            .eval(&d2)
    );

    // Set level: a NaN doc drops out of ranges but is kept by Ne.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "nan",
        &[
            (b"ok", map(&[("f", Value::Float(1.0))])),
            (b"bad", map(&[("f", Value::Float(f64::NAN))])),
            (b"none", map(&[("g", Value::Int(0))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("f").gt(Value::Float(0.5))),
        ks(&["ok"])
    );
    // Ne keeps every doc with a PRESENT f that is not 0.5 — the NaN doc
    // ("not equal to 0.5") and the 1.0 doc, but not the missing one.
    assert_eq!(
        matching_keys(&c, &field("f").ne(Value::Float(0.5))),
        ks(&["bad", "ok"])
    );
    // NaN cannot be selected by equality at all.
    assert_eq!(
        matching_keys(&c, &field("f").eq(Value::Float(f64::NAN))),
        ks(&[])
    );
}

// ===========================================================================
// Int/Float precision beyond 2^53 (documented conversion rule)
// ===========================================================================

#[test]
fn filters_int_float_precision_beyond_2_pow_53() {
    let p1: i64 = (1 << 53) + 1;
    let p2: i64 = (1 << 53) + 2;
    let d = map(&[("a", Value::Int(p1)), ("b", Value::Int(p2))]);
    // Documented precision rule: mixed Int/Float comparisons convert the
    // integer through f64, whose spacing is 2 above 2^53 — the ODD neighbor
    // 2^53+1 rounds down and compares EQUAL to 2^53, while the even
    // 2^53+2 is exactly representable and stays distinct.
    assert!(field("a").eq(Value::Float(9007199254740992.0)).eval(&d));
    assert!(!field("b").eq(Value::Float(9007199254740992.0)).eval(&d));
    assert!(field("b").eq(Value::Float(9007199254740994.0)).eval(&d));
    // Same-type Int comparisons stay exact.
    assert!(field("a").lt(Value::Int(p2)).eval(&d));
    assert!(!field("a").eq(Value::Int(p2)).eval(&d));
    assert!(field("a").ne(Value::Int(p2)).eval(&d));
    assert!(field("b").ge(Value::Int(p2)).eval(&d));

    // Set level: the float constant that equals a's rounding matches only a;
    // the even neighbor keeps its own f64 identity; Int constants stay exact.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "prec",
        &[
            (b"p1", map(&[("n", Value::Int(p1))])),
            (b"p2", map(&[("n", Value::Int(p2))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("n").eq(Value::Float(9007199254740992.0))),
        ks(&["p1"]) // 2^53+1 rounds to 2^53
    );
    assert_eq!(
        matching_keys(&c, &field("n").eq(Value::Float(9007199254740994.0))),
        ks(&["p2"]) // 2^53+2 is exactly representable
    );
    assert_eq!(
        matching_keys(&c, &field("n").ne(Value::Int(p1))),
        ks(&["p2"])
    );
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(p1), Value::Int(p2))),
        ks(&["p1", "p2"]) // exact same-type bounds
    );
}

// ===========================================================================
// Text ordering (UTF-8 byte order == code point order)
// ===========================================================================

#[test]
fn filters_text_ordering_lexicographic_utf8() {
    let e = map(&[
        ("up", text("Z")),
        ("lo", text("a")),
        ("empty", text("")),
        ("pre", text("ab")),
        ("acute", text("é")),
    ]);
    assert!(field("up").lt(text("a")).eval(&e)); // 'Z' (U+005A) < 'a' (U+0061)
    assert!(field("empty").lt(text("a")).eval(&e)); // "" sorts before anything
    assert!(field("lo").lt(text("ab")).eval(&e)); // "a" < "ab": prefix first
    assert!(field("pre").gt(text("a")).eval(&e));
    assert!(field("pre").lt(text("b")).eval(&e));
    assert!(field("acute").gt(text("z")).eval(&e)); // U+00E9 (2 UTF-8 bytes) > 'z'
    assert!(field("acute").lt(text("🐦")).eval(&e)); // BMP < U+1F426
    assert!(field("acute").ge(text("é")).eval(&e)); // equal text is >= and <=
    assert!(field("acute").le(text("é")).eval(&e));
    assert!(!field("acute").gt(text("é")).eval(&e));

    // Set level over a unicode corpus.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "texts",
        &[
            (b"t-apple", map(&[("t", text("apple"))])),
            (b"t-banana", map(&[("t", text("Banana"))])),
            (b"t-bird", map(&[("t", text("🐦"))])),
            (b"t-eclair", map(&[("t", text("éclair"))])),
            (b"t-empty", map(&[("t", text(""))])),
            (b"t-z", map(&[("t", text("z"))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("t").lt(text("m"))),
        ks(&["t-apple", "t-banana", "t-empty"]) // "", "Banana", "apple" < "m"
    );
    assert_eq!(
        matching_keys(&c, &field("t").gt(text("m"))),
        ks(&["t-bird", "t-eclair", "t-z"]) // "z" < "éclair" < "🐦"
    );
    assert_eq!(
        matching_keys(&c, &field("t").eq(text(""))),
        ks(&["t-empty"])
    );
    assert_eq!(
        matching_keys(&c, &field("t").between(text("a"), text("e"))),
        ks(&["t-apple"]) // only "apple" lies in ["a", "e"]
    );
}

// ===========================================================================
// Unordered kinds: bools, bytes, null, containers, mixed kinds
// ===========================================================================

#[test]
fn filters_unordered_kinds_compare_false_for_ordered_ops() {
    let d = kinds_doc();
    // PIN: bools are NOT ordered (no false < true) — equality only.
    assert!(field("b").eq(Value::Bool(true)).eval(&d));
    assert!(!field("b").gt(Value::Bool(false)).eval(&d));
    assert!(!field("b").lt(Value::Bool(true)).eval(&d));
    assert!(!field("b").ge(Value::Bool(true)).eval(&d)); // even equal values
    assert!(!field("b").le(Value::Bool(false)).eval(&d));
    // Bytes, Null, Vector, Array, Map: equal only structurally, never ordered.
    assert!(!field("by").lt(Value::Bytes(vec![2])).eval(&d));
    assert!(!field("z").lt(Value::Null).eval(&d));
    assert!(!field("v").lt(Value::Vector(vec![9.0])).eval(&d));
    assert!(!field("a").le(Value::Array(vec![])).eval(&d));
    assert!(!field("m").gt(map(&[])).eval(&d));
    // Mixed kinds never order and never equal.
    assert!(!field("i").lt(text("a")).eval(&d));
    assert!(!field("t").gt(Value::Int(0)).eval(&d));
    assert!(!field("b").lt(Value::Int(0)).eval(&d));
    assert!(!field("i").eq(Value::Bool(true)).eval(&d)); // 1 != true
    assert!(field("t").ne(Value::Int(7)).eval(&d)); // but "not equal" holds

    // Set level: unordered kinds fall out of range queries entirely.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "unord",
        &[
            (b"u1", map(&[("x", Value::Bool(true))])),
            (b"u2", map(&[("x", Value::Vector(vec![1.0]))])),
            (b"u3", map(&[("x", Value::Array(vec![Value::Int(1)]))])),
            (b"u4", map(&[("x", text("m"))])),
        ],
    );
    assert_eq!(matching_keys(&c, &field("x").gt(Value::Int(0))), ks(&[]));
    assert_eq!(
        matching_keys(&c, &field("x").between(Value::Int(0), Value::Int(9))),
        ks(&[])
    );
    assert_eq!(matching_keys(&c, &field("x").gt(text("a"))), ks(&["u4"]));
}

// ===========================================================================
// Nested dotted paths
// ===========================================================================

fn nested_doc(name: &str) -> Value {
    map(&[
        (
            "meta",
            map(&[("author", map(&[("name", text(name)), ("nick", text("n"))]))]),
        ),
        (
            "deep",
            map(&[("a", map(&[("b", map(&[("c", Value::Int(42))]))]))]),
        ),
        ("score", Value::Int(7)),
        ("tags", Value::Array(vec![Value::Int(1), text("x")])),
    ])
}

#[test]
fn filters_nested_dotted_paths_traverse_maps_only() {
    let d = nested_doc("ada");
    assert!(field("meta.author.name").eq(text("ada")).eval(&d)); // depth 3
    assert!(!field("meta.author.name").eq(text("bob")).eval(&d));
    assert!(field("deep.a.b.c").eq(Value::Int(42)).eval(&d)); // depth 4
    // A path may resolve to a whole container.
    assert!(
        field("meta.author")
            .eq(map(&[("name", text("ada")), ("nick", text("n"))]))
            .eval(&d)
    );
    assert!(
        field("tags")
            .eq(Value::Array(vec![Value::Int(1), text("x")]))
            .eval(&d)
    );
    // PIN: numeric array indexing is NOT supported — resolve descends only
    // through maps, so "tags.0" (and any path through an array) misses.
    assert!(!field("tags.0").exists().eval(&d));
    assert!(!field("tags.0").eq(Value::Int(1)).eval(&d));
    // Descending through a scalar or a missing key misses.
    assert!(!field("score.deeper").exists().eval(&d));
    assert!(!field("meta.nope.deeper").exists().eval(&d));

    // Set level: depth-3 filters select the right rows; a doc without the
    // nested structure is excluded (missing path → false, Ne included).
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "nested",
        &[
            (b"a", nested_doc("ada")),
            (b"b", nested_doc("bob")),
            (b"m", map(&[("score", Value::Int(1))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("meta.author.name").eq(text("ada"))),
        ks(&["a"])
    );
    assert_eq!(
        matching_keys(&c, &field("meta.author.name").ne(text("ada"))),
        ks(&["b"]) // the doc without meta.author.name drops out of Ne too
    );
    assert_eq!(
        matching_keys(&c, &field("deep.a.b.c").ge(Value::Int(42))),
        ks(&["a", "b"])
    );
}

// ===========================================================================
// Exists
// ===========================================================================

#[test]
fn filters_exists_presence_semantics() {
    let d = kinds_doc();
    assert!(field("i").exists().eval(&d));
    // PIN: a present Null COUNTS as present.
    assert!(field("z").exists().eval(&d));
    assert!(field("m").exists().eval(&d)); // containers too
    assert!(field("meta").exists().eval(&d));
    assert!(!field("nope").exists().eval(&d));
    assert!(!field("i.deeper").exists().eval(&d)); // through a scalar
    // Nested present vs missing.
    assert!(field("meta.author.name").exists().eval(&d));
    assert!(!field("meta.author.missing").exists().eval(&d));
    assert!(!field("meta.missing.deeper").exists().eval(&d));

    // Set level: Exists is the only way to select "field present" regardless
    // of kind; its negation selects exactly the docs missing the field.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kinds", &kinds_corpus());
    assert_eq!(
        matching_keys(&c, &field("x").exists()),
        ks(&[
            "k-arr", "k-bool", "k-bytes", "k-float", "k-int", "k-map", "k-null", "k-text", "k-vec"
        ])
    );
    assert_eq!(matching_keys(&c, &!field("x").exists()), ks(&["k-missing"]));
}

// ===========================================================================
// In
// ===========================================================================

#[test]
fn filters_in_membership_matrix() {
    let d = kinds_doc();
    assert!(field("i").is_in([Value::Int(6), Value::Int(7)]).eval(&d));
    assert!(field("i").is_in([Value::Float(7.0)]).eval(&d)); // numeric interop
    assert!(!field("i").is_in([Value::Int(6)]).eval(&d));
    assert!(field("i").is_in([Value::Int(7), Value::Int(7)]).eval(&d)); // duplicates
    // Mixed-kind lists compare per element.
    assert!(field("t").is_in([Value::Int(7), text("héllo 🐦")]).eval(&d));
    // Non-scalar members match structurally.
    assert!(field("v").is_in([Value::Vector(vec![1.0, 2.0])]).eval(&d));
    assert!(field("z").is_in([Value::Null]).eval(&d));
    // NaN membership never hits.
    assert!(!field("i").is_in([Value::Float(f64::NAN)]).eval(&d));
    // PIN: an EMPTY values vec matches nothing (not an error) — even when the
    // field is present.
    assert!(!field("i").is_in(Vec::<Value>::new()).eval(&d));

    // Set level over the kinds corpus.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kinds", &kinds_corpus());
    assert_eq!(
        matching_keys(&c, &field("x").is_in([Value::Int(7), text("7")])),
        ks(&["k-float", "k-int", "k-text"])
    );
    assert_eq!(
        matching_keys(
            &c,
            &field("x").is_in([Value::Vector(vec![1.0, 2.0]), Value::Null])
        ),
        ks(&["k-null", "k-vec"])
    );
    assert_eq!(
        matching_keys(&c, &field("x").is_in(Vec::<Value>::new())),
        ks(&[])
    );
    assert_eq!(
        matching_keys(&c, &field("nope").is_in([Value::Int(7)])),
        ks(&[]) // missing path → false
    );
}

// ===========================================================================
// Between
// ===========================================================================

#[test]
fn filters_between_inclusive_and_degenerate_bounds() {
    let d = map(&[("n", Value::Int(5)), ("t", text("blog"))]);
    assert!(field("n").between(Value::Int(5), Value::Int(9)).eval(&d)); // low edge in
    assert!(field("n").between(Value::Int(1), Value::Int(5)).eval(&d)); // high edge in
    assert!(field("n").between(Value::Int(5), Value::Int(5)).eval(&d)); // low == high == value
    assert!(!field("n").between(Value::Int(6), Value::Int(6)).eval(&d));
    // PIN: inverted bounds (low > high) match nothing — not an error.
    assert!(!field("n").between(Value::Int(9), Value::Int(4)).eval(&d));
    // Mixed int/float bounds interoperate numerically.
    assert!(
        field("n")
            .between(Value::Int(4), Value::Float(5.5))
            .eval(&d)
    );
    assert!(
        !field("n")
            .between(Value::Float(5.5), Value::Int(9))
            .eval(&d)
    );
    // Text ranges are lexicographic and inclusive.
    assert!(field("t").between(text("bl"), text("c")).eval(&d));
    assert!(!field("t").between(text("a"), text("bl")).eval(&d));
    assert!(field("t").between(text("blog"), text("blog")).eval(&d));
    // Missing path, NaN bounds, unordered kinds: all false.
    assert!(!field("nope").between(Value::Int(0), Value::Int(9)).eval(&d));
    assert!(
        !field("n")
            .between(Value::Float(f64::NAN), Value::Int(9))
            .eval(&d)
    );
    let u = map(&[("b", Value::Bool(true))]);
    assert!(
        !field("b")
            .between(Value::Bool(false), Value::Bool(true))
            .eval(&u)
    );

    // Set level.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "betw",
        &[
            (b"q2", map(&[("n", Value::Int(2))])),
            (b"q5", map(&[("n", Value::Int(5))])),
            (b"q9", map(&[("n", Value::Int(9))])),
            (b"q11", map(&[("n", Value::Int(11))])),
            (b"qblog", map(&[("t", text("blog"))])),
            (b"qnone", map(&[("x", Value::Null)])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(5), Value::Int(9))),
        ks(&["q5", "q9"])
    );
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(9), Value::Int(9))),
        ks(&["q9"])
    );
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(11), Value::Int(2))),
        ks(&[]) // inverted
    );
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(4), Value::Float(9.5))),
        ks(&["q5", "q9"])
    );
    assert_eq!(
        matching_keys(&c, &field("t").between(text("bl"), text("c"))),
        ks(&["qblog"])
    );
}

// ===========================================================================
// StartsWith
// ===========================================================================

#[test]
fn filters_starts_with_prefix_semantics() {
    let d = map(&[
        ("t", text("héllo 🐦")),
        ("empty", text("")),
        ("n", Value::Int(7)),
    ]);
    assert!(field("t").starts_with("h").eval(&d));
    assert!(field("t").starts_with("hé").eval(&d)); // unicode prefix
    assert!(field("t").starts_with("héllo 🐦").eval(&d)); // exact-equal matches
    assert!(!field("t").starts_with("H").eval(&d)); // case-sensitive
    assert!(!field("t").starts_with("llo").eval(&d)); // middle is not a prefix
    assert!(!field("t").starts_with("héllo 🐦!").eval(&d)); // longer than value
    // PIN: the empty prefix matches every TEXT — the empty string included.
    assert!(field("t").starts_with("").eval(&d));
    assert!(field("empty").starts_with("").eval(&d));
    assert!(!field("empty").starts_with("a").eval(&d));
    // Non-text fields never match, empty prefix included.
    assert!(!field("n").starts_with("").eval(&d));
    assert!(!field("n").starts_with("7").eval(&d));
    assert!(!field("nope").starts_with("").eval(&d));

    // Set level over a text corpus with non-text neighbors.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "prefix",
        &[
            (b"p1", map(&[("t", text("blog"))])),
            (b"p2", map(&[("t", text("news"))])),
            (b"p3", map(&[("t", text("almanac"))])),
            (b"p4", map(&[("t", text(""))])),
            (b"p5", map(&[("n", Value::Int(7))])),
            (b"p6", map(&[])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("t").starts_with("bl")),
        ks(&["p1"])
    );
    assert_eq!(matching_keys(&c, &field("t").starts_with("a")), ks(&["p3"]));
    assert_eq!(
        matching_keys(&c, &field("t").starts_with("")),
        ks(&["p1", "p2", "p3", "p4"]) // every TEXT value, nothing else
    );
    assert_eq!(matching_keys(&c, &field("t").starts_with("blog!")), ks(&[]));
}

// ===========================================================================
// Contains
// ===========================================================================

#[test]
fn filters_contains_substring_semantics() {
    let d = map(&[
        ("t", text("corvid db")),
        ("u", text("héllo 🐦")),
        ("empty", text("")),
        ("n", Value::Int(7)),
    ]);
    assert!(field("t").contains("cor").eval(&d)); // at the start
    assert!(field("t").contains("rvi").eval(&d)); // in the middle
    assert!(field("t").contains("db").eval(&d)); // at the end
    assert!(field("t").contains("corvid db").eval(&d)); // the whole string
    assert!(!field("t").contains("dbx").eval(&d));
    assert!(!field("t").contains("Cor").eval(&d)); // case-sensitive
    assert!(field("u").contains("é").eval(&d)); // unicode
    assert!(field("u").contains("🐦").eval(&d));
    assert!(field("u").contains("llo 🐦").eval(&d));
    // PIN: the empty substring matches every TEXT (empty string included) and
    // nothing else.
    assert!(field("t").contains("").eval(&d));
    assert!(field("empty").contains("").eval(&d));
    assert!(!field("n").contains("").eval(&d));
    assert!(!field("n").contains("7").eval(&d));
    assert!(!field("nope").contains("").eval(&d));

    // Set level.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "substr",
        &[
            (b"c1", map(&[("body", text("the quick brown fox"))])),
            (b"c2", map(&[("body", text("lazy dog"))])),
            (b"c3", map(&[("body", text("quickly done"))])),
            (b"c4", map(&[("n", Value::Int(7))])),
        ],
    );
    assert_eq!(
        matching_keys(&c, &field("body").contains("quick")),
        ks(&["c1", "c3"])
    );
    assert_eq!(
        matching_keys(&c, &field("body").contains("dog")),
        ks(&["c2"])
    );
    assert_eq!(matching_keys(&c, &field("body").contains("fox!")), ks(&[]));
    assert_eq!(
        matching_keys(&c, &field("body").contains("")),
        ks(&["c1", "c2", "c3"]) // every text body, not the int doc
    );
}

// ===========================================================================
// GeoWithin (deep geo cases are Task 10; formats, boundary, validation pin)
// ===========================================================================

fn geo_corpus() -> Vec<(&'static [u8], Value)> {
    vec![
        // [lat, lon] float array.
        (
            b"lon-arr",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(51.5), Value::Float(-0.13)]),
            )]),
        ),
        // {lat, lon} map (floats).
        (
            b"lon-map",
            map(&[(
                "loc",
                map(&[("lat", Value::Float(51.5)), ("lon", Value::Float(-0.13))]),
            )]),
        ),
        // Int coordinates are accepted in both formats.
        (
            b"origin-int",
            map(&[("loc", Value::Array(vec![Value::Int(0), Value::Int(0)]))]),
        ),
        // A map with extra keys beside lat/lon still extracts the point.
        (
            b"paris",
            map(&[(
                "loc",
                map(&[
                    ("lat", Value::Float(48.8566)),
                    ("lon", Value::Float(2.3522)),
                    ("label", text("fr")),
                ]),
            )]),
        ),
        // A point exactly at a known haversine distance east of (0,0).
        (
            b"east",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(1.0), Value::Float(0.0)]),
            )]),
        ),
        // Out-of-range latitude: PIN — no validation, evaluated mathematically.
        (
            b"weird",
            map(&[(
                "loc",
                Value::Array(vec![Value::Float(95.0), Value::Float(0.0)]),
            )]),
        ),
        // Invalid shapes: never match, never error.
        (
            b"bad-len",
            map(&[(
                "loc",
                Value::Array(vec![
                    Value::Float(1.0),
                    Value::Float(2.0),
                    Value::Float(3.0),
                ]),
            )]),
        ),
        (
            b"bad-map",
            map(&[("loc", map(&[("lat", Value::Float(1.0))]))]),
        ),
        (b"bad-kind", map(&[("loc", text("not a point"))])),
        (b"no-loc", map(&[("other", Value::Int(1))])),
    ]
}

#[test]
fn filters_geo_within_point_formats_and_boundary() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "geo", &geo_corpus());

    // Both point formats at the same location match the same query.
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(51.5, -0.13, 1.0)),
        ks(&["lon-arr", "lon-map"])
    );
    // Paris is ~344 km from London: inside 400, outside 300.
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(51.5, -0.13, 400.0)),
        ks(&["lon-arr", "lon-map", "paris"])
    );
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(51.5, -0.13, 300.0)),
        ks(&["lon-arr", "lon-map"])
    );
    // Int coordinates extract to the same point: (0,0).
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, 1.0)),
        ks(&["origin-int"])
    );
    // Zero radius: only a point at EXACTLY the center (distance 0 <= 0).
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(51.5, -0.13, 0.0)),
        ks(&["lon-arr", "lon-map"])
    );
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(51.5, 0.0, 0.0)),
        ks(&[])
    );
    // Boundary at EXACTLY radius_km is inclusive: the east doc sits at
    // haversine_km(0,0,1,0) from the origin.
    let d = haversine_km(0.0, 0.0, 1.0, 0.0);
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, d)),
        ks(&["east", "origin-int"])
    );
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, d - 0.001)), // 1 m short
        ks(&["origin-int"])
    );
    // PIN: out-of-range latitude (95°) is not rejected — it is evaluated
    // mathematically (~10,544 km from the origin along the meridian), so a
    // 10,000 km radius excludes it while taking the ~5,400-5,700 km London
    // and Paris points, and an 11,000 km radius takes it too.
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, 10000.0)),
        ks(&["east", "lon-arr", "lon-map", "origin-int", "paris"])
    );
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, 11000.0)),
        ks(&["east", "lon-arr", "lon-map", "origin-int", "paris", "weird"])
    );
    // Invalid shapes and missing fields never match, without erroring — a
    // radius beyond the maximum great-circle distance takes every VALID point.
    assert_eq!(
        matching_keys(&c, &field("loc").within_km(0.0, 0.0, 40075.0)),
        ks(&["east", "lon-arr", "lon-map", "origin-int", "paris", "weird"])
    );
}

// ===========================================================================
// And / Or / Not: nesting, operand order, De Morgan
// ===========================================================================

/// r1 (x,1) r2 (x,2) r3 (y,2) r4 (y,9) r5 (x,—) r6 (—,2): "—" = field missing.
fn logic_corpus() -> Vec<(&'static [u8], Value)> {
    vec![
        (b"r1", map(&[("t", text("x")), ("n", Value::Int(1))])),
        (b"r2", map(&[("t", text("x")), ("n", Value::Int(2))])),
        (b"r3", map(&[("t", text("y")), ("n", Value::Int(2))])),
        (b"r4", map(&[("t", text("y")), ("n", Value::Int(9))])),
        (b"r5", map(&[("t", text("x"))])),
        (b"r6", map(&[("n", Value::Int(2))])),
    ]
}

#[test]
fn filters_and_or_not_nesting_and_de_morgan() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "logic", &logic_corpus());
    let a = field("t").eq(text("x"));
    let b = field("n").ge(Value::Int(2));
    let cc = field("n").eq(Value::Int(9));

    // Nesting depth 3 (And over And over Not).
    assert_eq!(
        matching_keys(&c, &a.clone().and(b.clone()).and(!cc.clone())),
        ks(&["r2"])
    );
    // Not(A) matches docs whose t is present-and-different AND docs missing t
    // (their A is false) — the missing-field negation pin, at set level.
    assert_eq!(
        matching_keys(&c, &(!a.clone()).and(b.clone())),
        ks(&["r3", "r4", "r6"])
    );
    // Operand order symmetry for both connectives.
    assert_eq!(
        matching_keys(&c, &a.clone().and(b.clone())),
        matching_keys(&c, &b.clone().and(a.clone()))
    );
    assert_eq!(
        matching_keys(&c, &a.clone().or(b.clone())),
        matching_keys(&c, &b.clone().or(a.clone()))
    );
    // PIN: Not of a missing-field Compare is TRUE — such a predicate matches
    // EVERY document in the collection.
    assert_eq!(
        matching_keys(&c, &!field("zzz").gt(Value::Int(0))),
        ks(&["r1", "r2", "r3", "r4", "r5", "r6"])
    );
    // Not(Exists) selects exactly the docs missing the field.
    assert_eq!(matching_keys(&c, &!field("t").exists()), ks(&["r6"]));
    assert_eq!(
        matching_keys(&c, &!field("zz").exists()),
        ks(&["r1", "r2", "r3", "r4", "r5", "r6"])
    );
    // De Morgan equivalence, both directions, at set level.
    assert_eq!(
        matching_keys(&c, &!(a.clone().or(b.clone()))),
        matching_keys(&c, &(!a.clone()).and(!b.clone()))
    );
    assert_eq!(
        matching_keys(&c, &!(a.clone().and(b.clone()))),
        matching_keys(&c, &(!a.clone()).or(!b.clone()))
    );
    // And the concrete sets, so the equivalence is over non-trivial sets.
    assert_eq!(
        matching_keys(&c, &!(a.clone().and(b.clone()))),
        ks(&["r1", "r3", "r4", "r5", "r6"])
    );
}

#[test]
fn filters_predicate_combinators_and_direct_construction() {
    let a = field("t").eq(text("x"));
    let b = field("n").ge(Value::Int(2));
    let cc = field("n").eq(Value::Int(9));
    // The combinator methods build exactly the enum forms.
    assert_eq!(
        a.clone().and(b.clone()),
        Predicate::And(Box::new(a.clone()), Box::new(b.clone()))
    );
    assert_eq!(
        a.clone().or(b.clone()),
        Predicate::Or(Box::new(a.clone()), Box::new(b.clone()))
    );
    assert_eq!(!a.clone(), Predicate::Not(Box::new(a.clone())));

    // The builder has NO operator precedence: composition is explicit and
    // left-associative by chaining — the two shapes are different predicates
    // with different result sets.
    let left = a.clone().and(b.clone()).or(cc.clone());
    let right = a.clone().and(b.clone().or(cc.clone()));
    assert_ne!(left, right);
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "logic", &logic_corpus());
    // (t=x AND n>=2) OR n=9 → r2 and r4.
    assert_eq!(matching_keys(&c, &left), ks(&["r2", "r4"]));
    // t=x AND (n>=2 OR n=9) → only r2 (r4 has t=y).
    assert_eq!(matching_keys(&c, &right), ks(&["r2"]));
    // Direct enum construction drives the same result through the query path.
    let direct = Predicate::Or(
        Box::new(Predicate::And(Box::new(a), Box::new(b))),
        Box::new(cc),
    );
    assert_eq!(direct, left);
    assert_eq!(matching_keys(&c, &direct), ks(&["r2", "r4"]));
}

// ===========================================================================
// field()/FieldRef builders
// ===========================================================================

#[test]
fn filters_field_builders_produce_claimed_predicates() {
    let cmp = |op| Predicate::Compare {
        path: "n".to_owned(),
        op,
        value: Value::Int(1),
    };
    assert_eq!(field("n").eq(Value::Int(1)), cmp(CmpOp::Eq));
    assert_eq!(field("n").ne(Value::Int(1)), cmp(CmpOp::Ne));
    assert_eq!(field("n").lt(Value::Int(1)), cmp(CmpOp::Lt));
    assert_eq!(field("n").le(Value::Int(1)), cmp(CmpOp::Le));
    assert_eq!(field("n").gt(Value::Int(1)), cmp(CmpOp::Gt));
    assert_eq!(field("n").ge(Value::Int(1)), cmp(CmpOp::Ge));
    assert_eq!(field("n").exists(), Predicate::Exists("n".to_owned()));
    assert_eq!(
        field("n").is_in([Value::Int(1), Value::Int(2)]),
        Predicate::In {
            path: "n".to_owned(),
            values: vec![Value::Int(1), Value::Int(2)],
        }
    );
    assert_eq!(
        field("n").between(Value::Int(1), Value::Int(2)),
        Predicate::Between {
            path: "n".to_owned(),
            low: Value::Int(1),
            high: Value::Int(2),
        }
    );
    assert_eq!(
        field("t").starts_with("bl"),
        Predicate::StartsWith {
            path: "t".to_owned(),
            prefix: "bl".to_owned(),
        }
    );
    assert_eq!(
        field("t").contains("l"),
        Predicate::Contains {
            path: "t".to_owned(),
            substr: "l".to_owned(),
        }
    );
    assert_eq!(
        field("g").within_km(1.0, 2.0, 3.0),
        Predicate::GeoWithin {
            path: "g".to_owned(),
            lat: 1.0,
            lon: 2.0,
            radius_km: 3.0,
        }
    );
    // field() takes any Into<String>: String as well as &str.
    assert_eq!(
        field(String::from("meta.author")).exists(),
        Predicate::Exists("meta.author".to_owned())
    );
    // starts_with/contains take any Into<String> too.
    assert_eq!(
        field("t").starts_with(String::from("bl")),
        field("t").starts_with("bl")
    );
    // And the built predicates behave: a behavioral spot check via eval.
    let d = map(&[("n", Value::Int(7))]);
    assert!(field("n").gt(Value::Int(5)).eval(&d));
    assert!(!field("n").lt(Value::Int(5)).eval(&d));
}

#[test]
fn filters_multiple_filter_calls_intersect_like_and() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "logic", &logic_corpus());
    let a = field("t").eq(text("x"));
    let b = field("n").ge(Value::Int(2));
    let cc = field("n").eq(Value::Int(9));

    let chained = c.query().filter(a.clone()).filter(b.clone()).run().unwrap();
    let composed = c.query().filter(a.clone().and(b.clone())).run().unwrap();
    assert_eq!(keys(&chained), keys(&composed));
    assert_eq!(keys(&chained), ks(&["r2"]));

    // Three filter calls == the and-composition of all three.
    let three = c
        .query()
        .filter(a.clone())
        .filter(b.clone())
        .filter(!cc.clone())
        .run()
        .unwrap();
    let composed3 = c.query().filter(a.and(b.and(!cc))).run().unwrap();
    assert_eq!(keys(&three), keys(&composed3));
    assert_eq!(keys(&three), ks(&["r2"]));
}

// ===========================================================================
// Indexed vs scan equivalence — scalar windows
// ===========================================================================

#[test]
fn filters_indexed_vs_scan_scalar_predicates() {
    // Edge corpus: duplicate ints, an equal float, a text, a bool, a null,
    // and a MISSING value of the indexed field; plus a text field.
    let docs: Vec<(&[u8], Value)> = vec![
        (b"s1", map(&[("n", Value::Int(3)), ("t", text("alpha"))])),
        (b"s2", map(&[("n", Value::Int(7)), ("t", text("blog"))])),
        (b"s3", map(&[("n", Value::Int(7)), ("t", text("alz"))])),
        (
            b"s4",
            map(&[("n", Value::Float(7.0)), ("t", text("blog2"))]),
        ),
        (b"s5", map(&[("n", text("7")), ("t", text("zeta"))])),
        (
            b"s6",
            map(&[("n", Value::Bool(true)), ("t", text("blog3"))]),
        ),
        (b"s7", map(&[("t", text("blog4"))])),
        (b"s8", map(&[("n", Value::Null), ("t", text("q"))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "mix", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "mix", &docs);
    idx.create_scalar_index("n").unwrap();
    idx.create_scalar_index("t").unwrap();

    // Plan shapes: Eq/range/In/Between/StartsWith drive the scalar window;
    // Ne and Exists never do (they scan).
    assert_eq!(
        scan.query()
            .filter(field("n").eq(Value::Int(7)))
            .plan_shape(),
        PlanShape::Scan {
            collection: "mix".to_owned()
        }
    );
    assert_eq!(
        idx.query()
            .filter(field("n").eq(Value::Int(7)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert_eq!(
        idx.query()
            .filter(field("n").gt(Value::Int(2)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert_eq!(
        idx.query()
            .filter(field("n").between(Value::Int(1), Value::Int(4)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert_eq!(
        idx.query()
            .filter(field("n").is_in([Value::Int(3)]))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert_eq!(
        idx.query()
            .filter(field("t").starts_with("bl"))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );
    assert_eq!(
        idx.query()
            .filter(field("n").ne(Value::Int(7)))
            .plan_shape(),
        PlanShape::Scan {
            collection: "mix".to_owned()
        }
    );
    assert_eq!(
        idx.query().filter(field("n").exists()).plan_shape(),
        PlanShape::Scan {
            collection: "mix".to_owned()
        }
    );

    // Identical result sets with and without the indexes — including the
    // duplicate values, the numeric-lane interop, and the missing/mixed-type
    // documents in the indexed field.
    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        (field("n").eq(Value::Int(7)), vec!["s2", "s3", "s4"]),
        (field("n").eq(Value::Float(7.0)), vec!["s2", "s3", "s4"]),
        (field("n").gt(Value::Int(5)), vec!["s2", "s3", "s4"]),
        (field("n").lt(Value::Int(5)), vec!["s1"]),
        (field("n").ge(Value::Int(3)), vec!["s1", "s2", "s3", "s4"]),
        (
            field("n").between(Value::Int(5), Value::Float(10.0)),
            vec!["s2", "s3", "s4"],
        ),
        (
            field("n").ne(Value::Int(7)),
            vec!["s1", "s5", "s6", "s8"], // s7 (missing) drops out
        ),
        (
            field("n").is_in([Value::Int(3), text("7")]),
            vec!["s1", "s5"],
        ),
        (field("n").eq(Value::Bool(true)), vec!["s6"]),
        (field("t").starts_with("bl"), vec!["s2", "s4", "s6", "s7"]),
        (
            field("t").starts_with(""),
            vec!["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"],
        ),
        (
            field("n").exists(),
            vec!["s1", "s2", "s3", "s4", "s5", "s6", "s8"],
        ),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

// ===========================================================================
// Indexed vs scan equivalence — compound prefix windows
// ===========================================================================

#[test]
fn filters_indexed_vs_scan_compound_prefix() {
    // Edge corpus: c5 misses cat, c6 misses n — the compound index does not
    // index either, yet c6 still MATCHES a cat-only equality query.
    let docs: Vec<(&[u8], Value)> = vec![
        (b"c1", map(&[("cat", text("blog")), ("n", Value::Int(5))])),
        (b"c2", map(&[("cat", text("blog")), ("n", Value::Int(9))])),
        (b"c3", map(&[("cat", text("news")), ("n", Value::Int(5))])),
        (b"c4", map(&[("cat", text("news")), ("n", Value::Int(9))])),
        (b"c5", map(&[("n", Value::Int(5))])),
        (b"c6", map(&[("cat", text("blog"))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "compound", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "compound", &docs);
    idx.create_compound_index(&["cat", "n"]).unwrap();

    // PIN: compound windows key off TOP-LEVEL comparisons — several
    // `.filter()` calls drive the index, while a single And-composed tree
    // with the same semantics does not (it scans; results stay identical).
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .filter(field("n").ge(Value::Int(6)))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(
        idx.query()
            .filter(
                field("cat")
                    .eq(text("blog"))
                    .and(field("n").ge(Value::Int(6)))
            )
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        }
    );
    assert_eq!(
        scan.query()
            .filter(field("cat").eq(text("blog")))
            .filter(field("n").ge(Value::Int(6)))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        }
    );

    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        // Full-prefix equality (every field pinned): docs missing either
        // field cannot match anyway, so the index window is a superset.
        (
            field("cat")
                .eq(text("blog"))
                .and(field("n").eq(Value::Int(9))),
            vec!["c2"],
        ),
        // Prefix + tail range: docs missing the tail field fail the tail
        // comparison, so they are no matches and the window stays sound.
        (
            field("cat")
                .eq(text("blog"))
                .and(field("n").ge(Value::Int(6))),
            vec!["c2"],
        ),
        (
            field("cat")
                .eq(text("news"))
                .and(field("n").between(Value::Int(5), Value::Int(8))),
            vec!["c3"],
        ),
        // PREFIX-ONLY equality (no constraint on n): documents missing n
        // (c6) DO match the filters but are absent from the index (only
        // fully-present docs are indexed) — the window would NOT be a
        // superset, so the query must NOT use it. (The corpus has missing
        // fields, so the all_docs_indexed flag completed false — the
        // sound decline this test pins.)
        (field("cat").eq(text("blog")), vec!["c1", "c2", "c6"]),
        (field("cat").eq(text("news")), vec!["c3", "c4"]),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

/// The `all_docs_indexed` re-enable: on a corpus where EVERY document has
/// both compound fields, the def completes with the flag true and a
/// PREFIX-ONLY equality (tail unconstrained) is SERVED through
/// `IndexedWindow { kind: "compound" }` — with results identical to the
/// scan twin (every matching doc has the leading field, hence is indexed,
/// so the window is a verified superset).
#[test]
fn filters_compound_prefix_served_when_all_docs_indexed() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"c1", map(&[("cat", text("blog")), ("n", Value::Int(5))])),
        (b"c2", map(&[("cat", text("blog")), ("n", Value::Int(9))])),
        (b"c3", map(&[("cat", text("news")), ("n", Value::Int(5))])),
        (b"c4", map(&[("cat", text("news")), ("n", Value::Int(9))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "compound", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "compound", &docs);
    idx.create_compound_index(&["cat", "n"]).unwrap();

    // Prefix-only equality takes the compound window (was Scan before the
    // flag — compare filters_indexed_vs_scan_compound_prefix, whose corpus
    // has missing fields and still declines).
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert_eq!(
        scan.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        }
    );

    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        (field("cat").eq(text("blog")), vec!["c1", "c2"]),
        (field("cat").eq(text("news")), vec!["c3", "c4"]),
        (
            field("cat")
                .eq(text("blog"))
                .and(field("n").ge(Value::Int(6))),
            vec!["c2"],
        ),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

/// The flag is LIVE: after the all-present corpus earns the flag, one
/// insert whose doc leaves the tail field missing permanently clears it —
/// the prefix-only window declines again, and the scan twin still returns
/// the full result set including the new doc (the missing-tail doc matches
/// a prefix-only filter but sits outside the index). Re-creating the index
/// recomputes the flag: false while the offending doc remains, true again
/// once it is gone (a fresh cycle re-walks the whole corpus).
#[test]
fn filters_compound_prefix_flag_flips_false_on_missing_field_insert() {
    let idx_db = Db::open_in_memory().unwrap();
    let idx = idx_db.collection("compound");
    for (k, cat, n) in [
        (b"c1", "blog", Value::Int(5)),
        (b"c2", "blog", Value::Int(9)),
        (b"c3", "news", Value::Int(5)),
        (b"c4", "news", Value::Int(9)),
    ] {
        idx.insert(k, &map(&[("cat", text(cat)), ("n", n)]))
            .unwrap();
    }
    idx.create_compound_index(&["cat", "n"]).unwrap();
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" },
        "all-present corpus earns the flag"
    );

    // The offending insert: matches cat=blog, missing the tail field n.
    idx.insert(b"c5", &map(&[("cat", text("blog"))])).unwrap();
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        },
        "a missing-tail insert permanently clears the flag"
    );
    // Results stay identical to a plain scan twin over the same corpus.
    let scan_db = Db::open_in_memory().unwrap();
    let scan = scan_db.collection("compound");
    for (k, cat, n) in [
        (b"c1", "blog", Value::Int(5)),
        (b"c2", "blog", Value::Int(9)),
        (b"c3", "news", Value::Int(5)),
        (b"c4", "news", Value::Int(9)),
    ] {
        scan.insert(k, &map(&[("cat", text(cat)), ("n", n)]))
            .unwrap();
    }
    scan.insert(b"c5", &map(&[("cat", text("blog"))])).unwrap();
    let p = field("cat").eq(text("blog"));
    assert_eq!(matching_keys(&scan, &p), ks(&["c1", "c2", "c5"]));
    assert_eq!(matching_keys(&idx, &p), ks(&["c1", "c2", "c5"]));

    // Re-registration while the missing-field doc remains: recomputed
    // false (the backfill counts c5 as a miss).
    idx.create_compound_index(&["cat", "n"]).unwrap();
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        },
        "re-registration over a corpus with a missing-field doc recomputes false"
    );
    // Deleting it and re-registering re-earns the flag (fresh cycle).
    idx.delete(b"c5").unwrap();
    idx.create_compound_index(&["cat", "n"]).unwrap();
    assert_eq!(
        idx.query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" },
        "re-registration over an all-present corpus recomputes true"
    );
}

/// The persisted flag round-trips a reopen in BOTH directions: true stays
/// true (the def row's kind byte decodes), and a maintenance-cleared flag
/// stays false.
#[test]
fn filters_compound_prefix_flag_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("compound");
        c.insert(b"c1", &map(&[("cat", text("blog")), ("n", Value::Int(5))]))
            .unwrap();
        c.insert(b"c2", &map(&[("cat", text("news")), ("n", Value::Int(9))]))
            .unwrap();
        c.create_compound_index(&["cat", "n"]).unwrap();
    }
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.collection("compound")
                .query()
                .filter(field("cat").eq(text("blog")))
                .plan_shape(),
            PlanShape::IndexedWindow { kind: "compound" },
            "the earned flag decodes on reopen"
        );
        // Clear it with a missing-tail insert, then reopen again.
        db.collection("compound")
            .insert(b"c3", &map(&[("cat", text("blog"))]))
            .unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.collection("compound")
            .query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        },
        "the maintenance-cleared flag decodes on reopen"
    );
}

/// Dump→load round-trips the flag WITHOUT serializing it: load replays
/// `create_compound_index`, a fresh cycle that recomputes the flag from the
/// loaded corpus — exact, not merely conservative (true corpus → served,
/// missing-field corpus → declined).
#[test]
fn filters_compound_prefix_flag_roundtrips_dump_load() {
    let dump_of = |with_missing: bool| {
        let src = Db::open_in_memory().unwrap();
        let c = src.collection("compound");
        c.insert(b"c1", &map(&[("cat", text("blog")), ("n", Value::Int(5))]))
            .unwrap();
        c.insert(b"c2", &map(&[("cat", text("news")), ("n", Value::Int(9))]))
            .unwrap();
        if with_missing {
            c.insert(b"c3", &map(&[("cat", text("blog"))])).unwrap();
        }
        c.create_compound_index(&["cat", "n"]).unwrap();
        let mut buf = Vec::new();
        src.dump(&mut buf).unwrap();
        buf
    };
    for (with_missing, want_served) in [(false, true), (true, false)] {
        let dst = Db::open_in_memory().unwrap();
        dst.load(dump_of(with_missing).as_slice()).unwrap();
        let got = dst
            .collection("compound")
            .query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape();
        assert_eq!(
            got,
            if want_served {
                PlanShape::IndexedWindow { kind: "compound" }
            } else {
                PlanShape::Scan {
                    collection: "compound".to_owned(),
                }
            },
            "dump/load with missing-field corpus = {with_missing}"
        );
    }
}

/// A PRESENT-but-non-encodable field value (Array, Null, containers) in the
/// corpus at INDEX-CREATION time is a silent backfill miss: the page cannot
/// encode the tuple, so the doc is not indexed — the def must complete with
/// `all_docs_indexed` = false and prefix-only windows must decline, with
/// results identical to the scan twin (the offending doc still matches a
/// prefix-only filter on the leading field).
#[test]
fn filters_compound_prefix_nonencodable_at_backfill_completes_flag_false() {
    for offending in [
        Value::Array(vec![Value::Int(1)]),
        Value::Null,
        Value::Map(BTreeMap::new()),
    ] {
        let docs: Vec<(&[u8], Value)> = vec![
            (b"c1", map(&[("cat", text("blog")), ("n", Value::Int(5))])),
            (b"c2", map(&[("cat", text("blog")), ("n", Value::Int(9))])),
            (b"c3", map(&[("cat", text("news")), ("n", Value::Int(5))])),
            // Present `n`, but a value the compound encoder declines — the
            // doc is not indexed, so the flag must not complete true.
            (
                b"c4",
                map(&[("cat", text("blog")), ("n", offending.clone())]),
            ),
        ];
        let scan_db = Db::open_in_memory().unwrap();
        let scan = seed(&scan_db, "compound", &docs);
        let idx_db = Db::open_in_memory().unwrap();
        let idx = seed(&idx_db, "compound", &docs);
        idx.create_compound_index(&["cat", "n"]).unwrap();

        assert_eq!(
            idx.query()
                .filter(field("cat").eq(text("blog")))
                .plan_shape(),
            PlanShape::Scan {
                collection: "compound".to_owned()
            },
            "a non-encodable present value at backfill must complete the flag false ({offending:?})"
        );
        let p = field("cat").eq(text("blog"));
        assert_eq!(
            matching_keys(&scan, &p),
            ks(&["c1", "c2", "c4"]),
            "scan side of {offending:?}"
        );
        assert_eq!(
            matching_keys(&idx, &p),
            ks(&["c1", "c2", "c4"]),
            "indexed side of {offending:?}"
        );
    }
}

/// The same hole AFTER completion: an insert whose doc carries a
/// present-but-non-encodable value in an indexed field is unindexed by
/// maintenance — the write must flip the flag false, so subsequent
/// prefix-only queries decline and return the full correct set via the
/// scan fallback (including the offending doc).
#[test]
fn filters_compound_prefix_nonencodable_insert_after_completion_flips_flag() {
    for offending in [
        Value::Array(vec![Value::Int(1)]),
        Value::Null,
        Value::Map(BTreeMap::new()),
    ] {
        let idx_db = Db::open_in_memory().unwrap();
        let idx = idx_db.collection("compound");
        for (k, cat, n) in [
            (b"c1", "blog", Value::Int(5)),
            (b"c2", "blog", Value::Int(9)),
            (b"c3", "news", Value::Int(5)),
        ] {
            idx.insert(k, &map(&[("cat", text(cat)), ("n", n)]))
                .unwrap();
        }
        idx.create_compound_index(&["cat", "n"]).unwrap();
        assert_eq!(
            idx.query()
                .filter(field("cat").eq(text("blog")))
                .plan_shape(),
            PlanShape::IndexedWindow { kind: "compound" },
            "fixture sanity: all-present-encodable corpus earns the flag ({offending:?})"
        );

        // The offending insert: `n` present but non-encodable.
        idx.insert(
            b"c4",
            &map(&[("cat", text("blog")), ("n", offending.clone())]),
        )
        .unwrap();
        assert_eq!(
            idx.query()
                .filter(field("cat").eq(text("blog")))
                .plan_shape(),
            PlanShape::Scan {
                collection: "compound".to_owned()
            },
            "a non-encodable present insert must flip the flag false ({offending:?})"
        );
        let rows = idx
            .query()
            .filter(field("cat").eq(text("blog")))
            .run()
            .unwrap();
        assert_eq!(keys(&rows), ks(&["c1", "c2", "c4"]));
    }
}

/// Legacy def rows (pre-flag, no kind byte) decode as NOT-all-indexed:
/// even over an all-present corpus, a legacy `Complete` def declines
/// prefix-only windows until the index is re-created (backward
/// compatibility: absence of the flag byte reads as false).
#[test]
fn filters_legacy_compound_def_without_flag_byte_declines_prefix_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("compound");
        c.insert(b"c1", &map(&[("cat", text("blog")), ("n", Value::Int(5))]))
            .unwrap();
        c.insert(b"c2", &map(&[("cat", text("news")), ("n", Value::Int(9))]))
            .unwrap();
        c.create_compound_index(&["cat", "n"]).unwrap();
    }
    // Overwrite the def row with the legacy empty form (exactly the
    // pre-flag on-disk shape) through the public byte-store surface, with
    // the Db handle dropped so the file lock is free.
    {
        let store = Store::open(&path).unwrap();
        store
            .put("__cscalar_indexes__", b"compound\x00cat\x00n", b"")
            .unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.collection("compound")
            .query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::Scan {
            collection: "compound".to_owned()
        },
        "legacy def rows decode as not-all-indexed"
    );
    // Re-creating the index (a flag-aware cycle) re-earns the flag.
    db.collection("compound")
        .create_compound_index(&["cat", "n"])
        .unwrap();
    assert_eq!(
        db.collection("compound")
            .query()
            .filter(field("cat").eq(text("blog")))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" },
        "re-creation earns the flag on an all-present corpus"
    );
}

// ===========================================================================
// Indexed vs scan equivalence — OR union windows
// ===========================================================================

#[test]
fn filters_indexed_vs_scan_or_union() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"o1", map(&[("n", Value::Int(3))])),
        (b"o2", map(&[("n", Value::Int(7))])),
        (b"o3", map(&[("n", Value::Int(9))])),
        (b"o4", map(&[("n", Value::Int(12))])),
        (b"o5", map(&[("t", text("sevens"))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "orcoll", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "orcoll", &docs);
    idx.create_scalar_index("n").unwrap();

    // A top-level OR whose every disjunct is index-serviceable drives the
    // union window.
    let serviceable = field("n")
        .eq(Value::Int(7))
        .or(field("n").ge(Value::Int(10)));
    assert_eq!(
        idx.query().filter(serviceable.clone()).plan_shape(),
        PlanShape::IndexedWindow { kind: "or" }
    );
    // One unserviceable disjunct (Contains) declines the whole probe → scan.
    let unserviceable = field("n")
        .eq(Value::Int(3))
        .or(field("t").contains("seven"));
    assert_eq!(
        idx.query().filter(unserviceable.clone()).plan_shape(),
        PlanShape::Scan {
            collection: "orcoll".to_owned()
        }
    );

    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        (serviceable.clone(), vec!["o2", "o4"]),
        // Nested ORs flatten; a doc matching two disjuncts appears ONCE.
        (
            field("n")
                .eq(Value::Int(3))
                .or(field("n").eq(Value::Int(7)))
                .or(field("n").between(Value::Int(2), Value::Int(4))),
            vec!["o1", "o2"],
        ),
        // The unserviceable shape still returns scan-correct results.
        (unserviceable.clone(), vec!["o1", "o5"]),
        // An OR over a field some docs lack: the missing side is just false.
        (
            field("n").eq(Value::Int(12)).or(field("t").exists()),
            vec!["o4", "o5"],
        ),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
}

// ===========================================================================
// Indexed vs scan equivalence — geo window
// ===========================================================================

#[test]
fn filters_indexed_vs_scan_geo_window() {
    let docs = geo_corpus();
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "geocoll", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "geocoll", &docs);
    idx.create_geo_index("loc").unwrap();

    let cases: Vec<(Predicate, Vec<&str>)> = vec![
        (
            field("loc").within_km(51.5, -0.13, 400.0),
            vec!["lon-arr", "lon-map", "paris"],
        ),
        (
            field("loc").within_km(51.5, -0.13, 300.0),
            vec!["lon-arr", "lon-map"],
        ),
        (field("loc").within_km(0.0, 0.0, 1.0), vec!["origin-int"]),
        // Zero radius at an indexed point still finds it (distance 0 <= 0).
        (
            field("loc").within_km(51.5, -0.13, 0.0),
            vec!["lon-arr", "lon-map"],
        ),
    ];
    for (p, want) in &cases {
        let want_keys = ks(want);
        assert_eq!(matching_keys(&scan, p), want_keys, "scan side of {p:?}");
        assert_eq!(matching_keys(&idx, p), want_keys, "indexed side of {p:?}");
    }
    // Boundary at exactly the radius is inclusive WITH the index too.
    let d = haversine_km(0.0, 0.0, 1.0, 0.0);
    let want = ks(&["east", "origin-int"]);
    assert_eq!(
        matching_keys(&scan, &field("loc").within_km(0.0, 0.0, d)),
        want
    );
    assert_eq!(
        matching_keys(&idx, &field("loc").within_km(0.0, 0.0, d)),
        want
    );

    // Plan shapes.
    assert_eq!(
        idx.query()
            .filter(field("loc").within_km(51.5, -0.13, 400.0))
            .plan_shape(),
        PlanShape::IndexedWindow { kind: "geo" }
    );
    assert_eq!(
        scan.query()
            .filter(field("loc").within_km(51.5, -0.13, 400.0))
            .plan_shape(),
        PlanShape::Scan {
            collection: "geocoll".to_owned()
        }
    );
}

// ===========================================================================
// Filter + pagination (filter-then-paginate order)
// ===========================================================================

#[test]
fn filters_filter_then_limit_offset_pagination() {
    let db = Db::open_in_memory().unwrap();
    // Insert out of key order: pure-filter rows come back in KEY order.
    let c = seed(
        &db,
        "page",
        &[
            (b"d5", map(&[("n", Value::Int(5))])),
            (b"d2", map(&[("n", Value::Int(2))])),
            (b"d1", map(&[("n", Value::Int(1))])),
            (b"d4", map(&[("n", Value::Int(4))])),
            (b"d3", map(&[("n", Value::Int(3))])),
        ],
    );
    let p = field("n").ge(Value::Int(2)); // matches d2..d5 in key order
    let run = |q: corvid::QueryBuilder| {
        q.run()
            .unwrap()
            .into_iter()
            .map(|r| (r.key, r.score, r.document))
            .collect::<Vec<_>>()
    };

    // No limit/offset: all matches, key-sorted, full documents, zero scores.
    let all = run(c.query().filter(p.clone()));
    let keys: Vec<&[u8]> = all.iter().map(|(k, _, _)| k.as_slice()).collect();
    assert_eq!(keys, vec![&b"d2"[..], &b"d3"[..], &b"d4"[..], &b"d5"[..]]);
    assert!(all.iter().all(|(_, s, _)| *s == 0.0));
    assert_eq!(all[0].2.get("n"), Some(&Value::Int(2)));

    // limit: the FIRST n matching rows in key order (filter-then-paginate).
    let lim = run(c.query().filter(p.clone()).limit(2));
    assert_eq!(lim.len(), 2);
    assert_eq!(lim[0].0, b"d2".to_vec());
    assert_eq!(lim[1].0, b"d3".to_vec());
    // PIN: limit(0) yields the empty window, not "no limit".
    assert!(run(c.query().filter(p.clone()).limit(0)).is_empty());
    // limit larger than the match count.
    assert_eq!(run(c.query().filter(p.clone()).limit(99)).len(), 4);

    // offset skips matches first; offset + limit window into them.
    let off = run(c.query().filter(p.clone()).offset(1));
    let keys: Vec<&[u8]> = off.iter().map(|(k, _, _)| k.as_slice()).collect();
    assert_eq!(keys, vec![&b"d3"[..], &b"d4"[..], &b"d5"[..]]);
    let win = run(c.query().filter(p.clone()).offset(1).limit(2));
    let keys: Vec<&[u8]> = win.iter().map(|(k, _, _)| k.as_slice()).collect();
    assert_eq!(keys, vec![&b"d3"[..], &b"d4"[..]]);
    // offset(0) is a no-op; offset past the end is empty.
    assert_eq!(run(c.query().filter(p.clone()).offset(0)).len(), 4);
    assert!(run(c.query().filter(p.clone()).offset(10)).is_empty());
    assert_eq!(run(c.query().filter(p.clone()).offset(3)).len(), 1);

    // Filter on an EMPTY collection: empty result, no error.
    let ghost = db.collection("ghosts");
    assert!(ghost.query().filter(p).run().unwrap().is_empty());
}

// ===========================================================================
// Empty collections and never-matching predicates
// ===========================================================================

#[test]
fn filters_empty_collection_and_never_matching_predicates() {
    let db = Db::open_in_memory().unwrap();
    let ghost = db.collection("ghosts");
    assert!(
        ghost
            .query()
            .filter(field("x").eq(Value::Int(1)))
            .run()
            .unwrap()
            .is_empty()
    );
    assert!(
        ghost
            .query()
            .filter(field("x").exists())
            .run()
            .unwrap()
            .is_empty()
    );
    assert!(
        ghost
            .query()
            .filter(field("x").eq(Value::Int(1)))
            .limit(5)
            .offset(2)
            .run()
            .unwrap()
            .is_empty()
    );
    assert_eq!(ghost.delete_where(field("x").exists()).unwrap(), 0);

    // Never-matching predicates over a populated corpus: empty, not error.
    let c = seed(
        &db,
        "never",
        &[
            (b"v1", map(&[("t", text("hello")), ("n", Value::Int(5))])),
            (b"v2", map(&[("t", text("world")), ("n", Value::Int(9))])),
        ],
    );
    assert_eq!(matching_keys(&c, &field("t").contains("zzz-nope")), ks(&[]));
    assert_eq!(
        matching_keys(&c, &field("n").between(Value::Int(9), Value::Int(4))),
        ks(&[])
    );
    assert_eq!(
        matching_keys(&c, &field("n").is_in(Vec::<Value>::new())),
        ks(&[])
    );
    assert_eq!(matching_keys(&c, &field("nope").ne(Value::Int(1))), ks(&[]));
}

// ===========================================================================
// delete_where driven by varied predicate forms
// ===========================================================================

#[test]
fn filters_delete_where_drives_predicate_forms() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "dw",
        &[
            (b"e1", map(&[("cat", text("a")), ("n", Value::Int(1))])),
            (b"e2", map(&[("cat", text("b")), ("n", Value::Int(2))])),
            (b"e3", map(&[("cat", text("c")), ("n", Value::Int(3))])),
            (b"e4", map(&[("cat", text("a"))])),
            (b"e5", map(&[])),
        ],
    );

    // In: removes exactly the membership matches.
    assert_eq!(
        c.delete_where(field("cat").is_in([text("a"), text("b")]))
            .unwrap(),
        3
    );
    assert_eq!(matching_keys(&c, &field("cat").exists()), ks(&["e3"]));

    // Not(Exists): removes exactly the docs missing the field.
    assert_eq!(c.delete_where(!field("n").exists()).unwrap(), 1);
    assert_eq!(matching_keys(&c, &field("n").exists()), ks(&["e3"]));

    // A never-matching Between deletes nothing.
    assert_eq!(
        c.delete_where(field("n").between(Value::Int(9), Value::Int(1)))
            .unwrap(),
        0
    );
    assert_eq!(c.len().unwrap(), 1);
}

// ===========================================================================
// Value accessors on query results (get / get_path / as_*)
// ===========================================================================

#[test]
fn filters_value_accessors_read_stored_kinds() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "accessors", &[(b"k1", kinds_doc())]);
    let rows = c.query().filter(field("i").exists()).run().unwrap();
    assert_eq!(rows.len(), 1);
    let doc = &rows[0].document;

    // get reads map fields; accessors borrow the inner value on kind match.
    assert_eq!(doc.get("i"), Some(&Value::Int(7)));
    assert_eq!(doc.get("i").and_then(Value::as_int), Some(7));
    assert_eq!(doc.get("i").and_then(Value::as_float), None); // wrong kind
    assert_eq!(doc.get("f").and_then(Value::as_float), Some(2.5));
    assert_eq!(doc.get("f").and_then(Value::as_int), None);
    assert_eq!(doc.get("b").and_then(Value::as_bool), Some(true));
    assert_eq!(doc.get("t").and_then(Value::as_text), Some("héllo 🐦"));
    assert_eq!(
        doc.get("by").and_then(Value::as_bytes),
        Some(&[1u8, 2, 255][..])
    );
    assert_eq!(
        doc.get("v").and_then(Value::as_vector),
        Some(&[1.0f32, 2.0][..])
    );
    assert_eq!(doc.get("z"), Some(&Value::Null));
    assert_eq!(doc.get("nope"), None);
    // get only descends maps: a scalar has no fields.
    assert_eq!(doc.get("i").and_then(|v| v.get("x")), None);

    // get_path resolves the same dotted paths filters use.
    assert_eq!(doc.get_path("meta.author.name"), Some(&text("ada")));
    assert_eq!(
        doc.get_path("meta.author"),
        Some(&map(&[("name", text("ada"))]))
    );
    assert_eq!(doc.get_path("i"), Some(&Value::Int(7)));
    assert_eq!(doc.get_path("nope"), None);
    assert_eq!(doc.get_path("meta.nope"), None);
    assert_eq!(doc.get_path(""), None); // empty path is not a field
    assert_eq!(doc.get_path("i.deeper"), None); // through a scalar
}
