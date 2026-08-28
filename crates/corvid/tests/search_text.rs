//! Text-search conformance (Task 9): tokenization and the S-stemmer, the
//! analyzer variants, `Bm25Params` validation, the `idf`/`term_score`
//! primitives, BM25 ranking on fixed corpora (order + score asserted),
//! index-vs-scan equivalence (in-memory and on-disk), `text()` through the
//! builder, and `phrase_search` order-sensitivity — driven through the public
//! API only.
//!
//! Contracts pinned by these tests (read from `src/text.rs`, `src/fts.rs`,
//! `src/disk_fts.rs`, `src/query.rs`, and `src/builder.rs` first):
//!
//! * `tokenize` splits on non-alphanumeric runs and lowercases; `s_stem` is
//!   Harman's S-stemmer (plural normalization only — no `-ing` stripping),
//!   ASCII-only, leaving `...us`/`...ss`/len<=3 words alone. `analyze` =
//!   tokenize + stop-word removal + stemming; `Analyzer::raw` keeps
//!   everything; the pub fields toggle the two stages independently.
//! * BM25: score(doc) = Σ over distinct query terms of
//!   `term_score(tf, doc_len, avg_len, idf(n, df), defaults)` with
//!   whole-corpus stats over the documents that HAVE the text field — the
//!   exact public-primitive composition, so scores are asserted bitwise
//!   against a helper that calls `corvid::text::{idf, term_score}`, plus
//!   independent hand-computed constants at `1e-6` (f32 ln rounding).
//!   Ties break by key order. Creating an in-memory or on-disk text index
//!   never changes keys OR scores (audit B7).
//! * Phrase search analyzes the phrase too; positions are assigned AFTER
//!   stop-word removal, so removed stop words collapse out of adjacency;
//!   matched docs are scored by the BM25 sum over the phrase terms
//!   (duplicates score per occurrence). A single-term phrase is bitwise the
//!   same as the term search. k=0 / empty / stop-word-only / punctuation-only
//!   phrases are empty results, never errors.
//! * `text()` through the builder exposes the RRF fused score `1/(60+rank)`
//!   per single text source — the BM25 score itself is NOT exposed on
//!   `ResultRow` — and computes BM25 over the candidate set; the corpora
//!   below are chosen so every doc has the field (candidate stats == corpus
//!   stats), keeping the scan and index arms in exact agreement.
//!
//! The Wave-1 smoke test that anchored the radar skeleton is kept at the top.

use std::collections::BTreeMap;

use corvid::text::{Analyzer, Bm25Params, idf, s_stem, term_score, tokenize};
use corvid::{Db, TextHit, Value};

fn doc(body: &str) -> Value {
    let mut m = BTreeMap::new();
    m.insert("body".to_owned(), Value::Text(body.to_owned()));
    Value::Map(m)
}

fn seed<'a>(db: &'a Db, name: &'a str, docs: &[(&[u8], Value)]) -> corvid::Collection<'a> {
    let c = db.collection(name);
    for (k, v) in docs {
        c.insert(k, v).unwrap();
    }
    c
}

/// A fresh in-memory Db with `docs` inserted into `name` (for tests that
/// create the Db inside a helper and therefore cannot return a Collection
/// borrowing it — take `db.collection(name)` at the call site instead).
fn seeded_db(name: &str, docs: &[(&[u8], Value)]) -> Db {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection(name);
    for (k, v) in docs {
        c.insert(k, v).unwrap();
    }
    db
}

/// The BM25 contribution of one term with whole-corpus stats — the exact
/// public-primitive composition `text_search`/`phrase_search`/`ranked_bm25`
/// all use, so scores can be asserted bitwise against it.
fn bm25_term(tf: u32, doc_len: usize, avg_len: f32, n: usize, df: usize) -> f32 {
    term_score(tf, doc_len, avg_len, idf(n, df), Bm25Params::default())
}

fn hit_keys(hits: &[TextHit]) -> Vec<Vec<u8>> {
    hits.iter().map(|h| h.key.clone()).collect()
}

fn k(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|n| n.as_bytes().to_vec()).collect()
}

// ===========================================================================
// Smoke (radar anchor, kept from the Wave 1 skeleton)
// ===========================================================================

#[test]
fn search_text_smoke_ranks_most_relevant_first() {
    let db = Db::open_in_memory().unwrap();
    let c = db.collection("docs");
    c.insert(b"rust-db", &doc("rust embedded database design"))
        .unwrap();
    c.insert(b"py-web", &doc("python web frameworks")).unwrap();
    c.insert(b"rust-async", &doc("async rust patterns"))
        .unwrap();

    let hits = c.text_search("body", "rust database", 2).unwrap();
    assert_eq!(hits.len(), 2); // only docs containing at least one term
    assert_eq!(hits[0].key, b"rust-db".to_vec()); // matches both terms
    assert_eq!(hits[1].key, b"rust-async".to_vec()); // matches one
    assert!(hits[0].score > hits[1].score);
    assert!(hits.iter().all(|h| h.score > 0.0));
    assert_eq!(
        hits[0].document.get("body"),
        Some(&Value::Text("rust embedded database design".into()))
    );
}

