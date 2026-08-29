//! SELECT-shaping conformance (Task 5): projection, ordering, pagination,
//! and result-shape contracts of the query language, driven through the
//! public API only.
//!
//! Contract notes pinned by these tests (read from `src/builder.rs`,
//! `src/db.rs`, and `src/fusion.rs` first):
//! * `ResultRow` carries `key`, `score` (RRF fused score, exactly `0.0` for
//!   pure filter/scan queries, `1/(60+rank)` for a single ranked source),
//!   and `document` — there is no distance field (distances live on the
//!   direct-search `Hit` type, not on builder rows);
//! * `select` projects MAP documents to the named top-level/dotted paths
//!   (nested structure rebuilt, missing paths omitted, duplicate fields
//!   collapse via the map); an empty field list yields an empty map;
//!   non-map documents (scalars, arrays, ...) pass through UNCHANGED;
//!   ranking, filtering, and `score` still see/keep the full document;
//! * `order_by` sorts by CLASS first — comparable (int/float/text) values in
//!   value order, then pairwise-incomparable values (bools, bytes, nulls,
//!   vectors, arrays, maps, NaN) in kind-tag-then-key order (PINNED: NaN, a
//!   numeric kind, precedes the other incomparable kinds ascending and
//!   follows them descending), then rows MISSING the field — and
//!   `descending` reverses only the value order within the comparable
//!   class: the class order and the key tiebreak are FIXED, so incomparable
//!   and missing values sort last in BOTH directions;
//! * within the comparable class, cross-kind pairs (numbers vs texts) order
//!   by kind tag first — numbers before texts ascending, texts before
//!   numbers descending (the tag reverses with the value order) — so a
//!   mixed-type field groups deterministically by kind;
//! * `offset` is applied after ordering, before `limit`; `limit(0)` is the
//!   empty window, not "no limit";
//! * a filterless `order_by` over a scalar-indexed field is served by the
//!   index ORDER WALK (`PlanShape::SortIndex`): identical rows in the
//!   identical order as the scan path across the whole kind lattice —
//!   the walk only ever replaces the execution strategy, never the
//!   order contract above;
//! * `Page` keyset pagination: `next` is `None` on a short page and equals
//!   the LAST ROW's key on a full page (even when the following page turns
//!   out empty); `after` resumes strictly after the cursor, so
//!   `after = b""` skips exactly the empty key and nothing else; the page
//!   API orders by key only — it takes no `order_by`;
//! * `for_each_doc`'s closure returns `Result<bool>`: `Ok(false)` stops the
//!   walk early (a `Ok(true)` keeps going); the walk itself returns `Ok(())`.
//!
//! The smoke test that anchored the radar skeleton during Waves 1-2 is kept
//! as the first test below.

use std::collections::BTreeMap;

use corvid::db::Page;
use corvid::{Collection, Db, Metric, PlanShape, ResultRow, Value, field};

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

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> Collection<'a> {
    let c = db.collection(name);
    for (k, d) in docs {
        c.insert(k, d).unwrap();
    }
    c
}

/// The ordered keys of a query result — order-SENSITIVE (unlike filters.rs'
/// set assertions) because ordering IS the contract under test here.
fn row_keys(rows: &[ResultRow]) -> Vec<Vec<u8>> {
    rows.iter().map(|r| r.key.clone()).collect()
}

fn k(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|n| n.as_bytes().to_vec()).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn queries_smoke_order_by_limit_select_shapes_rows() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &map(&[("name", text("third")), ("n", Value::Int(3))]))
        .unwrap();
    c.insert(b"b", &map(&[("name", text("first")), ("n", Value::Int(1))]))
        .unwrap();
    c.insert(
        b"c",
        &map(&[("name", text("second")), ("n", Value::Int(2))]),
    )
    .unwrap();

    let rows = c
        .query()
        .select(["n"])
        .order_by("n", false)
        .limit(2)
        .run()
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].document.get("n"), Some(&Value::Int(1)));
    assert_eq!(rows[1].document.get("n"), Some(&Value::Int(2)));
    // Projection keeps only the selected top-level field.
    assert_eq!(rows[0].document.get("name"), None);
}

// ===========================================================================
// select — projection
// ===========================================================================

#[test]
fn queries_select_single_multiple_and_nested_dotted_paths() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "proj",
        &[(
            b"a",
            map(&[
                ("top", Value::Int(1)),
                ("other", text("kept-out")),
                (
                    "meta",
                    map(&[(
                        "author",
                        map(&[("name", text("ada")), ("age", Value::Int(36))]),
                    )]),
                ),
            ]),
        )],
    );

    // Single field.
    let rows = c.query().select(["top"]).run().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].document, map(&[("top", Value::Int(1))]));

    // Multiple fields: both kept, everything else dropped (order-free map).
    let rows = c.query().select(["other", "top"]).run().unwrap();
    assert_eq!(
        rows[0].document,
        map(&[("top", Value::Int(1)), ("other", text("kept-out"))])
    );

    // Nested dotted path: the projected structure is rebuilt NESTED, not
    // flattened, and the untouched sibling inside `meta` is gone.
    let rows = c.query().select(["meta.author.name", "top"]).run().unwrap();
    assert_eq!(
        rows[0].document,
        map(&[
            ("top", Value::Int(1)),
            ("meta", map(&[("author", map(&[("name", text("ada"))]))])),
        ])
    );
}

#[test]
fn queries_select_missing_fields_omitted_and_duplicates_collapse() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "proj",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("other", Value::Int(2))])),
        ],
    );

    // A field missing from ONE document is absent from that row only — the
    // row still appears (an empty projection, not an empty result).
    let rows = c.query().select(["n", "absent"]).run().unwrap();
    assert_eq!(rows[0].key, b"a".to_vec());
    assert_eq!(rows[0].document, map(&[("n", Value::Int(1))]));
    assert_eq!(rows[1].key, b"b".to_vec());
    assert_eq!(rows[1].document, Value::Map(BTreeMap::new()));

    // A duplicated field name projects once (the result is a map).
    let rows = c.query().select(["n", "n", "n"]).run().unwrap();
    assert_eq!(rows[0].document, map(&[("n", Value::Int(1))]));

    // A dotted path whose INTERMEDIATE segment is missing is omitted too.
    let rows = c.query().select(["meta.author"]).run().unwrap();
    assert_eq!(rows[0].document, Value::Map(BTreeMap::new()));
}

#[test]
fn queries_select_empty_field_list_yields_empty_map_for_map_docs() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "proj", &[(b"a", map(&[("n", Value::Int(1))]))]);

    let rows = c.query().select(Vec::<&str>::new()).run().unwrap();
    assert_eq!(rows.len(), 1);
    // Empty projection of a map document: an empty map, not the full doc
    // and not a dropped row.
    assert_eq!(rows[0].document, Value::Map(BTreeMap::new()));
}

