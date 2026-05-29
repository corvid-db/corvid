//! Scaling harness: load N records and time the core operations.
//!
//! Usage: `cargo run --release --example scaling -- <N> [vector_cap]`
//!
//! Loads N documents (category + body text + a small embedding) into a
//! file-backed store via batch insert, then times point/aggregate/search
//! operations. A vector index is built only when N <= `vector_cap` (default
//! 300_000) because the HNSW graph is in-memory; a text index is always built.

use std::collections::BTreeMap;
use std::time::Instant;

use corvid::{Db, Metric, Value, field};

const CATEGORIES: [&str; 5] = ["blog", "news", "wiki", "forum", "docs"];
const WORDS: [&str; 10] = [
    "rust", "embedded", "database", "vector", "search", "graph", "fox", "dog", "cloud", "memory",
];
const DIM: usize = 16;
const BATCH: usize = 10_000;

fn embedding(i: usize) -> Vec<f32> {
    let mut state = (i as u64).wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..DIM)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32
        })
        .collect()
}

fn doc(i: usize) -> Value {
    let mut m = BTreeMap::new();
    m.insert(
        "category".into(),
        Value::Text(CATEGORIES[i % CATEGORIES.len()].into()),
    );
    let body: String = (0..8)
        .map(|j| WORDS[(i + j) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ");
    m.insert("body".into(), Value::Text(body));
    m.insert("embedding".into(), Value::Vector(embedding(i)));
    m.insert("n".into(), Value::Int(i as i64));
    Value::Map(m)
}

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    println!("  {label:<34} {:>10.3?}", start.elapsed());
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let vector_cap: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(300_000);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("scale.db")).unwrap();
    let docs = db.collection("docs");

    println!("=== N = {n} ===");

    // Bulk load in batches (one commit per batch).
    time("insert", || {
        let mut i = 0;
        while i < n {
            let end = (i + BATCH).min(n);
            let owned: Vec<(Vec<u8>, Value)> = (i..end)
                .map(|j| (format!("{j:012}").into_bytes(), doc(j)))
                .collect();
            let items: Vec<(&[u8], &Value)> =
                owned.iter().map(|(k, v)| (k.as_slice(), v)).collect();
            docs.insert_batch(&items).unwrap();
            i = end;
        }
    });

    let count = time("count (O(1))", || docs.query().count().unwrap());
    assert_eq!(count, n);

    if n > 0 {
        let key = format!("{:012}", n / 2).into_bytes();
        time("point get", || docs.get(&key).unwrap());
    }

    time("filtered count (stream)", || {
        docs.query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .count()
            .unwrap()
    });

    // Scalar index on the high-cardinality `n`. The win is for *selective*
    // predicates: a selective equality or narrow range touches only the matching
    // key range plus a few point-gets, instead of scanning the whole collection.
    // (A low-selectivity filter that matches half the corpus is still better
    // served by a sequential scan — the index returns a verified superset, so
    // either path is correct.)
    // (Compare the timings below against "filtered count (stream)" above: that
    // is the cost of a full scan. The selective indexed ops should be far
    // cheaper because they touch only the matching key range.)
    time("build scalar index (n)", || {
        docs.create_scalar_index("n").unwrap()
    });
    if n > 0 {
        let mid = (n / 2) as i64;
        let eq = time("selective eq count (scalar index)", || {
            docs.query()
                .filter(field("n").eq(Value::Int(mid)))
                .count()
                .unwrap()
        });
        assert_eq!(eq, 1);

        let narrow = time("narrow range fetch (scalar index)", || {
            docs.query()
                .filter(field("n").ge(Value::Int(mid)))
                .filter(field("n").lt(Value::Int(mid + 100)))
                .run()
                .unwrap()
        });
        assert_eq!(narrow.len(), 100);
    }

    time("group_count (stream)", || {
        docs.query().group_count("category").unwrap()
    });

    time("order_by n desc, limit 10", || {
        docs.query().order_by("n", true).limit(10).run().unwrap()
    });

    time("build text index", || {
        docs.create_text_index("body").unwrap()
    });
    time("text_search (indexed)", || {
        docs.text_search("body", "rust database", 10).unwrap()
    });

    if n <= vector_cap {
        time("build vector index", || {
            docs.create_vector_index("embedding", Metric::Cosine)
                .unwrap();
            // Force the lazy build by issuing one search.
            docs.vector_search("embedding", &embedding(0), 1, Metric::Cosine)
                .unwrap();
        });
        time("vector_search (indexed)", || {
            docs.vector_search("embedding", &embedding(7), 10, Metric::Cosine)
                .unwrap()
        });
        time("hybrid filter+vector (approx)", || {
            docs.query()
                .filter(field("category").eq(Value::Text("blog".into())))
                .vector("embedding", embedding(7), 50, Metric::Cosine)
                .approx()
                .limit(10)
                .run()
                .unwrap()
        });
    } else {
        println!("  (in-memory vector index skipped: N > {vector_cap})");
    }

    // On-disk vector index: bounded memory, persists. Build it on a fresh
    // collection (its own namespace) to show it works at this N.
    let ann = db.collection("ann");
    time("ondisk index backfill insert", || {
        let mut i = 0;
        while i < n {
            let end = (i + BATCH).min(n);
            let owned: Vec<(Vec<u8>, Value)> = (i..end)
                .map(|j| (format!("{j:012}").into_bytes(), doc(j)))
                .collect();
            let items: Vec<(&[u8], &Value)> =
                owned.iter().map(|(k, v)| (k.as_slice(), v)).collect();
            ann.insert_batch(&items).unwrap();
            i = end;
        }
    });
    if n > 0 {
        time("ondisk create + backfill", || {
            ann.create_vector_index_ondisk("embedding", Metric::Cosine)
                .unwrap()
        });
        time("ondisk vector_search", || {
            ann.vector_search("embedding", &embedding(7), 10, Metric::Cosine)
                .unwrap()
        });
    }

    println!();
}
