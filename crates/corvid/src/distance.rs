//! Vector distance metrics.
//!
//! The kernels are written as straight fold loops over slices, which the
//! compiler auto-vectorizes well (4-wide NEON, release-assembly-verified).
//! Explicit SIMD was measured and DECLINED (2026-08-30): every faster shape
//! reassociates `f32` summation, which the bit-exact results here are pinned
//! against — the measurements and ceilings live in docs/BENCHES.md.
//!
//! Every metric is expressed so that **lower means nearer**, which lets the
//! search layer treat all metrics uniformly (sort ascending, keep smallest).

/// A similarity/distance metric over embedding vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Cosine distance, `1 - cosine_similarity`, in `[0, 2]`. A zero-norm
    /// vector has undefined direction and is treated as maximally distant.
    Cosine,
    /// Negated dot product, so larger dot products sort first.
    Dot,
    /// Squared Euclidean distance (monotonic with L2, avoids the sqrt).
    L2,
}

impl Metric {
    /// Distance between two equal-length vectors under this metric.
    /// Lower is nearer.
    ///
    /// # Panics
    /// Panics in debug builds if the lengths differ. The search layer
    /// validates dimensions before calling, so this guards an internal
    /// invariant rather than user input.
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len(), "distance on mismatched dimensions");
        match self {
            Metric::Cosine => cosine_distance(a, b),
            Metric::Dot => -dot(a, b),
            Metric::L2 => l2_squared(a, b),
        }
    }
}

/// SIMD width the kernels accumulate over. Eight independent `f32` lanes give
/// LLVM the freedom to vectorize (a single `sum()` can't be reordered because
/// `f32` addition isn't associative); the partial sums are combined at the end.
const LANES: usize = 8;

/// Dot product of two equal-length vectors.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    let (ca, ra) = a.as_chunks::<LANES>();
    let (cb, rb) = b.as_chunks::<LANES>();
    for (x, y) in ca.iter().zip(cb) {
        // `x`/`y` are exactly LANES long, so the bounds checks elide and the
        // loop vectorizes into a multiply-accumulate over the lanes.
        for j in 0..LANES {
            acc[j] += x[j] * y[j];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        sum += x * y;
    }
    sum
}

/// Squared Euclidean distance.
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    let (ca, ra) = a.as_chunks::<LANES>();
    let (cb, rb) = b.as_chunks::<LANES>();
    for (x, y) in ca.iter().zip(cb) {
        for j in 0..LANES {
            let d = x[j] - y[j];
            acc[j] += d * d;
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        let d = x - y;
        sum += d * d;
    }
    sum
}

/// Cosine distance, `1 - cos_sim`. Returns the maximum distance (`1.0`) when
/// either vector has zero magnitude.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot_ab = dot(a, b);
    let norm_a = dot(a, a).sqrt();
    let norm_b = dot(b, b).sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 1.0;
    }
    1.0 - dot_ab / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn dot_product_is_correct() {
        assert!(close(dot(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]), 32.0));
        assert!(close(dot(&[], &[]), 0.0));
    }

    #[test]
    fn l2_squared_is_correct() {
        assert!(close(l2_squared(&[0.0, 0.0], &[3.0, 4.0]), 25.0));
        assert!(close(l2_squared(&[1.0, 1.0], &[1.0, 1.0]), 0.0));
    }

    #[test]
    fn cosine_distance_known_cases() {
        // identical direction -> 0
        assert!(close(cosine_distance(&[1.0, 0.0], &[2.0, 0.0]), 0.0));
        // orthogonal -> 1
        assert!(close(cosine_distance(&[1.0, 0.0], &[0.0, 1.0]), 1.0));
        // opposite -> 2
        assert!(close(cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]), 2.0));
    }

    #[test]
    fn cosine_distance_zero_norm_is_max() {
        assert!(close(cosine_distance(&[0.0, 0.0], &[1.0, 1.0]), 1.0));
        assert!(close(cosine_distance(&[1.0, 1.0], &[0.0, 0.0]), 1.0));
    }

    #[test]
    fn metric_dispatch_lower_is_nearer() {
        let q = [1.0, 0.0];
        let near = [1.0, 0.1];
        let far = [-1.0, 0.0];

        for m in [Metric::Cosine, Metric::Dot, Metric::L2] {
            assert!(
                m.distance(&q, &near) < m.distance(&q, &far),
                "metric {m:?} should rank the near vector first"
            );
        }
    }

    #[test]
    fn dot_metric_is_negated() {
        assert!(close(
            Metric::Dot.distance(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]),
            -32.0
        ));
    }

    #[test]
    fn l2_metric_matches_squared() {
        assert!(close(Metric::L2.distance(&[0.0, 0.0], &[3.0, 4.0]), 25.0));
    }
}