#[test]
fn queries_select_non_map_documents_pass_through_unchanged() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "proj",
        &[
            (b"scalar", Value::Int(42)),
            (b"arr", Value::Array(vec![Value::Int(1), text("x")])),
            (b"null", Value::Null),
        ],
    );

    // Scalars, arrays, and nulls are not maps: `select` leaves the WHOLE
    // document in place, whatever the field list says. Rows come back in
    // key order ("arr" < "null" < "scalar").
    let rows = c.query().select(["n"]).run().unwrap();
    assert_eq!(rows[0].key, b"arr".to_vec());
    assert_eq!(
        rows[0].document,
        Value::Array(vec![Value::Int(1), text("x")])
    );
    assert_eq!(rows[1].key, b"null".to_vec());
    assert_eq!(rows[1].document, Value::Null);
    assert_eq!(rows[2].key, b"scalar".to_vec());
    assert_eq!(rows[2].document, Value::Int(42));
}

#[test]
fn queries_select_preserves_rank_scores_and_filter_visibility() {
    // Projection narrows only the RETURNED document: the FILTER still sees
    // the full document (below it matches on `kind`, which the projection
    // drops), the ranked field survives the projection intact, and the
    // fused `score` is untouched by `select`.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "proj",
        &[
            (
                b"a",
                map(&[("kind", text("keep")), ("e", Value::Vector(vec![0.5]))]),
            ),
            (
                b"b",
                map(&[("kind", text("drop")), ("e", Value::Vector(vec![3.0]))]),
            ),
        ],
    );

    let rows = c
        .query()
        .filter(field("kind").eq(text("keep")))
        .vector("e", vec![0.0], 10, Metric::L2)
        .select(["e"])
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["a"]));
    // The kind field was used for filtering but is gone from the projection.
    assert_eq!(rows[0].document, map(&[("e", Value::Vector(vec![0.5]))]));
    // Single ranked source at rank 1: the RRF fused score is 1/(60+1).
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32);
}

// ===========================================================================
// order_by — the class rule
// ===========================================================================

#[test]
fn queries_order_by_asc_desc_over_int_float_and_text() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "ord",
        &[
            (b"a", map(&[("n", Value::Int(30))])),
            (b"b", map(&[("n", Value::Float(2.5))])),
            (b"c", map(&[("n", Value::Int(10))])),
            (b"d", map(&[("n", Value::Float(-7.0))])),
            (b"e", map(&[("n", Value::Int(10))])), // duplicate value: key tiebreak
        ],
    );

    // Ascending: Int and Float interop through one numeric order; equal
    // values tie-break by key ascending (c before e).
    let rows = c.query().order_by("n", false).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["d", "b", "c", "e", "a"]));

    // Descending: value order reversed, key tiebreak still ascending.
    let rows = c.query().order_by("n", true).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["a", "c", "e", "b", "d"]));

    // Text orders lexically (UTF-8 byte order: 'A'(0x41) < 'a'(0x61) <
    // 'b'(0x62) < 'é'(0xC3..)), both directions.
    let c2 = seed(
        &db,
        "ordt",
        &[
            (b"a", map(&[("t", text("banana"))])),
            (b"b", map(&[("t", text("Apple"))])),
            (b"c", map(&[("t", text("apple"))])),
            (b"d", map(&[("t", text("élan"))])),
        ],
    );
    let rows = c2.query().order_by("t", false).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["b", "c", "a", "d"]));
    let rows = c2.query().order_by("t", true).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["d", "a", "c", "b"]));
}

#[test]
fn queries_order_by_class_rule_incomparable_then_missing_last_both_directions() {
    // Class order (audit C4): comparable (0) < incomparable (1: bools,
    // bytes, nulls, vectors, arrays, maps, NaN) < missing (2). The class
    // order is FIXED under descending — only the value order inside the
    // comparable class reverses — so incomparable and missing rows sort
    // last in BOTH directions.
    //
    // PINNED ACTUAL NUANCE: within the incomparable class, the same kind
    // tag still orders the pair — NaN (a NUMERIC kind, tag 0) sorts before
    // every other incomparable kind ascending, and after them descending
    // (the tag reverses with the value order); the remaining kinds are
    // mutually unordered and fall to key order. The `order_by` doc's
    // "in key order" shorthand elides this; the comparator is a
    // deterministic total order either way.
    let db = Db::open_in_memory().unwrap();
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("x", Value::Int(3))])),
        (b"b", map(&[("x", Value::Bool(true))])),
        (b"c", map(&[("x", Value::Bool(false))])),
        (b"d", map(&[("x", Value::Float(f64::NAN))])),
        (b"e", map(&[("x", Value::Array(vec![Value::Int(1)]))])),
        (b"f", map(&[("x", Value::Bytes(vec![9]))])),
        (b"g", map(&[("x", Value::Vector(vec![1.0]))])),
        (b"h", map(&[("x", Value::Null)])),
        (b"i", map(&[("x", map(&[("k", Value::Int(1))]))])),
        (b"j", map(&[("other", Value::Int(0))])), // missing x
    ];
    let c = seed(&db, "ord", &docs);

    // Ascending: the only comparable value, then NaN (numeric kind tag)
    // ahead of the other incomparable kinds, those in KEY order (no
    // false < true, no container comparison), then the missing row.
    let rows = c.query().order_by("x", false).run().unwrap();
    assert_eq!(
        row_keys(&rows),
        k(&["a", "d", "b", "c", "e", "f", "g", "h", "i", "j"])
    );

    // Descending: class order unchanged; the kind tag reversed inside the
    // incomparable class puts NaN LAST among the incomparables; the
    // mutually-unordered kinds stay in key order; missing still last.
    let rows = c.query().order_by("x", true).run().unwrap();
    assert_eq!(
        row_keys(&rows),
        k(&["a", "b", "c", "e", "f", "g", "h", "i", "d", "j"])
    );

    // A richer comparable group makes the fixed-class behavior visible:
    // descending reverses the numbers, yet missing still sorts LAST.
    let c2 = seed(
        &db,
        "ord2",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Bool(true))])),
            (b"d", map(&[])),
        ],
    );
    let rows = c2.query().order_by("n", true).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["b", "a", "c", "d"]));
}

