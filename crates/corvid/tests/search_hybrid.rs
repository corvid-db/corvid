//! Hybrid-search conformance (Task 9): RRF fusion (default/custom/invalid k,
//! the exact 1/(k+rank) arithmetic, duplicates, empty rankings), MMR
//! diversification (lambda 0/1 boundaries, out-of-range/NaN rejection, the
//! no-vector-source no-op, embedding-less docs surviving the rerank),
//! vector+text fusion ordering versus single sources, filters across both
//! sources, and the direct `reciprocal_rank_fusion`/`mmr` functions — driven
//! through the public API only.
//!
//! Contracts pinned by these tests (read from `src/fusion.rs` and
//! `src/builder.rs` first):
//!
//! * RRF: a key's fused score is the sum over rankings of
//!   `1/(k + rank)`, rank starting at 1, contributions added in source order;
//!   duplicates within ONE ranking count only at their best (first) rank;
//!   output is score-descending with key-order tiebreaks. All arithmetic
//!   below is over small exactly-representable reciprocals, so scores are
//!   asserted bitwise (`1/61 + 1/62` etc.). `fuse_rrf` validates at
//!   `run()`-time (audit C6): k must be finite and > 0 — zero, negative,
//!   NaN, ±inf all fail with `Error::InvalidArgument`.
//! * MMR: `lambda` in [0, 1] (boundaries included; NaN rejected by the range
//!   test) — also validated at run-time, even when the rerank would be a
//!   no-op. `lambda = 1` is pure relevance (`-metric.distance`), `lambda = 0`
//!   pure diversity (first pick: every score is exactly 0, so the key
//!   tiebreak decides); ties break by key. Through the builder, MMR reranks
//!   only the fused docs that HAVE a matching-dimension embedding, keeping
//!   their fused scores; the rest keep fused order after the reranked ones.
//! * The builder computes each source's ranking over the FILTERED candidate
//!   set (a true pre-ranking predicate), fuses in source-chaining order, and
//!   exposes only the fused score on `ResultRow`.
//!
//! The Wave-1 smoke test that anchored the radar skeleton is kept at the top.

use std::collections::BTreeMap;

use corvid::{Db, Metric, Value, field, mmr, reciprocal_rank_fusion};

fn doc(body: &str, v: Vec<f32>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    m.insert("v".to_owned(), Value::Vector(v));
    Value::Map(m)
}

fn doc_tagged(tag: &str, body: &str, v: Option<Vec<f32>>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("tag".to_owned(), Value::Text(tag.to_owned()));
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    if let Some(v) = v {
        m.insert("v".to_owned(), Value::Vector(v));
    }
    Value::Map(m)
}

fn keys(rows: &[corvid::ResultRow]) -> Vec<Vec<u8>> {
    rows.iter().map(|r| r.key.clone()).collect()
}

fn k(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|n| n.as_bytes().to_vec()).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

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

// ===========================================================================
// 1. Direct reciprocal_rank_fusion — formula, duplicates, empties, k
// ===========================================================================

