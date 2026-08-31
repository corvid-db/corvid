//! Opaque-handle infrastructure (spec §1.1, §2) and the FFI-owned
//! derived-handle counter (spec §4.13/§6).
//!
//! Every ABI handle is an opaque, single-pointer-sized, zero-sized
//! `#[repr(C)]` marker type (`corvid_db`, `corvid_strs`, …). The real
//! state lives in interior Rust structs (`DbHandle`, `StrsHandle`, …)
//! that never cross the boundary by value: constructors hand out
//! `Box::into_raw(body) as *mut Marker`, accessors borrow the body back,
//! and each family's `_free` runs `Box::from_raw` on the original body
//! pointer. C sees only the forward declaration in `corvid.h`;
//! cross-family frees are UB by contract (spec §2) because the marker
//! cast is only valid for the owning family.
//!
//! The families that depart from the wrapper-struct shape are the value
//! and predicate families (spec §4.3–§4.5): a `corvid_value*` and a
//! `corvid_pred*` are pointers at the bare engine types (`Value`,
//! `filter::Predicate`), boxed directly (`into_value` / `into_pred`),
//! because neither carries cursor or counter state. The value family
//! additionally has the ABI's second handle provenance — borrowed
//! children — documented with its plumbing below.
//!
//! # The derived-handle counter
//!
//! `corvid_compact` needs exclusive engine access (`Db::compact` takes
//! `&mut self`), so spec §4.13 checks quiescence with an FFI-owned
//! `AtomicUsize` on the db: it counts live handles holding a **clone of
//! the engine `Arc`** — initialized to 1 (the db handle itself) at open,
//! incremented when a derived engine handle is created, decremented by
//! that handle's `_free`; `corvid_compact` requires the count at exactly
//! 1 **and** sole ownership of the engine `Arc`
//! (`DbHandle::compact`), and otherwise fails with the FFI-only
//! `CORVID_E_BUSY`. The count alone is NOT engine-idle:
//! `QueryHandle::execute` releases its count at entry while its `Arc`
//! clone lives through the engine call, so `Arc` exclusivity is the
//! second half of the gate (the Task 5 review prepend). The count is
//! deterministic because this layer is the only `Arc` cloner — bindings
//! never see the `Arc`.
//!
//! **Wiring:** the increment lives at each derived-handle constructor —
//! collection handles (`corvid_collection`) and query handles
//! (`corvid_query_new`) are wired. The decrement lives at each handle's
//! single consumption point — a `_free`, or (for queries) the executing
//! call that consumes the handle per spec §5 rule 5 — which may run
//! AFTER `corvid_close` has already dropped the `DbHandle` box (spec
//! §2: a derived handle legitimately outlives its db handle) — the
//! counter is therefore an `Arc<AtomicUsize>` shared with every derived
//! handle, not a field the box owns alone. Cursors that own only
//! materialized data and hold no engine reference (`rows`, `strs`,
//! `geohits`, `groupiter`, `schemaiter` — the spec §2 backing table) do
//! **not** increment: they cannot keep the engine working and so cannot
//! block exclusive compaction.

use std::ffi::c_char;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use corvid::Db;
use corvid::Metric;
use corvid::ResultRow;
use corvid::filter::Predicate;

/// The opaque `corvid_db*` handle type (spec §1.1). Zero-sized and never
/// constructed — a pointer to it is a typed alias for the interior
/// `DbHandle` box (see the module docs for the provenance contract).
#[repr(C)]
pub struct corvid_db {
    _unused: [u8; 0],
}

/// The opaque `corvid_coll*` handle type (spec §1.1). Backed by
/// `CollHandle`.
#[repr(C)]
pub struct corvid_coll {
    _unused: [u8; 0],
}

/// The opaque `corvid_strs*` handle type (spec §1.1). Backed by
/// `StrsHandle`.
#[repr(C)]
pub struct corvid_strs {
    _unused: [u8; 0],
}

/// The opaque `corvid_geohits*` handle type (spec §1.1). Backed by
/// `GeoHitsHandle`.
#[repr(C)]
pub struct corvid_geohits {
    _unused: [u8; 0],
}

/// The opaque `corvid_value*` handle type (spec §1.1). Backed by a bare
/// boxed `corvid::Value` (`into_value`) — see the module docs for why
/// this family skips a wrapper struct, and the value plumbing below for
/// the owned-vs-borrowed provenance split.
#[repr(C)]
pub struct corvid_value {
    _unused: [u8; 0],
}

/// The opaque `corvid_pred*` handle type (spec §1.1). Backed by a bare
/// boxed `corvid::filter::Predicate` (`into_pred`) — like the value
/// family, a predicate carries no cursor or counter state, so it skips a
/// wrapper struct and boxes the engine type directly.
#[repr(C)]
pub struct corvid_pred {
    _unused: [u8; 0],
}

/// The opaque `corvid_rows*` handle type (spec §1.1). Backed by
/// `RowsHandle` — materialized rows plus a cursor, no engine reference.
#[repr(C)]
pub struct corvid_rows {
    _unused: [u8; 0],
}

/// The opaque `corvid_query*` handle type (spec §1.1). Backed by
/// `QueryHandle` — owned QueryBuilder state (an engine `Arc` + name +
/// filters + sources + knobs, per the spec §2 backing table).
#[repr(C)]
pub struct corvid_query {
    _unused: [u8; 0],
}

/// The opaque `corvid_groupiter*` handle type (spec §1.1). Backed by
/// `GroupIterHandle` — the materialized group list plus a cursor, no
/// engine reference.
#[repr(C)]
pub struct corvid_groupiter {
    _unused: [u8; 0],
}

/// The opaque `corvid_schemaiter*` handle type (spec §1.1). Backed by
/// `SchemaIterHandle` — the materialized field list plus a cursor, no
/// engine reference.
#[repr(C)]
pub struct corvid_schemaiter {
    _unused: [u8; 0],
}

/// Interior state behind a `corvid_db*`: the engine handle (shared with
/// derived handles as `Arc` clones) and the derived-handle counter.
pub(crate) struct DbHandle {
    db: Arc<Db>,
    /// Live handles holding a clone of `db`, this handle included — see
    /// the module docs. An `Arc` (not an inline field) because a derived
    /// handle's `_free` decrements it after `corvid_close` may have
    /// dropped this box: the counter must outlive every handle that can
    /// touch it. `Relaxed` orderings would suffice (the count only
    /// gates a quiescence check, not data publication), but the
    /// acquire/release pair matches §6's cross-thread contract in the
    /// most conservative reading.
    derived: Arc<AtomicUsize>,
}

impl DbHandle {
    /// Wrap a freshly opened engine `Db`. The counter starts at 1: the db
    /// handle itself (spec §4.13's "exactly 1" for compaction).
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db: Arc::new(db),
            derived: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// The engine behind the handle, borrowed for an in-place call.
    pub(crate) fn engine(&self) -> &Db {
        &self.db
    }

    /// A clone of the engine `Arc`, for a derived handle to hold (spec
    /// §2: a `corvid_coll` keeps its `corvid_db` alive).
    pub(crate) fn db(&self) -> Arc<Db> {
        self.db.clone()
    }

