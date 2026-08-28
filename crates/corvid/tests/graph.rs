//! Graph conformance (Task 7): `link`, `link_weighted`, `unlink`,
//! `neighbors`, `neighbors_weighted`, `in_neighbors`, and `traverse` through
//! the public API only — duplicates, self-loops, missing endpoints, relation
//! byte semantics, weights, cycles, the delete cascade, events, and
//! cross-collection isolation.
//!
//! Contract notes pinned by these tests (read from `src/graph.rs` and the
//! delete paths in `src/db.rs` first):
//! * edges are DIRECTED rows `(relation, from, to)` in sibling namespaces
//!   `__edges__<collection>` / `__redges__<collection>`; a `link` writes the
//!   forward row plus a reverse twin (nodes swapped) in ONE transaction, so
//!   `link(a→b)` and `link(b→a)` are FOUR distinct rows and `unlink` is
//!   DIRECTIONAL: it removes the forward row `from --relation--> to` and that
//!   row's own reverse twin only — an edge linked the other way survives;
//! * `link` is idempotent (re-linking overwrites, never errors) but notifies
//!   its Insert event on EVERY call, while `unlink` is silent when it removed
//!   nothing (`Ok(false)`, no event) — a documented asymmetry (PINNED);
//! * endpoints do NOT have to exist as documents, and linking creates no
//!   documents; deleting a document that exists removes every edge touching
//!   it in EITHER role (source or target) in the delete's own transaction,
//!   while a delete that removes nothing (missing key) does NOT cascade;
//! * `link`/`unlink` events use the USER collection name (never the edge
//!   namespace) and are keyed by the `from` key, kind Insert/Delete;
//! * relation names are unvalidated byte strings carried length-prefixed in
//!   the edge key: the empty string, unicode, and byte-prefix pairs ("know"
//!   vs "knows") are all legal and fully isolated from each other; endpoint
//!   keys are raw bytes (empty and unicode keys legal, results in byte
//!   order);
//! * a weighted link stores the f64 little-endian on the edge; a later
//!   PLAIN `link` of the same edge overwrites it with the empty value, which
//!   reads back as the `1.0` unweighted default;
//! * `traverse` is bounded BFS on one read snapshot: depth 0 is empty, depth
//!   1 equals `neighbors`, the start node is seeded visited (self-loops and
//!   cycles never re-emit it), results are deduped and ordered hop by hop
//!   (key order within each hop), and a depth past the diameter just returns
//!   the whole reachable set;
//! * edge namespaces are engine-internal: `Db::collections` never lists them
//!   and the user collection's `scan` never returns them.
//!
//! The smoke test that anchored the radar skeleton during Waves 1-2 is kept
//! as the first test below.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use corvid::{ChangeEvent, ChangeKind, Db, Error, Value, field};

fn doc(del: bool) -> Value {
    let mut m = BTreeMap::new();
    m.insert("del".to_owned(), Value::Bool(del));
    Value::Map(m)
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn graph_smoke_link_neighbors_traverse_unlink() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");

    c.link(b"a", "knows", b"b").unwrap();
    c.link(b"b", "knows", b"c").unwrap();

    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(
        c.traverse(b"a", "knows", 2).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec()]
    );

    // Unlink removes the edge (and its reverse twin).
    assert!(c.unlink(b"a", "knows", b"b").unwrap());
    assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
    assert!(!c.unlink(b"a", "knows", b"b").unwrap());
    // The untouched edge survives.
    assert_eq!(c.neighbors(b"b", "knows").unwrap(), vec![b"c".to_vec()]);
}

// ===========================================================================
// link — new edges, duplicates, self-loops, missing endpoints
// ===========================================================================

/// A new edge is immediately resolvable in both directions: `neighbors` from
/// the source, `in_neighbors` from the target.
#[test]
fn graph_link_new_edge_resolves_in_neighbors_and_in_neighbors() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "knows", b"b").unwrap();

    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.in_neighbors(b"b", "knows").unwrap(), vec![b"a".to_vec()]);
    // Nothing in the other cells of the 2x2 direction matrix.
    assert!(c.neighbors(b"b", "knows").unwrap().is_empty());
    assert!(c.in_neighbors(b"a", "knows").unwrap().is_empty());
}

