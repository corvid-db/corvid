//! Geo queries & the geohits cursor (spec §4.12).
//!
//! `geo_within_radius` / `geo_within_bbox` / `geo_nearest` wrap the
//! engine's haversine queries (geo.rs) and return `corvid_geohits*`
//! cursors: nearest-first for radius/nearest (ties by key), KEY order
//! for bbox. A location field holds `[lat, lon]` (array) or a `lat`/
//! `lon` map; documents without a valid point are skipped. Distances are
//! haversine kilometres (spherical Earth) — bbox hits carry the
//! documented **0.0 sentinel** instead (the box query has no center; the
//! engine returns no distance). Bounds are validated at entry (latitude
//! `[-90, 90]`, longitude `[-180, 180]`, NaN rejected, inverted latitude
//! rejected) with `CORVID_E_ARGUMENT`.
//!
//! `corvid_geohits_next` walks either producer shape: geo cursors hand
//! out the borrowed full document via `doc_out`; cursors from
//! `corvid_neighbors_weighted` (graph.rs) carry `(key, weight)` pairs
//! and read `doc_out = NULL`.

use std::ffi::c_char;
use std::ffi::c_int;

use crate::error::guard;
use crate::error::record_argument;
use crate::handle::GeoHitsHandle;
use crate::handle::borrow_coll;
use crate::handle::borrow_geohits_mut;
use crate::handle::corvid_coll;
use crate::handle::corvid_geohits;
use crate::handle::corvid_value;
use crate::handle::into_geohits;
use crate::handle::reclaim_geohits;
use crate::value::borrowed_utf8;

/// One geospatial / weighted hit (spec §1.2, POD): the output shape of
/// [`corvid_geohits_next`]. `key` is BORROWED until the next
/// `corvid_geohits_next` or `corvid_geohits_free` on that handle.
#[repr(C)]
pub struct corvid_geohit {
    /// The hit's key (a document key; arbitrary bytes, §1.5).
    pub key: *const u8,
    /// Key bytes.
    pub key_len: usize,
    /// Geo: kilometres from the query point (haversine). Bbox: the 0.0
    /// sentinel (no center). `neighbors_weighted`: the edge weight.
    pub distance_km: f64,
}

/// The §7 NULL-checked shared coll borrow (the read.rs/mutation.rs twin,
/// local to this module like its siblings).
fn borrow_coll_checked<'a>(
    fn_name: &str,
    c: *mut corvid_coll,
) -> Option<&'a crate::handle::CollHandle> {
    if c.is_null() {
        record_argument(&format!("{fn_name}: c is NULL"));
        return None;
    }
    // SAFETY: c is non-NULL (checked) with corvid_collection provenance,
    // not yet freed; the coll family is thread-safe (spec §2), so a
    // shared borrow is fine.
    unsafe { borrow_coll(c) }
}

/// Documents whose `field` point lies within `radius_km` (INCLUSIVE) of
/// `(lat, lon)`, nearest first, ties by key (spec §4.12; counterpart:
/// `Collection::geo_within_radius`). Documents lacking a valid point are
/// skipped. Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_geo_within_radius(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    lat: f64,
    lon: f64,
    radius_km: f64,
) -> *mut corvid_geohits {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_geo_within_radius", c),
        borrowed_utf8("corvid_geo_within_radius", "field", field, field_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_geo_within_radius", || {
        coll.collection()
            .geo_within_radius(field, lat, lon, radius_km)
    }) {
        Some(hits) => into_geohits(GeoHitsHandle::from_geo(hits)),
        None => std::ptr::null_mut(),
    }
}

/// Documents whose `field` point lies inside the box
/// `[min_lat, max_lat] × [min_lon, max_lon]`, in KEY order (spec §4.12;
/// counterpart: `Collection::geo_within_bbox`). Bounds are validated at
/// entry — latitude `[-90, 90]`, longitude `[-180, 180]`, NaN rejected,
/// inverted latitude rejected — with `CORVID_E_ARGUMENT`;
/// `min_lon > max_lon` wraps the antimeridian (both ranges, exact,
/// unaccelerated). The hits' `distance_km` is the 0.0 sentinel. Returns
/// NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_geo_within_bbox(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
) -> *mut corvid_geohits {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_geo_within_bbox", c),
        borrowed_utf8("corvid_geo_within_bbox", "field", field, field_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_geo_within_bbox", || {
        coll.collection()
            .geo_within_bbox(field, min_lat, min_lon, max_lat, max_lon)
    }) {
        Some(hits) => into_geohits(GeoHitsHandle::from_bbox(hits)),
        None => std::ptr::null_mut(),
    }
}

