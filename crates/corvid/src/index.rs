//! Derived ANN index maintenance.
//!
//! A collection can carry an HNSW index on a vector field, created with
//! [`Collection::create_vector_index`]. Index *definitions* are persisted (in a
//! reserved `__indexes__` collection) so they survive a reopen; the HNSW graph
//! itself is in-memory and built lazily on first use from a collection scan.
//!
//! After the initial build the graph is maintained **incrementally**: each
//! insert adds one node and each delete tombstones one, both O(log n) — there
//! is no full rebuild per write (which would be quadratic for a write-then-read
//! loop). Overwrites tombstone the old node and add a new one. When tombstones
//! exceed half the graph, it is compacted by a one-off rebuild from the store.
//! Documents remain the source of truth, so a query never observes a stale
//! index.

use std::collections::HashMap;

use crate::db::{Collection, Db};
use crate::disk_hnsw::{self, DiskParams};
use crate::distance::Metric;
use crate::error::Result;
use crate::hnsw::{DEFAULT_EF_CONSTRUCTION, DEFAULT_M, Hnsw, Quantization};
use crate::store::Store;
use crate::value::Value;

/// Reserved collection holding persisted index definitions.
const INDEX_DEFS: &str = "__indexes__";

/// Ranked `(key, distance)` results, nearest first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// Where a vector index lives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexKind {
    /// HNSW graph held in RAM, rebuilt lazily on open.
    InMemory,
    /// HNSW graph stored on disk (redb); bounded memory, persists across open.
    OnDisk,
    /// On-disk HNSW storing product-quantized vectors (a codebook persists
    /// alongside the graph).
    OnDiskPq,
}

impl IndexKind {
    fn is_on_disk(self) -> bool {
        matches!(self, IndexKind::OnDisk | IndexKind::OnDiskPq)
    }
}

/// A registered vector index definition.
#[derive(Clone)]
struct VectorDef {
    metric: Metric,
    quant: Quantization,
    kind: IndexKind,
    /// PQ codebook for [`IndexKind::OnDiskPq`] (loaded from disk on open).
    pq: Option<std::sync::Arc<crate::pq::Pq>>,
    /// Whether an on-disk creation backfill is still **building** (an
    /// interrupted creation). A building index is maintained on every write
    /// but never served — queries fall back to exact scans until a resume
    /// flips it complete. In-memory indexes rebuild lazily, so they are
    /// never building.
    pub(crate) building: bool,
}

impl VectorDef {
    fn disk_params(&self) -> DiskParams {
        let p = DiskParams::with_quant(self.metric, self.quant, DEFAULT_M, DEFAULT_EF_CONSTRUCTION);
        match &self.pq {
            Some(pq) => p.with_pq(pq.clone()),
            None => p,
        }
    }
}

/// Per-database derived-index state, guarded by a mutex on the [`Db`].
#[derive(Default)]
pub(crate) struct IndexState {
    /// Registered index definitions (`(collection, field) -> def`).
    defs: HashMap<(String, String), VectorDef>,
    /// Built in-memory indexes, populated lazily.
    built: HashMap<(String, String), BuiltIndex>,
}

/// A built HNSW graph plus the bookkeeping to map nodes to live keys.
struct BuiltIndex {
    hnsw: Hnsw,
    /// node id -> key, or `None` if the node was tombstoned.
    node_to_key: Vec<Option<Vec<u8>>>,
    /// live key -> current node id.
    key_to_node: HashMap<Vec<u8>, usize>,
    /// Dimension of the indexed vectors (fixed by the first accepted vector).
    dim: Option<usize>,
}

impl BuiltIndex {
    fn new(def: VectorDef) -> Self {
        Self {
            hnsw: Hnsw::with_quant(def.metric, def.quant, DEFAULT_M, DEFAULT_EF_CONSTRUCTION),
            node_to_key: Vec::new(),
            key_to_node: HashMap::new(),
            dim: None,
        }
    }

    fn dead(&self) -> usize {
        self.node_to_key.len() - self.key_to_node.len()
    }

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

    /// Tombstone `key`'s node if present.
    fn tombstone(&mut self, key: &[u8]) {
        if let Some(old) = self.key_to_node.remove(key) {
            self.node_to_key[old] = None;
        }
    }

    /// Whether `query` matches the graph's fixed dimension. `None`-dim means
    /// an empty graph, which serves any query (returning nothing).
    fn accepts(&self, query: &[f32]) -> bool {
        match self.dim {
            Some(d) => d == query.len(),
            None => true,
        }
    }

    /// Search for the nearest `k` live keys. Over-fetches by the tombstone
    /// count so that, even if every dead node ranks ahead, `k` live nodes
    /// remain.
    fn search(&self, query: &[f32], k: usize) -> RankedKeys {
        if k == 0 || !self.accepts(query) {
            return Vec::new();
        }
        let want = k + self.dead();
        let ef = (want * 4).max(64);
        self.hnsw
            .search(query, want, ef)
            .into_iter()
            .filter_map(|(id, dist)| self.node_to_key[id].clone().map(|key| (key, dist)))
            .take(k)
            .collect()
    }
}

/// How a dumped vector index should be recreated.
pub(crate) enum VectorMode {
    InMemory,
    OnDisk,
    OnDiskPq { m: usize, k: usize },
}

/// A vector index definition in portable form (for dump/migrate).
pub(crate) struct VectorSpec {
    pub collection: String,
    pub field: String,
    pub metric: Metric,
    pub quant: Quantization,
    pub mode: VectorMode,
}

impl Db {
    /// Enumerate vector index definitions in portable form.
    pub(crate) fn vector_specs(&self) -> Vec<VectorSpec> {
        let state = self.indexes().lock().expect("index lock");
        state
            .defs
            .iter()
            .map(|((c, f), d)| {
                let mode = match (d.kind, &d.pq) {
                    (IndexKind::InMemory, _) => VectorMode::InMemory,
                    (IndexKind::OnDisk, _) => VectorMode::OnDisk,
                    (IndexKind::OnDiskPq, Some(pq)) => {
                        let (m, k) = pq.params();
                        VectorMode::OnDiskPq { m, k }
                    }
                    (IndexKind::OnDiskPq, None) => VectorMode::OnDisk,
                };
                VectorSpec {
                    collection: c.clone(),
                    field: f.clone(),
                    metric: d.metric,
                    quant: d.quant,
                    mode,
                }
            })
            .collect()
    }

    /// Load persisted index definitions. Called once on open. Legacy rows
    /// without state bytes decode as `Complete`; a `Building` row marks an
    /// on-disk index for lazy resume on first use.
    pub(crate) fn load_index_defs(&self) -> Result<()> {
        let mut state = self.indexes().lock().expect("index lock");
        for (key, value) in self.store().scan(INDEX_DEFS)? {
            let Some((coll, field)) = split_def_key(&key) else {
                continue;
            };
            // Kind bytes are the def payload; the creation state follows them
            // (new format) or is absent (legacy rows → Complete).
            let (kb, st) = crate::index_build::decode_def(&value);
            let Some(metric) = kb.first().and_then(metric_from_byte) else {
                continue;
            };
            // Quantization and kind bytes are optional (older defs lack them).
            let quant = kb
                .get(1)
                .and_then(quant_from_byte)
                .unwrap_or(Quantization::None);
            let kind = kb
                .get(2)
                .and_then(kind_from_byte)
                .unwrap_or(IndexKind::InMemory);
            let building = matches!(st, crate::index_build::DefState::Building { .. });
            // A PQ index carries a codebook persisted in its graph namespace.
            let pq = if kind == IndexKind::OnDiskPq {
                let ns = disk_hnsw::namespace(&coll, &field);
                disk_hnsw::load_codebook(self.store(), &ns)?.map(std::sync::Arc::new)
            } else {
                None
            };
            state.defs.insert(
                (coll, field),
                VectorDef {
                    metric,
                    quant,
                    kind,
                    pq,
                    building,
                },
            );
        }
        Ok(())
    }

