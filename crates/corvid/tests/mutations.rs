//! Mutation conformance (skeleton). Task 3 fills this file with the full
//! case matrix (every mutation construct, every error variant, batch
//! atomicity, index maintenance, event emission). This smoke test anchors
//! the radar's test-existence check with real, asserted behavior.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(name: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

#[test]
fn mutations_smoke_insert_roundtrips() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    assert!(c.is_empty().unwrap());

    // Insert then read back: the stored document equals what was written.
    c.insert(b"k1", &doc("corvid", 8)).unwrap();
    assert_eq!(c.get(b"k1").unwrap(), Some(doc("corvid", 8)));
    assert_eq!(c.len().unwrap(), 1);

    // Overwrite is visible on the next read.
    c.insert(b"k1", &doc("corvid", 9)).unwrap();
    assert_eq!(c.get(b"k1").unwrap(), Some(doc("corvid", 9)));

    // Auto keys are distinct and sort in insertion order.
    let a = c.insert_auto(&doc("auto", 1)).unwrap();
    let b = c.insert_auto(&doc("auto", 2)).unwrap();
    assert!(a < b);
    assert_eq!(c.get(&a).unwrap(), Some(doc("auto", 1)));

    // Delete removes exactly the keyed document.
    assert!(c.delete(b"k1").unwrap());
    assert_eq!(c.get(b"k1").unwrap(), None);
    assert!(!c.delete(b"k1").unwrap());
    assert_eq!(c.len().unwrap(), 2);
}
