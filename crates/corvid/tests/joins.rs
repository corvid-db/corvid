//! Join conformance (Task 6): `Collection::join` and `JoinRow` through the
//! public API only — happy resolution, dotted foreign-key paths, every
//! foreign-key value kind, self-joins, empty sides, mutation interplay, and
//! row ordering.
//!
//! Contract notes pinned by these tests (read from `src/join.rs` first):
//! * the join is LEFT-OUTER at the ROW level: every document of the left
//!   collection produces exactly one `JoinRow`, in the left collection's
//!   KEY ORDER — a missing foreign-key field, an unusable field kind, and a
//!   dangling reference all yield `right: None` while KEEPING the row (rows
//!   are never dropped; an inner join is `rows.filter(|r| r.right.is_some())`);
//! * `JoinRow` carries `key` (the LEFT document's key), `left` (the full
//!   left document, unprojected), and `right` (the matched right document
//!   or `None`);
//! * the foreign key must be `Text`, `Bytes`, or `Int`: `Text` uses the
//!   string's bytes, `Bytes` the raw bytes, and `Int` its DECIMAL string —
//!   so `Int(5)` and `Text("5")` resolve the same right key `b"5"`, while
//!   every other kind (`Float`, `Bool`, `Null`, containers, vectors) is
//!   unusable and yields `right: None`;
//! * the foreign-key field may be a dotted path traversing nested maps;
//! * joining against a collection that does not exist is not an error —
//!   every lookup misses, so all rows carry `right: None`;
//! * both legs read one snapshot: the left scan and every right lookup
//!   reflect a single point in time.
//!
//! The smoke test that anchored the radar skeleton during Waves 1-2 is kept
//! as the first test below.

use std::collections::BTreeMap;

use corvid::{Db, JoinRow, Value};

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

/// The joined keys, in order — order-SENSITIVE because left key order IS
/// the contract under test here.
fn keys(rows: &[JoinRow]) -> Vec<Vec<u8>> {
    rows.iter().map(|r| r.key.clone()).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn joins_smoke_left_outer_resolves_and_misses() {
    let db = Db::open_in_memory().unwrap();
    let authors = db.collection("authors");
    authors
        .insert(b"ada", &map(&[("name", text("Ada"))]))
        .unwrap();
    let posts = db.collection("posts");
    posts
        .insert(b"p1", &map(&[("author_id", text("ada"))]))
        .unwrap();
    posts
        .insert(b"p2", &map(&[("author_id", text("ghost"))]))
        .unwrap();
    let mut no_fk = BTreeMap::new();
    no_fk.insert("title".to_owned(), Value::Text("no author field".into()));
    posts.insert(b"p3", &Value::Map(no_fk)).unwrap();

    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 3);
    // In the left collection's key order.
    assert_eq!(rows[0].key, b"p1".to_vec());
    assert_eq!(rows[1].key, b"p2".to_vec());
    assert_eq!(rows[2].key, b"p3".to_vec());
    // Resolved foreign key.
    assert_eq!(rows[0].right, Some(map(&[("name", text("Ada"))])));
    // Dangling reference and missing field both yield a left-outer None.
    assert_eq!(rows[1].right, None);
    assert_eq!(rows[2].right, None);
    assert_eq!(
        rows[0].left.get("author_id"),
        Some(&Value::Text("ada".into()))
    );
}

// ===========================================================================
// Happy path — JoinRow shape
// ===========================================================================

#[test]
fn joins_happy_path_join_row_shape_is_exact() {
    let db = Db::open_in_memory().unwrap();
    let ada = map(&[("name", text("Ada")), ("lang", text("Lean"))]);
    let p1 = map(&[
        ("title", text("Notes")),
        ("author_id", text("ada")),
        ("meta", map(&[("year", Value::Int(1843))])),
    ]);
    db.collection("authors").insert(b"ada", &ada).unwrap();
    db.collection("posts").insert(b"p1", &p1).unwrap();

    let rows = db.collection("posts").join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 1);
    // The full triple is exact: key, the complete unprojected left
    // document (nested structure intact), and the complete right document.
    assert_eq!(rows[0].key, b"p1".to_vec());
    assert_eq!(rows[0].left, p1);
    assert_eq!(rows[0].right, Some(ada));

    // JoinRow is Debug + Clone + PartialEq (comparable and printable).
    let cloned = rows[0].clone();
    assert_eq!(cloned, rows[0]);
    let _ = format!("{:?}", rows[0]);
}

