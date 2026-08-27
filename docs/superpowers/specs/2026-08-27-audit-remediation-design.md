# Audit remediation — design spec

Date: 2026-08-27
Status: approved design (pending implementation planning)
Source: full-workspace deep audit of 2026-08-27 (four domain line-reads plus
independent verification of every high-severity finding; build state at audit
time: 415 tests green, clippy `-D warnings` clean, CI gates real).

This spec turns the audit findings into a phased remediation. The guiding
rule, set by the project owner: **do what is best, not what saves work; no
tech debt carried forward.**

---

## Context

The 2026-08-26 audit (`AUDIT.md`) plus the 2026-08-27 re-audit found the
engine structurally sound (atomic write path, codec robustness, BM25/geo/sketch
math) with a specific set of correctness holes, lifecycle gaps, and doc/code
drift. This spec fixes all of them in five dependency-ordered waves. Finding
IDs below are referenced as `A#` (high), `B#` (medium), `C#` (low/hygiene),
`D#` (docs/claims). File:line anchors are from HEAD `166e0f4` and may drift;
the description is authoritative.

### Findings inventory

**High**
- A1 `index.rs:108-114` — `BuiltIndex::add` returns from the dimension guard
  *before* `tombstone(key)`: overwriting an indexed doc with a
  different-dimension vector leaves the old node live; ANN results diverge
  from exact. Regression from commit 166e0f4.
- A2 `scalar.rs:752-774` (+ compound `:781-805`, `geo_index.rs:351-373`,
  `fts.rs:483-506`, `index.rs:580-700`) — index creation is
  register-then-backfill: def commits first, backfill runs in separate
  per-page transactions. Crash/error/race leaves a permanently partial index
  that reopen trusts and queries serve (missing docs = silent false
  negatives; unique check trusts the same buckets; worst case on-disk HNSW
  returns silent-empty).
- A3 `schema.rs:285-292` — unique constraint silently skipped when the unique
  field has a scalar index and the value is a non-scalar type
  (`encode_value` → `None` → `continue`); NaN never conflicts with NaN.
  Creating an index weakens the constraint.
- A4 `pq.rs:146-149` — `adc_l2` indexes the ADC table with an unchecked code
  byte (panic from the public L2 path); `l2_table` (`pq.rs:124-126`) returns
  an all-zero table on dimension mismatch (every distance 0) instead of
  signaling unserviceable.
- A5 `index.rs:251-284` — re-registering a vector index never resets the
  on-disk namespace: old-encoding nodes/meta/entry survive under the new def;
  mixed-encoding decode panics (debug) or produces garbage distances
  (release); kind switches leak namespaces forever.

**Medium**
- B1 `db.rs:100-112` — `Db::bulk` leaks relaxed durability on panic and to
  concurrent writers.
- B2 `db.rs:194`/`db.rs:232` — TTL marker race: `ttl_enabled` read before the
  txn, `mark_ttl_collection` after commit; a plain insert can inherit a stale
  expiry and later be purged. `ttl.rs:195-207` — purge recheck-then-delete
  spans two transactions; its regression test (`ttl.rs:391-402`) never
  exercises the recheck path.
- B3 (prior M5) — builder/aggregations fetch documents in per-key read
  transactions; a query can return a set matching no point in time
  (omission-only). `traverse` has the same per-hop shape.
- B4 (prior M25) — document delete orphans graph edges in both directions;
  graph mutations emit no change events.
- B5 (prior M13) — on-disk HNSW tombstones never compact; recall collapses
  under churn.
- B6 `query.rs:147-160` — quantized ANN distances surfaced as the metric's
  own; no rerank, no approximate marker; `semantic_cache` thresholds break
  under Binary (Hamming) distances.
- B7 (prior M16) — phrase score differs indexed (BM25 sum) vs fallback
  (occurrence count); builder recomputes BM25 stats over the filtered/top-k
  subset, so entry points disagree on order.
- B8 `migrate.rs:130-249` — `dump` is not point-in-time; `load` reads the
  whole file into RAM; reserved-name rejection inconsistent (index-def and
  schema replay paths accept `__`); `create_scalar_index`/`set_schema` never
  call `ensure_writable`.
- B9 — one global `IndexState` mutex: a full build/compaction blocks all
  `vector_search` and holds writers' redb write transactions open db-wide.
- B10 `builder.rs:585-605` — `In`-predicate index union has no aggregate cap
  (the OR-union caps at 100k).

