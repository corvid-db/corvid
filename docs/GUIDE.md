# corvid user guide

A task-oriented tour of the engine. Every example is plain `corvid` API; the
crate is synchronous and in-process. For the *why* behind the design, see
[DESIGN.md](../DESIGN.md); for the capability matrix, the
[README](../README.md).

- [Install](#install)
- [Core concepts](#core-concepts)
- [Documents: write & read](#documents-write--read)
- [Values](#values)
- [Filters](#filters)
- [The query builder](#the-query-builder)
- [Aggregations](#aggregations)
- [Pagination](#pagination)
- [Vector search](#vector-search)
- [Text search](#text-search)
- [Indexes](#indexes)
- [Geospatial](#geospatial)
- [Graph](#graph)
- [Joins](#joins)
- [Schema & constraints](#schema--constraints)
- [TTL / expiry](#ttl--expiry)
- [Reactive change feeds](#reactive-change-feeds)
- [Semantic cache](#semantic-cache)
- [Probabilistic sketches](#probabilistic-sketches)
- [Operations: bulk, backup, compaction, migration](#operations)
- [The MCP sidecar](#the-mcp-sidecar)
- [Conventions & errors](#conventions--errors)

---

## Install

```toml
[dependencies]
corvid = { git = "https://github.com/rocky/corvid" }
```

Requires stable Rust (2024 edition, MSRV 1.88). The engine has no required
features to enable; it is `#![forbid(unsafe_code)]` and pulls in only `redb`.

## Core concepts

- A **`Db`** is one embedded database file (or in-memory). Open it once and
  share it (it is `Send + Sync`; wrap in `Arc` for threads).
- A **collection** is a named namespace of documents. Created lazily on first
  write. Names starting with `__` are reserved for the engine.
- A **key** is arbitrary bytes (`&[u8]`); documents sort by key.
- A **document** is a [`Value`](#values) — usually a `Value::Map`.
- **Indexes are derived** from documents (the source of truth), so a query
  never sees a stale index.

```rust
use corvid::Db;

let db = Db::open("app.corvid")?;          // file-backed
// let db = Db::open_in_memory()?;          // ephemeral
let docs = db.collection("docs");
# Ok::<(), corvid::Error>(())
```

## Documents: write & read

```rust
use corvid::{Db, Value};
use std::collections::BTreeMap;

let db = Db::open_in_memory()?;
let c = db.collection("users");

let mut u = BTreeMap::new();
u.insert("name".into(), Value::Text("ada".into()));
u.insert("age".into(), Value::Int(36));
c.insert(b"u1", &Value::Map(u))?;          // insert or overwrite

let got = c.get(b"u1")?;                    // Option<Value>
let n = c.len()?;                           // O(1) maintained count
c.delete(b"u1")?;                           // returns whether it existed
# Ok::<(), corvid::Error>(())
```

More write modes:

| Method | Use |
|---|---|
| `insert(key, &doc)` | insert / full overwrite |
| `insert_batch(&[(&[u8], &Value)])` | many docs in one transaction (one fsync) |
| `insert_auto(&doc) -> Vec<u8>` | append under a generated ordered key |
| `patch(key, &partial_map)` | merge top-level fields into an existing doc |
| `update(key, |cur| -> Option<Value>)` | read-modify-write (return `None` to delete) |
| `compare_and_set(key, expected, new)` | atomic conditional write / delete / insert-if-absent |
| `delete_where(predicate)` | delete every matching doc (index-accelerated) |
| `delete_batch(&[&[u8]])` | delete a set of keys |

```rust
# use corvid::{Db, Value, field}; use std::collections::BTreeMap;
# let db = Db::open_in_memory()?; let c = db.collection("users");
# let mut m = BTreeMap::new(); m.insert("age".into(), Value::Int(36)); c.insert(b"u1", &Value::Map(m))?;
// Patch: set/add fields without resending the whole document.
let mut p = BTreeMap::new();
p.insert("age".into(), Value::Int(37));
c.patch(b"u1", &Value::Map(p))?;

// Conditional write: only if absent.
let mut v = BTreeMap::new();
v.insert("name".into(), Value::Text("grace".into()));
let applied = c.compare_and_set(b"u2", None, Some(Value::Map(v)))?; // true
# let _ = applied;
# Ok::<(), corvid::Error>(())
```

## Values

`Value` is the document/field type:

```text
Null · Bool(bool) · Int(i64) · Float(f64) · Text(String)
Bytes(Vec<u8>) · Array(Vec<Value>) · Map(BTreeMap<String, Value>)
Vector(Vec<f32>)        // a dense embedding — first-class
```

Field access is by dotted path through nested maps: `doc.get_path("meta.author")`.
The same dotted paths work in filters *and* in index definitions.

## Filters

Build predicates with `field(path)`:

```rust
use corvid::{field, Value};

field("category").eq(Value::Text("blog".into()));
field("score").gt(Value::Int(5));
field("score").between(Value::Int(1), Value::Int(10));   // inclusive
field("tag").is_in([Value::Text("a".into()), Value::Text("b".into())]);
field("title").starts_with("intro");
field("body").contains("rust");
field("loc").within_km(51.5, -0.13, 25.0);               // geo
field("email").exists();

// Combine:
let p = field("category").eq(Value::Text("blog".into()))
    .and(field("score").ge(Value::Int(3)))
    .or(field("pinned").eq(Value::Bool(true)));
let p = !field("draft").eq(Value::Bool(true));            // negation
# let _ = p;
```

Comparisons on a missing path are `false`; ordered comparisons across
non-comparable types are `false`. Use `.exists()` for presence.

## The query builder

One chained call composes filtering, vector search, text search, fusion, and
reranking. Filtering happens **before** ranking, so the top-k is computed among
matching documents.

```rust
# use corvid::{Db, Metric, Value, field};
# let db = Db::open_in_memory()?; let docs = db.collection("docs");
let rows = docs.query()
    .filter(field("category").eq(Value::Text("blog".into())))
    .vector("embedding", vec![0.1, 0.9], 100, Metric::Cosine)  // a retrieval source
    .text("body", "rust embedded database", 100)               // another source
    .fuse_rrf(60.0)            // reciprocal-rank-fusion constant (optional)
    .rerank_mmr(0.7)           // diversify (optional; needs a vector source)
    .order_by("score", true)  // or order by a field instead of rank (optional)
    .offset(0)
    .limit(10)
    .select(["title", "meta.author"])  // project returned docs (optional)
    .run()?;                   // -> Vec<ResultRow> { key, score, document }
# let _ = rows;
# Ok::<(), corvid::Error>(())
```

Notes:
- Zero sources → a pure filter/scan query (streamed, bounded memory).
- One source → ranked by that source. Multiple → fused with RRF.
- `.approx()` lets a *filtered* vector query use the ANN index (over-fetch then
  filter); without it, filtered vector queries run exact.
- `.explain()` returns a human-readable plan string; `.plan()` returns a
  hashable [`QueryPlan`] you can key a `PlanCache` on.

## Aggregations

Over the filtered set (filters and indexes still apply):

```rust
# use corvid::{Db, field, Value};
# let db = Db::open_in_memory()?; let c = db.collection("sales");
c.query().count()?;                          // usize
c.query().filter(field("region").eq(Value::Text("eu".into()))).count()?;
c.query().sum("amount")?;                    // f64
c.query().avg("amount")?;                    // Option<f64>
c.query().min("amount")?; c.query().max("amount")?;  // Option<Value>
c.query().count_distinct("region")?;         // usize
c.query().group_count("region")?;            // BTreeMap<String, usize>
c.query().group_sum("region", "amount")?;    // BTreeMap<String, f64>
c.query().group_avg("region", "amount")?;
# Ok::<(), corvid::Error>(())
```

## Pagination

Keyset (cursor) pagination — no offset rescans:

```rust
# use corvid::{Db, field, Value};
# let db = Db::open_in_memory()?; let c = db.collection("docs");
let mut after: Option<Vec<u8>> = None;
loop {
    let page = c.page(after.as_deref(), 100)?;   // or page_where(after, n, predicate)
    for (key, doc) in &page.rows { /* ... */ let _ = (key, doc); }
    match page.next { Some(cursor) => after = Some(cursor), None => break }
}
# Ok::<(), corvid::Error>(())
```

## Vector search

```rust
# use corvid::{Db, Metric, Value};
# let db = Db::open_in_memory()?; let c = db.collection("docs");
let hits = c.vector_search("embedding", &[0.1, 0.9], 10, Metric::Cosine)?; // Vec<Hit>
# let _ = hits;
# Ok::<(), corvid::Error>(())
```

Metrics: `Metric::Cosine`, `Metric::Dot`, `Metric::L2` (squared). Exact
(brute-force, streamed top-k) until you create a vector index — then it is used
transparently. Image search is the same path over image embeddings (embed in
your app; corvid does not run models).

## Text search

BM25 ranking. The analyzer lowercases, removes common English stop words, and
applies a conservative plural stemmer, so `dog` matches `dogs`:

```rust
# use corvid::{Db, Value};
# let db = Db::open_in_memory()?; let c = db.collection("docs");
let hits = c.text_search("body", "rust databases", 10)?;     // Vec<TextHit>
let phrase = c.phrase_search("body", "embedded database", 10)?; // exact, in order
# let _ = (hits, phrase);
# Ok::<(), corvid::Error>(())
```

## Indexes

All indexes are derived and kept consistent on every write; definitions persist
across reopen. Choose by scale and footprint:

**Vector**

| Constructor | Storage | When |
|---|---|---|
| `create_vector_index(field, metric)` | in-RAM HNSW | default; fast, rebuilt on open |
| `create_vector_index_quantized(field, metric, quant)` | in-RAM, compressed | `Quantization::Binary` (~32×) / `Scalar` (~4×) |
| `create_vector_index_ondisk(field, metric)` | on-disk HNSW | bounded memory, persists, no rebuild |
| `create_vector_index_ondisk_quantized(field, metric, quant)` | on-disk, compressed | billions of vectors on a laptop |
| `create_vector_index_ondisk_pq(field, metric, m, k)` | on-disk, product-quantized | smallest footprint (`m` code bytes/vector) |

**Text** — `create_text_index(field)` (in-RAM) or `create_text_index_ondisk(field)`
(bounded memory, persists). Both back `text_search` and `phrase_search`.

**Scalar** — `create_scalar_index(field)` makes equality/range filters and
counts sub-linear. `create_compound_index(&["a", "b"])` covers
prefix-equality + a trailing range across several fields. Works on nested
fields (`create_scalar_index("meta.score")`).

**Geo** — `create_geo_index(field)` makes radius/bbox/nearest sub-linear.

You don't change your queries to use an index — the builder picks the most
selective available index automatically and falls back to a bounded scan when
none helps.

```rust
# use corvid::{Db, Metric, Quantization};
# let db = Db::open_in_memory()?; let c = db.collection("docs");
c.create_scalar_index("category")?;
c.create_vector_index_ondisk_quantized("embedding", Metric::Cosine, Quantization::Scalar)?;
# Ok::<(), corvid::Error>(())
```

## Geospatial

A location field is `[lat, lon]` or a `{lat, lon}` map. Distances are haversine
kilometres.

```rust
# use corvid::Db;
# let db = Db::open_in_memory()?; let c = db.collection("places");
c.geo_within_radius("loc", 51.5, -0.13, 25.0)?;     // Vec<GeoHit>, nearest first
c.geo_within_bbox("loc", 51.0, -1.0, 52.0, 1.0)?;   // bounding box
c.geo_nearest("loc", 51.5, -0.13, 5)?;              // k nearest, any distance
# Ok::<(), corvid::Error>(())
```

`field("loc").within_km(lat, lon, km)` also composes as a builder filter.

## Graph

A directed property graph over document keys, stored in a reserved namespace
(edges are atomic — forward and reverse in one transaction).

```rust
# use corvid::Db;
# let db = Db::open_in_memory()?; let g = db.collection("people");
g.link(b"alice", "follows", b"bob")?;
g.link_weighted(b"alice", "rates", b"film", 4.5)?;
g.neighbors(b"alice", "follows")?;                  // Vec<Vec<u8>>
g.in_neighbors(b"bob", "follows")?;                 // who follows bob
g.traverse(b"alice", "follows", 3)?;                // BFS up to 3 hops
g.unlink(b"alice", "follows", b"bob")?;
# Ok::<(), corvid::Error>(())
```

## Joins

Left-outer join one collection to another by a foreign-key field:

```rust
# use corvid::Db;
# let db = Db::open_in_memory()?; let orders = db.collection("orders");
let rows = orders.join("customers", "customer_id")?;  // Vec<JoinRow>
# let _ = rows;
# Ok::<(), corvid::Error>(())
```

## Schema & constraints

Optional and opt-in; schemaless collections are unaffected. Enforced on write.

```rust
use corvid::schema::{Schema, Field, FieldType};
# use corvid::Db;
# let db = Db::open_in_memory()?; let c = db.collection("users");
let schema = Schema::new()
    .field(Field::new("name", FieldType::Text).required())
    .field(Field::new("email", FieldType::Text).unique())
    .field(Field::new("age", FieldType::Int));
c.set_schema(&schema)?;   // future writes are validated; violations error
# Ok::<(), corvid::Error>(())
```

## TTL / expiry

The engine keeps no clock — you supply "now". Expired records stay visible until
a purge.

```rust
# use corvid::{Db, Value};
# let db = Db::open_in_memory()?; let c = db.collection("sessions");
# let doc = Value::Int(1);
c.insert_with_ttl(b"s1", &doc, 1_700_000_000)?;  // expiry timestamp (your epoch)
c.set_ttl(b"s1", 1_700_000_500)?;                 // change it
c.ttl(b"s1")?;                                    // Option<i64>
let purged = c.purge_expired(1_700_000_600)?;     // delete everything due by now
# let _ = purged;
# Ok::<(), corvid::Error>(())
```

## Reactive change feeds

```rust
# use corvid::Db;
# let db = Db::open_in_memory()?;
let id = db.subscribe(|ev| {
    println!("{:?} {} {:?}", ev.kind, ev.collection, ev.key);
});
// ... writes fire the callback ...
db.unsubscribe(id);
# Ok::<(), corvid::Error>(())
```

## Semantic cache

A vector-keyed cache: look up by nearest embedding within a threshold.

```rust
# use corvid::{Db, Metric, Value};
# let db = Db::open_in_memory()?;
let cache = db.collection("llm_cache")
    .semantic_cache("embedding", "answer", Metric::Cosine, 0.95);
cache.put(b"q1", vec![0.1, 0.9], Value::Text("the answer".into()))?;
let hit = cache.get(&[0.1, 0.89])?;   // Some(value) if close enough
# let _ = hit;
# Ok::<(), corvid::Error>(())
```

## Probabilistic sketches

```rust
use corvid::{HyperLogLog, BloomFilter};

let mut hll = HyperLogLog::new();
hll.add_bytes(b"user-1");
let approx_unique = hll.estimate();

let mut bloom = BloomFilter::new(10_000, 0.01);
bloom.add_bytes(b"seen");
let maybe = bloom.contains_bytes(b"seen");
# let _ = (approx_unique, maybe);
```

`Collection::approx_distinct(field)` estimates a field's distinct count via HLL.

## Operations

```rust
# use corvid::{Db, Value};
# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("app.corvid");
let mut db = Db::open(&path)?;

// Bulk load: one fsync instead of N (in-flight writes lost on crash before flush).
db.bulk(|| {
    let c = db.collection("docs");
    for i in 0..100_000u32 { c.insert(&i.to_le_bytes(), &Value::Int(i as i64))?; }
    Ok(())
})?;

// Consistent online backup (safe while writers run).
db.backup(dir.path().join("backup.corvid"))?;

// Reclaim file space after heavy deletes (offline; needs &mut).
db.compact()?;

// Logical migration across a format break: dump from old, load into new.
let mut bytes = Vec::new();
db.dump(&mut bytes)?;
let fresh = Db::open(dir.path().join("new.corvid"))?;
fresh.load(&bytes[..])?;   // documents + index/schema/TTL definitions
# Ok::<(), corvid::Error>(())
```

## The MCP sidecar

`corvid-mcp` exposes a store to agentic tools over MCP (JSON-RPC on stdio):

```sh
cargo run -p corvid-mcp -- app.corvid      # file-backed; omit the path for in-memory
```

Point an MCP client at it. Tools mirror the engine: `store`, `patch`,
`compare_and_set`, `get`, `delete`, `delete_where`, `page`, `search`,
`phrase_search`, `count`, `geo`, `join`, `link`, `unlink`, `neighbors`,
`in_neighbors`, `traverse`, `create_index`, `create_text_index`,
`create_scalar_index`, `create_compound_index`, `create_geo_index`, `backup`,
`dump`, `load`, `list_collections`, `insert_auto`. The `search` tool takes
`{filter, vector, text, mmr, rrf_k, select, limit}` — the hybrid builder as JSON.

## Conventions & errors

- **Sync API.** Calls block; share a `Db` across threads behind `Arc`.
- **Single writer, concurrent readers** (redb MVCC). Writes serialize.
- **Reserved names.** Collections beginning `__` are engine-internal and reject
  writes.
- **Errors** are a typed `corvid::Error` (`thiserror`); methods return
  `corvid::Result<T>`. User input never panics.
- **No backward-compat before 1.0.** A format change is migrated with
  [`dump`/`load`](#operations); old files are refused, not silently read.
