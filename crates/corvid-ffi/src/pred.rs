//! Predicates (spec §4.5) — the 11 `corvid_pred_*` functions.
//!
//! Ten constructors build `corvid::filter::Predicate` trees from dotted
//! field paths; `and`/`or`/not CONSUME their children (spec §5 rule 4),
//! and `corvid_pred_free` is for never-consumed roots only. Lands with
//! Task 4, together with the `corvid_pred` marker handle.
