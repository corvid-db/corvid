//! Vector-search conformance (Task 8): the Metric × Quantization cross,
//! approx vs exact dispatch, `Hit.approximate` semantics, k boundaries,
//! dimension-mismatch schema-on-read, zero-norm vectors, direct `Hnsw` API,
//! the `create_vector_index` overload family, and builder composition
//! (`vector()` + select/order/limit/offset/filter), driven through the public
//! API only.
//!
//! Contracts pinned by these tests (read from `src/distance.rs`,
//! `src/quant.rs`, `src/hnsw.rs`, `src/index.rs`, `src/query.rs`, and
//! `src/builder.rs` first):
//!
//! * Every metric is "lower is nearer": Cosine = `1 - cos_sim` (zero-norm →
//!   exactly `1.0`), Dot = negated dot (larger dot first), L2 = squared
//!   euclidean. Exact scores for the fixed corpus below are hand-computed
//!   and bitwise-assertable (every component is a small integer, exactly
//!   representable in `f32`, and the kernels are exact on such values).
//! * `Collection::vector_search` uses a registered ANN index whenever one
//!   matches `(field, metric)` — there is no `approx` switch at this layer —
//!   and marks every returned `Hit.approximate = true`; without a matching
//!   index (absent, metric mismatch, or dimension-mismatched graph) it falls
//!   back to the exact streaming scan with `approximate = false`. Either way
//!   `Hit.distance` is the exact metric distance recomputed from the stored
//!   document (audit B6) — quantization never leaks into the reported score.
//! * Quantization changes the *candidate set*, not the reported distances:
//!   after the exact rerank, `k >= corpus` results are in exact-metric order
//!   for every (metric, quantization) pair. For `k < corpus`, binary /
//!   scalar rankings may differ from exact (pinned below: binary Dot k=1
//!   returns a Hamming-tie winner, not the exact winner) — that is the
//!   documented recall cost of `create_vector_index_quantized`.
//! * `QueryBuilder::vector` + `filter` without `.approx()` ranks the
//!   *matches* (a true pre-filter: top-k among matching docs); with
//!   `.approx()` the ANN top-k overall is post-filtered (a selective filter
//!   may return fewer than `k` rows). `order_by` replaces the rank order but
//!   keeps each row's RRF score; a single ranked source scores
//!   `1/(60+rank)`.
//! * Dimension mismatch never errors (schema-on-read): docs whose vector
//!   dimension differs from the query are skipped; an index whose fixed
//!   dimension rejects the query falls back to the exact scan.
//! * `k = 0` is the empty window (not an error) on every path; `k > corpus`
//!   returns the whole corpus; the HNSW `ef_search` is raised to at least
//!   `k`.
//!
//! The Wave-1 smoke test that anchored the radar skeleton is kept at the top.

use std::collections::BTreeMap;

use corvid::distance::{cosine_distance, dot, l2_squared};
use corvid::hnsw::{DEFAULT_EF_CONSTRUCTION, DEFAULT_M};
use corvid::{Db, Hit, Hnsw, Metric, Quantization, Value, field};

fn doc(tag: &str, v: Vec<f32>) -> Value {
    let mut m = BTreeMap::new();
    m.insert("tag".to_owned(), Value::Text(tag.to_owned()));
    m.insert("v".to_owned(), Value::Vector(v));
    Value::Map(m)
}

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Vec<f32>)]) -> corvid::Collection<'a> {
    let c = db.collection(name);
    for (k, v) in docs {
        c.insert(k, &doc("t", v.clone())).unwrap();
    }
    c
}

/// The fixed-corpus keys of a hit list, in order (order IS the contract).
fn hit_keys(hits: &[Hit]) -> Vec<Vec<u8>> {
    hits.iter().map(|h| h.key.clone()).collect()
}

fn k(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|n| n.as_bytes().to_vec()).collect()
}

/// The reference corpus: axis units, a negated axis unit, and a scaled /
/// duplicated variant of `a`, so the three metrics order it three different
/// ways (ties by key, per the documented tiebreak):
///
/// | key  | vector      | Cosine | Dot    | L2   |
/// |------|-------------|--------|--------|------|
/// | `a`  | [1, 0]      | 0      | -1     | 0    |
/// | `b`  | [0, 1]      | 1      | -0.0   | 2    |
/// | `c`  | [-1, 0]     | 2      | 1      | 4    |
/// | `d`  | [2, 0]      | 0      | -2     | 1    |
/// | `e`  | [1, 0]      | 0      | -1     | 0    |
///
/// * Cosine order: `a d e b c` (a/d/e tie at 0, key order),
/// * Dot order:    `d a e b c` (d's larger dot first; a/e tie at -1),
/// * L2 order:     `a e d b c` (a/e tie at 0; magnitude matters).
const CORPUS: [(&[u8], [f32; 2]); 5] = [
    (b"a", [1.0, 0.0]),
    (b"b", [0.0, 1.0]),
    (b"c", [-1.0, 0.0]),
    (b"d", [2.0, 0.0]),
    (b"e", [1.0, 0.0]),
];

const QUERY: [f32; 2] = [1.0, 0.0];

/// Expected ranking (keys) and hand-computed exact distances for `QUERY`
/// under each metric over [`CORPUS`].
fn expected(metric: Metric) -> (Vec<&'static str>, Vec<f32>) {
    match metric {
        Metric::Cosine => (vec!["a", "d", "e", "b", "c"], vec![0.0, 0.0, 0.0, 1.0, 2.0]),
        Metric::Dot => (
            vec!["d", "a", "e", "b", "c"],
            vec![-2.0, -1.0, -1.0, 0.0, 1.0],
        ),
        Metric::L2 => (vec!["a", "e", "d", "b", "c"], vec![0.0, 0.0, 1.0, 2.0, 4.0]),
    }
}

fn all_metrics() -> [Metric; 3] {
    [Metric::Cosine, Metric::Dot, Metric::L2]
}

fn all_quants() -> [Quantization; 3] {
    [
        Quantization::None,
        Quantization::Binary,
        Quantization::Scalar,
    ]
}

/// The stored `Value::Vector` of a corpus key (documents keep the full f32
/// embedding regardless of index quantization).
fn corpus_vector(key: &[u8]) -> Value {
    let (_, v) = CORPUS.iter().find(|(k, _)| *k == key).expect("corpus key");
    Value::Vector(v.to_vec())
}

