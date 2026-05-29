//! Geospatial queries over document fields.
//!
//! A location field holds a point as either a two-element array `[lat, lon]` or
//! a map with `lat`/`lon` keys (numbers may be int or float). Distances use the
//! haversine formula on a spherical Earth (kilometres). Queries are exact scans
//! in v0.1 — the correctness baseline; an R-tree / geohash index can accelerate
//! them later behind the same methods.
//!
//! Geo also composes into the query builder as a filter predicate
//! ([`crate::field`]`(...).within_km(...)`), so "near here AND semantically
//! relevant" is a single query.

use crate::db::Collection;
use crate::error::Result;
use crate::value::Value;

/// Mean Earth radius in kilometres (IUGG).
const EARTH_RADIUS_KM: f64 = 6371.0088;

/// Great-circle distance between two `(lat, lon)` points in kilometres.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().clamp(0.0, 1.0).asin()
}

/// Extract a `(lat, lon)` point from a value: `[lat, lon]` array or a map with
/// `lat`/`lon` keys. Returns `None` if the shape or types don't match.
pub(crate) fn extract_point(value: &Value) -> Option<(f64, f64)> {
    match value {
        Value::Array(items) if items.len() == 2 => Some((as_f64(&items[0])?, as_f64(&items[1])?)),
        Value::Map(_) => Some((as_f64(value.get("lat")?)?, as_f64(value.get("lon")?)?)),
        _ => None,
    }
}

/// Read a [`Value`] as `f64`, accepting both `Float` and `Int`.
fn as_f64(v: &Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_int().map(|i| i as f64))
}

/// One geospatial result: key, distance from the query point (km), document.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoHit {
    /// The document's key.
    pub key: Vec<u8>,
    /// Distance from the query center in kilometres.
    pub distance_km: f64,
    /// The full stored document.
    pub document: Value,
}

