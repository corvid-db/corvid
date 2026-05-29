//! WASM bundle-size harness.
//!
//! This crate exists to prove the engine links into a `wasm32-unknown-unknown`
//! `cdylib` and to measure the resulting bundle size against the budget
//! (< 2 MB gzipped). It is **not** the browser API: a real browser build wraps
//! the engine with `wasm-bindgen` and an OPFS-backed [`StorageBackend`] (a
//! Worker-only concern, per `DESIGN.md`). Here a single exported entry point
//! exercises a representative slice of the engine — store, value codec, the
//! query builder, a vector index, a scalar filter — so the linker retains the
//! code that a real bundle would ship.

use std::collections::BTreeMap;

use corvid::{Db, Metric, Value, field};

/// Exercise the engine end to end and return how many rows the query produced.
///
/// Kept reachable from the cdylib so dead-code elimination does not strip the
/// engine away when measuring bundle size.
#[unsafe(no_mangle)]
pub extern "C" fn corvid_wasm_smoke() -> u32 {
    let Ok(db) = Db::open_in_memory() else {
        return u32::MAX;
    };
    let c = db.collection("docs");
    for i in 0..16u32 {
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(i as i64));
        m.insert("v".to_owned(), Value::Vector(vec![i as f32, 1.0]));
        m.insert("body".to_owned(), Value::Text(format!("doc number {i}")));
        if c.insert(&i.to_le_bytes(), &Value::Map(m)).is_err() {
            return u32::MAX;
        }
    }
    if c.create_scalar_index("n").is_err() {
        return u32::MAX;
    }
    let Ok(rows) = c
        .query()
        .filter(field("n").ge(Value::Int(4)))
        .vector("v", vec![1.0, 0.0], 8, Metric::Cosine)
        .text("body", "doc", 8)
        .limit(8)
        .run()
    else {
        return u32::MAX;
    };
    rows.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_runs_the_engine() {
        // 12 docs match n >= 4 (n in 4..=15); the query limits to 8.
        assert_eq!(corvid_wasm_smoke(), 8);
    }
}
