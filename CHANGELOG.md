# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/). Until 1.0 the on-disk
format and API may change without backward-compatibility guarantees.

## [Unreleased]

### Added
- Optional `tracing` cargo feature (default OFF) on the `corvid` crate:
  structured instrumentation at the engine's load-bearing points, via the
  `tracing` crate (default features trimmed: `span!`/`event!` only, so the
  `attributes` proc-macro tree stays out — the footprint is the facade +
  `tracing-core` + `pin-project-lite`), no public API change either way, and nothing at all when off (every call site goes
  through the engine-private `telemetry` shim, which compiles to no code
  and no dependency graph entries). Enable with
  `corvid = { features = ["tracing"] }` and attach any
  `tracing`-compatible subscriber (events carry `target = "corvid"`).
  Instrumented, at per-operation/per-page granularity (never
  per-document): index creation/backfill (a span per committed page with
  collection, index family — scalar/compound/text/geo/vector — page size,
  and cursor progress, plus a completion event with the page count);
  compactions (the in-memory and on-disk dead/live trigger math on the
  actual crossing, and the rebuild outcome); lazy index-build resumes and
  adjacency rebuilds (including whether the adjacency marker was absent
  or stale-shaped); query plan-shape selection (one event per query:
  which arm drove the candidate set — labels named after the public
  `PlanShape` variants, with two deliberate divergences: `indexed_window`
  carries no family discriminator (the scalar/compound/geo/or kind is
  `plan_shape()`/`explain()`'s to report, not the event's) and
  `stream_scan` is intentionally finer than `PlanShape::Scan` (it splits
  the bounded streaming filter pass from the materializing fallback) —
  and how many candidates it produced, making the
  index-walk vs point-get decision visible); the order-index walk's
  on-exhaustion tail scan; the edge-cascade rebuild fallback on a corrupt
  adjacency row; and semantic-cache hit/miss with the deciding distance.
  Counters (DESIGN's "cache hits, index probes" future item) are subsumed
  by these events — a subscriber aggregating `plan_shape` per `shape` or
  `semantic_cache_hit`/`miss` has the counter; plan-cache hit counters
  stay future (`PlanCache` is host-side state). CI enforces the default
  posture both ways: `cargo build --no-default-features` plus a
  `cargo tree` assertion that the default and wasm-target graphs never
  contain `tracing`, and clippy/doc/test with the feature on.
- `Collection::create_vector_index_pq(field, metric, m, k)` (and
  `Hnsw::with_pq(metric, pq, m, ef_construction)` on the direct index API):
  product quantization for the **in-memory** HNSW — the same compression the
  on-disk PQ index ships (`m` code bytes per vector instead of `dim * 4`;
  e.g. 16× at 64 dims with `m = 16`, plus the one-off `m × k × (dim/m)`-float
  codebook), applied to the RAM-resident graph. The codebook trains
  deterministically from a bounded sample of existing vectors
  (`EmptyIndexTraining` without any, or when `m` does not divide the field's
  dimension) and persists in the index's reserved namespace in the same
  transaction as the definition — so after a reopen the lazily rebuilt graph
  re-encodes under the SAME trained codebook, and dump/load round-trips the
  index as a new vector mode carrying `m`/`k`. Every metric serves, matching
  the on-disk contract exactly: L2 scores through the asymmetric-distance
  table (the ADC fast path), cosine and dot through reconstruct-then-distance
  (ADC tables for non-L2 metrics remain future work). Recall on the fixed
  clustered conformance corpus is 0.73 vs the exact scan (pinned ≥ 0.5;
  the number is `ef`-insensitive — the graph recovers the ADC ranking and
  the residual gap is codebook resolution). Measured on the pinned bench
  corpus (2000×64d, `m = 16, k = 256`): build 367.9 ms vs 124.9 ms
  full-precision, search 35.8 µs vs 19.1 µs — the footprint/time trade in
  two numbers (`hnsw_build_pq_2k_64d`, `hnsw_search_pq_2k_64d`).
- `Db::load_with_renames(reader, renames)` — the `a__b` migration path:
  loading a dump from a pre-wave-4 database whose collection names contain
  `__` (accepted then, rejected by name validation since) through a
  rename table `{ "a__b": "a_b", … }`. Every collection-name occurrence
  in the dump stream — documents, all index/schema definitions, TTL
  entries, graph edges, auto-id counters — is mapped before replay, so
  documents and definitions land together under the new name and every
  index is recreated from the renamed documents automatically (nothing to
  re-create by hand). Targets must pass name validation (the offending
  target's `InvalidName`); no two dump names may load into one output
  name (`InvalidArgument` — merging keyspaces would silently overwrite
  documents); reserved dump names are rejected before mapping (a rename
  cannot launder an engine-internal namespace); a map entry whose source
  never occurs in the dump is a no-op. The MCP `load` tool takes the same
  table as an optional `rename` object param. `Db::load` is unchanged
  (`load_with_renames` with an empty map), except one hardening fix: the
  dump's auto-id counter section — the only replay path without the
  reserved-name check — now rejects `__`-prefixed names like every other
  section.
- Syntax-conformance program: every public construct of `corvid` and
  `corvid-mcp` now has a committed surface manifest and radar
  (`crates/*/tests/surface/`) that fails CI on any drift between the
  manifest and the sources, rejects citations of nonexistent tests, and —
  since the waves filled every row — enforces strict covering: each row
  cites at least one conformance test. The only exemptions are the seven
  redb-passthrough `Error` variants (`Transaction`, `Table`, `Storage`,
  `Commit`, `SetDurability`, `Compaction`, `IncompatibleFormat`), fault
  paths unreachable from the public API, listed with justifications in the
  radar. The suites: 15 engine files (298 tests) by statement class —
  mutations, filters, queries, aggregations, joins, graph, vector/text/
  hybrid/geo search, schema, ttl, events, lifecycle, pq — plus the 78-test
  MCP wire matrix over in-process duplex I/O. `docs/SYNTAX.md` is generated
  from the manifests (a radar test re-renders and diffs it on every run;
  `CORVID_GEN_SYNTAX=1 cargo test -p corvid --test surface syntax_md`
  regenerates), documenting every construct with its covering tests and
  the cross-class semantics notes (BM25 re-normalization under
  pre-ranking predicates, `geo_within_bbox` key order, the NaN duality,
  equality-per-construct).
- New MCP tools `set_schema` / `get_schema` (`{collection, fields:
  [{name, type, required?, unique?}]}`, read back with explicit flags;
  `fields: []` declares an empty schema, distinct from no schema which
  reads `null`), backed by a new public engine getter
  `Collection::schema()`. Limit validation is unified across tools: an
  invalid `limit` (negative/non-integer) is a `BadParams` error everywhere
  instead of a silent default on `neighbors`/`in_neighbors`/`traverse`/
  `geo`/`join` (valid-but-oversized still clamps to 10000 on list tools),
  and invalid enum/flag params (`quant`, geo `k`, boolean flags) error
  instead of falling back to a default.

### Changed
- Dump format bumped to v2: `Db::dump` now stamps the header magic
  `CORVIDDUMPv2` and writes every length/count prefix — byte-field
  lengths (strings, keys, values), compound/schema per-definition field
  counts, and the PQ `m`/`k` parameters — as u64 instead of v1's u32, so
  a single value, key, string, or field count beyond 4 GiB is
  representable (v1's writer silently truncated such lengths). Section
  counts were already u64; all fixed-width fields are unchanged.
  `Db::load` (and `load_with_renames`) accept BOTH v1 and v2 dumps — the
  header magic decides the prefix width and nothing else differs — so
  existing dump files keep loading unchanged; only re-dumping with the
  new binary produces v2 bytes. An unknown magic (e.g. a future v3) is
  rejected with `Error::InvalidDump`, the one-way compat the format has
  always had.
- Deleting a document (directly, via `compare_and_set`, or through a TTL
  purge) now resolves its graph edges in O(edges-of-that-document) instead
  of scanning the collection's entire edge namespaces: two new private
  `__adj_out__`/`__adj_in__` namespaces hold the edges re-keyed
  endpoint-first as derived state (the edge rows remain the source of
  truth; their format is unchanged, no file migration). The adjacency is
  established inside the first edge write's own transaction (an empty
  build on fresh databases; a one-time re-derive from the edge rows on
  databases written before this change), maintained transactionally by
  `link`/`link_weighted`/`unlink`, self-healing if a derived row is
  corrupt (rebuild from source rows + re-run), and invisible — never
  listed by `collections()`, never in a dump (dump→load replays every edge
  through `link_weighted`, so the adjacency is built again by the end of
  load, not deferred to first use). Public graph behavior is byte-identical. Measured on the
  pinned benchmarks: the hub-heavy 100-doc delete sweep drops 241.0 ms →
  40.5 ms (~5.9×, asymptotically O(degree) vs O(collection edges)) and
  the mixed `delete_heavy` half-delete 566.8 ms → 194.5 ms; pure link
  throughput pays the dual-write (two extra rows per edge), ~1.4× on
  `edge_link_10k` after tuning (graph namespaces no longer maintain
  record counts — nothing read them — and write transactions cache
  collection-id lookups).
- Compound-index prefix-only equality queries (equality on a leading field
  with the trailing fields unconstrained) are now served through the index
  window when the index's def knows every document in the collection has
  all indexed fields present and encodable (`all_docs_indexed`, maintained
  at backfill completion and permanently cleared by any write that leaves
  a field missing/non-encodable; recomputed by re-creating the index).
  Results are identical to a scan by construction — a matching document
  necessarily has the leading field, hence is indexed. On a 5k-doc corpus
  where every doc has both fields, the pinned benchmark drops from
  ~1.54 ms (full scan) to ~0.24 ms (~6.4x). Indexes over corpora that
  contain (or ever wrote) missing-field documents keep the previous
  decline-to-scan behavior, and dumps replay index creation, so the flag
  is recomputed exactly on load.
- A filterless `order_by(field)` whose field has a complete scalar index is
  now served by an index order walk instead of materializing and sorting
  every row: rows and their order are identical (the walk enumerates the
  comparable class in the pinned total order; ties and large-integer
  encoding collisions re-sort with the exact comparator; incomparable and
  missing values still sort last via an on-exhaustion tail scan), but
  documents are fetched only for the `offset + limit` window. On a 5k-doc
  corpus with `limit 20`: ~9x faster ascending, ~2.7x faster descending.
  `plan_shape()`/`explain()` report the new `PlanShape::SortIndex` arm for
  this shape; queries with retrieval sources, filters, or no index on the
  ordered field keep their previous plans.
- Indexed query verification (a filter served by a scalar/compound/geo/OR
  index window) now batches its document fetch when the window is dense
  relative to the collection: instead of one point-get per candidate key,
  one ordered walk of the records streams the candidates in a single pass
  (results, order, and snapshot semantics are unchanged — this is purely
  an execution strategy pick, measured at a ~22% faster end-to-end query
  on a 10%-density window over a 5k corpus; sparse windows keep the
  point-gets).
- `Collection::compare_and_set` now compares the expected value with the
  engine's semantic value equality (the same rule unique constraints use:
  `NaN` equals `NaN` regardless of payload, `-0.0` equals `0.0`, containers
  element-wise) instead of encoded-byte identity — previously a `-0.0`
  expectation never matched a stored `0.0` (and vice versa), and differently
  payloaded NaNs never matched; the comparison now decodes the stored row,
  so a corrupt row surfaces its decode error instead of being byte-compared.
  (Task 3 review ruling F4)
- New public API: `QueryBuilder::plan_shape()` / `PlanShape` — the plan
  shape a query will take (what `explain()` prints); advisory.
- Release workflow hardening: the GitHub release is created only after every
  matrix build succeeds, the tag is checked against the workspace version,
  sha256 checksums are attached alongside the binaries, and `contents:write`
  is scoped to the release job. CI adds an `aarch64-apple-ios` cross-compile
  job (making the README's iOS row true), per-job `timeout-minutes`, and a
  cancel-in-progress concurrency group. (audit C11/D1)
- `explain()` now reports the plan shape the executor will actually take
  (`AnnIndex`/`TextIndex`/`IndexedWindow`/`StreamingTopK`/`Scan`), pinned to
  the planner's own decision logic by a parity test, instead of always
  printing `scan(...)`. (audit C3)
- `order_by` now sorts missing AND pairwise-incomparable values (mixed
  types, containers) last, stable by key — previously incomparable values
  interleaved by key. Descending reverses value order within the comparable
  class only; the class order (comparable < incomparable < missing) is
  fixed. Within the comparable class, cross-kind pairs (an Int against a
  Text) now order by a kind tag — numbers before texts — instead of
  falling back to key order: the fallback was not a total order, so a
  mixed-kind field could construct sort cycles (and a sort panic).
  (audit C4)
- `geo_within_bbox` with `min_lon > max_lon` now treats the box as wrapping
  the antimeridian and matches BOTH longitude ranges — previously it
  silently matched nothing. All four bounds are validated at entry
  (latitude in `[-90, 90]`, longitude in `[-180, 180]`, NaN rejected) with
  `Error::InvalidArgument`, and an inverted latitude box
  (`min_lat > max_lat`) is rejected too — latitude cannot wrap, so it used
  to match nothing silently. (audit C2)
- Argument validation (audit C6): `fuse_rrf(k)` rejects `k <= 0` and NaN;
  `rerank_mmr(lambda)` rejects λ outside `[0, 1]` and NaN; `Bm25Params`
  construction rejects `b` outside `[0, 1]` or negative/NaN `k1`. All with
  `Error::InvalidArgument` — previously garbage scores (infinity, inverted
  diversity) were accepted silently.
- A failed `Db::backup`/`Store::backup` now removes its partial destination
  file (best-effort) before returning the error, so a mid-copy failure never
  leaves debris that both masquerades as a backup and blocks future attempts
  via the existing-path refusal. (audit C8)
- User-supplied collection and field names may no longer contain `__`
  anywhere or a NUL byte: such names are rejected with a new
  `Error::InvalidName` (a leading `__` keeps `Error::ReservedCollection`).
  Interior `__` sequences could forge or collide with engine-internal
  namespaces and index-definition keys (`__edges__*`, `__ttl__*`, def
  separators); NUL corrupts length-prefixed encodings. Enforced at every
  write and index/schema-creation path. Breaking change ahead of 1.0:
  collections or fields named like `a__b` that earlier versions accepted
  are now rejected. (audit C7)
- `Db::dump` now takes the whole dump from ONE read snapshot (catalog walk,
  records, TTL/edge namespaces, auto-id counters) and streams records
  without materializing the corpus; `Db::load` streams the dump file
  through buffered reads instead of `read_to_end`, and rejects
  engine-reserved collection names on every replay path (index and schema
  sections join records/compound/TTL/edges). The dump format is unchanged.
  (audit B8)
- An `In` filter whose index-candidate union exceeds the shared 100_000-key
  aggregate cap now falls back to a scan, like `OR` unions always did,
  instead of materializing an unbounded key set. (audit B10)
- `insert_auto` reserves the auto id inside the insert transaction: a failed
  insert (schema or unique violation) no longer burns an id, and
  `Collection::len` saturates at `usize::MAX` instead of truncating on
  narrow targets. (audit C9)
- `Hit` gains `pub approximate: bool` (`true` when the candidate set came from
  an ANN index, `false` on the exact path), and `vector_search` now reranks
  ANN hits with exact metric distances recomputed from the stored documents —
  previously indexed modes returned the index's internal distances (Hamming
  bit counts for binary quantization, reconstruction approximations for
  scalar/PQ), which made metric-unit thresholds such as `SemanticCache`'s
  meaningless under quantized indexes. Breaking struct change ahead of 1.0.
  (audit B6)
- Group-key canonical form (affects `group_count`, `group_sum`, `group_avg`,
  `count_distinct`): text values are now bare keys (`"blog"`), non-text
  values are type-tagged (`i:1`, `f:1.5`, `b:true`), and texts that would be
  ambiguous with a tag are `t:`-escaped. Previously every key carried an
  `s:`-style type prefix. Breaking change ahead of 1.0.
- `Pq::l2_table` now returns `Option<Vec<f32>>` (`None` on dimension mismatch
  instead of an all-zero table); `adc_l2` scores out-of-range codes as
  `INFINITY` instead of panicking. Breaking public-API change ahead of 1.0.
  (audit A4)
- Unique constraints are now enforced for non-index-encodable values
  (Bytes/Array/Map/Vector) when a scalar index exists on the unique field,
  and NaN (at any depth) conflicts with NaN: writes previously accepted may
  now fail with `SchemaViolation`. (audit A3)
- New public API: `Store::begin_bulk` / `Store::BulkScope` — a thread-local,
  panic-safe relaxed-durability scope; `Db::bulk` now uses it (concurrent
  writers on other threads are no longer affected). (audit B1)
- Index creation is now crash- and error-safe: a creation interrupted between
  registration and backfill completion no longer leaves a permanently partial
  index that queries silently trust. Creation state is persisted
  (`Building{cursor}` → `Complete`); queries never serve a building index
  (exact or bounded fallbacks); the first query after a reopen resumes an
  interrupted build synchronously, so first-query latency can include the
  remaining backfill. The index-definition row format changed: binaries from
  before this change misread new-format rows — re-create indexes when
  downgrading. (audit A2)
- `Error::CorruptIndex { context }` is a new public variant (breaking for
  exhaustive matches on `Error`): corrupt on-disk index state that previously
  decoded as empty and served silently wrong (empty) results now errors
  loudly. (audit C1/C13)
- On-disk vector indexes now compact automatically once tombstones exceed a
  third of the index (`dead * 2 > live`), checked on the write path after the
  commit: expect a synchronous rebuild burst (write amplification) when a
  write crosses the threshold. (audit B5)
- The on-disk vector search's over-fetch is now scaled by the tombstone count
  (`ef_search.max(k) + dead`, mirroring the in-memory rule) instead of a
  fixed 2×: between compactions, recall no longer decays as tombstones
  accumulate — a tombstone-heavy graph widens its search frontier
  accordingly. (audit B5)
- `Db::dump`'s TTL section enumerates the persisted `__ttl__*` namespaces
  from the dump snapshot itself (catalog walk) instead of the in-memory
  session marker, so a marker lagging a concurrent TTL commit can no longer
  omit persisted entries from the dump.
- Re-registering an on-disk vector index — for any parameter change or none —
  now always rebuilds from scratch in one transactional reset; same-parameter
  re-creation no longer resumes a partial backfill. (audit A5)
- `phrase_search`'s no-index fallback now scores hits with BM25 (corpus
  stats gathered in the same pass) on the same scale as the indexed paths,
  instead of raw occurrence counts: `TextHit::score` values change for
  unindexed phrase queries and their ordering may change. Creating or
  dropping an index no longer reorders the same phrase query. (audit B7)
