//! Search-tool conformance: `search` (filter ops, vector, text, fusion,
//! select, limits), `phrase_search`, `geo`, and `create_index` (the vector
//! index tool, incl. on-disk and PQ variants) — result shapes asserted
//! against a known corpus, plus the full param-parser error surface.

use serde_json::{Value as Json, json};

use crate::wire::{self, Wire};

/// Collect the result keys of a `search` payload, sorted for set comparison.
fn keys(payload: &Json) -> Vec<String> {
    let mut out: Vec<String> = payload["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

/// Vector search ranks a known corpus by cosine similarity: exact match
/// first with score 1.0, then near, then orthogonal — and the embedded
/// document comes back in the `$vector` convention.
#[test]
fn search_vector_orders_by_similarity() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "a",
        json!({"cat": "blog", "embedding": {"$vector": [1.0, 0.0]}}),
    );
    w.store(
        "docs",
        "b",
        json!({"cat": "news", "embedding": {"$vector": [0.9, 0.1]}}),
    );
    w.store(
        "docs",
        "c",
        json!({"cat": "wiki", "embedding": {"$vector": [0.0, 1.0]}}),
    );
    let out = w.ok(
        "search",
        json!({
            "collection": "docs",
            "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 3, "metric": "cosine"},
        }),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["key"], "a");
    assert_eq!(results[1]["key"], "b");
    assert_eq!(results[2]["key"], "c");
    // Pinned: `search` always fuses through RRF, so the reported score is
    // the rank score 1/(rrf_k + rank) (default rrf_k = 60) — not the raw
    // cosine similarity. Ranks follow the vector order, so scores descend.
    assert_eq!(
        results[0]["score"].as_f64().unwrap(),
        f64::from(1.0_f32 / 61.0_f32)
    );
    let scores: Vec<f64> = results
        .iter()
        .map(|r| r["score"].as_f64().unwrap())
        .collect();
    assert!(
        scores.windows(2).all(|p| p[0] > p[1]),
        "scores are strictly decreasing: {scores:?}"
    );
    assert_eq!(
        results[0]["document"],
        json!({"cat": "blog", "embedding": {"$vector": [1.0, 0.0]}})
    );
}

/// Text search ranks by BM25: the document matching more query terms
/// outranks the one matching fewer.
#[test]
fn search_text_ranks_by_term_matches() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"body": "rust database performance"}));
    w.store("docs", "b", json!({"body": "rust only here"}));
    w.store("docs", "c", json!({"body": "python web framework"}));
    let out = w.ok(
        "search",
        json!({
            "collection": "docs",
            "text": {"field": "body", "query": "rust database", "k": 3},
        }),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "only the rust documents match");
    assert_eq!(results[0]["key"], "a", "two matching terms outrank one");
    assert_eq!(results[1]["key"], "b");
    assert!(results[0]["score"].as_f64().unwrap() > 0.0);
}

/// Hybrid parameters flow through the wire: vector + text fused with a
/// custom RRF constant and MMR rerank still returns ranked, deduplicated
/// rows.
#[test]
fn search_fusion_mmr_and_select_params() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "a",
        json!({"body": "rust", "embedding": {"$vector": [1.0, 0.0]}, "cat": "x"}),
    );
    w.store(
        "docs",
        "b",
        json!({"body": "rust", "embedding": {"$vector": [0.0, 1.0]}, "cat": "y"}),
    );
    let out = w.ok(
        "search",
        json!({
            "collection": "docs",
            "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 2},
            "text": {"field": "body", "query": "rust", "k": 2},
            "rrf_k": 30.0,
            "mmr": {"lambda": 0.5},
            "select": ["cat"],
        }),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for r in results {
        assert_eq!(
            r["document"],
            json!({"cat": if r["key"] == "a" { "x" } else { "y" }}),
            "select projects the requested field only"
        );
    }
}

