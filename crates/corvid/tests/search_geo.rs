//! Geo conformance (Task 10): `haversine_km`, `geo_within_radius`,
//! `geo_nearest`, `geo_within_bbox`, `GeoHit`, the `GeoWithin` predicate, and
//! the geo index — driven through the public API only.
//!
//! Contracts pinned by these tests (read from `src/geo.rs`,
//! `src/geo_index.rs`, `src/filter.rs`, and `src/builder.rs` first):
//!
//! * `haversine_km` is spherical with R = 6371.0088 km (IUGG mean radius):
//!   one degree of great-circle arc is R·π/180 ≈ 111.195080 km and the half
//!   circumference is πR ≈ 20015.114442 km (the maximum possible distance).
//!   sin²(Δλ/2) has period π, so the formula is automatically
//!   antimeridian-correct — Tokyo→San Francisco crosses the dateline and
//!   still measures ~8274.6 km, the short way.
//! * Radius tests are inclusive (`<=`): a document at exactly `radius_km`
//!   from the center matches. This is deterministic because the engine
//!   compares the very float the public `haversine_km` returns.
//! * `geo_within_radius` and `geo_nearest` perform NO input validation:
//!   out-of-domain centers (|lat| > 90) are evaluated mathematically (a
//!   95°→85° arc is just 10° ≈ 1111.95 km) and NaN/negative parameters
//!   simply match nothing. Only `geo_within_bbox` validates its bounds,
//!   raising `Error::InvalidArgument` for out-of-domain/NaN coordinates and
//!   for `min_lat > max_lat`.
//! * `geo_nearest` with k=0 is `Ok([])`, never an error; k beyond the number
//!   of valid points returns all of them, nearest first.
//! * Result ordering: radius and nearest sort by (distance, key) on every
//!   path. `geo_within_bbox` returns KEY order on the scan path and
//!   lat-cell/lon-cell/key order on the indexed path — both deterministic,
//!   but different; the cross-path contract is the SET (pinned below and
//!   reported: the `geo_within_bbox` doc comment overstates "in key order").
//! * Points are `[lat, lon]` arrays or `{lat, lon}` maps (int or float
//!   coordinates); extra map keys are ignored; arrays are ALWAYS [lat, lon]
//!   — a swapped `[lon, lat]` document is just a different location on the
//!   globe, not an error.
//! * A geo index never changes membership: indexed and unindexed twins
//!   return identical results (radius, bbox, nearest), before and after
//!   mutations. The planner drives `IndexedWindow { kind: "geo" }` only for
//!   serviceable windows — a radius whose bounding box would wrap the
//!   antimeridian or reach the poles is declined to `Scan` (the query stays
//!   exact via the scan fallback).
//!
//! The Wave-1 smoke test that anchored the radar skeleton is kept at the top.

use std::collections::BTreeMap;

use corvid::{Collection, Db, GeoHit, PlanShape, Value, field, haversine_km};

// ---- shared geography (the constants the assertions justify against) ----

/// The engine's spherical Earth radius (km), from `src/geo.rs`.
const EARTH_R_KM: f64 = 6371.0088;

/// One degree of great-circle arc: R·π/180 ≈ 111.195080234 km.
const ONE_DEG_KM: f64 = EARTH_R_KM * std::f64::consts::PI / 180.0;

/// Half circumference πR ≈ 20015.114442036 km — the maximum distance.
const HALF_CIRCUMFERENCE_KM: f64 = EARTH_R_KM * std::f64::consts::PI;

const LONDON: (f64, f64) = (51.5074, -0.1278);
const PARIS: (f64, f64) = (48.8566, 2.3522);
const TOKYO: (f64, f64) = (35.6762, 139.6503);
const SF: (f64, f64) = (37.7749, -122.4194);
/// The point antipodal to London.
const LONDON_ANTIPODE: (f64, f64) = (-51.5074, 179.8722);

// ---- helpers ----

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Map(m)
}

/// A `[lat, lon]` float-array point.
fn pt_arr(lat: f64, lon: f64) -> Value {
    Value::Array(vec![Value::Float(lat), Value::Float(lon)])
}

/// A `{lat, lon}` map point.
fn pt_map(lat: f64, lon: f64) -> Value {
    map(&[("lat", Value::Float(lat)), ("lon", Value::Float(lon))])
}

/// A document whose point lives under `field` (default "loc").
fn doc(field: &str, point: Value) -> Value {
    map(&[(field, point)])
}

fn doc_arr(lat: f64, lon: f64) -> Value {
    doc("loc", pt_arr(lat, lon))
}

fn doc_map(lat: f64, lon: f64) -> Value {
    doc("loc", pt_map(lat, lon))
}

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> Collection<'a> {
    let c = db.collection(name);
    for (k, v) in docs {
        c.insert(k, v).unwrap();
    }
    c
}

fn hit_keys(hits: &[GeoHit]) -> Vec<Vec<u8>> {
    hits.iter().map(|h| h.key.clone()).collect()
}

fn row_keys(rows: &[(Vec<u8>, Value)]) -> Vec<Vec<u8>> {
    rows.iter().map(|(k, _)| k.clone()).collect()
}

/// Sorted keys — set assertions that never depend on result order.
fn sorted_keys(keys: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut ks = keys;
    ks.sort();
    ks
}

/// Two in-memory twins with the same corpus; `indexed` gets a geo index on
/// `field`. Twin comparisons are the index-vs-scan equivalence harness.
fn twins(field: &str, docs: &[(&[u8], Value)]) -> (Db, Db) {
    let scan = Db::open_in_memory().unwrap();
    seed(&scan, "places", docs);
    let indexed = Db::open_in_memory().unwrap();
    let c = seed(&indexed, "places", docs);
    c.create_geo_index(field).unwrap();
    (scan, indexed)
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn search_geo_smoke_within_radius_and_nearest() {
    // Known great-circle distance: London–Paris is ~344 km.
    let d = haversine_km(LONDON.0, LONDON.1, PARIS.0, PARIS.1);
    assert!((330.0..=355.0).contains(&d), "London-Paris distance {d}");

    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"london", &doc_arr(LONDON.0, LONDON.1)).unwrap();
    c.insert(b"paris", &doc_map(PARIS.0, PARIS.1)).unwrap();
    c.insert(b"tokyo", &doc_arr(TOKYO.0, TOKYO.1)).unwrap();

    let hits = c
        .geo_within_radius("loc", LONDON.0, LONDON.1, 400.0)
        .unwrap();
    let keys: Vec<&[u8]> = hits.iter().map(|h| h.key.as_slice()).collect();
    assert_eq!(keys, vec![&b"london"[..], &b"paris"[..]]); // nearest first
    assert!(hits[0].distance_km < hits[1].distance_km);
    assert!((hits[1].distance_km - d).abs() < 1.0);

    let nearest = c.geo_nearest("loc", LONDON.0, LONDON.1, 1).unwrap();
    assert_eq!(nearest.len(), 1);
    assert_eq!(nearest[0].key, b"london".to_vec());
    assert_eq!(
        nearest[0].document.get("loc"),
        doc_arr(LONDON.0, LONDON.1).get("loc")
    );
}

