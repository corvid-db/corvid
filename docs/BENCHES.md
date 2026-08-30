# corvid benchmarks — the durable record

Every before/after number of the 2026-08-29 roadmap-execution program
(waves 1-4), committed in-repo because the per-task reports and the
wave-1 baselines live in git-ignored workspace dirs — this file is the
durable record. The program's standing bench rule binds future work: no
existing bench regresses beyond noise (>5%), and no "faster" claim
without a before/after table whose provenance is stated here.

**Machine:** Apple M1 Max (MacBookPro18,2), Darwin arm64, 32 GiB,
macOS 26.5.2. **Toolchain:** rustc 1.91.1 (ed61e7d7e 2025-11-07);
MSRV 1.88. **Method:** `cargo bench -p corvid --bench engine`
(criterion, bench profile, in-memory `Db`, deterministic corpora —
seeded xorshift/index math, no `rand`). Numbers are criterion **means
with 95% CI** unless a table says median. Single-machine numbers:
compare relatively, not absolutely.

## Invocation (audit C10 convention — literal commands)

```text
cargo bench -p corvid --bench engine                 # full suite
cargo bench -p corvid --bench engine -- codec        # value_encode/_decode
cargo bench -p corvid --bench engine -- hnsw         # build/search, None + PQ
cargo bench -p corvid --bench engine -- pq_train     # parallel k-means training
cargo bench -p corvid --bench engine -- text         # bm25 exact vs indexed
cargo bench -p corvid --bench engine -- distance     # dot/l2/cosine 768d
cargo bench -p corvid --bench engine -- index_creation_ondisk
cargo bench -p corvid --bench engine -- edge_churn
cargo bench -p corvid --bench engine -- compound_prefix_scan
cargo bench -p corvid --bench engine -- delete_heavy
cargo bench -p corvid --bench engine -- selective_window_verify
cargo bench -p corvid --bench engine -- order_by_indexed_5k
cargo bench -p corvid --bench engine -- neighbors_hub_10k
cargo bench -p corvid --bench engine -- kernel_scaling
cargo bench -p corvid --bench engine -- declined_probes
cargo bench -p corvid --bench engine -- bandwidth
cargo bench -p corvid --bench engine -- kernel_stream
cargo bench -p corvid --bench engine -- quantized_scan
```

The same literal commands live in the bench file's doc comments (one per
group, where the group was added).

## Program baseline (Task 1, 2026-08-28/29) — the pre-existing benches

Recorded at `54ce9da` before any perf work started; these are the
acceptance numbers for every "unchanged" claim below. **Medians** from
`target/criterion/*/new/estimates.json`:

| Bench | Median | 95% interval |
|---|---|---|
| value_encode | 387.2 ns | [386.20, 388.35] ns |
| value_decode | 824.4 ns | [822.60, 829.91] ns |
| hnsw_build_2k_64d | 119.2 ms | [118.96, 119.78] ms |
| hnsw_search_2k_64d | 17.70 µs | [17.687, 17.868] µs |
| bm25_exact_2k | 7.643 ms | [7.6203, 7.6779] ms |
| bm25_indexed_2k | 486.0 µs | [484.15, 488.67] µs |
| dot_768d | 76.50 ns | [76.533, 76.903] ns |
| l2_768d | 77.69 ns | [77.745, 78.422] ns |
| cosine_768d | 214.3 ns | [214.56, 216.47] ns |
| index_creation_ondisk/create_text_index_ondisk_5k | 251.0 ms | [250.60, 252.29] ms |
| index_creation_ondisk/create_vector_index_ondisk_2k_8d | 392.7 ms | [392.02, 393.87] ms |

## Wave-1 bench shapes (Task 2, `3479f90`) — baselines for the deferred paths

Three new groups measuring the then-declined paths; these BEFOREs are
what Tasks 6/7 had to beat (means [95% CI]). The edge_churn corpus
comments were corrected at `6b88334` (hub counts), corpus unchanged:

| Bench | BEFORE (mean) | 95% CI |
|---|---|---|
| edge_churn/edge_link_10k | 273.5 ms | [272.3, 275.0] |
| edge_churn/edge_delete_sweep_100 | 241.0 ms | [239.2, 243.5] |
| compound_prefix_scan/eq_leading_only_5k | 1.540 ms | [1.534, 1.547] |
| delete_heavy/insert_unique_scalar_5k | 177.0 ms | [176.5, 177.6] |
| delete_heavy/delete_half_2p5k | 566.8 ms | [564.7, 569.2] |