- Queries, aggregations (`count`/`group_count`/`sum`/`avg`), and `traverse`
  now execute against a single MVCC snapshot: candidate generation,
  verification, ranking, and document fetches all observe one point in
  time, so a query can no longer return a result set matching no committed
  state (omission-only mid-write anomalies remain possible within a query,
  never torn reads). The lazy in-memory index builds deliberately read
  fresh committed state under the registry lock, so a concurrent commit is
  never permanently hidden from an in-memory index. (audit B3)
- Deleting a document now removes its graph edges in the same transaction
  (previously the edges were orphaned and only surfaced as dangling
  references), and `link`/`link_weighted`/`unlink` emit change events.
  (audit B4)

### Fixed
- A compound-index window was served for prefix-only queries (trailing
  field unconstrained), but documents *missing* that field match the
  filters while being absent from the index — the window was not a
  verified superset and filtered queries silently omitted them. The
  planner now serves a compound window only when the query's constraints
  cover every field of the index (equality prefix + at most one trailing
  range); prefix-only queries plan as a `Scan` (a `plan_shape()`/
  `explain()` change for those queries) and return every matching
  document. (found by the WHERE conformance wave)
- On-disk vector index backfill: a page holding mixed vector dimensions
  corrupted the index — the dimension-mismatch arm's tombstone re-read the
  persisted meta row (stale mid-page, since a page persists meta only at
  its end) and the re-sync clobbered the batch's accumulated counts,
  reusing node ids and re-pinning the dimension to a later vector in the
  same page. The tombstone now applies inline against the in-flight
  meta/cache. (found by the schema/index conformance wave)