/// The true `k` nearest documents by `field` point, nearest first (spec
/// §4.12; counterpart: `Collection::geo_nearest` — expanding radius,
/// exact): fewer than `k` only when fewer valid points exist; `k == 0`
/// yields nothing. Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_geo_nearest(
    c: *mut corvid_coll,
    field: *const c_char,
    field_len: usize,
    lat: f64,
    lon: f64,
    k: usize,
) -> *mut corvid_geohits {
    let (Some(coll), Some(field)) = (
        borrow_coll_checked("corvid_geo_nearest", c),
        borrowed_utf8("corvid_geo_nearest", "field", field, field_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_geo_nearest", || {
        coll.collection().geo_nearest(field, lat, lon, k)
    }) {
        Some(hits) => into_geohits(GeoHitsHandle::from_geo(hits)),
        None => std::ptr::null_mut(),
    }
}

/// Advance the cursor (spec §4.12): returns 1 and fills `*out` (and
/// `*doc_out` when that out-param is non-NULL — it is nullable, §7) for
/// the next hit, 0 at exhaustion — out-params untouched at 0; never
/// errors (the list is materialized). `out->key` and the document are
/// BORROWED until the next `corvid_geohits_next` or
/// `corvid_geohits_free` on this handle — using or freeing them after
/// either is UB. **Cursors from `corvid_neighbors_weighted` set
/// `*doc_out = NULL`** (the engine returns `(key, weight)` pairs with no
/// document) and still return 1.
///
/// NULL handle or NULL out-parameter follows the non-status rule (spec
/// §7): defined inert value (0 = exhausted) AND `CORVID_E_ARGUMENT`
/// recorded — never UB, and never a status return (there is none).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_geohits_next(
    h: *mut corvid_geohits,
    out: *mut corvid_geohit,
    doc_out: *mut *const corvid_value,
) -> c_int {
    if h.is_null() || out.is_null() {
        record_argument("corvid_geohits_next: NULL handle or out-param (§7 inert rule)");
        return 0;
    }
    // SAFETY: handle non-NULL (checked) with a geo-query or
    // neighbors_weighted provenance, not yet freed; the §2 contract
    // confines a cursor to one thread, so the exclusive borrow is sound.
    let cursor = unsafe { borrow_geohits_mut(h) }.expect("non-NULL checked above");
    match cursor.next() {
        Some((key, distance_km, document)) => {
            // SAFETY: out is non-NULL (checked); doc_out is the nullable
            // optional out-param (§7) — written only when the caller
            // asked for it. The document pointer is an interior borrow of
            // the entry's Value (the rows-family borrowed shape).
            unsafe {
                *out = corvid_geohit {
                    key: key.as_ptr(),
                    key_len: key.len(),
                    distance_km,
                };
                if !doc_out.is_null() {
                    *doc_out = match document {
                        Some(doc) => doc as *const corvid::Value as *const corvid_value,
                        None => std::ptr::null(),
                    };
                }
            }
            1
        }
        None => 0,
    }
}