    /// Register (or replace) an HNSW index on `field` for `collection`, with
    /// a storage quantization mode.
    pub(crate) fn register_vector_index(
        &self,
        collection: &str,
        field: &str,
        metric: Metric,
        quant: Quantization,
        kind: IndexKind,
    ) -> Result<()> {
        self.register_vector_index_inner(collection, field, metric, quant, kind, None)
    }

    /// Register (or replace) an HNSW index definition.
    ///
    /// One transaction installs the whole replacement: the target namespace
    /// is cleared, the def row lands, and a PQ codebook (when present)
    /// persists with it. Every re-registration — same or different
    /// kind/quant/metric — therefore starts from an empty namespace with a
    /// fresh `Building` cursor (spec decision: "re-register clears the target
    /// namespace in the same transaction that installs the new Building
    /// def"). This closes the audit's mixed-encoding window (A5: stale nodes
    /// from a previous encoding decoding under the new params), the register
    /// get→put interleave (W2T9: a stale cursor skipping committed prefix
    /// docs), and the PQ def/codebook two-transaction window (W2T8) at once.
    ///
    /// Only the on-disk kinds have a durable backfill, so only they register
    /// `Building` — a crash between registration and backfill completion
    /// leaves a never-served, resumable def; an interrupted creation is
    /// simply replaced by the next registration. In-memory indexes rebuild
    /// lazily from documents, so their defs are born `Complete`.
    fn register_vector_index_inner(
        &self,
        collection: &str,
        field: &str,
        metric: Metric,
        quant: Quantization,
        kind: IndexKind,
        pq: Option<std::sync::Arc<crate::pq::Pq>>,
    ) -> Result<()> {
        let key = def_key(collection, field);
        let kb = [metric_byte(metric), quant_byte(quant), kind_byte(kind)];
        let state = if kind.is_on_disk() {
            // Always a fresh cursor: the namespace below was just reset in
            // this same transaction, so any preserved cursor would address a
            // build that no longer exists.
            crate::index_build::DefState::Building { cursor: Vec::new() }
        } else {
            crate::index_build::DefState::Complete
        };
        let ns = disk_hnsw::namespace(collection, field);
        install_def_over_cleared_namespace(self.store(), &key, &ns, &kb, pq.as_ref(), &state)?;
        let mut state = self.indexes().lock().expect("index lock");
        let map_key = (collection.to_owned(), field.to_owned());
        state.defs.insert(
            map_key.clone(),
            VectorDef {
                metric,
                quant,
                kind,
                pq,
                building: kind.is_on_disk(),
            },
        );
        // Drop any built in-memory graph so it rebuilds with the (possibly new) def.
        state.built.remove(&map_key);
        Ok(())
    }