/// The fused score is exactly `Σ 1/(k + rank)` with rank starting at 1,
/// contributions added in source order; output sorts score-desc with key
/// tiebreaks. `a` (ranks 1,2) and `b` (ranks 2,1) tie bitwise (IEEE addition
/// is commutative) and break by key; `c` and `d` tie at one contribution
/// each and break by key. The default constant is pinned at 60.
#[test]
fn hybrid_rrf_direct_formula_exact_scores_and_edges() {
    assert_eq!(corvid::DEFAULT_RRF_K, 60.0);

    let l1 = k(&["a", "b", "c"]);
    let l2 = k(&["b", "a", "d"]);
    let fused = reciprocal_rank_fusion(&[&l1, &l2], corvid::DEFAULT_RRF_K);
    assert_eq!(
        fused.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
        k(&["a", "b", "c", "d"])
    );
    // a: 1/(60+1) + 1/(60+2), in source order.
    assert_eq!(fused[0].1, 1.0f32 / 61.0f32 + 1.0f32 / 62.0f32);
    // b: same two contributions in the opposite order — bitwise equal.
    assert_eq!(fused[1].1, 1.0f32 / 62.0f32 + 1.0f32 / 61.0f32);
    assert_eq!(fused[0].1, fused[1].1);
    assert_eq!(fused[2].1, 1.0f32 / 63.0f32); // c: rank 3 of list 1 only
    assert_eq!(fused[3].1, 1.0f32 / 63.0f32); // d: rank 3 of list 2 only
    assert!(fused[1].1 > fused[2].1);

    // Duplicate keys WITHIN one ranking count only at their best (first)
    // rank: "a" counts once at rank 1, so "b" lands at rank 3.
    let dup = k(&["a", "a", "b"]);
    let fused = reciprocal_rank_fusion(&[&dup], corvid::DEFAULT_RRF_K);
    assert_eq!(fused[0].0, b"a".to_vec());
    assert_eq!(fused[0].1, 1.0f32 / 61.0f32);
    assert_eq!(fused[1].0, b"b".to_vec());
    assert_eq!(fused[1].1, 1.0f32 / 63.0f32);

    // One source empty: the other's contributions stand alone.
    let empty: Vec<Vec<u8>> = Vec::new();
    let solo = k(&["a", "b"]);
    let fused = reciprocal_rank_fusion(&[&empty, &solo], corvid::DEFAULT_RRF_K);
    assert_eq!(
        fused,
        vec![
            (b"a".to_vec(), 1.0f32 / 61.0f32),
            (b"b".to_vec(), 1.0f32 / 62.0f32)
        ]
    );
    // Both empty — and zero rankings — are empty.
    assert!(reciprocal_rank_fusion(&[&empty, &empty], 60.0).is_empty());
    assert!(reciprocal_rank_fusion(&[], 60.0).is_empty());

    // A duplicate doc ACROSS rankings merges by accumulation (covered above
    // via a/b); a custom k rescales every contribution exactly.
    let x = k(&["x", "y"]);
    let y = k(&["y", "z"]);
    let fused = reciprocal_rank_fusion(&[&x, &y], 1.0);
    assert_eq!(
        fused.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
        k(&["y", "x", "z"])
    );
    assert_eq!(fused[0].1, 1.0f32 / 3.0f32 + 1.0f32 / 2.0f32); // y: ranks 2, 1
    assert_eq!(fused[1].1, 1.0f32 / 2.0f32); // x: rank 1
    assert_eq!(fused[2].1, 1.0f32 / 3.0f32); // z: rank 2
}

// ===========================================================================
// 2. Direct mmr — lambda 0/1, diversity effect, k, ties
// ===========================================================================

