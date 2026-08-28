# Syntax conformance program — 100% coverage of everything a user can write

Date: 2026-08-28 · Status: BINDING · Author: controller session
Supersedes the line-coverage framing: the goal is **surface conformance**, not
llvm-cov percentages. The database's "SQL" is its fluent API + MCP wire
protocol; every construct in that language gets happy/edge/error/corner tests,
and a committed radar makes it impossible to add a construct without tests.

## Goal

For every public construct of `corvid` and `corvid-mcp`:

1. At least one conformance test drives it **through the public API only**
   (integration tests in `crates/*/tests/`), asserting real observable
   behavior against real stored state.
2. Every case class that applies is covered:
   - **happy** — normal operation, result asserted (not just `is_ok`).
   - **edge** — empty / one / missing / boundary: empty collection, empty
     key, empty vector, empty string, 1 element, missing field, missing doc,
     first/last keys, `i64::MIN`/`MAX`, `f32`/`f64` extremes (`NaN`, `±inf`,
     `-0.0`), unicode, nested-path depth.
   - **error** — every documented failure asserted on the exact `Error` /
     `ToolError` variant (unknown key, duplicate unique value, invalid name,
     invalid params, type mismatch, dimension mismatch, bad TTL, …).
   - **corner** — interactions: construct under index vs scan, with filters,
     with pagination boundaries, mixed-type ordering, NaN semantics,
     cross-metric behavior.
3. A **surface radar** test fails CI if a public item is added, removed, or
   left without a covering test: the manifest and reality cannot drift.

llvm-cov stays as a reported side metric (existing 90% CI floor unchanged);
it is not the goal. Where conformance work finds real bugs, they are fixed
in-wave under the standard review process.

## The language (statement-class taxonomy)

Every manifest row is classified into exactly one class; the conformance
suite is organized identically.

| Class | SQL analogue | Surface |
|---|---|---|
| Mutations | INSERT / UPDATE / DELETE | `insert`, `insert_batch`, `insert_auto`, `update`, `patch`, `compare_and_set`, `delete`, `delete_batch`, `delete_where`, `insert_with_ttl`, auto-id allocation, event emission per mutation |
| WHERE | WHERE | `Predicate::{Compare,Exists,In,Between,StartsWith,Contains,GeoWithin,And,Or,Not}` × `CmpOp::{Eq,Ne,Lt,Le,Gt,Ge}` × `Value` type lattice (missing field, wrong type, nested dotted path, containers), `field()` helpers, `.and()/.or()` composition |
| SELECT shaping | SELECT / ORDER / LIMIT / JOIN | `query()`, `select`, `order_by`, `limit`, `offset`, `run`, `scan`, `page`, `page_where`, `for_each_doc`, `len`, `is_empty`, `count` |
| Aggregations | GROUP BY / aggregates | `count`, `count_distinct` (+`BloomFilter::approx_distinct`), `sum`, `avg`, `min`, `max`, `group_count`, `group_sum`, `group_avg` — typed group keys, missing/container skips |
| Vector search | vector KNN | `vector(field, q, k, metric)` × `Metric::{Cosine,Dot,L2}` × `Quantization::{None,Binary,Scalar}`, `approx`, exact fallback, dim mismatch, zero-norm, `Hit.approximate` rerank |
| Text search | full-text | `text(field, q, k)`, `phrase_search`, `Bm25Params::new`/`validate`/`raw`, `tokenize`, `s_stem`, `Analyzer`, `analyze`, `idf`, `term_score` |
| Hybrid | fusion | `fuse_rrf(k)`, `rerank_mmr(λ)`, `reciprocal_rank_fusion`, `mmr` — param validation, single-source noop, docs without embeddings |
| Geo | spatial WHERE | `GeoWithin` predicate, `geo_within_radius`, `geo_within_bbox`, `geo_nearest`, `haversine_km`, `GeoHit` — poles, antimeridian, radius 0, bbox validation |
| Schema (ALTER) | CREATE INDEX / constraints | `create_scalar_index`, `create_compound_index`, `create_text_index`, `create_vector_index` (HNSW/PQ variants incl. `Hnsw::new/with_params/with_quant`), `create_geo_index`, unique constraints, name validation, index-vs-scan equivalence |
| TTL | expiry | `insert_with_ttl`, `set_ttl`, `ttl`, `purge_expired` — boundary `now == expires_at`, purge idempotence, hidden-from-query behavior |
| Graph | edges | `link`, `unlink`, `neighbors` (out), `in_neighbors`, `traverse`, delete cascade, missing endpoints, events |
| Joins | JOIN | `Collection::join(other, foreign_key_field)`, `JoinRow`, dotted-path FKs, missing FK rows |
| Lifecycle | admin | `Db::open`/`open_in_memory`, `backup`, `bulk`/`begin_bulk`, `compact`, `collections`, `set_relaxed_durability`, `Store::{transaction,read,put,get,delete,scan,scan_from,scan_prefix,count,for_each,next_auto_id,flush}`, `dump`, `load`, `ChangeEvent`/`subscribe`/`unsubscribe`, `SemanticCache::{put,get}`, `HyperLogLog::{new,with_precision,add_bytes,add_hash,estimate}`, `BloomFilter::{new,add_bytes,contains_bytes}`, `PlanCache::*`, `QueryPlan`, `explain`/`plan_shape`/`PlanShape` |
| MCP wire | wire protocol | JSON-RPC envelopes (`initialize`, `ping`, `tools/list`, `tools/call`, error response), all 30+ tools (`store`, `patch`, `compare_and_set`, `get`, `delete`, `delete_where`, `search`, `create_index`, `link`, `unlink`, `neighbors`, `traverse`, `geo`, `join`, `in_neighbors`, `page`, `phrase_search`, `create_text_index`, `create_scalar_index`, `create_geo_index`, `create_compound_index`, `backup`, `dump`, `load`, `list_collections`, `count`, `insert_auto`), param parsing (`uint_param`, `field_param`, `parse_metric`, `parse_quant`, `parse_predicate`, `bounded_limit`, …), result caps, `convert::{json_to_value,value_to_json}` conventions |

