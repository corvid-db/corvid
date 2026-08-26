//! Cross-collection lookup joins.
//!
//! The common shape: each document in one collection holds a foreign key into
//! another (e.g. a `post` with an `author_id` that is a key in `authors`).
//! [`Collection::join`] resolves that reference for every document, producing a
//! left-outer join — the right side is `None` when the field is missing, isn't
//! a usable key, or has no matching document.

use crate::db::Collection;
use crate::error::Result;
use crate::value::Value;

/// One joined pair: a left document and its resolved right document (if any).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinRow {
    /// The left document's key.
    pub key: Vec<u8>,
    /// The left document.
    pub left: Value,
    /// The matched right document, or `None` (left-outer join).
    pub right: Option<Value>,
}

impl Collection<'_> {
    /// Left-outer join every document in this collection to `other`, using the
    /// value at `foreign_key_field` as the key into `other`. The field may be
    /// a dotted path (`"meta.author_id"`).
    ///
    /// The foreign key must be [`Value::Text`], [`Value::Bytes`], or
    /// [`Value::Int`]; any other shape (or a missing field, or no matching
    /// document) yields `right: None`. Rows are returned in this collection's
    /// key order. Both legs read from one consistent snapshot — the joined
    /// pair reflects a single point in time even under concurrent writes.
    pub fn join(&self, other: &str, foreign_key_field: &str) -> Result<Vec<JoinRow>> {
        // One read transaction serves both the left scan and every right-hand
        // lookup, so a concurrent commit cannot produce torn pairs (old-left
        // joined to new-right).
        self.db().store().read(|reader| {
            let mut rows = Vec::new();
            for (key, bytes) in reader.scan(self.name())? {
                let left_doc = Value::decode(&bytes)?;
                let foreign_key = match left_doc.get_path(foreign_key_field) {
                    Some(Value::Text(s)) => Some(s.clone().into_bytes()),
                    Some(Value::Bytes(b)) => Some(b.clone()),
                    Some(Value::Int(i)) => Some(i.to_string().into_bytes()),
                    _ => None,
                };
                let right = match &foreign_key {
                    Some(fk) => match reader.get(other, fk)? {
                        Some(bytes) => Some(Value::decode(&bytes)?),
                        None => None,
                    },
                    None => None,
                };
                rows.push(JoinRow {
                    key,
                    left: left_doc,
                    right,
                });
            }
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, Value};
    use std::collections::BTreeMap;

    fn post(title: &str, author_id: &str) -> Value {
        let mut m = BTreeMap::new();
        m.insert("title".to_owned(), Value::Text(title.to_owned()));
        m.insert("author_id".to_owned(), Value::Text(author_id.to_owned()));
        Value::Map(m)
    }

    /// Foreign keys may live on nested paths, consistent with every other
    /// field-reference surface.
    #[test]
    fn join_resolves_nested_foreign_key_paths() {
        let db = Db::open_in_memory().unwrap();
        db.collection("authors")
            .insert(b"u1", &Value::Text("Ada".into()))
            .unwrap();
        let mut inner = BTreeMap::new();
        inner.insert("author_id".to_owned(), Value::Text("u1".into()));
        let mut doc = BTreeMap::new();
        doc.insert("meta".to_owned(), Value::Map(inner));
        db.collection("posts")
            .insert(b"p1", &Value::Map(doc))
            .unwrap();
        let rows = db
            .collection("posts")
            .join("authors", "meta.author_id")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].right, Some(Value::Text("Ada".into())));
    }

    fn author(name: &str) -> Value {
        let mut m = BTreeMap::new();
        m.insert("name".to_owned(), Value::Text(name.to_owned()));
        Value::Map(m)
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.collection("authors")
            .insert(b"rocky", &author("Rocky"))
            .unwrap();
        db.collection("posts")
            .insert(b"p1", &post("Hello", "rocky"))
            .unwrap();
        db
    }

    #[test]
    fn join_pairs_left_with_matching_right() {
        let db = seed();
        let rows = db.collection("posts").join("authors", "author_id").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, b"p1".to_vec());
        assert_eq!(rows[0].right, Some(author("Rocky")));
    }

    #[test]
    fn unmatched_foreign_key_yields_none() {
        let db = seed();
        db.collection("posts")
            .insert(b"p2", &post("Orphan", "ghost"))
            .unwrap();
        let rows = db.collection("posts").join("authors", "author_id").unwrap();
        let orphan = rows.iter().find(|r| r.key == b"p2".to_vec()).unwrap();
        assert_eq!(orphan.right, None);
    }

    #[test]
    fn missing_or_non_key_field_yields_none() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("posts");
        // No author_id field at all.
        let mut m = BTreeMap::new();
        m.insert("title".to_owned(), Value::Text("No author".into()));
        c.insert(b"p", &Value::Map(m)).unwrap();
        // author_id present but not a key type.
        let mut m2 = BTreeMap::new();
        m2.insert("author_id".to_owned(), Value::Int(5));
        c.insert(b"q", &Value::Map(m2)).unwrap();

        let rows = c.join("authors", "author_id").unwrap();
        assert!(rows.iter().all(|r| r.right.is_none()));
    }

    #[test]
    fn inner_join_is_filtering_for_some() {
        let db = seed();
        db.collection("posts")
            .insert(b"p2", &post("Orphan", "ghost"))
            .unwrap();
        let rows = db.collection("posts").join("authors", "author_id").unwrap();
        let inner: Vec<_> = rows.into_iter().filter(|r| r.right.is_some()).collect();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].key, b"p1".to_vec());
    }

    #[test]
    fn bytes_foreign_key_works() {
        let db = Db::open_in_memory().unwrap();
        db.collection("authors")
            .insert(b"\x01\x02", &author("Binary"))
            .unwrap();
        let mut m = BTreeMap::new();
        m.insert("author_id".to_owned(), Value::Bytes(vec![1, 2]));
        db.collection("posts").insert(b"p", &Value::Map(m)).unwrap();
        let rows = db.collection("posts").join("authors", "author_id").unwrap();
        assert_eq!(rows[0].right, Some(author("Binary")));
    }
}