/// [`CORPUS`] as seedable `(key, Vec<f32>)` pairs.
fn corpus_docs() -> Vec<(&'static [u8], Vec<f32>)> {
    CORPUS.iter().map(|(k, v)| (*k, v.to_vec())).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

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

// ===========================================================================
// 1. Metric × Quantization cross (3 × 3), plus the exact-path baseline
// ===========================================================================

/// The 3×3 cross: one quantized in-memory index per (metric, quantization).
///
/// Justification for asserting the FULL exact order at `k >= corpus` under
/// every quantization: the collection-level index is built with
/// `DEFAULT_M`/`DEFAULT_EF_CONSTRUCTION` (m0 = 2·16 = 32), so a 5-node graph
/// is complete on layer 0 (every insert links to all existing nodes), and
/// `BuiltIndex::search` over-fetches with `want = k + dead` (dead = 0 here —
/// nothing is ever deleted) and `ef = max(4·want, 64) >= 64 > 5` — the
/// on-disk path's `ef = max(4k, 64)` form is not what runs here. Either way
/// the beam provably reaches every node, so the candidate set is all live
/// keys regardless of quantization (quantization perturbs distances, not
/// reachability). The exact rerank then sorts by (exact distance, key),
/// reproducing the exact-path order with bitwise-exact distances.
#[test]
fn vector_metric_quantization_cross_orders_and_exact_distances() {
    for metric in all_metrics() {
        for quant in all_quants() {
            let db = Db::open_in_memory().unwrap();
            let name = format!("cross_{metric:?}_{quant:?}");
            let c = seed(&db, &name, &corpus_docs());
            c.create_vector_index_quantized("v", metric, quant).unwrap();

            let (want_keys, want_dists) = expected(metric);
            let hits = c.vector_search("v", &QUERY, 8, metric).unwrap();
            assert_eq!(
                hit_keys(&hits),
                k(&want_keys),
                "{metric:?}/{quant:?}: k > corpus must return every doc in exact order"
            );
            assert_eq!(hits.len(), 5);
            for (h, want) in hits.iter().zip(want_dists) {
                // Audit B6: the reported distance is the exact metric value
                // recomputed from the stored document — never the graph's
                // Hamming/reconstruction approximation.
                assert_eq!(h.distance, want, "{metric:?}/{quant:?} exact distance");
                assert!(h.approximate, "{metric:?}/{quant:?} served by the index");
                // The full stored document comes back with the hit.
                assert_eq!(h.document.get("v"), Some(&corpus_vector(&h.key)));
            }

            // k < corpus under full precision (None): the graph distances ARE
            // the metric distances, so the prefix of the exact order is
            // returned.
            if quant == Quantization::None {
                let hits = c.vector_search("v", &QUERY, 2, metric).unwrap();
                assert_eq!(hit_keys(&hits), k(&want_keys[..2]));
                assert!(hits.iter().all(|h| h.approximate));
            }
        }
    }
}

/// Scalar quantization is near-lossless on this corpus (per-component
/// reconstruction error <= scale/2 = (max-min)/510 < 0.004, far below the
/// 1.0 metric gaps; exact ties stay ties broken by node id), so its k=1
/// winner equals the exact winner for every metric. Binary quantization
/// keeps only sign bits: `[1,0]`, `[0,1]`, `[2,0]`, `[1,0]` all encode to
/// the same 2-bit pattern (a 0.0 component has sign ">= 0" and sets its
/// bit) and `[-1,0]` differs in one bit, so four nodes tie at Hamming
/// distance 0. Self-contained proof that the tie winner is `a` (so k=1
/// returns it under EVERY metric — including Dot, where the exact winner
/// is `d`; that pinned divergence is the documented recall cost of binary
/// quantization, and `approximate = true` records it): the lazily built
/// in-memory graph (`build_index` in index.rs) scans the collection in
/// ascending key order and assigns sequential HNSW ids, so ids ARE key
/// order — a=0, b=1, c=2, d=3, e=4 — and `Hnsw`'s candidate ordering
/// (the `Cand` heap and the final sort in search_layer) breaks distance
/// ties by smaller id. The Hamming-0 group therefore yields node 0 = `a`.
#[test]
fn vector_quantization_k1_binary_diverges_scalar_matches_exact() {
    // Scalar: matches the exact k=1 winner for every metric.
    for metric in all_metrics() {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, "scalar-k1", &corpus_docs());
        c.create_vector_index_quantized("v", metric, Quantization::Scalar)
            .unwrap();
        let (want_keys, _) = expected(metric);
        let hits = c.vector_search("v", &QUERY, 1, metric).unwrap();
        assert_eq!(hit_keys(&hits), k(&[want_keys[0]]), "{metric:?} scalar k=1");
        assert_eq!(hits[0].distance, expected(metric).1[0]);
    }

    // Binary: the Hamming-tie winner `a` for every metric; under Dot the
    // exact winner is `d` — the approx answer legitimately differs.
    for metric in all_metrics() {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, "binary-k1", &corpus_docs());
        c.create_vector_index_quantized("v", metric, Quantization::Binary)
            .unwrap();
        let hits = c.vector_search("v", &QUERY, 1, metric).unwrap();
        assert_eq!(hit_keys(&hits), k(&["a"]), "{metric:?} binary k=1");
        assert!(hits[0].approximate);
        if metric == Metric::Dot {
            // Divergence pinned: exact Dot top-1 is `d` at -2.0; binary
            // returns the Hamming-tie winner `a` at its exact distance.
            assert_eq!(hits[0].distance, -1.0);
            let plain = Db::open_in_memory().unwrap();
            let exact = seed(&plain, "binary-k1-exact", &corpus_docs());
            let exact_hits = exact.vector_search("v", &QUERY, 1, Metric::Dot).unwrap();
            assert_eq!(hit_keys(&exact_hits), k(&["d"]));
            assert_eq!(exact_hits[0].distance, -2.0);
            assert!(!exact_hits[0].approximate);
        }
    }
}

