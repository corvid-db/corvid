//! End-to-end integration test: a small RAG-style corpus exercised through the
//! public API only, including persistence across reopen. This validates the
//! composed system (storage → documents → hybrid search → builder) the way the
//! MCP sidecar will actually use it.

use std::collections::BTreeMap;

use corvid::{Db, Metric, Value, field};

/// Build a document with a category, body text, and an embedding.
fn doc(category: &str, body: &str, embedding: Vec<f32>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("category".to_owned(), Value::Text(category.to_owned()));
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    m.insert("embedding".to_owned(), Value::Vector(embedding));
    Value::Map(m)
}

fn seed(db: &Db) {
    let c = db.collection("docs");
    c.insert(
        b"rust-db",
        &doc("blog", "rust embedded database design", vec![1.0, 0.0, 0.0]),
    )
    .unwrap();
    c.insert(
        b"rust-async",
        &doc("blog", "async rust patterns", vec![0.9, 0.1, 0.0]),
    )
    .unwrap();
    c.insert(
        b"py-web",
        &doc("blog", "python web frameworks", vec![0.0, 1.0, 0.0]),
    )
    .unwrap();
    c.insert(
        b"news-rust",
        &doc("news", "rust 2.0 release notes", vec![0.8, 0.0, 0.2]),
    )
    .unwrap();
}

#[test]
fn hybrid_query_with_filter_finds_the_right_document() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);

    // "Find blog posts about an embedded rust database" — vector + text, scoped
    // to the blog category.
    let rows = db
        .collection("docs")
        .query()
        .filter(field("category").eq(Value::Text("blog".into())))
        .vector("embedding", vec![1.0, 0.0, 0.0], 10, Metric::Cosine)
        .text("body", "embedded database", 10)
        .limit(3)
        .run()
        .unwrap();

    // The embedded-database blog post wins both signals.
    assert_eq!(rows[0].key, b"rust-db".to_vec());
    // The "news" doc is excluded by the filter even though it's rust-related.
    assert!(!rows.iter().any(|r| r.key == b"news-rust".to_vec()));
}

#[test]
fn vector_search_alone_matches_baseline_ordering() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let hits = db
        .collection("docs")
        .vector_search("embedding", &[1.0, 0.0, 0.0], 2, Metric::Cosine)
        .unwrap();
    assert_eq!(hits[0].key, b"rust-db".to_vec());
    assert_eq!(hits[1].key, b"rust-async".to_vec());
}

#[test]
fn projection_returns_only_requested_fields() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let rows = db
        .collection("docs")
        .query()
        .text("body", "rust", 10)
        .select(["category"])
        .run()
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        if let Value::Map(m) = row.document {
            assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec!["category"]);
        } else {
            panic!("expected projected map");
        }
    }
}

#[test]
fn data_and_queries_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    {
        let db = Db::open(&path).unwrap();
        seed(&db);
    }
    let db = Db::open(&path).unwrap();
    let rows = db
        .collection("docs")
        .query()
        .text("body", "rust database", 10)
        .run()
        .unwrap();
    assert_eq!(rows[0].key, b"rust-db".to_vec());
}

#[test]
fn updates_are_reflected_in_subsequent_queries() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let c = db.collection("docs");

    // Re-embed an existing document so it now matches a different query.
    c.insert(
        b"py-web",
        &doc("blog", "python web frameworks", vec![1.0, 0.0, 0.0]),
    )
    .unwrap();

    let hits = c
        .vector_search("embedding", &[1.0, 0.0, 0.0], 10, Metric::Cosine)
        .unwrap();
    // py-web now sits among the nearest, tied with rust-db at distance ~0.
    assert!(hits.iter().take(2).any(|h| h.key == b"py-web".to_vec()));

    // Deleting it removes it from results.
    assert!(c.delete(b"py-web").unwrap());
    let hits = c
        .vector_search("embedding", &[1.0, 0.0, 0.0], 10, Metric::Cosine)
        .unwrap();
    assert!(!hits.iter().any(|h| h.key == b"py-web".to_vec()));
}
