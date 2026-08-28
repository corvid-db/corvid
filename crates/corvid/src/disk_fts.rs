//! On-disk inverted full-text index.
//!
//! The in-memory inverted index ([`crate::fts`]) holds all postings in RAM and
//! rebuilds on open. This stores postings as redb entries so memory is bounded
//! by the query terms' postings (not the corpus) and the index persists.
//!
//! Layout in a reserved collection (`__dfts__<coll>__<field>`):
//! - `P ‖ u32(term_len) ‖ term ‖ doc_key` → `tf(u32) ‖ doc_len(u32)` (postings,
//!   with the document length denormalised so scoring needs no extra lookup;
//!   the term-length prefix lets one prefix scan fetch a term's postings).
//! - `F ‖ doc_key` → `u32(doc_len) ‖ terms` (forward list, for removal).
//! - `M` → `u64(n_docs) ‖ u64(total_len)` (corpus stats).

use std::collections::HashMap;

use crate::error::Result;
use crate::store::SnapshotReader;
use crate::text::{Bm25Params, analyze, idf, term_score};

const TAG_POST: u8 = b'P';
const TAG_FWD: u8 = b'F';
const TAG_META: u8 = b'M';

type Ranked = Vec<(Vec<u8>, f32)>;

/// The reserved collection holding an on-disk text index.
pub(crate) fn namespace(collection: &str, field: &str) -> String {
    format!("__dfts__{collection}__{field}")
}

fn term_prefix(term: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 4 + term.len());
    k.push(TAG_POST);
    k.extend_from_slice(&(term.len() as u32).to_be_bytes());
    k.extend_from_slice(term.as_bytes());
    k
}

fn posting_key(term: &str, doc_key: &[u8]) -> Vec<u8> {
    let mut k = term_prefix(term);
    k.extend_from_slice(doc_key);
    k
}

fn fwd_key(doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + doc_key.len());
    k.push(TAG_FWD);
    k.extend_from_slice(doc_key);
    k
}

/// Posting value: `doc_len(u32) ‖ n_pos(u32) ‖ positions(u32 each)`. The term
/// frequency is `n_pos`; the positions enable phrase queries.
fn encode_posting(positions: &[u32], doc_len: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + positions.len() * 4);
    v.extend_from_slice(&doc_len.to_le_bytes());
    v.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    for p in positions {
        v.extend_from_slice(&p.to_le_bytes());
    }
    v
}

/// Decode a posting into `(tf, doc_len)`.
fn decode_posting(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 8 {
        return None;
    }
    let doc_len = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let n_pos = u32::from_le_bytes(b[4..8].try_into().unwrap());
    Some((n_pos, doc_len))
}

/// Decode a posting's positions list (for phrase matching).
fn decode_positions(b: &[u8]) -> Vec<u32> {
    if b.len() < 8 {
        return Vec::new();
    }
    let n_pos = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(n_pos);
    let mut pos = 8;
    for _ in 0..n_pos {
        match b.get(pos..pos + 4) {
            Some(chunk) => out.push(u32::from_le_bytes(chunk.try_into().unwrap())),
            None => break,
        }
        pos += 4;
    }
    out
}

fn encode_fwd(doc_len: u32, terms: &[String]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&doc_len.to_le_bytes());
    v.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    for t in terms {
        v.extend_from_slice(&(t.len() as u32).to_le_bytes());
        v.extend_from_slice(t.as_bytes());
    }
    v
}

fn decode_fwd(b: &[u8]) -> Option<(u32, Vec<String>)> {
    if b.len() < 8 {
        return None;
    }
    let doc_len = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let count = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let mut pos = 8;
    let mut terms = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u32::from_le_bytes(b.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let t = std::str::from_utf8(b.get(pos..pos + len)?).ok()?.to_owned();
        pos += len;
        terms.push(t);
    }
    Some((doc_len, terms))
}

#[derive(Clone, Copy, Default)]
struct Meta {
    n: u64,
    total_len: u64,
}

fn encode_meta(m: Meta) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&m.n.to_le_bytes());
    v.extend_from_slice(&m.total_len.to_le_bytes());
    v
}

fn decode_meta(b: &[u8]) -> Meta {
    if b.len() < 16 {
        return Meta::default();
    }
    Meta {
        n: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        total_len: u64::from_le_bytes(b[8..16].try_into().unwrap()),
    }
}

/// Test seam: index one document in its own transaction.
#[cfg(test)]
pub(crate) fn insert(
    store: &crate::store::Store,
    ns: &str,
    doc_key: &[u8],
    text: &str,
) -> Result<()> {
    store.transaction(|tx| insert_in_txn(tx, ns, doc_key, text))
}

