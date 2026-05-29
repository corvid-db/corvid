//! Secondary spatial index for sub-linear geo queries.
//!
//! Without an index, a radius/bbox query scans the whole collection. This
//! stores each point in a fixed-resolution grid cell as a redb key, so a query
//! scans only the cells its bounding box overlaps (plus the points it returns),
//! then verifies each candidate with the exact predicate. Like the scalar
//! index it is always on disk, persists, and returns a verified superset with a
//! cap-fallback to the bounded scan when the query area is too large.
//!
//! ## Encoding
//!
//! A point `(lat, lon)` maps to integer cell coordinates at a fixed `CELL_DEG`
//! resolution. The index key is `lat_cell(u32 BE) ‖ lon_cell(u32 BE) ‖ doc_key`,
//! so all points in a cell share a prefix and a row of cells (fixed `lat_cell`,
//! a `lon_cell` range) is one contiguous key range.
//!
//! Layout in a reserved collection (`__geo__<coll>__<field>`):
//! - `0x00 ‖ doc_key` → `lat_cell(u32 BE) ‖ lon_cell(u32 BE)` (forward map).
//! - `lat_cell ‖ lon_cell ‖ doc_key` → `[]` (the index entry; cell bytes are a
//!   fixed 8-byte prefix, so it never collides with the 1+doc_key forward map
//!   only because that map is read by exact key, never range-scanned).

use crate::db::{Collection, Db};
use crate::error::Result;
use crate::geo::extract_point;
use crate::store::Store;
use crate::value::Value;

/// Reserved collection holding persisted geo-index definitions.
const GEO_DEFS: &str = "__geo_indexes__";

/// Grid resolution in degrees. ~0.1° ≈ 11 km at the equator: a city-scale
/// radius query touches a handful of cells; a continental one exceeds the cap
/// and falls back to a scan.
const CELL_DEG: f64 = 0.1;

/// Max cell rows / candidates a query may touch before falling back to a scan.
const ROW_CAP: u32 = 4096;

const FWD_TAG: u8 = 0x00;

/// Per-database geo-index registry.
#[derive(Default)]
pub(crate) struct GeoState {
    defs: std::collections::HashSet<(String, String)>,
}

pub(crate) fn new_state() -> std::sync::Mutex<GeoState> {
    std::sync::Mutex::new(GeoState::default())
}

pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__geo__{collection}__{field}")
}

// ---- cell encoding ----

fn lat_cell(lat: f64) -> u32 {
    (((lat.clamp(-90.0, 90.0) + 90.0) / CELL_DEG).floor() as i64).clamp(0, u32::MAX as i64) as u32
}

fn lon_cell(lon: f64) -> u32 {
    (((lon.clamp(-180.0, 180.0) + 180.0) / CELL_DEG).floor() as i64).clamp(0, u32::MAX as i64)
        as u32
}

/// `lat_cell ‖ lon_cell` (8 bytes, big-endian, order-preserving).
fn cell_prefix(lat: f64, lon: f64) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&lat_cell(lat).to_be_bytes());
    p[4..8].copy_from_slice(&lon_cell(lon).to_be_bytes());
    p
}

fn row_start(lat_c: u32, lon_c: u32) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&lat_c.to_be_bytes());
    p[4..8].copy_from_slice(&lon_c.to_be_bytes());
    p
}

fn fwd_key(doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + doc_key.len());
    k.push(FWD_TAG);
    k.extend_from_slice(doc_key);
    k
}

// ---- maintenance ----

pub(crate) fn insert(store: &Store, ns: &str, doc_key: &[u8], value: &Value) -> Result<()> {
    store.transaction(|tx| {
        remove_in_txn(tx, ns, doc_key)?;
        if let Some((lat, lon)) = extract_point(value) {
            let cell = cell_prefix(lat, lon);
            let mut idx_key = cell.to_vec();
            idx_key.extend_from_slice(doc_key);
            tx.put(ns, &idx_key, &[])?;
            tx.put(ns, &fwd_key(doc_key), &cell)?;
        }
        Ok(())
    })
}

pub(crate) fn insert_many(store: &Store, ns: &str, items: &[(Vec<u8>, Value)]) -> Result<()> {
    store.transaction(|tx| {
        for (doc_key, value) in items {
            remove_in_txn(tx, ns, doc_key)?;
            if let Some((lat, lon)) = extract_point(value) {
                let cell = cell_prefix(lat, lon);
                let mut idx_key = cell.to_vec();
                idx_key.extend_from_slice(doc_key);
                tx.put(ns, &idx_key, &[])?;
                tx.put(ns, &fwd_key(doc_key), &cell)?;
            }
        }
        Ok(())
    })
}

pub(crate) fn delete(store: &Store, ns: &str, doc_key: &[u8]) -> Result<()> {
    store.transaction(|tx| remove_in_txn(tx, ns, doc_key))
}