- Unique-constraint checking keyed its bucket walk on the order-preserving
  f64 encoding, which collapses numerically equal but distinct stored
  values (`Int(7)` vs `Float(7.0)`, f64-rounded huge ints) — a second
  document was rejected *only* when a scalar index existed, diverging from
  the index-free path and from `compare_and_set`'s storage equality. The
  walk now re-checks the actual stored value with semantic equality.
  (found by the schema/index conformance wave)
- Dotted-path resolution is unified on `Value::get_path`: the filter
  layer's private resolver and `select()`'s projection used to resolve
  `""` to a top-level `""`-keyed field while index maintenance and windows
  treat `""` as resolving no field, so an unindexed collection could match
  a document an indexed one served an empty window for.
  (found by the schema/index conformance wave)
- `geo_within_bbox` now returns its result in key order on every path —
  the indexed path used to emit candidates in grid-cell order, so creating
  an index reordered the same query's results. (found by the geo
  conformance wave)
- Deleting an absent key now still runs the graph-edge cascade (edges
  linked against a never-inserted or already-deleted key were uncleanable
  by `delete`), and purging a stranded TTL entry (expiry on a key with no
  document) cascades the same way — the delete-path return and event
  semantics (false, no events) are unchanged. (found by the graph and TTL
  conformance waves)