/// Test seam: remove one document in its own transaction.
#[cfg(test)]
pub(crate) fn delete(store: &crate::store::Store, ns: &str, doc_key: &[u8]) -> Result<()> {
    store.transaction(|tx| delete_in_txn(tx, ns, doc_key))
}

/// Index (or re-index) `doc_key`'s `text` inside a caller's transaction, so
/// postings and corpus stats commit atomically with the document.
pub(crate) fn insert_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
    text: &str,
) -> Result<()> {
    let mut meta = read_meta(tx, ns)?;
    index_in_txn(tx, ns, &mut meta, doc_key, text)?;
    tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    Ok(())
}

/// Remove `doc_key` inside a caller's transaction. META is rewritten only
/// when something was actually removed (a delete of a non-indexed key is a
/// no-op rather than a stats rewrite).
pub(crate) fn delete_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    doc_key: &[u8],
) -> Result<()> {
    let mut meta = read_meta(tx, ns)?;
    if remove_in_txn(tx, ns, &mut meta, doc_key)? {
        tx.put(ns, &[TAG_META], &encode_meta(meta))?;
    }
    Ok(())
}

fn read_meta(tx: &crate::store::WriteBatch<'_>, ns: &str) -> Result<Meta> {
    Ok(tx
        .get(ns, &[TAG_META])?
        .map(|b| decode_meta(&b))
        .unwrap_or_default())
}

fn remove_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    meta: &mut Meta,
    doc_key: &[u8],
) -> Result<bool> {
    if let Some(fwd) = tx.get(ns, &fwd_key(doc_key))?
        && let Some((old_len, terms)) = decode_fwd(&fwd)
    {
        for t in &terms {
            tx.delete(ns, &posting_key(t, doc_key))?;
        }
        tx.delete(ns, &fwd_key(doc_key))?;
        meta.n = meta.n.saturating_sub(1);
        meta.total_len = meta.total_len.saturating_sub(old_len as u64);
        return Ok(true);
    }
    Ok(false)
}

fn index_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    ns: &str,
    meta: &mut Meta,
    doc_key: &[u8],
    text: &str,
) -> Result<()> {
    // Replace any existing entry first.
    remove_in_txn(tx, ns, meta, doc_key)?;

    let mut positions: HashMap<String, Vec<u32>> = HashMap::new();
    let mut len = 0u32;
    for (pos, token) in analyze(text).into_iter().enumerate() {
        positions.entry(token).or_default().push(pos as u32);
        len += 1;
    }
    if positions.is_empty() {
        // Still counts as a document (length 0) for corpus stats parity.
        tx.put(ns, &fwd_key(doc_key), &encode_fwd(0, &[]))?;
        meta.n += 1;
        return Ok(());
    }

    let terms: Vec<String> = positions.keys().cloned().collect();
    for (term, pos_list) in &positions {
        tx.put(
            ns,
            &posting_key(term, doc_key),
            &encode_posting(pos_list, len),
        )?;
    }
    tx.put(ns, &fwd_key(doc_key), &encode_fwd(len, &terms))?;
    meta.n += 1;
    meta.total_len += len as u64;
    Ok(())
}

/// BM25 search over the on-disk postings on `reader`'s snapshot, touching
/// only the query terms (audit B3: postings and the caller's document
/// fetches share one point in time).
pub(crate) fn search(
    reader: &dyn SnapshotReader,
    ns: &str,
    query: &str,
    k: usize,
) -> Result<Ranked> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let meta = reader
        .get(ns, &[TAG_META])?
        .map(|b| decode_meta(&b))
        .unwrap_or_default();
    let n = meta.n as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let avg_len = match meta.total_len as f32 / n as f32 {
        0.0 => 1.0,
        v => v,
    };
    let params = Bm25Params::default();

    let mut query_terms = analyze(query);
    query_terms.sort();
    query_terms.dedup();

    let mut scores: HashMap<Vec<u8>, f32> = HashMap::new();
    for term in &query_terms {
        let prefix = term_prefix(term);
        let postings = reader.scan_prefix(ns, &prefix)?;
        let df = postings.len();
        if df == 0 {
            continue;
        }
        let term_idf = idf(n, df);
        for (key, value) in postings {
            let Some((tf, doc_len)) = decode_posting(&value) else {
                continue;
            };
            let doc_key = key.get(prefix.len()..).unwrap_or(&[]).to_vec();
            *scores.entry(doc_key).or_insert(0.0) +=
                term_score(tf, doc_len as usize, avg_len, term_idf, params);
        }
    }

    let mut ranked: Vec<(Vec<u8>, f32)> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(k);
    Ok(ranked)
}

