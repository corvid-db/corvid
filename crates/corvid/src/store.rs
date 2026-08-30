//! Transactional byte-oriented key/value store over redb (engine layer L1).
//!
//! A [`Store`] holds many named *collections*. Rather than mapping each
//! collection to a redb table (redb table names must be `&'static str`,
//! which rules out user-defined names), every collection is assigned a
//! `u64` id and all records live in one physical table keyed by
//! `id.to_be_bytes() ++ user_key`. Big-endian ids keep collections
//! contiguous and ordered, so a collection scan is a single prefix range.
//!
//! With the optional `zstd` cargo feature, user-collection values are
//! transparently compressed on write and decompressed on every read at
//! this seam (the private `compression` module holds the on-disk marker
//! scheme); engine-reserved `__` namespaces and the default build store
//! bytes verbatim.
//!
//! Writes go through [`Store::transaction`], which exposes a [`WriteBatch`]
//! and commits once at the end — every operation in the closure lands
//! atomically or not at all. This underpins atomic multi-key writes such as a
//! graph edge and its reverse. Reads go through [`Store::read`], which exposes
//! a [`ReadBatch`] over one consistent snapshot. The single-op helpers
//! ([`Store::put`] etc.) are thin wrappers over these.
//!
//! Indexes come in two kinds with different consistency stories. *Persisted*
//! indexes (scalar, compound, geo, on-disk FTS, on-disk HNSW) are derived
//! state stored as ordinary records, and their maintenance joins the
//! document's own write transaction — a commit covers the row and every
//! persisted index update atomically, so a crash can never leave one
//! disagreeing with the documents. *In-memory* indexes (HNSW graph, inverted
//! text) are rebuilt lazily from the documents and maintained incrementally
//! after a commit; they are derived state the next open or first query can
//! always reconstruct, so they cannot go stale. Creating an index of either
//! kind persists its definition through a `Building{cursor}` → `Complete`
//! state machine (crash-safe creation; queries never serve a `Building`
//! index).

use std::marker::PhantomData;
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
    /// When set, write transactions commit without fsync (redb
    /// `Durability::None`) — a bulk-load fast path. Committed data stays
    /// consistent; the most recent writes may be lost on a crash until a
    /// following durable commit ([`Store::flush`]).
    relaxed: std::sync::atomic::AtomicBool,
}

