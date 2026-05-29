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
    /// Add a directed edge `from --relation--> to`. Idempotent.
    pub fn link(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        self.db()
            .store()
            .put(&self.edges_name(), &edge_key(relation, from, to), b"")
    }

    /// Remove the edge `from --relation--> to`. Returns whether one existed.
    pub fn unlink(&self, from: &[u8], relation: &str, to: &[u8]) -> Result<bool> {
        self.db()
            .store()
            .delete(&self.edges_name(), &edge_key(relation, from, to))
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

    /// The sibling collection holding this collection's edges.
    fn edges_name(&self) -> String {
        format!("__edges__{}", self.name())
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
