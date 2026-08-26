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
use crate::store::Store;
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
    /// Registered `(collection, field)` text indexes and where each lives.
    defs: HashMap<(String, String), TextKind>,
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
    /// Load persisted text-index definitions. Called once on open.
    pub(crate) fn load_text_defs(&self) -> Result<()> {
        let mut state = self.fts().lock().expect("fts lock");
        for (key, value) in self.store().scan(TEXT_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                state.defs.insert(def, kind_from(value.first()));
            }
        }
        Ok(())
    }

    /// All text index definitions as `(collection, field, on_disk)` (for dump).
    pub(crate) fn text_specs(&self) -> Vec<(String, String, bool)> {
        let state = self.fts().lock().expect("fts lock");
        state
            .defs
            .iter()
            .map(|((c, f), kind)| (c.clone(), f.clone(), *kind == TextKind::OnDisk))
            .collect()
    }

    /// Register (or replace) a text index on `field` for `collection`.
    pub(crate) fn register_text_index(
        &self,
        collection: &str,
        field: &str,
        kind: TextKind,
    ) -> Result<()> {
        self.store()
            .put(TEXT_DEFS, &def_key(collection, field), &[kind_byte(kind)])?;
        let mut state = self.fts().lock().expect("fts lock");
        let key = (collection.to_owned(), field.to_owned());
        state.defs.insert(key.clone(), kind);
        state.built.remove(&key);
        Ok(())
    }

    /// Maintain every on-disk text index on `collection` inside the caller's
    /// write transaction, so postings commit atomically with the document.
    /// In-memory indexes are handled post-commit by [`Db::fts_on_insert_memory`].
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
                .map(|((_, f), k)| (f.clone(), *k))
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
                .map(|((_, f), k)| (f.clone(), *k))
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
                .map(|((_, f), k)| (f.clone(), *k))
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
                .map(|((_, f), k)| (f.clone(), *k))
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
    /// otherwise `None` (the caller falls back to an exact scan).
    pub(crate) fn fts_search(
        &self,
        collection: &str,
        field: &str,
        query: &str,
        k: usize,
    ) -> Result<Option<RankedKeys>> {
        let map_key = (collection.to_owned(), field.to_owned());

        let needs_build = {
            let state = self.fts().lock().expect("fts lock");
            match state.defs.get(&map_key) {
                None => return Ok(None),
                // On-disk postings are maintained on every write; no build,
                // no lock held during the scan.
                Some(TextKind::OnDisk) => {
                    let ns = disk_fts::namespace(collection, field);
                    drop(state);
                    return Ok(Some(disk_fts::search(self.store(), &ns, query, k)?));
                }
                Some(TextKind::InMemory) => !state.built.contains_key(&map_key),
            }
        };
        if needs_build {
            let inv = build_inverted(self.store(), collection, field)?;
            let mut state = self.fts().lock().expect("fts lock");
            state.built.entry(map_key.clone()).or_insert(inv);
        }

        let state = self.fts().lock().expect("fts lock");
        Ok(state.built.get(&map_key).map(|inv| inv.search(query, k)))
    }

    /// Like [`Db::fts_search`] but matching `phrase` as a consecutive in-order
    /// run of tokens (positions). `None` if `field` has no text index.
    pub(crate) fn fts_phrase_search(
        &self,
        collection: &str,
        field: &str,
        phrase: &str,
        k: usize,
    ) -> Result<Option<RankedKeys>> {
        let map_key = (collection.to_owned(), field.to_owned());
        let needs_build = {
            let state = self.fts().lock().expect("fts lock");
            match state.defs.get(&map_key) {
                None => return Ok(None),
                Some(TextKind::OnDisk) => {
                    let ns = disk_fts::namespace(collection, field);
                    drop(state);
                    return Ok(Some(disk_fts::phrase_search(self.store(), &ns, phrase, k)?));
                }
                Some(TextKind::InMemory) => !state.built.contains_key(&map_key),
            }
        };
        if needs_build {
            let inv = build_inverted(self.store(), collection, field)?;
            let mut state = self.fts().lock().expect("fts lock");
            state.built.entry(map_key.clone()).or_insert(inv);
        }
        let state = self.fts().lock().expect("fts lock");
        Ok(state
            .built
            .get(&map_key)
            .map(|inv| inv.phrase_search(phrase, k)))
    }
}

/// Build an inverted index for `field` by scanning `collection`.
fn build_inverted(store: &Store, collection: &str, field: &str) -> Result<Inverted> {
    let mut inv = Inverted::default();
    for (key, bytes) in store.scan(collection)? {
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
        self.db()
            .register_text_index(self.name(), field, TextKind::InMemory)
    }

    /// Create (or replace) an **on-disk** inverted text index on `field`.
    ///
    /// Postings are stored as redb entries, so memory stays bounded by the
    /// query terms (not the corpus) and the index persists across reopen with
    /// no rebuild. Existing documents are backfilled now; later writes maintain
    /// it incrementally.
    pub fn create_text_index_ondisk(&self, field: &str) -> Result<()> {
        self.db()
            .register_text_index(self.name(), field, TextKind::OnDisk)?;
        let ns = disk_fts::namespace(self.name(), field);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = self.db().store().scan_from(self.name(), &cursor, 2048)?;
            if page.is_empty() {
                break;
            }
            let mut batch: Vec<(Vec<u8>, String)> = Vec::new();
            for (key, bytes) in &page {
                let doc = Value::decode(bytes)?;
                if let Some(text) = doc.get_path(field).and_then(Value::as_text) {
                    batch.push((key.clone(), text.to_owned()));
                }
            }
            if !batch.is_empty() {
                disk_fts::insert_many(self.db().store(), &ns, &batch)?;
            }
            cursor = next_key(&page.last().unwrap().0);
        }
        Ok(())
    }
}

/// The smallest key strictly greater than `key` (append a zero byte).
fn next_key(key: &[u8]) -> Vec<u8> {
    let mut k = key.to_vec();
    k.push(0);
    k
}

#[cfg(test)]
mod tests {
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
}