// ===========================================================================
// Dotted foreign-key paths
// ===========================================================================

#[test]
fn joins_dotted_foreign_key_path_resolves_nested_maps() {
    let db = Db::open_in_memory().unwrap();
    db.collection("authors")
        .insert(b"u1", &map(&[("name", text("Ada"))]))
        .unwrap();
    // The foreign key sits behind two levels of nesting; sibling fields
    // along the path are ignored.
    db.collection("posts")
        .insert(
            b"p1",
            &map(&[
                (
                    "meta",
                    map(&[(
                        "author",
                        map(&[("id", text("u1")), ("role", text("admin"))]),
                    )]),
                ),
                ("title", text("deep")),
            ]),
        )
        .unwrap();
    let rows = db
        .collection("posts")
        .join("authors", "meta.author.id")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].right, Some(map(&[("name", text("Ada"))])));

    // A dotted path that descends through a SCALAR misses (no traversal
    // into non-maps), yielding right: None — the row stays.
    db.collection("posts")
        .insert(
            b"p2",
            &map(&[("meta", Value::Int(7)), ("title", text("scalar"))]),
        )
        .unwrap();
    let rows = db
        .collection("posts")
        .join("authors", "meta.author.id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].right, Some(map(&[("name", text("Ada"))])));
    assert_eq!(rows[1].right, None);
}

// ===========================================================================
// Misses — missing field vs dangling reference (rows retained)
// ===========================================================================

#[test]
fn joins_missing_fk_field_and_dangling_reference_retain_rows_with_none() {
    let db = Db::open_in_memory().unwrap();
    let authors = db.collection("authors");
    authors
        .insert(b"u1", &map(&[("name", text("Ada"))]))
        .unwrap();
    let posts = db.collection("posts");
    let p_missing = map(&[("title", text("no fk field"))]);
    let p_dangling = map(&[("title", text("dangling")), ("fk", text("nobody"))]);
    let p_null = map(&[("title", text("null fk")), ("fk", Value::Null)]);
    posts.insert(b"a-missing", &p_missing).unwrap();
    posts.insert(b"b-dangling", &p_dangling).unwrap();
    posts.insert(b"c-null", &p_null).unwrap();
    posts
        .insert(
            b"d-hit",
            &map(&[("title", text("hit")), ("fk", text("u1"))]),
        )
        .unwrap();

    let rows = posts.join("authors", "fk").unwrap();
    // PINNED: the left-outer join keeps EVERY left row — a missing field
    // and a dangling reference are both simply `right: None`; only an
    // explicit `.filter(|r| r.right.is_some())` drops them.
    assert_eq!(
        keys(&rows),
        vec![
            b"a-missing".to_vec(),
            b"b-dangling".to_vec(),
            b"c-null".to_vec(),
            b"d-hit".to_vec()
        ]
    );
    assert_eq!(rows[0].left, p_missing);
    assert_eq!(rows[0].right, None);
    assert_eq!(rows[1].left, p_dangling);
    assert_eq!(rows[1].right, None);
    // A present Null is not a key kind: same left-outer miss.
    assert_eq!(rows[2].left, p_null);
    assert_eq!(rows[2].right, None);
    assert_eq!(
        rows[3].right,
        Some(map(&[("name", text("Ada"))])),
        "the real hit resolves"
    );
}

// ===========================================================================
// Foreign-key kinds
// ===========================================================================

