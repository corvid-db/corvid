//! Join conformance (skeleton). Task 6 fills this file with the full matrix:
//! dotted foreign-key paths, missing FK fields, dangling references,
//! self-joins, empty sides, JoinRow shape. This smoke test anchors the
//! radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn person(name: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".to_owned(), Value::Text(name.to_owned()));
    Value::Map(m)
}

fn post(author_id: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("author_id".to_owned(), Value::Text(author_id.to_owned()));
    Value::Map(m)
}

#[test]
fn joins_smoke_left_outer_resolves_and_misses() {
    let db = Db::open_in_memory().unwrap();
    let authors = db.collection("authors");
    authors.insert(b"ada", &person("Ada")).unwrap();
    let posts = db.collection("posts");
    posts.insert(b"p1", &post("ada")).unwrap();
    posts.insert(b"p2", &post("ghost")).unwrap();
    let mut no_fk = BTreeMap::new();
    no_fk.insert("title".to_owned(), Value::Text("no author field".into()));
    posts.insert(b"p3", &Value::Map(no_fk)).unwrap();

    let rows = posts.join("authors", "author_id").unwrap();
    assert_eq!(rows.len(), 3);
    // In the left collection's key order.
    assert_eq!(rows[0].key, b"p1".to_vec());
    assert_eq!(rows[1].key, b"p2".to_vec());
    assert_eq!(rows[2].key, b"p3".to_vec());
    // Resolved foreign key.
    assert_eq!(rows[0].right, Some(person("Ada")));
    // Dangling reference and missing field both yield a left-outer None.
    assert_eq!(rows[1].right, None);
    assert_eq!(rows[2].right, None);
    assert_eq!(
        rows[0].left.get("author_id"),
        Some(&Value::Text("ada".into()))
    );
}