    /// Maintain every on-disk vector index on `collection` inside the
    /// caller's write transaction, so graph state commits atomically with the
    /// document. In-memory graphs are handled post-commit by
    /// [`Db::index_on_insert_memory`].
    pub(crate) fn index_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .filter(|(.., d)| d.kind.is_on_disk())
                .map(|((_, f), d)| (f.clone(), d.clone()))
                .collect()
        };
        for (field, def) in defs {
            let ns = disk_hnsw::namespace(collection, &field);
            match doc.get_path(&field).and_then(Value::as_vector) {
                Some(v) => disk_hnsw::insert_in_txn(tx, &ns, &def.disk_params(), key, v)?,
                None => {
                    disk_hnsw::delete_in_txn(tx, &ns, &def.disk_params(), key)?;
                }
            }
        }
        Ok(())
    }

    /// Maintain already-built in-memory graphs after a successful commit. An
    /// unbuilt graph picks this up when it builds lazily from the store.
    pub(crate) fn index_on_insert_memory(&self, collection: &str, key: &[u8], doc: &Value) {
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .filter(|(.., d)| d.kind == IndexKind::InMemory)
                .map(|((_, f), d)| (f.clone(), d.clone()))
                .collect()
        };
        for (field, _def) in defs {
            let map_key = (collection.to_owned(), field);
            let mut state = self.indexes().lock().expect("index lock");
            // Only maintain an already-built graph; an unbuilt one picks
            // this up when it builds lazily.
            if let Some(built) = state.built.get_mut(&map_key) {
                match doc.get_path(&map_key.1).and_then(Value::as_vector) {
                    Some(v) => built.add(key, v.to_vec()),
                    None => built.tombstone(key),
                }
            }
        }
    }

    /// Remove `key` from every on-disk vector index inside the caller's
    /// write transaction.
    pub(crate) fn index_on_delete_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .filter(|(.., d)| d.kind.is_on_disk())
                .map(|((_, f), d)| (f.clone(), d.clone()))
                .collect()
        };
        for (field, def) in defs {
            let ns = disk_hnsw::namespace(collection, &field);
            disk_hnsw::delete_in_txn(tx, &ns, &def.disk_params(), key)?;
        }
        Ok(())
    }

    /// Tombstone `key` from every already-built in-memory graph after a
    /// successful commit.
    pub(crate) fn index_on_delete_memory(&self, collection: &str, key: &[u8]) {
        let fields: Vec<String> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .filter(|(.., d)| d.kind == IndexKind::InMemory)
                .map(|((_, f), _)| f.clone())
                .collect()
        };
        for field in fields {
            let map_key = (collection.to_owned(), field);
            let mut state = self.indexes().lock().expect("index lock");
            if let Some(built) = state.built.get_mut(&map_key) {
                built.tombstone(key);
            }
        }
    }

    /// If a matching index is registered, return the approximate nearest `k`
    /// keys with distances; otherwise `None` (the caller falls back to exact).
    pub(crate) fn ann_search(
        &self,
        collection: &str,
        field: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Option<RankedKeys>> {
        // Before consulting any index: resume interrupted builds (a Building
        // def is never served, so without this nothing would ever flip it
        // Complete). Must run before the index lock: a resume takes it.
        self.try_resume_index_builds(collection)?;
        let map_key = (collection.to_owned(), field.to_owned());

        // Decide what work to do under a short lock; build/scan unlocked.
        let def = {
            let state = self.indexes().lock().expect("index lock");
            match state.defs.get(&map_key) {
                Some(d) if d.metric == metric => d.clone(),
                _ => return Ok(None), // no index, or metric mismatch → exact
            }
        };

        // On-disk indexes are served directly from the store (bounded memory).
        // A building one is never served — the caller falls back to an exact
        // scan while the resume above finishes it (this also covers a crash
        // before the first backfill chunk: a Building def with an empty
        // namespace never serves silently-empty results). A `None` from the
        // search means "cannot serve this query" (dimension mismatch) — fall
        // back to exact like every other unserveable case.
        if def.kind.is_on_disk() {
            if def.building {
                return Ok(None);
            }
            let ns = disk_hnsw::namespace(collection, field);
            let ef = (k * 4).max(64);
            return match disk_hnsw::search(self.store(), &ns, &def.disk_params(), query, k, ef)? {
                Some(ranked) => Ok(Some(ranked)),
                None => Ok(None),
            };
        }

        // Build (or compact) while holding the registry lock. A concurrent
        // writer's maintenance blocks on this lock and then applies to the
        // freshly installed graph, so no committed document can fall between
        // the build's store snapshot and the install — the race that
        // otherwise permanently hid such documents from ANN search.
        {
            let mut state = self.indexes().lock().expect("index lock");
            if !state.built.contains_key(&map_key) {
                let built = build_index(self.store(), collection, field, def.clone())?;
                state.built.entry(map_key.clone()).or_insert(built);
            }

            // Compact if more than half the graph is tombstoned.
            let needs_compact = state
                .built
                .get(&map_key)
                .is_some_and(|b| !b.node_to_key.is_empty() && b.dead() * 2 > b.node_to_key.len());
            if needs_compact {
                let built = build_index(self.store(), collection, field, def.clone())?;
                state.built.insert(map_key.clone(), built);
            }
        }

        let state = self.indexes().lock().expect("index lock");
        match state.built.get(&map_key) {
            // Dimension mismatch: the graph cannot serve this query honestly.
            // Return None so the caller falls back to an exact scan (which
            // skips mismatched documents), keeping results identical whether
            // or not an index exists.
            Some(b) if !b.accepts(query) => Ok(None),
            other => Ok(other.map(|b| b.search(query, k))),
        }
    }

    /// Flip a vector index's in-memory def to complete after its backfill
    /// committed `Complete` on disk.
    pub(crate) fn mark_vector_complete(&self, collection: &str, field: &str) {
        let mut state = self.indexes().lock().expect("index lock");
        if let Some(def) = state
            .defs
            .get_mut(&(collection.to_owned(), field.to_owned()))
        {
            def.building = false;
        }
    }

    /// Building on-disk vector defs of `collection` as `(field, cursor)`
    /// jobs, read from the def rows (disk is the resume truth after a crash).
    /// Only on-disk kinds carry a durable backfill, so only they can be
    /// Building; rows whose kind bytes don't decode to a serveable on-disk
    /// def are inert (no resume, never served).
    pub(crate) fn collect_building_vector(
        &self,
        collection: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut jobs = Vec::new();
        for (key, value) in self.store().scan(INDEX_DEFS)? {
            let Some((coll, field)) = split_def_key(&key) else {
                continue;
            };
            if coll != collection {
                continue;
            }
            let (kb, st) = crate::index_build::decode_def(&value);
            if !kb
                .get(2)
                .and_then(kind_from_byte)
                .is_some_and(|k| k.is_on_disk())
            {
                continue;
            }
            if let crate::index_build::DefState::Building { cursor } = st {
                jobs.push((field, cursor));
            }
        }
        Ok(jobs)
    }

    /// (Re-)run the atomic backfill for one on-disk vector index from
    /// `cursor`, then mark it complete — the exact driver invocation the
    /// `create_vector_index_ondisk*` fns use, shared with lazy resumes.
    /// Build/search params (metric, quantization, PQ codebook) come from the
    /// registered def, mirroring the create fns' construction;
    /// `load_index_defs` reloads the codebook for PQ indexes on open.
    pub(crate) fn resume_vector(&self, collection: &str, field: &str, cursor: &[u8]) -> Result<()> {
        let def = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .get(&(collection.to_owned(), field.to_owned()))
                .cloned()
        };
        // No registered def → nothing to resume (and nothing to serve).
        let Some(def) = def else {
            return Ok(());
        };
        let ns = disk_hnsw::namespace(collection, field);
        let kb = [
            metric_byte(def.metric),
            quant_byte(def.quant),
            kind_byte(def.kind),
        ];
        let params = def.disk_params();
        crate::index_build::run_atomic_backfill(
            self.store(),
            collection,
            INDEX_DEFS,
            &def_key(collection, field),
            &kb,
            cursor,
            &mut |tx, page| {
                let mut batch: Vec<(Vec<u8>, Vec<f32>)> = Vec::with_capacity(page.len());
                for (key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    if let Some(v) = doc.get_path(field).and_then(Value::as_vector) {
                        batch.push((key.clone(), v.to_vec()));
                    }
                }
                if !batch.is_empty() {
                    disk_hnsw::insert_page_in_txn(tx, &ns, &params, &batch)?;
                }
                Ok(())
            },
        )?;
        self.mark_vector_complete(collection, field);
        Ok(())
    }

    /// After an applied write, check every on-disk vector index of
    /// `collection` for dead-fraction compaction (audit B5). Best effort
    /// only: the triggering write already committed and results stay
    /// correct without compaction (tombstones are filtered), and the dead
    /// counter survives a failed attempt, so the next applied write retries.
    pub(crate) fn compact_ondisk_vector_indexes(&self, collection: &str) {
        let defs: Vec<(String, VectorDef)> = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                // A Building def is mid-(re)build; its own driver will finish
                // with whatever dead state remains, and the next write after
                // completion re-arms this trigger.
                .filter(|(.., d)| d.kind.is_on_disk() && !d.building)
                .map(|((_, f), d)| (f.clone(), d.clone()))
                .collect()
        };
        for (field, _def) in defs {
            if self.compact_if_needed(collection, &field).is_err() {
                // See the doc comment: retried by the next applied write.
            }
        }
    }

    /// Audit B5: compact an on-disk HNSW index when tombstones dominate.
    ///
    /// On-disk HNSW never rewrites graph topology on delete — nodes are
    /// tombstoned and search merely filters them, with a fixed 2× over-fetch
    /// — so a large dead fraction progressively crowds live results out of
    /// the search frontier (recall degrades; results stay *correct*). When
    /// `dead * 2 > live`, run the compaction cycle: the Task-2 registration
    /// shape on the same def (one transaction: clear the namespace, re-persist
    /// the PQ codebook, install `Building { cursor: [] }`) followed by the
    /// atomic-backfill driver re-reading the documents, then completion.
    /// Concurrent queries during the cycle see `building` and fall back to
    /// exact scans (correct, temporarily uncompacted).
    ///
    /// The trigger lives on the WRITE path (post-commit, in
    /// [`Db::finish_applied`]), not in search — a binding deviation from the
    /// brief's letter ("trigger checked on delete paths AND search"): dead
    /// only grows via node tombstones, and every tombstone is written by a
    /// document write (delete, overwrite, or dimension-mismatch re-insert),
    /// so every threshold crossing is preceded by an applied write a
    /// search-only observer could never be first to. Keeping the read path
    /// free of maintenance also preserves its lock-free, read-only nature.
    /// Overwrites are covered too (they tombstone the old node), so
    /// overwrite-heavy workloads compact as well.
    ///
    /// Runs under the `index_resume` try-lock (serializing against lazy
    /// resumes and other compactions); on lock contention another thread is
    /// already working, so skipping is safe. Synchronous by design: the
    /// caller's write has committed, and the alternative (a background
    /// queue) would reintroduce crash-coordination the Building state just
    /// removed.
    fn compact_if_needed(&self, collection: &str, field: &str) -> Result<()> {
        let ns = disk_hnsw::namespace(collection, field);
        let Some((dead, live)) = disk_hnsw::dead_fraction(self.store(), &ns)? else {
            return Ok(()); // never built / just reset — nothing to compact
        };
        if (dead as u64) * 2 <= live {
            return Ok(());
        }
        let _guard = match self.index_resume().try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(()), // another thread is resuming/compacting
        };
        // Double-check under the lock: a finished compaction, or a
        // re-registration that reset the namespace, may have obviated this
        // attempt since the fraction was read. Re-read the def too, so the
        // reset below installs what the registry actually holds (metric /
        // quant / kind / codebook), not this possibly-stale snapshot.
        let Some((dead, live)) = disk_hnsw::dead_fraction(self.store(), &ns)? else {
            return Ok(());
        };
        if (dead as u64) * 2 <= live {
            return Ok(());
        }
        let fresh = {
            let state = self.indexes().lock().expect("index lock");
            state
                .defs
                .get(&(collection.to_owned(), field.to_owned()))
                .cloned()
        };
        let Some(def) = fresh else {
            return Ok(()); // unregistered under us; nothing to compact
        };
        if !def.kind.is_on_disk() || def.building {
            return Ok(()); // replaced by an in-memory kind / already building
        }
        // The registration transaction, verbatim: reset the namespace (dead
        // counter included), keep the codebook (it lives IN the namespace,
        // so it must be rewritten after the clear), fresh Building cursor.
        let kb = [
            metric_byte(def.metric),
            quant_byte(def.quant),
            kind_byte(def.kind),
        ];
        install_def_over_cleared_namespace(
            self.store(),
            &def_key(collection, field),
            &ns,
            &kb,
            def.pq.as_ref(),
            &crate::index_build::DefState::Building { cursor: Vec::new() },
        )?;
        {
            let mut state = self.indexes().lock().expect("index lock");
            match state
                .defs
                .get_mut(&(collection.to_owned(), field.to_owned()))
            {
                Some(d) if d.kind.is_on_disk() => d.building = true,
                _ => return Ok(()), // replaced under us; its driver owns it
            }
        }
        // Driver re-backfill from the fresh cursor; this also flips the def
        // Complete in memory (`mark_vector_complete`) once it commits.
        self.resume_vector(collection, field, &[])?;
        Ok(())
    }
}

