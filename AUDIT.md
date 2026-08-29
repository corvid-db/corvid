# corvid — audit status

A deep audit of the entire workspace ran 2026-08-26 (original text preserved
below). Its findings drove a five-wave remediation on 2026-08-27/28; all five
waves landed on `master`. Gates (fmt, clippy `-D warnings`, `cargo test
--workspace`, `cargo doc -D warnings`, coverage ≥ 90%) ran green at each wave
exit; the wave-5 exit numbers are recorded in the block below.

Finding IDs below follow the remediation spec's inventory
(`docs/superpowers/specs/2026-08-27-audit-remediation-design.md`), which
re-numbered the original audit's findings (its C0 / M1–M25 / minors) into
`A#` high, `B#` medium, `C#` low/hygiene, `D#` docs/claims.

Wave-5 exit (this commit, with `5134488`, `9704551`, `5b21e61`, `a9aba00`,
`e23ebd9`): final gates green — fmt clean; clippy `--all-targets --workspace
-D warnings` clean; `cargo test --workspace` 512 passed / 0 failed;
`cargo doc --no-deps --workspace` under `RUSTDOCFLAGS="-D warnings"` clean;
`cargo llvm-cov --workspace --fail-under-lines 90` green at 96.10% lines
(regions 94.56%, functions 96.68%).

## Fixed

| Finding | One-line description | Commit(s) |
|---|---|---|
| A1 | ANN: overwriting an indexed doc with a different-dimension vector tombstones the old node (results match exact) | `d85815a` |
| A2 | Index creation crash-safe: persisted `Building{cursor}`→`Complete` watermark, never-serving-Building, lazy resume, all five kinds | `b4982fd`..`5ac9eb5` |
| A3 | Unique constraints enforced on non-index-encodable values; NaN conflicts with NaN | `92c0130`, `b517ef3` |
| A4 | PQ: bound-checked ADC codes, `Option` L2 table on dim mismatch, gated codebook dim | `97a4684` |
| A5 | Re-registering a vector index resets its namespace transactionally; no mixed encodings or leaks | `c6d4635` |
| B1 | `Db::bulk`: thread-local, panic-safe relaxed-durability scope | `eccdefc` |
| B2 | TTL maintenance decided inside the write txn; purge deletes compare-expiry | `b9e792d` |
| B3 | Snapshot-scoped execution: one MVCC snapshot per query/aggregation/traverse | `f615dbd`, `ac9f0f3`, `b826640`, `cd14da0` |
| B4 | Document delete cascades graph edges in-txn; link/unlink emit change events | `0cef6b0` |
| B5 | On-disk HNSW compaction at `dead*2 > live`; over-fetch scales with dead count | `25d7d3f`, `c2cd13a` |
| B6 | `vector_search` reranks ANN hits with exact distances; `Hit.approximate` | `7712729` |
| B7 | Phrase fallback scores BM25 on the indexed paths' scale; index creation never reorders a query | `2ebe69d` |
| B8 | `dump` single-snapshot and streaming; `load` streams + rejects reserved names on every replay path | `4c31e02` |
| B9 | Availability characteristics documented (global registry mutex, namespace-clear write lock, O(E) edge cascade); lock split deferred | this commit (mitigation doc) |
| B10 | `In`-union honors the 100k aggregate cap (falls back to scan) | `4c31e02` |
| C1 | Decoders clamp allocations against remaining input; no unvalidated `with_capacity` | `19a8a74` |
| C2 | Antimeridian bbox wraps (two longitude ranges); lat/lon validated at entry | `5134488` |
| C3 | `explain()` reports the plan shape the executor will take (parity-pinned) | `9704551`, `5b21e61` |
| C4 | `order_by`: missing AND incomparable values sort last (stable by key) | `5134488` |
| C5 | i64-compared-via-f64 precision limit documented (value.rs/filter.rs) | this commit (docs) |
| C6 | RRF k, MMR λ, Bm25Params validated (`Error::InvalidArgument`) | `5134488` |
| C7 | Name validation: interior `__` and NUL rejected (`Error::InvalidName`) | `4c31e02` |
| C8 | Backup: partial destination removed on mid-copy failure; residual race documented | `5134488` |
| C9 | `insert_auto` reserves in-txn (no burned ids); `len` saturates | `4c31e02` |
| C10 | Index-path tests observe service; `plan_shape`↔served-path parity matrix | `9704551`, `5b21e61` |
| C11 | CI: iOS job, timeouts, concurrency; release: build-first, tag check, checksums, scoped permissions | `a9aba00` |
| C12 | MCP convert fidelity limits documented (u64 > i64::MAX → lossy float; non-finite → null) | this commit (docs) |
| C13 | Corrupt keymap values skip (no node-0 tombstoning); `Error::CorruptIndex` errors loudly | `19a8a74` |
| C14 | Stop-word-collapsed phrase positions + S-stemmer limits documented | this commit (docs) |
| D1 | README iOS/WASM rows made true (CI iOS job; measured wasm size) | `a9aba00` |
| D2 | README true-predicate claim qualified with the `.approx()` opt-out | this commit |
| D3 | store.rs/db.rs module docs rewritten to the post-remediation truth | this commit |
| D4 | Group keys: bare text, tagged non-text, `t:`-escaped ambiguity | `281e6f3` |
| D5 | DESIGN.md reconciled: layer map, future-spec markers, decision-log dates, closed open-questions, B9/deferred notes | this commit |
| D6 | AUDIT.md rewritten as this status doc | this commit |
| Verify-candidates batching | Indexed query verification picks its fetch by window density: `candidates × 17 ≥ collection count` takes ONE ordered `for_each` walk of the records (skipping non-candidates by key, stopping past the last), else the historical per-key point-gets; rows, candidate order, filter verdicts, and snapshot scope identical either way (measured crossover ≈5.8% density, dense windows ~22% faster — `selective_window_verify` bench) | this commit |