/// The exact (unindexed) path per metric: full order, hand-computed exact
/// scores per `distance.rs` formulas, `approximate = false`. The raw public
/// kernels are asserted on the same values.
#[test]
fn vector_exact_path_scores_match_hand_computed_formulas() {
    // The public kernels, by hand.
    assert_eq!(dot(&[1.0, 0.0], &[2.0, 0.0]), 2.0);
    assert_eq!(dot(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    assert_eq!(dot(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
    assert_eq!(l2_squared(&[1.0, 0.0], &[2.0, 0.0]), 1.0);
    assert_eq!(l2_squared(&[1.0, 0.0], &[0.0, 1.0]), 2.0);
    assert_eq!(l2_squared(&[1.0, 0.0], &[-1.0, 0.0]), 4.0);
    assert_eq!(cosine_distance(&[1.0, 0.0], &[2.0, 0.0]), 0.0);
    assert_eq!(cosine_distance(&[1.0, 0.0], &[0.0, 1.0]), 1.0);
    assert_eq!(cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]), 2.0);
    assert_eq!(Metric::Dot.distance(&[1.0, 0.0], &[2.0, 0.0]), -2.0);
    assert_eq!(Metric::L2.distance(&[1.0, 0.0], &[2.0, 0.0]), 1.0);
    assert_eq!(Metric::Cosine.distance(&[1.0, 0.0], &[2.0, 0.0]), 0.0);

    for metric in all_metrics() {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, "exact", &corpus_docs());
        let (want_keys, want_dists) = expected(metric);
        let hits = c.vector_search("v", &QUERY, 5, metric).unwrap();
        assert_eq!(hit_keys(&hits), k(&want_keys), "{metric:?} exact order");
        for (h, want) in hits.iter().zip(want_dists) {
            assert_eq!(h.distance, want, "{metric:?} exact score");
            assert!(!h.approximate, "{metric:?} exact path is not approximate");
        }
    }
}

/// Index-vs-scan twin parity for full precision: an index must never change
/// the answer, for every k from 1 past the corpus size.
#[test]
fn vector_indexed_none_matches_unindexed_twin_for_all_k() {
    for metric in all_metrics() {
        let plain = Db::open_in_memory().unwrap();
        let indexed = Db::open_in_memory().unwrap();
        let c_plain = seed(&plain, "twin", &corpus_docs());
        let c_indexed = seed(&indexed, "twin", &corpus_docs());
        c_indexed.create_vector_index("v", metric).unwrap();
        for k in 1..=6usize {
            let exact = c_plain.vector_search("v", &QUERY, k, metric).unwrap();
            let ann = c_indexed.vector_search("v", &QUERY, k, metric).unwrap();
            assert_eq!(
                hit_keys(&ann),
                hit_keys(&exact),
                "{metric:?} k={k}: index must match the scan"
            );
            assert_eq!(
                ann.iter().map(|h| h.distance).collect::<Vec<_>>(),
                exact.iter().map(|h| h.distance).collect::<Vec<_>>(),
                "{metric:?} k={k}: distances must match the scan"
            );
            assert!(ann.iter().all(|h| h.approximate));
            assert!(exact.iter().all(|h| !h.approximate));
        }
    }
}

// ===========================================================================
// 2. approx dispatch — Hit.approximate, metric-mismatch fallback
// ===========================================================================

/// `vector_search` has no approx switch of its own: the *presence of a
/// matching index* is the dispatch, observable as `Hit.approximate`. A
/// metric mismatch (index is Cosine, query is L2) is not consultable — the
/// search falls back to the exact scan and reports `approximate = false`
/// with the exact L2 answers.
#[test]
fn vector_index_dispatch_approximate_flag_and_metric_mismatch_fallback() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "dispatch", &corpus_docs());

    // No index: exact.
    let before = c.vector_search("v", &QUERY, 5, Metric::Cosine).unwrap();
    assert!(before.iter().all(|h| !h.approximate));

    // Same field+metric index: the ANN arm drives, flag flips, answers match.
    c.create_vector_index("v", Metric::Cosine).unwrap();
    let after = c.vector_search("v", &QUERY, 5, Metric::Cosine).unwrap();
    assert!(after.iter().all(|h| h.approximate));
    assert_eq!(hit_keys(&after), hit_keys(&before));
    assert_eq!(
        after.iter().map(|h| h.distance).collect::<Vec<_>>(),
        before.iter().map(|h| h.distance).collect::<Vec<_>>()
    );

    // Metric mismatch: the Cosine index cannot serve an L2 query — exact
    // fallback, approximate = false, exact L2 order/scores.
    let l2 = c.vector_search("v", &QUERY, 5, Metric::L2).unwrap();
    let (want_keys, want_dists) = expected(Metric::L2);
    assert_eq!(hit_keys(&l2), k(&want_keys));
    for (h, want) in l2.iter().zip(want_dists) {
        assert_eq!(h.distance, want);
        assert!(!h.approximate);
    }
}

/// The builder's `approx` flag: without it, a FILTERED vector query runs the
/// exact streaming arm (filter before rank: top-k of the MATCHES); with it,
/// the ANN top-k overall is post-filtered (all rows satisfy the filter, but
/// a selective filter may leave fewer than `k`). Corpus under L2, query
/// `[1,0]`: g1=0.0, g2~=0.01, h~=0.04, k=0.25 squared distance — the global
/// top-2 are both "drop" rows.
#[test]
fn vector_builder_approx_prefilters_exact_vs_postfilters_approx() {
    let docs: Vec<(&[u8], Vec<f32>)> = vec![
        (b"g1", vec![1.0, 0.0]),
        (b"g2", vec![0.9, 0.0]),
        (b"h", vec![0.8, 0.0]),
        (b"k", vec![0.5, 0.0]),
    ];
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("approx");
    for (key, v) in &docs {
        // `h` and `k` are the one-byte keys — the keepers.
        let tag = if key.len() == 1 { "keep" } else { "drop" };
        c.insert(key, &doc(tag, v.clone())).unwrap();
    }
    c.create_vector_index("v", Metric::L2).unwrap();
    let keep = field("tag").eq(Value::Text("keep".into()));

    // Exact (no approx): top-2 among the keepers — h (0.04) then k (0.25).
    let rows = c
        .query()
        .filter(keep.clone())
        .vector("v", vec![1.0, 0.0], 2, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
        k(&["h", "k"]),
        "no approx: top-k of the matches, not top-k overall then filter"
    );
    // Single ranked source: RRF fused score 1/(60+rank).
    assert_eq!(rows[0].score, 1.0f32 / 61.0);
    assert_eq!(rows[1].score, 1.0f32 / 62.0);

    // approx + filter, k=2: the ANN top-2 overall is {g1, g2}; both fail the
    // filter, so the (documented) result is SHORTER than k — not the exact
    // top-2 of matches.
    let rows = c
        .query()
        .filter(keep.clone())
        .approx()
        .vector("v", vec![1.0, 0.0], 2, Metric::L2)
        .run()
        .unwrap();
    assert!(
        rows.is_empty(),
        "approx post-filters the global top-k: both were dropped"
    );

    // approx with k wide enough to cover the corpus: every doc is a
    // candidate, so the keepers come back in exact order.
    let rows = c
        .query()
        .filter(keep)
        .approx()
        .vector("v", vec![1.0, 0.0], 4, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
        k(&["h", "k"])
    );
    // Every mode: all returned rows satisfy the filter (asserted by the key
    // sets above) and none of the dropped rows leaks in.
}

