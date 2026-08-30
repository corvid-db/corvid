//! Tokenization and BM25 scoring primitives for full-text search.
//!
//! These are pure functions over already-extracted text. The collection-level
//! search that uses them lives in [`crate::query`]. Like the vector path, the
//! v0.1 search scores by scanning the corpus; a persistent inverted index can
//! replace the scan later without changing the scoring math here.
//!
//! # CJK segmentation (ledger-closure Task 4)
//!
//! Runs of CJK characters — hiragana/katakana, the Han ideograph blocks,
//! the exact boundary documented on the private `is_cjk` predicate below —
//! are tokenized as sliding BIGRAMS of adjacent characters
//! (single-character run → that character),
//! the standard dictionary-free fallback for the unspaced CJK scripts: no
//! segmentation dictionaries, no dependencies. Latin behavior is unchanged,
//! and in mixed text the two coexist in one token stream. The same
//! [`analyze`] feeds index build and query on every serving path (scan,
//! in-memory index, on-disk index), so bigram positions line up and phrase
//! search works over bigrams naturally (a 3-character phrase is two adjacent
//! bigrams at consecutive positions). Stemming and case folding never apply
//! to CJK tokens — the S-stemmer is ASCII-only and CJK has no case — and
//! hangul is deliberately outside the bigram set (Korean is space-separated;
//! whole runs are its tokens, like latin).

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

/// The CJK codepoint set tokenized by bigram (the boundary is a recorded
/// decision, see the module docs):
///
/// * U+3040–30FF — hiragana + katakana, prolonged sound mark `ー`
///   (U+30FC) included. The combining dakuten U+3099/309A never reach
///   this check: std classifies them non-alphanumeric, so they are
///   separators like any other mark (the documented combining edge — no
///   normalization is applied; NFC is the recommended storage form).
/// * U+3400–4DBF — CJK Unified Ideographs Extension A.
/// * U+4E00–9FFF — CJK Unified Ideographs (the main block).
/// * U+F900–FAFF — CJK Compatibility Ideographs.
/// * U+20000–323AF — the supplementary-plane extensions (B–H).
///
/// Deliberately OUTSIDE: hangul (Korean is space-separated, so whole-run
/// tokens — the latin behavior — already segment it) and halfwidth
/// katakana U+FF66–FF9F (compatibility forms; NFKC-normalize upstream if
/// bigrams are wanted). CJK punctuation (、。」 etc.) is non-alphanumeric
/// and therefore a separator, as before.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3040}'..='\u{30FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{323AF}'
    )
}

/// Append the bigram tokens of one CJK run: the sliding window over
/// adjacent characters (a 3-char run yields 2 bigrams). A single-character
/// run yields that character.
fn push_cjk_bigrams(run: &[char], out: &mut Vec<String>) {
    if run.len() == 1 {
        out.push(run[0].to_string());
    } else {
        out.extend(run.windows(2).map(|w| w.iter().collect()));
    }
}

