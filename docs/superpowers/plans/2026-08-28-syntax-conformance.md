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

## Tasks

Every task: read this whole plan first (taxonomy + rulings bind you); work
only through the public API in `tests/`; per commit run fmt, clippy
`-D warnings`, `cargo test --workspace`, `cargo doc -D warnings`; update
manifest `covering_tests` for every row you satisfy; append your report.

### Task 1: Engine surface manifest, radar, and suite skeleton

- Create `crates/corvid/tests/surface/mod.rs`: `MANIFEST: &[Row]` covering
  EVERY public item of the engine (use
  `docs/superpowers/plans/2026-08-28-surface-raw-inventory.txt` as the
  mechanical seed, then deep-read every module to classify: item, class,
  covering_tests). Enum variants are separate rows (predicates, operators,
  metrics, quantizations, change kinds, error variants, PlanShape variants).
- Source-parsing tests: (a) manifest items == extracted public surface
  (set equality, both directions named in the failure), (b) every non-empty
  `covering_tests` entry names an existing `fn` in `crates/corvid/tests/`.
  Parsing must honor `pub(crate)` (excluded), `#[cfg(test)]` (excluded),
  impl blocks, and trait impls (pub fns in inherent impls of pub types).
- `const STRICT_COVERING: bool = false;` for now; when true, empty
  `covering_tests` fails. Task 15 deletes the flag (always strict).