#[test]
fn joins_foreign_key_kinds_text_bytes_int_and_unusable_shapes() {
    let db = Db::open_in_memory().unwrap();
    let right = db.collection("right");
    right
        .insert(b"u1", &map(&[("who", text("text-key"))]))
        .unwrap();
    right
        .insert(b"\x01\x02", &map(&[("who", text("bytes-key"))]))
        .unwrap();
    right
        .insert(b"5", &map(&[("who", text("decimal-key"))]))
        .unwrap();
    right
        .insert(b"", &map(&[("who", text("empty-key"))]))
        .unwrap();

    // Text and Bytes keys resolve by their raw bytes...
    let posts = db.collection("posts");
    posts.insert(b"t", &map(&[("fk", text("u1"))])).unwrap();
    posts
        .insert(b"b", &map(&[("fk", Value::Bytes(vec![1, 2]))]))
        .unwrap();
    // ...and Int keys by their DECIMAL string, so Int(5) and Text("5")
    // resolve the SAME right document.
    posts.insert(b"i", &map(&[("fk", Value::Int(5))])).unwrap();
    posts.insert(b"s", &map(&[("fk", text("5"))])).unwrap();
    // An empty Text is the empty key.
    posts.insert(b"e", &map(&[("fk", text(""))])).unwrap();
    // Every other kind is unusable: Float (even integral), Bool, Null,
    // and the containers.
    posts
        .insert(b"f", &map(&[("fk", Value::Float(5.0))]))
        .unwrap();
    posts
        .insert(b"o", &map(&[("fk", Value::Bool(true))]))
        .unwrap();
    posts.insert(b"n", &map(&[("fk", Value::Null)])).unwrap();
    posts
        .insert(b"a", &map(&[("fk", Value::Array(vec![Value::Int(5)]))]))
        .unwrap();
    posts
        .insert(b"m", &map(&[("fk", map(&[("k", Value::Int(5))]))]))
        .unwrap();
    posts
        .insert(b"v", &map(&[("fk", Value::Vector(vec![1.0]))]))
        .unwrap();

    let rows = posts.join("right", "fk").unwrap();
    let by_key = |k: &[u8]| rows.iter().find(|r| r.key == k).unwrap();
    assert_eq!(by_key(b"t").right, Some(map(&[("who", text("text-key"))])));
    assert_eq!(by_key(b"b").right, Some(map(&[("who", text("bytes-key"))])));
    // Int(5) and Text("5") both resolve the key b"5".
    assert_eq!(
        by_key(b"i").right,
        Some(map(&[("who", text("decimal-key"))]))
    );
    assert_eq!(
        by_key(b"s").right,
        Some(map(&[("who", text("decimal-key"))]))
    );
    assert_eq!(by_key(b"e").right, Some(map(&[("who", text("empty-key"))])));
    // PINNED: Float(5.0) does NOT resolve b"5" — only the three key kinds
    // convert; everything else is a left-outer miss.
    for k in [
        &b"f"[..],
        &b"o"[..],
        &b"n"[..],
        &b"a"[..],
        &b"m"[..],
        &b"v"[..],
    ] {
        assert_eq!(by_key(k).right, None, "key {k:?} must not resolve");
    }
}

// ===========================================================================
// Self-join
// ===========================================================================

#[test]
fn joins_self_join_references_within_one_collection() {
    let db = Db::open_in_memory().unwrap();
    let posts = db.collection("posts");
    posts
        .insert(
            b"p1",
            &map(&[("title", text("root")), ("next", text("p2"))]),
        )
        .unwrap();
    posts
        .insert(
            b"p2",
            &map(&[("title", text("middle")), ("next", text("p3"))]),
        )
        .unwrap();
    posts
        .insert(
            b"p3",
            &map(&[("title", text("leaf")), ("next", text("p1"))]),
        )
        .unwrap();
    // A self-referential cycle resolves: every next pointer is a key in the
    // SAME collection, so joining the collection against itself links each
    // row to its successor.
    let rows = posts.join("posts", "next").unwrap();
    assert_eq!(
        keys(&rows),
        vec![b"p1".to_vec(), b"p2".to_vec(), b"p3".to_vec()]
    );
    assert_eq!(
        rows[0].right,
        Some(map(&[("title", text("middle")), ("next", text("p3"))]))
    );
    assert_eq!(
        rows[1].right,
        Some(map(&[("title", text("leaf")), ("next", text("p1"))]))
    );
    assert_eq!(
        rows[2].right,
        Some(map(&[("title", text("root")), ("next", text("p2"))]))
    );
    // right IS a left document of the same collection: the cycle closes.
    assert_eq!(
        rows[2].right.as_ref().unwrap().get("title"),
        Some(&text("root"))
    );
}

// ===========================================================================
// Empty sides and unknown right collection
// ===========================================================================