## Open

Deferred by decision (each with its rationale and follow-up destination), not
dropped silently:

| Item | Rationale / follow-up |
|---|---|
| Endpoint-indexed edge layout | Document delete cascades edges by scanning the collection's two edge namespaces — O(collection edge count) per delete. An endpoint-indexed (adjacency) layout removes the scan; taken up if delete churn on edge-heavy collections grows. |
| Page-level single-snapshot | `page`/`page_where` walk in 1024-key chunks, each chunk its own read transaction; a page spanning concurrent writes observes mixed state (per-chunk consistent). Point-in-time needs use the snapshot-scoped builder. |
| In-memory PQ | The in-memory HNSW supports None/Binary/Scalar only; PQ (+ ADC) is wired into the on-disk index — the footprint-critical path. In-memory PQ is a deliberate deferral (DESIGN.md). |
| tracing / observability | No structured logging in the engine today; the Observability section is marked specified-not-implemented. Corrupt/unknown state surfaces as typed errors (`CorruptIndex`) rather than logs. |
| `a__b` migration tooling | Dumps from pre-wave-4 databases with `__`-containing names fail at load's index/schema replay (`InvalidName`); no automated rename tool — rename collections or re-create indexes after load (DESIGN.md deferred notes). |
| >4 GiB dump sections | The dump format's length prefixes are u32; a single value/count beyond 4 GiB cannot be represented. A future format version widens to u64 when a workload needs it. |
| Compound prefix-only windows scan | Declined for soundness after the missing-trailing-field omission bug (fixed `fabfe6e`): the compound index skips docs missing any indexed field, so a query leaving a field unconstrained can match unindexed docs. Sound re-enable needs per-def all-docs-indexed metadata. |
| Parallel HNSW build / PQ training | All engine hot paths are single-threaded; parallel HNSW construction (and PQ k-means) is the largest raw performance headroom. Taken up in the roadmap-execution program, with build determinism preserved bit-for-bit. |
| Sort indexes | `order_by` is an in-memory unbounded sort over the full match set; when a scalar index on the ordered field provably reproduces the documented total order, the index walk can serve it (identical results or decline). Taken up in the roadmap-execution program. |
| SIMD distance kernels | The distance kernels are manual lane-folds and already memory-bound at current widths; portable `std::simd` buys a constant factor on wide dims. Future, not scheduled this program. |

---

## Historical: original audit (2026-08-26)

Date: 2026-08-26. Scope: entire workspace (`corvid` engine, `corvid-mcp`, `corvid-wasm`, CI, docs).
Method: five parallel line-by-line adversarial reviews plus independent verification of every
headline claim against the source (all `file:line` refs below were confirmed). Build state at
audit time: `cargo test --workspace` 390 tests green, `cargo clippy -D warnings` clean.

