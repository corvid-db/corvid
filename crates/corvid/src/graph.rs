//! A lightweight directed property graph over document keys.
//!
//! Edges connect keys within a collection under a string relation label and are
//! stored in a sibling edge collection (`__edges__<collection>`) so they share
//! the same transactional store as the documents. Edge keys are length-prefixed
//! `relation ‖ from ‖ to`, which lets [`Collection::neighbors`] resolve all
//! targets of a `(from, relation)` pair with a single prefix scan.
//!
//! Because that layout orders edges by *relation*, an endpoint is not a key
//! prefix — the delete cascade used to page through BOTH edge namespaces per
//! delete (O(E)). Two private adjacency namespaces (`__adj_out__<collection>`,
//! `__adj_in__<collection>`) now hold the same edges re-keyed *endpoint-first*:
//! DERIVED state (the edge rows stay the only source of truth; their format is
//! unchanged), rebuilt lazily from them on first cascade use after open and
//! maintained transactionally by [`Collection::link`],
//! [`Collection::link_weighted`] and [`Collection::unlink`] — so the cascade
//! finds a document's edges directly, in O(edges-of-doc). See
//! `Db::edges_on_delete_in_txn` for the layout, rebuild and fallback rules.
//!
//! All four graph namespaces are engine-private, so their rows are written
//! with the store's UNCOUNTED put/delete (no maintained record count —
//! nothing in the engine reads one, and skipping the per-row count
//! read-modify-write keeps the dual-write cheap).
//!
//! Deleting a document cascades (in the delete's own transaction) to every
//! edge attached to it — see `Db::edges_on_delete_in_txn` (private; every
//! document-delete path calls it) — and linking/unlinking emits change events
//! after the commit, like every other write path.
//!
//! This is the traversal core for the agent-memory use case (entity/relation
//! graphs). Graph algorithms beyond neighbor lookup and bounded BFS traversal
//! are intentionally out of scope for now.

use std::collections::HashSet;

use crate::db::{Collection, Db};
use crate::error::Result;
use crate::reactive::{ChangeEvent, ChangeKind};
use crate::store::WriteBatch;

/// The reserved collection holding this collection's OUT adjacency rows
/// (one per edge, keyed by its source endpoint).
fn adj_out_name(collection: &str) -> String {
    format!("__adj_out__{collection}")
}

/// The reserved collection holding this collection's IN adjacency rows
/// (one per edge, keyed by its target endpoint).
fn adj_in_name(collection: &str) -> String {
    format!("__adj_in__{collection}")
}

/// Adjacency row keys start with this tag; the built-marker uses [`ADJ_TAG_META`]
/// so it can never collide with (or prefix) a row (the ttl.rs TAG_FWD/TAG_IDX
/// convention).
const ADJ_TAG_ROW: u8 = 0x01;
/// The built-marker's tag — sorts below every row.
const ADJ_TAG_META: u8 = 0x00;

/// The adjacency build marker's value: the adjacency layout's version. A
/// marker carrying any other value is stale-shaped and forces a rebuild (the
/// edge-row format itself never migrates; adjacency is derived state, so a
/// rebuild — not a file migration — is the whole upgrade story).
const ADJACENCY_VERSION: &[u8] = b"1";

/// Paged-scan page size for the adjacency build and cascade walks.
const ADJ_PAGE: usize = 1024;

/// The adjacency namespaces' build-marker key (`ADJ_TAG_META ‖ "adjacency"`).
fn adjacency_marker_key() -> [u8; 10] {
    let mut k = [0u8; 10];
    k[0] = ADJ_TAG_META;
    k[1..].copy_from_slice(b"adjacency");
    k
}

/// Encode an adjacency row key: the edge `(endpoint --relation--> other)`
/// re-keyed endpoint-first, `ADJ_TAG_ROW ‖ len(endpoint) ‖ endpoint ‖
/// len(relation) ‖ relation ‖ other` — the same length-prefixed triple as an
/// edge key with the endpoint moved to the front, so every row of one
/// endpoint shares a prefix and the cascade reaches them with one scan.
fn adj_key(endpoint: &[u8], relation: &str, other: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 8 + endpoint.len() + relation.len() + other.len());
    k.push(ADJ_TAG_ROW);
    k.extend_from_slice(&(endpoint.len() as u32).to_be_bytes());
    k.extend_from_slice(endpoint);
    k.extend_from_slice(&(relation.len() as u32).to_be_bytes());
    k.extend_from_slice(relation.as_bytes());
    k.extend_from_slice(other);
    k
}

/// The prefix shared by every adjacency row of `endpoint` (inverse of the
/// endpoint-leading portion of [`adj_key`]).
fn adj_prefix(endpoint: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 4 + endpoint.len());
    k.push(ADJ_TAG_ROW);
    k.extend_from_slice(&(endpoint.len() as u32).to_be_bytes());
    k.extend_from_slice(endpoint);
    k
}

/// Parse an adjacency row key back into `(endpoint, relation, other)`.
/// Inverse of [`adj_key`]; `None` on a malformed (truncated / non-UTF-8
/// relation) key — unreachable via the encoder, the corruption-fallback
/// shape.
fn decode_adj_key(key: &[u8]) -> Option<(Vec<u8>, String, Vec<u8>)> {
    if key.first() != Some(&ADJ_TAG_ROW) {
        return None;
    }
    let mut pos = 1usize; // skip the row tag
    let read_len = |pos: &mut usize| -> Option<u32> {
        let b = key.get(*pos..*pos + 4)?;
        *pos += 4;
        Some(u32::from_be_bytes(b.try_into().unwrap()))
    };
    let endpoint_len = read_len(&mut pos)? as usize;
    let endpoint = key.get(pos..pos + endpoint_len)?.to_vec();
    pos += endpoint_len;
    let rel_len = read_len(&mut pos)? as usize;
    let rel = std::str::from_utf8(key.get(pos..pos + rel_len)?)
        .ok()?
        .to_owned();
    pos += rel_len;
    let other = key.get(pos..)?.to_vec();
    Some((endpoint, rel, other))
}