/// Free the cursor (spec §4.12). `corvid_geohits_free(NULL)` is a no-op
/// (spec §7). Cross-family frees are UB (spec §2).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_geohits_free(h: *mut corvid_geohits) {
    // SAFETY: NULL is the documented no-op; otherwise h is a geo-query
    // or neighbors_weighted product, reclaimed exactly once here.
    drop(unsafe { reclaim_geohits(h) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_err;
    use crate::error::corvid_status::CORVID_OK;
    use crate::error::last_code;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open_memory;
    use crate::mutation::corvid_insert;
    use crate::value::corvid_value_float;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_len;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;

    type Coll = *mut corvid_coll;

    /// (pointer, length) for a borrowed UTF-8 parameter (§1.5).
    fn s(text: &str) -> (*const c_char, usize) {
        (text.as_ptr() as *const c_char, text.len())
    }

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let (name, len) = s("places");
        let coll = corvid_collection(db, name, len);
        assert!(!coll.is_null());
        (db, coll)
    }

    /// A doc with a `[lat, lon]` array location at field "loc" (the
    /// engine's extract_point reads an Array of two numbers — geo.rs).
    fn place(lat: f64, lon: f64) -> *mut corvid_value {
        use crate::value::corvid_value_array_new;
        use crate::value::corvid_value_array_push;
        let map = corvid_value_map_new();
        let (key, key_len) = s("loc");
        let point = corvid_value_array_new();
        assert_eq!(
            corvid_value_array_push(point, corvid_value_float(lat)),
            CORVID_OK
        );
        assert_eq!(
            corvid_value_array_push(point, corvid_value_float(lon)),
            CORVID_OK
        );
        assert_eq!(corvid_value_map_put(map, key, key_len, point), CORVID_OK);
        map
    }

    /// A doc with a `{lat, lon}` map location at field "loc".
    fn place_map(lat: f64, lon: f64) -> *mut corvid_value {
        let map = corvid_value_map_new();
        let (key, key_len) = s("loc");
        let inner = corvid_value_map_new();
        for (k, v) in [("lat", lat), ("lon", lon)] {
            let (ik, ik_len) = s(k);
            assert_eq!(
                corvid_value_map_put(inner, ik, ik_len, corvid_value_float(v)),
                CORVID_OK
            );
        }
        assert_eq!(corvid_value_map_put(map, key, key_len, inner), CORVID_OK);
        map
    }

    fn insert(coll: Coll, key: &[u8], document: *mut corvid_value) {
        assert_eq!(
            corvid_insert(coll, key.as_ptr(), key.len(), document),
            CORVID_OK
        );
        corvid_value_free(document);
    }

    /// Walk a geohits cursor: `(key, distance)` pairs, asserting the
    /// doc was present when `with_doc` and NULL otherwise.
    fn walk(hits: *mut corvid_geohits, with_doc: bool) -> Vec<(Vec<u8>, f64)> {
        let mut out = Vec::new();
        loop {
            let mut hit = corvid_geohit {
                key: std::ptr::null(),
                key_len: 0,
                distance_km: f64::NAN,
            };
            let mut doc: *const corvid_value = std::ptr::null();
            if corvid_geohits_next(hits, &mut hit, &mut doc) != 1 {
                return out;
            }
            assert_eq!(!doc.is_null(), with_doc, "doc_out presence");
            if with_doc {
                // The borrowed doc is this hit's stored document.
                assert_eq!(corvid_value_len(doc), 1, "one top-level field (loc)");
            }
            // SAFETY: hit.key borrows the cursor's current entry, read
            // before the next corvid_geohits_next.
            let key = unsafe { std::slice::from_raw_parts(hit.key, hit.key_len) }.to_vec();
            out.push((key, hit.distance_km));
        }
    }

    /// The corpus (all within a few hundred km of (52.52, 13.40)):
    /// berlin (0 km), potsdam (~27 km sw), hamburg (~255 km nw),
    /// munchen (~505 km s), plus two non-geo docs.
    fn corpus(coll: Coll) {
        insert(coll, b"berlin", place(52.52, 13.40));
        insert(coll, b"hamburg", place(53.55, 9.99));
        insert(coll, b"munchen", place(48.14, 11.58));
        insert(coll, b"potsdam", place_map(52.40, 13.06)); // the map form
        insert(coll, b"no-loc", {
            let m = corvid_value_map_new();
            let (k, kl) = s("n");
            corvid_value_map_put(m, k, kl, corvid_value_int(1));
            m
        });
        insert(coll, b"bad-loc", place(f64::NAN, 13.40)); // skipped
    }

    /// Radius: nearest-first with ties by key, the INCLUSIVE boundary,
    /// and both point encodings; invalid points are skipped.
    #[test]
    fn radius_is_nearest_first_with_inclusive_boundary() {
        let (db, coll) = fresh();
        corpus(coll);
        let (field, field_len) = s("loc");

        // Wide radius: nearest first.
        let hits = corvid_geo_within_radius(coll, field, field_len, 52.52, 13.40, 600.0);
        assert!(!hits.is_null());
        let walked = walk(hits, true);
        assert_eq!(
            walked.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![
                b"berlin".to_vec(),
                b"potsdam".to_vec(),
                b"hamburg".to_vec(),
                b"munchen".to_vec()
            ],
            "nearest first; the map-encoded point counts; invalid points skip"
        );
        // Distances: berlin 0, monotonically increasing.
        assert_eq!(walked[0].1, 0.0);
        assert!(
            walked[1].1 < walked[2].1 && walked[2].1 < walked[3].1,
            "monotone: {:?}",
            walked
        );
        // The engine's own haversine (pub) matches the reported distance.
        assert_eq!(
            walked[1].1,
            corvid::haversine_km(52.52, 13.40, 52.40, 13.06)
        );
        corvid_geohits_free(hits);

        // The INCLUSIVE boundary: radius == the exact distance includes
        // the point; one ulp below excludes it.
        let potsdam_km = corvid::haversine_km(52.52, 13.40, 52.40, 13.06);
        let hits = corvid_geo_within_radius(coll, field, field_len, 52.52, 13.40, potsdam_km);
        let keys: Vec<Vec<u8>> = walk(hits, true).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"berlin".to_vec(), b"potsdam".to_vec()]);
        corvid_geohits_free(hits);
        let hits = corvid_geo_within_radius(
            coll,
            field,
            field_len,
            52.52,
            13.40,
            f64::from_bits(potsdam_km.to_bits() - 1),
        );
        let keys: Vec<Vec<u8>> = walk(hits, true).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"berlin".to_vec()], "strictly outside: excluded");
        corvid_geohits_free(hits);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// Radius ties break by KEY (the Task 6 review prepend): two points
    /// at the exact same haversine distance from the query center —
    /// (0, 10) and (0, −10) mirror across the meridian, so the squared
    /// sin/cos terms are bitwise equal — come back in key order, with
    /// bitwise-equal distances. The nearest-`k` tie is pinned in
    /// `nearest_is_exact_with_key_tiebreak`; this is the radius form.
    #[test]
    fn radius_ties_break_by_key() {
        let (db, coll) = fresh();
        let (field, field_len) = s("loc");
        insert(coll, b"west", place(0.0, -10.0));
        insert(coll, b"east", place(0.0, 10.0));
        insert(coll, b"far", place(0.0, 90.0));

        let hits = corvid_geo_within_radius(coll, field, field_len, 0.0, 0.0, 12000.0);
        assert!(!hits.is_null());
        let walked = walk(hits, true);
        assert_eq!(
            walked.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![b"east".to_vec(), b"west".to_vec(), b"far".to_vec()],
            "the equidistant pair in key order (east < west), then far"
        );
        assert_eq!(
            walked[0].1, walked[1].1,
            "the tie is exact: haversine is mirror-symmetric, bitwise"
        );
        assert_eq!(walked[0].1, corvid::haversine_km(0.0, 0.0, 0.0, 10.0));
        corvid_geohits_free(hits);

        // A radius that reaches ONLY the tied pair keeps the key order
        // and the inclusive boundary.
        let pair_km = corvid::haversine_km(0.0, 0.0, 0.0, 10.0);
        let hits = corvid_geo_within_radius(coll, field, field_len, 0.0, 0.0, pair_km);
        let keys: Vec<Vec<u8>> = walk(hits, true).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"east".to_vec(), b"west".to_vec()]);
        corvid_geohits_free(hits);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// bbox: key order, the 0.0 distance sentinel, entry validation
    /// (lat/lon ranges, NaN, inverted latitude → `CORVID_E_ARGUMENT`),
    /// and the antimeridian wrap.
    #[test]
    fn bbox_is_key_order_with_sentinel_and_validated_bounds() {
        let (db, coll) = fresh();
        corpus(coll);
        let (field, field_len) = s("loc");

        // Germany-shaped box: all four, in KEY order, sentinel 0.0.
        let hits = corvid_geo_within_bbox(coll, field, field_len, 47.0, 5.0, 55.0, 15.0);
        assert!(!hits.is_null());
        assert_eq!(
            walk(hits, true),
            vec![
                (b"berlin".to_vec(), 0.0),
                (b"hamburg".to_vec(), 0.0),
                (b"munchen".to_vec(), 0.0),
                (b"potsdam".to_vec(), 0.0),
            ],
            "key order; the 0.0 sentinel"
        );
        corvid_geohits_free(hits);

        // A narrow box: one hit.
        let hits = corvid_geo_within_bbox(coll, field, field_len, 53.0, 9.0, 54.0, 10.5);
        assert_eq!(walk(hits, true), vec![(b"hamburg".to_vec(), 0.0)]);
        corvid_geohits_free(hits);

        // Antimeridian wrap: min_lon > max_lon matches BOTH ranges.
        insert(coll, b"fiji", place(-17.7, 178.0));
        insert(coll, b"samoa", place(-13.8, -172.1));
        let hits = corvid_geo_within_bbox(coll, field, field_len, -30.0, 170.0, 0.0, -170.0);
        assert_eq!(
            walk(hits, true),
            vec![(b"fiji".to_vec(), 0.0), (b"samoa".to_vec(), 0.0)],
            "wrapped: lon >= 170 OR lon <= -170"
        );
        corvid_geohits_free(hits);

        // Entry validation: each bad shape is E_ARGUMENT.
        for (mnla, mnlo, mxla, mxlo, why) in [
            (91.0, 0.0, 90.0, 0.0, "max lat out of range"),
            (0.0, -181.0, 0.0, 0.0, "min lon out of range"),
            (0.0, 0.0, 0.0, 180.5, "max lon out of range"),
            (f64::NAN, 0.0, 0.0, 0.0, "NaN rejected"),
            (0.0, 0.0, f64::NAN, 0.0, "NaN rejected"),
            (10.0, 0.0, 5.0, 0.0, "inverted latitude rejected"),
        ] {
            assert!(
                corvid_geo_within_bbox(coll, field, field_len, mnla, mnlo, mxla, mxlo).is_null(),
                "{why}"
            );
            assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT, "{why}");
        }

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// nearest: exact k ordering, the tie broken by key, k == 0 empty,
    /// k beyond the corpus truncated to what exists.
    #[test]
    fn nearest_is_exact_with_key_tiebreak() {
        let (db, coll) = fresh();
        let (field, field_len) = s("loc");

        // Two points at the SAME distance from (0, 0): (0, 10) and
        // (0, -10) — the tie breaks by key.
        insert(coll, b"west", place(0.0, -10.0));
        insert(coll, b"east", place(0.0, 10.0));
        insert(coll, b"far", place(0.0, 90.0));

        let hits = corvid_geo_nearest(coll, field, field_len, 0.0, 0.0, 3);
        assert!(!hits.is_null());
        assert_eq!(
            walk(hits, true),
            vec![
                (b"east".to_vec(), corvid::haversine_km(0.0, 0.0, 0.0, 10.0)),
                (b"west".to_vec(), corvid::haversine_km(0.0, 0.0, 0.0, -10.0)),
                (b"far".to_vec(), corvid::haversine_km(0.0, 0.0, 0.0, 90.0)),
            ],
            "ties by key (east < west in byte order)"
        );
        corvid_geohits_free(hits);

        // k == 0: an empty cursor (not NULL — NULL means error).
        let hits = corvid_geo_nearest(coll, field, field_len, 0.0, 0.0, 0);
        assert!(!hits.is_null());
        assert!(walk(hits, true).is_empty());
        corvid_geohits_free(hits);

        // k beyond the corpus: everything, still ordered.
        let hits = corvid_geo_nearest(coll, field, field_len, 0.0, 0.0, 50);
        assert_eq!(walk(hits, true).len(), 3);
        corvid_geohits_free(hits);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The cursor family's §7 rule (inert 0 + E_ARGUMENT, exhaustion
    /// leaves out-params untouched) and the NULL-coll/field discipline
    /// of the three queries.
    #[test]
    fn geo_family_null_discipline() {
        let (db, coll) = fresh();
        insert(coll, b"a", place(1.0, 1.0));
        let (field, field_len) = s("loc");

        // Queries: NULL coll / NULL field → NULL cursor + E_ARGUMENT.
        assert!(
            corvid_geo_within_radius(std::ptr::null_mut(), field, field_len, 0.0, 0.0, 1.0)
                .is_null()
        );
        assert!(corvid_geo_within_radius(coll, std::ptr::null(), 0, 0.0, 0.0, 1.0).is_null());
        assert!(
            corvid_geo_within_bbox(std::ptr::null_mut(), field, field_len, 0.0, 0.0, 1.0, 1.0)
                .is_null()
        );
        assert!(corvid_geo_nearest(std::ptr::null_mut(), field, field_len, 0.0, 0.0, 1).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // next: NULL handle / NULL out → 0 + E_ARGUMENT; exhaustion
        // leaves out-params untouched; doc_out is nullable.
        let mut hit = corvid_geohit {
            key: std::ptr::null(),
            key_len: 0,
            distance_km: f64::NAN,
        };
        assert_eq!(
            corvid_geohits_next(std::ptr::null_mut(), &mut hit, std::ptr::null_mut()),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        let hits = corvid_geo_nearest(coll, field, field_len, 1.0, 1.0, 1);
        assert!(!hits.is_null());
        assert_eq!(
            corvid_geohits_next(hits, std::ptr::null_mut(), std::ptr::null_mut()),
            0
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        assert_eq!(corvid_geohits_next(hits, &mut hit, std::ptr::null_mut()), 1);
        assert_eq!(hit.key_len, 1, "doc_out NULL is fine (optional, §7)");
        // Exhaustion: 0, out-params untouched.
        hit.key_len = usize::MAX;
        assert_eq!(corvid_geohits_next(hits, &mut hit, std::ptr::null_mut()), 0);
        assert_eq!(hit.key_len, usize::MAX);
        corvid_geohits_free(hits);
        corvid_geohits_free(std::ptr::null_mut()); // §7 no-op

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }
}
