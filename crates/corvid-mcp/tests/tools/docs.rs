//! Document-tool conformance: store / get / delete / patch /
//! compare_and_set / delete_where / insert_auto / count / list_collections —
//! happy paths asserted against state read back through the wire, plus every
//! param-shape and engine-error surface.

use serde_json::{Value as Json, json};

use crate::wire::{self, Wire};

/// store then get round-trips a document; a second store overwrites it.
#[test]
fn store_then_get_roundtrips_and_overwrites() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"title": "hello", "n": 3}));
    assert_eq!(w.get("docs", "k"), json!({"title": "hello", "n": 3}));
    w.store("docs", "k", json!({"title": "second"}));
    assert_eq!(w.get("docs", "k"), json!({"title": "second"}));
}

/// `document` may be any JSON kind — text, number, bool, null, array — the
/// engine stores scalars and containers, and get returns the same shape.
#[test]
fn store_accepts_every_json_document_kind() {
    let mut w = Wire::new();
    for (key, doc) in [
        ("text", json!("a string")),
        ("int", json!(42)),
        ("float", json!(1.5)),
        ("bool", json!(true)),
        ("null", json!(null)),
        ("array", json!([1, "two", false])),
    ] {
        w.store("docs", key, doc.clone());
        assert_eq!(w.get("docs", key), doc, "kind {key} must round-trip");
    }
    // A null document is distinguishable from an absent key by tool success:
    // both serialize `document: null` (pinned convert limitation).
    assert_eq!(w.get("docs", "null"), Json::Null);
    assert_eq!(w.get("docs", "absent"), Json::Null);
}

/// Missing or wrong-typed params surface as BadParams with the exact
/// message; a missing `document` is named explicitly.
#[test]
fn store_and_get_param_errors() {
    let mut w = Wire::new();
    wire::starts_with(
        &w.err("store", json!({"collection": "docs", "key": "k"})),
        "bad params: missing 'document'",
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": 7, "key": "k", "document": {}}),
        ),
        "bad params: missing string 'collection'",
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "docs", "key": ["k"], "document": {}}),
        ),
        "bad params: missing string 'key'",
    );
    wire::starts_with(
        &w.err("get", json!({"collection": "docs"})),
        "bad params: missing string 'key'",
    );
    wire::starts_with(
        &w.err("get", json!({"key": "k"})),
        "bad params: missing string 'collection'",
    );
}

/// Engine errors through the wire: reserved collection names (`__` prefix)
/// and names with interior `__` / NUL are refused by the engine, surfaced as
/// isError results carrying the engine's message.
#[test]
fn store_engine_name_errors_surface() {
    let mut w = Wire::new();
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "__internal", "key": "k", "document": {}}),
        ),
        "reserved collection name: __internal",
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "a__b", "key": "k", "document": {}}),
        ),
        "invalid name (NUL byte or `__` is not allowed): a__b",
    );
}

/// get on a missing key AND on a never-created collection both return
/// `document: null` — an unknown collection is not an error (pinned).
#[test]
fn get_missing_key_and_unknown_collection_are_null() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"n": 1}));
    assert_eq!(w.get("docs", "nope"), Json::Null);
    assert_eq!(w.get("never-created", "k"), Json::Null);
}

/// delete reports true then false; missing params are BadParams.
#[test]
fn delete_reports_outcome_and_param_errors() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"n": 1}));
    assert_eq!(
        w.ok("delete", json!({"collection": "docs", "key": "k"})),
        json!({"deleted": true})
    );
    assert_eq!(
        w.ok("delete", json!({"collection": "docs", "key": "k"})),
        json!({"deleted": false})
    );
    wire::starts_with(
        &w.err("delete", json!({"collection": "docs"})),
        "bad params: missing string 'key'",
    );
}

/// patch merges top-level fields (creating the doc when absent); a nested
/// map in the patch REPLACES the field (top-level merge only — pinned engine
/// semantics).
#[test]
fn patch_merges_top_level_and_creates_missing() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"a": 1, "nested": {"x": 1}}));
    assert_eq!(
        w.ok(
            "patch",
            json!({"collection": "docs", "key": "k", "patch": {"b": 2, "nested": {"y": 2}}})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.get("docs", "k"),
        json!({"a": 1, "b": 2, "nested": {"y": 2}}),
        "top-level fields merge; nested maps replace"
    );
    // Patching an absent key creates the document.
    assert_eq!(
        w.ok(
            "patch",
            json!({"collection": "docs", "key": "fresh", "patch": {"n": 9}})
        ),
        json!({"ok": true})
    );
    assert_eq!(w.get("docs", "fresh"), json!({"n": 9}));
    wire::starts_with(
        &w.err("patch", json!({"collection": "docs", "key": "k"})),
        "bad params: missing 'patch'",
    );
}