fn remove_in_txn(tx: &mut crate::store::WriteBatch<'_>, ns: &str, doc_key: &[u8]) -> Result<()> {
    if let Some(cell) = tx.get(ns, &fwd_key(doc_key))? {
        let mut idx_key = cell;
        idx_key.extend_from_slice(doc_key);
        tx.delete(ns, &idx_key)?;
        tx.delete(ns, &fwd_key(doc_key))?;
    }
    Ok(())
}

// ---- candidate scan ----

const PAGE: usize = 4096;

/// Candidate doc keys whose cell overlaps the bounding box (a verified superset
/// for the caller). Returns `None` when the field isn't indexed or the box
/// spans more than [`ROW_CAP`] cell rows / candidates (fall back to a scan).
/// The box must not wrap the antimeridian (`min_lon <= max_lon`).
fn bbox_candidates(
    store: &Store,
    ns: &str,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
) -> Result<Option<Vec<Vec<u8>>>> {
    let (lat0, lat1) = (lat_cell(min_lat), lat_cell(max_lat));
    let (lon0, lon1) = (lon_cell(min_lon), lon_cell(max_lon));
    if lat1.saturating_sub(lat0) >= ROW_CAP {
        return Ok(None);
    }

    let mut out = Vec::new();
    for lat_c in lat0..=lat1 {
        // One contiguous key range per cell row: [lat_c|lon0 .. lat_c|lon1].
        let start = row_start(lat_c, lon0);
        let mut cursor = start.to_vec();
        loop {
            let page = store.scan_from(ns, &cursor, PAGE)?;
            if page.is_empty() {
                break;
            }
            let mut advanced = false;
            for (key, _) in &page {
                if key.len() < 8 {
                    continue;
                }
                let klat = u32::from_be_bytes(key[0..4].try_into().unwrap());
                let klon = u32::from_be_bytes(key[4..8].try_into().unwrap());
                // Past this row, or past the lon range → done with the row.
                if klat != lat_c || klon > lon1 {
                    advanced = false;
                    break;
                }
                out.push(key[8..].to_vec());
                if out.len() > ROW_CAP as usize {
                    return Ok(None);
                }
                advanced = true;
            }
            if !advanced {
                break;
            }
            cursor = next_after(&page.last().unwrap().0);
        }
    }
    Ok(Some(out))
}

fn next_after(key: &[u8]) -> Vec<u8> {
    let mut k = key.to_vec();
    k.push(0);
    k
}

/// Bounding box (degrees) of a `radius_km` circle around `(lat, lon)`.
/// Returns `None` if the box would wrap the antimeridian or cover all
/// longitudes (caller falls back to a scan — still correct).
pub(crate) fn radius_bbox(lat: f64, lon: f64, radius_km: f64) -> Option<(f64, f64, f64, f64)> {
    const KM_PER_DEG_LAT: f64 = 110.574;
    const KM_PER_DEG_LON_EQ: f64 = 111.320;
    let dlat = radius_km / KM_PER_DEG_LAT;
    let cos = lat.to_radians().cos();
    if cos.abs() < 1e-6 {
        return None; // near a pole: longitude band is unbounded
    }
    let dlon = radius_km / (KM_PER_DEG_LON_EQ * cos);
    if dlon >= 180.0 {
        return None; // spans all longitudes
    }
    let (min_lon, max_lon) = (lon - dlon, lon + dlon);
    if min_lon < -180.0 || max_lon > 180.0 {
        return None; // antimeridian wrap
    }
    Some((lat - dlat, min_lon, lat + dlat, max_lon))
}

impl Db {
    pub(crate) fn load_geo_defs(&self) -> Result<()> {
        let mut state = self.geo().lock().expect("geo lock");
        for (key, _) in self.store().scan(GEO_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                state.defs.insert(def);
            }
        }
        Ok(())
    }

    pub(crate) fn register_geo_index(&self, collection: &str, field: &str) -> Result<()> {
        self.store()
            .put(GEO_DEFS, &def_key(collection, field), b"")?;
        let mut state = self.geo().lock().expect("geo lock");
        state.defs.insert((collection.to_owned(), field.to_owned()));
        Ok(())
    }

    pub(crate) fn geo_on_insert(&self, collection: &str, key: &[u8], doc: &Value) -> Result<()> {
        for field in self.geo_fields(collection) {
            let ns = namespace(collection, &field);
            match doc.get_path(&field) {
                Some(value) => insert(self.store(), &ns, key, value)?,
                None => delete(self.store(), &ns, key)?,
            }
        }
        Ok(())
    }

    pub(crate) fn geo_on_delete(&self, collection: &str, key: &[u8]) -> Result<()> {
        for field in self.geo_fields(collection) {
            delete(self.store(), &namespace(collection, &field), key)?;
        }
        Ok(())
    }

    fn geo_fields(&self, collection: &str) -> Vec<String> {
        let state = self.geo().lock().expect("geo lock");
        state
            .defs
            .iter()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// All geo index definitions (for dump/migrate).
    pub(crate) fn geo_specs(&self) -> Vec<(String, String)> {
        let state = self.geo().lock().expect("geo lock");
        state.defs.iter().cloned().collect()
    }

    pub(crate) fn has_geo_index(&self, collection: &str, field: &str) -> bool {
        let state = self.geo().lock().expect("geo lock");
        state
            .defs
            .contains(&(collection.to_owned(), field.to_owned()))
    }

    /// If `field` has a geo index, a verified superset of doc keys whose point
    /// falls in the bounding box. `None` when not indexed or over the cap.
    pub(crate) fn geo_candidates(
        &self,
        collection: &str,
        field: &str,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_geo_index(collection, field) || min_lon > max_lon {
            return Ok(None);
        }
        let ns = namespace(collection, field);
        bbox_candidates(self.store(), &ns, min_lat, min_lon, max_lat, max_lon)
    }
}

