# corvid-ffi — typed C ABI and the bindings ecosystem

Date: 2026-08-31 · Status: APPROVED (user-locked) · Controller session.
Binding ruling locked: **no SQL, no JSON, no serialization anywhere in the
runtime path.** The ABI is typed C function calls end to end. corvid-mcp
keeps JSON only because JSON-RPC is the MCP spec; the FFI never touches it.
Second locked ruling: **bindings expose idiomatic OOP**; FFI symbols never
leak into a binding's public API (§8 gates).

Execution: SDD as always — fresh implementer → independent reviewer → one
fix wave max → phase review. Gates per commit: fmt; clippy
`--all-targets --workspace -- -D warnings` (+ feature variants when they
exist); `cargo test --workspace`; `RUSTDOCFLAGS="-D warnings" cargo doc
--no-deps --workspace`. Standing guards: surface radar green in this repo;
BENCHES.md updated for any perf claim; bugs RED-first; docs stay true.

## 1. Handles (the ABI's nouns)

Opaque, one destructor per family:

| Handle | Backed by | Thread contract |
|---|---|---|
| corvid_db* | Arc<Db> | thread-safe (concurrent reads; writes serialized by engine) |
| corvid_coll* | Arc<Db> + name | thread-safe |
| corvid_value* | Value | builder handles single-thread; borrowed children ride the parent |
| corvid_pred* | Predicate tree | single-threaded construction |
| corvid_query* | QueryBuilder state | single-threaded |
| corvid_rows* / _strs* / _geohits* / _groupiter* / _schemaiter* | Vec/map + cursor | read-only cursors, single-threaded |

Enums with EXPLICIT values (never renumbered): corvid_status (OK=0/ERR=1),
corvid_cmp (EQ..GE), corvid_metric (COSINE/DOT/L2), corvid_quant
(NONE/BINARY/SCALAR), corvid_value_type (NULL..MAP, tags 0–8),
corvid_field_type, and 19 detailed error codes corvid_last_error_code()
mapping 1:1 from corvid::Error (exhaustiveness test pins the mapping;
adding an engine variant fails FFI compilation until mapped).

POD structs: corvid_kv {key,len,val}, corvid_field_def
{name,type,required,unique}, corvid_geohit {key,len,distance_km}.

## 2. Function inventory (122 exported symbols; count pinned by the C-surface radar)

Full signatures are specified in docs/FFI.md (Phase 0 Task 1) exactly as
approved in session — grouped here by family with counts and the load-
bearing semantics:

- Lifecycle/errors (8): ffi_version=1, open, open_memory, close,
  last_error_code/message (thread-local), free (the ONLY buffer allocator-
  free), collections.
- Collection (3): collection, collection_free, collection_name.
- Value construction (10): null/bool/int/float/text/bytes/vector
  constructors CLONE inputs; array_new/push, map_new/put.
- Value reads (10): type, as_bool/int/float (+ok flags), text/bytes/
  vector _ref (borrowed, zero copy), array_get/map_get (borrowed
  children), len, free (owned values only).
- Predicates (12): exists, compare(op,path,val), in(vals[]), between,
  starts_with, contains, geo_within, and/or/not (consume children),
  pred_free (never-consumed roots only).
- Query builder (16): new, filter (consumes pred), vector(field,q,dim,k,
  metric), text(field,s,k), fuse_rrf, rerank_mmr, approx, limit, offset,
  order_by(field,desc), select(fields[]), run (CONSUMES query — mirrors
  run(self)), rows_next (key/doc/score; doc borrowed until next),
  rows_free.
- Aggregations (10): count, count_distinct, sum, avg(+some), min/max
  (owned values), group_count/sum/avg (groupiter cursors), groupiter_next/
  free. All consume the query.
- Mutations (13): insert, put_many (bulk fast path), insert_auto,
  update (C fn-ptr + ctx; reentrancy documented), patch, compare_and_set
  (nullable expected/replacement), delete(+existed), delete_where
  (consumes pred), delete_batch, insert_with_ttl, set_ttl,
  purge_expired(now).
- Reads (4): get (owned), scan (row callback), page (cursor contract:
  next_after buffer), len.
- Indexes & schema (13): create_scalar/compound/text/geo/vector×5
  variants (incl. pq m/k and ondisk forms), set_schema(field_def[]),
  schema (schemaiter; NULL out when undeclared), schemaiter_next/free.
- Graph (8): link, link_weighted, unlink(+removed), neighbors,
  in_neighbors, neighbors_weighted, traverse (strs/geohits handles).
- Geo + shared iterators (5): geo_within_radius/bbox/nearest (geohits),
  strs_next/free.

## 3. v1 exclusions (recorded with reopen triggers)

Events/subscriptions (reentrancy across languages — v2 on demonstrated
need); direct vector/text/phrase_search fns (builder covers them);
sketches, semantic cache, PlanCache, explain; begin_bulk (put_many
covers it).

