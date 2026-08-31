//! Indexes & schema (spec §4.10) — the 15 create/declare/inspect
//! functions.
//!
//! Every create is (or replace); the eight HNSW variants (in-memory,
//! quantized, on-disk, product-quantized ×2 each) map 1:1 onto
//! `corvid::Collection::create_*`; `set_schema` builds the engine
//! `Schema` from a `corvid_field_def` array; `schema` returns a
//! schemaiter or absence-as-success. Lands with Task 6, together with
//! the `corvid_schemaiter` marker handle.
