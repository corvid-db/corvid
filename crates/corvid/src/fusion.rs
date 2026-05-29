//! Rank fusion and diversification operators.
//!
//! These compose the retrieval modalities into one result — the heart of the
//! hybrid search story. They work on keys and vectors, independent of how the
//! candidates were retrieved, so the same operators serve vector, text, and
//! any future modality.

use crate::distance::Metric;

/// The conventional RRF rank constant.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Fuse several ranked key lists with Reciprocal Rank Fusion.
///
/// Each input list is keys ordered best-first. A key's fused score is the sum
/// over lists of `1 / (k + rank)`, where `rank` starts at 1. Larger `k`
/// flattens the contribution of top ranks. Returns `(key, score)` sorted by
/// score descending, ties broken by key for determinism. A key appearing in
/// several lists accumulates across them; duplicates within one list count
/// only at their best (first) rank.
pub fn reciprocal_rank_fusion(rankings: &[&[Vec<u8>]], k: f32) -> Vec<(Vec<u8>, f32)> {
    use std::collections::HashMap;

    let mut scores: HashMap<Vec<u8>, f32> = HashMap::new();
    for ranking in rankings {
        let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
        for (rank, key) in ranking.iter().enumerate() {
            // Only the best rank of a key within a single list contributes.
            if !seen.insert(key.as_slice()) {
                continue;
            }
            let contribution = 1.0 / (k + (rank as f32 + 1.0));
            *scores.entry(key.clone()).or_insert(0.0) += contribution;
        }
    }

    let mut fused: Vec<(Vec<u8>, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused
}

/// Select up to `k` keys by Maximal Marginal Relevance, balancing relevance to
/// `query` against diversity among the chosen set.
///
/// `candidates` are `(key, embedding)` pairs; every embedding must match
/// `query`'s dimension (callers filter first). `lambda` in `[0, 1]` trades off
/// relevance (1.0) versus diversity (0.0). Similarity is `-metric.distance`, so
/// higher is more similar under any metric. Returns keys in selection order.
/// Ties break by key.
pub fn mmr(
    query: &[f32],
    candidates: &[(Vec<u8>, Vec<f32>)],
    lambda: f32,
    k: usize,
    metric: Metric,
) -> Vec<Vec<u8>> {
    let sim = |a: &[f32], b: &[f32]| -metric.distance(a, b);

    let relevance: Vec<f32> = candidates.iter().map(|(_, v)| sim(query, v)).collect();

    let mut remaining: Vec<usize> = (0..candidates.len()).collect();
    let mut selected: Vec<usize> = Vec::new();
    let limit = k.min(candidates.len());

    while selected.len() < limit {
        let mut best: Option<(usize, usize, f32)> = None; // (position in remaining, candidate idx, score)
        for (pos, &i) in remaining.iter().enumerate() {
            let diversity_penalty = selected
                .iter()
                .map(|&j| sim(&candidates[i].1, &candidates[j].1))
                .fold(f32::NEG_INFINITY, f32::max);
            let penalty = if selected.is_empty() {
                0.0
            } else {
                diversity_penalty
            };
            let score = lambda * relevance[i] - (1.0 - lambda) * penalty;

            let better = match best {
                None => true,
                Some((_, bi, bscore)) => {
                    score > bscore || (score == bscore && candidates[i].0 < candidates[bi].0)
                }
            };
            if better {
                best = Some((pos, i, score));
            }
        }

        let (pos, idx, _) = best.expect("remaining is non-empty while selected < limit");
        remaining.remove(pos);
        selected.push(idx);
    }

    selected
        .into_iter()
        .map(|i| candidates[i].0.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: &[u8]) -> Vec<u8> {
        b.to_vec()
    }

    #[test]
    fn rrf_rewards_agreement_across_lists() {
        let list1 = vec![key(b"a"), key(b"b"), key(b"c")];
        let list2 = vec![key(b"b"), key(b"a"), key(b"d")];
        let fused = reciprocal_rank_fusion(&[&list1, &list2], DEFAULT_RRF_K);
        // a: 1/61 + 1/62, b: 1/62 + 1/61 -> tie; tie-break by key -> a first.
        assert_eq!(fused[0].0, key(b"a"));
        assert_eq!(fused[1].0, key(b"b"));
        // c and d appear once each, ranked below.
        assert!(fused[0].1 > fused[2].1);
    }

    #[test]
    fn rrf_single_list_preserves_order() {
        let list = vec![key(b"x"), key(b"y"), key(b"z")];
        let fused = reciprocal_rank_fusion(&[&list], DEFAULT_RRF_K);
        let keys: Vec<_> = fused.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![key(b"x"), key(b"y"), key(b"z")]);
    }

    #[test]
    fn rrf_higher_rank_scores_more() {
        let list = vec![key(b"first"), key(b"second")];
        let fused = reciprocal_rank_fusion(&[&list], DEFAULT_RRF_K);
        assert!(fused[0].1 > fused[1].1);
    }

    #[test]
    fn rrf_dedupes_within_a_list() {
        let list = vec![key(b"a"), key(b"a"), key(b"b")];
        let fused = reciprocal_rank_fusion(&[&list], DEFAULT_RRF_K);
        // "a" counts once (at rank 1), so it still beats "b" (rank 3).
        let a = fused.iter().find(|(k, _)| k == &key(b"a")).unwrap().1;
        let single = 1.0 / (DEFAULT_RRF_K + 1.0);
        assert!((a - single).abs() < 1e-6);
    }

    #[test]
    fn rrf_empty_input_is_empty() {
        let fused = reciprocal_rank_fusion(&[], DEFAULT_RRF_K);
        assert!(fused.is_empty());
    }

    #[test]
    fn mmr_lambda_one_is_pure_relevance_order() {
        let q = vec![1.0, 0.0];
        let cands = vec![
            (key(b"near"), vec![1.0, 0.0]),
            (key(b"mid"), vec![0.7, 0.7]),
            (key(b"far"), vec![-1.0, 0.0]),
        ];
        let out = mmr(&q, &cands, 1.0, 3, Metric::Cosine);
        assert_eq!(out, vec![key(b"near"), key(b"mid"), key(b"far")]);
    }

    #[test]
    fn mmr_diversifies_against_near_duplicates() {
        let q = vec![1.0, 0.0];
        // Two near-duplicates of the query, one orthogonal alternative.
        let cands = vec![
            (key(b"dup1"), vec![1.0, 0.0]),
            (key(b"dup2"), vec![0.99, 0.01]),
            (key(b"diverse"), vec![0.0, 1.0]),
        ];
        // First pick is the most relevant; with diversity weight, the second
        // pick should be the orthogonal one, not the near-duplicate.
        let out = mmr(&q, &cands, 0.5, 2, Metric::Cosine);
        assert_eq!(out[0], key(b"dup1"));
        assert_eq!(out[1], key(b"diverse"));
    }

    #[test]
    fn mmr_respects_k_and_caps_at_candidate_count() {
        let q = vec![1.0, 0.0];
        let cands = vec![(key(b"a"), vec![1.0, 0.0]), (key(b"b"), vec![0.0, 1.0])];
        assert_eq!(mmr(&q, &cands, 0.5, 1, Metric::Cosine).len(), 1);
        assert_eq!(mmr(&q, &cands, 0.5, 10, Metric::Cosine).len(), 2);
    }

    #[test]
    fn mmr_empty_candidates_is_empty() {
        let q = vec![1.0, 0.0];
        let out = mmr(&q, &[], 0.5, 5, Metric::Cosine);
        assert!(out.is_empty());
    }

    #[test]
    fn mmr_tie_breaks_by_key() {
        let q = vec![1.0, 0.0];
        // Identical embeddings -> equal relevance -> key order decides first pick.
        let cands = vec![(key(b"z"), vec![1.0, 0.0]), (key(b"a"), vec![1.0, 0.0])];
        let out = mmr(&q, &cands, 1.0, 1, Metric::Cosine);
        assert_eq!(out, vec![key(b"a")]);
    }
}