// ===========================================================================
// 1. tokenize / s_stem / Analyzer / analyze — the public text primitives
// ===========================================================================

#[test]
fn text_tokenize_case_punct_unicode_numbers_and_empties() {
    // Case folding.
    assert_eq!(
        tokenize("The QUICK brown Fox"),
        vec!["the", "quick", "brown", "fox"]
    );
    // Punctuation (any non-alphanumeric run) is a separator.
    assert_eq!(
        tokenize("hello, world! (again)"),
        vec!["hello", "world", "again"]
    );
    assert_eq!(
        tokenize("dot.separated--words"),
        vec!["dot", "separated", "words"]
    );
    // Unicode words are kept, incl. uppercase-to-lowercase folding.
    assert_eq!(tokenize("café NAÏVE"), vec!["café", "naïve"]);
    assert_eq!(tokenize("日本語 テキスト"), vec!["日本語", "テキスト"]);
    // Empty and separator-only inputs.
    assert!(tokenize("").is_empty());
    assert!(tokenize("   \t\n").is_empty());
    assert!(tokenize(",.-!! ??").is_empty());
    // Numbers are alphanumeric: "3.14" splits at the dot, "v2" stays whole.
    assert_eq!(tokenize("42 3.14 v2"), vec!["42", "3", "14", "v2"]);
    // Mixed alnum is ONE token.
    assert_eq!(tokenize("abc123def"), vec!["abc123def"]);
}

/// The S-stemmer's actual behavior, pinned on a table: conservative plural
/// normalization only. It does NOT strip `-ing` ("running" is untouched), it
/// guards len<=3 ("was" survives because it is 3 bytes, not because it is
/// special), and it leaves `...us`/`...ss` alone. Accepted imperfections are
/// pinned too: "always" over-stems to "alway" and "goes" falls through the
/// `...oes` guard to the plain-s rule ("goe"), so "goes" never matches "go".
#[test]
fn text_s_stem_pins_conservative_plural_algorithm() {
    // Plurals that stem.
    assert_eq!(s_stem("cats"), "cat");
    assert_eq!(s_stem("parties"), "party");
    assert_eq!(s_stem("houses"), "house"); // ...es drops only the 's'
    assert_eq!(s_stem("boxes"), "boxe");
    // NOT a plural stemmer: -ing is never touched.
    assert_eq!(s_stem("running"), "running");
    // Non-plural / guarded words.
    assert_eq!(s_stem("was"), "was"); // len 3 guard
    assert_eq!(s_stem("is"), "is"); // len 2 guard
    assert_eq!(s_stem("press"), "press"); // ...ss
    assert_eq!(s_stem("class"), "class"); // ...ss
    assert_eq!(s_stem("bus"), "bus"); // ...us
    assert_eq!(s_stem("genus"), "genus"); // ...us
    assert_eq!(s_stem(""), "");
    // Pinned imperfections of the conservative algorithm.
    assert_eq!(s_stem("always"), "alway"); // plain-s rule over-stems
    assert_eq!(s_stem("goes"), "goe"); // ...oes skips the es rule, hits the s rule
    // Non-ASCII passes through untouched.
    assert_eq!(s_stem("cafés"), "cafés");
}

#[test]
fn text_analyzer_default_raw_and_flag_combinations() {
    // Default: lowercase + stop-word removal + S-stemming.
    assert_eq!(
        corvid::text::analyze("The quick brown foxes"),
        vec!["quick", "brown", "foxe"]
    );
    // All stop words analyze to nothing.
    assert!(corvid::text::analyze("the and of to").is_empty());
    assert!(corvid::text::analyze("").is_empty());

    // Raw: tokenize only — stop words kept, no stemming.
    assert_eq!(Analyzer::raw().analyze("The cats"), vec!["the", "cats"]);
    assert_eq!(
        Analyzer::raw().analyze("parties of boxes"),
        vec!["parties", "of", "boxes"]
    );

    // The pub fields toggle the two stages independently.
    let no_stem = Analyzer {
        remove_stop_words: true,
        stem: false,
    };
    assert_eq!(no_stem.analyze("The cats"), vec!["cats"]);
    let no_stop = Analyzer {
        remove_stop_words: false,
        stem: true,
    };
    assert_eq!(no_stop.analyze("The cats"), vec!["the", "cat"]);

    // Default analyzer fields pinned.
    let d = Analyzer::default();
    assert!(d.remove_stop_words && d.stem);
    assert_eq!(
        Analyzer::default().analyze("Dogs"),
        corvid::text::analyze("dogs")
    );
}