/// One transaction that resets an on-disk index for a full rebuild (the
/// Task-2 registration shape): clears the graph namespace, re-persists the
/// PQ codebook (it lives in the graph namespace, so it must be rewritten
/// after the clear to survive it), and installs `state` on the def row.
/// Shared by re-registration and dead-fraction compaction.
fn install_def_over_cleared_namespace(
    store: &Store,
    key: &[u8],
    ns: &str,
    kb: &[u8; 3],
    pq: Option<&std::sync::Arc<crate::pq::Pq>>,
    state: &crate::index_build::DefState,
) -> Result<()> {
    store.transaction(|tx| {
        // Reset first: the codebook put below must survive the clear.
        crate::store::clear_in_txn(tx, ns)?;
        // The PQ codebook lives in the graph namespace (not the def
        // bytes), committed atomically with the reset and the def row.
        if let Some(pq) = pq {
            disk_hnsw::store_codebook_in_txn(tx, ns, pq)?;
        }
        tx.put(INDEX_DEFS, key, &crate::index_build::encode_def(kb, state))?;
        Ok(())
    })
}

/// Build a fresh index for `field` by scanning `collection`.
fn build_index(store: &Store, collection: &str, field: &str, def: VectorDef) -> Result<BuiltIndex> {
    let mut built = BuiltIndex::new(def);
    for (key, bytes) in store.scan(collection)? {
        let doc = Value::decode(&bytes)?;
        if let Some(v) = doc.get_path(field).and_then(Value::as_vector) {
            built.add(&key, v.to_vec());
        }
    }
    Ok(built)
}

fn def_key(collection: &str, field: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(collection.len() + 1 + field.len());
    k.extend_from_slice(collection.as_bytes());
    k.push(0);
    k.extend_from_slice(field.as_bytes());
    k
}

fn split_def_key(key: &[u8]) -> Option<(String, String)> {
    let pos = key.iter().position(|&b| b == 0)?;
    let coll = std::str::from_utf8(&key[..pos]).ok()?.to_owned();
    let field = std::str::from_utf8(&key[pos + 1..]).ok()?.to_owned();
    Some((coll, field))
}

fn metric_byte(m: Metric) -> u8 {
    match m {
        Metric::Cosine => 0,
        Metric::Dot => 1,
        Metric::L2 => 2,
    }
}

fn metric_from_byte(b: &u8) -> Option<Metric> {
    match b {
        0 => Some(Metric::Cosine),
        1 => Some(Metric::Dot),
        2 => Some(Metric::L2),
        _ => None,
    }
}

fn quant_byte(q: Quantization) -> u8 {
    match q {
        Quantization::None => 0,
        Quantization::Binary => 1,
        Quantization::Scalar => 2,
    }
}

fn quant_from_byte(b: &u8) -> Option<Quantization> {
    match b {
        0 => Some(Quantization::None),
        1 => Some(Quantization::Binary),
        2 => Some(Quantization::Scalar),
        _ => None,
    }
}

fn kind_byte(k: IndexKind) -> u8 {
    match k {
        IndexKind::InMemory => 0,
        IndexKind::OnDisk => 1,
        IndexKind::OnDiskPq => 2,
    }
}

fn kind_from_byte(b: &u8) -> Option<IndexKind> {
    match b {
        0 => Some(IndexKind::InMemory),
        1 => Some(IndexKind::OnDisk),
        2 => Some(IndexKind::OnDiskPq),
        _ => None,
    }
}

