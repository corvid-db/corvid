# corvid — design

A single embedded, Rust, multi-modal data store. Built for personal use first, released as open source. No timeline.

This document is a living record of what we want to build and *why*. The "why" lines matter as much as the "what" — when future-me forgets the reasoning behind a decision, this is where I look.

---

## Posture

- **One person, no deadline.** Compounding for decades on a problem shape no one else cares about as much as I do. Not a race.
- **For me first.** "Best for the work I actually do" is the bar. Not "world's best." Specialists will out-engineer me on any single component; that's fine.
- **Used in real work from day one.** A 50-year database with no real user (including me) is a 50-year design exercise. Pick a real AI app, build it with this, let the app shape the database.
- **Wrap aggressively in v0.1.** Replaceable parts are the right primitive in a long-running project. Components get better over time; the job is to make sure the architecture absorbs that improvement without churn.
- **No backward compat ever** (per global preference). Break shape freely until we declare v1.0. The price of "stable forever" early is enormous; "stable when ready" later is one weekend of migration scripts each time.

## North star

> The only embedded data store where **vector + filter + full-text + graph traversal + rerank composes into one transactionally consistent, builder-driven call** — across desktop, mobile, and WASM.

The differentiator is **cross-modal composition under one transaction**, not any single index type. Every other engine bolts modalities together at the application layer. We make them first-class.

## Primary driver: the MCP sidecar

The first real consumer (and the thing that keeps the design honest) is a **sidecar MCP server** for agentic coding tools — Claude Code, Codex, Cursor, the VSCode and JetBrains MCP clients. It exposes corvid's capabilities as MCP tools so an agent gets persistent, queryable memory and retrieval with zero glue. This single driver subsumes the three workloads we considered separately:

- **RAG / second-brain** — embed + FTS + metadata filter, hybrid retrieval.
- **Agent memory** — episodic append + semantic recall + an entity/relation graph.
- **Code-aware store** — symbols, chunk embeddings, call/import graph, full-text, incremental reindex.

### The boundary that keeps the engine pure

- **`corvid` (engine crate)** — strictly embedded, in-process, zero networking. Unchanged by this driver.
- **`corvid-mcp` (separate crate/binary)** — embeds the engine and speaks **MCP over stdio** (JSON-RPC on stdin/stdout). That is how these tools launch MCP servers — a subprocess, not a TCP listener — so it does not violate the no-networking rule. All protocol/transport code lives here and never in the engine.

Implication for sequencing: the engine's v0.1 core (vector + FTS + filter + fusion) lets the sidecar ship RAG-style tools first; graph and temporal tools follow as L5 lands. The MCP tool surface (store / search / recall / link / traverse / forget) is its own design artifact, evolved alongside the engine — not specified here yet.

## Non-goals (permanent, not "deferred")

- SQL parser, ANSI semantics, NULL three-valued logic
- Replication, multi-node anything, distributed transactions
- Server wire protocol, network listeners, auth — *in the engine*. The separate `corvid-mcp` sidecar speaks MCP over stdio; see *Primary driver*.
- Cloud sync, hosted service
- Differential dataflow / Materialize-tier reactive
- General-purpose tensor ops (users plug in candle/burn)
- Arrow `RecordBatch` result paths — users convert to Arrow at the boundary (DECLINED, decision log 2026-08-30)
- A JSON path language (`$.a[0].b`-style) — dotted paths + `select` cover the surface (DECLINED, decision log 2026-08-30)
- Backward-compatible file format migrations (manual reimport, full stop)
- "World's best" on any single component — match within 2–5×, never claim leadership

## Targets

| Target | Memory | Storage | Concurrency |
|---|---|---|---|
| Desktop / server | abundant | pread/pwrite (redb `StorageBackend`) | single writer, multi-reader MVCC |
| Mobile / edge | tight | pread/pwrite + fsync | single writer, multi-reader MVCC |
| WASM / browser | tight | OPFS sync handle in Worker (truncate-first growth; SAHPool the recorded fallback) | single connection (OPFS constraint) |

Same Rust core. The storage backend is abstracted behind redb's `StorageBackend` trait (read/write/sync/set_len) in v0.1 — three impls, one per environment. No conditional features that change the data model — only the I/O path differs.