thread_local! {
    /// Depth of bulk-load scopes on *this* thread. Write transactions relax
    /// durability only when the issuing thread is inside a bulk scope (plus
    /// the explicit whole-store opt-in), so a bulk load on one thread never
    /// silently degrades concurrent writers' durability.
    static BULK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Guard marking the current thread as inside a bulk load (relaxed
/// durability for write transactions started here). Dropping it — normally
/// or on unwind — restores durable commits. Created by [`Store::begin_bulk`].
///
/// `!Send`/`!Sync` by construction: the guard owns a decrement of *this*
/// thread's bulk depth, so dropping it on any other thread would leak the
/// scope. Keep it on the thread that called `begin_bulk`.
#[must_use = "dropping the guard immediately ends the bulk scope"]
pub struct BulkScope(PhantomData<*const ()>);

impl Drop for BulkScope {
    fn drop(&mut self) {
        BULK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

impl Store {
    /// Open (creating if absent) a store backed by a file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = Self {
            db: Database::create(path)?,
            relaxed: std::sync::atomic::AtomicBool::new(false),
        };
        store.check_format_version()?;
        Ok(store)
    }

    /// Open a purely in-memory store. Nothing is persisted.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            db: Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?,
            relaxed: std::sync::atomic::AtomicBool::new(false),
        };
        store.check_format_version()?;
        Ok(store)
    }

    /// Enter or leave relaxed (eventual) durability for write transactions.
    pub fn set_relaxed_durability(&self, on: bool) {
        self.relaxed.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Enter a bulk-load scope on the current thread. Write transactions
    /// started on this thread skip the per-commit fsync until the returned
    /// guard drops; make them durable with [`Store::flush`].
    #[must_use = "dropping the guard immediately ends the bulk scope"]
    pub fn begin_bulk(&self) -> BulkScope {
        BULK_DEPTH.with(|d| d.set(d.get() + 1));
        BulkScope(PhantomData)
    }

    /// Whether write transactions on the current thread would relax
    /// durability (bulk scope active or explicit opt-in).
    fn bulk_active_on_this_thread(&self) -> bool {
        self.relaxed.load(std::sync::atomic::Ordering::Relaxed) || BULK_DEPTH.with(|d| d.get() > 0)
    }

    /// Force a durable (fsync) commit, making all prior eventual writes durable.
    pub fn flush(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.commit()?; // an immediate-durability commit fsyncs prior writes
        Ok(())
    }

    /// Reclaim unused file space (e.g. after many deletes) by compacting the
    /// underlying database. Returns whether compaction moved any data. Requires
    /// exclusive access (`&mut self`); no concurrent readers/writers.
    pub fn compact(&mut self) -> Result<bool> {
        Ok(self.db.compact()?)
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
    /// insertion order. Note this commits its own transaction: paths that
    /// must not burn an id on failure (e.g. `insert_auto`) reserve in-txn
    /// via [`WriteBatch::next_auto_id`] instead.
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

    /// Restore auto-id counters from a snapshot. Each counter becomes the max
    /// of its old value and the snapshot's (counters never go backwards).
    pub(crate) fn restore_auto_ids(&self, ids: &[(String, u64)]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            for (coll, next) in ids {
                let key = format!("auto:{coll}");
                let current = meta.get(key.as_str())?.map(|g| g.value()).unwrap_or(0);
                meta.insert(key.as_str(), (*next).max(current))?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Write a consistent copy of the store to a fresh database file at `path`.
    ///
    /// The copy is taken from one read snapshot, so it is point-in-time
    /// consistent and safe to call while writers are active (a concurrent
    /// commit simply isn't included). The target must not already exist:
    /// redb opens-or-creates, so writing over a previous backup would
    /// *merge* into it (resurrecting deleted records) rather than replace
    /// it — an existing path is refused with
    /// [`crate::Error::BackupTargetExists`]. The result is a complete,
    /// independent database openable with [`Store::open`].
    ///
    /// Audit C8: a backup that fails anywhere after the exists() check
    /// removes the partial destination (best-effort) before returning the
    /// original error, so a failed backup never leaves debris behind —
    /// debris would both masquerade as a valid backup and block every
    /// future attempt via the exists() refusal. The residual
    /// check-then-create race (another creator winning the window between
    /// `exists()` and `Database::create`) is accepted: corvid is a
    /// single-process embedded engine, so concurrent backups of one store
    /// are the caller's responsibility to serialize.
    ///
    /// The copy is physical (raw stored rows, verbatim), so it is NOT
    /// portable across feature configurations: rows written by a `zstd`
    /// feature-on binary fail per-row with clean `Decode` errors when read
    /// by a feature-off binary (and never misparse — the marker byte is
    /// outside the value codec's tag space). `dump`/`load` is the
    /// migration path between configurations: the dump stream reads
    /// through the store, so it carries raw value encodings either way.
    pub fn backup(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if path.exists() {
            return Err(crate::Error::BackupTargetExists(path.display().to_string()));
        }
        let result = self.backup_tables(path);
        if result.is_err() {
            // Audit C8: no debris. Best-effort — the remove can itself fail
            // (nothing was created; the file is held elsewhere) — the
            // original error is what the caller needs.
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// The table copy underlying [`Store::backup`]: snapshot the source,
    /// create the destination, copy each table in one destination
    /// transaction. Any error propagates to the caller, which then removes
    /// the partial destination.
    fn backup_tables(&self, path: &Path) -> Result<()> {
        let src = self.db.begin_read()?;
        let dst = Database::create(path)?;
        let wtx = dst.begin_write()?;

        // Copy each table from the snapshot. A table absent in the source
        // (never written) is simply skipped.
        macro_rules! copy_table {
            ($def:expr) => {{
                match src.open_table($def) {
                    Ok(rt) => {
                        let mut wt = wtx.open_table($def)?;
                        for entry in rt.iter()? {
                            let (k, v) = entry?;
                            wt.insert(k.value(), v.value())?;
                        }
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }};
        }
        copy_table!(RECORDS);
        copy_table!(CATALOG);
        copy_table!(META);

        wtx.commit()?;
        Ok(())
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
        let mut txn = self.db.begin_write()?;
        if self.bulk_active_on_this_thread() {
            // `None` skips the fsync; a later `Immediate` commit (flush) persists.
            txn.set_durability(redb::Durability::None)?;
        }
        let out = {
            let mut batch = WriteBatch {
                txn: &txn,
                ids: std::cell::RefCell::new(std::collections::HashMap::new()),
            };
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
    ///
    /// With the `zstd` feature on, a user-namespace `value` whose raw bytes
    /// begin with `0xFF` is force-compressed (possibly slightly larger): the
    /// compression marker byte is reserved in user namespaces under the
    /// feature, so a raw stored row can never begin with it — engine-written
    /// value encodings can never start with `0xFF` (tags `0..=8`), so this
    /// only binds direct [`Store`] users.
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

    /// Return up to `limit` `(key, value)` pairs in `collection` whose key is
    /// `>= start`, in key order. For cursor pagination: pass an empty `start`,
    /// then resume from `last_key` with a trailing `0` byte appended. An
    /// unknown collection yields an empty vector.
    pub fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
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
        lower.extend_from_slice(start);
        let upper = id.checked_add(1).map(|n| n.to_be_bytes().to_vec());
        let lower_slice: &[u8] = &lower;
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (Bound::Included(lower_slice), Bound::Excluded(u.as_slice())),
            None => (Bound::Included(lower_slice), Bound::Unbounded),
        };
        let mut out = Vec::new();
        for entry in records.range::<&[u8]>(bounds)? {
            if out.len() >= limit {
                break;
            }
            let (k, v) = entry?;
            out.push((
                user_key(k.value()),
                crate::compression::decompress(collection, v.value())?.into_owned(),
            ));
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
    /// collection size. `f` returns `false` to stop early. `f`'s slices are
    /// valid only for the duration of the call.
    pub fn for_each<F>(&self, collection: &str, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
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
            let value = crate::compression::decompress(collection, v.value())?;
            if !f(key.get(8..).unwrap_or(&[]), value.as_ref())? {
                break;
            }
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
            out.push((
                user_key(k.value()),
                crate::compression::decompress(collection, v.value())?.into_owned(),
            ));
        }
        Ok(out)
    }
}

/// Delete every key in `collection` inside the caller's transaction (audit
/// A5: namespace reset). Paged via [`WriteBatch::scan_from`] — the batch sees
/// its own deletes, and each page's resume point sits strictly past the keys
/// just removed, so nothing is materialized and no key survives. An unknown
/// or already-empty collection is a no-op.
///
/// The maintained count is adjusted once per PAGE (delta = the page's length,
/// captured before the page's keys are deleted) instead of once per key, so
/// clearing N keys costs ⌈N/PAGE⌉ META read-modify-writes, not N.
pub(crate) fn clear_in_txn(tx: &mut WriteBatch<'_>, collection: &str) -> Result<()> {
    const PAGE: usize = 2048;
    let mut start: Vec<u8> = Vec::new();
    loop {
        let page = tx.scan_from(collection, &start, PAGE)?;
        let Some((last, _)) = page.last().cloned() else {
            break;
        };
        tx.adjust_count(collection, -(page.len() as i64))?;
        for (key, _) in &page {
            tx.delete_uncounted(collection, key)?;
        }
        // Resume strictly past everything deleted above (the documented
        // cursor-pagination convention: `last_key` + trailing `0` byte).
        start = last;
        start.push(0);
    }
    Ok(())
}

/// A set of writes (and reads) executed inside one transaction.
///
/// Obtained via [`Store::transaction`]. Reads see this transaction's own
/// uncommitted writes.
pub struct WriteBatch<'txn> {
    txn: &'txn redb::WriteTransaction,
    /// Per-transaction cache of resolved collection ids. Every put/delete
    /// resolves its namespace id through the CATALOG table; a multi-row
    /// transaction (a graph `link` writes four namespaces) would re-open the
    /// catalog and re-get the name per row. The cache is per-`WriteBatch`
    /// (per transaction) and records ids this transaction assigned too, so
    /// it can never go stale. `RefCell`: id resolution is an implementation
    /// detail of `&self` reads (a probe `get` warms the cache too), not an
    /// observable mutation.
    ids: std::cell::RefCell<std::collections::HashMap<String, u64>>,
}

impl WriteBatch<'_> {
    /// Insert or overwrite `value` at `key` within `collection`, creating the
    /// collection on first use.
    pub fn put(&mut self, collection: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let id = self.ensure_id(collection)?;
        // Feature `zstd`: user-collection values may be stored compressed
        // (self-describing marker; see `compression`). Engine-reserved
        // namespaces and the OFF build store bytes verbatim.
        let value = crate::compression::compress(collection, value)?;
        let existed = {
            let mut records = self.txn.open_table(RECORDS)?;
            records
                .insert(physical_key(id, key).as_slice(), value.as_ref())?
                .is_some()
        };
        if !existed {
            self.adjust_count(collection, 1)?;
        }
        Ok(())
    }

    /// [`WriteBatch::put`] without the maintained-count adjustment, for
    /// engine-private namespaces whose count is never read (the adjacency
    /// namespaces): skips one META read-modify-write per row, which is the
    /// dominant per-row cost in bulk maintenance. The namespace's `count`
    /// is undefined once this is used — callers must never expose it.
    pub(crate) fn put_uncounted(
        &mut self,
        collection: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let id = self.ensure_id(collection)?;
        let value = crate::compression::compress(collection, value)?;
        let mut records = self.txn.open_table(RECORDS)?;
        records.insert(physical_key(id, key).as_slice(), value.as_ref())?;
        Ok(())
    }

    /// Fetch the value at `key` within `collection`, including writes made
    /// earlier in this transaction.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(None);
        };
        let records = self.txn.open_table(RECORDS)?;
        match records.get(physical_key(id, key).as_slice())? {
            Some(g) => Ok(Some(
                crate::compression::decompress(collection, g.value())?.into_owned(),
            )),
            None => Ok(None),
        }
    }

    /// Remove `key` from `collection`. Returns whether a value was removed.
    pub fn delete(&mut self, collection: &str, key: &[u8]) -> Result<bool> {
        let removed = self.delete_uncounted(collection, key)?;
        if removed {
            self.adjust_count(collection, -1)?;
        }
        Ok(removed)
    }

    /// Remove `key` without touching the maintained count, for callers that
    /// adjust the count themselves in batch ([`clear_in_txn`]) or write
    /// engine-private namespaces whose count is never read (adjacency).
    /// Returns whether a value was removed.
    pub(crate) fn delete_uncounted(&mut self, collection: &str, key: &[u8]) -> Result<bool> {
        // Resolve without creating: deleting from an unknown collection is a no-op.
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(false);
        };
        let mut records = self.txn.open_table(RECORDS)?;
        Ok(records.remove(physical_key(id, key).as_slice())?.is_some())
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
        collect_collection(&records, collection, id)
    }

    /// Return up to `limit` `(key, value)` pairs whose key is `>= start`,
    /// in key order, seeing this transaction's own uncommitted writes.
    /// Mirrors [`Store::scan_from`] for in-transaction consumers (e.g. the
    /// unique-constraint check, which must observe the batch's earlier puts).
    pub fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        let lower = physical_key(id, start);
        let upper = id.checked_add(1).map(|n| n.to_be_bytes().to_vec());
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (
                Bound::Included(lower.as_slice()),
                Bound::Excluded(u.as_slice()),
            ),
            None => (Bound::Included(lower.as_slice()), Bound::Unbounded),
        };
        let mut out = Vec::new();
        for entry in records.range::<&[u8]>(bounds)? {
            if out.len() >= limit {
                break;
            }
            let (k, v) = entry?;
            out.push((
                user_key(k.value()),
                crate::compression::decompress(collection, v.value())?.into_owned(),
            ));
        }
        Ok(out)
    }

    /// Resolve a collection id, assigning a fresh one if the collection is new.
    /// Resolved ids are cached for this transaction's lifetime (see
    /// [`WriteBatch`]).
    fn ensure_id(&self, collection: &str) -> Result<u64> {
        if let Some(id) = self.ids.borrow().get(collection) {
            return Ok(*id);
        }
        let id = {
            let mut catalog = self.txn.open_table(CATALOG)?;
            if let Some(id) = catalog.get(collection)?.map(|g| g.value()) {
                self.cache_id(collection, id);
                return Ok(id);
            }
            let next = {
                let mut meta = self.txn.open_table(META)?;
                let next = meta.get(NEXT_ID)?.map(|g| g.value()).unwrap_or(0);
                meta.insert(NEXT_ID, next + 1)?;
                next
            };
            catalog.insert(collection, next)?;
            next
        };
        self.cache_id(collection, id);
        Ok(id)
    }

    /// Resolve a collection id without creating it. In a write transaction
    /// opening the catalog table always succeeds (it is created on demand),
    /// so an unknown collection simply has no entry. Resolved ids are cached
    /// for this transaction's lifetime.
    fn lookup_id(&self, collection: &str) -> Result<Option<u64>> {
        if let Some(id) = self.ids.borrow().get(collection) {
            return Ok(Some(*id));
        }
        let catalog = self.txn.open_table(CATALOG)?;
        match catalog.get(collection)?.map(|g| g.value()) {
            Some(id) => {
                self.cache_id(collection, id);
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    /// Record a resolved id in the per-transaction cache.
    fn cache_id(&self, collection: &str, id: u64) {
        self.ids.borrow_mut().insert(collection.to_owned(), id);
    }

    /// Atomically reserve the next monotonic auto-key id for `collection`
    /// inside THIS transaction (audit C9): the read-increment-write becomes
    /// visible only when the surrounding transaction commits, so an aborted
    /// insert (schema or unique failure after the reservation) rolls the
    /// counter back instead of burning the id. In-transaction twin of
    /// [`Store::next_auto_id`].
    pub fn next_auto_id(&mut self, collection: &str) -> Result<u64> {
        let mut meta = self.txn.open_table(META)?;
        let key = format!("auto:{collection}");
        let id = meta.get(key.as_str())?.map(|g| g.value()).unwrap_or(0);
        meta.insert(key.as_str(), id + 1)?;
        Ok(id)
    }
}

/// A set of reads executed against one consistent snapshot.
///
/// Obtained via [`Store::read`].
pub struct ReadBatch<'txn> {
    txn: &'txn redb::ReadTransaction,
}

impl ReadBatch<'_> {
    /// Every collection name known to the catalog, in name order, from this
    /// batch's snapshot — so a dump's catalog walk shares its point in time
    /// with the record streams that follow (audit B8). Mirrors
    /// [`Store::collections`].
    pub fn collections(&self) -> Result<Vec<String>> {
        let catalog = match self.txn.open_table(CATALOG) {
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

    /// Every collection's auto-id counter as `(collection, next_id)`, from
    /// this batch's snapshot, for dump/migrate (audit B8: reading counters
    /// in the SAME snapshot as the records keeps a dump from capturing a
    /// counter ahead of the documents it named). Without this, a dump→load
    /// cycle would re-issue used ids.
    pub fn auto_ids(&self) -> Result<Vec<(String, u64)>> {
        let meta = match self.txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in meta.iter()? {
            let (k, v) = entry?;
            if let Some(coll) = k.value().strip_prefix("auto:") {
                out.push((coll.to_owned(), v.value()));
            }
        }
        Ok(out)
    }

    /// Fetch the value at `key` within `collection`, if present.
    pub fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(None);
        };
        let records = self.txn.open_table(RECORDS)?;
        match records.get(physical_key(id, key).as_slice())? {
            Some(g) => Ok(Some(
                crate::compression::decompress(collection, g.value())?.into_owned(),
            )),
            None => Ok(None),
        }
    }

    /// Return all `(key, value)` pairs in `collection`, in key order.
    pub fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        collect_collection(&records, collection, id)
    }

    /// Return up to `limit` `(key, value)` pairs in `collection` whose key is
    /// `>= start`, in key order, from this batch's snapshot. For cursor
    /// pagination: pass an empty `start`, then resume from `last_key` with a
    /// trailing `0` byte appended. An unknown collection yields an empty
    /// vector. Mirrors [`Store::scan_from`] on the shared snapshot.
    pub fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        let lower = physical_key(id, start);
        let upper = id.checked_add(1).map(|n| n.to_be_bytes().to_vec());
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (
                Bound::Included(lower.as_slice()),
                Bound::Excluded(u.as_slice()),
            ),
            None => (Bound::Included(lower.as_slice()), Bound::Unbounded),
        };
        let mut out = Vec::new();
        for entry in records.range::<&[u8]>(bounds)? {
            if out.len() >= limit {
                break;
            }
            let (k, v) = entry?;
            out.push((
                user_key(k.value()),
                crate::compression::decompress(collection, v.value())?.into_owned(),
            ));
        }
        Ok(out)
    }

    /// Return all `(key, value)` pairs in `collection` whose key starts with
    /// `prefix`, in key order, from this batch's snapshot. An unknown
    /// collection yields an empty vector. Mirrors [`Store::scan_prefix`].
    pub fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(Vec::new());
        };
        let records = self.txn.open_table(RECORDS)?;
        let lower = physical_key(id, prefix);
        let upper = next_key(&lower);
        let lower_slice: &[u8] = &lower;
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = match &upper {
            Some(u) => (Bound::Included(lower_slice), Bound::Excluded(u.as_slice())),
            None => (Bound::Included(lower_slice), Bound::Unbounded),
        };
        let mut out = Vec::new();
        for entry in records.range::<&[u8]>(bounds)? {
            let (k, v) = entry?;
            out.push((
                user_key(k.value()),
                crate::compression::decompress(collection, v.value())?.into_owned(),
            ));
        }
        Ok(out)
    }

    /// Stream every `(user_key, value)` in `collection` to `f`, in key order,
    /// from this batch's snapshot, without materializing the collection.
    /// Constant memory regardless of collection size. `f` returns `false` to
    /// stop early. `f`'s slices are valid only for the duration of the call.
    /// Mirrors [`Store::for_each`].
    #[allow(clippy::type_complexity)]
    pub fn for_each(
        &self,
        collection: &str,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let Some(id) = self.lookup_id(collection)? else {
            return Ok(());
        };
        let records = self.txn.open_table(RECORDS)?;
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
            let value = crate::compression::decompress(collection, v.value())?;
            if !f(key.get(8..).unwrap_or(&[]), value.as_ref())? {
                break;
            }
        }
        Ok(())
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

