//! Query builder, rows cursor, aggregations (spec §4.6/§4.7).
//!
//! A `corvid_query*` holds owned QueryBuilder state; `corvid_query_run`
//! and every aggregate CONSUME it (spec §5 rule 5), mirroring the
//! engine's `QueryBuilder` taking `self`. The rows cursor borrows each
//! row's key/document only until the next `corvid_rows_next`. Lands with
//! Task 5, together with the `corvid_query`/`corvid_rows`/
//! `corvid_groupiter` marker handles.
