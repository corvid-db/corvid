//! Tokenization and BM25 scoring primitives for full-text search.
//!
//! These are pure functions over already-extracted text. The collection-level
//! search that uses them lives in [`crate::query`]. Like the vector path, the
//! v0.1 search scores by scanning the corpus; a persistent inverted index can
//! replace the scan later without changing the scoring math here.

/// BM25 tuning parameters.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    /// Term-frequency saturation. Higher means tf keeps mattering longer.
    pub k1: f32,
    /// Length normalization in `[0, 1]`. 0 disables it.
    pub b: f32,
}

impl Default for Bm25Params {
    /// The widely used defaults `k1 = 1.2`, `b = 0.75`.
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

/// Split text into lowercased alphanumeric tokens.
///
/// Tokens are maximal runs of alphanumeric characters; everything else is a
/// separator. Unicode alphanumerics are kept. This is the deliberately simple
/// v0.1 analyzer — no stemming or stop-word removal yet.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Inverse document frequency (Lucene's BM25 variant), always non-negative.
///
/// `n_docs` is the corpus size, `doc_freq` the number of documents containing
/// the term. Rarer terms score higher.
pub fn idf(n_docs: usize, doc_freq: usize) -> f32 {
    let n = n_docs as f32;
    let df = doc_freq as f32;
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// BM25 contribution of a single query term occurring `tf` times in a document
/// of length `doc_len`, given the corpus `avg_len` and the term's `idf`.
pub fn term_score(tf: u32, doc_len: usize, avg_len: f32, idf: f32, params: Bm25Params) -> f32 {
    if tf == 0 {
        return 0.0;
    }
    let tf = tf as f32;
    let len_norm = 1.0 - params.b + params.b * (doc_len as f32 / avg_len);
    idf * (tf * (params.k1 + 1.0)) / (tf + params.k1 * len_norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn tokenize_splits_lowercases_and_drops_empties() {
        assert_eq!(
            tokenize("The Quick, brown FOX!"),
            vec!["the", "quick", "brown", "fox"]
        );
    }

    #[test]
    fn tokenize_handles_numbers_and_unicode() {
        assert_eq!(tokenize("abc123 café"), vec!["abc123", "café"]);
    }

    #[test]
    fn tokenize_empty_and_punctuation_only() {
        assert!(tokenize("").is_empty());
        assert!(tokenize(",.-!! ??").is_empty());
    }

    #[test]
    fn idf_is_higher_for_rarer_terms() {
        // term in 1 of 100 docs vs term in 50 of 100
        assert!(idf(100, 1) > idf(100, 50));
    }

    #[test]
    fn idf_is_non_negative_even_for_common_terms() {
        assert!(idf(10, 10) >= 0.0);
    }

    #[test]
    fn term_score_zero_when_absent() {
        assert!(close(
            term_score(0, 10, 10.0, 2.0, Bm25Params::default()),
            0.0
        ));
    }

    #[test]
    fn term_score_saturates_with_tf() {
        let p = Bm25Params::default();
        let s1 = term_score(1, 10, 10.0, 1.0, p);
        let s2 = term_score(2, 10, 10.0, 1.0, p);
        let s3 = term_score(3, 10, 10.0, 1.0, p);
        // Strictly increasing, but each equal-width step adds less (saturating).
        assert!(s2 > s1);
        assert!(s3 > s2);
        assert!(s2 - s1 > s3 - s2);
    }

    #[test]
    fn longer_documents_are_penalized() {
        let p = Bm25Params::default();
        let short = term_score(2, 5, 10.0, 1.0, p);
        let long = term_score(2, 20, 10.0, 1.0, p);
        assert!(short > long);
    }

    #[test]
    fn b_zero_disables_length_normalization() {
        let p = Bm25Params { k1: 1.2, b: 0.0 };
        let short = term_score(2, 5, 10.0, 1.0, p);
        let long = term_score(2, 50, 10.0, 1.0, p);
        assert!(close(short, long));
    }
}
