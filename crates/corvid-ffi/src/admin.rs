//! Admin (spec §4.13) — dump/load/backup/compact over paths.
//!
//! The FFI opens the files itself (`std::fs::File`) and hands them to
//! the engine's generic `Read`/`Write` methods: `dump<W: Write>` /
//! `load<R: Read>` / `load_with_renames<R: Read>` (migrate.rs) and
//! `Db::backup(path)` (db.rs). File-open failures are the FFI's own
//! `CORVID_E_IO` (the engine never sees them); everything after the
//! open is the engine's error surface unchanged. Paths follow the ABI's
//! UTF-8 rule (spec §1.5).
//!
//! `corvid_compact` is the FFI-only `CORVID_E_BUSY` call site: the
//! engine's `Db::compact(&mut self)` needs exclusive access, gated by
//! the derived-handle counter AND `Arc` exclusivity (spec §4.13 and
//! handle.rs's `DbHandle::compact` — the Task 5 review prepend: the
//! counter alone is not engine-idle).

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::ffi::c_int;
use std::fs::File;

use crate::error::corvid_err;
use crate::error::corvid_status;
use crate::error::guard;
use crate::error::record;
use crate::error::record_argument;
use crate::handle::borrow_db;
use crate::handle::borrow_db_mut;
use crate::handle::corvid_db;
use crate::index::utf8_array;
use crate::value::borrowed_utf8;

/// The §7 NULL-checked shared db borrow (the lifecycle twin, local to
/// this module).
fn borrow_db_checked<'a>(fn_name: &str, db: *mut corvid_db) -> Option<&'a crate::handle::DbHandle> {
    if db.is_null() {
        record_argument(&format!("{fn_name}: db is NULL"));
        return None;
    }
    // SAFETY: db is non-NULL (checked) with corvid_open/open_memory
    // provenance, not yet closed; the db family is thread-safe (spec
    // §2), so a shared borrow is fine.
    unsafe { borrow_db(db) }
}

/// Borrow a UTF-8 path parameter (§1.5) under `fn_name`.
fn path_of<'a>(fn_name: &str, path: *const c_char, path_len: usize) -> Option<&'a str> {
    borrowed_utf8(fn_name, "path", path, path_len)
}

/// Open a file for READING (the load side); an open failure is the
/// FFI's own `CORVID_E_IO` — the engine never sees it.
fn open_read(fn_name: &str, path: &str) -> Option<File> {
    match File::open(path) {
        Ok(file) => Some(file),
        Err(err) => {
            record(
                corvid_err::CORVID_E_IO,
                format!("{fn_name}: open {path}: {err}"),
            );
            None
        }
    }
}

/// Create (or truncate) a file for WRITING (the dump side); a create
/// failure is the FFI's own `CORVID_E_IO`.
fn create_write(fn_name: &str, path: &str) -> Option<File> {
    match File::create(path) {
        Ok(file) => Some(file),
        Err(err) => {
            record(
                corvid_err::CORVID_E_IO,
                format!("{fn_name}: create {path}: {err}"),
            );
            None
        }
    }
}

