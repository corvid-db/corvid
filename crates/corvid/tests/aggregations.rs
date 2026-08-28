//! Aggregation conformance (Task 6): `count`/`sum`/`avg`/`min`/`max`/
//! `count_distinct`/`group_count`/`group_sum`/`group_avg` and
//! `Collection::approx_distinct`, through the public API only — the full
//! numeric lattice (NaN, ±inf, missing-all, mixed Int+Float, values beyond
//! 2^53), typed group keys, skip rules, and filter/shaping interactions.
//!
//! Contract notes pinned by these tests (read from `src/builder.rs`,
//! `src/sketch.rs`, and `src/filter.rs` first):
//! * aggregates run over the FILTERED set only: retrieval sources, ranking,
//!   `limit`, `offset`, `select`, and `order_by` are ignored (a filter query
//!   with `.limit(1)` still counts/sums every match);
//! * `sum`/`avg` accumulate in `f64`: `Int` converts through `as f64`, so
//!   integers beyond 2^53 round BEFORE summing (the same precision rule as
//!   the filter/index numeric lane); missing fields and non-numeric values
//!   (text, bool, null, containers) are SKIPPED, never an error — a field
//!   missing from every document sums to exactly `0.0` and averages to
//!   `None`;
//! * NaN is a number for sum/avg — it POISONS the total and the mean (the
//!   member still counts in avg's denominator) — while min/max treat NaN as
//!   incomparable (a NaN-only field yields `None`; NaN never displaces an
//!   existing best); ±inf accumulate by IEEE rules, so `inf + (-inf)` is
//!   NaN;
//! * min/max return the ORIGINAL `Value`: numbers (Int/Float interoperating
//!   numerically) and text (byte-lexicographic) are comparable; bools,
//!   bytes, nulls, and containers are not (a bool-only field yields `None`);
//!   a field mixing comparable KINDS (number vs text) never replaces across
//!   kinds, so whichever kind appears first in key order wins (PINNED, both
//!   orders);
//! * group keys and `count_distinct` use the canonical typed form: text is
//!   bare, int/float/bool type-tagged (`i:1`, `f:1.5`, `b:true`), and a
//!   text that would collide with a tag (an `i:`/`f:`/`b:`/`t:` prefix) is
//!   `t:`-escaped — so `1`, `1.0`, `"1"`, and `true` are four DISTINCT
//!   buckets; `-0.0` and `+0.0` share `f:0`; NaN groups as `f:NaN`; missing
//!   fields and container values are skipped;
//! * `group_sum`/`group_avg` bucket on the group field's typed key but only
//!   fold documents whose VALUE field is numeric — a group whose members
//!   all lack the value field exists in `group_count` yet is ABSENT from
//!   `group_sum`/`group_avg` (PINNED asymmetry);
//! * every aggregate validates the fluent ranking arguments first (via
//!   `for_each_match`/`count`): an out-of-domain `fuse_rrf`/`rerank_mmr`
//!   value fails the aggregate with `Error::InvalidArgument` exactly as it
//!   fails `run`;
//! * `Collection::approx_distinct` is HyperLogLog (default precision p=14,
//!   ~0.8% relative standard error) fed each value's deterministic
//!   encoding: exact for small cardinalities, skips missing fields, and
//!   distinguishes `Int(1)` / `Float(1.0)` / `Text("1")` (different encode
//!   tags).
//!
//! The smoke test that anchored the radar skeleton during Waves 1-2 is kept
//! as the first test below.

use std::collections::BTreeMap;

use corvid::{Collection, Db, Error, Metric, Value, field};

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

/// Seed `name` with `(key, doc)` pairs, returning the collection.
fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> Collection<'a> {
    let c = db.collection(name);
    for (k, d) in docs {
        c.insert(k, d).unwrap();
    }
    c
}
/// A BTreeMap of `key -> count` for full-equality group assertions.
fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

/// A BTreeMap of `key -> f64` for full-equality group assertions.
fn sums(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn aggregations_smoke_sum_group_count_and_count() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    let cat = |s: &str, n: i64| map(&[("cat", text(s)), ("n", Value::Int(n))]);
    c.insert(b"a", &cat("x", 1)).unwrap();
    c.insert(b"b", &cat("x", 2)).unwrap();
    c.insert(b"c", &cat("y", 10)).unwrap();

    assert_eq!(c.query().sum("n").unwrap(), 13.0);

    let groups = c.query().group_count("cat").unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups.get("x"), Some(&2));
    assert_eq!(groups.get("y"), Some(&1));

    assert_eq!(c.query().count().unwrap(), 3);
    assert_eq!(
        c.query()
            .filter(field("cat").eq(Value::Text("x".into())))
            .count()
            .unwrap(),
        2
    );
}

// ===========================================================================
// count — filters, empty, after mutations
// ===========================================================================

#[test]
fn aggregations_count_matrix_filter_empty_and_after_mutations() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "cnt",
        &[
            (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
            (b"b", map(&[("cat", text("y")), ("n", Value::Int(2))])),
            (b"c", map(&[("cat", text("x")), ("n", Value::Int(3))])),
        ],
    );

    // No filter: the O(1) counter.
    assert_eq!(c.query().count().unwrap(), 3);
    // A non-Compare filter form (Between) narrows the same way.
    assert_eq!(
        c.query()
            .filter(field("n").between(Value::Int(2), Value::Int(3)))
            .count()
            .unwrap(),
        2
    );
    // A never-matching filter: zero, not an error.
    assert_eq!(
        c.query()
            .filter(field("cat").eq(text("nope")))
            .count()
            .unwrap(),
        0
    );

    // Mutations are reflected: insert, then delete down to empty.
    c.insert(b"d", &map(&[("cat", text("y")), ("n", Value::Int(4))]))
        .unwrap();
    assert_eq!(c.query().count().unwrap(), 4);
    for k in [&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..]] {
        c.delete(k).unwrap();
    }
    assert_eq!(c.query().count().unwrap(), 0, "empty after deleting all");

    // An unknown collection is an empty aggregate, never an error.
    assert_eq!(db.collection("ghosts").query().count().unwrap(), 0);
}

