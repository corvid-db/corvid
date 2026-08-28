//! Index-tool conformance: create_text_index / create_scalar_index /
//! create_geo_index / create_compound_index through the wire — creation
//! acknowledged, queries exact with and without the index, mutation after
//! creation stays correct, and every param/name error surfaces.

use serde_json::{Value as Json, json};

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

/// The exact field-list the set/get schema tests declare.
fn user_fields() -> Json {
    json!([
        {"name": "name", "type": "text", "required": true},
        {"type": "int", "name": "age"},
        {"name": "email", "type": "text", "unique": true},
    ])
}

/// set_schema then get_schema round-trips the declared fields (name, type,
/// required, unique — defaulted flags included); a collection without a
/// schema reports fields: null.
#[test]
fn set_schema_then_get_schema_roundtrips() {
    let mut w = Wire::new();
    assert_eq!(
        w.ok("get_schema", json!({"collection": "users"})),
        json!({"fields": null}),
        "no schema declared yet"
    );
    assert_eq!(
        w.ok(
            "set_schema",
            json!({"collection": "users", "fields": user_fields()}),
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok("get_schema", json!({"collection": "users"})),
        json!({"fields": [
            {"name": "name", "type": "text", "required": true, "unique": false},
            {"name": "age", "type": "int", "required": false, "unique": false},
            {"name": "email", "type": "text", "required": false, "unique": true},
        ]}),
        "flags are explicit on the way out, defaults included"
    );
    // Replacing a schema is allowed; the new one applies to later writes.
    assert_eq!(
        w.ok(
            "set_schema",
            json!({"collection": "users", "fields": [{"name": "n", "type": "any"}]}),
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok("get_schema", json!({"collection": "users"})),
        json!({"fields": [{"name": "n", "type": "any", "required": false, "unique": false}]})
    );
}

/// A unique constraint set through the wire is enforced on every later
/// store: a duplicate value fails with the engine's exact message, a fresh
/// value succeeds, and deleting the holder frees the value.
#[test]
fn set_schema_unique_enforced_on_stores() {
    let mut w = Wire::new();
    assert_eq!(
        w.ok(
            "set_schema",
            json!({"collection": "users", "fields": [
                {"name": "email", "type": "text", "unique": true},
            ]}),
        ),
        json!({"ok": true})
    );
    w.store("users", "a", json!({"email": "a@x"}));
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "users", "key": "b", "document": {"email": "a@x"}}),
        ),
        "schema violation: field 'email' must be unique; value already exists",
    );
    // The failed store left nothing behind.
    assert_eq!(w.get("users", "b"), Json::Null);
    assert_eq!(
        w.ok("count", json!({"collection": "users"})),
        json!({"count": 1})
    );
    // A different value is fine; deleting the holder frees the value.
    w.store("users", "b", json!({"email": "b@x"}));
    assert_eq!(
        w.ok("delete", json!({"collection": "users", "key": "a"})),
        json!({"deleted": true})
    );
    w.store("users", "c", json!({"email": "a@x"}));
    assert_eq!(w.get("users", "c"), json!({"email": "a@x"}));
}

/// Required-presence and type constraints surface the engine's
/// SchemaViolation messages; a null value counts as missing for `required`.
#[test]
fn set_schema_required_and_type_violations() {
    let mut w = Wire::new();
    w.ok(
        "set_schema",
        json!({"collection": "users", "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": "age", "type": "int"},
        ]}),
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "users", "key": "k", "document": {"age": 5}}),
        ),
        "schema violation: field 'name' is required",
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "users", "key": "k", "document": {"name": null, "age": 5}}),
        ),
        "schema violation: field 'name' is required",
    );
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "users", "key": "k", "document": {"name": "x", "age": "old"}}),
        ),
        "schema violation: field 'age' has the wrong type",
    );
    w.store("users", "k", json!({"name": "x", "age": 5}));
    assert_eq!(w.get("users", "k"), json!({"name": "x", "age": 5}));
}

/// set_schema param errors: fields must be an array of objects with string
/// names and known types; forbidden field names surface the engine's
/// InvalidName error.
#[test]
fn set_schema_param_and_name_errors() {
    let mut w = Wire::new();
    assert_eq!(
        w.err("set_schema", json!({"collection": "users"})),
        "bad params: 'fields' must be an array"
    );
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": ["email"]})
        ),
        "bad params: 'fields' entries must be objects"
    );
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": [{"type": "text"}]}),
        ),
        "bad params: 'fields' entries need a string 'name'"
    );
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": [{"name": "e", "type": "varchar"}]}),
        ),
        "bad params: 'type' must be one of: any, bool, int, float, text, bytes, vector, array, map"
    );
    assert_eq!(
        w.err("set_schema", json!({"fields": []})),
        "bad params: missing string 'collection'"
    );
    wire::starts_with(
        &w.err(
            "set_schema",
            json!({"collection": "users", "fields": [{"name": "bad__name", "type": "any"}]}),
        ),
        "invalid name (NUL byte or `__` is not allowed): bad__name",
    );
    assert_eq!(
        w.err("get_schema", json!({})),
        "bad params: missing string 'collection'"
    );
}