Severity legend: **CRITICAL** breaks the project's own central contract. **MAJOR** wrong results,
data loss, crash, or DoS reachable through normal/intended use. **MINOR** edge-case wrongness,
behavioral inconsistency, or doc claims with behavioral impact. **NIT** hygiene.

---

## 0. CRITICAL — the central invariant is not implemented (and its scaffolding doesn't exist either)

**C0. Row commit and index maintenance are separate transactions; on-disk indexes are never reconciled.**
- `db.rs:229-240` (`insert`): `store().put(...)` commits the row, *then* `maintain_insert`
  (`db.rs:176-184`) runs each index update in its **own** transaction (`scalar.rs:131-142`,
  `geo_index.rs:91-103`, `disk_fts.rs:145-152`, `disk_hnsw` likewise). Same shape in
  `compare_and_set` (`db.rs:292-326`), `insert_batch` (`db.rs:360-374`), `delete`.
- DESIGN.md §"Critical tension" promises contract (b) from day one, allowing scaffold (a)
  "reconcile-on-open". Neither exists: there is **no watermark, no replay, no rebuild of any
  persisted index on open** (`grep watermark|reconcile|replay` → only migrate.rs). `Db::open`
  loads index *definitions* only.
- Consequences:
  - Crash (or any mid-maintenance `Err`) between row commit and index commit leaves scalar /
    compound / geo / on-disk HNSW / on-disk FTS **permanently desynchronized**: an index miss is a
    silent false negative forever (candidates are re-verified, so nothing recovers it).
  - If `maintain_insert` fails midway, `insert()` returns `Err` although the row **is committed**
    — and subscribers never receive the event (`db.rs:366-373`). Callers retrying on Err
    double-write; callers trusting Err lose track of data.
  - Concurrent readers can observe a committed row with not-yet-updated indexes (window between
    the two transaction families).
- The safety story in the docs is factually wrong for every on-disk index kind added since:
  `store.rs:17-21` ("a crash can only lose in-memory index state"), `lib.rs:13` ("kept consistent
  on every write, so a query never sees a stale index"), `index.rs:13-14`, `fts.rs:11`,
  `quant.rs:5-6`. Fix shape: maintainers join the caller's `WriteBatch` (the seam already exists:
  `disk_hnsw::insert_in_txn`, `scalar::insert_many`), plus a reconcile/backfill-on-open for
  persisted kinds.

---

## 1. MAJOR

### Concurrency / atomicity
- **M1. Lazy-build install race permanently drops writes** — `index.rs:373-384` & `fts.rs:330-351`
  (phrase variant `364-386`): `needs_build` checked under lock, build scans **unlocked**, install
  via `entry().or_insert()`; meanwhile the write path no-ops while unbuilt (`index.rs:295-303`,
  `fts.rs:275-284`). A writer committing during the scan is skipped by maintenance *and* missed by
  the snapshot → invisible to ANN/BM25 until restart. All-`&self` API makes this reachable.
- **M2. Join is not single-snapshot** — `join.rs:27-56`: left leg `self.scan()?` is one read txn;
  right-leg lookups run inside a second `store.read`. Comment claims "within one read snapshot";
  DESIGN.md:215 lists "single-snapshot join" as resolved. Torn cross-collection pairs possible.
  (Bonus: `join` coerces `Value::Int` foreign keys via `to_string`, contradicting its own
  "must be Text or Bytes" doc.)
- **M3. Unique constraints are TOCTOU and defeated by batches** — check runs outside the write txn:
  `schema.rs:264-299` via `validate_schema` (`db.rs:231`) before `put` commits separately. Two
  threads inserting the same unique value both succeed; worse, `insert_batch` validates the whole
  batch against pre-batch state, so **two items in one batch sharing a unique value always
  commit** (`db.rs:354-365`).
- **M4. TTL purge deletes from a stale snapshot; TTL entries for absent keys strand forever** —
  `ttl.rs:137-170`: due-keys collected, then deleted one-by-one later; a concurrent plain
  `insert` (which clears expiry) can be overwritten by the purge deleting the fresh record.
  `set_ttl` on a nonexistent key (`ttl.rs:84-98`) creates entries whose purge finds nothing to
  delete → rescanned forever. Also `insert_with_ttl` is two transactions (`ttl.rs:185-188`) — a
  crash between them yields an immortal record.
