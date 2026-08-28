//! Graph-tool conformance: link / unlink / neighbors / in_neighbors /
//! traverse over the wire — including edges without documents, key order,
//! truncation, cycles, and the param-error surface.

use serde_json::json;

use crate::wire::{self, Wire};

fn link(w: &mut Wire, from: &str, relation: &str, to: &str) {
    let out = w.ok(
        "link",
        json!({"collection": "g", "from": from, "relation": relation, "to": to}),
    );
    assert_eq!(out, json!({"ok": true}));
}

/// link creates edges (documents are NOT required — edges live in their own
/// namespace, pinned) and re-linking the same edge is idempotent.
#[test]
fn link_without_docs_and_duplicate_is_idempotent() {
    let mut w = Wire::new();
    link(&mut w, "a", "knows", "b");
    link(&mut w, "a", "knows", "b"); // duplicate: still ok, one edge
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "knows"}),
    );
    assert_eq!(out["neighbors"], json!(["b"]));
}

/// neighbors lists OUT edges in key order; in_neighbors lists the sources of
/// IN edges; `limit` truncates.
#[test]
fn neighbors_and_in_neighbors_directions() {
    let mut w = Wire::new();
    for from in ["a", "b"] {
        for to in ["x", "y", "z"] {
            link(&mut w, from, "r", to);
        }
    }
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "r"}),
    );
    assert_eq!(out["neighbors"], json!(["x", "y", "z"]), "key order");
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "r", "limit": 2}),
    );
    assert_eq!(out["neighbors"], json!(["x", "y"]), "limit truncates");
    let out = w.ok(
        "in_neighbors",
        json!({"collection": "g", "to": "x", "relation": "r"}),
    );
    assert_eq!(out["neighbors"], json!(["a", "b"]));
    let out = w.ok(
        "in_neighbors",
        json!({"collection": "g", "to": "nope", "relation": "r"}),
    );
    assert_eq!(out["neighbors"], json!([]));
    // Unknown relation: empty, not an error.
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "other"}),
    );
    assert_eq!(out["neighbors"], json!([]));
}

/// unlink reports removed true then false for the same edge.
#[test]
fn unlink_reports_removed_true_then_false() {
    let mut w = Wire::new();
    link(&mut w, "a", "r", "b");
    assert_eq!(
        w.ok(
            "unlink",
            json!({"collection": "g", "from": "a", "relation": "r", "to": "b"}),
        ),
        json!({"removed": true})
    );
    assert_eq!(
        w.ok(
            "unlink",
            json!({"collection": "g", "from": "a", "relation": "r", "to": "b"}),
        ),
        json!({"removed": false})
    );
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "r"}),
    );
    assert_eq!(out["neighbors"], json!([]));
}

/// traverse walks breadth-first: hops bound the depth, cycles terminate
/// (visited set), hops 0 yields nothing, and an unknown start yields
/// nothing (both are empty, not errors — pinned).
#[test]
fn traverse_hops_cycles_and_empty_starts() {
    let mut w = Wire::new();
    // a -> b -> c -> d, plus cycle b -> a.
    for (from, to) in [("a", "b"), ("b", "c"), ("c", "d"), ("b", "a")] {
        link(&mut w, from, "r", to);
    }
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "a", "relation": "r", "hops": 1}),
    );
    assert_eq!(out["nodes"], json!(["b"]));
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "a", "relation": "r", "hops": 2}),
    );
    // Level 2 from b would revisit "a" (the cycle edge b->a) — the start is
    // in the visited set from the beginning, so only "c" appears.
    assert_eq!(
        out["nodes"],
        json!(["b", "c"]),
        "BFS order, start never revisited"
    );
    // A cycle with a huge hop budget terminates via the visited set.
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "a", "relation": "r", "hops": 50}),
    );
    assert_eq!(out["nodes"], json!(["b", "c", "d"]));
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "a", "relation": "r", "hops": 0}),
    );
    assert_eq!(out["nodes"], json!([]));
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "ghost", "relation": "r", "hops": 3}),
    );
    assert_eq!(out["nodes"], json!([]));
}

/// Graph param errors: every required string and the `hops` integer are
/// checked with exact messages.
#[test]
fn graph_param_errors() {
    let mut w = Wire::new();
    assert_eq!(
        w.err("link", json!({"collection": "g", "from": "a"})),
        "bad params: missing string 'relation'"
    );
    assert_eq!(
        w.err(
            "link",
            json!({"collection": "g", "from": "a", "relation": "r", "to": 9}),
        ),
        "bad params: missing string 'to'"
    );
    assert_eq!(
        w.err("neighbors", json!({"collection": "g", "relation": "r"})),
        "bad params: missing string 'from'"
    );
    assert_eq!(
        w.err("in_neighbors", json!({"collection": "g", "to": "x"})),
        "bad params: missing string 'relation'"
    );
    assert_eq!(
        w.err(
            "traverse",
            json!({"collection": "g", "start": "a", "relation": "r"}),
        ),
        "bad params: missing non-negative integer 'hops'"
    );
    assert_eq!(
        w.err(
            "traverse",
            json!({"collection": "g", "start": "a", "relation": "r", "hops": "2"}),
        ),
        "bad params: missing non-negative integer 'hops'"
    );
    assert_eq!(
        w.err(
            "traverse",
            json!({"collection": "g", "start": "a", "relation": "r", "hops": -1}),
        ),
        "bad params: missing non-negative integer 'hops'"
    );
    wire::starts_with(
        &w.err(
            "unlink",
            json!({"collection": "g", "from": "a", "relation": "r"}),
        ),
        "bad params: missing string 'to'",
    );
}