// ===========================================================================
// 1. haversine_km — the public distance primitive
// ===========================================================================

/// Known pairs against independently justified constants: one degree of arc
/// is R·π/180 by definition of the sphere model; the London–Paris spherical
/// distance on R = 6371.0088 is ≈ 343.5565 km (the ellipsoidal WGS-84 value
/// ≈ 343.9 km is commonly quoted as "~344 km"; the engine defines the sphere
/// and we assert it). Antipodal points are exactly half the circumference.
/// Symmetry, the poles, and the ±180° meridian identity are pinned too.
#[test]
fn geo_haversine_known_distances_symmetry_poles_antipodal() {
    // One degree of longitude on the equator = one degree of arc.
    assert!(
        (haversine_km(0.0, 0.0, 0.0, 1.0) - ONE_DEG_KM).abs() < 1e-9,
        "1° of arc must be R·π/180 ≈ 111.195080 km"
    );

    // London → Paris on the engine's sphere.
    let d = haversine_km(LONDON.0, LONDON.1, PARIS.0, PARIS.1);
    assert!((d - 343.5565).abs() < 1e-3, "London-Paris = {d}");
    // Symmetric.
    assert!(
        (haversine_km(PARIS.0, PARIS.1, LONDON.0, LONDON.1) - d).abs() < 1e-9,
        "haversine must be symmetric"
    );

    // Antipodal: the maximum possible distance, half the circumference.
    assert!(
        (haversine_km(0.0, 0.0, 0.0, 180.0) - HALF_CIRCUMFERENCE_KM).abs() < 1e-9,
        "antipodal = πR ≈ 20015.114442 km"
    );
    assert!(
        (haversine_km(LONDON.0, LONDON.1, LONDON_ANTIPODE.0, LONDON_ANTIPODE.1)
            - HALF_CIRCUMFERENCE_KM)
            .abs()
            < 1e-9,
        "the true antipode of London is exactly πR away"
    );

    // Poles: pole-to-pole is half the circumference, pole-to-equator a
    // quarter (πR/2), whatever the longitudes.
    assert!(
        (haversine_km(90.0, 0.0, -90.0, 0.0) - HALF_CIRCUMFERENCE_KM).abs() < 1e-9,
        "N pole → S pole = πR"
    );
    assert!(
        (haversine_km(90.0, -33.0, 0.0, 71.0) - HALF_CIRCUMFERENCE_KM / 2.0).abs() < 1e-9,
        "any point on the equator is πR/2 from either pole"
    );

    // The same point is exactly +0.0 km; −180° and +180° name the SAME
    // meridian, so that pair is zero up to the sin(π) fp residue (~1e-12 km).
    assert_eq!(
        haversine_km(51.5, -0.1, 51.5, -0.1).to_bits(),
        0.0f64.to_bits()
    );
    assert!(
        haversine_km(0.0, -180.0, 0.0, 180.0) < 1e-9,
        "lon −180 and +180 are the same meridian"
    );
}

// ===========================================================================
// 2. geo_within_radius — boundary, ordering, radii extremes
// ===========================================================================

/// The boundary at exactly `radius_km` is INCLUSIVE, and it is deterministic:
/// the engine compares the very float the public `haversine_km` returns, so
/// querying with `d = haversine_km(...)` includes the doc and `d` stepped a
/// few ULPs down excludes it. Results sort by (distance, key) — equidistant
/// docs (bitwise-equal floats here: Δλ = ±1°) fall back to key order.
#[test]
fn geo_within_radius_boundary_inclusive_and_ordering() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"east1", &doc_arr(0.0, 1.0)).unwrap();
    c.insert(b"west1", &doc_map(0.0, -1.0)).unwrap();
    c.insert(
        b"origin",
        &doc("loc", Value::Array(vec![Value::Int(0), Value::Int(0)])),
    )
    .unwrap();

    let d = haversine_km(0.0, 0.0, 0.0, 1.0);
    assert!((d - ONE_DEG_KM).abs() < 1e-9);

    // Exactly at the radius: all three, ordered (0, d, d) with the tie in
    // key order ("east1" < "west1"; "origin" sits at distance 0 first).
    let hits = c.geo_within_radius("loc", 0.0, 0.0, d).unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"origin".to_vec(), b"east1".to_vec(), b"west1".to_vec()]
    );
    assert_eq!(hits[0].distance_km.to_bits(), 0.0f64.to_bits());
    assert_eq!(hits[1].distance_km.to_bits(), d.to_bits());
    assert_eq!(
        hits[1].distance_km.to_bits(),
        hits[2].distance_km.to_bits(),
        "(0,1) and (0,-1) are bitwise equidistant from (0,0)"
    );

    // A few ULPs INSIDE the radius drops both boundary docs...
    let eps = d * f64::EPSILON; // ~1.7 ULPs at this magnitude
    let hits = c.geo_within_radius("loc", 0.0, 0.0, d - eps).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"origin".to_vec()]);
    // ...and a few ULPs OUTSIDE adds nothing new (already all three).
    let hits = c.geo_within_radius("loc", 0.0, 0.0, d + eps).unwrap();
    assert_eq!(hit_keys(&hits).len(), 3);
}

