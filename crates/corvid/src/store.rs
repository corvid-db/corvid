//! Transactional byte-oriented key/value store over redb (engine layer L1).
//!
//! A [`Store`] holds many named *collections*. Rather than mapping each
//! collection to a redb table (redb table names must be `&'static str`,
//! which rules out user-defined names), every collection is assigned a
//! `u64` id and all records live in one physical table keyed by
//! `id.to_be_bytes() ++ user_key`. Big-endian ids keep collections
//! contiguous and ordered, so a collection scan is a single prefix range.
//!
//! Writes go through [`Store::transaction`], which exposes a [`WriteBatch`]
//! and commits once at the end — every operation in the closure lands
//! atomically or not at all. This underpins atomic multi-key writes such as a
//! graph edge and its reverse. Reads go through [`Store::read`], which exposes
//! a [`ReadBatch`] over one consistent snapshot. The single-op helpers
//! ([`Store::put`] etc.) are thin wrappers over these.
//!
//! Derived indexes (vector, full-text) are *not* written inside the document's
//! transaction. They are maintained incrementally in memory and rebuilt from
//! the documents on open, so a query always sees an index consistent with the
//! committed documents (documents are the source of truth); a crash can only
//! lose in-memory index state, which is reconstructed on the next open.

use std::ops::Bound;
use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::error::Result;

/// All record bytes, keyed by `collection_id (8 bytes BE) ++ user_key`.
const RECORDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("records");
/// Maps a collection name to its `u64` id.
const CATALOG: TableDefinition<&str, u64> = TableDefinition::new("catalog");
/// Engine metadata. Currently holds the next collection id under [`NEXT_ID`].
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Key in [`META`] holding the next collection id to assign.
const NEXT_ID: &str = "next_collection_id";
/// Key in [`META`] holding the on-disk format version.
const FORMAT_VERSION_KEY: &str = "format_version";
/// The format version this engine writes. Bump on any breaking on-disk change.
const FORMAT_VERSION: u64 = 1;