/// Re-linking the same `(from, relation, to)` is idempotent in the data
/// (exactly one row, one neighbor) but re-emits the Insert event — the
/// documented asymmetry with `unlink`, which is silent when it removes
/// nothing.
#[test]
fn graph_link_duplicate_is_idempotent_and_reemits_insert_event() {
    let db = Db::open_in_memory().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    let c = db.collection("nodes");
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"a", "r", b"b").unwrap();

    // One edge, not three — in both namespaces' views.
    assert_eq!(c.neighbors(b"a", "r").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.in_neighbors(b"b", "r").unwrap(), vec![b"a".to_vec()]);
    // ...but every successful link call notified.
    let expected = vec![
        ChangeEvent {
            collection: "nodes".to_owned(),
            key: b"a".to_vec(),
            kind: ChangeKind::Insert,
        };
        3
    ];
    assert_eq!(*events.lock().unwrap(), expected);
}

/// A self-loop is allowed: `neighbors`/`in_neighbors` list the node itself,
/// while `traverse` seeds its visited set with the start, so the loop adds
/// nothing. Unlink removes it like any other edge.
#[test]
fn graph_link_self_loop_lists_self_but_traverse_excludes_start() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "self", b"a").unwrap();

    assert_eq!(c.neighbors(b"a", "self").unwrap(), vec![b"a".to_vec()]);
    assert_eq!(c.in_neighbors(b"a", "self").unwrap(), vec![b"a".to_vec()]);
    assert!(c.traverse(b"a", "self", 5).unwrap().is_empty());

    // A self-loop does not drown out other targets or the traversal of them.
    c.link(b"a", "self", b"b").unwrap();
    assert_eq!(
        c.neighbors(b"a", "self").unwrap(),
        vec![b"a".to_vec(), b"b".to_vec()]
    );
    assert_eq!(c.traverse(b"a", "self", 1).unwrap(), vec![b"b".to_vec()]);

    assert!(c.unlink(b"a", "self", b"a").unwrap());
    assert_eq!(c.neighbors(b"a", "self").unwrap(), vec![b"b".to_vec()]);
}

/// Endpoints are NOT required to exist as documents: linking to (and from)
/// absent keys succeeds, creates no documents, and the edges are fully
/// queryable.
#[test]
fn graph_link_missing_endpoints_allowed_without_documents() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");

    // No document was ever inserted.
    c.link(b"a", "knows", b"ghost").unwrap();
    c.link(b"phantom", "knows", b"a").unwrap();

    assert_eq!(c.get(b"a").unwrap(), None);
    assert_eq!(c.get(b"ghost").unwrap(), None);
    assert_eq!(c.scan().unwrap(), Vec::<(Vec<u8>, Value)>::new());
    assert_eq!(c.len().unwrap(), 0);

    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"ghost".to_vec()]);
    assert_eq!(
        c.in_neighbors(b"a", "knows").unwrap(),
        vec![b"phantom".to_vec()]
    );
    assert_eq!(
        c.traverse(b"phantom", "knows", 2).unwrap(),
        vec![b"a".to_vec(), b"ghost".to_vec()]
    );
}