- **M5. Builder execution spans many snapshots** — `builder.rs:753-764`: per-candidate
  `Collection::get` (own txn each); scalar windows page through up to ~25 `scan_from` txns.
  Deviation from "snapshot isolation for reads"; omissions only, hence major-not-critical.

### Silent wrong results
- **M6. Scalar index misses ±0.0 ties** — `scalar.rs:87-97` encodes `-0.0` and `+0.0` as different
  keys, but predicates treat them equal (`filter.rs:263` `PartialEq`; `partial_cmp` Equal).
  `eq/ge(Float(0.0))` builds window `[payload(+0.0) …]` and starts scanning *at* it
  (`scalar.rs:196-260`) → stored `-0.0` documents are never returned when the index drives;
  scan path returns them. Falsifies "a range scan never excludes a true match"
  (`scalar.rs:16-19`). Fix: canonicalize −0.0→+0.0 in `num_payload` (encode + bounds).
- **M7. Geo bbox is not a superset — `within_km` drops matching rows** — `geo_index.rs:203-220`:
  `dlon = r/(111.32·cos(center_lat))` but maximal longitude excursion occurs poleward where cos is
  smaller. Hand-verified counterexample: center (60°,60°), r=1000 km → bbox caps lon at 77.97°,
  yet doc (62°,78°) is 992 km away and matches. Index-driven `GeoWithin` (builder
  `builder.rs:609-621, 733-751`) and `Collection::geo_within_radius` silently omit it. Use
  `cos(lat+dlat)`.
- **M8. Geo index namespace mixes key layouts; forward-map keys truncate cell-row scans** —
  `geo_index.rs:143-192`: forward keys are `[0x00 ‖ doc_key]`, cell keys `[latBE ‖ lonBE ‖ …]`
  with lat ≤ 1800 ⇒ leading `[0x00,0x00]`. Doc keys beginning with zero bytes (BE counters!)
  interleave into row ranges; the loop treats them as cell entries (`172-177`) — garbage
  candidates (harmless post-verification) or early `break` that **skips real cell entries** →
  missing results. Module doc's premise ("forward map is read by exact key, never range-scanned",
  lines 18-21) is false.
- **M9. Nested-field geo is broken end-to-end** — maintenance resolves dotted paths
  (`db.rs` maintain → `get_path`), but verification uses top-level `.get(field)`:
  `geo.rs:75` (`geo_within_radius`), `geo.rs:157` (`geo_within_bbox`); `join.rs:33` and
  `sketch.rs:136-148` do the same. `create_geo_index("meta.loc")` succeeds; queries silently
  return nothing. Contradicts `value.rs:67-71`'s "accessor used uniformly".
- **M10. No dimension validation anywhere in the vector path** — `hnsw.rs`/`disk_hnsw.rs` record
  no dim and never check; `Metric::distance`'s `debug_assert` (`distance.rs:31`) fires in debug
  builds (panic on user input) and release builds silently compute prefix-truncated dot/L2/cosine
  (garbage distances poisoning the whole graph). Exact paths skip mismatched docs
  (`query.rs:168`, `builder.rs:465`) so behavior flips depending on index existence. Directly
  contradicts `distance.rs:27-29` ("The search layer validates dimensions") and `query.rs:139`
  ("dimension differs … skipped").
- **M11. PQ: OOB panic on corrupt codes; wrong-dim vectors become phantom points; zero tables** —
  `pq.rs:146-152` `adc_l2` indexes `table[s*k + c]` with unchecked `c` from on-disk bytes →
  index-out-of-bounds panic during search on a corrupted/hostile file; `l2_table` returns an
  all-zero table on dim mismatch (`123-126`) making every node distance 0.0; `encode` maps
  wrong-dim vectors to all-zero codes (`82-95`) which decode to centroid-0 points that compete in
  the graph; `Pq::from_bytes` accepts `k > 256` (only `train` checks) enabling huge allocations.