/// Radius extremes: zero radius matches only a point at EXACTLY the center
/// (distance +0.0 <= 0); a tiny radius misses even nearby docs; a radius
/// just under the half circumference excludes only the antipode; a radius
/// past it (and any larger one) covers every point on the sphere.
#[test]
fn geo_within_radius_zero_tiny_and_full_globe_radii() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"london", &doc_arr(LONDON.0, LONDON.1)).unwrap();
    c.insert(b"paris", &doc_map(PARIS.0, PARIS.1)).unwrap();
    c.insert(b"tokyo", &doc_arr(TOKYO.0, TOKYO.1)).unwrap();
    c.insert(b"npole", &doc_arr(90.0, 0.0)).unwrap();
    c.insert(b"spole", &doc_map(-90.0, 0.0)).unwrap();
    c.insert(b"antipode", &doc_arr(LONDON_ANTIPODE.0, LONDON_ANTIPODE.1))
        .unwrap();

    // Zero radius at a document: that document only.
    let hits = c.geo_within_radius("loc", LONDON.0, LONDON.1, 0.0).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"london".to_vec()]);
    assert_eq!(hits[0].distance_km.to_bits(), 0.0f64.to_bits());

    // Zero radius at a NEARBY point (≈ 0.071 km from London): nothing at
    // radius 0.05, only the centered doc once the radius covers it.
    let hits = c.geo_within_radius("loc", 51.5070, -0.1270, 0.05).unwrap();
    assert!(hits.is_empty());
    let hits = c.geo_within_radius("loc", 51.5070, -0.1270, 0.1).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"london".to_vec()]);
    // A tiny radius around the London doc reaches only that doc (Paris is
    // 343.6 km away).
    let hits = c.geo_within_radius("loc", LONDON.0, LONDON.1, 0.5).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"london".to_vec()]);

    // Just under the half circumference from London: everything EXCEPT the
    // antipode (πR ≈ 20015.114442 km away).
    let mut hits = c
        .geo_within_radius("loc", LONDON.0, LONDON.1, 20010.0)
        .unwrap();
    assert_eq!(
        hit_keys(&hits).len(),
        5,
        "only the antipode is out of reach"
    );
    assert!(!hit_keys(&hits).contains(&b"antipode".to_vec()));

    // Past the half circumference: the whole sphere, still distance-sorted.
    for r in [20016.0, 25000.0] {
        hits = c.geo_within_radius("loc", LONDON.0, LONDON.1, r).unwrap();
        assert_eq!(hit_keys(&hits).len(), 6, "radius {r} covers the globe");
        assert!(
            hits.windows(2)
                .all(|w| w[0].distance_km <= w[1].distance_km)
        );
    }
    // The antipode is at the mathematical maximum, πR.
    let anti = hits.iter().find(|h| h.key == b"antipode".to_vec()).unwrap();
    assert!((anti.distance_km - HALF_CIRCUMFERENCE_KM).abs() < 1e-6);
}

/// Center placements: on a document (distance exactly 0, that doc first),
/// in a spot with no documents nearby (empty), and on an empty collection.
#[test]
fn geo_within_radius_center_on_doc_and_no_docs() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"london", &doc_arr(LONDON.0, LONDON.1)).unwrap();
    c.insert(b"paris", &doc_map(PARIS.0, PARIS.1)).unwrap();
    c.insert(b"tokyo", &doc_arr(TOKYO.0, TOKYO.1)).unwrap();

    // Center on the tokyo document itself (London ≈ 9558.6 km, Paris
    // ≈ 9711.7 km from Tokyo).
    let hits = c
        .geo_within_radius("loc", TOKYO.0, TOKYO.1, 9800.0)
        .unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"tokyo".to_vec(), b"london".to_vec(), b"paris".to_vec()]
    );
    assert_eq!(hits[0].distance_km.to_bits(), 0.0f64.to_bits());
    assert!(
        (hits[1].distance_km - haversine_km(TOKYO.0, TOKYO.1, LONDON.0, LONDON.1)).abs() < 1e-9
    );

    // A radius small enough to hold only the centered doc.
    let hits = c.geo_within_radius("loc", PARIS.0, PARIS.1, 100.0).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"paris".to_vec()]); // London is 343.6 km away

    // Center in an empty ocean (0, -140): nothing within 2000 km.
    let hits = c.geo_within_radius("loc", 0.0, -140.0, 2000.0).unwrap();
    assert!(hits.is_empty());

    // An empty collection is a successful empty result.
    let none = db
        .collection("empty")
        .geo_within_radius("loc", 0.0, 0.0, 100.0)
        .unwrap();
    assert!(none.is_empty());
}

/// The radius entry points perform NO input validation (only
/// `geo_within_bbox` does): an out-of-domain center is evaluated
/// mathematically — a 95°→85° arc is 10° ≈ 1111.95 km — while NaN
/// parameters and negative radii simply match nothing. The geo-indexed path
/// behaves identically (it declines to a scan for these shapes).
#[test]
fn geo_within_radius_no_input_validation_mathematical_semantics() {
    let docs = [(b"lat85" as &[u8], doc_arr(85.0, 0.0))];
    let (scan, indexed) = twins("loc", &docs);
    let scan = scan.collection("places");
    let indexed = indexed.collection("places");

    // |lat| > 90 center: evaluated as the plain 10-degree arc.
    for c in [&scan, &indexed] {
        let hits = c.geo_within_radius("loc", 95.0, 0.0, 1200.0).unwrap();
        assert_eq!(hit_keys(&hits), vec![b"lat85".to_vec()]);
        assert!((hits[0].distance_km - 10.0 * ONE_DEG_KM).abs() < 1e-6);
        let hits = c.geo_within_radius("loc", 95.0, 0.0, 1111.0).unwrap();
        assert!(hits.is_empty(), "the 10° arc is ≈ 1111.95 km");
    }

    // NaN center, NaN radius, negative radius: no match, no error.
    for c in [&scan, &indexed] {
        assert!(
            c.geo_within_radius("loc", f64::NAN, 0.0, 100.0)
                .unwrap()
                .is_empty()
        );
        assert!(
            c.geo_within_radius("loc", 0.0, f64::NAN, 100.0)
                .unwrap()
                .is_empty()
        );
        assert!(
            c.geo_within_radius("loc", 0.0, 0.0, f64::NAN)
                .unwrap()
                .is_empty()
        );
        assert!(
            c.geo_within_radius("loc", 0.0, 0.0, -10.0)
                .unwrap()
                .is_empty()
        );
    }
}

