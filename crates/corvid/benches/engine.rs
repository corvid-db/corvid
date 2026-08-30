//! Microbenchmarks for the perf-sensitive paths: the value codec, HNSW
//! build/search, indexed vs exact text search, and the roadmap program's
//! deferred-path baselines (edge churn, compound prefix windows, mixed
//! delete-heavy churn, selective eq-window verification).

use std::collections::BTreeMap;

use corvid::hnsw::{DEFAULT_EF_CONSTRUCTION, DEFAULT_M, Hnsw};
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

    // The same corpus stored as PQ codes (m=16 code bytes vs 256 f32 bytes —
    // 16× smaller vector payload; the codebook is trained once outside the
    // timed region, matching how a collection trains at create time and
    // reuses the codebook for every insert/search).
    let pq = std::sync::Arc::new(corvid::pq::Pq::train(&data, 16, 256).unwrap());
    c.bench_function("hnsw_build_pq_2k_64d", |b| {
        b.iter(|| {
            let mut h = Hnsw::with_pq(Metric::L2, pq.clone(), DEFAULT_M, DEFAULT_EF_CONSTRUCTION);
            for v in &data {
                h.insert(v.clone());
            }
            h
        })
    });
    let mut pq_index = Hnsw::with_pq(Metric::L2, pq, DEFAULT_M, DEFAULT_EF_CONSTRUCTION);
    for v in &data {
        pq_index.insert(v.clone());
    }
    c.bench_function("hnsw_search_pq_2k_64d", |b| {
        b.iter(|| pq_index.search(std::hint::black_box(&query), 10, 64))
    });
}

