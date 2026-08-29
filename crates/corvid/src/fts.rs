//! Inverted full-text index.
//!
//! Without an index, BM25 (`ranked_bm25` in [`crate::query`]) rescans and
//! re-tokenizes the entire corpus on every query. A text index instead
//! maintains postings (`term -> {doc -> term-frequency}`) plus per-document
//! lengths incrementally on each write, so a query touches only the postings
//! of its query terms — O(query terms × matching docs) instead of O(corpus).
//!
//! Like the vector index, definitions persist (in `__text_indexes__`) and the
//! postings are built lazily on first use; documents remain the source of
//! truth, so a query never sees a stale index.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::db::{Collection, Db};
use crate::disk_fts;
use crate::error::Result;
use crate::store::SnapshotReader;
use crate::text::{Bm25Params, analyze, idf, term_score};
use crate::value::Value;

/// Reserved collection holding persisted text-index definitions.
const TEXT_DEFS: &str = "__text_indexes__";

/// Ranked `(key, score)` results, most relevant first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// Where a text index lives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
    /// Postings held in RAM, built lazily on first query, rebuilt on open.
    InMemory,
    /// Postings stored as redb entries: bounded memory, persists across reopen.
    OnDisk,
}

fn kind_byte(k: TextKind) -> u8 {
    match k {
        TextKind::InMemory => 0,
        TextKind::OnDisk => 1,
    }
}

fn kind_from(b: Option<&u8>) -> TextKind {
    match b {
        Some(1) => TextKind::OnDisk,
        _ => TextKind::InMemory,
    }
}

/// Per-database full-text index state.
#[derive(Default)]
pub(crate) struct FtsState {
    /// Registered `(collection, field)` text indexes: where each lives and
    /// whether it is still **building** (an interrupted on-disk creation).
    /// Maintenance iterates all defs; serving an on-disk index requires
    /// `building == false`. In-memory indexes rebuild lazily, so they are
    /// never building.
    defs: HashMap<(String, String), (TextKind, bool)>,
    /// Built in-memory inverted indexes, populated lazily.
    built: HashMap<(String, String), Inverted>,
}

/// An inverted index over one text field.
#[derive(Default)]
struct Inverted {
    /// term -> (doc key -> sorted token positions). The term frequency is the
    /// number of positions; positions enable phrase queries.
    postings: HashMap<String, HashMap<Vec<u8>, Vec<u32>>>,
    /// doc key -> token count.
    doc_len: HashMap<Vec<u8>, usize>,
    /// doc key -> its distinct terms (forward index, for removal).
    doc_terms: HashMap<Vec<u8>, Vec<String>>,
    total_len: usize,
}

impl Inverted {
    /// Index (or re-index) `key`'s text. An existing entry is removed first.
    fn add(&mut self, key: &[u8], text: &str) {
        self.remove(key);
        let mut len = 0u32;
        let mut terms: Vec<String> = Vec::new();
        for (pos, token) in analyze(text).into_iter().enumerate() {
            let entry = self.postings.entry(token.clone()).or_default();
            let positions = entry.entry(key.to_vec()).or_default();
            if positions.is_empty() {
                terms.push(token);
            }
            positions.push(pos as u32);
            len += 1;
        }
        terms.sort();
        terms.dedup();
        self.doc_terms.insert(key.to_vec(), terms);
        self.doc_len.insert(key.to_vec(), len as usize);
        self.total_len += len as usize;
    }

    /// Remove `key` from the index if present.
    fn remove(&mut self, key: &[u8]) {
        if let Some(terms) = self.doc_terms.remove(key) {
            for term in terms {
                if let Some(p) = self.postings.get_mut(&term) {
                    p.remove(key);
                    if p.is_empty() {
                        self.postings.remove(&term);
                    }
                }
            }
        }
        if let Some(len) = self.doc_len.remove(key) {
            self.total_len -= len;
        }
    }

    /// BM25 search touching only the query terms' postings.
    fn search(&self, query: &str, k: usize) -> RankedKeys {
        let params = Bm25Params::default();
        let n = self.doc_len.len();
        if n == 0 || k == 0 {
            return Vec::new();
        }
        let avg_len = match self.total_len as f32 / n as f32 {
            0.0 => 1.0,
            v => v,
        };

        let mut query_terms = analyze(query);
        query_terms.sort();
        query_terms.dedup();

        let mut scores: HashMap<Vec<u8>, f32> = HashMap::new();
        for term in &query_terms {
            let Some(posting) = self.postings.get(term) else {
                continue;
            };
            let term_idf = idf(n, posting.len());
            for (doc, positions) in posting {
                let dl = self.doc_len.get(doc).copied().unwrap_or(0);
                *scores.entry(doc.clone()).or_insert(0.0) +=
                    term_score(positions.len() as u32, dl, avg_len, term_idf, params);
            }
        }

        let mut ranked: Vec<(Vec<u8>, f32)> =
            scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(k);
        ranked
    }

