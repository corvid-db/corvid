//! Cuckoo filter: approximate set membership with deletion.
//!
//! Like [`BloomFilter`](super::BloomFilter) there are no false negatives for
//! admitted items, but the bucketed fingerprint layout also supports
//! `delete_bytes` — the differentiator — and stays at a bounded false
//! positive rate as it fills. Deterministic throughout: fingerprints and
//! bucket indices derive from the shared `DefaultHasher` hash and the
//! displacement victim is a fixed rotation, never a random choice.

use super::hash64;

/// Slots per bucket (the classic b = 4 of Fan et al., *Cuckoo Filter:
/// Practically Better Than Bloom*, 2014).
const SLOTS: usize = 4;
/// Displacement attempts before an insert gives up (the paper's 500).
const MAX_KICKS: usize = 500;

/// A cuckoo filter for approximate set membership (no false negatives for
/// admitted items, unlike Bloom it supports deletion).
///
/// False positives are possible — bounded by the configured rate — and stem
/// from fingerprint aliasing: two distinct items whose fingerprints collide
/// are indistinguishable. False negatives NEVER occur for an item
/// [`add_bytes`](CuckooFilter::add_bytes) accepted, with one documented
/// caveat: `delete_bytes` of an item that was never added but aliases an
/// added one removes that slot, so only delete items you actually inserted.
pub struct CuckooFilter {
    /// Fingerprint slots; 0 marks an empty slot, fingerprints are nonzero.
    buckets: Vec<[u32; SLOTS]>,
    /// Bucket count — a power of two, so XOR-remapping stays in range.
    num_buckets: usize,
    /// The fingerprint mask `(1 << bits) - 1`.
    fp_mask: u32,
}

impl CuckooFilter {
    /// A filter sized for `expected_items` at the target `fp_rate` in
    /// `(0, 1)` (clamped, as Bloom does, not rejected).
    ///
    /// The fingerprint width follows the paper's bound for two-bucket
    /// lookup, ε ≈ 2·b/2^f with b = 4 slots, and the bucket count targets a
    /// 90% load factor (the b = 4 layout tops out around 95.6%), rounded up
    /// to a power of two.
    pub fn new(expected_items: usize, fp_rate: f64) -> Self {
        let n = expected_items.max(1) as f64;
        let p = fp_rate.clamp(f64::MIN_POSITIVE, 0.5);
        // f = ceil(log2(2b/p)); a u32 slot bounds it to 32 bits.
        let bits = ((2.0 * SLOTS as f64 / p).log2().ceil() as u32).clamp(1, 32);
        let fp_mask = if bits >= 32 {
            u32::MAX
        } else {
            (1u32 << bits) - 1
        };
        let need = (n / (SLOTS as f64 * 0.90)).ceil() as usize;
        let num_buckets = need.next_power_of_two().max(2);
        Self {
            buckets: vec![[0; SLOTS]; num_buckets],
            num_buckets,
            fp_mask,
        }
    }

    /// Insert an item. Returns `true` when the item is in the filter
    /// afterwards (inserted, or already present under an identical
    /// fingerprint), `false` when the table is exhausted.
    ///
    /// Victim-slot semantics when displacement fails: the whole swap chain
    /// is rolled back — a rejected add leaves the filter byte-identical to
    /// its state before the call and the item is NOT contained — so every
    /// previously admitted item survives. (The paper instead drops the last
    /// evicted fingerprint, silently losing one admitted item per failure;
    /// rolling back is strictly safer.)
    pub fn add_bytes(&mut self, bytes: &[u8]) -> bool {
        let (i1, i2, fp) = self.derive(bytes);
        // Duplicate (or aliased twin): already represented, nothing to do.
        if self.buckets[i1].contains(&fp) || self.buckets[i2].contains(&fp) {
            return true;
        }
        if let Some(slot) = self.buckets[i1].iter().position(|&s| s == 0) {
            self.buckets[i1][slot] = fp;
            return true;
        }
        if let Some(slot) = self.buckets[i2].iter().position(|&s| s == 0) {
            self.buckets[i2][slot] = fp;
            return true;
        }
        // Displace: evict a rotating victim slot and chase the victim to its
        // alternate bucket. Roll the whole chain back on exhaustion.
        let mut carried = fp;
        let mut idx = i1;
        let mut trail: Vec<(usize, usize)> = Vec::with_capacity(MAX_KICKS);
        for kick in 0..MAX_KICKS {
            let slot = kick % SLOTS;
            std::mem::swap(&mut self.buckets[idx][slot], &mut carried);
            trail.push((idx, slot));
            let next = idx ^ self.alt_index(carried);
            if let Some(free) = self.buckets[next].iter().position(|&s| s == 0) {
                self.buckets[next][free] = carried;
                return true;
            }
            idx = next;
        }
        // Exhausted: undo every swap, dropping the item being inserted.
        for (bucket, slot) in trail.iter().rev() {
            std::mem::swap(&mut self.buckets[*bucket][*slot], &mut carried);
        }
        false
    }