- Skeleton files (each with one real smoke test so the tree compiles and
  the radar's test-existence check has anchors): `mutations.rs`, `filters.rs`,
  `queries.rs`, `aggregations.rs`, `joins.rs`, `graph.rs`,
  `search_vector.rs`, `search_text.rs`, `search_hybrid.rs`, `search_geo.rs`,
  `schema.rs`, `ttl.rs`, `events.rs`, `lifecycle.rs`.
- Exit: radar green; manifest classified 100%; no row has covering_tests
  pointing at unit tests inside `src/`.

### Task 2: MCP surface manifest and radar

- `crates/corvid-mcp/tests/surface/mod.rs`: manifest of every public item
  (Server methods, ToolError variants, convert fns, protocol surfaces) PLUS
  one row per MCP tool name and per JSON-RPC envelope kind (initialize,
  ping, tools/list, tools/call, error), with a `tools.rs` skeleton.
- Same source-parsing completeness + test-existence checks as Task 1.

### Task 3: Mutation conformance

Fill `mutations.rs` to the case standard: insert (new key, overwrite,
empty key, empty doc kinds — every Value variant as document), insert_batch
(empty slice, duplicate key inside batch rolls back whole batch),
insert_auto (sequence across collections, failed insert does not burn an
id), update (missing doc, transform to every Value kind, index maintenance
observed), patch (deep merge semantics, missing doc, non-map doc),
compare_and_set (expected match/mismatch, index maintenance), delete
(existing/missing), delete_batch, delete_where (0/N matches, predicate
interaction with indexes), insert_with_ttl basics here if public through
Collection (deep TTL cases are Task 12). Assert event emission per kind
where publicly observable. Every error case asserts the exact variant.

### Task 4: WHERE/filter conformance

Fill `filters.rs`: for EACH Predicate form × applicable CmpOp × Value kind:
Int/Float (incl. NaN, ±inf, -0.0, int-vs-float compare), Text (unicode,
empty), Bool, Bytes, Vector, Array, Map, Null; missing path, wrong-type
path, nested dotted paths, deep nesting; In (empty values, duplicate,
mixed types), Between (inclusivity both ends, low>high, equal bounds),
StartsWith/Contains (empty prefix/substr, non-text field), GeoWithin happy
path (deep geo cases are Task 10), And/Or/Not nesting and precedence via
composition, `.and()`/`.or()` builders, indexed vs scan equivalence for
every serviceable predicate (scalar index present vs absent must return
identical sets), OR-union path. filter-only queries with limit/offset.

### Task 5: SELECT shaping conformance

Prepend (W2 wave-review debts, binding): back-cite MANIFEST rows Wave 2
already drives (QueryBuilder::plan_shape, PlanShape + IndexedWindow + Scan
variants, create_compound_index, create_geo_index, QueryBuilder::offset —
cite the filters.rs tests that drive them).
Fill `queries.rs`: select (projection, nested paths, missing fields,
non-map docs, empty list), order_by asc/desc × comparable kinds × missing
× mixed-type total order (class rule), limit (0, 1, n>matches), offset
(0, =len, >len), pagination with page/page_where (after=None, first/last
key, after past end, limit 0/1/N, filtered), scan, for_each_doc (early
return behavior, empty), len/is_empty/count with filters, run() on empty
collection, ResultRow shape. explain()/plan_shape() observable behavior
per shape class (string mentions index family; PlanShape variant match).

### Task 6: Aggregation + join conformance

Fill `aggregations.rs`: count/sum/avg/min/max over Int/Float (NaN, inf,
empty, missing-all, mixed, non-numeric error), count_distinct (+BloomFilter
approx_distinct bound), group_count/group_sum/group_avg (typed group keys
i:/f:/b:/t: escape so distinct types stay distinct, missing skip, container
skip, empty result map), filters respected everywhere.
Fill `joins.rs`: join happy, dotted foreign key path, missing FK field,
dangling reference (row omitted), self-join, empty sides, JoinRow shape.

### Task 7: Graph conformance

Fill `graph.rs`: link (new, duplicate, self-loop, missing endpoints
error/allowed per API), unlink (existing/missing), neighbors outgoing,
in_neighbors, traverse (depth 0/1/N, cycles terminate, relation filtering,
missing start), delete cascade (doc delete removes edges both directions,
observed via neighbors), events on link/unlink where observable, edge
weight if public.

### Task 8: Vector search conformance

Prepend (W3 wave-review housekeeping, binding): (a) tighten the
graph.rs:285-293 edges_on_delete_in_txn doc clause so it cannot be read as
TTL-purge-purges-on-absent-rows (it does not — Task 12 owns that corner);
(b) add graph.rs cascade tests as citations on the Collection::delete /
delete_batch / delete_where rows.
Fill `search_vector.rs`: Metric × Quantization full cross (None/Binary/
Scalar × Cosine/Dot/L2) on a fixed corpus with known geometry: ranking
order asserted, exact vs approx (approx=true uses index and returns
`Hit.approximate` correctly; with filter; recall sanity ≥ threshold for
the fixed corpus), zero-norm vectors, dimension mismatch error, k=0/1/N,
k> corpus, ef/param variants exposed publicly (Hnsw::with_params/
with_quant via create path), empty collection, single doc, query through
builder vector() with select/order interplay, vector field missing from
some docs (excluded deterministically).

### Task 9: Text + hybrid conformance

Prepend (Task 8 review nits, binding): in tests/search_vector.rs fix three
comment-precision points — (1) the ef over-fetch comment: in-memory formula
is want=k+dead, ef=max(4*want,64) (the max(4k,64) form is the on-disk path
only); (2) make the k1-binary tie-winner proof self-contained by
cross-referencing that lazy builds scan keys in order so ids are key order;
(3) soften the PQ top-1 comment to what the assertion needs.
Fill `search_text.rs`: BM25 ranking on fixed corpus (order asserted),
tokenization (case, punctuation, unicode, s_stem irregulars), phrase_search
(order-sensitive match/non-match, repeated terms, cross-field non-match),
empty query, no hits, k bounds, analyzer raw() vs default.
Fill `search_hybrid.rs`: vector+text fusion ordering (RRF k default/
custom/invalid — error variants), mmr diversification (λ 0/1/out-of-range/
NaN errors), single-source noop, docs without embeddings survive rerank,
fuse_rrf on empty rankings, reciprocal_rank_fusion/mmr direct fn behavior.

### Task 10: Geo conformance

Prepend (Task 9 review nits, binding — comment fixes only): in
tests/search_text.rs fix the s_stem "bus" comment (len<=3 guard, not the
...us rule; "genus" is the ...us case); in tests/search_hybrid.rs fix the
garbled λ=0.1 margin notation (actual decision margin 0.8); make
Bm25Params message pinning consistent with hybrid's starts_with style.
Fill `search_geo.rs`: haversine_km known distances, geo_within_radius
(boundary inclusion at ==radius, tiny/large radius, center on doc),
geo_nearest (k=0/1/N, farther-than-all), geo_within_bbox (normal, inverted
corners error, degenerate bbox, antimeridian-crossing if supported —
assert actual behavior), GeoWithin predicate deep cases (poles, invalid
lat/lon), point formats ([lat,lon] array vs lat/lon map), geo index
present vs absent equivalence, GeoHit fields.

### Task 11: Schema/index conformance

Fill `schema.rs`: create_scalar_index (dedupe, mixed types, re-create),
create_compound_index (order of fields matters, prefix behavior),
create_text_index, create_vector_index variants, create_geo_index; unique
constraints (violation error variant, NaN==NaN rule, Vector equality,
null uniqueness); name validation (interior `__`, NUL, reserved, empty);
index-vs-scan result equivalence for every family; index observably used
via plan_shape/explain; behavior when index exists on field then docs
mutate (insert/update/delete keep results correct) — INCLUDING
compound-index maintenance under mutation: update/patch/CAS that removes
or adds a TRAILING indexed field, then a prefix-only query (scan path
must reflect the removal — stale entries must never surface; W2 ruling).
Also re-verify the fix-round hardening from W2: the full-coverage
soundness gate on compound windows (every indexed field constrained, or
decline to scan) must hold — pin it once from the public API.

### Task 12: TTL + events conformance

Prepend (W3 wave-review ruling, binding): make `purge_due_key` run the
edge cascade regardless of `existed` (same contract as delete — a stranded
TTL entry must not leave dangling edges; RED first).
Fill `ttl.rs`: insert_with_ttl/set_ttl/ttl roundtrip, purge_expired at
boundary (now == expires_at included/excluded per contract), purge
idempotence, expired docs hidden from queries/get/count, TTL + index
maintenance (expired leaves index), set_ttl overwrite/clear semantics,
errors (missing doc).
Fill `events.rs`: subscribe returns id, Insert/Delete events per mutation
path (insert, update, patch, compare_and_set, delete, delete_where,
insert_auto, batch ops, TTL purge), unsubscribe (true/false), events after
unsubscribe stop, cross-collection isolation.

### Task 13: Lifecycle conformance

Prepend (Task 12 review nits, binding): (a) trim the doc comment on
tests/events.rs events_dispatch_is_synchronous_post_commit... to claim only
what the test asserts (mid-dispatch unsubscribe semantics are true of the
source but not pinned there — either add the assertion or drop the claim);
(b) add the one-line cost note at ttl.rs's purge cascade (unconditional
cascade = paged scan of both edge namespaces, parity with delete-of-missing).
Prepend (Task 11 routing, binding): drive `Error::CorruptIndex` through
the public API (write a real db file with a vector index, corrupt index
bytes on disk, reopen, query -> exact variant) so the manifest row gets a
covering test before strict mode.
Fill `lifecycle.rs`: Db::open real file (create/reopen persistence across
handles), open_in_memory, backup (restores identical state; error on bad
path), bulk (atomicity on panic-free failure, !Send scope respected,
nested rejection if any), compact (returns bool, data intact), collections
(user namespaces only), set_relaxed_durability + flush, Store::{transaction
commit/rollback-on-Err, read isolation, put/get/delete/scan/scan_from/
scan_prefix/count/for_each/next_auto_id}, dump→load round-trip exercising
EVERY Value variant, vector indexes (each Metric × Quantization), text
indexes, geo indexes, TTL entries, graph edges, auto-ids; load rejects
reserved names; dump of empty db; SemanticCache put/get (threshold hit/
miss), HyperLogLog (new/with_precision/add_bytes/add_hash/estimate bounds,
precision bounds), BloomFilter (new fp bounds, add/contains, no false
negatives), PlanCache (get miss/hit, get_or_insert_with, len/is_empty).