// ===========================================================================
// 3. k boundaries
// ===========================================================================

#[test]
fn vector_k_boundaries_zero_one_n_and_beyond() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kb", &corpus_docs());
    c.create_vector_index("v", Metric::Cosine).unwrap();

    // k = 0: the empty window on every path — never an error.
    assert!(
        c.vector_search("v", &QUERY, 0, Metric::Cosine)
            .unwrap()
            .is_empty()
    );
    assert!(
        c.query()
            .vector("v", QUERY.to_vec(), 0, Metric::Cosine)
            .run()
            .unwrap()
            .is_empty()
    );

    // k = 1: the single best (Cosine ties a/d/e at 0 break by key -> a).
    let hits = c.vector_search("v", &QUERY, 1, Metric::Cosine).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a"]));
    assert_eq!(hits[0].distance, 0.0);

    // k = n: everything, in order.
    let hits = c.vector_search("v", &QUERY, 5, Metric::Cosine).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "d", "e", "b", "c"]));

    // k > n: still everything, no panic, no duplicates.
    let hits = c.vector_search("v", &QUERY, 100, Metric::Cosine).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "d", "e", "b", "c"]));

    // Filter shrinking the candidate pool below k (exact streaming arm):
    // the top-k of the two matches, not k rows.
    let db2 = Db::open_in_memory().unwrap();
    let c2 = db2.collection("kb2");
    c2.insert(b"h", &doc("keep", vec![0.8, 0.0])).unwrap();
    c2.insert(b"k", &doc("keep", vec![0.5, 0.0])).unwrap();
    c2.insert(b"g", &doc("drop", vec![1.0, 0.0])).unwrap();
    let rows = c2
        .query()
        .filter(field("tag").eq(Value::Text("keep".into())))
        .vector("v", vec![1.0, 0.0], 5, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
        k(&["h", "k"]),
        "k above the match count yields the matches, all of them"
    );
}

// ===========================================================================
// 4. Dimension mismatch (schema-on-read, never an error)
// ===========================================================================

#[test]
fn vector_dimension_mismatch_skips_docs_and_index_falls_back() {
    // Query dim != every doc dim (indexed and not): no error, empty result.
    for indexed in [false, true] {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, "dim", &corpus_docs());
        if indexed {
            c.create_vector_index("v", Metric::Cosine).unwrap();
        }
        let hits = c
            .vector_search("v", &[1.0, 0.0, 0.0], 5, Metric::Cosine)
            .unwrap();
        assert!(hits.is_empty(), "no 3-dim docs exist (indexed={indexed})");
    }

    // Mixed dims, no index: each query dim sees only its own dimension.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("mixed");
    c.insert(b"m1", &doc("t", vec![1.0, 0.0])).unwrap();
    c.insert(b"m2", &doc("t", vec![1.0, 0.0, 0.0])).unwrap();
    c.insert(b"m3", &doc("t", vec![0.0, 1.0])).unwrap();
    let hits = c.vector_search("v", &[1.0, 0.0], 5, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["m1", "m3"]));
    let hits = c
        .vector_search("v", &[1.0, 0.0, 0.0], 5, Metric::L2)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["m2"]));

    // Creating an index over a mixed-dim field succeeds (the first scanned
    // vector pins the graph's dimension; the rest are skipped); every query
    // dim still sees exactly its own docs.
    c.create_vector_index("v", Metric::L2).unwrap();
    let hits = c.vector_search("v", &[1.0, 0.0], 5, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["m1", "m3"]));
    let hits = c
        .vector_search("v", &[1.0, 0.0, 0.0], 5, Metric::L2)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["m2"]));

    // Inserting a wrong-dim doc AFTER index creation: the document is stored
    // (gettable) but never indexed or returned by the matching-dim search.
    let db2 = Db::open_in_memory().unwrap();
    let c2 = seed(&db2, "dim2", &corpus_docs());
    c2.create_vector_index("v", Metric::L2).unwrap();
    c2.insert(b"w", &doc("t", vec![1.0, 0.0, 0.0])).unwrap();
    assert!(
        c2.get(b"w").unwrap().is_some(),
        "the document itself is stored"
    );
    let hits = c2.vector_search("v", &QUERY, 10, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "e", "d", "b", "c"]));
    assert!(!hits.iter().any(|h| h.key == b"w".to_vec()));
    // ...and a wrong-dim query still returns only the wrong-dim doc.
    let hits = c2
        .vector_search("v", &[1.0, 0.0, 0.0], 10, Metric::L2)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["w"]));
    // Deleting it works and changes nothing else.
    assert!(c2.delete(b"w").unwrap());
    let hits = c2
        .vector_search("v", &[1.0, 0.0, 0.0], 10, Metric::L2)
        .unwrap();
    assert!(hits.is_empty());
}

// ===========================================================================
// 5. Zero-norm vectors
// ===========================================================================

