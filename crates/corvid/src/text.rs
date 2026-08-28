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
    /// Must be non-negative and finite.
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

impl Bm25Params {
    /// Construct BM25 parameters, validating their domains (audit C6):
    /// `k1` must be non-negative and finite, `b` in `[0, 1]` (NaN rejected
    /// for both — it poisons every score silently). Out-of-range input
    /// returns [`crate::Error::InvalidArgument`] instead of degrading
    /// ranking. [`Bm25Params::default`] is always valid.
    pub fn new(k1: f32, b: f32) -> crate::error::Result<Self> {
        let p = Self { k1, b };
        p.validate()?;
        Ok(p)
    }

    /// Check the parameter domains (audit C6): `k1 >= 0` and finite,
    /// `b` in `[0, 1]`. NaN fails both tests.
    pub fn validate(&self) -> crate::error::Result<()> {
        if !self.k1.is_finite() || self.k1 < 0.0 {
            return Err(crate::Error::InvalidArgument(format!(
                "Bm25Params: k1 must be >= 0, got {}",
                self.k1
            )));
        }
        if !(0.0..=1.0).contains(&self.b) {
            return Err(crate::Error::InvalidArgument(format!(
                "Bm25Params: b must be in [0, 1], got {}",
                self.b
            )));
        }
        Ok(())
    }
}

/// Split text into lowercased alphanumeric tokens.
///
/// Tokens are maximal runs of alphanumeric characters; everything else is a
/// separator. Unicode alphanumerics are kept. This is the raw split with no
/// stop-word removal or stemming — see [`Analyzer`] / [`analyze`] for the
/// pipeline used by the search indexes.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Common English stop words removed by the default analyzer. Kept small and
/// fixed (determinism matters: the same words must be dropped at index and
/// query time).
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "do", "does", "for", "from",
    "had", "has", "have", "he", "i", "if", "in", "into", "is", "it", "its", "me", "my", "no",
    "not", "of", "on", "or", "she", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "we", "were", "will", "with", "you",
];

fn is_stop_word(token: &str) -> bool {
    STOP_WORDS.binary_search(&token).is_ok()
}

/// Harman's S-stemmer: conservative plural normalization only. It maps common
/// plural forms to their singular (`cats`→`cat`, `parties`→`party`,
/// `boxes`→`boxe`) without the aggressive suffix stripping (and resulting
/// false merges) of a full Porter stemmer — deterministic and safe to apply at
/// both index and query time. ASCII-only; non-ASCII tokens pass through.
pub fn s_stem(word: &str) -> String {
    if !word.is_ascii() || word.len() <= 3 {
        return word.to_owned();
    }
    if let Some(base) = word.strip_suffix("ies")
        && !base.ends_with(['a', 'e'])
    {
        // "parties" -> "party"; leave "...aies"/"...eies".
        return format!("{base}y");
    }
    if word.ends_with("es")
        && !(word.ends_with("aes") || word.ends_with("ees") || word.ends_with("oes"))
    {
        // Drop only the trailing 's': "boxes" -> "boxe", "houses" -> "house"
        // (so it matches the singular "house"); leave "...aes/ees/oes".
        return word[..word.len() - 1].to_owned();
    }
    if word.ends_with('s') && !word.ends_with("us") && !word.ends_with("ss") {
        // "cats" -> "cat"; leave "...us"/"...ss".
        return word[..word.len() - 1].to_owned();
    }
    word.to_owned()
}

/// A text analyzer: tokenize, optionally drop stop words, optionally stem.
///
/// The same analyzer must be used at index and query time, which is why the
/// search paths share one [`Analyzer::default`]; the type exists so callers can
/// opt out (e.g. exact-substring needs) without per-index persistence.
#[derive(Debug, Clone, Copy)]
pub struct Analyzer {
    /// Drop common English stop words.
    pub remove_stop_words: bool,
    /// Apply the S-stemmer (plural normalization).
    pub stem: bool,
}

impl Default for Analyzer {
    /// Lowercase + stop-word removal + S-stemming.
    fn default() -> Self {
        Self {
            remove_stop_words: true,
            stem: true,
        }
    }
}

impl Analyzer {
    /// The raw analyzer: tokenize only (no stop words removed, no stemming).
    pub fn raw() -> Self {
        Self {
            remove_stop_words: false,
            stem: false,
        }
    }

    /// Analyze `text` into the terms used for indexing and matching.
    pub fn analyze(&self, text: &str) -> Vec<String> {
        tokenize(text)
            .into_iter()
            .filter(|t| !(self.remove_stop_words && is_stop_word(t)))
            .map(|t| if self.stem { s_stem(&t) } else { t })
            .collect()
    }
}

/// Analyze `text` with the default analyzer (stop-word removal + S-stemming).
/// This is what the full-text indexes and BM25 search use, at both index and
/// query time, so the two always agree.
pub fn analyze(text: &str) -> Vec<String> {
    Analyzer::default().analyze(text)
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
    fn s_stem_normalizes_plurals_conservatively() {
        assert_eq!(s_stem("cats"), "cat");
        assert_eq!(s_stem("parties"), "party");
        // "es" drops only the 's', so plurals of -e nouns match their singular.
        assert_eq!(s_stem("houses"), "house");
        assert_eq!(s_stem("boxes"), "boxe");
        // Conservative: leaves words that aren't simple plurals.
        assert_eq!(s_stem("bus"), "bus"); // ...us
        assert_eq!(s_stem("class"), "class"); // ...ss
        assert_eq!(s_stem("the"), "the"); // too short
    }

    #[test]
    fn analyze_removes_stopwords_and_stems() {
        assert_eq!(
            analyze("The quick brown foxes"),
            vec!["quick", "brown", "foxe"]
        );
        // All stop words → empty.
        assert!(analyze("the and of to").is_empty());
    }

    #[test]
    fn analyze_is_deterministic_and_matches_singular_plural() {
        // A plural in the document and its singular in the query analyze to the
        // same term, so they match — the point of stemming.
        let doc = analyze("two dogs running");
        let q = analyze("dog");
        assert!(doc.contains(&q[0]));
        assert_eq!(analyze("Dogs"), analyze("dogs"));
    }

    #[test]
    fn raw_analyzer_keeps_everything() {
        assert_eq!(Analyzer::raw().analyze("The cats"), vec!["the", "cats"]);
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

    /// Audit C6: the validated constructor rejects out-of-domain parameters
    /// (`k1 < 0` or NaN, `b` outside `[0, 1]` or NaN) with
    /// `Error::InvalidArgument` instead of letting them silently poison
    /// every score; the closed-interval boundaries are accepted, and the
    /// engine's defaults always validate.
    #[test]
    fn bm25_params_new_validates_ranges() {
        // Valid, including the boundaries.
        assert!(Bm25Params::new(1.2, 0.75).is_ok());
        assert!(Bm25Params::new(0.0, 0.0).is_ok());
        assert!(Bm25Params::new(2.0, 1.0).is_ok());
        // Invalid k1 (negative or NaN).
        assert!(Bm25Params::new(-0.1, 0.75).is_err());
        assert!(Bm25Params::new(f32::NAN, 0.75).is_err());
        // Invalid b (outside [0, 1] or NaN).
        assert!(Bm25Params::new(1.2, -0.1).is_err());
        assert!(Bm25Params::new(1.2, 1.5).is_err());
        assert!(Bm25Params::new(1.2, f32::NAN).is_err());
        // The engine's defaults always validate.
        assert!(Bm25Params::default().validate().is_ok());
    }
}