/// Relation names are unvalidated length-prefixed byte strings: the empty
/// string, unicode, and a byte-prefix pair are all accepted and completely
/// isolated from one another in every graph read.
#[test]
fn graph_link_relation_isolation_empty_unicode_and_byte_prefix() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "know", b"b").unwrap();
    c.link(b"a", "knows", b"c").unwrap();
    c.link(b"a", "", b"e").unwrap();
    c.link(b"a", "знает", b"f").unwrap();
    // A relation whose bytes are a prefix of another's never bleeds into it.
    c.link(b"a", "knows", b"d").unwrap();

    assert_eq!(c.neighbors(b"a", "know").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(
        c.neighbors(b"a", "knows").unwrap(),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(c.neighbors(b"a", "").unwrap(), vec![b"e".to_vec()]);
    assert_eq!(c.neighbors(b"a", "знает").unwrap(), vec![b"f".to_vec()]);

    // Isolation holds for in_neighbors and traverse too.
    assert_eq!(c.in_neighbors(b"b", "know").unwrap(), vec![b"a".to_vec()]);
    assert!(c.in_neighbors(b"b", "knows").unwrap().is_empty());
    // d is reachable only through "knows", never through "know".
    assert_eq!(c.traverse(b"a", "know", 9).unwrap(), vec![b"b".to_vec()]);
    assert_eq!(
        c.traverse(b"a", "knows", 9).unwrap(),
        vec![b"c".to_vec(), b"d".to_vec()]
    );

    // Unlink under one relation leaves the prefix-mate untouched.
    assert!(c.unlink(b"a", "know", b"b").unwrap());
    assert!(c.neighbors(b"a", "know").unwrap().is_empty());
    assert_eq!(
        c.neighbors(b"a", "knows").unwrap(),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
}

/// Endpoint keys are raw bytes: empty and unicode keys are legal, and every
/// result list is in byte order.
#[test]
fn graph_link_endpoint_keys_empty_and_unicode_in_byte_order() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    // Targets of "z" in byte order: "" < "y" (0x79) < "ключ" (0xD0..) <
    // "鍵" (0xE9..).
    c.link(b"z", "r", b"").unwrap();
    c.link(b"z", "r", b"y").unwrap();
    c.link(b"z", "r", "ключ".as_bytes()).unwrap();
    c.link(b"z", "r", "鍵".as_bytes()).unwrap();
    assert_eq!(
        c.neighbors(b"z", "r").unwrap(),
        vec![
            b"".to_vec(),
            b"y".to_vec(),
            "ключ".as_bytes().to_vec(),
            "鍵".as_bytes().to_vec(),
        ]
    );

    // An empty SOURCE key is legal too (the key encoding is length-prefixed,
    // so "" is representable and never confused with another source).
    c.link(b"", "r", b"x").unwrap();
    assert_eq!(c.neighbors(b"", "r").unwrap(), vec![b"x".to_vec()]);
    assert_eq!(c.in_neighbors(b"x", "r").unwrap(), vec![b"".to_vec()]);

    // Unicode sources land in byte order among the others' in_neighbors.
    c.link("к".as_bytes(), "r", b"x").unwrap();
    assert_eq!(
        c.in_neighbors(b"x", "r").unwrap(),
        vec![b"".to_vec(), "к".as_bytes().to_vec()]
    );
}

// ===========================================================================
// link_weighted / neighbors_weighted
// ===========================================================================

/// The weight is stored on the edge and read back exactly; a PLAIN `link` of
/// the same edge overwrites it with the empty value, which decodes as the
/// 1.0 unweighted default.
#[test]
fn graph_link_weighted_roundtrip_and_overwrite_semantics() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");

    // Unweighted edges report the 1.0 default.
    c.link(b"a", "r", b"b").unwrap();
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), 1.0)]
    );

    // A weighted link replaces the empty value...
    c.link_weighted(b"a", "r", b"b", 0.25).unwrap();
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), 0.25)]
    );
    // ...a re-weight overwrites in place (no duplicate edge)...
    c.link_weighted(b"a", "r", b"b", -3.5).unwrap();
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), -3.5)]
    );
    // ...and a later plain link resets the weight to the 1.0 default.
    c.link(b"a", "r", b"b").unwrap();
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), 1.0)]
    );

    // The reverse twin carries the weight too: the weighted view of
    // in-edges is out of scope (in_neighbors returns keys only), but the
    // weighted row sits in the same (relation, from) prefix as unweighted
    // ones and keeps key order.
    c.link_weighted(b"a", "r", b"c", 7.5).unwrap();
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), 1.0), (b"c".to_vec(), 7.5)]
    );

    // Unlink removes weighted edges like any other.
    assert!(c.unlink(b"a", "r", b"c").unwrap());
    assert_eq!(
        c.neighbors_weighted(b"a", "r").unwrap(),
        vec![(b"b".to_vec(), 1.0)]
    );
}

/// Every f64 payload round-trips bit-exactly through the edge: zeros (with
/// sign), the maxima, infinities, and NaN.
#[test]
fn graph_link_weighted_float_extremes_round_trip() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link_weighted(b"a", "r", b"zero", 0.0).unwrap();
    c.link_weighted(b"a", "r", b"negzero", -0.0).unwrap();
    c.link_weighted(b"a", "r", b"max", f64::MAX).unwrap();
    c.link_weighted(b"a", "r", b"min", f64::MIN).unwrap();
    c.link_weighted(b"a", "r", b"inf", f64::INFINITY).unwrap();
    c.link_weighted(b"a", "r", b"ninf", f64::NEG_INFINITY)
        .unwrap();
    c.link_weighted(b"a", "r", b"nan", f64::NAN).unwrap();

    let w: std::collections::HashMap<_, _> = c
        .neighbors_weighted(b"a", "r")
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(w.len(), 7);
    assert_eq!(w[b"zero".as_slice()], 0.0);
    // -0.0 survives with its sign (0.0 == -0.0, so check is_sign_negative).
    assert_eq!(w[b"negzero".as_slice()], 0.0);
    assert!(w[b"negzero".as_slice()].is_sign_negative());
    assert_eq!(w[b"max".as_slice()], f64::MAX);
    assert_eq!(w[b"min".as_slice()], f64::MIN);
    assert_eq!(w[b"inf".as_slice()], f64::INFINITY);
    assert_eq!(w[b"ninf".as_slice()], f64::NEG_INFINITY);
    assert!(w[b"nan".as_slice()].is_nan());
}

// ===========================================================================
// link / unlink on reserved and invalid collection names
// ===========================================================================

