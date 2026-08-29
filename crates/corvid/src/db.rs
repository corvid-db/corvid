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
use crate::store::{SnapshotReader, Store};
use crate::value::Value;

/// An embedded document database.
///
/// Documents are the source of truth; every index is derived from them.
/// Persisted indexes are maintained *inside* the document's write
/// transaction, so they always agree with the committed rows. The in-memory
/// ANN cache holds derived graphs rebuilt lazily from a fresh committed
/// state under the registry lock — never a stale snapshot — so queries never
/// observe an index behind the documents. Query execution reads one MVCC
/// snapshot per query, and each `page`/`page_where` call walks one snapshot
/// end to end.
pub struct Db {
    store: Store,
    indexes: Mutex<IndexState>,
    fts: Mutex<crate::fts::FtsState>,
    scalar: Mutex<crate::scalar::ScalarState>,
    geo: Mutex<crate::geo_index::GeoState>,
    schemas: Mutex<crate::schema::SchemaState>,
    ttl: Mutex<crate::ttl::TtlState>,
    subscribers: Mutex<Subscribers>,
    /// Serialization point for lazy index-build resumes (try-lock only: a
    /// query arriving while another thread resumes proceeds on fallbacks).
    index_resume: Mutex<()>,
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
            index_resume: Mutex::new(()),
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
            index_resume: Mutex::new(()),
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
    /// Relaxed durability applies only to write transactions on the calling
    /// thread; concurrent writers are unaffected. The scope is panic-safe.
    ///
    /// Turns an N-document load from ~N fsyncs into ~1.
    pub fn bulk<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        // Relaxed durability applies only to transactions begun on this
        // thread, and the scope is panic-safe (RAII).
        let _scope = self.store.begin_bulk();
        let result = f();
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

    /// Serialization point for lazy index-build resumes (try-lock only: a
    /// query arriving while another thread resumes proceeds on fallbacks).
    pub(crate) fn index_resume(&self) -> &Mutex<()> {
        &self.index_resume
    }