// ===========================================================================
// 2. Bm25Params validation (audit C6) — exact Error variant per text.rs
// ===========================================================================

#[test]
fn text_bm25_params_new_and_validate_error_variants() {
    // Defaults pinned.
    let d = Bm25Params::default();
    assert_eq!(d.k1, 1.2);
    assert_eq!(d.b, 0.75);
    assert!(d.validate().is_ok());

    // Valid constructions, including the closed-interval boundaries.
    for (k1, b) in [(1.2f32, 0.75f32), (0.0, 0.0), (2.0, 1.0), (0.0, 1.0)] {
        let p = Bm25Params::new(k1, b).expect("boundary values are valid");
        assert_eq!((p.k1, p.b), (k1, b));
        assert!(p.validate().is_ok());
    }

    // Invalid k1: negative, NaN, +inf, -inf — all Error::InvalidArgument
    // naming k1's domain rule.
    for bad in [-0.1f32, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        match Bm25Params::new(bad, 0.75) {
            Ok(p) => panic!("k1={bad} must be rejected, got {p:?}"),
            Err(corvid::Error::InvalidArgument(msg)) => {
                assert!(
                    msg.contains("k1 must be >= 0"),
                    "k1={bad}: message names the parameter's rule: {msg}"
                );
            }
            Err(e) => panic!("k1={bad}: wrong variant {e:?}"),
        }
    }
    // Invalid b: below 0, above 1, NaN, inf.
    for bad in [-0.1f32, 1.5f32, f32::NAN, f32::INFINITY] {
        match Bm25Params::new(1.2, bad) {
            Ok(p) => panic!("b={bad} must be rejected, got {p:?}"),
            Err(corvid::Error::InvalidArgument(msg)) => {
                assert!(
                    msg.contains("b must be in [0, 1]"),
                    "b={bad}: message names the parameter's rule: {msg}"
                );
            }
            Err(e) => panic!("b={bad}: wrong variant {e:?}"),
        }
    }

    // validate() re-checks a struct built through the pub fields.
    assert!(matches!(
        Bm25Params { k1: -1.0, b: 0.5 }.validate(),
        Err(corvid::Error::InvalidArgument(_))
    ));
    assert!(matches!(
        Bm25Params { k1: 1.2, b: 2.0 }.validate(),
        Err(corvid::Error::InvalidArgument(_))
    ));
    assert!(matches!(
        Bm25Params {
            k1: f32::NAN,
            b: f32::NAN
        }
        .validate(),
        Err(corvid::Error::InvalidArgument(_))
    ));
    assert!(Bm25Params { k1: 0.0, b: 1.0 }.validate().is_ok());
}

// ===========================================================================
// 3. idf / term_score — the public scoring primitives
// ===========================================================================

#[test]
fn text_idf_values_monotonicity_and_nonnegativity() {
    // Hand-computed: idf(n, df) = ln(1 + (n - df + 0.5)/(df + 0.5)).
    // idf(4,1) = ln(1 + 3.5/1.5) = ln(10/3)
    assert!((idf(4, 1) - 1.2039728_f32).abs() < 1e-6);
    // idf(4,3) = ln(1 + 1.5/3.5) = ln(10/7)
    assert!((idf(4, 3) - 0.35667494_f32).abs() < 1e-6);
    // idf(4,4) = ln(1 + 0.5/4.5) = ln(10/9)
    assert!((idf(4, 4) - 0.105360516_f32).abs() < 1e-6);
    // idf(3,3) = ln(1 + 0.5/3.5) = ln(8/7)
    assert!((idf(3, 3) - 0.13353139_f32).abs() < 1e-6);

    // Rarer terms score higher; monotone in df.
    assert!(idf(100, 1) > idf(100, 2));
    assert!(idf(100, 2) > idf(100, 50));
    assert!(idf(100, 50) > idf(100, 100));
    // Lucene's variant stays non-negative even for a term in EVERY document.
    assert!(idf(10, 10) >= 0.0);
    assert!(idf(1, 1) >= 0.0);
}