// ===========================================================================
// sum — the numeric lattice
// ===========================================================================

#[test]
fn aggregations_sum_int_float_mixed_and_negative_exact() {
    let db = Db::open_in_memory().unwrap();
    // Int-only: exact integer arithmetic in the f64 accumulator.
    let ints = seed(
        &db,
        "ints",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Int(3))])),
        ],
    );
    assert_eq!(ints.query().sum("n").unwrap(), 6.0);

    // Float-only: values picked for exact f64 sums (powers of two).
    let floats = seed(
        &db,
        "floats",
        &[
            (b"a", map(&[("x", Value::Float(0.5))])),
            (b"b", map(&[("x", Value::Float(0.25))])),
            (b"c", map(&[("x", Value::Float(0.25))])),
        ],
    );
    assert_eq!(floats.query().sum("x").unwrap(), 1.0);

    // Mixed Int+Float in one field: interop through f64, exact here.
    let mixed = seed(
        &db,
        "mixed",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Float(2.5))])),
        ],
    );
    assert_eq!(mixed.query().sum("n").unwrap(), 5.5);

    // Negative totals, int and float.
    let negs = seed(
        &db,
        "negs",
        &[
            (b"a", map(&[("n", Value::Int(-5))])),
            (b"b", map(&[("n", Value::Int(3))])),
            (b"c", map(&[("n", Value::Int(-7))])),
            (b"d", map(&[("f", Value::Float(-2.5))])),
            (b"e", map(&[("f", Value::Float(0.5))])),
        ],
    );
    assert_eq!(negs.query().sum("n").unwrap(), -9.0);
    assert_eq!(negs.query().sum("f").unwrap(), -2.0);

    // A filter narrows the summed set.
    assert_eq!(
        negs.query()
            .filter(field("n").lt(Value::Int(0)))
            .sum("n")
            .unwrap(),
        -12.0
    );
}

#[test]
fn aggregations_sum_skips_missing_and_non_numeric_missing_all_is_zero() {
    let db = Db::open_in_memory().unwrap();
    // Text/bool/null/container values in the summed field are SKIPPED (no
    // error); only the numeric members fold.
    let c = seed(
        &db,
        "docs",
        &[
            (b"a", map(&[("n", Value::Int(5))])),
            (b"b", map(&[("n", text("not a number"))])),
            (b"c", map(&[("n", Value::Bool(true))])),
            (b"d", map(&[("n", Value::Null)])),
            (
                b"e",
                map(&[("n", Value::Array(vec![Value::Int(1), Value::Int(2)]))]),
            ),
            (b"f", map(&[("other", Value::Int(100))])), // field missing
        ],
    );
    assert_eq!(c.query().sum("n").unwrap(), 5.0);

    // The field missing from EVERY document: exactly 0.0 (not an error).
    assert_eq!(c.query().sum("other2").unwrap(), 0.0);

    // A field present but never numeric anywhere: also 0.0.
    let textual = seed(
        &db,
        "textual",
        &[
            (b"a", map(&[("t", text("1"))])),
            (b"b", map(&[("t", text("2"))])),
        ],
    );
    assert_eq!(textual.query().sum("t").unwrap(), 0.0);

    // Empty collection: 0.0 as well.
    assert_eq!(db.collection("empty").query().sum("n").unwrap(), 0.0);
}

#[test]
fn aggregations_sum_nan_poisons_and_infinities_follow_ieee() {
    let db = Db::open_in_memory().unwrap();
    // PINNED: NaN is a numeric member — it poisons the total (it is NOT
    // skipped the way text is).
    let poisoned = seed(
        &db,
        "poison",
        &[
            (b"a", map(&[("x", Value::Float(1.0))])),
            (b"b", map(&[("x", Value::Float(f64::NAN))])),
        ],
    );
    assert!(poisoned.query().sum("x").unwrap().is_nan());

    // A single NaN alone is still NaN.
    let lone_nan = seed(
        &db,
        "nan1",
        &[(b"a", map(&[("x", Value::Float(f64::NAN))]))],
    );
    assert!(lone_nan.query().sum("x").unwrap().is_nan());

    // +inf absorbs finite addends; inf + (-inf) is NaN by IEEE rules.
    let pos_inf = seed(
        &db,
        "pinf",
        &[
            (b"a", map(&[("x", Value::Float(f64::INFINITY))])),
            (b"b", map(&[("x", Value::Float(1.0))])),
        ],
    );
    assert_eq!(pos_inf.query().sum("x").unwrap(), f64::INFINITY);

    let both_inf = seed(
        &db,
        "binf",
        &[
            (b"a", map(&[("x", Value::Float(f64::INFINITY))])),
            (b"b", map(&[("x", Value::Float(f64::NEG_INFINITY))])),
        ],
    );
    assert!(both_inf.query().sum("x").unwrap().is_nan());
}

