//! Graph family (spec §4.11) — the 7 edge/neighborhood functions.
//!
//! A directed property graph over document keys in the same database,
//! indexed by relation. All wrap `corvid::Collection` methods (graph.rs):
//! `link` is idempotent with default weight 1.0 (a plain link overwrites
//! a prior weighted edge's weight), `unlink` removes the edge and its
//! reverse atomically (`*removed_out` reports the forward edge — false is
//! not an error), and endpoints need not exist as documents. `neighbors`
//! / `in_neighbors` / `traverse` return `corvid_strs*` cursors (the
//! plumbing in [`crate::strs`]) — endpoints in **key order** (the engine
//! contract, both adjacency and scan backings byte-identical), `traverse`
//! in **BFS order** (`hops == 0` yields nothing, cycles terminate, one
//! read snapshot covers the walk). `neighbors_weighted` reuses the
//! geohits cursor shape: `distance_km` carries the edge weight (1.0 for
//! unweighted edges) and `doc_out` reads NULL (spec §4.12's documented
//! shape for that producer).

use std::ffi::c_char;

use crate::error::corvid_status;
use crate::error::guard;
use crate::error::record_argument;
use crate::handle::GeoHitsHandle;
use crate::handle::StrsHandle;
use crate::handle::borrow_coll;
use crate::handle::corvid_coll;
use crate::handle::corvid_geohits;
use crate::handle::corvid_strs;
use crate::handle::into_geohits;
use crate::handle::into_strs;
use crate::value::borrowed_bytes;
use crate::value::borrowed_utf8;

/// The §7 NULL-checked shared coll borrow (the read.rs/mutation.rs twin,
/// local to this module like its siblings).
fn borrow_coll_checked<'a>(
    fn_name: &str,
    c: *mut corvid_coll,
) -> Option<&'a crate::handle::CollHandle> {
    if c.is_null() {
        record_argument(&format!("{fn_name}: c is NULL"));
        return None;
    }
    // SAFETY: c is non-NULL (checked) with corvid_collection provenance,
    // not yet freed; the coll family is thread-safe (spec §2), so a
    // shared borrow is fine.
    unsafe { borrow_coll(c) }
}

