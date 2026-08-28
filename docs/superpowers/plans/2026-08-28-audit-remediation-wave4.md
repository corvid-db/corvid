# Audit Remediation Wave 4 — Consistency Windows Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the consistency-window findings: per-query single-snapshot execution (B3), TTL races (B2), graph-edge orphaning (B4), phrase/BM25 per-path divergence (B7), dump/load hardening (B8), `In`-union cap (B10), name validation (C7), auto-id/len fixes (C9), plus wave-3 carryovers (dead-scaled over-fetch; clear-txn count churn; redundant cursor read).

**Architecture:** A `SnapshotReader` trait abstracts the read operations the query planner needs. `Store` implements it by opening a fresh read transaction per op (today's behavior — every non-query path keeps working unchanged); `ReadBatch` implements it by delegating to its ONE shared snapshot. `QueryBuilder::run()`, the aggregations, candidate generation, ANN/text fetch, and `traverse` execute inside a single `Store::read` closure. The smaller findings (TTL, graph, dump/load, misc) are independent tasks.

**Tech Stack:** Rust 2024, redb 4.1.

**Spec:** `docs/superpowers/specs/2026-08-27-audit-remediation-design.md` — Wave 4 section + Decision 2. Read before starting.

## Global Constraints

- Gates per commit: fmt, clippy `-D warnings`, `cargo test --workspace`; TDD.
- Never panic on user input; typed errors.
- Coverage ≥ 90% at exit; perf-sensitive changes (the snapshot path) get a criterion check that query latency didn't regress beyond noise (existing benches must stay within ~1.2× of the wave-3 exit numbers on the same host — record numbers in the report).
- B3 binding semantics: a query must observe one consistent snapshot — every document/index read for one `run()`/aggregation/traverse comes from the same read transaction. Omission-only anomalies (missing rows mid-write) become impossible WITHIN a query; concurrent-write visibility BETWEEN queries is unchanged.
- Line anchors from `2a2d00d`; quoted code authoritative.

---

### Task 1: `SnapshotReader` trait + `ReadBatch` gains the read ops

**Files:** Modify `crates/corvid/src/store.rs`.

**Interfaces (Produces — every later task consumes):**
```rust
/// The read operations the query layer needs, abstracted over "own
/// transaction per op" (Store) and "one shared snapshot" (ReadBatch).
pub(crate) trait SnapshotReader {
    fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn scan_from(&self, collection: &str, start: &[u8], limit: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn for_each(&self, collection: &str, f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()>;
}
```
- `impl SnapshotReader for Store` — delegates to the existing per-op methods (byte-identical behavior).
- `impl SnapshotReader for ReadBatch<'_>` — `get`/`scan` already exist as inherent methods (keep them; the trait impl calls them); ADD inherent `scan_from`, `scan_prefix`, `for_each` to `ReadBatch` (ports of the Store versions but on `self.txn`; unknown-collection → empty, same as Store).
- NOTE `for_each` takes `&mut dyn FnMut` (object-safe; the closure returns `false` to stop).
- Unit tests: `ReadBatch::scan_from`/`scan_prefix`/`for_each` on a seeded store match the `Store` versions key-for-key (including resume-with-appended-0x00, unknown-collection-empty, early-stop); snapshot isolation pin — a writer thread commits doc2 after the reader's first `get` inside one `read(|r| ...)` closure; the reader's subsequent ops in the SAME ReadBatch do NOT see doc2 (redb read transactions are MVCC snapshots; this pins the guarantee the whole wave rests on).
- Commit: `store: SnapshotReader trait; ReadBatch gains scan_from/scan_prefix/for_each (audit B3 core)`

### Task 2: Thread the reader through the builder

**Files:** Modify `crates/corvid/src/builder.rs` (+ small Db plumbing in `db.rs`).

- `QueryBuilder::run_with(&self, reader: &dyn SnapshotReader)` becomes the execution core; `run()` calls `self.collection.db().store().read(|r| self.run_with(r))` — ONE read txn for the whole query. `run_scan_only` folds into `run_with` (same closure).
- Every read inside execution switches to `reader`: `verify_candidates`, `ann_candidates`' doc fetch, `text_candidates`' doc fetch, `streaming_vector_candidates` (needs `reader.for_each` over the collection), the full-scan fallback, `run_scan_only`'s streaming, `count`/`for_each_match` (aggregations gain `run`-equivalent snapshot scope via the same `read(...)` wrapper).
- `Db::scalar_candidates`/`scalar_prefix_candidates`/`compound_candidates`/`geo_candidates`/`fts_search`/`ann_search` are called from the builder — Task 3 threads the reader into THEM; until Task 3 lands, those calls keep their own txns (the builder diff alone already fixes verify/fetch/stream reads — the biggest multi-snapshot source, M5's core). Split the two tasks so each diff is reviewable.
- Snapshot/lock discipline: the reader is held ONLY within the `read(...)` closure — no kind locks, no write txns inside (resume/compaction never run inside `run()`; `try_resume_index_builds` at the chokepoints runs BEFORE `store().read(...)` opens). Verify by construction: `run_with` takes no `&self` methods that write.
- Tests: deterministic interleaving — writer thread flips doc X between two reads; the OLD shape (per-op txns) could return a set matching no point in time; the NEW shape can't. Write it as: seed 10 docs; spawn writer flipping doc "k" between two variants A/B (distinct filter results); loop `query().filter(...)` 200×; assert every result set is one of the valid single-snapshot answers (docs-with-A or docs-with-B, never mixed). This is the spec's own example made real.
- Commit: `builder: run()/aggregations execute on one read snapshot (audit B3)`

### Task 3: Thread the reader through the index candidate paths + traverse

**Files:** Modify `crates/corvid/src/scalar.rs`, `geo_index.rs`, `fts.rs` (disk_fts search calls), `disk_hnsw.rs` (search reads), `index.rs`, `graph.rs`, `builder.rs` (call sites).

- The window/candidate helpers (`window_candidates`, prefix/compound/geo window scans in scalar.rs/geo_index.rs; `disk_fts::search`/`phrase_search`; `disk_hnsw::search`) change their `store: &Store` parameter to `reader: &dyn SnapshotReader`. All existing callers pass `store` (trait impl) — behavior identical; `run_with` passes the ReadBatch — single snapshot.
- `ann_search`'s disk branch and `fts_search`'s OnDisk branch: the SEARCH itself takes the reader (candidate keys from the same snapshot as the doc fetches). The registry locks and resume stay outside/unchanged.
- `graph::traverse`: wraps its whole BFS in one `store.read(|r| ...)`; `neighbors_at` reads via `r`.
- `count()` and the aggregation entry points already snapshot via Task 2; `indexed_candidates` now fully snapshot (index windows + doc fetch in one txn).
- Tests: the wave-2 parity tests (indexed vs plain, selectivity, OR-union) all still green (they must be — the Store trait impl is byte-identical); add one end-to-end snapshot test combining a scalar-index window + doc verification: writer mutates the indexed field concurrently; every query result set matches one point in time.
- Run the benches; record before/after in the report (accept ≤ ~1.2×; the trait is `&dyn` — if a hot path shows worse, note it, don't pre-optimize).
- Commit: `query: index candidate paths and traverse on the query snapshot (audit B3 complete)`

### Task 4: Phrase fallback scores BM25 (B7)

Per the spec's decided option: `phrase_search`'s no-index fallback scores BM25 over the corpus it already scans (compute corpus stats during the same pass — analyze lengths and doc frequencies in one `for_each`, then score occurrences with the same `idf/term_score` the indexed path uses, matching its scale). Builder re-ranking keeps candidate-subset statistics; its docs state this explicitly. Parity test: same no-filter phrase query via `phrase_search` and via the builder's `.text()` source returns the same ORDER (create/drop the index around it — all four combinations: fallback vs indexed × direct vs builder — orders agree). Commit: `fts: phrase fallback scores BM25 (audit B7)`

### Task 5: TTL races (B2)

- `write_document` decides TTL-maintenance INSIDE the write transaction: replace the pre-txn `ttl_enabled(collection)` read with an in-txn namespace check (`tx.scan_from("__ttl__<collection>", &[TAG_IDX], 1)` non-empty → enabled — read the TAG_IDX tag from ttl.rs; any due entry proves the namespace exists). The in-memory marker remains as a fast path ONLY if confirmed consistent — simpler: drop the pre-txn read entirely, always check in-txn (cost: one point-range probe per write — fine).
- `purge_expired`: the delete becomes compare-expiry-and-delete — one txn per key: re-read the forward entry; if `Some(ts')` and `ts' == ts` then delete doc + entries; else skip. (Replaces the current re-then-delete two-txn shape.)
- Replace `purge_skips_records_whose_expiry_changed_since_scan` with a test that ACTUALLY drives the collect→mutate→delete interleaving: expose the collect phase (a `pub(crate) fn due_keys(collection, now) -> Vec<(Vec<u8>, i64)>`) so the test can collect, then mutate (plain insert clearing the expiry), then run the delete phase, asserting the fresh record survives. Keep the existing end-to-end purge tests green.
- Marker-race regression: thread A `insert_with_ttl` commits; thread B plain `insert` interleaved BEFORE A's `mark_ttl_collection` — with the in-txn check B's write clears the stale expiry (namespace visible in-txn). Deterministic test via the exposed phases or the lock-hold trick.
- Commit: `ttl: in-transaction expiry maintenance; compare-expiry purge (audit B2)`

### Task 6: Graph edges cascade + events (B4)

- `write_document`'s delete branch: after row delete, remove the doc's edges in BOTH directions inside the same transaction (forward edges from the key: `__edges__<coll>` rows keyed by from+label+to — read graph.rs's key layout and mirror it; reverse `__redges__` rows likewise; page the scans with the key prefixes the layout gives you).
- `link`/`link_weighted`/`unlink` emit `ChangeEvent`s (Insert for edge writes, Delete for unlink) — the subscribers contract (reactive.rs:48) says every insert/delete.
- Tests: delete a linked doc → `neighbors`/`in_neighbors`/`traverse` exclude it, edge rows gone in both namespaces, subscriber received a Delete event for the doc; `link`/`unlink` events observed by a subscriber; existing graph tests green (including "link accepts nonexistent endpoints" semantics — keep, but now documented).
- Commit: `graph: delete cascades edges in-txn; link/unlink emit change events (audit B4)`

### Task 7: dump/load hardening + misc (B8, B10, C7, C9)

- **B8 dump**: `Db::dump` takes ONE `store.read(...)` snapshot and streams every collection within it (per-collection `reader.for_each`, no materialization of the whole corpus; def/spec/TTL/edge reads inside the same closure). **B8 load**: stream the dump file (buffered `BufReader` reads instead of `read_to_end`; the reader already has u8/u32/string/bytes primitives — convert to a streaming cursor); reserved-name rejection on EVERY replay path (vector/text/scalar/geo index defs + schemas join records/compound/TTL/edges — one shared check); `create_scalar_index`/`create_compound_index`/`create_geo_index`/`create_text_index*`/`create_vector_index*`/`set_schema` call `ensure_writable`.
- **B10**: `In`-predicate union honors the same aggregate CAP as OR-union (builder.rs `predicate_candidates` In arm: bail to `Ok(None)` past 100_000 accumulated).
- **C7**: name validation — `Db::collection(name)` stores nothing yet, so enforce at the WRITE/API boundary: a `fn validate_name(name: &str) -> Result<()>` (reject `\0` anywhere; reject interior `__` sequences that could forge namespaces — simplest correct rule: reject any `__` at all except the reserved leading-`__` which `ensure_writable` already blocks, i.e. user-visible names must not contain `__` anywhere; return `Error::InvalidName`). Call it in `ensure_writable` and in every `create_*` index/schema setter (field names too). Tests: crafted `a__b`, NUL-bearing, and cross-kind namespace-collision names rejected; all existing names accepted.
- **C9**: `insert_auto` reserves the id INSIDE the insert transaction (move the META read-increment into `write_document`'s txn via a `reserve_auto_id_in_txn` on Store, one extra param or a dedicated `write_auto_document` path); `len()` → `try_into().unwrap_or(usize::MAX)`.
- Tests per item (streaming load of a >memory-sized dump can be simulated by size accounting rather than a real huge file; parity of dump/load output before/after the snapshot change).
- Commit: `migrate: single-snapshot streaming dump/load, reserved-name hardening; misc B10/C7/C9 (audit B8)`

### Task 8: Wave-3 carryovers

- **Dead-scaled over-fetch** (the B5 recall completion): `disk_hnsw::search`'s `want = ef_search.max(k) * 2` becomes dead-scaled — mirror the in-memory rule: `want = (ef_search.max(k) + dead)` (over-fetch by the dead count so k live nodes always remain). Recall test: the Task-3 compaction corpus between crossings (dead ≈ live/2) now returns full k (pin ≥ 0.75 recall at a dead fraction where the old fixed 2× measured 0.655).
- **clear-txn count churn**: `clear_in_txn` batches the META count adjust (one read + one write per PAGE instead of per key — the count delta = page length, computed before deleting).
- **Redundant cursor read**: the create fns' `read_building_cursor` after their own always-fresh registration is replaced by just `Vec::new()` (the registration wrote it; a crash between leaves Building{[]} which resumes from scratch — same semantics).
- Commit: `disk-hnsw: dead-scaled over-fetch (B5 recall); clear/creation cleanups (wave-3 carryovers)`

### Task 9: Wave exit

1. Full gates + llvm-cov ≥ 90 (dips → report).
2. Bench check: `cargo bench -p corvid --bench engine` — query benches within ~1.2× of wave-3 exit; record numbers.
3. DESIGN.md decision-log row:
```markdown
| 2026-08-27 | Query execution (run/aggregations/candidate paths/traverse) reads one MVCC snapshot via a SnapshotReader trait; phrase fallback scores BM25; TTL maintenance decided in-txn; doc delete cascades graph edges; dump is one snapshot, load streams | Closes B2/B3/B4/B7/B8: per-key read txns could return sets matching no point in time; purge raced rewrites; deletes orphaned edges; path-dependent phrase scores; dump was torn + unbounded |
```
4. AUDIT.md wave-4 block: B2, B3, B4, B7, B8, B10, C7, C9 fixed with hashes; carryover note.
5. Commit: `AUDIT/DESIGN: wave 4 — consistency windows landed (B2/B3/B4/B7/B8/B10/C7/C9 fixed)`
