//! Admin-tool conformance: backup / dump / load through the wire — a backup
//! reopens as a live database, a dump loads into a fresh server with every
//! convention intact, and the file/engine error surfaces are exact.

use corvid_mcp::Server;
use serde_json::{Value as Json, json};

use crate::wire::{self, Wire};

/// The full `docs/a` document used by the dump round-trip: one field per
/// JSON kind plus both convert conventions.
fn rich() -> Json {
    json!({
        "text": "hello", "n": 7, "f": 1.5, "t": true, "nil": null,
        "arr": [1, "two"], "vec": {"$vector": [0.5, 1.5]}, "blob": {"$bytes": [0, 255]},
    })
}

/// backup writes a file that reopens as a full server with the data intact
/// (verified through the wire on the reopened server).
#[test]
fn backup_reopens_as_a_live_database() {
    let dir = tempfile::tempdir().unwrap();
    let ps = dir.path().join("backup.db");
    let ps = ps.to_str().unwrap();
    let mut w = Wire::new();
    w.store("docs", "k", json!({"n": 1, "vec": {"$vector": [1.0, 2.0]}}));
    assert_eq!(
        w.ok("backup", json!({"path": ps})),
        json!({"ok": true, "path": ps})
    );
    // The backup is a complete database: serve it and read through the wire.
    let mut reopened = Wire::over(Server::open(ps).unwrap());
    assert_eq!(
        reopened.get("docs", "k"),
        json!({"n": 1, "vec": {"$vector": [1.0, 2.0]}})
    );
}

/// A second backup to the SAME path is an engine error (the target exists);
/// a missing path param is BadParams.
#[test]
fn backup_existing_target_and_missing_path_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ps = dir.path().join("only-once.db");
    let ps = ps.to_str().unwrap();
    let mut w = Wire::new();
    w.store("docs", "k", json!({"n": 1}));
    w.ok("backup", json!({"path": ps}));
    wire::starts_with(
        &w.err("backup", json!({"path": ps})),
        "backup target already exists: ",
    );
    assert_eq!(
        w.err("backup", json!({})),
        "bad params: missing string 'path'"
    );
}

/// dump -> load round-trips through the wire: documents (including the
/// $vector/$bytes conventions), graph edges, and auto-id counters survive;
/// the loaded server serves them identically.
#[test]
fn dump_then_load_roundtrips_through_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let ps = dir.path().join("dump.bin");
    let ps = ps.to_str().unwrap();
    let mut w = Wire::new();
    w.store("docs", "a", rich());
    w.store("docs", "b", json!({"body": "rust embedded database"}));
    w.ok(
        "link",
        json!({"collection": "docs", "from": "a", "relation": "rel", "to": "b"}),
    );
    let auto = w.ok(
        "insert_auto",
        json!({"collection": "seq", "document": {"n": 1}}),
    );
    assert_eq!(
        w.ok("dump", json!({"path": ps})),
        json!({"ok": true, "path": ps})
    );

    let mut fresh = Wire::new();
    assert_eq!(fresh.ok("load", json!({"path": ps})), json!({"ok": true}));
    assert_eq!(fresh.get("docs", "a"), rich());
    // The text corpus searches identically on the loaded server.
    let out = fresh.ok(
        "search",
        json!({"collection": "docs", "text": {"field": "body", "query": "rust", "k": 5}}),
    );
    assert_eq!(out["results"][0]["key"], "b");
    // Edges and auto-id state survived.
    let out = fresh.ok(
        "neighbors",
        json!({"collection": "docs", "from": "a", "relation": "rel"}),
    );
    assert_eq!(out["neighbors"], json!(["b"]));
    let next = fresh.ok(
        "insert_auto",
        json!({"collection": "seq", "document": {"n": 2}}),
    );
    assert!(
        next["key"].as_str().unwrap() > auto["key"].as_str().unwrap(),
        "the auto-id sequence resumes after the loaded high-water mark"
    );
}

/// load errors: a nonexistent path is BadParams ("cannot open dump file");
/// a file that exists but is not a dump is the engine's InvalidDump error;
/// an unwritable dump target names its failure too.
#[test]
fn load_missing_and_garbage_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Wire::new();
    let missing = dir.path().join("nope.bin");
    wire::starts_with(
        &w.err("load", json!({"path": missing.to_str().unwrap()})),
        "bad params: cannot open dump file: ",
    );
    let garbage = dir.path().join("garbage.bin");
    std::fs::write(&garbage, b"definitely not a corvid dump").unwrap();
    wire::starts_with(
        &w.err("load", json!({"path": garbage.to_str().unwrap()})),
        "invalid dump: ",
    );
    // A path under a regular FILE cannot be created: ENOTDIR.
    let inside = garbage.join("sub").join("x.bin");
    wire::starts_with(
        &w.err("dump", json!({"path": inside.to_str().unwrap()})),
        "bad params: cannot create dump file: ",
    );
    assert_eq!(
        w.err("dump", json!({})),
        "bad params: missing string 'path'"
    );
    assert_eq!(
        w.err("load", json!({})),
        "bad params: missing string 'path'"
    );
}

/// load's optional `rename` param (the `a__b` migration): a wire dump of
/// `docs` loads under a new name with documents and edges intact, the old
/// name absent; `null` behaves like absent; a non-string value or a
/// non-object `rename` is BadParams (no silent fallback); and an invalid
/// target surfaces the engine's exact `InvalidName` error.
#[test]
fn load_rename_param_migrates_collections_through_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let ps = dir.path().join("dump.bin");
    let ps = ps.to_str().unwrap();
    let mut w = Wire::new();
    w.store("docs", "a", json!({"n": 1}));
    w.store("docs", "b", json!({"n": 2}));
    w.ok(
        "link",
        json!({"collection": "docs", "from": "a", "relation": "rel", "to": "b"}),
    );
    w.ok("dump", json!({"path": ps}));

    let mut fresh = Wire::new();
    assert_eq!(
        fresh.ok(
            "load",
            json!({"path": ps, "rename": {"docs": "renamed_docs"}})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        fresh.ok("list_collections", json!({})),
        json!({"collections": ["renamed_docs"]})
    );
    assert_eq!(fresh.get("renamed_docs", "a"), json!({"n": 1}));
    let out = fresh.ok(
        "neighbors",
        json!({"collection": "renamed_docs", "from": "a", "relation": "rel"}),
    );
    assert_eq!(out["neighbors"], json!(["b"]));

    // `null` behaves like absent: a plain load with the same dump.
    let mut plain = Wire::new();
    assert_eq!(
        plain.ok("load", json!({"path": ps, "rename": null})),
        json!({"ok": true})
    );
    assert_eq!(
        plain.ok("list_collections", json!({})),
        json!({"collections": ["docs"]})
    );

    // No silent fallbacks: a non-object rename and a non-string value are
    // BadParams; an invalid target is the engine's InvalidName.
    let mut bad = Wire::new();
    assert_eq!(
        bad.err("load", json!({"path": ps, "rename": ["docs"]})),
        "bad params: 'rename' must be an object of string to string"
    );
    assert_eq!(
        bad.err("load", json!({"path": ps, "rename": {"docs": 7}})),
        "bad params: 'rename' values must be strings: 'docs'"
    );
    wire::starts_with(
        &bad.err("load", json!({"path": ps, "rename": {"docs": "x__y"}})),
        "invalid name (NUL byte or `__` is not allowed): ",
    );
    assert!(
        bad.ok("list_collections", json!({}))["collections"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a rejected rename must not have loaded anything"
    );
}
