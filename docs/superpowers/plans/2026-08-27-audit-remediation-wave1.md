# Audit Remediation Wave 1 — Correctness Hot-Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the five independent correctness fixes from the audit (A1 stale ANN node, A3 unique-constraint bypass, A4 PQ panic/zero-table, B1 bulk durability leak, D4 group-key format) as five test-first commits.

**Architecture:** No structural changes. Each fix touches one subsystem, is developed test-first, and lands as its own commit on `master`. Index/unique/PQ fixes preserve existing public signatures except `Pq::l2_table` (returns `Option`, crate-internal callers only).

**Tech Stack:** Rust 2024 edition, redb 4.1, std-only concurrency primitives (`thread_local!`).

**Spec:** `docs/superpowers/specs/2026-08-27-audit-remediation-design.md` — Wave 1 section and Decisions 3/6. Read both before starting.

## Global Constraints

- `cargo test --workspace` green before every commit (project rule, `CLAUDE.md`).
- `cargo clippy --all-targets --workspace -- -D warnings` clean before every commit.
- `cargo fmt --all` before every commit (CI checks `--check`).
- TDD: write the failing test, watch it fail, then implement.
- Never panic on user input; errors are typed via `thiserror` (`CLAUDE.md`).
- Coverage ≥ 90% line coverage must hold (`cargo llvm-cov --workspace --fail-under-lines 90`); verified once at wave exit (Task 6).
- Line anchors (e.g. `index.rs:108`) are from commit `08d05c2` and may drift; the quoted code is authoritative.

---

### Task 1: Tombstone before the dimension guard in `BuiltIndex::add` (A1)

**Files:**
- Modify: `crates/corvid/src/index.rs:104-119` (`BuiltIndex::add`)
- Test: `crates/corvid/src/index.rs` (inline `mod tests`, helper `doc(v)` already exists)

**Interfaces:**
- Consumes: nothing new.
- Produces: no signature changes; behavior fix only.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `index.rs` (the helper `fn doc(v: Vec<f32>) -> Value` with field `"embedding"` already exists in that module):

```rust
/// Regression (audit A1): overwriting an indexed document with a
/// different-dimension vector used to leave the old node live, so ANN
/// results diverged from exact search. The old node must be tombstoned.
#[test]
fn overwrite_with_different_dimension_tombstones_old_node() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
    c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
    c.create_vector_index("embedding", Metric::Cosine).unwrap();
    // Force the lazy build to run.
    let _ = c.vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine).unwrap();
    // Overwrite "a" with a 3-dim vector (plain overwrite; no schema).
    c.insert(b"a", &doc(vec![1.0, 0.0, 0.0])).unwrap();
    // A 2-dim query must not return "a" — parity with the exact path,
    // which skips dimension-mismatched documents.
    let hits = c.vector_search("embedding", &[1.0, 0.0], 2, Metric::Cosine).unwrap();
    assert!(
        hits.iter().all(|h| h.key != b"a".to_vec()),
        "stale node for 'a' still served: {hits:?}"
    );
    assert_eq!(hits.len(), 1, "only 'b' should remain");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p corvid overwrite_with_different_dimension`
Expected: FAIL — the assertion fires because `"a"` is still returned from the live old node (before the fix, `add` returns from the dimension guard before `tombstone`).

- [ ] **Step 3: Implement the fix**

Replace `BuiltIndex::add` in `index.rs` (currently at lines 104-119) with:

```rust
    /// Add (or replace) `key`'s vector. An existing node for `key` is
    /// tombstoned first — even when the new vector is skipped below, the old
    /// node must not stay live (a live stale node is exactly what the exact
    /// paths never show). Vectors whose dimension differs from the index's
    /// fixed dimension are skipped, matching the exact-search paths (which
    /// skip them too) — the document stays queryable by everything else.
    fn add(&mut self, key: &[u8], vector: Vec<f32>) {
        self.tombstone(key);
        match self.dim {
            Some(d) if d != vector.len() => return,
            None => self.dim = Some(vector.len()),
            _ => {}
        }
        let id = self.hnsw.insert(vector);
        debug_assert_eq!(id, self.node_to_key.len(), "hnsw ids are dense");
        self.node_to_key.push(Some(key.to_vec()));
        self.key_to_node.insert(key.to_vec(), id);
    }
```

