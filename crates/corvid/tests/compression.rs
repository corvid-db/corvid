//! Conformance for the optional `zstd` value-compression feature
//! (ledger-closure Task 5). The entire file is compiled only with
//! `--features zstd`: with the feature off the engine's byte behavior is
//! pinned instead by the store-level unit tests (`src/store.rs`) and the
//! whole existing suite, which runs unmodified in both configurations —
//! that is itself the feature's core contract (compression is transparent;
//! every read path decodes; OFF writes byte-identical rows).
//!
//! Corpora follow the repo conventions: deterministic seeded xorshift /
//! arithmetic patterns, no `rand`.

#![cfg(feature = "zstd")]

use std::collections::BTreeMap;

use corvid::{Db, Value, field};

/// Deterministic pseudo-random bytes (seeded xorshift64*, no `rand`).
fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 33) as u8);
    }
    out
}

/// Repetitive-but-structured text (compresses well at zstd level 3).
fn text_blob(n: usize) -> String {
    let base =
        "the quick brown fox jumps over the lazy dog; pack my box with five dozen liquor jugs. ";
    (0..n)
        .map(|i| base.as_bytes()[i % base.len()] as char)
        .collect()
}

/// Deterministic "embedding-like" floats — smooth, so the f32 byte stream
/// has exploitable structure — with the IEEE specials sprinkled in.
fn float_blob(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| match i % 97 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => -0.0,
            _ => ((i as f32) * 0.031).sin() * 10.0,
        })
        .collect()
}

/// A document exercising EVERY `Value` variant, big enough that its
/// encoding is far above the compression threshold.
fn kitchen_sink_doc() -> Value {
    let mut inner = BTreeMap::new();
    inner.insert("null".to_owned(), Value::Null);
    inner.insert("bool".to_owned(), Value::Bool(true));
    inner.insert("int".to_owned(), Value::Int(i64::MIN));
    inner.insert(
        "float_specials".to_owned(),
        Value::Array(vec![
            Value::Float(f64::NAN),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-0.0),
            Value::Float(0.0),
        ]),
    );
    inner.insert(
        "bytes_random".to_owned(),
        Value::Bytes(pseudo_random(2048, 42)),
    );
    let mut doc = BTreeMap::new();
    doc.insert("body".to_owned(), Value::Text(text_blob(8192)));
    doc.insert("vector".to_owned(), Value::Vector(float_blob(768)));
    doc.insert(
        "array".to_owned(),
        Value::Array((0..64).map(Value::Int).collect()),
    );
    doc.insert("nested".to_owned(), Value::Map(inner));
    Value::Map(doc)
}

/// Every variant above the threshold round-trips BIT-EXACTLY: asserts on
/// `encode()` equality, which (unlike semantic `==`) distinguishes -0.0
/// from 0.0 and preserves NaN payloads — the mutations.rs round-trip pins
/// are the oracle; this is their above-threshold twin under compression.
#[test]
fn every_value_variant_roundtrips_bit_exact_above_threshold() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    let doc = kitchen_sink_doc();
    assert!(
        doc.encode().len() > 8192,
        "doc must be well above threshold"
    );

    c.insert(b"k", &doc).unwrap();
    let got = c.get(b"k").unwrap().unwrap();
    // Encode equality is the oracle: strictly stronger than `==` for
    // round-tripping (PartialEq is a derive — it cannot see through the
    // NaN payload this doc deliberately carries).
    assert_eq!(got.encode(), doc.encode(), "bit-exact round-trip");
}

/// Every read path agrees on a compressed row: point get, full scan,
/// paging, streaming, and query execution (which reads through the
/// snapshot-scoped twins).
#[test]
fn every_read_path_agrees_on_compressed_rows() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    for i in 0..5u8 {
        let mut doc = kitchen_sink_doc();
        if let Value::Map(m) = &mut doc {
            m.insert("i".to_owned(), Value::Int(i as i64));
        }
        c.insert(&[i], &doc).unwrap();
    }

    let scanned: Vec<u8> = c.scan().unwrap().iter().map(|(k, _)| k[0]).collect();
    assert_eq!(scanned, vec![0, 1, 2, 3, 4]);

    let paged = c.page(Some(b""), 3).unwrap();
    assert_eq!(paged.rows.len(), 3);
    assert!(paged.next.is_some());
    let rest = c.page(paged.next.as_deref(), 10).unwrap();
    assert_eq!(paged.rows.len() + rest.rows.len(), 5);

    let mut streamed = 0;
    c.for_each_doc(|_, v| {
        assert!(v.encode().len() > 8192);
        streamed += 1;
        Ok(true)
    })
    .unwrap();
    assert_eq!(streamed, 5);

    // Query path (snapshot reader twins): filter decodes compressed docs.
    let hits: Vec<Vec<u8>> = c
        .query()
        .filter(field("i").eq(Value::Int(2)))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(hits, vec![vec![2u8]]);
    // The scan twin and the point-read twin agree bit-for-bit, and the
    // filtered result's document matches the point read of its key.
    let by_query = c.get(b"\x02").unwrap().unwrap();
    assert_eq!(c.scan().unwrap()[2].1.encode(), by_query.encode());
    let mut expected = kitchen_sink_doc();
    if let Value::Map(m) = &mut expected {
        m.insert("i".to_owned(), Value::Int(2));
    }
    assert_eq!(by_query.encode(), expected.encode());
}

