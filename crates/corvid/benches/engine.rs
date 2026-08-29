//! Microbenchmarks for the perf-sensitive paths: the value codec, HNSW
//! build/search, indexed vs exact text search, and the roadmap program's
//! deferred-path baselines (edge churn, compound prefix windows, mixed
//! delete-heavy churn).

use std::collections::BTreeMap;

use corvid::hnsw::Hnsw;
use corvid::schema::{Field, FieldType, Schema};
use corvid::{Collection, Db, Metric, Value, field};
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

/// One step of the seeded xorshift64 generator used by the deterministic
/// (rand-free) corpora below — same shape as the `pq.rs` conformance suite.
fn xorshift(state: u64) -> u64 {
    let mut s = state;
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    s
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

/// On-disk index-creation baselines (audit wave-2 perf rule). Each iteration
/// seeds a fresh in-memory Db and then creates the index — creation is
/// once-per-db, so the per-iteration seeding cost is deliberately inside the
/// measured routine (fine for a relative before/after baseline; absolute
/// numbers overstate creation cost). Sample size is kept low to bound wall
/// time given multi-second iterations.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- index_creation_ondisk
/// ```
fn bench_creation_ondisk(c: &mut Criterion) {
    const N_TEXT: usize = 5_000; // 3 backfill pages (PAGE = 2048)
    // One doc past the 2048-page boundary: exercises the multi-page cursor
    // path of the atomic driver while keeping the bench around ~30 s.
    const N_VEC: usize = 2_049;
    let words = [
        "rust", "embedded", "database", "vector", "search", "graph", "fox", "dog",
    ];
    let bodies: Vec<String> = (0..N_TEXT)
        .map(|i| {
            (0..20)
                .map(|j| words[(i + j) % words.len()])
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let vecs = corpus(N_VEC, 8);

    let mut g = c.benchmark_group("index_creation_ondisk");
    g.sample_size(10);
    g.bench_function("create_text_index_ondisk_5k", |b| {
        b.iter(|| {
            let db = Db::open_in_memory().unwrap();
            let coll = db.collection("docs");
            for (i, body) in bodies.iter().enumerate() {
                let mut m = BTreeMap::new();
                m.insert("body".to_owned(), Value::Text(body.clone()));
                coll.insert(format!("k{i}").as_bytes(), &Value::Map(m))
                    .unwrap();
            }
            coll.create_text_index_ondisk("body").unwrap();
        })
    });
    g.bench_function("create_vector_index_ondisk_2k_8d", |b| {
        b.iter(|| {
            let db = Db::open_in_memory().unwrap();
            let coll = db.collection("docs");
            for (i, v) in vecs.iter().enumerate() {
                let mut m = BTreeMap::new();
                m.insert("embedding".to_owned(), Value::Vector(v.clone()));
                coll.insert(format!("k{i}").as_bytes(), &Value::Map(m))
                    .unwrap();
            }
            coll.create_vector_index_ondisk("embedding", Metric::L2)
                .unwrap();
        })
    });
    g.finish();
}

/// Deferred-path baseline: edge churn (roadmap Task 7's target). The delete
/// cascade (`Db::edges_on_delete_in_txn`) pages through BOTH edge namespaces
/// for every deleted key — O(E) per delete regardless of the deleted doc's
/// degree. `edge_delete_sweep_100` measures a sweep over docs that carry
/// edges, so the per-delete O(E) scan dominates (seeded per sample with
/// `iter_batched` — the seeding itself is NOT in the number); `edge_link_10k`
/// isolates the link-heavy insert phase (one transaction per edge, forward +
/// reverse row).
///
/// Corpus (deterministic, index math + seeded xorshift — no rand dep):
/// 1k docs, 10k edges over 3 relations, bimodal degree distribution (every
/// 4th edge targets one of 16 hub keys; the rest draw targets uniformly).
/// `edge_link_10k` seeds a fresh in-memory Db per iteration inside the
/// measured routine (the `index_creation_ondisk` convention — links are ~90%
/// of that iteration, fine for a relative before/after baseline). Sample
/// size is kept low to bound wall time given multi-hundred-ms iterations.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- edge_churn
/// ```
fn bench_edge_churn(c: &mut Criterion) {
    const DOCS: usize = 1_000;
    const EDGES: usize = 10_000;
    const HUBS: usize = 16;
    const SWEEP: usize = 100;
    const RELATIONS: [&str; 3] = ["knows", "likes", "follows"];

    let key = |i: usize| format!("k{i:06}");
    // Targets: hub-skewed — every 4th edge hits one of the HUBS low keys
    // (~250 in-edges each), the rest draw a seeded-uniform target.
    let targets: Vec<usize> = {
        let mut state: u64 = 0x1234_5678;
        (0..EDGES)
            .map(|i| {
                if i % 4 == 0 {
                    i % HUBS
                } else {
                    state = xorshift(state);
                    (state as usize) % DOCS
                }
            })
            .collect()
    };

    let seed = |coll: &Collection<'_>| {
        for i in 0..DOCS {
            let mut m = BTreeMap::new();
            m.insert("v".to_owned(), Value::Int(i as i64));
            coll.insert(key(i).as_bytes(), &Value::Map(m)).unwrap();
        }
        for (i, to) in targets.iter().enumerate() {
            coll.link(
                key(i % DOCS).as_bytes(),
                RELATIONS[i % RELATIONS.len()],
                key(*to).as_bytes(),
            )
            .unwrap();
        }
    };

    let mut g = c.benchmark_group("edge_churn");
    g.sample_size(10);
    g.bench_function("edge_link_10k", |b| {
        b.iter(|| {
            let db = Db::open_in_memory().unwrap();
            seed(&db.collection("docs"));
        })
    });
    g.bench_function("edge_delete_sweep_100", |b| {
        b.iter_batched(
            || {
                let db = Db::open_in_memory().unwrap();
                seed(&db.collection("docs"));
                db
            },
            |db| {
                let coll = db.collection("docs");
                // The measured sweep: SWEEP docs that all carry edges (every
                // doc is the source of ~10, and the first 16 are hubs).
                for i in 0..SWEEP {
                    std::hint::black_box(coll.delete(key(i).as_bytes()).unwrap());
                }
            },
            criterion::BatchSize::PerIteration,
        )
    });
    g.finish();
}

/// Deferred-path baseline: prefix-only equality on a populated compound
/// index (roadmap Task 6's target). With a compound index on (a, b) and a
/// filter constraining only `a`, the planner must decline the index window
/// today (a doc missing `b` would match the filter but sit outside the
/// index) and serve the query from the full-collection scan. This bench
/// measures `query().filter(field("a").eq(..)).run()` end to end on a
/// corpus where EVERY doc has both fields — the shape Task 6 re-enables —
/// so the number is the BEFORE the windowed path must beat.
///
/// Corpus (deterministic index math): 5k docs, `a` in 100 distinct values
/// (50 docs per value), `b` spread over 977; the fixed query pins one `a`
/// value and returns 50 rows. Setup happens once outside the measured
/// routine (the workload is read-only), so default sampling applies.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- compound_prefix_scan
/// ```
fn bench_compound_prefix_scan(c: &mut Criterion) {
    const DOCS: usize = 5_000;
    const A_VALUES: i64 = 100; // 50 docs share each `a`

    let db = Db::open_in_memory().unwrap();
    let coll = db.collection("docs");
    for i in 0..DOCS {
        let mut m = BTreeMap::new();
        m.insert("a".to_owned(), Value::Int((i as i64) % A_VALUES));
        m.insert("b".to_owned(), Value::Int((i as i64) % 977));
        m.insert(
            "body".to_owned(),
            Value::Text("a benchmark document with a realistic body".into()),
        );
        coll.insert(format!("k{i:06}").as_bytes(), &Value::Map(m))
            .unwrap();
    }
    coll.create_compound_index(&["a", "b"]).unwrap();

    let mut g = c.benchmark_group("compound_prefix_scan");
    g.bench_function("eq_leading_only_5k", |b| {
        b.iter(|| {
            coll.query()
                .filter(field("a").eq(Value::Int(A_VALUES / 2)))
                .run()
                .unwrap()
        })
    });
    g.finish();
}

/// Deferred-path baseline: mixed delete-heavy churn under a declared unique
/// constraint, a scalar index, and graph edges — the combined insert/delete
/// cost the conformance program added (in-transaction unique re-verify via
/// the index bucket walk, scalar index maintenance, and the O(E) edge
/// cascade on every delete). `insert_unique_scalar_5k` isolates the insert
/// phase (whole iteration is the phase, per the `index_creation_ondisk`
/// convention); `delete_half_2p5k` measures ONLY the delete phase — deleting
/// half the corpus by key in a seeded-shuffled (deterministic) order, with
/// the corpus seeded per sample via `iter_batched`.
///
/// Corpus (deterministic): 5k docs with a required-unique `sku` (Int, all
/// distinct — the unique check probes the scalar index bucket, its fast
/// path) plus an unindexed `cat`; every 5th doc links a `next` edge to its
/// successor (1k edges), so deletes pay the edge cascade too. Sample size
/// is kept low to bound wall time.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- delete_heavy
/// ```
fn bench_delete_heavy(c: &mut Criterion) {
    const DOCS: usize = 5_000;
    const DELETES: usize = DOCS / 2;

    let key = |i: usize| format!("k{i:06}");
    // Seeded Fisher-Yates: a deterministic "random-ish" delete order.
    let order: Vec<usize> = {
        let mut keys: Vec<usize> = (0..DOCS).collect();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for i in (1..keys.len()).rev() {
            state = xorshift(state);
            let j = (state % (i as u64 + 1)) as usize;
            keys.swap(i, j);
        }
        keys.truncate(DELETES);
        keys
    };

    let schema = Schema::new().field(Field::new("sku", FieldType::Int).required().unique());
    let doc = |i: usize| {
        let mut m = BTreeMap::new();
        m.insert("sku".to_owned(), Value::Int(i as i64));
        m.insert("cat".to_owned(), Value::Int((i % 25) as i64));
        Value::Map(m)
    };
    let seed_db = || {
        let db = Db::open_in_memory().unwrap();
        let coll = db.collection("docs");
        coll.set_schema(&schema).unwrap();
        coll.create_scalar_index("sku").unwrap();
        for i in 0..DOCS {
            coll.insert(key(i).as_bytes(), &doc(i)).unwrap();
        }
        for i in 0..DOCS {
            if i % 5 == 0 {
                coll.link(key(i).as_bytes(), "next", key((i + 1) % DOCS).as_bytes())
                    .unwrap();
            }
        }
        db
    };

    let mut g = c.benchmark_group("delete_heavy");
    g.sample_size(10);
    g.bench_function("insert_unique_scalar_5k", |b| {
        b.iter(|| {
            let db = Db::open_in_memory().unwrap();
            let coll = db.collection("docs");
            coll.set_schema(&schema).unwrap();
            coll.create_scalar_index("sku").unwrap();
            for i in 0..DOCS {
                coll.insert(key(i).as_bytes(), &doc(i)).unwrap();
            }
        })
    });
    g.bench_function("delete_half_2p5k", |b| {
        b.iter_batched(
            seed_db,
            |db| {
                let coll = db.collection("docs");
                // The measured phase: delete half the corpus, shuffled order.
                for i in &order {
                    std::hint::black_box(coll.delete(key(*i).as_bytes()).unwrap());
                }
            },
            criterion::BatchSize::PerIteration,
        )
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_codec,
    bench_hnsw,
    bench_text,
    bench_distance,
    bench_creation_ondisk,
    bench_edge_churn,
    bench_compound_prefix_scan,
    bench_delete_heavy
);
criterion_main!(benches);