/// Every graph write path runs the same writability gate as document writes:
/// a leading `__` (the edge namespaces themselves) is
/// `Error::ReservedCollection`; an interior `__` or a NUL byte is
/// `Error::InvalidName`.
#[test]
fn graph_link_unlink_reject_reserved_and_invalid_collection_names() {
    let db = Db::open_in_memory().unwrap();

    let err = db
        .collection("__edges__nodes")
        .link(b"a", "r", b"b")
        .unwrap_err();
    assert!(matches!(&err, Error::ReservedCollection(n) if n == "__edges__nodes"));
    let err = db
        .collection("__redges__nodes")
        .link_weighted(b"a", "r", b"b", 1.0)
        .unwrap_err();
    assert!(matches!(&err, Error::ReservedCollection(n) if n == "__redges__nodes"));
    let err = db
        .collection("__edges__nodes")
        .unlink(b"a", "r", b"b")
        .unwrap_err();
    assert!(matches!(&err, Error::ReservedCollection(n) if n == "__edges__nodes"));

    let err = db.collection("a__b").link(b"a", "r", b"b").unwrap_err();
    assert!(matches!(&err, Error::InvalidName(n) if n == "a__b"));
    let err = db.collection("a\0b").unlink(b"a", "r", b"b").unwrap_err();
    assert!(matches!(&err, Error::InvalidName(n) if n == "a\0b"));

    // Nothing was written anywhere by the rejected calls.
    assert!(
        db.collection("nodes")
            .neighbors(b"a", "r")
            .unwrap()
            .is_empty()
    );
}

// ===========================================================================
// unlink — removal, directionality, silence, relation scoping
// ===========================================================================

/// Unlink removes the forward row AND its reverse twin: both the source's
/// `neighbors` and the target's `in_neighbors` go empty; a second unlink
/// reports `false`.
#[test]
fn graph_unlink_removes_edge_and_reverse_twin() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "knows", b"b").unwrap();

    assert!(c.unlink(b"a", "knows", b"b").unwrap());
    assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
    assert!(c.in_neighbors(b"b", "knows").unwrap().is_empty());
    assert!(!c.unlink(b"a", "knows", b"b").unwrap());
}

/// `link(a→b)` and `link(b→a)` are four distinct rows. Unlinking `a→b`
/// leaves `b→a` fully intact — removal is directional, and unlinking the
/// not-currently-linked direction is a silent `false`.
#[test]
fn graph_unlink_is_directional_reverse_direction_edge_survives() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"b", "r", b"a").unwrap();
    assert_eq!(c.neighbors(b"a", "r").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.neighbors(b"b", "r").unwrap(), vec![b"a".to_vec()]);

    assert!(c.unlink(b"a", "r", b"b").unwrap());
    // a→b is gone in both views; b→a survives in both of ITS views.
    assert!(c.neighbors(b"a", "r").unwrap().is_empty());
    assert!(c.in_neighbors(b"b", "r").unwrap().is_empty());
    assert_eq!(c.neighbors(b"b", "r").unwrap(), vec![b"a".to_vec()]);
    assert_eq!(c.in_neighbors(b"a", "r").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.traverse(b"b", "r", 5).unwrap(), vec![b"a".to_vec()]);
    assert!(c.traverse(b"a", "r", 5).unwrap().is_empty());

    // Now the twin-direction unlink removes the survivor.
    assert!(c.unlink(b"b", "r", b"a").unwrap());
    assert!(c.neighbors(b"b", "r").unwrap().is_empty());
    assert!(c.in_neighbors(b"a", "r").unwrap().is_empty());
    assert!(!c.unlink(b"b", "r", b"a").unwrap());
}

/// Unlink of an edge that was never linked is a silent no-op: `Ok(false)`,
/// no state change, no event.
#[test]
fn graph_unlink_missing_edge_is_silent_noop_returning_false() {
    let db = Db::open_in_memory().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    let c = db.collection("nodes");
    c.link(b"a", "r", b"b").unwrap();
    events.lock().unwrap().clear();

    assert!(!c.unlink(b"x", "r", b"y").unwrap());
    assert!(!c.unlink(b"a", "other", b"b").unwrap());
    // The existing edge is untouched by all the misses.
    assert_eq!(c.neighbors(b"a", "r").unwrap(), vec![b"b".to_vec()]);
    assert!(events.lock().unwrap().is_empty());
}

