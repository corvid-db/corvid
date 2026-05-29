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
- Text analyzer: stop-word removal + Harman S-stemmer (plural normalization),
  shared by index and query so singular/plural match (`dog`↔`dogs`). Configurable
  via `Analyzer`; raw `tokenize` still available.
- On-disk indexes (bounded memory, persist across reopen, no rebuild): on-disk
  HNSW vector index (`create_vector_index_ondisk`, with a quantized variant
  `create_vector_index_ondisk_quantized` for binary/scalar on-disk footprint),
  on-disk inverted text index (`create_text_index_ondisk`), and a scalar
  secondary index (`create_scalar_index`) making equality/range filters and
  counts sub-linear instead of full scans, a compound multi-field index
  (`create_compound_index`) for prefix-equality + trailing-range queries, and a
  spatial index (`create_geo_index`) making radius/bbox geo queries scan only
  nearby grid cells.
- Quantization extracted to a shared module used by both the in-memory and
  on-disk vector indexes.
- Product Quantization: `create_vector_index_ondisk_pq(field, metric, m, k)`
  (and MCP `create_index` with `pq:{m,k}`) stores vectors as m code bytes via a
  trained, persisted codebook — the smallest vector footprint.
- Logical dump/load migration (`Db::dump`/`Db::load`, MCP `dump`/`load`):
  version-stamped export of documents + index/schema/TTL definitions, replayed
  into a fresh DB (indexes rebuilt) — for migrating across format breaks.
- Compaction: `Db::compact()` reclaims file space after heavy deletes (redb
  compaction); offline maintenance (&mut self), data unchanged.
- Bulk-load fast path: `Db::bulk(|| ..)` runs writes under non-fsync
  durability and flushes once at the end (~N fsyncs -> ~1); committed data
  stays consistent, in-flight writes may be lost on crash until the flush.
- Online backup: `Db::backup(path)` / `Store::backup(path)` (and MCP `backup`)
  write a consistent point-in-time copy from one read snapshot, safe to run
  while writers are active.
- WASM: the engine compiles to `wasm32-unknown-unknown`; a `corvid-wasm` cdylib
  harness links it into a ≈0.2 MB gzipped bundle, CI-enforced under 2 MB. The
  engine also cross-compiles for aarch64 iOS/Android.
- Keyset (cursor) pagination: `page`/`page_where(after, limit)` return a page
  of rows plus a `next` cursor, resuming by key without offset rescans;
  streamed and bounded. MCP `page`.
- Identity-hashable query plans: `QueryBuilder::plan()` returns a canonical
  `QueryPlan` (equal iff the query shape is equal); `PlanCache` keys prepared
  work by shape (caches shape, not results, so never stale).
- Bounded ranked execution: the builder uses the text index for a single text
  source (no corpus rescan) and a streaming bounded top-k for an unindexed
  single vector source, so single-source ranked queries don't materialize the
  whole collection.
- Fluent multi-modal query builder: filter + vector + text + RRF fusion + MMR
  rerank + projection + `order_by`/`offset` pagination + `count`/`group_count`.
- Aggregations: sum/avg/min/max/count_distinct globally and group_sum/group_avg grouped, over the filtered set (respecting filters and indexes).
- Filter predicates (`field().gt()`, and/or/not, dotted paths, `within_km` geo).
- Phrase search (`phrase_search`): exact consecutive in-order token matches,
  via positions stored in both inverted indexes (in-memory + on-disk); MCP
  `phrase_search`. Falls back to an exact scan when no text index.
- OR queries use an index union: a top-level OR whose disjuncts are all
  index-serviceable scans each index and unions the candidates, instead of a
  full scan.
- More predicates: `is_in` (set membership), `between` (inclusive range),
  `starts_with`/`contains` (text); between/in/starts_with route through the
  scalar index (text prefix scan), the rest verify on scan.
- Delete-by-query (`delete_where`, index-accelerated) and `delete_batch`,
  cascading through every index; exposed over MCP (delete_where).
- Partial writes: `patch` (merge fields), `update` (read-modify-write), and
  `compare_and_set` (atomic conditional write/delete/insert-if-absent). All keep
  indexes consistent; exposed over MCP.
- Optional per-collection declared schema (`set_schema`): field types,
  `required`, and `unique` enforced on write; schemaless collections unaffected.
- Per-record TTL/expiry (`insert_with_ttl`/`set_ttl`/`purge_expired`): time is
  injected (no engine clock); `purge_expired(now)` reclaims due records via the
  normal delete path. Sorted TTL index; persists across reopen.
- Directed property graph: `link`/`unlink`/`neighbors`/`in_neighbors`/`traverse`.
- Geospatial radius / bounding-box queries (haversine).
- k-nearest geo (`geo_nearest`): the k closest points regardless of radius,
  index-accelerated and exact (expanding radius); MCP `geo` accepts `k`.
- Cross-collection lookup joins, semantic (vector-keyed) cache, in-process
  reactive change feeds, HyperLogLog / Bloom sketches.
- `corvid-mcp`: a runnable MCP server over stdio exposing the engine as tools.
- On-disk format version marker (refuses incompatible files).

[Unreleased]: https://github.com/rocky/corvid