    /// The derived-handle counter, for a derived handle's `_free` (which
    /// may outlive this box — see the field docs).
    pub(crate) fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.derived)
    }

    /// Count a newly created derived engine handle (module docs). Wired
    /// at `corvid_collection` and `corvid_query_new`.
    pub(crate) fn retain_derived(&self) {
        self.derived.fetch_add(1, Ordering::Release);
    }

    /// `corvid_compact`'s quiescence check, first half: exactly the db
    /// handle itself is live (spec §4.13). (pub(crate): the collection
    /// and query tests pin the counter arithmetic through it.)
    pub(crate) fn is_exclusive(&self) -> bool {
        self.derived.load(Ordering::Acquire) == 1
    }

    /// `corvid_compact`'s engine call (spec §4.13): the exclusive
    /// `Db::compact(&mut self)` under a TWO-part gate —
    ///
    /// 1. the derived-handle counter at exactly 1 (only this handle
    ///    holds a count), and
    /// 2. sole ownership of the engine `Arc` (`Arc::get_mut` succeeds).
    ///
    /// Counter alone is NOT engine-idle: [`QueryHandle::execute`]
    /// releases its count at entry while its `Arc` clone lives through
    /// the engine call, so a query in flight on another thread can show
    /// count 1 with a live clone — the `Arc` check catches it (the Task
    /// 5 review prepend; `Arc::get_mut` over `try_unwrap` keeps the `Arc`
    /// in place, no take-out-and-rewrap dance around the call). Either
    /// half failing records the FFI-only `CORVID_E_BUSY` and answers
    /// `None`; success runs the engine compact under the panic guard and
    /// returns whether any data moved.
    pub(crate) fn compact(&mut self) -> Option<bool> {
        if !self.is_exclusive() {
            crate::error::record(
                crate::error::corvid_err::CORVID_E_BUSY,
                "corvid_compact: derived handles are still open — free \
                 every collection/query handle first (spec §4.13)",
            );
            return None;
        }
        // SAFETY-free Arc::get_mut: Some only when the strong (and weak)
        // count is exactly 1 — this handle's own reference. The engine
        // call takes &mut through it and the Arc stays put afterwards.
        let Some(engine) = Arc::get_mut(&mut self.db) else {
            crate::error::record(
                crate::error::corvid_err::CORVID_E_BUSY,
                "corvid_compact: an engine reference outlives its \
                 derived-handle count (a query in flight releases its \
                 count at execute() entry but holds its Arc clone through \
                 the call) — retry at quiescence (spec §4.13)",
            );
            return None;
        };
        crate::error::guard("corvid_compact", || engine.compact())
    }
}

/// Interior state behind a `corvid_coll*` (spec §2: "`Arc<Db>` +
/// collection name", thread-safe): the shared engine, the stored name,
/// and the counter clone its `_free` releases.
pub(crate) struct CollHandle {
    db: Arc<Db>,
    /// The name exactly as given — engine `Db::collection` validates
    /// nothing (lazily created on first write), so this may hold a
    /// would-be-invalid name that fails at write time (spec §4.2).
    name: String,
    /// `name`'s bytes plus one trailing NUL, for
    /// `corvid_collection_name`'s NUL-terminated borrowed view (an
    /// interior-NUL name truncates the C view; `*len_out` carries the
    /// exact byte length — spec §4.2).
    name_z: Vec<u8>,
    /// The db's derived-handle counter clone — released exactly once by
    /// `corvid_collection_free`, whenever that runs.
    derived: Arc<AtomicUsize>,
}

impl CollHandle {
    /// Wrap a derived engine reference. The caller owns the matching
    /// `retain_derived` on the db handle (spec §4.13 — the count and the
    /// handle are born together in `corvid_collection`).
    pub(crate) fn new(db: Arc<Db>, name: String, derived: Arc<AtomicUsize>) -> Self {
        let mut name_z = Vec::with_capacity(name.len() + 1);
        name_z.extend_from_slice(name.as_bytes());
        name_z.push(0);
        Self {
            db,
            name,
            name_z,
            derived,
        }
    }

    /// The engine collection handle (a cheap copyable borrow — db.rs
    /// `Collection`), for one call.
    pub(crate) fn collection(&self) -> corvid::Collection<'_> {
        self.db.collection(&self.name)
    }

    /// The stored name, for `*len_out`.
    pub(crate) fn name_len(&self) -> usize {
        self.name.len()
    }

    /// The NUL-terminated name view, for the borrowed return (valid
    /// until `corvid_collection_free` — the buffer never moves after
    /// construction).
    pub(crate) fn name_ptr(&self) -> *const c_char {
        self.name_z.as_ptr() as *const c_char
    }

    /// The release half of the derived-handle count (spec §4.13) — the
    /// single decrement this handle owes, run by
    /// `corvid_collection_free`.
    pub(crate) fn release_derived(&self) {
        self.derived.fetch_sub(1, Ordering::Release);
    }

    /// Birth a query handle over this collection (the `corvid_query_new`
    /// body): increments the shared derived count BEFORE the handle
    /// exists (the count and the handle are born together, spec §4.13 —
    /// the same discipline as `corvid_collection`) and returns the
    /// handle holding this coll's engine `Arc`, name, and counter
    /// clone.
    pub(crate) fn spawn_query(&self) -> QueryHandle {
        self.derived.fetch_add(1, Ordering::Release);
        QueryHandle::new(
            self.db.clone(),
            self.name.clone(),
            Arc::clone(&self.derived),
        )
    }
}

/// One retrieval source accumulated on a query handle — the FFI-side
/// mirror of the engine builder's private `Source` enum (builder.rs):
/// the parts must be owned because a `corvid_query*` outlives the call
/// that adds them, and the engine's variant is not `pub`.
#[derive(Debug)]
pub(crate) enum Source {
    /// A vector-search source: `corvid_query_vector`.
    Vector {
        field: String,
        query: Vec<f32>,
        k: usize,
        metric: Metric,
    },
    /// A BM25 text-search source: `corvid_query_text`.
    Text {
        field: String,
        query: String,
        k: usize,
    },
}