#[test]
fn text_term_score_zero_saturation_length_and_b_zero() {
    let d = Bm25Params::default();

    // Absent term contributes exactly zero.
    assert_eq!(term_score(0, 10, 10.0, 2.0, d), 0.0);

    // Exact identity: with k1 = 0 and b = 0 the tf factor collapses to
    // tf·1/(tf + 0) = 1, so the score IS the idf — bitwise.
    let p0 = Bm25Params::new(0.0, 0.0).unwrap();
    assert_eq!(term_score(7, 100, 5.0, 0.25, p0), 0.25);
    assert_eq!(term_score(1, 3, 9.0, idf(4, 1), p0), idf(4, 1));

    // tf saturation: strictly increasing with diminishing increments.
    let s1 = term_score(1, 10, 10.0, 1.0, d);
    let s2 = term_score(2, 10, 10.0, 1.0, d);
    let s3 = term_score(3, 10, 10.0, 1.0, d);
    assert!(s2 > s1 && s3 > s2 && s2 - s1 > s3 - s2);

    // Length normalization: longer docs are penalized at default b.
    assert!(term_score(2, 5, 10.0, 1.0, d) > term_score(2, 20, 10.0, 1.0, d));
    // b = 0 disables it entirely — bitwise equal scores.
    assert_eq!(
        term_score(2, 5, 10.0, 1.0, p0),
        term_score(2, 50, 10.0, 1.0, p0)
    );

    // Hand-computed full-form value: tf=2, doc_len=avg_len=10, idf=1, k1=1.2
    // → len_norm = 1 → 1 · (2·2.2)/(2 + 1.2) = 4.4/3.2 = 1.375 up to f32
    // rounding.
    assert!((term_score(2, 10, 10.0, 1.0, d) - 1.375_f32).abs() < 1e-6);
}

// ===========================================================================
// 4. BM25 ranking on fixed corpora — order, score, ties
// ===========================================================================

/// tf and length: `x3` (tf=3, avg length) > `x1short` (tf=1, short) >
/// `x1long` (tf=1, long). Justification: a single query term shares one idf
/// multiplier, so the order is the tf/length factor alone — tf 3 in an
/// avg-length doc gives 3·2.2/(3+1.2) = 1.571, tf 1 in a short doc gives
/// 2.2/(1+1.2·0.5) = 1.375, and among equal-tf docs the shorter doc wins
/// (1.375 > 2.2/(1+1.2·1.5) = 0.786). The term is in all 3 docs (df = n),
/// pinning the in-every-doc idf corner (still positive). Scores asserted
/// bitwise against the public-primitive composition, plus one independent
/// hand constant.
#[test]
fn text_search_bm25_ranking_tf_length_and_ties() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "rank1",
        &[
            (b"x3", doc("cat cat cat")),
            (b"x1short", doc("cat")),
            (b"x1long", doc("cat dog mouse bird fish")),
        ],
    );
    // n=3 docs, avg_len = 9/3 = 3, df(cat) = 3.
    let avg = 9.0f32 / 3.0f32;
    let hits = c.text_search("body", "cat", 10).unwrap();
    assert_eq!(hit_keys(&hits), k(&["x3", "x1short", "x1long"]));
    assert_eq!(hits[0].score, bm25_term(3, 3, avg, 3, 3));
    assert_eq!(hits[1].score, bm25_term(1, 1, avg, 3, 3));
    assert_eq!(hits[2].score, bm25_term(1, 5, avg, 3, 3));
    // Independent hand constant: ln(8/7) · 11/7.
    assert!((hits[0].score - 0.20983505_f32).abs() < 1e-6);
    assert!(hits.iter().all(|h| h.score > 0.0));

    // Ties: three identical docs tie exactly; the documented tiebreak is key
    // order (c1 < c2 < c3 bytewise).
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "ties",
        &[
            (b"c1", doc("same words")),
            (b"c2", doc("same words")),
            (b"c3", doc("same words")),
        ],
    );
    let hits = c.text_search("body", "words", 10).unwrap();
    assert_eq!(hit_keys(&hits), k(&["c1", "c2", "c3"]));
    assert_eq!(hits[0].score, hits[1].score);
    assert_eq!(hits[1].score, hits[2].score);
    // k=2 truncates the tie in key order.
    assert_eq!(
        hit_keys(&c.text_search("body", "words", 2).unwrap()),
        k(&["c1", "c2"])
    );
}

/// Rare vs common term (idf): `r` holds the rare "zebra" (df=1), the others
/// the common "cat" (df=3 of 4). All docs are length 1 with tf 1 and
/// avg_len = 1, so len_norm = 1 and each term factor is exactly 2.2/2.2 = 1
/// — the score IS the idf, bitwise. The rare-term document scores ~3.4x the
/// common-term ones across the two queries: pure idf logic.
#[test]
fn text_search_rare_term_outscores_common_via_idf() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "rare",
        &[
            (b"r", doc("zebra")),
            (b"c1", doc("cat")),
            (b"c2", doc("cat")),
            (b"c3", doc("cat")),
        ],
    );
    let rare = c.text_search("body", "zebra", 10).unwrap();
    assert_eq!(hit_keys(&rare), k(&["r"]));
    assert_eq!(rare[0].score, idf(4, 1));
    assert!((rare[0].score - 1.2039728_f32).abs() < 1e-6); // ln(10/3)

    let common = c.text_search("body", "cat", 10).unwrap();
    assert_eq!(hit_keys(&common), k(&["c1", "c2", "c3"]));
    for h in &common {
        assert_eq!(h.score, idf(4, 3));
    }
    assert!(rare[0].score > common[0].score);
}