/// Unlink is scoped to the named relation: siblings under other relations
/// (including a wrong-relation unlink of the same endpoints) survive.
#[test]
fn graph_unlink_removes_only_the_named_relation() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "knows", b"b").unwrap();
    c.link(b"a", "likes", b"b").unwrap();

    // A miss under the wrong relation removes nothing.
    assert!(!c.unlink(b"a", "hates", b"b").unwrap());
    assert!(c.unlink(b"a", "knows", b"b").unwrap());

    assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
    assert!(c.in_neighbors(b"b", "knows").unwrap().is_empty());
    assert_eq!(c.neighbors(b"a", "likes").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.in_neighbors(b"b", "likes").unwrap(), vec![b"a".to_vec()]);
}

// ===========================================================================
// neighbors (outgoing)
// ===========================================================================

/// Outgoing neighbors come back in byte key order; a node with no out-edges
/// and a node that does not exist at all both yield an empty Vec (never an
/// error), as does a relation with no edges.
#[test]
fn graph_neighbors_key_order_no_out_edges_and_missing_node() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "r", b"d").unwrap();
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"a", "r", b"c").unwrap();
    // A pure target: it has an in-edge but no out-edges.
    c.link(b"z", "r", b"t").unwrap();

    assert_eq!(
        c.neighbors(b"a", "r").unwrap(),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // Node with no out-edges, node that never appeared anywhere, and a
    // relation with no edges: all empty, all Ok.
    assert!(c.neighbors(b"t", "r").unwrap().is_empty());
    assert!(c.neighbors(b"ghost", "r").unwrap().is_empty());
    assert!(c.neighbors(b"a", "no-such-relation").unwrap().is_empty());
    // A collection that was never touched at all behaves the same.
    assert!(
        db.collection("elsewhere")
            .neighbors(b"a", "r")
            .unwrap()
            .is_empty()
    );
}

// ===========================================================================
// in_neighbors (incoming)
// ===========================================================================

/// in_neighbors mirrors neighbors on the reverse namespace: sources in byte
/// order, isolated by relation, empty for a pure source / a missing node.
#[test]
fn graph_in_neighbors_mirror_target_only_and_mixed() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"d", "r", b"x").unwrap();
    c.link(b"b", "r", b"x").unwrap();
    c.link(b"c", "r", b"x").unwrap();
    c.link(b"a", "r", b"y").unwrap();
    c.link(b"x", "r", b"y").unwrap();
    c.link(b"b", "likes", b"x").unwrap();

    // Only-a-target node: incoming from three sources, in byte order.
    assert_eq!(
        c.in_neighbors(b"x", "r").unwrap(),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // Relation isolation on the incoming side.
    assert_eq!(c.in_neighbors(b"x", "likes").unwrap(), vec![b"b".to_vec()]);
    // Mixed node: x is both a target (of b/c/d) and a source (of y).
    assert_eq!(c.neighbors(b"x", "r").unwrap(), vec![b"y".to_vec()]);
    // y is a target of two sources under one relation, in byte order.
    assert_eq!(
        c.in_neighbors(b"y", "r").unwrap(),
        vec![b"a".to_vec(), b"x".to_vec()]
    );
    // A pure source has no in-edges; a missing node has none either.
    assert!(c.in_neighbors(b"a", "r").unwrap().is_empty());
    assert!(c.in_neighbors(b"ghost", "r").unwrap().is_empty());
}

// ===========================================================================
// traverse
// ===========================================================================

/// Depth 0 is always empty; depth 1 is exactly `neighbors` (same set, same
/// key order).
#[test]
fn graph_traverse_depth_zero_empty_depth_one_equals_neighbors() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "r", b"c").unwrap();
    c.link(b"a", "r", b"b").unwrap();

    assert!(c.traverse(b"a", "r", 0).unwrap().is_empty());
    assert_eq!(
        c.traverse(b"a", "r", 1).unwrap(),
        c.neighbors(b"a", "r").unwrap()
    );
    assert_eq!(
        c.traverse(b"a", "r", 1).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
}