/// Interior state behind a `corvid_query*` (spec §2: "owned
/// QueryBuilder state (`Arc<Db>` + name + filters + sources + knobs)",
/// single-threaded build).
///
/// **Why parts, not a live `QueryBuilder`:** the engine's
/// `QueryBuilder<'c>` BORROWS a `Collection<'c>` (`{ db: &'c Db, name
/// }`), and an opaque C handle cannot carry that borrow — giving the
/// collection a sound `'static` lifetime would mean leaking or leaking-
/// equivalent tricks (`Box::leak` of the engine) that defeat
/// `corvid_close` and the counter lifecycle. The spec's own backing
/// table rules the shape instead: the handle owns the PARTS (engine
/// `Arc` + name + filters + sources + knobs) and
/// [`QueryHandle::execute`] materializes a real engine
/// `QueryBuilder` from them at execution time — every run/aggregate
/// consumes the handle (spec §5 rule 5), so the chain is applied
/// exactly once, by value, with no clone. The parts are applied in
/// insertion order (filter calls AND; source order is RRF's
/// source-chaining order; later knob calls overwrite earlier ones —
/// `order_by`/`select` replace, `limit`/`offset`/`rrf` overwrite),
/// matching the fluent builder a Rust caller would have chained.
pub(crate) struct QueryHandle {
    /// The engine reference that keeps the db working after
    /// `corvid_close` (spec §2 — the same derived-handle shape
    /// `CollHandle` uses).
    db: Arc<Db>,
    /// The collection name as stored on the coll handle.
    name: String,
    /// Filters, AND-combined in call order (`corvid_query_filter`).
    filters: Vec<Predicate>,
    /// Retrieval sources in call order.
    sources: Vec<Source>,
    /// The RRF constant (`corvid_query_fuse_rrf`; engine default
    /// `corvid::DEFAULT_RRF_K` until overridden — validated at
    /// execution, audit C6).
    rrf_k: f32,
    /// The MMR lambda (`corvid_query_rerank_mmr`; `None` until set).
    mmr_lambda: Option<f32>,
    /// The row cap (`corvid_query_limit`; `None` until set).
    limit: Option<usize>,
    /// The rows to skip after ordering (`corvid_query_offset`).
    offset: usize,
    /// The ordering field and direction (`corvid_query_order_by`).
    order_by: Option<(String, bool)>,
    /// The projection field list (`corvid_query_select`).
    projection: Option<Vec<String>>,
    /// The approximate-execution flag (`corvid_query_approx`).
    approx: bool,
    /// The db's derived-handle counter clone — released exactly once, by
    /// this handle's single consumption point (`corvid_query_free`, or
    /// the executing call that consumed the handle), whenever that runs.
    derived: Arc<AtomicUsize>,
}

impl QueryHandle {
    /// Wrap the builder state. The caller owns the matching
    /// `retain_derived` on the db handle (spec §4.13 — the count and the
    /// handle are born together in `corvid_query_new`).
    pub(crate) fn new(db: Arc<Db>, name: String, derived: Arc<AtomicUsize>) -> Self {
        Self {
            db,
            name,
            filters: Vec::new(),
            sources: Vec::new(),
            rrf_k: corvid::DEFAULT_RRF_K,
            mmr_lambda: None,
            limit: None,
            offset: 0,
            order_by: None,
            projection: None,
            approx: false,
            derived,
        }
    }

    /// Add a filter (the `corvid_query_filter` body; the pred was
    /// consumed by the caller's reclaim).
    pub(crate) fn push_filter(&mut self, predicate: Predicate) {
        self.filters.push(predicate);
    }

    /// Add a vector source (the `corvid_query_vector` body).
    pub(crate) fn push_vector(&mut self, field: String, query: Vec<f32>, k: usize, metric: Metric) {
        self.sources.push(Source::Vector {
            field,
            query,
            k,
            metric,
        });
    }

    /// Add a text source (the `corvid_query_text` body).
    pub(crate) fn push_text(&mut self, field: String, query: String, k: usize) {
        self.sources.push(Source::Text { field, query, k });
    }

    /// Set the RRF constant (`corvid_query_fuse_rrf`).
    pub(crate) fn set_rrf_k(&mut self, k: f32) {
        self.rrf_k = k;
    }

    /// Set the MMR lambda (`corvid_query_rerank_mmr`).
    pub(crate) fn set_mmr_lambda(&mut self, lambda: f32) {
        self.mmr_lambda = Some(lambda);
    }

    /// Set the limit (`corvid_query_limit`).
    pub(crate) fn set_limit(&mut self, n: usize) {
        self.limit = Some(n);
    }

    /// Set the offset (`corvid_query_offset`).
    pub(crate) fn set_offset(&mut self, n: usize) {
        self.offset = n;
    }

    /// Set (replace) the ordering (`corvid_query_order_by`).
    pub(crate) fn set_order_by(&mut self, field: String, descending: bool) {
        self.order_by = Some((field, descending));
    }

    /// Set (replace) the projection (`corvid_query_select`).
    pub(crate) fn set_projection(&mut self, fields: Vec<String>) {
        self.projection = Some(fields);
    }

    /// Allow approximate execution (`corvid_query_approx`).
    pub(crate) fn set_approx(&mut self) {
        self.approx = true;
    }

    /// CONSUME the handle: release the derived-handle count (spec
    /// §4.13 — execution consumes the handle unconditionally, spec §8,
    /// so the release happens before the engine call, on every path),
    /// materialize the engine `QueryBuilder` from the parts (see the
    /// struct docs), and run `f` on it under the panic guard.
    pub(crate) fn execute<T>(
        self,
        context: &str,
        f: impl FnOnce(corvid::QueryBuilder<'_>) -> corvid::Result<T>,
    ) -> Option<T> {
        self.derived.fetch_sub(1, Ordering::Release);
        let Self {
            db,
            name,
            filters,
            sources,
            rrf_k,
            mmr_lambda,
            limit,
            offset,
            order_by,
            projection,
            approx,
            derived: _,
        } = self;
        // The builder borrows the engine Arc and the name — both locals
        // of this scope, outliving `f`'s call; no lifetime escapes.
        let mut builder = db.collection(&name).query();
        for predicate in filters {
            builder = builder.filter(predicate);
        }
        for source in sources {
            builder = match source {
                Source::Vector {
                    field,
                    query,
                    k,
                    metric,
                } => builder.vector(field, query, k, metric),
                Source::Text { field, query, k } => builder.text(field, query, k),
            };
        }
        builder = builder.fuse_rrf(rrf_k);
        if let Some(lambda) = mmr_lambda {
            builder = builder.rerank_mmr(lambda);
        }
        if approx {
            builder = builder.approx();
        }
        builder = builder.offset(offset);
        if let Some(n) = limit {
            builder = builder.limit(n);
        }
        if let Some((field, descending)) = order_by {
            builder = builder.order_by(field, descending);
        }
        if let Some(fields) = projection {
            builder = builder.select(fields);
        }
        crate::error::guard(context, || f(builder))
    }

    /// The release half of the derived-handle count (spec §4.13), for
    /// `corvid_query_free`'s abandoned-builder path — the executing calls
    /// release inside [`Self::execute`] instead.
    pub(crate) fn release_derived(&self) {
        self.derived.fetch_sub(1, Ordering::Release);
    }
}

/// Interior state behind a `corvid_rows*` (spec §2: "materialized
/// `Vec<corvid::ResultRow>` + cursor", read-only, single-threaded use).
/// Holds no engine reference, so it does not touch the derived-handle
/// counter (spec §4.13 — see the module docs).
pub(crate) struct RowsHandle {
    rows: Vec<ResultRow>,
    cursor: usize,
}

impl RowsHandle {
    pub(crate) fn new(rows: Vec<ResultRow>) -> Self {
        Self { rows, cursor: 0 }
    }

