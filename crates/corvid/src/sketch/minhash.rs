//! MinHash signatures and LSH banding: approximate Jaccard similarity
//! between sets of byte-string items, and a bucketed index that surfaces
//! candidate pairs without comparing everything.
//!
//! Deterministic throughout: the k signature components are k fixed seeded
//! permutations of the shared `DefaultHasher` base hash (a bijective mixer
//! XOR a per-component odd seed — no `rand`), and band bucket keys hash the
//! band's signature slice with the same hasher.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hasher;

use super::hash64;

/// The SplitMix64 finalizer: a bijection on `u64`, used to derive
/// independent-looking permutations of a base hash.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// MinHash estimates the Jaccard similarity `|A ∩ B| / |A ∪ B|` of two sets
/// from fixed-size signatures: component `i` of a set's signature is the
/// minimum, over its items, of permutation `i` applied to the item's hash.
/// The fraction of matching components between two signatures estimates
/// the similarity.
pub struct MinHash {
    k: usize,
}

impl MinHash {
    /// A MinHash producing `k_hashes`-component signatures (`k_hashes` is
    /// clamped to at least 1). Estimation error shrinks like
    /// `sqrt(J·(1−J)/k)`.
    pub fn new(k_hashes: usize) -> Self {
        Self { k: k_hashes.max(1) }
    }

    /// The signature of a set of items (each identified by its bytes).
    ///
    /// Order and duplicates in `items` do not matter — the result is a
    /// function of the set of distinct byte strings, so permutations and
    /// repeats produce byte-identical signatures.
    pub fn signature(&self, items: &[&[u8]]) -> Vec<u64> {
        let mut sig = vec![u64::MAX; self.k];
        for item in items {
            let h = hash64(item);
            for (i, slot) in sig.iter_mut().enumerate() {
                // Permutation i: an odd per-component seed (odd so it has
                // full period as a multiplier), then the bijective mixer.
                let seed = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                let v = mix(h ^ seed);
                if v < *slot {
                    *slot = v;
                }
            }
        }
        sig
    }

    /// The estimated Jaccard similarity of the sets behind two signatures:
    /// the fraction of equal components. `None` when the lengths differ
    /// (signatures from different `k`) or either is empty.
    ///
    /// Identical sets estimate exactly `1.0` and disjoint sets exactly
    /// `0.0` (absent an astronomically unlikely 64-bit hash collision);
    /// everything else is within sampling error of the true similarity.
    pub fn jaccard_estimate(a: &[u64], b: &[u64]) -> Option<f64> {
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        let equal = a.iter().zip(b).filter(|(x, y)| x == y).count();
        Some(equal as f64 / a.len() as f64)
    }
}

/// Locality-sensitive hashing over MinHash signatures: each signature is
/// split into `bands` bands of `rows` components, and sets whose signatures
/// agree on ALL components of at least one band land in the same bucket —
/// so [`candidates`](LshIndex::candidates) returns them without a full
/// pairwise pass.
///
/// The candidate probability for a pair at true similarity `J` follows the
/// banding curve `1 − (1 − J^rows)^bands`: near 1 for similarities above
/// the banding threshold, near 0 below it. Pick `bands × rows` = signature
/// length.
pub struct LshIndex {
    bands: usize,
    rows: usize,
    /// Band bucket → keys inserted under it (sorted, deduplicated).
    buckets: BTreeMap<(usize, u64), BTreeSet<Vec<u8>>>,
}

impl LshIndex {
    /// An index over signatures of `bands * rows` components (both clamped
    /// to at least 1).
    pub fn new(bands: usize, rows: usize) -> Self {
        Self {
            bands: bands.max(1),
            rows: rows.max(1),
            buckets: BTreeMap::new(),
        }
    }

    /// Index `key` under its signature's band buckets. Returns `true` when
    /// stored; `false` when the signature length is not `bands * rows`, in
    /// which case nothing is stored.
    pub fn insert(&mut self, key: &[u8], signature: &[u64]) -> bool {
        if signature.len() != self.bands * self.rows {
            return false;
        }
        for (band, bucket) in self.band_buckets(signature) {
            self.buckets
                .entry((band, bucket))
                .or_default()
                .insert(key.to_vec());
        }
        true
    }

    /// Keys whose signatures share at least one full band with the given
    /// signature, in byte order, each key once. Empty when the signature
    /// length is not `bands * rows`.
    ///
    /// High-similarity pairs appear with probability
    /// `1 − (1 − J^rows)^bands`; low-similarity pairs mostly do not.
    pub fn candidates(&self, signature: &[u64]) -> Vec<Vec<u8>> {
        if signature.len() != self.bands * self.rows {
            return Vec::new();
        }
        let mut hits: BTreeSet<Vec<u8>> = BTreeSet::new();
        for (band, bucket) in self.band_buckets(signature) {
            if let Some(keys) = self.buckets.get(&(band, bucket)) {
                hits.extend(keys.iter().cloned());
            }
        }
        hits.into_iter().collect()
    }

    /// The `(band, bucket)` pairs a signature hashes into.
    fn band_buckets(&self, signature: &[u64]) -> Vec<(usize, u64)> {
        (0..self.bands)
            .map(|band| {
                let mut h = DefaultHasher::new();
                for component in &signature[band * self.rows..(band + 1) * self.rows] {
                    h.write_u64(*component);
                }
                (band, h.finish())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minhash_identical_and_disjoint_sets_are_exact() {
        let mh = MinHash::new(64);
        let a = mh.signature(&[b"apple", b"banana", b"cherry"]);
        let same = mh.signature(&[b"cherry", b"apple", b"banana", b"apple"]);
        assert_eq!(a, same, "order and duplicates must not matter");
        let b = mh.signature(&[b"dog", b"elephant", b"fox"]);
        assert_eq!(MinHash::jaccard_estimate(&a, &a), Some(1.0));
        assert_eq!(MinHash::jaccard_estimate(&a, &b), Some(0.0));
        assert_eq!(MinHash::jaccard_estimate(&a, &[]), None);
        assert_eq!(MinHash::jaccard_estimate(&a, &a[..3]), None);
    }

    #[test]
    fn minhash_estimate_is_within_sampling_error() {
        // |A| = |B| = 40, overlap 30 → true J = 30/50 = 0.6.
        let mh = MinHash::new(256);
        let a_owned: Vec<String> = (0..40).map(|i| format!("a{i}")).collect();
        let b_owned: Vec<String> = (10..50).map(|i| format!("a{i}")).collect();
        let a: Vec<&[u8]> = a_owned.iter().map(|s| s.as_bytes()).collect();
        let b: Vec<&[u8]> = b_owned.iter().map(|s| s.as_bytes()).collect();
        let est = MinHash::jaccard_estimate(&mh.signature(&a), &mh.signature(&b)).unwrap();
        let sigma = (0.6 * 0.4 / 256.0_f64).sqrt();
        assert!(
            (est - 0.6).abs() <= 3.0 * sigma,
            "estimate {est} outside 3σ of 0.6 (σ = {sigma})"
        );
    }

    #[test]
    fn lsh_banding_finds_similar_and_rejects_mismatched_lengths() {
        let mh = MinHash::new(16);
        let mut idx = LshIndex::new(4, 4);
        let sig = mh.signature(&[b"x", b"y"]);
        assert!(idx.insert(b"k", &sig));
        assert!(!idx.insert(b"bad", &sig[..10]), "wrong length");
        assert_eq!(idx.candidates(&sig), vec![b"k".to_vec()]);
        assert!(idx.candidates(&sig[..3]).is_empty());
    }
}
