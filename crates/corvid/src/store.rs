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
//! atomically or not at all. This is the foundation the cross-modal
//! consistency invariant is built on: a row and all of its derived index
//! entries are written in a single transaction. Reads go through
//! [`Store::read`], which exposes a [`ReadBatch`] over one consistent
//! snapshot. The single-op helpers ([`Store::put`] etc.) are thin wrappers
//! over these.

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

/// An embedded transactional key/value store.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open (creating if absent) a store backed by a file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            db: Database::create(path)?,
        })
    }

    /// Open a purely in-memory store. Nothing is persisted.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            db: Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?,
        })
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
        let mut records = self.txn.open_table(RECORDS)?;
        records.insert(physical_key(id, key).as_slice(), value)?;
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
        let mut records = self.txn.open_table(RECORDS)?;
        Ok(records.remove(physical_key(id, key).as_slice())?.is_some())
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

/// Compose a physical record key: `collection_id (BE) ++ user_key`.
fn physical_key(id: u64, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + key.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(key);
    out
}

/// Strip the 8-byte collection id prefix from a physical key.
fn user_key(physical: &[u8]) -> Vec<u8> {
    physical[8..].to_vec()
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