**Low / hygiene**
- C1 `schema.rs:158`, `disk_hnsw.rs:205-209` — `Vec::with_capacity` from
  unvalidated u32 counts (OOM abort on forged/corrupt defs).
- C2 `geo.rs:158-159` — antimeridian bbox `(min_lon..=max_lon)` is vacuous →
  silent empty; no lat/lon range validation.
- C3 `builder.rs:797` — `explain()` always prints `scan(...)` regardless of
  the plan actually taken.
- C4 `builder.rs:183` vs `:896`/`1086` — `order_by` doc promises incomparable
  values sort last; they interleave by key.
- C5 `filter.rs:303-312`, `scalar.rs:74` — i64 compared via f64: values beyond
  2^53 collapse (nanosecond timestamps affected).
- C6 `fusion.rs:21-40/77` — RRF k, MMR λ, Bm25Params unvalidated (negative k →
  inf scores; λ outside [0,1] inverts diversity).
- C7 `scalar.rs:57-64`, `:710-746` — namespace/def-key collisions from
  crafted names (NUL in names; `a__b` vs `a`/`b__c`); no name validation.
- C8 `store.rs:201-235` — backup TOCTOU (`exists()` then `create`) and
  partial-file debris on mid-copy failure.
- C9 `db.rs:459-467` — `insert_auto` burns an id on failed insert;
  `db.rs:416` `len()` truncates `as usize` on 32-bit.
- C10 — weak index-path tests (`builder.rs:1456-1544`) stay green if the index
  code is deleted; `custom_rrf_constant_is_accepted` never observes the
  constant.
- C11 — ci.yml lacks `timeout-minutes`/`concurrency`; release.yml: release
  created before builds, no tag↔version check, no checksums.
- C12 — MCP `convert.rs`: u64 > i64::MAX silently becomes a lossy float;
  non-finite floats become JSON `null` (round-trip fidelity).
- C13 — `disk_hnsw.rs:444/546` — corrupt keymap value tombstones node 0.
- C14 — phrase adjacency computed after stop-word removal (documented behavior
  needed); S-stemmer misses (`boxes`↔`box`).

**Docs / claims**
- D1 README:113 — "iOS/Android ✅" but ci.yml has no iOS job.
- D2 README:56-57 — "never a post-hoc trim" stated unconditionally; `.approx()`
  is a documented post-hoc trim.
- D3 `store.rs:17-21`, `db.rs:19-22` — module docs still describe the
  pre-a0bffbb world (indexes "not written inside the document's transaction").
- D4 `builder.rs:238-240` — group-key doc says "text as-is"; code emits
  `s:`-prefixed keys into public results.
- D5 DESIGN.md:76 — "layers above v0.1 are empty traits" contradicts shipped
  graph/geo/sketches/reactive/cache; :89 zstd and :385 tracing specified but
  absent; decision log uniformly dated 2026-05-29; open-question #3 answered
  but not closed.
- D6 AUDIT.md describes the pre-fix state; 15 of 25 findings fixed at HEAD.

---

## Decisions (made 2026-08-27, with rationale)

1. **Index-creation atomicity = watermark + reconcile-on-open** (over
   single-txn backfill and staging-namespace swap). Same crash-safety as the
   alternatives without redb's single write lock held for a whole-collection
   backfill (the availability cliff the lazy-build fix just escaped) and
   without duplicate maintenance paths. This is the standard online-index-build
   shape. Recorded for DESIGN.md's decision log in wave 2.
2. **Query reads = snapshot-scoped execution.** One read transaction per
   query/aggregation, plummed as a `Snapshot` handle; per-operation snapshot
   wording in DESIGN.md becomes literally true rather than softened. Chosen
   because the consistency invariant is the project's identity.
3. **Group keys: bare text, tagged non-text.** `Text("blog")` → `"blog"`,
   `Int(1)` → `"i:1"`, `Float(1.0)` → `"f:1"`, `Bool(true)` → `"b:true"`.
   Natural for the dominant case, still collision-free across types. Breaking
   change to the `s:`-prefixed form is acceptable pre-1.0.
4. **Old index defs (no state byte) read as `Complete`** on open — preserves
   pre-existing behavior, no silent rebuilds of user data. The def encoding is
   otherwise free to change (pre-1.0, no backward compat per project rules).
5. **Corrupt on-disk index state surfaces as a typed error**
   (`Error::CorruptIndex`) rather than silent-empty — matches the
   never-panic, typed-errors rule and makes corruption diagnosable.