#[test]
fn aggregations_sum_large_ints_round_through_f64_beyond_2_pow_53() {
    let db = Db::open_in_memory().unwrap();
    // PINNED: `sum` accumulates in f64, converting each Int through
    // `as f64` FIRST — so integers beyond 2^53 round before they add,
    // exactly like the filter/index numeric lane. 2^53+1 is not
    // representable: it rounds to 2^53.
    let one = seed(
        &db,
        "big1",
        &[(b"a", map(&[("n", Value::Int(9_007_199_254_740_993))]))], // 2^53+1
    );
    assert_eq!(one.query().sum("n").unwrap(), 9_007_199_254_740_992.0);

    // i64::MAX rounds up to 2^63; adding 1 is below one ulp there, so the
    // total stays 2^63.
    let maxes = seed(
        &db,
        "big2",
        &[
            (b"a", map(&[("n", Value::Int(i64::MAX))])),
            (b"b", map(&[("n", Value::Int(1))])),
        ],
    );
    assert_eq!(maxes.query().sum("n").unwrap(), 9_223_372_036_854_775_808.0);

    // Contrast: min/max over the same magnitudes stay EXACT, because
    // same-type Int comparisons use i64 ordering, not f64.
    let pair = seed(
        &db,
        "big3",
        &[
            (b"a", map(&[("n", Value::Int(9_007_199_254_740_993))])), // 2^53+1
            (b"b", map(&[("n", Value::Int(9_007_199_254_740_994))])), // 2^53+2
        ],
    );
    assert_eq!(
        pair.query().min("n").unwrap(),
        Some(Value::Int(9_007_199_254_740_993))
    );
    assert_eq!(
        pair.query().max("n").unwrap(),
        Some(Value::Int(9_007_199_254_740_994))
    );
}

// ===========================================================================
// avg
// ===========================================================================

#[test]
fn aggregations_avg_matrix_single_mixed_skipped_and_empty() {
    let db = Db::open_in_memory().unwrap();

    // Empty collection: None (no error).
    assert_eq!(db.collection("none").query().avg("x").unwrap(), None);

    // Field missing everywhere: None.
    let missing_all = seed(
        &db,
        "miss",
        &[
            (b"a", map(&[("t", text("x"))])),
            (b"b", map(&[("t", text("y"))])),
        ],
    );
    assert_eq!(missing_all.query().avg("n").unwrap(), None);

    // Never-numeric field: None as well.
    assert_eq!(missing_all.query().avg("t").unwrap(), None);

    // Single Int member.
    let single = seed(&db, "one", &[(b"a", map(&[("n", Value::Int(42))]))]);
    assert_eq!(single.query().avg("n").unwrap(), Some(42.0));

    // Single Float member.
    let single_f = seed(&db, "onef", &[(b"a", map(&[("x", Value::Float(2.5))]))]);
    assert_eq!(single_f.query().avg("x").unwrap(), Some(2.5));

    // Mixed Int+Float with an exact mean: (1 + 2 + 2.5 + 0.5) / 4 = 1.5.
    let mixed = seed(
        &db,
        "avg",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Float(2.5))])),
            (b"d", map(&[("n", Value::Float(0.5))])),
        ],
    );
    assert_eq!(mixed.query().avg("n").unwrap(), Some(1.5));

    // Non-numeric members are skipped in both numerator and denominator:
    // mean of {Int 4, text, missing} is 4.0, not 4/3.
    let skipped = seed(
        &db,
        "skip",
        &[
            (b"a", map(&[("n", Value::Int(4))])),
            (b"b", map(&[("n", text("four"))])),
            (b"c", map(&[("other", Value::Int(9))])),
        ],
    );
    assert_eq!(skipped.query().avg("n").unwrap(), Some(4.0));

    // A filter narrows the averaged set: gt(1) keeps Int 2 and Float 2.5,
    // so the mean is (2 + 2.5) / 2 = 2.25.
    assert_eq!(
        mixed
            .query()
            .filter(field("n").gt(Value::Int(1)))
            .avg("n")
            .unwrap(),
        Some(2.25)
    );
}

#[test]
fn aggregations_avg_nan_member_poisons_the_mean() {
    let db = Db::open_in_memory().unwrap();
    // PINNED: a NaN member poisons the mean AND still counts in the
    // denominator (total = NaN, n = 2) — the result is Some(NaN), not None.
    let c = seed(
        &db,
        "navg",
        &[
            (b"a", map(&[("x", Value::Float(1.0))])),
            (b"b", map(&[("x", Value::Float(f64::NAN))])),
        ],
    );
    let avg = c.query().avg("x").unwrap();
    assert!(avg.is_some());
    assert!(avg.unwrap().is_nan());
}

// ===========================================================================
// min / max
// ===========================================================================