/// Phrase search: BM25-ranked docs containing the analyzed `phrase` as a
/// consecutive in-order run of tokens, using stored positions, all reads on
/// `reader`'s snapshot (audit B3).
pub(crate) fn phrase_search(
    reader: &dyn SnapshotReader,
    ns: &str,
    phrase: &str,
    k: usize,
) -> Result<Ranked> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let meta = reader
        .get(ns, &[TAG_META])?
        .map(|b| decode_meta(&b))
        .unwrap_or_default();
    let n = meta.n as usize;
    let terms = analyze(phrase);
    if n == 0 || terms.is_empty() {
        return Ok(Vec::new());
    }
    let avg_len = match meta.total_len as f32 / n as f32 {
        0.0 => 1.0,
        v => v,
    };
    let params = Bm25Params::default();

    // For each term, map doc_key -> (positions, doc_len), and remember df.
    type DocPostings = HashMap<Vec<u8>, (Vec<u32>, u32)>;
    let mut per_term: Vec<(DocPostings, usize)> = Vec::with_capacity(terms.len());
    for term in &terms {
        let prefix = term_prefix(term);
        let postings = reader.scan_prefix(ns, &prefix)?;
        let df = postings.len();
        if df == 0 {
            return Ok(Vec::new()); // a term absent everywhere → no phrase match
        }
        let mut m = HashMap::with_capacity(df);
        for (key, value) in postings {
            let doc_key = key.get(prefix.len()..).unwrap_or(&[]).to_vec();
            let (_, doc_len) = decode_posting(&value).unwrap_or((0, 0));
            m.insert(doc_key, (decode_positions(&value), doc_len));
        }
        per_term.push((m, df));
    }

    // Candidate docs are those containing the first term; verify alignment.
    let (first_map, _) = &per_term[0];
    let mut ranked: Vec<(Vec<u8>, f32)> = Vec::new();
    'docs: for (doc, (first_pos, doc_len)) in first_map {
        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(terms.len());
        lists.push(first_pos);
        for (map, _) in &per_term[1..] {
            match map.get(doc) {
                Some((ps, _)) => lists.push(ps),
                None => continue 'docs,
            }
        }
        if !phrase_aligned(&lists) {
            continue;
        }
        let score: f32 = per_term
            .iter()
            .zip(&lists)
            .map(|((_, df), ps)| {
                term_score(
                    ps.len() as u32,
                    *doc_len as usize,
                    avg_len,
                    idf(n, *df),
                    params,
                )
            })
            .sum();
        ranked.push((doc.clone(), score));
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(k);
    Ok(ranked)
}

/// Whether the sorted position lists occur consecutively and in order: some
/// `p` with `lists[i]` containing `p + i` for all `i`.
fn phrase_aligned(lists: &[&Vec<u32>]) -> bool {
    if lists.len() == 1 {
        return !lists[0].is_empty();
    }
    for &start in lists[0] {
        if (1..lists.len()).all(|i| {
            start
                .checked_add(i as u32)
                .is_some_and(|want| lists[i].binary_search(&want).is_ok())
        }) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn build(store: &Store, ns: &str) {
        insert(store, ns, b"a", "the quick brown fox").unwrap();
        insert(store, ns, b"b", "the lazy dog sleeps").unwrap();
        insert(store, ns, b"c", "a fox and a dog play").unwrap();
    }

    #[test]
    fn search_ranks_and_filters() {
        let store = Store::open_in_memory().unwrap();
        build(&store, "ix");
        let hits = search(&store, "ix", "fox dog", 10).unwrap();
        // c contains both query terms → ranks first.
        assert_eq!(hits[0].0, b"c".to_vec());
        // a (fox), b (dog), c (both) all match a query term.
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn empty_and_missing_term() {
        let store = Store::open_in_memory().unwrap();
        build(&store, "ix");
        assert!(search(&store, "ix", "", 5).unwrap().is_empty());
        assert!(search(&store, "ix", "zzz", 5).unwrap().is_empty());
    }

    #[test]
    fn overwrite_reindexes() {
        let store = Store::open_in_memory().unwrap();
        build(&store, "ix");
        insert(&store, "ix", b"a", "totally different now").unwrap();
        assert!(
            search(&store, "ix", "quick", 5)
                .unwrap()
                .iter()
                .all(|(k, _)| k != b"a")
        );
        assert!(
            search(&store, "ix", "different", 5)
                .unwrap()
                .iter()
                .any(|(k, _)| k == b"a")
        );
    }

    #[test]
    fn delete_removes() {
        let store = Store::open_in_memory().unwrap();
        build(&store, "ix");
        delete(&store, "ix", b"a").unwrap();
        assert!(search(&store, "ix", "quick", 5).unwrap().is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.db");
        {
            let store = Store::open(&path).unwrap();
            build(&store, "ix");
        }
        let store = Store::open(&path).unwrap();
        let hits = search(&store, "ix", "fox", 10).unwrap();
        assert_eq!(hits.len(), 2);
    }
}
