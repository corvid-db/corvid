//! Change-feed conformance (skeleton). Task 12 fills this file with the
//! full matrix: events per mutation path (insert, update, patch,
//! compare_and_set, delete, delete_where, insert_auto, batch ops, TTL purge,
//! link/unlink), unsubscribe semantics, cross-collection isolation. This
//! smoke test anchors the radar's test-existence check.

use std::sync::{Arc, Mutex};

use corvid::{ChangeEvent, ChangeKind, Db, Value};

#[test]
fn events_smoke_subscribe_records_insert_and_delete() {
    let db = Db::open_in_memory().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let id = db.subscribe(move |e: &ChangeEvent| sink.lock().unwrap().push(e.clone()));

    let c = db.collection("docs");
    c.insert(b"k", &Value::Int(1)).unwrap();
    c.delete(b"k").unwrap();

    // A delete of a missing key emits nothing.
    c.delete(b"absent").unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ChangeEvent {
                collection: "docs".to_owned(),
                key: b"k".to_vec(),
                kind: ChangeKind::Insert,
            },
            ChangeEvent {
                collection: "docs".to_owned(),
                key: b"k".to_vec(),
                kind: ChangeKind::Delete,
            },
        ]
    );

    // Unsubscribing stops delivery and reports false on repeat.
    assert!(db.unsubscribe(id));
    c.insert(b"k2", &Value::Int(2)).unwrap();
    assert_eq!(events.lock().unwrap().len(), 2);
    assert!(!db.unsubscribe(id));
}
