# corvid

An embedded, multi-modal data store for AI applications, with a fluent builder
API instead of SQL. One in-process dependency that does vector search,
full-text search, metadata filtering, and rank fusion — composed into a single
call.

> **What this is.** A personal experiment, and an honest one: corvid was
> *entirely vibe coded* — built by directing an AI coding agent, not hand-written
> line by line. It is not a product, has no roadmap, and comes with no support
> promises.
>
> **What it isn't.** A toy. The code is solid and genuinely usable: 1,000+
> tests across four feature configurations, >96% line coverage,
> zero-warning clippy, criterion benchmarks on the hot paths (recorded
> with before/after history in [docs/BENCHES.md](docs/BENCHES.md)), and a
> correctness-first design (filters are true predicates, indexes are never
> stale, writes are transactional). If a corner is rough, it's a missing
> feature, not a broken one.
>
> Status: **v0.1**, pre-1.0. The API changes freely until 1.0 — no
> backward-compatibility guarantees yet; a format change is migrated with
> `dump`/`load`, never silently. Built for the author's own use first; shared in
> the open under MIT. Use it, fork it, learn from it.

## Why

AI apps usually glue together a vector database, a full-text engine, and a
metadata store, then reconcile them in application code. corvid puts them
behind one embedded engine and one query builder, so a hybrid query is one
chained call rather than three round trips and a reranker:

```rust
use corvid::{Db, Metric, Value, field};

let db = Db::open("memory.corvid")?;
let docs = db.collection("docs");

// Store a document (any JSON-like value; embeddings are first-class).
let mut doc = std::collections::BTreeMap::new();
doc.insert("category".into(), Value::Text("blog".into()));
doc.insert("body".into(), Value::Text("rust embedded database design".into()));
doc.insert("embedding".into(), Value::Vector(vec![0.1, 0.9, 0.2]));
docs.insert(b"post-1", &Value::Map(doc))?;

// Hybrid query: filter + vector + text, fused and reranked, in one call.
let rows = docs
    .query()
    .filter(field("category").eq(Value::Text("blog".into())))
    .vector("embedding", vec![0.1, 0.9, 0.2], 100, Metric::Cosine)
    .text("body", "rust embedded database", 100)
    .rerank_mmr(0.7)
    .limit(10)
    .run()?;
# Ok::<(), corvid::Error>(())
```