    /// Borrow the row at the cursor and advance it; `None` at exhaustion
    /// (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<&ResultRow> {
        let row = self.rows.get(self.cursor);
        if row.is_some() {
            self.cursor += 1;
        }
        row
    }
}

/// Interior state behind a `corvid_strs*`: the materialized byte-string
/// list and the read cursor (spec §2, single-threaded use). The items are
/// `Vec<u8>`, not `String`: the cursor is shared by `corvid_collections`
/// (UTF-8 names) and the graph family, whose endpoints are document
/// KEYS — arbitrary bytes under spec §1.5 — so the backing preserves
/// bytes and the cursor hands out §1.5's binary-safe `(pointer, length)`
/// pairs either way.
pub(crate) struct StrsHandle {
    items: Vec<Vec<u8>>,
    cursor: usize,
}

impl StrsHandle {
    pub(crate) fn new(items: Vec<Vec<u8>>) -> Self {
        Self { items, cursor: 0 }
    }

    /// Borrow the string bytes at the cursor and advance them; `None` at
    /// exhaustion (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<&[u8]> {
        let item = self.items.get(self.cursor).map(Vec::as_slice);
        if item.is_some() {
            self.cursor += 1;
        }
        item
    }
}

/// Interior state behind a `corvid_geohits*` (spec §2: "owned
/// `Vec<corvid::GeoHit>` (or `(key, weight)` pairs) + cursor",
/// single-threaded use). One entry shape serves both producers: the three
/// geo queries carry the full document (borrowed out via `geohits_next`'s
/// `doc_out`), `corvid_neighbors_weighted` carries only `(key, weight)`
/// and reports `doc_out = NULL`. Holds no engine reference, so it does
/// not touch the derived-handle counter (spec §4.13 — see the module
/// docs).
pub(crate) struct GeoHitsHandle {
    hits: Vec<GeoEntry>,
    cursor: usize,
}

/// One materialized geohits row.
pub(crate) struct GeoEntry {
    /// The hit's key (document key; arbitrary bytes, spec §1.5).
    pub key: Vec<u8>,
    /// The hit's `distance_km` — the haversine kilometres for the geo
    /// queries, the 0.0 sentinel for `geo_within_bbox` (no center), the
    /// edge weight for `neighbors_weighted`.
    pub distance_km: f64,
    /// The full document for the geo queries; `None` for
    /// `neighbors_weighted` (the engine returns `(key, weight)` pairs
    /// with no document — `doc_out` is NULL there, spec §4.12).
    pub document: Option<corvid::Value>,
}

impl GeoHitsHandle {
    /// From the three geo queries' `Vec<GeoHit>` (document present).
    pub(crate) fn from_geo(hits: Vec<corvid::GeoHit>) -> Self {
        Self {
            hits: hits
                .into_iter()
                .map(|hit| GeoEntry {
                    key: hit.key,
                    distance_km: hit.distance_km,
                    document: Some(hit.document),
                })
                .collect(),
            cursor: 0,
        }
    }

    /// From `geo_within_bbox`'s `(key, document)` pairs — the documented
    /// 0.0 distance sentinel (the box query has no center; the engine
    /// returns no distance).
    pub(crate) fn from_bbox(hits: Vec<(Vec<u8>, corvid::Value)>) -> Self {
        Self {
            hits: hits
                .into_iter()
                .map(|(key, document)| GeoEntry {
                    key,
                    distance_km: 0.0,
                    document: Some(document),
                })
                .collect(),
            cursor: 0,
        }
    }

    /// From `neighbors_weighted`'s `(key, weight)` pairs (no document —
    /// `doc_out` reads NULL, spec §4.12).
    pub(crate) fn from_weighted(pairs: Vec<(Vec<u8>, f64)>) -> Self {
        Self {
            hits: pairs
                .into_iter()
                .map(|(key, weight)| GeoEntry {
                    key,
                    distance_km: weight,
                    document: None,
                })
                .collect(),
            cursor: 0,
        }
    }

    /// Borrow the hit at the cursor and advance it; `None` at exhaustion
    /// (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<(&[u8], f64, Option<&corvid::Value>)> {
        let hit = self
            .hits
            .get(self.cursor)
            .map(|e| (e.key.as_slice(), e.distance_km, e.document.as_ref()));
        if hit.is_some() {
            self.cursor += 1;
        }
        hit
    }
}

/// Interior state behind a `corvid_schemaiter*` (spec §2: "owned
/// `Vec<schema::Field>` + cursor", read-only, single-threaded use).
/// `corvid_schema` materializes the declared fields in declaration
/// order. Holds no engine reference, so it does not touch the
/// derived-handle counter (spec §4.13 — see the module docs).
pub(crate) struct SchemaIterHandle {
    fields: Vec<corvid::schema::Field>,
    cursor: usize,
}

impl SchemaIterHandle {
    pub(crate) fn new(fields: Vec<corvid::schema::Field>) -> Self {
        Self { fields, cursor: 0 }
    }

    /// Borrow the field at the cursor and advance it; `None` at
    /// exhaustion (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<&corvid::schema::Field> {
        let field = self.fields.get(self.cursor);
        if field.is_some() {
            self.cursor += 1;
        }
        field
    }
}

/// Interior state behind a `corvid_groupiter*` (spec §2: "owned group
/// list (sorted by group key) + cursor", read-only, single-threaded use).
/// The list is the engine aggregate's `BTreeMap` iteration order —
/// ascending group-key bytes — with `usize` counts widened to `f64`
/// (exact to 2^53, the spec §4.7 note). Holds no engine reference, so it
/// does not touch the derived-handle counter (spec §4.13 — see the
/// module docs).
pub(crate) struct GroupIterHandle {
    groups: Vec<(String, f64)>,
    cursor: usize,
}

impl GroupIterHandle {
    pub(crate) fn new(groups: Vec<(String, f64)>) -> Self {
        Self { groups, cursor: 0 }
    }

    /// Borrow the `(key, value)` pair at the cursor and advance it;
    /// `None` at exhaustion (the cursor stays, so exhaustion is sticky).
    pub(crate) fn next(&mut self) -> Option<(&str, f64)> {
        let pair = self.groups.get(self.cursor).map(|(k, v)| (k.as_str(), *v));
        if pair.is_some() {
            self.cursor += 1;
        }
        pair
    }
}

// --- db handle plumbing ----------------------------------------------------

/// Box a db body and hand out its opaque ABI pointer.
pub(crate) fn into_db(body: DbHandle) -> *mut corvid_db {
    Box::into_raw(Box::new(body)) as *mut corvid_db
}

/// Shared borrow of a db body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_db`] and not yet reclaimed by
/// [`reclaim_db`]; the borrow honors `corvid_db`'s thread-safe contract
/// (spec §2) — concurrent shared borrows are fine, a concurrent
/// [`reclaim_db`] is not.
pub(crate) unsafe fn borrow_db<'a>(ptr: *mut corvid_db) -> Option<&'a DbHandle> {
    // SAFETY: caller guarantees provenance (doc comment above); the box
    // pointer round-trips through the zero-sized marker with the body's
    // original alignment, so the reference is valid.
    unsafe { (ptr as *mut DbHandle).as_ref() }
}

