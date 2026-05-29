# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/). Until 1.0 the on-disk
format and API may change without backward-compatibility guarantees.

## [Unreleased]

### Added
- Embedded transactional KV store over redb with atomic multi-op transactions
  and snapshot reads.
- Typed `Value` model with a deterministic binary codec; document layer with a
  fluent `Collection` handle and auto-generated ordered keys (`insert_auto`).
- Vector search: distance metrics (cosine/dot/L2), exact KNN, and an
  incremental, persistent HNSW index (`create_vector_index`) used transparently
  by `vector_search` and the builder (with an `.approx()` filtered-ANN path).
- Full-text search: BM25 with an incremental inverted index (`create_text_index`).
- On-disk indexes (bounded memory, persist across reopen, no rebuild): on-disk
  HNSW vector index (`create_vector_index_ondisk`, with a quantized variant
  `create_vector_index_ondisk_quantized` for binary/scalar on-disk footprint),
  on-disk inverted text index (`create_text_index_ondisk`), and a scalar
  secondary index (`create_scalar_index`) making equality/range filters and
  counts sub-linear instead of full scans, and a spatial index
  (`create_geo_index`) making radius/bbox geo queries scan only nearby grid
  cells.
- Quantization extracted to a shared module used by both the in-memory and
  on-disk vector indexes.
- Online backup: `Db::backup(path)` / `Store::backup(path)` (and MCP `backup`)
  write a consistent point-in-time copy from one read snapshot, safe to run
  while writers are active.
- WASM: the engine compiles to `wasm32-unknown-unknown`; a `corvid-wasm` cdylib
  harness links it into a ≈0.2 MB gzipped bundle, CI-enforced under 2 MB. The
  engine also cross-compiles for aarch64 iOS/Android.
- Fluent multi-modal query builder: filter + vector + text + RRF fusion + MMR
  rerank + projection + `order_by`/`offset` pagination + `count`/`group_count`.
- Filter predicates (`field().gt()`, and/or/not, dotted paths, `within_km` geo).
- Directed property graph: `link`/`unlink`/`neighbors`/`in_neighbors`/`traverse`.
- Geospatial radius / bounding-box queries (haversine).
- Cross-collection lookup joins, semantic (vector-keyed) cache, in-process
  reactive change feeds, HyperLogLog / Bloom sketches.
- `corvid-mcp`: a runnable MCP server over stdio exposing the engine as tools.
- On-disk format version marker (refuses incompatible files).

[Unreleased]: https://github.com/rocky/corvid