fn def_key(collection: &str, field: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(collection.len() + 1 + field.len());
    k.extend_from_slice(collection.as_bytes());
    k.push(0);
    k.extend_from_slice(field.as_bytes());
    k
}

fn split_def_key(key: &[u8]) -> Option<(String, String)> {
    let pos = key.iter().position(|&b| b == 0)?;
    let coll = std::str::from_utf8(&key[..pos]).ok()?.to_owned();
    let field = std::str::from_utf8(&key[pos + 1..]).ok()?.to_owned();
    Some((coll, field))
}

impl Collection<'_> {
    /// Create (or replace) a spatial index on `field`, backfilling existing
    /// documents. Radius and bbox queries on `field` then scan only the cells
    /// their bounding box overlaps instead of the whole collection. The index
    /// is on disk and persists.
    pub fn create_geo_index(&self, field: &str) -> Result<()> {
        self.db().register_geo_index(self.name(), field)?;
        let ns = namespace(self.name(), field);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = self.db().store().scan_from(self.name(), &cursor, 2048)?;
            if page.is_empty() {
                break;
            }
            let mut batch: Vec<(Vec<u8>, Value)> = Vec::new();
            for (key, bytes) in &page {
                let doc = Value::decode(bytes)?;
                if let Some(value) = doc.get_path(field) {
                    batch.push((key.clone(), value.clone()));
                }
            }
            if !batch.is_empty() {
                insert_many(self.db().store(), &ns, &batch)?;
            }
            cursor = next_after(&page.last().unwrap().0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use std::collections::BTreeMap;

    fn place(lat: f64, lon: f64) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "loc".to_owned(),
            Value::Array(vec![Value::Float(lat), Value::Float(lon)]),
        );
        Value::Map(m)
    }

    fn seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        c.insert(b"london", &place(51.5074, -0.1278)).unwrap();
        c.insert(b"greenwich", &place(51.4779, 0.0015)).unwrap();
        c.insert(b"paris", &place(48.8566, 2.3522)).unwrap();
        c.create_geo_index("loc").unwrap();
        db
    }

    #[test]
    fn radius_bbox_basic() {
        let (mn_lat, mn_lon, mx_lat, mx_lon) = radius_bbox(51.5, -0.13, 50.0).unwrap();
        assert!(mn_lat < 51.5 && mx_lat > 51.5);
        assert!(mn_lon < -0.13 && mx_lon > -0.13);
    }

    #[test]
    fn radius_bbox_falls_back_near_pole_and_large() {
        assert!(radius_bbox(89.9999, 0.0, 50.0).is_none());
        assert!(radius_bbox(0.0, 0.0, 50_000.0).is_none());
    }

    #[test]
    fn candidates_cover_true_matches() {
        let db = seeded();
        // bbox around London: must include london + greenwich, exclude paris.
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0)
            .unwrap()
            .unwrap();
        assert!(got.iter().any(|k| k == b"london"));
        assert!(got.iter().any(|k| k == b"greenwich"));
        assert!(!got.iter().any(|k| k == b"paris"));
    }

    #[test]
    fn unindexed_returns_none() {
        let db = seeded();
        assert!(
            db.geo_candidates("places", "other", 0.0, 0.0, 1.0, 1.0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn wrapping_box_returns_none() {
        let db = seeded();
        // min_lon > max_lon (antimeridian wrap) → fall back.
        assert!(
            db.geo_candidates("places", "loc", 0.0, 170.0, 1.0, -170.0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn maintained_on_overwrite_and_delete() {
        let db = seeded();
        let c = db.collection("places");
        // Move london far away → no longer in the London box.
        c.insert(b"london", &place(0.0, 0.0)).unwrap();
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0)
            .unwrap()
            .unwrap();
        assert!(!got.iter().any(|k| k == b"london"));
        // Delete greenwich → gone.
        c.delete(b"greenwich").unwrap();
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0)
            .unwrap()
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("places");
            c.insert(b"london", &place(51.5074, -0.1278)).unwrap();
            c.create_geo_index("loc").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0)
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"london".to_vec()]);
    }
}