/// compare_and_set: expected ABSENT inserts; a wrong expected value refuses
/// without touching state; JSON null `expected` behaves as absent (pinned).
#[test]
fn compare_and_set_absent_expected_and_mismatch() {
    let mut w = Wire::new();
    // Omitted expected = must-be-absent.
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "new": {"v": 1}})
        ),
        json!({"applied": true})
    );
    // Wrong expected: not applied, document unchanged.
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "expected": {"v": 999}, "new": {"v": 2}})
        ),
        json!({"applied": false})
    );
    assert_eq!(w.get("docs", "k"), json!({"v": 1}));
    // Explicit null expected behaves exactly like omission: must-be-absent.
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "expected": null, "new": {"v": 2}})
        ),
        json!({"applied": false})
    );
    // Matching expected applies.
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "expected": {"v": 1}, "new": {"v": 2}})
        ),
        json!({"applied": true})
    );
    assert_eq!(w.get("docs", "k"), json!({"v": 2}));
    wire::starts_with(
        &w.err("compare_and_set", json!({"key": "k", "new": {}})),
        "bad params: missing string 'collection'",
    );
}

/// compare_and_set with `new` omitted (or null) DELETES when expected
/// matches; a mismatch refuses and the document survives.
#[test]
fn compare_and_set_new_omitted_deletes() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"v": 1}));
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "expected": {"v": 1}})
        ),
        json!({"applied": true})
    );
    assert_eq!(w.get("docs", "k"), Json::Null);
    // Mismatch + no new: nothing deleted.
    w.store("docs", "k", json!({"v": 2}));
    assert_eq!(
        w.ok(
            "compare_and_set",
            json!({"collection": "docs", "key": "k", "expected": {"v": 1}})
        ),
        json!({"applied": false})
    );
    assert_eq!(w.get("docs", "k"), json!({"v": 2}));
}

/// delete_where removes exactly the matching documents and reports the
/// count; bad filter shapes are BadParams with distinct messages.
#[test]
fn delete_where_counts_and_filter_errors() {
    let mut w = Wire::new();
    for i in 0..5 {
        w.store(
            "docs",
            &format!("k{i}"),
            json!({"n": i, "cat": if i < 2 { "a" } else { "b" }}),
        );
    }
    assert_eq!(
        w.ok(
            "delete_where",
            json!({"collection": "docs",
                   "filter": {"op": "eq", "field": "cat", "value": "a"}})
        ),
        json!({"removed": 2})
    );
    assert_eq!(
        w.ok("count", json!({"collection": "docs"})),
        json!({"count": 3})
    );
    // No matches removes zero.
    assert_eq!(
        w.ok(
            "delete_where",
            json!({"collection": "docs",
                   "filter": {"op": "eq", "field": "cat", "value": "zzz"}})
        ),
        json!({"removed": 0})
    );
    // Param/filter error surfaces.
    wire::starts_with(
        &w.err("delete_where", json!({"collection": "docs"})),
        "bad params: missing 'filter'",
    );
    wire::starts_with(
        &w.err("delete_where", json!({"collection": "docs", "filter": 42})),
        "bad params: filter must be an object",
    );
    wire::starts_with(
        &w.err(
            "delete_where",
            json!({"collection": "docs", "filter": {"op": "wat", "field": "n"}}),
        ),
        "bad params: unknown filter op: wat",
    );
}

/// insert_auto returns distinct ordered keys that round-trip through get;
/// `document` is required.
#[test]
fn insert_auto_keys_ordered_and_distinct() {
    let mut w = Wire::new();
    let a = w.ok(
        "insert_auto",
        json!({"collection": "q", "document": {"n": 1}}),
    );
    let b = w.ok(
        "insert_auto",
        json!({"collection": "q", "document": {"n": 2}}),
    );
    let ka = a["key"].as_str().unwrap();
    let kb = b["key"].as_str().unwrap();
    assert_ne!(ka, kb, "auto keys are distinct");
    assert!(
        ka < kb,
        "auto keys sort in insertion order: {ka:?} < {kb:?}"
    );
    assert_eq!(w.get("q", ka), json!({"n": 1}));
    assert_eq!(w.get("q", kb), json!({"n": 2}));
    wire::starts_with(
        &w.err("insert_auto", json!({"collection": "q"})),
        "bad params: missing 'document'",
    );
}