/// Every filter op the wire accepts returns the exact expected key set:
/// comparisons, exists, in, between, starts_with, contains, geo_within, and
/// the and/or/not combinators.
#[test]
fn search_filter_op_matrix() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"cat": "blog", "score": 9}));
    w.store("docs", "b", json!({"cat": "blog", "score": 2}));
    w.store("docs", "c", json!({"cat": "news", "score": 9}));
    w.store("docs", "d", json!({"loc": [51.5, -0.13]}));
    let run = |w: &mut Wire, filter: Json| -> Vec<String> {
        keys(&w.ok("search", json!({"collection": "docs", "filter": filter})))
    };
    assert_eq!(
        run(&mut w, json!({"op": "eq", "field": "cat", "value": "blog"})),
        ["a", "b"]
    );
    // ne on a MISSING field is false (predicates never match absent paths),
    // so only c matches — the engine's missing-field semantics through the wire.
    assert_eq!(
        run(&mut w, json!({"op": "ne", "field": "cat", "value": "blog"})),
        ["c"]
    );
    assert_eq!(
        run(&mut w, json!({"op": "gt", "field": "score", "value": 5})),
        ["a", "c"]
    );
    assert_eq!(
        run(&mut w, json!({"op": "ge", "field": "score", "value": 9})),
        ["a", "c"]
    );
    assert_eq!(
        run(&mut w, json!({"op": "lt", "field": "score", "value": 9})),
        ["b"]
    );
    assert_eq!(
        run(&mut w, json!({"op": "le", "field": "score", "value": 2})),
        ["b"]
    );
    assert_eq!(
        run(&mut w, json!({"op": "exists", "field": "cat"})),
        ["a", "b", "c"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "in", "field": "cat", "values": ["blog", "news"]}),
        ),
        ["a", "b", "c"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "between", "field": "score", "low": 2, "high": 9}),
        ),
        ["a", "b", "c"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "starts_with", "field": "cat", "value": "bl"})
        ),
        ["a", "b"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "contains", "field": "cat", "value": "og"})
        ),
        ["a", "b"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "not", "clause": {"op": "eq", "field": "cat", "value": "blog"}}),
        ),
        ["c", "d"],
        "not(eq) DOES match a missing field (eq is false there)"
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "and", "clauses": [
                {"op": "eq", "field": "cat", "value": "blog"},
                {"op": "gt", "field": "score", "value": 5},
            ]}),
        ),
        ["a"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "or", "clauses": [
                {"op": "eq", "field": "cat", "value": "news"},
                {"op": "exists", "field": "loc"},
            ]}),
        ),
        ["c", "d"]
    );
    assert_eq!(
        run(
            &mut w,
            json!({"op": "geo_within", "field": "loc", "lat": 51.5, "lon": -0.13, "radius_km": 10.0}),
        ),
        ["d"]
    );
}

/// Every malformed filter shape surfaces as BadParams with its exact
/// message: the full parse_predicate error matrix.
#[test]
fn search_filter_shape_errors() {
    let mut w = Wire::new();
    let cases: [(Json, &str); 10] = [
        (
            json!({"collection": "docs", "filter": 42}),
            "bad params: filter must be an object",
        ),
        (
            json!({"collection": "docs", "filter": {"field": "x"}}),
            "bad params: filter missing 'op'",
        ),
        (
            json!({"collection": "docs", "filter": {"op": 7, "field": "x"}}),
            "bad params: filter missing 'op'",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "eq", "value": 1}}),
            "bad params: filter missing string 'field'",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "eq", "field": "x"}}),
            "bad params: comparison needs 'value'",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "and", "clauses": []}}),
            "bad params: 'and'/'or' need at least one clause",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "and"}}),
            "bad params: 'and'/'or' need a 'clauses' array",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "not"}}),
            "bad params: 'not' needs a 'clause'",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "in", "field": "x"}}),
            "bad params: 'in' needs a 'values' array",
        ),
        (
            json!({"collection": "docs", "filter": {"op": "between", "field": "x", "low": 1}}),
            "bad params: 'between' needs 'high'",
        ),
    ];
    for (args, msg) in cases {
        assert_eq!(w.err("search", args), msg, "filter error surface");
    }
    // starts_with with a non-string value, and geo_within missing lat, both
    // name their field.
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "filter": {"op": "starts_with", "field": "x", "value": 3}}),
        ),
        "bad params: text match needs a string 'value'"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "filter": {"op": "geo_within", "field": "loc", "lon": 0.0, "radius_km": 1.0}}),
        ),
        "bad params: missing number 'lat'"
    );
}