/// Fixed geometry (Cosine, query [1,0]): near [1,0] at distance 0, mid
/// [0.7,0.7] at 1 - 0.7/sqrt(0.98) ≈ 0.2929, far [-1,0] at 2. `lambda = 1`
/// is pure relevance — that exact order. `lambda = 0` is pure diversity:
/// the first pick has every score exactly 0 (empty selected set), so the KEY
/// tiebreak decides (`a` over the more relevant `z`), and later picks
/// maximize distance from what is already selected. At `lambda = 0.1` the
/// diversity term dominates with a ~0.8 margin: the orthogonal candidate
/// beats the near-duplicate of the first pick. Ties between identical
/// embeddings break by key; k truncates and caps at the candidate count.
#[test]
fn hybrid_mmr_direct_lambda_zero_one_diversity_and_k() {
    let q = vec![1.0f32, 0.0];
    let near = (b"near".to_vec(), vec![1.0, 0.0]);
    let mid = (b"mid".to_vec(), vec![0.7, 0.7]);
    let far = (b"far".to_vec(), vec![-1.0, 0.0]);

    // lambda = 1: pure relevance.
    let out = mmr(
        &q,
        &[near.clone(), mid.clone(), far.clone()],
        1.0,
        3,
        Metric::Cosine,
    );
    assert_eq!(out, k(&["near", "mid", "far"]));

    // lambda = 0: pure diversity. Key-smallest `a` wins the all-zero first
    // round; then the candidate farthest from `a` ([0,1], orthogonal).
    let z = (b"z".to_vec(), vec![1.0, 0.0]);
    let y = (b"y".to_vec(), vec![0.99, 0.01]);
    let a = (b"a".to_vec(), vec![0.0, 1.0]);
    let out = mmr(
        &q,
        &[z.clone(), y.clone(), a.clone()],
        0.0,
        3,
        Metric::Cosine,
    );
    assert_eq!(out, k(&["a", "z", "y"]));

    // lambda = 0.1: relevance picks the query copy first, then the diversity
    // term (weight 0.9) sends the orthogonal candidate ahead of the
    // near-duplicate: `diverse` scores 0.1·(−1) − 0.9·(−1) = 0.8 while `dup2`
    // scores ≈ 0.1·(−0.00005) − 0.9·(−0.00005) ≈ 0.00004 — a decision margin
    // of ~0.8.
    let dup1 = (b"dup1".to_vec(), vec![1.0, 0.0]);
    let dup2 = (b"dup2".to_vec(), vec![0.99, 0.01]);
    let diverse = (b"diverse".to_vec(), vec![0.0, 1.0]);
    let out = mmr(&q, &[dup1, dup2, diverse], 0.1, 3, Metric::Cosine);
    assert_eq!(out, k(&["dup1", "diverse", "dup2"]));

    // k truncates and caps at the candidate count; empty candidates stay
    // empty; identical embeddings tie by key.
    let cands = [near.clone(), mid.clone(), far.clone()];
    assert_eq!(mmr(&q, &cands, 0.5, 1, Metric::Cosine).len(), 1);
    assert_eq!(mmr(&q, &cands, 0.5, 10, Metric::Cosine).len(), 3);
    assert!(mmr(&q, &[], 0.5, 5, Metric::Cosine).is_empty());
    let twins = [
        (b"z".to_vec(), vec![1.0, 0.0]),
        (b"a".to_vec(), vec![1.0, 0.0]),
    ];
    assert_eq!(mmr(&q, &twins, 1.0, 1, Metric::Cosine), k(&["a"]));
}

// ===========================================================================
// 3. Builder parameter validation — exact variants at run()
// ===========================================================================

/// `fuse_rrf` (audit C6): the builder stays fluent and validates at every
/// execution entry point — zero, negative, NaN, and ±inf k all fail
/// `run()` with `Error::InvalidArgument` naming the parameter, BEFORE any
/// ranking happens (the message format is pinned as a prefix).
#[test]
fn hybrid_fuse_rrf_rejects_invalid_k_at_run() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("rrf-bad");
    c.insert(b"a", &doc("rust database", vec![1.0, 0.0]))
        .unwrap();

    for bad in [0.0f32, -60.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = c.query().text("body", "rust", 5).fuse_rrf(bad).run();
        match err {
            Ok(rows) => panic!("fuse_rrf({bad}) must fail at run(), got {rows:?}"),
            Err(corvid::Error::InvalidArgument(msg)) => {
                assert!(
                    msg.starts_with("fuse_rrf: k must be > 0"),
                    "k={bad}: message pins the parameter: {msg}"
                );
            }
            Err(e) => panic!("k={bad}: wrong variant {e:?}"),
        }
    }
    // The default (and any positive finite custom k) runs.
    assert!(!c.query().text("body", "rust", 5).run().unwrap().is_empty());
    assert!(
        !c.query()
            .text("body", "rust", 5)
            .fuse_rrf(1.0)
            .run()
            .unwrap()
            .is_empty()
    );
}

