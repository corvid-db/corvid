//! WHERE/filter conformance (skeleton). Task 4 fills this file with the
//! full matrix: every Predicate form × CmpOp × Value kind, missing and
//! nested paths, NaN semantics, indexed-vs-scan equivalence. This smoke test
//! anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value, field};

fn doc(category: &str, n: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("category".to_owned(), Value::Text(category.to_owned()));
    m.insert("n".to_owned(), Value::Int(n));
    Value::Map(m)
}

#[test]
fn filters_smoke_field_eq_selects_matching_rows() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"a", &doc("blog", 1)).unwrap();
    c.insert(b"b", &doc("news", 2)).unwrap();
    c.insert(b"c", &doc("blog", 3)).unwrap();

    let rows = c
        .query()
        .filter(field("category").eq(Value::Text("blog".into())))
        .run()
        .unwrap();

    let mut keys: Vec<&[u8]> = rows.iter().map(|r| r.key.as_slice()).collect();
    keys.sort();
    assert_eq!(keys, vec![&b"a"[..], &b"c"[..]]);
    for r in &rows {
        assert_eq!(
            r.document.get("category"),
            Some(&Value::Text("blog".into()))
        );
        assert_eq!(r.score, 0.0); // a pure filter query carries no rank score
    }
}