/// Exclusive borrow of a db body, or `None` on NULL — for
/// `corvid_compact`, the one call that needs `&mut` (the engine's
/// `Db::compact(&mut self)`).
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_db`] and not yet reclaimed.
/// The borrow is EXCLUSIVE: sound only at the quiescent point spec §6
/// demands of a compacting caller — no other thread may be inside any
/// call on this same db handle (shared or reclaiming) while it is live.
/// The §4.13 gate makes the engine-side half of that duty checkable; the
/// handle-side half is the caller's documented quiescence, the same
/// discipline as `corvid_close`'s single reclaim.
pub(crate) unsafe fn borrow_db_mut<'a>(ptr: *mut corvid_db) -> Option<&'a mut DbHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut DbHandle).as_mut() }
}

/// Take a db body back for `corvid_close`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_db`], exactly once, and not
/// yet reclaimed.
pub(crate) unsafe fn reclaim_db(ptr: *mut corvid_db) -> Option<Box<DbHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of a pointer
    // produced by into_db (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut DbHandle) })
}

// --- strs handle plumbing --------------------------------------------------

/// Box a strs body and hand out its opaque ABI pointer.
pub(crate) fn into_strs(body: StrsHandle) -> *mut corvid_strs {
    Box::into_raw(Box::new(body)) as *mut corvid_strs
}

/// Exclusive borrow of a strs body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_strs`] and not yet reclaimed
/// by [`reclaim_strs`]; cursors are single-threaded by contract (spec
/// §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_strs_mut<'a>(ptr: *mut corvid_strs) -> Option<&'a mut StrsHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut StrsHandle).as_mut() }
}

/// Take a strs body back for `corvid_strs_free`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_strs`], exactly once, and not
/// yet reclaimed.
pub(crate) unsafe fn reclaim_strs(ptr: *mut corvid_strs) -> Option<Box<StrsHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of a pointer
    // produced by into_strs (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut StrsHandle) })
}

// --- geohits handle plumbing --------------------------------------------------

/// Box a geohits body and hand out its opaque ABI pointer (the three geo
/// queries and `corvid_neighbors_weighted` produce these).
pub(crate) fn into_geohits(body: GeoHitsHandle) -> *mut corvid_geohits {
    Box::into_raw(Box::new(body)) as *mut corvid_geohits
}

/// Exclusive borrow of a geohits body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_geohits`] and not yet
/// reclaimed by [`reclaim_geohits`]; cursors are single-threaded by
/// contract (spec §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_geohits_mut<'a>(
    ptr: *mut corvid_geohits,
) -> Option<&'a mut GeoHitsHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut GeoHitsHandle).as_mut() }
}

/// Take a geohits body back for `corvid_geohits_free`, or `None` on
/// NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_geohits`], exactly once, and
/// not yet reclaimed.
pub(crate) unsafe fn reclaim_geohits(ptr: *mut corvid_geohits) -> Option<Box<GeoHitsHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of a pointer
    // produced by into_geohits (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut GeoHitsHandle) })
}

// --- schemaiter handle plumbing ------------------------------------------------

/// Box a schemaiter body and hand out its opaque ABI pointer
/// (`corvid_schema` produces these).
pub(crate) fn into_schemaiter(body: SchemaIterHandle) -> *mut corvid_schemaiter {
    Box::into_raw(Box::new(body)) as *mut corvid_schemaiter
}

/// Exclusive borrow of a schemaiter body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_schemaiter`] and not yet
/// reclaimed by [`reclaim_schemaiter`]; cursors are single-threaded by
/// contract (spec §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_schemaiter_mut<'a>(
    ptr: *mut corvid_schemaiter,
) -> Option<&'a mut SchemaIterHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut SchemaIterHandle).as_mut() }
}

/// Take a schemaiter body back for `corvid_schemaiter_free`, or `None`
/// on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_schemaiter`], exactly once,
/// and not yet reclaimed.
pub(crate) unsafe fn reclaim_schemaiter(
    ptr: *mut corvid_schemaiter,
) -> Option<Box<SchemaIterHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of a pointer
    // produced by into_schemaiter (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut SchemaIterHandle) })
}

// --- coll handle plumbing ---------------------------------------------------

/// Box a coll body and hand out its opaque ABI pointer. The caller has
/// already run `DbHandle::retain_derived` (the count and the handle are
/// born together — spec §4.13).
pub(crate) fn into_coll(body: CollHandle) -> *mut corvid_coll {
    Box::into_raw(Box::new(body)) as *mut corvid_coll
}

/// Shared borrow of a coll body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_coll`] and not yet reclaimed
/// by [`reclaim_coll`]; `corvid_coll` is the thread-safe family (spec
/// §2 — it shares the engine `Arc`), so concurrent shared borrows are
/// fine and a concurrent reclaim is not.
pub(crate) unsafe fn borrow_coll<'a>(ptr: *mut corvid_coll) -> Option<&'a CollHandle> {
    // SAFETY: caller guarantees provenance (doc comment above); the box
    // pointer round-trips through the zero-sized marker with the body's
    // original alignment, so the reference is valid.
    unsafe { (ptr as *mut CollHandle).as_ref() }
}

/// Take a coll body back for `corvid_collection_free`, or `None` on
/// NULL. The caller owes the counter release (spec §4.13).
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_coll`], exactly once, and
/// not yet reclaimed.
pub(crate) unsafe fn reclaim_coll(ptr: *mut corvid_coll) -> Option<Box<CollHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of a pointer
    // produced by into_coll (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut CollHandle) })
}

// --- pred handle plumbing ---------------------------------------------------

/// Box a predicate and hand out its opaque ABI pointer: an OWNED
/// `corvid_pred*` (spec §4.5 — the constructors' return shape). Like a
/// value handle, a pred points at the bare engine type; unlike a value,
/// it has no borrowed provenance — the ABI never reads a pred's
/// interior (no `pred_*` reader exists), so every live `corvid_pred*`
/// is either a never-consumed root or a dangling post-consumption
/// pointer whose use is the documented UB of spec §4.5. That also means
/// this family has a `borrow`-free plumbing pair: `into_pred` and the
/// consuming `reclaim_pred` below are all there is.
pub(crate) fn into_pred(body: Predicate) -> *mut corvid_pred {
    Box::into_raw(Box::new(body)) as *mut corvid_pred
}

/// Take a predicate body back — the consumption that `pred_free` and
/// every consuming call (`and`/`or`/`not`, later `query_filter` and
/// `delete_where`) perform exactly once, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or an OWNED handle produced by [`into_pred`], exactly
/// once, and not yet consumed. Consuming an already-consumed pred (a
/// double free) is UB — spec §4.5.
pub(crate) unsafe fn reclaim_pred(ptr: *mut corvid_pred) -> Option<Box<Predicate>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of an
    // into_pred product (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut Predicate) })
}

// --- rows handle plumbing ---------------------------------------------------

/// Box a rows body and hand out its opaque ABI pointer (spec §4.6's
/// `corvid_query_run` and §4.9's `corvid_page` produce these).
pub(crate) fn into_rows(body: RowsHandle) -> *mut corvid_rows {
    Box::into_raw(Box::new(body)) as *mut corvid_rows
}

/// Exclusive borrow of a rows body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_rows`] and not yet reclaimed
/// by [`reclaim_rows`]; cursors are single-threaded by contract (spec
/// §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_rows_mut<'a>(ptr: *mut corvid_rows) -> Option<&'a mut RowsHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut RowsHandle).as_mut() }
}