/// `rerank_mmr` (audit C6): lambda outside [0, 1] (and NaN — the range test
/// rejects it) fails at run() with `Error::InvalidArgument`. The check fires
/// even when the rerank would be a NO-OP (no vector source), because
/// validation precedes execution. The closed boundaries 0.0 and 1.0 are
/// valid and exercised in the tests below.
#[test]
fn hybrid_rerank_mmr_rejects_out_of_range_and_nan_at_run() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("mmr-bad");
    c.insert(b"a", &doc("rust database", vec![1.0, 0.0]))
        .unwrap();

    for bad in [-0.01f32, 1.01, f32::NAN, f32::INFINITY] {
        // With a vector source available...
        let err = c
            .query()
            .vector("v", vec![1.0, 0.0], 5, Metric::Cosine)
            .rerank_mmr(bad)
            .run();
        match err {
            Ok(rows) => panic!("rerank_mmr({bad}) must fail at run(), got {rows:?}"),
            Err(corvid::Error::InvalidArgument(msg)) => {
                assert!(
                    msg.starts_with("rerank_mmr: lambda must be in [0, 1]"),
                    "lambda={bad}: message pins the parameter: {msg}"
                );
            }
            Err(e) => panic!("lambda={bad}: wrong variant {e:?}"),
        }
        // ...and on the no-op path (text source only, nothing to rerank).
        let err = c.query().text("body", "rust", 5).rerank_mmr(bad).run();
        assert!(
            matches!(err, Err(corvid::Error::InvalidArgument(_))),
            "lambda={bad}: validation must precede the no-op path"
        );
    }
}

// ===========================================================================
// 4. Vector + text fusion — the RRF boost, filters, select/limit
// ===========================================================================

/// The fusion corpus (Cosine, query [1,0]; every doc has both fields):
///
/// | key       | v        | body                          | vector rank | text rank |
/// |-----------|----------|-------------------------------|-------------|-----------|
/// | star      | [1,0]    | "rust database rust database" | 1 (0.0)     | 1         |
/// | vecnear   | [0.8,0.6]| "filler words only here"      | 2 (0.2)     | —         |
/// | textnear  | [0,1]    | "rust database memo pad extra"| 3 (1.0)     | 2         |
///
/// Text ranking (BM25, whole corpus: n=3, avg=13/3, df(rust)=df(database)=2):
/// star tf=2/len=4 ≈ 1.321 vs textnear tf=1/len=5 ≈ 0.884. Fused scores are
/// exact reciprocals: star 1/61+1/61 = 2/61, textnear 1/63+1/62,
/// vecnear 1/62 — star tops BOTH single-source runs at 1/61 each, so the
/// doc present in both sources wins the hybrid with a numerically pinned
/// RRF boost.
#[test]
fn hybrid_fusion_rrf_boost_beats_single_source() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("fuse");
    c.insert(
        b"star",
        &doc_tagged("a", "rust database rust database", Some(vec![1.0, 0.0])),
    )
    .unwrap();
    c.insert(
        b"vecnear",
        &doc_tagged("a", "filler words only here", Some(vec![0.8, 0.6])),
    )
    .unwrap();
    c.insert(
        b"textnear",
        &doc_tagged("b", "rust database memo pad extra", Some(vec![0.0, 1.0])),
    )
    .unwrap();

    // The hybrid run: fused order star, textnear, vecnear with exact scores
    // (contributions added vector-source-first, matching the chaining order).
    let rows = c
        .query()
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust database", 3)
        .run()
        .unwrap();
    assert_eq!(keys(&rows), k(&["star", "textnear", "vecnear"]));
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 61.0f32); // 2/61
    assert_eq!(rows[1].score, 1.0f32 / 63.0f32 + 1.0f32 / 62.0f32);
    assert_eq!(rows[2].score, 1.0f32 / 62.0f32);

    // The single-source runs: star still tops each — but at 1/61, not 2/61.
    let vec_only = c
        .query()
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .run()
        .unwrap();
    assert_eq!(keys(&vec_only), k(&["star", "vecnear", "textnear"]));
    assert_eq!(vec_only[0].score, 1.0f32 / 61.0f32);
    let text_only = c.query().text("body", "rust database", 3).run().unwrap();
    assert_eq!(keys(&text_only), k(&["star", "textnear"]));
    assert_eq!(text_only[0].score, 1.0f32 / 61.0f32);
    // The boost, numerically: presence in both sources doubles the fused
    // score of the top doc versus either single source.
    assert!(rows[0].score > vec_only[0].score && rows[0].score > text_only[0].score);

    // A filter is respected across BOTH sources and is applied BEFORE
    // ranking (rankings are computed among the matching docs only): keeping
    // tag "a" drops textnear, so vecnear's text absence no longer matters —
    // and keeping ONLY textnear makes it rank 1 of both filtered rankings
    // (fused 2/61), the pre-ranking-predicate pin.
    let keep_a = field("tag").eq(Value::Text("a".into()));
    let rows = c
        .query()
        .filter(keep_a.clone())
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust database", 3)
        .run()
        .unwrap();
    assert_eq!(keys(&rows), k(&["star", "vecnear"]));
    assert_eq!(rows[0].score, 2.0f32 * (1.0f32 / 61.0f32));
    assert_eq!(rows[1].score, 1.0f32 / 62.0f32);
    let rows = c
        .query()
        .filter(field("tag").eq(Value::Text("b".into())))
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust database", 3)
        .run()
        .unwrap();
    assert_eq!(keys(&rows), k(&["textnear"]));
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 61.0f32);

    // select + limit interplay: limit windows the FUSED order (star,
    // textnear), select narrows each document without touching key/score.
    let rows = c
        .query()
        .select(["body"])
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust database", 3)
        .limit(2)
        .run()
        .unwrap();
    assert_eq!(keys(&rows), k(&["star", "textnear"]));
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 61.0f32);
    assert_eq!(
        rows[0].document,
        Value::Map(BTreeMap::from([(
            "body".to_owned(),
            Value::Text("rust database rust database".into())
        )]))
    );
    assert!(rows[0].document.get("v").is_none());
}

