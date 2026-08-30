//! t-digest: approximate quantiles and CDF over a stream of `f64`
//! observations (Dunning et al., *Computing Extremely Accurate Quantiles
//! Using t-Digests*).
//!
//! A merging t-digest: observations accumulate in a small buffer, then are
//! folded into a sorted list of weighted centroids whose scale-function
//! widths stay under `1/compression` (the k1 scale, which spends resolution
//! at the tails). Deterministic given a fixed sequence of `add`/`merge`
//! calls: sorting is a total order on means and every arithmetic step is
//! plain f64 — no sampling, no floating-point randomness. Queries are pure,
//! so an answer depends only on the add/merge history, never on how many
//! queries ran before it.

use std::f64::consts::PI;

/// One weighted centroid: a mean and the number of observations at it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Centroid {
    mean: f64,
    count: u64,
}

/// A t-digest estimates quantiles ([`TDigest::quantile`]) and cumulative
/// probabilities ([`TDigest::cdf`]) over any number of observed `f64`
/// values using memory bounded by the compression parameter. `Clone`-able,
/// so a digest can be kept and merged into others non-destructively.
#[derive(Clone)]
pub struct TDigest {
    /// Compressed centroids, sorted by mean.
    centroids: Vec<Centroid>,
    /// Fresh observations waiting for the next flush.
    buffer: Vec<Centroid>,
    /// The compression factor δ (larger = more centroids = more accurate).
    compression: f64,
    /// Flush the buffer once it holds this many observations.
    buffer_cap: usize,
}

impl TDigest {
    /// An empty digest with compression `δ` clamped to `>= 1` (NaN,
    /// negatives, and zero become 1; `+inf` is allowed and keeps every
    /// distinct value — an exact mode).
    ///
    /// Accuracy scales as roughly `1/δ` in quantile space around the middle
    /// of the distribution; `δ = 100` is a common default.
    pub fn new(compression: f64) -> Self {
        let compression = if compression.is_nan() {
            1.0
        } else {
            compression.max(1.0)
        };
        // The buffer only paces flushes; cap it so pathological δ values
        // cannot size an unbounded Vec up front.
        let buffer_cap = (compression.min(65_536.0).ceil() as usize).max(10);
        Self {
            centroids: Vec::new(),
            buffer: Vec::new(),
            compression,
            buffer_cap,
        }
    }

    /// Observe a value. Non-finite values (NaN and ±infinity) are rejected:
    /// the observation is dropped and the digest is left exactly as it was.
    pub fn add(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        self.buffer.push(Centroid { mean: x, count: 1 });
        if self.buffer.len() >= self.buffer_cap {
            self.flush();
        }
    }

    /// Fold another digest into this one. Both sides' pending buffers are
    /// included (a buffered observation is just a weight-1 centroid).
    pub fn merge(&mut self, other: &TDigest) {
        self.flush();
        self.centroids.extend_from_slice(&other.centroids);
        self.centroids.extend_from_slice(&other.buffer);
        self.centroids.sort_by(|a, b| a.mean.total_cmp(&b.mean));
        self.compress();
    }

    /// The q-quantile of the observations, or `None` when nothing was
    /// observed or `q` is outside `[0, 1]` (NaN included).
    /// `quantile(0.0)` is the minimum and `quantile(1.0)` the maximum,
    /// exactly.
    ///
    /// Values are interpolated linearly between centroid centers; flat
    /// outside them, which is what makes the 0.0/1.0 boundaries exact.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if !(0.0..=1.0).contains(&q) {
            return None;
        }
        let centroids = self.sorted_view();
        if centroids.is_empty() {
            return None;
        }
        let centers = centroid_centers(&centroids);
        let target = q * total_count(&centroids) as f64;
        let last = centroids.len() - 1;
        if target <= centers[0] {
            return Some(centroids[0].mean);
        }
        if target >= centers[last] {
            return Some(centroids[last].mean);
        }
        for i in 0..last {
            if target <= centers[i + 1] {
                let t = (target - centers[i]) / (centers[i + 1] - centers[i]);
                let m0 = centroids[i].mean;
                let m1 = centroids[i + 1].mean;
                return Some(m0 + t * (m1 - m0));
            }
        }
        Some(centroids[last].mean)
    }

    /// The fraction of observations `<= x`, interpolated on the same
    /// centroid-center curve as [`TDigest::quantile`]: 0.0 below the
    /// smallest mean, 1.0 at or above the largest. Monotonically
    /// non-decreasing. An empty digest returns 0.0.
    pub fn cdf(&self, x: f64) -> f64 {
        let centroids = self.sorted_view();
        if centroids.is_empty() {
            return 0.0;
        }
        let last = centroids.len() - 1;
        if x < centroids[0].mean {
            return 0.0;
        }
        if x >= centroids[last].mean {
            return 1.0;
        }
        // Invariant: x >= centroids[i].mean when segment i is reached, so
        // x < centroids[i+1].mean implies a strictly positive denominator.
        let centers = centroid_centers(&centroids);
        let n = total_count(&centroids) as f64;
        for i in 0..last {
            let m1 = centroids[i + 1].mean;
            if x < m1 {
                let t = (x - centroids[i].mean) / (m1 - centroids[i].mean);
                let rank = centers[i] + t * (centers[i + 1] - centers[i]);
                return rank / n;
            }
        }
        1.0
    }

    /// Sort centroids plus pending buffer by mean — the pure read view the
    /// queries interpolate over.
    fn sorted_view(&self) -> Vec<Centroid> {
        let mut all: Vec<Centroid> = self
            .centroids
            .iter()
            .chain(self.buffer.iter())
            .copied()
            .collect();
        all.sort_by(|a, b| a.mean.total_cmp(&b.mean));
        all
    }

    /// Fold the buffer into the centroid list and compress.
    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        self.centroids.append(&mut self.buffer);
        self.centroids.sort_by(|a, b| a.mean.total_cmp(&b.mean));
        self.compress();
    }

    /// The merging step: walk the sorted centroids and absorb each into the
    /// previous output centroid while the k1-scale width of the merged
    /// span stays within `1/δ`.
    fn compress(&mut self) {
        let n = total_count(&self.centroids);
        if n == 0 {
            return;
        }
        let nf = n as f64;
        let limit = 1.0 / self.compression;
        // k1 scale: maps [0, 1] onto itself, stretching both tails.
        let k1 = |q: f64| ((2.0 * q - 1.0).asin() + PI / 2.0) / PI;

        let mut out: Vec<Centroid> = Vec::with_capacity(self.centroids.len());
        let mut completed = 0u64; // count held by fully-emitted centroids
        for &c in &self.centroids {
            match out.last_mut() {
                None => out.push(c),
                Some(last) => {
                    // Equal means always merge: two centroids at the same
                    // value are the same point in value space, so joining
                    // them is lossless however tail-stretched the scale is.
                    let merged_end = completed + last.count + c.count;
                    let width = k1(merged_end as f64 / nf) - k1(completed as f64 / nf);
                    if last.mean == c.mean || width <= limit {
                        let total = last.count + c.count;
                        last.mean = (last.mean * last.count as f64 + c.mean * c.count as f64)
                            / total as f64;
                        last.count = total;
                    } else {
                        completed += last.count;
                        out.push(c);
                    }
                }
            }
        }
        self.centroids = out;
    }
}

