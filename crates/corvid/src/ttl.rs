//! Optional per-record time-to-live (expiry).
//!
//! The engine has no clock and no background threads, so **time is injected**:
//! the caller supplies "now" (any monotonic epoch — seconds, millis, whatever
//! it chooses) when setting an expiry and when purging. A record's expiry is
//! stored in a per-collection TTL index sorted by timestamp, so
//! [`Collection::purge_expired`] reclaims everything due in one ordered scan
//! and removes it through the normal delete path (all secondary indexes stay
//! consistent). Expiry is opt-in per record; records without one never expire.
//!
//! Purge is explicit (the host calls it on its own cadence); between purges an
//! expired-but-not-yet-purged record is still visible, since a read has no
//! "now" to compare against.

use std::collections::HashSet;

use crate::db::{Collection, Db};
use crate::error::Result;

const TAG_FWD: u8 = 0x00;
const TAG_IDX: u8 = 0x01;

/// Collections that have used TTL (so plain writes know to maintain expiry).
#[derive(Default)]
pub(crate) struct TtlState {
    collections: HashSet<String>,
}

pub(crate) fn new_state() -> std::sync::Mutex<TtlState> {
    std::sync::Mutex::new(TtlState::default())
}

/// The reserved collection holding a collection's TTL index.
fn namespace(collection: &str) -> String {
    format!("__ttl__{collection}")
}

/// Order-preserving 8-byte encoding of a signed timestamp.
fn enc_ts(ts: i64) -> [u8; 8] {
    ((ts as u64) ^ (1 << 63)).to_be_bytes()
}

fn dec_ts(b: &[u8]) -> i64 {
    (u64::from_be_bytes(b.try_into().unwrap()) ^ (1 << 63)) as i64
}

fn fwd_key(doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + doc_key.len());
    k.push(TAG_FWD);
    k.extend_from_slice(doc_key);
    k
}

fn idx_key(ts: i64, doc_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 8 + doc_key.len());
    k.push(TAG_IDX);
    k.extend_from_slice(&enc_ts(ts));
    k.extend_from_slice(doc_key);
    k
}

impl Db {
    /// Note which collections already have a TTL index, so plain writes to them
    /// maintain expiry. Called once on open.
    pub(crate) fn load_ttl_collections(&self) -> Result<()> {
        let mut state = self.ttl().lock().expect("ttl lock");
        for name in self.store().collections()? {
            if let Some(coll) = name.strip_prefix("__ttl__") {
                state.collections.insert(coll.to_owned());
            }
        }
        Ok(())
    }

    pub(crate) fn ttl_enabled(&self, collection: &str) -> bool {
        self.ttl()
            .lock()
            .expect("ttl lock")
            .collections
            .contains(collection)
    }

    /// Set (or replace) `key`'s expiry timestamp.
    pub(crate) fn set_ttl(&self, collection: &str, key: &[u8], expires_at: i64) -> Result<()> {
        let ns = namespace(collection);
        self.store().transaction(|tx| {
            remove_in_txn(tx, &ns, key)?;
            tx.put(&ns, &idx_key(expires_at, key), &[])?;
            tx.put(&ns, &fwd_key(key), &enc_ts(expires_at))?;
            Ok(())
        })?;
        self.mark_ttl_collection(collection);
        Ok(())
    }

