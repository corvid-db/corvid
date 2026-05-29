//! A lightweight directed property graph over document keys.
//!
//! Edges connect keys within a collection under a string relation label and are
//! stored in a sibling edge collection (`__edges__<collection>`) so they share
//! the same transactional store as the documents. Edge keys are length-prefixed
//! `relation ‖ from ‖ to`, which lets [`Collection::neighbors`] resolve all
//! targets of a `(from, relation)` pair with a single prefix scan.
//!
//! This is the traversal core for the agent-memory use case (entity/relation
//! graphs). Graph algorithms beyond neighbor lookup and bounded BFS traversal
//! are intentionally out of scope for now.

use std::collections::HashSet;

use crate::db::Collection;
use crate::error::Result;

impl Collection<'_> {
    /// Add a directed edge `from --relation--> to`. Idempotent. A reverse edge
    /// is stored too, so [`Collection::in_neighbors`] can answer "who links to
    /// `to`?".
    pub fn link(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        // Forward and reverse edges commit together — no half-linked state.
        self.db().store().transaction(|tx| {
            tx.put(&forward, &fwd_key, b"")?;
            tx.put(&reverse, &rev_key, b"")?;
            Ok(())
        })
    }

    /// Add a directed edge carrying a `weight` (e.g. confidence or cost). Like
    /// [`Collection::link`] but the weight is stored on the edge and readable
    /// via [`Collection::neighbors_weighted`].
    pub fn link_weighted(&self, from: &[u8], relation: &str, to: &[u8], weight: f64) -> Result<()> {
        self.ensure_writable()?;
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        let value = weight.to_le_bytes();
        self.db().store().transaction(|tx| {
            tx.put(&forward, &fwd_key, &value)?;
            tx.put(&reverse, &rev_key, &value)?;
            Ok(())
        })
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
    pub fn unlink(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<bool> {
        let (forward, reverse) = (self.edges_name(), self.redges_name());
        let (fwd_key, rev_key) = (edge_key(relation, from, to), edge_key(relation, to, from));
        self.db().store().transaction(|tx| {
            let removed = tx.delete(&forward, &fwd_key)?;
            tx.delete(&reverse, &rev_key)?;
            Ok(removed)
        })
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
    pub fn traverse(&self, start: &[u8], relation: &str, hops: usize) -> Result<Vec<Vec<u8>>> {
        let mut visited: HashSet<Vec<u8>> = HashSet::new();
        visited.insert(start.to_vec());
        let mut frontier = vec![start.to_vec()];
        let mut result = Vec::new();

        for _ in 0..hops {
            let mut next = Vec::new();
            for node in &frontier {
                for to in self.neighbors(node, relation)? {
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

#[cfg(test)]
mod tests {
    use crate::Db;

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
}