#[test]
fn aggregations_min_max_numbers_interop_and_text_is_lexicographic() {
    let db = Db::open_in_memory().unwrap();

    // Ints: exact i64 ordering.
    let ints = seed(
        &db,
        "ints",
        &[
            (b"a", map(&[("n", Value::Int(3))])),
            (b"b", map(&[("n", Value::Int(1))])),
            (b"c", map(&[("n", Value::Int(2))])),
        ],
    );
    assert_eq!(ints.query().min("n").unwrap(), Some(Value::Int(1)));
    assert_eq!(ints.query().max("n").unwrap(), Some(Value::Int(3)));

    // Int/Float interop numerically; the ORIGINAL Value kind is returned.
    let mixed = seed(
        &db,
        "mix",
        &[
            (b"a", map(&[("n", Value::Int(3))])),
            (b"b", map(&[("n", Value::Float(2.5))])),
            (b"c", map(&[("n", Value::Int(10))])),
        ],
    );
    assert_eq!(mixed.query().min("n").unwrap(), Some(Value::Float(2.5)));
    assert_eq!(mixed.query().max("n").unwrap(), Some(Value::Int(10)));

    // Text: UTF-8 byte-lexicographic ("Apple" < "banana" < "cherry";
    // uppercase sorts before lowercase).
    let texts = seed(
        &db,
        "texts",
        &[
            (b"a", map(&[("t", text("banana"))])),
            (b"b", map(&[("t", text("cherry"))])),
            (b"c", map(&[("t", text("Apple"))])),
            (b"d", map(&[("t", text("banana"))])), // duplicate value
        ],
    );
    assert_eq!(texts.query().min("t").unwrap(), Some(text("Apple")));
    assert_eq!(texts.query().max("t").unwrap(), Some(text("cherry")));

    // A single document: min == max == its value.
    let single = seed(&db, "one", &[(b"a", map(&[("n", Value::Int(42))]))]);
    assert_eq!(single.query().min("n").unwrap(), Some(Value::Int(42)));
    assert_eq!(single.query().max("n").unwrap(), Some(Value::Int(42)));

    // ±inf are comparable extrema (only NaN among floats is not).
    let infs = seed(
        &db,
        "inf",
        &[
            (b"a", map(&[("x", Value::Float(1.0))])),
            (b"b", map(&[("x", Value::Float(f64::INFINITY))])),
            (b"c", map(&[("x", Value::Float(f64::NEG_INFINITY))])),
        ],
    );
    assert_eq!(
        infs.query().min("x").unwrap(),
        Some(Value::Float(f64::NEG_INFINITY))
    );
    assert_eq!(
        infs.query().max("x").unwrap(),
        Some(Value::Float(f64::INFINITY))
    );

    // A filter narrows the extremum set like every other aggregate:
    // gt(1) over {3, 1, 2} keeps {3, 2}.
    assert_eq!(
        ints.query()
            .filter(field("n").gt(Value::Int(1)))
            .min("n")
            .unwrap(),
        Some(Value::Int(2))
    );
    assert_eq!(
        ints.query()
            .filter(field("n").gt(Value::Int(1)))
            .max("n")
            .unwrap(),
        Some(Value::Int(3))
    );
}

#[test]
fn aggregations_min_max_incomparable_kinds_yield_none() {
    let db = Db::open_in_memory().unwrap();

    // PINNED: bools are NOT comparable — a bool-only field has no min/max,
    // even though true/false are orderable in many languages.
    let bools = seed(
        &db,
        "bools",
        &[
            (b"a", map(&[("b", Value::Bool(true))])),
            (b"b", map(&[("b", Value::Bool(false))])),
        ],
    );
    assert_eq!(bools.query().min("b").unwrap(), None);
    assert_eq!(bools.query().max("b").unwrap(), None);

    // Bytes, nulls, and containers are equally incomparable.
    let kinds = seed(
        &db,
        "kinds",
        &[
            (b"a", map(&[("x", Value::Bytes(vec![1, 2]))])),
            (b"b", map(&[("x", Value::Null)])),
            (b"c", map(&[("x", Value::Vector(vec![1.0, 2.0]))])),
        ],
    );
    assert_eq!(kinds.query().min("x").unwrap(), None);
    assert_eq!(kinds.query().max("x").unwrap(), None);

    // PINNED: NaN is incomparable even with itself — a NaN-only field has
    // no extremum...
    let nans = seed(
        &db,
        "nans",
        &[(b"a", map(&[("x", Value::Float(f64::NAN))]))],
    );
    assert_eq!(nans.query().min("x").unwrap(), None);
    assert_eq!(nans.query().max("x").unwrap(), None);

    // ...and a NaN member never displaces an existing best.
    let with_nan = seed(
        &db,
        "wnan",
        &[
            (b"a", map(&[("x", Value::Float(1.0))])),
            (b"b", map(&[("x", Value::Float(f64::NAN))])),
            (b"c", map(&[("x", Value::Float(3.0))])),
        ],
    );
    assert_eq!(with_nan.query().min("x").unwrap(), Some(Value::Float(1.0)));
    assert_eq!(with_nan.query().max("x").unwrap(), Some(Value::Float(3.0)));

    // Field missing everywhere, empty collection: None.
    let docs = seed(&db, "docs", &[(b"a", map(&[("n", Value::Int(1))]))]);
    assert_eq!(docs.query().min("absent").unwrap(), None);
    assert_eq!(docs.query().max("absent").unwrap(), None);
    assert_eq!(db.collection("none").query().min("x").unwrap(), None);
    assert_eq!(db.collection("none").query().max("x").unwrap(), None);
}

#[test]
fn aggregations_min_max_mixed_kinds_pin_first_comparable_kind_wins() {
    let db = Db::open_in_memory().unwrap();
    // PINNED: numbers and text are each comparable, but ACROSS kinds no
    // replacement ever happens — so the extremum is whichever comparable
    // KIND the key-ordered scan met first. Text first ("a" holds the text):
    let text_first = seed(
        &db,
        "tf",
        &[
            (b"a", map(&[("x", text("zz"))])),
            (b"b", map(&[("x", Value::Int(5))])),
        ],
    );
    assert_eq!(text_first.query().min("x").unwrap(), Some(text("zz")));
    assert_eq!(text_first.query().max("x").unwrap(), Some(text("zz")));

    // Number first ("a" holds the number):
    let int_first = seed(
        &db,
        "nf",
        &[
            (b"a", map(&[("x", Value::Int(5))])),
            (b"b", map(&[("x", text("zz"))])),
        ],
    );
    assert_eq!(int_first.query().min("x").unwrap(), Some(Value::Int(5)));
    assert_eq!(int_first.query().max("x").unwrap(), Some(Value::Int(5)));
}

// ===========================================================================
// count_distinct
// ===========================================================================

