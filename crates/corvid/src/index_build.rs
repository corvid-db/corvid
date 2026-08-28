//! Atomic index-creation state (audit A2): one codec and one backfill driver
//! shared by every persisted index kind.
//!
//! A persisted index definition carries a creation state so a crash or error
//! between registration and backfill completion can never leave a permanently
//! partial index that queries trust. Every backfill page commits its index
//! writes together with an advanced cursor in ONE transaction; completion is
//! its own final transaction. Writes maintain every index — building or
//! complete — inside the document transaction; index entries are keyed by
//! encoded value ‖ doc key, so backfill and maintenance overlap safely
//! (idempotent upserts). Queries never serve from a `Building` index.
//!
//! Legacy defs (no state marker) decode as `Complete`; a malformed state
//! decodes as `Building` with an empty cursor (backfill restarts — safe).

use crate::error::Result;
use crate::store::Store;

/// Marker byte starting every new-format def value. Never a valid legacy
/// first byte: legacy scalar/compound/geo defs are empty, text kind bytes are
/// 0/1, vector def bytes are small metric/quant/kind tags.
const NEW_FORMAT: u8 = 0xFF;
const TAG_COMPLETE: u8 = 0;
const TAG_BUILDING: u8 = 1;
/// Backfill page size (documents per transaction).
const PAGE: usize = 2048;

pub(crate) enum DefState {
    Complete,
    Building { cursor: Vec<u8> },
}

pub(crate) fn encode_def(kind_bytes: &[u8], state: &DefState) -> Vec<u8> {
    let mut out = Vec::with_capacity(kind_bytes.len() + 10);
    out.push(NEW_FORMAT);
    match state {
        DefState::Complete => out.push(TAG_COMPLETE),
        DefState::Building { cursor } => {
            out.push(TAG_BUILDING);
            out.extend_from_slice(&(cursor.len() as u32).to_be_bytes());
            out.extend_from_slice(cursor);
        }
    }
    out.extend_from_slice(kind_bytes);
    out
}

/// Decode a def-row value into `(kind_bytes, state)`. Empty and non-`0xFF`
/// values are legacy `Complete` with the whole value as kind bytes; a
/// malformed `0xFF` form decodes as `Building { cursor: vec![] }`.
pub(crate) fn decode_def(value: &[u8]) -> (Vec<u8>, DefState) {
    if value.first() != Some(&NEW_FORMAT) {
        return (value.to_vec(), DefState::Complete);
    }
    let rest = &value[1..];
    match rest.first() {
        Some(&TAG_COMPLETE) => (rest[1..].to_vec(), DefState::Complete),
        Some(&TAG_BUILDING) => {
            let body = &rest[1..];
            let len = u32::from_be_bytes(
                body.get(0..4)
                    .and_then(|b| b.try_into().ok())
                    .unwrap_or([0u8; 4]),
            ) as usize;
            // The length is untrusted (audit C1): slice without a `4 + len`
            // addition — which a near-`u32::MAX` len can overflow on 32-bit
            // targets — while keeping the exact old semantics of "cursor
            // must fit in the remaining bytes". (A plain `len.min(len-4)`
            // clamp would instead trust the whole body as a cursor and skip
            // the backfill; the filter keeps overruns on the restart arm.)
            match body.get(4..).filter(|tail| tail.len() >= len) {
                Some(tail) => {
                    let (cursor, kind) = tail.split_at(len);
                    (
                        kind.to_vec(),
                        DefState::Building {
                            cursor: cursor.to_vec(),
                        },
                    )
                }
                // Truncated cursor: restart the backfill from the beginning.
                None => (Vec::new(), DefState::Building { cursor: Vec::new() }),
            }
        }
        _ => (Vec::new(), DefState::Building { cursor: Vec::new() }),
    }
}