/// Index-vs-scan equivalence: creating an in-memory OR on-disk text index
/// never changes the ranked keys or the scores (audit B7). Run over both
/// corpora above and both query kinds.
#[test]
fn text_search_index_inmemory_ondisk_match_scan_twin() {
    let corpus = || {
        vec![
            (b"x3".as_slice(), doc("cat cat cat")),
            (b"x1short".as_slice(), doc("cat")),
            (b"x1long".as_slice(), doc("cat dog mouse bird fish")),
            (b"r".as_slice(), doc("zebra")),
        ]
    };
    let build = |ondisk: bool, name: &str| {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, name, &corpus());
        if ondisk {
            c.create_text_index_ondisk("body").unwrap();
        } else {
            c.create_text_index("body").unwrap();
        }
        db
    };
    let scan_db = seeded_db("twin-scan", &corpus());
    let mem_db = build(false, "twin-mem");
    let disk_db = build(true, "twin-disk");
    let scan = scan_db.collection("twin-scan");
    let mem = mem_db.collection("twin-mem");
    let disk = disk_db.collection("twin-disk");

    for q in ["cat", "zebra", "cat dog"] {
        let base = scan.text_search("body", q, 10).unwrap();
        let a = mem.text_search("body", q, 10).unwrap();
        let b = disk.text_search("body", q, 10).unwrap();
        assert_eq!(hit_keys(&a), hit_keys(&base), "in-memory index, q={q}");
        assert_eq!(hit_keys(&b), hit_keys(&base), "on-disk index, q={q}");
        assert_eq!(
            a.iter().map(|h| h.score).collect::<Vec<_>>(),
            base.iter().map(|h| h.score).collect::<Vec<_>>(),
            "in-memory scores, q={q}"
        );
        assert_eq!(
            b.iter().map(|h| h.score).collect::<Vec<_>>(),
            base.iter().map(|h| h.score).collect::<Vec<_>>(),
            "on-disk scores, q={q}"
        );
    }
}

/// k boundaries and corpus edges: k=0 is an empty window (never an error) on
/// the direct and builder paths; k=1/n/>n; empty collection; a corpus whose
/// every text is empty/whitespace (the avg_len divide-by-zero guard) returns
/// empty.
#[test]
fn text_search_k_boundaries_and_corpus_edges() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "kb",
        &[
            (b"a", doc("alpha beta")),
            (b"b", doc("beta gamma")),
            (b"c", doc("alpha")),
        ],
    );
    assert!(c.text_search("body", "alpha beta", 0).unwrap().is_empty());
    assert_eq!(
        hit_keys(&c.text_search("body", "alpha beta", 1).unwrap()),
        k(&["a"])
    );
    assert_eq!(c.text_search("body", "alpha beta", 3).unwrap().len(), 3);
    assert_eq!(c.text_search("body", "alpha beta", 100).unwrap().len(), 3);
    assert!(
        c.query()
            .text("body", "alpha beta", 0)
            .run()
            .unwrap()
            .is_empty(),
        "builder k=0 is the empty window"
    );

    // Empty collection.
    let empty = db.collection("kb-empty");
    assert!(empty.text_search("body", "alpha", 5).unwrap().is_empty());

    // All-empty-text corpus: analyze yields no terms, every doc_len is 0
    // (avg_len guarded to 1.0), nothing matches anything.
    let db = Db::open_in_memory().unwrap();
    let c = seed(&db, "kb-blank", &[(b"e1", doc("")), (b"e2", doc("   "))]);
    assert!(c.text_search("body", "anything", 5).unwrap().is_empty());
    assert!(c.text_search("body", "", 5).unwrap().is_empty());
}

// ===========================================================================
// 5. text() through the builder
// ===========================================================================

/// The builder's text source: k bounds, degenerate queries (empty string,
/// whitespace, stop-words only, punctuation only — all pinned as empty, not
/// errors), and no-hit queries.
#[test]
fn text_builder_text_k_bounds_empty_and_stopword_queries() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "bt1",
        &[
            (b"a", doc("cat cat cat")),
            (b"b", doc("cat")),
            (b"c", doc("cat dog mouse")),
        ],
    );
    let run = |q: &str, k: usize| {
        c.query()
            .text("body", q, k)
            .run()
            .unwrap()
            .iter()
            .map(|r| r.key.clone())
            .collect::<Vec<_>>()
    };
    // k bounds: 0 empty, 1 top only, n and >n everything (no duplicates).
    assert!(run("cat", 0).is_empty());
    assert_eq!(run("cat", 1), k(&["a"]));
    assert_eq!(run("cat", 3), k(&["a", "b", "c"]));
    assert_eq!(run("cat", 99), k(&["a", "b", "c"]));

    // Degenerate queries: empty string, whitespace, stop-words only,
    // punctuation only — all analyze to zero terms → zero rows, no error.
    for q in ["", "   ", "the and of", "!!! ,,, ???"] {
        assert!(run(q, 5).is_empty(), "q={q:?} must yield no rows");
    }
    // No document contains the term.
    assert!(run("zzzunknown", 5).is_empty());
}