/// An embedded transactional key/value store.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open (creating if absent) a store backed by a file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = Self {
            db: Database::create(path)?,
        };
        store.check_format_version()?;
        Ok(store)
    }

    /// Open a purely in-memory store. Nothing is persisted.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            db: Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?,
        };
        store.check_format_version()?;
        Ok(store)
    }

    /// Verify (or stamp) the on-disk format version. A file from an
    /// incompatible version is refused rather than silently mis-decoded.
    fn check_format_version(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        let found = {
            let meta = txn.open_table(META)?;
            meta.get(FORMAT_VERSION_KEY)?.map(|g| g.value())
        };
        match found {
            Some(v) if v != FORMAT_VERSION => {
                txn.abort()?;
                Err(crate::Error::IncompatibleFormat {
                    found: v,
                    expected: FORMAT_VERSION,
                })
            }
            Some(_) => {
                txn.abort()?;
                Ok(())
            }
            None => {
                {
                    let mut meta = txn.open_table(META)?;
                    meta.insert(FORMAT_VERSION_KEY, FORMAT_VERSION)?;
                }
                txn.commit()?;
                Ok(())
            }
        }
    }

    /// Overwrite the stored format version (test-only, to simulate a file
    /// written by a different engine version).
    #[cfg(test)]
    pub(crate) fn set_format_version_for_test(&self, version: u64) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert(FORMAT_VERSION_KEY, version)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Atomically reserve the next monotonic auto-key id for `collection`.
    /// Big-endian encoding of the returned id keeps auto-keyed records in
    /// insertion order.
    pub fn next_auto_id(&self, collection: &str) -> Result<u64> {
        let txn = self.db.begin_write()?;
        let id = {
            let mut meta = txn.open_table(META)?;
            let key = format!("auto:{collection}");
            let id = meta.get(key.as_str())?.map(|g| g.value()).unwrap_or(0);
            meta.insert(key.as_str(), id + 1)?;
            id
        };
        txn.commit()?;
        Ok(id)
    }

    /// Run `f` inside a single write transaction and commit once.
    ///
    /// Every operation performed on the [`WriteBatch`] commits together. If
    /// `f` returns an error the transaction is dropped without committing, so
    /// no partial state is left behind.
    pub fn transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut WriteBatch<'_>) -> Result<T>,
    {
        let txn = self.db.begin_write()?;
        let out = {
            let mut batch = WriteBatch { txn: &txn };
            f(&mut batch)?
        };
        txn.commit()?;
        Ok(out)
    }

    /// Run `f` against one consistent read snapshot.
    pub fn read<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&ReadBatch<'_>) -> Result<T>,
    {
        let txn = self.db.begin_read()?;
        let batch = ReadBatch { txn: &txn };
        f(&batch)
    }

    /// Insert or overwrite `value` at `key` within `collection`.
    pub fn put(&self, collection: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.transaction(|tx| tx.put(collection, key, value))
    }

    /// Fetch the value at `key` within `collection`, if present.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read(|r| r.get(collection, key))
    }

    /// Remove `key` from `collection`. Returns whether a value was removed.
    pub fn delete(&self, collection: &str, key: &[u8]) -> Result<bool> {
        self.transaction(|tx| tx.delete(collection, key))
    }

    /// Return all `(key, value)` pairs in `collection`, in key order.
    pub fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.read(|r| r.scan(collection))
    }

    /// List every collection name known to the catalog, in name order.
    pub fn collections(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let catalog = match txn.open_table(CATALOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in catalog.iter()? {
            let (name, _) = entry?;
            out.push(name.value().to_owned());
        }
        Ok(out)
    }

    /// The maintained record count for `collection` (O(1), no scan).
    pub fn count(&self, collection: &str) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let meta = match txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        Ok(meta
            .get(count_key(collection).as_str())?
            .map(|g| g.value())
            .unwrap_or(0))
    }

    /// Stream every `(user_key, value)` in `collection` to `f`, in key order,
    /// without materializing the collection. Constant memory regardless of
    /// collection size. `f`'s slices are valid only for the duration of the call.
    pub fn for_each<F>(&self, collection: &str, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<()>,
    {
        let txn = self.db.begin_read()?;
        let catalog = match txn.open_table(CATALOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let Some(id) = catalog.get(collection)?.map(|g| g.value()) else {
            return Ok(());
        };
        drop(catalog);

        let records = txn.open_table(RECORDS)?;
        let lower = id.to_be_bytes().to_vec();
        let upper = id.checked_add(1).map(|n| n.to_be_bytes().to_vec());
        let lower_slice: &[u8] = &lower;
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (Bound::Included(lower_slice), Bound::Excluded(u.as_slice())),
            None => (Bound::Included(lower_slice), Bound::Unbounded),
        };
        for entry in records.range::<&[u8]>(bounds)? {
            let (k, v) = entry?;
            let key = k.value();
            f(key.get(8..).unwrap_or(&[]), v.value())?;
        }
        Ok(())
    }

    /// Return all `(key, value)` pairs in `collection` whose key starts with
    /// `prefix`, in key order. An unknown collection yields an empty vector.
    pub fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let catalog = match txn.open_table(CATALOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let Some(id) = catalog.get(collection)?.map(|g| g.value()) else {
            return Ok(Vec::new());
        };
        drop(catalog);

        let records = txn.open_table(RECORDS)?;
        let mut lower = id.to_be_bytes().to_vec();
        lower.extend_from_slice(prefix);
        let upper = next_key(&lower);
        let lower_slice: &[u8] = &lower;
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (Bound::Included(lower_slice), Bound::Excluded(u.as_slice())),
            None => (Bound::Included(lower_slice), Bound::Unbounded),
        };

        let mut out = Vec::new();
        for entry in records.range::<&[u8]>(bounds)? {
            let (k, v) = entry?;
            out.push((user_key(k.value()), v.value().to_vec()));
        }
        Ok(out)
    }
}

/// A set of writes (and reads) executed inside one transaction.
///
/// Obtained via [`Store::transaction`]. Reads see this transaction's own
/// uncommitted writes.
pub struct WriteBatch<'txn> {
    txn: &'txn redb::WriteTransaction,
}