/// Multi-hop traversal returns nodes in BFS order — hop 1 in key order, then
/// hop 2 in key order — and a depth past the diameter simply returns the
/// whole reachable set (the frontier empties and the walk stops).
#[test]
fn graph_traverse_multi_hop_bfs_order_exact() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    // Chain a → b → c → d.
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"b", "r", b"c").unwrap();
    c.link(b"c", "r", b"d").unwrap();

    assert_eq!(
        c.traverse(b"a", "r", 2).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        c.traverse(b"a", "r", 3).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // Far past the diameter: same full reachable set, and never the start.
    let full = c.traverse(b"a", "r", 1000).unwrap();
    assert_eq!(full, vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    assert!(!full.contains(&b"a".to_vec()));

    // Starting mid-chain sees only the suffix.
    assert_eq!(
        c.traverse(b"b", "r", 100).unwrap(),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
    // A depth that cuts the chain short.
    assert_eq!(c.traverse(b"c", "r", 1).unwrap(), vec![b"d".to_vec()]);
}

/// Cycles terminate: a 2-cycle, a 3-cycle, and a self-loop-in-passing all
/// yield the deduped reachable set with the start excluded, at a depth far
/// beyond the node count.
#[test]
fn graph_traverse_cycles_terminate_with_deduped_set() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    // 2-cycle a ↔ b.
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"b", "r", b"a").unwrap();
    assert_eq!(c.traverse(b"a", "r", 100).unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.traverse(b"b", "r", 100).unwrap(), vec![b"a".to_vec()]);

    // 3-cycle c → d → e → c.
    c.link(b"c", "r", b"d").unwrap();
    c.link(b"d", "r", b"e").unwrap();
    c.link(b"e", "r", b"c").unwrap();
    assert_eq!(
        c.traverse(b"c", "r", 100).unwrap(),
        vec![b"d".to_vec(), b"e".to_vec()]
    );
    assert_eq!(
        c.traverse(b"e", "r", 100).unwrap(),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
}

/// Branching walks breadth-first (hop 1's targets before hop 2's) and a
/// diamond converges on the shared node exactly once.
#[test]
fn graph_traverse_branching_and_diamond_convergence_order() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    // Fan-out: a → {b, c}, then b → d, c → e.
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"a", "r", b"c").unwrap();
    c.link(b"b", "r", b"d").unwrap();
    c.link(b"c", "r", b"e").unwrap();
    assert_eq!(
        c.traverse(b"a", "r", 2).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec(), b"e".to_vec()]
    );

    // Diamond: x → {p, q}, both p and q → z. z is discovered once, at
    // hop 2, in the position of its first discovery.
    c.link(b"x", "r", b"p").unwrap();
    c.link(b"x", "r", b"q").unwrap();
    c.link(b"p", "r", b"z").unwrap();
    c.link(b"q", "r", b"z").unwrap();
    assert_eq!(
        c.traverse(b"x", "r", 2).unwrap(),
        vec![b"p".to_vec(), b"q".to_vec(), b"z".to_vec()]
    );
}

/// Traversal follows exactly one relation: a path whose second hop is under
/// a different relation stops after the first hop.
#[test]
fn graph_traverse_relation_isolated_from_other_relations() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "road", b"b").unwrap();
    c.link(b"b", "rail", b"c").unwrap();
    c.link(b"c", "road", b"d").unwrap();

    assert_eq!(c.traverse(b"a", "road", 5).unwrap(), vec![b"b".to_vec()]);
    assert_eq!(c.traverse(b"a", "rail", 5).unwrap(), Vec::<Vec<u8>>::new());
    // From b the rail line is walkable and dead-ends at c (c's out-edge is
    // a road).
    assert_eq!(c.traverse(b"b", "rail", 5).unwrap(), vec![b"c".to_vec()]);
}

/// A start node with no edges at all — including one that never existed —
/// traverses to nothing, at any depth.
#[test]
fn graph_traverse_missing_start_node_is_empty() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "r", b"b").unwrap();
    c.link(b"b", "r", b"c").unwrap();

    assert!(c.traverse(b"ghost", "r", 4).unwrap().is_empty());
    // A node that exists as a target but has no out-edges.
    assert!(c.traverse(b"c", "r", 4).unwrap().is_empty());
    // ...and an untouched collection.
    assert!(
        db.collection("elsewhere")
            .traverse(b"a", "r", 4)
            .unwrap()
            .is_empty()
    );
}

// ===========================================================================
// Delete cascade
// ===========================================================================

/// Deleting a document removes every edge that has it as an endpoint — in
/// EITHER role, under EVERY relation — observable from both directions:
/// the deleted node's own neighbors/in_neighbors go empty, AND the other
/// endpoints' views drop the deleted node. Untouched edges and documents
/// survive.
#[test]
fn graph_delete_cascades_edges_in_both_namespaces() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    for k in [&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..]] {
        c.insert(k, &Value::Int(1)).unwrap();
    }
    // a is the source of two edges (two relations) and the target of d's.
    // b→c is a bystander.
    c.link(b"a", "knows", b"b").unwrap();
    c.link(b"a", "likes", b"c").unwrap();
    c.link(b"d", "knows", b"a").unwrap();
    c.link(b"b", "knows", b"c").unwrap();

    assert!(c.delete(b"a").unwrap());

    // The deleted node's own views: out-edges (every relation) and
    // in-edges all gone.
    assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
    assert!(c.neighbors(b"a", "likes").unwrap().is_empty());
    assert!(c.in_neighbors(b"a", "knows").unwrap().is_empty());

    // The other endpoints' views: d's only out-edge pointed at a; b no
    // longer has a among its in-neighbors; c no longer has a under "likes".
    assert!(c.neighbors(b"d", "knows").unwrap().is_empty());
    assert!(c.traverse(b"d", "knows", 5).unwrap().is_empty());
    assert_eq!(
        c.in_neighbors(b"b", "knows").unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert!(c.in_neighbors(b"c", "likes").unwrap().is_empty());

    // The bystander edge and the surviving documents are intact.
    assert_eq!(c.neighbors(b"b", "knows").unwrap(), vec![b"c".to_vec()]);
    assert_eq!(c.in_neighbors(b"c", "knows").unwrap(), vec![b"b".to_vec()]);
    for k in [&b"b"[..], &b"c"[..], &b"d"[..]] {
        assert_eq!(c.get(k).unwrap(), Some(Value::Int(1)));
    }
}