/// The read operations the query layer needs, abstracted over "own
/// transaction per op" ([`Store`]) and "one shared snapshot"
/// ([`ReadBatch`]). Dispatch is via `&dyn`, so an execution path runs
/// unchanged against either backing.
pub(crate) trait SnapshotReader {
    /// Fetch the value at `key` within `collection`, if present.
    fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Return all `(key, value)` pairs in `collection`, in key order.
    fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    /// Every collection name in the catalog, in name order. Whole-database
    /// consumers (dump) walk the catalog on the same snapshot as the records
    /// (audit B8).
    fn collections(&self) -> Result<Vec<String>>;
    /// Return up to `limit` pairs whose key is `>= start`, in key order.
    /// Paged window scans over the index namespaces run on this (audit B3).
    fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    /// Return all pairs whose key starts with `prefix`, in key order.
    /// Prefix scans (postings, edges) run on this (audit B3).
    fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    /// The maintained record count for `collection` (O(1), no scan).
    /// Scoped to the backing: a [`ReadBatch`] reads the counter on the
    /// caller's shared snapshot; the [`Store`] impl opens its own read
    /// transaction per call (its per-op contract). `verify_candidates`
    /// (builder.rs) reads it on the same snapshot as the candidate fetch
    /// as the density signal of its fetch-strategy pick — a heuristic
    /// signal only; both strategies are correct for any count.
    fn count(&self, collection: &str) -> Result<u64>;
    /// Stream every pair in `collection` to `f` in key order; `f` returns
    /// `false` to stop early. Its slices are valid only for the call.
    #[allow(clippy::type_complexity)]
    fn for_each(
        &self,
        collection: &str,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()>;
}

/// Per-op read transactions: byte-identical to calling the [`Store`]
/// methods directly.
impl SnapshotReader for Store {
    fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Store::get(self, collection, key)
    }

    fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Store::scan(self, collection)
    }

    fn collections(&self) -> Result<Vec<String>> {
        Store::collections(self)
    }

    fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Store::scan_from(self, collection, start, limit)
    }

    fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Store::scan_prefix(self, collection, prefix)
    }

    fn count(&self, collection: &str) -> Result<u64> {
        Store::count(self, collection)
    }

    fn for_each(
        &self,
        collection: &str,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        Store::for_each(self, collection, f)
    }
}

/// One shared MVCC snapshot: every op reads the same [`ReadBatch`]
/// transaction.
impl SnapshotReader for ReadBatch<'_> {
    fn get(&self, collection: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        ReadBatch::get(self, collection, key)
    }

    fn scan(&self, collection: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        ReadBatch::scan(self, collection)
    }

    fn collections(&self) -> Result<Vec<String>> {
        ReadBatch::collections(self)
    }

    fn scan_from(
        &self,
        collection: &str,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        ReadBatch::scan_from(self, collection, start, limit)
    }

    fn scan_prefix(&self, collection: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        ReadBatch::scan_prefix(self, collection, prefix)
    }

    /// Mirrors [`Store::count`] on this batch's snapshot (a fresh database
    /// has no META table yet — count 0).
    fn count(&self, collection: &str) -> Result<u64> {
        let meta = match self.txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        Ok(meta
            .get(count_key(collection).as_str())?
            .map(|g| g.value())
            .unwrap_or(0))
    }

    fn for_each(
        &self,
        collection: &str,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        ReadBatch::for_each(self, collection, f)
    }
}