/// Add a directed edge `from --relation--> to` (spec §4.11; counterpart:
/// `Collection::link`). Idempotent; the default weight is 1.0 — a plain
/// link OVERWRITES a prior weighted edge's weight. Keys are borrowed
/// bytes (any content, §1.5); `relation` is borrowed UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_link(
    c: *mut corvid_coll,
    from: *const u8,
    from_len: usize,
    relation: *const c_char,
    rel_len: usize,
    to: *const u8,
    to_len: usize,
) -> corvid_status {
    let (Some(coll), Some(from), Some(relation), Some(to)) = (
        borrow_coll_checked("corvid_link", c),
        borrowed_bytes("corvid_link", "from", from, from_len),
        borrowed_utf8("corvid_link", "relation", relation, rel_len),
        borrowed_bytes("corvid_link", "to", to, to_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_link", || coll.collection().link(from, relation, to)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Add a directed edge carrying a `weight` (spec §4.11; counterpart:
/// `Collection::link_weighted`) — readable back through
/// [`corvid_neighbors_weighted`]. A later plain [`corvid_link`] of the
/// same edge overwrites the weight with 1.0.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_link_weighted(
    c: *mut corvid_coll,
    from: *const u8,
    from_len: usize,
    relation: *const c_char,
    rel_len: usize,
    to: *const u8,
    to_len: usize,
    weight: f64,
) -> corvid_status {
    let (Some(coll), Some(from), Some(relation), Some(to)) = (
        borrow_coll_checked("corvid_link_weighted", c),
        borrowed_bytes("corvid_link_weighted", "from", from, from_len),
        borrowed_utf8("corvid_link_weighted", "relation", relation, rel_len),
        borrowed_bytes("corvid_link_weighted", "to", to, to_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_link_weighted", || {
        coll.collection().link_weighted(from, relation, to, weight)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Remove the edge (and its reverse) atomically (spec §4.11;
/// counterpart: `Collection::unlink -> bool`). `*removed_out` (nullable)
/// reports whether the FORWARD edge existed — false is not an error.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_unlink(
    c: *mut corvid_coll,
    from: *const u8,
    from_len: usize,
    relation: *const c_char,
    rel_len: usize,
    to: *const u8,
    to_len: usize,
    removed_out: *mut std::ffi::c_int,
) -> corvid_status {
    let (Some(coll), Some(from), Some(relation), Some(to)) = (
        borrow_coll_checked("corvid_unlink", c),
        borrowed_bytes("corvid_unlink", "from", from, from_len),
        borrowed_utf8("corvid_unlink", "relation", relation, rel_len),
        borrowed_bytes("corvid_unlink", "to", to, to_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_unlink", || {
        coll.collection().unlink(from, relation, to)
    }) {
        Some(removed) => {
            if !removed_out.is_null() {
                // SAFETY: removed_out is non-NULL (checked); one c_int
                // store, the optional-out-param shape of corvid_delete.
                unsafe { *removed_out = removed as std::ffi::c_int };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

/// The targets of every `from --relation--> ?` edge, in key order, as a
/// strs cursor (spec §4.11; counterpart: `Collection::neighbors ->
/// Vec<Vec<u8>>`). Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_neighbors(
    c: *mut corvid_coll,
    from: *const u8,
    from_len: usize,
    relation: *const c_char,
    rel_len: usize,
) -> *mut corvid_strs {
    let (Some(coll), Some(from), Some(relation)) = (
        borrow_coll_checked("corvid_neighbors", c),
        borrowed_bytes("corvid_neighbors", "from", from, from_len),
        borrowed_utf8("corvid_neighbors", "relation", relation, rel_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_neighbors", || {
        coll.collection().neighbors(from, relation)
    }) {
        Some(targets) => into_strs(StrsHandle::new(targets)),
        None => std::ptr::null_mut(),
    }
}

/// The sources of every `? --relation--> to` edge, in key order, as a
/// strs cursor (spec §4.11; counterpart: `Collection::in_neighbors ->
/// Vec<Vec<u8>>`). Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_in_neighbors(
    c: *mut corvid_coll,
    to: *const u8,
    to_len: usize,
    relation: *const c_char,
    rel_len: usize,
) -> *mut corvid_strs {
    let (Some(coll), Some(to), Some(relation)) = (
        borrow_coll_checked("corvid_in_neighbors", c),
        borrowed_bytes("corvid_in_neighbors", "to", to, to_len),
        borrowed_utf8("corvid_in_neighbors", "relation", relation, rel_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_in_neighbors", || {
        coll.collection().in_neighbors(to, relation)
    }) {
        Some(sources) => into_strs(StrsHandle::new(sources)),
        None => std::ptr::null_mut(),
    }
}

/// `(target, weight)` for every `from --relation--> ?` edge (spec
/// §4.11; counterpart: `Collection::neighbors_weighted ->
/// Vec<(Vec<u8>, f64)>`) — the `(key, double)` shape REUSES the geohits
/// cursor: `distance_km` carries the edge weight (1.0 for unweighted
/// edges) and `corvid_geohits_next` reports `doc_out = NULL` for these
/// cursors (spec §4.12's documented shape). Results in key order. Returns
/// NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_neighbors_weighted(
    c: *mut corvid_coll,
    from: *const u8,
    from_len: usize,
    relation: *const c_char,
    rel_len: usize,
) -> *mut corvid_geohits {
    let (Some(coll), Some(from), Some(relation)) = (
        borrow_coll_checked("corvid_neighbors_weighted", c),
        borrowed_bytes("corvid_neighbors_weighted", "from", from, from_len),
        borrowed_utf8("corvid_neighbors_weighted", "relation", relation, rel_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_neighbors_weighted", || {
        coll.collection().neighbors_weighted(from, relation)
    }) {
        Some(pairs) => into_geohits(GeoHitsHandle::from_weighted(pairs)),
        None => std::ptr::null_mut(),
    }
}

/// Breadth-first traversal following `relation` up to `hops` hops from
/// `start` (spec §4.11; counterpart: `Collection::traverse`): the
/// reachable nodes EXCLUDING `start`, each once, in BFS order; `hops ==
/// 0` yields nothing; cycles terminate. One read snapshot covers the
/// walk. Returns NULL + error on failure.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_traverse(
    c: *mut corvid_coll,
    start: *const u8,
    start_len: usize,
    relation: *const c_char,
    rel_len: usize,
    hops: usize,
) -> *mut corvid_strs {
    let (Some(coll), Some(start), Some(relation)) = (
        borrow_coll_checked("corvid_traverse", c),
        borrowed_bytes("corvid_traverse", "start", start, start_len),
        borrowed_utf8("corvid_traverse", "relation", relation, rel_len),
    ) else {
        return std::ptr::null_mut();
    };
    match guard("corvid_traverse", || {
        coll.collection().traverse(start, relation, hops)
    }) {
        Some(reached) => into_strs(StrsHandle::new(reached)),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_err;
    use crate::error::corvid_status::CORVID_ERR;
    use crate::error::corvid_status::CORVID_OK;
    use crate::error::last_code;
    use crate::geo::corvid_geohit;
    use crate::geo::corvid_geohits_next;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open_memory;
    use crate::strs::corvid_strs_free;
    use crate::strs::corvid_strs_next;

    type Coll = *mut corvid_coll;

    /// (pointer, length) for a borrowed UTF-8 parameter (§1.5).
    fn s(text: &str) -> (*const c_char, usize) {
        (text.as_ptr() as *const c_char, text.len())
    }

    fn fresh() -> (*mut crate::handle::corvid_db, Coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let (name, len) = s("net");
        let coll = corvid_collection(db, name, len);
        assert!(!coll.is_null());
        (db, coll)
    }

    fn link(coll: Coll, from: &[u8], relation: &str, to: &[u8]) {
        let (rel, rel_len) = s(relation);
        assert_eq!(
            corvid_link(
                coll,
                from.as_ptr(),
                from.len(),
                rel,
                rel_len,
                to.as_ptr(),
                to.len()
            ),
            CORVID_OK
        );
    }

    fn link_weighted(coll: Coll, from: &[u8], relation: &str, to: &[u8], weight: f64) {
        let (rel, rel_len) = s(relation);
        assert_eq!(
            corvid_link_weighted(
                coll,
                from.as_ptr(),
                from.len(),
                rel,
                rel_len,
                to.as_ptr(),
                to.len(),
                weight
            ),
            CORVID_OK
        );
    }

    /// Walk a strs cursor, collecting the byte strings.
    fn strs_items(cursor: *mut corvid_strs) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut ptr: *const c_char = std::ptr::null();
            let mut len = 0usize;
            if corvid_strs_next(cursor, &mut ptr, &mut len) != 1 {
                return out;
            }
            // SAFETY: ptr borrows the cursor's current item, read before
            // the next corvid_strs_next.
            out.push(unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }.to_vec());
        }
    }

    /// The edge corpus: a→b, a→c, b→d, c→d, d→a (a cycle back), plus an
    /// isolated e→f under another relation.
    fn net(coll: Coll) -> Coll {
        link(coll, b"a", "r", b"b");
        link(coll, b"a", "r", b"c");
        link(coll, b"b", "r", b"d");
        link(coll, b"c", "r", b"d");
        link(coll, b"d", "r", b"a");
        link(coll, b"e", "other", b"f");
        coll
    }

    /// Neighbors answer in key order; in_neighbors is the reverse
    /// direction; endpoints need not exist as documents; unlink removes
    /// atomically and reports the forward edge exactly.
    #[test]
    fn neighbors_in_neighbors_and_unlink_pin_key_order() {
        let (db, coll) = fresh();
        net(coll);

        let (rel, rel_len) = s("r");
        let a = corvid_neighbors(coll, b"a".as_ptr(), 1, rel, rel_len);
        assert!(!a.is_null());
        assert_eq!(
            strs_items(a),
            vec![b"b".to_vec(), b"c".to_vec()],
            "key order (b before c)"
        );
        corvid_strs_free(a);

        // The isolated relation does not leak into r's neighborhood.
        let e = corvid_neighbors(coll, b"e".as_ptr(), 1, rel, rel_len);
        assert_eq!(strs_items(e), Vec::<Vec<u8>>::new());
        corvid_strs_free(e);

        // In-neighbors: who links TO d (b and c), key order.
        let d_in = corvid_in_neighbors(coll, b"d".as_ptr(), 1, rel, rel_len);
        assert!(!d_in.is_null());
        assert_eq!(strs_items(d_in), vec![b"b".to_vec(), b"c".to_vec()]);
        corvid_strs_free(d_in);

        // Endpoints need not exist as documents (nothing was inserted).
        let ghost = corvid_neighbors(coll, b"zzz".as_ptr(), 3, rel, rel_len);
        assert!(!ghost.is_null(), "absent node: empty cursor, not error");
        assert!(strs_items(ghost).is_empty());
        corvid_strs_free(ghost);

        // unlink: removed_out reports the forward edge; a second unlink
        // reports false (not an error).
        let mut removed: std::ffi::c_int = -1;
        assert_eq!(
            corvid_unlink(
                coll,
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"c".as_ptr(),
                1,
                &mut removed
            ),
            CORVID_OK
        );
        assert_eq!(removed, 1);
        assert_eq!(
            corvid_unlink(
                coll,
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"c".as_ptr(),
                1,
                &mut removed
            ),
            CORVID_OK
        );
        assert_eq!(removed, 0, "already gone: false is not an error");
        // removed_out is optional (§7).
        assert_eq!(
            corvid_unlink(
                coll,
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"b".as_ptr(),
                1,
                std::ptr::null_mut()
            ),
            CORVID_OK
        );
        // Both removals are reflected.
        let a = corvid_neighbors(coll, b"a".as_ptr(), 1, rel, rel_len);
        assert!(strs_items(a).is_empty());
        corvid_strs_free(a);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// traverse: BFS order pinned (frontier by frontier, key order
    /// within a hop by the engine's expansion), start excluded, each
    /// node once, cycles terminate, `hops == 0` yields nothing.
    #[test]
    fn traverse_pins_bfs_order_and_cycle_termination() {
        let (db, coll) = fresh();
        net(coll);
        let (rel, rel_len) = s("r");

        // hops 0: nothing.
        let zero = corvid_traverse(coll, b"a".as_ptr(), 1, rel, rel_len, 0);
        assert!(!zero.is_null());
        assert!(strs_items(zero).is_empty());
        corvid_strs_free(zero);

        // hops 1: the start's out-edges, key order.
        let one = corvid_traverse(coll, b"a".as_ptr(), 1, rel, rel_len, 1);
        assert_eq!(strs_items(one), vec![b"b".to_vec(), b"c".to_vec()]);
        corvid_strs_free(one);

        // hops 2: b's and c's out-edges appended AFTER hop 1 — d appears
        // once even though both b and c reach it; a is excluded (the
        // cycle d→a terminates).
        let two = corvid_traverse(coll, b"a".as_ptr(), 1, rel, rel_len, 2);
        assert_eq!(
            strs_items(two),
            vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()],
            "BFS order: hop-1 frontier first, d once, start excluded"
        );
        corvid_strs_free(two);

        // Unbounded (hops past the diameter): same set, same order —
        // the cycle back to a adds nothing.
        let far = corvid_traverse(coll, b"a".as_ptr(), 1, rel, rel_len, 99);
        assert_eq!(
            strs_items(far),
            vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
        corvid_strs_free(far);

        // The other relation stays separate.
        let other = {
            let (o, o_len) = s("other");
            corvid_traverse(coll, b"e".as_ptr(), 1, o, o_len, 5)
        };
        assert_eq!(strs_items(other), vec![b"f".to_vec()]);
        corvid_strs_free(other);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// neighbors_weighted: the geohits reuse — `distance_km` carries the
    /// weight (1.0 for plain links), `doc_out` reads NULL, plain `link`
    /// overwrites a weighted edge's weight with 1.0.
    #[test]
    fn neighbors_weighted_reuses_geohits_with_doc_null() {
        let (db, coll) = fresh();
        let (rel, rel_len) = s("r");

        link_weighted(coll, b"a", "r", b"b", 0.25);
        link(coll, b"a", "r", b"c"); // plain: default 1.0
        link_weighted(coll, b"a", "r", b"d", 4.5);

        let hits = corvid_neighbors_weighted(coll, b"a".as_ptr(), 1, rel, rel_len);
        assert!(!hits.is_null());
        let mut seen: Vec<(Vec<u8>, f64)> = Vec::new();
        loop {
            let mut hit = corvid_geohit {
                key: std::ptr::null(),
                key_len: 0,
                distance_km: 0.0,
            };
            let mut doc: *const crate::handle::corvid_value = std::ptr::null();
            if corvid_geohits_next(hits, &mut hit, &mut doc) != 1 {
                break;
            }
            assert!(doc.is_null(), "spec §4.12: doc_out is NULL here");
            // SAFETY: hit.key borrows the cursor's current entry, read
            // before the next corvid_geohits_next.
            let key = unsafe { std::slice::from_raw_parts(hit.key, hit.key_len) }.to_vec();
            seen.push((key, hit.distance_km));
        }
        assert_eq!(
            seen,
            vec![
                (b"b".to_vec(), 0.25),
                (b"c".to_vec(), 1.0),
                (b"d".to_vec(), 4.5),
            ],
            "key order; plain link = 1.0 default"
        );
        crate::geo::corvid_geohits_free(hits);

        // A plain re-link overwrites the weighted edge's weight.
        link(coll, b"a", "r", b"b");
        let hits = corvid_neighbors_weighted(coll, b"a".as_ptr(), 1, rel, rel_len);
        assert!(!hits.is_null());
        let mut hit = corvid_geohit {
            key: std::ptr::null(),
            key_len: 0,
            distance_km: 0.0,
        };
        let mut doc: *const crate::handle::corvid_value = std::ptr::null();
        assert_eq!(corvid_geohits_next(hits, &mut hit, &mut doc), 1);
        assert_eq!(hit.distance_km, 1.0, "plain link resets the weight");
        crate::geo::corvid_geohits_free(hits);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// The family's §7 discipline: every NULL pointer answers the
    /// signature's failure shape with `CORVID_E_ARGUMENT` recorded.
    #[test]
    fn graph_family_null_discipline() {
        let (db, coll) = fresh();
        let (rel, rel_len) = s("r");

        // status fns: NULL coll / from / relation / to.
        assert_eq!(
            corvid_link(
                std::ptr::null_mut(),
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"b".as_ptr(),
                1
            ),
            CORVID_ERR
        );
        assert_eq!(
            corvid_link(coll, std::ptr::null(), 0, rel, rel_len, b"b".as_ptr(), 1),
            CORVID_ERR
        );
        assert_eq!(
            corvid_link(
                coll,
                b"a".as_ptr(),
                1,
                std::ptr::null(),
                0,
                b"b".as_ptr(),
                1
            ),
            CORVID_ERR
        );
        assert_eq!(
            corvid_link(coll, b"a".as_ptr(), 1, rel, rel_len, std::ptr::null(), 0),
            CORVID_ERR
        );
        assert_eq!(
            corvid_link_weighted(
                std::ptr::null_mut(),
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"b".as_ptr(),
                1,
                1.0
            ),
            CORVID_ERR
        );
        assert_eq!(
            corvid_unlink(
                std::ptr::null_mut(),
                b"a".as_ptr(),
                1,
                rel,
                rel_len,
                b"b".as_ptr(),
                1,
                std::ptr::null_mut()
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // non-UTF-8 relation.
        let bad = [0xFF_u8];
        assert_eq!(
            corvid_link(
                coll,
                b"a".as_ptr(),
                1,
                bad.as_ptr() as *const c_char,
                1,
                b"b".as_ptr(),
                1
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // cursor fns: NULL anything → NULL cursor + E_ARGUMENT.
        assert!(corvid_neighbors(std::ptr::null_mut(), b"a".as_ptr(), 1, rel, rel_len).is_null());
        assert!(corvid_neighbors(coll, std::ptr::null(), 0, rel, rel_len).is_null());
        assert!(
            corvid_in_neighbors(std::ptr::null_mut(), b"a".as_ptr(), 1, rel, rel_len).is_null()
        );
        assert!(
            corvid_neighbors_weighted(std::ptr::null_mut(), b"a".as_ptr(), 1, rel, rel_len)
                .is_null()
        );
        assert!(corvid_traverse(std::ptr::null_mut(), b"a".as_ptr(), 1, rel, rel_len, 1).is_null());
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }
}