(The only change: `self.tombstone(key);` moved above the `match`, and the doc comment updated.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p corvid overwrite_with_different_dimension`
Expected: PASS.

Run: `cargo test -p corvid`
Expected: all tests PASS (the pre-existing wrong-dimension regression tests from commit 166e0f4 must stay green).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
git add crates/corvid/src/index.rs
git commit -m "ANN: tombstone old node before the dimension guard (audit A1)

Overwriting an indexed document with a different-dimension vector left the
old node live, so ANN results diverged from exact search. Tombstone first;
skip only the insert."
```

---

### Task 2: Unique constraints enforced for non-scalar values and NaN (A3)

**Files:**
- Modify: `crates/corvid/src/schema.rs:258-332` (`validate_unique_in_txn` + new helpers)
- Test: `crates/corvid/src/schema.rs` (inline `mod tests`; helper `fn doc(pairs: &[(&str, Value)]) -> Value` already exists)

**Interfaces:**
- Consumes: `crate::scalar::encode_value` (unchanged), `WriteBatch::scan` (unchanged).
- Produces: two private helpers in `schema.rs`: `unique_value_eq(a: &Value, b: &Value) -> bool` and `unique_scan_in_txn(tx, collection, key, field, value) -> Result<()>`. No public signature changes.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `schema.rs`:

```rust
/// Regression (audit A3): with a scalar index on a unique field whose values
/// are not index-encodable (Bytes/Array/Map/Vector), the constraint was
/// silently skipped. It must fall back to the scan comparison.
#[test]
fn unique_bytes_field_is_enforced_even_with_scalar_index() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("users");
    c.set_schema(&Schema::new().field(Field::new("blob", FieldType::Bytes).unique()))
        .unwrap();
    c.create_scalar_index("blob").unwrap();
    c.insert(b"u1", &doc(&[("blob", Value::Bytes(vec![1, 2, 3]))]))
        .unwrap();
    let err = c.insert(b"u2", &doc(&[("blob", Value::Bytes(vec![1, 2, 3]))]));
    assert!(
        matches!(err, Err(Error::SchemaViolation(_))),
        "duplicate unique Bytes value must be rejected"
    );
    // A different value is still fine.
    c.insert(b"u3", &doc(&[("blob", Value::Bytes(vec![4]))]))
        .unwrap();
    assert_eq!(c.len().unwrap(), 2);
}

/// Regression (audit A3): NaN never conflicted with NaN on a unique Float
/// field (IEEE `!=`). For uniqueness, NaN is the same stored value as NaN.
#[test]
fn unique_float_nan_conflicts_with_nan() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("users");
    c.set_schema(&Schema::new().field(Field::new("x", FieldType::Float).unique()))
        .unwrap();
    c.insert(b"a", &doc(&[("x", Value::Float(f64::NAN))])).unwrap();
    let err = c.insert(b"b", &doc(&[("x", Value::Float(f64::NAN))]));
    assert!(
        matches!(err, Err(Error::SchemaViolation(_))),
        "duplicate NaN on a unique field must be rejected"
    );
    // Still true when a scalar index exists (NaN is not bucket-walkable).
    c.create_scalar_index("x").unwrap();
    c.delete(b"a").unwrap();
    c.insert(b"c", &doc(&[("x", Value::Float(f64::NAN))])).unwrap();
    let err = c.insert(b"d", &doc(&[("x", Value::Float(f64::NAN))]));
    assert!(matches!(err, Err(Error::SchemaViolation(_))));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p corvid unique_bytes_field_is_enforced_even_with_scalar_index unique_float` — if the runner rejects two filters, run them separately.
Expected: both FAIL — the first because the duplicate insert returns `Ok(())` (the `continue` skip); the second because NaN `!=` NaN so no conflict is found.

- [ ] **Step 3: Implement the fix**

In `schema.rs`, add these two private helpers (above `impl Db`):

```rust
/// Value equality for unique constraints: like `PartialEq`, except NaN
/// equals NaN (uniqueness is about identity of stored values, not IEEE
/// ordering) and containers compare element-wise under the same rule.
fn unique_value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| unique_value_eq(x, y))
        }
        (Value::Map(xs), Value::Map(ys)) => {
            xs.len() == ys.len()
                && xs.iter()
                    .zip(ys.iter())
                    .all(|((kx, vx), (ky, vy))| kx == ky && unique_value_eq(vx, vy))
        }
        _ => a == b,
    }
}

/// Scan-side unique check inside the caller's write transaction: reject any
/// *other* key whose value at `field` equals `value` under
/// [`unique_value_eq`]. Used when no scalar index serves the field, when the
/// value is not index-encodable (containers), and for NaN (whose encoded
/// bucket key is not guaranteed to collide).
fn unique_scan_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    collection: &str,
    key: &[u8],
    field: &str,
    value: &Value,
) -> Result<()> {
    for (k, bytes) in tx.scan(collection)? {
        if k == key {
            continue;
        }
        let d = Value::decode(&bytes)?;
        if d.get_path(field).is_some_and(|v| unique_value_eq(v, value)) {
            return Err(Error::SchemaViolation(format!(
                "field '{field}' must be unique; value already exists"
            )));
        }
    }
    Ok(())
}
```

Then rewrite `validate_unique_in_txn` (currently `schema.rs:262-332`) so the indexed branch falls through to the scan instead of skipping, and the no-index branch uses the helper. Replace everything from `if self.has_scalar_index(collection, &f.name) {` through the end of the `else` block with:

```rust
            let nan = matches!(value, Value::Float(x) if x.is_nan());
            match if nan {
                None
            } else {
                crate::scalar::encode_value(value)
            } {
                Some(enc) if self.has_scalar_index(collection, &f.name) => {
                    // Walk exactly this value's bucket: index keys are
                    // `encoded_value ‖ doc_key`, so everything from `enc`
                    // until the first non-prefixed key shares the value.
                    let ns = crate::scalar::namespace(collection, &f.name);
                    let mut cursor = enc.clone();
                    'bucket: loop {
                        let page = tx.scan_from(&ns, &cursor, 256)?;
                        if page.is_empty() {
                            break 'bucket;
                        }
                        let mut next = None;
                        for (k, _) in &page {
                            if !k.starts_with(&enc) {
                                break 'bucket;
                            }
                            let doc_key = &k[enc.len()..];
                            if doc_key != key {
                                return Err(conflict_msg());
                            }
                            next = Some(k.clone());
                        }
                        match next {
                            Some(mut c) => {
                                c.push(0);
                                cursor = c;
                            }
                            None => break 'bucket,
                        }
                    }
                }
                _ => unique_scan_in_txn(tx, collection, key, &f.name, value)?,
            }
```

(The `conflict_msg` closure and the `for f in schema.fields.iter().filter(|f| f.unique)` loop head stay exactly as they are; only the branch structure changes. The `let ns = ...` line replaces the previous `let Some(enc) = ... else { continue; }` handling.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p corvid unique_bytes_field unique_float` (or separately).
Expected: both PASS.

Run: `cargo test -p corvid`
Expected: all PASS — in particular `enforces_uniqueness`, `batch_unique_violation_is_atomic`, `concurrent_unique_inserts_allow_at_most_one_winner`, and `uniqueness_uses_scalar_index_when_present` must stay green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
git add crates/corvid/src/schema.rs
git commit -m "Schema: enforce unique on non-index-encodable values and NaN (audit A3)

With a scalar index on a unique field, values encode_value cannot encode
(Bytes/Array/Map/Vector) silently skipped the constraint, and NaN never
conflicted with NaN. Fall through to an in-txn scan comparison with
NaN-equals-NaN semantics; creating an index no longer weakens uniqueness."
```

---

### Task 3: PQ `adc_l2` bounds guard and `l2_table` optionality (A4)

**Files:**
- Modify: `crates/corvid/src/pq.rs:119-152` (`l2_table`, `adc_l2`)
- Modify: `crates/corvid/src/disk_hnsw.rs:76-84` (`pq_probe`), `:576-581` (`search` dimension gate region)
- Test: `crates/corvid/src/pq.rs` (inline `mod tests`; helper `clustered(n, dim, clusters)` already exists), `crates/corvid/src/index.rs` (one regression pin)

**Interfaces:**
- Consumes: `Pq::dim()` (`pq.rs:42`, exists), `Pq::distance` (unchanged).
- Produces: `Pq::l2_table(&self, query: &[f32]) -> Option<Vec<f32>>` — `None` when `query.len() != self.dim`. `adc_l2` signature unchanged (returns `f32::INFINITY` for out-of-range codes). `DiskParams::pq_probe` signature unchanged (falls back to `DProbe::Pq`).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `pq.rs`:

```rust
/// Regression (audit A4): a code byte outside [0, k) used to index the ADC
/// table out of bounds (panic from the query path). Such a node must score
/// INFINITY — never rank, never panic.
#[test]
fn adc_l2_rejects_out_of_range_codes() {
    let data = clustered(100, 8, 5);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let table = pq.l2_table(&data[0]).unwrap();
    assert_eq!(pq.adc_l2(&table, &[255, 255, 255, 255]), f32::INFINITY);
    // In-range codes still score finitely.
    let code = pq.encode(&data[1]);
    assert!(pq.adc_l2(&table, &code).is_finite());
}

/// Regression (audit A4): a dimension-mismatched query used to get an
/// all-zero table (every node distance 0). It must get `None` so callers
/// can decline to serve or fall back.
#[test]
fn l2_table_returns_none_on_dimension_mismatch() {
    let data = clustered(50, 8, 4);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let wrong: Vec<f32> = (0..pq.dim() + 1).map(|i| i as f32).collect();
    assert!(pq.l2_table(&wrong).is_none());
    assert!(pq.l2_table(&data[0]).is_some());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p corvid adc_l2_rejects l2_table_returns_none` (or separately).
Expected: the first FAILS with an index-out-of-bounds panic; the second FAILS because `l2_table` returns a (zero) table, not `None` — note the second may not compile until the signature changes; if so, that compile error *is* the observed failure; proceed to Step 3.

- [ ] **Step 3: Implement the fix**

In `pq.rs`, replace `l2_table` and `adc_l2` (currently at lines 119-152) with:

```rust
    /// Precompute the L2 asymmetric-distance table for `query`:
    /// `table[s * k + c]` is the squared-L2 distance from `query`'s subvector
    /// `s` to centroid `c`. Use with [`Pq::adc_l2`] for fast L2 scoring.
    /// Returns `None` when the query dimension does not match the codebook —
    /// a mismatched query has no meaningful table, and an all-zero one would
    /// score every node at distance 0.
    pub fn l2_table(&self, query: &[f32]) -> Option<Vec<f32>> {
        if query.len() != self.dim {
            return None;
        }
        let mut table = vec![0.0f32; self.m * self.k];
        for s in 0..self.m {
            let off = s * self.sub_dim;
            let qsub = &query[off..off + self.sub_dim];
            let cb = &self.codebooks[s];
            for c in 0..self.k {
                let cs = c * self.sub_dim;
                let mut d = 0.0f32;
                for j in 0..self.sub_dim {
                    let diff = qsub[j] - cb[cs + j];
                    d += diff * diff;
                }
                table[s * self.k + c] = d;
            }
        }
        Some(table)
    }

    /// Squared-L2 distance from a coded vector to the query whose `table`
    /// was built with [`Pq::l2_table`] — a sum of `m` table lookups. A code
    /// byte outside `[0, k)` (corrupt or foreign-format state) scores
    /// `INFINITY`: the node never ranks and nothing panics.
    pub fn adc_l2(&self, table: &[f32], code: &[u8]) -> f32 {
        let mut sum = 0.0f32;
        for (s, &c) in code.iter().enumerate().take(self.m) {
            let c = c as usize;
            if c >= self.k {
                return f32::INFINITY;
            }
            sum += table[s * self.k + c];
        }
        sum
    }
```

Update the two existing test call sites in `pq.rs` to unwrap: line ~352 `let table = pq.l2_table(q);` → `let table = pq.l2_table(q).unwrap();` and line ~382 `let table = pq.l2_table(q);` → `let table = pq.l2_table(q).unwrap();`.

In `disk_hnsw.rs`, change `pq_probe` (currently at lines 76-84) to fall back to the reconstruction probe when no table can be built:

```rust
    /// Build the PQ probe for a query: under L2, precompute the asymmetric-
    /// distance table once (so each node is O(m) table lookups instead of an
    /// O(dim) reconstruct + distance); other metrics keep the query vector.
    /// If the table cannot be built (dimension mismatch), fall back to the
    /// reconstruction probe — slower, but correct; `search` declines
    /// mismatched queries up front, so this is defense in depth.
    fn pq_probe(pq: &Pq, metric: Metric, query: Vec<f32>) -> DProbe {
        match metric {
            Metric::L2 => match pq.l2_table(&query) {
                Some(table) => DProbe::PqAdc(table),
                None => DProbe::Pq(query),
            },
            _ => DProbe::Pq(query),
        }
    }
```

And in `search` (`disk_hnsw.rs`), extend the dimension gate (currently lines 576-581) so a PQ index also requires the query to match its codebook:

```rust
        // Dimension gate (unset on legacy namespaces → accept-all).
        if let Some(d) = meta.dim
            && d as usize != query.len()
        {
            return Ok(None);
        }
        // A PQ index can only serve queries matching its codebook dimension
        // (covers legacy namespaces whose meta.dim is unset).
        if let Some(pq) = &p.pq
            && pq.dim() != query.len()
        {
            return Ok(None);
        }
```

- [ ] **Step 4: Add a regression pin in `index.rs` tests**

This behavior (wrong-dim query on an on-disk PQ index returns exact-path
results) already holds via the meta gate; this test pins it end-to-end and
protects the new codebook gate's interaction:

```rust
/// A query whose dimension mismatches an on-disk PQ index falls back to the
/// exact path — same results as an unindexed collection (audit A4 pin).
#[test]
fn pq_index_dimension_mismatch_falls_back_to_exact() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    let corpus = pq_corpus(40, 8);
    for (i, v) in corpus.iter().enumerate() {
        c.insert(&(i as u32).to_le_bytes(), &doc(v.clone())).unwrap();
    }
    c.create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16)
        .unwrap();
    let wrong = vec![0.5f32; 7];
    let hits = c.vector_search("embedding", &wrong, 5, Metric::L2).unwrap();
    assert!(hits.is_empty(), "no 7-dim vectors exist");
    // The correct dimension still serves via the index.
    let hits = c.vector_search("embedding", &corpus[0], 5, Metric::L2).unwrap();
    assert!(!hits.is_empty());
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p corvid adc_l2_rejects l2_table_returns_none pq_index_dimension` (or separately)
Expected: all PASS.

Run: `cargo test -p corvid`
Expected: all PASS (including `adc_l2_matches_reconstruction_distance` and `pq_ranking_recall_vs_exact` after the `.unwrap()` updates).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
git add crates/corvid/src/pq.rs crates/corvid/src/disk_hnsw.rs crates/corvid/src/index.rs
git commit -m "PQ: bound-check ADC codes, option the L2 table, gate codebook dim (audit A4)

adc_l2 indexed its table with an unchecked code byte (panic on corrupt or
foreign-format state); out-of-range codes now score INFINITY. l2_table
returned an all-zero table for mismatched dimensions; it now returns None,
disk_hnsw falls back to the reconstruction probe, and search declines
queries that mismatch the PQ codebook dimension."
```

---

### Task 4: Thread-local, panic-safe bulk durability scope (B1)

**Files:**
- Modify: `crates/corvid/src/store.rs:44-51` (struct area), `:237-257` (`transaction`), new `BulkScope`
- Modify: `crates/corvid/src/db.rs:93-112` (`Db::bulk`)
- Test: `crates/corvid/src/store.rs` (inline `mod tests`; helper `fn mem() -> Store` already exists)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct BulkScope` and `pub fn begin_bulk(&self) -> BulkScope` on `Store` (public; `Store` is re-exported from the crate root). `Store::set_relaxed_durability` stays as the explicit whole-store opt-in.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `store.rs`:

```rust
    /// Regression (audit B1): relaxed durability leaked to threads not
    /// inside the bulk scope, and a panicking bulk closure left it on
    /// forever. Bulk scope is thread-local and panic-safe (RAII).
    #[test]
    fn bulk_scope_relaxes_only_the_bulk_thread() {
        let s = Arc::new(mem());
        let _scope = s.begin_bulk();
        assert!(s.bulk_active_on_this_thread());
        let other = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.bulk_active_on_this_thread())
                .join()
                .unwrap()
        };
        assert!(!other, "concurrent writer must keep durable commits");
    }

    #[test]
    fn panicking_bulk_closure_restores_durability() {
        let s = mem();
        let boomed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = s.begin_bulk();
            panic!("bulk closure failed mid-load");
        }));
        assert!(boomed.is_err());
        assert!(!s.bulk_active_on_this_thread());
        // And a subsequent ordinary transaction is durable again (flag-wise).
        s.put("docs", b"k", b"v").unwrap();
        assert!(!s.bulk_active_on_this_thread());
    }
```

(Add `use std::sync::Arc;` to the test module imports if not present — `Arc` is already used in the backup tests at the top of the module via fully-qualified paths; a local `use` in the test fns is fine too.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p corvid bulk_scope_relaxes panicking_bulk`
Expected: compile error — `begin_bulk` / `bulk_active_on_this_thread` do not exist. That compile failure is the observed red state; proceed.

- [ ] **Step 3: Implement the fix**

In `store.rs`, add after the `Store` struct definition (around line 51):

```rust
thread_local! {
    /// Depth of bulk-load scopes on *this* thread. Write transactions relax
    /// durability only when the issuing thread is inside a bulk scope (plus
    /// the explicit whole-store opt-in), so a bulk load on one thread never
    /// silently degrades concurrent writers' durability.
    static BULK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Guard marking the current thread as inside a bulk load (relaxed
/// durability for write transactions started here). Dropping it — normally
/// or on unwind — restores durable commits. Created by [`Store::begin_bulk`].
pub struct BulkScope;

impl Drop for BulkScope {
    fn drop(&mut self) {
        BULK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}
```

Inside `impl Store`, add:

```rust
    /// Enter a bulk-load scope on the current thread. Write transactions
    /// started on this thread skip the per-commit fsync until the returned
    /// guard drops; make them durable with [`Store::flush`].
    pub fn begin_bulk(&self) -> BulkScope {
        BULK_DEPTH.with(|d| d.set(d.get() + 1));
        BulkScope
    }

    /// Whether write transactions on the current thread would relax
    /// durability (bulk scope active or explicit opt-in).
    fn bulk_active_on_this_thread(&self) -> bool {
        self.relaxed.load(std::sync::atomic::Ordering::Relaxed)
            || BULK_DEPTH.with(|d| d.get() > 0)
    }
```

Change the relax check at the top of `Store::transaction` (currently lines 246-250) from:

```rust
        if self.relaxed.load(std::sync::atomic::Ordering::Relaxed) {
```

to:

```rust
        if self.bulk_active_on_this_thread() {
```

(`flush` needs no change: it never sets `Durability::None`, so the closing flush is durable regardless of scope.)

In `db.rs`, change `Db::bulk` (currently lines 100-112) to:

```rust
    pub fn bulk<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        // Relaxed durability applies only to transactions begun on this
        // thread, and the scope is panic-safe (RAII).
        let _scope = self.store.begin_bulk();
        let result = f();
        // Make the bulk writes durable even if `f` failed partway.
        let flush = self.store.flush();
        let out = result?;
        flush?;
        Ok(out)
    }
```

(Keep the existing doc comment on `bulk`, adding one line: "Relaxed durability applies only to write transactions on the calling thread; concurrent writers are unaffected. The scope is panic-safe.")

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p corvid bulk_scope_relaxes panicking_bulk`
Expected: both PASS.

Run: `cargo test -p corvid`
Expected: all PASS — `bulk_load_is_durable_after_flush` (db.rs) must stay green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
git add crates/corvid/src/store.rs crates/corvid/src/db.rs
git commit -m "bulk: thread-local, panic-safe relaxed-durability scope (audit B1)

Db::bulk flipped a process-wide flag: a panicking closure left every later
write non-durable, and concurrent writers silently lost fsync during a bulk
load. The scope is now a thread-local depth with an RAII guard; flush is
unaffected (always immediate durability)."
```

---

### Task 5: Canonical group keys — bare text, tagged non-text (D4, Decision 3)

**Files:**
- Modify: `crates/corvid/src/builder.rs:1033-1048` (`group_key`), `:236-241` and `:331-345` (doc comments on `group_count` / `group_sum`)
- Modify: `crates/corvid/src/builder.rs` tests (assertions using `"s:…"` keys)
- Modify: `CHANGELOG.md`
- Check (modify only if they show prefixed keys): `docs/GUIDE.md`, `site/index.html`, `README.md`

**Interfaces:**
- Consumes: nothing new.
- Produces: the canonical group-key form used by `group_count`, `group_sum`, `group_avg`, `count_distinct`: **text bare; int/float/bool tagged `i:`/`f:`/`b:`; any text starting with `i:`, `f:`, `b:`, or `t:` escaped with a `t:` prefix.** Breaking change vs the current `s:`-prefixed-everything form (pre-1.0, allowed).

- [ ] **Step 1: Update the tests first**

In `builder.rs` tests, change the assertions that encode the old form:

- `aggregations_global_and_grouped`: `gs.get("s:a")` → `gs.get("a")`; `gs.get("s:b")` → `gs.get("b")`; `gs.get("s:c")` → `gs.get("c")`; `ga.get("s:a")` → `ga.get("a")`; `ga.get("s:b")` → `ga.get("b")`.
- `group_count_buckets_by_field`: `groups.get("s:blog")` → `groups.get("blog")`; `groups.get("s:news")` → `groups.get("news")`.
- `group_count_skips_missing_and_container_fields`: `groups.get("s:blog")` → `groups.get("blog")`.
- `group_count_respects_filters`: `groups.get("s:blog")` → `groups.get("blog")`.
- `group_keys_are_typed_so_distinct_types_stay_distinct`: `groups.get("s:1")` → `groups.get("1")`, and extend the value list + assertions to pin the escape rule:

```rust
        for (i, v) in [
            Value::Text("1".into()),
            Value::Text("i:1".into()), // ambiguous with Int tag → escaped
            Value::Int(1),
            Value::Float(1.0),
            Value::Float(-0.0),
            Value::Float(0.0),
        ]
        .into_iter()
        .enumerate()
        {
            let mut m = BTreeMap::new();
            m.insert("v".to_owned(), v);
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        let groups = c.query().group_count("v").unwrap();
        assert_eq!(groups.get("1"), Some(&1)); // bare text
        assert_eq!(groups.get("t:i:1"), Some(&1)); // escaped text
        assert_eq!(groups.get("i:1"), Some(&1));
        assert_eq!(groups.get("f:1"), Some(&1));
        assert_eq!(groups.get("f:0"), Some(&2)); // -0.0 == 0.0 for grouping
        assert_eq!(groups.len(), 5);
        assert_eq!(c.query().count_distinct("v").unwrap(), 5);
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p corvid group`
Expected: FAIL — the updated assertions get `None`/wrong counts because `group_key` still tags text with `s:`.

- [ ] **Step 3: Implement the new `group_key`**

Replace `group_key` in `builder.rs` (currently lines 1033-1048) with:

```rust
/// A canonical group key for a scalar value, or `None` for containers/null.
///
/// The canonical form (spec decision 3): **text is used bare** — the
/// natural, dominant case; int/float/bool are type-tagged (`i:1`, `f:1.5`,
/// `b:true`) so distinct types never collapse into one group; and a text
/// that would be ambiguous with a tagged form (it starts with `i:`, `f:`,
/// `b:`, or `t:`) is escaped with a `t:` prefix. The mapping is injective:
/// bare texts never start with any tag, tagged non-text keys start with
/// `i:`/`f:`/`b:`, and escaped texts start with `t:` — three disjoint
/// prefixes plus disjoint bare text. `-0.0` and `+0.0` share a group
/// (numerically equal); NaN groups as `f:NaN`.
fn group_key(v: &Value) -> Option<String> {
    const TAGS: [&str; 4] = ["i:", "f:", "b:", "t:"];
    match v {
        Value::Text(s) => Some(if TAGS.iter().any(|t| s.starts_with(t)) {
            format!("t:{s}")
        } else {
            s.clone()
        }),
        Value::Int(i) => Some(format!("i:{i}")),
        Value::Float(f) => Some(format!("f:{}", if *f == 0.0 { 0.0 } else { *f })),
        Value::Bool(b) => Some(format!("b:{b}")),
        _ => None,
    }
}
```

Update the doc comments on `group_count` (`builder.rs:236-241`), `count_distinct` (`:319-320`), `group_sum` (`:331-332`), and `group_avg` (`:345-346`) to describe the canonical form in the same words as the `group_key` doc above (replace the "text as-is; int/float/bool stringified" sentence).

- [ ] **Step 4: Check docs for stale examples**

Run: `grep -rn "group_count\|group_sum\|group_avg\|count_distinct" docs/GUIDE.md site/index.html README.md CHANGELOG.md`
For each hit that shows example output containing `s:`-prefixed keys, update it to the new canonical form. If none show keys (likely), no change is needed.

Add a `CHANGELOG.md` entry under the Unreleased section:

```markdown
### Changed
- Group-key canonical form (affects `group_count`, `group_sum`, `group_avg`,
  `count_distinct`): text values are now bare keys (`"blog"`), non-text
  values are type-tagged (`i:1`, `f:1.5`, `b:true`), and texts that would be
  ambiguous with a tag are `t:`-escaped. Previously every key carried an
  `s:`-style type prefix. Breaking change ahead of 1.0.
```

(Match the file's existing heading style; if there is no Unreleased section, create one at the top following the existing entry format.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p corvid group`
Expected: all group tests PASS.

Run: `cargo test --workspace`
Expected: all PASS (MCP tests included — the MCP surface has no group tools, so no server changes are expected; if any MCP test asserts prefixed keys, update it the same way).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
git add crates/corvid/src/builder.rs CHANGELOG.md docs/GUIDE.md site/index.html README.md
git commit -m "Group keys: bare text, tagged non-text, t:-escaped ambiguity (audit D4)

The typed-group-key fix leaked s:/i:/f:/b: prefixes into public results
while the docs promised text-as-is. Canonical form now: text bare; int/
float/bool tagged; texts that would collide with a tag are t:-escaped —
injective and natural for the dominant case. CHANGELOG notes the break."
```

(Only `git add` the doc files that actually changed.)

---

### Task 6: Wave exit verification

**Files:** none (verification only).

- [ ] **Step 1: Full gate run**

```bash
cargo fmt --all --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --fail-under-lines 90
```

Expected: all four green. If coverage dips below 90, add tests for the least-covered new code paths (the plan's new tests were chosen to cover their fixes; a dip indicates an untested branch — e.g. the `unique_value_eq` container arms — cover it before proceeding).

- [ ] **Step 2: Update AUDIT.md status**

In `AUDIT.md`, mark findings A1, A3, A4, B1, D4 as fixed with the wave-1 commit hashes (the file is rewritten fully in wave 5, so a one-line status note per finding is enough now).

```bash
git add AUDIT.md
git commit -m "AUDIT: mark A1/A3/A4/B1/D4 fixed (wave 1)"
```