> **Concurrency note:** redb is single-writer *for the whole database*, not per-table. Per-table writer concurrency (floated earlier as a win over SQLite's global write lock) is **not** available while wrapping redb — it would require either a different substrate or one redb instance per table. Out of scope for v0.1; revisit only if a real workload is write-bound across tables.

---

## Architectural pillars

1. **VFS as the only environment-specific layer.** Three backends: pread/pwrite, mmap, OPFS-SAHPool. Everything above the VFS is pure logic.
2. **CoW B+-tree storage substrate.** Single-writer, multi-reader MVCC. Stable file format eventually. Wrap [redb](https://github.com/cberner/redb/) in v0.1; revisit only if redb stops being the right primitive.
3. **Cross-modal atomic indexes.** Every secondary index (vector, FTS, graph adjacency, spatial) updates atomically with the row. This is the central invariant. No "eventually consistent" indexes.
4. **Tree-of-nodes fluent builder.** Chained method calls produce an AST. The AST is the query plan input. Identity-hashable for plan caching. Predicate pushdown happens on the AST before execution. Modeled after polars LazyFrame / DataFusion DataFrame, not Diesel typestate.
5. **Logical change feed from the WAL.** In-process subscriptions piggyback on the write path. No separate event log. Lets us build materialized views and reactive operators without becoming a server.
6. **WASM is a Worker by default.** Main thread is RPC proxy only. OPFS-SAHPool is the production VFS. Single-connection limit is acceptable.

---

## Capability layers

Bottom-up. Each layer depends only on those below. L0–L4 shipped in v0.1; L5
partially shipped (graph, geo, sketches, reactive feeds, semantic cache, TTL —
see the L5 section: every remaining item carries its decision-log
disposition). Layers are a conceptual map, not
a build order: the L5 features that shipped did so against the stable L4 API.

### L0 — storage backend

**v0.1 (wrapping redb):** this layer is redb's `pread/pwrite` file backend on desktop/mobile. The browser backend — one OPFS file per database behind a `FileSystemSyncAccessHandle`, reached through redb's public `StorageBackend` seam (`Store::open_with_backend`, engine v0.3.4) — **shipped 2026-09-02** in corvid-js (the Worker runtime, the async surface, and the review-gated contract live in that repo, `docs/OPFS-SPEC.md`; decision log row of the same date closes the 2026-05-29 deferral). redb does **not** require mmap, which is what made the OPFS path tractable. Checksums, group commit, page format, and crash recovery are **redb's concern**, not ours, as long as we wrap it.

**Only if redb is ever replaced (post-v1):** the owned page format is **never** while redb is the substrate (decision log, 2026-08-30 — the seam below is documented, not scheduled); if we ever replace redb, *this* layer grows to own the page format — big pages (16K–64K), prefix-compressed keys, whole-block CRC32C (hardware CRC), group commit, fsync coalescing.

### L1 — Storage substrate

- Wrap **redb** as primary KV store
- WAL + MVCC reads via redb's existing model
- Compression: zstd for values above a threshold (applied by us, above redb) — **shipped behind the opt-in `zstd` cargo feature** (user-collection documents ≥1 KiB at level 3, self-describing leading marker, decompressed on every read; default build byte-identical; ledger-closure Task 5)
- Single writer (whole database), MVCC readers — redb's model, unchanged

### L2 — Schema and document layer

- Strict typed schemas (no dynamic typing)
- Primitive types: bool, i32, i64, f32, f64, bytes, string, timestamp, decimal
- Container types: array, map, struct, JSON (typed-on-write where possible)
- Embedding type: fixed-dimension f32/f16/u8/binary vector with declared metric
- Tensor type: deferred (the L4+ deferral stands; general tensor ops are a non-goal — decision log, 2026-08-30)

### L3 — Indexes

All indexes update transactionally with row writes. Same WAL.

- **Primary**: B+-tree on primary key (from redb)
- **Secondary scalar**: B+-tree, supports range, prefix, equality
- **Vector**: HNSW algorithm (reference: [hnswlib-rs](https://github.com/jean-pierreBoth/hnswlib-rs)), graph state stored as redb entries — not the library's own persistence (invariant). Quantization: binary, scalar, PQ (on-disk and, since the roadmap program, in-memory). LM-DiskANN: deferred with trigger — on-disk HNSW measurably insufficient on a real workload (decision log, 2026-08-30).
- **Full-text**: BM25 over posting lists stored as redb entries. Stripped tokenizer (Unicode + ASCII fold + light stemming). Borrow tokenizer/Levenshtein-automaton ideas from [tantivy](https://github.com/quickwit-oss/tantivy); reference [bm25 crate](https://docs.rs/bm25/latest/bm25/). Do not adopt tantivy's segment storage.
- **Graph adjacency**: deferred to L5
- **Spatial (R-tree)**: deferred to L5
- **JSON path**: **DECLINED** (decision log, 2026-08-30) — dotted paths + `select` cover the surface; a path language adds SQL-ish surface area

### L4 — Query algebra (the most important layer)

This is what survives across rewrites of everything below. Design carefully.

- Relational core: scan, filter, project, join, group, sort, limit
- Vector: `vector_search(column, query, k, metric)` returning candidates with distance
- Text: `text_search(column, query, k)` returning candidates with BM25 score
- Hybrid: `rrf([vector_search, text_search])` and `mmr(candidates, lambda)` as first-class operators
- Composition: every operator takes and returns a plan node. Filters push down into index scans.

### L5 — Extended capabilities

Shipped: graph (native adjacency storage, `traverse`), spatial (grid-cell
index for radius/bbox/nearest), probabilistic sketches (HyperLogLog, Bloom,
cuckoo, t-digest, MinHash + LSH), reactive subscriptions on the write path,
semantic cache, per-record TTL. Deferred items are marked below, each with
its decision-log disposition (2026-08-30 program exit).

- Graph: native adjacency storage, BFS/DFS, multi-hop traversal with vector-similarity-as-edge-predicate — *shipped* (`link`/`neighbors`/`traverse`)
- Spatial: R-tree (via [rstar](https://crates.io/crates/rstar)) and H3 (via [h3o](https://crates.io/crates/h3o)) — *deferred (grid cells serve the current geo workload; decision log, 2026-08-30); shipped instead: fixed-resolution grid cells (`create_geo_index`)*
- Time-series patterns: append-only tables, delta encoding, Gorilla compression, sliding window aggregations — *deferred with trigger: a real time-series workload (decision log, 2026-08-30; per-record TTL shipped)*
- Probabilistic sketches: bloom, cuckoo, HLL, t-digest, MinHash + LSH — *shipped in full (cuckoo/t-digest/MinHash+LSH closed 2026-08-30, ledger-closure Task 3)*
- Reactive: subscriptions on tables and queries via WAL change capture — *shipped (in-process change feeds)*
- Semantic cache: vector-keyed cache layer for LLM responses — *shipped*
- Embedding pipeline as column type: declare model, auto-embed + auto-index on insert — *deferred (model-dependency policy undecided; users embed at the boundary; decision log, 2026-08-30)*
- Approximate top-K with metadata filter pushdown — *partial: `.approx()` serves filtered ANN as over-fetch-then-filter (not a pushdown into the graph walk); true pushdown deferred with trigger: filtered ANN becomes hot (decision log, 2026-08-30)*

### L6 — API surface

- Fluent builder, sync API surface (async wrapper available)
- Native Rust result types everywhere; an Arrow `RecordBatch` analytical path is **DECLINED** (decision log, 2026-08-30 — heavy dependency against the embedded posture; users convert at the boundary)
- One canonical host: Rust. Other-language bindings considered later, never first-class.

---

## Component map: wrap vs build vs skip

| Component | Decision | Notes |
|---|---|---|
| Storage substrate | **Wrap** redb | Replaceable. Maybe fjall later when LSM patterns matter (append-heavy logs). |
| WAL / MVCC | **Inherit** from redb | Don't reinvent. |
| Compression | **Wrap** zstd via FFI (shipped, opt-in `zstd` feature — closed 2026-08-30, decision log) | Physics, not legacy. |
| JSON parsing | **Build thin** pure-Rust behind a trait (v0.1) | Replaceable component, not the identity. Seam to FFI C++ simdjson on native later if measured hot. |
| Vector index | **Algorithm from hnswlib-rs, state in redb** | Not wrapped as storage (breaks the invariant). v0.1 may scaffold against hnswlib-rs's own persistence behind the (b) contract; HNSW state moves into redb entries as the real impl. |
| Vector quantization | **Build** (light) | Binary + scalar + PQ (all shipped). Algorithm-clear, integration-heavy. |
| SIMD distance kernels | **Build** (lane folds; closed 2026-08-30) | LLVM already vectorizes the folds 4-wide; explicit SIMD declined by measurement — exactness oracle + nightly/unsafe posture (decision log, 2026-08-30). |
| FTS | **Algorithm (BM25 + posting lists), state in redb** | Posting lists as redb entries, not tantivy's segment files — same invariant reason as vector. Borrow tantivy's tokenizer/automaton ideas; don't adopt its storage. bm25 crate as a reference. |
| Tokenizer | **Build** (Unicode + light stemming + CJK bigram fallback) | Stripped, English-first; CJK runs (Han + kana) bigram-indexed — no dictionary segmentation (closed 2026-08-30, decision log). |
| Builder API | **Build** | Project identity. Every line goes through our hands. |
| C ABI (FFI surface) | **Build** (`crates/corvid-ffi`: the `corvid` cdylib — libcorvid.so/.dylib/corvid.dll — plus the generated, drift-gated `corvid.h`; 124 symbols, docs/FFI.md is the locked contract; shipped as per-platform release artifacts — 2026-08-31, decision log; 2026-09-01 additive expansion, decision log) | The engine's cross-language contract: typed handles end to end, zero parsing by ruling, crossing cost measured at parity with native Rust (BENCHES.md, corvid-ffi Task 8). |
| Query planner / executor | **Build** | Same. |
| AI operators (MMR, RRF, semantic cache) | **Build** | Field is young; integration is the value. |
| Graph adjacency | **Build** (post-v0.1) | Custom storage, fits cross-modal model. |
| Spatial | **Defer** — grid cells serve the current workload (decision log, 2026-08-30); would wrap rstar + h3o if a workload needs true spatial hierarchy | Well-solved by these crates. |
| Probabilistic sketches | **Build** (all shipped; closed 2026-08-30) | The wrap plan lost to the zero-dependency posture and the deterministic-`DefaultHasher` discipline; Bloom/HLL/cuckoo/t-digest/MinHash+LSH are in-house (`src/sketch/`, decision log 2026-08-30) |
| Reactive | **Build** (post-v0.1) | WAL change capture, in-process subscribers. |
| Tensor ops | **Skip** | Out of scope (non-goal; the storage-only tensor deferral stands, decision log 2026-08-30). Bring your own (candle/burn). |
| SQL | **Skip** | Permanently. |
| Replication / network | **Skip** | Permanently. |

---

## v0.1 cut

Smallest coherent thing that's usable for my own AI work. Target: usable within months, not years.

### In v0.1

- L0 VFS: pread/pwrite via redb on desktop/mobile; the browser backend (one OPFS file over a sync access handle, through redb's `StorageBackend` seam) shipped 2026-09-02 in corvid-js (decision log, same date)
- L1 storage (redb wrap)
- L2 schema with primitives + array + struct + JSON + embedding type
- L3 indexes: primary B+-tree, secondary scalar B+-tree, HNSW vector with binary quantization, BM25 FTS
- L4 algebra: scan, filter, project, join (single-field key-lookup left-outer), group, sort, limit, vector_search, text_search, rrf, mmr
- L5 (shipped beyond the original cut): graph, grid-cell spatial index, sketches, reactive feeds, semantic cache, TTL
- L6: sync fluent builder API, native Rust result types
- WASM: engine compiles to wasm32; the Worker + OPFS runtime shipped 2026-09-02 in corvid-js (async `openOpfs` surface, contract `docs/OPFS-SPEC.md` there)

### Out of v0.1 (back-burner, in roughly this priority order)

Status: items 1, 3, and 6 shipped in full; 2, 5, and 8 partially
(subscriptions / grid-cell spatial index / on-disk PQ); every remaining
item — 4, 7, 9, and 10 — carries an explicit 2026-08-30 decision-log
disposition. Nothing on this list is unprioritized.

1. Graph adjacency + multi-hop traversal — shipped
2. Reactive: in-process subscriptions and materialized views — subscriptions shipped; materialized views deferred with trigger (API/product intent; decision log, 2026-08-30)
3. Semantic cache as a primitive — shipped
4. Embedding-as-column-type with automatic pipeline — deferred (decision log, 2026-08-30)
5. Spatial (R-tree + H3) — shipped as grid-cell spatial index; R-tree/H3 deferred (decision log, 2026-08-30)
6. Probabilistic sketches as first-class index types — shipped (Bloom, HLL)
7. Time-series compression — deferred with trigger (decision log, 2026-08-30)
8. PQ vector quantization, DiskANN path for large indexes — PQ shipped (on-disk and in-memory); DiskANN deferred with trigger (decision log, 2026-08-30)
9. Approximate top-K with metadata pushdown — deferred with trigger (decision log, 2026-08-30; `.approx()` is over-fetch-then-filter)
10. Arrow `RecordBatch` zero-copy result path for analytical queries — **DECLINED** (decision log, 2026-08-30; users convert at the boundary)

---

## Implementation status (living)

As of the first build pass. All code is tested (≥90% line coverage, mostly ~99%), fmt + clippy clean under `-D warnings`.

**Built and working:**
- **L1 storage** (`store.rs`): collections over one redb table (BE id-prefixed keys), `put/get/delete/scan`, atomic multi-op `transaction(|tx| …)` and snapshot `read(|r| …)`, file-backed and in-memory; optional transparent zstd compression of user-collection values (`compression.rs`, feature `zstd`, OFF by default — ledger-closure Task 5).
- **L2 values** (`value.rs`): `Value` (null/bool/int/float/text/bytes/array/map/vector) with a deterministic tag/length codec; field accessors.
- **Document layer** (`db.rs`): `Db` + `Collection` handle over typed `Value` documents.
- **Vector search** (`distance.rs`, `query.rs`): cosine/dot/L2 kernels; exact KNN baseline + HNSW index.
- **HNSW index** (`hnsw.rs`, `index.rs`): **incremental and persistent-definition**. `create_vector_index` registers a derived index maintained with O(log n) inserts/tombstones per write (no per-write rebuild); definitions reload on open; `vector_search` and the builder use it (the latter via `.approx()` for filtered queries). Documents are the source of truth.
- **Full-text** (`text.rs`, `fts.rs`): tokenizer + BM25 exact baseline **plus an incremental inverted index** (`create_text_index`) so queries touch only query-term postings. CJK runs (Han + kana) tokenize as sliding bigrams — the dictionary-free segmentation fallback; stemming/case-folding never apply to them (ledger-closure Task 4).
- **Fusion** (`fusion.rs`): Reciprocal Rank Fusion + MMR.
- **Filters** (`filter.rs`): `field("a.b").gt(…)` predicate tree, and/or/not, dotted paths, `within_km` geo.
- **L4 fluent builder** (`builder.rs`): filter → vector → text → RRF → MMR → `order_by`/`offset` → `select` (nested paths) → `limit`; aggregations (`count`, `sum`, `avg`, `min`, `max`, `count_distinct`, `group_count`, `group_sum`, `group_avg`); `.approx()`; `.explain()`; `.plan()` (identity-hashable `QueryPlan` for `PlanCache`). Filter predicates extend to `is_in`/`between`/`starts_with`/`contains` alongside the comparison/and/or/not core. Keyset pagination via `page`/`page_where`; `phrase_search` for exact in-order matches.
- **Graph** (`graph.rs`): `link`/`link_weighted`/`unlink`/`neighbors`/`neighbors_weighted`/`in_neighbors`/`traverse`; edges atomic (forward+reverse in one txn). Deletes cascade through derived endpoint-first adjacency namespaces (O(edges-of-doc); Task 7); `neighbors`/`in_neighbors`/`neighbors_weighted`/`traverse` read endpoint-direct through the same adjacency (measured parity; ledger-closure Task 1).
- **Geo** (`geo.rs`): haversine radius / bbox, composable as a builder filter.
- **Join, semantic cache, reactive feeds, sketches, document layer** as before; auto-keys (`insert_auto`), `collections()` listing, on-disk format version.
- **Sidecar** (`corvid-mcp`): MCP server over stdio exposing 29 tools — `store`, `patch`, `compare_and_set`, `get`, `delete`, `delete_where`, `page`, `search`, `phrase_search`, `count`, `geo`, `join`, `link`, `unlink`, `neighbors`, `in_neighbors`, `traverse`, `create_index`, `create_text_index`, `create_scalar_index`, `create_compound_index`, `create_geo_index`, `backup`, `dump`, `load`, `list_collections`, `insert_auto`, `set_schema`, `get_schema` — with default result caps. `set_schema` declares (or replaces) a collection's schema (a fields array of `{name, type, required?, unique?}`) that subsequent stores are validated against; `get_schema` returns the declared fields, or `{fields: null}` when none is declared.
- **C ABI** (`corvid-ffi`, 2026-08-31): the 124-symbol typed cdylib (`libcorvid.so`/`libcorvid.dylib`/`corvid.dll`) + generated, drift-gated `corvid.h` over the whole engine surface — values, collections, predicates, the full query builder + rows cursors, aggregations, indexes/schema, graph, geo, admin, and (since the 2026-09-01 additive expansion) map key enumeration + direct phrase search — with golden fixtures, a C smoke suite driving every symbol (radar-enforced 124/124), header drift gate, 3-OS release-profile CI smoke + ASan/UBSan/LSan on Linux, and per-platform release artifacts (cdylib + `corvid.h` + `golden/`). docs/FFI.md is the locked contract; crossing cost measured at parity with native (BENCHES.md).

**Audit gaps resolved** (see the gap sweep): incremental+persistent indexes (#4/#7), inverted FTS (#3), filtered-ANN in builder (#5), order_by/offset (#10), reverse edges (#11), format version (#12), auto-keys (#8), MCP surface+caps (#13/#27), reserved-name & length & slicing hardening (#17/#18/#20), atomic edges + single-snapshot join (#2/#14), nested select + explain (#22/#26 partial), edge weights (#25), CI/LICENSE/CHANGELOG/MSRV + property/concurrency tests + benchmarks (#28–32).

**On-disk indexes (DONE).** All three store state as redb entries, bound memory to the operation, and persist with no rebuild on open:
- `create_vector_index_ondisk` — HNSW graph nodes on disk; insert/search touch only nodes-per-op; bulk backfill batches commits with a shared node cache. Now also supports **quantized on-disk storage** (`create_vector_index_ondisk_quantized`, binary ≈32× / scalar ≈4× smaller on disk + page cache) via the shared `quant` module — the footprint path for billions of vectors on a laptop. Recall holds (scalar ≥0.80 vs exact).
- `create_text_index_ondisk` — BM25 postings on disk; a query touches only its query terms.
- `create_scalar_index` — order-preserving keys; selective equality/range filters and counts go sub-linear (1M: 662ms scan → ~3ms eq / ~0.5ms 100-row range), with a candidate cap that falls back to the bounded streaming scan when a filter isn't selective.
- `create_geo_index` — fixed-resolution grid cells (~0.1°) as order-preserving keys; radius/bbox queries (and the builder's `within_km` filter) scan only the cells the bounding box overlaps, then verify exact haversine. Cap-fallback to a scan for continental-scale or antimeridian-wrapping queries.

**WASM / mobile (engine + browser runtime shipped).** The engine compiles to `wasm32-unknown-unknown`; a `cdylib` harness (`corvid-wasm`) links it and the bundle is **≈0.2 MB gzipped** — well under the 2 MB budget — enforced in CI. The engine also cross-compiles for `aarch64` iOS/Android (CI builds android). The browser runtime shipped 2026-09-02 in corvid-js: a dedicated Worker hosts the engine over one OPFS file per database (`FileSystemSyncAccessHandle` behind redb's `StorageBackend` seam), the main thread gets the Promise-flavored `openOpfs`/`AsyncDb` mirror, and the whole golden suite — including the two file-backed fixture files — runs in real Chromium in CI (267/267; contract: corvid-js `docs/OPFS-SPEC.md`).

**Product Quantization (DONE — both indexes).** `create_vector_index_ondisk_pq(field, metric, m, k)` trains a deterministic per-subspace codebook (k-means) from a bounded sample of existing vectors, stores each vector as `m` code bytes, and persists the codebook in the index namespace (reloaded on open). `create_vector_index_pq(field, metric, m, k)` (roadmap Task 10) applies the same storage to the in-memory HNSW via `Hnsw::with_pq`: the trained codebook persists in the index's reserved namespace in the same transaction as the definition, so the lazy post-reopen rebuild re-encodes under the same codebook rather than a retrained one. The metric×storage matrix is identical on both indexes: every metric serves — L2 through the asymmetric-distance table (`Pq::l2_table`/`adc_l2`, the fast path), cosine and dot through reconstruction (decode + metric). Footprint arithmetic: `m` code bytes per vector vs `dim * 4` (e.g. 64-d at `m=16` → 16× smaller vector payload, plus the one-off `m × k × dim/m`-float codebook). Measured on the pinned bench corpus (2000×64d, `m=16, k=256`): build 367.9 ms vs 124.9 ms full-precision (2.9×), search 35.8 µs vs 19.1 µs (1.9× — ADC's compressed distance scale breaks HNSW's early-stop less often, so the frontier touches more nodes). The `pq.rs` core is standalone and validated (recall ≥0.6 at 8× compression); in-memory PQ recall on the fixed clustered conformance corpus measures 1.0 (pinned ≥0.7 — the public path's over-fetch + exact rerank recover the full top-k), and the direct-API unit corpus measures 0.56, identical at ef 100/200/400 (pinned ≥0.55; the thin unit margin is recorded in docs/BENCHES.md).

**Deliberately deferred (large subsystems, with reasons):**
- **ADC fast path for non-L2 metrics under PQ**: PQ storage serves every metric on both indexes, but only L2 has the table-based asymmetric-distance fast path — cosine and dot score by reconstruction (decode + metric). Asymmetric inner-product/cosine tables are future work if ever measured hot.
- **Browser support (OPFS StorageBackend + wasm-bindgen)** — *SHIPPED 2026-09-02, deferral closed by execution* (the 2026-05-29 deferral stood until product signal arrived — the corvid-js binding program became that signal). One OPFS file per database over `createSyncAccessHandle` (truncate-first growth; the SAHPool block-device fallback was recorded and never needed — the plan's YAGNI ruling held), reached through redb's public `StorageBackend` seam (`Store/Db::open_with_backend` + `backup_with_backend`, engine v0.3.4), a Worker-hosted engine with postMessage RPC, an async OOP mirror on the main thread, and Chromium CI proving all 8 golden fixture files plus reload/cross-tab persistence contracts (corvid-js `docs/OPFS-SPEC.md` is the review-gated contract). Durability is bounded by the browser's `flush()` semantics; crash-consistency is redb's checksummed format, as on desktop.
- **Streaming cursor over ranked result sets** — *deferred with trigger: a workload whose ranked sets are too large to materialize (decision log, 2026-08-30)*: keyset (cursor) pagination over a collection shipped (`page`/`page_where`, with a `next` cursor, no offset rescans); single-source ranked queries are bounded (index fast paths + streaming top-k) and filter-only queries stream. What remains is a public streaming *cursor* over arbitrary *ranked* result sets (incremental pull) — multi-source RRF fusion still materializes the candidate set.
- **CJK / positional analysis** — *shipped in full (CJK bigrams closed 2026-08-30, ledger-closure Task 4)*: phrase / positional text search shipped earlier (`phrase_search`, positions stored in both the in-memory and on-disk inverted indexes; the analyzer does stop-word removal + S-stemming, shared by index and query), and CJK runs now tokenize as sliding BIGRAMS — single-char run → that char; the Han↔kana script transition inside one run does not restart the window; boundary: hiragana/katakana U+3040–30FF (prolonged mark ー included), Han U+3400–4DBF / U+4E00–9FFF / U+F900–FAFF / U+20000–323AF; hangul and halfwidth kana deliberately outside (Korean is space-separated, so whole runs are its tokens — the latin behavior); the iteration-mark class (々 U+3005, 〆 U+3006, 〱–〵 U+3031–3035) is likewise outside — std-alphanumeric but not in the CJK ranges, so a mark SPLITS the surrounding CJK run (the mark itself joins the non-CJK whole-token piece, like any latin run) — index and query share the one tokenizer, so both sides split identically at marks (NFKC upstream if bigram segmentation across them is wanted); no dictionary segmentation, no dependencies. Stemming and case folding never apply to CJK; the one shared analyzer feeds index and query on every serving path, so bigram positions line up and phrases over bigrams match in order (`東京タワー` matches, `タワー東京` does not — pinned).
- **Statistics-driven cost-based planning**: selectivity-driven index selection shipped — the builder probes every serviceable index (each capped) and drives on the smallest candidate set, so the most selective index wins without persisted statistics; `.plan()` gives an identity-hashable [`QueryPlan`] and `PlanCache` keys prepared work by shape. Choosing between viable index paths by an estimated *cost model* over persisted statistics is deferred (decision log, 2026-08-30: selectivity probing serves the current scale).
- **Page-level single-snapshot for `page`/`page_where`** — *shipped (roadmap Task 12, 2026-08-29)*: the entire chunked walk runs inside ONE read transaction (the builder's `run()` → `run_with(reader)` discipline; chunked reads inside the txn keep memory bounded), so each page is one MVCC snapshot — identical results, `Page{rows,next}` shape, and cursor contract on a static database (only the consistency guarantee tightened). Snapshot-holding cost is space, not latency: redb never blocks the writer on a read txn, but the walk pins pages freed by concurrent commits until it ends — bounded by `limit` rows; successive page calls each see the then-current state.
- **`geo_nearest` per-radius snapshots**: the expanding-radius search re-runs `geo_within_radius` (its own read snapshot) per radius step; results are exact per step, but the k-nearest answer as a whole is not one snapshot.
- **`bulk` panic skips the closing flush**: `Db::bulk` flushes (fsyncs) only on normal return — a panic inside the closure unwinds past the flush; the next durable commit on any thread makes the writes durable. Rebulk or reopen if you must not rely on that.

### Migrating pre-wave-4 dumps (`a__b`) — shipped (Task 8)

Collection names containing an interior `__` (e.g. `a__b`) were accepted
before audit-remediation wave 4 and are rejected by name validation since
(a user `a__b` could forge or collide with the engine's `__`-separated
internal namespaces and index-def keys). The dump format preserves such
names, so a dump from an old database still carries them — a plain
`Db::load` fails at index/schema replay with
`Error::InvalidName` naming the old collection.

Recipe: dump the old database with the OLD binary version (or use an
existing dump file), open a fresh database with the current engine, and

```rust
let mut renames = BTreeMap::new();
renames.insert("a__b".to_owned(), "a_b".to_owned());
db.load_with_renames(reader, &renames)?;
```

Every collection-name occurrence in the dump stream — documents, all
index/schema definitions, TTL entries, graph edges, auto-id counters — is
mapped through `renames` before replay. Indexes therefore rebuild under
the new name automatically: definitions replay via the create-* backfill
path, which reads the records already written under the target name
(there is nothing to re-create by hand). Contract: each target must be a
valid user name (the offending target's `Error::InvalidName`, checked
before the stream is read); no two dump names may load into one output
name — two map sources sharing a target, or a target colliding with an
unmapped dump collection, is `Error::InvalidArgument` (either merges two
collections' rows into one keyspace and would silently overwrite
documents); engine-reserved dump names are rejected before mapping (a
rename cannot launder an internal namespace); a map entry whose source
never occurs in the dump is a no-op. Loading into a non-empty database
still merges with pre-existing collections, exactly like `Db::load`. The
MCP `load` tool takes the same table as an optional `rename` object
param.

### Dump format versions (v2, u64 prefixes) — shipped (Task 9)

The dump's 12-byte magic IS the version marker: `CORVIDDUMPv1` (legacy)
or `CORVIDDUMPv2` (what `Db::dump` writes today). v1's length/count
prefixes — byte-field lengths (strings, keys, values), the compound and
schema per-def field counts, and the PQ `m`/`k` parameters — were u32, so
a single value, key, string, or field count at or above 4 GiB could not be
represented (and the writer's `as u32` truncated silently). v2 widens
every one of those prefixes to u64: no 32-bit representable limit remains
anywhere in the format. Section counts (the record count and every
section's entry count) were already u64 and stay so; fixed-width fields
(i64 TTL expiries, f64 edge weights, u64 auto-id counters) are
byte-identical across versions.

Compat matrix (one-way, by design): the loader accepts v1 AND v2 — the
prefix width is decided by the header magic and nothing else differs; the
writer emits v2 only. A v1 dump in the wild keeps loading unchanged; a
dump re-taken with the current binary comes out v2; an unknown magic
(e.g. a future v3) is `Error::InvalidDump` in older binaries, exactly as
a v2 dump is in pre-v2 ones — that is the migration story: dump with the
old binary, load with the new. `load_with_renames` applies identically
to both versions. The full format spec lives in `migrate.rs`'s module
doc.

---

## Scaling characteristics (measured)

From the `scaling` example (file-backed, 16-dim embeddings), after the
streaming/index optimizations. Times are indicative (one machine), the point is
the *shape*.

| Operation | 1k | 100k | 1M | memory |
|---|---|---|---|---|
| batch insert | ~16ms | ~0.6s | ~4.9s | bounded (per batch) |
| `count()` (no filter) | µs | µs | ~12µs | O(1) — maintained counter |
| point `get` | µs | ~15µs | ~22µs | O(1) |
| filtered `count` / `group_count` | <1ms | ~55ms | ~0.55s | **constant** (streamed) |
| `order_by` + `limit` | ~1ms | ~0.13s | ~0.58s | **bounded** (≈ page size) |
| `text_search` (indexed) | µs | µs | µs | — (after build) |
| HNSW build | — | ~15s | (minutes) | in-memory |

**What scales (constant or bounded memory, O(1)/O(n) time):** storage, point
ops, `count`/`len` (O(1)), streamed aggregates, ordered pagination, and the
bounded-heap exact vector search. These hold at 50M (slower, but no OOM).

**The walls at 1M–50M — and what now addresses each:**
- **In-memory index build/rebuild.** The *in-memory* HNSW and inverted index
  live in RAM and rebuild on open; at 1M the in-memory HNSW build is minutes, at
  50M they don't fit. The **on-disk** variants (`create_vector_index_ondisk`,
  `create_text_index_ondisk`, plus the quantized/PQ on-disk vector indexes)
  remove this wall — state lives as redb entries, an op touches only the
  nodes/postings it needs, and the index persists with no rebuild on open. Use
  the in-memory variants up to ~100k–1M, on-disk above.
- **Unindexed `filter`/`order_by`** are O(n) scans (constant memory, linear
  time). The **scalar / compound / geo indexes** (`create_scalar_index`,
  `create_compound_index`, `create_geo_index`) make selective equality/range,
  prefix-equality + trailing-range, and radius/bbox/nearest queries sub-linear;
  the builder picks the most selective available index automatically and falls
  back to a bounded scan when none is selective enough.
- **Exact (unindexed) search** is O(n) time — correct and OOM-free (streamed),
  but you want an index past ~100k.

So: storage, point ops, counts, streamed aggregates, and ordered pagination
scale to 50M with bounded memory; large-scale *search* and *selective filters*
are served by the on-disk and scalar/compound/geo indexes — leaving only the
in-memory index variants' RAM build as a deliberate small-scale convenience.

## Transaction model

- **Single writer (whole database)**, multi-reader MVCC. Inherited from redb.
- **Snapshot isolation for reads.** Serializability is not on the roadmap — it costs more than I want to pay.
- **No savepoints in v0.1.** Add if needed for a specific real use case.

### The hard invariant

> Every secondary index reflects the same committed state as the primary table, at the same MVCC version.

This is the project's central technical commitment. The reason: it's the only way the fluent builder API can compose multi-modal queries without users having to reason about cross-index staleness.

### ⚠ Critical tension: the invariant vs. "wrap hnswlib-rs / tantivy"

The invariant says *one committed state, one WAL*. But hnswlib-rs and tantivy each carry **their own persistence and their own crash recovery**. They do not enlist in redb's write transaction. A commit touching `row + HNSW graph + FTS posting list` writes to **three independent storage systems**. You cannot have a true single-WAL atomic cross-modal commit *and* wrap those two as storage. Pick one:

- **(a) Reconcile-on-open (pragmatic, v0.1 default).** Each index records the last row-version it has absorbed. Indexes are persisted separately. On open, replay row changes past each index's watermark. A crash mid-commit costs a bounded catch-up pass, not corruption. The invariant becomes *"consistent after open completes"* rather than *"atomic at every instant."* Cheapest path to shipping.
- **(b) State-in-redb (true invariant, expensive).** Use the libraries' *algorithms* but store their state as redb entries (HNSW nodes as KV records, posting lists as KV records). One WAL, genuinely atomic. This is **not "wrapping"** — it's most of the engineering, and it means forking hnswlib-rs/tantivy down to their algorithms. The real version of the north star eventually wants this.
- **(c) Per-index durability forever.** Drop the "same WAL" language; accept that indexes have independent durability and document the reconciliation semantics as the contract.

**Decided: destination is (b).** The invariant is the entire reason this database exists, and the identity gets built from scratch — so we do not wrap hnswlib-rs/tantivy *as storage*. We use their algorithms and store index state as redb entries. The one subtlety that keeps it shippable: **the contract is (b) from day one.** The public API and the guarantee the builder makes promise atomic single-WAL cross-modal semantics immediately, because the data model + API are the part we can't cheaply migrate. v0.1's *implementation* may scaffold with (a) reconcile-on-open behind that contract — but (a) is a hidden, throwaway implementation detail, never a promise. When (b) lands underneath, no API changes.

### Known availability characteristics (audit B9, documented — not fixed)

- **One global index-registry mutex during builds/compaction.** A lazy in-memory build or a compaction runs under the single registry mutex; vector searches on *any* collection block behind it for the build's duration. A per-collection lock split is deferred with trigger — build-vs-search contention observed on a real workload (decision log, 2026-08-30).
- **Registration/compaction holds the write lock while clearing a namespace.** Re-registering or compacting an on-disk index clears its whole namespace in one write transaction; for a large index, redb's single database-wide write lock is held for that clear, so writers db-wide wait. Splitting the clear into bounded batches is the follow-up if re-registration of very large indexes becomes routine.
- **Graph edge cascade is O(edges-of-document) per delete, and the first
  cascade (or first link) on a legacy database builds the adjacency.**
  Deleting a document resolves its edges through two private endpoint-first
  adjacency namespaces (`__adj_out__`/`__adj_in__`; derived state re-keyed
  from the edge rows — see graph.rs), maintained transactionally by
  link/unlink, so steady-state deletes touch only the deleted key's rows.
  The one-time lazy build runs inside the first edge write's or first
  cascade's own transaction (serialized against concurrent edge writes by
  the store's write lock), which for a large legacy edge namespace holds
  the write lock for the build's duration. `link`/`link_weighted` write
  two extra rows per edge (measured ~1.4× on the pure-link microbench —
  the price of O(degree) cascades; see `edge_churn`). Tampering posture:
  a valid current-version marker with externally deleted adjacency rows
  diverges silently (the marker invariant trusts "present ⇒ complete") —
  the same pre-existing posture as externally deleted `__edges__` rows
  themselves; out of threat model.
- **A PQ index definition can outlive its training corpus, and then a
  dump→load of that database fails.** Deleting every document that
  carries the indexed field leaves the definition intact (deletes never
  unregister), so `dump` still emits the PQ def — and `load`'s replay
  calls the create path, which requires documents to train on and fails
  `EmptyIndexTraining` (both modes: the on-disk PQ index, dump vector
  mode 2, and the in-memory PQ index, mode 3 — a pre-existing family
  shape, not new in either). Recover by re-creating the index after the
  load, or dropping the def before the dump of a drained collection.

---

## Fluent builder API

### Shape

```rust
db.table("docs")
    .vector_search("embedding", &query_vec, 100)
    .filter(col("category").eq("blog"))
    .filter(col("metadata").json_path("$.author").eq("rocky"))
    .text_search("body", "rust embedded database")  // hybrid
    .fuse(Fusion::Rrf { k: 60 })
    .rerank(Mmr { lambda: 0.7 })
    .limit(10)
    .select(&["title", "body", "embedding_distance"])
    .run()?
```

### Properties

- Each method returns a `Plan` node. The chain builds a tree.
- The tree is identity-hashable for plan cache lookup.
- Predicate pushdown applies before execution (filters move into vector_search and text_search candidate generation when possible).
- No closures in the public API for predicates. Filters are expression nodes (`col(...).eq(...)`). User-defined functions are a separate, opt-in mechanism (deferral past v0.1 stands; decision log, 2026-08-30).
- Sync by default. Async wrapper is shallow.

### What this rejects

- Diesel-style typestate that encodes every column in the type signature. Collapses on multi-modal joins. Use runtime errors for shape mismatches; rely on tests.
- SQL string interpolation, even hidden. The builder is the only entrypoint.

---

## WASM specifics

- **Build artifact:** one `.wasm` module + JS wrapper. Loaded inside a dedicated Worker.
- **VFS:** one OPFS file per database over `FileSystemSyncAccessHandle` (truncate-first growth; SAHPool block-device fallback recorded, never needed). Accept single-connection-per-tab constraint (surfaced as `Busy`).
- **Threading:** main thread is RPC. All DB work happens in the Worker. No `SharedArrayBuffer` requirement (the COOP/CEP synchronous API remains a recorded non-goal).
- **Bundle size goal:** under 2MB gzipped for v0.1. Tantivy may push past this; if so, ship lighter BM25.
- **Storage layout:** single OPFS file via a sync access handle. Easier crash recovery than directory-of-files.

## Mobile specifics

- **VFS:** pread/pwrite with WAL and fsync.
- **No mmap by default.** Jetsam kills + fd lifecycle make mmap fraught.
- **Durability default:** WAL + fsync on commit. Crash safety over throughput.
- **Bundle size goal:** under 5MB for a release-stripped binary, ideally under 3MB.

---

## Cross-cutting

### SIMD

- **Distance kernels — closed 2026-08-30 by measurement** (decision log): the lane folds auto-vectorize (4-wide NEON verified in release assembly) and sit at the exactness-preserving envelope — every measured faster shape reassociates `f32` summation, which the bit-exactness oracle declines; volume scans are memory-side; the real throughput lever is Binary quantization, which ships.
- `std::simd` (portable) by default *if* any future hot path reopens this — it is nightly-only as of rustc 1.91
- x86_64 AVX2 + AVX-512 specializations behind `cfg(target_feature)`
- aarch64 NEON specializations behind cfg
- Coverage: BM25 scoring, posting list decode, JSON parsing (pure-Rust SIMD), hash table probing, filter evaluation (vector distance removed from this list by the closure)

### Memory

- Query-scope arenas (`bumpalo`) on hot paths, zero allocations during query execution where possible — **DECLINED** (decision log, 2026-08-30: Task 2's measurements show the hot paths are bandwidth-bound; arena allocation buys nothing the numbers support)
- One global buffer pool, sized per VFS backend — deferred with redb, its page cache serving the layer (decision log, 2026-08-30)
- Cache-line-aligned hot structures.

### Async vs sync

- Public API is sync. Async wrapper is shallow (offload to a thread pool).
- I/O is sync underneath. io_uring is interesting but not in v0.1 — adds platform complexity for unclear win on embedded workloads.

### Zero-copy

- Native result paths return owned Rust types in v0.1.
- Arrow `RecordBatch` path for analytical queries: **DECLINED** (decision log, 2026-08-30) — users convert at the boundary.
- Bytes from disk → query result without intermediate decoding where the data type permits.

### Error model

- Typed errors per layer. `thiserror` for definitions.
- No panics on user input ever. Panics on internal invariants (assertion failures) are acceptable in debug, never in release without a clear contract violation.

### Observability

- Built-in query trace (`.explain()` and `.profile()`) — `.explain()` shipped; `.profile()` deferred: the `tracing` feature's per-operation events (plan-shape selection, backfill pages, compactions) already carry what a profiler would report, so a separate profiler reopens only if a need outgrows them
- Structured logging via `tracing` — **implemented behind the non-default `tracing` cargo feature** (Task 11). Off by default so the zero-dependency posture and the WASM size budget stay contracts, not trade-offs; enabling adds the `tracing` facade (default features trimmed — `span!`/`event!` only, no `attributes` proc-macro tree; the footprint is tracing + tracing-core + pin-project-lite) and no public API. All call sites go through the private `telemetry` shim (`src/telemetry.rs`), which compiles to nothing when the feature is off — no code, no field evaluation, no dependency (CI asserts both the default graph and the wasm graph never contain `tracing`). Instrumented at per-operation/per-page granularity (never per-document): index backfill pages and completion (collection, index family, cursor, page size), compaction trigger math and outcome (dead/live counts, in-memory and on-disk), lazy index resumes and adjacency rebuilds (including stale-vs-absent marker reason), query plan-shape selection (the arm that drove candidates, its row count — labels are named after the `PlanShape` variants with two deliberate divergences: `indexed_window` carries no family discriminator, the scalar/compound/geo/or kind is `plan_shape()`/`explain()`'s to report, and `stream_scan` is intentionally finer than `PlanShape::Scan`, splitting the bounded streaming filter pass from the materializing fallback), the order-index walk's tail-scan fallback, the edge-cascade corrupt-row fallback, and semantic-cache hit/miss.
- Counters for cache hits, index probes, plan cache hits — **subsumed by the tracing events above**: a subscriber counting `plan_shape` events per `shape` is the index-probe counter per shape (families within `indexed_window` — scalar/compound/geo/or — split via `plan_shape()`/`explain()`, not the event); `semantic_cache_hit`/`semantic_cache_miss` are the cache-hit-rate counters. Explicitly deferred: plan-cache hit counters (`PlanCache` is host-side state — the engine sees no get/miss traffic to count; no trigger until that changes) and any metrics-export subsystem (non-goal below; no workload has asked).
- No metrics export to external systems in v0.1.

### Documentation discipline

- Every non-obvious decision gets a paragraph in `decisions/`.
- File format changes get a separate migration note.
- Public API examples are runnable doctests.

---

## Open questions

These need answers before specific layers can be implemented. Listed in rough order of when they block work.

1. **Tantivy WASM bundle size — decided (moot).** Own BM25 over own posting lists shipped instead; tantivy was never adopted, so the bundle-size question never arose. (Kept for the record: borrowing its tokenizer/automaton ideas remains allowed by the component map.)
2. **redb's behavior on OPFS-SAHPool — decided by shipping (2026-09-02, moot as asked).** redb works over OPFS out of the box — not via SAHPool but simpler: a `StorageBackend` over one sync access handle's read/write/truncate/flush (the CoW B+-tree needed only the pread/pwrite surface, as the 2026-05-29 row predicted). Proven by the corvid-js browser CI: all 8 golden fixture files over real OPFS, including persistence-across-reload and cross-tab single-writer.
3. **Cross-modal commit semantics — decided and implemented** (see *Critical tension*): destination (b) state-in-redb, contract (b) from day one. The former "remaining concrete work" (redb layout for HNSW nodes and posting lists; commit ordering inside one write txn) is done — persisted index maintenance commits inside the document's transaction, and creation runs the `Building{cursor}`→`Complete` state machine (decision log, 2026-08-27 wave-2 row).
7. **JSON parsing — decided.** One pure-Rust path behind a `JsonParser` trait for all targets in v0.1 (portable SIMD128 on WASM for free, no C++ FFI, smallest bundle). Clean seam to drop in C++ simdjson via FFI on **native only** *if and when* profiling shows JSON parsing is a real hot path. No premature optimization of a replaceable component. (General note: "SIMD everywhere" degrades on WASM — no AVX-512/NEON, only SIMD128.)
8. **Filtered vector search — decided (semantics), partially implemented.** `.filter()` on a vector source is a **true predicate**: the result is the top-k rows satisfying the filter, never top-k-then-drop. Shipped: the default filtered path runs exact (a bounded scan over matching rows — correct at any selectivity), and `.approx()` opts into the index (over-fetch then filter, which may return fewer than `limit` on a highly selective filter). Pushing the predicate into the graph walk itself (filtered-HNSW) is deferred with trigger — filtered ANN becomes hot (decision log, 2026-08-30) — so the *indexed* path stays over-fetch-then-filter until then.
4. **Plan cache identity hashing.** AST nodes need stable `Hash` impls that ignore allocator addresses. Closures cannot appear in the AST (already decided); but constants embed user data — design the hash to treat structural identity, not value identity, where appropriate.
5. **Schema migration story.** "No backward compat" doesn't mean "no migration." Need a one-shot dump/load utility from format N to format N+1. Design it before declaring v1.0.
6. **The first real app.** Which one? It shapes everything. Likely candidates: a personal RAG / second-brain system; a code-aware AI assistant memory store; an agent's working memory layer. Pick one, build it in parallel with v0.1.

---

## Decision log (start)

> Dating note: rows dated 2026-05-29 were backdated to the project's start
> when this log was first written — they record decisions made during the
> v0.1 build (history preserved, not rewritten). The 2026-08-27/28 rows are
> the audit-remediation decisions, dated when decided (spec approval
> 2026-08-27) with landing spread across the five remediation waves ending
> 2026-08-28.

| Date | Decision | Rationale |
|---|---|---|
| 2026-05-29 | Wrap redb for storage, not fjall | CoW B+-tree easier to integrate with cross-modal atomic indexes than LSM (no compaction races). Author publishes unfavorable benchmarks — credibility signal. |
| 2026-05-29 | Vector/FTS: use the algorithms, store state in redb — don't wrap their persistence | Wrapping their storage breaks the single-WAL invariant. The invariant is the identity, so the storage is ours. |
| 2026-05-29 | Single writer per table, MVCC reads | Inherits from redb. No need for full MVCC writers; embedded workloads rarely need them. |
| 2026-05-29 | No SQL, ever | Frees the planner from ANSI semantics; the fluent builder is the only entrypoint. |
| 2026-05-29 | WASM build is Worker-only | `createSyncAccessHandle` is Worker-only by spec. Main thread is RPC proxy. |
| 2026-05-29 | Native Rust results in v0.1, Arrow later | Avoid Arrow's binary-size cost in the WASM build until analytical queries are first-class. |
| 2026-05-29 | No tensor ops, ever | Users plug in candle/burn. Scope discipline. |
| 2026-05-29 | No backward compat in file format until v1.0 | Per global preference. Migration scripts on each shape change. |
| 2026-05-29 | Single writer is whole-database, not per-table | redb's actual model. Per-table writers would need a different substrate; not worth it for embedded write patterns. |
| 2026-05-29 | v0.1 storage layer = redb `StorageBackend` impls, not an owned page format | Wrapping redb means checksums/commit/recovery are redb's. The owned page format is a future-engine concern, documented at L0 only as a seam. |
| 2026-05-29 | redb does not require mmap | Makes the OPFS-SAHPool WASM path "implement `StorageBackend` over OPFS" — no mmap to work around. |
| 2026-05-29 | Cross-modal atomicity destination is (b) state-in-redb; contract is (b) from day one | The invariant is the project's identity → built from scratch, not wrapped. API promises (b) immediately; v0.1 may scaffold with (a) reconcile-on-open behind that contract, swappable with no API change. |
| 2026-05-29 | JSON: one pure-Rust path behind a trait in v0.1; FFI simdjson on native only if measured hot | Replaceable component, not identity. Perf is last priority; data model/API first. |
| 2026-05-29 | Primary driver = sidecar MCP server (`corvid-mcp`) for agentic IDEs/tools, over stdio | Keeps the design honest with a real consumer; subsumes RAG + agent-memory + code-store. Engine stays embedded; protocol lives in a separate crate. |
| 2026-05-29 | License: MIT only | User's call. |
| 2026-05-29 | Name: `corvid` (engine), `corvid-mcp` (sidecar) | `neodb` was taken on crates.io. Corvid = crows/ravens: eat anything, highly intelligent, cache food across thousands of remembered locations — "stores everything and recalls it" maps onto an AI memory store. Confirmed available on crates.io. |
| 2026-05-29 | Filtered vector search = true predicate (correct by default, `.approx()` opts out) | Correctness-first; the filter is a real predicate, not top-k-then-drop. |
| 2026-05-29 | First build pass: L1–L4 + sidecar core, exact baselines for vector/FTS | Ship a usable, fully-tested core fast; HNSW/inverted indexes slot in behind the stable API later. Builder applies filters before ranking to honor the true-predicate contract. |
| 2026-05-29 | Sidecar built as transport-agnostic tool layer first | `Server::handle` (JSON in/out) holds all behavior and is fully testable; MCP/stdio framing is mechanical glue deferred to wire against the MCP SDK. |
| 2026-05-29 | On-disk indexes store state as redb entries, never a wrapped library's own persistence | Per CLAUDE.md. On-disk HNSW (graph nodes), on-disk inverted text (postings), and the scalar index (order-preserving keys) all persist as redb entries: bounded memory (touch only what the op needs), no rebuild on open. |
| 2026-05-29 | Scalar index = order-preserving key encoding, returns a verified superset | Numbers (int+float) share a lane keyed by IEEE-754 total order of the f64; the i64→f64 cast is monotonic, so a range scan never excludes a true match. The builder re-checks every candidate against the exact predicate, so encoding ties only cost a few extra checks, never correctness. `Ne` is not serviced (a full anti-scan isn't sub-linear). |
| 2026-05-29 | Quantization extracted to a shared `quant` module; on-disk index stores quantized vectors | One implementation (encode/probe/distance + binary/scalar codecs) backs both the in-memory and on-disk HNSW, so their recall matches. The on-disk graph stores the quantized form (decoded with the index's mode on read), cutting disk + page-cache footprint for the billions-of-vectors path. |
| 2026-05-29 | WASM proven via a `cdylib` size harness, not a full browser build | The engine compiles to wasm32 and links into a ≈0.2 MB-gzipped bundle (CI-enforced < 2 MB). A real browser API (wasm-bindgen + OPFS StorageBackend) is Worker-only and needs a browser to validate — kept separate so the engine's wasm-readiness is checked every CI run without a browser harness. |
| 2026-05-29 | Released v0.1.0 then v0.1.1; multi-platform release binaries | v0.1.0 shipped a Linux-only binary; v0.1.1 builds `corvid-mcp` for Linux (x86_64 + aarch64), macOS (Intel + Apple Silicon), and Windows via a matrix. v0.1.0's published release is left intact; a new tag is cleaner than rewriting a published one. |
| 2026-05-29 | Docs reconciled to the shipped surface; `cargo doc -D warnings` gated in CI | Verified every README/GUIDE/DESIGN claim against the code. Confirmed in-memory PQ is **not** shipped (in-memory HNSW supports None/Binary/Scalar only; PQ + the L2 ADC fast path are on-disk), corrected the stale "deferred" list and scaling walls, fixed broken/redundant rustdoc links, and added a CI doc-lint so the API reference can't silently rot. |
| 2026-08-27 | Index creation = persisted Building{cursor}→Complete watermark with lazy first-use resume; queries never serve Building defs (exact/bounded fallbacks); dump/load materializes Complete via create_* replay | Closes audit A2: register-then-backfill left permanently partial indexes after crash/error/race; single-txn backfill would hold redb's write lock for the whole corpus (the availability cliff), staging-swap doubles maintenance paths |
| 2026-08-27 | Re-registration = one transaction {def→Building, codebook?, clear namespace}; compaction = same cycle triggered at >50% dead (Meta.dead counter); both re-backfill via the wave-2 driver | Closes A5/B5: mixed-encoding graphs, leaked namespaces, tombstone recall collapse — and collapses the register two-txn race + PQ codebook window |
| 2026-08-27 | vector_search reranks ANN hits with exact distances; Hit.approximate marks the candidate source; corrupt on-disk state errors (CorruptIndex) instead of silent-empty | Closes B6 + C1/C13/decision 5: quantized-scale distances broke semantic_cache thresholds; silent-empty hid corruption |
| 2026-08-27 | Query execution (run/aggregations/candidate paths/traverse) reads one MVCC snapshot via a SnapshotReader trait; phrase fallback scores BM25; TTL maintenance decided in-txn; doc delete cascades graph edges; dump is one snapshot, load streams; user names reject interior `__` and NUL (InvalidName, breaking) | Closes B2/B3/B4/B7/B8: per-key read txns could return sets matching no point in time; purge raced rewrites; deletes orphaned edges; path-dependent phrase scores; dump was torn + unbounded |
| 2026-08-28 | Wave 5: argument validation (execution-time), antimeridian bbox, order_by class semantics, backup debris cleanup, explain() real plan shapes, CI (iOS/timeout/concurrency) + release hardening, full docs/claims reconciliation, AUDIT rewritten as status doc | Closes the audit's D-family and C2-C14 residue; the public surface now claims exactly what the code does |
| 2026-08-29 | Verify-candidates fetch is density-driven: one ordered `for_each` walk of the records when `candidates × 17 ≥ collection count` (dense window), else the historical per-key point-gets — identical rows, candidate order, filter verdicts, and snapshot either way (audit B3 unchanged: the walk runs on the caller's reader) | A point-get costs a catalog lookup, a table open, and a tree descent per key — measured ≈17× one sequential row visit of a walk (crossover ≈5.8% window density on the 5k bench); dense windows verify ~22% faster end-to-end, sparse windows keep the point-gets' sub-linear reads instead of walking the whole collection. Closes AUDIT Open "Verify-candidates batching" |
| 2026-08-29 | A filterless `order_by(field)` over a complete scalar index is served by an index ORDER WALK (`PlanShape::SortIndex`): the numeric then text lane enumerates the comparable class in the pinned total order, same-encoded buckets re-sort with the exact comparator (i64s beyond 2^53 share an f64 encoding; NaN entries are skipped to the tail), docs are fetched only for the offset+limit window, and an on-exhaustion scan appends the incomparable/missing tail — identical results by construction, or decline (any source, any filter, or no complete index keeps the existing paths) | The lanes already sort to the class-0 contract order (numbers before texts ascending), so the walk replaces the unbounded materialize+sort with window-bounded reads: 5k-doc `order_by(n).limit(20)` goes 2.64 ms → 0.29 ms ascending, 2.79 ms → 1.02 ms descending (no reverse range scans in redb: descending pages forward keeping a bounded newest-buckets buffer). Closes AUDIT Open "Sort indexes" |
| 2026-08-29 | Compound prefix-only windows re-enabled soundly: each compound def persists an `all_docs_indexed` flag (def-record kind byte; legacy rows decode false) set at backfill completion iff no miss was ever observed — backfill pages commit a miss marker per declined doc, document maintenance persists a miss (marker while Building, def-rewrite once Complete) in the write's own transaction, re-registration clears the markers (fresh cycle), and the completion computes `flag-aware ∧ marker-free` in one transaction; the planner probe and its plan_shape twin admit prefix-only windows only under the flag, full-coverage shapes regardless | With the flag, every document has ALL index fields present and encodable, so every doc matching a prefix-only filter (matching requires the leading field present, encodable, equal) IS in the index — the window is a verified superset; without it, the fabfe6e omission bug's shape (missing-tail docs match but sit outside the index) stands and the query declines to scan. 5k-doc all-present corpus: prefix-only equality goes 1.540 ms scan → 232.41 µs window (6.6×, `compound_prefix_scan` bench). Closes AUDIT Open "Compound prefix-only windows" |
| 2026-08-29 | Edge adjacency is DERIVED state in two private namespaces (`__adj_out__<coll>`/`__adj_in__<coll>`), one row per edge per endpoint, endpoint-first re-keyed; NOT a change to the edge-row layout | The edge rows (`__edges__`/`__redges__`) stay the only source of truth and keep their format — no file migration (a permanent non-goal), legacy databases simply build the adjacency lazily inside the first edge write's or first cascade's transaction (one clear + one paged re-derive from the source rows + a version marker, all atomic — a crash rolls it back and the next use rebuilds). Per-edge rows (not per-endpoint consolidated values): a hub's value would make every link to it an O(degree) read-modify-write, while per-edge rows keep link O(1) and idempotent by key. `link`/`unlink` maintain the rows transactionally (two extra rows per edge, the measured ~1.4× on the pure-link microbench); a malformed adjacency row falls back to rebuild-from-source + re-run. Deletes go O(edges-of-doc): `edge_delete_sweep_100` 241.0 → 40.5 ms, `delete_half_2p5k` 566.8 → 194.5 ms. Closes AUDIT Open "Endpoint-indexed edge layout" |
| 2026-08-29 | `a__b` migration: `Db::load_with_renames(reader, renames)` maps EVERY collection-name occurrence in the dump stream (records, index/schema definitions, TTLs, edges, auto-id counters) through a caller-supplied rename table before replay; reserved dump names are rejected before mapping, targets are validated upfront, and one output name is allowed per dump name | Pre-wave-4 databases accepted `__`-containing collection names; `validate_name` rejects them since wave 4, so their dumps failed `Db::load` at index/schema replay with no automated path (rename by hand on the source, or re-create indexes after load). Mapping all occurrences together keeps documents and their definitions in one keyspace — index defs replay through the create-* backfill over the already-renamed records, so every index rebuilds under the new name automatically. Collisions (two sources sharing a target, or a target an unmapped dump collection already occupies) merge keyspaces and are rejected `InvalidArgument`; the MCP `load` tool takes the table as an optional `rename` param. Closes AUDIT Open "`a__b` migration tooling" |
| 2026-08-29 | Dump format v2: the header magic IS the version marker (`CORVIDDUMPv1`/`CORVIDDUMPv2`, 12 bytes), and every u32 length/count prefix of v1 (byte-field lengths, compound/schema field counts, PQ m/k) widens to u64 in v2; section counts were already u64, fixed-width fields are unchanged | v1 could not represent a single value, key, string, or def field count at or above 4 GiB and its writer truncated silently (`as u32`) — the audited limitation; v2 has no 32-bit representable limit anywhere. Compat is one-way by design: the loader accepts v1 AND v2 (the magic decides the prefix width, nothing else differs), the writer emits v2 only — an unknown magic (future v3) is `InvalidDump` in older binaries, which IS the migration story (dump old, load new). Closes AUDIT Open ">4 GiB dump sections" |
| 2026-08-29 | In-memory PQ: `create_vector_index_pq(field, metric, m, k)` + `Hnsw::with_pq(metric, pq, m, ef)` — PQ joins `None`/`Binary`/`Scalar` as in-memory storage; the def kind byte gains `InMemoryPq` (3), and the trained codebook persists in the index's reserved namespace in the SAME transaction as the def (the namespace holds only the codebook row for this kind — no graph), reloaded on open so the lazy rebuild re-encodes under the same codebook; dump/migrate gains vector-mode 3 (`InMemoryPq { m, k }`) | Matching the on-disk PQ contract exactly, no more: every metric serves (L2 via the ADC table, cosine/dot via reconstruction — ADC for non-L2 stays deferred). Footprint is arithmetic (`m` bytes/vector vs `dim*4`; 16× at 64d/m=16, plus the one-off codebook) — pinned by a byte-counting unit test, not an allocator claim. Measured (2000×64d, m=16 k=256): build 367.9 ms vs 124.9 ms None (2.9×), search 35.8 µs vs 19.1 µs (1.9×); in-memory prune scores by reconstruction instead of rebuilding a k·dim ADC table per prune (identical scores for L2 — `adc_l2` IS the squared L2 to the reconstruction — and 2.6× cheaper builds). Recall on the fixed clustered conformance corpus measures 1.0 (pinned ≥0.7); the direct-API unit corpus measures 0.56, identical at ef 100/200/400 (pinned ≥0.55). Closes AUDIT Open "In-memory PQ" |
| 2026-08-29 | Structured logging ships behind a non-default `tracing` cargo feature; every call site goes through the engine-private `telemetry` shim (`src/telemetry.rs`) whose macros expand to nothing (no dependency, no code, no field evaluation) when the feature is off; instrumentation is per-operation/per-page (backfill pages + completion, compaction trigger math + outcome, lazy resumes + adjacency rebuilds with absent-vs-stale reason, plan-shape selection with row counts (labels named after the `PlanShape` variants; two deliberate divergences: `indexed_window` carries no family discriminator — the kind is `plan_shape()`/`explain()`'s to report — and `stream_scan` is finer than `PlanShape::Scan`, splitting the bounded streaming pass from the materializing fallback), order-index tail-scan fallback, edge-cascade corrupt-row fallback, semantic-cache hit/miss); counters are subsumed by those events (plan-cache hit counters explicitly future) | The default build's zero-dependency posture and the WASM <2 MB gzipped budget are contracts; observability must not break either — so it is opt-in at compile time and adds the `tracing` facade (span!/event! only — the attributes proc-macro tree stays out) when on. CI enforces the posture both ways: the check job builds `--no-default-features` and asserts `cargo tree -p corvid --edges normal` never contains `tracing` (plus clippy/doc/test with the feature on); the wasm job asserts the wasm-target graph never contains it. Closes AUDIT Open "tracing / observability" |
| 2026-08-29 | `page`/`page_where` become single-snapshot: the entire chunked walk (1024-key `scan_from` steps) executes inside ONE `store().read()` closure — the query builder's `run()` → `run_with(reader)` discipline applied to pagination, with chunked reads INSIDE the transaction preserving bounded memory | A page call spanning concurrent writes previously observed mixed state (each chunk its own read transaction — per-chunk consistent, the page not one snapshot), an accepted-then-deferred gap once query execution itself became snapshot-scoped (audit B3); closing it costs nothing observable on a static database (`ReadBatch::scan_from` is byte-identical to `Store::scan_from`, pinned in store.rs; the queries.rs cursor pins stayed green untouched) and the snapshot-holding cost is space, not latency: redb MVCC never blocks the writer on an open read txn, the pin (pages freed by mid-walk commits, temporary file growth, `Db::compact`-reclaimable) is bounded by `limit` rows, and successive page calls each open their own snapshot per the keyset contract. Proven by `chunked_read_batch_walk_ignores_mid_walk_writes`: same-thread mid-walk commits of every mutation shape are invisible to the walk's remainder, while the identical timing against per-chunk-own-transaction walks yields a mixed state. Closes AUDIT Open "Page-level single-snapshot" |
| 2026-08-29 | Parallel PQ training (roadmap Task 13): `Pq::train` runs each Lloyd iteration's assignment step chunk-parallel over a scoped, std-only worker team (`src/team.rs` — `thread::scope`-spawned per training call, `available_parallelism` capped at 8, spin-then-park workers, `#![forbid(unsafe_code)]`-clean, no new dependencies) while iterations stay sequential and the update step consumes assignments in input order — the codebook is bit-identical to the sequential path's because a point's nearest centroid is a pure function of the point and the current codebook, and item-indexed chunk outputs preserve the reduction order exactly (pinned by an equivalence test at a corpus large enough to engage the team; recall corpora unchanged). The deterministic HNSW-build parallelizations the task brief sketched were implemented and measured SLOWER — the layer-search heap loop consumes distance evaluations one at a time, so per-insert batches carry microseconds of parallelizable work against a multi-microsecond fork/join handshake (0.21-0.45× at 2k×64d, 0.20-0.25× at 10k×128d; 7 spin-pollers also degraded the caller), and pre-computing per-vector PQ encodes/probes measured ~1.0× (ADC-table materialization traffic) — so HNSW builds keep the seeded-PRNG sequential shape and the parallel headroom is redirected to per-eval work (SIMD kernels, next audit row). |
| 2026-08-30 | Endpoint-direct graph reads: `neighbors`/`in_neighbors`/`neighbors_weighted`/`traverse`'s frontier expansion serve from the adjacency namespaces via the exact length-delimited `(endpoint, relation)` pair prefix (`TAG ‖ len(endpoint) ‖ endpoint ‖ len(relation) ‖ relation`), weights verbatim from the adjacency values; an EMPTY adjacency scan resolves the built-marker with one point-get (current ⇒ genuinely empty; absent ⇒ the source edge-namespace scan answers — legacy databases and never-linked collections), and a traversal hoists marker + namespace-name formatting out of its hop loop | Both backings are provably byte-identical (the same row set under raw-byte order of the same trailing field), so every pinned ordering — traverse's BFS order included — is unchanged, and the measured verdict (`neighbors_hub_10k`) is parity within ±2%: the source prefix was already one contiguous range per fixed-relation pair, so the adjacency buys a fixed-relation read nothing asymptotically — but retiring the row's two mistaken reasons costs nothing either (no relation-filtering pass: the pair prefix is exact; no weight re-fetch: values are carried). Kept because reads and the delete cascade now share ONE endpoint-keyed layout and an endpoint-wide neighbor API would be pre-served. Two load-bearing engineering notes: the non-empty serve skips the marker point-get because rows and marker commit in one transaction (rows on a snapshot ⇒ complete there), and per-hop marker gets cost +28% / per-hop name formatting ~5% on traverse before hoisting (`ReadBatch` has no collection-id cache — every point-get is a catalog seek plus a records seek). Closes AUDIT Open "Endpoint-direct `neighbors`/`traverse` reads" |
| 2026-08-30 | SIMD distance kernels: CLOSED BY MEASUREMENT, no code change — the lane folds stay as written (LLVM already emits 4-wide NEON for both kernels; release-assembly-verified, horizontal reduction preserving source summation order), because every measured faster shape changes `f32` summation and the exactness oracle (bit-identical kernel results underpin the exact-value unit tests and the deterministic HNSW/recall corpora, one of which has a 0.01 recall margin) declines them: 16 accumulator lanes are −29% at 768d but a different summation order (and +14% slower at 64d); fp-fusion via `mul_add` is a different rounding AND de-vectorizes to scalar `fmadd` (+24% slower on stable Rust) | The kernels hold 62–83% of the machine's same-shape read ceiling across dims 64–3072 with no small-dim cliff, and the gap to the ceiling is exactly the forbidden reassociation (the 16-lane shape reaches 91% of it); volume scans are memory-side (exact 768d scan sustains 41–42 GB/s inside the measured 43–58 GB/s single-core streaming band), so kernel-side wins cannot pass through at corpus scale; `portable_simd` is nightly-only and unsafe intrinsics are out of posture. The throughput lever that actually moves volume already ships: Binary quantization's packed-byte Hamming scan measures 50.8× full precision at 768d (`quantized_scan` bench; Scalar's reconstruct-then-distance pays 1.28×). Method + tables: docs/BENCHES.md Task 2. Closes AUDIT Open "SIMD distance kernels" |
| 2026-08-30 | Sketches trio shipped as engine surface: `CuckooFilter` (membership + `delete_bytes`, displacement-chain rollback on overflow so a rejected add never costs an admitted item — a deliberate divergence from the paper, which drops the last evicted fingerprint), `TDigest` (merging t-digest, k1 scale, NaN/±inf rejected, equal-mean centroids merged unconditionally as lossless), `MinHash`/`LshIndex` (k seeded bijective permutations of the shared DefaultHasher hash; band bucketing `1 − (1 − J^rows)^bands`); all built in-house under `src/sketch/` | The component-map "wrap existing crates" plan lost to the two standing contracts: the zero-dependency default build (hyperloglog-rs/probminhash drag transitives) and run-to-run determinism (fixed DefaultHasher/fixed mixers, no rand) — the same posture HLL/Bloom already follow; the rollback and equal-mean rules are the two places where the textbook algorithm would violate the no-false-negatives/lossless pins, so the algorithm bends to the pins, not vice versa. Closes the DESIGN future row (cuckoo/t-digest/MinHash+LSH) |
| 2026-08-30 | Sketch derive-symmetry: `TDigest`'s `Clone` derive stays (need-driven — the merge-commutativity/associativity conformance pins exercise `a.merge(&b)` vs `b.merge(&a)` on cloned operands); `BloomFilter`/`HyperLogLog`/`CuckooFilter` stay bare | Derives follow need, not symmetry: cloning is not part of any sketch's contract surface, so adding `Clone` everywhere would grow the API without a caller. A future need reopens the row; no symmetry churn until then (recorded review decision, ledger-closure Task 4) |
| 2026-08-30 | CJK segmentation = sliding BIGRAMS over CJK runs inside the shared tokenizer (no dictionary, no deps); boundary: hiragana/katakana U+3040–30FF ( prolonged mark ー included), Han U+3400–4DBF / U+4E00–9FFF / U+F900–FAFF / U+20000–323AF; hangul + halfwidth kana deliberately OUTSIDE; the Han↔kana script transition does not restart the window; stemming and case folding never apply to CJK; signature unchanged (`tokenize`/`analyze` still `Vec<String>`), so every serving path (scan, in-memory index, on-disk index, phrase positions) inherits bigrams for free | Bigram indexing is the standard embedded-engine fallback for the unspaced CJK scripts (dictionary segmentation drags lexicon data and licensing, against the zero-dependency posture); hangul stays out because Korean is space-separated, so whole-run tokens already segment it — bigramming would only halve term granularity; positions come out consecutive by construction, so `phrase_search` is order-correct over bigrams (`東京タワー` ≠ `タワー東京`, pinned); combining dakuten U+3099/309A are std-non-alphanumeric separators, so NFD text splits at them deterministically — no normalization is applied, NFC is the documented storage recommendation. Closes the DESIGN future row (CJK segmentation) |
| 2026-08-30 | Value compression shipped as the opt-in `zstd` cargo feature: user-collection documents at/above 1 KiB compress at zstd level 3 on write and decompress on every read path, stored as `0xFF ++ zstd frame` (0xFF outside the value codec's tag space `0..=8`, so each stored row is self-describing with no format-version bump); marked form stored only when strictly smaller (incompressible data never balloons) or when the raw value itself begins with 0xFF (forced — a raw stored row can never begin with the marker, keeping read-side disambiguation exact); engine-reserved `__` namespaces are never compressed; OFF builds store bytes byte-identical to the pre-feature engine (CI cargo-tree greps keep the zstd crate — C FFI via cc — out of the default and wasm graphs, exactly the tracing discipline) | Opt-in because the default build's zero-dependency posture and the WASM <2 MB budget are contracts, and zstd drags a C toolchain into every build that enables it — a deployment that wants compression trades compile-time FFI for ~12× smaller text documents (measured: structured-text map 8.3%, BENCHES.md); 1 KiB threshold because a frame's fixed overhead eats the win below it and small values are the common write; level 3 because it is zstd's own default (fast end of the general-purpose band — embedded writes favor latency over the last percent of ratio); the marker is per-VALUE rather than per-file so OFF-written rows read fine under ON (their encodings start with a tag) and `dump` stays format-stable v2 either way (dump reads through the store, i.e. decompressed — pinned); honest limitation recorded: f32 vector payloads barely compress (91.4% — IEEE mantissas are near-full entropy), so this is a text/document lever, not a vector one (quantization remains the vector footprint lever), and incompressible ≥1 KiB writes pay the must-compress-to-know attempt (~0.85 µs/KiB) while storing raw. Closes the DESIGN future row (compression) |
| 2026-08-30 | **Ledger-closure program exit (Task 6): every remaining DESIGN-future item carries an explicit controller decision.** The 18 rows below are those decisions, one line of rationale + trigger each; the body's "future" markers point here | After this row, nothing on any ledger sits unprioritized: each item is SHIPPED, deferred with a trigger, or DECLINED with the reason recorded where the next reader will look for it |
| 2026-08-30 | Browser/OPFS persistence (OPFS-SAHPool `StorageBackend` + wasm-bindgen + Worker RPC): KEEPS its 2026-05-29 deferral | Desktop/server focus stands; the engine is wasm-ready and size-validated (in-memory works on wasm today). Reopening needs product signal — someone actually building the browser runtime — not engineering appetite |
| 2026-09-02 | Browser/OPFS persistence: **the 2026-05-29 deferral is CLOSED — shipped.** The product signal the 2026-08-30 row required arrived (the corvid-js binding program building the browser runtime); the engine gained four additive seam APIs (`Store/Db::open_with_backend`, `Store/Db::backup_with_backend`, v0.3.4) and corvid-js shipped the rest: an `OpfsBackend` over one OPFS sync access handle per database (truncate-first growth — the SAHPool fallback was recorded by the program plan and never needed), a Worker-hosted engine with postMessage RPC, the async `openOpfs`/`AsyncDb` OOP mirror beside the untouched sync surface, single-writer `Busy` across tabs, dump/load byte-stream + physical backup forms, and Chromium CI running all 8 golden fixture files (267/267) plus reload/cross-tab contracts. The review-gated binding contract is corvid-js `docs/OPFS-SPEC.md` | The mechanism is exactly the 2026-05-29 insight ("redb does not require mmap" → implement `StorageBackend` over OPFS), so no architecture changed — only the trigger fired. Durability is bounded by the browser's `flush()`; crash-consistency stays redb's checksummed format. Triggers that remain recorded (not scheduled): SAHPool if a browser's truncate-growth proves unreliable; a COOP/CEP synchronous main-thread API if a workload demands it |
| 2026-08-30 | DiskANN (LM-DiskANN path for very large indexes): DEFER, trigger = on-disk HNSW measurably insufficient on a real workload | The quantized/PQ on-disk indexes currently serve the billions-of-vectors path (bounded memory, persists); swap the substrate only when a real workload shows the graph walk failing |
| 2026-08-30 | Filtered-HNSW pushdown (predicate pushed into the graph walk): DEFER, trigger = filtered ANN becomes hot | `.approx()` (over-fetch-then-filter) and the exact bounded scan both serve filtered vector search today and the true-predicate contract holds on every non-approx path; the pushdown buys latency, not correctness |
| 2026-08-30 | Materialized views: DEFER, trigger = API/product intent for them | In-process subscriptions already cover the reactive case (change feeds on the write path); views add a maintained-state contract nobody has asked for yet |
| 2026-08-30 | Embedding pipeline as a column type (declare model, auto-embed + auto-index): DEFER, trigger = a decided model-dependency policy | Which model, what footprint, what posture is undecided (and cuts against the zero-dependency default); users embed at the boundary today and store `$vector` — the engine stays model-free by design |
| 2026-08-30 | Streaming ranked cursors (incremental pull over arbitrary ranked sets): DEFER, trigger = a workload whose ranked sets are too large to materialize | Single-source ranked queries are already bounded (index fast paths + streaming top-k) and filter-only queries stream; only multi-source RRF fusion materializes, and no workload has outgrown it |
| 2026-08-30 | Cost-based planner (cost model over persisted statistics): DEFER, trigger = selectivity probing measurably insufficient at scale | The probe-every-serviceable-index-and-drive-on-the-smallest strategy needs no persisted statistics and serves the current scale; a cost model adds state to persist, invalidate, and explain |
| 2026-08-30 | Time-series patterns (append-only tables, delta encoding, Gorilla compression, sliding-window aggregates): DEFER, trigger = a real time-series workload | Per-record TTL shipped and covers the expiry-shaped needs; the rest is a storage layout nobody currently writes |
| 2026-08-30 | R-tree/H3 spatial backends: DEFER, trigger = a geo workload the fixed-resolution grid demonstrably fails | Grid cells (`create_geo_index`) serve the current workload sub-linearly with exact verification and cap-fallback; revisit only with hierarchy-shaped queries (large-scale containment, precision bands) |
| 2026-08-30 | Arrow `RecordBatch` result path: **DECLINE** | A heavy dependency against the embedded zero-dependency posture and the WASM budget (the 2026-05-29 "Arrow later" lean is closed as no); users convert to Arrow at the boundary where an analytical workload actually wants it |
| 2026-08-30 | JSON path language (`$.a[0].b`-style): **DECLINE** | Dotted paths (`field("a.b")`, `select`) cover the surface; a path language drags SQL-ish syntax and semantics in through a side door, against the builder-only posture |
| 2026-08-30 | User-defined functions: the deferral past v0.1 STANDS, trigger = a concrete caller need that expression nodes cannot express | Predicates are expression nodes (no closures in the public API) so plans stay identity-hashable and cacheable; an opt-in UDF mechanism reopens only against a real workload |
| 2026-08-30 | bumpalo query-scope arenas: **DECLINE** | Task 2's closure measurements show the hot paths are bandwidth-bound (kernels hold 62–83% of the read ceiling; volume scans sit at the streaming band) — arena allocation buys nothing the numbers support |
| 2026-08-30 | Owned buffer pool: DEFER with redb, trigger = redb's page cache measurably failing the layer | While redb is the substrate its page cache serves the layer; a corvid-owned pool is part of the owned-page-format seam that only a redb replacement opens |
| 2026-08-30 | Per-collection lock split (audit B9's global registry mutex): DEFER, trigger = build-vs-search contention observed on a real workload | A lazy build or compaction holding the registry mutex blocks searches today — documented as an availability characteristic; split the lock only when a real workload measures the contention |
| 2026-08-30 | Tensor type: the L4+ deferral STANDS; general tensor ops remain a non-goal | Bring candle/burn; a stored tensor type reopens only if a first-class workload wants tensor storage without engine-side ops |
| 2026-08-30 | Owned page format: **NEVER** for the redb engine — the L0 seam is documented, not scheduled | Checksums/commit/recovery are redb's while we wrap it; only a redb replacement opens this (the standing condition of the 2026-05-29 row and L0's future-engine paragraph) |
| 2026-08-30 | MCP framing stays hand-rolled stdio (no MCP SDK / rmcp dependency): DEFER, trigger = protocol features the hand-rolled transport cannot carry | The stdio framing is tested (the 78-test wire matrix) and working; adopting an SDK is a dependency against the sidecar's zero-friction build, unmotivated until the protocol outgrows JSON-RPC-over-stdio |
| 2026-08-30 | Adjacency version-skew rule (routed from the Task-1 review): bumping `ADJACENCY_VERSION` must either bump `FORMAT_VERSION` (older binaries refuse the file outright) or keep the adjacency rows decodable by older markers — a marker with an unrecognized value is stale-shaped and forces a derived-state rebuild, never a misparse | Adjacency is derived state: its upgrade story is rebuild-from-source-rows, not file migration; the rule pins that a layout change can never make an older binary silently misread newer rows as current |
| 2026-08-31 | The C ABI is TYPED C calls, not JSON/serialization: `crates/corvid-ffi` ships a 122-symbol `corvid` cdylib + generated `corvid.h` (docs/FFI.md, the LOCKED contract, written against the real engine sources — every function cites the Rust item it wraps); documents are built/read through `corvid_value` handles, results walk cursors, buffers are borrowed views — no parse step, no string-formatted queries, no byte-blob documents on the runtime path | The no-JSON ruling is the whole point of the surface: a serialization boundary would put a parser (plus its allocations, error taxonomy, and version drift) on every call and re-create the MCP sidecar's cost structure in every binding; typed calls measure at parity with native Rust instead — put/get/scan/hybrid-query through the ABI run 0.99–1.02× their native twins (BENCHES.md, corvid-ffi Task 8), so the crossing cost is bounded at noise and "zero parsing" is a measurement, not a claim. `corvid-mcp` keeps JSON only because JSON-RPC is the MCP spec |
| 2026-08-31 | FFI v1 carries a 10-item exclusion list, each with a falsifiable reopen trigger (docs/FFI.md §9): events/subscriptions, direct `vector_search`/`text_search`/`phrase_search` fns, the six sketch types, semantic cache, `PlanCache`/`explain`, `Db::bulk`, `page_where`, the `Store`-level byte API, non-UTF-8 paths | Same discipline as the ledger closure: absent-on-purpose must be written down with its trigger or it reads as an accident; the triggers are workload-shaped (e.g. direct search fns reopen only if the FFI bench shows builder overhead mattering — the Task 8 bench shows parity, so it stays closed) |
| 2026-08-31 | Binding posture: bindings expose idiomatic OOP (handles → native classes, cursors → native iteration, `CORVID_ERR` → native exceptions, destructors → the language's dispose pattern) and FFI symbols never leak into a binding's public API; v1 bindings are synchronous because the engine is synchronous | The ABI is the stability seam, not the user experience; a binding that leaks `corvid_*` names its users into C hygiene (manual free discipline, status codes) for no benefit. Sync-first because an async veneer over a sync engine would only relocate blocking, and async runtimes are per-language choices binding authors already own |
| 2026-08-31 | `FFI_VERSION = 1` stability policy (docs/FFI.md §8): enum values frozen and append-only (`corvid_status`/`corvid_err`/`corvid_cmp`/`corvid_metric`/`corvid_quant`/`corvid_value_type`/`corvid_field_type`; a snapshot test fails until a new engine `Error` variant is mapped); pre-1.0 breaks allowed but loud (bump `FFI_VERSION`, rename artifacts, CHANGELOG + this log); post-1.0 soname discipline (additive keeps `.1`, breaking bumps to `.2` with a migration note); `corvid.h` committed and drift-gated, spec↔header↔smoke coverage enforced by the radar (122/122), the C smoke suite, and CI (3-OS release-profile smoke + ASan/UBSan/LSan on Linux); the cdylib + header + golden fixtures ship as per-platform release artifacts with sha256 entries | Bindings pin exact engine tags, so a silent break is a coordinated-update failure across eight binding repos; the drift gates exist because byte-gating the header without compiling it missed that `corvid_value_type` was both a typedef and a function (found in Task 7 — the emitted C type is `corvid_value_type_t`) |
| 2026-09-01 | The C ABI's first ADDITIVE expansion inside `FFI_VERSION = 1` (docs/FFI.md's §4.4/§4.6 errata, engine v0.3.0): `corvid_value_map_keys` (a map's keys as the strs cursor, ascending key bytes — non-maps answer an empty cursor) and `corvid_phrase_search` (the direct positional text search over a coll, returning the rows cursor with BM25 phrase scores; `k == 0` is inert-empty per the engine) grow Appendix A 122 → 124 — no signature, enum, or behavior change, soname/`FFI_VERSION` stay at 1; §9's exclusion row for direct search fns is amended (the phrase half was never coverable by the bag-of-words `.text` source); the golden fixtures gain executable lines (values/mutations/queries — a fixture-set change bindings re-vendor at pin bump) | Both symbols were logged as gaps by real binding work, not invented: corvid-go's bootstrap needed key enumeration so badly it built a candidate-key oracle at every map decode, and every example suite hit phrase semantics the builder cannot express; the additions reuse the existing strs/rows cursors (no new handle family, no counter wiring — both are materialized cursors), so the cost is two functions and their spec text while the oracle work retires at the next bump wave |
| 2026-09-01 | `corvid.h`'s enum SYNTAX is portable C11/C++ (engine v0.3.1, docs/FFI.md §1.3/§1.4 erratum): the generator folds cbindgen's C23 fixed-underlying-type guards — emitted mechanically for every `#[repr(u32)]` enum, no cbindgen config suppresses them — into the plain `typedef enum <tag> { ... } <tag>;` the spec has always shown (src/header.rs `portable_enum_form`, a loud-failing post-pass inside the drift gate's render); the Rust-side `#[repr(u32)]` wire-type pin stays, values/signatures/124 symbols unchanged, and the header dual-compiles clean as C11/C17/C23 and C++98..26 | The guards' pre-C23 fallback (`typedef uint32_t <tag>;` beside the enum tag) is ill-formed C++ — tag and typedef share a namespace there — found by corvid-cpp, which had to preprocessor-mask the guards in its ABI TUs; nothing is lost by the plain form because the frozen contract is the explicit VALUES (0..=19, int-sized on gcc/clang/MSVC — verified by the 3-OS C smoke legs), not the C23 spelling, and the C-level enum type returns to being the spec's shape instead of `uint32_t` |
| 2026-09-02 | `corvid_page`'s zero-length cursor is the EXCLUSIVE continuation past the legal empty key, never a start (docs/FFI.md §4.9 erratum, found by the corvid-zig acceptance round): `after == NULL` is the ONLY start form; a non-NULL `after` of any length — including 0 — is the cursor, exactly the engine's `Option<&[u8]>` domain (`None` starts AT `b""`; `Some(b"")` resumes strictly after it — `page_inner`'s `after+0x00` cursor was always correct). The FFI's old fold (`after_len == 0` → start) and §4.9's old "`NULL || len 0` starts" wording are retired; §1.5/§7's "empty is a non-NULL pointer with length 0, distinct from NULL" governs | The fold made the ABI's own cursor inexpressible: `b""` sorts first, so a page boundary landing on it returns a zero-length `next_after`, which fed back under the fold re-walked from the top forever (and treating it as end silently truncates) — the exact trap the binding round feared; behavior is bit-identical for collections without the empty key, no signature/symbol/enum moved, so `FFI_VERSION` stays 1 and the fix rides the next tag. The empty-cursor edge is pinned by the in-crate FFI tests and the engine conformance suite, not a golden fixture line: the fixture grammar's splitter drops empty tokens, so neither the empty key nor its cursor can be written as a `PAGE`/`INSERT` line (documented in §4.9) |
| 2026-09-02 | The engine crate publishes as `corvid-db`: bare `corvid` is taken on crates.io by an unrelated crate, so the package name is the org-aligned `corvid-db`, while `[lib] name = "corvid"` keeps the compiled ident — every `use corvid::…` in the workspace, the bindings, and downstream code is unchanged; only consumers' dependency KEY reads `corvid-db` | The lib ident is API surface (renaming it would churn `use` paths across four in-repo crates and every external binding for zero user value), whereas the package name is only what crates.io and dependency keys see — so the name collision is absorbed exactly where it lives. `corvid-mcp`/`corvid-ffi`/`corvid-wasm` stay unpublished (`publish = false`) — binaries/cdylibs ship on engine releases with their own cadence. Verified by `cargo publish --dry-run`; the first real publish awaits a registry token |
| 2026-09-03 | Mobile platforms join the release artifact set (Android + Apple, additive): `corvid-ffi` gains `staticlib` in its crate-type (the `.a` slices Apple consumers link — bare dylibs outside frameworks are unsupported on iOS; `cdylib` unchanged for every desktop leg), and every engine release additionally ships two Android cdylib sets (`aarch64-linux-android` + `x86_64-linux-android`, NDK r28b at API 24, same tarball shape as the desktop sets) and the `corvid-swift-<tag>.zip` holding `CorvidEngine.xcframework` (iOS device + fat iOS-simulator + fat macOS staticlib slices, headers with an umbrella + module map so SwiftPM forms the `CorvidEngine` clang module). ABIs deliberately excluded: 32-bit Android (Play has required 64-bit since 2019) and watchOS/visionOS/tvOS slices (no consumer has asked; a slice is added the same way) | The bindings are the product signal, same as the OPFS row: corvid-swift (SPM, Apple) and corvid-android (the AAR riding corvid-jvm's repo and Central publishing — its PLAN's "first Android consumer" trigger fired) both consume engine-release artifacts exactly like the seven desktop bindings, so the platform story is one artifact pipeline, not per-binding build hacks. Everything was proven locally end-to-end before CI: the Android pair (engine `.so` + NDK-compiled JNI shim, `DT_NEEDED libcorvid.so` SONAME-resolved) and the xcframework (scratch SPM package, `swift test` calling the ABI through the binary target). One ABI portability fix rode along: the JNI shim requests JNI 1.6 on `__ANDROID__` (ART is a 1.6 VM; `GetEnv(1_8)` there returns `JNI_EVERSION`) |