/// `distance.rs`: a zero-norm vector has undefined direction and is treated
/// as maximally distant under Cosine (exactly `1.0`). Under Dot a zero
/// vector dots to (negated) zero; under L2 it is an ordinary point.
#[test]
fn vector_zero_norm_cosine_dot_l2_ranking() {
    // The zero-norm corpus: aligned, orthogonal, and zero vectors.
    let zn: [(&[u8], Vec<f32>); 3] = [
        (b"a", vec![1.0, 0.0]),
        (b"b", vec![0.0, 1.0]),
        (b"z", vec![0.0, 0.0]),
    ];

    // Exact path, Cosine, zero-norm DOCUMENT: distance exactly 1.0, tied
    // with the orthogonal doc and behind the aligned one.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "zn1", &zn);
    let hits = c
        .vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "b", "z"]));
    assert_eq!(hits[0].distance, 0.0);
    assert_eq!(hits[1].distance, 1.0);
    assert_eq!(hits[2].distance, 1.0);

    // Same corpus through a full-precision index: identical order/scores
    // (complete 3-node graph, exact rerank).
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "zn2", &zn);
    c.create_vector_index("v", Metric::Cosine).unwrap();
    let hits = c
        .vector_search("v", &[1.0, 0.0], 3, Metric::Cosine)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "b", "z"]));
    assert_eq!(hits[2].distance, 1.0);
    assert!(hits.iter().all(|h| h.approximate));

    // Zero-norm QUERY under Cosine: every doc is maximally distant at 1.0;
    // the order is pure key order.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "zn3", &zn);
    let hits = c
        .vector_search("v", &[0.0, 0.0], 3, Metric::Cosine)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "b", "z"]));
    assert!(hits.iter().all(|h| h.distance == 1.0));

    // Zero norm under Dot: -0.0 against everything; ties by key. The aligned
    // doc still wins (dot 1), the zero doc and the orthogonal doc tie at 0.
    let hits = c.vector_search("v", &[1.0, 0.0], 3, Metric::Dot).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "b", "z"]));
    assert_eq!(hits[0].distance, -1.0);
    assert_eq!(hits[1].distance, 0.0);
    assert_eq!(hits[2].distance, 0.0);

    // Zero-norm QUERY under Dot: every distance is (negated) zero -> key order.
    let hits = c.vector_search("v", &[0.0, 0.0], 3, Metric::Dot).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "b", "z"]));
    assert!(hits.iter().all(|h| h.distance == 0.0));

    // Zero norm under L2: an ordinary point — the zero doc sits at distance
    // 1.0 from the query, between the aligned and orthogonal docs.
    let hits = c.vector_search("v", &[1.0, 0.0], 3, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "z", "b"]));
    assert_eq!(hits[0].distance, 0.0);
    assert_eq!(hits[1].distance, 1.0);
    assert_eq!(hits[2].distance, 2.0);
}

// ===========================================================================
// 6. Direct Hnsw API (pub) — params, determinism, k/ef rules
// ===========================================================================

/// `Hnsw` is public: drive it directly. `with_params(metric, 1, 1)` clamps
/// m to 2 (documented `.max(2)`) and ef_construction to m; on a 4-node
/// corpus the layer-0 graph stays connected (every insert links back to at
/// least one existing node), so with `ef_search >= 4` the search provably
/// visits every node — the ranking equals the brute-force order. Insert ids
/// are the insertion order, the build is reproducible (seeded PRNG), k=0
/// yields empty, ef_search is raised to at least k, and results are
/// nearest-first.
#[test]
fn vector_hnsw_direct_api_extreme_params_and_determinism() {
    // The documented defaults, pinned.
    assert_eq!(DEFAULT_M, 16);
    assert_eq!(DEFAULT_EF_CONSTRUCTION, 128);

    let data: Vec<Vec<f32>> = vec![
        vec![1.0, 0.0],  // id 0: L2 dist 0 from the query
        vec![0.0, 1.0],  // id 1: 2 (ties with id 2 break by id)
        vec![0.0, -1.0], // id 2: 2
        vec![-1.0, 0.0], // id 3: 4
    ];

    let mut h = Hnsw::with_params(Metric::L2, 1, 1);
    assert!(h.is_empty());
    assert_eq!(h.len(), 0);
    assert!(h.search(&[1.0, 0.0], 3, 8).is_empty(), "empty index");

    for (i, v) in data.iter().enumerate() {
        assert_eq!(h.insert(v.clone()), i, "ids are insertion order");
    }
    assert_eq!(h.len(), 4);
    assert!(!h.is_empty());

    // Brute-force expectation under L2 from [1,0]: id0=0, id1=2, id2=2,
    // id3=4 — the 2-tie breaks by id. The m=2 graph on 4 nodes is connected
    // (every insert links to at least one existing node, so the undirected
    // graph cannot split), and ef_search=8 > 4 keeps the beam open, making
    // the search exhaustive.
    let got = h.search(&[1.0, 0.0], 4, 8);
    assert_eq!(
        got,
        vec![(0usize, 0.0f32), (1, 2.0), (2, 2.0), (3, 4.0)],
        "exhaustive over the connected 4-node graph"
    );
    // Nearest-first ordering holds on every prefix.
    let got = h.search(&[1.0, 0.0], 2, 8);
    assert_eq!(got, vec![(0usize, 0.0f32), (1, 2.0)]);

    // k = 0: empty. ef_search below k is raised to k.
    assert!(h.search(&[1.0, 0.0], 0, 8).is_empty());
    assert_eq!(h.search(&[1.0, 0.0], 3, 1).len(), 3);

    // Hnsw::new default params, same corpus: same answers.
    let mut d = Hnsw::new(Metric::L2);
    for v in &data {
        d.insert(v.clone());
    }
    assert_eq!(d.search(&[1.0, 0.0], 4, 8), h.search(&[1.0, 0.0], 4, 8));

    // with_quant driven directly (Binary): all nodes returned, ascending.
    let mut b = Hnsw::with_quant(Metric::Cosine, Quantization::Binary, 2, 2);
    for v in &data {
        b.insert(v.clone());
    }
    let got = b.search(&[1.0, 0.0], 4, 8);
    assert_eq!(got.len(), 4);
    for w in got.windows(2) {
        assert!(w[0].1 <= w[1].1, "nearest first");
    }

    // Reproducibility: two identical builds answer identically.
    let build = || {
        let mut x = Hnsw::with_params(Metric::L2, 2, 2);
        for v in &data {
            x.insert(v.clone());
        }
        x.search(&[1.0, 0.0], 3, 8)
    };
    assert_eq!(build(), build());
}

