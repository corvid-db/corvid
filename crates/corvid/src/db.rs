//! The document database surface (bridges L1 storage and L2 values).
//!
//! [`Db`] wraps the byte [`Store`](crate::Store) and speaks in typed
//! [`Value`]s: a document is any `Value` (typically a [`Value::Map`]) encoded
//! with the deterministic codec. Access goes through a [`Collection`] handle —
//! `db.collection("docs").insert(...)` — which is the shape the fluent query
//! builder will extend with vector / text / filter operations.

use std::sync::Mutex;

use crate::error::Result;
use crate::index::IndexState;
use crate::reactive::{ChangeEvent, ChangeKind, Subscribers};
use crate::store::Store;
use crate::value::Value;

/// An embedded document database.
///
/// Holds the persistent byte store plus an in-memory cache of derived ANN
/// indexes. Documents are the source of truth; an index is rebuilt from them
/// whenever a write to its collection invalidates it, so queries never observe
/// a stale index.
pub struct Db {
    store: Store,
    indexes: Mutex<IndexState>,
    fts: Mutex<crate::fts::FtsState>,
    subscribers: Mutex<Subscribers>,
}

impl Db {
    /// Open (creating if absent) a database backed by a file at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Self {
            store: Store::open(path)?,
            indexes: Mutex::new(IndexState::default()),
            fts: crate::fts::new_state(),
            subscribers: Mutex::new(Subscribers::default()),
        };
        db.load_index_defs()?;
        db.load_text_defs()?;
        Ok(db)
    }

    /// Open a purely in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            store: Store::open_in_memory()?,
            indexes: Mutex::new(IndexState::default()),
            fts: crate::fts::new_state(),
            subscribers: Mutex::new(Subscribers::default()),
        };
        db.load_index_defs()?;
        db.load_text_defs()?;
        Ok(db)
    }

    /// Get a handle to a named collection. The collection is created lazily on
    /// first write.
    pub fn collection<'a>(&'a self, name: &'a str) -> Collection<'a> {
        Collection { db: self, name }
    }

    /// List user collection names (engine-internal `__`-prefixed namespaces
    /// such as graph edges and index metadata are excluded), in name order.
    pub fn collections(&self) -> Result<Vec<String>> {
        Ok(self
            .store
            .collections()?
            .into_iter()
            .filter(|n| !n.starts_with("__"))
            .collect())
    }

    /// Access the underlying byte store (e.g. for multi-collection
    /// transactions). Intended for engine-internal and advanced use.
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// The in-memory derived-index cache.
    pub(crate) fn indexes(&self) -> &Mutex<IndexState> {
        &self.indexes
    }

    /// The full-text index state.
    pub(crate) fn fts(&self) -> &Mutex<crate::fts::FtsState> {
        &self.fts
    }

    /// The change-feed subscriber registry.
    pub(crate) fn subscribers(&self) -> &Mutex<Subscribers> {
        &self.subscribers
    }
}

/// A handle to one collection of documents.
///
/// Cheap to copy — it is just a borrow of the database and the collection
/// name — so query builders can hold one by value.
#[derive(Clone, Copy)]
pub struct Collection<'a> {
    db: &'a Db,
    name: &'a str,
}

impl Collection<'_> {
    /// The owning database. Engine-internal.
    pub(crate) fn db(&self) -> &Db {
        self.db
    }

    /// This collection's name. Engine-internal.
    pub(crate) fn name(&self) -> &str {
        self.name
    }

    /// Reject writes to engine-reserved collection names (the `__` prefix is
    /// used for internal namespaces such as graph edges).
    pub(crate) fn ensure_writable(&self) -> Result<()> {
        if self.name.starts_with("__") {
            return Err(crate::Error::ReservedCollection(self.name.to_owned()));
        }
        Ok(())
    }

    /// Insert or overwrite the document stored at `key`.
    pub fn insert(&self, key: &[u8], doc: &Value) -> Result<()> {
        self.ensure_writable()?;
        self.db.store().put(self.name, key, &doc.encode())?;
        self.db.index_on_insert(self.name, key, doc);
        self.db.fts_on_insert(self.name, key, doc);
        self.db.notify(ChangeEvent {
            collection: self.name.to_owned(),
            key: key.to_vec(),
            kind: ChangeKind::Insert,
        });
        Ok(())
    }

    /// Insert `doc` under a freshly generated, monotonically increasing key
    /// (big-endian, so keys sort in insertion order). Returns the new key.
    pub fn insert_auto(&self, doc: &Value) -> Result<Vec<u8>> {
        self.ensure_writable()?;
        let id = self.db.store().next_auto_id(self.name)?;
        // Zero-padded decimal: UTF-8 (round-trips through text APIs like MCP)
        // and lexicographically ordered by id.
        let key = format!("{id:020}").into_bytes();
        self.insert(&key, doc)?;
        Ok(key)
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
        let removed = self.db.store().delete(self.name, key)?;
        if removed {
            self.db.index_on_delete(self.name, key);
            self.db.fts_on_delete(self.name, key);
            self.db.notify(ChangeEvent {
                collection: self.name.to_owned(),
                key: key.to_vec(),
                kind: ChangeKind::Delete,
            });
        }
        Ok(removed)
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
    fn collections_lists_user_collections_only() {
        let db = Db::open_in_memory().unwrap();
        db.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
        db.collection("notes").insert(b"k", &Value::Int(1)).unwrap();
        // A graph edge creates a reserved __edges__ collection that must be hidden.
        db.collection("docs").link(b"a", "r", b"b").unwrap();
        let mut names = db.collections().unwrap();
        names.sort();
        assert_eq!(names, vec!["docs".to_string(), "notes".to_string()]);
    }

    #[test]
    fn insert_auto_generates_ordered_keys() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("events");
        let k0 = c.insert_auto(&Value::Int(10)).unwrap();
        let k1 = c.insert_auto(&Value::Int(20)).unwrap();
        assert_ne!(k0, k1);
        // Keys sort in insertion order; scan reflects it.
        let scanned = c.scan().unwrap();
        assert_eq!(scanned[0].0, k0);
        assert_eq!(scanned[1].0, k1);
        assert_eq!(c.get(&k0).unwrap(), Some(Value::Int(10)));
    }

    #[test]
    fn incompatible_format_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            db.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
        }
        // Corrupt the stored format version directly via the store.
        {
            let store = crate::store::Store::open(&path).unwrap();
            store.set_format_version_for_test(999).unwrap();
        }
        let err = Db::open(&path);
        assert!(matches!(err, Err(crate::Error::IncompatibleFormat { .. })));
    }

    #[test]
    fn reserved_collection_names_are_rejected_on_write() {
        let db = Db::open_in_memory().unwrap();
        let err = db.collection("__edges__docs").insert(b"k", &Value::Int(1));
        assert!(matches!(err, Err(crate::Error::ReservedCollection(_))));
        // A normal collection is fine.
        assert!(db.collection("docs").insert(b"k", &Value::Int(1)).is_ok());
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
