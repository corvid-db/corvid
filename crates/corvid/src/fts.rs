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
use crate::error::Result;
use crate::store::Store;
use crate::text::{Bm25Params, idf, term_score, tokenize};
use crate::value::Value;

/// Reserved collection holding persisted text-index definitions.
const TEXT_DEFS: &str = "__text_indexes__";

/// Ranked `(key, score)` results, most relevant first.
type RankedKeys = Vec<(Vec<u8>, f32)>;

/// Per-database full-text index state.
#[derive(Default)]
pub(crate) struct FtsState {
    /// Registered `(collection, field)` text indexes.
    defs: std::collections::HashSet<(String, String)>,
    /// Built inverted indexes, populated lazily.
    built: HashMap<(String, String), Inverted>,
}

/// An inverted index over one text field.
#[derive(Default)]
struct Inverted {
    /// term -> (doc key -> term frequency).
    postings: HashMap<String, HashMap<Vec<u8>, u32>>,
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
        let mut tf: HashMap<String, u32> = HashMap::new();
        let mut len = 0usize;
        for token in tokenize(text) {
            *tf.entry(token).or_insert(0) += 1;
            len += 1;
        }
        for (term, count) in &tf {
            self.postings
                .entry(term.clone())
                .or_default()
                .insert(key.to_vec(), *count);
        }
        self.doc_terms
            .insert(key.to_vec(), tf.keys().cloned().collect());
        self.doc_len.insert(key.to_vec(), len);
        self.total_len += len;
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

        let mut query_terms = tokenize(query);
        query_terms.sort();
        query_terms.dedup();

        let mut scores: HashMap<Vec<u8>, f32> = HashMap::new();
        for term in &query_terms {
            let Some(posting) = self.postings.get(term) else {
                continue;
            };
            let term_idf = idf(n, posting.len());
            for (doc, &tf) in posting {
                let dl = self.doc_len.get(doc).copied().unwrap_or(0);
                *scores.entry(doc.clone()).or_insert(0.0) +=
                    term_score(tf, dl, avg_len, term_idf, params);
            }
        }

        let mut ranked: Vec<(Vec<u8>, f32)> =
            scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(k);
        ranked
    }
}

impl Db {
    /// Load persisted text-index definitions. Called once on open.
    pub(crate) fn load_text_defs(&self) -> Result<()> {
        let mut state = self.fts().lock().expect("fts lock");
        for (key, _) in self.store().scan(TEXT_DEFS)? {
            if let Some(def) = split_def_key(&key) {
                state.defs.insert(def);
            }
        }
        Ok(())
    }

    /// Register (or replace) a text index on `field` for `collection`.
    pub(crate) fn register_text_index(&self, collection: &str, field: &str) -> Result<()> {
        self.store()
            .put(TEXT_DEFS, &def_key(collection, field), b"")?;
        let mut state = self.fts().lock().expect("fts lock");
        let key = (collection.to_owned(), field.to_owned());
        state.defs.insert(key.clone());
        state.built.remove(&key);
        Ok(())
    }

    /// Maintain every text index on `collection` after a document write.
    pub(crate) fn fts_on_insert(&self, collection: &str, key: &[u8], doc: &Value) {
        let mut state = self.fts().lock().expect("fts lock");
        let fields: Vec<String> = state
            .defs
            .iter()
            .filter(|(c, _)| c == collection)
            .map(|(_, f)| f.clone())
            .collect();
        for field in fields {
            let map_key = (collection.to_owned(), field.clone());
            if let Some(inv) = state.built.get_mut(&map_key) {
                match doc.get(&field).and_then(Value::as_text) {
                    Some(text) => inv.add(key, text),
                    None => inv.remove(key),
                }
            }
        }
    }

    /// Remove `key` from every built text index on `collection` after a delete.
    pub(crate) fn fts_on_delete(&self, collection: &str, key: &[u8]) {
        let mut state = self.fts().lock().expect("fts lock");
        let map_keys: Vec<(String, String)> = state
            .built
            .keys()
            .filter(|(c, _)| c == collection)
            .cloned()
            .collect();
        for mk in map_keys {
            if let Some(inv) = state.built.get_mut(&mk) {
                inv.remove(key);
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
            if !state.defs.contains(&map_key) {
                return Ok(None);
            }
            !state.built.contains_key(&map_key)
        };
        if needs_build {
            let inv = build_inverted(self.store(), collection, field)?;
            let mut state = self.fts().lock().expect("fts lock");
            state.built.entry(map_key.clone()).or_insert(inv);
        }

        let state = self.fts().lock().expect("fts lock");
        Ok(state.built.get(&map_key).map(|inv| inv.search(query, k)))
    }
}

/// Build an inverted index for `field` by scanning `collection`.
fn build_inverted(store: &Store, collection: &str, field: &str) -> Result<Inverted> {
    let mut inv = Inverted::default();
    for (key, bytes) in store.scan(collection)? {
        let doc = Value::decode(&bytes)?;
        if let Some(text) = doc.get(field).and_then(Value::as_text) {
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
        self.db().register_text_index(self.name(), field)
    }
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
