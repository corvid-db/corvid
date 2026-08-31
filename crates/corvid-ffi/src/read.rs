//! Reads (spec §4.9) — `corvid_get`, `corvid_scan`, `corvid_page`,
//! `corvid_len`.
//!
//! `get` uses the optional-value convention (absence is `CORVID_OK` +
//! `*out == NULL`), `scan` streams through a row callback, `page` is
//! keyset pagination whose `next_after` resume cursor is an ABI-owned
//! buffer freed with `corvid_free` (born in
//! [`crate::lifecycle::buffer_new`]). Lands with Task 4.
