//! The document database surface (bridges L1 storage and L2 values).
//!
//! [`Db`] wraps the byte [`Store`](crate::Store) and speaks in typed
//! [`Value`]s: a document is any `Value` (typically a [`Value::Map`]) encoded
//! with the deterministic codec. Access goes through a [`Collection`] handle —
//! `db.collection("docs").insert(...)` — which is the shape the fluent query
//! builder will extend with vector / text / filter operations.

use crate::error::Result;
use crate::store::Store;
use crate::value::Value;

/// An embedded document database.
pub struct Db {
    store: Store,
}

impl Db {
    /// Open (creating if absent) a database backed by a file at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(Self {
            store: Store::open(path)?,
        })
    }

    /// Open a purely in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            store: Store::open_in_memory()?,
        })
    }

    /// Get a handle to a named collection. The collection is created lazily on
    /// first write.
    pub fn collection<'a>(&'a self, name: &'a str) -> Collection<'a> {
        Collection { db: self, name }
    }

    /// Access the underlying byte store (e.g. for multi-collection
    /// transactions). Intended for engine-internal and advanced use.
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
}

/// A handle to one collection of documents.
pub struct Collection<'a> {
    db: &'a Db,
    name: &'a str,
}

impl Collection<'_> {
    /// Insert or overwrite the document stored at `key`.
    pub fn insert(&self, key: &[u8], doc: &Value) -> Result<()> {
        self.db.store().put(self.name, key, &doc.encode())
    }

    /// Fetch and decode the document at `key`, if present.
    pub fn get(&self, key: &[u8]) -> Result<Option<Value>> {
        match self.db.store().get(self.name, key)? {
            Some(bytes) => Ok(Some(Value::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Remove the document at `key`. Returns whether one was removed.
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        self.db.store().delete(self.name, key)
    }

    /// Return every `(key, document)` pair, in key order.
    pub fn scan(&self) -> Result<Vec<(Vec<u8>, Value)>> {
        let mut out = Vec::new();
        for (k, bytes) in self.db.store().scan(self.name)? {
            out.push((k, Value::decode(&bytes)?));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(name: &str, n: i64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("name".to_owned(), Value::Text(name.to_owned()));
        m.insert("n".to_owned(), Value::Int(n));
        Value::Map(m)
    }

    #[test]
    fn insert_then_get_roundtrips_a_document() {
        let db = Db::open_in_memory().unwrap();
        let d = doc("corvid", 8);
        db.collection("docs").insert(b"k1", &d).unwrap();
        assert_eq!(db.collection("docs").get(b"k1").unwrap(), Some(d));
    }

    #[test]
    fn get_missing_document_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.collection("docs").get(b"absent").unwrap(), None);
    }

    #[test]
    fn insert_overwrites() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc("first", 1)).unwrap();
        c.insert(b"k", &doc("second", 2)).unwrap();
        assert_eq!(c.get(b"k").unwrap(), Some(doc("second", 2)));
    }

    #[test]
    fn delete_removes_document() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc("x", 1)).unwrap();
        assert!(c.delete(b"k").unwrap());
        assert_eq!(c.get(b"k").unwrap(), None);
        assert!(!c.delete(b"k").unwrap());
    }

    #[test]
    fn scan_returns_decoded_documents_in_key_order() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"b", &doc("two", 2)).unwrap();
        c.insert(b"a", &doc("one", 1)).unwrap();
        let got = c.scan().unwrap();
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), doc("one", 1)),
                (b"b".to_vec(), doc("two", 2)),
            ]
        );
    }

    #[test]
    fn collections_are_independent() {
        let db = Db::open_in_memory().unwrap();
        db.collection("a").insert(b"k", &Value::Int(1)).unwrap();
        db.collection("b").insert(b"k", &Value::Int(2)).unwrap();
        assert_eq!(db.collection("a").get(b"k").unwrap(), Some(Value::Int(1)));
        assert_eq!(db.collection("b").get(b"k").unwrap(), Some(Value::Int(2)));
    }

    #[test]
    fn non_map_values_are_valid_documents() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("scalars");
        c.insert(b"v", &Value::Vector(vec![1.0, 2.0, 3.0])).unwrap();
        assert_eq!(
            c.get(b"v").unwrap(),
            Some(Value::Vector(vec![1.0, 2.0, 3.0]))
        );
    }

    #[test]
    fn documents_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            db.collection("docs")
                .insert(b"k", &doc("persist", 9))
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.collection("docs").get(b"k").unwrap(),
            Some(doc("persist", 9))
        );
    }
}