    /// Resume any interrupted index builds for `collection` before its
    /// indexes are consulted. Try-lock: if another thread is already
    /// resuming, return and let callers run on their fallbacks.
    pub(crate) fn try_resume_index_builds(&self, collection: &str) -> Result<()> {
        let scalar_jobs = self.collect_building_scalar(collection)?;
        let compound_jobs = self.collect_building_compound(collection)?;
        let geo_jobs = self.collect_building_geo(collection)?;
        let text_jobs = self.collect_building_text(collection)?;
        let vector_jobs = self.collect_building_vector(collection)?;
        if scalar_jobs.is_empty()
            && compound_jobs.is_empty()
            && geo_jobs.is_empty()
            && text_jobs.is_empty()
            && vector_jobs.is_empty()
        {
            return Ok(());
        }
        // Bound to this call: a concurrent caller's try_lock fails and it
        // proceeds on its fallback (results stay correct either way).
        let resume = self.index_resume().try_lock();
        if resume.is_err() {
            return Ok(());
        }
        crate::telemetry::event!(
            DEBUG,
            message = "lazy_resume_batch",
            collection = crate::telemetry::display(collection),
            scalar = scalar_jobs.len() as u64,
            compound = compound_jobs.len() as u64,
            geo = geo_jobs.len() as u64,
            text = text_jobs.len() as u64,
            vector = vector_jobs.len() as u64,
        );
        for (field, cursor) in scalar_jobs {
            self.resume_scalar(collection, &field, &cursor)?;
        }
        for (fields, cursor) in compound_jobs {
            self.resume_compound(collection, &fields, &cursor)?;
        }
        for (field, cursor) in geo_jobs {
            self.resume_geo(collection, &field, &cursor)?;
        }
        for (field, cursor) in text_jobs {
            self.resume_text(collection, &field, &cursor)?;
        }
        for (field, cursor) in vector_jobs {
            self.resume_vector(collection, &field, &cursor)?;
        }
        Ok(())
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
        // Audit B2: TTL maintenance is decided INSIDE the transaction (the
        // per-key probe in `ttl_clear_in_txn` sees committed `__ttl__` state),
        // never from a pre-transaction read that a concurrent writer could
        // race.
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
                        None => self.ttl_clear_in_txn(tx, collection, key)?,
                    }
                }
                None => {
                    let existed = tx.delete(collection, key)?;
                    if !existed {
                        // No document row — but the key may still carry
                        // DANGLING edges (`link` allows absent endpoints).
                        // Purge them so a delete is always a full cleanup
                        // of the key; the call still reports that no
                        // document was removed and fires no event.
                        self.edges_on_delete_in_txn(tx, collection, key)?;
                        return Ok(false);
                    }
                    self.index_on_delete_in_txn(tx, collection, key)?;
                    self.fts_on_delete_in_txn(tx, collection, key)?;
                    self.scalar_on_delete_in_txn(tx, collection, key)?;
                    self.compound_on_delete_in_txn(tx, collection, key)?;
                    self.geo_on_delete_in_txn(tx, collection, key)?;
                    // Audit B4: a deleted document takes its edges with it —
                    // both directions, in this same transaction.
                    self.edges_on_delete_in_txn(tx, collection, key)?;
                    self.ttl_clear_in_txn(tx, collection, key)?;
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

    /// The `insert_auto` write path (audit C9): the auto-id reservation, the
    /// row write, the unique-constraint check, and every *persisted* index's
    /// maintenance all commit in ONE transaction, so a failed insert cannot
    /// burn an id — the counter increments only with the document it named.
    /// Returns the freshly generated key.
    pub(crate) fn write_auto_document(&self, collection: &str, doc: &Value) -> Result<Vec<u8>> {
        let key = self.store().transaction(|tx| {
            let id = tx.next_auto_id(collection)?;
            // Zero-padded decimal: UTF-8 (round-trips through text APIs like
            // MCP) and lexicographically ordered by id.
            let key = format!("{id:020}").into_bytes();
            self.validate_schema(collection, &key, doc)?;
            self.validate_unique_in_txn(tx, collection, &key, doc)?;
            tx.put(collection, &key, &doc.encode())?;
            self.index_on_insert_in_txn(tx, collection, &key, doc)?;
            self.fts_on_insert_in_txn(tx, collection, &key, doc)?;
            self.scalar_on_insert_in_txn(tx, collection, &key, doc)?;
            self.compound_on_insert_in_txn(tx, collection, &key, doc)?;
            self.geo_on_insert_in_txn(tx, collection, &key, doc)?;
            self.ttl_clear_in_txn(tx, collection, &key)?;
            Ok(key)
        })?;
        self.finish_applied(collection, &key, Some(doc));
        Ok(key)
    }

    /// Post-commit work for an applied write: in-memory index maintenance
    /// (rebuildable state only), on-disk dead-fraction compaction checks
    /// (audit B5), and change events. Never affects durability — by this
    /// point the data and all persisted indexes are committed, and no locks
    /// are held, so the compaction's `index_resume` try-lock is safe.
    pub(crate) fn finish_applied(&self, collection: &str, key: &[u8], doc: Option<&Value>) {
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
        // Audit B5: dead only grows through applied writes (deletes AND
        // overwrites tombstone the old node), so checking here — once per
        // applied write, one meta point-get per on-disk index — observes
        // every threshold crossing without putting maintenance on the read
        // path. Skips collections with no on-disk vector index for free.
        self.compact_ondisk_vector_indexes(collection);
    }
}

/// A u64 record count as usize, saturating (audit C9): on targets where
/// usize is narrower than u64 a huge maintained count reports `usize::MAX`
/// rather than truncating to a small, wildly wrong length.
fn count_as_usize(n: u64) -> usize {
    n.try_into().unwrap_or(usize::MAX)
}

