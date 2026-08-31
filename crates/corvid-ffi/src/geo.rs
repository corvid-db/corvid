//! Geo queries & the geohits cursor (spec §4.12).
//!
//! `geo_within_radius`/`geo_within_bbox`/`geo_nearest` wrap the engine's
//! haversine queries and return `corvid_geohits*` cursors; bbox hits
//! carry the documented 0.0 distance sentinel (no center). Bounds are
//! validated at entry (spec §4.12) with `CORVID_E_ARGUMENT`. Lands with
//! Task 6, together with the `corvid_geohits` marker handle.
