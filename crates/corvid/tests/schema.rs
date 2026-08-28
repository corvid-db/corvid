//! Schema/index conformance (skeleton). Task 11 fills this file with the
//! full matrix: every index family (creation, re-creation, index-vs-scan
//! equivalence, mutation maintenance), unique constraints (NaN rule,
//! containers), name validation. This smoke test anchors the radar's
//! test-existence check.

use std::collections::BTreeMap;

use corvid::schema::{Field, FieldType, Schema};
use corvid::{Db, Error, Value};

fn doc(n: i64, tag: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("n".to_owned(), Value::Int(n));
    m.insert("tag".to_owned(), Value::Text(tag.to_owned()));
    Value::Map(m)
}

#[test]
fn schema_smoke_set_schema_rejects_bad_documents() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.set_schema(
        &Schema::new()
            .field(Field::new("n", FieldType::Int).required())
            .field(Field::new("tag", FieldType::Text)),
    )
    .unwrap();

    // A conforming document is accepted.
    c.insert(b"ok", &doc(1, "x")).unwrap();
    assert_eq!(c.get(b"ok").unwrap(), Some(doc(1, "x")));

    // A required field missing is a schema violation, and nothing is stored.
    let mut missing = BTreeMap::new();
    missing.insert("tag".to_owned(), Value::Text("no n".into()));
    let err = c.insert(b"bad1", &Value::Map(missing)).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)));
    assert_eq!(c.get(b"bad1").unwrap(), None);

    // A wrong-typed field is a schema violation too.
    let mut wrong_ty = BTreeMap::new();
    wrong_ty.insert("n".to_owned(), Value::Text("not an int".into()));
    wrong_ty.insert("tag".to_owned(), Value::Text("x".into()));
    let err = c.insert(b"bad2", &Value::Map(wrong_ty)).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)));
    assert_eq!(c.get(b"bad2").unwrap(), None);
}