/// Validate a user-supplied name (collection or field), audit C7: it must
/// contain no NUL byte (NUL corrupts length-prefixed key/value encodings)
/// and no `__` sequence (the engine builds internal namespaces and def keys
/// from `__`-separated parts, so a user `a__b` could forge or collide with
/// one — e.g. `x__edges__docs` or an index-def key). A LEADING `__` is
/// additionally reserved and reported as [`crate::Error::ReservedCollection`]
/// by [`Collection::ensure_writable`]. Breaking change ahead of 1.0: names
/// with an interior `__` that pre-1.0 versions accepted are now rejected.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.contains('\0') || name.contains("__") {
        return Err(crate::Error::InvalidName(name.to_owned()));
    }
    Ok(())
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
    /// used for internal namespaces such as graph edges) and names that
    /// could forge them (audit C7: any interior `__`, any NUL byte).
    pub(crate) fn ensure_writable(&self) -> Result<()> {
        if self.name.starts_with("__") {
            return Err(crate::Error::ReservedCollection(self.name.to_owned()));
        }
        validate_name(self.name)
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

    /// Read-modify-write `key`: `f` receives the current document (`None`
    /// when `key` is absent — a missing document is not an error) and returns
    /// the new document (`Some`) or a deletion (`None`). Indexes stay
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
    /// happen in one transaction. Equality is the engine's semantic value
    /// equality (the same rule unique constraints use,
    /// `schema::unique_value_eq`): `NaN` equals `NaN` regardless of payload,
    /// `-0.0` equals `0.0`, and containers compare element-wise.
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
        let applied = self.db.store().transaction(|tx| {
            let current = match tx.get(self.name, key)? {
                Some(bytes) => Some(Value::decode(&bytes)?),
                None => None,
            };
            let matches = match (&current, expected) {
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
                (Some(cur), Some(exp)) => crate::schema::unique_value_eq(cur, exp),
            };
            if !matches {
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
                    self.db.ttl_clear_in_txn(tx, self.name, key)?;
                }
                None => {
                    let existed = tx.delete(self.name, key)?;
                    if !existed {
                        // Same dangling-edge purge as the plain delete
                        // path: the compare matched, no document was
                        // removed (return false), but edges linked against
                        // the absent key still go.
                        self.db.edges_on_delete_in_txn(tx, self.name, key)?;
                        return Ok(false);
                    }
                    self.db.index_on_delete_in_txn(tx, self.name, key)?;
                    self.db.fts_on_delete_in_txn(tx, self.name, key)?;
                    self.db.scalar_on_delete_in_txn(tx, self.name, key)?;
                    self.db.compound_on_delete_in_txn(tx, self.name, key)?;
                    self.db.geo_on_delete_in_txn(tx, self.name, key)?;
                    // Audit B4: same in-transaction edge cascade as a plain delete.
                    self.db.edges_on_delete_in_txn(tx, self.name, key)?;
                    self.db.ttl_clear_in_txn(tx, self.name, key)?;
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

    /// Snapshot-scoped twin of [`Collection::for_each_doc`]: stream every
    /// `(key, document)` from `reader`'s snapshot to `f`, in key order,
    /// decoding one document at a time. Constant memory regardless of size;
    /// the decode stays here so reader call sites share one implementation.
    pub(crate) fn for_each_doc_in<F>(&self, reader: &dyn SnapshotReader, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], Value) -> Result<bool>,
    {
        reader.for_each(self.name, &mut |key, bytes| {
            let doc = Value::decode(bytes)?;
            f(key, doc)
        })
    }

    /// The number of documents in the collection (O(1), maintained counter).
    pub fn len(&self) -> Result<usize> {
        Ok(count_as_usize(self.db.store().count(self.name)?))
    }

    /// Whether the collection is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Insert many documents in a single transaction (bulk load). Far faster
    /// than repeated [`Collection::insert`] (one commit instead of N). The
    /// whole batch is atomic: schema and unique checks see earlier items in
    /// the same batch, and every persisted index commits with the rows.
    /// Duplicate keys inside one batch follow [`Collection::insert`]'s
    /// overwrite contract (last write wins); whole-batch rollback applies to
    /// schema and unique violations. In-memory index updates and change
    /// events follow a successful commit.
    pub fn insert_batch(&self, items: &[(&[u8], &Value)]) -> Result<()> {
        self.ensure_writable()?;
        // Fail fast on pure schema violations before opening the transaction.
        for (key, doc) in items {
            self.db.validate_schema(self.name, key, doc)?;
        }
        self.db.store().transaction(|tx| {
            for (key, doc) in items {
                self.db.validate_unique_in_txn(tx, self.name, key, doc)?;
                tx.put(self.name, key, &doc.encode())?;
                self.db.index_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.fts_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.scalar_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.compound_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.geo_on_insert_in_txn(tx, self.name, key, doc)?;
                self.db.ttl_clear_in_txn(tx, self.name, key)?;
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
    ///
    /// Atomic (audit C9): the id is reserved inside the insert transaction,
    /// so a failed insert (schema/unique violation) does not consume an id.
    pub fn insert_auto(&self, doc: &Value) -> Result<Vec<u8>> {
        self.ensure_writable()?;
        self.db.write_auto_document(self.name, doc)
    }

    /// Fetch and decode the document at `key`, if present.
    pub fn get(&self, key: &[u8]) -> Result<Option<Value>> {
        match self.db.store().get(self.name, key)? {
            Some(bytes) => Ok(Some(Value::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Snapshot-scoped twin of [`Collection::get`]: fetch and decode the
    /// document at `key` from `reader`'s snapshot. The caller holds one read
    /// transaction for a whole query (audit B3), so every per-key fetch
    /// observes the same point in time.
    pub(crate) fn get_in(&self, reader: &dyn SnapshotReader, key: &[u8]) -> Result<Option<Value>> {
        match reader.get(self.name, key)? {
            Some(bytes) => Ok(Some(Value::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Remove the document at `key`. Returns whether one was removed.
    ///
    /// Atomic: the row deletion and every persisted index's cleanup commit in
    /// one transaction. In-memory index state and change events follow a
    /// successful commit. Every graph edge touching `key` is removed in the
    /// same transaction — including edges dangling on a key that never
    /// existed as a document, so a delete returning `false` still cleans the
    /// key's edges.
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

    /// Snapshot-scoped twin of [`Collection::scan`]: every `(key, document)`
    /// pair from `reader`'s snapshot, in key order.
    pub(crate) fn scan_in(&self, reader: &dyn SnapshotReader) -> Result<Vec<(Vec<u8>, Value)>> {
        let mut out = Vec::new();
        for (k, bytes) in reader.scan(self.name)? {
            out.push((k, Value::decode(&bytes)?));
        }
        Ok(out)
    }

    /// Keyset pagination: up to `limit` documents in key order whose key is
    /// strictly greater than `after` (`None` starts at the beginning), with a
    /// `next` cursor to resume from. Unlike `offset`, this does not rescan
    /// earlier pages — each page resumes exactly where the last ended.
    ///
    /// The whole page is read from ONE MVCC snapshot: a writer committing
    /// mid-walk is invisible to the page in progress, so the returned rows
    /// always match some committed point in time (see [`Self::page_where`]
    /// for the snapshot-holding cost).
    pub fn page(&self, after: Option<&[u8]>, limit: usize) -> Result<Page> {
        self.page_inner(after, limit, None)
    }

    /// Like [`Collection::page`] but returning only documents matching
    /// `predicate`. The scan is streamed; the `next` cursor resumes after the
    /// last examined key, so every matching document is paged exactly once.
    ///
    /// Single-snapshot like [`Collection::page`]: the entire chunked walk
    /// runs inside one read transaction.
    ///
    /// Snapshot-holding cost: redb is MVCC, so the read transaction pins the
    /// snapshot but never blocks the (single) writer — writers commit and
    /// readers proceed concurrently. The pin's cost is space, not latency:
    /// pages freed by commits landing during the walk stay in the file
    /// (pending-free) until the transaction ends, so a long-lived walk shows
    /// up as temporary file growth, reclaimable by a later
    /// [`Db::compact`]. That exposure is bounded here: the walk stops after
    /// `limit` rows (and per-chunk reads are capped at 1024 keys), so the
    /// transaction lives for one page call, never across calls — successive
    /// pages each see the then-current state (the usual keyset-pagination
    /// contract).
    pub fn page_where(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        predicate: crate::filter::Predicate,
    ) -> Result<Page> {
        self.page_inner(after, limit, Some(predicate))
    }

    /// The shared core of [`Collection::page`] / [`Collection::page_where`].
    ///
    /// The entire walk — every 1024-key chunk — executes inside ONE
    /// `store().read()` closure, so the page observes a single MVCC snapshot
    /// (audit B3 discipline, mirroring the query builder's `run()` →
    /// `run_with(reader)` split: chunked reads INSIDE the transaction keep
    /// memory bounded — `ReadBatch::scan_from` pages — while the snapshot
    /// makes the page point-in-time consistent even under concurrent
    /// writers). `ReadBatch::scan_from` is byte-identical to
    /// `Store::scan_from` on a static database (pinned in store.rs), so
    /// results, `Page` shape, and the cursor contract are unchanged; only
    /// the consistency guarantee tightens.
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
        self.db.store().read(|reader| {
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
                let chunk = reader.scan_from(self.name, &cursor, CHUNK)?;
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
        })
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

    /// Audit C9: a failed `insert_auto` must not burn an id. The reservation
    /// happens INSIDE the insert transaction, so a schema-violating document
    /// (rejected up front) and a unique-constraint violation (rejected inside
    /// the transaction, after the reservation) both roll the counter back and
    /// the next insert reuses the same id.
    #[test]
    fn failed_insert_auto_does_not_burn_an_id() {
        use crate::schema::{Field, FieldType, Schema};
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("events");
        let s = Schema::new()
            .field(Field::new("n", FieldType::Int).required())
            .field(Field::new("u", FieldType::Text).unique());
        c.set_schema(&s).unwrap();
        c.create_scalar_index("u").unwrap();

        fn ev(n: i64, u: &str) -> Value {
            let mut m = std::collections::BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(n));
            m.insert("u".to_owned(), Value::Text(u.to_owned()));
            Value::Map(m)
        }

        // 1. Schema-violating doc → Err; the id it would have taken is NOT
        //    consumed.
        let mut bad = std::collections::BTreeMap::new();
        bad.insert("n".to_owned(), Value::Text("not an int".into()));
        assert!(c.insert_auto(&Value::Map(bad)).is_err());
        assert_eq!(
            c.insert_auto(&ev(0, "a")).unwrap(),
            b"00000000000000000000".to_vec(),
            "id 0 must be reissued"
        );

        // 2. Unique-constraint failure INSIDE the transaction (after the
        //    reservation): the counter rolls back with the row.
        assert!(c.insert_auto(&ev(1, "a")).is_err());
        assert_eq!(
            c.insert_auto(&ev(2, "b")).unwrap(),
            b"00000000000000000001".to_vec(),
            "id 1 must be reissued after the unique violation"
        );
    }

    /// Audit C9: `len()` saturates instead of truncating on platforms where
    /// usize is narrower than the u64 maintained count (unreachable on
    /// 64-bit targets; pinned at the conversion).
    #[test]
    fn len_count_conversion_saturates() {
        assert_eq!(count_as_usize(0), 0);
        assert_eq!(count_as_usize(42), 42);
        assert_eq!(count_as_usize(u64::MAX), usize::MAX);
        assert_eq!(count_as_usize(usize::MAX as u64), usize::MAX);
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

    /// C7 accept/reject table for user-supplied names: anything goes except
    /// a NUL byte (corrupts length-prefixed encodings) and `__` anywhere
    /// (could forge the engine's `__`-separated internal namespaces). A
    /// LEADING `__` stays `ReservedCollection` (see ensure_writable).
    #[test]
    fn validate_name_accepts_and_rejects() {
        // Accepts: single underscore, dashes, dots, spaces, unicode, empty.
        for ok in ["docs", "a_b", "user-events", "v2.0", "my docs", "文档", ""] {
            assert!(validate_name(ok).is_ok(), "{ok:?} must be accepted");
        }
        // Rejects: interior/trailing/leading `__` and any NUL byte.
        for bad in ["a__b", "a__", "__x", "doc\0s", "\0"] {
            assert!(
                matches!(validate_name(bad), Err(crate::Error::InvalidName(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    /// Interior `__` and NUL in COLLECTION names are rejected at the write
    /// boundary (audit C7): such a name could collide with an engine
    /// namespace (`__edges__`, `__ttl__`, index-def keys).
    #[test]
    fn collection_names_with_interior_underscores_or_nul_are_rejected() {
        let db = Db::open_in_memory().unwrap();
        for bad in ["a__b", "doc\0s"] {
            let err = db.collection(bad).insert(b"k", &Value::Int(1));
            assert!(
                matches!(err, Err(crate::Error::InvalidName(_))),
                "{bad:?} must be rejected on write"
            );
            assert_eq!(db.collection(bad).get(b"k").unwrap(), None);
        }
        // Leading `__` keeps its dedicated error.
        assert!(matches!(
            db.collection("__x").insert(b"k", &Value::Int(1)),
            Err(crate::Error::ReservedCollection(_))
        ));
        // A single underscore is fine.
        assert!(db.collection("a_b").insert(b"k", &Value::Int(1)).is_ok());
    }

    /// Field names on every index-creation path and on `set_schema` get the
    /// same validation as collection names (audit C7 + B8: these paths also
    /// now refuse engine-reserved COLLECTION names via ensure_writable).
    #[test]
    fn index_and_schema_field_names_are_validated() {
        use crate::schema::{Field, FieldType, Schema};
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &Value::Int(1)).unwrap();
        for bad in ["a__b", "f\0"] {
            assert!(matches!(
                c.create_scalar_index(bad),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_compound_index(&["x", bad]),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_geo_index(bad),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_text_index(bad),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_text_index_ondisk(bad),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_vector_index(bad, crate::Metric::Cosine),
                Err(crate::Error::InvalidName(_))
            ));
            assert!(matches!(
                c.create_vector_index_ondisk(bad, crate::Metric::Cosine),
                Err(crate::Error::InvalidName(_))
            ));
            let s = Schema::new().field(Field::new(bad, FieldType::Int));
            assert!(matches!(
                c.set_schema(&s),
                Err(crate::Error::InvalidName(_))
            ));
        }
        // A reserved COLLECTION name is refused on these paths too (B8):
        // creating an index over `__edges__docs` would corrupt the namespace.
        let r = db.collection("__edges__docs");
        assert!(matches!(
            r.create_scalar_index("f"),
            Err(crate::Error::ReservedCollection(_))
        ));
        let s = Schema::new().field(Field::new("f", FieldType::Int));
        assert!(matches!(
            r.set_schema(&s),
            Err(crate::Error::ReservedCollection(_))
        ));
        // Valid field names still work.
        assert!(c.create_scalar_index("a_b").is_ok());
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
