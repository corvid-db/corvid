//! Graph conformance (skeleton). Task 7 fills this file with the full
//! matrix: link/link_weighted (duplicate, self-loop, missing endpoints),
//! unlink, neighbors/in_neighbors/neighbors_weighted, traverse (depth 0/N,
//! cycles), delete cascade, edge events. This smoke test anchors the radar's
//! test-existence check.

use corvid::Db;

#[test]
fn graph_smoke_link_neighbors_traverse_unlink() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nodes");

    c.link(b"a", "knows", b"b").unwrap();
    c.link(b"b", "knows", b"c").unwrap();

    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
    assert_eq!(
        c.traverse(b"a", "knows", 2).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec()]
    );

    // Unlink removes the edge (and its reverse twin).
    assert!(c.unlink(b"a", "knows", b"b").unwrap());
    assert!(c.neighbors(b"a", "knows").unwrap().is_empty());
    assert!(!c.unlink(b"a", "knows", b"b").unwrap());
    // The untouched edge survives.
    assert_eq!(c.neighbors(b"b", "knows").unwrap(), vec![b"c".to_vec()]);
}