// ===========================================================================
// 3. Point formats — arrays, maps, ints, nesting, swapped order
// ===========================================================================

/// Both point formats (plus int coordinates, extra map keys, and a nested
/// dotted path) extract the SAME point; malformed shapes are skipped, never
/// errors. An array is ALWAYS `[lat, lon]`: a swapped `[lon, lat]` document
/// is not rejected — it is simply a different location, matching queries
/// centered THERE.
#[test]
fn geo_point_formats_array_map_extra_keys_reversed_and_nested() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"arr", &doc_arr(51.5, -0.13)).unwrap();
    c.insert(b"map", &doc_map(51.5, -0.13)).unwrap();
    // Extra keys beside lat/lon are ignored.
    c.insert(
        b"extra",
        &doc(
            "loc",
            map(&[
                ("lat", Value::Float(51.5)),
                ("lon", Value::Float(-0.13)),
                ("city", Value::Text("london".to_owned())),
            ]),
        ),
    )
    .unwrap();
    // Int coordinates extract to the same point (0, 0).
    c.insert(
        b"origin-int",
        &doc("loc", Value::Array(vec![Value::Int(0), Value::Int(0)])),
    )
    .unwrap();
    // Malformed shapes: skipped by every geo query.
    c.insert(
        b"missing-lon",
        &doc("loc", map(&[("lat", Value::Float(51.5))])),
    )
    .unwrap();
    c.insert(
        b"triple",
        &doc(
            "loc",
            Value::Array(vec![
                Value::Float(51.5),
                Value::Float(-0.13),
                Value::Float(0.0),
            ]),
        ),
    )
    .unwrap();
    c.insert(
        b"text-lat",
        &doc(
            "loc",
            map(&[
                ("lat", Value::Text("51.5".to_owned())),
                ("lon", Value::Float(-0.13)),
            ]),
        ),
    )
    .unwrap();
    // Swapped [lon, lat]: a valid point — at a different place ((-0.13, 51.5)
    // is in the Indian Ocean near Somalia). The API has no order heuristic.
    c.insert(b"swapped", &doc_arr(-0.13, 51.5)).unwrap();
    // A nested point under a dotted path.
    c.insert(
        b"nested",
        &map(&[("meta", map(&[("loc", pt_map(51.5, -0.13))]))]),
    )
    .unwrap();

    // The three London-adjacent docs (plus the nested one via its path).
    let hits = c.geo_within_radius("loc", 51.5, -0.13, 1.0).unwrap();
    assert_eq!(
        sorted_keys(hit_keys(&hits)),
        vec![b"arr".to_vec(), b"extra".to_vec(), b"map".to_vec()]
    );
    let hits = c.geo_within_radius("meta.loc", 51.5, -0.13, 1.0).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"nested".to_vec()]);
    // Ints: (0,0).
    let hits = c.geo_within_radius("loc", 0.0, 0.0, 1.0).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"origin-int".to_vec()]);

    // The swapped doc does NOT match the London query...
    let hits = c.geo_within_radius("loc", 51.5, -0.13, 6000.0).unwrap();
    assert!(!hit_keys(&hits).contains(&b"swapped".to_vec()));
    // ...but matches (exactly, distance +0.0) a query centered at ITS
    // reading of the coordinates.
    let hits = c.geo_within_radius("loc", -0.13, 51.5, 0.0).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"swapped".to_vec()]);
}

// ===========================================================================
// 4. geo_nearest — k values, GeoHit shape, ties, non-points
// ===========================================================================

/// k=0 is `Ok([])` (not an error); k=1/n/>n return the k nearest points,
/// distance-ascending; GeoHit carries the key, the exact distance, and the
/// full stored document.
#[test]
fn geo_nearest_k_zero_one_n_beyond_and_hit_fields() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    let london_doc = map(&[
        ("loc", pt_arr(LONDON.0, LONDON.1)),
        ("name", Value::Text("london".to_owned())),
    ]);
    c.insert(b"london", &london_doc).unwrap();
    c.insert(b"paris", &doc_map(PARIS.0, PARIS.1)).unwrap();
    c.insert(b"tokyo", &doc_arr(TOKYO.0, TOKYO.1)).unwrap();

    // k = 0: empty, never an error.
    assert!(
        c.geo_nearest("loc", LONDON.0, LONDON.1, 0)
            .unwrap()
            .is_empty()
    );

    // k = 1: GeoHit fields asserted exactly.
    let hits = c.geo_nearest("loc", LONDON.0, LONDON.1, 1).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"london".to_vec()]);
    assert_eq!(hits[0].distance_km.to_bits(), 0.0f64.to_bits());
    assert_eq!(
        hits[0].document, london_doc,
        "GeoHit carries the FULL document"
    );

    // k = 2: paris second, at the London-Paris distance pinned above.
    let hits = c.geo_nearest("loc", LONDON.0, LONDON.1, 2).unwrap();
    assert_eq!(hit_keys(&hits), vec![b"london".to_vec(), b"paris".to_vec()]);
    assert!(
        (hits[1].distance_km - haversine_km(LONDON.0, LONDON.1, PARIS.0, PARIS.1)).abs() < 1e-9
    );
    assert!((hits[1].distance_km - 343.5565).abs() < 1e-3);

    // k beyond the corpus: ALL valid points, strictly distance-ascending.
    let hits = c.geo_nearest("loc", LONDON.0, LONDON.1, 10).unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"london".to_vec(), b"paris".to_vec(), b"tokyo".to_vec()]
    );
    assert!(
        hits.windows(2)
            .all(|w| w[0].distance_km <= w[1].distance_km)
    );
}

/// Equidistant documents tie-break by KEY: the distances are bitwise equal
/// (Δλ = ±1° from (0,0)), so the documented `then_with(key)` decides.
#[test]
fn geo_nearest_equidistant_ties_break_by_key() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"z-east", &doc_arr(0.0, 1.0)).unwrap();
    c.insert(b"a-west", &doc_map(0.0, -1.0)).unwrap();

    let d = haversine_km(0.0, 0.0, 0.0, 1.0);
    let hits = c.geo_nearest("loc", 0.0, 0.0, 1).unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"a-west".to_vec()],
        "ties pick the smaller key"
    );
    let hits = c.geo_nearest("loc", 0.0, 0.0, 2).unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"a-west".to_vec(), b"z-east".to_vec()]
    );
    assert_eq!(hits[0].distance_km.to_bits(), hits[1].distance_km.to_bits());
    assert_eq!(hits[0].distance_km.to_bits(), d.to_bits());
}