/// The builder corpus: a/b/c carry Text bodies; `n1` lacks the field and
/// `n2` carries a non-Text value — both must be excluded from every ranking.
fn bt_corpus<'a>(db: &'a Db, name: &'a str) -> corvid::Collection<'a> {
    let v: Vec<(&[u8], Value)> = vec![
        (b"a", doc("cat cat cat")),
        (b"b", doc("cat")),
        (b"c", doc("cat dog mouse bird fish")),
        (b"n1", {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("no body".into()));
            Value::Map(m)
        }),
        (b"n2", {
            let mut m = BTreeMap::new();
            m.insert("body".to_owned(), Value::Int(42));
            Value::Map(m)
        }),
    ];
    seed(db, name, &v)
}

/// The builder's score contract: `ResultRow.score` for a single text source
/// is the RRF fused score 1/(60+rank) — the BM25 score is NOT exposed.
/// Multi-term queries rank the doc matching more terms first. `select`
/// narrows the document without touching key/score; `limit` windows the
/// ranked list. Documents missing the field or carrying a non-Text value are
/// deterministically excluded, with and without an index.
#[test]
fn text_builder_text_ranking_scores_select_limit_and_missing_fields() {
    let run_all = |indexed: bool| {
        let db = Db::open_in_memory().unwrap();
        let c = bt_corpus(&db, if indexed { "bt-mixed" } else { "bt-plain" });
        if indexed {
            c.create_text_index("body").unwrap();
        }

        // Single term: rank order a, b, c with fused scores 1/(60+rank);
        // the corpus-excluded n1/n2 never appear.
        let rows = c.query().text("body", "cat", 5).run().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["a", "b", "c"]),
            "indexed={indexed}"
        );
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.score, 1.0f32 / (61.0 + i as f32), "indexed={indexed}");
        }
        assert!(rows.iter().all(|r| r.document.get("body").is_some()));

        // Multi-term: c matches both "cat" and "dog" (rare "dog": df=1)
        // while a/b match only "cat" — c ranks first by a wide margin.
        let rows = c.query().text("body", "cat dog", 5).run().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["c", "a", "b"]),
            "indexed={indexed}"
        );

        // select narrows the document; scores and keys are untouched.
        let rows = c
            .query()
            .select(["body"])
            .text("body", "cat", 5)
            .run()
            .unwrap();
        assert_eq!(rows[0].key, b"a".to_vec());
        assert_eq!(rows[0].score, 1.0f32 / 61.0);
        assert_eq!(
            rows[0].document,
            Value::Map(BTreeMap::from([(
                "body".to_owned(),
                Value::Text("cat cat cat".into())
            )]))
        );

        // limit windows the ranked list.
        let rows = c.query().text("body", "cat dog", 5).limit(2).run().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            k(&["c", "a"])
        );

        // The excluded docs are still stored documents.
        assert!(c.get(b"n1").unwrap().is_some() && c.get(b"n2").unwrap().is_some());

        indexed
    };
    let plain = run_all(false);
    let indexed = run_all(true);
    assert!((plain, indexed) == (false, true)); // both arms executed
}

/// The builder's text-index arm must agree with its scan arm: same keys,
/// same fused scores. (Both corpora have the field on every doc, so the
/// candidate-set stats equal the whole-corpus stats and BM25 orders agree.)
#[test]
fn text_builder_text_index_arm_matches_scan_arm() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", doc("cat cat cat")),
        (b"b", doc("cat")),
        (b"c", doc("cat dog mouse bird fish")),
    ];
    let make = |name: &str, index: bool| {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, name, &docs);
        if index {
            c.create_text_index("body").unwrap();
        }
        db
    };
    let scan_db = make("bt-arm-scan", false);
    let mem_db = make("bt-arm-mem", true);
    let scan = scan_db.collection("bt-arm-scan");
    let mem = mem_db.collection("bt-arm-mem");
    for q in ["cat", "cat dog", "mouse"] {
        let base = scan.query().text("body", q, 5).run().unwrap();
        let idx = mem.query().text("body", q, 5).run().unwrap();
        assert_eq!(
            idx.iter()
                .map(|r| (r.key.clone(), r.score))
                .collect::<Vec<_>>(),
            base.iter()
                .map(|r| (r.key.clone(), r.score))
                .collect::<Vec<_>>(),
            "q={q}"
        );
    }
}