/// Reopen, backup, and dump/load all preserve compressed documents: the
/// dump must contain the RAW value encoding (compression is a storage
/// concern below the dump format — v2 either way), and load re-compresses
/// on write.
#[test]
fn reopen_backup_and_dump_load_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corvid.db");
    let doc = kitchen_sink_doc();
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        c.insert(b"k", &doc).unwrap();
    }
    // Reopen: rows decompress transparently.
    let db = Db::open(&path).unwrap();
    let c = db.collection("docs");
    assert_eq!(c.get(b"k").unwrap().unwrap().encode(), doc.encode());

    // Backup copies raw (possibly compressed) rows; the copy reads the same.
    let bak = dir.path().join("backup.db");
    db.backup(&bak).unwrap();
    let bdb = Db::open(&bak).unwrap();
    assert_eq!(
        bdb.collection("docs").get(b"k").unwrap().unwrap().encode(),
        doc.encode()
    );

    // Dump: the raw encoding appears VERBATIM in the stream (dump reads
    // through the store, i.e. decompressed — format-stable v2 with the
    // feature on), and no marker-prefixed row leaks into it.
    let mut dump = Vec::new();
    db.dump(&mut dump).unwrap();
    let needle = doc.encode();
    assert!(
        dump.windows(needle.len()).any(|w| w == needle),
        "dump must carry the uncompressed value encoding"
    );
    drop(db);

    // Load into a fresh database: documents re-compress on write and read
    // back bit-exact.
    let dst = Db::open_in_memory().unwrap();
    dst.load(&dump[..]).unwrap();
    assert_eq!(
        dst.collection("docs").get(b"k").unwrap().unwrap().encode(),
        doc.encode()
    );
}

/// Non-vacuous engagement pin: 100 documents of 64 KiB compressible text
/// (~6.4 MiB of encodings) must leave the database file far smaller than
/// the raw payload — with compression this corpus is a few tens of KiB of
/// frames plus redb structure.
#[test]
fn compressible_corpus_shrinks_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("docs");
        for i in 0..100u32 {
            let mut m = BTreeMap::new();
            m.insert("id".to_owned(), Value::Int(i as i64));
            m.insert("body".to_owned(), Value::Text(text_blob(64 * 1024)));
            c.insert(&i.to_be_bytes(), &Value::Map(m)).unwrap();
        }
    }
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(
        size < 1_500_000,
        "100 x 64 KiB of repetitive text must compress (file is {size} bytes; \
         raw payload alone would be ~6.4 MiB)"
    );
    // And the data is all there.
    let db = Db::open(&path).unwrap();
    assert_eq!(db.collection("docs").len().unwrap(), 100);
}

/// Indexes, TTL, and graph edges ride the same value path: maintenance
/// decodes compressed documents (index builders read through `Value`), TTL
/// purges by key, and edge namespaces are themselves never compressed but
/// reference compressed collections fine.
#[test]
fn indexes_ttl_and_edges_ride_the_value_path() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.create_text_index("body").unwrap();
    c.create_scalar_index("id").unwrap();
    for i in 0..10i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_owned(), Value::Int(i));
        m.insert("body".to_owned(), Value::Text(text_blob(4096)));
        m.insert("v".to_owned(), Value::Vector(float_blob(64)));
        c.insert(&i.to_be_bytes(), &Value::Map(m)).unwrap();
    }
    c.create_vector_index("v", corvid::Metric::Cosine).unwrap();

    // Text index over compressed docs.
    let hits = c.text_search("body", "fox", 3).unwrap();
    assert_eq!(hits.len(), 3);
    // Scalar index.
    let keys: Vec<Vec<u8>> = c
        .query()
        .filter(field("id").eq(Value::Int(7)))
        .run()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(keys, vec![7i64.to_be_bytes().to_vec()]);
    // Vector index over compressed vector payloads.
    let vhits = c
        .vector_search("v", &float_blob(64), 2, corvid::Metric::Cosine)
        .unwrap();
    assert_eq!(vhits.len(), 2);

    // TTL: set, read, purge — on a compressed document.
    let mut m = BTreeMap::new();
    m.insert("ttl".to_owned(), Value::Bool(true));
    m.insert("body".to_owned(), Value::Text(text_blob(2048)));
    c.insert_with_ttl(b"expiring", &Value::Map(m), 12345)
        .unwrap();
    assert_eq!(c.ttl(b"expiring").unwrap(), Some(12345));
    assert_eq!(c.purge_expired(12345).unwrap(), 1);
    assert_eq!(c.get(b"expiring").unwrap(), None);

    // Edges on a compressed collection (edge namespaces stay raw; they
    // key into documents that are compressed).
    c.link(b"a", "knows", b"b").unwrap();
    assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
    // Deleting the endpoint document cascades the edge away.
    c.insert(b"a", &Value::Text(text_blob(4096))).unwrap();
    assert!(c.delete(b"a").unwrap());
    assert_eq!(c.neighbors(b"a", "knows").unwrap(), Vec::<Vec<u8>>::new());
}