/// Non-point documents are excluded from the k budget (never returned,
/// never erroring); an empty collection — or one with NO valid points —
/// yields `Ok([])`; and a document at the far side of the globe
/// (London's antipode, πR away) is still found, exercising the
/// expanding-radius search past its largest geo-index window and into the
/// full-scan fallback.
#[test]
fn geo_nearest_skips_non_points_empty_and_finds_antipodal() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"near", &doc_arr(LONDON.0, LONDON.1)).unwrap();
    c.insert(b"text-doc", &Value::Text("no location".to_owned()))
        .unwrap();
    c.insert(
        b"bad-map",
        &doc(
            "loc",
            map(&[
                ("lat", Value::Text("x".to_owned())),
                ("lon", Value::Float(0.0)),
            ]),
        ),
    )
    .unwrap();
    c.insert(b"antipode", &doc_arr(LONDON_ANTIPODE.0, LONDON_ANTIPODE.1))
        .unwrap();

    // Only the two valid points come back, nearest first — the text and
    // malformed docs do not consume the k budget.
    let hits = c.geo_nearest("loc", LONDON.0, LONDON.1, 5).unwrap();
    assert_eq!(
        hit_keys(&hits),
        vec![b"near".to_vec(), b"antipode".to_vec()]
    );
    assert_eq!(hits[0].distance_km.to_bits(), 0.0f64.to_bits());
    assert!((hits[1].distance_km - HALF_CIRCUMFERENCE_KM).abs() < 1e-6);

    // Empty collection.
    assert!(
        db.collection("empty")
            .geo_nearest("loc", 0.0, 0.0, 3)
            .unwrap()
            .is_empty()
    );

    // Only non-point documents.
    let db2 = Db::open_in_memory().unwrap();
    let c2 = db2.collection("junk");
    c2.insert(b"a", &Value::Int(1)).unwrap();
    c2.insert(b"b", &doc("loc", Value::Null)).unwrap();
    assert!(c2.geo_nearest("loc", 0.0, 0.0, 3).unwrap().is_empty());
}

// ===========================================================================
// 5. geo_within_bbox — membership, edges, degenerate/pole boxes, wrap
// ===========================================================================

/// A normal box with a document sitting EXACTLY on each edge: all four edges
/// are inclusive (`..=` ranges). The scan path returns KEY order — asserted
/// as an exact sequence (keys chosen so key order differs from every other
/// ordering).
#[test]
fn geo_within_bbox_normal_inclusive_edges_and_key_order() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    // Insertion order deliberately differs from key order.
    c.insert(b"mid", &doc_arr(10.0, 10.0)).unwrap();
    c.insert(b"edge-w", &doc_map(0.0, 10.0)).unwrap(); // min_lat edge
    c.insert(b"edge-n", &doc_arr(10.0, 20.0)).unwrap(); // max_lon edge
    c.insert(b"edge-s", &doc_map(10.0, 0.0)).unwrap(); // min_lon edge
    c.insert(b"edge-e", &doc_arr(20.0, 10.0)).unwrap(); // max_lat edge
    c.insert(b"far", &doc_arr(30.0, 30.0)).unwrap();

    let rows = c.geo_within_bbox("loc", 0.0, 0.0, 20.0, 20.0).unwrap();
    assert_eq!(
        row_keys(&rows),
        vec![
            b"edge-e".to_vec(),
            b"edge-n".to_vec(),
            b"edge-s".to_vec(),
            b"edge-w".to_vec(),
            b"mid".to_vec(),
        ],
        "all four edges inclusive, key order on the scan path"
    );
    assert_eq!(
        rows[4].1,
        doc_arr(10.0, 10.0),
        "rows carry the full document"
    );

    // A box matching nothing.
    let rows = c.geo_within_bbox("loc", -5.0, -5.0, -1.0, -1.0).unwrap();
    assert!(rows.is_empty());
}

/// Degenerate boxes are valid, not inversions: min==max on both axes is a
/// point box matching exactly that point; min==max on ONE axis is a line.
/// Pole-touching boxes accept lat ±90 (any longitude), and the full-globe
/// box matches everything.
#[test]
fn geo_within_bbox_degenerate_point_line_pole_and_globe() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"pt", &doc_arr(10.0, 10.0)).unwrap();
    c.insert(b"pt-near", &doc_arr(10.0, 10.0001)).unwrap();
    c.insert(b"line", &doc_map(10.0, 15.0)).unwrap();
    c.insert(b"off-line", &doc_arr(10.0001, 15.0)).unwrap();
    c.insert(b"npole", &doc_arr(90.0, 33.0)).unwrap();
    c.insert(b"spole", &doc_map(-90.0, -120.0)).unwrap();

    // Point box: only the exact point.
    let rows = c.geo_within_bbox("loc", 10.0, 10.0, 10.0, 10.0).unwrap();
    assert_eq!(row_keys(&rows), vec![b"pt".to_vec()]);
    // A point box at an empty location.
    let rows = c.geo_within_bbox("loc", 10.0, 12.0, 10.0, 12.0).unwrap();
    assert!(rows.is_empty());

    // One-axis degenerate: the whole parallel lat=10, lon in [0,20].
    let rows = c.geo_within_bbox("loc", 10.0, 0.0, 10.0, 20.0).unwrap();
    assert_eq!(
        sorted_keys(row_keys(&rows)),
        vec![b"line".to_vec(), b"pt".to_vec(), b"pt-near".to_vec()]
    );

    // Boxes touching the poles: lat ranges hitting ±90 with ANY longitudes.
    let rows = c.geo_within_bbox("loc", 85.0, -180.0, 90.0, 180.0).unwrap();
    assert_eq!(row_keys(&rows), vec![b"npole".to_vec()]);
    let rows = c
        .geo_within_bbox("loc", -90.0, -180.0, -85.0, 180.0)
        .unwrap();
    assert_eq!(row_keys(&rows), vec![b"spole".to_vec()]);

    // The full-globe box: everything.
    let rows = c
        .geo_within_bbox("loc", -90.0, -180.0, 90.0, 180.0)
        .unwrap();
    assert_eq!(row_keys(&rows).len(), 6);
}

