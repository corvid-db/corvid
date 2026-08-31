//! Mutations (spec §4.8) — the 13 write functions.
//!
//! All wrap `corvid::Collection` methods; document inputs are CLONED into
//! the engine, `corvid_update` crosses a C fn-ptr callback (spec §1.6's
//! no-reentrancy contract), CAS `expected`/`replacement` are nullable
//! with semantics, `delete_where` consumes its predicate, and TTL writes
//! (insert_with_ttl/set_ttl/get_ttl/purge_expired) take caller-supplied
//! epochs. Lands with Task 4.