#[test]
fn aggregations_count_distinct_scalars_duplicates_missing_and_empty() {
    let db = Db::open_in_memory().unwrap();

    // Exact distinct count for scalars.
    let three = seed(
        &db,
        "three",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("v", Value::Int(2))])),
            (b"c", map(&[("v", Value::Int(3))])),
        ],
    );
    assert_eq!(three.query().count_distinct("v").unwrap(), 3);

    // Duplicates collapse.
    let dups = seed(
        &db,
        "dups",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("v", Value::Int(1))])),
            (b"c", map(&[("v", Value::Int(2))])),
            (b"d", map(&[("v", Value::Int(2))])),
            (b"e", map(&[("v", Value::Int(2))])),
        ],
    );
    assert_eq!(dups.query().count_distinct("v").unwrap(), 2);

    // Documents missing the field are excluded (not a null bucket); so are
    // container and null values (no canonical group key).
    let sparse = seed(
        &db,
        "sparse",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("other", Value::Int(9))])), // missing v
            (b"c", map(&[("v", Value::Array(vec![]))])),
            (b"d", map(&[("v", Value::Map(BTreeMap::new()))])),
            (b"e", map(&[("v", Value::Null)])),
        ],
    );
    assert_eq!(sparse.query().count_distinct("v").unwrap(), 1);

    // Empty collection and missing-everywhere: 0.
    assert_eq!(
        db.collection("none").query().count_distinct("v").unwrap(),
        0
    );
    assert_eq!(sparse.query().count_distinct("other2").unwrap(), 0);

    // A filter narrows the counted set.
    let tagged = seed(
        &db,
        "tagged",
        &[
            (
                b"a",
                map(&[("v", Value::Int(1)), ("keep", Value::Bool(true))]),
            ),
            (
                b"b",
                map(&[("v", Value::Int(1)), ("keep", Value::Bool(true))]),
            ),
            (
                b"c",
                map(&[("v", Value::Int(2)), ("keep", Value::Bool(false))]),
            ),
        ],
    );
    // Values are 1, 1, 2 across the three docs: two distinct.
    assert_eq!(tagged.query().count_distinct("v").unwrap(), 2);
    // A filter narrows the counted set: keep=true holds both Int(1) docs,
    // so exactly one distinct value remains.
    assert_eq!(
        tagged
            .query()
            .filter(field("keep").eq(Value::Bool(true)))
            .count_distinct("v")
            .unwrap(),
        1
    );
}

#[test]
fn aggregations_count_distinct_and_groups_separate_types_by_tag() {
    let db = Db::open_in_memory().unwrap();
    // The typed-key rule: 1, 1.0, "1", and true are FOUR distinct values
    // (tags i:/f:/b: plus bare text) — and a text that would collide with a
    // tag is t:-escaped, so Text("i:1") is a fifth.
    let c = seed(
        &db,
        "typed",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("v", Value::Float(1.0))])),
            (b"c", map(&[("v", text("1"))])),
            (b"d", map(&[("v", Value::Bool(true))])),
            (b"e", map(&[("v", text("i:1"))])),
            (b"f", map(&[("v", Value::Int(1))])), // duplicate of a
        ],
    );
    assert_eq!(c.query().count_distinct("v").unwrap(), 5);

    // group_count buckets them with the exact canonical keys.
    assert_eq!(
        c.query().group_count("v").unwrap(),
        counts(&[
            ("i:1", 2), // Int 1 twice
            ("f:1", 1), // Float 1.0 renders as "1"
            ("1", 1),   // bare text "1"
            ("b:true", 1),
            ("t:i:1", 1), // escaped colliding text
        ])
    );

    // Bools split into their two buckets.
    let bools = seed(
        &db,
        "bools",
        &[
            (b"a", map(&[("v", Value::Bool(true))])),
            (b"b", map(&[("v", Value::Bool(true))])),
            (b"c", map(&[("v", Value::Bool(false))])),
        ],
    );
    assert_eq!(bools.query().count_distinct("v").unwrap(), 2);
    assert_eq!(
        bools.query().group_count("v").unwrap(),
        counts(&[("b:true", 2), ("b:false", 1)])
    );
}

// ===========================================================================
// group_count — typed buckets, escapes, skips, filters
// ===========================================================================

#[test]
fn aggregations_group_count_escapes_every_ambiguous_tag_prefix() {
    let db = Db::open_in_memory().unwrap();
    // Each of the four tag prefixes escapes; none collides with the genuine
    // tagged keys computed alongside.
    let c = seed(
        &db,
        "esc",
        &[
            (b"a", map(&[("g", text("i:1"))])),
            (b"b", map(&[("g", text("f:1.5"))])),
            (b"c", map(&[("g", text("b:true"))])),
            (b"d", map(&[("g", text("t:x"))])),
            (b"e", map(&[("g", Value::Int(1))])), // genuine i:1
            (b"f", map(&[("g", Value::Float(1.5))])), // genuine f:1.5
            (b"g", map(&[("g", Value::Bool(true))])), // genuine b:true
            (b"h", map(&[("g", text("plain"))])), // bare text stays bare
        ],
    );
    assert_eq!(
        c.query().group_count("g").unwrap(),
        counts(&[
            ("t:i:1", 1),
            ("t:f:1.5", 1),
            ("t:b:true", 1),
            ("t:t:x", 1),
            ("i:1", 1),
            ("f:1.5", 1),
            ("b:true", 1),
            ("plain", 1),
        ])
    );
}