/// A box with `min_lon > max_lon` WRAPS the antimeridian and matches BOTH
/// longitude ranges (`lon >= min_lon || lon <= max_lon`) — it is not an
/// error. Verified on the scan path and (via the fallback) with an index
/// present; the wrapped window is not index-accelerated but stays exact.
#[test]
fn geo_bbox_antimeridian_wrap_matches_both_sides() {
    let docs = [
        (b"east" as &[u8], doc_arr(15.0, 175.0)),
        (b"west", doc_map(15.0, -175.0)),
        (b"mid", doc_arr(15.0, 0.0)),
        (b"south", doc_map(-15.0, 178.0)),
        (b"far-east", doc_arr(15.0, 165.0)), // below min_lon, outside the wrap
    ];
    let (scan, indexed) = twins("loc", &docs);
    for db in [&scan, &indexed] {
        let rows = db
            .collection("places")
            .geo_within_bbox("loc", 10.0, 170.0, 20.0, -170.0)
            .unwrap();
        assert_eq!(
            sorted_keys(row_keys(&rows)),
            vec![b"east".to_vec(), b"west".to_vec()],
            "the wrapped box covers 170°E..180 and -180..170°W at lat 10..20"
        );
    }
}

/// `geo_within_bbox` is the only validating geo entry point: every
/// out-of-domain bound (|lat| > 90, |lon| > 180), every NaN bound, and an
/// inverted latitude box raise `Error::InvalidArgument` with the parameter
/// named at the start of the message. Validation precedes scanning (it
/// fires on an EMPTY collection).
#[test]
fn geo_bbox_validation_exact_error_variants() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("empty"); // no documents: pure argument validation

    // (min_lat, min_lon, max_lat, max_lon, expected message prefix).
    let cases: [(f64, f64, f64, f64, &str); 7] = [
        (
            91.0,
            0.0,
            0.0,
            0.0,
            "geo_within_bbox: min_lat = 91 is outside [-90, 90]",
        ),
        (
            0.0,
            0.0,
            -90.5,
            0.0,
            "geo_within_bbox: max_lat = -90.5 is outside [-90, 90]",
        ),
        (
            0.0,
            181.0,
            0.0,
            0.0,
            "geo_within_bbox: min_lon = 181 is outside [-180, 180]",
        ),
        (
            0.0,
            0.0,
            0.0,
            -180.5,
            "geo_within_bbox: max_lon = -180.5 is outside [-180, 180]",
        ),
        (
            f64::NAN,
            0.0,
            0.0,
            0.0,
            "geo_within_bbox: min_lat = NaN is outside [-90, 90]",
        ),
        (
            0.0,
            f64::NAN,
            0.0,
            0.0,
            "geo_within_bbox: min_lon = NaN is outside [-180, 180]",
        ),
        (
            10.0,
            -1.0,
            5.0,
            1.0,
            "geo_within_bbox: min_lat = 10 is greater than max_lat = 5",
        ),
    ];
    for (min_lat, min_lon, max_lat, max_lon, prefix) in cases {
        match c.geo_within_bbox("loc", min_lat, min_lon, max_lat, max_lon) {
            Ok(rows) => panic!(
                "bbox ({min_lat}, {min_lon}, {max_lat}, {max_lon}) must be rejected, got {rows:?}"
            ),
            Err(corvid::Error::InvalidArgument(msg)) => {
                assert!(
                    msg.starts_with(prefix),
                    "expected prefix {prefix:?}, got {msg:?} for ({min_lat}, {min_lon}, {max_lat}, {max_lon})"
                );
            }
            Err(e) => {
                panic!("bbox ({min_lat}, {min_lon}, {max_lat}, {max_lon}): wrong variant {e:?}")
            }
        }
    }

    // The domain boundaries themselves (and equal lat bounds) are accepted.
    assert!(c.geo_within_bbox("loc", -90.0, -180.0, 90.0, 180.0).is_ok());
    assert!(c.geo_within_bbox("loc", 10.0, 5.0, 10.0, 6.0).is_ok());
}

// ===========================================================================
// 6. GeoWithin predicate (deep) — dateline, poles, invalid centers
// ===========================================================================

/// The builder's `within_km` predicate (Task 4 pinned its point formats and
/// no-validation stance; these are the deep cases): the Tokyo→San Francisco
/// great circle crosses the antimeridian and `haversine_km` measures it the
/// short way — pinned to a 1e-4-wide radius window around 8274.626 km;
/// pole-to-pole matches at πR; an out-of-domain center is evaluated
/// mathematically; NaN centers match nothing; radius 0 is exact-point only.
#[test]
fn geo_predicate_deep_dateline_poles_invalid_centers() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"sf", &doc_map(SF.0, SF.1)).unwrap();
    c.insert(b"npole", &doc_arr(90.0, 0.0)).unwrap();
    c.insert(b"spole", &doc_map(-90.0, 0.0)).unwrap();
    c.insert(b"lat85", &doc_arr(85.0, 0.0)).unwrap();
    c.insert(b"london", &doc_arr(LONDON.0, LONDON.1)).unwrap();

    let matches = |lat: f64, lon: f64, r: f64| -> Vec<Vec<u8>> {
        let mut ks: Vec<Vec<u8>> = c
            .query()
            .filter(field("loc").within_km(lat, lon, r))
            .run()
            .unwrap()
            .into_iter()
            .map(|row| row.key)
            .collect();
        ks.sort();
        ks
    };

    // Tokyo → SF crosses the dateline the SHORT way: 8274.626408 km. A naive
    // unwrap-unaware plan would route the long way (~31700 km). The radius
    // window (8274.6, 8274.63] pins that value; the corpus's nearer docs
    // (npole 6040.5 km, lat85 6471.0 km from Tokyo) match both sides, London
    // (9558.6 km) neither.
    assert_eq!(
        matches(TOKYO.0, TOKYO.1, 8274.63),
        vec![b"lat85".to_vec(), b"npole".to_vec(), b"sf".to_vec()]
    );
    assert_eq!(
        matches(TOKYO.0, TOKYO.1, 8274.6),
        vec![b"lat85".to_vec(), b"npole".to_vec()],
        "SF at 8274.626408 km drops out below the true distance"
    );

    // Poles: from the south pole every corpus doc is within 20016 km and
    // only the antipodal north pole (πR ≈ 20015.114442 km away) drops out
    // just under the half circumference.
    assert_eq!(
        matches(-90.0, 0.0, 20016.0),
        vec![
            b"lat85".to_vec(),
            b"london".to_vec(),
            b"npole".to_vec(),
            b"sf".to_vec(),
            b"spole".to_vec()
        ]
    );
    assert_eq!(
        matches(-90.0, 0.0, 20010.0),
        vec![
            b"lat85".to_vec(),
            b"london".to_vec(),
            b"sf".to_vec(),
            b"spole".to_vec()
        ],
        "only the antipode is out of reach at 20010 km"
    );

    // Out-of-domain center (95°): the plain arcs — 10° ≈ 1111.95 km to the
    // 85° doc, 5° ≈ 555.98 km to the pole.
    assert_eq!(
        matches(95.0, 0.0, 1200.0),
        vec![b"lat85".to_vec(), b"npole".to_vec()]
    );
    assert_eq!(
        matches(95.0, 0.0, 1111.0),
        vec![b"npole".to_vec()],
        "the 10° arc to lat85 is ≈ 1111.95 km"
    );
    // NaN center: mathematically nothing (NaN <= r is false).
    assert!(matches(f64::NAN, 0.0, 25000.0).is_empty());
    // Radius 0: only the exact point.
    assert_eq!(matches(LONDON.0, LONDON.1, 0.0), vec![b"london".to_vec()]);
}

