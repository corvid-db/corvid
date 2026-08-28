//! TTL conformance (skeleton). Task 12 fills this file with the full
//! matrix: set_ttl overwrite/clear, purge boundary semantics, purge
//! idempotence, expired-doc visibility, TTL + index maintenance, errors.
//! This smoke test anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(name: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    Value::Map(m)
}

#[test]
fn ttl_smoke_insert_with_ttl_purges_at_boundary() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");

    c.insert_with_ttl(b"k", &doc("ephemeral"), 100).unwrap();
    assert_eq!(c.ttl(b"k").unwrap(), Some(100));

    // Before the boundary the record is alive; the purge collects nothing.
    assert_eq!(c.purge_expired(99).unwrap(), 0);
    assert_eq!(c.get(b"k").unwrap(), Some(doc("ephemeral")));

    // At the boundary (now == expires_at) the record is due and purged,
    // through the normal delete path.
    assert_eq!(c.purge_expired(100).unwrap(), 1);
    assert_eq!(c.get(b"k").unwrap(), None);

    // Purging again is a no-op.
    assert_eq!(c.purge_expired(100).unwrap(), 0);
}