/// The cascade runs only for a document that was actually removed: a delete
/// of a missing key returns `false` and leaves dangling edges alone, while
/// deleting the endpoint once it exists cleans them up.
#[test]
fn graph_delete_missing_document_does_not_cascade() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.link(b"a", "r", b"ghost").unwrap();

    // ghost is not a document: the delete removes nothing and cascades
    // nothing.
    assert!(!c.delete(b"ghost").unwrap());
    assert_eq!(c.neighbors(b"a", "r").unwrap(), vec![b"ghost".to_vec()]);

    // Once ghost exists as a document, deleting it takes the edge along.
    c.insert(b"ghost", &Value::Int(1)).unwrap();
    assert!(c.delete(b"ghost").unwrap());
    assert!(c.neighbors(b"a", "r").unwrap().is_empty());
    // Re-deleting is a no-op again.
    assert!(!c.delete(b"ghost").unwrap());
}

/// The batch delete paths cascade exactly like the single-key path:
/// `delete_where` (each match removed through the delete path) and
/// `delete_batch`.
#[test]
fn graph_delete_where_and_delete_batch_cascade_edges() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.insert(b"a", &doc(true)).unwrap();
    c.insert(b"b", &doc(false)).unwrap();
    c.insert(b"c", &doc(true)).unwrap();
    c.insert(b"d", &doc(false)).unwrap();
    c.insert(b"e", &doc(false)).unwrap();
    c.insert(b"f", &doc(false)).unwrap();
    // Three bidirectional pairs: a↔b, c↔d (each has one del=true endpoint),
    // and the untouched e↔f.
    for (x, y) in [(b"a", b"b"), (b"c", b"d"), (b"e", b"f")] {
        c.link(x, "r", y).unwrap();
        c.link(y, "r", x).unwrap();
    }

    assert_eq!(
        c.delete_where(field("del").eq(Value::Bool(true))).unwrap(),
        2
    );
    // a and c are gone; EVERY edge touching them went too — including the
    // halves owned by the survivors b and d.
    for k in [&b"a"[..], &b"c"[..]] {
        assert!(c.neighbors(k, "r").unwrap().is_empty());
        assert!(c.in_neighbors(k, "r").unwrap().is_empty());
    }
    for k in [&b"b"[..], &b"d"[..]] {
        assert!(c.neighbors(k, "r").unwrap().is_empty());
        assert!(c.in_neighbors(k, "r").unwrap().is_empty());
    }
    // e↔f still stands.
    assert_eq!(c.neighbors(b"e", "r").unwrap(), vec![b"f".to_vec()]);
    assert_eq!(c.in_neighbors(b"f", "r").unwrap(), vec![b"e".to_vec()]);

    // delete_batch removes the surviving pair and its edge with it.
    assert_eq!(c.delete_batch(&[b"e", b"f"]).unwrap(), 2);
    assert!(c.neighbors(b"e", "r").unwrap().is_empty());
    assert!(c.in_neighbors(b"f", "r").unwrap().is_empty());
}

// ===========================================================================
// Edge namespaces are engine-internal
// ===========================================================================

/// Linking never surfaces the edge namespaces as user data: `collections()`
/// lists only user collections, and the user collection's `scan` returns
/// only real documents.
#[test]
fn graph_edge_namespaces_hidden_from_collections_and_scan() {
    let db = Db::open_in_memory().unwrap();
    let nodes = db.collection("nodes");
    let docs = db.collection("docs");
    nodes.insert(b"a", &Value::Int(1)).unwrap();
    docs.insert(b"x", &Value::Int(2)).unwrap();
    nodes.link(b"a", "r", b"b").unwrap();
    nodes.link(b"b", "r", b"a").unwrap();
    nodes.link_weighted(b"a", "w", b"c", 0.5).unwrap();
    docs.link(b"x", "r", b"a").unwrap();

    assert_eq!(
        db.collections().unwrap(),
        vec!["docs".to_owned(), "nodes".to_owned()]
    );
    assert_eq!(nodes.scan().unwrap(), vec![(b"a".to_vec(), Value::Int(1))]);
    assert_eq!(docs.scan().unwrap(), vec![(b"x".to_vec(), Value::Int(2))]);
    assert_eq!(nodes.len().unwrap(), 1);
    assert_eq!(docs.len().unwrap(), 1);
}

