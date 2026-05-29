//! Concurrency tests: the database is shared across threads via `Arc<Db>`
//! (it is `Send + Sync`). These exercise concurrent writers and readers to
//! surface deadlocks or lost updates in the mutex-guarded index/subscriber
//! state.

use std::sync::Arc;
use std::thread;

use corvid::{Db, Metric, Value};

#[test]
fn concurrent_writers_to_distinct_keys_all_persist() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let threads = 8;
    let per_thread = 100;

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for i in 0..per_thread {
                    let key = format!("t{t}-{i}");
                    db.collection("docs")
                        .insert(key.as_bytes(), &Value::Int((t * per_thread + i) as i64))
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let total = db.collection("docs").query().count().unwrap();
    assert_eq!(total, threads * per_thread);
}

#[test]
fn concurrent_readers_and_a_writer_do_not_deadlock() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let c = db.collection("docs");
    c.create_vector_index("v", Metric::L2).unwrap();
    for i in 0..200i64 {
        let mut m = std::collections::BTreeMap::new();
        m.insert("v".to_owned(), Value::Vector(vec![i as f32, 0.0]));
        c.insert(format!("k{i}").as_bytes(), &Value::Map(m))
            .unwrap();
    }

    let mut handles = Vec::new();
    // Readers run vector searches (which may rebuild/compact the index).
    for _ in 0..6 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                let hits = db
                    .collection("docs")
                    .vector_search("v", &[10.0, 0.0], 5, Metric::L2)
                    .unwrap();
                assert!(!hits.is_empty());
            }
        }));
    }
    // A writer mutates concurrently, invalidating the index repeatedly.
    {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for i in 200..300i64 {
                let mut m = std::collections::BTreeMap::new();
                m.insert("v".to_owned(), Value::Vector(vec![i as f32, 0.0]));
                db.collection("docs")
                    .insert(format!("k{i}").as_bytes(), &Value::Map(m))
                    .unwrap();
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.collection("docs").query().count().unwrap(), 300);
}

#[test]
fn concurrent_subscribers_receive_events() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let db = Arc::new(Db::open_in_memory().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&count);
    db.subscribe(move |_e| {
        sink.fetch_add(1, Ordering::SeqCst);
    });

    let handles: Vec<_> = (0..4)
        .map(|t| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for i in 0..25 {
                    db.collection("docs")
                        .insert(format!("{t}-{i}").as_bytes(), &Value::Int(1))
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 100);
}