// ===========================================================================
// 7. Geo index — twin equivalence, live mutations, planner dispatch
// ===========================================================================

/// The index-vs-scan harness: a mixed corpus (both point formats, poles, a
/// dateline pair, int coordinates, a nested path, non-point docs) loaded
/// into twins, one with a geo index. Radius (boundary, zero, tiny, global,
/// dateline-straddling), bbox (normal, wrapped, degenerate, globe), and
/// nearest (k=1/3/beyond) all agree EXACTLY — including distances, since
/// `GeoHit: PartialEq`. Mutations after index creation (move a doc, delete
/// a doc, insert a doc) keep the twins in agreement.
#[test]
fn geo_index_twins_equivalence_and_live_mutations() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"london", doc_arr(LONDON.0, LONDON.1)),
        (b"paris", doc_map(PARIS.0, PARIS.1)),
        (b"tokyo", doc_arr(TOKYO.0, TOKYO.1)),
        (b"sf", doc_map(SF.0, SF.1)),
        (b"npole", doc_arr(90.0, 0.0)),
        (b"spole", doc_map(-90.0, 0.0)),
        (b"dl-east", doc_arr(15.0, 179.9)),
        (b"dl-west", doc_map(15.0, -179.9)),
        (
            b"origin-int",
            doc("loc", Value::Array(vec![Value::Int(0), Value::Int(0)])),
        ),
        (
            b"nested",
            map(&[("meta", map(&[("loc", pt_map(TOKYO.0, TOKYO.1))]))]),
        ),
        (b"nopoint", Value::Text("no location".to_owned())),
        (
            b"bad-map",
            doc("loc", map(&[("lat", Value::Text("x".to_owned()))])),
        ),
    ];
    let (scan_db, idx_db) = twins("loc", &docs);
    let scan = scan_db.collection("places");
    let idx = idx_db.collection("places");

    // -- radius parity (exact Vec<GeoHit> equality: keys, distances, docs) --
    let radii: [(f64, f64, f64); 6] = [
        (LONDON.0, LONDON.1, 400.0),
        (LONDON.0, LONDON.1, 20016.0),
        (0.0, 0.0, 0.0),
        (0.0, 0.0, ONE_DEG_KM),
        (15.0, 179.9, 50.0), // straddles the dateline: dl-east AND dl-west
        (f64::NAN, 0.0, 100.0),
    ];
    for (lat, lon, r) in radii {
        assert_eq!(
            scan.geo_within_radius("loc", lat, lon, r).unwrap(),
            idx.geo_within_radius("loc", lat, lon, r).unwrap(),
            "radius ({lat}, {lon}, {r}): index must not change membership"
        );
    }
    // The dateline-straddling radius really matched both sides.
    assert_eq!(
        sorted_keys(hit_keys(
            &idx.geo_within_radius("loc", 15.0, 179.9, 50.0).unwrap()
        )),
        vec![b"dl-east".to_vec(), b"dl-west".to_vec()]
    );
    // Nested path parity.
    assert_eq!(
        scan.geo_within_radius("meta.loc", TOKYO.0, TOKYO.1, 1.0)
            .unwrap(),
        idx.geo_within_radius("meta.loc", TOKYO.0, TOKYO.1, 1.0)
            .unwrap()
    );

    // -- bbox parity (sets: the two paths emit different orders) --
    let boxes: [(f64, f64, f64, f64); 4] = [
        (51.0, -1.0, 52.0, 1.0),
        (10.0, 170.0, 20.0, -170.0),  // antimeridian wrap
        (90.0, 0.0, 90.0, 0.0),       // degenerate point box on the pole
        (-90.0, -180.0, 90.0, 180.0), // full globe
    ];
    for (a, b, cc, d) in boxes {
        assert_eq!(
            sorted_keys(row_keys(&scan.geo_within_bbox("loc", a, b, cc, d).unwrap())),
            sorted_keys(row_keys(&idx.geo_within_bbox("loc", a, b, cc, d).unwrap())),
            "bbox ({a}, {b}, {cc}, {d}): index must not change membership"
        );
    }

    // -- nearest parity --
    for k in [1usize, 3, 100] {
        assert_eq!(
            scan.geo_nearest("loc", LONDON.0, LONDON.1, k).unwrap(),
            idx.geo_nearest("loc", LONDON.0, LONDON.1, k).unwrap(),
            "nearest k={k}: index must not change membership or order"
        );
    }
    // k=100 returned the 9 valid "loc" points (the nested doc's point lives
    // under "meta.loc"; the two non-point docs are excluded).
    assert_eq!(idx.geo_nearest("loc", 0.0, 0.0, 100).unwrap().len(), 9);

    // -- live mutations keep the twins in agreement --
    // Move london to the mid-Indian Ocean.
    scan.insert(b"london", &doc_arr(0.0, 80.0)).unwrap();
    idx.insert(b"london", &doc_arr(0.0, 80.0)).unwrap();
    // Delete the north pole doc.
    scan.delete(b"npole").unwrap();
    idx.delete(b"npole").unwrap();
    // Insert New York.
    scan.insert(b"nyc", &doc_arr(40.7128, -74.0060)).unwrap();
    idx.insert(b"nyc", &doc_arr(40.7128, -74.0060)).unwrap();

    for c in [&scan, &idx] {
        // london is gone from London...
        let hits = c
            .geo_within_radius("loc", LONDON.0, LONDON.1, 400.0)
            .unwrap();
        assert_eq!(sorted_keys(hit_keys(&hits)), vec![b"paris".to_vec()]);
        // ...and present at its new location.
        let hits = c.geo_within_radius("loc", 0.0, 80.0, 10.0).unwrap();
        assert_eq!(hit_keys(&hits), vec![b"london".to_vec()]);
        // npole is gone from the degenerate pole box.
        let rows = c.geo_within_bbox("loc", 90.0, 0.0, 90.0, 0.0).unwrap();
        assert!(rows.is_empty());
        // nyc is nearest to itself.
        let hits = c.geo_nearest("loc", 40.7128, -74.0060, 2).unwrap();
        assert_eq!(hit_keys(&hits).first(), Some(&b"nyc".to_vec()));
    }
    // Twins still agree end to end after the mutations (9 "loc" points:
    // london moved but kept a point, npole deleted, nyc added).
    assert_eq!(
        scan.geo_nearest("loc", 0.0, 0.0, 100).unwrap(),
        idx.geo_nearest("loc", 0.0, 0.0, 100).unwrap()
    );
    assert_eq!(idx.geo_nearest("loc", 0.0, 0.0, 100).unwrap().len(), 9);
}