- **M12. Crash windows flip quantization/PQ modes persistently** — `index.rs:240-264` persists
  the def *before* the codebook; `create_vector_index_ondisk_quantized/_pq` register the new mode
  *then* backfill chunk-by-chunk. Crash between steps ⇒ on reopen PQ codes parse as f32
  (`StoredVec::from_bytes(Quantization::None, …)`), old-quant nodes decode under the new mode;
  mismatches decode as garbage or `f32::INFINITY` (`quant.rs:110`, `disk_hnsw.rs:109`) —
  documents become invisible with no error. `(OnDiskPq, None)` even dumps silently as plain
  OnDisk (`index.rs:159-167`).
- **M13. On-disk HNSW returns fewer than k live hits under churn; never compacts** —
  `disk_hnsw.rs:536` over-fetches a fixed `ef.max(k)*2` while deletes only tombstone
  (`486-501`); in-memory uses exact `k + dead()` (`index.rs:124`). Tombstones grow monotonically;
  no on-disk rebuild exists. Compaction trigger for the in-memory kind lives only in `ann_search`
  (`index.rs:386-398`) despite the module doc claiming maintenance-time compaction.
- **M14. Numeric `eq`/`is_in` are type-strict while ordered comparisons interoperate** —
  `filter.rs:263` `Eq => found == rhs` (`Int(2) != Float(2.0)`) vs `value_order` numeric casts
  (`filter.rs:282-291`). Via MCP: store `{"n":2}`, filter `{"op":"eq","value":2.0}` matches
  nothing; `"ge"` matches. The scalar index encodes both identically, so index and predicate
  disagree about equality. Silent empty results for the primary JSON/LLM audience.
- **M15. Group-key canonicalization collides across types** — `builder.rs:1033-1042`:
  `Text("1")`, `Int(1)`, `Float(1.0)` all canonicalize to `"1"`; `Bool(true)` collides with
  `Text("true")` → `count_distinct`/`group_count` undercount on heterogeneous fields.
- **M16. Phrase/text relevance differs per physical path** — indexed phrase scores BM25 sum
  (`fts.rs`), fallback scores raw occurrence counts (`query.rs:112-121`); filtered hybrid ranking
  computes IDF over the *filtered subset* (`query.rs ranked_bm25`) while the index uses
  corpus-wide stats. Registering an index changes ordering for the same logical query.

### Data loss / migration integrity
- **M17. `dump`/`load` loses the entire graph** — edges live in `__edges__*/__redges__*`
  (`graph.rs:127-134`), hidden from `collections()` (`db.rs:124-131`), so `migrate.rs:118-135`
  never exports them. Documented migration procedure (dump old binary → load new) silently
  deletes every link. Dump also omits auto-id counters (`store.rs:140-151` META keys) →
  post-load `insert_auto` regenerates used keys and **silently overwrites existing documents**
  (upsert semantics). `backup` gets both right; the two mechanisms disagree.
- **M18. `dump` is neither point-in-time nor bounded-memory; `load` is neither atomic nor
  hardened** — per-collection scans use separate read txns (`migrate.rs:123-129`); whole dump
  assembled in RAM; `load` writes records one-by-one before definitions replay and aborts
  mid-stream on legitimate dumps (PQ index whose training docs were later deleted →
  `EmptyIndexTraining` leaves a partially loaded DB, `migrate.rs:252-259`).
- **M19. `load` bypasses reserved-name protection and enables panics** — `migrate.rs:241` writes
  via `store().put` with no `ensure_writable`/schema validation: a crafted dump forges
  `__edges__*`, `__schemas__`, `__ttl__*` content; a short forged TTL value panics `dec_ts`
  (`ttl.rs:43-45` `try_into().unwrap()`) on later reads. Reached directly from MCP tool `load`.
- **M20. `load` allocation from unvalidated length** — `migrate.rs:291`
  `Vec::with_capacity(nf)` with attacker-controlled u32 → ~100 GB alloc abort from a ~40-byte
  dump (the value decoder caps correctly; migrate forgot). Same ingress: **unbounded recursion**
  in `Value::decode`/`encode`/drop (`value.rs:146-268`) — nested-array dumps stack-overflow the
  process (serde_json guards the JSON path at depth 128; the dump path has nothing).
- **M21. `backup` onto an existing path merges instead of replacing** — `store.rs:153-188`:
  reded `Database::create` opens-or-creates (this repo relies on that in `Store::open`);
  backing up over last night's backup resurrects deleted records and desyncs META counters.
  Doc says "created/overwritten".

