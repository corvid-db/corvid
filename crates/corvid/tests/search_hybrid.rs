//! Hybrid-search conformance (skeleton). Task 9 fills this file with the
//! full matrix: RRF k validation, MMR lambda validation, single-source
//! no-ops, docs without embeddings, fusion ordering. This smoke test
//! anchors the radar's test-existence check.

use std::collections::BTreeMap;

use corvid::{Db, Metric, Value};

fn doc(body: &str, v: Vec<f32>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    m.insert("v".to_owned(), Value::Vector(v));
    Value::Map(m)
}

#[test]
fn search_hybrid_smoke_rrf_fuses_vector_and_text() {
    // Direct fusion: a key ranked first by both sources must fuse first.
    let vec_ranking = vec![b"both".to_vec(), b"vec-only".to_vec()];
    let text_ranking = vec![b"both".to_vec(), b"text-only".to_vec()];
    let fused =
        corvid::reciprocal_rank_fusion(&[&vec_ranking, &text_ranking], corvid::DEFAULT_RRF_K);
    assert_eq!(fused[0].0, b"both".to_vec());
    assert!(fused[0].1 > fused[1].1);

    // Through the builder: vector + text sources fused into one result set.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"strong", &doc("rust embedded database", vec![1.0, 0.0]))
        .unwrap();
    c.insert(b"weak", &doc("python web frameworks", vec![0.0, 1.0]))
        .unwrap();

    let rows = c
        .query()
        .vector("v", vec![1.0, 0.0], 2, Metric::Cosine)
        .text("body", "rust database", 2)
        .limit(2)
        .run()
        .unwrap();
    assert_eq!(rows.len(), 2);
    // The doc strong in both modalities fuses to the top.
    assert_eq!(rows[0].key, b"strong".to_vec());
    assert!(rows[0].score > rows[1].score);
}