## Radar design (Wave 1 deliverable)

- `crates/corvid/tests/surface/mod.rs` (+ `crates/corvid-mcp` equivalent):
  - `MANIFEST: &[Row]` where `Row { item, class, covering_tests: &[&str] }`
    — `item` is the fully qualified path (`corvid::Collection::insert`,
    `corvid::Predicate::Between`, `corvid::Metric::Dot`, …).
  - One `#[test]` walks the crate's `src/` from `CARGO_MANIFEST_DIR`,
    extracts the public item inventory (pub fn / enum variant / struct /
    trait / const, honoring `pub(crate)` exclusion and `#[cfg(test)]`
    blocks), and asserts **set equality** with the manifest: additions and
    removals both fail until the manifest is maintained.
  - Another `#[test]` asserts every `covering_tests` entry names a test fn
    that exists in the `tests/` tree (source-parsed the same way).
- Manifest rows are only added with covering tests; reviewers verify the
  case-matrix standard per row, not just presence.
- Enums contribute one row per semantically distinct variant (operators,
  metrics, quantizations, predicates, change kinds, error variants);
  derived getters (`len`, `is_empty`, `key`) get rows too — nothing is
  "too trivial".
- `corvid-wasm` is a size harness, not a language surface; it keeps its
  existing smoke test and stays out of the radar (documented here as the
  ruling).

## Waves

Execution: subagent-driven development, one implementer + one reviewer per
task, fix rounds, whole-branch review per wave. All prior gates apply per
commit: `cargo fmt --all`; `cargo clippy --all-targets --workspace --
-D warnings`; `cargo test --workspace`;
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`.

- **W1 — Radar + manifest.** Build the surface manifest for both crates by
  deep-reading every module (the taxonomy above is the checklist), the
  source-parsing completeness tests, and the `tests/` skeleton modules named
  by statement class. Exit: radar green over a manifest classified 100%.
- **W2 — Mutations + WHERE.** Fill `mutations.rs` + `filters.rs`: every
  mutation construct and every predicate/operator/lattice cell.
- **W3 — SELECT + aggregations + joins + graph.** Fill `queries.rs`,
  `aggregations.rs`, `graph.rs`, `joins.rs`.
- **W4 — Search.** Fill `search_vector.rs`, `search_text.rs`,
  `search_hybrid.rs`, `search_geo.rs` — full parameter cross-products.
- **W5 — Schema + lifecycle.** Fill `schema.rs`, `ttl.rs`, `lifecycle.rs`
  (incl. dump/load round-trip of every construct, events, caches, sketches).
- **W6 — MCP wire + docs.** Fill `crates/corvid-mcp/tests/tools.rs` (every
  tool × envelope × error, in-process over duplex I/O — child-process
  spawning is banned because it evades coverage and adds flake); generate
  `docs/SYNTAX.md` from the manifest; CHANGELOG; final whole-branch review.

## Rulings (binding)

1. Conformance tests use **only** the public API; unit tests remain for
   internals. If a public construct cannot be exercised publicly, that is an
   API defect — raise it, do not test around it.
2. No test may assert only `is_ok`/`is_err` — assert values and variants.
3. No `#[cfg(test)]` reach-ins from integration tests; no new test-only
   hooks in production code; no weakening of validation to make tests pass.
4. Duplicate coverage is acceptable (unit + conformance may overlap);
   absence is not.
5. Bugs found by conformance tests are fixed in the same task, RED test
   first, and noted in the task report.
6. The manifest is the single source of truth for what "everything" means;
   `docs/SYNTAX.md` is generated from it in W6, not hand-maintained.