impl WriteBatch<'_> {
    /// Insert or overwrite `value` at `key` within `collection`, creating the
    /// collection on first use.
    pub fn put(&mut self, collection: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let id = self.ensure_id(collection)?;
        let existed = {
            let mut records = self.txn.open_table(RECORDS)?;
            records
                .insert(physical_key(id, key).as_slice(), value)?
                .is_some()
        };
        if !existed {
            self.adjust_count(collection, 1)?;
        }
        Ok(())
    }

    /// Fetch the value at `key` within `collection`, including writes made
    /// earlier in this transaction.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(None);
        };
        let records = self.txn.open_table(RECORDS)?;
        Ok(records
            .get(physical_key(id, key).as_slice())?
            .map(|g| g.value().to_vec()))
    }

    /// Remove `key` from `collection`. Returns whether a value was removed.
    pub fn delete(&mut self, collection: &str, key: &[u8]) -> Result<bool> {
        // Resolve without creating: deleting from an unknown collection is a no-op.
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(false);
        };
        let removed = {
            let mut records = self.txn.open_table(RECORDS)?;
            records.remove(physical_key(id, key).as_slice())?.is_some()
        };
        if removed {
            self.adjust_count(collection, -1)?;
        }
        Ok(removed)
    }

    /// Adjust the maintained record count for `collection` by `delta`.
    fn adjust_count(&self, collection: &str, delta: i64) -> Result<()> {
        let mut meta = self.txn.open_table(META)?;
        let key = count_key(collection);
        let current = meta.get(key.as_str())?.map(|g| g.value()).unwrap_or(0);
        let updated = current.saturating_add_signed(delta);
        meta.insert(key.as_str(), updated)?;
        Ok(())
    }

    /// Return all `(key, value)` pairs in `collection`, in key order.
    pub fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        collect_collection(&records, id)
    }

    /// Resolve a collection id, assigning a fresh one if the collection is new.
    fn ensure_id(&self, collection: &str) -> Result<u64> {
        let mut catalog = self.txn.open_table(CATALOG)?;
        if let Some(id) = catalog.get(collection)?.map(|g| g.value()) {
            return Ok(id);
        }
        let next = {
            let mut meta = self.txn.open_table(META)?;
            let next = meta.get(NEXT_ID)?.map(|g| g.value()).unwrap_or(0);
            meta.insert(NEXT_ID, next + 1)?;
            next
        };
        catalog.insert(collection, next)?;
        Ok(next)
    }

    /// Resolve a collection id without creating it. In a write transaction
    /// opening the catalog table always succeeds (it is created on demand),
    /// so an unknown collection simply has no entry.
    fn lookup_id(&self, collection: &str) -> Result<Option<u64>> {
        let catalog = self.txn.open_table(CATALOG)?;
        Ok(catalog.get(collection)?.map(|g| g.value()))
    }
}

/// A set of reads executed against one consistent snapshot.
///
/// Obtained via [`Store::read`].
pub struct ReadBatch<'txn> {
    txn: &'txn redb::ReadTransaction,
}

impl ReadBatch<'_> {
    /// Fetch the value at `key` within `collection`, if present.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(None);
        };
        let records = self.txn.open_table(RECORDS)?;
        Ok(records
            .get(physical_key(id, key).as_slice())?
            .map(|g| g.value().to_vec()))
    }

    /// Return all `(key, value)` pairs in `collection`, in key order.
    pub fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        collect_collection(&records, id)
    }

    /// Resolve a collection id without creating the collection. A read
    /// transaction never creates tables, so a missing catalog table means
    /// nothing has ever been written.
    fn lookup_id(&self, collection: &str) -> Result<Option<u64>> {
        let catalog = match self.txn.open_table(CATALOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(catalog.get(collection)?.map(|g| g.value()))
    }
}

/// Collect every record belonging to collection `id` from an open records
/// table, stripping the id prefix back to user keys.
fn collect_collection<T>(records: &T, id: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    let lower = id.to_be_bytes().to_vec();
    let upper = id.checked_add(1).map(|n| n.to_be_bytes().to_vec());
    let lower_slice: &[u8] = &lower;
    let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
        Some(u) => (Bound::Included(lower_slice), Bound::Excluded(u.as_slice())),
        None => (Bound::Included(lower_slice), Bound::Unbounded),
    };

    let mut out = Vec::new();
    for entry in records.range::<&[u8]>(bounds)? {
        let (k, v) = entry?;
        out.push((user_key(k.value()), v.value().to_vec()));
    }
    Ok(out)
}

/// The [`META`] key holding `collection`'s maintained record count.
fn count_key(collection: &str) -> String {
    format!("cnt:{collection}")
}

/// Compose a physical record key: `collection_id (BE) ++ user_key`.
fn physical_key(id: u64, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + key.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(key);
    out
}

/// Strip the 8-byte collection id prefix from a physical key. Defensive
/// against malformed (too-short) keys: returns empty rather than panicking.
fn user_key(physical: &[u8]) -> Vec<u8> {
    physical.get(8..).unwrap_or(&[]).to_vec()
}