## 4. Ownership & transfer rules (complete)

1. ABI-returned buffers (strings, next_after, auto-keys) → corvid_free(ptr)
   only. 2. Handles → their own _free, never cross-family. 3. const
   corvid_value* inputs are CLONED — caller keeps ownership. 4. Predicates
   consumed by and/or/not/filter/delete_where. 5. run and aggregations
   CONSUME the query. 6. Owned-vs-borrowed outputs documented per
   signature (rows doc + value children are borrowed; freeing them is UB,
   documented in bold). 7. NULL discipline per parameter; unexpected NULL
   → CORVID_E_ARGUMENT, never UB.

## 5. This-repo mechanics

crates/corvid-ffi/ (module per family); no_mangle extern "C" thin wrappers;
SAFETY-commented unsafe; deny(unsafe_op_in_unsafe_fn). cbindgen header,
committed, drift-gated (SYNTAX.md pattern). C-surface radar: header-parsed
symbol set must equal the golden-suite-covered set — no untested exports.
CI: cdylib on the release matrix; C smoke job (compile header + smoke.c,
link, run) on ubuntu/macos/windows; ASan+LSan linux; FFI bench job
(put/get/scan/query vs native — "zero parsing, bounded crossing cost"
becomes a BENCHES.md measurement). Release artifacts per platform: cdylib
+ corvid.h + golden/ + SHA256SUMS via the existing dry-run-proven
pipeline.

## 6. Golden vectors (test-time only)

Line-based fixtures (OP<TAB>args<TAB>expected) including NaN/±inf/-0.0,
cursors, unique violations, geo boundaries. The C smoke suite drives them
through the typed ABI; every binding ports the harness to its NATIVE API.
One behavioral truth, N native implementations, zero runtime parsing.

## 7. Binding repos

corvid-c (CMake/pkg-config; the reference golden port), corvid-node
(napi-rs, prebuilt optionalDeps), corvid-js (wasm-bindgen typed exports;
in-memory; OPFS boundary stated plainly), corvid-go (cgo + pkg-config),
corvid-jvm (JNI, Maven Central, AAR per ABI), corvid-dart (ffigen, pub.dev,
bundled dylibs), corvid-php (C extension, PIE, PHP 8.1–8.4 × NTS/ZTS),
corvid-python (pyo3, candidate). Version rule: bindings pin exact engine
tags; artifacts consumed from that tag's release; bump PRs scripted.

## 8. Binding surface idiom (LOCKED gate)

- Every binding exposes idiomatic OOP: handles become native classes
  (Db, Collection, Query builder with fluent chaining matching Rust's),
  corvid_rows becomes the language's native iteration protocol,
  CORVID_ERR becomes native exceptions, handle destructors map to the
  language's dispose pattern (Symbol.dispose/using TS, AutoCloseable JVM,
  __destruct PHP, Close+finalizer Go, Finalizable Dart).
- FFI symbols in a binding's public API = review-blocking defect.
- The golden suite ports against the OOP surface (the idiom layer is what
  is proven), not the plumbing.
- v1 bindings are synchronous (the engine is sync); async variants are
  additive later, decided by the FFI bench.

## 9. Phasing

Phase 0 (this repo, 8 tasks): (1) plan + docs/FFI.md spec — review gate on
the SPEC before crate code; (2) skeleton: handles, error mapping +
exhaustiveness test, lifecycle, cbindgen drift gate; (3) values + tests;
(4) predicates + mutations + reads + put_many; (5) query builder + rows +
aggregations; (6) indexes/schema/TTL/graph/geo/admin; (7) C-surface radar +
golden generator + C smoke suite (122/122 symbols); (8) CI (matrix, ASan)
+ FFI bench + release wiring + DESIGN decision-log rows.
Phase 1: corvid-node + corvid-c. Phase 2: corvid-js + corvid-go.
Phase 3: corvid-jvm + corvid-dart. Phase 4: corvid-php + corvid-python.
Each binding repo opens with its own plan doc (same machine) porting the
golden suite before any ergonomic sugar.

## 10. Risks (with mitigations)

Lifetime/ownership across repos → §4 rules + ASan/LSan + borrowed-contract
docs. Pre-1.0 churn → tag pinning + FFI_VERSION bumps, enum values frozen.
PHP ZTS/RINIT → last + vectors under both thread modes. JNI overhead →
one crossing per engine op by design; bench gate. wasm persistence
expectations → README boundary. Surface creep → §3 exclusions +
decision-log requirement.

## Tasks (Phase 0)

### Task 1: The ABI specification (docs/FFI.md)