#[test]
fn aggregations_group_count_zero_signed_floats_share_and_nan_groups() {
    let db = Db::open_in_memory().unwrap();
    // PINNED: -0.0 and +0.0 are numerically equal, so they share the f:0
    // bucket (the key normalizes the signed zero); every NaN payload lands
    // in one f:NaN bucket.
    let c = seed(
        &db,
        "zeros",
        &[
            (b"a", map(&[("x", Value::Float(-0.0))])),
            (b"b", map(&[("x", Value::Float(0.0))])),
            (b"c", map(&[("x", Value::Float(f64::NAN))])),
            (b"d", map(&[("x", Value::Float(f64::NAN))])),
            (b"e", map(&[("x", Value::Float(1.0))])),
        ],
    );
    assert_eq!(
        c.query().group_count("x").unwrap(),
        counts(&[("f:0", 2), ("f:NaN", 2), ("f:1", 1)])
    );
}

#[test]
fn aggregations_group_count_skips_missing_and_container_fields() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "skip",
        &[
            (b"a", map(&[("g", text("x"))])),
            (b"b", map(&[("g", text("x"))])),
            (b"c", map(&[("other", Value::Int(1))])), // g missing
            (b"d", map(&[("g", Value::Array(vec![Value::Int(1)]))])), // container
            (b"e", map(&[("g", Value::Map(BTreeMap::new()))])), // container
            (b"f", map(&[("g", Value::Vector(vec![1.0]))])), // container
            (b"g", map(&[("g", Value::Bytes(vec![1]))])), // container
            (b"h", map(&[("g", Value::Null)])),       // null
        ],
    );
    assert_eq!(
        c.query().group_count("g").unwrap(),
        counts(&[("x", 2)]),
        "only the two text-x docs bucket; missing/containers/null skip"
    );

    // No matching documents at all: an EMPTY map (not an error).
    assert!(
        c.query()
            .filter(field("other").eq(Value::Int(99)))
            .group_count("g")
            .unwrap()
            .is_empty()
    );
    // So for a whole empty collection.
    assert!(
        db.collection("none")
            .query()
            .group_count("g")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn aggregations_group_count_respects_filters() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "f",
        &[
            (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
            (b"b", map(&[("cat", text("x")), ("n", Value::Int(5))])),
            (b"c", map(&[("cat", text("y")), ("n", Value::Int(2))])),
        ],
    );
    // Unfiltered: two buckets.
    assert_eq!(
        c.query().group_count("cat").unwrap(),
        counts(&[("x", 2), ("y", 1)])
    );
    // Filter keeps only the n>=2 docs: x loses one, y keeps its one.
    assert_eq!(
        c.query()
            .filter(field("n").ge(Value::Int(2)))
            .group_count("cat")
            .unwrap(),
        counts(&[("x", 1), ("y", 1)])
    );
    // Never-matching filter: empty map.
    assert!(
        c.query()
            .filter(field("n").gt(Value::Int(100)))
            .group_count("cat")
            .unwrap()
            .is_empty()
    );
}

// ===========================================================================
// group_sum / group_avg
// ===========================================================================

#[test]
fn aggregations_group_sum_avg_exact_per_bucket_and_absent_empty_buckets() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "gs",
        &[
            (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
            (b"b", map(&[("cat", text("x")), ("n", Value::Float(2.5))])),
            (b"c", map(&[("cat", text("y")), ("n", Value::Int(10))])),
            (b"d", map(&[("cat", text("y")), ("n", Value::Int(-4))])),
            // A doc whose value is non-numeric contributes nothing to its
            // bucket's sum (but still counts in group_count below).
            (b"e", map(&[("cat", text("y")), ("n", text("n/a"))])),
        ],
    );
    // Exact sums per bucket (mixed Int+Float folds through f64).
    assert_eq!(
        c.query().group_sum("cat", "n").unwrap(),
        sums(&[("x", 3.5), ("y", 6.0)])
    );
    // Means per bucket: x = 3.5/2, y = (10 + -4)/2 — the text member is
    // excluded from the denominator too.
    assert_eq!(
        c.query().group_avg("cat", "n").unwrap(),
        sums(&[("x", 1.75), ("y", 3.0)])
    );
    // The group_count asymmetry: e counts in the y BUCKET COUNT...
    assert_eq!(
        c.query().group_count("cat").unwrap(),
        counts(&[("x", 2), ("y", 3)])
    );
    // ...but group_sum's y bucket folds only the numeric members. A bucket
    // whose members ALL lack the numeric value exists in group_count and is
    // ABSENT from group_sum/group_avg:
    let holes = seed(
        &db,
        "holes",
        &[
            (b"a", map(&[("cat", text("full")), ("n", Value::Int(7))])),
            (b"b", map(&[("cat", text("hole"))])), // n missing
        ],
    );
    assert_eq!(
        holes.query().group_count("cat").unwrap(),
        counts(&[("full", 1), ("hole", 1)])
    );
    assert_eq!(
        holes.query().group_sum("cat", "n").unwrap(),
        sums(&[("full", 7.0)])
    );
    assert_eq!(
        holes.query().group_avg("cat", "n").unwrap(),
        sums(&[("full", 7.0)])
    );

    // A never-numeric value field: group_sum is empty, group_avg too.
    let textual = seed(
        &db,
        "txt",
        &[
            (b"a", map(&[("cat", text("x")), ("n", text("1"))])),
            (b"b", map(&[("cat", text("y")), ("n", text("2"))])),
        ],
    );
    assert!(textual.query().group_sum("cat", "n").unwrap().is_empty());
    assert!(textual.query().group_avg("cat", "n").unwrap().is_empty());

    // Typed group keys apply here exactly as in group_count: Int 1 and
    // Text "1" are different buckets.
    let typed = seed(
        &db,
        "typed",
        &[
            (b"a", map(&[("g", Value::Int(1)), ("n", Value::Int(10))])),
            (
                b"b",
                map(&[("g", Value::Float(1.0)), ("n", Value::Int(20))]),
            ),
            (b"c", map(&[("g", text("1")), ("n", Value::Int(30))])),
        ],
    );
    assert_eq!(
        typed.query().group_sum("g", "n").unwrap(),
        sums(&[("i:1", 10.0), ("f:1", 20.0), ("1", 30.0)])
    );
}