/// Take a rows body back, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_rows`], exactly once, and
/// not yet reclaimed.
pub(crate) unsafe fn reclaim_rows(ptr: *mut corvid_rows) -> Option<Box<RowsHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of a pointer
    // produced by into_rows (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut RowsHandle) })
}

// --- query handle plumbing ---------------------------------------------------

/// Box a query body and hand out its opaque ABI pointer. The caller has
/// already run `DbHandle::retain_derived` (the count and the handle are
/// born together — spec §4.13).
pub(crate) fn into_query(body: QueryHandle) -> *mut corvid_query {
    Box::into_raw(Box::new(body)) as *mut corvid_query
}

/// Exclusive borrow of a query body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_query`] and not yet consumed
/// by [`reclaim_query`]; the query family is single-threaded by contract
/// (spec §2/§6 — build AND execution), so this is the only borrow.
pub(crate) unsafe fn borrow_query_mut<'a>(ptr: *mut corvid_query) -> Option<&'a mut QueryHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the zero-sized marker.
    unsafe { (ptr as *mut QueryHandle).as_mut() }
}

/// Take a query body back — the consumption that `corvid_query_free` and
/// every executing call (`corvid_query_run`, the aggregates) perform
/// exactly once, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_query`], exactly once, and
/// not yet consumed. Consuming an already-consumed query (a double free
/// — run/free twice, or free after run) is UB — spec §4.6/§8.
pub(crate) unsafe fn reclaim_query(ptr: *mut corvid_query) -> Option<Box<QueryHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of an into_query
    // product (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut QueryHandle) })
}

// --- groupiter handle plumbing ------------------------------------------------

/// Box a groupiter body and hand out its opaque ABI pointer (the
/// `corvid_query_group_*` constructors produce these).
pub(crate) fn into_groupiter(body: GroupIterHandle) -> *mut corvid_groupiter {
    Box::into_raw(Box::new(body)) as *mut corvid_groupiter
}

/// Exclusive borrow of a groupiter body, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_groupiter`] and not yet
/// reclaimed by [`reclaim_groupiter`]; cursors are single-threaded by
/// contract (spec §2/§6), so this is the only borrow.
pub(crate) unsafe fn borrow_groupiter_mut<'a>(
    ptr: *mut corvid_groupiter,
) -> Option<&'a mut GroupIterHandle> {
    // SAFETY: caller guarantees provenance and exclusivity (doc comment
    // above); the box pointer round-trips through the marker.
    unsafe { (ptr as *mut GroupIterHandle).as_mut() }
}

/// Take a groupiter body back, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or was produced by [`into_groupiter`], exactly once,
/// and not yet reclaimed.
pub(crate) unsafe fn reclaim_groupiter(ptr: *mut corvid_groupiter) -> Option<Box<GroupIterHandle>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the single reclaim of a pointer
    // produced by into_groupiter (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut GroupIterHandle) })
}

// --- value handle plumbing --------------------------------------------------
//
// A `corvid_value*` has TWO provenances (spec §2/§4.4/§5 rule 6), both
// pointing at a `corvid::Value`:
//
// * OWNED — `into_value` boxes the Value; the value constructors,
//   `corvid_value_clone`, and (later tasks) `corvid_get` / query
//   min-max return these. `reclaim_value` (via `corvid_value_free`) is
//   their only destructor.
// * BORROWED — `corvid_value_array_get` / `corvid_value_map_get` return
//   interior pointers straight into a live parent Value's `Vec` /
//   `BTreeMap` storage: a lightweight borrowed-view handle, valid until
//   the parent's next mutation (`array_push` / `map_put`) or free. A
//   parent-held child registry was an alternative design weighed here
//   (the plan itself only requires "value children are borrowed" — the
//   interior view satisfies it as written); it lost because it would
//   tax every child read on the document hot path with bookkeeping for
//   a guarantee C cannot check anyway — the interior view is zero-cost
//   and its lifetime is exactly the spec's "rides the parent" wording.
//
// The two are indistinguishable by pointer VALUE (an interior pointer's
// bits are unremarkable), so misuse — freeing a borrowed child, or
// passing one where an owned value is consumed — is undetectable at
// runtime: documented UB, bold in the spec, netted by the Task 7
// ASan/LSan CI runs.

/// Box a value and hand out its opaque ABI pointer: an OWNED
/// `corvid_value*` (spec §4.3 — the constructors' return shape).
pub(crate) fn into_value(body: corvid::Value) -> *mut corvid_value {
    Box::into_raw(Box::new(body)) as *mut corvid_value
}

/// Shared borrow of a value, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL, an owned handle produced by [`into_value`] and not yet
/// reclaimed, or a borrowed child whose parent has not been mutated
/// (`array_push`/`map_put`) or freed since the child was obtained; the
/// §2 contract confines a value handle to one thread.
pub(crate) unsafe fn borrow_value<'a>(ptr: *const corvid_value) -> Option<&'a corvid::Value> {
    // SAFETY: caller guarantees provenance (doc comment above); both
    // handle shapes point at a live `corvid::Value`, and the box pointer
    // round-trips through the zero-sized marker with its alignment.
    unsafe { (ptr as *const corvid::Value).as_ref() }
}

/// Exclusive borrow of a value for mutation, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or an OWNED handle produced by [`into_value`], not yet
/// reclaimed. The mutation sites (`corvid_value_array_push` /
/// `corvid_value_map_put`) take `corvid_value*` non-const, so a borrowed
/// child cannot reach one without a deliberate const-cast — which is the
/// documented UB of spec §4.4.
pub(crate) unsafe fn borrow_value_mut<'a>(ptr: *mut corvid_value) -> Option<&'a mut corvid::Value> {
    // SAFETY: caller guarantees owned provenance (doc comment above);
    // the §2 single-thread contract makes this the only borrow.
    unsafe { (ptr as *mut corvid::Value).as_mut() }
}

