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

/// Collections that have used TTL this session. Not load-bearing for write
/// correctness (audit B2): plain writes decide expiry maintenance inside
/// their transaction by probing the `__ttl__<collection>` namespace itself,
/// so a marker that lags a concurrent commit cannot skip a needed clear.
/// No read path consults it either: dump enumerates the persisted
/// `__ttl__*` namespaces on its own snapshot ([`ttl_specs_in`]), so the
/// durable namespaces are the only source of truth. The marker remains a
/// session-local bookkeeping record (maintained on every TTL-writing commit
/// and rebuilt on open), retained so future session-scoped fast paths have
/// a cache to consult.
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
    /// Rebuild the session's TTL-collection markers from the store. Called
    /// once on open. (The `__ttl__*` namespaces themselves are the durable
    /// record; the marker is only a cache.)
    pub(crate) fn load_ttl_collections(&self) -> Result<()> {
        let mut state = self.ttl().lock().expect("ttl lock");
        for name in self.store().collections()? {
            if let Some(coll) = name.strip_prefix("__ttl__") {
                state.collections.insert(coll.to_owned());
            }
        }
        Ok(())
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
    ///
    /// The maintenance decision is made IN the transaction (audit B2): the
    /// per-key forward lookup inside [`remove_in_txn`] is the probe. A write
    /// transaction observes the latest committed state, so even a collection
    /// whose in-memory marker has not been set yet (another writer committed
    /// TTL state moments ago) gets its stale expiry cleared — the marker is
    /// never consulted here, so it cannot race the decision. For a key with
    /// no expiry entry the probe is a single point-read and a no-op.
    pub(crate) fn ttl_clear_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
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

    /// `key`'s expiry timestamp, if one is set.
    pub(crate) fn ttl_of(&self, collection: &str, key: &[u8]) -> Result<Option<i64>> {
        let ns = namespace(collection);
        Ok(self.store().get(&ns, &fwd_key(key))?.map(|b| dec_ts(&b)))
    }

    /// The collect phase of a purge: every `(doc_key, expires_at)` in
    /// `collection` whose expiry is `<= now`, in expiry order. Exposed so
    /// the collect → mutate → delete interleaving is testable (audit B2):
    /// the delete phase re-verifies each entry, so anything mutated after
    /// this scan is skipped, not deleted.
    pub(crate) fn due_keys(&self, collection: &str, now: i64) -> Result<Vec<(Vec<u8>, i64)>> {
        let ns = namespace(collection);
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
        Ok(due)
    }

    /// The delete phase for one key collected by [`Db::due_keys`]:
    /// compare-expiry-and-delete in ONE transaction (audit B2). The forward
    /// entry is re-read inside the transaction, and the record is removed —
    /// through the same in-transaction index maintenance a delete uses —
    /// only if it still decodes to exactly `ts`. If the entry changed or
    /// vanished (the record was rewritten, re-expired, or deleted since the
    /// scan), nothing happens: the purge can never remove a legitimately
    /// rewritten record. A stranded expiry entry (no document behind it) is
    /// dropped without counting — but still through the edge cascade, like
    /// every delete path (W3 ruling: a stranded TTL entry must not leave
    /// dangling edges). Returns whether a document was removed.
    pub(crate) fn purge_due_key(&self, collection: &str, key: &[u8], ts: i64) -> Result<bool> {
        let ns = namespace(collection);
        let removed = self.store().transaction(|tx| {
            // Compare-expiry: the forward entry must still be the exact
            // expiry the collect phase observed.
            match tx.get(&ns, &fwd_key(key))? {
                Some(b) if dec_ts(&b) == ts => {}
                _ => return Ok(false), // rewritten or cleared since the scan
            }
            let existed = tx.delete(collection, key)?;
            if existed {
                // The same in-transaction cascade a normal delete performs
                // (TTL entries are removed explicitly below instead).
                self.index_on_delete_in_txn(tx, collection, key)?;
                self.fts_on_delete_in_txn(tx, collection, key)?;
                self.scalar_on_delete_in_txn(tx, collection, key)?;
                self.compound_on_delete_in_txn(tx, collection, key)?;
                self.geo_on_delete_in_txn(tx, collection, key)?;
            }
            // The edge cascade runs REGARDLESS of `existed` (W3 ruling):
            // same contract as delete — a stranded expiry entry on a key
            // that never was (or no longer is) a document must not leave
            // dangling edges behind.
            self.edges_on_delete_in_txn(tx, collection, key)?;
            // Drop both TTL entries. The forward entry was verified above,
            // inside this same transaction, so these are exactly the
            // collected entries.
            tx.delete(&ns, &idx_key(ts, key))?;
            tx.delete(&ns, &fwd_key(key))?;
            Ok(existed)
        })?;
        if removed {
            self.finish_applied(collection, key, None);
        }
        Ok(removed)
    }

    /// Delete every record in `collection` whose expiry is `<= now`. Returns the
    /// number purged. Records are removed through the normal delete path, so all
    /// indexes and the TTL index stay consistent.
    ///
    /// Each candidate's expiry is re-verified inside the same transaction as
    /// its delete: a record whose expiry changed (or was cleared by a plain
    /// overwrite) since the scan is skipped, so the purge can never remove a
    /// legitimately rewritten record.
    pub(crate) fn purge_expired(&self, collection: &str, now: i64) -> Result<usize> {
        let mut purged = 0;
        for (key, ts) in self.due_keys(collection, now)? {
            if self.purge_due_key(collection, &key, ts)? {
                purged += 1;
            }
        }
        Ok(purged)
    }
}