// ===========================================================================
// 7. create_vector_index overload family (in-memory, on-disk, PQ)
// ===========================================================================

/// Every public creation overload: the plain and quantized in-memory ones,
/// the on-disk plain and quantized ones, and the PQ variant (which needs
/// training documents and errors with `Error::EmptyIndexTraining`
/// otherwise). On-disk searches over-fetch (`ef = max(4k, 64)` with a
/// complete small graph), so `k >= corpus` returns the exact order for the
/// on-disk kinds exactly as for in-memory ones.
#[test]
fn vector_create_index_overloads_inmemory_ondisk_and_pq() {
    // In-memory plain: covered extensively above; sanity here.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "ov-mem", &corpus_docs());
    c.create_vector_index("v", Metric::L2).unwrap();
    let hits = c.vector_search("v", &QUERY, 8, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "e", "d", "b", "c"]));
    assert!(hits.iter().all(|h| h.approximate));

    // On-disk plain (backfills existing docs, then serves).
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "ov-disk", &corpus_docs());
    c.create_vector_index_ondisk("v", Metric::L2).unwrap();
    let hits = c.vector_search("v", &QUERY, 8, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "e", "d", "b", "c"]));
    assert!(hits.iter().all(|h| h.approximate));
    assert_eq!(hits[0].distance, 0.0);

    // On-disk quantized (Binary): k >= corpus still exact after rerank.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "ov-diskq", &corpus_docs());
    c.create_vector_index_ondisk_quantized("v", Metric::Cosine, Quantization::Binary)
        .unwrap();
    let hits = c.vector_search("v", &QUERY, 8, Metric::Cosine).unwrap();
    assert_eq!(hit_keys(&hits), k(&["a", "d", "e", "b", "c"]));
    assert!(hits.iter().all(|h| h.approximate));

    // PQ: dim 8, m=4 subspaces (8 % 4 == 0), 16 centroids. Five bitwise
    // duplicates of the query vector [3,0,...] plus 8 axis variants at
    // magnitude 2. What the assertions below need is only: (i) the
    // candidate set contains every live node — the on-disk search
    // over-fetches with ef = max(4k, 64) = 64 > 13 over a complete small
    // graph (m0 = 32 links every node), and quantization perturbs
    // distances, not reachability — and (ii) nothing can beat the five
    // duplicates at exact L2 distance 0.0 (they are bitwise copies of the
    // query; every axis is at squared distance >= 1.0: axis0 (3-2)^2, the
    // rest 3^2+2^2). The exact rerank therefore returns exactly the five
    // duplicates with p0 first (0.0 ties break by key); no claim about
    // where PQ places the axes is needed.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("ov-pq");
    let axis = |i: usize| {
        let mut v = vec![0.0f32; 8];
        v[i] = 2.0;
        v
    };
    let mut query = vec![0.0f32; 8];
    query[0] = 3.0;
    for n in 0..5u8 {
        c.insert(format!("p{n}").as_bytes(), &doc("t", query.clone()))
            .unwrap();
    }
    for i in 0..8usize {
        c.insert(format!("a{i}").as_bytes(), &doc("t", axis(i)))
            .unwrap();
    }
    c.create_vector_index_ondisk_pq("v", Metric::L2, 4, 16)
        .unwrap();
    let hits = c.vector_search("v", &query, 5, Metric::L2).unwrap();
    assert_eq!(hits[0].key, b"p0".to_vec());
    assert_eq!(hits[0].distance, 0.0);
    assert!(hits[0].approximate);
    assert_eq!(hits.len(), 5);

    // PQ on an empty collection: the codebook cannot be trained — the exact
    // documented variant.
    let db = Db::open_in_memory().unwrap();
    let err = db
        .collection("ov-pq-empty")
        .create_vector_index_ondisk_pq("v", Metric::L2, 4, 16);
    assert!(matches!(err, Err(corvid::Error::EmptyIndexTraining)));
}

// ===========================================================================
// 8. Empty / one / missing-field corpora
// ===========================================================================

#[test]
fn vector_empty_collection_single_doc_and_missing_field() {
    // Empty collection, indexed and not: empty result, no error.
    for indexed in [false, true] {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("empty");
        if indexed {
            c.create_vector_index("v", Metric::Cosine).unwrap();
        }
        assert!(
            c.vector_search("v", &QUERY, 5, Metric::Cosine)
                .unwrap()
                .is_empty()
        );
        assert!(
            c.query()
                .vector("v", QUERY.to_vec(), 5, Metric::Cosine)
                .run()
                .unwrap()
                .is_empty()
        );
    }

    // Single doc: k=1 finds itself at distance 0; k beyond returns it once.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "one", &[(b"solo", vec![0.5, 0.5])]);
    let hits = c.vector_search("v", &QUERY, 1, Metric::Cosine).unwrap();
    assert_eq!(hit_keys(&hits), k(&["solo"]));
    assert_eq!(hits[0].distance, cosine_distance(&QUERY, &[0.5, 0.5]));
    assert_eq!(
        c.vector_search("v", &QUERY, 9, Metric::Cosine)
            .unwrap()
            .len(),
        1
    );

    // All docs missing the vector field (or carrying a non-vector value):
    // deterministically excluded on both the direct and builder paths.
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("nofield");
    let mut m = BTreeMap::new();
    m.insert("tag".to_owned(), Value::Text("no-vector".into()));
    c.insert(b"n1", &Value::Map(m.clone())).unwrap();
    m.insert("v".to_owned(), Value::Text("not a vector".into()));
    c.insert(b"n2", &Value::Map(m)).unwrap();
    assert!(
        c.vector_search("v", &QUERY, 5, Metric::L2)
            .unwrap()
            .is_empty()
    );
    assert!(
        c.query()
            .vector("v", QUERY.to_vec(), 5, Metric::L2)
            .run()
            .unwrap()
            .is_empty()
    );

    // Mixed: docs with and without the field — only the embedded ones rank.
    c.insert(b"y1", &doc("t", vec![1.0, 0.0])).unwrap();
    c.insert(b"y2", &doc("t", vec![0.0, 1.0])).unwrap();
    let hits = c.vector_search("v", &QUERY, 5, Metric::L2).unwrap();
    assert_eq!(hit_keys(&hits), k(&["y1", "y2"]));
    let rows = c
        .query()
        .vector("v", QUERY.to_vec(), 5, Metric::L2)
        .run()
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
        k(&["y1", "y2"])
    );
}

