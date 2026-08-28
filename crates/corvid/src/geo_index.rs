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
use crate::store::SnapshotReader;
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
    /// Geo indexes: `(collection, field)` → whether the index is still
    /// **building** (an interrupted creation). Maintenance iterates all defs;
    /// serviceability requires `building == false`.
    defs: std::collections::HashMap<(String, String), bool>,
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

/// Index (or re-index) `doc_key`'s point inside a caller's transaction.
pub(crate) fn insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
    value: &Value,
) -> Result<()> {
    remove_in_txn(tx, ns, doc_key)?;
    if let Some((lat, lon)) = extract_point(value) {
        let cell = cell_prefix(lat, lon);
        let mut idx_key = cell.to_vec();
        idx_key.extend_from_slice(doc_key);
        tx.put(ns, &idx_key, &[])?;
        tx.put(ns, &fwd_key(doc_key), &cell)?;
    }
    Ok(())
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
/// for the caller), read from `reader`'s snapshot (audit B3). Returns `None`
/// when the field isn't indexed or the box spans more than [`ROW_CAP`] cell
/// rows / candidates (fall back to a scan). The box must not wrap the
/// antimeridian (`min_lon <= max_lon`).
fn bbox_candidates(
    reader: &dyn SnapshotReader,
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
            let page = reader.scan_from(ns, &cursor, PAGE)?;
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
///
/// The longitude half-width uses the exact spherical bound
/// `Δλ = asin(sin δ / cos φ)` (δ = angular radius): a circle's maximal
/// longitude excursion is reached poleward of the center, where parallels
/// shorten, so the naive `δ / cos φ` linear form underestimates the box and
/// silently drops matching documents.
pub(crate) fn radius_bbox(lat: f64, lon: f64, radius_km: f64) -> Option<(f64, f64, f64, f64)> {
    const KM_PER_DEG_LAT: f64 = 110.574;
    let dlat = radius_km / KM_PER_DEG_LAT;
    // A circle a quarter-circumference or more in radius reaches every
    // longitude (and wraps the globe); the asin bound below is only valid
    // for smaller radii.
    if dlat >= 90.0 {
        return None;
    }
    let ang_rad = dlat.to_radians();
    let lat_rad = lat.to_radians();
    let cos_lat = lat_rad.cos();
    // Near-pole centers (or |lat| = 90) have an unbounded longitude band.
    if !cos_lat.is_finite() || cos_lat.abs() < 1e-12 {
        return None;
    }
    let s = ang_rad.sin() / cos_lat;
    if !s.is_finite() || s.abs() >= 1.0 {
        return None; // circle reaches every longitude
    }
    let dlon = s.asin().to_degrees();
    let (min_lon, max_lon) = (lon - dlon, lon + dlon);
    if min_lon < -180.0 || max_lon > 180.0 {
        return None; // antimeridian wrap
    }
    Some((lat - dlat, min_lon, lat + dlat, max_lon))
}

impl Db {
    /// Load persisted geo-index definitions. Called once on open. Legacy rows
    /// without state bytes decode as `Complete`; a `Building` row marks the
    /// index for lazy resume on first use.
    pub(crate) fn load_geo_defs(&self) -> Result<()> {
        let mut state = self.geo().lock().expect("geo lock");
        for (key, value) in self.store().scan(GEO_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                // Kind bytes are unused for geo defs (empty).
                let (_, st) = crate::index_build::decode_def(&value);
                state.defs.insert(
                    def,
                    matches!(st, crate::index_build::DefState::Building { .. }),
                );
            }
        }
        Ok(())
    }

    /// Register (or replace) a geo index on `field` for `collection`: the
    /// def row becomes `Building` (empty cursor) so a crash between
    /// registration and backfill completion leaves a never-served, resumable
    /// state. An in-flight `Building` row keeps its cursor, so a
    /// re-registration resumes the interrupted backfill instead of rescanning.
    pub(crate) fn register_geo_index(&self, collection: &str, field: &str) -> Result<()> {
        let key = def_key(collection, field);
        let in_flight =
            crate::index_build::read_building_cursor(self.store(), GEO_DEFS, &key)?.is_some();
        if !in_flight {
            self.store().put(
                GEO_DEFS,
                &key,
                &crate::index_build::encode_def(
                    &[],
                    &crate::index_build::DefState::Building { cursor: vec![] },
                ),
            )?;
        }
        let mut state = self.geo().lock().expect("geo lock");
        state
            .defs
            .insert((collection.to_owned(), field.to_owned()), true);
        Ok(())
    }

    /// Maintain every geo index on `collection` inside the caller's write
    /// transaction.
    pub(crate) fn geo_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        for field in self.geo_fields(collection) {
            let ns = namespace(collection, &field);
            match doc.get_path(&field) {
                Some(value) => insert_in_txn(tx, &ns, key, value)?,
                None => remove_in_txn(tx, &ns, key)?,
            }
        }
        Ok(())
    }

    /// Remove `key` from every geo index on `collection` inside the caller's
    /// write transaction.
    pub(crate) fn geo_on_delete_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        for field in self.geo_fields(collection) {
            remove_in_txn(tx, &namespace(collection, &field), key)?;
        }
        Ok(())
    }

    /// Every field of `collection` with a geo index, building or complete —
    /// maintenance must keep all of them current so a resumed backfill and
    /// concurrent writes overlap safely (idempotent upserts).
    fn geo_fields(&self, collection: &str) -> Vec<String> {
        let state = self.geo().lock().expect("geo lock");
        state
            .defs
            .keys()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// All geo index definitions (for dump/migrate). State is intentionally
    /// dropped: dump/load replays creation, materializing each def as
    /// `Complete`.
    pub(crate) fn geo_specs(&self) -> Vec<(String, String)> {
        let state = self.geo().lock().expect("geo lock");
        state.defs.keys().cloned().collect()
    }

    /// Whether `field` of `collection` has a **complete** geo index. A
    /// building index is never serviceable: geo queries conservatively fall
    /// back (the first probe resumes the build).
    pub(crate) fn has_geo_index(&self, collection: &str, field: &str) -> bool {
        let state = self.geo().lock().expect("geo lock");
        state
            .defs
            .get(&(collection.to_owned(), field.to_owned()))
            .is_some_and(|building| !*building)
    }

    /// Flip a geo index's in-memory def to complete after its backfill
    /// committed `Complete` on disk.
    pub(crate) fn mark_geo_complete(&self, collection: &str, field: &str) {
        let mut state = self.geo().lock().expect("geo lock");
        state
            .defs
            .insert((collection.to_owned(), field.to_owned()), false);
    }

    /// Building geo defs of `collection` as `(field, cursor)` jobs, read from
    /// the def rows (disk is the resume truth after a crash).
    pub(crate) fn collect_building_geo(&self, collection: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let mut jobs = Vec::new();
        for (key, value) in self.store().scan(GEO_DEFS)? {
            let Some((coll, field)) = split_def_key(&key) else {
                continue;
            };
            if coll != collection {
                continue;
            }
            if let crate::index_build::DefState::Building { cursor } =
                crate::index_build::decode_def(&value).1
            {
                jobs.push((field, cursor));
            }
        }
        Ok(jobs)
    }

    /// (Re-)run the atomic backfill for one geo index from `cursor`, then mark
    /// it complete — the exact driver invocation `create_geo_index` uses,
    /// shared with lazy resumes.
    pub(crate) fn resume_geo(&self, collection: &str, field: &str, cursor: &[u8]) -> Result<()> {
        let ns = namespace(collection, field);
        let kb: Vec<u8> = Vec::new();
        crate::index_build::run_atomic_backfill(
            self.store(),
            collection,
            GEO_DEFS,
            &def_key(collection, field),
            &kb,
            cursor,
            &mut |tx, page| {
                for (key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    if let Some(value) = doc.get_path(field) {
                        insert_in_txn(tx, &ns, key, value)?;
                    }
                }
                Ok(())
            },
        )?;
        self.mark_geo_complete(collection, field);
        Ok(())
    }

    /// If `field` has a geo index, a verified superset of doc keys whose point
    /// falls in the bounding box, read from `reader`'s snapshot (audit B3).
    /// `None` when not indexed or over the cap. Interrupted builds are NOT
    /// resumed here (resuming writes): the caller resumes before its snapshot
    /// opens.
    #[allow(clippy::too_many_arguments)] // the bbox travels as its four bounds
    pub(crate) fn geo_candidates(
        &self,
        collection: &str,
        field: &str,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.has_geo_index(collection, field) || min_lon > max_lon {
            return Ok(None);
        }
        let ns = namespace(collection, field);
        bbox_candidates(reader, &ns, min_lat, min_lon, max_lat, max_lon)
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
    ///
    /// Atomic and crash-safe (audit A2): the def is registered `Building`
    /// before any backfill work; every page's index writes and cursor advance
    /// commit in one transaction; completion is its own final transaction. A
    /// crash or error leaves a resumable `Building` def that queries never
    /// serve — the first geo query (or a re-creation) resumes it.
    pub fn create_geo_index(&self, field: &str) -> Result<()> {
        self.db().register_geo_index(self.name(), field)?;
        // A def still Building from an interrupted creation resumes from its
        // saved cursor; a Complete (or fresh) def backfills from the start.
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            GEO_DEFS,
            &def_key(self.name(), field),
        )?;
        self.db()
            .resume_geo(self.name(), field, &cursor.unwrap_or_default())
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

    /// Regression: the longitude extent must be governed by the poleward
    /// reach of the circle, not the center parallel. Center (60°, 60°),
    /// r = 1000 km — the point (62°, 78°) is 992 km away (a match), but its
    /// longitude exceeds the naive `δ / cos(center)` box.
    #[test]
    fn radius_bbox_covers_poleward_longitude_extent() {
        let (mn_lat, mn_lon, mx_lat, mx_lon) = radius_bbox(60.0, 60.0, 1000.0).unwrap();
        // Exact bound: asin(sin δ / cos φ).
        let dlat: f64 = 1000.0 / 110.574;
        let expected_dlon = (dlat.to_radians().sin() / 60.0f64.to_radians().cos())
            .asin()
            .to_degrees();
        assert!((mx_lon - (60.0 + expected_dlon)).abs() < 1e-9);
        assert!((mn_lon - (60.0 - expected_dlon)).abs() < 1e-9);
        // The previously-dropped match now lies inside the box...
        assert!(mn_lat <= 62.0 && 62.0 <= mx_lat);
        assert!(mn_lon <= 78.0 && 78.0 <= mx_lon);
        // ...and is genuinely within the radius.
        assert!(crate::geo::haversine_km(60.0, 60.0, 62.0, 78.0) <= 1000.0);
    }

    #[test]
    fn candidates_cover_true_matches() {
        let db = seeded();
        // bbox around London: must include london + greenwich, exclude paris.
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
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
            db.geo_candidates("places", "other", 0.0, 0.0, 1.0, 1.0, db.store())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn wrapping_box_returns_none() {
        let db = seeded();
        // min_lon > max_lon (antimeridian wrap) → fall back.
        assert!(
            db.geo_candidates("places", "loc", 0.0, 170.0, 1.0, -170.0, db.store())
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
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
            .unwrap()
            .unwrap();
        assert!(!got.iter().any(|k| k == b"london"));
        // Delete greenwich → gone.
        c.delete(b"greenwich").unwrap();
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
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
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"london".to_vec()]);
    }

    /// A building geo index is never served: geo queries fall back to a scan
    /// and stay correct; the first such query resumes the build.
    #[test]
    fn building_geo_index_falls_back_then_resumes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        c.insert(b"london", &place(51.5074, -0.1278)).unwrap();
        c.insert(b"greenwich", &place(51.4779, 0.0015)).unwrap();
        c.insert(b"paris", &place(48.8566, 2.3522)).unwrap();
        // Forge a Building def exactly as an interrupted creation would leave it.
        db.register_geo_index("places", "loc").unwrap(); // registers Building
        assert!(
            !db.has_geo_index("places", "loc"),
            "building def must not be serviceable"
        );
        // Before resume: a radius query must still be correct (scan fallback;
        // the query itself resumes the build).
        let hits = c.geo_within_radius("loc", 51.5, -0.13, 50.0).unwrap();
        let mut keys: Vec<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![b"greenwich".to_vec(), b"london".to_vec()]);
        // After the resume the def is complete and serviceable.
        assert!(db.has_geo_index("places", "loc"));
        // And the builder's geo filter drives off the resumed index.
        let rows = c
            .query()
            .filter(crate::field("loc").within_km(51.5, -0.13, 50.0))
            .run()
            .unwrap();
        let mut keys: Vec<Vec<u8>> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![b"greenwich".to_vec(), b"london".to_vec()]);
    }

    /// Contention: while another thread holds the resume lock mid-backfill, a
    /// geo query must not serve the building index — it falls back to an
    /// exact scan and stays correct, and the def stays building; once the
    /// lock is free, the next query resumes the build and serves.
    #[test]
    fn building_geo_index_with_resume_lock_held_falls_back() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("places");
        c.insert(b"london", &place(51.5074, -0.1278)).unwrap();
        c.insert(b"greenwich", &place(51.4779, 0.0015)).unwrap();
        c.insert(b"paris", &place(48.8566, 2.3522)).unwrap();
        // Forge a Building def exactly as an interrupted creation would leave it.
        db.register_geo_index("places", "loc").unwrap();
        // With the resume lock held (another thread resuming), the building
        // def must not be served: geo_candidates reports "no usable index"...
        let _guard = db.index_resume().lock().unwrap();
        assert!(
            db.geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
                .unwrap()
                .is_none(),
            "a building geo index must not be served"
        );
        // ...so the radius query falls back to an exact scan and stays
        // correct, while the contended resume never runs (def still building).
        let hits = c.geo_within_radius("loc", 51.5, -0.13, 50.0).unwrap();
        let mut keys: Vec<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![b"greenwich".to_vec(), b"london".to_vec()]);
        assert!(!db.has_geo_index("places", "loc"));
        assert_eq!(db.collect_building_geo("places").unwrap().len(), 1);
        drop(_guard);
        // Once the resume lock is free, the next query resumes the backfill
        // (resumes live at the query entry points now, audit B3) and the
        // completed index serves.
        let hits = c.geo_within_radius("loc", 51.5, -0.13, 50.0).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(db.has_geo_index("places", "loc"));
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
            .unwrap()
            .unwrap();
        assert!(got.iter().any(|k| k == b"london"));
        assert!(got.iter().any(|k| k == b"greenwich"));
        assert!(!got.iter().any(|k| k == b"paris"));
        assert!(db.has_geo_index("places", "loc"));
    }

    #[test]
    fn legacy_stateless_geo_def_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("places");
            c.insert(b"london", &place(51.5074, -0.1278)).unwrap();
            c.create_geo_index("loc").unwrap();
            // Overwrite the def row with the legacy empty form.
            db.store().put(GEO_DEFS, b"places\x00loc", b"").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.has_geo_index("places", "loc")); // legacy → Complete → serviceable
        assert!(db.collect_building_geo("places").unwrap().is_empty());
        let got = db
            .geo_candidates("places", "loc", 51.0, -1.0, 52.0, 1.0, db.store())
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![b"london".to_vec()]);
    }
}
