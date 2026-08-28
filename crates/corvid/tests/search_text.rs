//! Text-search conformance (skeleton). Task 9 fills this file with the full
//! matrix: BM25 ranking on a fixed corpus, tokenization, phrase_search
//! order-sensitivity, analyzer variants, k bounds. This smoke test anchors
//! the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Value};

fn doc(body: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    Value::Map(m)
}

#[test]
fn search_text_smoke_ranks_most_relevant_first() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"rust-db", &doc("rust embedded database design"))
        .unwrap();
    c.insert(b"py-web", &doc("python web frameworks")).unwrap();
    c.insert(b"rust-async", &doc("async rust patterns"))
        .unwrap();

    let hits = c.text_search("body", "rust database", 2).unwrap();
    assert_eq!(hits.len(), 2); // only docs containing at least one term
    assert_eq!(hits[0].key, b"rust-db".to_vec()); // matches both terms
    assert_eq!(hits[1].key, b"rust-async".to_vec()); // matches one
    assert!(hits[0].score > hits[1].score);
    assert!(hits.iter().all(|h| h.score > 0.0));
    assert_eq!(
        hits[0].document.get("body"),
        Some(&Value::Text("rust embedded database design".into()))
    );
}