// ===========================================================================
// 6. phrase_search
// ===========================================================================

/// Order-sensitive matching on the fixed phrase corpus (avg_len = 14/5):
/// `pq_c` (len 3) beats `pq_a` (len 4) on "quick brown" — same tf, shorter
/// doc; the reversed-order doc `pq_b` and the non-adjacent doc `pq_gap`
/// (both terms present, not adjacent) do not match; `pq_split` carries
/// "quick" in body and "brown" only in title — a phrase over body can never
/// see the other field. Scores are the BM25 sum over the phrase terms
/// (bitwise vs the public-primitive composition).
#[test]
fn text_phrase_order_sensitive_match_and_scores() {
    let split = {
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), Value::Text("quick".into()));
        m.insert("title".to_owned(), Value::Text("brown".into()));
        Value::Map(m)
    };
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "phrase",
        &[
            (b"pq_a", doc("the quick brown fox jumps")),
            (b"pq_b", doc("a brown quick fox")),
            (b"pq_c", doc("the quick brown dog")),
            (b"pq_gap", doc("quick very brown")),
            (b"pq_split", split),
        ],
    );
    // n = 5 docs with a body, avg = 14/5; df(quick) = 5, df(brown) = 4.
    let avg = 14.0f32 / 5.0f32;
    let hits = c.phrase_search("body", "quick brown", 10).unwrap();
    assert_eq!(hit_keys(&hits), k(&["pq_c", "pq_a"]));
    assert_eq!(
        hits[0].score,
        bm25_term(1, 3, avg, 5, 5) + bm25_term(1, 3, avg, 5, 4)
    );
    assert_eq!(
        hits[1].score,
        bm25_term(1, 4, avg, 5, 5) + bm25_term(1, 4, avg, 5, 4)
    );
    assert!(hits[0].score > hits[1].score);

    // Reversed order: only pq_b holds "brown quick" adjacently.
    assert_eq!(
        hit_keys(&c.phrase_search("body", "brown quick", 10).unwrap()),
        k(&["pq_b"])
    );
    // Terms present but NOT adjacent (pq_gap) never matches.
    assert!(
        !c.phrase_search("body", "quick brown", 10)
            .unwrap()
            .iter()
            .any(|h| h.key == b"pq_gap".to_vec())
    );
    // Case-insensitive phrase input.
    assert_eq!(
        hit_keys(&c.phrase_search("body", "QUICK Brown", 10).unwrap()),
        k(&["pq_c", "pq_a"])
    );
    // A phrase over body cannot match a term living in another field.
    assert!(
        !c.phrase_search("body", "quick brown", 10)
            .unwrap()
            .iter()
            .any(|h| h.key == b"pq_split".to_vec())
    );
}

/// Repeated phrase terms require the repeated adjacency: "buffalo buffalo"
/// matches the 2- and 3-repeat docs (not the single occurrence), and
/// "buffalo buffalo buffalo" matches only the 3-repeat doc. Scores count
/// each phrase-term occurrence (the duplicated term scores twice). Ranking:
/// rep3 (tf=3, len=3) edges rep2 (tf=2, len=2) — 6.6/4.31 > 4.4/2.98 after
/// each is doubled.
#[test]
fn text_phrase_repeated_terms_and_non_adjacent_non_match() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "rep",
        &[
            (b"rep3", doc("buffalo buffalo buffalo")),
            (b"rep2", doc("buffalo buffalo")),
            (b"rep1", doc("buffalo alone here")),
        ],
    );
    let avg = 8.0f32 / 3.0f32; // (3 + 2 + 3) / 3
    let hits = c.phrase_search("body", "buffalo buffalo", 10).unwrap();
    assert_eq!(hit_keys(&hits), k(&["rep3", "rep2"]));
    assert_eq!(hits[0].score, 2.0 * bm25_term(3, 3, avg, 3, 3));
    assert_eq!(hits[1].score, 2.0 * bm25_term(2, 2, avg, 3, 3));

    // Three repeats: only the 3-token doc has a 3-window.
    let hits = c
        .phrase_search("body", "buffalo buffalo buffalo", 10)
        .unwrap();
    assert_eq!(hit_keys(&hits), k(&["rep3"]));
    assert_eq!(hits[0].score, 3.0 * bm25_term(3, 3, avg, 3, 3));
}