// ===========================================================================
// 9. Builder interplay: select / order_by / limit / offset
// ===========================================================================

/// `vector()` through the builder: rank order with RRF scores
/// `1/(60+rank)`; `select` narrows the document without touching key/score;
/// `order_by` REPLACES the rank order while keeping each row's fused score;
/// `limit`/`offset` window the ranked list. Pinned on the exact arm and on
/// the indexed (ANN) arm — both must agree.
#[test]
fn vector_builder_select_order_limit_offset_interplay() {
    // Distinct tags whose alphabetical order (t1..t5 -> e, d, a, c, b)
    // differs from the L2 rank order (a, e, d, b, c), so order_by and rank
    // order are distinguishable.
    let tags: [(&[u8], &str, [f32; 2]); 5] = [
        (b"a", "t3", [1.0, 0.0]),
        (b"b", "t5", [0.0, 1.0]),
        (b"c", "t4", [-1.0, 0.0]),
        (b"d", "t2", [2.0, 0.0]),
        (b"e", "t1", [1.0, 0.0]),
    ];
    let run = |indexed: bool| {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection(if indexed { "bp-idx" } else { "bp" });
        for (key, tag, v) in tags {
            c.insert(key, &doc(tag, v.to_vec())).unwrap();
        }
        if indexed {
            c.create_vector_index("v", Metric::L2).unwrap();
        }

        // Rank order (L2): a, e, d, b, c with fused scores 1/(60+rank).
        let rows = c
            .query()
            .select(["tag"])
            .vector("v", QUERY.to_vec(), 5, Metric::L2)
            .run()
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["a", "e", "d", "b", "c"])
        );
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.score, 1.0f32 / (61.0 + i as f32));
            // Projection narrowed the document to the selected field only.
            let want_tag = tags
                .iter()
                .find(|(key, _, _)| *key == r.key.as_slice())
                .unwrap()
                .1;
            assert_eq!(r.document.get("tag"), Some(&Value::Text(want_tag.into())));
            assert_eq!(r.document.get("v"), None);
        }

        // order_by replaces the rank order (tag order: e, d, a, c, b), but
        // every row keeps ITS fused score: rank-1 `a` still scores 1/61.
        let rows = c
            .query()
            .order_by("tag", false)
            .vector("v", QUERY.to_vec(), 5, Metric::L2)
            .run()
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["e", "d", "a", "c", "b"]),
            "order_by wins over rank order"
        );
        let score_of = |key: &[u8]| rows.iter().find(|r| r.key == key).map(|r| r.score).unwrap();
        assert_eq!(score_of(b"a"), 1.0f32 / 61.0); // rank 1
        assert_eq!(score_of(b"e"), 1.0f32 / 62.0); // rank 2
        assert_eq!(score_of(b"c"), 1.0f32 / 65.0); // rank 5

        // limit / offset window the RANKED list: limit(2) is ranks 1-2;
        // offset(1).limit(2) is ranks 2-3.
        let rows = c
            .query()
            .vector("v", QUERY.to_vec(), 5, Metric::L2)
            .limit(2)
            .run()
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["a", "e"])
        );
        let rows = c
            .query()
            .vector("v", QUERY.to_vec(), 5, Metric::L2)
            .offset(1)
            .limit(2)
            .run()
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["e", "d"])
        );
    };
    run(false);
    run(true);
}

// ===========================================================================
// 9. In-memory PQ (create_vector_index_pq) — the metric×storage matrix
// ===========================================================================

/// Deterministic clustered vectors (the `pq.rs` suite shape: centers +
/// noise), so PQ has structure to learn — uniform noise compresses poorly
/// for any quantizer.
fn pq_clustered(n: usize, dim: usize, clusters: usize) -> Vec<Vec<f32>> {
    let mut state: u64 = 0x1357_9BDF_2468_ACE0;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as f32 / u32::MAX as f32
    };
    let centers: Vec<Vec<f32>> = (0..clusters)
        .map(|_| (0..dim).map(|_| next() * 10.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            let c = &centers[i % clusters];
            c.iter().map(|&x| x + (next() - 0.5)).collect()
        })
        .collect()
}

/// PQ joins the in-memory quantization modes for EVERY metric: `dim = 2`
/// splits into `m = 2` subspaces; the same complete-graph + exact-rerank
/// argument as `vector_metric_quantization_cross_orders_and_exact_distances`
/// applies (a 5-node graph at `DEFAULT_M` is complete on layer 0, the search
/// over-fetches past the whole corpus, and quantization perturbs distances,
/// not reachability), so `k >= corpus` returns the exact order with
/// bitwise-exact hand-computed distances under each metric.
#[test]
fn vector_inmemory_pq_cross_metrics_orders_and_exact_distances() {
    for metric in all_metrics() {
        let db = Db::open_in_memory().unwrap();
        let name = format!("pqx_{metric:?}");
        let c = seed(&db, &name, &corpus_docs());
        c.create_vector_index_pq("v", metric, 2, 16).unwrap();

        let (want_keys, want_dists) = expected(metric);
        let hits = c.vector_search("v", &QUERY, 8, metric).unwrap();
        assert_eq!(
            hit_keys(&hits),
            k(&want_keys),
            "{metric:?}/PQ: k > corpus must return every doc in exact order"
        );
        assert_eq!(hits.len(), 5);
        for (h, want) in hits.iter().zip(want_dists) {
            assert_eq!(h.distance, want, "{metric:?}/PQ exact distance");
            assert!(h.approximate, "{metric:?}/PQ served by the index");
            assert_eq!(h.document.get("v"), Some(&corpus_vector(&h.key)));
        }
    }
}