/// Split text into lowercased alphanumeric tokens.
///
/// Tokens are maximal runs of alphanumeric characters; everything else is a
/// separator. Unicode alphanumerics are kept. Within an alphanumeric run,
/// a maximal sub-run of CJK characters (see the `is_cjk` boundary's
/// documented ranges) is emitted as sliding BIGRAMS of adjacent characters instead
/// of one whole token — the standard dictionary-free CJK segmentation
/// fallback for search — while a CJK↔non-CJK transition splits the run
/// (latin pieces keep today's whole-token + lowercase behavior). The
/// Han↔kana script transition inside a CJK run does NOT restart the
/// window: `東京タワー` bigrams as one 5-character run. This is the raw
/// split with no stop-word removal or stemming — see [`Analyzer`] /
/// [`analyze`] for the pipeline used by the search indexes (and note the
/// pipeline never stems CJK tokens).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut latin = String::new();
    let mut cjk: Vec<char> = Vec::new();

    let flush_latin = |latin: &mut String, tokens: &mut Vec<String>| {
        if !latin.is_empty() {
            // Case folding applies to the non-CJK pieces only; CJK has no
            // case mappings, so the bigrams are stored as written.
            tokens.push(std::mem::take(latin).to_lowercase());
        }
    };
    let flush_cjk = |cjk: &mut Vec<char>, tokens: &mut Vec<String>| {
        if !cjk.is_empty() {
            push_cjk_bigrams(cjk, tokens);
            cjk.clear();
        }
    };

    for c in text.chars() {
        if !c.is_alphanumeric() {
            flush_latin(&mut latin, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        } else if is_cjk(c) {
            flush_latin(&mut latin, &mut tokens);
            cjk.push(c);
        } else {
            flush_cjk(&mut cjk, &mut tokens);
            latin.push(c);
        }
    }
    flush_latin(&mut latin, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
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
    ///
    /// CJK runs arrive here as bigrams (see [`tokenize`]) and pass through
    /// both stages untouched: the stop-word list is English-only and the
    /// S-stemmer is ASCII-only, so stemming never applies to CJK tokens —
    /// `東京` never merges with `東` (pinned by conformance).
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

    /// The CJK bigram rule at unit level: the documented boundary in, the
    /// exact token stream out (mixed strings, script transitions, single
    /// chars, the hangul/halfwidth exclusions, the combining-dakuten edge).
    #[test]
    fn tokenize_cjk_runs_bigram_with_documented_boundary() {
        assert_eq!(tokenize("日本語"), vec!["日本", "本語"]);
        assert_eq!(tokenize("東京"), vec!["東京"]);
        assert_eq!(tokenize("東"), vec!["東"]);
        assert_eq!(tokenize("あいう"), vec!["あい", "いう"]);
        // One run across the Han↔katakana transition; ー is in the set.
        assert_eq!(tokenize("東京タワー"), vec!["東京", "京タ", "タワ", "ワー"]);
        // CJK/non-CJK transitions split; latin still folds.
        assert_eq!(tokenize("abc東京def"), vec!["abc", "東京", "def"]);
        assert_eq!(tokenize("Tokyo駅"), vec!["tokyo", "駅"]);
        assert_eq!(tokenize("rustで検索"), vec!["rust", "で検", "検索"]);
        // Extension B is inside the set; hangul and halfwidth kana are not.
        assert_eq!(tokenize("𠀀𠀁𠀂"), vec!["𠀀𠀁", "𠀁𠀂"]);
        assert_eq!(tokenize("한국어"), vec!["한국어"]);
        assert_eq!(tokenize("ﾃｷｽﾄ"), vec!["ﾃｷｽﾄ"]);
        // Combining dakuten are non-alphanumeric separators (std tables):
        // NFD text splits at them; no normalization is applied here.
        assert_eq!(tokenize("か\u{3099}き"), vec!["か", "き"]);
    }

    /// The boundary predicate itself: every range endpoint is in, the
    /// nearest codepoints outside each range are out.
    #[test]
    fn cjk_boundary_range_endpoints() {
        for c in [
            '\u{3040}',
            '\u{30FF}',
            '\u{3400}',
            '\u{4DBF}',
            '\u{4E00}',
            '\u{9FFF}',
            '\u{F900}',
            '\u{FAFF}',
            '\u{20000}',
            '\u{323AF}',
        ] {
            assert!(is_cjk(c), "U+{:04X} must be inside the set", c as u32);
        }
        for c in [
            '\u{303F}', // CJK punctuation edge below hiragana
            '\u{3100}', // below Ext A
            '\u{4DC0}', // Yijing hexagrams between Ext A and URO
            '\u{A000}', // Yi syllables above URO
            '\u{FB00}', // latin ligatures below compat ideographs
            '\u{AC00}', // hangul syllables — outside by decision
            '\u{D7AF}', '\u{FF66}', // halfwidth katakana — outside by decision
            '\u{FF9F}',
        ] {
            assert!(!is_cjk(c), "U+{:04X} must be outside the set", c as u32);
        }
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