/// The smallest byte string strictly greater than every string starting with
/// `bytes` — i.e. the exclusive upper bound for a prefix range. `None` when
/// `bytes` is empty or all `0xFF` (range is unbounded above).
fn next_key(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = bytes.to_vec();
    while let Some(last) = out.last_mut() {
        if *last < 0xFF {
            *last += 1;
            return Some(out);
        }
        out.pop();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn get_missing_key_in_missing_collection_is_none() {
        let s = mem();
        assert_eq!(s.get("nope", b"k").unwrap(), None);
    }

    #[test]
    fn put_then_get_roundtrips() {
        let s = mem();
        s.put("docs", b"a", b"alpha").unwrap();
        assert_eq!(s.get("docs", b"a").unwrap(), Some(b"alpha".to_vec()));
    }

    #[test]
    fn put_overwrites_existing_value() {
        let s = mem();
        s.put("docs", b"a", b"first").unwrap();
        s.put("docs", b"a", b"second").unwrap();
        assert_eq!(s.get("docs", b"a").unwrap(), Some(b"second".to_vec()));
    }

    #[test]
    fn get_missing_key_in_existing_collection_is_none() {
        let s = mem();
        s.put("docs", b"a", b"alpha").unwrap();
        assert_eq!(s.get("docs", b"absent").unwrap(), None);
    }

    #[test]
    fn collections_are_isolated() {
        let s = mem();
        s.put("a", b"k", b"in-a").unwrap();
        s.put("b", b"k", b"in-b").unwrap();
        assert_eq!(s.get("a", b"k").unwrap(), Some(b"in-a".to_vec()));
        assert_eq!(s.get("b", b"k").unwrap(), Some(b"in-b".to_vec()));
    }

    #[test]
    fn delete_existing_returns_true_and_removes() {
        let s = mem();
        s.put("docs", b"a", b"alpha").unwrap();
        assert!(s.delete("docs", b"a").unwrap());
        assert_eq!(s.get("docs", b"a").unwrap(), None);
    }

    #[test]
    fn delete_missing_key_returns_false() {
        let s = mem();
        s.put("docs", b"a", b"alpha").unwrap();
        assert!(!s.delete("docs", b"absent").unwrap());
    }

    #[test]
    fn delete_in_missing_collection_returns_false() {
        let s = mem();
        assert!(!s.delete("ghost", b"a").unwrap());
    }

    #[test]
    fn scan_missing_collection_is_empty() {
        let s = mem();
        assert!(s.scan("ghost").unwrap().is_empty());
    }

    #[test]
    fn scan_returns_pairs_in_key_order() {
        let s = mem();
        s.put("docs", b"c", b"3").unwrap();
        s.put("docs", b"a", b"1").unwrap();
        s.put("docs", b"b", b"2").unwrap();
        let got = s.scan("docs").unwrap();
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
    }

    #[test]
    fn scan_does_not_bleed_across_collections() {
        let s = mem();
        s.put("a", b"x", b"1").unwrap();
        s.put("b", b"y", b"2").unwrap();
        assert_eq!(s.scan("a").unwrap(), vec![(b"x".to_vec(), b"1".to_vec())]);
        assert_eq!(s.scan("b").unwrap(), vec![(b"y".to_vec(), b"2".to_vec())]);
    }

    #[test]
    fn empty_key_and_empty_value_are_valid() {
        let s = mem();
        s.put("docs", b"", b"").unwrap();
        assert_eq!(s.get("docs", b"").unwrap(), Some(Vec::new()));
        assert_eq!(s.scan("docs").unwrap(), vec![(Vec::new(), Vec::new())]);
    }

    #[test]
    fn data_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let s = Store::open(&path).unwrap();
            s.put("docs", b"a", b"alpha").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.get("docs", b"a").unwrap(), Some(b"alpha".to_vec()));
    }

    #[test]
    fn physical_key_layout_is_prefix_plus_key() {
        assert_eq!(
            physical_key(1, b"xy"),
            vec![0, 0, 0, 0, 0, 0, 0, 1, b'x', b'y']
        );
        assert_eq!(user_key(&physical_key(7, b"abc")), b"abc".to_vec());
    }

    #[test]
    fn transaction_commits_all_writes_atomically() {
        let s = mem();
        s.transaction(|tx| {
            tx.put("docs", b"a", b"1")?;
            tx.put("docs", b"b", b"2")?;
            tx.put("notes", b"x", b"y")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(s.get("docs", b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(s.get("docs", b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(s.get("notes", b"x").unwrap(), Some(b"y".to_vec()));
    }

    #[test]
    fn transaction_error_rolls_back_every_write() {
        let s = mem();
        let result: Result<()> = s.transaction(|tx| {
            tx.put("docs", b"a", b"1")?;
            tx.put("docs", b"b", b"2")?;
            Err(crate::Error::Storage(redb::StorageError::Corrupted(
                "intentional".into(),
            )))
        });
        assert!(result.is_err());
        // Nothing from the aborted transaction is visible, and the collection
        // was never created.
        assert_eq!(s.get("docs", b"a").unwrap(), None);
        assert!(s.scan("docs").unwrap().is_empty());
    }

    #[test]
    fn transaction_sees_its_own_writes() {
        let s = mem();
        let seen = s
            .transaction(|tx| {
                tx.put("docs", b"a", b"1")?;
                let mid = tx.get("docs", b"a")?;
                tx.delete("docs", b"a")?;
                let after = tx.get("docs", b"a")?;
                Ok((mid, after))
            })
            .unwrap();
        assert_eq!(seen, (Some(b"1".to_vec()), None));
    }

    #[test]
    fn write_batch_scan_includes_uncommitted_writes() {
        let s = mem();
        let in_txn = s
            .transaction(|tx| {
                tx.put("docs", b"a", b"1")?;
                tx.put("docs", b"b", b"2")?;
                tx.scan("docs")
            })
            .unwrap();
        assert_eq!(
            in_txn,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
    }

    #[test]
    fn write_batch_delete_on_missing_collection_is_false() {
        let s = mem();
        let removed = s.transaction(|tx| tx.delete("ghost", b"a")).unwrap();
        assert!(!removed);
    }

    #[test]
    fn read_snapshot_sees_consistent_view_across_ops() {
        let s = mem();
        s.put("docs", b"a", b"1").unwrap();
        s.put("docs", b"b", b"2").unwrap();
        let (a, all) = s
            .read(|r| Ok((r.get("docs", b"a")?, r.scan("docs")?)))
            .unwrap();
        assert_eq!(a, Some(b"1".to_vec()));
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn scan_prefix_returns_matching_keys_in_order() {
        let s = mem();
        s.put("c", b"ax", b"1").unwrap();
        s.put("c", b"ay", b"2").unwrap();
        s.put("c", b"bz", b"3").unwrap();
        assert_eq!(
            s.scan_prefix("c", b"a").unwrap(),
            vec![
                (b"ax".to_vec(), b"1".to_vec()),
                (b"ay".to_vec(), b"2".to_vec())
            ]
        );
        assert_eq!(
            s.scan_prefix("c", b"b").unwrap(),
            vec![(b"bz".to_vec(), b"3".to_vec())]
        );
    }

    #[test]
    fn scan_prefix_empty_prefix_returns_all() {
        let s = mem();
        s.put("c", b"x", b"1").unwrap();
        s.put("c", b"y", b"2").unwrap();
        assert_eq!(s.scan_prefix("c", b"").unwrap().len(), 2);
    }

    #[test]
    fn scan_prefix_no_match_and_missing_collection() {
        let s = mem();
        s.put("c", b"x", b"1").unwrap();
        assert!(s.scan_prefix("c", b"z").unwrap().is_empty());
        assert!(s.scan_prefix("ghost", b"a").unwrap().is_empty());
    }

    #[test]
    fn scan_prefix_handles_all_0xff_prefix() {
        let s = mem();
        s.put("c", &[0xff, 0xff], b"hi").unwrap();
        s.put("c", b"a", b"lo").unwrap();
        let got = s.scan_prefix("c", &[0xff, 0xff]).unwrap();
        assert_eq!(got, vec![(vec![0xff, 0xff], b"hi".to_vec())]);
    }

    #[test]
    fn next_key_increments_and_overflows() {
        assert_eq!(next_key(b"a"), Some(b"b".to_vec()));
        assert_eq!(next_key(&[1, 0xff]), Some(vec![2]));
        assert_eq!(next_key(&[0xff, 0xff]), None);
        assert_eq!(next_key(b""), None);
    }

    #[test]
    fn read_batch_missing_collection_paths() {
        let s = mem();
        // Read transaction against a store with no tables at all.
        let (g, sc) = s
            .read(|r| Ok((r.get("ghost", b"k")?, r.scan("ghost")?)))
            .unwrap();
        assert_eq!(g, None);
        assert!(sc.is_empty());
    }
}