The filter runs *before* ranking, so it is a true predicate — the top-k is
computed among matching documents, never a post-hoc trim. Opting into
[`.approx()`](https://i-rocky.github.io/corvid/api/corvid/struct.QueryBuilder.html#method.approx)
trades this: the index's top-k is fetched first and the filter applied after,
so a highly selective filter may return fewer than `limit` rows (see the
`.approx()` docs).

## What's here

- **`corvid`** — the embedded engine (this is a library; strictly in-process,
  no networking).
- **`corvid-mcp`** — a sidecar that exposes a corvid store to agentic coding
  tools over MCP (JSON-RPC on stdio). Run `corvid-mcp [PATH]` and point an MCP
  client at it; tools: `store`, `patch`, `compare_and_set`, `get`, `delete`,
  `delete_where`, `page`, `search`, `phrase_search`, `count`, `geo`, `join`,
  `link`, `unlink`, `neighbors`, `in_neighbors`, `traverse`, `create_index`,
  `create_text_index`, `create_scalar_index`, `create_compound_index`,
  `create_geo_index`, `backup`, `dump`, `load`, `list_collections`,
  `insert_auto`, `set_schema`, `get_schema`. `set_schema` declares (or
  replaces) a collection's schema — a fields array of `{name, type,
  required?, unique?}` — that subsequent stores are validated against;
  `get_schema` returns the declared fields, or `{fields: null}` when none is
  declared.

A task-oriented walkthrough of every feature is in the **[user guide](docs/GUIDE.md)**.
The **[website](https://i-rocky.github.io/corvid/)** hosts an overview and the
full **[API reference](https://i-rocky.github.io/corvid/api/corvid/)**.

## Capabilities (v0.1)

| Area | Status |
|---|---|
| Transactional KV storage (redb), atomic multi-op transactions | ✅ |
| Typed values + documents (incl. embeddings) | ✅ |
| Vector search (cosine / dot / L2) | ✅ exact baseline |
| Full-text search (BM25) | ✅ exact baseline |
| Filter predicates (`field().gt()`, and/or/not, dotted paths) | ✅ |
| Rank fusion (RRF) and MMR diversification | ✅ |
| Fluent multi-modal query builder + projection + aggregation | ✅ |
| Aggregations (sum/avg/min/max/distinct, grouped) | ✅ |
| Predicates: in / between / starts_with / contains (+ indexed) | ✅ |
| Nested/dotted-path field indexing | ✅ |
| patch / update / compare-and-set; delete-by-query | ✅ |
| Phrase / positional text search | ✅ |
| CJK text tokenization (Han + kana runs → sliding bigrams, dictionary-free; phrase-order-correct) | ✅ |
| k-nearest geo (`geo_nearest`) | ✅ |
| Keyset (cursor) pagination (`page`) | ✅ |
| Compound (multi-field) scalar index | ✅ |
| Logical dump/load migration (`Db::dump`/`load`, format v2; `load_with_renames` maps old `__`-containing names on load) | ✅ |
| HNSW approximate index (`create_vector_index`) | ✅ in-memory, derived |
| On-disk HNSW (`create_vector_index_ondisk`) | ✅ bounded memory, persists |
| Vector quantization (binary ≈32×; scalar ≈4×, asymptotic — less at low dim where the 8-byte header dominates; product quantization ≈16× via `create_vector_index_pq` in-memory and `create_vector_index_ondisk_pq` on disk) | ✅ in-memory **and** on-disk |
| On-disk inverted text index (`create_text_index_ondisk`) | ✅ bounded memory, persists |
| Scalar index (`create_scalar_index`): sub-linear eq/range filters | ✅ on disk, persists |
| Directed property graph (`link`/`neighbors`/`traverse`) | ✅ |
| Geospatial: radius / bounding-box / `within_km` filter | ✅ |
| Spatial index (`create_geo_index`): sub-linear radius/bbox | ✅ on disk, persists |
| Cross-collection lookup joins | ✅ |
| Semantic (vector-keyed) cache | ✅ |
| Probabilistic sketches (HyperLogLog, Bloom, cuckoo, t-digest, MinHash + LSH) | ✅ |
| Reactive change feeds | ✅ |
| Online consistent backup (`Db::backup`) | ✅ |
| Optional declared schema (`set_schema`): types/required/unique | ✅ |
| Per-record TTL / expiry (`insert_with_ttl`, `purge_expired`) | ✅ injected clock |
| Optional zstd compression of stored documents (`zstd` cargo feature, OFF by default; ~12× smaller text docs, transparent reads, default build unchanged) | ✅ opt-in feature |
| Optional structured instrumentation (`tracing` cargo feature, OFF by default) | ✅ opt-in feature |
| MCP sidecar over stdio | ✅ |
| WASM build (engine, ≈0.2 MB gzipped; CI prints the measured size and enforces a 2 MB budget) | ✅ in-memory; OPFS persistence ⏳ |
| Mobile cross-compile (aarch64 iOS/Android) | ✅ engine builds |

Image search is vector search over image embeddings: embed in your app (CLIP
etc.), store the `$vector`, query — same engine as text vectors. corvid does
not run the embedding model itself (by design).

Vector and text search are **exact** (brute-force over a scan) by default — the
correctness baseline. Calling `create_vector_index` registers an HNSW index
that `vector_search` then uses transparently (approximate, faster); the index
is derived from the documents and rebuilt automatically after writes, so it is
never stale at query time.

For scale beyond what fits in RAM, the index can live on disk: an insert or
search touches only the nodes/postings it needs, so memory is bounded by the
operation, not the collection, and the index persists across reopen with no
rebuild. `create_vector_index_ondisk` (graph nodes), `create_text_index_ondisk`
(BM25 postings), and `create_scalar_index` (order-preserving keys for sub-linear
equality/range filters and counts) all store their state as ordinary records.
The scalar index returns a verified candidate superset and falls back to a
bounded scan when a filter isn't selective, so it never trades memory or
correctness for speed.

## Design

See [DESIGN.md](DESIGN.md) for the architecture, the cross-modal consistency
invariant, the layer map, and the decision log. Working rules are in
[CLAUDE.md](CLAUDE.md).

Non-goals (permanent): SQL, networking/replication in the engine, distributed
transactions, a hosted service.

## Building

```sh
cargo test            # all tests
cargo run -p corvid-mcp   # start the MCP sidecar (in-memory)
```

Requires a recent stable Rust (2024 edition). Two optional cargo features,
both OFF by default so the zero-dependency default build and the WASM size
budget stay contracts: `tracing` (structured instrumentation events) and
`zstd` (transparent compression of stored documents ≥1 KiB — enable with
`corvid = { features = ["zstd"] }`; databases stay readable, and `dump`/
`load` moves data between feature configurations).

## License

MIT.
