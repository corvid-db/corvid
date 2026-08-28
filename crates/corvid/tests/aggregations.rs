//! Aggregation conformance (skeleton). Task 6 fills this file with the full
//! matrix: count/sum/avg/min/max over every numeric shape (NaN, ±inf, empty,
//! missing-all, mixed), count_distinct and BloomFilter bounds, typed group
//! keys, filters respected. This smoke test anchors the radar's
//! test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value, field};

fn doc(cat: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("cat".to_owned(), Value::Text(cat.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

#[test]
fn aggregations_smoke_sum_group_count_and_count() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc("x", 1)).unwrap();
    c.insert(b"b", &doc("x", 2)).unwrap();
    c.insert(b"c", &doc("y", 10)).unwrap();

    assert_eq!(c.query().sum("n").unwrap(), 13.0);

    let groups = c.query().group_count("cat").unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups.get("x"), Some(&2));
    assert_eq!(groups.get("y"), Some(&1));

    assert_eq!(c.query().count().unwrap(), 3);
    assert_eq!(
        c.query()
            .filter(field("cat").eq(Value::Text("x".into())))
            .count()
            .unwrap(),
        2
    );
}