## Task 4 — verify-candidates batching (`86fbfd2`)

Dense indexed windows verify candidates with ONE ordered walk of the
records instead of per-key point-gets (density pick:
`candidates × 17 ≥ collection count`). Rows, order, filters, snapshot
identical either way.

**BEFORE provenance:** the bench did not exist at the base, so BEFORE was
produced by backporting the bench file (unmodified, implementation-less)
to the pre-Task-4 base (`86fbfd2^`) in a throwaway worktree and running
it there — the convention Task 5 reuses.

| Bench | BEFORE | AFTER | Δ |
|---|---|---|---|
| selective_window_verify/eq_50_of_5k (1% density) | 221.21 µs | 229.82 µs | +3.9% (point-gets kept — the count-read cost; within guard) |
| selective_window_verify/eq_500_of_5k (10% density) | 745.14 µs | 576.25 µs | **−22.7%** |

## Task 5 — sort indexes (`1cdb558`)

A filterless `order_by(field)` with a complete scalar index is served by
an index order walk (`PlanShape::SortIndex`); results identical by
construction or decline.

**BEFORE provenance:** bench file backported to base `c691f3a` in a
throwaway worktree (same convention as Task 4).

| Bench | BEFORE | AFTER | Δ |
|---|---|---|---|
| order_by_indexed_5k/asc_limit20 | 2.642 ms | 289.5 µs | **−89.0%** |
| order_by_indexed_5k/desc_limit20 | 2.795 ms | 1.024 ms | **−63.4%** |

## Task 6 — compound prefix-only windows (`43c0d8a`, fix `17e2266`)

Re-enabled soundly via the per-def `all_docs_indexed` flag. BEFORE = the
Task 2 baseline. The recorded AFTER is the fix-wave re-run at `17e2266`
(the first measurement at `43c0d8a` was 240.65 µs; the fix's added
miss-check never fires on this all-present corpus — the delta is noise):

| Bench | BEFORE | AFTER | Δ |
|---|---|---|---|
| compound_prefix_scan/eq_leading_only_5k | 1.540 ms | 232.41 µs [232.06, 232.76] | **−84.9% (6.6×)** |

## Task 7 — adjacency edge layout (`a0d37f8`), incl. the ratified link trade

O(degree) delete cascades via derived endpoint-first adjacency
namespaces. BEFORE = Task 2 baselines:

| Bench | BEFORE | AFTER | Δ |
|---|---|---|---|
| edge_churn/edge_delete_sweep_100 | 241.0 ms | 40.5 ms [40.3, 40.7] (46.8 ms in full-suite context) | **~5.9×, O(degree) vs O(E)** |
| delete_heavy/delete_half_2p5k | 566.8 ms | 194.5 ms | **~2.9×** |
| delete_heavy/insert_unique_scalar_5k | 177.0 ms | 160.8 ms | ~9% (id cache) |
| edge_churn/edge_link_10k | 273.5 ms (Task 2) / 257.5 ms (day-of) | 359.4 ms [358.5, 360.4] (390.4 ms full-suite rerun) | **+40% — RATIFIED** |

The link regression is the controller-ratified permanent trade (Task 7
exit): 2 extra redb upserts per link buy O(degree) cascades; deletes were
the workload-blocking hazard. Tuning already banked: uncounted graph
namespaces (+49% → +40%) and a per-transaction collection-id cache
(−9% on insert_unique). A per-endpoint consolidated value cannot do
better (same 2 upserts + O(degree) decode/append, quadratic on fan-in);
the only path under +10% is pure-lazy build at first delete, which
forfeits the sweep (≈1.8× instead of 5.9×).

## Task 10 — in-memory PQ: the storage/time premiums (`41540aa`)

PQ (m=16, k=256) vs full-precision None on the pinned 2000×64d corpus;
training outside the timed region:

| Bench | None (f32) | PQ (m=16, k=256) | premium |
|---|---|---|---|
| hnsw_build_2k_64d | 124.9 ms | 367.9 ms (`hnsw_build_pq_2k_64d`) | 2.9× slower |
| hnsw_search_2k_64d | 19.1 µs | 35.8 µs (`hnsw_search_pq_2k_64d`) | 1.9× slower |
| vector payload | 256 B/doc (dim·4) | 16 B/doc (m code bytes) | **16× smaller** |

