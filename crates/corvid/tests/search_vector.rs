//! Vector-search conformance (skeleton). Task 8 fills this file with the
//! full matrix: Metric × Quantization cross, approx vs exact, zero-norm,
//! dimension mismatch, k boundaries, builder composition. This smoke test
//! anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Metric, Value};

fn doc(tag: &str, v: Vec<f32>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("tag".to_owned(), Value::Text(tag.to_owned()));
    m.insert("v".to_owned(), Value::Vector(v));
    Value::Map(m)
}

#[test]
fn search_vector_smoke_ranks_nearest_first_exact() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"near", &doc("near", vec![1.0, 0.0])).unwrap();
    c.insert(b"mid", &doc("mid", vec![0.9, 0.1])).unwrap();
    c.insert(b"far", &doc("far", vec![0.0, 1.0])).unwrap();

    let hits = c
        .vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
        .unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].key, b"near".to_vec());
    assert_eq!(hits[1].key, b"mid".to_vec());
    assert_eq!(hits[2].key, b"far".to_vec());
    // Distances ascend; the exact path reports approximate = false.
    assert!(hits[0].distance < hits[1].distance && hits[1].distance < hits[2].distance);
    assert!(hits[0].distance < 1e-6);
    assert!(!hits[0].approximate);
}