impl Collection<'_> {
    /// Create (or replace) a full-precision in-memory HNSW index on `field`.
    ///
    /// The definition persists across reopen; the graph builds lazily and is
    /// then maintained incrementally. [`Collection::vector_search`] on the same
    /// `field`/`metric` uses it; other fields/metrics stay exact.
    pub fn create_vector_index(&self, field: &str, metric: Metric) -> Result<()> {
        self.db().register_vector_index(
            self.name(),
            field,
            metric,
            Quantization::None,
            IndexKind::InMemory,
        )
    }

    /// Like [`Collection::create_vector_index`] but storing vectors with a
    /// [`Quantization`] mode to cut index memory (binary ≈ 32×, scalar ≈ 4×) at
    /// some recall cost.
    pub fn create_vector_index_quantized(
        &self,
        field: &str,
        metric: Metric,
        quant: Quantization,
    ) -> Result<()> {
        self.db()
            .register_vector_index(self.name(), field, metric, quant, IndexKind::InMemory)
    }

    /// Create an **on-disk** HNSW index on `field`. The graph is stored in the
    /// database (not RAM) and persists across reopen, so search memory is
    /// bounded by nodes touched per query rather than by collection size —
    /// suitable for very large collections. Existing documents are backfilled.
    ///
    /// Atomic and crash-safe (audit A2): the def is registered `Building`
    /// before any backfill work; every page's graph writes and cursor advance
    /// commit in one transaction; completion is its own final transaction. A
    /// crash or error leaves a resumable `Building` def that queries never
    /// serve — the first vector query resumes it. Re-creating the index
    /// replaces it wholesale: the namespace resets in the same transaction
    /// that installs the fresh `Building` def (audit A5), so the backfill
    /// always rebuilds from scratch.
    pub fn create_vector_index_ondisk(&self, field: &str, metric: Metric) -> Result<()> {
        self.create_vector_index_ondisk_quantized(field, metric, Quantization::None)
    }

    /// Like [`Collection::create_vector_index_ondisk`] but storing each vector
    /// quantized (binary ≈32× / scalar ≈4× smaller on disk and in the page
    /// cache), trading a little recall for a much smaller footprint — the path
    /// for billions of vectors on a laptop.
    pub fn create_vector_index_ondisk_quantized(
        &self,
        field: &str,
        metric: Metric,
        quant: Quantization,
    ) -> Result<()> {
        self.db()
            .register_vector_index(self.name(), field, metric, quant, IndexKind::OnDisk)?;
        // Registration always installs a fresh Building cursor over a reset
        // namespace (audit A5), so the backfill below always starts at the
        // beginning; read_building_cursor simply recovers that fresh cursor.
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            INDEX_DEFS,
            &def_key(self.name(), field),
        )?;
        self.db()
            .resume_vector(self.name(), field, &cursor.unwrap_or_default())
    }

    /// Create an on-disk HNSW index storing **product-quantized** vectors: a
    /// codebook of `m` subspaces × `k` centroids is trained from up to a sample
    /// of existing vectors, then each vector is stored as `m` code bytes — far
    /// smaller than f32 (e.g. a 128-dim vector → `m` bytes). `field`'s
    /// dimension must be divisible by `m`. The codebook persists with the index.
    ///
    /// Requires existing documents to train on (a codebook can't be learned
    /// from nothing); returns [`Error::EmptyIndexTraining`](crate::Error::EmptyIndexTraining) if none have a
    /// usable vector at `field`.
    ///
    /// The backfill is atomic and crash-safe (audit A2), exactly like
    /// [`Collection::create_vector_index_ondisk_quantized`]; only the
    /// pre-registration training scan (bounded, read-only) precedes it.
    pub fn create_vector_index_ondisk_pq(
        &self,
        field: &str,
        metric: Metric,
        m: usize,
        k: usize,
    ) -> Result<()> {
        let store = self.db().store();
        // Gather a training sample (bounded) from existing vectors.
        const SAMPLE_CAP: usize = 50_000;
        let mut sample: Vec<Vec<f32>> = Vec::new();
        let mut cursor: Vec<u8> = Vec::new();
        'outer: loop {
            let page = store.scan_from(self.name(), &cursor, 2048)?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            let mut next = last_key.clone();
            next.push(0);
            for (_, bytes) in &page {
                let doc = Value::decode(bytes)?;
                if let Some(v) = doc.get_path(field).and_then(Value::as_vector) {
                    sample.push(v.to_vec());
                    if sample.len() >= SAMPLE_CAP {
                        break 'outer;
                    }
                }
            }
            cursor = next;
        }
        let pq = crate::pq::Pq::train(&sample, m, k).ok_or(crate::Error::EmptyIndexTraining)?;
        let pq = std::sync::Arc::new(pq);

        // Register Building with the trained codebook, then run the atomic
        // backfill through the shared driver from the fresh cursor the
        // registration just installed (over a reset namespace, audit A5).
        self.db().register_vector_index_inner(
            self.name(),
            field,
            metric,
            Quantization::None,
            IndexKind::OnDiskPq,
            Some(pq),
        )?;
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            INDEX_DEFS,
            &def_key(self.name(), field),
        )?;
        self.db()
            .resume_vector(self.name(), field, &cursor.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(v: Vec<f32>) -> Value {
        let mut m = BTreeMap::new();
        m.insert("embedding".to_owned(), Value::Vector(v));
        Value::Map(m)
    }

    fn pq_corpus(n: usize, dim: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0xA5A5_1234_DEAD_0001;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
        let centers: Vec<Vec<f32>> = (0..8)
            .map(|_| (0..dim).map(|_| next() * 10.0).collect())
            .collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % centers.len()];
                c.iter().map(|&x| x + (next() - 0.5)).collect()
            })
            .collect()
    }

    #[test]
    fn ondisk_pq_index_is_used_persists_and_recalls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        let data = pq_corpus(400, 16);
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            for (i, v) in data.iter().enumerate() {
                c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                    .unwrap();
            }
            c.create_vector_index_ondisk_pq("embedding", Metric::L2, 8, 32)
                .unwrap();
        }
        // Reopen: the codebook reloads from disk (no retrain) and serves search.
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        let mut hits = 0;
        for (i, v) in data.iter().enumerate().take(40) {
            let got = c.vector_search("embedding", v, 5, Metric::L2).unwrap();
            let want = format!("k{i}").into_bytes();
            if got.iter().any(|h| h.key == want) {
                hits += 1;
            }
        }
        // The querying vector itself should usually be in its own top-5.
        assert!(hits >= 30, "PQ self-recall {hits}/40 too low");
    }

    #[test]
    fn ondisk_pq_reflects_incremental_writes_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let data = pq_corpus(200, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        c.create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16)
            .unwrap();
        // Insert after creation → encoded with the existing codebook. An
        // in-distribution duplicate of data[5] is retrievable near data[5]
        // (generous k so quantization coarseness doesn't hide it).
        c.insert(b"new", &doc(data[5].clone())).unwrap();
        let got = c
            .vector_search("embedding", &data[5], 50, Metric::L2)
            .unwrap();
        assert!(
            got.iter().any(|h| h.key == b"new".to_vec()),
            "freshly inserted vector must be retrievable"
        );
        // Delete is reflected (the tombstoned key never appears, exactly).
        c.delete(b"new").unwrap();
        let got = c
            .vector_search("embedding", &data[5], 50, Metric::L2)
            .unwrap();
        assert!(!got.iter().any(|h| h.key == b"new".to_vec()));
    }

    #[test]
    fn ondisk_pq_on_empty_collection_errors() {
        let db = Db::open_in_memory().unwrap();
        let err =
            db.collection("docs")
                .create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16);
        assert!(matches!(err, Err(crate::Error::EmptyIndexTraining)));
    }

    /// A query whose dimension mismatches an on-disk PQ index falls back to the
    /// exact path — same results as an unindexed collection (audit A4 pin).
    #[test]
    fn pq_index_dimension_mismatch_falls_back_to_exact() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let corpus = pq_corpus(40, 8);
        for (i, v) in corpus.iter().enumerate() {
            c.insert(&(i as u32).to_le_bytes(), &doc(v.clone()))
                .unwrap();
        }
        c.create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16)
            .unwrap();
        let wrong = vec![0.5f32; 7];
        let hits = c.vector_search("embedding", &wrong, 5, Metric::L2).unwrap();
        assert!(hits.is_empty(), "no 7-dim vectors exist");
        // The correct dimension still serves via the index.
        let hits = c
            .vector_search("embedding", &corpus[0], 5, Metric::L2)
            .unwrap();
        assert!(!hits.is_empty());
    }

    fn seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.insert(b"c", &doc(vec![-1.0, 0.0])).unwrap();
        db
    }

    /// A building on-disk vector index is never served: vector queries fall
    /// back to an exact scan and stay correct; the first unobstructed query
    /// resumes the backfill.
    #[test]
    fn building_ondisk_vector_def_falls_back_then_resumes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.insert(b"c", &doc(vec![0.9, 0.1])).unwrap();
        // Forge a Building def exactly as an interrupted creation would
        // leave it, then reload the registry from that row.
        db.store()
            .put(
                INDEX_DEFS,
                &def_key("docs", "embedding"),
                &crate::index_build::encode_def(
                    &[
                        metric_byte(Metric::L2),
                        quant_byte(Quantization::None),
                        kind_byte(IndexKind::OnDisk),
                    ],
                    &crate::index_build::DefState::Building { cursor: vec![] },
                ),
            )
            .unwrap();
        db.load_index_defs().unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(true));
        // With the resume lock held (another thread resuming), the building
        // def must not be served: ann_search reports "no usable index"...
        let _guard = db.index_resume().lock().unwrap();
        assert!(
            db.ann_search("docs", "embedding", &[1.0, 0.0], 3, Metric::L2)
                .unwrap()
                .is_none(),
            "a building on-disk index must not be served"
        );
        // ...so vector_search falls back to an exact scan and stays correct.
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
        drop(_guard);
        // Once the resume lock is free, the next query resumes the backfill
        // and serves from the on-disk graph.
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        assert!(
            db.ann_search("docs", "embedding", &[1.0, 0.0], 3, Metric::L2)
                .unwrap()
                .is_some(),
            "a completed on-disk index must serve"
        );
    }

    /// A vector def row in the legacy kind-bytes-only format (pre-state rows
    /// written by earlier versions) decodes as `Complete`: the index stays
    /// serviceable across the upgrade with no re-backfill.
    #[test]
    fn legacy_stateless_ondisk_def_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
            c.create_vector_index_ondisk("embedding", Metric::L2)
                .unwrap();
            // Overwrite the def row with the legacy kind-bytes-only form.
            db.store()
                .put(
                    INDEX_DEFS,
                    &def_key("docs", "embedding"),
                    &[
                        metric_byte(Metric::L2),
                        quant_byte(Quantization::None),
                        kind_byte(IndexKind::OnDisk),
                    ],
                )
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        assert!(db.collect_building_vector("docs").unwrap().is_empty());
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec()); // served from the on-disk graph
    }

    /// An in-memory registration has no durable backfill: its def is born
    /// `Complete` and `create_vector_index` behavior is unchanged.
    #[test]
    fn inmemory_registration_is_born_complete() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.create_vector_index("embedding", Metric::L2).unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        assert!(db.collect_building_vector("docs").unwrap().is_empty());
        // Lazily built on first query, correct results (unchanged behavior).
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
        // The def row is the new-format Complete encoding of the InMemory kind.
        let row = db
            .store()
            .get(INDEX_DEFS, &def_key("docs", "embedding"))
            .unwrap()
            .unwrap();
        let (kb, st) = crate::index_build::decode_def(&row);
        assert_eq!(
            kb,
            vec![
                metric_byte(Metric::L2),
                quant_byte(Quantization::None),
                kind_byte(IndexKind::InMemory)
            ]
        );
        assert!(matches!(st, crate::index_build::DefState::Complete));
    }

    /// PQ smoke (audit A2): a completed PQ creation leaves a `Complete` def
    /// on disk whose quantized on-disk graph serves search.
    #[test]
    fn pq_create_completes_and_serves() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let data = pq_corpus(60, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        c.create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16)
            .unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        // The def row is Complete on disk...
        let row = db
            .store()
            .get(INDEX_DEFS, &def_key("docs", "embedding"))
            .unwrap()
            .unwrap();
        let (kb, st) = crate::index_build::decode_def(&row);
        assert_eq!(
            kb,
            vec![
                metric_byte(Metric::L2),
                quant_byte(Quantization::None),
                kind_byte(IndexKind::OnDiskPq)
            ]
        );
        assert!(matches!(st, crate::index_build::DefState::Complete));
        // ...and the quantized graph serves.
        let got = c
            .vector_search("embedding", &data[0], 5, Metric::L2)
            .unwrap();
        assert!(!got.is_empty());
    }

    /// Regression (review round 1): re-creating a PQ index with different m/k
    /// over an interrupted build must NOT resume the old cursor. PQ kind
    /// bytes don't capture the hyperparameters, and every registration
    /// retrains a fresh codebook — resuming would leave the committed prefix
    /// encoded with the previous codebook while the completed index decodes
    /// every node with the new one (silently wrong vectors). A retrain
    /// implies a full re-backfill.
    #[test]
    fn pq_recreate_with_new_params_rebackfills_under_new_codebook() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let data = pq_corpus(60, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        // First creation completes under (m=4, k=16).
        c.create_vector_index_ondisk_pq("embedding", Metric::L2, 4, 16)
            .unwrap();
        // Forge an interrupted re-create: Building with a mid-corpus cursor,
        // exactly as a crash mid-backfill leaves it (prefix committed).
        db.store()
            .put(
                INDEX_DEFS,
                &def_key("docs", "embedding"),
                &crate::index_build::encode_def(
                    &[
                        metric_byte(Metric::L2),
                        quant_byte(Quantization::None),
                        kind_byte(IndexKind::OnDiskPq),
                    ],
                    &crate::index_build::DefState::Building {
                        cursor: b"k29\0".to_vec(),
                    },
                ),
            )
            .unwrap();
        db.load_index_defs().unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(true));
        // Re-create with different hyperparameters: the retrain must
        // re-backfill the WHOLE corpus under the new codebook (fresh
        // cursor), never resume the stale one.
        c.create_vector_index_ondisk_pq("embedding", Metric::L2, 2, 8)
            .unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        let row = db
            .store()
            .get(INDEX_DEFS, &def_key("docs", "embedding"))
            .unwrap()
            .unwrap();
        let (kb, st) = crate::index_build::decode_def(&row);
        assert_eq!(
            kb,
            vec![
                metric_byte(Metric::L2),
                quant_byte(Quantization::None),
                kind_byte(IndexKind::OnDiskPq)
            ]
        );
        assert!(matches!(st, crate::index_build::DefState::Complete));
        // Prefix docs (committed before the forged cursor) must serve with
        // new-codebook accuracy, at parity with the exact path: an
        // unindexed collection's top-1 (the doc itself) comes back in the
        // indexed top-5. A mixed-codebook prefix decodes as garbage and
        // collapses this recall to chance.
        let plain = Db::open_in_memory().unwrap();
        let pc = plain.collection("docs");
        for (i, v) in data.iter().enumerate() {
            pc.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        let mut hits = 0;
        for (i, v) in data.iter().enumerate().take(20) {
            let exact = pc.vector_search("embedding", v, 1, Metric::L2).unwrap();
            assert_eq!(exact[0].key, format!("k{i}").into_bytes()); // sanity
            let got = c.vector_search("embedding", v, 5, Metric::L2).unwrap();
            if got.iter().any(|h| h.key == exact[0].key) {
                hits += 1;
            }
        }
        assert!(
            hits >= 15,
            "prefix self-recall {hits}/20 too low (mixed codebooks?)"
        );
    }

    /// Audit A5: re-registering an index with a different quantization must
    /// reset the graph namespace in the SAME transaction that installs the
    /// new def. Previously the first build's differently-encoded nodes
    /// survived the switch as tombstoned garbage (the node counter never
    /// reset either), so every re-registration leaked a full stale graph.
    #[test]
    fn recreate_with_different_quantization_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        let data = pq_corpus(120, 8);
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            for (i, v) in data.iter().enumerate() {
                c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                    .unwrap();
            }
            c.create_vector_index_ondisk_quantized("embedding", Metric::L2, Quantization::Scalar)
                .unwrap();
            let hits = c
                .vector_search("embedding", &data[0], 5, Metric::L2)
                .unwrap();
            assert!(!hits.is_empty());
            // Re-register over the scalar build with plain (None) storage.
            c.create_vector_index_ondisk("embedding", Metric::L2)
                .unwrap();

            // (a) Parity with an unindexed twin collection: every doc is its
            // own exact top-1 and appears in the indexed top-5.
            let plain = Db::open_in_memory().unwrap();
            let pc = plain.collection("docs");
            for (i, v) in data.iter().enumerate() {
                pc.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                    .unwrap();
            }
            let mut parity = 0;
            for (i, v) in data.iter().enumerate().take(20) {
                let exact = pc.vector_search("embedding", v, 1, Metric::L2).unwrap();
                assert_eq!(exact[0].key, format!("k{i}").into_bytes()); // sanity
                let got = c.vector_search("embedding", v, 5, Metric::L2).unwrap();
                if got.iter().any(|h| h.key == exact[0].key) {
                    parity += 1;
                }
            }
            assert!(parity >= 18, "quant-switch parity {parity}/20 too low");

            // (b) The namespace holds exactly the second build: one node and
            // one keymap per live doc plus one meta row — nothing leaked
            // from the scalar-encoded first build.
            let ns = disk_hnsw::namespace("docs", "embedding");
            let rows = db.store().scan(&ns).unwrap();
            let nodes = rows
                .iter()
                .filter(|(k, _)| k.first() == Some(&b'n'))
                .count();
            let keymaps = rows
                .iter()
                .filter(|(k, _)| k.first() == Some(&b'k'))
                .count();
            let metas = rows
                .iter()
                .filter(|(k, _)| k.first() == Some(&b'm'))
                .count();
            assert_eq!(keymaps, data.len(), "live keymaps must match live docs");
            assert_eq!(nodes, data.len(), "leaked nodes from the first build");
            assert_eq!(metas, 1);
            assert_eq!(rows.len(), 2 * data.len() + 1);
        }
        // Reopen: the re-created (full-precision) graph still serves at
        // parity — the doc itself is its own top-1 under L2.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &data[0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"k0".to_vec());
    }

    /// Audit A5: switching an indexed field from an on-disk kind to InMemory
    /// must remove the disk graph (the namespace resets with the def), not
    /// leak a dead graph that a later re-switch would resurrect.
    #[test]
    fn kind_switch_to_inmemory_removes_disk_namespace() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        let ns = disk_hnsw::namespace("docs", "embedding");
        assert!(
            !db.store().scan(&ns).unwrap().is_empty(),
            "sanity: the on-disk build wrote graph rows"
        );
        // Replace with the in-memory kind.
        c.create_vector_index("embedding", Metric::L2).unwrap();
        assert!(
            db.store().scan(&ns).unwrap().is_empty(),
            "the disk graph must be removed when the kind switches to InMemory"
        );
        // And the in-memory index serves correctly.
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    /// The deferred W2T5/W2T9 race: a re-registration landing while a lazy
    /// resume is stalled must fully replace the interrupted build. The
    /// registration resets the namespace and installs a fresh Building
    /// cursor in one transaction, so the backfill rebuilds every document —
    /// no stale cursor skips a committed prefix, and post-replace queries
    /// sit at parity with exact search.
    #[test]
    fn replace_during_resume_is_consistent() {
        use std::sync::mpsc::channel;
        let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
        let c = db.collection("docs");
        let data = pq_corpus(80, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        // Forge a mid-cursor Building def exactly as an interrupted creation
        // would leave it (prefix k0..=k39 notionally committed).
        db.store()
            .put(
                INDEX_DEFS,
                &def_key("docs", "embedding"),
                &crate::index_build::encode_def(
                    &[
                        metric_byte(Metric::L2),
                        quant_byte(Quantization::None),
                        kind_byte(IndexKind::OnDisk),
                    ],
                    &crate::index_build::DefState::Building {
                        cursor: b"k39\0".to_vec(),
                    },
                ),
            )
            .unwrap();
        db.load_index_defs().unwrap();
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(true));

        // Hold the resume lock so the query thread's lazy resume stalls; it
        // must fall back to an exact scan (correct results either way).
        let guard = db.index_resume().lock().unwrap();
        let (done_tx, done_rx) = channel();
        let querier = {
            let db = std::sync::Arc::clone(&db);
            let q = data[0].clone();
            std::thread::spawn(move || {
                let got = db
                    .collection("docs")
                    .vector_search("embedding", &q, 5, Metric::L2)
                    .unwrap();
                done_tx.send(()).unwrap();
                got
            })
        };
        done_rx.recv().unwrap(); // the fallback query finished mid-race
        // Re-register on the main thread while the resume stays stalled:
        // fresh Building + cleared namespace, then the full backfill.
        db.collection("docs")
            .create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        drop(guard);
        let fallback = querier.join().unwrap();
        // The stalled query's fallback was already correct: exact parity.
        assert!(
            fallback.iter().any(|h| h.key == b"k0".to_vec()),
            "fallback query must return the exact nearest doc"
        );

        // The def ended Complete and the namespace holds the full rebuild.
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
        let ns = disk_hnsw::namespace("docs", "embedding");
        let keymaps = db
            .store()
            .scan(&ns)
            .unwrap()
            .into_iter()
            .filter(|(k, _)| k.first() == Some(&b'k'))
            .count();
        assert_eq!(keymaps, data.len(), "the rebuild must cover every doc");

        // Post-replace queries serve from the rebuilt graph at exact parity.
        let plain = Db::open_in_memory().unwrap();
        let pc = plain.collection("docs");
        for (i, v) in data.iter().enumerate() {
            pc.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        let mut parity = 0;
        for (i, v) in data.iter().enumerate().take(20) {
            let exact = pc.vector_search("embedding", v, 1, Metric::L2).unwrap();
            assert_eq!(exact[0].key, format!("k{i}").into_bytes()); // sanity
            let got = db
                .collection("docs")
                .vector_search("embedding", v, 5, Metric::L2)
                .unwrap();
            if got.iter().any(|h| h.key == exact[0].key) {
                parity += 1;
            }
        }
        assert!(parity >= 18, "post-replace parity {parity}/20 too low");
    }

    /// Whether `field` of `coll` is registered and still building (test probe
    /// into the registry; `None` when unregistered).
    fn vector_def_building(db: &Db, coll: &str, field: &str) -> Option<bool> {
        let state = db.indexes().lock().unwrap();
        state
            .defs
            .get(&(coll.to_owned(), field.to_owned()))
            .map(|d| d.building)
    }

    #[test]
    fn indexed_search_matches_exact_on_small_corpus() {
        let db = seeded();
        let c = db.collection("docs");
        let exact = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        c.create_vector_index("embedding", Metric::L2).unwrap();
        let indexed = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert_eq!(
            exact.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            indexed.iter().map(|h| h.key.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn incremental_insert_is_reflected_without_full_rebuild() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        // Build the graph.
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        // New uniquely-nearest doc — maintained incrementally.
        c.insert(b"exact", &doc(vec![5.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[5.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"exact".to_vec());
    }

    #[test]
    fn delete_tombstones_from_index() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        c.delete(b"a").unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
    }

    #[test]
    fn overwrite_updates_indexed_vector() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();
        // Move "a" far away; it should no longer be the nearest to (1,0).
        c.insert(b"a", &doc(vec![9.0, 9.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_ne!(hits[0].key, b"a".to_vec());
    }

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
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
            .unwrap();
        // Overwrite "a" with a 3-dim vector (plain overwrite; no schema).
        c.insert(b"a", &doc(vec![1.0, 0.0, 0.0])).unwrap();
        // A 2-dim query must not return "a" — parity with the exact path,
        // which skips dimension-mismatched documents.
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 2, Metric::Cosine)
            .unwrap();
        assert!(
            hits.iter().all(|h| h.key != b"a".to_vec()),
            "stale node for 'a' still served: {hits:?}"
        );
        assert_eq!(hits.len(), 1, "only 'b' should remain");
    }

    #[test]
    fn many_overwrites_then_query_stays_correct_after_compaction() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        c.insert(b"k", &doc(vec![0.0, 0.0])).unwrap();
        let _ = c
            .vector_search("embedding", &[0.0, 0.0], 1, Metric::L2)
            .unwrap();
        // Overwrite the same key many times → many tombstones → triggers compaction.
        for i in 0..20 {
            c.insert(b"k", &doc(vec![i as f32, 0.0])).unwrap();
        }
        c.insert(b"target", &doc(vec![100.0, 0.0])).unwrap();
        let hits = c
            .vector_search("embedding", &[100.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"target".to_vec());
    }

    #[test]
    fn index_definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.create_vector_index("embedding", Metric::Cosine).unwrap();
        }
        // Reopen: the index definition should be reloaded and used.
        let db = Db::open(&path).unwrap();
        db.collection("docs")
            .insert(b"b", &doc(vec![0.0, 1.0]))
            .unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn quantized_index_is_used_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
            c.create_vector_index_quantized("embedding", Metric::Cosine, Quantization::Scalar)
                .unwrap();
            let hits = c
                .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
                .unwrap();
            assert_eq!(hits[0].key, b"a".to_vec());
        }
        // The quantized definition reloads on reopen and is still used.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn binary_quantized_index_via_collection() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"pos", &doc(vec![1.0, 1.0])).unwrap();
        c.insert(b"neg", &doc(vec![-1.0, -1.0])).unwrap();
        c.create_vector_index_quantized("embedding", Metric::Cosine, Quantization::Binary)
            .unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 1.0], 1, Metric::Cosine)
            .unwrap();
        assert_eq!(hits[0].key, b"pos".to_vec());
    }

    #[test]
    fn ondisk_index_searches_persists_and_backfills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            // Insert BEFORE creating the index → exercises backfill.
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
            c.create_vector_index_ondisk("embedding", Metric::L2)
                .unwrap();
            // Insert AFTER → exercises incremental on-disk maintenance.
            c.insert(b"c", &doc(vec![0.9, 0.1])).unwrap();
            let hits = c
                .vector_search("embedding", &[1.0, 0.0], 2, Metric::L2)
                .unwrap();
            assert_eq!(hits[0].key, b"a".to_vec());
            assert_eq!(hits[1].key, b"c".to_vec());
        }
        // Reopen: on-disk graph is used directly — no rebuild.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }

    #[test]
    fn ondisk_quantized_index_is_used_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap(); // backfilled
            c.create_vector_index_ondisk_quantized("embedding", Metric::L2, Quantization::Scalar)
                .unwrap();
            c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap(); // incremental
            let hits = c
                .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
                .unwrap();
            assert_eq!(hits[0].key, b"a".to_vec());
        }
        // Reopen: the scalar-quantized on-disk graph decodes with its stored
        // mode and is used directly — no rebuild.
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }

    #[test]
    fn ondisk_index_reflects_delete() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        c.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        c.insert(b"b", &doc(vec![0.0, 1.0])).unwrap();
        c.delete(b"a").unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 5, Metric::L2)
            .unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
    }

    #[test]
    fn metric_mismatch_falls_back_to_exact() {
        let db = seeded();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        let hits = c
            .vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn unindexed_field_uses_exact() {
        let db = seeded();
        let hits = db
            .collection("docs")
            .vector_search("embedding", &[0.0, 1.0], 1, Metric::L2)
            .unwrap();
        assert_eq!(hits[0].key, b"b".to_vec());
    }

    /// Regression: a query whose dimension differs from the indexed vectors
    /// used to panic in debug builds and compute truncated-distance garbage in
    /// release. It must behave exactly like the unindexed path instead.
    #[test]
    fn wrong_dimension_query_falls_back_to_exact_semantics() {
        let db = seeded(); // docs have 2-dimensional embeddings
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::L2).unwrap();
        // Force the graph to build so the ANN branch is live.
        let _ = c
            .vector_search("embedding", &[1.0, 0.0], 3, Metric::L2)
            .unwrap();

        let wrong_dim = c.vector_search("embedding", &[1.0], 3, Metric::L2);
        assert!(
            wrong_dim.unwrap().is_empty(),
            "no doc has a 1-dim embedding"
        );

        let on_disk = Db::open_in_memory().unwrap();
        let oc = on_disk.collection("docs");
        oc.insert(b"a", &doc(vec![1.0, 0.0])).unwrap();
        oc.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        let got = oc
            .vector_search("embedding", &[1.0], 3, Metric::L2)
            .unwrap();
        assert!(got.is_empty());
        // Matching dims still work through both index kinds.
        assert_eq!(
            c.vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
                .unwrap()[0]
                .key,
            b"a".to_vec()
        );
        assert_eq!(
            oc.vector_search("embedding", &[1.0, 0.0], 1, Metric::L2)
                .unwrap()[0]
                .key,
            b"a".to_vec()
        );
    }

    /// Audit B5: a mass delete (90% of the corpus) must drive on-disk HNSW
    /// compaction — the dead-fraction trigger resets the namespace and the
    /// driver re-backfills only the survivors — so the graph ends freshly
    /// compacted (node rows == live docs) and search sits at parity with an
    /// unindexed twin, instead of the uncompacted behavior where tombstones
    /// crowd live results out of the fixed-over-fetch frontier.
    #[test]
    fn mass_delete_compacts_and_restores_recall() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let n = 2000usize;
        let survivors = 200usize;
        let data = pq_corpus(n, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        // Delete the first 90%.
        for i in 0..n - survivors {
            c.delete(format!("k{i}").as_bytes()).unwrap();
        }
        let ns = disk_hnsw::namespace("docs", "embedding");
        let node_rows = |db: &Db| {
            db.store()
                .scan(&ns)
                .unwrap()
                .into_iter()
                .filter(|(key, _)| key.first() == Some(&b'n'))
                .count() as u64
        };
        // Keep deleting (a small tail of the survivor range) until the last
        // applied write left the graph freshly compacted: node rows == live
        // docs. The trigger's rest state keeps dead ≤ live/2 between
        // crossings, so a handful of further deletes reaches the next one;
        // without compaction this never happens and the bound trips.
        let mut extra = 0usize;
        while node_rows(&db)
            != disk_hnsw::dead_fraction(db.store(), &ns)
                .unwrap()
                .map(|(_, live)| live)
                .unwrap_or(0)
        {
            assert!(
                extra < survivors / 2,
                "no compaction observed after {extra} extra deletes"
            );
            c.delete(format!("k{}", n - survivors + extra).as_bytes())
                .unwrap();
            extra += 1;
        }
        let live_now = survivors - extra;

        // (a) Recall parity with an unindexed twin holding exactly the same
        // live docs: the freshly re-backfilled graph must match the exact
        // top-10 (set overlap; a tombstone-heavy graph collapses this).
        let twin = Db::open_in_memory().unwrap();
        let tc = twin.collection("docs");
        for (i, v) in data.iter().enumerate().skip(n - survivors + extra) {
            tc.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        let k = 10;
        let mut overlap_sum = 0.0f64;
        let mut probes = 0usize;
        for (i, v) in data
            .iter()
            .enumerate()
            .skip(n - survivors + extra)
            .step_by(3)
        {
            let exact = tc.vector_search("embedding", v, k, Metric::L2).unwrap();
            let got = c.vector_search("embedding", v, k, Metric::L2).unwrap();
            assert_eq!(got.len(), exact.len(), "short results for query k{i}");
            let exact_keys: std::collections::HashSet<&Vec<u8>> =
                exact.iter().map(|h| &h.key).collect();
            let hits = got.iter().filter(|h| exact_keys.contains(&h.key)).count();
            overlap_sum += hits as f64 / k as f64;
            probes += 1;
        }
        let overlap = overlap_sum / probes as f64;
        // Measured on this corpus/query set: a freshly built index over the
        // same live docs scores ≈0.81, the uncompacted 90%-dead graph ≈0.32
        // (with short result lists). 0.7 separates "freshly re-backfilled"
        // from "tombstone-crowded" with margin on both sides.
        assert!(
            overlap >= 0.7,
            "post-compaction overlap {overlap} too low (uncompacted ≈ 0.3)"
        );

        // (b) Compaction ran: the namespace holds exactly the live docs'
        // rows (an uncompacted graph keeps all ~2000 nodes), and the def is
        // Complete (served, not mid-build).
        let rows = db.store().scan(&ns).unwrap();
        let nodes = rows
            .iter()
            .filter(|(key, _)| key.first() == Some(&b'n'))
            .count();
        let keymaps = rows
            .iter()
            .filter(|(key, _)| key.first() == Some(&b'k'))
            .count();
        assert_eq!(keymaps, live_now, "live keymaps must match live docs");
        assert_eq!(
            nodes, live_now,
            "compaction must leave one node per live doc"
        );
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
    }

    /// Audit B5 pin: a single delete stays far below the dead-fraction
    /// threshold — no reset, no re-backfill (node rows unchanged, def stays
    /// Complete). Passes before and after the compaction feature.
    #[test]
    fn dead_below_threshold_does_not_compact() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let data = pq_corpus(50, 8);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                .unwrap();
        }
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();
        let ns = disk_hnsw::namespace("docs", "embedding");
        let nodes_before = db
            .store()
            .scan(&ns)
            .unwrap()
            .into_iter()
            .filter(|(key, _)| key.first() == Some(&b'n'))
            .count();
        assert_eq!(nodes_before, data.len());
        // One delete: dead=1, live=49 — far under the trigger.
        c.delete(b"k0").unwrap();
        let rows = db.store().scan(&ns).unwrap();
        let nodes = rows
            .iter()
            .filter(|(key, _)| key.first() == Some(&b'n'))
            .count();
        let keymaps = rows
            .iter()
            .filter(|(key, _)| key.first() == Some(&b'k'))
            .count();
        assert_eq!(nodes, data.len(), "one delete must not rebuild the graph");
        assert_eq!(keymaps, data.len() - 1);
        assert_eq!(vector_def_building(&db, "docs", "embedding"), Some(false));
    }

    /// Stress the lazy build against concurrent writes: a reader triggers the
    /// first (full-scan) build while a writer commits documents. Every
    /// committed document must be findable afterwards — before the
    /// build-under-lock fix, a write landing during the scan was skipped by
    /// maintenance *and* missed by the snapshot, permanently.
    #[test]
    fn concurrent_writes_are_never_lost_from_the_lazy_built_index() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let db = Arc::new(Db::open_in_memory().unwrap());
        db.collection("docs")
            .create_vector_index("embedding", Metric::L2)
            .unwrap();
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let db = Arc::clone(&db);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for i in 0..300u32 {
                    let v = vec![i as f32, 0.0];
                    db.collection("docs")
                        .insert(format!("k{i}").as_bytes(), &doc(v.clone()))
                        .unwrap();
                }
                done.store(true, Ordering::Release);
            })
        };
        let reader = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                loop {
                    if done.load(Ordering::Acquire) {
                        break;
                    }
                    // Triggers the first lazy build while writes are in flight.
                    let _ =
                        db.collection("docs")
                            .vector_search("embedding", &[0.0], 10, Metric::L2);
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();

        // One final search per document: every committed key is its own exact
        // nearest neighbour, so it must come back in the top hit.
        let c = db.collection("docs");
        for i in 0..300u32 {
            let hits = c
                .vector_search("embedding", &[i as f32, 0.0], 1, Metric::L2)
                .unwrap();
            assert_eq!(
                hits[0].key,
                format!("k{i}").into_bytes(),
                "document k{i} was lost from the index"
            );
        }
    }
}