/// All per-record expiries as `(collection, doc_key, expires_at)` (dump).
///
/// Enumerates the TTL collections from the READER's catalog — stripping the
/// `__ttl__` prefixes, mirroring [`Db::load_ttl_collections`] — and reads
/// each namespace's forward entries through the same reader, so the whole
/// enumeration shares the caller's snapshot (audit B8). The in-memory
/// session marker is deliberately not consulted: it is a cache that can lag
/// a concurrent commit (the mark lands after the transaction), so deriving
/// the collection list from it could silently omit persisted entries.
pub(crate) fn ttl_specs_in(
    reader: &dyn crate::store::SnapshotReader,
) -> Result<Vec<(String, Vec<u8>, i64)>> {
    let mut out = Vec::new();
    for name in reader.collections()? {
        let Some(coll) = name.strip_prefix("__ttl__") else {
            continue;
        };
        // Forward entries: [0x00] ‖ doc_key -> enc_ts.
        for (key, value) in reader.scan_prefix(&name, &[TAG_FWD])? {
            if key.len() > 1 && value.len() == 8 {
                out.push((coll.to_owned(), key[1..].to_vec(), dec_ts(&value)));
            }
        }
    }
    Ok(out)
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
    ///
    /// Rejects engine-reserved collection names, like every write path.
    pub fn insert_with_ttl(&self, key: &[u8], doc: &crate::Value, expires_at: i64) -> Result<()> {
        self.ensure_writable()?;
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

    /// Audit B4: an expired record's purge cascades its graph edges exactly
    /// like a plain delete (the purge path performs its own in-transaction
    /// maintenance instead of going through `write_document`).
    #[test]
    fn purge_cascades_edges() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"doomed", &rec(1), 100).unwrap();
        c.insert(b"stays", &rec(2)).unwrap();
        c.link(b"doomed", "knows", b"stays").unwrap();
        c.link(b"stays", "knows", b"doomed").unwrap();

        assert_eq!(c.purge_expired(100).unwrap(), 1);
        assert_eq!(c.get(b"doomed").unwrap(), None);
        // Both directions of both edges are gone; the survivor keeps its doc.
        assert!(c.neighbors(b"doomed", "knows").unwrap().is_empty());
        assert!(c.neighbors(b"stays", "knows").unwrap().is_empty());
        assert!(c.in_neighbors(b"stays", "knows").unwrap().is_empty());
        assert_eq!(c.get(b"stays").unwrap(), Some(rec(2)));
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

    /// The marker race, made deterministic: writer A's `insert_with_ttl`
    /// commits its `__ttl__` entries but has not yet marked the collection
    /// in memory when writer B's plain insert runs. B's write transaction
    /// must still see the committed `__ttl__` namespace and clear the stale
    /// expiry — the fresh immortal document must not carry A's old expiry
    /// (and must not be deleted by a later purge at that timestamp).
    #[test]
    fn plain_insert_clears_stale_expiry_committed_concurrently() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"k", &rec(1), 100).unwrap();
        // Forge the raced state directly: committed TTL entries, marker absent.
        db.ttl().lock().unwrap().collections.remove("docs");
        // Writer B: plain insert at the same key.
        c.insert(b"k", &rec(2)).unwrap();
        // The stale expiry was cleared inside B's transaction.
        assert_eq!(c.ttl(b"k").unwrap(), None);
        // A purge at the old timestamp finds nothing due and keeps the doc.
        assert_eq!(c.purge_expired(1000).unwrap(), 0);
        assert_eq!(c.get(b"k").unwrap(), Some(rec(2)));
    }

    /// `ttl_specs_in` enumerates the persisted `__ttl__*` namespaces from the
    /// reader's catalog — NOT the in-memory session marker. With the marker
    /// emptied (the lagged/raced state: entries committed, marker not yet
    /// updated), the dump enumeration must still see every persisted expiry.
    #[test]
    fn ttl_specs_enumerate_persisted_namespaces_not_the_marker() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"a", &rec(1), 100).unwrap();
        c.insert_with_ttl(b"b", &rec(2), 200).unwrap();
        c.insert(b"plain", &rec(3)).unwrap(); // no TTL on this one
        // Forge the lagged marker: committed TTL entries, empty marker.
        db.ttl().lock().unwrap().collections.clear();
        let mut specs = db.store().read(|r| ttl_specs_in(r)).unwrap();
        specs.sort();
        assert_eq!(
            specs,
            vec![
                ("docs".to_owned(), b"a".to_vec(), 100),
                ("docs".to_owned(), b"b".to_vec(), 200),
            ]
        );
    }

    /// A purge must never delete a record whose expiry changed after the
    /// scan — a plain overwrite clears the expiry, and the rewritten record
    /// (new value, no TTL) has to survive the purge that was in flight.
    #[test]
    fn purge_delete_survives_interleaved_rewrite() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert_with_ttl(b"doomed", &rec(1), 100).unwrap();
        c.insert_with_ttl(b"stays", &rec(2), 100).unwrap();

        // Collect the due set, exactly as a purge does...
        let due = db.due_keys("docs", 200).unwrap();
        assert_eq!(due.len(), 2);

        // ...then interleave a plain rewrite at one collected key: the fresh
        // document is immortal (the rewrite clears the expiry in its own
        // transaction).
        c.insert(b"doomed", &rec(9)).unwrap();

        // Delete phase: the rewritten record survives; the still-due one goes.
        let mut purged = 0;
        for (key, ts) in &due {
            if db.purge_due_key("docs", key, *ts).unwrap() {
                purged += 1;
            }
        }
        assert_eq!(purged, 1);
        assert_eq!(c.get(b"doomed").unwrap(), Some(rec(9))); // fresh doc lives
        assert_eq!(c.ttl(b"doomed").unwrap(), None);
        assert_eq!(c.get(b"stays").unwrap(), None); // still-due key purged
        assert_eq!(c.ttl(b"stays").unwrap(), None);
        // A later full purge finds nothing left.
        assert_eq!(c.purge_expired(1000).unwrap(), 0);
    }

    /// Wave-4 final review: `insert_with_ttl` must reject unwritable
    /// collection names like every other write path (`insert` etc. all run
    /// `ensure_writable`). Pre-fix it wrote straight through `write_document`:
    /// a reserved name like `__edges__docs` is the edge namespace of a real
    /// collection, and an interior `__` could forge engine namespaces
    /// (audit C7).
    #[test]
    fn insert_with_ttl_rejects_reserved_and_invalid_names() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .collection("__edges__docs")
            .insert_with_ttl(b"k", &rec(1), 100)
            .unwrap_err();
        assert!(matches!(err, crate::Error::ReservedCollection(_)));
        let err = db
            .collection("a__b")
            .insert_with_ttl(b"k", &rec(1), 100)
            .unwrap_err();
        assert!(matches!(err, crate::Error::InvalidName(_)));
    }
}