// ===========================================================================
// 5. rerank_mmr through the builder
// ===========================================================================

/// MMR without a vector source is a documented no-op: a text-only query
/// with `.rerank_mmr(0.7)` returns byte-identical rows (keys, fused scores,
/// documents) to the same query without it.
#[test]
fn hybrid_rerank_mmr_noop_without_vector_source() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("noop");
    c.insert(b"a", &doc("rust database rust database", vec![1.0, 0.0]))
        .unwrap();
    c.insert(b"b", &doc("rust database extra words", vec![0.0, 1.0]))
        .unwrap();

    let plain = c.query().text("body", "rust database", 5).run().unwrap();
    let reranked = c
        .query()
        .text("body", "rust database", 5)
        .rerank_mmr(0.7)
        .run()
        .unwrap();
    assert_eq!(plain, reranked);
    assert_eq!(keys(&plain), k(&["a", "b"]));
}

/// lambda = 1 through the builder is pure vector relevance over the fused
/// set (fused scores kept per row): on the fusion corpus the fused order is
/// star, textnear, vecnear but the Cosine relevance order is star (0.0),
/// vecnear (0.2), textnear (1.0) — the rerank swaps the tail pair while
/// every row keeps its own fused score.
#[test]
fn hybrid_rerank_mmr_lambda_one_reorders_by_relevance() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("mmr-l1");
    c.insert(
        b"star",
        &doc_tagged("a", "rust database rust database", Some(vec![1.0, 0.0])),
    )
    .unwrap();
    c.insert(
        b"vecnear",
        &doc_tagged("a", "filler words only here", Some(vec![0.8, 0.6])),
    )
    .unwrap();
    c.insert(
        b"textnear",
        &doc_tagged("b", "rust database memo pad extra", Some(vec![0.0, 1.0])),
    )
    .unwrap();

    let rows = c
        .query()
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust database", 3)
        .rerank_mmr(1.0)
        .run()
        .unwrap();
    assert_eq!(keys(&rows), k(&["star", "vecnear", "textnear"]));
    assert_eq!(rows[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 61.0f32); // star keeps 2/61
    assert_eq!(rows[1].score, 1.0f32 / 62.0f32); // vecnear keeps its fused score
    assert_eq!(rows[2].score, 1.0f32 / 63.0f32 + 1.0f32 / 62.0f32); // textnear ditto
}