/// Total weight of a centroid list.
fn total_count(centroids: &[Centroid]) -> u64 {
    centroids.iter().map(|c| c.count).sum()
}

/// The rank-space center of each centroid: the count strictly below it plus
/// half its own count (strictly increasing, so interpolation denominators
/// are positive).
fn centroid_centers(centroids: &[Centroid]) -> Vec<f64> {
    let mut centers = Vec::with_capacity(centroids.len());
    let mut below = 0u64;
    for c in centroids {
        centers.push(below as f64 + c.count as f64 / 2.0);
        below += c.count;
    }
    centers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tdigest_small_corpora_are_exact() {
        // Distinct evenly spaced values stay unmerged at δ = 10 (the k1
        // width of a quarter of the mass exceeds 1/10), so quartiles are
        // the exact order statistics.
        let mut d = TDigest::new(10.0);
        for x in [1.0, 2.0, 3.0, 4.0] {
            d.add(x);
        }
        d.flush();
        assert_eq!(d.quantile(0.0), Some(1.0));
        assert_eq!(d.quantile(0.25), Some(1.5));
        assert_eq!(d.quantile(0.5), Some(2.5));
        assert_eq!(d.quantile(0.75), Some(3.5));
        assert_eq!(d.quantile(1.0), Some(4.0));

        // Duplicates collapse to one weighted centroid (mean 5, weight 3).
        let mut d = TDigest::new(10.0);
        for _ in 0..3 {
            d.add(5.0);
        }
        d.flush();
        assert_eq!(
            d.centroids,
            vec![Centroid {
                mean: 5.0,
                count: 3
            }]
        );
        assert_eq!(d.quantile(0.0), Some(5.0));
        assert_eq!(d.quantile(0.5), Some(5.0));
        assert_eq!(d.quantile(1.0), Some(5.0));
        assert_eq!(d.cdf(4.999), 0.0);
        assert_eq!(d.cdf(5.0), 1.0);
    }

    #[test]
    fn tdigest_rejects_nan_and_out_of_range_quantiles() {
        let mut d = TDigest::new(10.0);
        d.add(1.0);
        d.add(f64::NAN);
        d.add(f64::INFINITY);
        d.add(f64::NEG_INFINITY);
        assert_eq!(
            d.quantile(0.5),
            Some(1.0),
            "non-finite adds must be dropped"
        );
        assert_eq!(d.quantile(-0.1), None);
        assert_eq!(d.quantile(1.1), None);
        assert_eq!(d.quantile(f64::NAN), None);
        assert_eq!(TDigest::new(10.0).quantile(0.5), None);
        assert_eq!(TDigest::new(10.0).cdf(0.0), 0.0);
    }

    #[test]
    fn tdigest_cdf_is_monotone() {
        let mut d = TDigest::new(4.0);
        for i in 0..200 {
            d.add(i as f64 % 17.0);
        }
        let mut prev = 0.0;
        let mut x = -1.0;
        while x < 20.0 {
            let c = d.cdf(x);
            assert!(c >= prev, "cdf decreased at {x}");
            assert!((0.0..=1.0).contains(&c));
            prev = c;
            x += 0.5;
        }
    }
}