### Robustness / crashes
- **M22. `Collection::delete` lacks `ensure_writable`** — `db.rs:398-409` (every other write path
  checks, e.g. `db.rs:230`): users can delete from `__edges__*`, `__schemas__`, `__indexes__`,
  `__scalar__*`, … directly, desynchronizing primary vs derived state through the sanctioned API.
  Corollaries: `Collection::set_ttl`/`purge_expired` (`ttl.rs:182-204`) and `unlink`
  (`graph.rs:65-73`) skip the guard too; reads (`get`/`page`) accept reserved names (asymmetry
  is what makes M19 exploitable).
- **M23. MCP: `search` has no default cap; `page`/`phrase_search` accept unbounded caller limits**
  — `server.rs:198-267` applies `limit` only if present (engine default `None` =
  materialize-everything, `builder.rs:907-911, 966-977`); `uint_param` accepts any u64
  (`server.rs:613-618`). One `search {"collection":"docs"}` serializes the whole DB into one
  JSON-RPC line. DESIGN.md:213 claims tools come "with default result caps" — enforced on
  neighbors/traverse/geo/join only, applied **post-hoc** after full computation.
- **M24. MCP transport: one invalid UTF-8 byte kills the server; no frame limit** —
  `protocol.rs:35-51`: `reader.lines()` yields `Err(InvalidData)` which propagates out of `run`
  → process exit (malformed *JSON* recovers with −32700; malformed *bytes* don't); unbounded
  line length = memory DoS.
- **M25. Deleting a document orphans all its graph edges** — `maintain_delete` (`db.rs:187-195`)
  knows nothing about `__edges__`; `neighbors`/`traverse` happily return ghost nodes forever.
  For the advertised agent-memory/entity-graph use case this is a silent consistency hole (at
  minimum undocumented; ideally cascade cleanup). `link` also accepts nonexistent endpoints.

---

## 2. MINOR

Engine:
- `bulk` leaks relaxed durability across a panic — `db.rs:100-112`, flag stays `true` after unwind.
- Committed-writes-reported-as-Err + missed events on maintenance failure — `db.rs:229-240,
  292-326, 354-375`.
- Graph mutations emit no change events though they're document writes — `graph.rs` vs `reactive.rs:48`.
- Cross-writer event ordering unspecified/unorderable (no sequence number) — `reactive.rs:69-82`.
- Ambiguous namespace encodings collide: `ns("a","b__c") == ns("a__b","c")` (`scalar.rs:57-64`,
  same pattern `__geo__`/`__cscalar__`/def_key splitting at first NUL `index.rs:417-430`) —
  legal user names can fuse two indexes into one storage namespace.
- Corrupt persisted defs silently dropped/downgraded, changing enforcement: schemas skipped
  (`schema.rs:190-199`), unknown text-kind byte → InMemory (`fts.rs:45-50`), malformed index-def
  rows skipped without warning (`index.rs:180-215`) — policy says "skipped with a warning"; there
  is no logging anywhere (see docs drift).
- `disk_fts` decoders allocate from unvalidated length prefixes (`Vec::with_capacity(n_pos)`
  up to ~16 GB, `disk_fts.rs:75-119`) — contrast `value.rs`'s deliberate `min(1024)/min(4096)`.
- Phrase positions assigned after stop-word removal → phrases match across removed stop words
  (`text.rs:113-119`, consistent across paths but undocumented; Lucene keeps position gaps).
- No max-token-length guard; CJK text becomes one giant token → silent recall collapse;
  10 MB blob = 10 MB posting strings (`text.rs:30-35`).
- `Analyzer` is dead public API: documented as the opt-out mechanism, accepted by no API
  (`text.rs:82-84`); `Analyzer::raw` referenced only by its own test.
- `Bm25Params` fields unvalidated (doc says b ∈ [0,1]; −3 accepted) — `text.rs:13-14, 146`.
- Antimeridian: `geo_within_bbox` verification `(min_lon..=max_lon).contains` is vacuous for
  wrapped boxes (silent empty; `geo.rs:158-159`); indexed bbox results are in cell order, not the
  documented key order (`geo_index.rs` scan vs `geo.rs:144-164`); `ROW_CAP`'s row-half is dead
  (lat spans ≤ 1800 < 4096 cells).
- Cosine: zero-norm returns 1.0 (orthogonal), contradicting "[0,2] maximally distant" docs
  (`distance.rs:13-15, 83-93`); huge magnitudes overflow to NaN (silently vanishing nodes).
- `disk_hnsw` corrupt keymap value → tombstones node 0 / reports success
  (`unwrap_or([0;8])`, `disk_hnsw.rs:393, 491`).
- Scalar quantization: NaN component clamps to 0 == min (silent distortion) — `scalar.rs:132-153`.
- PQ k-means seeds duplicate when n < k and can never diverge (empty clusters keep centroids) —
  `pq.rs:228-239`.
- Schema evolution is replace-only: no revalidation API, duplicate field names accepted silently
  (`schema.rs:202-208, 302-308`).
- `ne` on missing field is `false` while `!eq` is `true` — asymmetric closed-world semantics,
  pinned by test but undocumented (`filter.rs:113-115`); `Not` is never pushed into index
  selection (correct, just worth stating).
- `order_by`: incomparable present values interleave by key; doc says they sort last
  (`builder.rs:181-184` vs `891-904`).
- `explain()` always prints `scan(...)` regardless of the index path actually taken; prints MMR
  with no vector source; can't distinguish sub-linear plans from scans (`builder.rs:795-831`).
- RRF `k` and MMR λ unvalidated (negative k → inf scores; λ<0 rewards similarity) —
  `fusion.rs:32,57`, `builder.rs:155-166`.
- Streaming top-k bound is really `min(4k, matches)`; "~4k" doc overstated (`builder.rs:435-439`).
- Hybrid fusion silently drops filter-passing docs that no source can rank (RRF = union of lists)
  — undocumented (`builder.rs:863-876`).
- `plan()` identity is a Debug-format string (fragile across refactors; embeds full query
  vectors; PlanCache has no eviction) and `PlanCache<V>`'s "never serves a stale answer" doc is
  falsified by the crate's own result-caching test (`plan.rs:10-13, 169-190`).
- `patch`/`update` are non-atomic get-then-insert (documented, but sits next to CAS in MCP/GUIDE
  without caveat) — `db.rs:247-275`.
- `count() as usize` truncates >4 GiB on 32-bit; dump `put_u32` silently truncates lengths
  (>4 GiB) — `db.rs:343`, `migrate.rs:27-29`; `value.rs:204` panics instead (documented, but
  violates the never-panic rule).
- `−0.0`/`0.0` and NaN payloads break value.rs's byte-determinism claim
  (CAS with `Float(-0.0)` fails against stored `+0.0` although `==` holds) — `value.rs:7-11`.
- Semantic cache inherits ANN approximation silently: nearest-within-threshold can be missed or
  threshold-crossed via quantized distances (`semantic_cache.rs:61-71`).

Sidecar / protocol:
- Integers beyond i64 silently become lossy floats; non-finite floats silently become JSON null
  (`convert.rs:23-26, 57-83`).
- Invalid `limit` shapes (-5, 2.5) silently fall back to default on list tools but error on
  search/page — inconsistent (`server.rs:606-611`).
- JSON-RPC gaps: batch arrays silently swallowed; bare scalars treated as notifications;
  `jsonrpc` version never validated; `"notifications/initialized"` with id never answered;
  no initialize version/capability negotiation (`protocol.rs:22-92`).
- Server-side IO failures reported as `BadParams`; raw OS/redb internals leak into client-visible
  `isError` text (`server.rs:405-414`, `error.rs:17-42`).
- Unknown argument keys silently ignored everywhere (typo'd params become defaults).
- Keys rendered via `from_utf8_lossy` (non-UTF-8 keys corrupted, unusable as cursors).

CI / release / docs:
- README.md:113 & CHANGELOG.md:55 claim iOS cross-compile ✅ — no iOS job exists; the android job
  installs no NDK and links nothing (type-check only).
- No MSRV gate despite rust-version 1.88 (named custom profiles stabilized in Cargo 1.82, so the
  manifest itself parses on 1.88 — the profile is *not* an MSRV violation; the missing gate is).
- release.yml creates the release before builds; fail-fast:false can leave an incomplete permanent
  tag; no tag↔version check; no checksums; workflow-wide contents:write.
- ci.yml has no timeout-minutes/concurrency group.
- GUIDE.md: patch described as merging into existing doc (code creates-if-absent); "zero sources…
  bounded memory" only true with a limit; MSRV claimed, never tested.
- DESIGN.md stale: v0.1 cut still lists three VFS backends incl. OPFS and "L5: none"; zstd
  compression specified but no zstd dependency exists; `tracing` observability promised, no
  logging exists anywhere.
- corvid-wasm exposes exactly one smoke symbol (no wasm-bindgen API); README's WASM ✅ row
  oversells; the ≈0.2 MB figure is asserted, never measured in-repo (CI enforces only 2 MB).

## 3. NITS (selected)

`unlink` asymmetric guard; `insert_auto` burns an id on failed insert; `delete` of a
non-indexed doc rewrites disk-FTS META; scalar cap off-by-one (`> cap` admits cap+1);
`fp_rate` silently clamps to 0.5 below range; binary quantization maps −0.0 to positive sign;
`approx_distinct` counts `Int(1)`/`Float(1.0)` distinctly and only top-level fields;
disk_hnsw mixes BE ids with LE u32 lengths; `uint_param` u64→usize truncates on 32-bit;
S-stemmer quirks ('goes'→'goe', 'boxes'≠'box'); no lat/lon range validation in `extract_point`.

---

## 4. Docs/comments the code does not uphold (front-page items)

1. `lib.rs:13` / `index.rs:13` / `fts.rs:11` / `store.rs:17-21` / `quant.rs:5-6` — "never sees a
   stale index", "crash can only lose in-memory state", "recall behaviour matches" (C0, M1, M13).
2. DESIGN.md:213 — sidecar tools "with default result caps" (M23). DESIGN.md:215 —
   "single-snapshot join" (M2); "hash + nested loop join" — shipped join is a single-field
   key-lookup left-outer join.
3. `distance.rs:27-29` — "The search layer validates dimensions" (M10). `distance.rs:13-15` —
   zero-norm "maximally distant" (Minor).
4. `query.rs:4-5,139` — "Both searches are exact… ties break by key order" (ANN branch does
   neither).
5. `scalar.rs:16-19` — "a range scan never excludes a true match" (±0.0, M6).
6. `geo_index.rs:18-21,139-141` — superset premise falsified by bbox scanning and cos()
   underestimate (M7, M8).
7. `value.rs:67-71` — uniform field accessor (geo/join/sketch use top-level get, M9).
8. `schema.rs:8-10` — strict unique enforcement (M3). `store.rs:157-158` — backup
   "created/overwritten" (M21).
9. `join.rs:28-34` — Text/Bytes-only FKs; one-snapshot resolution (M2).
10. `plan.rs:11-13` — PlanCache staleness claim (Minor).
11. `reactive.rs:48` — "invoked on every subsequent insert/delete" (graph writes and failed
    maintenance emit nothing).
12. `fusion.rs:46` — λ ∈ [0,1] unenforced. `builder.rs:436` — "~4k" bound.
13. README iOS/WASM rows; GUIDE patch/bounded-memory rows; DESIGN zstd/tracing/v0.1-cut (above).

## 5. Verified clean (for confidence)

Tests/build: 390 tests green, clippy `-D warnings` clean, CI gates exist and can fail
(fmt/clippy/doc-lint/llvm-cov ≥90%/wasm ≤2 MiB gzip). Tool surface: exactly the 27 documented
tools, no drift. compare_and_set atomic; graph link/unlink forward+reverse atomic;
next_auto_id reservation atomic; RRF/MMR math and tie-breaks correct; BM25 baseline↔index
pointwise bitwise-consistent (incl. idf variant, avg_len guard, empty-doc parity);
haversine textbook-correct (IUGG radius, clamped); true-predicate contract honored on all
non-approx vector/text/scalar/compound/OR paths (full re-verification everywhere);
Ne/mixed-lane correctly disable index service; NOT conservatively unserviced; cursor pagination
encoding correct; k=0/limit(0)/empty-vector edges guarded; no stdout pollution; serde_json depth
guard covers JSON ingress; no unsafe (`#![forbid(unsafe_code)]`); no HashMap iteration order
leaks into any result ordering; level assignment deterministic in both HNSW engines; epoch-stamp
visited-set reset sound; ADC formula equals reconstruction distance by hand-check.

*Note: web_search was unavailable this session; the one external fact relied upon from model
knowledge is Cargo named-profiles stabilizing in 1.82 (< declared MSRV 1.88).*