    /// BM25-ranked docs that contain the analyzed `phrase` as a consecutive
    /// run of tokens (in order). Scored by the phrase terms' BM25 sum.
    fn phrase_search(&self, phrase: &str, k: usize) -> RankedKeys {
        let params = Bm25Params::default();
        let n = self.doc_len.len();
        let terms = analyze(phrase);
        if n == 0 || k == 0 || terms.is_empty() {
            return Vec::new();
        }
        let avg_len = match self.total_len as f32 / n as f32 {
            0.0 => 1.0,
            v => v,
        };
        // Candidate docs: those containing the first phrase term.
        let Some(first) = self.postings.get(&terms[0]) else {
            return Vec::new();
        };
        let mut ranked: Vec<(Vec<u8>, f32)> = Vec::new();
        'docs: for (doc, first_pos) in first {
            // Collect each term's positions in this doc.
            let mut per_term: Vec<&Vec<u32>> = Vec::with_capacity(terms.len());
            per_term.push(first_pos);
            for t in &terms[1..] {
                match self.postings.get(t).and_then(|m| m.get(doc)) {
                    Some(ps) => per_term.push(ps),
                    None => continue 'docs,
                }
            }
            if !phrase_aligned(&per_term) {
                continue;
            }
            let dl = self.doc_len.get(doc).copied().unwrap_or(0);
            let score: f32 = terms
                .iter()
                .zip(&per_term)
                .map(|(t, ps)| {
                    let df = self.postings.get(t).map(|m| m.len()).unwrap_or(1);
                    term_score(ps.len() as u32, dl, avg_len, idf(n, df), params)
                })
                .sum();
            ranked.push((doc.clone(), score));
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(k);
        ranked
    }
}

/// Whether there is a start position `p` such that `per_term[i]` contains
/// `p + i` for every `i` — i.e. the terms occur consecutively and in order.
/// Each positions list is sorted ascending.
fn phrase_aligned(per_term: &[&Vec<u32>]) -> bool {
    if per_term.len() == 1 {
        return !per_term[0].is_empty();
    }
    for &start in per_term[0] {
        if (1..per_term.len()).all(|i| {
            start
                .checked_add(i as u32)
                .is_some_and(|want| per_term[i].binary_search(&want).is_ok())
        }) {
            return true;
        }
    }
    false
}