#[test]
fn queries_order_by_mixed_kind_field_groups_numbers_before_texts() {
    // Cross-kind pairs inside the comparable class order by kind tag FIRST
    // (numbers before texts), then by value — a deterministic total order.
    // `descending` reverses the whole within-class order INCLUDING the tag,
    // so texts come before numbers.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "ord",
        &[
            (b"a", map(&[("x", text("2"))])),
            (b"b", map(&[("x", Value::Int(10))])),
            (b"c", map(&[("x", Value::Float(1.0))])),
            (b"d", map(&[("x", text("10"))])),
            (b"e", map(&[("x", Value::Int(2))])),
        ],
    );

    let rows = c.query().order_by("x", false).run().unwrap();
    // Ascending: 1.0, 2, 10, then texts LEXICALLY ("10" < "2").
    assert_eq!(row_keys(&rows), k(&["c", "e", "b", "d", "a"]));

    let rows = c.query().order_by("x", true).run().unwrap();
    // Descending: texts first ("2" > "10" lexically), then numbers reversed.
    assert_eq!(row_keys(&rows), k(&["a", "d", "b", "e", "c"]));
}

// ===========================================================================
// order_by × pagination, filters, and index-vs-scan equivalence
// ===========================================================================

#[test]
fn queries_order_by_limit_offset_window_after_ordering() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "ord",
        &[
            (b"a", map(&[("n", Value::Int(5))])),
            (b"b", map(&[("n", Value::Int(3))])),
            (b"c", map(&[("n", Value::Int(4))])),
            (b"d", map(&[("n", Value::Int(1))])),
            (b"e", map(&[("n", Value::Int(2))])),
        ],
    );

    // offset is applied to the ORDERED list, before limit: rows 2..4 of the
    // ascending order (2, 3, 4).
    let rows = c
        .query()
        .order_by("n", false)
        .offset(1)
        .limit(3)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["e", "b", "c"]));

    // The same window of the descending order (4, 3, 2).
    let rows = c
        .query()
        .order_by("n", true)
        .offset(1)
        .limit(3)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["c", "b", "e"]));

    // offset == length: empty (clamped, not an error); offset beyond: empty.
    assert!(
        c.query()
            .order_by("n", false)
            .offset(5)
            .run()
            .unwrap()
            .is_empty()
    );
    assert!(
        c.query()
            .order_by("n", false)
            .offset(50)
            .run()
            .unwrap()
            .is_empty()
    );

    // limit(0) on an ordered query is the empty window.
    assert!(
        c.query()
            .order_by("n", false)
            .limit(0)
            .run()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn queries_order_by_with_filters_orders_only_matches() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "ord",
        &[
            (b"a", map(&[("cat", text("x")), ("n", Value::Int(1))])),
            (b"b", map(&[("cat", text("y")), ("n", Value::Int(2))])),
            (b"c", map(&[("cat", text("x")), ("n", Value::Int(3))])),
            (b"d", map(&[("cat", text("x"))])), // matches filter, missing n
        ],
    );

    // Only filter matches are ordered; the missing-n match still sorts last.
    let rows = c
        .query()
        .filter(field("cat").eq(text("x")))
        .order_by("n", false)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["a", "c", "d"]));

    // ...and stays last under descending too (fixed class order).
    let rows = c
        .query()
        .filter(field("cat").eq(text("x")))
        .order_by("n", true)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["c", "a", "d"]));
}

#[test]
fn queries_order_by_indexed_vs_scan_equivalent() {
    // Twin-DB harness: the same corpus and the same ordered queries, once
    // over a scan and once with a scalar index on the ordering field.
    //
    // The unfiltered order_by is served by the SORT-INDEX walk (the index
    // enumerates the comparable class in the total-order contract's order);
    // with a filter that drives the index, the ordering is applied ON TOP of
    // the index-window results — both identical to the scan side.
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("n", Value::Int(3)), ("cat", text("x"))])),
        (b"b", map(&[("n", Value::Int(7)), ("cat", text("x"))])),
        (b"c", map(&[("n", Value::Int(1)), ("cat", text("y"))])),
        (b"d", map(&[("cat", text("x"))])),
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "ord", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "ord", &docs);
    idx.create_scalar_index("n").unwrap();

    // Unfiltered order_by over the indexed field plans as the sort-index
    // walk (the scan side still plans as Scan).
    assert_eq!(
        idx.query().order_by("n", false).plan_shape(),
        PlanShape::SortIndex {
            field: "n".to_owned()
        }
    );
    assert_eq!(
        scan.query().order_by("n", false).plan_shape(),
        PlanShape::Scan {
            collection: "ord".to_owned()
        }
    );
    // A filter on the INDEXED field still drives the window (the filtered
    // query declines the sort-index walk).
    let window = field("n").ge(Value::Int(0)); // matches a, b, c (d misses n)
    assert_eq!(
        idx.query().filter(window.clone()).plan_shape(),
        PlanShape::IndexedWindow { kind: "scalar" }
    );

    for descending in [false, true] {
        // Unfiltered: identical full order both sides.
        let want = row_keys(&scan.query().order_by("n", descending).run().unwrap());
        assert_eq!(
            row_keys(&idx.query().order_by("n", descending).run().unwrap()),
            want,
            "unfiltered desc={descending}"
        );
        // Filtered (index-driven on the idx side): identical order.
        let want = row_keys(
            &scan
                .query()
                .filter(window.clone())
                .order_by("n", descending)
                .run()
                .unwrap(),
        );
        assert_eq!(
            row_keys(
                &idx.query()
                    .filter(window.clone())
                    .order_by("n", descending)
                    .run()
                    .unwrap()
            ),
            want,
            "filtered desc={descending}"
        );
        // ...and with an offset/limit window over the order.
        let want = row_keys(
            &scan
                .query()
                .filter(window.clone())
                .order_by("n", descending)
                .offset(1)
                .limit(2)
                .run()
                .unwrap(),
        );
        assert_eq!(
            row_keys(
                &idx.query()
                    .filter(window.clone())
                    .order_by("n", descending)
                    .offset(1)
                    .limit(2)
                    .run()
                    .unwrap()
            ),
            want,
            "windowed desc={descending}"
        );
    }
}

/// Run one (scan, indexed) twin pair through the same ordered query shape
/// and assert identical rows: `None` limit = no limit; `(off, lim)` = an
/// offset+limit window over the ordered rows.
fn assert_order_parity(
    scan: &Collection<'_>,
    idx: &Collection<'_>,
    field: &str,
    descending: bool,
    window: Option<(usize, Option<usize>)>,
    label: &str,
) {
    let run = |c: &Collection<'_>| {
        let mut q = c.query().order_by(field, descending);
        if let Some((off, lim)) = window {
            q = q.offset(off);
            if let Some(l) = lim {
                q = q.limit(l);
            }
        }
        row_keys(&q.run().unwrap())
    };
    let want = run(scan);
    assert_eq!(
        run(idx),
        want,
        "{label} desc={descending} window={window:?}"
    );
}

