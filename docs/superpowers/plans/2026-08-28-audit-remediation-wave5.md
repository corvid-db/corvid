# Audit Remediation Wave 5 — Docs, Claims, CI, Test Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every remaining doc-claim, CI, and test-hygiene finding (D1-D6, C2-C6, C8, C10, C11, C14) plus the deferred minors routed here from waves 1-4, and leave the project's public claims exactly true.

**Architecture:** Four implementation tasks (engine semantics, explain/tests, CI/release, docs) plus the exit task. No storage or transaction changes — validation, geo wrap, backup exclusivity, docs, and CI wiring.

**Tech Stack:** Rust 2024, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-27-audit-remediation-design.md` — Wave 5 section. Read before starting.

## Global Constraints

- Gates per commit: fmt, clippy `-D warnings`, `cargo test --workspace`; TDD for behavior changes.
- Typed errors (`Error::InvalidArgument` — add to error.rs); never panic on user input.
- Coverage ≥ 90% at exit.
- Docs changes must be verified against code (no claim lands without a check).
- Line anchors from `cd14da0`.

---

### Task 1: Engine semantics — validation, geo wrap, backup hardening, deferred guards

1. **C6 validation**: `fuse_rrf(k)` → `Error::InvalidArgument` if `k <= 0.0` or NaN; `rerank_mmr(lambda)` if `!(0.0..=1.0).contains(&lambda)` or NaN; `Bm25Params` construction (text.rs — add a validated constructor or validate where params are consumed) if `b` outside `[0,1]` or `k1 < 0`. Tests for each rejection + acceptance boundaries.
2. **C4 order_by**: implement the documented promise — missing AND pairwise-incomparable values sort LAST (stable by key). Today incomparable values interleave by key (`unwrap_or(Equal)` at the comparator arms). Change both comparators (builder.rs `run_with`'s order arm + `sort_by_field`): rank rows by a three-way class (comparable-present < incomparable-present < missing), compare within class as today. Tests: mixed Int/Text/Bool field → comparable group ordered, incomparable group after it, missing last; descending reverses value order but NOT the class order (missing/incomparable always last — decide and document: classes stay fixed, values within the comparable class reverse). Update the doc comments to state the class rule.
3. **C2 antimeridian**: `geo_within_bbox` + the indexed path: when `min_lon > max_lon` (wrapped box), match `lon >= min_lon || lon <= max_lon` (two ranges); validate lat ∈ [-90,90], lon ∈ [-180,180] at entry (`Error::InvalidArgument` otherwise). The geo-index candidate path already falls back to scan on wrap (geo_index returns None) — keep; make the verification wrap-aware. Tests: docs at 175° and -175°, box (10,170)→(20,-170) → both matched (fails today: silent empty).
4. **C8 backup**: `Store::backup` — create-exclusive (open the destination with `OpenOptions::new().write(true).create_new(true)` equivalent for redb… redb's `Database::create` opens-or-creates; instead: after the exists() check, also do a best-effort `remove_file` on ANY error path before returning Err (cleanup debris), and narrow the TOCTOU by re-checking via the create path's behavior — practical fix: keep the exists() refusal, add debris cleanup on mid-copy failure (wrap the copy in a closure; on Err, best-effort std::fs::remove_file(path), then return the original error). Document the residual create-race as accepted (single-process embedded). Tests: failing backup (corrupt source read — simulate via a poisoned table? simplest: backup onto a path whose parent is read-only → Err → no debris file left; and successful backup twice → second refused, target intact).
5. **Deferred guards**: (a) FTS OnDisk registry-lag silent-empty — mirror the ANN empty-result re-check in `fts_search_in`/`fts_phrase_search_in`'s OnDisk arms (empty ranked + def row says Building → `Ok(None)`); (b) `want = ef_search.max(k) + dead` → `.saturating_add(dead as usize)`; (c) graph twin-deletion assertion added to the wave-4 cascade test (assert the reverse twin of an a→c edge is gone). Tests for (a): mirror index.rs's registry-lag test shape for text.
- Commit: `engine: argument validation, antimeridian bbox, order_by classes, backup hardening, deferred guards (audit C2/C4/C6/C8)`

### Task 2: explain() real plans + test hygiene (C3, C10)

1. **C3**: `explain()` prints the plan shape the planner will actually take. Reuse the planner's own decision logic: factor a `plan_shape(&self) -> PlanShape` (enum: AnnIndex{field}, TextIndex{field}, IndexedWindow{kind}, StreamingTopK, Scan) from the same conditions `run_with` uses (single-source + index consultable + approx rules + filter/indexed gates — call the same predicate fns where cheap, or duplicate their conditions with a comment tying them). `explain()` renders it; `run_with` is UNCHANGED (explain is advisory). Tests: each shape rendered for a query that takes it (assert the string contains the right arm — and cross-check one query's explain against its actual path via the C10 instrumentation below).
2. **C10**: replace the weak index tests: (a) `builder_uses_ann_index_for_vector_only_query` etc. gain an observable index-served check — add a `#[cfg(test)] pub(crate) thread_local served_via_index: Cell<usize>` counter bumped where ann/text/indexed candidates serve (reset per test), assert it increments; (b) `custom_rrf_constant_is_accepted` observes the constant — construct two rankings whose fused order differs between k=1 and k=60 (e.g. rank sets {[a:1,b:2],[b:1,a:3]}), assert different orders for two k values. (c) Record the literal bench invocation (`cargo bench -p corvid --bench engine -- index_creation_ondisk`) in a comment by the bench.
3. Pin the explain/plan-shape conditions to the planner with a parity test: for a matrix of query shapes (filtered/unfiltered × single/multi-source × approx), `plan_shape()` predicts the path `run_with` took (via the served counter).
- Commit: `builder: explain() reports the real plan shape; index-path tests observe service (audit C3/C10)`

