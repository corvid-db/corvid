//! Probabilistic sketches: HyperLogLog (approximate distinct counts),
//! Bloom and cuckoo filters (approximate set membership), a t-digest
//! (approximate quantiles/CDF), and MinHash signatures with LSH banding
//! (approximate set similarity and candidate lookup).
//!
//! All of them trade a bounded, tunable error for tiny memory and share one
//! determinism discipline: hashing goes through the standard library's
//! `DefaultHasher` (or fixed integer mixers derived from it) — no `rand`,
//! no dependencies — so a sketch built from the same inputs in the same
//! order is reproducible run to run. [`Collection::approx_distinct`] wires
//! HyperLogLog over a document field.

mod cuckoo;
mod minhash;
mod tdigest;

pub use cuckoo::CuckooFilter;
pub use minhash::{LshIndex, MinHash};
pub use tdigest::TDigest;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::db::Collection;
use crate::error::Result;

/// Hash bytes to a 64-bit value with the standard (deterministic) hasher.
fn hash64(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// A HyperLogLog cardinality estimator.
pub struct HyperLogLog {
    registers: Vec<u8>,
    p: u32,
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::with_precision(14)
    }
}

impl HyperLogLog {
    /// A HyperLogLog with the default precision (p = 14, ~0.8% error,
    /// 16 KiB of registers).
    pub fn new() -> Self {
        Self::default()
    }

    /// A HyperLogLog with precision `p` registers exponent, clamped to
    /// `4..=16` (`2^p` registers).
    pub fn with_precision(p: u32) -> Self {
        let p = p.clamp(4, 16);
        Self {
            registers: vec![0u8; 1 << p],
            p,
        }
    }

    /// Observe an item by its raw bytes.
    pub fn add_bytes(&mut self, bytes: &[u8]) {
        self.add_hash(hash64(bytes));
    }

    /// Observe an item by a precomputed 64-bit hash.
    pub fn add_hash(&mut self, hash: u64) {
        let index = (hash >> (64 - self.p)) as usize;
        let w = hash << self.p;
        // OR in a sentinel bit so the rank is bounded by the remaining width.
        let rank = (w | (1u64 << (self.p - 1))).leading_zeros() as u8 + 1;
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }
    }

    /// Estimate the number of distinct items observed.
    pub fn estimate(&self) -> u64 {
        let m = self.registers.len() as f64;
        let alpha = match self.p {
            4 => 0.673,
            5 => 0.697,
            6 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / m),
        };
        let sum: f64 = self.registers.iter().map(|&r| 2f64.powi(-(r as i32))).sum();
        let raw = alpha * m * m / sum;

        // Small-range correction via linear counting.
        if raw <= 2.5 * m {
            let zeros = self.registers.iter().filter(|&&r| r == 0).count();
            if zeros > 0 {
                return (m * (m / zeros as f64).ln()).round() as u64;
            }
        }
        raw.round() as u64
    }
}

/// A Bloom filter for approximate set membership (no false negatives).
pub struct BloomFilter {
    words: Vec<u64>,
    num_bits: usize,
    k: u32,
}

impl BloomFilter {
    /// A filter sized for `expected_items` at the target `fp_rate` in `(0, 1)`.
    pub fn new(expected_items: usize, fp_rate: f64) -> Self {
        let n = expected_items.max(1) as f64;
        let p = fp_rate.clamp(f64::MIN_POSITIVE, 0.5);
        let ln2 = std::f64::consts::LN_2;
        let num_bits = (-(n * p.ln()) / (ln2 * ln2)).ceil().max(1.0) as usize;
        let k = (((num_bits as f64 / n) * ln2).round() as u32).max(1);
        Self {
            words: vec![0u64; num_bits.div_ceil(64)],
            num_bits,
            k,
        }
    }

    /// Insert an item.
    pub fn add_bytes(&mut self, bytes: &[u8]) {
        let bits: Vec<usize> = self.bit_indices(bytes).collect();
        for bit in bits {
            self.words[bit / 64] |= 1u64 << (bit % 64);
        }
    }

    /// Test membership. `false` is definitive; `true` may be a false positive.
    pub fn contains_bytes(&self, bytes: &[u8]) -> bool {
        self.bit_indices(bytes)
            .all(|bit| self.words[bit / 64] & (1u64 << (bit % 64)) != 0)
    }

