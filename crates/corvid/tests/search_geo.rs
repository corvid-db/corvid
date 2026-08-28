//! Geo conformance (skeleton). Task 10 fills this file with the full
//! matrix: haversine known distances, radius boundary inclusion, bbox
//! validation and antimeridian, poles, point formats, geo index equivalence,
//! GeoHit fields. This smoke test anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(at: (f64, f64)) -> Value {
    let mut m = BTreeMap::new();
    m.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::Float(at.0), Value::Float(at.1)]),
    );
    Value::Map(m)
}

const LONDON: (f64, f64) = (51.5074, -0.1278);
const PARIS: (f64, f64) = (48.8566, 2.3522);
const TOKYO: (f64, f64) = (35.6762, 139.6503);

#[test]
fn search_geo_smoke_within_radius_and_nearest() {
    // Known great-circle distance: London–Paris is ~344 km.
    let d = corvid::haversine_km(LONDON.0, LONDON.1, PARIS.0, PARIS.1);
    assert!((330.0..=355.0).contains(&d), "London-Paris distance {d}");

    let db = Db::open_in_memory().unwrap();
    let c = db.collection("places");
    c.insert(b"london", &doc(LONDON)).unwrap();
    c.insert(b"paris", &doc(PARIS)).unwrap();
    c.insert(b"tokyo", &doc(TOKYO)).unwrap();

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
    assert_eq!(nearest[0].document.get("loc"), doc(LONDON).get("loc"));
}