// ===========================================================================
// Events
// ===========================================================================

/// link/link_weighted notify `ChangeKind::Insert` and unlink
/// `ChangeKind::Delete`, always AFTER the commit, on the USER collection
/// name (never an edge namespace) and keyed by the `from` endpoint; a
/// failed unlink notifies nothing.
#[test]
fn graph_link_unlink_events_exact_shape_keyed_by_from_on_user_collection() {
    let db = Db::open_in_memory().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    let c = db.collection("nodes");
    c.link(b"a", "knows", b"b").unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![ChangeEvent {
            collection: "nodes".to_owned(),
            key: b"a".to_vec(),
            kind: ChangeKind::Insert,
        }]
    );

    // Weighted links carry the same event shape (and the key is still the
    // SOURCE, not the target).
    c.link_weighted(b"a", "trusts", b"c", 0.5).unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Insert,
            },
            ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Insert,
            },
        ]
    );

    c.unlink(b"a", "knows", b"b").unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Insert,
            },
            ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Insert,
            },
            ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Delete,
            },
        ]
    );

    // The failed unlink is silent.
    assert!(!c.unlink(b"a", "knows", b"b").unwrap());
    assert_eq!(events.lock().unwrap().len(), 3);
}

/// The delete cascade emits exactly ONE event — the document's own Delete —
/// never a per-edge event.
#[test]
fn graph_delete_cascade_emits_only_document_delete_event() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");
    c.insert(b"a", &Value::Int(1)).unwrap();
    c.link(b"a", "knows", b"b").unwrap();
    c.link(b"a", "likes", b"c").unwrap();
    c.link(b"d", "knows", b"a").unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    assert!(c.delete(b"a").unwrap());
    assert_eq!(
        *events.lock().unwrap(),
        vec![ChangeEvent {
            collection: "nodes".to_owned(),
            key: b"a".to_vec(),
            kind: ChangeKind::Delete,
        }]
    );
}

// ===========================================================================
// Cross-collection isolation
// ===========================================================================

/// The same relation name in two collections is two independent graphs:
/// links, reads, and unlinks never cross the collection boundary.
#[test]
fn graph_same_relation_independent_across_collections() {
    let db = Db::open_in_memory().unwrap();
    let a = db.collection("alpha");
    let b = db.collection("beta");
    a.link(b"n", "knows", b"b").unwrap();
    b.link(b"n", "knows", b"c").unwrap();

    assert_eq!(a.neighbors(b"n", "knows").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(b.neighbors(b"n", "knows").unwrap(), vec![b"c".to_vec()]);
    assert_eq!(a.in_neighbors(b"b", "knows").unwrap(), vec![b"n".to_vec()]);
    assert_eq!(b.in_neighbors(b"c", "knows").unwrap(), vec![b"n".to_vec()]);

    // Unlinking in one collection leaves the other intact.
    assert!(a.unlink(b"n", "knows", b"b").unwrap());
    assert!(a.neighbors(b"n", "knows").unwrap().is_empty());
    assert_eq!(b.neighbors(b"n", "knows").unwrap(), vec![b"c".to_vec()]);
}

/// Edge namespaces are per-collection, so endpoint keys that collide across
/// collections do not interact: deleting the colliding DOCUMENT in one
/// collection cascades only that collection's edges.
#[test]
fn graph_cross_collection_endpoint_key_collision_namespaced() {
    let db = Db::open_in_memory().unwrap();
    let a = db.collection("alpha");
    let b = db.collection("beta");
    a.insert(b"x", &Value::Int(1)).unwrap();
    b.insert(b"x", &Value::Int(2)).unwrap();
    a.link(b"x", "r", b"z").unwrap();

    // Deleting beta's "x" document does not touch alpha's edges.
    assert!(b.delete(b"x").unwrap());
    assert_eq!(a.neighbors(b"x", "r").unwrap(), vec![b"z".to_vec()]);
    assert_eq!(a.get(b"x").unwrap(), Some(Value::Int(1)));

    // Deleting alpha's "x" cascades alpha's edge (and only that).
    assert!(a.delete(b"x").unwrap());
    assert!(a.neighbors(b"x", "r").unwrap().is_empty());
}