6. **NaN equals NaN for uniqueness** (bit-canonical comparison) — uniqueness
   is about identity of stored values, not IEEE comparison semantics.

Non-goals: no new features, no performance work beyond what fixes require,
no rewrite of subsystems the audit verified solid.

---

## Wave 1 — correctness hot-fixes

Each item is its own commit, test written first. No format changes.

1. **A1** — reorder `BuiltIndex::add` (`index.rs:108-114`): tombstone the
   existing node for `key` *before* the dimension check returns. Test:
   overwrite an indexed doc with a different-dimension vector; ANN result for
   the old-dim query must exclude the key (parity with exact search).
2. **A3** — `validate_unique_in_txn` (`schema.rs:285-292`): when
   `encode_value` returns `None`, fall through to the in-txn scan comparison
   (same path as the no-index case) instead of `continue`. NaN-equals-NaN in
   that comparison (D6). Tests: unique Bytes field + scalar index rejects
   duplicates; NaN Float unique field rejects duplicate NaN; same-key
   overwrite still allowed.
3. **A4** — `adc_l2` (`pq.rs:146-149`): guard `c` against `k`; an
   out-of-range code contributes `f32::INFINITY` for the node (never ranks,
   never panics). `l2_table` (`pq.rs:122-126`) returns `Option<Vec<f32>>`;
   `None` on dim mismatch propagates as "cannot serve" so callers take the
   existing exact fallback. Tests: malformed codes; mismatched-dim query
   falls back.
4. **B1** — `Db::bulk` (`db.rs:100-112`): a thread-local bulk depth on
   `Store` (relaxed durability applies only to write transactions begun on
   the bulk-owning thread) plus a drop guard that restores state on panic.
   Tests: panic inside bulk leaves durability on (catch_unwind asserting the
   thread-local/flag state); a write transaction on a non-bulk thread keeps
   durable commit (unit-test the flag logic in `store.rs`).
5. **D4 + decision 3** — `group_key` (`builder.rs:1040-1048`): bare text,
   tagged non-text; update `group_count`/`group_sum`/`group_avg` docs to
   specify the canonical form; update tests (incl. the typed-keys regression
   test) and MCP-facing docs if any examples show prefixed keys.

Exit criteria: all new tests green; `cargo test` + clippy clean; coverage
≥ 90% held.

## Wave 2 — index-creation atomicity (A2)

Scope: `create_scalar_index`, `create_compound_index`, `create_geo_index`,
`create_text_index_ondisk`, `create_vector_index_ondisk{,_quantized,_pq}`,
plus the open path and dump/load.

Design:
- Index defs gain a persisted state: `Building { cursor: Vec<u8> } |
  Complete`. Encoding lives in each kind's def storage (def value bytes or the
  index namespace — implementer's choice, but it must commit atomically with
  backfill batches).
- `create_*` registers the def as `Building { cursor: start }` in one
  transaction, then backfills in batches; each batch's transaction also
  advances the cursor; the final batch flips the state to `Complete`.
- Write-path maintenance is unchanged (in-transaction, as of a0bffbb) and is
  idempotent against backfill overlap (index entries are keyed by
  encoded-value‖doc-key / doc-key, so re-insertion is an upsert).
- Query paths treat `Building` as *not serviceable*: `scalar_candidates` /
  `geo_candidates` / `fts_search` / `ann_search` return the existing
  "cannot serve" signal; the builder's exact/bounded fallbacks serve the
  query. Correct, temporarily slower — never wrong.
- Open: `load_*_defs` reads state; a `Building` index resumes backfill
  lazily on first use, under the registry lock (same discipline as the lazy
  HNSW build; queries during resume use the fallback). Rationale: keeps opens
  fast at any collection size and reuses the proven lock discipline.
- Re-registering an existing index starts a fresh `Building` cycle
  (foundation for A5 in wave 3).
- Old defs without state bytes decode as `Complete` (decision 4).
- `dump`/`load` round-trip index state; a dumped `Building` index reloads and
  resumes (or is dumped as `Building` with its cursor — the load target
  finishes the backfill on first use).

Tests: failpoint-injected mid-backfill abort (process-level, like
`tests/durability.rs`) leaves a `Building` index that (a) never serves
queries post-reopen until resumed, (b) resumes to `Complete` and then serves
correctly; concurrent-writer-during-creation race test (the
scalar_fields-before-register interleaving from the audit); a partial index
produces fallback results identical to an unindexed collection (parity
property test); decode of legacy stateless defs.