/// PQ training in isolation (roadmap Task 13's shipped parallel path): the
/// k-means assignment step — each training point's nearest centroid, a
/// pure per-point function — runs chunk-parallel over a scoped std-only
/// worker team, while the Lloyd iterations stay sequential (each depends
/// on the last) and the update step consumes the assignments in input
/// order, so the trained codebook is bit-identical to the sequential
/// path's (pinned by pq.rs's equivalence test). Setup (the corpus) is
/// deterministic and lives outside the timed region.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- pq_train
/// ```
fn bench_pq_train(c: &mut Criterion) {
    let mut g = c.benchmark_group("pq_train");
    g.sample_size(20);
    {
        let data = corpus(2_000, 64);
        g.bench_function("pq_train_2k_64d", |b| {
            b.iter(|| corvid::pq::Pq::train(std::hint::black_box(&data), 16, 256))
        });
    }
    // The bigger shape: training cost scales with corpus × k; the
    // assignment step's parallel win holds as it grows.
    {
        let data = corpus(10_000, 128);
        g.bench_function("pq_train_10k_128d", |b| {
            b.iter(|| corvid::pq::Pq::train(std::hint::black_box(&data), 16, 256))
        });
    }
    g.finish();
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
/// 1k docs, 10k edge WRITES over 3 relations (~9.0k distinct rows), bimodal
/// in-degree distribution: every 4th edge targets a hub key, and the rest
/// draw targets uniformly over all 1k keys (~7.5 in-edges per key on
/// average) — see the targets comment for the hub side of the contrast.
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
    // Targets: hub-skewed — every 4th edge is a hub edge whose target is
    // `i % HUBS`; under the `i % 4 == 0` guard that expression only ever
    // yields {0, 4, 8, 12}, so the hubs are 4 keys (not HUBS=16) at
    // 2_500 / 4 = 625 hub-edge WRITES each. Those calls are not distinct
    // rows: writes i and i+6000 produce the same (relation, source, target)
    // triple (6000 is the lcm of the 16/3/1000 cycles, so ~250 of each
    // hub's 625 calls collapse), leaving ~375 distinct hub rows plus the
    // ~7.5 uniform draws that land on each hub key — ~382 distinct in-edges
    // per hub vs the ~7.5 non-hub mean, a bimodal contrast of ~51:1.
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
                // doc is the source of exactly 10, and the four hub keys —
                // 0, 4, 8, 12 — all sit inside the first SWEEP keys).
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

/// Selective eq-window verification over a scalar index (roadmap Task 4's
/// target). The planner serves `query().filter(field("cat").eq(..)).run()`
/// from the index window, then `verify_candidates` fetches every candidate
/// document — historically one point-get per key, now a batched ordered
/// walk when the window is dense. Corpus (deterministic index math): 5k
/// docs, `cat` in 100 distinct values (50 docs per value) plus a realistic
/// body; the fixed query pins one `cat` value and returns 50 rows of 5k.
/// `eq_500_of_5k` is the same shape at 10x density (`cat` in 10 values),
/// bracketing the density crossover of the fetch-strategy heuristic.
/// Setup happens once outside the measured routine (the workload is
/// read-only), so default sampling applies.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- selective_window_verify
/// ```
fn bench_selective_window_verify(c: &mut Criterion) {
    const DOCS: usize = 5_000;

    let seed = |buckets: i64| {
        let db = Db::open_in_memory().unwrap();
        let coll = db.collection("docs");
        for i in 0..DOCS {
            let mut m = BTreeMap::new();
            m.insert("cat".to_owned(), Value::Int((i as i64) % buckets));
            m.insert(
                "body".to_owned(),
                Value::Text("a benchmark document with a realistic body".into()),
            );
            coll.insert(format!("k{i:06}").as_bytes(), &Value::Map(m))
                .unwrap();
        }
        coll.create_scalar_index("cat").unwrap();
        db
    };

    let mut g = c.benchmark_group("selective_window_verify");
    {
        let db = seed(100);
        let coll = db.collection("docs");
        g.bench_function("eq_50_of_5k", |b| {
            b.iter(|| {
                coll.query()
                    .filter(field("cat").eq(Value::Int(50)))
                    .run()
                    .unwrap()
            })
        });
    }
    {
        let db = seed(10);
        let coll = db.collection("docs");
        g.bench_function("eq_500_of_5k", |b| {
            b.iter(|| {
                coll.query()
                    .filter(field("cat").eq(Value::Int(5)))
                    .run()
                    .unwrap()
            })
        });
    }
    g.finish();
}

/// order_by over a scalar-indexed field (roadmap Task 5's target). A
/// filterless `order_by(n).limit(20)` historically materialized and sorted
/// the whole corpus (`stream_scan_only`'s pruned buffer); the sort-index
/// walk now serves the window from the index (documents fetched only for
/// the 20 emitted rows). Corpus (deterministic index math + seeded
/// xorshift shuffle): 5k docs with DISTINCT `n` (so the walk's encoding
/// buckets are singletons — the common case) in shuffled insertion order,
/// plus a realistic body. `asc_limit20` walks forward and stops at 20
/// rows; `desc_limit20` pages the whole index forward keeping a bounded
/// newest-buckets buffer (scan_from is forward-only), then emits the top
/// window — still no sort and no corpus fetch. Setup happens once outside
/// the measured routine (the workload is read-only), so default sampling
/// applies.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- order_by_indexed_5k
/// ```
fn bench_order_by_indexed(c: &mut Criterion) {
    const DOCS: usize = 5_000;

    // Seeded Fisher-Yates over 0..DOCS: distinct values, shuffled order.
    let ns: Vec<i64> = {
        let mut vals: Vec<i64> = (0..DOCS as i64).collect();
        let mut state: u64 = 0x1234_5678;
        for i in (1..vals.len()).rev() {
            state = xorshift(state);
            let j = (state % (i as u64 + 1)) as usize;
            vals.swap(i, j);
        }
        vals
    };

    let db = Db::open_in_memory().unwrap();
    let coll = db.collection("docs");
    for (i, n) in ns.iter().enumerate() {
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(*n));
        m.insert(
            "body".to_owned(),
            Value::Text("a benchmark document with a realistic body".into()),
        );
        coll.insert(format!("k{i:06}").as_bytes(), &Value::Map(m))
            .unwrap();
    }
    coll.create_scalar_index("n").unwrap();

    let mut g = c.benchmark_group("order_by_indexed_5k");
    g.bench_function("asc_limit20", |b| {
        b.iter(|| coll.query().order_by("n", false).limit(20).run().unwrap())
    });
    g.bench_function("desc_limit20", |b| {
        b.iter(|| coll.query().order_by("n", true).limit(20).run().unwrap())
    });
    g.finish();
}

