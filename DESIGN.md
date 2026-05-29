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
- Backward-compatible file format migrations (manual reimport, full stop)
- "World's best" on any single component — match within 2–5×, never claim leadership

## Targets

| Target | Memory | Storage | Concurrency |
|---|---|---|---|
| Desktop / server | abundant | pread/pwrite (redb `StorageBackend`) | single writer, multi-reader MVCC |
| Mobile / edge | tight | pread/pwrite + fsync | single writer, multi-reader MVCC |
| WASM / browser | tight | OPFS-SAHPool in Worker | single connection (OPFS constraint) |

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

Bottom-up. Each layer depends only on those below. Layers above v0.1 are scaffolded as empty traits, not implemented.

### L0 — storage backend

**v0.1 (wrapping redb):** this layer is just three impls of redb's `StorageBackend` trait — `pread/pwrite` (desktop/mobile), and `OPFS-SAHPool` (WASM, runs in a Worker). redb does **not** require mmap, which is what makes the OPFS path tractable. Checksums, group commit, page format, and crash recovery are **redb's concern**, not ours, as long as we wrap it.

**Future (from-scratch engine, post-v1):** if we ever replace redb, *this* layer grows to own the page format — big pages (16K–64K), prefix-compressed keys, whole-block CRC32C (hardware CRC), group commit, fsync coalescing. Listed here so the seam is documented, not because v0.1 builds it.

### L1 — Storage substrate

- Wrap **redb** as primary KV store
- WAL + MVCC reads via redb's existing model
- Compression: zstd for values above a threshold (applied by us, above redb)
- Single writer (whole database), MVCC readers — redb's model, unchanged

### L2 — Schema and document layer

- Strict typed schemas (no dynamic typing)
- Primitive types: bool, i32, i64, f32, f64, bytes, string, timestamp, decimal
- Container types: array, map, struct, JSON (typed-on-write where possible)
- Embedding type: fixed-dimension f32/f16/u8/binary vector with declared metric
- Tensor type: deferred (consider in L4+)

### L3 — Indexes

All indexes update transactionally with row writes. Same WAL.

