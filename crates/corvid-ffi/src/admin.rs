//! Admin (spec §4.13) — dump/load/backup/compact over paths.
//!
//! The FFI opens the files itself (`std::fs::File`) and hands them to
//! the engine's generic `Read`/`Write` methods. `corvid_compact` is the
//! FFI-only `CORVID_E_BUSY` call site: it gates on the derived-handle
//! counter ([`crate::handle::DbHandle::is_exclusive`]). Paths follow the
//! ABI's UTF-8 rule (spec §1.5). Lands with Task 6.