    /// The `k` bit positions for an item, via double hashing.
    fn bit_indices(&self, bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
        let h = hash64(bytes);
        let h1 = h;
        let h2 = h.rotate_left(32) | 1; // odd, so it strides the whole space
        let num_bits = self.num_bits as u64;
        (0..self.k).map(move |i| (h1.wrapping_add((i as u64).wrapping_mul(h2)) % num_bits) as usize)
    }
}

impl Collection<'_> {
    /// Estimate the number of distinct values of `field` across the
    /// collection, using HyperLogLog. Documents lacking the field are ignored.
    pub fn approx_distinct(&self, field: &str) -> Result<u64> {
        let mut hll = HyperLogLog::new();
        self.for_each_doc(|_, doc| {
            if let Some(v) = doc.get_path(field) {
                hll.add_bytes(&v.encode());
            }
            Ok(true)
        })?;
        Ok(hll.estimate())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, Value};

    #[test]
    fn hll_empty_is_zero() {
        assert_eq!(HyperLogLog::new().estimate(), 0);
    }

    #[test]
    fn hll_estimates_large_cardinality_within_error() {
        let mut hll = HyperLogLog::new();
        let n = 10_000u64;
        for i in 0..n {
            hll.add_bytes(format!("item-{i}").as_bytes());
        }
        let est = hll.estimate() as f64;
        let err = (est - n as f64).abs() / n as f64;
        assert!(err < 0.03, "relative error {err} too high (est {est})");
    }

    #[test]
    fn hll_ignores_duplicates() {
        let mut hll = HyperLogLog::new();
        for _ in 0..1000 {
            hll.add_bytes(b"same");
        }
        assert_eq!(hll.estimate(), 1);
    }

    #[test]
    fn hll_small_cardinality_is_accurate() {
        let mut hll = HyperLogLog::new();
        for i in 0..50 {
            hll.add_bytes(format!("v{i}").as_bytes());
        }
        let est = hll.estimate();
        assert!(
            (48..=52).contains(&est),
            "estimate {est} off for 50 distinct"
        );
    }

    #[test]
    fn hll_precision_is_clamped() {
        assert_eq!(HyperLogLog::with_precision(2).registers.len(), 1 << 4);
        assert_eq!(HyperLogLog::with_precision(99).registers.len(), 1 << 16);
    }

    #[test]
    fn bloom_has_no_false_negatives() {
        let mut bloom = BloomFilter::new(1000, 0.01);
        for i in 0..1000 {
            bloom.add_bytes(format!("k{i}").as_bytes());
        }
        for i in 0..1000 {
            assert!(bloom.contains_bytes(format!("k{i}").as_bytes()));
        }
    }

    #[test]
    fn bloom_false_positive_rate_is_bounded() {
        let mut bloom = BloomFilter::new(1000, 0.01);
        for i in 0..1000 {
            bloom.add_bytes(format!("k{i}").as_bytes());
        }
        let mut fp = 0;
        let trials = 10_000;
        for i in 0..trials {
            if bloom.contains_bytes(format!("absent-{i}").as_bytes()) {
                fp += 1;
            }
        }
        let rate = fp as f64 / trials as f64;
        // Configured for 1%; allow generous slack for the finite sample.
        assert!(rate < 0.05, "false-positive rate {rate} too high");
    }

    #[test]
    fn approx_distinct_over_a_field() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        // 10 distinct categories spread over 100 documents.
        for i in 0..100 {
            let mut m = std::collections::BTreeMap::new();
            m.insert("cat".to_owned(), Value::Text(format!("c{}", i % 10)));
            c.insert(format!("k{i}").as_bytes(), &Value::Map(m))
                .unwrap();
        }
        let est = c.approx_distinct("cat").unwrap();
        assert!(
            (9..=11).contains(&est),
            "estimate {est} off for 10 distinct"
        );
    }

    #[test]
    fn approx_distinct_ignores_missing_field() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &Value::Int(1)).unwrap(); // no "cat" field
        assert_eq!(c.approx_distinct("cat").unwrap(), 0);
    }
}