### Task 3: CI + release (C11, D1)

- ci.yml: add `aarch64-apple-ios` cross-compile job (mirror the Android job: `cargo build -p corvid --target aarch64-apple-ios --release`); add `timeout-minutes: 20` to every job; add a top-level `concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: true }`.
- release.yml: builds BEFORE the release step (reorder — release created only after all matrix builds succeed); tag↔version check (extract version from Cargo.toml, compare to the tag, fail on mismatch); sha256 checksums (`shasum -a 256` per artifact, uploaded as an extra artifact); scope `permissions: contents: write` to the release job only.
- README.md:113 mobile row stays ✅ (now true); verify the wasm row's claims still hold (CI enforces 2MB; ≈0.2MB figure: either measure once in the workflow output and cite the number, or soften to "well under the 2 MB CI budget" — pick soften if measuring isn't in the job).
- Commit: `ci: iOS job, timeouts+concurrency; release: build-first, tag check, checksums, scoped permissions (audit C11/D1)`

### Task 4: Docs reconciliation (D2-D6, C5, C12, C14, deferred doc notes)

Every change verified against code:
- README:56-57: qualify the true-predicate claim with the `.approx()` opt-out (link the rustdoc); confirm the capability table rows against HEAD (fix any drift — e.g. the quantization row's "≈4×" asymptotic note if absent).
- store.rs module doc (lines ~17-21): rewrite to the post-a0bffbb truth (persisted indexes commit inside the document transaction; in-memory indexes are post-commit derived state rebuilt lazily; creation state machine). db.rs Db doc likewise.
- DESIGN.md: layer map line ~76 (L5 features shipped — fix the "empty traits" sentence); zstd/tracing/bumpalo/zero-alloc marked as future-specs-not-implemented (or removed); open-question #3 closed with a pointer to the decision log; decision-log dates: add an honest note that early rows were backdated to the project's start date (do NOT rewrite history rows; add the note + ensure 2026-08-27/28 rows carry real dates — verify they do); decision 6 wording: "all NaN bit patterns compare equal for uniqueness (payload-insensitive)" to match the implementation; B9 mitigation note (global index mutex + registration-clear availability + graph cascade O(E) — the three known availability characteristics, each with its follow-up); the deferred-notes: page/page_where per-chunk snapshots, geo_nearest per-radius snapshots, bulk-panic flush skip, a__b dump→load migration failure callout.
- Precision docs: value.rs/filter.rs doc note (i64 compared via f64 beyond 2^53 — nanosecond timestamps affected); convert.rs doc note (u64 > i64::MAX → lossy float; non-finite → JSON null).
- C14: phrase_search docs state stop-word-collapsed positions ("quick the brown" matches "quick brown"); GUIDE.md known-limitations note for the S-stemmer (boxes≠box).
- rustdoc cleanup: remove the "(spec decision 3)" parentheticals from builder.rs group-key docs (builder.rs:238/341/360 area).
- AUDIT.md (D6): rewrite as a dated re-audit status doc: a header stating the 2026-08-27/28 remediation landed (waves 1-5), a fixed-table (all A/B/C/D findings → commit hashes), an open-table (the surviving deferred items with their follow-up destinations: endpoint-indexed edge layout, page-level single-snapshot, in-memory PQ, tracing/observability), and keep the original audit text below a "historical" marker.
- CHANGELOG: verify every user-visible change across waves 1-5 has an entry (audit the Unreleased section against the wave commits; add any missing).
- Commit: `docs: reconcile all claims with the shipped surface; AUDIT rewritten as status doc (audit D2-D6, C5/C12/C14)`

### Task 5: Wave exit + final verification

1. Full gates + llvm-cov ≥ 90.
2. Docs-claims spot audit: one fresh read of README/GUIDE/DESIGN against HEAD (any claim that regressed during waves 4-5?).
3. Final CHANGELOG check.
4. DESIGN decision-log row for wave 5 + AUDIT wave-5 block.
5. Commit: `wave-5 exit: gates green, claims audit clean (D1-D6, C2-C6, C8, C10, C11, C14 fixed)`