#[test]
fn aggregations_group_sum_avg_respect_filters() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "gf",
        &[
            (
                b"a",
                map(&[
                    ("cat", text("x")),
                    ("n", Value::Int(1)),
                    ("keep", Value::Bool(true)),
                ]),
            ),
            (
                b"b",
                map(&[
                    ("cat", text("x")),
                    ("n", Value::Int(5)),
                    ("keep", Value::Bool(false)),
                ]),
            ),
            (
                b"c",
                map(&[
                    ("cat", text("y")),
                    ("n", Value::Int(2)),
                    ("keep", Value::Bool(true)),
                ]),
            ),
        ],
    );
    assert_eq!(
        c.query()
            .filter(field("keep").eq(Value::Bool(true)))
            .group_sum("cat", "n")
            .unwrap(),
        sums(&[("x", 1.0), ("y", 2.0)])
    );
    assert_eq!(
        c.query()
            .filter(field("keep").eq(Value::Bool(true)))
            .group_avg("cat", "n")
            .unwrap(),
        sums(&[("x", 1.0), ("y", 2.0)])
    );
    // The filtered-out b means x's average comes from a alone.
    assert_eq!(
        c.query()
            .filter(field("keep").eq(Value::Bool(true)))
            .group_count("cat")
            .unwrap(),
        counts(&[("x", 1), ("y", 1)])
    );
}

// ===========================================================================
// Collection::approx_distinct (HyperLogLog)
// ===========================================================================

#[test]
fn aggregations_approx_distinct_exact_small_counts_and_duplicates() {
    let db = Db::open_in_memory().unwrap();
    // 100 documents cycling 5 values: the estimate is exactly 5 (linear
    // counting is exact at small cardinalities).
    let c = db.collection("docs");
    for i in 0..100u32 {
        let v = i % 5;
        c.insert(
            format!("k{i}").as_bytes(),
            &map(&[("v", Value::Int(v as i64))]),
        )
        .unwrap();
    }
    assert_eq!(c.approx_distinct("v").unwrap(), 5);

    // Duplicate TEXT values collapse too.
    let t = db.collection("texts");
    for i in 0..40u32 {
        let v = format!("tag-{}", i % 3);
        t.insert(format!("k{i}").as_bytes(), &map(&[("v", text(&v))]))
            .unwrap();
    }
    assert_eq!(t.approx_distinct("v").unwrap(), 3);
}

#[test]
fn aggregations_approx_distinct_distinguishes_encoded_kinds_and_skips_missing() {
    let db = Db::open_in_memory().unwrap();
    // The sketch hashes each value's deterministic ENCODING, whose leading
    // tag differs per kind: Int(1), Float(1.0), and Text("1") are three
    // distinct observations even though they share a numeric lane elsewhere.
    let c = seed(
        &db,
        "kinds",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("v", Value::Float(1.0))])),
            (b"c", map(&[("v", text("1"))])),
        ],
    );
    assert_eq!(c.approx_distinct("v").unwrap(), 3);

    // Documents lacking the field are ignored entirely.
    let sparse = seed(
        &db,
        "sparse",
        &[
            (b"a", map(&[("v", Value::Int(1))])),
            (b"b", map(&[("other", Value::Int(1))])),
        ],
    );
    assert_eq!(sparse.approx_distinct("v").unwrap(), 1);

    // Missing everywhere: 0. Empty collection: 0.
    assert_eq!(sparse.approx_distinct("absent").unwrap(), 0);
    assert_eq!(db.collection("none").approx_distinct("v").unwrap(), 0);
}

#[test]
fn aggregations_approx_distinct_bounded_error_on_larger_corpus() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    const N: u64 = 3000;
    for i in 0..N {
        c.insert(
            format!("k{i}").as_bytes(),
            &map(&[("v", Value::Int(i as i64))]),
        )
        .unwrap();
    }
    let est = c.approx_distinct("v").unwrap();
    // The default HyperLogLog precision (p = 14) carries a documented
    // ~0.81% relative standard error (1.04/sqrt(2^14)). The bound below is
    // a generous 5% — more than six standard deviations — because the
    // std-lib hasher is fixed-keyed, making the estimate deterministic for
    // this corpus within a build; anything beyond it is a real regression.
    let rel = (est as f64 - N as f64).abs() / N as f64;
    assert!(
        rel < 0.05,
        "approx_distinct({N}) = {est}, relative error {rel}"
    );
}

// ===========================================================================
// Shaping / sources / validation / index-vs-scan
// ===========================================================================

#[test]
fn aggregations_ignore_limit_offset_select_order_by_and_sources() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "shape",
        &[
            (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
            (b"b", map(&[("cat", text("x")), ("n", Value::Int(2))])),
            (b"c", map(&[("cat", text("y")), ("n", Value::Int(3))])),
        ],
    );
    // Contrast proof: as a RUN, a vector source over a field no document
    // has yields nothing, and limit(1).offset(1) yields one row.
    let run_rows = c
        .query()
        .vector("emb", vec![1.0, 0.0], 10, Metric::Cosine)
        .run()
        .unwrap();
    assert!(run_rows.is_empty(), "no doc has an embedding");
    let paged = c.query().limit(1).offset(1).run().unwrap();
    assert_eq!(paged.len(), 1);

    // The aggregates see the FULL filtered set regardless: shaping and
    // retrieval sources are ignored (they are aggregates over the filtered
    // set, not over the shaped result).
    let shaped = || {
        c.query()
            .vector("emb", vec![1.0, 0.0], 10, Metric::Cosine)
            .text("body", "x", 10)
            .select(["cat"])
            .order_by("n", true)
            .limit(1)
            .offset(1)
    };
    assert_eq!(shaped().count().unwrap(), 3);
    assert_eq!(shaped().sum("n").unwrap(), 6.0);
    assert_eq!(shaped().avg("n").unwrap(), Some(2.0));
    assert_eq!(shaped().min("n").unwrap(), Some(Value::Int(1)));
    assert_eq!(shaped().max("n").unwrap(), Some(Value::Int(3)));
    assert_eq!(shaped().count_distinct("n").unwrap(), 3);
    assert_eq!(
        shaped().group_count("cat").unwrap(),
        counts(&[("x", 2), ("y", 1)])
    );
    assert_eq!(
        shaped().group_sum("cat", "n").unwrap(),
        sums(&[("x", 3.0), ("y", 3.0)])
    );
    assert_eq!(
        shaped().group_avg("cat", "n").unwrap(),
        sums(&[("x", 1.5), ("y", 3.0)])
    );
}