/// count is exact with and without a filter; an unknown collection counts
/// zero (not an error — pinned).
#[test]
fn count_exact_with_filter_and_unknown_collection() {
    let mut w = Wire::new();
    for (key, n) in [("a", 1), ("b", 2), ("c", 2)] {
        w.store("docs", key, json!({"n": n}));
    }
    assert_eq!(
        w.ok("count", json!({"collection": "docs"})),
        json!({"count": 3})
    );
    assert_eq!(
        w.ok(
            "count",
            json!({"collection": "docs", "filter": {"op": "eq", "field": "n", "value": 2}}),
        ),
        json!({"count": 2})
    );
    assert_eq!(
        w.ok("count", json!({"collection": "no-such"})),
        json!({"count": 0})
    );
    wire::starts_with(
        &w.err("count", json!({})),
        "bad params: missing string 'collection'",
    );
}

/// list_collections returns exactly the user collections touched through the
/// wire (internal namespaces are never listed).
#[test]
fn list_collections_lists_user_names_exactly() {
    let mut w = Wire::new();
    w.store("alpha", "k", json!({"n": 1}));
    w.store("beta", "k", json!({"n": 1}));
    // Tools that touch engine-internal namespaces (graph edges, indexes)
    // must not leak them into the listing.
    w.ok(
        "link",
        json!({"collection": "alpha", "from": "k", "relation": "r", "to": "k2"}),
    );
    w.ok(
        "create_scalar_index",
        json!({"collection": "beta", "field": "n"}),
    );
    let out = w.ok("list_collections", json!({}));
    assert_eq!(out, json!({"collections": ["alpha", "beta"]}));
}

/// join is left-outer: rows carry (key, left, right) with right null for a
/// missing FK field, a dangling reference, and a non-key FK shape.
#[test]
fn join_left_outer_rows_and_missing_references() {
    let mut w = Wire::new();
    w.store("authors", "rocky", json!({"name": "Rocky"}));
    w.store("posts", "p1", json!({"title": "Hi", "author_id": "rocky"}));
    w.store(
        "posts",
        "p2",
        json!({"title": "Gone", "author_id": "ghost"}),
    );
    w.store("posts", "p3", json!({"title": "NoField"}));
    w.store(
        "posts",
        "p4",
        json!({"title": "BadShape", "author_id": {"x": 1}}),
    );
    let out = w.ok(
        "join",
        json!({"collection": "posts", "other": "authors", "foreign_key_field": "author_id"}),
    );
    assert_eq!(
        out,
        json!({"rows": [
            {"key": "p1", "left": {"title": "Hi", "author_id": "rocky"},
             "right": {"name": "Rocky"}},
            {"key": "p2", "left": {"title": "Gone", "author_id": "ghost"}, "right": null},
            {"key": "p3", "left": {"title": "NoField"}, "right": null},
            {"key": "p4", "left": {"title": "BadShape", "author_id": {"x": 1}}, "right": null},
        ]})
    );
    wire::starts_with(
        &w.err("join", json!({"collection": "posts", "other": "authors"})),
        "bad params: missing string 'foreign_key_field'",
    );
}

/// An Int foreign key joins to the TEXT key with the same decimal encoding
/// (Int(7) matches key "7") — the engine's Int≡Text join rule, pinned
/// through the wire.
#[test]
fn join_int_foreign_key_matches_decimal_text_key() {
    let mut w = Wire::new();
    w.store("authors", "7", json!({"name": "Seven"}));
    w.store("authors", "07", json!({"name": "ZeroSeven"}));
    w.store("posts", "p", json!({"author_id": 7}));
    let out = w.ok(
        "join",
        json!({"collection": "posts", "other": "authors", "foreign_key_field": "author_id"}),
    );
    assert_eq!(
        out,
        json!({"rows": [
            {"key": "p", "left": {"author_id": 7}, "right": {"name": "Seven"}},
        ]}),
        "Int 7 joins the text key \"7\", not \"07\""
    );
}