- MCP boolean flags (`set_schema`'s `required`/`unique`, `create_index`
  and `create_text_index`'s `on_disk`) no longer coerce non-bool JSON to
  `false`: a present-but-non-boolean value is a `BadParams` error naming
  the flag. (found by the Task 14 fix round)
- Overwriting a document whose vector field is HNSW-indexed with a
  different-dimension vector previously left the old node live in the graph:
  ANN results for the old dimension kept returning the key while exact
  search excluded it. The old node is now tombstoned before the dimension
  check. (audit A1)
- `purge_expired` no longer deletes a record that was rewritten after its
  expiry was collected: each due key is re-read and deleted only if the
  timestamp still matches, inside one transaction. TTL maintenance is also
  decided inside the write transaction, so a plain insert can no longer
  inherit a stale expiry from a racing `set_ttl`/`mark_ttl_collection`.
  (audit B2)

## [0.1.1] - 2026-05-29

### Changed
- Release workflow builds the `corvid-mcp` binary for all desktop/server
  platforms — Linux (x86_64 + aarch64), macOS (Intel + Apple Silicon), and
  Windows (x86_64) — attaching each to the tagged GitHub release.

## [0.1.0] - 2026-05-29

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
- Selectivity-driven index choice: the builder probes every serviceable index
  (each capped) and drives on the smallest candidate set, so the most
  selective index wins and unselective ones drop out at the cap — no persisted
  statistics needed.
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

### Performance
- Autovectorized distance kernels (`dot`/`l2_squared` via multi-accumulator
  chunks; cosine builds on `dot`), under `#![forbid(unsafe_code)]`.
- On-disk HNSW build speedup: an `Rc`-shared node cache and once-per-batch
  dirty flush cut bulk-backfill time materially.
- On-disk PQ distance: asymmetric-distance (ADC) fast path for the L2 metric,
  scoring codes against a per-query table built from the codebook.

[0.1.1]: https://github.com/i-rocky/corvid/releases/tag/v0.1.1
[0.1.0]: https://github.com/i-rocky/corvid/releases/tag/v0.1.0