#[test]
fn aggregations_validate_ranking_args_before_aggregating() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "v",
        &[(b"a", map(&[("cat", text("x")), ("n", Value::Int(1))]))],
    );

    // Every aggregate entry point validates the fluent ranking arguments
    // exactly like run(): non-positive / NaN fuse_rrf k and out-of-range /
    // NaN rerank_mmr lambda fail with Error::InvalidArgument.
    let bad_rrf = || c.query().fuse_rrf(0.0);
    let err = bad_rrf().count().unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument(ref m) if m.contains("fuse_rrf")),
        "got {err:?}"
    );
    let err = bad_rrf().sum("n").unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument(ref m) if m.contains("fuse_rrf")),
        "got {err:?}"
    );
    let err = c.query().fuse_rrf(f32::NAN).group_count("cat").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");

    let bad_mmr = || c.query().rerank_mmr(1.5);
    let err = bad_mmr().avg("n").unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument(ref m) if m.contains("rerank_mmr")),
        "got {err:?}"
    );
    let err = c.query().rerank_mmr(-0.5).min("n").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");
    let err = c.query().rerank_mmr(-0.5).max("n").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");
    let err = c
        .query()
        .rerank_mmr(f32::NAN)
        .count_distinct("n")
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");

    // In-domain values pass through every aggregate (rerank with no vector
    // source is a no-op for the aggregate).
    assert_eq!(
        c.query().fuse_rrf(60.0).rerank_mmr(0.7).sum("n").unwrap(),
        1.0
    );
    assert_eq!(
        c.query()
            .fuse_rrf(60.0)
            .rerank_mmr(0.0)
            .group_count("cat")
            .unwrap(),
        counts(&[("x", 1)])
    );
}

#[test]
fn aggregations_indexed_vs_scan_equivalent_for_every_aggregate() {
    // Twin-DB harness (the queries.rs convention): the same corpus and the
    // same filtered aggregates, once over a scan and once with a scalar
    // index on the filtered field — the aggregate must be identical either
    // way (the index window only changes how candidates are found).
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
        (b"b", map(&[("cat", text("x")), ("n", Value::Int(2))])),
        (b"c", map(&[("cat", text("y")), ("n", Value::Float(2.5))])),
        (b"d", map(&[("cat", text("y")), ("n", Value::Float(4.5))])),
        (b"e", map(&[("cat", text("x"))])), // n missing
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "aggr", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "aggr", &docs);
    idx.create_scalar_index("n").unwrap();

    let window = field("n").ge(Value::Int(2)); // matches b, c, d (e misses n)
    let sq = || scan.query().filter(window.clone());
    let iq = || idx.query().filter(window.clone());

    assert_eq!(sq().count().unwrap(), iq().count().unwrap(), "count");
    assert_eq!(sq().sum("n").unwrap(), iq().sum("n").unwrap(), "sum");
    assert_eq!(sq().avg("n").unwrap(), iq().avg("n").unwrap(), "avg");
    assert_eq!(sq().min("n").unwrap(), iq().min("n").unwrap(), "min");
    assert_eq!(sq().max("n").unwrap(), iq().max("n").unwrap(), "max");
    assert_eq!(
        sq().count_distinct("n").unwrap(),
        iq().count_distinct("n").unwrap(),
        "count_distinct"
    );
    assert_eq!(
        sq().group_count("cat").unwrap(),
        iq().group_count("cat").unwrap(),
        "group_count"
    );
    assert_eq!(
        sq().group_sum("cat", "n").unwrap(),
        iq().group_sum("cat", "n").unwrap(),
        "group_sum"
    );
    assert_eq!(
        sq().group_avg("cat", "n").unwrap(),
        iq().group_avg("cat", "n").unwrap(),
        "group_avg"
    );

    // And the concrete expected values (so equality is not vacuous):
    // members are n = 2, 2.5, 4.5 over cats x, y, y — sum 9.0, mean an
    // exact literal 3.0.
    assert_eq!(iq().count().unwrap(), 3);
    assert_eq!(iq().sum("n").unwrap(), 9.0);
    assert_eq!(iq().avg("n").unwrap(), Some(3.0));
    assert_eq!(iq().min("n").unwrap(), Some(Value::Int(2)));
    assert_eq!(iq().max("n").unwrap(), Some(Value::Float(4.5)));
    assert_eq!(iq().count_distinct("n").unwrap(), 3);
    assert_eq!(
        iq().group_count("cat").unwrap(),
        counts(&[("x", 1), ("y", 2)])
    );
    assert_eq!(
        iq().group_sum("cat", "n").unwrap(),
        sums(&[("x", 2.0), ("y", 7.0)])
    );
}
