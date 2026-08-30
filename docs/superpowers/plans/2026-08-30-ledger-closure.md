# Ledger closure program — finish the job

Date: 2026-08-30 · Status: BINDING · Controller session.
Goal: after this program, every item on every ledger (AUDIT Open,
DESIGN future, session TODOs) is either SHIPPED or carries an explicit
recorded decision (do-never / defer-with-trigger) with rationale. Nothing
sits "unprioritized."

Process unchanged: per task fresh implementer → independent reviewer → at
most one fix wave → wave-exit review. Gates per commit: `cargo fmt --all`;
`cargo clippy --all-targets --workspace -- -D warnings` (plus
`-p corvid --features tracing` variant); `cargo test --workspace`;
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`.
Standing guards: strict surface radar green (new pub surface lands with
manifest rows + conformance tests + SYNTAX.md regen); no bench regresses
>5% without ratification; bugs RED-first; docs stay true (DESIGN decision
rows, AUDIT flips, CHANGELOG); BENCHES.md updated for any perf claim.

## Task 1: Endpoint-direct neighbors/traverse via adjacency

`neighbors`/`in_neighbors`/`neighbors_weighted`/`traverse` still range-scan
the edge namespaces by (relation, endpoint) prefix; the adjacency
namespaces (Task 7 of the roadmap program) already key by endpoint. Serve
these reads from adjacency where semantics stay EXACTLY identical
(ordering! relation filtering! weights! dedup semantics!) — walk the
adjacency rows for the endpoint, then fetch edge rows only for the
relations requested. If exact-order preservation forces extra sorting that
eats the win, bound the change to `traverse`'s frontier expansion (where
order feeds BFS determinism — CAREFUL: traverse order is pinned) or
decline with measurements. Bench: add `neighbors_hub_10k` before/after.
AUDIT row (endpoint-direct reads) flips Fixed or gets the measured
verdict.

## Task 2: Distance-kernel closure (SIMD item)

The AUDIT row claims SIMD headroom. Close it honestly: benchmark the
kernels against the machine's memory-bandwidth ceiling (768d f32 = 3 KiB
read; measure effective GB/s vs a memcpy/streaming baseline); check
whether the compiler already auto-vectorizes the lane folds (inspect
assembly or measure chunk-size scaling); attempt bounded scalar
improvements ONLY if the numbers show compute-bound (not bandwidth-bound)
behavior. Deliverable: numbers + verdict in BENCHES.md; AUDIT row flips
Fixed with the outcome (likely "bandwidth-bound; quantized scans are the
real lever; no stable-Rust SIMD without unsafe/nightly" — but the
measurements decide, not the assumption). No unsafe, no nightly.

## Task 3: Sketches trio

Prepend (Task 2 review wording fixes, binding): BENCHES.md verdict — (a)
"sits inside the streaming band" → "at/just below the low end (41-42 vs
43.3-57.9 GB/s)"; (b) "exact-value kernel tests" → name the actual
load-bearing pins (PQ recall floor, hnsw/disk recall corpora, exact-eq
reproducibility asserts). — cuckoo filter, t-digest, MinHash+LSH

Three bounded data structures joining Bloom/HLL as public surface
(conventions from sketch.rs; conformance conventions from tests/pq.rs and
the manifest discipline):
- CuckooFilter: new(expected, fp), add_bytes/contains_bytes, victim-slot
  semantics documented, no-false-negatives pinned, fp bound justified.
- TDigest: new(compression), add(f64), merge, quantile(0..1)/cdf —
  exactness pins on small corpora, monotonicity, boundary quantiles
  (0.0/1.0), NaN rejection pinned.
- MinHash + LSH: MinHash signature (k hash functions via seeded
  permutations), jaccard_estimate with justified bound on fixed sets;
  LSH banding bucketing (bands/rows parameters) with candidate-pair
  recall/skew pins on a fixed corpus.
Each: pub surface → manifest rows + conformance tests + SYNTAX.md regen.
DESIGN future row flips.

## Task 4: CJK bigram analysis

Prepend (Task 3 review hardening, binding): (a) LSH recall pin: add the
per-test fragility note AND slack (≥9 of 10 twin pairs or more pairs — the
current all-10 assertion carries ~0.7% hasher-redraw exposure); (b) add
the delete-half-then-verify-all-survivors cuckoo pin; (c) fix the LSH
comment arithmetic (180 dissimilar unordered pairs, ordered-link count
≈0.115); (d) DESIGN note: TDigest's Clone derive is need-driven
(merge-commutativity clones); Bloom/HLL stay bare — recorded decision, no
symmetry churn.

text.rs analyzer extension: when text contains CJK codepoints, tokenize by
bigram (standard CJK segmentation fallback for search); no external deps;
mixed CJK+latin strings handled (latin words tokenized as today, CJK runs
bigrammed); phrase_search positions stay consistent (adjacent bigrams).
Conformance: ranking + phrase tests on CJK corpora; stemming not applied
to CJK. DESIGN future row flips.

## Task 5: zstd value compression (feature-gated)

Prepend (Task 4 review nits, binding): (a) correct task-4-report.md and
commit-message attribution: the indexed ~8% reading is within historical
variance, not an improvement (the indexed bench re-tokenizes only the
ASCII query — no mechanism); (b) DESIGN boundary note gains the
U+3005/U+3006/U+3031-3035 class (iteration marks split CJK runs;
index/query self-consistent); (c) BENCHES.md one-line note on the
+2.5-3.2% bm25_exact residual (CJK-aware tokenize costs on the latin
bench, inside guard).

Optional cargo feature `zstd` (default OFF; `zstd` crate via FFI — same
discipline as tracing: default/wasm graphs stay clean, CI greps enforce,
feature-on gates in CI). Compression at the value layer: threshold-sized
values compressed on write, transparently decompressed on read; on-disk
format marker per value (self-describing; old rows read fine). Round-trip
every Value variant; ratio recorded for representative docs in BENCHES.md;
the feature is opt-in so no default-build behavior change. DESIGN row
flips (was "specified, not implemented").

## Task 6: Decision ledger + program exit

Every remaining DESIGN-future item gets an explicit controller decision
recorded in DESIGN.md's decision log (table row or dated entry):
- Browser/OPFS: KEEPS its 2026-05-29 deferral (desktop/server focus
  stands; reopening needs product signal, not engineering appetite).
- DiskANN: defer with trigger (on-disk HNSW measurably insufficient on a
  real workload).
- Filtered-HNSW pushdown: defer with trigger (filtered ANN becomes hot).
- Materialized views: defer with trigger (needs API/product intent;
  subscriptions cover the reactive case today).
- Embedding pipeline: defer (model dependency policy undecided; users
  embed today).
- Streaming ranked cursors: defer with trigger (workload with ranked
  sets too large to materialize).
- Cost-based planner: defer (selectivity probing serves current scale).
- Time-series patterns: defer with trigger.
- R-tree/H3: defer (grid cells serve current geo workload).
- Arrow RecordBatch: DECLINE (heavy dependency against the embedded
  posture; users convert at the boundary).
- JSON path: DECLINE (dotted paths + select cover the surface; a path
  language adds SQL-ish surface area).
- UDFs: deferral past v0.1 stands.
- bumpalo arenas: DECLINE (hot paths are bandwidth-bound per Task 2's
  measurements — arena allocation buys nothing the numbers support).
- Buffer pool: defer with redb (its page cache serves the layer).
- Per-collection lock split: defer with trigger (build-vs-search
  contention observed on a real workload).
- Tensor type: L4+ deferral stands (general tensor ops are a non-goal).
- Owned page format: never (documented seam; only if redb is replaced).
- MCP SDK framing: defer (hand-rolled stdio is tested and working; SDK
  maturity unmotivated).
Each entry: one-line rationale + trigger. Then: AUDIT/DESIGN/README
reconciliation sweep, CHANGELOG, BENCHES.md final table, full gates both
feature configs, radar/SYNTAX current, final whole-branch review, push,
CI green.
