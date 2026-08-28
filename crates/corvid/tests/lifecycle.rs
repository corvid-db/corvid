//! Lifecycle conformance (skeleton). Task 13 fills this file with the full
//! matrix: file-backed open/reopen, backup restore, bulk/compact, Store
//! transactions and scans, dump→load round-trip of every Value variant and
//! index family, caches and sketches. This smoke test anchors the radar's
//! test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(name: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

#[test]
fn lifecycle_smoke_dump_load_roundtrips_documents() {
    let source = Db::open_in_memory().unwrap();
    source
        .collection("docs")
        .insert(b"k", &doc("corvid", 8))
        .unwrap();
    source
        .collection("notes")
        .insert(b"n", &Value::Vector(vec![1.0, 2.0]))
        .unwrap();

    let mut buf = Vec::new();
    source.dump(&mut buf).unwrap();
    assert!(!buf.is_empty());

    let target = Db::open_in_memory().unwrap();
    target.load(buf.as_slice()).unwrap();
    assert_eq!(
        target.collection("docs").get(b"k").unwrap(),
        Some(doc("corvid", 8))
    );
    assert_eq!(
        target.collection("notes").get(b"n").unwrap(),
        Some(Value::Vector(vec![1.0, 2.0]))
    );
    let mut names = target.collections().unwrap();
    names.sort();
    assert_eq!(names, vec!["docs".to_owned(), "notes".to_owned()]);
}