impl Collection<'_> {
    /// Find documents whose `field` point is within `radius_km` of
    /// `(lat, lon)`, nearest first. Documents lacking a valid point are
    /// skipped. Ties break by key.
    pub fn geo_within_radius(
        &self,
        field: &str,
        lat: f64,
        lon: f64,
        radius_km: f64,
    ) -> Result<Vec<GeoHit>> {
        // Spatial-index fast path: scan only the cells the circle's bounding box
        // overlaps, then verify exact haversine distance. Falls back to a full
        // scan when there is no index or the area is too large / wraps.
        let bbox = crate::geo_index::radius_bbox(lat, lon, radius_km);
        let scanned = self.geo_scan_set(field, bbox)?;

        let mut hits: Vec<GeoHit> = Vec::new();
        for (key, document) in scanned {
            if let Some((plat, plon)) = document.get(field).and_then(extract_point) {
                let distance_km = haversine_km(lat, lon, plat, plon);
                if distance_km <= radius_km {
                    hits.push(GeoHit {
                        key,
                        distance_km,
                        document,
                    });
                }
            }
        }
        hits.sort_by(|a, b| {
            a.distance_km
                .total_cmp(&b.distance_km)
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(hits)
    }

    /// The set of `(key, document)` to evaluate a geo predicate against: the
    /// indexed candidate set for `bbox` if a spatial index serves it, else the
    /// full collection scan. `bbox` is `(min_lat, min_lon, max_lat, max_lon)`.
    fn geo_scan_set(
        &self,
        field: &str,
        bbox: Option<(f64, f64, f64, f64)>,
    ) -> Result<Vec<(Vec<u8>, Value)>> {
        if let Some((min_lat, min_lon, max_lat, max_lon)) = bbox
            && let Some(keys) =
                self.db()
                    .geo_candidates(self.name(), field, min_lat, min_lon, max_lat, max_lon)?
        {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(doc) = self.get(&key)? {
                    out.push((key, doc));
                }
            }
            return Ok(out);
        }
        self.scan()
    }

    /// Find documents whose `field` point falls within the bounding box
    /// `[min_lat, max_lat] × [min_lon, max_lon]`, in key order.
    pub fn geo_within_bbox(
        &self,
        field: &str,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
    ) -> Result<Vec<(Vec<u8>, Value)>> {
        let scanned = self.geo_scan_set(field, Some((min_lat, min_lon, max_lat, max_lon)))?;
        let mut out = Vec::new();
        for (key, document) in scanned {
            if let Some((lat, lon)) = document.get(field).and_then(extract_point)
                && (min_lat..=max_lat).contains(&lat)
                && (min_lon..=max_lon).contains(&lon)
            {
                out.push((key, document));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, Value};
    use std::collections::BTreeMap;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    fn point_array(lat: f64, lon: f64) -> Value {
        Value::Array(vec![Value::Float(lat), Value::Float(lon)])
    }

    fn point_map(lat: f64, lon: f64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("lat".to_owned(), Value::Float(lat));
        m.insert("lon".to_owned(), Value::Float(lon));
        Value::Map(m)
    }

    #[test]
    fn haversine_known_distance() {
        // London (51.5074, -0.1278) to Paris (48.8566, 2.3522) ≈ 343 km.
        let d = haversine_km(51.5074, -0.1278, 48.8566, 2.3522);
        assert!(approx(d, 343.0, 5.0), "got {d}");
    }

    #[test]
    fn haversine_zero_for_same_point() {
        assert!(approx(haversine_km(10.0, 20.0, 10.0, 20.0), 0.0, 1e-9));
    }

    #[test]
    fn extract_point_from_array_and_map() {
        assert_eq!(extract_point(&point_array(1.0, 2.0)), Some((1.0, 2.0)));
        assert_eq!(extract_point(&point_map(3.0, 4.0)), Some((3.0, 4.0)));
        // Int coordinates are accepted.
        assert_eq!(
            extract_point(&Value::Array(vec![Value::Int(5), Value::Int(6)])),
            Some((5.0, 6.0))
        );
        // Wrong shapes.
        assert_eq!(extract_point(&Value::Array(vec![Value::Float(1.0)])), None);
        assert_eq!(extract_point(&Value::Int(1)), None);
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        // A few London-area points and one far away.
        c.insert(b"london", &place("London", 51.5074, -0.1278))
            .unwrap();
        c.insert(b"greenwich", &place("Greenwich", 51.4779, 0.0015))
            .unwrap();
        c.insert(b"paris", &place("Paris", 48.8566, 2.3522))
            .unwrap();
        db
    }

    fn place(name: &str, lat: f64, lon: f64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("name".to_owned(), Value::Text(name.to_owned()));
        m.insert("loc".to_owned(), point_array(lat, lon));
        Value::Map(m)
    }

    #[test]
    fn within_radius_returns_sorted_nearby() {
        let db = seed();
        // 50 km around central London: London + Greenwich, not Paris.
        let hits = db
            .collection("places")
            .geo_within_radius("loc", 51.5074, -0.1278, 50.0)
            .unwrap();
        let keys: Vec<_> = hits.iter().map(|h| h.key.clone()).collect();
        assert_eq!(keys, vec![b"london".to_vec(), b"greenwich".to_vec()]);
        assert!(hits[0].distance_km <= hits[1].distance_km);
    }

    #[test]
    fn within_radius_skips_docs_without_point() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        c.insert(b"ok", &place("ok", 51.5, -0.1)).unwrap();
        c.insert(b"nopoint", &Value::Text("no location".into()))
            .unwrap();
        let hits = c.geo_within_radius("loc", 51.5, -0.1, 10.0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, b"ok".to_vec());
    }

    #[test]
    fn within_bbox_filters_by_box() {
        let db = seed();
        // Box roughly around London only.
        let out = db
            .collection("places")
            .geo_within_bbox("loc", 51.0, -1.0, 52.0, 1.0)
            .unwrap();
        let keys: Vec<_> = out.iter().map(|(k, _)| k.clone()).collect();
        assert!(keys.contains(&b"london".to_vec()));
        assert!(keys.contains(&b"greenwich".to_vec()));
        assert!(!keys.contains(&b"paris".to_vec()));
    }

    #[test]
    fn indexed_radius_matches_unindexed() {
        // Same data, one collection indexed, one not → identical results.
        fn fill(c: &crate::Collection) {
            for i in 0..200i64 {
                let lat = 51.0 + (i as f64) * 0.01;
                let lon = -0.5 + (i as f64) * 0.005;
                let mut m = BTreeMap::new();
                m.insert("loc".to_owned(), point_array(lat, lon));
                m.insert("n".to_owned(), Value::Int(i));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("p"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("p");
        fill(&ic);
        ic.create_geo_index("loc").unwrap();

        let a = plain
            .collection("p")
            .geo_within_radius("loc", 51.5, -0.3, 30.0)
            .unwrap();
        let b = indexed
            .collection("p")
            .geo_within_radius("loc", 51.5, -0.3, 30.0)
            .unwrap();
        assert_eq!(a, b);
        assert!(!a.is_empty());

        // bbox parity too.
        let ba = plain
            .collection("p")
            .geo_within_bbox("loc", 51.2, -0.4, 51.8, 0.0)
            .unwrap();
        let bb = indexed
            .collection("p")
            .geo_within_bbox("loc", 51.2, -0.4, 51.8, 0.0)
            .unwrap();
        assert_eq!(ba, bb);
    }

    #[test]
    fn builder_geo_filter_uses_index() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        c.insert(b"london", &place("London", 51.5074, -0.1278))
            .unwrap();
        c.insert(b"paris", &place("Paris", 48.8566, 2.3522))
            .unwrap();
        c.create_geo_index("loc").unwrap();
        let rows = c
            .query()
            .filter(crate::field("loc").within_km(51.5, -0.13, 50.0))
            .run()
            .unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec![b"london".to_vec()]);
    }

    #[test]
    fn empty_radius_excludes_everything_far() {
        let db = seed();
        let hits = db
            .collection("places")
            .geo_within_radius("loc", 0.0, 0.0, 1.0)
            .unwrap();
        assert!(hits.is_empty());
    }
}