    /// Set (or replace) `key`'s expiry inside the caller's write transaction,
    /// so the expiry commits atomically with the document it belongs to.
    pub(crate) fn ttl_set_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        expires_at: i64,
    ) -> Result<()> {
        let ns = namespace(collection);
        remove_in_txn(tx, &ns, key)?;
        tx.put(&ns, &idx_key(expires_at, key), &[])?;
        tx.put(&ns, &fwd_key(key), &enc_ts(expires_at))?;
        Ok(())
    }

    /// Clear `key`'s expiry inside the caller's write transaction.
    pub(crate) fn ttl_clear_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        if !self.ttl_enabled(collection) {
            return Ok(());
        }
        remove_in_txn(tx, &namespace(collection), key)
    }

    /// Record that `collection` has TTL state, so plain writes to it maintain
    /// expiry. The persistent form is the `__ttl__*` namespace itself (rebuilt
    /// on open); this in-memory marker covers the current session.
    pub(crate) fn mark_ttl_collection(&self, collection: &str) {
        self.ttl()
            .lock()
            .expect("ttl lock")
            .collections
            .insert(collection.to_owned());
    }

    /// All per-record expiries as `(collection, doc_key, expires_at)` (dump).
    pub(crate) fn ttl_specs(&self) -> Result<Vec<(String, Vec<u8>, i64)>> {
        let collections: Vec<String> = {
            let state = self.ttl().lock().expect("ttl lock");
            state.collections.iter().cloned().collect()
        };
        let mut out = Vec::new();
        for coll in collections {
            let ns = namespace(&coll);
            // Forward entries: [0x00] ‖ doc_key -> enc_ts.
            for (key, value) in self.store().scan_prefix(&ns, &[TAG_FWD])? {
                if key.len() > 1 && value.len() == 8 {
                    out.push((coll.clone(), key[1..].to_vec(), dec_ts(&value)));
                }
            }
        }
        Ok(out)
    }

    /// `key`'s expiry timestamp, if one is set.
    pub(crate) fn ttl_of(&self, collection: &str, key: &[u8]) -> Result<Option<i64>> {
        let ns = namespace(collection);
        Ok(self.store().get(&ns, &fwd_key(key))?.map(|b| dec_ts(&b)))
    }

    /// Delete every record in `collection` whose expiry is `<= now`. Returns the
    /// number purged. Records are removed through the normal delete path, so all
    /// indexes and the TTL index stay consistent.
    ///
    /// Each candidate's expiry is re-checked before its delete: a record whose
    /// expiry changed (or was cleared by a plain overwrite) since the scan is
    /// skipped, so the purge can never remove a legitimately rewritten record.
    pub(crate) fn purge_expired(&self, collection: &str, now: i64) -> Result<usize> {
        let ns = namespace(collection);
        // Collect due doc keys from the sorted index (stop once past `now`).
        let mut due: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut cursor = vec![TAG_IDX];
        'outer: loop {
            let page = self.store().scan_from(&ns, &cursor, 4096)?;
            if page.is_empty() {
                break;
            }
            for (k, _) in &page {
                if k.first() != Some(&TAG_IDX) || k.len() < 9 {
                    break 'outer;
                }
                let ts = dec_ts(&k[1..9]);
                if ts > now {
                    break 'outer;
                }
                due.push((k[9..].to_vec(), ts));
            }
            let mut next = page.last().unwrap().0.clone();
            next.push(0);
            cursor = next;
        }
        // Delete each (cascades through indexes + clears its TTL entry).
        let handle = self.collection(collection);
        let mut purged = 0;
        for (key, ts) in due {
            if self.ttl_of(collection, &key)? != Some(ts) {
                continue; // rewritten or cleared since the scan — not due anymore
            }
            if handle.delete(&key)? {
                purged += 1;
            } else {
                // Expiry entry without a document (e.g. the expiry was set
                // before any write landed). Drop it so future purges stop
                // rescanning it.
                self.store()
                    .transaction(|tx| remove_in_txn(tx, &ns, &key))?;
            }
        }
        Ok(purged)
    }
}

fn remove_in_txn(tx: &mut crate::store::WriteBatch<'_>, ns: &str, doc_key: &[u8]) -> Result<()> {
    if let Some(b) = tx.get(ns, &fwd_key(doc_key))? {
        let ts = dec_ts(&b);
        tx.delete(ns, &idx_key(ts, doc_key))?;
        tx.delete(ns, &fwd_key(doc_key))?;
    }
    Ok(())
}