/// A single-term phrase is exactly the term search: same docs, same order,
/// bitwise-equal scores (both paths score one term_score call per doc).
#[test]
fn text_phrase_single_term_equals_term_search() {
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "single",
        &[
            (b"a", doc("the quick brown fox jumps")),
            (b"b", doc("a brown quick fox")),
            (b"c", doc("the quick brown dog")),
            (b"d", doc("quick very brown")),
        ],
    );
    let phrase = c.phrase_search("body", "quick", 10).unwrap();
    let term = c.text_search("body", "quick", 10).unwrap();
    assert_eq!(hit_keys(&phrase), hit_keys(&term));
    for (p, t) in phrase.iter().zip(&term) {
        assert_eq!(p.score, t.score);
        assert_eq!(p.document, t.document);
    }
    // Equal-tf/len docs tie by key (b, c, d all len 3; a len 4 last).
    assert_eq!(hit_keys(&phrase), k(&["b", "c", "d", "a"]));
}

/// Stop words collapse out of adjacency (positions are assigned after
/// removal), sentence boundaries and punctuation are not adjacency barriers,
/// and stemming applies to the phrase on both sides.
#[test]
fn text_phrase_stopword_collapse_and_sentence_boundary() {
    // Stop-word collapse, query side and document side.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "sw",
        &[
            (b"sw1", doc("sly fox the quick brown")),
            (b"sw3", doc("brown the quick")),
        ],
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "quick brown", 10).unwrap()),
        k(&["sw1"])
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "quick the brown", 10).unwrap()),
        k(&["sw1"]),
        "a stop word inside the phrase collapses out of adjacency"
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "brown quick", 10).unwrap()),
        k(&["sw3"])
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "brown the quick", 10).unwrap()),
        k(&["sw3"])
    );

    // Sentence boundary / punctuation: ". Start" is adjacency-transparent.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "sent",
        &[
            (b"s_a", doc("end of sentence. Start of new")),
            (b"s_b", doc("start of sentence")),
        ],
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "sentence start", 10).unwrap()),
        k(&["s_a"])
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "SENTENCE Start", 10).unwrap()),
        k(&["s_a"])
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "start sentence", 10).unwrap()),
        k(&["s_b"])
    );

    // Stemming on both sides: plural doc matches singular phrase term.
    let db = Db::open_in_memory().unwrap();
    let c = seed(
        &db,
        "stem",
        &[
            (b"st_a", doc("the running dogs")),
            (b"st_b", doc("dogs running fast")),
        ],
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "running dog", 10).unwrap()),
        k(&["st_a"])
    );
    assert_eq!(
        hit_keys(&c.phrase_search("body", "dog running", 10).unwrap()),
        k(&["st_b"])
    );
}

/// k bounds and degenerate phrases: k=0 / empty / whitespace / stop-word-only
/// / punctuation-only phrases are empty results, never errors; k=1 returns
/// only the best match; a text index (in-memory or on-disk) returns the same
/// keys AND scores as the exact scan.
#[test]
fn text_phrase_k_boundaries_empty_phrase_and_index_arms() {
    let docs: Vec<(&[u8], Value)> = vec![
        (b"a", doc("the quick brown fox jumps")),
        (b"b", doc("a brown quick fox")),
        (b"c", doc("the quick brown dog")),
        (b"d", doc("quick very brown")),
    ];
    let make = |name: &str, index: u8| {
        let db = Db::open_in_memory().unwrap();
        let c = seed(&db, name, &docs);
        if index == 1 {
            c.create_text_index("body").unwrap();
        } else if index == 2 {
            c.create_text_index_ondisk("body").unwrap();
        }
        db
    };
    let scan_db = make("ph-kb-scan", 0);
    let scan = scan_db.collection("ph-kb-scan");

    // Degenerate phrases.
    for p in ["", "   ", "the of a", "!!! ???"] {
        assert!(
            scan.phrase_search("body", p, 10).unwrap().is_empty(),
            "p={p:?}"
        );
    }
    // k bounds.
    assert!(
        scan.phrase_search("body", "quick brown", 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        hit_keys(&scan.phrase_search("body", "quick brown", 1).unwrap()),
        k(&["c"])
    );
    assert_eq!(
        scan.phrase_search("body", "quick brown", 99).unwrap().len(),
        2
    );

    // Index arms: identical keys and scores.
    let base = scan.phrase_search("body", "quick brown", 10).unwrap();
    for (arm, name, db) in [
        ("mem", "ph-kb-mem", make("ph-kb-mem", 1)),
        ("disk", "ph-kb-disk", make("ph-kb-disk", 2)),
    ] {
        let got = db
            .collection(name)
            .phrase_search("body", "quick brown", 10)
            .unwrap();
        assert_eq!(hit_keys(&got), hit_keys(&base), "{arm} keys");
        assert_eq!(
            got.iter().map(|h| h.score).collect::<Vec<_>>(),
            base.iter().map(|h| h.score).collect::<Vec<_>>(),
            "{arm} scores"
        );
    }
}