- **Primary**: B+-tree on primary key (from redb)
- **Secondary scalar**: B+-tree, supports range, prefix, equality
- **Vector**: HNSW algorithm (reference: [hnswlib-rs](https://github.com/jean-pierreBoth/hnswlib-rs)), graph state stored as redb entries — not the library's own persistence (invariant). Quantization: binary, scalar, eventually PQ. Path to LM-DiskANN later when working set exceeds RAM.
- **Full-text**: BM25 over posting lists stored as redb entries. Stripped tokenizer (Unicode + ASCII fold + light stemming). Borrow tokenizer/Levenshtein-automaton ideas from [tantivy](https://github.com/quickwit-oss/tantivy); reference [bm25 crate](https://docs.rs/bm25/latest/bm25/). Do not adopt tantivy's segment storage.
- **Graph adjacency**: deferred to L5
- **Spatial (R-tree)**: deferred to L5
- **JSON path**: deferred to L5

### L4 — Query algebra (the most important layer)

This is what survives across rewrites of everything below. Design carefully.

- Relational core: scan, filter, project, join, group, sort, limit
- Vector: `vector_search(column, query, k, metric)` returning candidates with distance
- Text: `text_search(column, query, k)` returning candidates with BM25 score
- Hybrid: `rrf([vector_search, text_search])` and `mmr(candidates, lambda)` as first-class operators
- Composition: every operator takes and returns a plan node. Filters push down into index scans.

### L5 — Extended capabilities (post-v0.1)

- Graph: native adjacency storage, BFS/DFS, multi-hop traversal with vector-similarity-as-edge-predicate
- Spatial: R-tree (via [rstar](https://crates.io/crates/rstar)) and H3 (via [h3o](https://crates.io/crates/h3o))
- Time-series patterns: append-only tables, delta encoding, Gorilla compression, sliding window aggregations
- Probabilistic sketches: bloom, cuckoo, HLL, t-digest, MinHash + LSH
- Reactive: subscriptions on tables and queries via WAL change capture
- Semantic cache: vector-keyed cache layer for LLM responses
- Embedding pipeline as column type: declare model, auto-embed + auto-index on insert
- Approximate top-K with metadata filter pushdown

### L6 — API surface

- Fluent builder, sync API surface (async wrapper available)
- Zero-copy result paths where possible (return Arrow `RecordBatch` for analytical paths, native Rust types for transactional)
- One canonical host: Rust. Other-language bindings considered later, never first-class.

---

## Component map: wrap vs build vs skip

| Component | Decision | Notes |
|---|---|---|
| Storage substrate | **Wrap** redb | Replaceable. Maybe fjall later when LSM patterns matter (append-heavy logs). |
| WAL / MVCC | **Inherit** from redb | Don't reinvent. |
| Compression | **Wrap** zstd via FFI | Physics, not legacy. |
| JSON parsing | **Build thin** pure-Rust behind a trait (v0.1) | Replaceable component, not the identity. Seam to FFI C++ simdjson on native later if measured hot. |
| Vector index | **Algorithm from hnswlib-rs, state in redb** | Not wrapped as storage (breaks the invariant). v0.1 may scaffold against hnswlib-rs's own persistence behind the (b) contract; HNSW state moves into redb entries as the real impl. |
| Vector quantization | **Build** (light) | Binary + scalar + (later) PQ. Algorithm-clear, integration-heavy. |
| SIMD distance kernels | **Build** | `std::simd` for portability, intrinsic-specialized paths behind cfg. |
| FTS | **Algorithm (BM25 + posting lists), state in redb** | Posting lists as redb entries, not tantivy's segment files — same invariant reason as vector. Borrow tantivy's tokenizer/automaton ideas; don't adopt its storage. bm25 crate as a reference. |
| Tokenizer | **Build** (Unicode + light stemming) | Stripped, English-first. |
| Builder API | **Build** | Project identity. Every line goes through our hands. |
| Query planner / executor | **Build** | Same. |
| AI operators (MMR, RRF, semantic cache) | **Build** | Field is young; integration is the value. |
| Graph adjacency | **Build** (post-v0.1) | Custom storage, fits cross-modal model. |
| Spatial | **Wrap** rstar + h3o (post-v0.1) | Well-solved by these crates. |
| Probabilistic sketches | **Wrap** existing crates | hyperloglog-rs, probminhash, etc. |
| Reactive | **Build** (post-v0.1) | WAL change capture, in-process subscribers. |
| Tensor ops | **Skip** | Out of scope. Bring your own (candle/burn). |
| SQL | **Skip** | Permanently. |
| Replication / network | **Skip** | Permanently. |

---

## v0.1 cut

Smallest coherent thing that's usable for my own AI work. Target: usable within months, not years.

### In v0.1

- L0 VFS with all three backends
- L1 storage (redb wrap)
- L2 schema with primitives + array + struct + JSON + embedding type
- L3 indexes: primary B+-tree, secondary scalar B+-tree, HNSW vector with binary quantization, BM25 FTS
- L4 algebra: scan, filter, project, join (hash + nested loop), group, sort, limit, vector_search, text_search, rrf, mmr
- L5: none
- L6: sync fluent builder API, native Rust result types
- WASM: Worker build with OPFS-SAHPool

### Out of v0.1 (back-burner, in roughly this priority order)

1. Graph adjacency + multi-hop traversal
2. Reactive: in-process subscriptions and materialized views
3. Semantic cache as a primitive
4. Embedding-as-column-type with automatic pipeline
5. Spatial (R-tree + H3)
6. Probabilistic sketches as first-class index types
7. Time-series compression
8. PQ vector quantization, DiskANN path for large indexes
9. Approximate top-K with metadata pushdown
10. Arrow `RecordBatch` zero-copy result path for analytical queries

---

## Implementation status (living)

As of the first build pass. All code is tested (≥90% line coverage, mostly ~99%), fmt + clippy clean under `-D warnings`.

**Built and working:**
- **L1 storage** (`store.rs`): collections over one redb table (BE id-prefixed keys), `put/get/delete/scan`, atomic multi-op `transaction(|tx| …)` and snapshot `read(|r| …)`, file-backed and in-memory.
- **L2 values** (`value.rs`): `Value` (null/bool/int/float/text/bytes/array/map/vector) with a deterministic tag/length codec; field accessors.
- **Document layer** (`db.rs`): `Db` + `Collection` handle over typed `Value` documents.
- **Vector search** (`distance.rs`, `query.rs`): cosine/dot/L2 kernels; exact KNN baseline + HNSW index.
- **HNSW index** (`hnsw.rs`, `index.rs`): **incremental and persistent-definition**. `create_vector_index` registers a derived index maintained with O(log n) inserts/tombstones per write (no per-write rebuild); definitions reload on open; `vector_search` and the builder use it (the latter via `.approx()` for filtered queries). Documents are the source of truth.
- **Full-text** (`text.rs`, `fts.rs`): tokenizer + BM25 exact baseline **plus an incremental inverted index** (`create_text_index`) so queries touch only query-term postings.
- **Fusion** (`fusion.rs`): Reciprocal Rank Fusion + MMR.
- **Filters** (`filter.rs`): `field("a.b").gt(…)` predicate tree, and/or/not, dotted paths, `within_km` geo.
- **L4 fluent builder** (`builder.rs`): filter → vector → text → RRF → MMR → `order_by`/`offset` → `select` (nested paths) → `limit`; `count`/`group_count`; `.approx()`; `.explain()`.
- **Graph** (`graph.rs`): `link`/`link_weighted`/`unlink`/`neighbors`/`neighbors_weighted`/`in_neighbors`/`traverse`; edges atomic (forward+reverse in one txn).
- **Geo** (`geo.rs`): haversine radius / bbox, composable as a builder filter.
- **Join, semantic cache, reactive feeds, sketches, document layer** as before; auto-keys (`insert_auto`), `collections()` listing, on-disk format version.
- **Sidecar** (`corvid-mcp`): MCP server over stdio exposing store/get/delete/search/create_index/create_text_index/link/unlink/neighbors/in_neighbors/traverse/geo/join/list_collections/count/insert_auto, with default result caps.

**Audit gaps resolved** (see the gap sweep): incremental+persistent indexes (#4/#7), inverted FTS (#3), filtered-ANN in builder (#5), order_by/offset (#10), reverse edges (#11), format version (#12), auto-keys (#8), MCP surface+caps (#13/#27), reserved-name & length & slicing hardening (#17/#18/#20), atomic edges + single-snapshot join (#2/#14), nested select + explain (#22/#26 partial), edge weights (#25), CI/LICENSE/CHANGELOG/MSRV + property/concurrency tests + benchmarks (#28–32).

**On-disk indexes (DONE).** All three store state as redb entries, bound memory to the operation, and persist with no rebuild on open:
- `create_vector_index_ondisk` — HNSW graph nodes on disk; insert/search touch only nodes-per-op; bulk backfill batches commits with a shared node cache. Now also supports **quantized on-disk storage** (`create_vector_index_ondisk_quantized`, binary ≈32× / scalar ≈4× smaller on disk + page cache) via the shared `quant` module — the footprint path for billions of vectors on a laptop. Recall holds (scalar ≥0.80 vs exact).
- `create_text_index_ondisk` — BM25 postings on disk; a query touches only its query terms.
- `create_scalar_index` — order-preserving keys; selective equality/range filters and counts go sub-linear (1M: 662ms scan → ~3ms eq / ~0.5ms 100-row range), with a candidate cap that falls back to the bounded streaming scan when a filter isn't selective.
- `create_geo_index` — fixed-resolution grid cells (~0.1°) as order-preserving keys; radius/bbox queries (and the builder's `within_km` filter) scan only the cells the bounding box overlaps, then verify exact haversine. Cap-fallback to a scan for continental-scale or antimeridian-wrapping queries.

**WASM / mobile (engine ready, measured).** The engine compiles to `wasm32-unknown-unknown`; a `cdylib` harness (`corvid-wasm`) links it and the bundle is **≈0.2 MB gzipped** — well under the 2 MB budget — enforced in CI. The engine also cross-compiles for `aarch64` iOS/Android (CI builds android). The remaining browser-runtime piece is the OPFS-SAHPool `StorageBackend` + `wasm-bindgen` surface (Worker-only, needs a browser to validate); in-memory works on wasm today.

**Product Quantization (DONE).** `create_vector_index_ondisk_pq(field, metric, m, k)` trains a deterministic per-subspace codebook (k-means) from a bounded sample of existing vectors, stores each vector as `m` code bytes, and persists the codebook in the index namespace (reloaded on open). Distance is by reconstruction (decode + metric, metric-agnostic) with an L2 asymmetric-distance table available. The `pq.rs` core is standalone and validated (recall ≥0.6 at 8× compression).

**Deliberately deferred (large subsystems, with reasons):**
- **In-memory PQ + ADC in the HNSW hot path**: PQ is wired into the on-disk index (the footprint-critical path); using it for the in-memory index and switching distance to the table-based ADC are follow-ons on the same `pq.rs` core.
- **Browser support (OPFS StorageBackend + wasm-bindgen)** — *deferred by decision (2026-05-29)*. The engine is wasm-ready and size-validated (≈0.2 MB gzipped, CI-enforced) and runs in-memory on wasm; the browser-persistence layer (OPFS-SAHPool VFS, Worker RPC, JS bindings) is explicitly out of scope for now. Desktop + server is the focus.
- **Cursor/iterator result API** (#15 remainder): single-source ranked queries are now bounded (index fast paths + streaming top-k), and filter-only queries already stream; a public streaming *cursor* over arbitrary result sets (incremental pull) is still a larger API addition. Multi-source RRF fusion still materializes the candidate set.
- **Phrase / positional text queries** (#23 remainder): the analyzer now does stop-word removal + S-stemming (shared by index and query); phrase/positional matching needs positions in the postings and is a larger format change. CJK segmentation also future.
- **Cost-based planning**: `.plan()` gives an identity-hashable [`QueryPlan`] and `PlanCache` keys prepared work by shape; choosing *between* viable index paths by estimated cost (statistics-driven) is future.

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

**The honest walls at 1M–50M:**
- **In-memory index build/rebuild.** HNSW and the inverted index live in RAM and
  build lazily (and rebuild on open). At 1M the HNSW build is minutes; at 50M
  neither fits in RAM. This is the **on-disk index** deferral (#16) — the real
  frontier for large-scale vector/text *search*.
- **No secondary scalar index.** `filter`/`order_by` are O(n) scans (constant
  memory now, but linear time). A scalar B-tree index (#6) is the optimization.
- **Exact (unindexed) search** is O(n) time — correct and OOM-free (streamed),
  but you want an index past ~100k.

So: everything except large-scale *index build/search* scales to 50M with
bounded memory today; large-scale search needs the deferred on-disk indexes.

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
- No closures in the public API for predicates. Filters are expression nodes (`col(...).eq(...)`). User-defined functions are a separate, opt-in mechanism (deferred past v0.1).
- Sync by default. Async wrapper is shallow.

### What this rejects

- Diesel-style typestate that encodes every column in the type signature. Collapses on multi-modal joins. Use runtime errors for shape mismatches; rely on tests.
- SQL string interpolation, even hidden. The builder is the only entrypoint.

---

## WASM specifics

- **Build artifact:** one `.wasm` module + JS wrapper. Loaded inside a dedicated Worker.
- **VFS:** OPFS-SAHPool. Accept single-connection-per-tab constraint.
- **Threading:** main thread is RPC. All DB work happens in the Worker. No `SharedArrayBuffer` requirement for v0.1.
- **Bundle size goal:** under 2MB gzipped for v0.1. Tantivy may push past this; if so, ship lighter BM25.
- **Storage layout:** single OPFS file via SAHPool. Easier crash recovery than directory-of-files.

## Mobile specifics

- **VFS:** pread/pwrite with WAL and fsync.
- **No mmap by default.** Jetsam kills + fd lifecycle make mmap fraught.
- **Durability default:** WAL + fsync on commit. Crash safety over throughput.
- **Bundle size goal:** under 5MB for a release-stripped binary, ideally under 3MB.

---

## Cross-cutting

### SIMD

- `std::simd` (portable) by default
- x86_64 AVX2 + AVX-512 specializations behind `cfg(target_feature)`
- aarch64 NEON specializations behind cfg
- Coverage: vector distance, BM25 scoring, posting list decode, JSON parsing (pure-Rust SIMD), hash table probing, filter evaluation

### Memory

- Query-scope arenas (`bumpalo`) on hot paths. Zero allocations during query execution where possible.
- One global buffer pool, sized per VFS backend.
- Cache-line-aligned hot structures.

### Async vs sync

- Public API is sync. Async wrapper is shallow (offload to a thread pool).
- I/O is sync underneath. io_uring is interesting but not in v0.1 — adds platform complexity for unclear win on embedded workloads.

### Zero-copy

- Native result paths return owned Rust types in v0.1.
- Arrow `RecordBatch` path is post-v0.1 for analytical queries.
- Bytes from disk → query result without intermediate decoding where the data type permits.

### Error model

- Typed errors per layer. `thiserror` for definitions.
- No panics on user input ever. Panics on internal invariants (assertion failures) are acceptable in debug, never in release without a clear contract violation.

### Observability

- Built-in query trace (`.explain()` and `.profile()`).
- Structured logging via `tracing`.
- Counters for cache hits, index probes, plan cache hits.
- No metrics export to external systems in v0.1.

### Documentation discipline

- Every non-obvious decision gets a paragraph in `decisions/`.
- File format changes get a separate migration note.
- Public API examples are runnable doctests.

---

## Open questions

These need answers before specific layers can be implemented. Listed in rough order of when they block work.

1. **Tantivy WASM bundle size.** Measured, not estimated. If unacceptable, ship bm25 crate over our own posting lists. Blocks: L3 FTS implementation.
2. **redb's behavior on OPFS-SAHPool.** Does it work out of the box? CoW B+-tree should be fine with single fd + sync I/O in Worker, but verify with a spike. Blocks: WASM build.
3. **Cross-modal commit semantics — decided** (see *Critical tension*): destination (b) state-in-redb, contract (b) from day one, v0.1 may scaffold (a) behind it. Remaining concrete work: define the redb layout for HNSW nodes and posting lists, and the commit ordering within a single redb write txn.
7. **JSON parsing — decided.** One pure-Rust path behind a `JsonParser` trait for all targets in v0.1 (portable SIMD128 on WASM for free, no C++ FFI, smallest bundle). Clean seam to drop in C++ simdjson via FFI on **native only** *if and when* profiling shows JSON parsing is a real hot path. No premature optimization of a replaceable component. (General note: "SIMD everywhere" degrades on WASM — no AVX-512/NEON, only SIMD128.)
8. **Filtered vector search — decided (semantics), open (implementation).** `.filter()` on `.vector_search()` is a **true predicate**: the result is the top-k rows satisfying the filter, never top-k-then-drop. Default is correct: push the predicate into the graph walk (filtered-ANN), and when selectivity makes ANN recall unreliable, fall back to exhaustive scan over matching rows to keep the guarantee. Explicit `.approx()` opts out for speed. Open part is purely the implementation (filtered-HNSW strategy + the selectivity threshold for the exhaustive fallback), not the contract.
4. **Plan cache identity hashing.** AST nodes need stable `Hash` impls that ignore allocator addresses. Closures cannot appear in the AST (already decided); but constants embed user data — design the hash to treat structural identity, not value identity, where appropriate.
5. **Schema migration story.** "No backward compat" doesn't mean "no migration." Need a one-shot dump/load utility from format N to format N+1. Design it before declaring v1.0.
6. **The first real app.** Which one? It shapes everything. Likely candidates: a personal RAG / second-brain system; a code-aware AI assistant memory store; an agent's working memory layer. Pick one, build it in parallel with v0.1.

---

## Decision log (start)

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