Exit criteria: property "an index never serves a query unless `Complete`,
and once `Complete` reflects all committed documents" is test-enforced for
all five index kinds.

## Wave 3 — on-disk index lifecycle (A5, B5, B6, C1, C13)

1. **A5** — re-register clears the target namespace (nodes, meta, entry,
   keymap, codebook) in the same transaction that installs the new `Building`
   def (wave 2's cycle). Kind switches (OnDisk→InMemory, →PQ, →quantized)
   clean up the old namespace. No mixed-encoding graphs, no leaked
   namespaces. Tests: switch quantization modes and kind, then search +
   reopen; assert no stale nodes survive (namespace cleared) and results
   match exact.
2. **B5** — on-disk HNSW compaction: when the dead-node fraction exceeds a
   threshold (mirror the in-memory >50% rule), a rebuild-under-lock rewrites
   live nodes compactly and atomically swaps meta/entry. Trigger checked on
   delete paths and search (like the in-memory variant). Tests: bulk-load,
   mass-delete, search returns full k; namespace size shrinks.
3. **B6** — `vector_search` reranks ANN candidates with exact distances
   computed from the fetched documents (parity with the builder path, which
   already discards index distances); `Hit` gains `pub approximate: bool`
   (true when served by an index). `semantic_cache` documents/relies on
   exact-scale distances. Tests: quantized index returns distances equal to
   the exact metric on the same pairs; `approximate` flag correct per path.
4. **C1/C13 + decision 5** — `decode_node`/`Schema::decode` clamp
   `with_capacity` against remaining input bytes; corrupt keymap values skip
   the entry (no node-0 tombstoning); unrecoverable graph/meta corruption
   returns `Error::CorruptIndex` (new typed variant) instead of silent-empty.
   Tests: truncated/forged values error cleanly, never abort, never
   mis-tombstone.

## Wave 4 — consistency windows (B2, B3, B4, B7, B8, B10)

1. **B2** — TTL: decide TTL-maintenance inside the write transaction by
   checking the `__ttl__<collection>` namespace presence in-txn (replacing
   the pre-txn in-memory marker read at `db.rs:194`); `purge_expired` deletes
   each due key via compare-expiry-and-delete (one txn: re-read the forward
   entry; delete only if the timestamp still matches); replace the fake
   regression test with one that drives the actual collect→mutate→delete
   interleaving (expose the collect/delete phases as separable internal
   functions for testing).
2. **B4** — document delete cascades edge cleanup: the `write_document` delete
   branch removes forward and reverse edges for the key in the same
   transaction (endpoint-prefix scans of the edge namespaces);
   `link`/`unlink` emit `ChangeEvent`s. Tests: delete a linked doc →
   neighbors/in_neighbors/traverse exclude it and the edge rows are gone;
   subscribers see graph events.
3. **B3 (decision 2)** — snapshot-scoped execution: introduce a `Snapshot`
   read handle (`store().read(...)` closure scoped for the query); plumb it
   through `run()`, `run_scan_only`, all aggregations (`for_each_match`),
   `verify_candidates`, `ann_candidates`/`text_candidates` fetch loops, and
   `graph::traverse`. `Collection` gains snapshot read variants
   (`get_in`, `for_each_doc_in`, …); index window scans execute within the
   same read transaction. Tests: a writer mutating docs between candidate
   fetch phases cannot produce a set unmatched by any snapshot (deterministic
   interleaving test); existing parity tests stay green.
4. **B7** — phrase fallback scores BM25 over the corpus it already scans
   (matching the indexed path's scale, replacing raw occurrence counts);
   builder re-ranking keeps candidate-subset statistics but its docs state
   this explicitly (it is the documented "rank the filtered set" semantics).
   Parity test: the same no-filter phrase/text query returns the same order
   via `phrase_search`/`text_search` and the builder, with and without an
   index registered.
5. **B8** — `dump` takes one read snapshot (stream per collection within it);
   `load` streams the file (bounded read buffer) and validates reserved names
   on every replay path (index defs, schemas included);
   `create_scalar_index`/`set_schema`/all `create_*` call `ensure_writable`.
6. **B10** — `In`-union honors the same aggregate CAP as OR-union (falls back
   to scan past the cap).
7. **C7** — name validation at API entry: reject NUL bytes in
   collection/field names and embedded `__` sequences that could forge
   namespaces (typed `Error::InvalidName`). Applied in `Db::collection`
   write paths and index creation.
8. **C9** — `insert_auto` reserves the id inside the insert transaction (move
   `next_auto_id` into the write path); `len()` saturates
   (`try_into().unwrap_or(usize::MAX)`).

## Wave 5 — docs, claims, tests, CI (D1-D6, C2-C6, C8, C11, C12, C14)

- **D1** README + ci.yml: add an `aarch64-apple-ios` cross-compile job (same
  shape as the Android job) so the ✅ row is real.
- **D2** README: qualify the true-predicate claim with the `.approx()`
  opt-out (linking the rustdoc).
- **D3** store.rs/db.rs module docs rewritten to the post-a0bffbb truth
  (persisted indexes commit inside the document transaction; in-memory
  indexes are post-commit derived state).
- **D5** DESIGN.md: fix the layer map (shipped L5 features), mark
  zstd/tracing/bumpalo/zero-alloc as unimplemented future specs (or delete),
  make decision-log dates real (append a 2026-08-27 entry for this
  remediation's decisions 1-6), close open-question #3.
- **D6** AUDIT.md: rewrite as a dated re-audit status doc — fixed/open tables
  at the new HEAD, each wave's landing updates it.
- **C3** `explain()` prints the plan shape the planner will take (ANN index /
  text index / scalar-geo window / streaming top-k / scan), not unconditional
  `scan(...)`.
- **C4** `order_by` implements the documented promise: missing *and*
  incomparable values sort last (stable by key); tests updated.
- **C5/C12** document precision limits where they live (value.rs/filter docs
  for i64 > 2^53; convert.rs for u64→float and non-finite→null round-trip).
- **C6** validate RRF k (> 0), MMR λ (∈ [0,1]), Bm25Params (b ∈ [0,1], k1 ≥ 0)
  with a typed `Error::InvalidArgument`; tests for each rejection.
- **C2** antimeridian bbox: implement wrap (two longitude ranges) with
  validation of lat ∈ [-90,90], lon ∈ [-180,180]; tests at the wrap line.
- **C8** backup: create-exclusive file creation (kills the TOCTOU);
  on mid-copy failure, best-effort remove the partial destination.
- **C10** replace weak index tests with parity/guard tests: force the index
  path observably (e.g. assert the index served via an instrumentation hook
  or `explain()` output from C3) and compare results against exact-path runs;
  make `custom_rrf_constant_is_accepted` observe the constant (two different
  constants produce different orderings on a crafted ranking).
- **C11** ci.yml: `timeout-minutes` + `concurrency` group; release.yml:
  tag↔version check, checksums (sha256), create the release only after all
  builds succeed.
- **C14** document stop-word position semantics in `phrase_search` docs
  (phrases match across removed stop words); stemmer misses listed as known
  limitations in the guide.
- **B9 (mitigation, not rewrite)**: document the global-index-lock
  availability property honestly (a build/compaction blocks vector search
  db-wide) in DESIGN.md; a per-collection lock split is explicitly deferred
  as a future decision — recorded in the decision log, not silently dropped.

## Cross-cutting standards (every wave)

- Test-driven development; every fix lands with the test that fails first.
- `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`
  green before each commit; commits are small and self-contained on `master`.
- Coverage ≥ 90% line coverage maintained (`cargo llvm-cov --fail-under-lines 90`).
- Perf-sensitive changes (creation, compaction, snapshot `run()`) carry a
  criterion benchmark; regressions block the change.
- Format/data-model changes ship the dump/load update in the same change.
- Every non-obvious decision lands in DESIGN.md's decision log with real
  dates; AUDIT.md status rows update as waves land.

## Risks and mitigations

- **Snapshot plumbing breadth (wave 4)** touches most of builder.rs —
  mitigated by landing after waves 2-3 (no double rebase), parity tests, and
  keeping `Collection`'s existing methods as thin wrappers over the snapshot
  variants.
- **Cursor-vs-maintenance overlap (wave 2)**: index upserts must be idempotent
  — audited as true for all five kinds; tests pin it.
- **Lazy resume under lock (wave 2)** reuses the proven build-under-lock
  discipline; the availability note (B9) is documented rather than fixed.
- **Group-key format change (wave 1)** breaks any consumer string-matching
  `s:`-prefixed keys — acceptable pre-1.0; called out in the CHANGELOG.
