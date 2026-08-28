//! Index-tool conformance: create_text_index / create_scalar_index /
//! create_geo_index / create_compound_index through the wire — creation
//! acknowledged, queries exact with and without the index, mutation after
//! creation stays correct, and every param/name error surfaces.

use serde_json::json;

use crate::wire::{self, Wire};

/// create_scalar_index: ok acknowledged; filtered counts are exact after
/// creation AND after further mutations (the index is maintained).
#[test]
fn create_scalar_index_exact_under_mutation() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"n": 1}));
    w.store("docs", "b", json!({"n": 2}));
    assert_eq!(
        w.ok(
            "create_scalar_index",
            json!({"collection": "docs", "field": "n"})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok(
            "count",
            json!({"collection": "docs", "filter": {"op": "eq", "field": "n", "value": 2}}),
        ),
        json!({"count": 1})
    );
    // Mutate after creation: the index follows.
    w.store("docs", "c", json!({"n": 2}));
    w.store("docs", "a", json!({"n": 2}));
    assert_eq!(
        w.ok(
            "count",
            json!({"collection": "docs", "filter": {"op": "eq", "field": "n", "value": 2}}),
        ),
        json!({"count": 3})
    );
    assert_eq!(
        w.ok("delete", json!({"collection": "docs", "key": "c"})),
        json!({"deleted": true})
    );
    assert_eq!(
        w.ok(
            "count",
            json!({"collection": "docs", "filter": {"op": "eq", "field": "n", "value": 2}}),
        ),
        json!({"count": 2})
    );
}

/// create_compound_index: ok acknowledged; a prefix-equality query returns
/// the exact rows; malformed `fields` params are BadParams.
#[test]
fn create_compound_index_and_fields_errors() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "a",
        json!({"tenant": "t1", "status": "open", "id": 1}),
    );
    w.store(
        "docs",
        "b",
        json!({"tenant": "t1", "status": "closed", "id": 2}),
    );
    w.store(
        "docs",
        "c",
        json!({"tenant": "t2", "status": "open", "id": 3}),
    );
    assert_eq!(
        w.ok(
            "create_compound_index",
            json!({"collection": "docs", "fields": ["tenant", "status"]}),
        ),
        json!({"ok": true})
    );
    let out = w.ok(
        "search",
        json!({"collection": "docs", "filter": {"op": "eq", "field": "tenant", "value": "t1"}}),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let ks: Vec<&str> = results.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(ks, ["a", "b"]);

    assert_eq!(
        w.err(
            "create_compound_index",
            json!({"collection": "docs", "fields": "n"})
        ),
        "bad params: 'fields' must be an array"
    );
    assert_eq!(
        w.err(
            "create_compound_index",
            json!({"collection": "docs", "fields": []})
        ),
        "bad params: 'fields' must be non-empty"
    );
    assert_eq!(
        w.err(
            "create_compound_index",
            json!({"collection": "docs", "fields": ["a", 5]})
        ),
        "bad params: 'fields' must be strings"
    );
    assert_eq!(
        w.err("create_compound_index", json!({"fields": ["a"]})),
        "bad params: missing string 'collection'"
    );
}

/// create_text_index (in-memory and on-disk): text search stays exact and
/// identically ordered either way.
#[test]
fn create_text_index_memory_and_ondisk() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"body": "rust embedded database"}));
    w.store("docs", "b", json!({"body": "python web"}));
    assert_eq!(
        w.ok(
            "create_text_index",
            json!({"collection": "docs", "field": "body"})
        ),
        json!({"ok": true})
    );
    let out = w.ok(
        "search",
        json!({"collection": "docs", "text": {"field": "body", "query": "rust", "k": 5}}),
    );
    assert_eq!(out["results"][0]["key"], "a");

    // The on-disk variant on another collection behaves identically.
    w.store("docs2", "a", json!({"body": "rust embedded database"}));
    w.store("docs2", "b", json!({"body": "python web"}));
    assert_eq!(
        w.ok(
            "create_text_index",
            json!({"collection": "docs2", "field": "body", "on_disk": true}),
        ),
        json!({"ok": true})
    );
    let out = w.ok(
        "search",
        json!({"collection": "docs2", "text": {"field": "body", "query": "rust", "k": 5}}),
    );
    assert_eq!(out["results"][0]["key"], "a");
}

/// create_geo_index: ok acknowledged; radius queries stay exact with the
/// index present.
#[test]
fn create_geo_index_then_radius_exact() {
    let mut w = Wire::new();
    w.store("docs", "london", json!({"loc": [51.5074, -0.1278]}));
    w.store("docs", "paris", json!({"loc": [48.8566, 2.3522]}));
    assert_eq!(
        w.ok(
            "create_geo_index",
            json!({"collection": "docs", "field": "loc"})
        ),
        json!({"ok": true})
    );
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13, "radius_km": 50.0}),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["key"], "london");
}

/// The index tools share one error surface: a missing field is BadParams; a
/// field name the engine forbids (interior `__`) surfaces the engine's
/// InvalidName error through the wire.
#[test]
fn index_tools_param_and_name_errors() {
    let mut w = Wire::new();
    assert_eq!(
        w.err("create_scalar_index", json!({"collection": "docs"})),
        "bad params: missing string 'field'"
    );
    assert_eq!(
        w.err("create_geo_index", json!({"field": "loc"})),
        "bad params: missing string 'collection'"
    );
    wire::starts_with(
        &w.err(
            "create_text_index",
            json!({"collection": "docs", "field": "bad__name"}),
        ),
        "invalid name (NUL byte or `__` is not allowed): bad__name",
    );
    wire::starts_with(
        &w.err(
            "create_compound_index",
            json!({"collection": "docs", "fields": ["ok", "bad__name"]}),
        ),
        "invalid name (NUL byte or `__` is not allowed): bad__name",
    );
}