/// The sort-index walk must be byte-identical to the scan path across the
/// full kind lattice: all-numeric (int/float interop, ±0.0, negatives),
/// numeric+text mixed (cross-kind tag order), some-missing, and
/// some-incomparable (bools, NaN, containers) — both directions, full
/// order and every window shape, including windows that reach PAST the
/// comparable set into the tail (the exhaustion fallback).
#[test]
fn queries_order_by_index_walk_parity_across_kind_lattice() {
    // All-numeric: int/float interop (2 == 2.0), negatives, ±0.0 (equal —
    // key tiebreak), and an f64-precision pair (2^53 vs 2^53+1 share one
    // f64 in the numeric lane; the contract orders them EXACTLY, so the
    // walk must re-sort the shared encoding bucket, not trust doc-key
    // order). Keys are chosen so doc-key order OPPOSES every value order
    // (z holds the smallest value, a the largest) — any order leak shows.
    let numeric: Vec<(&[u8], Value)> = vec![
        (b"z1", map(&[("x", Value::Int(i64::MIN))])),
        (b"y2", map(&[("x", Value::Float(-1.5))])),
        (b"x3", map(&[("x", Value::Float(-0.0))])),
        (b"w4", map(&[("x", Value::Int(0))])),
        (b"v5", map(&[("x", Value::Float(0.0))])),
        (b"u6", map(&[("x", Value::Int(2))])),
        (b"t7", map(&[("x", Value::Float(2.0))])),
        (b"s8", map(&[("x", Value::Float(2.5))])),
        (b"r9", map(&[("x", Value::Int(1 << 53))])),
        (b"q0", map(&[("x", Value::Int((1 << 53) + 1))])),
        (b"pa", map(&[("x", Value::Int(i64::MAX))])),
    ];
    // Numeric + text mixed: comparable class orders numbers (tag 0)
    // before texts (tag 1) ascending, and REVERSES the tag descending —
    // texts first, then numbers, each reversed by value.
    let mixed: Vec<(&[u8], Value)> = vec![
        (b"e1", map(&[("x", Value::Text("rust".into()))])),
        (b"d2", map(&[("x", Value::Int(9))])),
        (b"c3", map(&[("x", Value::Text("ada".into()))])),
        (b"b4", map(&[("x", Value::Text("ada".into()))])), // text tie → key
        (b"a5", map(&[("x", Value::Float(-2.0))])),
    ];
    // Missing + incomparable: bools and NaN are INDEXED but incomparable
    // (class 1) — the walk must skip them; arrays/null/missing are not
    // indexed at all. Class order is FIXED: comparable, then class 1
    // (NaN — a numeric kind — before the other incomparables ascending,
    // after them descending; key order within), then missing (key order).
    let lattice: Vec<(&[u8], Value)> = vec![
        (b"k1", map(&[("x", Value::Bool(true))])),
        (b"j2", map(&[("x", Value::Float(f64::NAN))])),
        (b"i3", map(&[("x", Value::Array(vec![Value::Int(1)]))])),
        (b"h4", map(&[("x", Value::Null)])),
        (b"g5", map(&[("other", Value::Int(1))])), // x missing
        (b"f6", map(&[("x", Value::Text("zed".into()))])),
        (b"e7", map(&[("x", Value::Int(-3))])),
        (b"d8", Value::Int(42)), // non-map: x missing
        (b"c9", map(&[("x", Value::Int(7))])),
        (b"b0", map(&[("x", Value::Float(f64::NEG_INFINITY))])),
        (b"aa", map(&[("x", Value::Float(f64::INFINITY))])),
    ];
    // All-tail: NO comparable docs at all — the walk exhausts immediately
    // and the whole result comes from the tail scan (the exhaustion
    // fallback's degenerate arm).
    let all_tail: Vec<(&[u8], Value)> = vec![
        (b"c1", map(&[("x", Value::Bool(false))])),
        (b"b2", map(&[("x", Value::Float(f64::NAN))])),
        (b"a3", map(&[("other", Value::Int(1))])), // x missing
    ];

    for (label, docs) in [
        ("numeric", &numeric),
        ("mixed", &mixed),
        ("lattice", &lattice),
        ("all-tail", &all_tail),
    ] {
        let scan_db = Db::open_in_memory().unwrap();
        let scan = seed(&scan_db, "lat", docs);
        let idx_db = Db::open_in_memory().unwrap();
        let idx = seed(&idx_db, "lat", docs);
        idx.create_scalar_index("x").unwrap();
        // The indexed side really takes the walk (not a silent decline).
        assert_eq!(
            idx.query().order_by("x", false).plan_shape(),
            PlanShape::SortIndex {
                field: "x".to_owned()
            },
            "{label}: expected the sort-index arm"
        );

        for descending in [false, true] {
            // Full order (no window) and offset-only.
            assert_order_parity(&scan, &idx, "x", descending, None, label);
            assert_order_parity(&scan, &idx, "x", descending, Some((2, None)), label);
            // Windows: inside the comparable prefix, straddling the
            // comparable/tail boundary, and fully past the comparable set
            // (offset ≥ comparable count → exhaustion + pure tail rows).
            for (off, lim) in [
                (0, 1),
                (0, 3),
                (1, 2),
                (2, 100),
                (3, 4),
                (docs.len(), 3), // offset == corpus: empty
                (docs.len() + 5, 2),
            ] {
                assert_order_parity(&scan, &idx, "x", descending, Some((off, Some(lim))), label);
            }
            // limit(0): the empty window on both paths.
            assert_order_parity(&scan, &idx, "x", descending, Some((1, Some(0))), label);
        }
    }
}

/// The exhaustion fallback composes with INSERTS and DELETES between query
/// and query: the walk (index) and the tail (scan of non-comparable docs)
/// both read one snapshot, so the twin stays identical while the corpus
/// churns — including when churn moves docs between the comparable class
/// and the tail.
#[test]
fn queries_order_by_index_walk_parity_under_updates() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("x", Value::Int(5))])),
        (b"b", map(&[("x", Value::Text("m".into()))])),
        (b"c", map(&[("x", Value::Bool(false))])), // tail (incomparable)
        (b"d", map(&[("other", Value::Int(1))])),  // tail (missing)
    ];
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "upd", &docs);
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "upd", &docs);
    idx.create_scalar_index("x").unwrap();
    assert_order_parity(&scan, &idx, "x", false, Some((0, Some(10))), "initial");

    // a: 5 → NaN (comparable → incomparable: moves to the tail).
    idx.insert(b"a", &map(&[("x", Value::Float(f64::NAN))]))
        .unwrap();
    scan.insert(b"a", &map(&[("x", Value::Float(f64::NAN))]))
        .unwrap();
    // c: false → 1 (incomparable → comparable: joins the walk).
    idx.insert(b"c", &map(&[("x", Value::Int(1))])).unwrap();
    scan.insert(b"c", &map(&[("x", Value::Int(1))])).unwrap();
    // d: deleted (a tail row disappears).
    idx.delete(b"d").unwrap();
    scan.delete(b"d").unwrap();
    // e: new comparable row.
    idx.insert(b"e", &map(&[("x", Value::Int(-7))])).unwrap();
    scan.insert(b"e", &map(&[("x", Value::Int(-7))])).unwrap();

    for descending in [false, true] {
        assert_order_parity(&scan, &idx, "x", descending, None, "post-update");
        assert_order_parity(
            &scan,
            &idx,
            "x",
            descending,
            Some((1, Some(2))),
            "post-update",
        );
        assert_order_parity(
            &scan,
            &idx,
            "x",
            descending,
            Some((3, Some(2))),
            "post-update",
        );
    }
}

