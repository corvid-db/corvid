//! Transactional byte-oriented key/value store over redb (engine layer L1).
//!
//! A [`Store`] holds many named *collections*. Rather than mapping each
//! collection to a redb table (redb table names must be `&'static str`,
//! which rules out user-defined names), every collection is assigned a
//! `u64` id and all records live in one physical table keyed by
//! `id.to_be_bytes() ++ user_key`. Big-endian ids keep collections
//! contiguous and ordered, so a collection scan is a single prefix range.

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

    /// Insert or overwrite `value` at `key` within `collection`.
    ///
    /// The collection is created on first write. The catalog update and the
    /// record write share one transaction, so a failure leaves neither behind.
    pub fn put(&self, collection: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        let id = {
            let mut catalog = txn.open_table(CATALOG)?;
            let existing = catalog.get(collection)?.map(|g| g.value());
            match existing {
                Some(id) => id,
                None => {
                    let next = {
                        let mut meta = txn.open_table(META)?;
                        let next = meta.get(NEXT_ID)?.map(|g| g.value()).unwrap_or(0);
                        meta.insert(NEXT_ID, next + 1)?;
                        next
                    };
                    catalog.insert(collection, next)?;
                    next
                }
            }
        };
        {
            let mut records = txn.open_table(RECORDS)?;
            records.insert(physical_key(id, key).as_slice(), value)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch the value at `key` within `collection`, if present.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let Some(id) = self.lookup_id(&txn, collection)? else {
            return Ok(None);
        };
        let records = txn.open_table(RECORDS)?;
        Ok(records
            .get(physical_key(id, key).as_slice())?
            .map(|g| g.value().to_vec()))
    }

    /// Remove `key` from `collection`. Returns whether a value was removed.
    pub fn delete(&self, collection: &str, key: &[u8]) -> Result<bool> {
        let txn = self.db.begin_write()?;
        // Resolve without creating: deleting from an unknown collection is a no-op.
        let id = {
            let catalog = txn.open_table(CATALOG)?;
            catalog.get(collection)?.map(|g| g.value())
        };
        let removed = match id {
            None => false,
            Some(id) => {
                let mut records = txn.open_table(RECORDS)?;
                records.remove(physical_key(id, key).as_slice())?.is_some()
            }
        };
        txn.commit()?;
        Ok(removed)
    }

    /// Return all `(key, value)` pairs in `collection`, in key order.
    ///
    /// An unknown collection yields an empty vector.
    pub fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let Some(id) = self.lookup_id(&txn, collection)? else {
            return Ok(Vec::new());
        };
        let records = txn.open_table(RECORDS)?;

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

    /// Resolve a collection id without creating the collection.
    fn lookup_id(&self, txn: &redb::ReadTransaction, collection: &str) -> Result<Option<u64>> {
        let catalog = match txn.open_table(CATALOG) {
            Ok(t) => t,
            // No catalog table yet means no collection has ever been written.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(catalog.get(collection)?.map(|g| g.value()))
    }
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
}
