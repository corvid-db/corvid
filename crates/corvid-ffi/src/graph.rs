//! Graph family (spec §4.11) — the 7 edge/neighborhood functions.
//!
//! Directed edges over document keys in the same database; `neighbors`/
//! `in_neighbors`/`traverse` return `corvid_strs*` cursors (the plumbing
//! in [`crate::strs`]) and `neighbors_weighted` reuses the geohits
//! cursor shape with `distance_km` carrying the edge weight. Lands with
//! Task 6.