/// Decline rules: the sort-index walk only serves a filterless, sourceless
/// order_by over a COMPLETE scalar index on the ordering field. Everything
/// else keeps its existing plan — and stays row-identical to a scan twin.
#[test]
fn queries_order_by_index_walk_declines_documented_shapes() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", map(&[("n", Value::Int(3)), ("cat", text("x"))])),
        (b"b", map(&[("n", Value::Int(7)), ("cat", text("y"))])),
        (b"c", map(&[("cat", text("x"))])),
    ];
    let idx_db = Db::open_in_memory().unwrap();
    let idx = seed(&idx_db, "dec", &docs);
    idx.create_scalar_index("n").unwrap();
    idx.create_scalar_index("cat").unwrap();
    idx.create_text_index("cat").unwrap();

    // Filtered order_by on the indexed field: the scalar WINDOW drives the
    // candidates (ordering on top), not the walk.
    let q = idx
        .query()
        .filter(field("n").ge(Value::Int(0)))
        .order_by("n", false);
    assert_eq!(q.plan_shape(), PlanShape::IndexedWindow { kind: "scalar" });
    // order_by over an UNindexed field: no walk to take.
    assert_eq!(
        idx.query().order_by("other", false).plan_shape(),
        PlanShape::Scan {
            collection: "dec".to_owned()
        }
    );
    // Any retrieval source: rank order is the contract; the walk declines
    // (single text source with a text index here).
    let q = idx.query().text("cat", "x", 10).order_by("n", false);
    assert_eq!(
        q.plan_shape(),
        PlanShape::TextIndex {
            field: "cat".to_owned()
        }
    );

    // The filtered shape is still row-identical to a scan twin.
    let scan_db = Db::open_in_memory().unwrap();
    let scan = seed(&scan_db, "dec", &docs);
    for descending in [false, true] {
        let run = |c: &Collection<'_>| {
            row_keys(
                &c.query()
                    .filter(field("n").ge(Value::Int(0)))
                    .order_by("n", descending)
                    .run()
                    .unwrap(),
            )
        };
        assert_eq!(run(&scan), run(&idx), "filtered desc={descending}");
    }
}

// ===========================================================================
// limit / offset boundaries (order-free)
// ===========================================================================

#[test]
fn queries_limit_zero_one_exact_and_over_match_count() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "lim",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Int(3))])),
        ],
    );

    // limit(0) is the EMPTY window (not "no limit").
    assert_eq!(c.query().limit(0).run().unwrap(), Vec::<ResultRow>::new());
    // limit(1): the first row in key order.
    assert_eq!(row_keys(&c.query().limit(1).run().unwrap()), k(&["a"]));
    // limit == matches: everything.
    assert_eq!(
        row_keys(&c.query().limit(3).run().unwrap()),
        k(&["a", "b", "c"])
    );
    // limit > matches: everything, no error.
    assert_eq!(
        row_keys(&c.query().limit(99).run().unwrap()),
        k(&["a", "b", "c"])
    );
}

#[test]
fn queries_offset_boundaries_and_full_range_pagination_loop() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "off",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Int(3))])),
            (b"d", map(&[("n", Value::Int(4))])),
        ],
    );

    // offset(0) is a no-op; offset WITHOUT limit returns the whole tail.
    assert_eq!(
        row_keys(&c.query().offset(0).run().unwrap()),
        k(&["a", "b", "c", "d"])
    );
    assert_eq!(
        row_keys(&c.query().offset(2).run().unwrap()),
        k(&["c", "d"])
    );
    // offset == len: empty; offset > len: clamped to empty, no error.
    assert!(c.query().offset(4).run().unwrap().is_empty());
    assert!(c.query().offset(40).run().unwrap().is_empty());

    // Paginate the whole range with a loop: offset+limit windows must
    // tile the key order exactly, with a final short (here: empty) page.
    let mut seen = Vec::new();
    let mut off = 0usize;
    loop {
        let page = c.query().offset(off).limit(2).run().unwrap();
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().map(|r| r.key.clone()));
        off += 2;
    }
    assert_eq!(seen, k(&["a", "b", "c", "d"]));
}

#[test]
fn queries_limit_offset_on_empty_collection() {
    let db = Db::open_in_memory().unwrap();
    let ghost = db.collection("ghosts");
    assert!(ghost.query().run().unwrap().is_empty());
    assert!(ghost.query().limit(5).run().unwrap().is_empty());
    assert!(ghost.query().offset(5).run().unwrap().is_empty());
    assert!(ghost.query().limit(5).offset(5).run().unwrap().is_empty());
}

// ===========================================================================
// page / page_where — keyset pagination
// ===========================================================================

#[test]
fn queries_page_cursor_semantics() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "pg",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Int(3))])),
            (b"d", map(&[("n", Value::Int(4))])),
        ],
    );

    // limit(0): the empty page, with NO cursor (not even a start cursor).
    let p = c.page(None, 0).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );

    // after=None: the first N rows in key order; next is the LAST ROW's key
    // on a full page (strictly-greater resume).
    let p = c.page(None, 2).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["a", "b"])
    );
    assert_eq!(p.rows[0].1.get("n"), Some(&Value::Int(1))); // rows are (key, doc)
    assert_eq!(p.next, Some(b"b".to_vec()));

    // after=first key: resumes strictly after it.
    let p = c.page(Some(b"a"), 2).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["b", "c"])
    );
    assert_eq!(p.next, Some(b"c".to_vec()));

    // after=LAST key: nothing remains, next=None.
    let p = c.page(Some(b"d"), 2).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );

    // after PAST the end: same empty page.
    let p = c.page(Some(b"zzz"), 2).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );

    // A page whose size exactly exhausts the collection still yields a
    // cursor — the NEXT page is what proves the end (PINNED: a full page
    // always has `next`, even when the next page turns out empty).
    let p = c.page(None, 4).unwrap();
    assert_eq!(p.rows.len(), 4);
    assert_eq!(p.next, Some(b"d".to_vec()));
    let p = c.page(p.next.as_deref(), 4).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );

    // limit(1) walks one row per page.
    let p = c.page(None, 1).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["a"])
    );
    assert_eq!(p.next, Some(b"a".to_vec()));
}