#[test]
fn joins_empty_left_empty_right_and_unknown_right_collection() {
    let db = Db::open_in_memory().unwrap();
    db.collection("authors")
        .insert(b"u1", &map(&[("name", text("Ada"))]))
        .unwrap();

    // Empty LEFT: no rows at all.
    let rows = db.collection("posts").join("authors", "fk").unwrap();
    assert!(rows.is_empty());

    // Empty RIGHT: every row is retained with right: None.
    db.collection("posts")
        .insert(b"p1", &map(&[("fk", text("u1"))]))
        .unwrap();
    let rows = db.collection("posts").join("empty_authors", "fk").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, b"p1".to_vec());
    assert_eq!(rows[0].right, None);

    // PINNED: a right collection that does not exist is NOT an error —
    // every lookup misses, so all rows carry right: None.
    let rows = db.collection("posts").join("never_created", "fk").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].right, None);
}

// ===========================================================================
// Ordering
// ===========================================================================

#[test]
fn joins_rows_follow_left_collection_key_order() {
    let db = Db::open_in_memory().unwrap();
    let right = db.collection("right");
    right.insert(b"r", &map(&[("who", text("r"))])).unwrap();
    let left = db.collection("left");
    // Insert in scrambled order; the join output is sorted by the LEFT
    // collection's keys (byte order), independent of insertion order.
    for k in [&b"c"[..], &b"a"[..], &b"B"[..], &b"b"[..]] {
        left.insert(k, &map(&[("fk", text("r"))])).unwrap();
    }
    // Even when right-side keys would sort differently.
    let rows = left.join("right", "fk").unwrap();
    assert_eq!(
        keys(&rows),
        vec![b"B".to_vec(), b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
        "uppercase B (0x42) sorts before the lowercase letters"
    );
    assert!(rows.iter().all(|r| r.right.is_some()));
}

// ===========================================================================
// Mutations on both sides
// ===========================================================================

#[test]
fn joins_track_right_and_left_side_mutations() {
    let db = Db::open_in_memory().unwrap();
    let authors = db.collection("authors");
    let posts = db.collection("posts");
    posts
        .insert(
            b"p1",
            &map(&[("title", text("first")), ("author_id", text("u1"))]),
        )
        .unwrap();
    posts
        .insert(
            b"p2",
            &map(&[("title", text("second")), ("author_id", text("u2"))]),
        )
        .unwrap();

    // Initially u2 dangles.
    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].right, None);

    // Inserting the missing right document makes the dangling reference
    // resolve — the row set itself is unchanged.
    authors
        .insert(b"u2", &map(&[("name", text("Grace"))]))
        .unwrap();
    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].right, Some(map(&[("name", text("Grace"))])));

    // Deleting the right document re-dangles it (right back to None).
    authors.delete(b"u2").unwrap();
    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].right, None);

    // Left-side mutations change the ROW SET: a new post joins in key
    // order, and deleting a post removes its row.
    posts
        .insert(
            b"p0",
            &map(&[("title", text("zeroth")), ("author_id", text("u1"))]),
        )
        .unwrap();
    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(
        keys(&rows),
        vec![b"p0".to_vec(), b"p1".to_vec(), b"p2".to_vec()]
    );
    posts.delete(b"p1").unwrap();
    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(keys(&rows), vec![b"p0".to_vec(), b"p2".to_vec()]);
}

// ===========================================================================
// Non-map left documents
// ===========================================================================

#[test]
fn joins_non_map_left_documents_retained_with_none() {
    let db = Db::open_in_memory().unwrap();
    db.collection("authors")
        .insert(b"x", &map(&[("name", text("Ada"))]))
        .unwrap();
    let c = db.collection("things");
    // A scalar document has no path to walk, but it is still a left row.
    c.insert(b"scalar", &Value::Int(5)).unwrap();
    c.insert(b"array", &Value::Array(vec![Value::Int(1)]))
        .unwrap();
    // The document that IS a usable foreign key still cannot self-resolve:
    // the FK lookup reads a FIELD, never the document itself.
    c.insert(b"text", &text("x")).unwrap();

    let rows = c.join("authors", "fk").unwrap();
    assert_eq!(
        keys(&rows),
        vec![b"array".to_vec(), b"scalar".to_vec(), b"text".to_vec()]
    );
    assert!(rows.iter().all(|r| r.right.is_none()));
    assert_eq!(rows[2].left, text("x"));
}