/// Bad vector/text/mmr/rrf/select param shapes: exact messages.
#[test]
fn search_modality_param_errors() {
    let mut w = Wire::new();
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": 3, "k": 1}}),
        ),
        "bad params: 'query' must be an array of numbers"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0, "x"], "k": 1}}),
        ),
        "bad params: 'query' entries must be numbers"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0], "k": 1, "metric": "manhattan"}}),
        ),
        "bad params: 'metric' must be one of: cosine, dot, l2"
    );
    // `quant` inside the vector object is validated like create_index's:
    // invalid strings error; the valid names are accepted.
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0], "k": 1, "quant": "wat"}}),
        ),
        "bad params: 'quant' must be one of: none, binary, scalar"
    );
    assert_eq!(
        w.ok(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0], "k": 1, "quant": "scalar"}}),
        )["results"],
        json!([])
    );
    assert_eq!(
        w.err("search", json!({"collection": "docs", "mmr": {}})),
        "bad params: mmr needs numeric 'lambda'"
    );
    assert_eq!(
        w.err("search", json!({"collection": "docs", "rrf_k": "big"})),
        "bad params: 'rrf_k' must be a number"
    );
    assert_eq!(
        w.err("search", json!({"collection": "docs", "select": "cat"})),
        "bad params: 'select' must be an array"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "select": ["cat", 7]})
        ),
        "bad params: 'select' entries must be strings"
    );
}

/// Limit validation: wrong types and over-max counts are rejected with
/// exact messages, for the call-level `limit` and the vector/text `k`s.
#[test]
fn search_limit_validation_matrix() {
    let mut w = Wire::new();
    assert_eq!(
        w.err("search", json!({"collection": "docs", "limit": "ten"})),
        "bad params: 'limit' must be a non-negative integer"
    );
    assert_eq!(
        w.err("search", json!({"collection": "docs", "limit": -1})),
        "bad params: 'limit' must be a non-negative integer"
    );
    assert_eq!(
        w.err("search", json!({"collection": "docs", "limit": 10001})),
        "bad params: 'limit' exceeds the maximum of 10000"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0], "k": 10001}}),
        ),
        "bad params: 'k' exceeds the maximum of 10000"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "text": {"field": "b", "query": "x", "k": 10001}}),
        ),
        "bad params: 'k' exceeds the maximum of 10000"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "vector": {"field": "e", "query": [1.0]}}),
        ),
        "bad params: missing non-negative integer 'k'"
    );
    assert_eq!(
        w.err(
            "search",
            json!({"collection": "docs", "text": {"field": "b", "k": 1}}),
        ),
        "bad params: missing string 'query'"
    );
}

/// Out-of-domain ranking constants pass MCP parsing but the ENGINE rejects
/// them at execution: InvalidArgument surfaced as isError with the engine's
/// message (the Engine taxonomy through the wire).
#[test]
fn search_engine_invalid_argument_mmr_and_rrf() {
    let mut w = Wire::new();
    w.store(
        "docs",
        "a",
        json!({"body": "rust", "embedding": {"$vector": [1.0, 0.0]}}),
    );
    wire::starts_with(
        &w.err(
            "search",
            json!({
                "collection": "docs",
                "vector": {"field": "embedding", "query": [1.0], "k": 1},
                "mmr": {"lambda": 2.0},
            }),
        ),
        "invalid argument: rerank_mmr: lambda must be in [0, 1], got 2",
    );
    wire::starts_with(
        &w.err(
            "search",
            json!({
                "collection": "docs",
                "text": {"field": "body", "query": "rust", "k": 1},
                "rrf_k": 0.0,
            }),
        ),
        "invalid argument: fuse_rrf: k must be > 0, got 0",
    );
}

