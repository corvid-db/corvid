# Roadmap execution program — waves 1-4 (perf, graph layout, format, depth)

Date: 2026-08-29 · Status: BINDING · Controller session.
Scope decided with the user: AUDIT Open items A1-A7, perf items C1-C4,
process chores D1-D5. DESIGN.md "future" subsystems (B) are OUT of scope —
they get a separate prioritization session. Permanent non-goals stay never.

Process: same as the two prior programs — per task: fresh implementer →
independent reviewer → at most one fix wave (5-round cap) → wave-exit
whole-branch review. Gates per commit: `cargo fmt --all`;
`cargo clippy --all-targets --workspace -- -D warnings`;
`cargo test --workspace`;
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`.
Standing guards on every task:

- **Surface radar stays green** (strict; new pub surface must land with
  manifest rows + conformance tests; SYNTAX.md regenerated).
- **Bench rule**: no existing bench regresses beyond noise (>5%); the new
  Wave-1 benches are the acceptance numbers for W2/W3 perf claims — no
  "faster" claim without a before/after table in the task report.
- **Bugs found are Ruling-5**: RED test first, fix in-task, CHANGELOG.
- Docs stay true: DESIGN.md decision-log row + AUDIT.md Open→Fixed flip
  for every shipped item; CHANGELOG for user-visible changes.
- The two prior programs' conventions bind (unique test names, non-citable
  radar self-tests, shared error-variant citations, equality-per-construct).

## Task 1: Ledger + chores

- AUDIT.md Open table gains the four perf items (parallel build/search,
  verify-candidates batching, sort indexes, SIMD kernels) with rationale
  and follow-up destination, phrased to match existing rows.
- `.gitignore` gains `.mimosa/` (scan-history dir of a local tool).
- Bump `actions/checkout` to the current major in all workflows (silences
  Node-20 deprecation annotations); verify CI green on push.
- Commit message records current bench numbers as the program baseline.

## Task 2: Bench shapes for the deferred paths

Three new criterion benches in `crates/corvid/benches/engine.rs` (or a
second bench file if engine.rs grows unwieldy — keep group naming stable):
1. `edge_churn` — link/delete-heavy graph workload (the O(E) cascade
   path): N docs, E edges, delete sweep; measures the delete cascade cost
   that Task 7 will attack.
2. `compound_prefix_scan` — prefix-only equality on a populated compound
   index (the declined window): measures the scan Task 6 re-enables.
3. `delete_heavy` — mixed insert/delete churn with scalar+unique indexes
   (CAS decode + unique re-verify + edge purge cost from the conformance
   program's fixes).
Deterministic corpora, sample sizes bounded like `index_creation_ondisk`.
Run and record baselines in the task report.

## Task 3: Release workflow dry-run

Prepend (Task 2 review nits, binding): in benches/engine.rs edge_churn: fix
the hub-count comment (the `i % 4 == 0` guard makes it 4 hubs of ~625, not
16 of ~250 — either correct the comment or change the corpus to draw hub
targets without the multiple-of-4 coincidence, pick one and re-run that one
bench to refresh the baseline number); note the ~9k-distinct-edges fact in
the report if you touch the corpus.

- Read `.github/workflows/release.yml`. If it has no `workflow_dispatch`,
  add it with a `dry_run` input (default false) that runs the full
  build/checksum matrix but skips the publish/upload step(s).
- Dry-run remotely via `gh workflow run` and watch to green (or, if the
  runner lacks a dispatch path, validate by pushing a `v0.0.0-dryrun`-style
  tag ONLY if the workflow is provably non-publishing for prerelease tags —
  read it first; if it would publish, the workflow_dispatch route is
  mandatory).
- Record the outcome in the task report; delete any dry-run tag/release
  artifacts afterward.

## Task 4: Verify-candidates batching

`QueryBuilder::verify_candidates` (builder.rs) fetches each candidate key
with an individual `get_in`. Batch it: one ordered range/prefetch read per
window when the SnapshotReader supports it (scan_from/scan_prefix exist),
preserving exact semantics (same rows, same order, same filter
application, same snapshot scope). Before/after numbers on a new or
existing bench that exercises a selective scalar window (add one if none
exists — e.g. `selective_window_verify`). All conformance suites green
(filters.rs indexed-vs-scan twins are the correctness oracle).

## Task 5: Sort indexes (order_by via scalar index)

When `order_by(field)` matches a scalar index on that field and no
filter/limit/offset semantics are violated, serve the order from the index
walk instead of an in-memory unbounded sort. HARD constraint: the total
order contract (comparable kinds, then incomparable by kind tag, then
missing; ties by key; descending reverses within-class order — pinned by
queries.rs) must hold on the index path. The index only contains
encodable comparable kinds, so the implementation must prove the union
with non-indexed docs (missing/incomparable) still yields the documented
order — if it cannot be made provably identical, serve the index order
only when it demonstrably covers the full result (e.g., no missing-field
docs exist — cheaply checkable per-corpus? if not provable, decline).
Decision rule: identical results or decline — never approximate.
New bench `order_by_indexed_5k` before/after. Parity tests on both paths
across the full kind lattice (extend queries.rs).

## Task 6: Compound prefix windows — sound re-enable

Per-def metadata `all_docs_indexed` on the compound index def (persisted
in the def record): maintained true on creation backfill (every doc has
all fields encodable) and flipped false permanently on any insert/update
that leaves a field missing/non-encodable (never re-flipped without a
rebuild — compaction/re-registration recomputes it). The planner probe and
its plan_shape twin admit prefix-only windows only when the flag is true.
Conformance: the W2 soundness pins stay green (they assert decline on
corpora with missing fields); new tests assert service on all-present
corpora; `compound_prefix_scan` bench shows the win. AUDIT Open row flips
to Fixed with the mechanism named.

## Task 7: Adjacency edge layout (derived indexes)

Deletes/purges currently scan both edge namespaces per delete (O(E)).
Design (controller ruling, binding unless the implementer proves it
unsound): add endpoint-keyed ADJACENCY structures as derived state —
rebuilt lazily from the source-of-truth edge rows on first use after open,
maintained transactionally in link/unlink/cascade. No change to the edge
row format itself (no format migration — non-goal respected); adjacency
rows live in their own private namespaces and are invisible to user scans,
collections(), and dump (dump already writes only source namespaces —
verify). Cascade then finds the doc's edges via adjacency in
O(edges-of-doc). Fallback: if adjacency is absent/stale-shaped, current
scan path remains as the correct fallback. `edge_churn` bench before/
after. Conformance: graph.rs suite green untouched (public behavior
identical); add a test proving adjacency rebuild after reopen.

## Task 8: `a__b` migration tooling

Pre-wave-4 dumps with `__`-containing collection names fail at load.
Ship a rename mechanism: `Db::load` gains an optional rename map (or a
sibling API `load_with_renames`) — engine-level, tested; plus a small CLI
path in corvid-mcp or a binary flag? Ruling: engine API + documented
recipe in DESIGN.md (migration guide section); MCP `load` tool gains an
optional `rename` object param if cheap and consistent. AUDIT row → Fixed.

## Task 9: Dump format v2 (u64 sections)

Bump the dump format to v2 with u64 length prefixes (loader accepts v1
and v2; dumper writes v2). Round-trip tests: v2 dump→load; v1 fixture
load (craft a small v1 dump by hand/bytestring in the test); boundary
encodings (lengths near u32::MAX encoded correctly — crafted byte-level,
no multi-GB fixtures). CHANGELOG + DESIGN format section updated. AUDIT
row → Fixed.

## Task 10: In-memory PQ

Wire `pq.rs` into the in-memory HNSW as a `Quantization` option (new
variant or flag — public API addition → manifest rows + conformance
tests: train/encode/search determinism, recall-vs-exact on fixed corpora
with justified bounds, all three metrics or documented L2-only if that is
the honest scope). DESIGN.md deferral note updated (in-memory PQ row
moves out of deferred; ADC non-L2 remains deferred if unshipped).
Bench: in-memory build/search with PQ vs None (footprint claim needs a
memory proxy — e.g., size_of heap estimate or documented reasoning).

## Task 11: tracing (feature-gated)

`tracing` as an OPTIONAL cargo feature (default-off; must not enter the
default build, the WASM budget, or the zero-dep posture — CI verifies
`cargo build --no-default-features` and the wasm job stays green).
Instrument: index builds/backfill pages, compactions, lazy resumes, query
plan selection, edge-cascade fallback. DESIGN Observability section
updated from "specified-not-implemented". AUDIT row → Fixed (counters
subsumed or explicitly left future).

## Task 12: Page-level single-snapshot

`page`/`page_where` execute their whole walk inside ONE read transaction
(chunked reads INSIDE the txn to preserve bounded memory). Conformance:
existing page tests green; new test proves page-consistency under an
interleaved writer (channel/thread — deterministic ordering, no sleeps).
AUDIT row → Fixed.

## Task 13: Parallel HNSW build (+ PQ training)

Parallelize in-memory HNSW construction and PQ k-means with std scoped
threads (no new deps) OR rayon — implementer picks and justifies in the
report (bundle-size and MSRV constraints). HARD constraint: build
determinism must hold bit-for-bit (conformance tests rely on deterministic
corpora; if parallel insertion changes graph shapes, seed/level
assignment must stay deterministic and the report must prove recall
equivalence on fixed corpora — hnsw_build bench before/after is the
number). Registry-lock interplay: parallel build must not hold the global
registry mutex longer than today (it currently serializes builds anyway —
parallelism is INSIDE one build).

## Task 14: Program exit

Full gates; all benches recorded (before/after tables consolidated);
SYNTAX.md regenerated if surface grew; CHANGELOG complete; AUDIT.md
Open→Fixed flips verified; DESIGN.md decision-log rows for every shipped
item; final whole-branch review; ledger close; push.