    /// Test membership. `false` is definitive for items whose adds were
    /// accepted (and not deleted); `true` may be a false positive.
    pub fn contains_bytes(&self, bytes: &[u8]) -> bool {
        let (i1, i2, fp) = self.derive(bytes);
        self.buckets[i1].contains(&fp) || self.buckets[i2].contains(&fp)
    }

    /// Remove an item. Returns `true` when a fingerprint slot was removed,
    /// `false` when the item was not present.
    ///
    /// Because of fingerprint aliasing this can remove a slot installed by
    /// a *different* item with the same fingerprint and bucket pair — the
    /// documented route to a false negative. Only delete items that were
    /// actually added.
    pub fn delete_bytes(&mut self, bytes: &[u8]) -> bool {
        let (i1, i2, fp) = self.derive(bytes);
        for idx in [i1, i2] {
            if let Some(slot) = self.buckets[idx].iter().position(|&s| s == fp) {
                self.buckets[idx][slot] = 0;
                return true;
            }
        }
        false
    }

    /// The item's fingerprint and its two candidate buckets. Bucket two is
    /// bucket one XOR-remapped through the fingerprint (partial-key
    /// cuckoo hashing), so a displaced fingerprint can always find its
    /// alternate from the fingerprint alone.
    fn derive(&self, bytes: &[u8]) -> (usize, usize, u32) {
        let h = hash64(bytes);
        let fp = self.fingerprint(h);
        let i1 = ((h >> 32) as usize) & (self.num_buckets - 1);
        let i2 = i1 ^ self.alt_index(fp);
        (i1, i2, fp)
    }

    /// Fingerprint of a hash: the low bits, forced nonzero so 0 can mark
    /// empty slots.
    fn fingerprint(&self, h: u64) -> u32 {
        let fp = (h as u32) & self.fp_mask;
        if fp == 0 { 1 } else { fp }
    }

    /// The XOR offset into bucket space for a fingerprint.
    fn alt_index(&self, fp: u32) -> usize {
        (hash64(&fp.to_le_bytes()) as usize) & (self.num_buckets - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuckoo_no_false_negatives_and_delete_works() {
        let mut f = CuckooFilter::new(1000, 0.01);
        for i in 0..1000 {
            assert!(f.add_bytes(format!("k{i}").as_bytes()));
        }
        for i in 0..1000 {
            assert!(f.contains_bytes(format!("k{i}").as_bytes()));
        }
        assert!(f.delete_bytes(b"k0"));
        assert!(!f.contains_bytes(b"k0"));
        assert!(f.contains_bytes(b"k1"));
        assert!(!f.delete_bytes(b"k0"), "second delete finds nothing");
    }

    #[test]
    fn cuckoo_overflow_rejects_without_losing_admitted_items() {
        // 4 buckets x 4 slots = 16 slots; well past that, adds must fail.
        let mut f = CuckooFilter::new(10, 0.01);
        let mut admitted: Vec<Vec<u8>> = Vec::new();
        let mut rejected = 0;
        for i in 0..500 {
            let bytes = format!("x{i}").into_bytes();
            if f.add_bytes(&bytes) {
                admitted.push(bytes);
            } else {
                rejected += 1;
                // The rejected item is not contained…
                assert!(!f.contains_bytes(&bytes));
                // …and the rollback kept every admitted item.
                for a in &admitted {
                    assert!(
                        f.contains_bytes(a),
                        "admitted item lost after a rejected add"
                    );
                }
            }
        }
        assert!(rejected > 0, "500 items into 16 slots must overflow");
        assert!(
            admitted.len() >= 12,
            "16 slots should admit most items, got {}",
            admitted.len()
        );
    }
}