Footprint is arithmetic on the stored representations (byte-counted in a
unit test), not an allocator claim. The build premium scales linearly in
n (constant per-insert cost); the search premium is frontier churn
(ADC's compressed scale breaks early-stop less often). First-cut build
was 914 ms — the prune path's per-prune ADC-table rebuild replaced by
reconstruction scoring brought it to 368 ms (identical scores for L2).

## In-memory PQ recall margins — including the thin unit margin

| Corpus | Measured | Pinned | Margin |
|---|---|---|---|
| Conformance (`vector_inmemory_pq_recall_determinism_and_reopen`, 300×16d, 10 clusters, m=8 k=64, via `vector_search`) | **1.0** | ≥ 0.7 | wide — the public path's over-fetch + exact rerank recover the full top-k |
| Unit (`pq_recall_matches_exact_baseline`, direct `Hnsw` API, m=8 k=64, `ef_search` 100/200/400) | **0.56, identical at all three ef** | ≥ 0.55 | **thin (0.01)** — deliberate: the corpus is fixed and deterministic, so the measured value cannot drift without the corpus changing; the bound sits just under it. The ef-insensitivity is asserted (equal at 100/200/400), not just claimed |

The residual gap to exact is codebook resolution, not graph reach.
(Recall bounds were raised to these values at `0274f67`.)

## Task 13 — parallel PQ training (`72df3ed`), and the declined HNSW-build parallelism

**BEFORE provenance:** the sequential BEFORE is the identical corpus
through the forced-sequential path (`train_inner(.., allow_team=false)`)
timed in release, three runs averaged — the bench group itself is new
with the feature.

| Bench | BEFORE (sequential) | AFTER (parallel) | Δ |
|---|---|---|---|
| pq_train/pq_train_2k_64d | 177.2 ms | 67.4 ms | **2.6×** |
| pq_train/pq_train_10k_128d | 1465.0 ms | 356.2 ms | **4.1×** |

The codebook is bit-identical either way (item-indexed chunk outputs;
pinned by an equivalence test on a corpus large enough to engage the
team). The win grows with corpus × k — the assignment step dominates
training cost while the per-iteration dispatch is a fixed ~µs.

**Declined, with numbers** (HNSW graph-build parallelism):

| Design | 2k×64d | 10k×128d |
|---|---|---|
| Speculative-memo distance batching in `search_layer` (insertion order/levels/heap decisions sequential) | 128 ms → 285-595 ms (**0.21-0.45×**) | 1.35 s → 5.3-6.8 s (**0.20-0.25×**) |
| Per-vector PQ encode + ADC-probe pre-pass (one dispatch per build) | 359 → 365 ms (~1.0×) | — |
| `hnsw_build_2k_64d` (shipped: sequential path, byte-for-byte) | 123.3 → 124.1 ms (+0.7%, noise) | — |

Root cause: the heap loop consumes distances one at a time, so the
batchable pure work per insert (~15-40 µs at these dims) never clears
the multi-µs 8-thread fork/join handshake (measured wait ≈ 94 µs per
dispatch); 7 spin-polling workers degraded the caller's own sequential
phase. The redirected lever is per-eval SIMD kernels (AUDIT Open).

## Current full suite (program exit, 2026-08-29) — the NEW baseline

Full-suite run at program exit (all groups in one `cargo bench`
invocation, same machine, working tree at the Task-14 carry-in commit).
This is the baseline future work compares against. Means [95% CI]:

| Bench | Mean | 95% CI |
|---|---|---|
| value_encode | 408.28 ns | [406.93, 410.70] |
| value_decode | 847.18 ns | [842.44, 853.69] |
| hnsw_build_2k_64d | 124.08 ms | [123.37, 125.11] |
| hnsw_search_2k_64d | 18.74 µs | [18.637, 18.909] |
| hnsw_build_pq_2k_64d | 366.26 ms | [365.46, 367.11] |
| hnsw_search_pq_2k_64d | 35.37 µs | [35.315, 35.431] |
| bm25_exact_2k | 7.9152 ms in-suite / **7.5113 ms isolated re-run** | [7.4893, 7.5400] (isolated) |
| bm25_indexed_2k | 499.40 µs | [497.12, 503.17] |
| dot_768d | 76.67 ns | [76.539, 76.798] |
| l2_768d | 77.95 ns | [77.762, 78.272] |
| cosine_768d | 217.90 ns | [216.94, 219.59] |
| index_creation_ondisk/create_text_index_ondisk_5k | 223.30 ms | [222.38, 224.49] |
| index_creation_ondisk/create_vector_index_ondisk_2k_8d | 392.75 ms | [391.27, 394.23] |
| edge_churn/edge_link_10k | 385.61 ms | [383.93, 387.09] |
| edge_churn/edge_delete_sweep_100 | 45.49 ms | [45.157, 45.815] |
| compound_prefix_scan/eq_leading_only_5k | 235.29 µs | [234.63, 236.31] |
| delete_heavy/insert_unique_scalar_5k | 170.49 ms | [169.95, 171.21] |
| delete_heavy/delete_half_2p5k | 214.38 ms | [213.01, 216.15] |
| selective_window_verify/eq_50_of_5k | 226.61 µs | [225.77, 227.91] |
| selective_window_verify/eq_500_of_5k | 577.91 µs | [576.36, 579.45] |
| order_by_indexed_5k/asc_limit20 | 294.08 µs | [292.97, 295.85] |
| order_by_indexed_5k/desc_limit20 | 982.57 µs | [979.83, 985.95] |
| pq_train/pq_train_2k_64d | 51.88 ms | [50.95, 52.90] |
| pq_train/pq_train_10k_128d | 312.60 ms | [308.43, 316.36] |

## Regression check at exit

Two reference frames, both checked:

**1. vs each task's recorded AFTER** (the guard's letter). Every
code-touched bench is within ±5%: hnsw_build −0.0%, hnsw_search +0.7%,
hnsw_build_pq −1.4%, hnsw_search_pq −1.2%, compound +1.2%, eq_50
−1.4%, eq_500 +0.3%, asc_limit20 +1.6%, desc_limit20 −4.0%,
edge_link −1.2%, edge_delete_sweep −2.8% (vs the full-suite-context
AFTERs Task 7 recorded alongside its isolated ones). Three multi-hundred-ms
delete-path benches sit above their ISOLATED-context references
(insert_unique +6.0% vs 160.8 ms, delete_half +10.2% vs 194.5 ms) —
Task 7 documented this exact isolated-vs-full-suite context gap on the
same benches (+8.6%/+15.6% for link/sweep in its own full-suite rerun);
this run is a full-suite run.