/// Take a value body back for `corvid_value_free`, or `None` on NULL.
///
/// # Safety
///
/// `ptr` is NULL or an OWNED handle produced by [`into_value`], exactly
/// once, and not yet reclaimed. Reclaiming a borrowed child (an interior
/// pointer from `array_get`/`map_get`) is UB — spec §4.4, in bold.
pub(crate) unsafe fn reclaim_value(ptr: *mut corvid_value) -> Option<Box<corvid::Value>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees this is the single reclaim of an
    // into_value product (doc comment above).
    Some(unsafe { Box::from_raw(ptr as *mut corvid::Value) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter's lifecycle: 1 at open, +1 per derived handle, back to
    /// 1 at release; `is_exclusive` is the compaction gate (spec §4.13).
    /// The production retain call sites are `corvid_collection`
    /// (collection.rs) and `corvid_query_new` (query.rs); this pins the
    /// arithmetic.
    #[test]
    fn derived_counter_gates_exclusive_compaction() {
        let db = DbHandle::new(corvid::Db::open_in_memory().unwrap());
        assert!(
            db.is_exclusive(),
            "a fresh db handle is alone: count starts at 1"
        );

        db.retain_derived();
        assert!(!db.is_exclusive(), "one derived handle blocks compact");

        db.retain_derived();
        let counter = db.counter(); // the coll-side release handle
        counter.fetch_sub(1, Ordering::Release);
        assert!(!db.is_exclusive(), "still one derived handle live");

        counter.fetch_sub(1, Ordering::Release);
        assert!(db.is_exclusive(), "last release returns to exactly 1");
    }

    /// The release half must work AFTER the db handle box is gone (spec
    /// §2: a collection handle legitimately outlives `corvid_close`) —
    /// the `Arc<AtomicUsize>` counter outlives the `DbHandle` that
    /// seeded it. Dropping the last reference (the `CollHandle`'s
    /// release + drop) is the crash test.
    #[test]
    fn derived_release_survives_the_db_handle() {
        let db = DbHandle::new(corvid::Db::open_in_memory().unwrap());
        db.retain_derived();
        let counter = db.counter();
        let engine = db.db();
        drop(db); // the corvid_close side

        // The coll-side release, after the box is gone.
        counter.fetch_sub(1, Ordering::Release);
        assert_eq!(counter.load(Ordering::Acquire), 1, "back to exactly 1");
        drop(counter);
        drop(engine); // and the engine Arc goes last — nothing dangles.
    }

    /// The engine `Arc` the counter guards: a derived clone keeps the
    /// engine alive after `corvid_close` drops the db handle (spec §2's
    /// "a corvid_coll keeps its corvid_db alive").
    #[test]
    fn derived_arc_clone_outlives_the_db_handle() {
        let db = DbHandle::new(corvid::Db::open_in_memory().unwrap());
        let engine = db.db();
        assert!(std::ptr::eq(Arc::as_ptr(&engine), db.engine()));
        drop(db);
        // The engine reference still resolves collections — nothing
        // touched the file lock or state.
        assert!(engine.collections().unwrap().is_empty());
    }

    /// The `CollHandle`'s name view: NUL-terminated for C, exact length
    /// for `*len_out`, stable across calls (the buffer never moves).
    #[test]
    fn coll_handle_carries_the_name_and_its_nul_terminated_view() {
        let engine = Arc::new(corvid::Db::open_in_memory().unwrap());
        let counter = Arc::new(AtomicUsize::new(1));
        let coll = CollHandle::new(Arc::clone(&engine), "docs".to_owned(), counter);

        assert_eq!(coll.name_len(), 4);
        // SAFETY: name_ptr borrows the handle's own buffer; the handle
        // outlives this read.
        let view = coll.name_ptr();
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(view) }.to_bytes(),
            b"docs"
        );
        // Stability: repeated calls hand out the same pointer.
        assert_eq!(coll.name_ptr(), view);

        // The engine-collection bridge works off the stored name (a real
        // engine call through Collection's public surface).
        assert_eq!(coll.collection().len().unwrap(), 0);
        // An interior-NUL name (invalid at write time, fine at handle
        // time — spec §4.2's lazy validation) truncates only the C view;
        // the exact length is kept.
        let odd = CollHandle::new(engine, "a\0b".to_owned(), AtomicUsize::new(1).into());
        assert_eq!(odd.name_len(), 3);
        // SAFETY: same provenance as above.
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(odd.name_ptr()) }.to_bytes(),
            b"a"
        );
    }

    #[test]
    fn rows_cursor_walks_and_sticks_at_exhaustion() {
        let row = |key: &[u8], n: i64| ResultRow {
            key: key.to_vec(),
            score: 0.0,
            document: corvid::Value::Int(n),
        };
        let mut rows = RowsHandle::new(vec![row(b"a", 1), row(b"b", 2)]);
        assert_eq!(rows.next().unwrap().document, corvid::Value::Int(1));
        assert_eq!(rows.next().unwrap().key, b"b".to_vec());
        assert!(rows.next().is_none());
        assert!(rows.next().is_none(), "exhaustion is sticky");

        let mut empty = RowsHandle::new(Vec::new());
        assert!(empty.next().is_none());
    }

    #[test]
    fn strs_cursor_walks_and_sticks_at_exhaustion() {
        let mut strs = StrsHandle::new(vec![b"alpha".to_vec(), b"beta".to_vec()]);
        assert_eq!(strs.next(), Some(&b"alpha"[..]));
        assert_eq!(strs.next(), Some(&b"beta"[..]));
        assert_eq!(strs.next(), None);
        assert_eq!(strs.next(), None, "exhaustion is sticky");

        let mut empty = StrsHandle::new(Vec::new());
        assert_eq!(empty.next(), None);
    }

    #[test]
    fn geohits_cursor_serves_geo_bbox_and_weighted_shapes() {
        use corvid::GeoHit;
        // Geo shape: key + distance + full document.
        let mut geo = GeoHitsHandle::from_geo(vec![GeoHit {
            key: b"near".to_vec(),
            distance_km: 12.5,
            document: corvid::Value::Int(1),
        }]);
        let (key, distance, doc) = geo.next().expect("one hit");
        assert_eq!(key, b"near");
        assert_eq!(distance, 12.5);
        assert_eq!(doc, Some(&corvid::Value::Int(1)));
        assert!(geo.next().is_none(), "exhaustion is sticky");

        // bbox shape: 0.0 sentinel distance, document present.
        let mut bbox = GeoHitsHandle::from_bbox(vec![(b"in-box".to_vec(), corvid::Value::Null)]);
        let (key, distance, doc) = bbox.next().expect("one hit");
        assert_eq!((key, distance), (b"in-box".as_slice(), 0.0));
        assert_eq!(doc, Some(&corvid::Value::Null));
        assert!(bbox.next().is_none());

        // Weighted shape: the weight rides distance_km, doc is None.
        let mut weighted = GeoHitsHandle::from_weighted(vec![(b"to".to_vec(), 0.5)]);
        let (key, distance, doc) = weighted.next().expect("one pair");
        assert_eq!((key, distance), (b"to".as_slice(), 0.5));
        assert_eq!(doc, None, "neighbors_weighted carries no document");
        assert!(weighted.next().is_none());
    }

    #[test]
    fn schemaiter_cursor_walks_fields_in_declaration_order() {
        use corvid::schema::{Field, FieldType};
        let fields = vec![
            Field::new("name", FieldType::Text).required(),
            Field::new("email", FieldType::Text).unique(),
        ];
        let mut it = SchemaIterHandle::new(fields);
        let first = it.next().expect("field 1");
        assert_eq!(
            (first.name.as_str(), first.ty, first.required, first.unique),
            ("name", FieldType::Text, true, false)
        );
        let second = it.next().expect("field 2");
        assert_eq!(
            (second.name.as_str(), second.required, second.unique),
            ("email", false, true)
        );
        assert!(it.next().is_none());
        assert!(it.next().is_none(), "exhaustion is sticky");
    }

    /// The compact gate's two halves (spec §4.13, the Task 5 review
    /// prepend): a derived count > 1 fails BUSY even when the Arc is
    /// otherwise exclusive, and a count of exactly 1 with a live clone
    /// (the execute()-in-flight shape) fails BUSY too — only count 1 AND
    /// sole Arc ownership lets the engine call through.
    #[test]
    fn compact_gate_requires_count_one_and_sole_arc_ownership() {
        let mut db = DbHandle::new(corvid::Db::open_in_memory().unwrap());

        // Count 1, Arc exclusive: the engine call runs (whether an
        // in-memory db reports movement is redb's business — the gate's
        // answer is that the call goes THROUGH).
        assert!(db.compact().is_some(), "fresh in-memory db: the gate opens");

        // Count 2 (a live derived handle): BUSY.
        db.retain_derived();
        assert!(db.compact().is_none());
        assert_eq!(
            crate::error::last_code(),
            crate::error::corvid_err::CORVID_E_BUSY
        );

        // Count back to 1 but a clone lives (the execute()-in-flight
        // shape): still BUSY — the counter alone is not engine-idle.
        let engine = db.db();
        db.derived.fetch_sub(1, Ordering::Release);
        assert_eq!(db.derived.load(Ordering::Acquire), 1);
        assert!(db.compact().is_none());
        assert_eq!(
            crate::error::last_code(),
            crate::error::corvid_err::CORVID_E_BUSY
        );

        // Clone dropped: count 1 AND sole ownership — compacts again.
        drop(engine);
        assert!(db.compact().is_some(), "sole ownership: the gate opens");
    }

    #[test]
    fn groupiter_cursor_walks_pairs_and_sticks_at_exhaustion() {
        let mut groups = GroupIterHandle::new(vec![("1".into(), 1.0), ("b:true".into(), 2.0)]);
        assert_eq!(groups.next(), Some(("1", 1.0)));
        assert_eq!(groups.next(), Some(("b:true", 2.0)));
        assert_eq!(groups.next(), None);
        assert_eq!(groups.next(), None, "exhaustion is sticky");

        let mut empty = GroupIterHandle::new(Vec::new());
        assert_eq!(empty.next(), None);
    }

    /// The query handle's derived count is released exactly once on the
    /// EXECUTION path (even a failing execution — spec §8's unconditional
    /// consumption), and once on the free path: both halves of the
    /// wiring, at the interior-state level.
    #[test]
    fn query_execute_and_free_each_release_the_derived_count_once() {
        let engine = Arc::new(corvid::Db::open_in_memory().unwrap());
        let counter = Arc::new(AtomicUsize::new(1));
        // The born-together retain `corvid_query_new` performs via
        // `CollHandle::spawn_query` (spec §4.13) — replicated per handle.
        let retain = || counter.fetch_add(1, Ordering::Release);

        // The free path: release_derived is free's single decrement.
        retain();
        let query = QueryHandle::new(Arc::clone(&engine), "docs".into(), Arc::clone(&counter));
        query.release_derived();
        assert_eq!(counter.load(Ordering::Acquire), 1, "free path: back to 1");

        // The execution path: execute consumes (and releases) even when
        // the engine call FAILS — a bad rrf_k makes run reject.
        retain();
        let mut query = QueryHandle::new(Arc::clone(&engine), "docs".into(), Arc::clone(&counter));
        query.set_rrf_k(0.0);
        let failed: Option<Vec<corvid::ResultRow>> = query.execute("test", |b| b.run());
        assert!(failed.is_none(), "rrf_k 0.0 fails at execution (audit C6)");
        assert_eq!(
            counter.load(Ordering::Acquire),
            1,
            "execute path: back to 1"
        );

        // And a SUCCEEDING execution releases too, handing back the
        // engine's rows.
        retain();
        let query = QueryHandle::new(Arc::clone(&engine), "docs".into(), Arc::clone(&counter));
        let rows: Option<Vec<corvid::ResultRow>> = query.execute("test", |b| b.run());
        assert_eq!(rows.expect("empty collection queries fine").len(), 0);
        assert_eq!(counter.load(Ordering::Acquire), 1);
    }

    #[test]
    fn opaque_pointers_round_trip_through_the_marker() {
        let ptr = into_db(DbHandle::new(corvid::Db::open_in_memory().unwrap()));
        assert!(!ptr.is_null());
        // SAFETY: ptr was produced by into_db just above and not yet
        // reclaimed; single-threaded test.
        assert!(unsafe { borrow_db(ptr) }.is_some());
        // SAFETY: same provenance, first and only reclaim.
        drop(unsafe { reclaim_db(ptr) });

        let null: *mut corvid_db = std::ptr::null_mut();
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_db(null) }.is_none());
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { reclaim_db(null) }.is_none());

        let strs = into_strs(StrsHandle::new(Vec::new()));
        // SAFETY: fresh into_strs product, exclusive test-thread borrow.
        assert!(unsafe { borrow_strs_mut(strs) }.is_some());
        // SAFETY: first and only reclaim.
        drop(unsafe { reclaim_strs(strs) });
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_strs_mut(std::ptr::null_mut()) }.is_none());
    }

    #[test]
    fn value_pointers_round_trip_and_children_live_inside_the_parent() {
        use crate::handle::borrow_value;
        use crate::handle::into_value;
        use crate::handle::reclaim_value;

        let owned = into_value(corvid::Value::Array(vec![corvid::Value::Int(7)]));
        // The borrowed-child shape of array_get: an interior pointer into
        // the parent's Vec. Scoped so the shared borrow provably ends
        // before the reclaim below (raw-pointer provenance is a SAFETY
        // contract, not something borrowck checks).
        let child: *const corvid_value = {
            // SAFETY: fresh into_value product on this thread.
            let parent = unsafe { borrow_value(owned) }.expect("non-NULL handle");
            match parent {
                corvid::Value::Array(items) => {
                    &items[0] as *const corvid::Value as *const corvid_value
                }
                _ => unreachable!("constructed as an array above"),
            }
        };
        // SAFETY: the child borrows the still-live parent; read before
        // the reclaim below.
        assert_eq!(
            unsafe { borrow_value(child) }.and_then(corvid::Value::as_int),
            Some(7)
        );
        // SAFETY: first and only reclaim of the owned product; the child
        // borrow ended with the statement above.
        drop(unsafe { reclaim_value(owned) });

        let null: *const corvid_value = std::ptr::null();
        // SAFETY: NULL is the documented None input.
        assert!(unsafe { borrow_value(null) }.is_none());
        // SAFETY: NULL is the documented no-op reclaim.
        assert!(unsafe { reclaim_value(std::ptr::null_mut()) }.is_none());
    }
}