#[test]
fn queries_page_after_empty_bytes_skips_only_the_empty_key() {
    // `after` resumes STRICTLY after the cursor. None starts at the very
    // beginning (the empty key is the first key in key order); after=b""
    // resumes after the empty key — which skips exactly that one row, the
    // empty key itself, and nothing else.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "pg",
        &[
            (b"", map(&[("n", Value::Int(0))])),
            (b"a", map(&[("n", Value::Int(1))])),
        ],
    );

    let p = c.page(None, 10).unwrap();
    let mut keys: Vec<Vec<u8>> = p.rows.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(keys.remove(0), Vec::<u8>::new()); // empty key IS the first key
    assert_eq!(keys, k(&["a"]));

    let p = c.page(Some(b""), 10).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["a"])
    );
    assert_eq!(p.next, None); // short page: no cursor
}

#[test]
fn queries_page_where_predicate_and_full_walk() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "pgw",
        &[
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
            (b"c", map(&[("n", Value::Int(3))])),
            (b"d", map(&[("n", Value::Int(4))])),
            (b"e", map(&[("n", Value::Int(5))])),
        ],
    );
    let even = field("n").is_in([Value::Int(2), Value::Int(4)]);

    // Same cursor edges as page(): first page, resume, past-end, limit 0.
    let p = c.page_where(None, 1, even.clone()).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["b"])
    );
    assert_eq!(p.next, Some(b"b".to_vec()));

    let p = c.page_where(p.next.as_deref(), 5, even.clone()).unwrap();
    assert_eq!(
        p.rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        k(&["d"])
    );
    assert_eq!(p.next, None); // short page ends the walk

    let p = c.page_where(Some(b"e"), 5, even.clone()).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );
    let p = c.page_where(None, 0, even.clone()).unwrap();
    assert_eq!(
        p,
        Page {
            rows: Vec::new(),
            next: None
        }
    );

    // Full walk: every matching document exactly once, in key order, with a
    // loop over the cursor.
    let mut seen = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let p = c.page_where(after.as_deref(), 2, even.clone()).unwrap();
        seen.extend(p.rows.iter().map(|(k, _)| k.clone()));
        match p.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, k(&["b", "d"]));
}

#[test]
fn queries_page_over_empty_collection_yields_empty_page_with_no_cursor() {
    let db = Db::open_in_memory().unwrap();
    let ghost = db.collection("ghosts");
    assert_eq!(
        ghost.page(None, 5).unwrap(),
        Page {
            rows: Vec::new(),
            next: None
        }
    );
    assert_eq!(
        ghost.page_where(None, 5, field("x").exists()).unwrap(),
        Page {
            rows: Vec::new(),
            next: None
        }
    );
}

/// End-to-end multi-chunk pin (Task 12 review): `page`/`page_where` walk a
/// corpus past the 1024-key chunk boundary of `page_inner`'s `scan_from`
/// loop — 2503 keys forces at least three chunks per FULL walk — and the
/// cursor-resumed page sequence yields every key exactly once, in key
/// order, for both entry points. The store-level chunked-walk semantics
/// are pinned in store.rs; this pins the public page contract on top of a
/// corpus where a single page cannot be served by one chunk read.
#[test]
fn queries_page_walks_multi_chunk_corpus_end_to_end() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("big");
    let mut docs = Vec::new();
    for i in 0..2503 {
        docs.push((
            format!("k{i:05}").into_bytes(),
            map(&[("n", Value::Int(i as i64))]),
        ));
    }
    for (key, doc) in &docs {
        c.insert(key, doc).unwrap();
    }
    let want: Vec<Vec<u8>> = docs.iter().map(|(k, _)| k.clone()).collect();

    // page: full cursor walk over pages that each span several chunks.
    let mut seen = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let p = c.page(after.as_deref(), 1000).unwrap();
        assert!(p.rows.len() == 1000 || p.next.is_none());
        seen.extend(p.rows.iter().map(|(k, _)| k.clone()));
        match p.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, want);

    // page_where with a half-matching predicate: the same walk visits the
    // same chunks but keeps only the matches — every match exactly once.
    let even = field("n").gt(Value::Int(1250));
    let want_even: Vec<Vec<u8>> = docs
        .iter()
        .filter(|(_, d)| matches!(d.get("n"), Some(Value::Int(n)) if *n > 1250))
        .map(|(k, _)| k.clone())
        .collect();
    let mut seen = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let p = c.page_where(after.as_deref(), 700, even.clone()).unwrap();
        seen.extend(p.rows.iter().map(|(k, _)| k.clone()));
        match p.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, want_even);
}

// ===========================================================================
// scan / for_each_doc
// ===========================================================================

#[test]
fn queries_scan_returns_pairs_in_key_order() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "scn",
        &[
            (b"c", map(&[("n", Value::Int(3))])),
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
        ],
    );
    // Insertion order above is deliberately shuffled.
    let all = c.scan().unwrap();
    let keys: Vec<Vec<u8>> = all.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(keys, k(&["a", "b", "c"]));
    // Pairs carry the decoded documents.
    assert_eq!(all[0], (b"a".to_vec(), map(&[("n", Value::Int(1))])));
}

#[test]
fn queries_for_each_doc_visits_key_order_and_early_stops_on_false() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "fe",
        &[
            (b"c", map(&[("n", Value::Int(3))])),
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
        ],
    );

    // Full walk: key order, decoded documents, Ok(()) overall.
    let mut seen: Vec<(Vec<u8>, i64)> = Vec::new();
    c.for_each_doc(|key, doc| {
        seen.push((key.to_vec(), doc.get("n").and_then(Value::as_int).unwrap()));
        Ok(true)
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![(b"a".to_vec(), 1), (b"b".to_vec(), 2), (b"c".to_vec(), 3),]
    );

    // The closure returns Result<bool>: Ok(false) STOPS the walk early —
    // visits before the stop are kept, the walk itself still returns Ok(()).
    let mut visited = 0usize;
    c.for_each_doc(|_, _| {
        visited += 1;
        Ok(visited < 2) // stop after the second document
    })
    .unwrap();
    assert_eq!(visited, 2);
}

