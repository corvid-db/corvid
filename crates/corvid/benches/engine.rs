//! Microbenchmarks for the perf-sensitive paths: the value codec, HNSW
//! build/search, and indexed vs exact text search.

use std::collections::BTreeMap;

use corvid::hnsw::Hnsw;
use corvid::{Db, Metric, Value};
use criterion::{Criterion, criterion_group, criterion_main};

fn nested_value() -> Value {
    let mut inner = BTreeMap::new();
    inner.insert("embedding".to_owned(), Value::Vector(vec![0.1; 64]));
    inner.insert(
        "tags".to_owned(),
        Value::Array(vec![Value::Text("a".into()); 8]),
    );
    let mut m = BTreeMap::new();
    m.insert(
        "title".to_owned(),
        Value::Text("a benchmark document".into()),
    );
    m.insert("n".to_owned(), Value::Int(42));
    m.insert("meta".to_owned(), Value::Map(inner));
    Value::Map(m)
}

/// Deterministic pseudo-random vectors.
fn corpus(n: usize, dim: usize) -> Vec<Vec<f32>> {
    let mut state: u64 = 0x1234_5678;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f32 / (1u64 << 53) as f32
    };
    (0..n).map(|_| (0..dim).map(|_| next()).collect()).collect()
}

fn bench_codec(c: &mut Criterion) {
    let value = nested_value();
    let bytes = value.encode();
    c.bench_function("value_encode", |b| b.iter(|| value.encode()));
    c.bench_function("value_decode", |b| {
        b.iter(|| Value::decode(std::hint::black_box(&bytes)).unwrap())
    });
}

fn bench_hnsw(c: &mut Criterion) {
    let data = corpus(2000, 64);
    c.bench_function("hnsw_build_2k_64d", |b| {
        b.iter(|| {
            let mut h = Hnsw::new(Metric::L2);
            for v in &data {
                h.insert(v.clone());
            }
            h
        })
    });

    let mut index = Hnsw::new(Metric::L2);
    for v in &data {
        index.insert(v.clone());
    }
    let query = data[0].clone();
    c.bench_function("hnsw_search_2k_64d", |b| {
        b.iter(|| index.search(std::hint::black_box(&query), 10, 64))
    });
}

fn bench_text(c: &mut Criterion) {
    let db = Db::open_in_memory().unwrap();
    let coll = db.collection("docs");
    let words = [
        "rust", "embedded", "database", "vector", "search", "graph", "fox", "dog",
    ];
    for i in 0..2000 {
        let body: String = (0..20)
            .map(|j| words[(i + j) % words.len()])
            .collect::<Vec<_>>()
            .join(" ");
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), Value::Text(body));
        coll.insert(format!("k{i}").as_bytes(), &Value::Map(m))
            .unwrap();
    }
    c.bench_function("bm25_exact_2k", |b| {
        b.iter(|| coll.text_search("body", "rust database", 10).unwrap())
    });
    coll.create_text_index("body").unwrap();
    let _ = coll.text_search("body", "warm", 1).unwrap(); // build index
    c.bench_function("bm25_indexed_2k", |b| {
        b.iter(|| coll.text_search("body", "rust database", 10).unwrap())
    });
}

fn bench_distance(c: &mut Criterion) {
    use corvid::distance::{cosine_distance, dot, l2_squared};
    let a: Vec<f32> = (0..768).map(|i| (i as f32 * 0.013).sin()).collect();
    let b: Vec<f32> = (0..768).map(|i| (i as f32 * 0.017).cos()).collect();
    c.bench_function("dot_768d", |bn| bn.iter(|| dot(&a, &b)));
    c.bench_function("l2_768d", |bn| bn.iter(|| l2_squared(&a, &b)));
    c.bench_function("cosine_768d", |bn| bn.iter(|| cosine_distance(&a, &b)));
}

criterion_group!(benches, bench_codec, bench_hnsw, bench_text, bench_distance);
criterion_main!(benches);