/// Write a logical, version-stamped dump of the whole database —
/// documents, index/schema/TTL definitions, graph edges, auto-id
/// counters — to `path`, from one read snapshot (spec §4.13;
/// counterpart: `Db::dump<W: Write>`, migrate.rs; the FFI supplies the
/// `File`).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_dump_to_path(
    db: *mut corvid_db,
    path: *const c_char,
    path_len: usize,
) -> corvid_status {
    let (Some(handle), Some(path)) = (
        borrow_db_checked("corvid_dump_to_path", db),
        path_of("corvid_dump_to_path", path, path_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(file) = create_write("corvid_dump_to_path", path) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_dump_to_path", || handle.engine().dump(file)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Replay a dump into this database (spec §4.13; counterpart:
/// `Db::load<R: Read>` — equivalent to `load_with_renames` with an
/// empty map; loading merges with pre-existing collections per the
/// engine contract).
#[unsafe(no_mangle)]
pub extern "C" fn corvid_load_from_path(
    db: *mut corvid_db,
    path: *const c_char,
    path_len: usize,
) -> corvid_status {
    let (Some(handle), Some(path)) = (
        borrow_db_checked("corvid_load_from_path", db),
        path_of("corvid_load_from_path", path, path_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(file) = open_read("corvid_load_from_path", path) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_load_from_path", || handle.engine().load(file)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Replay a dump with a collection-RENAME map (spec §4.13; counterpart:
/// `Db::load_with_renames(r, &BTreeMap<String, String>)` — the
/// migration path for legacy `__`-containing names): every collection
/// occurrence in the stream lands under the target name. Same engine
/// contract, validated BEFORE the stream is read: an invalid target
/// fails with `CORVID_E_INVALID_NAME`, two-sources-one-target (or a
/// target colliding with another mapped/unmapped dump collection) fails
/// with `CORVID_E_ARGUMENT`. The arrays may be NULL only at
/// `count == 0` (the `pred_in` array rule); every pair is borrowed
/// UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_load_from_path_with_renames(
    db: *mut corvid_db,
    path: *const c_char,
    path_len: usize,
    old_names: *const *const c_char,
    new_names: *const *const c_char,
    old_lens: *const usize,
    new_lens: *const usize,
    count: usize,
) -> corvid_status {
    let (Some(handle), Some(path)) = (
        borrow_db_checked("corvid_load_from_path_with_renames", db),
        path_of("corvid_load_from_path_with_renames", path, path_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    // Collect both arrays up front (nothing partially consumed — the
    // utf8_array discipline), then pair them; a duplicate old name keeps
    // the LAST pair (a BTreeMap::insert, like the Rust API's).
    let Some(old) = utf8_array(
        "corvid_load_from_path_with_renames",
        "old_names",
        old_names,
        old_lens,
        count,
    ) else {
        return corvid_status::CORVID_ERR;
    };
    let Some(new) = utf8_array(
        "corvid_load_from_path_with_renames",
        "new_names",
        new_names,
        new_lens,
        count,
    ) else {
        return corvid_status::CORVID_ERR;
    };
    let renames: BTreeMap<String, String> = old.into_iter().zip(new).collect();
    let Some(file) = open_read("corvid_load_from_path_with_renames", path) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_load_from_path_with_renames", || {
        handle.engine().load_with_renames(file, &renames)
    }) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Consistent point-in-time PHYSICAL backup to a FRESH file at `path`
/// (spec §4.13; counterpart: `Db::backup`) — an existing target fails
/// with `CORVID_E_BACKUP_TARGET_EXISTS`; safe while writers are active.
/// Physical means feature-configuration-dependent — use dump/load to
/// move between feature builds.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_backup(
    db: *mut corvid_db,
    path: *const c_char,
    path_len: usize,
) -> corvid_status {
    let (Some(handle), Some(path)) = (
        borrow_db_checked("corvid_backup", db),
        path_of("corvid_backup", path, path_len),
    ) else {
        return corvid_status::CORVID_ERR;
    };
    match guard("corvid_backup", || handle.engine().backup(path)) {
        Some(()) => corvid_status::CORVID_OK,
        None => corvid_status::CORVID_ERR,
    }
}

/// Reclaim file space after heavy deletes — offline maintenance (spec
/// §4.13; counterpart: `Db::compact(&mut self) -> Result<bool>`). The
/// engine requires EXCLUSIVE access, so this call requires quiescence:
/// every handle derived from this `db` must already be freed, checked by
/// the derived-handle counter AND `Arc` exclusivity (spec §4.13's
/// intent; the Task 5 review prepend — a query's `execute()` releases
/// its count at entry while its `Arc` clone lives, so the counter alone
/// is not engine-idle). Otherwise fails with the FFI-only
/// `CORVID_E_BUSY`. `*moved_out` (nullable) reports whether any data
/// moved. In-memory databases have no file to reclaim: the call
/// succeeds and reports no movement.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_compact(db: *mut corvid_db, moved_out: *mut c_int) -> corvid_status {
    if db.is_null() {
        record_argument("corvid_compact: db is NULL");
        return corvid_status::CORVID_ERR;
    }
    // SAFETY: db is non-NULL (checked) with corvid_open/open_memory
    // provenance, not yet closed. The exclusive borrow is sound only at
    // the quiescent point spec §6 demands of a compacting caller — the
    // gate below makes the engine-side half checkable; the handle-side
    // half is the caller's documented quiescence (the corvid_close
    // discipline).
    let handle = unsafe { borrow_db_mut(db) }.expect("non-NULL checked above");
    match handle.compact() {
        Some(moved) => {
            if !moved_out.is_null() {
                // SAFETY: moved_out is non-NULL (checked); one c_int
                // store, the optional-out-param shape of corvid_unlink.
                unsafe { *moved_out = moved as c_int };
            }
            corvid_status::CORVID_OK
        }
        None => corvid_status::CORVID_ERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::corvid_collection;
    use crate::collection::corvid_collection_free;
    use crate::error::corvid_status::CORVID_ERR;
    use crate::error::corvid_status::CORVID_OK;
    use crate::error::last_code;
    use crate::lifecycle::corvid_close;
    use crate::lifecycle::corvid_open;
    use crate::lifecycle::corvid_open_memory;
    use crate::mutation::corvid_delete;
    use crate::mutation::corvid_insert;
    use crate::query::corvid_query_filter;
    use crate::query::corvid_query_new;
    use crate::query::corvid_query_run;
    use crate::query::corvid_rows_free;
    use crate::query::corvid_rows_next;
    use crate::read::corvid_get;
    use crate::value::corvid_value_as_int;
    use crate::value::corvid_value_free;
    use crate::value::corvid_value_int;
    use crate::value::corvid_value_map_new;
    use crate::value::corvid_value_map_put;
    use crate::value::corvid_value_text;

    /// (pointer, length) for a borrowed UTF-8 parameter (§1.5).
    fn s(text: &str) -> (*const c_char, usize) {
        (text.as_ptr() as *const c_char, text.len())
    }

    /// An open-memory db plus a collection handle named "docs".
    fn fresh() -> (*mut corvid_db, *mut crate::handle::corvid_coll) {
        let db = corvid_open_memory();
        assert!(!db.is_null());
        let (name, len) = s("docs");
        let coll = corvid_collection(db, name, len);
        assert!(!coll.is_null());
        (db, coll)
    }

    fn doc(pairs: &[(&str, *mut crate::handle::corvid_value)]) -> *mut crate::handle::corvid_value {
        let map = corvid_value_map_new();
        for (key, item) in pairs {
            let (key, key_len) = s(key);
            assert_eq!(corvid_value_map_put(map, key, key_len, *item), CORVID_OK);
        }
        map
    }

    fn text_value(text: &str) -> *mut crate::handle::corvid_value {
        let (ptr, len) = s(text);
        corvid_value_text(ptr, len)
    }

    fn insert(
        coll: *mut crate::handle::corvid_coll,
        key: &[u8],
        document: *mut crate::handle::corvid_value,
    ) {
        assert_eq!(
            corvid_insert(coll, key.as_ptr(), key.len(), document),
            CORVID_OK
        );
        corvid_value_free(document);
    }

    /// Walk a rows cursor, collecting keys (the query-family shape).
    fn keys_of(rows: *mut crate::handle::corvid_rows) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut key: *const u8 = std::ptr::null();
            let mut key_len = 0usize;
            let mut d: *const crate::handle::corvid_value = std::ptr::null();
            let mut score = 0f32;
            if corvid_rows_next(rows, &mut key, &mut key_len, &mut d, &mut score) != 1 {
                return out;
            }
            // SAFETY: key borrows the cursor's current row, read before
            // the next corvid_rows_next.
            out.push(unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec());
        }
    }

    /// A tag-eq filter consumed by the call.
    fn tag_eq(tag: &str) -> *mut crate::handle::corvid_pred {
        let (path, path_len) = s("tag");
        crate::pred::corvid_pred_compare(
            path,
            path_len,
            crate::pred::corvid_cmp::CORVID_CMP_EQ,
            text_value(tag),
        )
    }

    /// The dump→load→query equivalence through the ABI: the loaded
    /// database answers the SAME filtered query with the SAME rows.
    #[test]
    fn dump_load_query_equivalence_through_the_abi() {
        let dir = tempfile::tempdir().unwrap();
        let dump_path = dir.path().join("dump.corvid-dump");
        let dump = dump_path.to_str().unwrap();
        let (dump_ptr, dump_len) = s(dump);

        let (db, coll) = fresh();
        insert(
            coll,
            b"a",
            doc(&[("tag", text_value("x")), ("n", corvid_value_int(1))]),
        );
        insert(
            coll,
            b"b",
            doc(&[("tag", text_value("x")), ("n", corvid_value_int(2))]),
        );
        insert(
            coll,
            b"c",
            doc(&[("tag", text_value("y")), ("n", corvid_value_int(3))]),
        );

        assert_eq!(corvid_dump_to_path(db, dump_ptr, dump_len), CORVID_OK);
        assert!(dump_path.exists(), "the dump file was written");

        // A loaded db answers the same query identically (and a load
        // MERGES: the empty target gains everything).
        let (db2, coll2) = fresh();
        assert_eq!(corvid_load_from_path(db2, dump_ptr, dump_len), CORVID_OK);
        for c in [coll, coll2] {
            let q = corvid_query_new(c);
            assert_eq!(corvid_query_filter(q, tag_eq("x")), CORVID_OK);
            let rows = corvid_query_run(q);
            assert_eq!(keys_of(rows), vec![b"a".to_vec(), b"b".to_vec()]);
            corvid_rows_free(rows);
        }

        corvid_collection_free(coll);
        corvid_collection_free(coll2);
        assert_eq!(corvid_close(db), CORVID_OK);
        assert_eq!(corvid_close(db2), CORVID_OK);
    }

    // ---- the hand-crafted legacy dump (the a__b rename path) ----------

    /// v2 dump encoders (migrate.rs's format: u64 length/count
    /// prefixes; section order records → vectors → texts → scalars →
    /// compounds → geos → schemas → ttls → autos → edges).
    fn put_u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
        put_u64(out, b.len() as u64);
        out.extend_from_slice(b);
    }
    fn put_str(out: &mut Vec<u8>, text: &str) {
        put_bytes(out, text.as_bytes());
    }

    /// A minimal dump with ONE record under legacy `a__b` and every
    /// other section empty (the migrate.rs test fixture's shape — the
    /// current engine cannot produce `__`-named collections, so the
    /// rename path's fixture is hand-crafted exactly like the engine's
    /// own test).
    fn legacy_dump() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"CORVIDDUMPv2");
        put_u64(&mut bytes, 1); // one record
        put_str(&mut bytes, "a__b");
        put_bytes(&mut bytes, b"k");
        put_bytes(&mut bytes, &corvid::Value::Int(7).encode());
        for _ in 0..9 {
            put_u64(&mut bytes, 0); // vectors…edges: all empty
        }
        bytes
    }

    fn write_fixture(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// The rename map migrates a legacy `a__b` dump: the record lands
    /// under `a_b` and reads back through the ABI; the engine's
    /// up-front contract holds through the ABI too (invalid target =
    /// `CORVID_E_INVALID_NAME`, two-sources-one-target =
    /// `CORVID_E_ARGUMENT`).
    #[test]
    fn load_with_renames_migrates_a_legacy_dump() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir, "legacy.dump", &legacy_dump());

        let (old, old_len) = s("a__b");
        let (new, new_len) = s("a_b");
        let olds = [old];
        let old_lens = [old_len];
        let news = [new];
        let new_lens = [new_len];
        let (p, pl) = s(&path);

        let (db, _) = fresh();
        assert_eq!(
            corvid_load_from_path_with_renames(
                db,
                p,
                pl,
                olds.as_ptr(),
                news.as_ptr(),
                old_lens.as_ptr(),
                new_lens.as_ptr(),
                1
            ),
            CORVID_OK
        );

        // The record answers under the NEW name through the ABI.
        let (name, name_len) = s("a_b");
        let coll = corvid_collection(db, name, name_len);
        let mut out: *mut crate::handle::corvid_value = std::ptr::null_mut();
        assert_eq!(corvid_get(coll, b"k".as_ptr(), 1, &mut out), CORVID_OK);
        let mut ok = 0;
        assert_eq!(corvid_value_as_int(out, &mut ok), 7);
        assert_eq!(ok, 1);
        corvid_value_free(out);
        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);

        // Invalid target: the engine's up-front InvalidName.
        let (db, _) = fresh();
        let (bad, bad_len) = s("x__y");
        let bads = [bad];
        let bad_lens = [bad_len];
        assert_eq!(
            corvid_load_from_path_with_renames(
                db,
                p,
                pl,
                olds.as_ptr(),
                bads.as_ptr(),
                old_lens.as_ptr(),
                bad_lens.as_ptr(),
                1
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_INVALID_NAME);
        assert_eq!(corvid_close(db), CORVID_OK);

        // Two sources, one target: the injectivity contract.
        let (db, _) = fresh();
        let (c_d, c_d_len) = s("c__d");
        let two_olds = [old, c_d];
        let two_old_lens = [old_len, c_d_len];
        let two_news = [new, new];
        let two_new_lens = [new_len, new_len];
        assert_eq!(
            corvid_load_from_path_with_renames(
                db,
                p,
                pl,
                two_olds.as_ptr(),
                two_news.as_ptr(),
                two_old_lens.as_ptr(),
                two_new_lens.as_ptr(),
                2
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);
        assert_eq!(corvid_close(db), CORVID_OK);
    }

    /// backup produces a FRESH reopenable copy; an existing target is
    /// `CORVID_E_BACKUP_TARGET_EXISTS`; the copy answers through the
    /// ABI.
    #[test]
    fn backup_restores_through_the_abi() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("live.corvid");
        let backup = dir.path().join("copy.corvid");
        let (file_str, backup_str) = (
            file.to_str().unwrap().to_owned(),
            backup.to_str().unwrap().to_owned(),
        );
        let (fb, fl) = s(&file_str);
        let (bb, bl) = s(&backup_str);

        let db = corvid_open(fb, fl);
        assert!(!db.is_null());
        let (name, name_len) = s("docs");
        let coll = corvid_collection(db, name, name_len);
        insert(coll, b"k", doc(&[("n", corvid_value_int(42))]));

        assert_eq!(corvid_backup(db, bb, bl), CORVID_OK);
        // A second backup to the SAME target: the engine's refusal.
        assert_eq!(corvid_backup(db, bb, bl), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_BACKUP_TARGET_EXISTS);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);

        // The backup reopens and answers through the ABI (the map doc
        // reads back through map_get — as_int on a Map is 0/!ok, the
        // value family's wrong-type contract).
        let db2 = corvid_open(bb, bl);
        assert!(!db2.is_null());
        let coll2 = corvid_collection(db2, name, name_len);
        let mut out: *mut crate::handle::corvid_value = std::ptr::null_mut();
        assert_eq!(corvid_get(coll2, b"k".as_ptr(), 1, &mut out), CORVID_OK);
        assert!(!out.is_null(), "the backed-up row is present");
        let (n_field, n_len) = s("n");
        // SAFETY-free borrowed read: map_get hands out a borrowed child.
        let n_val = crate::value::corvid_value_map_get(out, n_field, n_len);
        let mut ok = 0;
        assert_eq!(corvid_value_as_int(n_val, &mut ok), 42);
        assert_eq!(ok, 1);
        corvid_value_free(out);
        corvid_collection_free(coll2);
        assert_eq!(corvid_close(db2), CORVID_OK);
    }

    /// compact's exclusivity gate: a live derived handle answers the
    /// FFI-only `CORVID_E_BUSY`; freeing it lets the call through, with
    /// `*moved_out` written (and nullable).
    #[test]
    fn compact_gates_on_derived_handles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("compact.corvid");
        let (fb, fl) = {
            let text = file.to_str().unwrap();
            let (p, l) = s(text);
            (p, l)
        };

        let db = corvid_open(fb, fl);
        assert!(!db.is_null());
        let (name, name_len) = s("docs");
        let coll = corvid_collection(db, name, name_len);
        insert(coll, b"a", doc(&[("n", corvid_value_int(1))]));
        insert(coll, b"b", doc(&[("n", corvid_value_int(2))]));
        let mut existed = 0;
        assert_eq!(
            corvid_delete(coll, b"b".as_ptr(), 1, &mut existed),
            CORVID_OK
        );
        assert_eq!(existed, 1);

        // Gated: the live coll handle is +1 on the counter.
        let mut moved: c_int = -1;
        assert_eq!(corvid_compact(db, &mut moved), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_BUSY);
        assert_eq!(moved, -1, "no out-param write on the gated path");

        // Ungated: free the coll handle (back to exactly the db handle),
        // compact runs; moved_out is written 0/1 and is optional.
        corvid_collection_free(coll);
        assert_eq!(corvid_compact(db, &mut moved), CORVID_OK);
        assert!(
            moved == 0 || moved == 1,
            "moved_out reports the engine's answer ({moved})"
        );
        assert_eq!(corvid_compact(db, std::ptr::null_mut()), CORVID_OK);

        // The db keeps working after a compact (index/data unchanged).
        let (name2, name2_len) = s("docs");
        let coll2 = corvid_collection(db, name2, name2_len);
        let mut n = 0usize;
        assert_eq!(crate::read::corvid_len(coll2, &mut n), CORVID_OK);
        assert_eq!(n, 1, "compact reclaims space, not data");
        corvid_collection_free(coll2);
        assert_eq!(corvid_close(db), CORVID_OK);

        // An in-memory db compacts trivially (nothing to move).
        let mem = corvid_open_memory();
        assert!(!mem.is_null());
        assert_eq!(corvid_compact(mem, std::ptr::null_mut()), CORVID_OK);
        assert_eq!(corvid_close(mem), CORVID_OK);
    }

    /// The family's §7 discipline: NULL db / NULL path / non-UTF-8 path
    /// / a missing dump file (`CORVID_E_IO`).
    #[test]
    fn admin_family_null_discipline() {
        let (db, coll) = fresh();
        let (path, path_len) = s("/definitely/not/a/real/path/x.dump");

        assert_eq!(
            corvid_dump_to_path(std::ptr::null_mut(), path, path_len),
            CORVID_ERR
        );
        assert_eq!(corvid_dump_to_path(db, std::ptr::null(), 0), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // A dump under a missing parent: the FFI's own E_IO.
        assert_eq!(corvid_dump_to_path(db, path, path_len), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_IO);

        // A load of a nonexistent file: the same E_IO.
        assert_eq!(corvid_load_from_path(db, path, path_len), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_IO);
        assert_eq!(
            corvid_load_from_path_with_renames(
                db,
                path,
                path_len,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0
            ),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_IO);

        // Non-UTF-8 path bytes: §1.5's encoding rule.
        let bad = [0xFF_u8, 0xFE];
        assert_eq!(
            corvid_dump_to_path(db, bad.as_ptr() as *const c_char, bad.len()),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // Renames: NULL arrays at count > 0.
        assert_eq!(
            corvid_load_from_path_with_renames(
                db,
                path,
                path_len,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                1
            ),
            CORVID_ERR,
            "NULL arrays with count > 0"
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        assert_eq!(
            corvid_backup(std::ptr::null_mut(), path, path_len),
            CORVID_ERR
        );
        assert_eq!(
            corvid_compact(std::ptr::null_mut(), std::ptr::null_mut()),
            CORVID_ERR
        );
        assert_eq!(last_code(), corvid_err::CORVID_E_ARGUMENT);

        // compact is gated here too: the coll handle is live.
        assert_eq!(corvid_compact(db, std::ptr::null_mut()), CORVID_ERR);
        assert_eq!(last_code(), corvid_err::CORVID_E_BUSY);

        corvid_collection_free(coll);
        assert_eq!(corvid_close(db), CORVID_OK);
    }
}
