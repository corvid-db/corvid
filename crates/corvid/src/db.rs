//! The document database surface (bridges L1 storage and L2 values).
//!
//! [`Db`] wraps the byte [`Store`] and speaks in typed
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
    scalar: Mutex<crate::scalar::ScalarState>,
    geo: Mutex<crate::geo_index::GeoState>,
    schemas: Mutex<crate::schema::SchemaState>,
    ttl: Mutex<crate::ttl::TtlState>,
    subscribers: Mutex<Subscribers>,
}

impl Db {
    /// Open (creating if absent) a database backed by a file at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Self {
            store: Store::open(path)?,
            indexes: Mutex::new(IndexState::default()),
            fts: crate::fts::new_state(),
            scalar: crate::scalar::new_state(),
            geo: crate::geo_index::new_state(),
            schemas: crate::schema::new_state(),
            ttl: crate::ttl::new_state(),
            subscribers: Mutex::new(Subscribers::default()),
        };
        db.load_index_defs()?;
        db.load_text_defs()?;
        db.load_scalar_defs()?;
        db.load_compound_defs()?;
        db.load_geo_defs()?;
        db.load_schemas()?;
        db.load_ttl_collections()?;
        Ok(db)
    }

    /// Open a purely in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            store: Store::open_in_memory()?,
            indexes: Mutex::new(IndexState::default()),
            fts: crate::fts::new_state(),
            scalar: crate::scalar::new_state(),
            geo: crate::geo_index::new_state(),
            schemas: crate::schema::new_state(),
            ttl: crate::ttl::new_state(),
            subscribers: Mutex::new(Subscribers::default()),
        };
        db.load_index_defs()?;
        db.load_text_defs()?;
        db.load_scalar_defs()?;
        db.load_compound_defs()?;
        db.load_geo_defs()?;
        db.load_schemas()?;
        db.load_ttl_collections()?;
        Ok(db)
    }

    /// Get a handle to a named collection. The collection is created lazily on
    /// first write.
    pub fn collection<'a>(&'a self, name: &'a str) -> Collection<'a> {
        Collection { db: self, name }
    }

    /// Write a consistent, point-in-time backup of the entire database
    /// (documents, indexes, graph, all reserved state) to a fresh file at
    /// `path`. Safe to call while writers are active — the copy is taken from
    /// one read snapshot. Reopen it with [`Db::open`].
    pub fn backup(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        self.store.backup(path)
    }

    /// Run `f` as a bulk load under relaxed (eventual) durability: write
    /// transactions inside `f` skip the per-commit fsync, then a single durable
    /// flush at the end makes everything durable. A crash *during* the load can
    /// lose the in-flight writes (the database stays consistent), so use this
    /// only for rebuildable bulk ingestion, not for writes you must not lose.
    ///
    /// Turns an N-document load from ~N fsyncs into ~1.
    pub fn bulk<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        self.store.set_relaxed_durability(true);
        let result = f();
        self.store.set_relaxed_durability(false);
        // Make the bulk writes durable even if `f` failed partway.
        let flush = self.store.flush();
        let out = result?;
        flush?;
        Ok(out)
    }

    /// Reclaim unused file space after heavy deletes by compacting the
    /// database file. Returns whether any data was moved. Needs exclusive
    /// access (`&mut self`) — run it as offline maintenance, not concurrently
    /// with queries. Document data and index definitions are unchanged.
    pub fn compact(&mut self) -> Result<bool> {
        self.store.compact()
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

    /// The scalar secondary-index state.
    pub(crate) fn scalar(&self) -> &Mutex<crate::scalar::ScalarState> {
        &self.scalar
    }

    /// The spatial-index state.
    pub(crate) fn geo(&self) -> &Mutex<crate::geo_index::GeoState> {
        &self.geo
    }

    /// The declared-schema registry.
    pub(crate) fn schemas(&self) -> &Mutex<crate::schema::SchemaState> {
        &self.schemas
    }

    /// The TTL/expiry registry.
    pub(crate) fn ttl(&self) -> &Mutex<crate::ttl::TtlState> {
        &self.ttl
    }

    /// The change-feed subscriber registry.
    pub(crate) fn subscribers(&self) -> &Mutex<Subscribers> {
        &self.subscribers
    }

    /// The shared write path. One transaction covers the row write, the
    /// unique-constraint check (which observes this transaction's own earlier
    /// puts), and every *persisted* index's maintenance — so a crash or error
    /// can never leave an index disagreeing with committed documents.
    /// In-memory index updates and change events run after a successful
    /// commit (they are rebuilt lazily from documents, so they cannot go stale).
    ///
    /// `doc = Some` inserts/overwrites; `None` deletes. `expires_at = Some`
    /// sets (or replaces) the record's expiry in the same commit.
    /// Returns whether anything was applied.
    pub(crate) fn write_document(
        &self,
        collection: &str,
        key: &[u8],
        doc: Option<&Value>,
        expires_at: Option<i64>,
    ) -> Result<bool> {
        if let Some(doc) = doc {
            self.validate_schema(collection, key, doc)?;
        }
        let ttl_on = self.ttl_enabled(collection);
        let applied = self.store().transaction(|tx| {
            match doc {
                Some(doc) => {
                    self.validate_unique_in_txn(tx, collection, key, doc)?;
                    tx.put(collection, key, &doc.encode())?;
                    self.index_on_insert_in_txn(tx, collection, key, doc)?;
                    self.fts_on_insert_in_txn(tx, collection, key, doc)?;
                    self.scalar_on_insert_in_txn(tx, collection, key, doc)?;
                    self.compound_on_insert_in_txn(tx, collection, key, doc)?;
                    self.geo_on_insert_in_txn(tx, collection, key, doc)?;
                    match expires_at {
                        Some(ts) => self.ttl_set_in_txn(tx, collection, key, ts)?,
                        None => {
                            if ttl_on {
                                self.ttl_clear_in_txn(tx, collection, key)?
                            }
                        }
                    }
                }
                None => {
                    let existed = tx.delete(collection, key)?;
                    if !existed {
                        return Ok(false);
                    }
                    self.index_on_delete_in_txn(tx, collection, key)?;
                    self.fts_on_delete_in_txn(tx, collection, key)?;
                    self.scalar_on_delete_in_txn(tx, collection, key)?;
                    self.compound_on_delete_in_txn(tx, collection, key)?;
                    self.geo_on_delete_in_txn(tx, collection, key)?;
                    if ttl_on {
                        self.ttl_clear_in_txn(tx, collection, key)?;
                    }
                }
            }
            Ok(true)
        })?;
        if applied {
            if expires_at.is_some() {
                self.mark_ttl_collection(collection);
            }
            self.finish_applied(collection, key, doc);
        }
        Ok(applied)
    }

    /// Post-commit work for an applied write: in-memory index maintenance
    /// (rebuildable state only) and change events. Never affects durability —
    /// by this point the data and all persisted indexes are committed.
    fn finish_applied(&self, collection: &str, key: &[u8], doc: Option<&Value>) {
        match doc {
            Some(doc) => {
                self.index_on_insert_memory(collection, key, doc);
                self.fts_on_insert_memory(collection, key, doc);
                self.notify(ChangeEvent {
                    collection: collection.to_owned(),
                    key: key.to_vec(),
                    kind: ChangeKind::Insert,
                });
            }
            None => {
                self.index_on_delete_memory(collection, key);
                self.fts_on_delete_memory(collection, key);
                self.notify(ChangeEvent {
                    collection: collection.to_owned(),
                    key: key.to_vec(),
                    kind: ChangeKind::Delete,
                });
            }
        }
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
    ///
    /// Atomic: the row, every persisted index's entry, and the unique-
    /// constraint check commit in one transaction. In-memory index state and
    /// change events follow a successful commit.
    pub fn insert(&self, key: &[u8], doc: &Value) -> Result<()> {
        self.ensure_writable()?;
        self.db.write_document(self.name, key, Some(doc), None)?;
        Ok(())
    }

    /// Read-modify-write `key`: `f` receives the current document (if any) and
    /// returns the new document (`Some`) or a deletion (`None`). Indexes stay
    /// consistent. This is a convenience over get-then-insert and is **not**
    /// linearizable against concurrent writers to the same key — use
    /// [`Collection::compare_and_set`] when that matters.
    pub fn update<F>(&self, key: &[u8], f: F) -> Result<()>
    where
        F: FnOnce(Option<Value>) -> Option<Value>,
    {
        self.ensure_writable()?;
        let current = self.get(key)?;
        match f(current) {
            Some(doc) => self.insert(key, &doc),
            None => {
                self.delete(key)?;
                Ok(())
            }
        }
    }

    /// Merge the top-level fields of `patch` into the existing map document at
    /// `key` (creating it if absent), then store. If either the existing
    /// document or `patch` is not a map, the document is replaced by `patch`.
    pub fn patch(&self, key: &[u8], patch: &Value) -> Result<()> {
        self.update(key, |current| match (current, patch) {
            (Some(Value::Map(mut m)), Value::Map(p)) => {
                for (k, v) in p {
                    m.insert(k.clone(), v.clone());
                }
                Some(Value::Map(m))
            }
            _ => Some(patch.clone()),
        })
    }

    /// Atomically write `new` (or delete, with `new = None`) only if the current
    /// value equals `expected` (`expected = None` means "must be absent").
    /// Returns whether the write was applied. The compare, the row write, the
    /// unique-constraint check, and every persisted index's maintenance all
    /// happen in one transaction.
    pub fn compare_and_set(
        &self,
        key: &[u8],
        expected: Option<&Value>,
        new: Option<Value>,
    ) -> Result<bool> {
        self.ensure_writable()?;
        if let Some(doc) = &new {
            self.db.validate_schema(self.name, key, doc)?;
        }
        let expected_bytes = expected.map(Value::encode);
        let ttl_on = self.db.ttl_enabled(self.name);
        let applied = self.db.store().transaction(|tx| {
            let current = tx.get(self.name, key)?;
            if current != expected_bytes {
                return Ok(false);
            }
            match &new {
                Some(doc) => {
                    self.db.validate_unique_in_txn(tx, self.name, key, doc)?;
                    tx.put(self.name, key, &doc.encode())?;
                    self.db.index_on_insert_in_txn(tx, self.name, key, doc)?;
                    self.db.fts_on_insert_in_txn(tx, self.name, key, doc)?;
                    self.db.scalar_on_insert_in_txn(tx, self.name, key, doc)?;
                    self.db.compound_on_insert_in_txn(tx, self.name, key, doc)?;
                    self.db.geo_on_insert_in_txn(tx, self.name, key, doc)?;
                    if ttl_on {
                        self.db.ttl_clear_in_txn(tx, self.name, key)?;
                    }
                }
                None => {
                    let existed = tx.delete(self.name, key)?;
                    if !existed {
                        return Ok(false);
                    }
                    self.db.index_on_delete_in_txn(tx, self.name, key)?;
                    self.db.fts_on_delete_in_txn(tx, self.name, key)?;
                    self.db.scalar_on_delete_in_txn(tx, self.name, key)?;
                    self.db.compound_on_delete_in_txn(tx, self.name, key)?;
                    self.db.geo_on_delete_in_txn(tx, self.name, key)?;
                    if ttl_on {
                        self.db.ttl_clear_in_txn(tx, self.name, key)?;
                    }
                }
            }
            Ok(true)
        })?;
        if applied {
            self.db.finish_applied(self.name, key, new.as_ref());
        }
        Ok(applied)
    }

    /// Stream every `(key, document)` in the collection to `f`, in key order,
    /// decoding one document at a time — constant memory regardless of size.
    /// Prefer this over [`Collection::scan`] for large collections.
    pub fn for_each_doc<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], Value) -> Result<bool>,
    {
        self.db.store().for_each(self.name, |key, bytes| {
            let doc = Value::decode(bytes)?;
            f(key, doc)
        })
    }

    /// The number of documents in the collection (O(1), maintained counter).
    pub fn len(&self) -> Result<usize> {
        Ok(self.db.store().count(self.name)? as usize)
    }

    /// Whether the collection is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Insert many documents in a single transaction (bulk load). Far faster
    /// than repeated [`Collection::insert`] (one commit instead of N). The
    /// whole batch is atomic: schema and unique checks see earlier items in
    /// the same batch, and every persisted index commits with the rows.
    /// In-memory index updates and change events follow a successful commit.
    pub fn insert_batch(&self, items: &[(&[u8], &Value)]) -> Result<()> {
        self.ensure_writable()?;
        // Fail fast on pure schema violations before opening the transaction.
        for (key, doc) in items {
            self.db.validate_schema(self.name, key, doc)?;
        }
        let ttl_on = self.db.ttl_enabled(self.name);
        self.db.store().transaction(|tx| {
            for (key, doc) in items {
                self.db.validate_unique_in_txn(tx, self.name, key, doc)?;
                tx.put(self.name, key, &doc.encode())?;
                self.db.index_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.fts_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.scalar_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.compound_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.geo_on_insert_in_txn(tx, self.name, key, doc)?;
                if ttl_on {
                    self.db.ttl_clear_in_txn(tx, self.name, key)?;
                }
            }
            Ok(())
        })?;
        for (key, doc) in items {
            self.db.finish_applied(self.name, key, Some(doc));
        }
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
    ///
    /// Atomic: the row deletion and every persisted index's cleanup commit in
    /// one transaction. In-memory index state and change events follow a
    /// successful commit.
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        self.ensure_writable()?;
        self.db.write_document(self.name, key, None, None)
    }

    /// Delete every document matching `predicate`; returns the number removed.
    /// Matching uses the query builder (so it is index-accelerated where
    /// possible); each match is removed through the normal delete path, keeping
    /// all indexes consistent.
    pub fn delete_where(&self, predicate: crate::filter::Predicate) -> Result<usize> {
        self.ensure_writable()?;
        let keys: Vec<Vec<u8>> = self
            .query()
            .filter(predicate)
            .run()?
            .into_iter()
            .map(|r| r.key)
            .collect();
        let mut removed = 0;
        for key in keys {
            if self.delete(&key)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Delete each of `keys`; returns how many existed and were removed.
    pub fn delete_batch(&self, keys: &[&[u8]]) -> Result<usize> {
        self.ensure_writable()?;
        let mut removed = 0;
        for key in keys {
            if self.delete(key)? {
                removed += 1;
            }
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

    /// Keyset pagination: up to `limit` documents in key order whose key is
    /// strictly greater than `after` (`None` starts at the beginning), with a
    /// `next` cursor to resume from. Unlike `offset`, this does not rescan
    /// earlier pages — each page resumes exactly where the last ended.
    pub fn page(&self, after: Option<&[u8]>, limit: usize) -> Result<Page> {
        self.page_inner(after, limit, None)
    }

    /// Like [`Collection::page`] but returning only documents matching
    /// `predicate`. The scan is streamed; the `next` cursor resumes after the
    /// last examined key, so every matching document is paged exactly once.
    pub fn page_where(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        predicate: crate::filter::Predicate,
    ) -> Result<Page> {
        self.page_inner(after, limit, Some(predicate))
    }

    fn page_inner(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        predicate: Option<crate::filter::Predicate>,
    ) -> Result<Page> {
        if limit == 0 {
            return Ok(Page {
                rows: Vec::new(),
                next: None,
            });
        }
        let mut cursor: Vec<u8> = match after {
            Some(k) => {
                let mut c = k.to_vec();
                c.push(0); // strictly greater than `after`
                c
            }
            None => Vec::new(),
        };
        let mut rows: Vec<(Vec<u8>, Value)> = Vec::new();
        let mut last_examined: Option<Vec<u8>> = None;
        const CHUNK: usize = 1024;
        'outer: loop {
            let chunk = self.db.store().scan_from(self.name, &cursor, CHUNK)?;
            if chunk.is_empty() {
                break;
            }
            for (k, bytes) in &chunk {
                last_examined = Some(k.clone());
                let doc = Value::decode(bytes)?;
                if predicate.as_ref().is_none_or(|p| p.eval(&doc)) {
                    rows.push((k.clone(), doc));
                    if rows.len() == limit {
                        break 'outer;
                    }
                }
            }
            cursor = {
                let mut c = chunk.last().unwrap().0.clone();
                c.push(0);
                c
            };
        }
        // A full page implies there may be more; resume after the last key we
        // looked at. A short page means we reached the end.
        let next = if rows.len() == limit {
            last_examined
        } else {
            None
        };
        Ok(Page { rows, next })
    }
}

/// One page of keyset-paginated results.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The `(key, document)` rows in this page, in key order.
    pub rows: Vec<(Vec<u8>, Value)>,
    /// Cursor to pass as `after` for the next page, or `None` at the end.
    pub next: Option<Vec<u8>>,
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
    fn compact_preserves_data_after_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let mut db = Db::open(&path).unwrap();
            {
                let c = db.collection("docs");
                for i in 0..500u32 {
                    c.insert(&i.to_le_bytes(), &doc("x", i as i64)).unwrap();
                }
                // Delete most of them to create reclaimable space.
                for i in 0..450u32 {
                    c.delete(&i.to_le_bytes()).unwrap();
                }
            }
            // Compaction succeeds and leaves the surviving data intact.
            db.compact().unwrap();
            assert_eq!(db.collection("docs").len().unwrap(), 50);
            assert_eq!(
                db.collection("docs").get(&475u32.to_le_bytes()).unwrap(),
                Some(doc("x", 475))
            );
        }
        // And it reopens cleanly.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.collection("docs").len().unwrap(), 50);
    }

    #[test]
    fn bulk_load_is_durable_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let db = Db::open(&path).unwrap();
            db.bulk(|| {
                let c = db.collection("docs");
                for i in 0..500u32 {
                    c.insert(&i.to_le_bytes(), &doc("x", i as i64))?;
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(db.collection("docs").len().unwrap(), 500);
        }
        // After the bulk flush + drop, everything is durable on reopen.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.collection("docs").len().unwrap(), 500);
        assert_eq!(
            db.collection("docs").get(&250u32.to_le_bytes()).unwrap(),
            Some(doc("x", 250))
        );
    }

    #[test]
    fn keyset_pagination_covers_all_rows_once() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..25u8 {
            c.insert(&[i], &doc("x", i as i64)).unwrap();
        }
        // Page through in pages of 10; collect all keys.
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = c.page(after.as_deref(), 10).unwrap();
            for (k, _) in &page.rows {
                seen.push(k.clone());
            }
            match page.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        let expected: Vec<Vec<u8>> = (0..25u8).map(|i| vec![i]).collect();
        assert_eq!(seen, expected); // every row once, in key order

        // Filtered keyset pagination: even n only.
        let mut even = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = c
                .page_where(
                    after.as_deref(),
                    4,
                    field("n").between(Value::Int(0), Value::Int(100)),
                )
                .unwrap();
            for (k, d) in &page.rows {
                assert!(d.get("n").is_some());
                even.push(k.clone());
            }
            match page.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        assert_eq!(even.len(), 25);
    }

    #[test]
    fn delete_where_and_batch() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_scalar_index("n").unwrap();
        for i in 0..20i64 {
            c.insert(&[i as u8], &doc("x", i)).unwrap();
        }
        // delete-by-query (index-accelerated): remove n >= 15 → 5 docs.
        let removed = c.delete_where(field("n").ge(Value::Int(15))).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(c.len().unwrap(), 15);
        // The scalar index reflects the deletions.
        assert!(
            c.query()
                .filter(field("n").ge(Value::Int(15)))
                .run()
                .unwrap()
                .is_empty()
        );
        // batch delete (some keys absent).
        let n = c.delete_batch(&[&[0u8], &[1u8], &[99u8]]).unwrap();
        assert_eq!(n, 2);
        assert_eq!(c.len().unwrap(), 13);
    }

    #[test]
    fn patch_merges_fields() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc("corvid", 1)).unwrap();
        // Merge: set n=2, add a new field, keep name.
        let mut p = BTreeMap::new();
        p.insert("n".to_owned(), Value::Int(2));
        p.insert("extra".to_owned(), Value::Bool(true));
        c.patch(b"k", &Value::Map(p)).unwrap();
        let got = c.get(b"k").unwrap().unwrap();
        assert_eq!(got.get("name"), Some(&Value::Text("corvid".into())));
        assert_eq!(got.get("n"), Some(&Value::Int(2)));
        assert_eq!(got.get("extra"), Some(&Value::Bool(true)));
        // Patch on an absent key creates it.
        c.patch(b"new", &doc("x", 9)).unwrap();
        assert_eq!(c.get(b"new").unwrap(), Some(doc("x", 9)));
    }

    #[test]
    fn update_can_modify_or_delete() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc("a", 1)).unwrap();
        c.update(b"k", |cur| {
            let mut m = match cur {
                Some(Value::Map(m)) => m,
                _ => BTreeMap::new(),
            };
            m.insert("n".to_owned(), Value::Int(5));
            Some(Value::Map(m))
        })
        .unwrap();
        assert_eq!(c.get(b"k").unwrap().unwrap().get("n"), Some(&Value::Int(5)));
        // Return None → delete.
        c.update(b"k", |_| None).unwrap();
        assert_eq!(c.get(b"k").unwrap(), None);
    }

    #[test]
    fn compare_and_set_is_conditional_and_maintains_indexes() {
        use crate::field;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.create_scalar_index("n").unwrap();
        c.insert(b"k", &doc("a", 1)).unwrap();

        // Wrong expected → not applied.
        assert!(
            !c.compare_and_set(b"k", Some(&doc("a", 999)), Some(doc("a", 2)))
                .unwrap()
        );
        assert_eq!(c.get(b"k").unwrap(), Some(doc("a", 1)));
        // Correct expected → applied; the scalar index reflects the new value.
        assert!(
            c.compare_and_set(b"k", Some(&doc("a", 1)), Some(doc("a", 2)))
                .unwrap()
        );
        let hits: Vec<_> = c
            .query()
            .filter(field("n").eq(Value::Int(2)))
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(hits, vec![b"k".to_vec()]);
        assert!(
            c.query()
                .filter(field("n").eq(Value::Int(1)))
                .run()
                .unwrap()
                .is_empty()
        );
        // Insert-if-absent: expected None on an existing key fails.
        assert!(!c.compare_and_set(b"k", None, Some(doc("a", 3))).unwrap());
        // Conditional delete.
        assert!(c.compare_and_set(b"k", Some(&doc("a", 2)), None).unwrap());
        assert_eq!(c.get(b"k").unwrap(), None);
        // Insert-if-absent now succeeds.
        assert!(
            c.compare_and_set(b"fresh", None, Some(doc("z", 7)))
                .unwrap()
        );
        assert_eq!(c.get(b"fresh").unwrap(), Some(doc("z", 7)));
    }

    #[test]
    fn insert_then_get_roundtrips_a_document() {
        let db = Db::open_in_memory().unwrap();
        let d = doc("corvid", 8);
        db.collection("docs").insert(b"k1", &d).unwrap();
        assert_eq!(db.collection("docs").get(b"k1").unwrap(), Some(d));
    }

    #[test]
    fn backup_preserves_documents_and_indexes() {
        use crate::field;
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("backup.db");
        {
            let db = Db::open_in_memory().unwrap();
            let c = db.collection("docs");
            for i in 0..20i64 {
                c.insert(&[i as u8], &doc("d", i)).unwrap();
            }
            c.create_scalar_index("n").unwrap();
            db.backup(&bak).unwrap();
        }
        // Reopen the backup: documents present and the scalar index still
        // serves a filtered query (its definition was copied and reloads).
        let db = Db::open(&bak).unwrap();
        let c = db.collection("docs");
        assert_eq!(c.len().unwrap(), 20);
        let rows = c
            .query()
            .filter(field("n").ge(Value::Int(18)))
            .run()
            .unwrap();
        let mut keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec![vec![18u8], vec![19u8]]);
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
    fn reserved_collection_names_are_rejected_on_delete() {
        let db = Db::open_in_memory().unwrap();
        // Seed a real internal namespace, then try to bypass its owner.
        db.collection("docs").link(b"a", "r", b"b").unwrap();
        let err = db.collection("__edges__docs").delete(b"whatever");
        assert!(matches!(err, Err(crate::Error::ReservedCollection(_))));
        // The edge itself is untouched.
        assert_eq!(
            db.collection("docs").neighbors(b"a", "r").unwrap(),
            vec![b"b".to_vec()]
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