**2. vs the last stored full-suite run** (criterion's native change
report — the Task-13-era baseline): every bench within ±5% or faster,
except `bm25_exact_2k` (+10.3% in-suite) — re-run in isolation it lands
at 7.5113 ms, −1.7% vs the Task-1 program baseline (untouched code
since program start; the in-suite number carried context load from the
preceding multi-second pq_train benches). `pq_train` measured FASTER
than its recorded AFTER (−23%/−12% — run-context; Task 13's own probe
runs spread 67-71 ms / 356-458 ms), and `value_decode` −6.8% (faster,
untouched). **No regression attributable to code; the guard holds.**


## Ledger-closure Task 1 — endpoint-direct graph reads: measured PARITY

`neighbors`/`in_neighbors`/`neighbors_weighted`/`traverse`'s frontier
expansion now serve from the adjacency namespaces via the exact
length-delimited `(endpoint, relation)` pair prefix (byte-identical order,
weights verbatim in the adjacency values; empty adjacency scan → one
marker point-get → current marker means genuinely empty, absent means the
source edge-namespace scan answers). BEFORE provenance: the
`neighbors_hub_10k` group was added to the pre-change tree and run there
(current code = the source-namespace `(relation, endpoint)` prefix scans);
AFTER on the implementation, same session/machine. Corpus: 2_001 docs,
10k edge writes — 4 hubs × ~938 distinct fan-in edges + ~938 weighted
fan-out edges (~313 per (hub, relation) per direction), plus 2_500
uniform degree-~1 edges (the contrast case). Means [95% CI];
`cargo bench -p corvid --bench engine -- neighbors_hub_10k`:

| Bench | BEFORE (mean) | AFTER (mean) | Δ |
|---|---|---|---|
| hub_out_knows (313 rows) | 37.08 µs [36.91, 37.34] | 37.12 µs [37.04, 37.24] | +0.1% |
| hub_in_knows (313 rows) | 29.58 µs [29.45, 29.77] | 29.96 µs [29.89, 30.05] | +1.3% |
| hub_weighted_knows (313 rows) | 37.46 µs [37.32, 37.66] | 37.27 µs [37.15, 37.48] | −0.5% |
| uniform_deg1 (2 rows) | 1.708 µs [1.700, 1.718] | 1.708 µs [1.702, 1.717] | ±0.0% |
| traverse_hub_2hops (~630 per-hop scans) | 570.5 µs [569.1, 572.1] | 581.4 µs [578.0, 585.7] | +1.9% |

**Verdict: parity** (every reader within ±2%, guard ≤5%). The asymptotic
analysis holds — the source prefix was ALREADY one contiguous B-tree
range per fixed-`relation` pair, so endpoint-major clustering buys a
fixed-relation read nothing. Kept anyway: results are provably
byte-identical (so the pinned orderings, traverse's BFS order included,
are untouched), reads and the delete cascade now share one endpoint-keyed
layout, and any endpoint-wide neighbor API (the old AUDIT row's own reopen
trigger) is pre-served. Two intermediate shapes were measured and fixed
BEFORE landing (never shipped): a per-hop marker point-get cost **+28%**
on traverse (`ReadBatch` has no collection-id cache — each point-get is a
catalog seek + a records seek), fixed by resolving the marker once per
traversal; per-hop namespace-name formatting cost ~5% (the 610 µs
intermediate), fixed by hoisting the name strings out of the hop loop.
Residual bounded cost, by design: an EMPTY read on a built adjacency pays
one extra point-get (marker), and a read on a legacy pre-adjacency
database pays one empty adjacency seek + the marker point-get before the
source scan — both disappear at that collection's first edge write or
cascade (which establishes adjacency permanently).

## Ledger-closure Task 2 — distance-kernel closure (the SIMD verdict)

Does the AUDIT's "SIMD headroom" row hide real wins? Settled by
measurement (same machine/toolchain as every table above; means [95% CI]
unless quoted as a band) across five new groups — `kernel_scaling`,
`declined_probes`, `bandwidth`, `kernel_stream`, `quantized_scan`
(invocations in the list at the top) — plus a release-assembly
inspection of the kernels. **No kernel code changed**; the legacy
`distance` group re-ran flat after the bench additions (dot +1.2%, l2
+1.4%, cosine +0.2% vs the exit baseline — guard holds).

**Assembly (release rlib, aarch64; `ar x` + `objdump -d` on the CGU
object):** both `dot` and `l2_squared` ALREADY auto-vectorize to 4-wide
NEON — `ldp q` 32-byte operand loads feeding `fmul.4s` + `fadd.4s` (l2
adds `fsub.4s`), scalar unrolled tails, and the horizontal reduction
emits exactly the source summation order (8 lane partials, then
left-to-right). No fused `fmla` anywhere: Rust does not contract FP, so
`acc += x * y` is two rounded ops by contract.

**Ceilings** (`bandwidth` group): independent 8-accumulator `u64`
wrapping-add folds over the kernels' exact operand shapes. Wrapping
integer addition is associative, so the probe vectorizes and
reassociates freely — it saturates LOAD bandwidth, which is the point:
the ceiling is reachable only by reassociating, precisely what the
`f32` exactness oracle forbids. L1-resident probes run 64 passes per
iteration (call overhead amortized, working set cached; the throughput
column prints full per-iteration bytes):

| Probe | Same shape as | Rate |
|---|---|---|
| l1_read_256b | 64d operands | 111.9 GB/s |
| l1_read_1kib | 256d | 130.9 GB/s |
| l1_read_3kib | 768d | 121.9 GB/s |
| l1_read_6kib | 1536d | 108.9 GB/s |
| l1_read_12kib | 3072d | 100.8 GB/s |
| dram_read_192mib (one pass, 4× the 48 MiB system cache) | — | 43.3–57.9 GB/s (two runs; machine-state-dependent, see streaming below) |

**Kernel scaling** (`kernel_scaling`; hot criterion shape — both
operands L1-resident, as in a cached HNSW graph walk; effective rate =
2·dim·4 bytes / time):