/// The def row's current cursor iff it exists and is `Building`. Read via
/// `reader`, so an in-snapshot re-check (e.g. the ANN registry-lag guard)
/// stays on the caller's snapshot (audit B3).
pub(crate) fn read_building_cursor(
    reader: &dyn crate::store::SnapshotReader,
    defs_ns: &str,
    def_key: &[u8],
) -> Result<Option<Vec<u8>>> {
    Ok(match reader.get(defs_ns, def_key)? {
        Some(v) => match decode_def(&v).1 {
            DefState::Building { cursor } => Some(cursor),
            DefState::Complete => None,
        },
        None => None,
    })
}

/// Test failpoint: `CORVID_TEST_ABORT_AFTER_PAGES=n` aborts the process after
/// `n` committed backfill pages (simulates a crash mid-creation). Unset → off.
fn abort_after_pages() -> Option<usize> {
    static N: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("CORVID_TEST_ABORT_AFTER_PAGES")
            .ok()
            .and_then(|v| v.parse().ok())
    })
}

/// Drive one atomic backfill over `collection` starting at `start_cursor`.
/// Each page's `index_page` writes and the cursor advance commit in ONE
/// transaction; a final transaction marks the def `Complete`.
#[allow(clippy::type_complexity)]
pub(crate) fn run_atomic_backfill(
    store: &Store,
    collection: &str,
    defs_ns: &str,
    def_key: &[u8],
    kind_bytes: &[u8],
    start_cursor: &[u8],
    index_page: &mut dyn FnMut(
        &mut crate::store::WriteBatch<'_>,
        &[(Vec<u8>, Vec<u8>)],
    ) -> Result<()>,
) -> Result<()> {
    let mut cursor = start_cursor.to_vec();
    let mut committed_pages = 0usize;
    loop {
        let page = store.scan_from(collection, &cursor, PAGE)?;
        let Some((last_key, _)) = page.last() else {
            break;
        };
        let mut next = last_key.clone();
        next.push(0);
        let def_value = encode_def(
            kind_bytes,
            &DefState::Building {
                cursor: next.clone(),
            },
        );
        store.transaction(|tx| {
            index_page(tx, &page)?;
            tx.put(defs_ns, def_key, &def_value)?;
            Ok(())
        })?;
        cursor = next;
        committed_pages += 1;
        if let Some(n) = abort_after_pages()
            && committed_pages >= n
        {
            std::process::abort();
        }
    }
    let complete = encode_def(kind_bytes, &DefState::Complete);
    store.transaction(|tx| tx.put(defs_ns, def_key, &complete))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_values_decode_complete_with_kind_bytes_intact() {
        let (kb, st) = decode_def(b"");
        assert!(kb.is_empty());
        assert!(matches!(st, DefState::Complete));
        let (kb, st) = decode_def(&[1]); // legacy on-disk text kind byte
        assert_eq!(kb, vec![1]);
        assert!(matches!(st, DefState::Complete));
        let (kb, st) = decode_def(&[7, 0, 1]); // legacy vector def [metric, quant, kind]
        assert_eq!(kb, vec![7, 0, 1]);
        assert!(matches!(st, DefState::Complete));
    }

    #[test]
    fn building_round_trips_with_cursor_and_kind_bytes() {
        for cursor in [vec![], vec![0u8], b"long-cursor-key-42".to_vec()] {
            let enc = encode_def(
                &[1, 2, 3],
                &DefState::Building {
                    cursor: cursor.clone(),
                },
            );
            let (kb, st) = decode_def(&enc);
            assert_eq!(kb, vec![1, 2, 3]);
            match st {
                DefState::Building { cursor: c } => assert_eq!(c, cursor),
                DefState::Complete => panic!("expected Building"),
            }
        }
        let enc = encode_def(&[9], &DefState::Complete);
        let (kb, st) = decode_def(&enc);
        assert_eq!(kb, vec![9]);
        assert!(matches!(st, DefState::Complete));
    }

    #[test]
    fn malformed_state_decodes_as_building_from_scratch() {
        // Every malformed input must decode as Building with an empty cursor:
        // the backfill restarts from scratch, so the worst case is wasted
        // work, never a trusted partial index.
        let cases: Vec<(Vec<u8>, &str)> = vec![
            // Truncated 0xFF form: tag says Building but cursor length overruns.
            (vec![0xFFu8, 1, 0, 0, 0, 9, 1, 2], "cursor length overrun"),
            // Bare marker: no tag byte at all.
            (vec![0xFFu8], "bare marker"),
            // Garbage tag: neither Complete (0) nor Building (1).
            (vec![0xFFu8, 2, 1, 2, 3], "garbage tag"),
            // Truncated length prefix: fewer than 4 bytes after the tag.
            (vec![0xFFu8, 1, 0, 0], "truncated length prefix"),
            // Cursor length near u32::MAX: the range computation must not
            // overflow (32-bit targets) and the overrun still restarts.
            (
                vec![0xFFu8, 1, 0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3],
                "cursor length near u32::MAX",
            ),
        ];
        for (bad, what) in cases {
            let (kb, st) = decode_def(&bad);
            assert!(kb.is_empty(), "{what}: kind bytes must be empty");
            match st {
                DefState::Building { cursor } => {
                    assert!(cursor.is_empty(), "{what}: cursor must be empty")
                }
                DefState::Complete => panic!("{what}: malformed must not decode Complete"),
            }
        }
    }

    #[test]
    fn backfill_commits_pages_and_completes() {
        let s = crate::store::Store::open_in_memory().unwrap();
        for i in 0..10u8 {
            s.put("docs", &[i], &[i]).unwrap();
        }
        let mut pages: Vec<Vec<Vec<u8>>> = Vec::new();
        run_atomic_backfill(
            &s,
            "docs",
            "__tdefs__",
            b"docs\x00f",
            &[5],
            b"",
            &mut |tx, page| {
                for (k, _) in page {
                    tx.put("__tix__", k, b"x")?;
                }
                pages.push(page.iter().map(|(k, _)| k.clone()).collect());
                Ok(())
            },
        )
        .unwrap();
        // One page (10 docs < 2048), then Complete.
        assert_eq!(pages.len(), 1);
        let (_, st) = decode_def(&s.get("__tdefs__", b"docs\x00f").unwrap().unwrap());
        assert!(matches!(st, DefState::Complete));
        assert_eq!(s.scan("__tix__").unwrap().len(), 10);
        // Resume from a mid-cursor only processes the remainder.
        let mut seen = 0usize;
        run_atomic_backfill(
            &s,
            "docs",
            "__tdefs__",
            b"docs\x00f",
            &[5],
            &[4, 0],
            &mut |_, page| {
                seen += page.len();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, 5); // keys 5..=9
        assert!(matches!(
            decode_def(&s.get("__tdefs__", b"docs\x00f").unwrap().unwrap()).1,
            DefState::Complete
        ));
    }

    #[test]
    fn backfill_error_leaves_building_cursor_at_last_good_page() {
        let s = crate::store::Store::open_in_memory().unwrap();
        for i in 0..6u8 {
            s.put("docs", &[i], &[i]).unwrap();
        }
        let r = run_atomic_backfill(
            &s,
            "docs",
            "__tdefs__",
            b"docs\x00f",
            &[],
            b"",
            &mut |tx, page| {
                for (k, _) in page {
                    tx.put("__tix__", k, b"x")?;
                }
                if page.iter().any(|(k, _)| k == &vec![3u8]) {
                    return Err(crate::Error::Storage(redb::StorageError::Corrupted(
                        "boom".into(),
                    )));
                }
                Ok(())
            },
        );
        assert!(r.is_err());
        // Single page means the failed txn rolled back entirely: no def row yet.
        // With a fresh driver call the failpoint story is the crash test's job.
        let row = s.get("__tdefs__", b"docs\x00f").unwrap();
        assert!(row.is_none() || matches!(decode_def(&row.unwrap()).1, DefState::Building { .. }));
        assert_eq!(s.scan("__tix__").unwrap().len(), 0); // page txn rolled back
    }
}