#[test]
fn queries_for_each_doc_on_empty_collection_visits_nothing() {
    let db = Db::open_in_memory().unwrap();
    let ghost = db.collection("ghosts");
    let mut visits = 0;
    ghost
        .for_each_doc(|_, _| {
            visits += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(visits, 0);
}

// ===========================================================================
// len / is_empty / count
// ===========================================================================

#[test]
fn queries_len_and_is_empty_boundaries() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("cnt");
    // Empty collection: len 0, is_empty true.
    assert_eq!(c.len().unwrap(), 0);
    assert!(c.is_empty().unwrap());
    // One document: the boundary flips.
    c.insert(b"a", &map(&[("n", Value::Int(1))])).unwrap();
    assert_eq!(c.len().unwrap(), 1);
    assert!(!c.is_empty().unwrap());
    c.insert(b"b", &map(&[("n", Value::Int(2))])).unwrap();
    assert_eq!(c.len().unwrap(), 2);
    c.delete(b"a").unwrap();
    c.delete(b"b").unwrap();
    assert_eq!(c.len().unwrap(), 0);
    assert!(c.is_empty().unwrap());
}

#[test]
fn queries_count_with_filter_and_after_mutations() {
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

    // No filter: count == len (the O(1) counter).
    assert_eq!(c.query().count().unwrap(), 3);
    // With a filter: the filtered cardinality.
    assert_eq!(
        c.query()
            .filter(field("cat").eq(text("x")))
            .count()
            .unwrap(),
        2
    );
    // Shaping is IGNORED by count: limit/offset/select/order_by do not
    // narrow it (it is an aggregate over the filtered set).
    assert_eq!(
        c.query()
            .filter(field("cat").eq(text("x")))
            .limit(1)
            .offset(1)
            .select(["n"])
            .order_by("n", true)
            .count()
            .unwrap(),
        2
    );

    // count tracks mutations.
    c.insert(b"d", &map(&[("cat", text("x")), ("n", Value::Int(4))]))
        .unwrap();
    assert_eq!(
        c.query()
            .filter(field("cat").eq(text("x")))
            .count()
            .unwrap(),
        3
    );
    c.delete(b"a").unwrap();
    assert_eq!(
        c.query()
            .filter(field("cat").eq(text("x")))
            .count()
            .unwrap(),
        2
    );
    // On an empty collection: 0, matching len.
    assert_eq!(db.collection("ghosts").query().count().unwrap(), 0);
}

// ===========================================================================
// run() basics
// ===========================================================================

#[test]
fn queries_run_on_empty_collection_returns_empty_vec() {
    let db = Db::open_in_memory().unwrap();
    let rows = db.collection("ghosts").query().run().unwrap();
    assert!(rows.is_empty());
}

#[test]
fn queries_run_select_only_returns_all_in_key_order() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "all",
        &[
            (b"c", map(&[("n", Value::Int(3))])),
            (b"a", map(&[("n", Value::Int(1))])),
            (b"b", map(&[("n", Value::Int(2))])),
        ],
    );
    // No filter, no ranking: every document, in key order, full documents.
    let rows = c.query().select(["n"]).run().unwrap();
    assert_eq!(row_keys(&rows), k(&["a", "b", "c"]));
    assert_eq!(rows[0].document, map(&[("n", Value::Int(1))]));
    assert!(rows.iter().all(|r| r.score == 0.0));
}

// ===========================================================================
// explain() / plan_shape() — the public observability contract
// ===========================================================================

#[test]
fn queries_plan_shape_ann_index_for_single_vector_source() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "vec",
        &[(b"a", map(&[("e", Value::Vector(vec![1.0, 0.0]))]))],
    );
    c.create_vector_index("e", Metric::Cosine).unwrap();

    let q = c.query().vector("e", vec![1.0, 0.0], 10, Metric::Cosine);
    assert_eq!(
        q.plan_shape(),
        PlanShape::AnnIndex {
            field: "e".to_owned()
        }
    );
    // The explain head names the ANN index family.
    let explained = q.explain();
    assert!(explained.starts_with("ann(e)"), "got: {explained}");

    // A metric mismatch (index is Cosine, query is L2) is not consultable:
    // falls through to the streaming top-k arm.
    let q = c.query().vector("e", vec![1.0, 0.0], 10, Metric::L2);
    assert_eq!(q.plan_shape(), PlanShape::StreamingTopK);
    assert!(q.explain().starts_with("streaming-topk"));

    // A FILTERED vector query declines the ANN index unless approx is set —
    // but it is still a single vector source, so it falls to the EXACT
    // streaming top-k arm (not a scan: streaming filters while it ranks).
    let q = c
        .query()
        .filter(field("e").exists())
        .vector("e", vec![1.0, 0.0], 10, Metric::Cosine);
    assert_eq!(q.plan_shape(), PlanShape::StreamingTopK);
    let q = q.approx();
    assert_eq!(
        q.plan_shape(),
        PlanShape::AnnIndex {
            field: "e".to_owned()
        }
    );
    assert!(q.explain().contains("approx"), "got: {}", q.explain());
}

#[test]
fn queries_plan_shape_text_index_for_single_text_source() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "txt", &[(b"a", map(&[("body", text("rust db"))]))]);
    c.create_text_index("body").unwrap();

    let q = c.query().text("body", "rust", 10);
    assert_eq!(
        q.plan_shape(),
        PlanShape::TextIndex {
            field: "body".to_owned()
        }
    );
    assert!(
        q.explain().starts_with("text-index(body)"),
        "got: {}",
        q.explain()
    );

    // Without the index (a second collection): the multi-source fallback is
    // a Scan.
    let bare = seed(&db, "txt2", &[(b"a", map(&[("body", text("rust db"))]))]);
    let q = bare.query().text("body", "rust", 10);
    assert_eq!(
        q.plan_shape(),
        PlanShape::Scan {
            collection: "txt2".to_owned()
        }
    );
    assert!(
        q.explain().starts_with("scan(txt2)"),
        "got: {}",
        q.explain()
    );
}