/// lambda = 0 through the builder is pure diversity: fused order a, b, c
/// (a: 2/61, b: 2/62, c: 1/63), but `b` is a near-duplicate of `a` in
/// vector space while `c` is orthogonal — after the rerank `c` jumps to
/// second (first pick `a` wins the all-zero key tiebreak).
#[test]
fn hybrid_rerank_mmr_lambda_zero_diversifies() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("mmr-l0");
    c.insert(b"a", &doc("rust db rust db", vec![1.0, 0.0]))
        .unwrap();
    c.insert(b"b", &doc("rust db extra words", vec![0.99, 0.01]))
        .unwrap();
    c.insert(b"c", &doc("filler here", vec![0.0, 1.0])).unwrap();

    let fused = c
        .query()
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust db", 3)
        .run()
        .unwrap();
    assert_eq!(keys(&fused), k(&["a", "b", "c"]));
    assert_eq!(fused[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 61.0f32);
    assert_eq!(fused[1].score, 1.0f32 / 62.0f32 + 1.0f32 / 62.0f32);
    assert_eq!(fused[2].score, 1.0f32 / 63.0f32);

    let reranked = c
        .query()
        .vector("v", vec![1.0, 0.0], 3, Metric::Cosine)
        .text("body", "rust db", 3)
        .rerank_mmr(0.0)
        .run()
        .unwrap();
    assert_eq!(keys(&reranked), k(&["a", "c", "b"]));
    // Fused scores travel with the rows.
    for r in &reranked {
        let base = fused.iter().find(|f| f.key == r.key).unwrap();
        assert_eq!(r.score, base.score);
    }
}

/// Docs without embeddings survive the rerank: they can only enter the
/// fused set through the text source; the rerank orders every embedded doc
/// first (by MMR) and appends the embedding-less tail in fused order. Fused
/// order here is mid, near, noemb, noemb2 (mid 1/62+1/61 edges near
/// 1/61+1/64 — tf=2 text beats tf=1 by ~5e-4); at lambda = 1 the relevance
/// order of the embedded pair is near (0.0), mid (0.2), so `near` jumps to
/// the top while `noemb`/`noemb2` keep their fused tail order at the end.
#[test]
fn hybrid_rerank_mmr_docs_without_embeddings_survive() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("mmr-tail");
    c.insert(
        b"mid",
        &doc_tagged("t", "rust database rust database", Some(vec![0.8, 0.6])),
    )
    .unwrap();
    c.insert(b"near", &doc_tagged("t", "rust", Some(vec![1.0, 0.0])))
        .unwrap();
    c.insert(b"noemb", &doc_tagged("t", "rust database extra", None))
        .unwrap();
    c.insert(
        b"noemb2",
        &doc_tagged("t", "rust database more extra padding", None),
    )
    .unwrap();

    let fused = c
        .query()
        .vector("v", vec![1.0, 0.0], 4, Metric::Cosine)
        .text("body", "rust database", 4)
        .run()
        .unwrap();
    assert_eq!(keys(&fused), k(&["mid", "near", "noemb", "noemb2"]));
    assert_eq!(fused[1].score, 1.0f32 / 61.0f32 + 1.0f32 / 64.0f32); // near

    let reranked = c
        .query()
        .vector("v", vec![1.0, 0.0], 4, Metric::Cosine)
        .text("body", "rust database", 4)
        .rerank_mmr(1.0)
        .run()
        .unwrap();
    assert_eq!(keys(&reranked), k(&["near", "mid", "noemb", "noemb2"]));
    assert_eq!(reranked[0].score, 1.0f32 / 61.0f32 + 1.0f32 / 64.0f32);
    // The tail keeps the fused order after ALL reranked docs.
    assert_eq!(reranked[2].key, b"noemb".to_vec());
    assert_eq!(reranked[3].key, b"noemb2".to_vec());
    assert_eq!(reranked[2].score, 1.0f32 / 62.0f32); // noemb keeps its fused score
}
