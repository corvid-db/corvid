# Audit Remediation Wave 3 — On-Disk Index Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the on-disk lifecycle findings: re-registration transactionally resets the index namespace (A5), tombstone accumulation compacts (B5), ANN distances become exact-scale with an `approximate` flag (B6), corrupt on-disk state surfaces as a typed error instead of silent-empty (C1/C13 + decision 5), and the wave-2 creation-perf regression is paid down (page-level batching).

**Architecture:** Everything reuses wave-2's machinery: a namespace reset is `clear namespace + def→Building{cursor:[]}` in one transaction followed by the atomic backfill driver; compaction is that same cycle triggered by a dead-fraction counter in `Meta`; the perf fix restores the deleted bulk-insert path *inside* the driver's per-page transaction. `Hit` gains `approximate` and `vector_search` reranks ANN hits with exact distances computed from the fetched documents.

**Tech Stack:** Rust 2024, redb 4.1.

**Spec:** `docs/superpowers/specs/2026-08-27-audit-remediation-design.md` — Wave 3 section + Decision 5. Read before starting.

## Global Constraints

- Gates per commit: `cargo fmt --all`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`; TDD (red first).
- Never panic on user input; typed errors (`thiserror`).
- Coverage ≥ 90% at wave exit; no padding tests.
- Perf rule: Task 1's acceptance is the existing `bench_creation_ondisk` vector number moving from the recorded 3.36 s (@2049 docs) back to ≤ 2× the pre-driver baseline 545 ms (i.e. ≤ ~1.1 s) on the same machine; record the new number in the commit message.
- `Meta` gains an optional trailing `dead: u32` field — legacy metas without it decode `dead: 0`; encoders always write it. No format-version bump (pre-1.0; old files readable).
- Line anchors from `46318ed`; quoted code authoritative.

---

### Task 1: Page-level batch insert for on-disk vector creation (perf fix)

**Files:** Modify `crates/corvid/src/disk_hnsw.rs` (new `insert_page_in_txn`), `crates/corvid/src/index.rs` (`resume_vector` closure).

**Interfaces (Produces):**
```rust
/// Insert a page of (doc_key, vector) pairs inside the caller's transaction
/// with ONE node cache and ONE meta read-modify-write for the whole page —
/// the atomicity of per-tx inserts at (near-)bulk speed.
pub(crate) fn insert_page_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    page: &[(Vec<u8>, Vec<f32>)],
) -> Result<()>
```

- [ ] **Step 1: Baseline check.** Run `cargo bench -p corvid --bench engine -- bench_creation_ondisk/vector` and record the number (expect ≈3.3 s @2049 docs per the wave-2 exit commit).
- [ ] **Step 2: Failing perf-shape test** (disk_hnsw.rs tests — behavior pin, the bench is the perf pin):

```rust
/// A page insert produces the same graph state as per-doc inserts: build the
/// same corpus into two namespaces — one via insert_page_in_txn, one via
/// repeated insert_in_txn in a single transaction — and require identical
/// search results (top-5 keys, both k-ordered).
#[test]
fn page_insert_matches_per_doc_state() {
    let store = Store::open_in_memory().unwrap();
    let p = DiskParams::with_quant(Metric::L2, Quantization::None, 8, 64);
    let items: Vec<(Vec<u8>, Vec<f32>)> = (0..50u8)
        .map(|i| (vec![i], vec![i as f32, 1.0, 0.0, 0.0]))
        .collect();
    store
        .transaction(|tx| insert_page_in_txn(tx, "ix", &p, &items))
        .unwrap();
    store
        .transaction(|tx| {
            for (k, v) in &items {
                insert_in_txn(tx, "ix2", &p, k, v)?;
            }
            Ok(())
        })
        .unwrap();
    let a = search(&store, "ix", &p, &[0.0, 1.0, 0.0, 0.0], 5, 64).unwrap().unwrap();
    let b = search(&store, "ix2", &p, &[0.0, 1.0, 0.0, 0.0], 5, 64).unwrap().unwrap();
    let keys = |r: &[(Vec<u8>, f32)]| r.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    assert_eq!(keys(&a), keys(&b));
}
```

- [ ] **Step 3: Implement** — `insert_page_in_txn` is the deleted `insert_many`'s body reborn as an in-txn function:

```rust
pub(crate) fn insert_page_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    p: &DiskParams,
    page: &[(Vec<u8>, Vec<f32>)],
) -> Result<()> {
    let mut meta = read_meta(tx, ns)?;
    let mut cache = Cache::new();
    for (doc_key, vector) in page {
        insert_node_in_txn(tx, ns, p, &mut cache, &mut meta, doc_key, vector)?;
    }
    flush_dirty(tx, ns, &mut cache)?;
    tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    Ok(())
}
```

Then `resume_vector`'s driver closure (index.rs) collects the page's `(key, vector)` pairs first and calls `insert_page_in_txn(tx, &ns, &params, &batch)` once per page (skipping the page's docs without vectors, as today).

- [ ] **Step 4: Green + gates; re-run the bench; commit** with the before/after numbers in the message:
```
disk-hnsw: page-level batch insert in the atomic driver (wave-2 perf fix)