Write `docs/FFI.md`: the complete C surface exactly as locked in §1–§4 —
every handle, every enum WITH its explicit integer values, every POD
struct, ALL 122 function signatures verbatim (grouped by family), the
ownership/transfer table (§4 verbatim), thread contracts, NULL
discipline, the corvid::Error → error-code mapping table (all engine
variants), naming conventions (corvid_* prefix, _ref/_new/_free/_next
suffixes), and the stability policy (FFI_VERSION=1, enums never
renumbered, pre-1.0 breaking allowed with loud bumps). No code. This
document is the contract Tasks 2–8 implement and the binding repos code
against; the review gate checks EVERY signature against the real engine
API (does each fn map to a real pub item with matching semantics —
arity, optionality, return shape).

### Task 2: Crate skeleton — handles, errors, lifecycle, header pipeline

crates/corvid-ffi as a cdylib crate: module skeleton per family; the
error mapping (thread-local last-error, the 19 codes) + the
exhaustiveness test against corvid::Error (compile fails on unmapped
variants); the lifecycle family (8 fns from the spec); cbindgen wired
(config + build.rs or xtask — committed header at a canonical path);
the header drift gate (a test regenerates and diffs, SYNTAX.md
pattern); corvid_free. Gates green; no engine changes.

### Task 3: Value construction + reads

The 20 value functions per spec (constructors clone; _ref accessors
borrow zero-copy; array/map builders; owned-vs-borrowed children).
Unit tests: every Value variant round-trips through the ABI bit-exact
(NaN/±inf/-0.0 payload-preserving — the mutations.rs oracle adapted);
borrow lifetimes exercised (child valid until parent next/free); NULL
discipline (unexpected NULL → CORVID_E_ARGUMENT, never UB).

### Task 4: Predicates + mutations + reads + put_many

Prepend (Task 3 review, binding): (a) fix the map_put invalidation
doc-precision gap — the header/source wording implies only the replaced
child dangles; BTreeMap node splits can relocate EXISTING entries on a
new-key put, so the wording must match array_push's conservative rule
("invalidates previously borrowed children of map") + header regen;
(b) add the alias-path sentence to array_push/map_put docs (on the
self-insertion rejection path the shared handle is consumed — free
neither); (c) correct the T3 report's "plan allowed either" overstatement
(registry was never a plan option; interior views satisfied the plan as
written).

The 12 predicate constructors (consumption semantics for and/or/not);
the 13 mutations (incl. the update fn-ptr callback with its
no-reentrancy contract documented, CAS nullability, delete_where
consuming preds, insert_with_ttl/set_ttl/purge_expired); the 4 reads
(get owned-out, scan callback, page cursor incl. next_after buffer
ownership, len). Tests: pin engine behavior through the ABI — unique
violations, reserved names, TTL boundaries, CAS swap/mismatch, bulk
put_many atomicity.

### Task 5: Query builder + rows cursor + aggregations

The 16 query fns (build a real QueryBuilder under the handle; run
CONSUMES as documented) + rows_next cursor (borrowed doc contract);
the 10 aggregation fns (consume semantics; groupiter cursors). Tests:
a hybrid query through the ABI (filter+vector+text+mmr+limit) with
scores asserted; every aggregation's exact-value oracle; cursor
walks incl. score presence/absence per source shape.

### Task 6: Indexes, schema, TTL-admin, graph, geo, admin

The 13 index/schema fns (all create variants incl. pq m/k + ondisk,
set_schema from field_def array, schema iterator); graph 8; geo 3 +
shared iterators; admin: dump_to_path/load_from_path/
load_from_path_with_renames/backup/compact (path-based per spec).
Tests: index creation drives plan behavior through the ABI; schema
round-trip; graph traverse order pinned; geo hits with distances;
dump→load→query equivalence through the ABI.

### Task 7: C-surface radar + golden vectors + C smoke suite

The radar: parse corvid.h, extract exported symbols, assert the C
smoke suite drives every one (no untested exports — 122/122). The
golden fixture generator + committed fixtures (§6 format; cover
NaN/±inf/-0.0, cursors, unique violations, geo boundaries). smoke.c:
reads fixtures at TEST TIME, drives the typed ABI, checks expected
outputs — compiled + run as a cargo test (cc crate) so the workspace
suite enforces it. ASan/LSan variant wired as a separate test target
or CI step.

### Task 8: CI matrix + FFI bench + release artifacts + decision log

CI: cdylib build on the release platform matrix; the C smoke job on
ubuntu/macos/windows; ASan+LSan linux job; FFI bench (put/get/scan/
hybrid-query through the ABI vs native Rust) with numbers recorded in
BENCHES.md. Release: cdylib + corvid.h + golden/ + SHA256SUMS as
tag artifacts via the existing pipeline (dry-run green). DESIGN.md
decision-log rows: typed-ABI ruling, exclusions with triggers, OOP
idiom gate, sync-first ruling. CHANGELOG.