| dim | dot | eff. | % ceiling | l2 | eff. | % ceiling |
|---|---|---|---|---|---|---|
| 64d | 5.53 ns | 92.7 GB/s | 83% | 6.01 ns | 85.1 GB/s | 76% |
| 256d | 24.60 ns | 83.3 GB/s | 64% | 25.13 ns | 81.5 GB/s | 62% |
| 768d | 78.16 ns | 78.6 GB/s | 64% | 79.31 ns | 77.5 GB/s | 64% |
| 1536d | 171.25 ns | 71.7 GB/s | 66% | 172.69 ns | 71.2 GB/s | 65% |
| 3072d | 357.06 ns | 68.8 GB/s | 68% | 358.34 ns | 68.6 GB/s | 68% |

Per-element cost is flat-to-falling toward small dims (0.086 ns/f32 at
64d vs 0.102 at 768d): there is NO latency-bound small-dim cliff to
rescue — 64d already runs at 83% of its same-shape ceiling.

**The declined levers, measured** (`declined_probes`; bench-local
copies, never shipped):

| 768d | time | vs shipped |
|---|---|---|
| shipped dot (8 lanes, mul+add) | 78.15 ns | — |
| 16 accumulator lanes | 55.36 ns | **−29%** |
| `mul_add` (fused, stable) | 97.01 ns | **+24% slower** |

| 64d | time | vs shipped |
|---|---|---|
| shipped dot | 5.63 ns | — |
| 16 lanes | 6.42 ns | +14% (wider reduction tail hurts) |
| `mul_add` | 5.69 ns | +1% (parity) |

Sixteen lanes changes the per-lane summation order (lane j accumulates
{j, j+16, …} and the 16 partials reduce in a different tree), so the
result is not bit-identical — declined by the exactness oracle (the
exact-value kernel tests and the deterministic HNSW/recall corpora are
the pinned results). `mul_add` computes a different rounding (one
instead of two) AND de-vectorizes: LLVM emits scalar `fmadd` chains
(assembly-checked), so the fused loop is slower than the 4-wide mul+add
it would replace. Explicit SIMD cannot beat the emitted code without
reaching for exactly these two levers (more lanes, or fusion) — both
change results; `portable_simd` is nightly-only and unsafe intrinsics
are out of posture. **The kernels sit at the exactness-preserving
envelope**; at 768d the 16-lane shape would reach 111 GB/s (91% of
ceiling), i.e. the residual ceiling gap IS the forbidden
reassociation.

**Streaming — the volume shape** (`kernel_stream`; exact scan, dot vs a
fixed query over an N×768d corpus; query L1-hot; rate = corpus
bytes/time):

| Corpus | time | corpus rate |
|---|---|---|
| 256 × 768d (0.75 MiB, cache) | 18.55 µs | 42.4 GB/s |
| 4 096 × 768d (12 MiB, system cache) | 307.0 µs | 41.0 GB/s |
| 32 768 × 768d (96 MiB, beyond cache) | 2.412–2.450 ms | 41.1–41.7 GB/s |

The scan is the stable measurement here (±2% across four runs); the u64
DRAM probe swung 43.3→57.9 GB/s between same-session runs, so the
ceiling is quoted as a band. Per evaluation the scan runs at 72.5–74.8
ns ≈ the hot kernel's own rate, and wherever the corpus exceeds cache
the sustained rate sits inside the streaming band — memory-side, not
kernel-side. A 29%-faster kernel (the declined shape) cannot lift a
DRAM-resident scan past that band.

**Quantized scans — the actual volume lever** (`quantized_scan`; same
2 000×768d corpus, same graph params, cosine, k=10, ef=64):

| Storage | search | vs None |
|---|---|---|
| None (f32) | 239.8 µs | — |
| Binary (96 packed bytes; Hamming) | 4.72 µs | **50.8× faster** |
| Scalar (reconstruct to f32) | 306.8 µs | 1.28× slower |

Binary's packed-byte Hamming path (quant.rs) is the throughput answer
for big corpora — ~32× less traffic and a cheaper kernel; Scalar pays a
per-eval decode (with allocation) and lands slower than full precision
at this dim.

**Verdict (flips the AUDIT row Fixed):** LLVM already vectorizes both
kernels 4-wide; hot kernels hold 62–83% of the same-shape read ceiling
across 64–3072d with no small-dim cliff; every measured faster shape
changes `f32` summation (16 lanes −29%; fusion +24% slower AND
de-vectorized on stable Rust), which the bit-exactness oracle declines;
volume scans are memory-side; portable SIMD is nightly-only and unsafe
intrinsics are out of posture. The available throughput lever already
ships: Binary quantization, 50.8× at 768d.