#[test]
fn queries_plan_shape_indexed_window_kinds_and_explain_families() {
    // One collection per family: the window probes attribute by PROBE ORDER
    // (scalar comparisons first, then compound, geo, or), so a scalar index
    // on a compound-tested field would shadow the compound arm.
    let db = Db::open_in_memory().unwrap();
    let point = || Value::Array(vec![Value::Float(10.0), Value::Float(10.0)]);
    let docs: Vec<(&[u8], Value)> = vec![
        (
            b"a",
            map(&[("n", Value::Int(1)), ("cat", text("x")), ("loc", point())]),
        ),
        (
            b"b",
            map(&[("n", Value::Int(2)), ("cat", text("y")), ("loc", point())]),
        ),
    ];

    // Scalar window.
    let scalar = seed(&db, "w-scalar", &docs);
    scalar.create_scalar_index("n").unwrap();
    let q = scalar.query().filter(field("n").ge(Value::Int(1)));
    assert_eq!(q.plan_shape(), PlanShape::IndexedWindow { kind: "scalar" });
    assert!(
        q.explain().starts_with("indexed-window(scalar)"),
        "got: {}",
        q.explain()
    );

    // Compound window (equality prefix on `cat` + range tail on `n`
    // covering every field of the ["cat", "n"] index, and NO scalar indexes
    // that would win the earlier probe).
    let compound = seed(&db, "w-compound", &docs);
    compound.create_compound_index(&["cat", "n"]).unwrap();
    let q = compound
        .query()
        .filter(field("cat").eq(text("x")))
        .filter(field("n").ge(Value::Int(0)));
    assert_eq!(
        q.plan_shape(),
        PlanShape::IndexedWindow { kind: "compound" }
    );
    assert!(
        q.explain().starts_with("indexed-window(compound)"),
        "got: {}",
        q.explain()
    );

    // Geo window.
    let geo = seed(&db, "w-geo", &docs);
    geo.create_geo_index("loc").unwrap();
    let q = geo.query().filter(field("loc").within_km(10.0, 10.0, 1.0));
    assert_eq!(q.plan_shape(), PlanShape::IndexedWindow { kind: "geo" });
    assert!(
        q.explain().starts_with("indexed-window(geo)"),
        "got: {}",
        q.explain()
    );

    // OR union of index-serviceable disjuncts (both on scalar-indexed
    // fields — an unserviceable disjunct anywhere would decline to Scan).
    let or = seed(&db, "w-or", &docs);
    or.create_scalar_index("n").unwrap();
    or.create_scalar_index("cat").unwrap();
    let q = or
        .query()
        .filter(field("n").eq(Value::Int(1)).or(field("cat").eq(text("y"))));
    assert_eq!(q.plan_shape(), PlanShape::IndexedWindow { kind: "or" });
    assert!(
        q.explain().starts_with("indexed-window(or)"),
        "got: {}",
        q.explain()
    );

    // No sources, no serviceable filter: the Scan fallback, naming the
    // collection — and nothing else to decorate.
    let q = scalar.query();
    assert_eq!(
        q.plan_shape(),
        PlanShape::Scan {
            collection: "w-scalar".to_owned()
        }
    );
    assert_eq!(q.explain(), "scan(w-scalar)");
}

#[test]
fn queries_plan_shape_sort_index_for_order_by_on_indexed_field() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "sort",
        &[
            (b"a", map(&[("n", Value::Int(3))])),
            (b"b", map(&[("n", Value::Int(1))])),
            (b"c", map(&[("x", Value::Bool(true))])), // not the ordered field
        ],
    );
    c.create_scalar_index("n").unwrap();

    // A filterless order_by over the indexed field: the sort-index walk.
    let q = c.query().order_by("n", false);
    assert_eq!(
        q.plan_shape(),
        PlanShape::SortIndex {
            field: "n".to_owned()
        }
    );
    assert!(
        q.explain().starts_with("sort-index(n)"),
        "got: {}",
        q.explain()
    );
    assert!(q.explain().contains("order_by(n)"), "got: {}", q.explain());

    // Descending and windowed: same arm.
    let q = c.query().order_by("n", true).offset(1).limit(1);
    assert_eq!(
        q.plan_shape(),
        PlanShape::SortIndex {
            field: "n".to_owned()
        }
    );

    // No order_by at all: plain Scan, even with the index present.
    assert_eq!(
        c.query().plan_shape(),
        PlanShape::Scan {
            collection: "sort".to_owned()
        }
    );
    // order_by over a field with NO index: no walk to take.
    assert_eq!(
        c.query().order_by("x", false).plan_shape(),
        PlanShape::Scan {
            collection: "sort".to_owned()
        }
    );
}

#[test]
fn queries_plan_shape_streaming_topk_without_index() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "stream",
        &[(b"a", map(&[("e", Value::Vector(vec![1.0, 0.0]))]))],
    );
    // A single vector source with NO registered ANN index: bounded
    // streaming top-k, not a scan.
    let q = c.query().vector("e", vec![1.0, 0.0], 10, Metric::L2);
    assert_eq!(q.plan_shape(), PlanShape::StreamingTopK);
    assert!(
        q.explain().starts_with("streaming-topk"),
        "got: {}",
        q.explain()
    );

    // explain() decorates the query shape after the head: source, ordering,
    // pagination, projection.
    let q = c
        .query()
        .filter(field("e").exists())
        .vector("e", vec![1.0, 0.0], 10, Metric::L2)
        .order_by("e", true)
        .offset(1)
        .limit(2)
        .select(["e"]);
    let explained = q.explain();
    assert!(explained.contains("filter x1"), "got: {explained}");
    assert!(
        explained.contains("vector(e, k=10, L2)"),
        "got: {explained}"
    );
    assert!(explained.contains("order_by(e desc)"), "got: {explained}");
    assert!(explained.contains("offset 1"), "got: {explained}");
    assert!(explained.contains("limit 2"), "got: {explained}");
    assert!(explained.contains("select [e]"), "got: {explained}");
}

// ===========================================================================
// ResultRow shape
// ===========================================================================

#[test]
fn queries_result_row_fields_per_query_shape() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "rows",
        &[
            (
                b"a",
                map(&[("n", Value::Int(1)), ("e", Value::Vector(vec![0.5]))]),
            ),
            (
                b"b",
                map(&[("n", Value::Int(2)), ("e", Value::Vector(vec![1.5]))]),
            ),
            (
                b"c",
                map(&[("n", Value::Int(3)), ("e", Value::Vector(vec![3.0]))]),
            ),
        ],
    );

    // Filter-only rows: the stored key, the FULL document, and score
    // EXACTLY 0.0 (there is no rank to report — and no distance field on
    // ResultRow at all; distances belong to the direct-search Hit type).
    let rows = c
        .query()
        .filter(field("n").ge(Value::Int(1)))
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["a", "b", "c"]));
    assert_eq!(rows[0].document, c.get(b"a").unwrap().unwrap());
    assert!(rows.iter().all(|r| r.score == 0.0));

    // Single vector source: rows come back best-first with the RRF fused
    // score 1/(60+rank) — positive, strictly decreasing down the list.
    let rows = c
        .query()
        .vector("e", vec![0.0], 10, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["a", "b", "c"]));
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32);
    assert_eq!(rows[1].score, 1.0f32 / 62.0f32);
    assert_eq!(rows[2].score, 1.0f32 / 63.0f32);
    // Full documents still ride along on ranked rows.
    assert_eq!(rows[0].document, c.get(b"a").unwrap().unwrap());

    // order_by REPLACES the rank order but keeps each row's fused score.
    let rows = c
        .query()
        .vector("e", vec![0.0], 10, Metric::L2)
        .order_by("n", true)
        .run()
        .unwrap();
    assert_eq!(row_keys(&rows), k(&["c", "b", "a"]));
    assert_eq!(rows[0].score, 1.0f32 / 63.0f32); // c was rank 3

    // With select: key and score unchanged, document narrowed.
    let rows = c
        .query()
        .vector("e", vec![0.0], 10, Metric::L2)
        .select(["n"])
        .run()
        .unwrap();
    assert_eq!(rows[0].key, b"a".to_vec());
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32);
    assert_eq!(rows[0].document, map(&[("n", Value::Int(1))]));
}