### Task 13.5 (folded into Task 15): strict-mode exemptions and Pq rows

Controller rulings at W5 exit (bind Task 15):
- The seven redb-passthrough variants (`Error::{Transaction, Table,
  Storage, Commit, SetDurability, Compaction, IncompatibleFormat}`) are
  verified undrivable from the public API (fault paths unreachable; no
  hooks per Ruling 3; IncompatibleFormat's version lives in redb META,
  not the public byte layer). Strict mode ships with an explicit
  `EXEMPT_FROM_STRICT` list in the radar — each entry with its one-line
  justification — and the strict test must assert every exempt row is
  EMPTY (no fake citations) and every non-exempt row is non-empty.
  Growing the list requires a controller-reviewed commit.
- The 12 `corvid::pq::Pq*` rows are publicly drivable and MUST gain real
  conformance tests (drive Pq directly: train/encode/search determinism,
  codebook sizes, recall sanity on fixed corpus) before strict mode.
- Minor from Task 13: add the HLL bound-fragility note (std-upgrade could
  shift DefaultHasher; bounds safe per-toolchain) to the test comment.

### Task 14: MCP wire conformance

Fill `crates/corvid-mcp/tests/tools.rs` driving the server IN-PROCESS over
duplex in-memory I/O (no child processes — they evade coverage and flake):
initialize/ping/tools/list envelopes; EVERY tool happy + each error path
(bad params shape, unknown tool, engine error surfacing); $vector/$bytes
convert conventions both directions; result caps and bounded limits
(search/page limits clamp); tools/call error envelope codes; large
payload behavior. Assert result JSON shapes exactly.