impl Collection<'_> {
    /// Add a directed edge `from --relation--> to`. Idempotent (re-linking
    /// an existing edge is not an error). A reverse edge is stored too, so
    /// [`Collection::in_neighbors`] can answer "who links to `to`?".
    ///
    /// The edge carries the default weight `1.0`: a plain `link` overwrites
    /// any prior [`Collection::link_weighted`] value for the same edge.
    ///
    /// Endpoints do not have to exist as documents: an edge to an absent key
    /// is allowed and is cleaned up automatically when [`Collection::delete`]
    /// runs on that endpoint — even if it never existed as a document (the
    /// same cascade `Db::edges_on_delete_in_txn` runs for every delete path).
    ///
    /// Emits a [`ChangeEvent`] (kind [`ChangeKind::Insert`], keyed by `from`)
    /// after the transaction commits — never before, so subscribers only ever
    /// observe committed edges. Re-linking an existing edge re-emits the
    /// event: like an overwriting `insert`, the data write is idempotent but
    /// the notification is not skipped.
    pub fn link(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (adj_out, adj_in) = (adj_out_name(self.name()), adj_in_name(self.name()));
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let (out_adj, in_adj) = (adj_key(from, relation, to), adj_key(to, relation, from));
        // Forward and reverse edges (plus both adjacency rows — see the
        // module docs) commit together: no half-linked state. All four rows
        // are UNCOUNTED: the graph namespaces are engine-private and nothing
        // reads their maintained counts, so skipping the per-row META
        // read-modify-write is pure savings. The first link (re)establishes
        // the adjacency namespaces, keeping the marker invariant "present ⇒
        // complete for the committed edge rows" true from the first edge on:
        // on a fresh database the build is an empty no-op, and on a legacy
        // (pre-adjacency) database it derives the rows from the existing
        // edge rows exactly once.
        self.db().store().transaction(|tx| {
            ensure_adjacency_in_txn(tx, self.name())?;
            tx.put_uncounted(&forward, &fwd_key, b"")?;
            tx.put_uncounted(&reverse, &rev_key, b"")?;
            tx.put_uncounted(&adj_out, &out_adj, b"")?;
            tx.put_uncounted(&adj_in, &in_adj, b"")?;
            Ok(())
        })?;
        self.db().notify(ChangeEvent {
            collection: self.name().to_owned(),
            key: from.to_vec(),
            kind: ChangeKind::Insert,
        });
        Ok(())
    }

    /// Add a directed edge carrying a `weight` (e.g. confidence or cost). Like
    /// [`Collection::link`] but the weight is stored on the edge and readable
    /// via [`Collection::neighbors_weighted`]. Emits the same post-commit
    /// [`ChangeEvent`] as [`Collection::link`]. A later plain
    /// [`Collection::link`] of the same edge overwrites the weight with the
    /// default `1.0`.
    pub fn link_weighted(&self, from: &[u8], relation: &str, to: &[u8], weight: f64) -> Result<()> {
        self.ensure_writable()?;
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (adj_out, adj_in) = (adj_out_name(self.name()), adj_in_name(self.name()));
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let (out_adj, in_adj) = (adj_key(from, relation, to), adj_key(to, relation, from));
        let value = weight.to_le_bytes();
        self.db().store().transaction(|tx| {
            ensure_adjacency_in_txn(tx, self.name())?;
            tx.put_uncounted(&forward, &fwd_key, &value)?;
            tx.put_uncounted(&reverse, &rev_key, &value)?;
            tx.put_uncounted(&adj_out, &out_adj, &value)?;
            tx.put_uncounted(&adj_in, &in_adj, &value)?;
            Ok(())
        })?;
        self.db().notify(ChangeEvent {
            collection: self.name().to_owned(),
            key: from.to_vec(),
            kind: ChangeKind::Insert,
        });
        Ok(())
    }

    /// Return `(target, weight)` for every `from --relation--> ?` edge.
    /// Unweighted edges report a weight of `1.0`.
    pub fn neighbors_weighted(&self, from: &[u8], relation: &str) -> Result<Vec<(Vec<u8>, f64)>> {
        let prefix = neighbor_prefix(relation, from);
        let edges = self.db().store().scan_prefix(&self.edges_name(), &prefix)?;
        Ok(edges
            .into_iter()
            .map(|(key, value)| {
                let to = key.get(prefix.len()..).unwrap_or(&[]).to_vec();
                (to, decode_weight(&value))
            })
            .collect())
    }

    /// Remove the edge `from --relation--> to` (and its reverse), atomically.
    /// Returns whether the forward edge existed.
    ///
    /// Emits a [`ChangeEvent`] (kind [`ChangeKind::Delete`], keyed by `from`)
    /// after the commit, and only when an edge was actually removed — a failed
    /// unlink is silent, like a delete of a missing document.
    pub fn unlink(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<bool> {
        self.ensure_writable()?;
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (adj_out, adj_in) = (adj_out_name(self.name()), adj_in_name(self.name()));
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let (out_adj, in_adj) = (adj_key(from, relation, to), adj_key(to, relation, from));
        let removed = self.db().store().transaction(|tx| {
            let removed = tx.delete_uncounted(&forward, &fwd_key)?;
            tx.delete_uncounted(&reverse, &rev_key)?;
            // The edge's two adjacency rows go with it. When no adjacency
            // exists yet (marker absent ⇒ no rows anywhere — see the marker
            // invariant) the deletes are skipped entirely; once it exists
            // they are no-op-safe uncounted removals.
            if adjacency_ready_in_txn(tx, self.name())? {
                tx.delete_uncounted(&adj_out, &out_adj)?;
                tx.delete_uncounted(&adj_in, &in_adj)?;
            }
            Ok(removed)
        })?;
        if removed {
            self.db().notify(ChangeEvent {
                collection: self.name().to_owned(),
                key: from.to_vec(),
                kind: ChangeKind::Delete,
            });
        }
        Ok(removed)
    }

    /// Return the targets of every `from --relation--> ?` edge, in key order.
    pub fn neighbors(&self, from: &[u8], relation: &str) -> Result<Vec<Vec<u8>>> {
        let prefix = neighbor_prefix(relation, from);
        let edges = self.db().store().scan_prefix(&self.edges_name(), &prefix)?;
        Ok(edges
            .into_iter()
            .map(|(key, _)| key.get(prefix.len()..).unwrap_or(&[]).to_vec())
            .collect())
    }

    /// Return the sources of every `? --relation--> to` edge, in key order
    /// (incoming edges).
    pub fn in_neighbors(&self, to: &[u8], relation: &str) -> Result<Vec<Vec<u8>>> {
        let prefix = neighbor_prefix(relation, to);
        let edges = self
            .db()
            .store()
            .scan_prefix(&self.redges_name(), &prefix)?;
        Ok(edges
            .into_iter()
            .map(|(key, _)| key.get(prefix.len()..).unwrap_or(&[]).to_vec())
            .collect())
    }

    /// Breadth-first traversal following `relation` up to `hops` hops from
    /// `start`. Returns the reachable nodes (excluding `start`) in BFS order,
    /// each at most once. Cycles terminate; `hops == 0` yields nothing.
    ///
    /// The whole traversal runs on ONE read snapshot (audit B3): every hop's
    /// neighbor scan observes a single point in time, so the reachable set
    /// always matches some committed state even while writers link/unlink
    /// concurrently.
    pub fn traverse(&self, start: &[u8], relation: &str, hops: usize) -> Result<Vec<Vec<u8>>> {
        self.db().store().read(|r| {
            let mut visited: HashSet<Vec<u8>> = HashSet::new();
            visited.insert(start.to_vec());
            let mut frontier = vec![start.to_vec()];
            let mut result = Vec::new();

            for _ in 0..hops {
                let mut next = Vec::new();
                for node in &frontier {
                    for to in self.neighbors_in(r, node, relation)? {
                        if visited.insert(to.clone()) {
                            result.push(to.clone());
                            next.push(to);
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
            Ok(result)
        })
    }

    /// Snapshot-scoped twin of [`Collection::neighbors`]: the targets of
    /// every `from --relation--> ?` edge, in key order, read from `reader`'s
    /// snapshot (audit B3 — used by [`Collection::traverse`] so a whole
    /// traversal sees one point in time).
    fn neighbors_in(
        &self,
        reader: &dyn crate::store::SnapshotReader,
        from: &[u8],
        relation: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let prefix = neighbor_prefix(relation, from);
        let edges = reader.scan_prefix(&self.edges_name(), &prefix)?;
        Ok(edges
            .into_iter()
            .map(|(key, _)| key.get(prefix.len()..).unwrap_or(&[]).to_vec())
            .collect())
    }

    /// The sibling collection holding this collection's forward edges.
    fn edges_name(&self) -> String {
        format!("__edges__{}", self.name())
    }

    /// The sibling collection holding this collection's reverse edges.
    fn redges_name(&self) -> String {
        format!("__redges__{}", self.name())
    }
}

/// Decode an edge weight value: an 8-byte little-endian f64, or `1.0` if the
/// edge carries no weight (empty value).
fn decode_weight(value: &[u8]) -> f64 {
    match value.try_into() {
        Ok(bytes) => f64::from_le_bytes(bytes),
        Err(_) => 1.0,
    }
}

/// Encode an edge key: `len(relation) ‖ relation ‖ len(from) ‖ from ‖ to`.
fn edge_key(relation: &str, from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut k = neighbor_prefix(relation, from);
    k.extend_from_slice(to);
    k
}

/// The prefix shared by all edges of a `(from, relation)` pair.
fn neighbor_prefix(relation: &str, from: &[u8]) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(&(relation.len() as u32).to_be_bytes());
    k.extend_from_slice(relation.as_bytes());
    k.extend_from_slice(&(from.len() as u32).to_be_bytes());
    k.extend_from_slice(from);
    k
}

/// Parse an edge key back into `(relation, from, to)`. Inverse of
/// [`edge_key`]; the `to` field is length-delimited by the key's end.
pub(crate) fn decode_edge_key(key: &[u8]) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let mut pos = 0usize;
    let read_len = |pos: &mut usize| -> Option<u32> {
        let b = key.get(*pos..*pos + 4)?;
        *pos += 4;
        Some(u32::from_be_bytes(b.try_into().unwrap()))
    };
    let rel_len = read_len(&mut pos)? as usize;
    let rel = std::str::from_utf8(key.get(pos..pos + rel_len)?)
        .ok()?
        .to_owned();
    pos += rel_len;
    let from_len = read_len(&mut pos)? as usize;
    let from = key.get(pos..pos + from_len)?.to_vec();
    pos += from_len;
    let to = key.get(pos..)?.to_vec();
    Some((rel, from, to))
}

impl Db {
    /// Remove every edge that has `key` as an endpoint, inside the caller's
    /// write transaction (audit B4: a deleted document must never leave
    /// dangling edges). Called by every document-delete path —
    /// [`crate::Db::write_document`], [`Collection::compare_and_set`], and the
    /// TTL purge — so the two edge namespaces can never disagree with the
    /// documents. Every one of these paths runs the cascade EVEN WHEN the
    /// document row is absent, so edges linked against a never-inserted (or
    /// already-deleted) key are purgeable through [`Collection::delete`] and
    /// through a TTL purge of a stranded expiry entry alike (W3 ruling: the
    /// cascade is part of the delete contract, not conditional on a row
    /// having existed).
    ///
    /// The edges are located through the collection's ADJACENCY namespaces —
    /// endpoint-first re-keyings of the source edge rows (one row per edge
    /// per endpoint; see [`adj_key`]) — so the work is proportional to the
    /// deleted key's degree, not the collection's edge count. Adjacency is
    /// DERIVED state: the edge rows remain the only source of truth and keep
    /// their format. [`Collection::link`] establishes the adjacency on the
    /// collection's first edge write; this function covers the remaining
    /// entry points (the first delete on a legacy pre-adjacency database) by
    /// lazily (re)building inside the caller's transaction — marker absent
    /// or stale-shaped (an unrecognized version) — by clearing both
    /// adjacency namespaces and re-deriving them from the `__edges__` rows.
    /// Because the build shares the caller's transaction, and
    /// `link`/`unlink`/every cascade maintain adjacency in their own
    /// transactions, the build can never race a concurrent edge write (the
    /// store serializes write transactions — the same discipline
    /// `index_resume` enforces for lazy index resumes, achieved here by
    /// keeping the whole lazy build inside one write transaction, which also
    /// makes it atomic: a crash rolls it back and the next use rebuilds).
    ///
    /// If an adjacency ROW fails to decode, the derived state is untrusted:
    /// it is rebuilt from the source edge rows (the full O(E) scan — the
    /// pre-adjacency cascade's own fallback cost) and the cascade re-runs
    /// over the repaired index. Entries removed before the malformed row was
    /// reached were individually well-formed (every decode is
    /// self-validating), and the re-run is idempotent, so the result is
    /// exactly the pre-adjacency two-namespace scan's.
    ///
    /// Memory stays bounded by the page size in every walk.
    pub(crate) fn edges_on_delete_in_txn(
        &self,
        tx: &mut WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        ensure_adjacency_in_txn(tx, collection)?;
        if !cascade_edges_via_adjacency(tx, collection, key)? {
            build_adjacency_in_txn(tx, collection)?;
            cascade_edges_via_adjacency(tx, collection, key)?;
        }
        Ok(())
    }

    /// Every edge in the database as
    /// `(collection, relation, from, to, weight)`, for dump/migrate. Edges of
    /// reserved collections are engine-internal and excluded — as is the
    /// derived adjacency (a dump contains only source-of-truth namespaces;
    /// `load` rebuilds adjacency lazily like any fresh database). Reads the
    /// edge namespaces (and the catalog walk) through `reader`, so a dump
    /// enumerates them on the same snapshot as its records (audit B8).
    pub(crate) fn all_edges_in(
        &self,
        reader: &dyn crate::store::SnapshotReader,
    ) -> Result<Vec<EdgeRecord>> {
        let collections = reader
            .collections()?
            .into_iter()
            .filter(|n| !n.starts_with("__"))
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        for coll in collections {
            let c = self.collection(&coll);
            for (key, value) in reader.scan_prefix(&c.edges_name(), &[])? {
                if let Some((rel, from, to)) = decode_edge_key(&key) {
                    out.push(EdgeRecord {
                        collection: coll.clone(),
                        relation: rel,
                        from,
                        to,
                        weight: decode_weight(&value),
                    });
                }
            }
        }
        Ok(out)
    }
}

/// Whether `collection`'s adjacency namespaces carry a current built-marker
/// (one point-get). The marker is written ONLY by a full build
/// ([`build_adjacency_in_txn`]) and invalidated only by a version change, so
/// its presence always means "complete for the committed edge rows" — the
/// invariant every maintenance write (link/unlink/cascade) relies on to skip
/// its own completeness checks.
fn adjacency_ready_in_txn(tx: &WriteBatch<'_>, collection: &str) -> Result<bool> {
    Ok(
        matches!(tx.get(&adj_out_name(collection), &adjacency_marker_key())?,
            Some(v) if v.as_slice() == ADJACENCY_VERSION),
    )
}

/// Marker present, or build it now (inside the caller's transaction): the
/// lazy-establishment point shared by `link` (first edge write) and the
/// delete cascade (a legacy database whose first operation is a delete).
fn ensure_adjacency_in_txn(tx: &mut WriteBatch<'_>, collection: &str) -> Result<()> {
    if !adjacency_ready_in_txn(tx, collection)? {
        build_adjacency_in_txn(tx, collection)?;
    }
    Ok(())
}

/// (Re)derive `collection`'s adjacency namespaces from its source edge rows,
/// inside the caller's transaction: clear both namespaces, then page through
/// `__edges__` writing each edge `(rel, from, to)` as an OUT row under `from`
/// and an IN row under `to` (value = the edge row's value, verbatim — the
/// adjacency is a pure re-keying). Finally write the built-marker. All in the
/// ONE transaction: atomic against crashes, and serialized against every
/// concurrent edge write by the store's write lock.
fn build_adjacency_in_txn(tx: &mut WriteBatch<'_>, collection: &str) -> Result<()> {
    let (out_ns, in_ns) = (adj_out_name(collection), adj_in_name(collection));
    crate::store::clear_in_txn(tx, &out_ns)?;
    crate::store::clear_in_txn(tx, &in_ns)?;
    let edges_ns = format!("__edges__{collection}");
    let mut start: Vec<u8> = Vec::new();
    loop {
        let page = tx.scan_from(&edges_ns, &start, ADJ_PAGE)?;
        let Some((last, _)) = page.last().cloned() else {
            break;
        };
        for (row, value) in &page {
            // Undecodable edge rows are skipped — exactly what the scan
            // cascade and `all_edges_in` do with them (the source of truth
            // itself is never rewritten here). Uncounted puts: the adjacency
            // namespaces are engine-private and nothing reads their counts.
            if let Some((rel, from, to)) = decode_edge_key(row) {
                tx.put_uncounted(&out_ns, &adj_key(&from, &rel, &to), value)?;
                tx.put_uncounted(&in_ns, &adj_key(&to, &rel, &from), value)?;
            }
        }
        // Resume strictly past everything examined above (the documented
        // cursor-pagination convention: `last_key` + trailing `0` byte).
        start = last;
        start.push(0);
    }
    tx.put(&out_ns, &adjacency_marker_key(), ADJACENCY_VERSION)?;
    Ok(())
}

/// Remove every edge that has `key` as an endpoint, via the adjacency
/// namespaces: the OUT half lists `key`'s outgoing edges, the IN half its
/// incoming ones, and each entry deletes the edge row, its twin in the
/// sibling edge namespace, and BOTH adjacency rows (this one and the twin
/// endpoint's). A self-loop appears in both halves; the second pass's deletes
/// are no-ops. Returns `false` if any adjacency row failed to decode (the
/// caller then rebuilds and re-runs) — everything removed before that point
/// was individually well-formed, so the re-run completes exactly the same
/// final state the old two-namespace scan produced.
fn cascade_edges_via_adjacency(
    tx: &mut WriteBatch<'_>,
    collection: &str,
    key: &[u8],
) -> Result<bool> {
    let out_clean = cascade_adj_half(tx, collection, key, true)?;
    let in_clean = cascade_adj_half(tx, collection, key, false)?;
    Ok(out_clean && in_clean)
}

/// One adjacency half of the delete cascade: page through `ns`'s rows under
/// `key`'s prefix (`out = true` → the OUT namespace, edges FROM `key`;
/// `false` → the IN namespace, edges TO `key`) and drop each edge, its twin,
/// and both adjacency rows. Rows past `key`'s prefix range end the walk.
fn cascade_adj_half(
    tx: &mut WriteBatch<'_>,
    collection: &str,
    key: &[u8],
    out: bool,
) -> Result<bool> {
    let edges_ns = format!("__edges__{collection}");
    let redges_ns = format!("__redges__{collection}");
    let (out_ns, in_ns) = (adj_out_name(collection), adj_in_name(collection));
    let (ns, twin_ns) = if out {
        (&out_ns, &in_ns)
    } else {
        (&in_ns, &out_ns)
    };
    let prefix = adj_prefix(key);
    let mut start = prefix.clone();
    let mut clean = true;
    loop {
        let page = tx.scan_from(ns, &start, ADJ_PAGE)?;
        if page.is_empty() {
            break;
        }
        let mut last: Option<Vec<u8>> = None;
        for (row, _) in &page {
            if !row.starts_with(&prefix) {
                // Sorted past `key`'s range: this half is done.
                return Ok(clean);
            }
            let Some((_, rel, other)) = decode_adj_key(row) else {
                // Malformed derived row: skip; the caller rebuilds and
                // re-runs (entries before this one were well-formed).
                clean = false;
                continue;
            };
            // The edge row itself and its twin (nodes swapped) in the two
            // source namespaces (uncounted — see the module docs: the graph
            // namespaces' maintained counts are write-only bookkeeping).
            let (fwd, rev) = if out {
                (edge_key(&rel, key, &other), edge_key(&rel, &other, key))
            } else {
                (edge_key(&rel, &other, key), edge_key(&rel, key, &other))
            };
            tx.delete_uncounted(&edges_ns, &fwd)?;
            tx.delete_uncounted(&redges_ns, &rev)?;
            // Both adjacency rows (uncounted — engine-private namespaces):
            // this endpoint's and the twin's.
            tx.delete_uncounted(ns, row)?;
            tx.delete_uncounted(twin_ns, &adj_key(&other, &rel, key))?;
            last = Some(row.clone());
        }
        match last {
            // Resume strictly past the last row removed (the batch sees its
            // own deletes, so the pages keep advancing).
            Some(mut l) => {
                l.push(0);
                start = l;
            }
            // A page whose rows were all malformed (none set `last`): the
            // rebuild-and-re-run the caller performs on `clean = false`
            // covers whatever follows.
            None => break,
        }
    }
    Ok(clean)
}

/// One graph edge in portable form (dump/migrate).
pub(crate) struct EdgeRecord {
    pub collection: String,
    pub relation: String,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub weight: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        ADJ_PAGE, ADJ_TAG_META, ADJ_TAG_ROW, ADJACENCY_VERSION, adj_in_name, adj_key, adj_out_name,
        adj_prefix, adjacency_marker_key, decode_adj_key, neighbor_prefix,
    };
    use crate::{Db, Value};

    #[test]
    fn link_and_neighbors() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "knows", b"b").unwrap();
        c.link(b"a", "knows", b"c").unwrap();
        assert_eq!(
            c.neighbors(b"a", "knows").unwrap(),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn relations_are_isolated() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "knows", b"b").unwrap();
        c.link(b"a", "likes", b"x").unwrap();
        assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
        assert_eq!(c.neighbors(b"a", "likes").unwrap(), vec![b"x".to_vec()]);
    }

    #[test]
    fn link_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        c.link(b"a", "r", b"b").unwrap();
        assert_eq!(c.neighbors(b"a", "r").unwrap().len(), 1);
    }

    #[test]
    fn unlink_on_reserved_collection_is_rejected() {
        let db = Db::open_in_memory().unwrap();
        // The edge namespace itself must not be writable through the public API.
        let err = db.collection("__edges__nodes").unlink(b"a", "r", b"b");
        assert!(matches!(err, Err(crate::Error::ReservedCollection(_))));
    }

    #[test]
    fn in_neighbors_finds_incoming_edges() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "knows", b"x").unwrap();
        c.link(b"b", "knows", b"x").unwrap();
        c.link(b"a", "knows", b"y").unwrap();
        // Who knows x? a and b.
        assert_eq!(
            c.in_neighbors(b"x", "knows").unwrap(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert_eq!(c.in_neighbors(b"y", "knows").unwrap(), vec![b"a".to_vec()]);
        assert!(c.in_neighbors(b"nobody", "knows").unwrap().is_empty());
    }

    #[test]
    fn weighted_edges_carry_weight() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link_weighted(b"a", "rel", b"b", 0.8).unwrap();
        c.link_weighted(b"a", "rel", b"c", 0.2).unwrap();
        c.link(b"a", "rel", b"d").unwrap(); // unweighted -> 1.0
        let weighted = c.neighbors_weighted(b"a", "rel").unwrap();
        assert_eq!(weighted.len(), 3);
        let w: std::collections::HashMap<_, _> = weighted.into_iter().collect();
        assert!((w[&b"b".to_vec()] - 0.8).abs() < 1e-9);
        assert!((w[&b"c".to_vec()] - 0.2).abs() < 1e-9);
        assert!((w[&b"d".to_vec()] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn unlink_removes_reverse_edge_too() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "knows", b"x").unwrap();
        c.unlink(b"a", "knows", b"x").unwrap();
        assert!(c.in_neighbors(b"x", "knows").unwrap().is_empty());
    }

    #[test]
    fn unlink_removes_edge() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        assert!(c.unlink(b"a", "r", b"b").unwrap());
        assert!(c.neighbors(b"a", "r").unwrap().is_empty());
        assert!(!c.unlink(b"a", "r", b"b").unwrap());
    }

    #[test]
    fn neighbors_of_unknown_is_empty() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.collection("nodes")
                .neighbors(b"ghost", "r")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn traverse_follows_multiple_hops() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        c.link(b"b", "r", b"c").unwrap();
        c.link(b"c", "r", b"d").unwrap();
        assert_eq!(
            c.traverse(b"a", "r", 2).unwrap(),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            c.traverse(b"a", "r", 10).unwrap(),
            vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn traverse_zero_hops_is_empty() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        assert!(c.traverse(b"a", "r", 0).unwrap().is_empty());
    }

    #[test]
    fn traverse_terminates_on_cycles() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        c.link(b"b", "r", b"a").unwrap();
        // Should not loop forever; visits b, then a is already seen.
        assert_eq!(c.traverse(b"a", "r", 100).unwrap(), vec![b"b".to_vec()]);
    }

    #[test]
    fn traverse_branches() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.link(b"a", "r", b"b").unwrap();
        c.link(b"a", "r", b"c").unwrap();
        c.link(b"b", "r", b"d").unwrap();
        let two = c.traverse(b"a", "r", 2).unwrap();
        assert!(two.contains(&b"b".to_vec()));
        assert!(two.contains(&b"c".to_vec()));
        assert!(two.contains(&b"d".to_vec()));
        assert_eq!(two.len(), 3);
    }

    /// Audit B4: deleting a document removes every edge that has it as an
    /// endpoint — both directions, forward AND reverse rows, across every
    /// relation — while edges and documents it never touched survive. The
    /// delete itself still emits its own document Delete event.
    #[test]
    fn document_delete_cascades_edges() {
        use crate::reactive::{ChangeEvent, ChangeKind};
        use std::sync::{Arc, Mutex};

        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        for k in [&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..]] {
            c.insert(k, &Value::Int(1)).unwrap();
        }
        // a is the source of edges under two relations, the target of d's
        // edge, and b->c is a bystander that must survive.
        c.link(b"a", "knows", b"b").unwrap();
        c.link(b"a", "likes", b"c").unwrap();
        c.link(b"d", "knows", b"a").unwrap();
        c.link(b"b", "knows", b"c").unwrap();

        // Subscribe after linking: the only event left to see is the delete's.
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

        assert!(c.delete(b"a").unwrap());
        assert_eq!(
            *events.lock().unwrap(),
            vec![ChangeEvent {
                collection: "nodes".to_owned(),
                key: b"a".to_vec(),
                kind: ChangeKind::Delete,
            }]
        );

        // Outgoing edges (every relation) and incoming edges are gone.
        assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
        assert!(c.neighbors(b"a", "likes").unwrap().is_empty());
        assert!(c.in_neighbors(b"a", "knows").unwrap().is_empty());
        // d's only edge pointed at a; traversing from d reaches nothing.
        assert!(c.neighbors(b"d", "knows").unwrap().is_empty());
        assert!(c.traverse(b"d", "knows", 5).unwrap().is_empty());
        // Edge rows are absent in BOTH namespaces: a's key ranges scan empty,
        // and so does the range keyed by c that held the reverse TWIN of
        // a's a-likes->c edge (the twin's first node is c, so it is not
        // covered by a's own prefix scans above).
        for (rel, ns, node) in [
            ("knows", "__edges__nodes", &b"a"[..]),
            ("likes", "__edges__nodes", &b"a"[..]),
            ("knows", "__redges__nodes", &b"a"[..]),
            ("likes", "__redges__nodes", &b"c"[..]),
        ] {
            assert!(
                db.store()
                    .scan_prefix(ns, &neighbor_prefix(rel, node))
                    .unwrap()
                    .is_empty()
            );
        }
        // The bystander edge and the b/c/d documents are unaffected.
        assert_eq!(c.neighbors(b"b", "knows").unwrap(), vec![b"c".to_vec()]);
        assert_eq!(c.in_neighbors(b"c", "knows").unwrap(), vec![b"b".to_vec()]);
        for k in [&b"b"[..], &b"c"[..], &b"d"[..]] {
            assert_eq!(c.get(k).unwrap(), Some(Value::Int(1)));
        }
    }

    /// The conditional-delete path cascades edges just like a plain delete.
    #[test]
    fn conditional_delete_cascades_edges() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.insert(b"a", &Value::Int(1)).unwrap();
        c.insert(b"b", &Value::Int(2)).unwrap();
        c.link(b"a", "r", b"b").unwrap();
        assert!(c.compare_and_set(b"a", Some(&Value::Int(1)), None).unwrap());
        assert!(c.neighbors(b"a", "r").unwrap().is_empty());
        assert!(c.in_neighbors(b"b", "r").unwrap().is_empty());
    }

    /// Audit B4: link/link_weighted emit an Insert change event (keyed by the
    /// from-key) after the commit; unlink emits Delete, and only when an edge
    /// was actually removed.
    #[test]
    fn link_and_unlink_emit_change_events() {
        use crate::reactive::{ChangeEvent, ChangeKind};
        use std::sync::{Arc, Mutex};

        let db = Db::open_in_memory().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

        let c = db.collection("nodes");
        c.link(b"a", "knows", b"b").unwrap();
        {
            let evs = events.lock().unwrap();
            assert_eq!(
                *evs,
                vec![ChangeEvent {
                    collection: "nodes".to_owned(),
                    key: b"a".to_vec(),
                    kind: ChangeKind::Insert,
                }]
            );
        }

        c.link_weighted(b"a", "trusts", b"c", 0.5).unwrap();
        {
            let evs = events.lock().unwrap();
            assert_eq!(evs.len(), 2);
            assert_eq!(
                evs[1],
                ChangeEvent {
                    collection: "nodes".to_owned(),
                    key: b"a".to_vec(),
                    kind: ChangeKind::Insert,
                }
            );
        }

        assert!(c.unlink(b"a", "knows", b"b").unwrap());
        {
            let evs = events.lock().unwrap();
            assert_eq!(evs.len(), 3);
            assert_eq!(
                evs[2],
                ChangeEvent {
                    collection: "nodes".to_owned(),
                    key: b"a".to_vec(),
                    kind: ChangeKind::Delete,
                }
            );
        }

        // A failed unlink (no such edge) emits nothing.
        assert!(!c.unlink(b"a", "knows", b"b").unwrap());
        assert_eq!(events.lock().unwrap().len(), 3);
    }

    // ---- adjacency-derived cascade (Task 7) ----

    /// The adjacency key codec round-trips every shape: empty endpoint,
    /// empty relation, empty other, unicode, and byte-prefix pairs — and is
    /// the exact inverse of `adj_key` (endpoint-first, tag-prefixed).
    #[test]
    fn adj_key_round_trips_all_shapes() {
        for (endpoint, rel, other) in [
            (&b"a"[..], "knows", &b"b"[..]),
            (&b""[..], "", &b""[..]),
            (&b""[..], "r", &b"x"[..]),
            (&b"x"[..], "", &b""[..]),
            ("ключ".as_bytes(), "знает", "鍵".as_bytes()),
            (&b"k"[..], "know", &b"b"[..]),
            (&b"k"[..], "knows", &b"b"[..]),
        ] {
            let k = adj_key(endpoint, rel, other);
            assert_eq!(
                decode_adj_key(&k),
                Some((endpoint.to_vec(), rel.to_owned(), other.to_vec())),
                "round-trip {endpoint:?}/{rel:?}/{other:?}"
            );
            // The endpoint prefix really is a prefix of its own rows only.
            assert!(k.starts_with(&adj_prefix(endpoint)));
        }
        // Truncated and mistagged keys decode to None (the corruption
        // fallback shape), never panic.
        for bad in [
            vec![ADJ_TAG_ROW],
            vec![ADJ_TAG_ROW, 0, 0, 0, 5, b'a'], // endpoint length overruns
            vec![ADJ_TAG_ROW, 0, 0, 0, 1, b'a', 0, 0, 0, 9, b'r'], // rel overruns
            vec![ADJ_TAG_META, 0, 0, 0, 1, b'a', 0, 0, 0, 1, b'r', b'b'], // meta tag
            vec![ADJ_TAG_ROW, 0, 0, 0, 1, b'a', 0, 0, 0, 1, 0xFF], // non-UTF-8 rel
        ] {
            assert!(decode_adj_key(&bad).is_none(), "must be None: {bad:02x?}");
        }
    }

    /// One delete builds the adjacency (marker present, rows derived from
    /// the source edge rows); link/unlink after the build keep it exact
    /// (their rows land and vanish transactionally), so later cascades —
    /// including the twin-adjacency rows on OTHER endpoints — stay correct.
    #[test]
    fn adjacency_built_once_then_maintained_transactionally() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        for k in [&b"a"[..], &b"b"[..], &b"c"[..]] {
            c.insert(k, &Value::Int(1)).unwrap();
        }
        c.link(b"a", "r", b"b").unwrap();
        c.link(b"c", "r", b"a").unwrap();
        assert!(c.delete(b"b").unwrap()); // builds adjacency + marker

        let (out_ns, in_ns) = (adj_out_name("nodes"), adj_in_name("nodes"));
        assert_eq!(
            db.store().get(&out_ns, &adjacency_marker_key()).unwrap(),
            Some(ADJACENCY_VERSION.to_vec())
        );
        // Derived rows exactly mirror the surviving edge.
        assert_eq!(
            db.store().scan(&out_ns).unwrap(),
            vec![
                (adjacency_marker_key().to_vec(), ADJACENCY_VERSION.to_vec()),
                (adj_key(b"c", "r", b"a"), Vec::new()),
            ]
        );
        assert_eq!(
            db.store().scan(&in_ns).unwrap(),
            vec![(adj_key(b"a", "r", b"c"), Vec::new())]
        );

        // Post-build maintenance: a new edge lands in adjacency with its
        // weight value verbatim...
        c.link_weighted(b"a", "w", b"c", 0.5).unwrap();
        assert_eq!(
            db.store().get(&out_ns, &adj_key(b"a", "w", b"c")).unwrap(),
            Some(0.5f64.to_le_bytes().to_vec())
        );
        assert_eq!(
            db.store().get(&in_ns, &adj_key(b"c", "w", b"a")).unwrap(),
            Some(0.5f64.to_le_bytes().to_vec())
        );
        // ...unlink removes both adjacency rows...
        assert!(c.unlink(b"a", "w", b"c").unwrap());
        assert_eq!(
            db.store().get(&out_ns, &adj_key(b"a", "w", b"c")).unwrap(),
            None
        );
        assert_eq!(
            db.store().get(&in_ns, &adj_key(b"c", "w", b"a")).unwrap(),
            None
        );

        // ...and the next cascade removes a's remaining edges from BOTH
        // endpoints' views (a's IN row for c's edge was maintained, not
        // rebuilt).
        assert!(c.delete(b"a").unwrap());
        assert!(c.neighbors(b"c", "r").unwrap().is_empty());
        assert_eq!(
            db.store().scan(&out_ns).unwrap(),
            vec![(adjacency_marker_key().to_vec(), ADJACENCY_VERSION.to_vec())]
        );
        assert!(db.store().scan(&in_ns).unwrap().is_empty());
    }

    /// A marker carrying an unrecognized version (a stale-shaped adjacency
    /// from a future layout) forces a rebuild inside the deleting
    /// transaction; the cascade is still exact.
    #[test]
    fn stale_shaped_adjacency_marker_forces_rebuild() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.insert(b"a", &Value::Int(1)).unwrap();
        c.insert(b"b", &Value::Int(2)).unwrap();
        c.link(b"a", "r", b"b").unwrap();
        assert!(c.delete(b"a").unwrap()); // builds marker v1
        // Forge a future-version marker.
        db.store()
            .put(&adj_out_name("nodes"), &adjacency_marker_key(), b"9")
            .unwrap();
        // Re-link an edge and delete its endpoint: the stale marker forces
        // the rebuild, which sees the CURRENT edge rows.
        c.link(b"b", "r", b"a").unwrap();
        assert!(c.delete(b"b").unwrap());
        assert!(c.in_neighbors(b"a", "r").unwrap().is_empty());
        assert_eq!(
            db.store()
                .get(&adj_out_name("nodes"), &adjacency_marker_key())
                .unwrap(),
            Some(ADJACENCY_VERSION.to_vec()),
            "the rebuild must restore the current version marker"
        );
    }

    /// A corrupted adjacency row (starts with the endpoint's prefix but does
    /// not decode) makes the derived state untrusted: the cascade still
    /// removes EVERY real edge of the endpoint — the malformed row cannot
    /// hide one — and the adjacency is repaired in the same transaction
    /// (rebuilt from the source rows; no malformed rows remain).
    #[test]
    fn corrupt_adjacency_row_self_heals_and_cascade_stays_exact() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.insert(b"a", &Value::Int(1)).unwrap();
        c.insert(b"b", &Value::Int(2)).unwrap();
        c.link(b"a", "r1", b"b").unwrap();
        c.link(b"a", "r2", b"b").unwrap();
        c.link(b"b", "r3", b"a").unwrap();
        // Build the adjacency with an edge-free delete (a no-edge key's
        // cascade still runs the lazy build).
        assert!(!c.delete(b"zzz").unwrap());
        // Corrupt: insert a malformed row under a's OUT prefix that sorts
        // BEFORE the real rows (a relation length that overruns the key)...
        let mut bad = adj_prefix(b"a");
        bad.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0]);
        db.store().put(&adj_out_name("nodes"), &bad, b"").unwrap();
        // ...and one under a's IN prefix that sorts AFTER them (non-UTF-8
        // relation bytes).
        let mut bad2 = adj_prefix(b"a");
        bad2.extend_from_slice(&[0, 0, 0, 1, b'z', 0xFF]);
        db.store().put(&adj_in_name("nodes"), &bad2, b"").unwrap();

        assert!(c.delete(b"a").unwrap());
        // Every real edge of a is gone in both namespaces and both views.
        assert!(db.store().scan("__edges__nodes").unwrap().is_empty());
        assert!(db.store().scan("__redges__nodes").unwrap().is_empty());
        // The adjacency was repaired: only the marker remains.
        assert_eq!(
            db.store().scan(&adj_out_name("nodes")).unwrap(),
            vec![(adjacency_marker_key().to_vec(), ADJACENCY_VERSION.to_vec())]
        );
        assert!(db.store().scan(&adj_in_name("nodes")).unwrap().is_empty());
    }

    /// An endpoint with more than one adjacency PAGE of edges (1024-row
    /// pages) cascades completely: the paged walk resumes past each deleted
    /// row and never misses the tail. Relations and directions are mixed so
    /// both halves page.
    #[test]
    fn adjacency_cascades_multi_page_endpoints() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("nodes");
        c.insert(b"hub", &Value::Int(0)).unwrap();
        let n = ADJ_PAGE + 37;
        for i in 0..n {
            let k = format!("s{i:05}").into_bytes();
            c.insert(&k, &Value::Int(i as i64)).unwrap();
            c.link(&k, if i % 2 == 0 { "even" } else { "odd" }, b"hub")
                .unwrap();
            if i % 3 == 0 {
                c.link(b"hub", "back", &k).unwrap();
            }
        }
        // Delete a source first so the adjacency is built before the hub's
        // multi-page cascade.
        assert!(c.delete(&format!("s{:05}", 0).into_bytes()).unwrap());
        // The hub: OUT half pages over the "back" edges; IN half over n-1
        // rows.
        assert!(c.delete(b"hub").unwrap());
        for i in 0..n {
            let k = format!("s{i:05}").into_bytes();
            assert!(
                c.neighbors(&k, "even").unwrap().is_empty()
                    && c.neighbors(&k, "odd").unwrap().is_empty(),
                "source {i} still sees its edge to the hub"
            );
            assert!(c.in_neighbors(&k, "back").unwrap().is_empty());
        }
        // Nothing survives in either source namespace.
        assert!(db.store().scan("__edges__nodes").unwrap().is_empty());
        assert!(db.store().scan("__redges__nodes").unwrap().is_empty());
    }
}