/// Collect every record belonging to collection `id` from an open records
/// table, stripping the id prefix back to user keys.
fn collect_collection<T>(records: &T, collection: &str, id: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>>
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
        out.push((
            user_key(k.value()),
            crate::compression::decompress(collection, v.value())?.into_owned(),
        ));
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
    fn backup_copies_all_data_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let bak_path = dir.path().join("backup.db");
        {
            let s = Store::open(&src_path).unwrap();
            for i in 0..50u8 {
                s.put("docs", &[i], &[i, i]).unwrap();
            }
            s.put("other", b"x", b"y").unwrap();
            s.backup(&bak_path).unwrap();
        }
        // The backup is an independent, complete database.
        let b = Store::open(&bak_path).unwrap();
        assert_eq!(b.count("docs").unwrap(), 50);
        assert_eq!(b.get("docs", &[7]).unwrap(), Some(vec![7, 7]));
        assert_eq!(b.get("other", b"x").unwrap(), Some(b"y".to_vec()));
        let cols = b.collections().unwrap();
        assert!(cols.contains(&"docs".to_owned()) && cols.contains(&"other".to_owned()));
    }

    #[test]
    fn backup_of_empty_store_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("empty.db");
        mem().backup(&bak).unwrap();
        let b = Store::open(&bak).unwrap();
        assert_eq!(b.get("docs", b"k").unwrap(), None);
    }

    #[test]
    fn backup_refuses_existing_target_instead_of_merging() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let bak = dir.path().join("bak.db");
        {
            let s = Store::open(&src_path).unwrap();
            s.put("docs", b"a", b"v1").unwrap();
            s.backup(&bak).unwrap();
            // Delete after the first backup; a merged second copy would
            // resurrect the record.
            s.delete("docs", b"a").unwrap();
        }
        let s = Store::open(&src_path).unwrap();
        assert!(matches!(
            s.backup(&bak),
            Err(crate::Error::BackupTargetExists(_))
        ));
        // The old backup is untouched by the refused attempt.
        let b = Store::open(&bak).unwrap();
        assert_eq!(b.get("docs", b"a").unwrap(), Some(b"v1".to_vec()));
    }

    /// Audit C8: a backup that fails anywhere after the exists() check must
    /// leave no partial destination behind — debris would both masquerade as
    /// a valid backup and block every future attempt (the exists() refusal).
    /// Failure is forced by making the target's parent directory read-only.
    #[test]
    #[cfg(unix)]
    fn failed_backup_leaves_no_debris() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let s = Store::open(&src_path).unwrap();
        s.put("docs", b"a", b"v1").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let target = sub.join("backup.db");

        let mut perms = std::fs::metadata(&sub).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&sub, perms).unwrap();
        let result = s.backup(&target);
        // Restore writability before asserting so tempdir cleanup always works.
        let mut perms = std::fs::metadata(&sub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&sub, perms).unwrap();

        assert!(result.is_err(), "backup into a read-only parent must fail");
        assert!(
            !target.exists(),
            "a failed backup must not leave a partial destination behind"
        );
    }

    #[test]
    fn backup_is_consistent_under_concurrent_writes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let s = Arc::new(Store::open(&src_path).unwrap());
        for i in 0..100u32 {
            s.put("docs", &i.to_be_bytes(), b"v").unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let s = Arc::clone(&s);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut i = 100u32;
                while !stop.load(Ordering::Relaxed) {
                    s.put("docs", &i.to_be_bytes(), b"v").unwrap();
                    i += 1;
                }
            })
        };
        let bak = dir.path().join("snap.db");
        s.backup(&bak).unwrap();
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // The snapshot is internally consistent: every record it contains is a
        // real committed record, and the maintained count matches the data.
        let b = Store::open(&bak).unwrap();
        let mut n = 0u64;
        b.for_each("docs", |_, v| {
            assert_eq!(v, b"v");
            n += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(n, b.count("docs").unwrap());
        assert!(n >= 100, "backup must include all pre-existing records");
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

    /// Regression (audit B1): relaxed durability leaked to threads not
    /// inside the bulk scope, and a panicking bulk closure left it on
    /// forever. Bulk scope is thread-local and panic-safe (RAII).
    #[test]
    fn bulk_scope_relaxes_only_the_bulk_thread() {
        use std::sync::Arc;
        let s = Arc::new(mem());
        let _scope = s.begin_bulk();
        assert!(s.bulk_active_on_this_thread());
        let other = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.bulk_active_on_this_thread())
                .join()
                .unwrap()
        };
        assert!(!other, "concurrent writer must keep durable commits");
    }

    #[test]
    fn panicking_bulk_closure_restores_durability() {
        let s = mem();
        let boomed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = s.begin_bulk();
            panic!("bulk closure failed mid-load");
        }));
        assert!(boomed.is_err());
        assert!(!s.bulk_active_on_this_thread());
        // And a subsequent ordinary transaction is durable again (flag-wise).
        s.put("docs", b"k", b"v").unwrap();
        assert!(!s.bulk_active_on_this_thread());
    }

    /// clear_in_txn empties a collection inside the caller's transaction, so
    /// the removal commits atomically with whatever else the batch writes.
    #[test]
    fn clear_in_txn_empties_collection_atomically() {
        let s = mem();
        s.put("docs", b"a", b"1").unwrap();
        s.put("docs", b"b", b"2").unwrap();
        s.put("other", b"x", b"keep").unwrap();
        s.transaction(|tx| {
            clear_in_txn(tx, "docs")?;
            tx.put("docs", b"fresh", b"v")
        })
        .unwrap();
        assert_eq!(
            s.scan("docs").unwrap(),
            vec![(b"fresh".to_vec(), b"v".to_vec())]
        );
        assert_eq!(s.count("docs").unwrap(), 1);
        // A sibling collection is untouched.
        assert_eq!(s.get("other", b"x").unwrap(), Some(b"keep".to_vec()));
    }

    /// The clear pages through large collections (more than one page) and
    /// leaves nothing behind, including keys sorting past the page boundary.
    /// The maintained count must land exactly on 0 — the per-PAGE batched
    /// adjustment (delta = page length) replaces the per-key one — and stay
    /// exact for writes after the clear.
    #[test]
    fn clear_in_txn_pages_large_collections() {
        let s = mem();
        s.transaction(|tx| {
            for i in 0..5000u32 {
                tx.put("docs", &i.to_be_bytes(), &[i as u8])?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(s.count("docs").unwrap(), 5000);
        s.transaction(|tx| clear_in_txn(tx, "docs")).unwrap();
        assert!(s.scan("docs").unwrap().is_empty());
        assert_eq!(s.count("docs").unwrap(), 0);
        // The count stays honest for writes after a batched clear.
        s.put("docs", b"x", b"1").unwrap();
        s.put("docs", b"y", b"2").unwrap();
        s.put("docs", b"z", b"3").unwrap();
        assert_eq!(s.count("docs").unwrap(), 3);
    }

    /// Clearing an unknown or empty collection is a no-op, so registering an
    /// index over a fresh namespace costs one empty scan.
    #[test]
    fn clear_in_txn_missing_collection_is_noop() {
        let s = mem();
        s.transaction(|tx| clear_in_txn(tx, "ghost")).unwrap();
        assert_eq!(s.count("ghost").unwrap(), 0);
    }

    /// Every read op on `ReadBatch` behaves exactly like the `Store` version,
    /// key for key — including resume-with-appended-0x00 paging, the
    /// all-0xFF prefix edge, `for_each` early stop, and the
    /// unknown-collection-empty contract. Both sides go through
    /// `&dyn SnapshotReader`, which also pins object safety and both impls.
    #[test]
    fn read_batch_ops_match_store_versions() {
        let s = mem();
        s.put("docs", b"a1", b"v1").unwrap();
        s.put("docs", b"a2", b"v2").unwrap();
        s.put("docs", b"b1", b"v3").unwrap();
        s.put("docs", &[0xff, 0x00], b"v4").unwrap();
        s.put("docs", &[0xff, 0xff], b"v5").unwrap();
        s.put("other", b"a1", b"other").unwrap();

        // Store-side expectations, dispatched through the trait.
        let store: &dyn SnapshotReader = &s;
        let page1 = store.scan_from("docs", b"", 2).unwrap();
        let mut resume = page1.last().unwrap().0.clone();
        resume.push(0);
        let page2 = store.scan_from("docs", &resume, 2).unwrap();
        let prefix = store.scan_prefix("docs", &[0xff]).unwrap();
        let prefix_ff = store.scan_prefix("docs", &[0xff, 0xff]).unwrap();
        let mut all = Vec::new();
        store
            .for_each("docs", &mut |k, v| {
                all.push((k.to_vec(), v.to_vec()));
                Ok(true)
            })
            .unwrap();
        let mut stopped = Vec::new();
        store
            .for_each("docs", &mut |k, v| {
                stopped.push((k.to_vec(), v.to_vec()));
                Ok(stopped.len() < 2)
            })
            .unwrap();
        let ghost_from = store.scan_from("ghost", b"", 5).unwrap();
        let ghost_prefix = store.scan_prefix("ghost", b"x").unwrap();
        let mut ghost_calls = 0usize;
        store
            .for_each("ghost", &mut |_, _| {
                ghost_calls += 1;
                Ok(true)
            })
            .unwrap();

        assert_eq!(page1.len(), 2);
        assert_eq!(stopped.len(), 2, "early stop must halt at two entries");
        assert_eq!(stopped, all[..2]);
        assert!(ghost_from.is_empty() && ghost_prefix.is_empty() && ghost_calls == 0);

        // The same ops inside ONE read closure must match key for key.
        s.read(|r| {
            let rb: &dyn SnapshotReader = r;
            assert_eq!(rb.get("docs", b"a1").unwrap(), Some(b"v1".to_vec()));
            assert_eq!(
                rb.scan("other").unwrap(),
                vec![(b"a1".to_vec(), b"other".to_vec())]
            );

            assert_eq!(rb.scan_from("docs", b"", 2).unwrap(), page1);
            assert_eq!(rb.scan_from("docs", &resume, 2).unwrap(), page2);
            // Resume paging (last key + trailing 0x00) walks the whole
            // collection exactly once, in order.
            let mut start = Vec::new();
            let mut paged = Vec::new();
            loop {
                let page = rb.scan_from("docs", &start, 2).unwrap();
                let Some((last, _)) = page.last().cloned() else {
                    break;
                };
                paged.extend(page);
                start = last;
                start.push(0);
            }
            assert_eq!(paged, all);

            assert_eq!(rb.scan_prefix("docs", &[0xff]).unwrap(), prefix);
            assert_eq!(rb.scan_prefix("docs", &[0xff, 0xff]).unwrap(), prefix_ff);

            let mut rb_all = Vec::new();
            rb.for_each("docs", &mut |k, v| {
                rb_all.push((k.to_vec(), v.to_vec()));
                Ok(true)
            })
            .unwrap();
            assert_eq!(rb_all, all);
            let mut rb_stopped = Vec::new();
            rb.for_each("docs", &mut |k, v| {
                rb_stopped.push((k.to_vec(), v.to_vec()));
                Ok(rb_stopped.len() < 2)
            })
            .unwrap();
            assert_eq!(rb_stopped, stopped);

            assert_eq!(rb.scan_from("ghost", b"", 5).unwrap(), ghost_from);
            assert_eq!(rb.scan_prefix("ghost", b"x").unwrap(), ghost_prefix);
            let mut rb_ghost_calls = 0usize;
            rb.for_each("ghost", &mut |_, _| {
                rb_ghost_calls += 1;
                Ok(true)
            })
            .unwrap();
            assert_eq!(rb_ghost_calls, ghost_calls);
            Ok(())
        })
        .unwrap();
    }

    /// Task 5 (feature `zstd`) physical-row pins: what actually lands in
    /// the RECORDS table. These read the raw redb row directly (bypassing
    /// every decompressing helper) through the store's own fields, so they
    /// pin the ON-DISK form, not just the round-trip.
    mod compression_layout {
        use super::super::*;
        use super::mem;

        /// Repetitive text: compresses well at zstd level 3.
        fn blob(n: usize) -> Vec<u8> {
            let base = b"the quick brown fox jumps over the lazy dog; ";
            (0..n).map(|i| base[i % base.len()]).collect()
        }

        /// Read the physical stored row for (collection, key) straight from
        /// the RECORDS table — no compression helpers in the path.
        fn raw_row(s: &Store, collection: &str, key: &[u8]) -> Vec<u8> {
            let txn = s.db.begin_read().unwrap();
            let id = {
                let catalog = txn.open_table(CATALOG).unwrap();
                catalog.get(collection).unwrap().unwrap().value()
            };
            let records = txn.open_table(RECORDS).unwrap();
            records
                .get(physical_key(id, key).as_slice())
                .unwrap()
                .unwrap()
                .value()
                .to_vec()
        }

        /// Insert `value` at (collection, key) as a LEGACY row — written
        /// straight into RECORDS, exactly as a pre-feature (OFF) binary
        /// would have — leaving the count untouched.
        #[cfg(feature = "zstd")]
        fn raw_put(s: &Store, collection: &str, key: &[u8], value: &[u8]) {
            s.put(collection, b"seed", b"").unwrap(); // ensure the catalog entry
            let txn = s.db.begin_write().unwrap();
            {
                let id = {
                    let catalog = txn.open_table(CATALOG).unwrap();
                    catalog.get(collection).unwrap().unwrap().value()
                };
                let mut records = txn.open_table(RECORDS).unwrap();
                records
                    .insert(physical_key(id, key).as_slice(), value)
                    .unwrap();
            }
            txn.commit().unwrap();
        }

        /// Every Store read path agrees on a compressible above-threshold
        /// value: point get, full scan, window scan, prefix scan, stream.
        #[test]
        fn roundtrips_through_every_read_path() {
            let s = mem();
            let big = blob(8192);
            s.put("docs", b"k", &big).unwrap();
            assert_eq!(s.get("docs", b"k").unwrap(), Some(big.clone()));
            assert_eq!(s.scan("docs").unwrap(), vec![(b"k".to_vec(), big.clone())]);
            assert_eq!(
                s.scan_from("docs", b"", 10).unwrap(),
                vec![(b"k".to_vec(), big.clone())]
            );
            assert_eq!(
                s.scan_prefix("docs", b"k").unwrap(),
                vec![(b"k".to_vec(), big.clone())]
            );
            let mut streamed = Vec::new();
            s.for_each("docs", |k, v| {
                streamed.push((k.to_vec(), v.to_vec()));
                Ok(true)
            })
            .unwrap();
            assert_eq!(streamed, vec![(b"k".to_vec(), big)]);
        }

        /// With the feature ON, a user-collection value at/above the
        /// threshold is stored marker-prefixed and strictly smaller; the
        /// same value in an engine-reserved namespace is stored verbatim.
        #[cfg(feature = "zstd")]
        #[test]
        fn user_rows_store_marked_reserved_rows_store_raw() {
            let s = mem();
            let big = blob(4096);
            s.put("docs", b"k", &big).unwrap();
            let row = raw_row(&s, "docs", b"k");
            assert_eq!(row[0], crate::compression::MARKER);
            assert!(row.len() < big.len(), "must shrink, got {}", row.len());

            s.put("__edges__docs", b"k", &big).unwrap();
            assert_eq!(raw_row(&s, "__edges__docs", b"k"), big);
        }

        /// With the feature OFF, stored bytes are byte-identical to the
        /// written value regardless of size or namespace — today's exact
        /// behavior, pinned.
        #[cfg(not(feature = "zstd"))]
        #[test]
        fn off_build_stores_values_verbatim() {
            let s = mem();
            let big = blob(4096);
            s.put("docs", b"k", &big).unwrap();
            assert_eq!(raw_row(&s, "docs", b"k"), big);
        }

        /// Legacy rows (written raw by an OFF binary, above threshold and
        /// starting with a value-codec tag) read fine under an ON binary:
        /// only marker-prefixed rows are ever interpreted as frames.
        #[cfg(feature = "zstd")]
        #[test]
        fn legacy_raw_rows_read_fine_under_the_feature() {
            let s = mem();
            // A Value encoding far above the threshold: TAG_TEXT + len + text.
            let text = String::from_utf8(blob(8192)).unwrap();
            let encoded = crate::value::Value::Text(text).encode();
            assert!(encoded.len() > crate::compression::THRESHOLD);
            raw_put(&s, "docs", b"legacy", &encoded);
            // Reads back verbatim (no marker → identity) and decodes.
            assert_eq!(s.get("docs", b"legacy").unwrap(), Some(encoded));
            // And a fresh engine-level write of the same value compresses,
            // reading back equal through the normal path.
            let text2 = String::from_utf8(blob(8192)).unwrap();
            let v = crate::value::Value::Text(text2);
            s.put("docs", b"fresh", &v.encode()).unwrap();
            assert_eq!(s.get("docs", b"fresh").unwrap(), Some(v.encode()));
        }
    }

    /// The pin the whole B3 wave rests on: one `ReadBatch` is one MVCC
    /// snapshot. A writer thread commits doc2 after the reader's first `get`
    /// inside the `read(|r| ...)` closure (synchronized via channels, so the
    /// commit happens-before the reader continues); the same batch's
    /// subsequent reads never see doc2, while a fresh read afterwards does.
    #[test]
    fn read_batch_is_an_mvcc_snapshot() {
        use std::sync::Arc;
        use std::sync::mpsc;

        let s = Arc::new(mem());
        s.put("docs", b"doc1", b"v1").unwrap();

        let (reader_pinned, writer_go) = mpsc::channel::<()>();
        let (writer_done, writer_committed) = mpsc::channel::<()>();
        let writer = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                writer_go.recv().expect("reader is alive");
                s.put("docs", b"doc2", b"v2").unwrap(); // commits
                writer_done.send(()).expect("reader is alive");
            })
        };

        s.read(|r| {
            // First read in this snapshot.
            assert_eq!(r.get("docs", b"doc1").unwrap(), Some(b"v1".to_vec()));
            reader_pinned.send(()).unwrap();
            // Channel happens-before: doc2 is committed by this point.
            writer_committed.recv().unwrap();

            assert_eq!(
                r.get("docs", b"doc2").unwrap(),
                None,
                "same ReadBatch must not see a commit made after its first read"
            );
            assert_eq!(r.scan("docs").unwrap().len(), 1);
            assert_eq!(r.scan_from("docs", b"", 10).unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
        writer.join().unwrap();

        // Outside the closure, a fresh read transaction sees the commit.
        assert_eq!(s.get("docs", b"doc2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(s.scan("docs").unwrap().len(), 2);
    }

    /// Task 12's core pin: the CHUNKED `scan_from` walk that
    /// `Collection::page`/`page_where` runs inside one `read()` closure sees
    /// NO mid-walk mutation, so the page is one point-in-time view.
    ///
    /// Reasoning chain (why this lives at the ReadBatch level):
    /// `page()` is synchronous and hook-free, so a write cannot be
    /// deterministically interleaved between its chunk reads from outside —
    /// the brief's thread/channel shapes would need sleeps. Instead `page`
    /// now delegates its whole walk to `store().read(|r| ...)` (the same
    /// discipline as the query builder's `run()` → `run_with(reader)`), and
    /// THIS test pins the semantics that delegation inherits: writes
    /// committed BETWEEN two chunk reads of the same `ReadBatch` — insert
    /// ahead of the cursor, delete ahead, delete behind, overwrite ahead —
    /// are invisible to the walk's remainder, and the concatenated chunks
    /// equal the original corpus exactly. `read_batch_is_an_mvcc_snapshot`
    /// pins the same for `get`/`scan`; this pins it for the paged-walk
    /// shape. Static equivalence of `ReadBatch::scan_from` and
    /// `Store::scan_from` is pinned by `read_batch_ops_match_store_versions`
    /// (so page results on a static db are unchanged), and the page cursor
    /// contract by the queries.rs suite.
    ///
    /// The mutations run on the SAME thread via the same handle while the
    /// read transaction is open — deterministic ordering, no channels, no
    /// sleeps — and their commits succeeding at all is itself the
    /// "snapshot-holding cost" claim made real: redb is MVCC, so an open
    /// read transaction pins old pages (space, reclaimed once it ends) but
    /// never blocks the writer.
    #[test]
    fn chunked_read_batch_walk_ignores_mid_walk_writes() {
        let s = mem();
        // 2500 fixed-width keys = 3 chunks at the page walk's 1024-key step.
        s.transaction(|tx| {
            for i in 0..2500u32 {
                let key = format!("k{i:04}");
                tx.put("docs", key.as_bytes(), format!("v{i}").as_bytes())?;
            }
            Ok(())
        })
        .unwrap();
        let original = s.scan("docs").unwrap();
        assert_eq!(original.len(), 2500);

        let walked = s
            .read(|r| {
                let mut start = Vec::new();
                let mut out = Vec::new();
                let mut chunks = 0;
                loop {
                    let page = r.scan_from("docs", &start, 1024)?;
                    let Some((last, _)) = page.last().cloned() else {
                        break;
                    };
                    out.extend(page);
                    chunks += 1;
                    if chunks == 1 {
                        // Between the first and second chunk read — exactly
                        // where a per-chunk-transaction walk would pick the
                        // writer's commits up — commit every mutation shape:
                        // a key ahead of the cursor (insert), one deleted
                        // ahead, one overwritten ahead, and one deleted
                        // behind (already emitted).
                        s.put("docs", b"k2100zz", b"inserted-mid-walk").unwrap();
                        s.delete("docs", b"k2000").unwrap();
                        s.put("docs", b"k1800", b"overwritten-mid-walk").unwrap();
                        s.delete("docs", b"k0001").unwrap();
                        // The writes really committed while this read
                        // transaction stays open (fresh per-op reads see
                        // them; the shared-snapshot walk below must not).
                        assert_eq!(
                            s.get("docs", b"k2100zz").unwrap(),
                            Some(b"inserted-mid-walk".to_vec())
                        );
                        assert_eq!(s.get("docs", b"k2000").unwrap(), None);
                    }
                    start = last;
                    start.push(0);
                }
                assert!(chunks >= 3, "walk must cross chunk boundaries");
                assert_eq!(out, original, "the whole walk is the opening snapshot");
                Ok(out)
            })
            .unwrap();

        // The walk returned the ORIGINAL view; the commits are durable facts
        // a fresh read transaction observes (not vacuously invisible).
        assert_eq!(walked, original);
        let after = s.scan("docs").unwrap();
        assert_eq!(after.len(), 2499, "2 deletes + 1 insert: 2500 − 1 net");
        assert_eq!(
            s.get("docs", b"k2100zz").unwrap(),
            Some(b"inserted-mid-walk".to_vec())
        );
        assert_eq!(s.get("docs", b"k2000").unwrap(), None);
        assert_eq!(s.get("docs", b"k0001").unwrap(), None);
        assert_eq!(
            s.get("docs", b"k1800").unwrap(),
            Some(b"overwritten-mid-walk".to_vec())
        );

        // The contrast that makes this pin load-bearing: the SAME mutation
        // timing against a per-chunk-own-transaction walk (`Store::scan_from`
        // — page's pre-Task-12 shape) produces the mixed state this task
        // removes: chunk 2 onwards observe the mid-walk writes, so the
        // concatenated walk is a state that never existed as a commit.
        let before = s.scan("docs").unwrap();
        let mut start = Vec::new();
        let mut mixed = Vec::new();
        let mut chunks = 0;
        loop {
            let page = s.scan_from("docs", &start, 1024).unwrap();
            let Some((last, _)) = page.last().cloned() else {
                break;
            };
            mixed.extend(page);
            chunks += 1;
            if chunks == 1 {
                s.put("docs", b"k2200zz", b"second-mid-walk").unwrap();
                s.delete("docs", b"k1500").unwrap();
            }
            start = last;
            start.push(0);
        }
        assert!(chunks >= 3);
        // Length happens to be preserved (one insert, one delete); the
        // CONTENT is the mixed state: a row that did not exist when the
        // walk started, and a hole where one did.
        assert!(mixed.iter().any(|(k, _)| k == b"k2200zz"));
        assert!(!mixed.iter().any(|(k, _)| k == b"k1500"));
        assert_ne!(
            mixed, before,
            "per-chunk-transaction walks are NOT one snapshot"
        );
    }
}