Creation bench (2049 docs, same host): 3.36s -> <NEW>. Restores the deleted
bulk path's shared node cache + single meta RMW per page, inside the driver's
per-page transaction — atomicity unchanged, bulk speed back.
```
Acceptance: `cargo test -p corvid` green (all existing HNSW parity/recall tests unchanged — the graph algorithm is untouched), bench ≤ ~1.1 s.

---

### Task 2: Transactional registration with namespace reset (A5)

**Files:** Modify `crates/corvid/src/index.rs` (`register_vector_index_inner`), `crates/corvid/src/store.rs` (add `clear_in_txn`), `crates/corvid/src/db.rs` if wiring needs it.

**Interfaces (Produces):**
```rust
// store.rs
/// Delete every key in `collection` inside the caller's transaction
/// (paged via scan_from; no materialization).
pub(crate) fn clear_in_txn(tx: &mut WriteBatch<'_>, collection: &str) -> Result<()>
```
`register_vector_index_inner` becomes ONE transaction: { def row put (Building{cursor: vec![]} — always fresh; PQ cursor rule stays), codebook put when PQ, `clear_in_txn(tx, ns)` for the target namespace }, then the in-memory def insert + `built` removal as today. Every re-registration — same or different kind/quant/metric — starts from an empty namespace (spec decision: "re-register clears the target namespace in the same transaction that installs the new Building def"; kind switches clean up; the PQ two-txn window and the register get→put interleave collapse into this single transaction).

- [ ] **Step 1: Failing tests** (index.rs tests):
  1. `recreate_with_different_quantization_is_clean`: seed docs with vectors; `create_vector_index_ondisk_quantized(Scalar)`; search; re-create as `Quantization::None` (or →PQ); assert (a) search results still parity-match an unindexed twin collection, (b) after re-creation the namespace contains no stale-encoding node — observable via: reopen the db file, search again, parity still holds, and (in-crate) a scan of the `__dann__` namespace shows node count == live docs (no leaked tombstones from the first build).
  2. `kind_switch_to_inmemory_removes_disk_namespace`: `create_vector_index_ondisk` then `create_vector_index` (InMemory) — the `__dann__` namespace scan is empty afterwards (the disk graph is gone, not leaked).
  3. `replace_during_resume_is_consistent` (the W2T5/W2T9 deferred race): hold `index_resume` via `.lock()` to stall a lazy resume, forge a mid-cursor Building def, start a query on another thread (it falls back), re-register the index (fresh Building + cleared ns), drop the lock, query — results parity-match exact; def ends Complete.
- [ ] **Step 2: Red → implement** (`clear_in_txn` paged `scan_from`+delete loop inside the txn; registration restructure). **Step 3: Green + gates. Commit:** `vector: transactional registration with namespace reset (audit A5)`

---

### Task 3: On-disk compaction (B5)

**Files:** Modify `crates/corvid/src/disk_hnsw.rs` (Meta.dead field; delete_in_txn increment; search returns dead fraction or exposes it), `crates/corvid/src/index.rs` (trigger + rebuild in ann_search path).

Mechanism (reuses Task 2's reset + wave-2's driver):
- `Meta` gains `dead: u32` (encode_meta appends 4 BE bytes; decode_meta reads them if present, else 0). `delete_in_txn` increments after tombstoning.
- In `ann_search`'s on-disk branch, after a successful search, if `meta.dead * 2 > live_count` (live = count - dead; mirror the in-memory >50% rule): try-lock `index_resume`; if acquired, synchronously run the compaction cycle = the Task-2 registration shape on the SAME def (one txn: def row → Building{cursor: vec![]}, clear ns) followed by `resume_vector` (driver re-backfills from documents); release. Concurrent queries during the cycle see `building` → exact fallback (correct, temporarily uncompacted). If try-lock fails, skip (another thread is working).
- Tests (index.rs): `mass_delete_compacts_and_restores_recall` — build on-disk index over ~2000 docs, delete ~90%, search k=10 → exactly the surviving top-k (parity with an unindexed twin), and the namespace's node-tag count is ≈ live docs (compaction ran; assert via in-crate namespace scan); `dead_below_threshold_does_not_compact` — one delete, no rebuild (node count unchanged).
- Commit: `disk-hnsw: dead-fraction compaction via reset + atomic re-backfill (audit B5)`

---

### Task 4: Honest ANN distances + `Hit.approximate` (B6)

**Files:** Modify `crates/corvid/src/query.rs` (Hit field + vector_search rerank), doc touch: `crates/corvid/src/semantic_cache.rs` (doc comment), `CHANGELOG.md`.

- `Hit` gains `pub approximate: bool` (true when served via an ANN index, false on the exact path). Update every `Hit { .. }` construction site (exact path, ANN path, any tests).
- In `vector_search`'s ANN arm: after fetching each `document`, recompute `distance = metric.distance(query, doc_vector)` from the document's stored vector (skip docs whose field is missing/not a vector/dim-mismatched — parity with the exact path), then re-sort by `(distance.total_cmp, key)` and truncate to k. The index's approximate distances are discarded — `Hit.distance` is always the exact metric distance; `approximate` tells the caller the *candidate set* came from ANN (recall may be < 1).
- `semantic_cache.get` doc comment: thresholds are exact-metric distances under any index mode (the rerank makes it true); no code change needed there.
- CHANGELOG `Unreleased`/`Changed`: `Hit` gains `approximate`; ANN-returned distances are now exact-metric reranks (previously quantized/Hamming-scale for indexed modes) — breaking struct change ahead of 1.0. (audit B6)
- Tests (query.rs): `ann_hits_have_exact_distances_and_flag` — build a binary-quantized on-disk index (Hamming-scale raw distances) over a small corpus; `vector_search` hits' distances EQUAL the exact path's distances for the same keys (bitwise or < 1e-6), `approximate == true`; exact path `approximate == false`; `semantic_cache_under_quantized_index_hits_threshold` — cosine 0.05 cache with a Binary-quantized index on the embedding field returns the exact-hit value (previously impossible: Hamming ≥ 1 > 0.05).
- Commit: `vector_search: exact rerank of ANN hits + Hit.approximate (audit B6)`

---

### Task 5: Corrupt-state hardening (C1/C13, Decision 5, W2T1/W2T10 residue)

**Files:** Modify `crates/corvid/src/error.rs` (new variant), `disk_hnsw.rs`, `schema.rs`, `index_build.rs`, `index.rs`.

- `Error::CorruptIndex { context: String }` (thiserror; store/mcp error-mapping surfaces it as text).
- `disk_hnsw::search`/`load`/`load_r`: a *present but malformed* node/meta value (decode returns None) → `Err(Error::CorruptIndex)` instead of silent `Ok(Some(vec![]))` (absent key stays "empty index"). Keymap value shorter than 8 bytes → skip the entry (no node-0 tombstoning, C13).
- Capacity clamps against remaining input: `decode_node`'s `Vec::with_capacity(n_layers)`/`with_capacity(cnt)` → `.min(bytes_remaining / MIN_ITEM)` bounds (compute from the slice length); `Schema::decode`'s `Vec::with_capacity(n)` → `n.min(4096)` (migrate.rs precedent); `index_build::decode_def`'s `4 + len` → `len.min(value.len().saturating_sub(4))` style clamp (kills the 32-bit overflow, keeps Building-restart semantics).
- `collect_building_vector` (W2T10): skip rows whose kind bytes don't decode to a def the registry knows (no job churn for corrupt-metric rows).
- Tests: forged truncated node value in the namespace → `vector_search` returns `Err(CorruptIndex)` (not empty); forged huge counts in node bytes / schema bytes / def cursor-len → no abort, decodes to safe fallbacks; keymap short value → intended delete no-ops instead of tombstoning node 0.
- Commit: `engine: corrupt on-disk index state errors loudly; decode clamps (audit C1/C13, decision 5)`

---

### Task 6: Test hardening + wave exit

1. Contention-fallback tests for scalar/compound/geo (copy the fts/vector 2-line lock-hold trick): hold `index_resume`, forged Building def, query → correct fallback results, def still building; drop lock, query → complete.
2. Full gates + `cargo llvm-cov --workspace --fail-under-lines 90` (dips → report).
3. DESIGN.md decision log rows (match table format):
```markdown
| 2026-08-27 | Re-registration = one transaction {def→Building, codebook?, clear namespace}; compaction = same cycle triggered at >50% dead (Meta.dead counter); both re-backfill via the wave-2 driver | Closes A5/B5: mixed-encoding graphs, leaked namespaces, tombstone recall collapse — and collapses the register two-txn race + PQ codebook window |
| 2026-08-27 | vector_search reranks ANN hits with exact distances; Hit.approximate marks the candidate source; corrupt on-disk state errors (CorruptIndex) instead of silent-empty | Closes B6 + C1/C13/decision 5: quantized-scale distances broke semantic_cache thresholds; silent-empty hid corruption |
```
4. AUDIT.md status: A5, B5, B6, C1, C13 fixed with commit hashes; perf-fix note (creation bench restored to ≤2× baseline).
5. Commit: `AUDIT/DESIGN: wave 3 — on-disk lifecycle landed (A5/B5/B6/C1/C13 fixed)`
