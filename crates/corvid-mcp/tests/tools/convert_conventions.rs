//! The `$vector` / `$bytes` convert conventions through the WIRE: storing a
//! wrapped object yields a typed engine value (searchable as a vector) and
//! get returns the same wrapper; malformed wrappers fall back to plain maps;
//! numeric conversion edges are pinned. convert.rs pins these at unit level;
//! these pin them end-to-end.

use serde_json::{Value as Json, json};

use crate::wire::Wire;

/// {"$vector": [...]} stores a typed vector: get returns the same shape, and
/// the field is queryable as an embedding.
#[test]
fn vector_wrapper_roundtrips_through_the_wire() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "k",
        json!({"embedding": {"$vector": [1.0, 0.5, -2.0]}}),
    );
    assert_eq!(
        w.get("docs", "k"),
        json!({"embedding": {"$vector": [1.0, 0.5, -2.0]}})
    );
    // It is a real vector: search ranks by it.
    let out = w.ok(
        "search",
        json!({"collection": "docs",
               "vector": {"field": "embedding", "query": [1.0, 0.5, -2.0], "k": 1}}),
    );
    assert_eq!(out["results"][0]["key"], "k");
}

/// {"$bytes": [0..=255]} round-trips as a byte string.
#[test]
fn bytes_wrapper_roundtrips_through_the_wire() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"blob": {"$bytes": [0, 1, 128, 255]}}));
    assert_eq!(
        w.get("docs", "k"),
        json!({"blob": {"$bytes": [0, 1, 128, 255]}})
    );
    // An empty byte string is legal too.
    w.store("docs", "e", json!({"blob": {"$bytes": []}}));
    assert_eq!(w.get("docs", "e"), json!({"blob": {"$bytes": []}}));
}

/// The wrappers convert NESTED too — inside arrays and maps — and only a
/// single-key object converts (a sibling key forces the plain-map path).
#[test]
fn convert_wrappers_nested_and_multi_key() {
    let mut w = Wire::new();
    let doc = json!({
        "list": [{"$vector": [1.0]}, {"$bytes": [7]}],
        "map": {"inner": {"$vector": [2.0, 3.0]}},
        "both": {"$vector": [1.0], "extra": 1},
    });
    w.store("docs", "k", doc.clone());
    assert_eq!(
        w.get("docs", "k"),
        doc,
        "nested wrappers convert; multi-key maps stay maps"
    );
    // The nested vector is a real embedding (dotted path through the wire).
    let out = w.ok(
        "search",
        json!({"collection": "docs",
               "vector": {"field": "map.inner", "query": [2.0, 3.0], "k": 1}}),
    );
    assert_eq!(out["results"][0]["key"], "k");
}

/// A wrapper whose payload does not match the convention falls back to a
/// plain map and round-trips as the map the client sent.
#[test]
fn convert_malformed_wrappers_fall_back_to_maps() {
    let mut w = Wire::new();
    for (key, doc) in [
        ("str", json!({"$vector": "notanarray"})),
        ("mixed", json!({"$vector": [1.0, "nope"]})),
        ("bytes", json!({"$bytes": [256]})),
        ("bytes-neg", json!({"$bytes": [-1]})),
        ("bytes-str", json!({"$bytes": ["ff"]})),
        ("scalar", json!({"$vector": 3})),
    ] {
        w.store("docs", key, doc.clone());
        assert_eq!(w.get("docs", key), doc, "{key}: fallback preserves the map");
    }
}

/// Integer vs float distinction survives the wire: 5 stays an int, 5.0 stays
/// a float (and they are NOT equal JSON).
#[test]
fn convert_int_float_distinction_survives() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "k",
        json!({"int": 5, "float": 5.0, "neg": -7, "frac": 1.5}),
    );
    assert_eq!(
        w.get("docs", "k"),
        json!({"int": 5, "float": 5.0, "neg": -7, "frac": 1.5})
    );
    // And filters compare with the engine's int/float lattice through the wire.
    let out = w.ok(
        "search",
        json!({"collection": "docs",
               "filter": {"op": "eq", "field": "float", "value": 5.0}}),
    );
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
}

/// A JSON integer beyond i64::MAX has no engine integer: it converts to a
/// LOSSY f64 and comes back as a float (pinned convert limitation).
#[test]
fn convert_u64_beyond_i64_is_lossy_float() {
    let mut w = Wire::new();
    let big: u64 = u64::MAX;
    w.store("docs", "k", json!({"n": big}));
    let got = w.get("docs", "k");
    assert_eq!(
        got["n"].as_f64().unwrap(),
        u64::MAX as f64,
        "u64::MAX converts to its f64 approximation"
    );
    assert!(
        got["n"].as_i64().is_none(),
        "the value no longer parses as an integer"
    );
}

/// Vector components are f32: a value needing more precision comes back at
/// f32 precision through the wire (pinned).
#[test]
fn convert_vector_components_are_f32_precision() {
    let mut w = Wire::new();
    w.store("docs", "k", json!({"embedding": {"$vector": [1.23456789]}}));
    let got = w.get("docs", "k");
    let comps = got["embedding"]["$vector"].as_array().unwrap();
    assert_eq!(
        comps[0].as_f64().unwrap(),
        1.23456789_f64 as f32 as f64,
        "f32-rounded on the way in, expanded on the way out"
    );
    // Scores likewise travel as f32-precision floats.
    let out = w.ok(
        "search",
        json!({"collection": "docs",
               "vector": {"field": "embedding", "query": [1.0], "k": 1}}),
    );
    assert!(
        out["results"][0]["score"]
            .as_f64()
            .is_some_and(|s| (0.0..=1.0).contains(&s))
    );
}

/// Unicode text and keys survive the wire byte-for-byte.
#[test]
fn convert_unicode_text_survives() {
    let mut w = Wire::new();
    w.store("docs", "键", json!({"text": "héllo wörld ✓ 你好"}));
    assert_eq!(w.get("docs", "键"), json!({"text": "héllo wörld ✓ 你好"}));
    assert_eq!(w.get("docs", "absent"), Json::Null);
}
