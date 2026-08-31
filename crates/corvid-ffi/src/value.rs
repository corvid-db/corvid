//! Value construction & reads (spec §4.3/§4.4) — the 23 value functions.
//!
//! Constructors return OWNED `corvid_value*` handles and CLONE their
//! byte/text/vector inputs; `_ref` accessors borrow zero-copy;
//! `array_get`/`map_get` borrow children that ride the parent's
//! lifetime; `corvid_value_free` is for OWNED values only. Lands with
//! Task 3, together with the `corvid_value` marker handle.