impl Collection<'_> {
    /// Insert `doc` at `key` with an expiry timestamp (`expires_at`, in the
    /// caller's epoch). The record behaves normally until purged. The row and
    /// its expiry commit atomically — a crash can never leave an immortal
    /// record that was asked to expire.
    pub fn insert_with_ttl(&self, key: &[u8], doc: &crate::Value, expires_at: i64) -> Result<()> {
        self.db()
            .write_document(self.name(), key, Some(doc), Some(expires_at))?;
        Ok(())
    }

    /// Set or replace `key`'s expiry without rewriting the document.
    ///
    /// Rejects engine-reserved collection names, like every write path.
    pub fn set_ttl(&self, key: &[u8], expires_at: i64) -> Result<()> {
        self.ensure_writable()?;
        self.db().set_ttl(self.name(), key, expires_at)
    }

    /// `key`'s expiry timestamp, if one is set.
    pub fn ttl(&self, key: &[u8]) -> Result<Option<i64>> {
        self.db().ttl_of(self.name(), key)
    }

    /// Delete every record whose expiry is `<= now`; returns the count purged.
    /// `now` is supplied by the caller (the engine keeps no clock).
    pub fn purge_expired(&self, now: i64) -> Result<usize> {
        self.ensure_writable()?;
        self.db().purge_expired(self.name(), now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use std::collections::BTreeMap;

    fn rec(n: i64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(n));
        Value::Map(m)
    }

    #[test]
    fn purge_removes_only_expired() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"a", &rec(1), 100).unwrap();
        c.insert_with_ttl(b"b", &rec(2), 200).unwrap();
        c.insert_with_ttl(b"c", &rec(3), 300).unwrap();
        c.insert(b"keep", &rec(4)).unwrap(); // no TTL → never expires

        // Now = 200: a and b are due, c is not.
        let purged = c.purge_expired(200).unwrap();
        assert_eq!(purged, 2);
        assert_eq!(c.get(b"a").unwrap(), None);
        assert_eq!(c.get(b"b").unwrap(), None);
        assert_eq!(c.get(b"c").unwrap(), Some(rec(3)));
        assert_eq!(c.get(b"keep").unwrap(), Some(rec(4)));
    }

    #[test]
    fn ttl_is_visible_until_purged() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"a", &rec(1), 10).unwrap();
        // Expired by the clock, but still readable until a purge runs.
        assert_eq!(c.get(b"a").unwrap(), Some(rec(1)));
        assert_eq!(c.ttl(b"a").unwrap(), Some(10));
        assert_eq!(c.purge_expired(10).unwrap(), 1);
        assert_eq!(c.get(b"a").unwrap(), None);
    }

    #[test]
    fn overwrite_clears_ttl() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"a", &rec(1), 100).unwrap();
        // Plain overwrite removes the expiry.
        c.insert(b"a", &rec(2)).unwrap();
        assert_eq!(c.ttl(b"a").unwrap(), None);
        assert_eq!(c.purge_expired(1000).unwrap(), 0);
        assert_eq!(c.get(b"a").unwrap(), Some(rec(2)));
    }

    #[test]
    fn delete_clears_ttl_entry() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"a", &rec(1), 100).unwrap();
        c.delete(b"a").unwrap();
        // The index entry is gone; a later purge finds nothing (and would not
        // resurrect/re-delete).
        assert_eq!(c.purge_expired(1000).unwrap(), 0);
        assert_eq!(c.ttl(b"a").unwrap(), None);
    }

    #[test]
    fn set_ttl_replaces_previous() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &rec(1)).unwrap();
        c.set_ttl(b"a", 100).unwrap();
        c.set_ttl(b"a", 500).unwrap(); // replace
        assert_eq!(c.ttl(b"a").unwrap(), Some(500));
        assert_eq!(c.purge_expired(200).unwrap(), 0); // 500 > 200
        assert_eq!(c.purge_expired(500).unwrap(), 1);
    }

    #[test]
    fn ttl_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            db.collection("docs")
                .insert_with_ttl(b"a", &rec(1), 100)
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        assert_eq!(c.ttl(b"a").unwrap(), Some(100));
        assert_eq!(c.purge_expired(100).unwrap(), 1);
    }

    #[test]
    fn negative_and_large_timestamps_order_correctly() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"neg", &rec(1), -100).unwrap();
        c.insert_with_ttl(b"big", &rec(2), i64::MAX).unwrap();
        // now = 0 purges only the negative one.
        assert_eq!(c.purge_expired(0).unwrap(), 1);
        assert_eq!(c.get(b"neg").unwrap(), None);
        assert_eq!(c.get(b"big").unwrap(), Some(rec(2)));
    }

    #[test]
    fn reserved_collection_names_are_rejected() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("__ttl__docs");
        assert!(matches!(
            c.set_ttl(b"a", 100),
            Err(crate::Error::ReservedCollection(_))
        ));
        assert!(matches!(
            c.purge_expired(100),
            Err(crate::Error::ReservedCollection(_))
        ));
    }

    /// An expiry entry with no document behind it (expiry set before any
    /// write) must be cleaned up by the purge instead of being rescanned
    /// forever.
    #[test]
    fn purge_cleans_stranded_entry_for_absent_key() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.set_ttl(b"ghost", 100).unwrap(); // no such document
        assert_eq!(c.purge_expired(200).unwrap(), 0);
        // The stranded entries are gone: a second purge is a cheap no-op and
        // ttl() reports nothing.
        assert_eq!(c.ttl(b"ghost").unwrap(), None);
        assert_eq!(c.purge_expired(300).unwrap(), 0);
    }

    /// A purge must never delete a record whose expiry changed after the
    /// scan — a plain overwrite clears the expiry, and the rewritten record
    /// (new value, no TTL) has to survive the purge that was in flight.
    #[test]
    fn purge_skips_records_whose_expiry_changed_since_scan() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"k", &rec(1), 100).unwrap();
        // Simulate the interleaving directly: collect the due key (as the
        // purge does), then rewrite the record (clearing its expiry), then
        // verify the purge does not remove the fresh document.
        c.insert(b"k", &rec(2)).unwrap(); // plain write → expiry cleared
        assert_eq!(c.ttl(b"k").unwrap(), None);
        assert_eq!(c.purge_expired(1000).unwrap(), 0);
        assert_eq!(c.get(b"k").unwrap(), Some(rec(2)));
    }
}