/// phrase_search is order-sensitive: "quick brown" matches, "brown quick"
/// does not; the result shape is {key, score, document} and k bounds the
/// results. k=0 returns empty; k is required.
#[test]
fn phrase_search_ordered_tokens_and_k_bounds() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"body": "the quick brown fox jumps"}));
    w.store("docs", "b", json!({"body": "brown quick swap"}));
    let out = w.ok(
        "phrase_search",
        json!({"collection": "docs", "field": "body", "phrase": "quick brown", "k": 10}),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "order matters within a phrase");
    assert_eq!(results[0]["key"], "a");
    assert!(results[0]["score"].as_f64().unwrap() > 0.0);
    assert_eq!(
        results[0]["document"],
        json!({"body": "the quick brown fox jumps"})
    );

    let out = w.ok(
        "phrase_search",
        json!({"collection": "docs", "field": "body", "phrase": "brown quick", "k": 10}),
    );
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        1,
        "b has that order"
    );

    // k bounds the result list; k=0 answers empty without error.
    let out = w.ok(
        "phrase_search",
        json!({"collection": "docs", "field": "body", "phrase": "brown", "k": 1}),
    );
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
    let out = w.ok(
        "phrase_search",
        json!({"collection": "docs", "field": "body", "phrase": "brown", "k": 0}),
    );
    assert_eq!(out["results"], json!([]));

    assert_eq!(
        w.err(
            "phrase_search",
            json!({"collection": "docs", "field": "body", "phrase": "x"}),
        ),
        "bad params: missing non-negative integer 'k'"
    );
    assert_eq!(
        w.err(
            "phrase_search",
            json!({"collection": "docs", "field": "body", "phrase": "x", "k": "3"}),
        ),
        "bad params: missing non-negative integer 'k'"
    );
    assert_eq!(
        w.err(
            "phrase_search",
            json!({"collection": "docs", "field": "body", "phrase": "x", "k": -2}),
        ),
        "bad params: missing non-negative integer 'k'"
    );
    wire::starts_with(
        &w.err(
            "phrase_search",
            json!({"collection": "docs", "field": "body"}),
        ),
        "bad params: missing string 'phrase'",
    );
}

/// geo: within-radius and nearest-k both work through the wire, k takes
/// precedence over radius_km, and `limit` truncates the result list.
#[test]
fn geo_radius_nearest_and_limit() {
    let mut w = Wire::new();
    w.store("docs", "london", json!({"loc": [51.5074, -0.1278]}));
    w.store("docs", "paris", json!({"loc": [48.8566, 2.3522]}));
    w.store("docs", "brighton", json!({"loc": [50.8225, -0.1372]}));

    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13, "radius_km": 90.0}),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "London and Brighton, not Paris");
    assert_eq!(results[0]["key"], "london", "nearest first");
    assert_eq!(results[1]["key"], "brighton");
    assert!(results[0]["distance_km"].as_f64().unwrap() < 1.0);
    assert_eq!(results[0]["document"], json!({"loc": [51.5074, -0.1278]}));

    // Nearest-2 by k; k also takes precedence when radius is present.
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13, "k": 2}),
    );
    assert_eq!(keys(&out), ["brighton", "london"]);
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "k": 1, "radius_km": 1.0}),
    );
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        1,
        "k wins over radius_km"
    );

    // The list-level `limit` truncates after the engine returns its hits.
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "radius_km": 4000.0}),
    );
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        3,
        "all three hit unbounded"
    );
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "radius_km": 4000.0, "limit": 2}),
    );
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        2,
        "limit truncates"
    );
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "radius_km": 4000.0, "limit": 1}),
    );
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
}