impl Db {
    /// Load persisted text-index definitions. Called once on open. Legacy
    /// rows without state bytes decode as `Complete`; a `Building` row marks
    /// an on-disk index for lazy resume on first use.
    pub(crate) fn load_text_defs(&self) -> Result<()> {
        let mut state = self.fts().lock().expect("fts lock");
        for (key, value) in self.store().scan(TEXT_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                let (kind_bytes, st) = crate::index_build::decode_def(&value);
                let kind = kind_from(kind_bytes.first());
                let building = matches!(st, crate::index_build::DefState::Building { .. });
                state.defs.insert(def, (kind, building));
            }
        }
        Ok(())
    }

    /// All text index definitions as `(collection, field, on_disk)` (for
    /// dump). State is intentionally dropped: dump/load replays creation.
    pub(crate) fn text_specs(&self) -> Vec<(String, String, bool)> {
        let state = self.fts().lock().expect("fts lock");
        state
            .defs
            .iter()
            .map(|((c, f), (kind, _))| (c.clone(), f.clone(), *kind == TextKind::OnDisk))
            .collect()
    }

    /// Register (or replace) a text index on `field` for `collection`.
    ///
    /// Only the on-disk kind has a durable backfill, so only it carries
    /// creation state: it registers `Building` (empty cursor) — a crash
    /// between registration and backfill completion leaves a never-served,
    /// resumable def, and an in-flight `Building` row keeps its cursor so a
    /// re-registration resumes instead of rescanning. An in-memory index
    /// rebuilds lazily from documents on first query, so its def is born
    /// `Complete`.
    pub(crate) fn register_text_index(
        &self,
        collection: &str,
        field: &str,
        kind: TextKind,
    ) -> Result<()> {
        let key = def_key(collection, field);
        let building = kind == TextKind::OnDisk;
        if building {
            let in_flight =
                crate::index_build::read_building_cursor(self.store(), TEXT_DEFS, &key)?.is_some();
            if !in_flight {
                self.store().put(
                    TEXT_DEFS,
                    &key,
                    &crate::index_build::encode_def(
                        &[kind_byte(kind)],
                        &crate::index_build::DefState::Building { cursor: vec![] },
                    ),
                )?;
            }
        } else {
            self.store().put(
                TEXT_DEFS,
                &key,
                &crate::index_build::encode_def(
                    &[kind_byte(kind)],
                    &crate::index_build::DefState::Complete,
                ),
            )?;
        }
        let mut state = self.fts().lock().expect("fts lock");
        let map_key = (collection.to_owned(), field.to_owned());
        state.defs.insert(map_key.clone(), (kind, building));
        state.built.remove(&map_key);
        Ok(())
    }

    /// Maintain every on-disk text index on `collection` inside the caller's
    /// write transaction, so postings commit atomically with the document.
    /// In-memory indexes are handled post-commit by [`Db::fts_on_insert_memory`].
    /// Building indexes are maintained too: their backfill resumes from a
    /// cursor and re-indexing is an idempotent upsert, so the two overlap safely.
    pub(crate) fn fts_on_insert_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        let fields: Vec<(String, TextKind)> = {
            let state = self.fts().lock().expect("fts lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), (k, _))| (f.clone(), *k))
                .collect()
        };
        for (field, kind) in fields {
            let text = doc.get_path(&field).and_then(Value::as_text);
            if kind == TextKind::OnDisk {
                let ns = disk_fts::namespace(collection, &field);
                match text {
                    Some(t) => disk_fts::insert_in_txn(tx, &ns, key, t)?,
                    None => disk_fts::delete_in_txn(tx, &ns, key)?,
                }
            }
        }
        Ok(())
    }

    /// Maintain in-memory text indexes after a successful commit. An unbuilt
    /// index picks this up when it builds lazily from the store.
    pub(crate) fn fts_on_insert_memory(&self, collection: &str, key: &[u8], doc: &Value) {
        let fields: Vec<(String, TextKind)> = {
            let state = self.fts().lock().expect("fts lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), (k, _))| (f.clone(), *k))
                .collect()
        };
        for (field, kind) in fields {
            let text = doc.get_path(&field).and_then(Value::as_text);
            if kind == TextKind::InMemory {
                let map_key = (collection.to_owned(), field);
                let mut state = self.fts().lock().expect("fts lock");
                if let Some(inv) = state.built.get_mut(&map_key) {
                    match text {
                        Some(t) => inv.add(key, t),
                        None => inv.remove(key),
                    }
                }
            }
        }
    }

    /// Remove `key` from every on-disk text index inside the caller's write
    /// transaction.
    pub(crate) fn fts_on_delete_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        let defs: Vec<(String, TextKind)> = {
            let state = self.fts().lock().expect("fts lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), (k, _))| (f.clone(), *k))
                .collect()
        };
        for (field, kind) in defs {
            if kind == TextKind::OnDisk {
                let ns = disk_fts::namespace(collection, &field);
                disk_fts::delete_in_txn(tx, &ns, key)?;
            }
        }
        Ok(())
    }

    /// Remove `key` from every in-memory text index after a successful commit.
    pub(crate) fn fts_on_delete_memory(&self, collection: &str, key: &[u8]) {
        let defs: Vec<(String, TextKind)> = {
            let state = self.fts().lock().expect("fts lock");
            state
                .defs
                .iter()
                .filter(|((c, _), _)| c == collection)
                .map(|((_, f), (k, _))| (f.clone(), *k))
                .collect()
        };
        for (field, kind) in defs {
            if kind == TextKind::InMemory {
                let map_key = (collection.to_owned(), field);
                let mut state = self.fts().lock().expect("fts lock");
                if let Some(inv) = state.built.get_mut(&map_key) {
                    inv.remove(key);
                }
            }
        }
    }

    /// If a text index is registered, return the BM25-ranked top `k` keys;
    /// otherwise `None` (the caller falls back to an exact scan). Opens its
    /// own per-op read transactions and resumes interrupted builds first —
    /// the resume-before-snapshot contract lives in the query entry points,
    /// which use [`Db::fts_search_in`] instead; this standalone form remains
    /// for direct (test) probing outside a snapshot.
    #[cfg(test)]
    pub(crate) fn fts_search(
        &self,
        collection: &str,
        field: &str,
        query: &str,
        k: usize,
    ) -> Result<Option<RankedKeys>> {
        // Before consulting any index: resume interrupted builds (a Building
        // def is never served, so without this nothing would ever flip it
        // Complete). Must run before the fts lock: a resume takes it.
        self.try_resume_index_builds(collection)?;
        self.fts_search_in(collection, field, query, k, self.store())
    }

    /// Audit C3: cheap, execution-free probe for `plan_shape`/`explain` —
    /// would [`Db::fts_search_in`] find a def it could serve? Mirrors that
    /// function's registry gate exactly (a def is registered for
    /// `(collection, field)` and is not an on-disk index mid-build —
    /// in-memory defs are never building) without doing any of its work: no
    /// lazy build, no postings scan, no snapshot.
    pub(crate) fn text_index_consultable(&self, collection: &str, field: &str) -> bool {
        let state = self.fts().lock().expect("fts lock");
        matches!(
            state.defs.get(&(collection.to_owned(), field.to_owned())),
            Some((TextKind::InMemory, _)) | Some((TextKind::OnDisk, false))
        )
    }

    /// Snapshot-scoped twin of [`Db::fts_search`] (audit B3) for the reads
    /// that were ALWAYS query-snapshot reads: on-disk postings, corpus
    /// stats, and the caller's document fetches all read `reader`, so the
    /// ranked keys and the documents share one point in time. The lazy
    /// in-memory BUILD is deliberately NOT one of those reads — see the
    /// build-under-lock contract inline. Does NOT resume interrupted builds
    /// (resuming writes): the caller resumes before its snapshot opens.
    pub(crate) fn fts_search_in(
        &self,
        collection: &str,
        field: &str,
        query: &str,
        k: usize,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<RankedKeys>> {
        let map_key = (collection.to_owned(), field.to_owned());

        // Build while holding the registry lock. Build-under-lock contract
        // (wave-4 final review): the build reads a FRESH read transaction
        // (`self.store()`, opened here under the lock — exactly the
        // pre-wave-4 shape), NOT the caller's `reader`: it must observe all
        // state committed as of this lock acquisition, so a doc committed
        // after the caller's snapshot is correctly IN the installed
        // postings (its maintenance no-opped only while unbuilt); reading
        // the stale snapshot would hide it PERMANENTLY. The stale query
        // itself merely omits it (fetch via the older reader returns None —
        // omission-only). A concurrent writer's maintenance blocks on this
        // lock and then applies to the fresh index, so nothing committed
        // falls between the build's fresh read and the install.
        {
            let mut state = self.fts().lock().expect("fts lock");
            match state.defs.get(&map_key) {
                None => return Ok(None),
                // On-disk postings are maintained on every write; no build.
                // A building one is never served — the caller falls back to
                // an exact scan while the resume above finishes it.
                Some((TextKind::OnDisk, false)) => {
                    let ns = disk_fts::namespace(collection, field);
                    drop(state);
                    let ranked = disk_fts::search(reader, &ns, query, k)?;
                    // Registry-lag guard (wave-5, twin of the ANN one in
                    // [`Db::ann_search_in`]): a concurrent registration or
                    // compaction may have committed between this reader's
                    // registry snapshot (Complete, taken above) and the
                    // postings search just performed — its transaction
                    // cleared the namespace and flipped the def row to
                    // `Building`, and absent postings search as empty.
                    // Serving that empty vector would be a silent wrong
                    // answer; re-read the def row (one point-get, on the
                    // same snapshot) and if it now says `Building`, declare
                    // the index unserviceable (`Ok(None)` → the caller's
                    // exact fallback). A row that still says `Complete`
                    // means a genuinely empty index — serve the empty result
                    // as before.
                    if ranked.is_empty()
                        && crate::index_build::read_building_cursor(
                            reader,
                            TEXT_DEFS,
                            &def_key(collection, field),
                        )?
                        .is_some()
                    {
                        return Ok(None);
                    }
                    return Ok(Some(ranked));
                }
                Some((TextKind::OnDisk, true)) => return Ok(None),
                Some((TextKind::InMemory, _)) => {
                    if !state.built.contains_key(&map_key) {
                        let inv = build_inverted(self.store(), collection, field)?;
                        state.built.entry(map_key.clone()).or_insert(inv);
                    }
                }
            }
        }

        let state = self.fts().lock().expect("fts lock");
        Ok(state.built.get(&map_key).map(|inv| inv.search(query, k)))
    }

    /// Snapshot-scoped phrase search (audit B3): the reads that were always
    /// query-snapshot reads — on-disk postings with positions, corpus
    /// stats, the caller's document fetches — come from `reader`. The lazy
    /// in-memory build reads FRESH state under the lock instead, per the
    /// build-under-lock contract in [`Db::fts_search_in`]. Resume discipline
    /// as [`Db::fts_search_in`]: the caller resumes before its snapshot
    /// opens.
    pub(crate) fn fts_phrase_search_in(
        &self,
        collection: &str,
        field: &str,
        phrase: &str,
        k: usize,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<RankedKeys>> {
        let map_key = (collection.to_owned(), field.to_owned());
        // Same build-under-lock contract as [`Db::fts_search_in`]: the
        // in-memory build reads `self.store()` (fresh), never `reader`.
        {
            let mut state = self.fts().lock().expect("fts lock");
            match state.defs.get(&map_key) {
                None => return Ok(None),
                Some((TextKind::OnDisk, false)) => {
                    let ns = disk_fts::namespace(collection, field);
                    drop(state);
                    let ranked = disk_fts::phrase_search(reader, &ns, phrase, k)?;
                    // Registry-lag guard, exactly as in [`Db::fts_search_in`]:
                    // empty ranked + a def row that now says `Building` means
                    // the namespace was cleared beneath this reader's stale
                    // registry snapshot — fall back (`Ok(None)`) instead of
                    // serving the silent empty result. `Complete` is a
                    // genuinely empty index and still serves empty.
                    if ranked.is_empty()
                        && crate::index_build::read_building_cursor(
                            reader,
                            TEXT_DEFS,
                            &def_key(collection, field),
                        )?
                        .is_some()
                    {
                        return Ok(None);
                    }
                    return Ok(Some(ranked));
                }
                Some((TextKind::OnDisk, true)) => return Ok(None),
                Some((TextKind::InMemory, _)) => {
                    if !state.built.contains_key(&map_key) {
                        let inv = build_inverted(self.store(), collection, field)?;
                        state.built.entry(map_key.clone()).or_insert(inv);
                    }
                }
            }
        }
        let state = self.fts().lock().expect("fts lock");
        Ok(state
            .built
            .get(&map_key)
            .map(|inv| inv.phrase_search(phrase, k)))
    }

    /// Flip a text index's in-memory def to complete after its backfill
    /// committed `Complete` on disk.
    pub(crate) fn mark_text_complete(&self, collection: &str, field: &str) {
        let mut state = self.fts().lock().expect("fts lock");
        let key = (collection.to_owned(), field.to_owned());
        let kind = state.defs.get(&key).map_or(TextKind::OnDisk, |(k, _)| *k);
        state.defs.insert(key, (kind, false));
    }

    /// Building text defs of `collection` as `(field, cursor)` jobs, read from
    /// the def rows (disk is the resume truth after a crash). Only on-disk
    /// defs carry a durable backfill, so only they can be Building.
    pub(crate) fn collect_building_text(&self, collection: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let mut jobs = Vec::new();
        for (key, value) in self.store().scan(TEXT_DEFS)? {
            let Some((coll, field)) = split_def_key(&key) else {
                continue;
            };
            if coll != collection {
                continue;
            }
            let (kind_bytes, st) = crate::index_build::decode_def(&value);
            if kind_from(kind_bytes.first()) != TextKind::OnDisk {
                continue;
            }
            if let crate::index_build::DefState::Building { cursor } = st {
                jobs.push((field, cursor));
            }
        }
        Ok(jobs)
    }

    /// (Re-)run the atomic backfill for one on-disk text index from `cursor`,
    /// then mark it complete — the exact driver invocation
    /// `create_text_index_ondisk` uses, shared with lazy resumes. Documents
    /// without a text value on `field` are skipped, matching maintenance's
    /// corpus rules.
    pub(crate) fn resume_text(&self, collection: &str, field: &str, cursor: &[u8]) -> Result<()> {
        let ns = disk_fts::namespace(collection, field);
        let kb = [kind_byte(TextKind::OnDisk)];
        crate::index_build::run_atomic_backfill(
            self.store(),
            collection,
            "text",
            TEXT_DEFS,
            &def_key(collection, field),
            &kb,
            cursor,
            &mut |tx, page| {
                for (key, bytes) in page {
                    let doc = Value::decode(bytes)?;
                    if let Some(text) = doc.get_path(field).and_then(Value::as_text) {
                        disk_fts::insert_in_txn(tx, &ns, key, text)?;
                    }
                }
                Ok(())
            },
        )?;
        self.mark_text_complete(collection, field);
        Ok(())
    }
}

/// Build an inverted index for `field` by scanning `collection` on a FRESH
/// read transaction from `reader` (the pre-wave-4 shape passes
/// `self.store()`). NOT the caller's query snapshot: the build runs under
/// the registry lock and must observe everything committed as of lock
/// acquisition, so a doc committed after the caller's snapshot lands IN the
/// postings (the stale query only omits it via its own fetch) instead of
/// being permanently missing. See the build-under-lock contract in
/// [`Db::fts_search_in`].
fn build_inverted(reader: &dyn SnapshotReader, collection: &str, field: &str) -> Result<Inverted> {
    let mut inv = Inverted::default();
    for (key, bytes) in reader.scan(collection)? {
        let doc = Value::decode(&bytes)?;
        if let Some(text) = doc.get_path(field).and_then(Value::as_text) {
            inv.add(&key, text);
        }
    }
    Ok(inv)
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

/// Construct the FTS state mutex for a fresh database.
pub(crate) fn new_state() -> Mutex<FtsState> {
    Mutex::new(FtsState::default())
}

impl Collection<'_> {
    /// Create (or replace) an inverted text index on `field`.
    ///
    /// The definition persists across reopen; postings build lazily and are
    /// maintained incrementally. [`Collection::text_search`] on the same field
    /// then uses it instead of rescanning the corpus.
    pub fn create_text_index(&self, field: &str) -> Result<()> {
        self.ensure_writable()?;
        crate::db::validate_name(field)?;
        self.db()
            .register_text_index(self.name(), field, TextKind::InMemory)
    }

    /// Create (or replace) an **on-disk** inverted text index on `field`.
    ///
    /// Postings are stored as redb entries, so memory stays bounded by the
    /// query terms (not the corpus) and the index persists across reopen with
    /// no rebuild. Existing documents are backfilled now; later writes maintain
    /// it incrementally.
    ///
    /// Atomic and crash-safe (audit A2): the def is registered `Building`
    /// before any backfill work; every page's postings and cursor advance
    /// commit in one transaction; completion is its own final transaction. A
    /// crash or error leaves a resumable `Building` def that queries never
    /// serve — the first text query (or a re-creation) resumes it.
    pub fn create_text_index_ondisk(&self, field: &str) -> Result<()> {
        self.ensure_writable()?;
        crate::db::validate_name(field)?;
        self.db()
            .register_text_index(self.name(), field, TextKind::OnDisk)?;
        // A def still Building from an interrupted creation resumes from its
        // saved cursor; a Complete (or fresh) def backfills from the start.
        let cursor = crate::index_build::read_building_cursor(
            self.db().store(),
            TEXT_DEFS,
            &def_key(self.name(), field),
        )?;
        self.db()
            .resume_text(self.name(), field, &cursor.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::{TEXT_DEFS, TextKind, def_key, kind_byte};
    use crate::Db;
    use std::collections::BTreeMap;

    fn doc(body: &str) -> crate::Value {
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), crate::Value::Text(body.to_owned()));
        crate::Value::Map(m)
    }

    fn seed(db: &Db) {
        let c = db.collection("docs");
        c.insert(b"a", &doc("the quick brown fox")).unwrap();
        c.insert(b"b", &doc("the lazy dog sleeps")).unwrap();
        c.insert(b"c", &doc("a fox and a dog play")).unwrap();
    }

    #[test]
    fn indexed_text_search_matches_exact() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        let exact = c.text_search("body", "fox dog", 10).unwrap();

        c.create_text_index("body").unwrap();
        let indexed = c.text_search("body", "fox dog", 10).unwrap();

        assert_eq!(
            exact.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            indexed.iter().map(|h| h.key.clone()).collect::<Vec<_>>()
        );
        assert_eq!(indexed[0].key, b"c".to_vec()); // matches both terms
    }

    #[test]
    fn index_maintained_on_insert_and_delete() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        c.create_text_index("body").unwrap();
        let _ = c.text_search("body", "fox", 10).unwrap(); // build

        c.insert(b"d", &doc("another fox appears")).unwrap();
        let hits = c.text_search("body", "fox", 10).unwrap();
        assert!(hits.iter().any(|h| h.key == b"d".to_vec()));

        c.delete(b"a").unwrap();
        let hits = c.text_search("body", "quick", 10).unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
    }

    #[test]
    fn overwrite_reindexes_text() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        c.create_text_index("body").unwrap();
        let _ = c.text_search("body", "fox", 10).unwrap();
        // Replace doc a's text; "quick" should no longer match it.
        c.insert(b"a", &doc("totally different content")).unwrap();
        let hits = c.text_search("body", "quick", 10).unwrap();
        assert!(!hits.iter().any(|h| h.key == b"a".to_vec()));
        let hits = c.text_search("body", "different", 10).unwrap();
        assert!(hits.iter().any(|h| h.key == b"a".to_vec()));
    }

    #[test]
    fn definition_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            seed(&db);
            db.collection("docs").create_text_index("body").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let hits = db
            .collection("docs")
            .text_search("body", "fox", 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn ondisk_index_matches_inmemory_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        let expected = {
            let db = Db::open(&path).unwrap();
            seed(&db);
            let c = db.collection("docs");
            c.create_text_index_ondisk("body").unwrap();
            let hits = c.text_search("body", "fox dog", 10).unwrap();
            assert_eq!(hits[0].key, b"c".to_vec()); // matches both terms
            hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>()
        };
        // Reopen: no rebuild, postings already on disk.
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        let reopened = c
            .text_search("body", "fox dog", 10)
            .unwrap()
            .iter()
            .map(|h| h.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(expected, reopened);
    }

    #[test]
    fn ondisk_index_maintained_on_write_and_delete() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        c.create_text_index_ondisk("body").unwrap();

        c.insert(b"d", &doc("another fox appears")).unwrap();
        assert!(
            c.text_search("body", "fox", 10)
                .unwrap()
                .iter()
                .any(|h| h.key == b"d".to_vec())
        );

        c.insert(b"a", &doc("totally different content")).unwrap();
        assert!(
            !c.text_search("body", "quick", 10)
                .unwrap()
                .iter()
                .any(|h| h.key == b"a".to_vec())
        );

        c.delete(b"b").unwrap();
        assert!(
            !c.text_search("body", "dog", 10)
                .unwrap()
                .iter()
                .any(|h| h.key == b"b".to_vec())
        );
    }

    #[test]
    fn phrase_search_requires_adjacency_in_order() {
        for on_disk in [false, true] {
            let db = Db::open_in_memory().unwrap();
            let c = db.collection("docs");
            c.insert(b"a", &doc("the quick brown fox jumps")).unwrap();
            c.insert(b"b", &doc("a brown quick fox")).unwrap(); // wrong order
            c.insert(b"c", &doc("the quick brown dog")).unwrap();
            if on_disk {
                c.create_text_index_ondisk("body").unwrap();
            } else {
                c.create_text_index("body").unwrap();
            }
            // "quick brown" is adjacent+in-order in a and c, not in b.
            let hits = c.phrase_search("body", "quick brown", 10).unwrap();
            let keys: std::collections::HashSet<_> = hits.iter().map(|h| h.key.clone()).collect();
            assert!(keys.contains(b"a".as_slice()), "on_disk={on_disk}");
            assert!(keys.contains(b"c".as_slice()), "on_disk={on_disk}");
            assert!(!keys.contains(b"b".as_slice()), "on_disk={on_disk}");
            // A phrase not present anywhere → empty.
            assert!(
                c.phrase_search("body", "brown fox", 10)
                    .unwrap()
                    .iter()
                    .all(|h| h.key != b"b".to_vec())
            );
            assert!(
                !c.phrase_search("body", "quick fox", 10)
                    .unwrap()
                    .iter()
                    .any(|h| h.key == b"a".to_vec())
            );
        }
    }

    #[test]
    fn phrase_search_without_index_scans() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("alpha beta gamma")).unwrap();
        c.insert(b"b", &doc("beta alpha gamma")).unwrap();
        // No text index → exact scan fallback.
        let hits = c.phrase_search("body", "alpha beta", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, b"a".to_vec());
    }

    #[test]
    fn stemming_matches_singular_and_plural() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("the running dogs")).unwrap();
        c.create_text_index("body").unwrap();
        // Query the singular; the plural in the doc was stemmed to match.
        let hits = c.text_search("body", "dog", 10).unwrap();
        assert!(hits.iter().any(|h| h.key == b"a".to_vec()));
        // A stop word matches nothing (it was never indexed).
        assert!(c.text_search("body", "the", 10).unwrap().is_empty());
    }

    #[test]
    fn empty_query_and_unindexed_field() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        c.create_text_index("body").unwrap();
        assert!(c.text_search("body", "  ", 10).unwrap().is_empty());
        // A field with no index still works via the exact path.
        assert!(c.text_search("title", "fox", 10).unwrap().is_empty());
    }

    /// A building on-disk text index is never served: text queries fall back
    /// to an exact scan and stay correct; the first unobstructed query
    /// resumes the backfill.
    #[test]
    fn building_ondisk_text_def_falls_back_then_resumes() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        // Forge a Building def exactly as an interrupted creation would
        // leave it, then reload the registry from that row.
        db.store()
            .put(
                TEXT_DEFS,
                &def_key("docs", "body"),
                &crate::index_build::encode_def(
                    &[kind_byte(TextKind::OnDisk)],
                    &crate::index_build::DefState::Building { cursor: vec![] },
                ),
            )
            .unwrap();
        db.load_text_defs().unwrap();
        assert_eq!(text_def_building(&db, "docs", "body"), Some(true));
        // With the resume lock held (another thread resuming), the building
        // def must not be served: fts_search reports "no usable index"...
        let _guard = db.index_resume().lock().unwrap();
        assert!(
            db.fts_search("docs", "body", "fox dog", 10)
                .unwrap()
                .is_none(),
            "a building on-disk index must not be served"
        );
        // ...so text_search falls back to an exact scan and stays correct.
        let hits = c.text_search("body", "fox dog", 10).unwrap();
        assert_eq!(hits[0].key, b"c".to_vec()); // matches both terms
        drop(_guard);
        // Once the resume lock is free, the next query resumes the backfill
        // and serves from the on-disk postings.
        let hits = c.text_search("body", "fox dog", 10).unwrap();
        assert_eq!(hits[0].key, b"c".to_vec());
        assert_eq!(text_def_building(&db, "docs", "body"), Some(false));
        assert!(
            db.fts_search("docs", "body", "fox dog", 10)
                .unwrap()
                .is_some(),
            "a completed on-disk index must serve"
        );
    }

    /// Registry-lag regression (wave-5 deferred guard, twin of the ANN one in
    /// index.rs): a reader whose registry snapshot says `Complete` can reach
    /// the disk postings search AFTER a concurrent registration/compaction
    /// committed its namespace clear + `Building` flip — the absent postings
    /// search as empty, which used to be served as a silent wrong answer.
    /// The empty-result re-check must re-read the def row: `Building` →
    /// `Ok(None)` (exact fallback), `Complete` → a genuinely empty index
    /// still serves empty.
    #[test]
    fn registry_lag_empty_disk_result_falls_back_when_row_says_building() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        // Register an on-disk index WITHOUT backfilling: def row → Building
        // over an EMPTY namespace (exactly the post-registration /
        // mid-compaction disk state).
        db.register_text_index("docs", "body", TextKind::OnDisk)
            .unwrap();
        // Pin the interleaving: with the resume lock held, the query's own
        // lazy-resume attempt cannot run (try-lock fails), so the forged
        // state below is what the reader actually observes.
        let _guard = db.index_resume().lock().unwrap();
        // Forge the in-memory def to Complete — the reader's stale registry
        // snapshot after a concurrent registration flipped the row beneath it.
        {
            let mut state = db.fts().lock().unwrap();
            state.defs.insert(
                ("docs".to_owned(), "body".to_owned()),
                (TextKind::OnDisk, false),
            );
        }
        // fts_search must NOT serve the silent empty result: the def row
        // says Building → Ok(None) (cannot serve → exact fallback).
        assert!(
            db.fts_search("docs", "body", "fox", 10).unwrap().is_none(),
            "a postings search over a just-cleared namespace must fall back, not serve empty"
        );
        // So text_search takes the exact fallback — at parity with an
        // unindexed twin.
        let twin = Db::open_in_memory().unwrap();
        seed(&twin);
        let c = db.collection("docs");
        let tc = twin.collection("docs");
        for q in ["fox", "dog", "fox dog"] {
            let got: Vec<_> = c
                .text_search("body", q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.key)
                .collect();
            let want: Vec<_> = tc
                .text_search("body", q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.key)
                .collect();
            assert_eq!(got, want, "fallback results for {q:?} must equal the twin");
        }
        // Negative control: flip the ROW to Complete as well — a genuinely
        // empty index keeps serving the empty result as before.
        db.store()
            .put(
                TEXT_DEFS,
                &def_key("docs", "body"),
                &crate::index_build::encode_def(
                    &[kind_byte(TextKind::OnDisk)],
                    &crate::index_build::DefState::Complete,
                ),
            )
            .unwrap();
        assert_eq!(
            db.fts_search("docs", "body", "fox", 10).unwrap(),
            Some(Vec::new()),
            "a Complete def over an empty namespace is a genuinely empty index"
        );
    }

    /// A text def row in the legacy kind-byte-only format (pre-state rows
    /// written by earlier versions) decodes as `Complete`: the index stays
    /// serviceable across the upgrade with no re-backfill.
    #[test]
    fn legacy_stateless_ondisk_def_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            seed(&db);
            db.collection("docs")
                .create_text_index_ondisk("body")
                .unwrap();
            // Overwrite the def row with the legacy kind-byte-only form.
            db.store()
                .put(
                    TEXT_DEFS,
                    &def_key("docs", "body"),
                    &[kind_byte(TextKind::OnDisk)],
                )
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(text_def_building(&db, "docs", "body"), Some(false));
        assert!(db.collect_building_text("docs").unwrap().is_empty());
        let hits = db
            .collection("docs")
            .text_search("body", "fox dog", 10)
            .unwrap();
        assert_eq!(hits[0].key, b"c".to_vec()); // served from the on-disk postings
    }

    /// An in-memory registration has no durable backfill: its def is born
    /// `Complete` and `create_text_index` behavior is unchanged.
    #[test]
    fn inmemory_registration_is_born_complete() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let c = db.collection("docs");
        c.create_text_index("body").unwrap();
        assert_eq!(text_def_building(&db, "docs", "body"), Some(false));
        assert!(db.collect_building_text("docs").unwrap().is_empty());
        // Lazily built on first query, correct results (unchanged behavior).
        let hits = c.text_search("body", "fox dog", 10).unwrap();
        assert_eq!(hits[0].key, b"c".to_vec());
        // The def row is the new-format Complete encoding of the InMemory kind.
        let row = db
            .store()
            .get(TEXT_DEFS, &def_key("docs", "body"))
            .unwrap()
            .unwrap();
        let (kb, st) = crate::index_build::decode_def(&row);
        assert_eq!(kb, vec![kind_byte(TextKind::InMemory)]);
        assert!(matches!(st, crate::index_build::DefState::Complete));
    }

    /// Whether `field` of `coll` is registered and still building (test probe
    /// into the registry; `None` when unregistered).
    fn text_def_building(db: &Db, coll: &str, field: &str) -> Option<bool> {
        let state = db.fts().lock().unwrap();
        state
            .defs
            .get(&(coll.to_owned(), field.to_owned()))
            .map(|(_, building)| *building)
    }

    /// Wave-4 final review, deterministic form of the first-query race
    /// (twin of the index.rs one): a read snapshot is pinned BEFORE any
    /// concurrent write, the writer then commits text documents, and only
    /// afterwards does the first query — running on that stale snapshot —
    /// trigger the lazy in-memory inverted-index build. The build MUST read
    /// fresh state (its own read transaction, opened under the registry
    /// lock): reading the caller's stale snapshot installs postings missing
    /// every writer document PERMANENTLY, because their post-commit
    /// maintenance (`fts_on_insert_memory`) no-opped while the index was
    /// still unbuilt. Reading fresh, only the stale query itself omits
    /// them — its document fetch drops keys its snapshot lacks
    /// (omission-only).
    #[test]
    fn stale_snapshot_first_query_hides_no_committed_text() {
        use std::sync::Arc;
        use std::sync::mpsc;

        let db = Arc::new(Db::open_in_memory().unwrap());
        let c = db.collection("docs");
        for i in 0..5u32 {
            c.insert(
                format!("s{i}").as_bytes(),
                &doc("seed corpus fox text about caching"),
            )
            .unwrap();
        }
        // Registered but UNBUILT: the first search below builds it.
        c.create_text_index("body").unwrap();

        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let writer = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                go_rx.recv().unwrap();
                for i in 0..50u32 {
                    db.collection("docs")
                        .insert(
                            format!("w{i}").as_bytes(),
                            &doc("writer corpus fox text about streaming"),
                        )
                        .unwrap();
                }
                done_tx.send(()).unwrap();
            })
        };

        // Pin the read snapshot NOW — the writer's commits are strictly
        // after it, so this reader can never see them.
        db.store()
            .read(|r| {
                go_tx.send(()).unwrap();
                done_rx.recv().unwrap();

                // First query on the stale snapshot: builds the postings.
                let ranked = db
                    .fts_search_in("docs", "body", "fox", 100, r)
                    .unwrap()
                    .expect("the registered in-memory index serves");
                // The build observed fresh state, so the writer's docs are
                // in the postings...
                assert!(
                    ranked.iter().any(|(k, _)| k.starts_with(b"w")),
                    "the lazy build must see commits newer than the caller's snapshot"
                );
                // ...but this stale query's document fetch drops exactly
                // those (omission-only).
                for (key, _) in &ranked {
                    let fetched = db.collection("docs").get_in(r, key).unwrap();
                    assert_eq!(
                        fetched.is_some(),
                        key.starts_with(b"s"),
                        "stale-query fetch must drop post-snapshot keys only"
                    );
                }
                Ok(())
            })
            .unwrap();
        writer.join().unwrap();

        // The permanent-hiding property: once writes stop, a later (fresh)
        // query finds EVERY committed document via the shared term "fox".
        let keys: Vec<Vec<u8>> = db
            .collection("docs")
            .text_search("body", "fox", 1000)
            .unwrap()
            .into_iter()
            .map(|h| h.key)
            .collect();
        for i in 0..5u32 {
            assert!(
                keys.contains(&format!("s{i}").into_bytes()),
                "seed document s{i} missing from the index"
            );
        }
        for i in 0..50u32 {
            assert!(
                keys.contains(&format!("w{i}").into_bytes()),
                "document w{i} was permanently hidden from the index"
            );
        }
    }
}