/// Endpoint-direct neighbor reads (ledger-closure Task 1). The adjacency
/// namespaces already key edges endpoint-first; `neighbors`/
/// `in_neighbors`/`neighbors_weighted`/`traverse` prefix-scan the SOURCE
/// edge namespaces by (relation, endpoint) instead. This group is the
/// before/after ruler for serving those reads endpoint-direct: a hub-heavy
/// corpus (the edge_churn family's shape — many edges on few endpoints)
/// plus the uniform-degree case.
///
/// Corpus (deterministic index math, no rand): 2_001 docs (`d000000`..
/// `d002000`, the first four are hubs), 10_000 edge writes in three
/// families over 3 relations:
/// * fan-in — `doc[4 + i % 1997] --RELS[i%3]--> hub[i % 4]`: for a fixed
///   hub the writes differ in (source, relation) only at i ≡ 0 mod 5991
///   (1997 is prime), so all 938 writes per hub are distinct rows — ~313
///   per (hub, relation);
/// * fan-out — `hub[i % 4] --RELS[i%3]--> doc[4 + 7i % 1997]` via
///   `link_weighted` (weights exercise the value-carrying rows), the same
///   distinctness argument → ~313 distinct out-edges per (hub, relation);
/// * uniform — `doc[4 + i % 1997] --RELS[i%3]--> doc[4 + 11i+13 % 1997]`:
///   degree ~1.25 writes per doc, ≤ 1 per (doc, relation), the contrast
///   case where every read returns a couple of rows.
///
/// The measured routines are read-only; the corpus is seeded once outside
/// the timing (setup also asserts the corpus math above, so a drifted
/// corpus cannot silently invalidate recorded numbers). Default sampling.
///
/// Literal invocation (audit C10, recorded so before/after numbers are
/// reproducible):
///
/// ```text
/// cargo bench -p corvid --bench engine -- neighbors_hub_10k
/// ```
fn bench_neighbors_hub(c: &mut Criterion) {
    const NON_HUBS: usize = 1_997; // docs 4..=2000
    const FAN: usize = 3_750; // per hub family
    const UNIFORM: usize = 2_500;
    const RELATIONS: [&str; 3] = ["knows", "likes", "follows"];

    let key = |i: usize| format!("d{i:06}");
    let db = Db::open_in_memory().unwrap();
    let coll = db.collection("docs");
    for i in 0..(4 + NON_HUBS) {
        let mut m = BTreeMap::new();
        m.insert("v".to_owned(), Value::Int(i as i64));
        coll.insert(key(i).as_bytes(), &Value::Map(m)).unwrap();
    }
    for i in 0..FAN {
        coll.link(
            key(4 + i % NON_HUBS).as_bytes(),
            RELATIONS[i % RELATIONS.len()],
            key(i % 4).as_bytes(),
        )
        .unwrap();
    }
    for i in 0..FAN {
        coll.link_weighted(
            key(i % 4).as_bytes(),
            RELATIONS[i % RELATIONS.len()],
            key(4 + (i * 7) % NON_HUBS).as_bytes(),
            (i as f64) * 0.25,
        )
        .unwrap();
    }
    for i in 0..UNIFORM {
        coll.link(
            key(4 + i % NON_HUBS).as_bytes(),
            RELATIONS[i % RELATIONS.len()],
            key(4 + (i * 11 + 13) % NON_HUBS).as_bytes(),
        )
        .unwrap();
    }
    // Corpus-math pins (see the doc comment): the hub carries ~313 distinct
    // edges per relation in each direction, and the probe doc exactly two
    // "knows" out-edges (hub0 via fan-in i=0, d000017 via uniform i=0).
    debug_assert_eq!(
        coll.neighbors(key(0).as_bytes(), "knows").unwrap().len(),
        313
    );
    debug_assert_eq!(
        coll.in_neighbors(key(0).as_bytes(), "knows").unwrap().len(),
        313
    );
    debug_assert_eq!(coll.neighbors(key(4).as_bytes(), "knows").unwrap().len(), 2);
    debug_assert!(coll.traverse(key(0).as_bytes(), "knows", 2).unwrap().len() > 313);

    let hub0 = key(0).into_bytes();
    let mut g = c.benchmark_group("neighbors_hub_10k");
    g.bench_function("hub_out_knows", |b| {
        b.iter(|| coll.neighbors(&hub0, "knows").unwrap())
    });
    g.bench_function("hub_in_knows", |b| {
        b.iter(|| coll.in_neighbors(&hub0, "knows").unwrap())
    });
    g.bench_function("hub_weighted_knows", |b| {
        b.iter(|| coll.neighbors_weighted(&hub0, "knows").unwrap())
    });
    g.bench_function("uniform_deg1", |b| {
        b.iter(|| coll.neighbors(key(4).as_bytes(), "knows").unwrap())
    });
    g.bench_function("traverse_hub_2hops", |b| {
        b.iter(|| coll.traverse(&hub0, "knows", 2).unwrap())
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
    bench_delete_heavy,
    bench_selective_window_verify,
    bench_order_by_indexed,
    bench_pq_train,
    bench_neighbors_hub
);
criterion_main!(benches);