/// geo param errors: lat/lon/radius_km are required numbers (a non-numeric
/// or missing one is BadParams naming the field); radius is required when k
/// is absent; and a `k` that is not a non-negative integer (string,
/// negative, float) is a BadParams error, not a silent radius fallback.
#[test]
fn geo_param_errors() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"loc": [51.5, -0.13]}));
    assert_eq!(
        w.err(
            "geo",
            json!({"collection": "docs", "field": "loc", "lon": 0.0, "radius_km": 5.0}),
        ),
        "bad params: missing number 'lat'"
    );
    assert_eq!(
        w.err(
            "geo",
            json!({"collection": "docs", "field": "loc", "lat": "51.5", "lon": 0.0, "radius_km": 5.0}),
        ),
        "bad params: missing number 'lat'"
    );
    assert_eq!(
        w.err(
            "geo",
            json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": 0.0}),
        ),
        "bad params: missing number 'radius_km'"
    );
    wire::starts_with(
        &w.err(
            "geo",
            json!({"collection": "docs", "lat": 51.5, "lon": 0.0}),
        ),
        "bad params: missing string 'field'",
    );
    // A non-u64 k errors instead of silently taking the radius path.
    for bad in [json!(-1), json!("2"), json!(1.5)] {
        assert_eq!(
            w.err(
                "geo",
                json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
                       "k": bad, "radius_km": 5.0}),
            ),
            "bad params: 'k' must be a non-negative integer",
            "geo k {bad}"
        );
    }
    // k = 0 is a valid u64: nearest-0 returns no hits (not the radius path).
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "k": 0, "radius_km": 5.0}),
    );
    assert_eq!(out["results"], json!([]));
}

/// create_index variants: the default in-memory build, explicit metric and
/// quant, on-disk, and product-quantized (trainable only with existing
/// vectors) — each reports ok, and search still ranks correctly afterwards.
#[test]
fn create_index_variants_then_search() {
    let mut w = Wire::new();
    for (key, v) in [("a", [1.0, 0.0]), ("b", [0.0, 1.0])] {
        w.store("docs", key, json!({"embedding": {"$vector": v}}));
    }
    assert_eq!(
        w.ok(
            "create_index",
            json!({"collection": "docs", "field": "embedding"})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "metric": "dot", "quant": "scalar"}),
        ),
        json!({"ok": true})
    );
    assert_eq!(
        w.ok(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "on_disk": true}),
        ),
        json!({"ok": true})
    );
    // PQ trains on the stored vectors (dim 2, m=1, k=2).
    assert_eq!(
        w.ok(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "pq": {"m": 1, "k": 2}}),
        ),
        json!({"ok": true})
    );
    let out = w.ok(
        "search",
        json!({
            "collection": "docs",
            "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 1, "metric": "dot"},
        }),
    );
    assert_eq!(out["results"][0]["key"], "a");
}

/// create_index error paths: BadParams for missing/mistyped params and
/// unparseable enum strings; Engine EmptyIndexTraining when PQ cannot train
/// (m not dividing the dimension).
#[test]
fn create_index_param_and_training_errors() {
    let mut w = Wire::new();
    w.store("docs", "a", json!({"embedding": {"$vector": [1.0, 0.0]}}));
    wire::starts_with(
        &w.err("create_index", json!({"collection": "docs"})),
        "bad params: missing string 'field'",
    );
    assert_eq!(
        w.err(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "metric": "euclid"}),
        ),
        "bad params: 'metric' must be one of: cosine, dot, l2"
    );
    assert_eq!(
        w.err(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "quant": "int8"}),
        ),
        "bad params: 'quant' must be one of: none, binary, scalar"
    );
    assert_eq!(
        w.err(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "pq": {"k": 2}}),
        ),
        "bad params: missing non-negative integer 'm'"
    );
    // dim 2 is not divisible by m=3: the codebook cannot train.
    wire::starts_with(
        &w.err(
            "create_index",
            json!({"collection": "docs", "field": "embedding", "pq": {"m": 3, "k": 2}}),
        ),
        "cannot train a PQ codebook",
    );
    // PQ with no vectors at all is the same engine error.
    wire::starts_with(
        &w.err(
            "create_index",
            json!({"collection": "empty", "field": "embedding", "pq": {"m": 1, "k": 2}}),
        ),
        "cannot train a PQ codebook",
    );
}