### Task 15: SYNTAX.md, strict radar, changelog, final sweep

Generate `docs/SYNTAX.md` from the manifest (statement-class sections,
every construct with its covering test names); SYNTAX.md must state: the
pre-ranking-predicate semantics (filtered builder text queries
re-normalize BM25 stats over the filtered candidate set);
geo_within_bbox's portable key order; the NaN-duality note (predicate
comparisons: NaN matches nothing; storage equality (CAS/unique): NaN==NaN);
an explicit equality-is-per-construct section (CAS semantic NaN==NaN vs
predicate NaN-never-equal vs join decimal-string Int≡Text vs tagged group
keys). DELETE `STRICT_COVERING` (radar always requires non-empty
covering_tests) and make the manifest green under strict mode; CHANGELOG
entries — including the W2 compound-window soundness fix (omitted-
documents bug + plan changes IndexedWindow→Scan for prefix-only) and a
half-sentence on CAS now surfacing decode errors on corrupt rows; verify
no manifest row cites a test that does not exist; full gates + coverage
report attached to report.

## Conventions (added at W1 exit; bind all later tasks)

1. **Shared error variants cite every raiser.** A row like
   `Error::InvalidArgument` (raised by hybrid params, geo validation, and
   text k-bounds alike) keeps ONE primary class but its `covering_tests`
   must, by the time strict mode lands, cite tests driving it through every
   raiser's class. Task 15's SYNTAX.md generator annotates such rows as
   shared across classes.
2. **Covering-test names are globally unique and descriptive.**
   `covering_tests` entries are bare fn names, so uniqueness across the
   whole tests/ tree is enforced by the radar (added in Task 3
   housekeeping); prefer `<construct>_<behavior>` names.
3. **Radar self-tests are not citable.** `tests/surface/` is excluded from
   the covering-test index (Task 3 housekeeping).
4. **Ruling reaffirmed:** conformance tests use only the public API; the
   radar's `#[test]`-only indexer is the citation mechanism (a cited fn
   must be a real `#[test]`).