/// Schemas survive dump→load through the wire: the loaded server reports
/// the declared fields and still enforces the unique constraint.
#[test]
fn dump_load_preserves_schema_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let ps = dir.path().join("d.bin");
    let ps = ps.to_str().unwrap();
    let mut w = Wire::new();
    w.ok(
        "set_schema",
        json!({"collection": "users", "fields": [{"name": "email", "type": "text", "unique": true}]}),
    );
    w.store("users", "a", json!({"email": "a@x"}));
    w.ok("dump", json!({"path": ps}));

    let mut fresh = Wire::new();
    fresh.ok("load", json!({"path": ps}));
    assert_eq!(
        fresh.ok("get_schema", json!({"collection": "users"})),
        json!({"fields": [{"name": "email", "type": "text", "required": false, "unique": true}]})
    );
    wire::starts_with(
        &fresh.err(
            "store",
            json!({"collection": "users", "key": "b", "document": {"email": "a@x"}}),
        ),
        "schema violation: field 'email' must be unique; value already exists",
    );
}

/// Boolean flags never coerce: a `required`/`unique` that is not a JSON
/// boolean is BadParams naming the key (Task 14 fix round — previously a
/// string flag silently meant `false`).
#[test]
fn set_schema_flag_type_errors() {
    let mut w = Wire::new();
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": [
                {"name": "name", "type": "text", "required": "yes"}
            ]}),
        ),
        "bad params: 'required' must be a boolean (true or false)"
    );
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": [
                {"name": "email", "type": "text", "unique": 1}
            ]}),
        ),
        "bad params: 'unique' must be a boolean (true or false)"
    );
    assert_eq!(
        w.err(
            "set_schema",
            json!({"collection": "users", "fields": [
                {"name": "ok", "type": "any", "required": null}
            ]}),
        ),
        "bad params: 'required' must be a boolean (true or false)"
    );
    // An explicit false is accepted and round-trips as false.
    assert_eq!(
        w.ok(
            "set_schema",
            json!({"collection": "users", "fields": [
                {"name": "n", "type": "any", "required": false, "unique": false}
            ]}),
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok("get_schema", json!({"collection": "users"})),
        json!({"fields": [{"name": "n", "type": "any", "required": false, "unique": false}]})
    );
}

/// The declared-empty vs undeclared schema distinction, pinned through the
/// wire: `fields: []` declares an (empty) schema read back as `[]`; no
/// schema at all reads back as `null`; `fields: null` is BadParams, not a
/// third silent state.
#[test]
fn set_schema_declared_empty_vs_undeclared_fields() {
    let mut w = Wire::new();
    assert_eq!(
        w.ok("get_schema", json!({"collection": "docs"})),
        json!({"fields": null}),
        "undeclared: null"
    );
    assert_eq!(
        w.ok("set_schema", json!({"collection": "docs", "fields": []})),
        json!({"ok": true}),
        "an explicitly empty fields array is a valid declaration"
    );
    assert_eq!(
        w.ok("get_schema", json!({"collection": "docs"})),
        json!({"fields": []}),
        "declared-empty: []"
    );
    // The empty schema is a schema (present, vacuous), not a removal: any
    // document still stores.
    w.store("docs", "a", json!({"anything": 1}));
    assert_eq!(w.get("docs", "a"), json!({"anything": 1}));
    // JSON null is not an array.
    assert_eq!(
        w.err("set_schema", json!({"collection": "docs2", "fields": null}),),
        "bad params: 'fields' must be an array"
    );
    // A distinct collection stays undeclared throughout.
    assert_eq!(
        w.ok("get_schema", json!({"collection": "docs3"})),
        json!({"fields": null})
    );
}

/// The `on_disk` flag gets the same no-silent-coercion treatment: a
/// non-boolean value is BadParams on both tools that take it; explicit
/// false keeps its in-memory meaning.
#[test]
fn index_tools_on_disk_flag_type_errors() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"v": [1.0, 0.0]}));
    assert_eq!(
        w.err(
            "create_index",
            json!({"collection": "docs", "field": "v", "on_disk": "true"}),
        ),
        "bad params: 'on_disk' must be a boolean (true or false)"
    );
    assert_eq!(
        w.err(
            "create_text_index",
            json!({"collection": "docs", "field": "v", "on_disk": 0}),
        ),
        "bad params: 'on_disk' must be a boolean (true or false)"
    );
    assert_eq!(
        w.ok(
            "create_index",
            json!({"collection": "docs", "field": "v", "on_disk": false}),
        ),
        json!({"ok": true})
    );
}