/// Recall vs the exact scan twin on a fixed clustered corpus. The bound is
/// raised to what the public path actually delivers on this shape: measured
/// recall is 1.0 (300 docs in 10 tight clusters, `m = 8`: the in-memory
/// index's over-fetch plus the exact rerank from stored documents recovers
/// the complete top-k — the quantization loss is hidden by candidates the
/// walk gathers beyond k), and the 0.7 bound leaves room for corpus-shape
/// sensitivity while staying far above chance (10 clusters → ~0.1). Also
/// pins determinism (two identically built databases answer identically —
/// training, encoding and the graph's level RNG are all seeded) and reopen
/// (the trained codebook persists with the def; the post-reopen lazy
/// rebuild re-encodes under it and answers identically — the codebook-row
/// pin itself is in `index.rs`'s unit tests). The `ef`-insensitivity
/// premise of the direct-API path is pinned separately in `hnsw.rs`'s
/// `pq_recall_matches_exact_baseline` (this test goes through
/// `vector_search`, which does not expose `ef`).
#[test]
fn vector_inmemory_pq_recall_determinism_and_reopen() {
    let data = pq_clustered(300, 16, 10);
    let query = |c: &corvid::Collection<'_>, q: &[f32], k: usize| {
        hit_keys(&c.vector_search("v", q, k, Metric::L2).unwrap())
    };

    let build_db = |name: &str| -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection(name);
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc("t", v.clone()))
                .unwrap();
        }
        c.create_vector_index_pq("v", Metric::L2, 8, 64).unwrap();
        db
    };

    // Recall vs the exact twin (an unindexed database of the same docs).
    let exact = build_db("pq-exact");
    let indexed = build_db("pq-ix");
    let queries = pq_clustered(15, 16, 10);
    let k = 10;
    let mut total = 0.0;
    for q in &queries {
        let got: std::collections::HashSet<Vec<u8>> = query(&indexed.collection("pq-ix"), q, k)
            .into_iter()
            .collect();
        let want: std::collections::HashSet<Vec<u8>> = query(&exact.collection("pq-exact"), q, k)
            .into_iter()
            .collect();
        total += got.intersection(&want).count() as f64 / k as f64;
    }
    let recall = total / queries.len() as f64;
    assert!(recall >= 0.7, "in-memory PQ recall {recall} below 0.7");

    // Determinism: a second identical build answers identically.
    let twin = build_db("pq-twin");
    for q in &queries {
        assert_eq!(
            query(&indexed.collection("pq-ix"), q, k),
            query(&twin.collection("pq-twin"), q, k),
        );
    }

    // Reopen: identical answers after the post-reopen lazy rebuild.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pq.db");
    {
        let db = Db::open(&path).unwrap();
        let c = db.collection("pq-file");
        for (i, v) in data.iter().enumerate() {
            c.insert(format!("k{i}").as_bytes(), &doc("t", v.clone()))
                .unwrap();
        }
        c.create_vector_index_pq("v", Metric::L2, 8, 64).unwrap();
    }
    let before = {
        let db = Db::open(&path).unwrap();
        queries
            .iter()
            .map(|q| query(&db.collection("pq-file"), q, k))
            .collect::<Vec<_>>()
    };
    let after = {
        let db = Db::open(&path).unwrap();
        queries
            .iter()
            .map(|q| query(&db.collection("pq-file"), q, k))
            .collect::<Vec<_>>()
    };
    assert_eq!(before, after, "reopen must rebuild under the same codebook");
}

/// PQ creation needs something to train on, and `m` must divide the field's
/// dimension — both surface as the documented `EmptyIndexTraining`.
#[test]
fn vector_inmemory_pq_creation_requires_training_documents() {
    let db = Db::open_in_memory().unwrap();
    let err = db
        .collection("pq-none")
        .create_vector_index_pq("v", Metric::L2, 4, 16);
    assert!(matches!(err, Err(corvid::Error::EmptyIndexTraining)));

    // dim 2 is not divisible by m = 3.
    let c = seed(&db, "pq-badm", &corpus_docs());
    let err = c.create_vector_index_pq("v", Metric::L2, 3, 16);
    assert!(matches!(err, Err(corvid::Error::EmptyIndexTraining)));
}

/// The direct `Hnsw::with_pq` API pins the metric×storage plumbing bitwise:
/// under L2 every reported distance IS the ADC table lookup sum for the
/// stored code (`Pq::adc_l2` over `Pq::l2_table`), and under cosine it IS
/// the reconstruct-then-distance value (`Pq::distance`) — the same contract
/// the on-disk PQ index exposes. Builds are reproducible.
#[test]
fn vector_hnsw_direct_pq_adc_and_reconstruction_paths() {
    use corvid::pq::Pq;
    use std::sync::Arc;

    let data = pq_clustered(120, 8, 5);
    let pq = Arc::new(Pq::train(&data, 4, 16).unwrap());

    // L2: the ADC fast path.
    let mut l2 = Hnsw::with_pq(Metric::L2, pq.clone(), 8, 64);
    for v in &data {
        l2.insert(v.clone());
    }
    let q = &data[10];
    let table = pq.l2_table(q).unwrap();
    for (id, dist) in l2.search(q, 5, 50) {
        let want = pq.adc_l2(&table, &pq.encode(&data[id]));
        assert_eq!(dist, want, "L2 must score through the ADC table");
    }

    // Cosine: the reconstruction path.
    let mut cos = Hnsw::with_pq(Metric::Cosine, pq.clone(), 8, 64);
    for v in &data {
        cos.insert(v.clone());
    }
    for (id, dist) in cos.search(q, 5, 50) {
        let want = pq.distance(Metric::Cosine, q, &pq.encode(&data[id]));
        assert_eq!(dist, want, "cosine must score by reconstruction");
    }

    // Dot serves too (same reconstruction path).
    let mut dot = Hnsw::with_pq(Metric::Dot, pq.clone(), 8, 64);
    for v in &data {
        dot.insert(v.clone());
    }
    for (id, dist) in dot.search(q, 5, 50) {
        let want = pq.distance(Metric::Dot, q, &pq.encode(&data[id]));
        assert_eq!(dist, want, "dot must score by reconstruction");
    }

    // Reproducibility: identical builds answer identically.
    let build = || {
        let pq = Arc::new(Pq::train(&data, 4, 16).unwrap());
        let mut h = Hnsw::with_pq(Metric::L2, pq, 8, 64);
        for v in &data {
            h.insert(v.clone());
        }
        h.search(q, 5, 50)
    };
    assert_eq!(build(), build());
}