/// `geo_within_bbox` result ORDER is path-dependent (pinned): the scan path
/// emits key order, the indexed path emits lat-cell/lon-cell/key order
/// (cells ascend northward here: z at lat 0 → m at 5 → a at 10). Membership
/// is identical; only the sequence differs. NOTE: the method's doc comment
/// says "in key order" — that holds for the scan path only (reported as an
/// ambiguity; sets are the portable contract).
#[test]
fn geo_bbox_result_order_scan_keys_vs_index_cells() {
    let docs = [
        (b"a" as &[u8], doc_arr(10.0, 10.0)),
        (b"m", doc_arr(5.0, 5.0)),
        (b"z", doc_arr(0.0, 0.0)),
    ];
    let (scan_db, idx_db) = twins("loc", &docs);
    let rows = scan_db
        .collection("places")
        .geo_within_bbox("loc", -1.0, -1.0, 11.0, 11.0)
        .unwrap();
    assert_eq!(
        row_keys(&rows),
        vec![b"a".to_vec(), b"m".to_vec(), b"z".to_vec()]
    );
    let rows = idx_db
        .collection("places")
        .geo_within_bbox("loc", -1.0, -1.0, 11.0, 11.0)
        .unwrap();
    assert_eq!(
        row_keys(&rows),
        vec![b"z".to_vec(), b"m".to_vec(), b"a".to_vec()],
        "the indexed window emits cell order (lat 0 → 5 → 10), not key order"
    );
}

/// Planner dispatch: a `GeoWithin` filter on a geo-indexed field plans
/// `IndexedWindow { kind: "geo" }` when the radius's bounding box is real,
/// and declines to `Scan` when there is no index, when the box would wrap
/// the antimeridian, or when the center is near enough to a pole that the
/// longitude band is unbounded. (`geo_nearest` has no builder stage, so it
/// has no plan_shape surface at all; its index use is pinned by the twins
/// equivalence above instead.)
#[test]
fn geo_index_plan_shape_serviceable_and_declined() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"london", &doc_arr(LONDON.0, LONDON.1)).unwrap();
    c.insert(b"tokyo", &doc_arr(TOKYO.0, TOKYO.1)).unwrap();
    c.insert(b"dl-east", &doc_arr(15.0, 179.9)).unwrap();
    c.insert(b"npole", &doc_arr(90.0, 0.0)).unwrap();

    // Without an index: Scan.
    let q = c
        .query()
        .filter(field("loc").within_km(LONDON.0, LONDON.1, 50.0));
    assert_eq!(
        q.plan_shape(),
        PlanShape::Scan {
            collection: "places".to_owned()
        }
    );
    assert!(q.explain().starts_with("scan("));

    // With an index and a serviceable radius: the geo window.
    c.create_geo_index("loc").unwrap();
    let q = c
        .query()
        .filter(field("loc").within_km(LONDON.0, LONDON.1, 50.0));
    assert_eq!(q.plan_shape(), PlanShape::IndexedWindow { kind: "geo" });
    assert!(
        q.explain().starts_with("indexed-window(geo)"),
        "got: {}",
        q.explain()
    );
    // ...and it still returns the right rows.
    let keys: Vec<Vec<u8>> = q.run().unwrap().into_iter().map(|r| r.key).collect();
    assert_eq!(keys, vec![b"london".to_vec()]);

    // Declined with the index PRESENT: a radius whose bounding box wraps
    // the antimeridian (center at 179.95°E)...
    let q = c
        .query()
        .filter(field("loc").within_km(15.0, 179.95, 100.0));
    assert!(
        matches!(q.plan_shape(), PlanShape::Scan { .. }),
        "wrapping radius is declined"
    );
    // ...and a near-pole center whose longitude band is unbounded.
    let q = c.query().filter(field("loc").within_km(89.999, 0.0, 50.0));
    assert!(
        matches!(q.plan_shape(), PlanShape::Scan { .. }),
        "near-pole radius is declined"
    );
    // Both still return exact results via the scan fallback.
    let keys: Vec<Vec<u8>> = c
        .query()
        .filter(field("loc").within_km(15.0, 179.95, 100.0))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(keys, vec![b"dl-east".to_vec()]);
}
