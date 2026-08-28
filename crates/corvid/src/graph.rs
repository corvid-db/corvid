//! A lightweight directed property graph over document keys.
//!
//! Edges connect keys within a collection under a string relation label and are
//! stored in a sibling edge collection (`__edges__<collection>`) so they share
//! the same transactional store as the documents. Edge keys are length-prefixed
//! `relation ‖ from ‖ to`, which lets [`Collection::neighbors`] resolve all
//! targets of a `(from, relation)` pair with a single prefix scan.
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
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        // Forward and reverse edges commit together — no half-linked state.
        self.db().store().transaction(|tx| {
            tx.put(&forward, &fwd_key, b"")?;
            tx.put(&reverse, &rev_key, b"")?;
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
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let value = weight.to_le_bytes();
        self.db().store().transaction(|tx| {
            tx.put(&forward, &fwd_key, &value)?;
            tx.put(&reverse, &rev_key, &value)?;
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
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let removed = self.db().store().transaction(|tx| {
            let removed = tx.delete(&forward, &fwd_key)?;
            tx.delete(&reverse, &rev_key)?;
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
    /// Edge keys are `len(relation) ‖ relation ‖ len(node) ‖ node ‖ other`, so
    /// an endpoint is *not* a byte prefix of the key: each namespace is paged
    /// through with [`WriteBatch::scan_from`] (the batch sees its own deletes,
    /// so the pages keep advancing) and every row decoded. A forward row whose
    /// source is `key` also removes its reverse twin, and a reverse row whose
    /// target is `key` also removes its forward twin, keeping the namespaces
    /// exact mirrors. Memory stays bounded by the page size; work is
    /// proportional to the collection's edge count, a no-op when there are
    /// none.
    pub(crate) fn edges_on_delete_in_txn(
        &self,
        tx: &mut WriteBatch<'_>,
        collection: &str,
        key: &[u8],
    ) -> Result<()> {
        cascade_edges_of(tx, collection, key, true)?;
        cascade_edges_of(tx, collection, key, false)
    }

    /// Every edge in the database as
    /// `(collection, relation, from, to, weight)`, for dump/migrate. Edges of
    /// reserved collections are engine-internal and excluded. Reads the edge
    /// namespaces (and the catalog walk) through `reader`, so a dump
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

/// One namespace pass of the delete cascade (audit B4): page through `ns` and
/// drop every row whose first node is `key`, along with its twin (the same
/// edge stored with the nodes swapped) in the sibling namespace. With
/// `forward = true` the first node is the edge's source and the twin lives in
/// `__redges__`; with `false` it is the target and the twin lives in
/// `__edges__`.
fn cascade_edges_of(
    tx: &mut WriteBatch<'_>,
    collection: &str,
    key: &[u8],
    forward: bool,
) -> Result<()> {
    const PAGE: usize = 1024;
    let ns = if forward {
        format!("__edges__{collection}")
    } else {
        format!("__redges__{collection}")
    };
    let twin_ns = if forward {
        format!("__redges__{collection}")
    } else {
        format!("__edges__{collection}")
    };
    let mut start: Vec<u8> = Vec::new();
    loop {
        let page = tx.scan_from(&ns, &start, PAGE)?;
        let Some((last, _)) = page.last().cloned() else {
            break;
        };
        for (row, _) in &page {
            // Keep only rows whose first node is `key`.
            if let Some((rel, first, second)) =
                decode_edge_key(row).filter(|(_, from, _)| from.as_slice() == key)
            {
                // The row itself, plus its twin (nodes swapped).
                tx.delete(&ns, row)?;
                tx.delete(&twin_ns, &edge_key(&rel, &second, &first))?;
            }
        }
        // Resume strictly past everything examined above (the documented
        // cursor-pagination convention: `last_key` + trailing `0` byte).
        start = last;
        start.push(0);
    }
    Ok(())
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
    use super::neighbor_prefix;
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
}
