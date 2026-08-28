//! SELECT-shaping conformance (skeleton). Task 5 fills this file with the
//! full matrix: projection (nested paths, missing fields, non-map docs),
//! order_by classes and mixed-type total order, limit/offset boundaries,
//! pagination, scan/for_each/len/count, plan_shape observability. This smoke
//! test anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(name: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

#[test]
fn queries_smoke_order_by_limit_select_shapes_rows() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc("third", 3)).unwrap();
    c.insert(b"b", &doc("first", 1)).unwrap();
    c.insert(b"c", &doc("second", 2)).unwrap();

    let rows = c
        .query()
        .select(["n"])
        .order_by("n", false)
        .limit(2)
        .run()
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].document.get("n"), Some(&Value::Int(1)));
    assert_eq!(rows[1].document.get("n"), Some(&Value::Int(2)));
    // Projection keeps only the selected top-level field.
    assert_eq!(rows[0].document.get("name"), None);
}
