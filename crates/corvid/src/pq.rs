//! Product Quantization (PQ): compress vectors to a few bytes via per-subspace
//! codebooks, for far smaller vector storage than scalar/binary at comparable
//! recall.
//!
//! A `dim`-vector is split into `m` contiguous subvectors of `dim/m` each. For
//! every subspace we train a codebook of `k` centroids (k-means) and encode a
//! subvector as the index of its nearest centroid — so a whole vector becomes
//! `m` bytes (with `k ≤ 256`). Distance is computed by reconstructing the
//! approximate vector from its codes and applying the configured metric (the
//! same "decode then measure" shape as scalar quantization), which keeps PQ
//! correct for every metric; an L2 asymmetric-distance table is also provided
//! as a faster path.
//!
//! Training is deterministic (fixed centroid seeding + fixed iterations) so an
//! index built from the same vectors is reproducible, and the codebook
//! persists as bytes alongside the index. Training parallelizes
//! deterministically too (roadmap Task 13): each Lloyd iteration's
//! assignment step — every point's nearest centroid, a pure function of the
//! point and the current codebook — runs as chunk-parallel batches over a
//! scoped worker team, and the update step consumes those assignments in
//! input order, exactly as the sequential loop would. Iterations stay
//! sequential (each depends on the last), so the result is the same
//! codebook bit-for-bit.

use crate::distance::Metric;
use crate::team::{Team, parallelism, with_team};
use std::sync::Arc;

/// A trained product quantizer: `m` subspace codebooks of `k` centroids each.
#[derive(Clone, Debug, PartialEq)]
pub struct Pq {
    m: usize,
    k: usize,
    sub_dim: usize,
    dim: usize,
    /// `codebooks[s]` is `k * sub_dim` flat: centroid `c`, component `d` at
    /// `c * sub_dim + d`.
    codebooks: Vec<Vec<f32>>,
}

/// Default Lloyd iterations during training.
const DEFAULT_ITERS: usize = 16;

/// Training samples below this run their k-means sequentially — the team
/// spawn would not amortize over a small sample.
const PAR_TRAIN_MIN: usize = 512;

impl Pq {
    /// Number of code bytes per vector.
    pub fn code_len(&self) -> usize {
        self.m
    }

    /// The dimensionality this quantizer was trained for.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The training parameters `(m, k)` (subspaces, centroids per subspace).
    pub fn params(&self) -> (usize, usize) {
        (self.m, self.k)
    }

    /// Train a quantizer on `sample` with `m` subspaces and `k` centroids
    /// (`2 ≤ k ≤ 256`). Returns `None` if the parameters or sample are
    /// unusable (empty sample, `dim` not divisible by `m`, ragged vectors).
    ///
    /// Large samples train with the assignment steps chunk-parallel over a
    /// scoped worker team — deterministic, see the module docs; small
    /// samples (and single-threaded machines) train sequentially. Either
    /// way the codebook is the same bits.
    pub fn train(sample: &[Vec<f32>], m: usize, k: usize) -> Option<Pq> {
        Self::train_inner(sample, m, k, true)
    }

    /// The training core; `allow_team` exists so the equivalence tests can
    /// force the sequential path against the same corpus.
    pub(crate) fn train_inner(
        sample: &[Vec<f32>],
        m: usize,
        k: usize,
        allow_team: bool,
    ) -> Option<Pq> {
        if sample.is_empty() || m == 0 || !(2..=256).contains(&k) {
            return None;
        }
        let dim = sample[0].len();
        if dim == 0 || !dim.is_multiple_of(m) || sample.iter().any(|v| v.len() != dim) {
            return None;
        }
        let sub_dim = dim / m;
        let par = allow_team && parallelism() > 1 && sample.len() >= PAR_TRAIN_MIN;
        let mut codebooks = Vec::with_capacity(m);
        if par {
            with_team(parallelism(), |team| {
                for s in 0..m {
                    let offset = s * sub_dim;
                    // Gather this subspace's points (slices into the sample).
                    let subs: Vec<&[f32]> = sample
                        .iter()
                        .map(|v| &v[offset..offset + sub_dim])
                        .collect();
                    codebooks.push(kmeans(&subs, k, sub_dim, Some(team)));
                }
            });
        } else {
            for s in 0..m {
                let offset = s * sub_dim;
                // Gather this subspace's points (slices into the sample).
                let subs: Vec<&[f32]> = sample
                    .iter()
                    .map(|v| &v[offset..offset + sub_dim])
                    .collect();
                codebooks.push(kmeans(&subs, k, sub_dim, None));
            }
        }
        Some(Pq {
            m,
            k,
            sub_dim,
            dim,
            codebooks,
        })
    }

    /// Encode a vector to `m` centroid-index bytes. A vector of the wrong
    /// dimension yields an all-zero code (it cannot be matched meaningfully).
    pub fn encode(&self, v: &[f32]) -> Vec<u8> {
        if v.len() != self.dim {
            return vec![0u8; self.m];
        }
        let mut code = Vec::with_capacity(self.m);
        for s in 0..self.m {
            let off = s * self.sub_dim;
            let sub = &v[off..off + self.sub_dim];
            code.push(nearest_centroid(sub, &self.codebooks[s], self.sub_dim) as u8);
        }
        code
    }

    /// Reconstruct the approximate vector from its codes.
    pub fn decode(&self, code: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.dim);
        for (s, &c) in code.iter().enumerate().take(self.m) {
            let cb = &self.codebooks[s];
            let start = (c as usize) * self.sub_dim;
            // Guard a malformed code byte (centroid index out of range).
            if start + self.sub_dim <= cb.len() {
                out.extend_from_slice(&cb[start..start + self.sub_dim]);
            } else {
                out.extend(std::iter::repeat_n(0.0, self.sub_dim));
            }
        }
        out
    }

    /// Distance from `query` to a PQ-coded vector under `metric`, via
    /// reconstruction. Correct for any metric.
    pub fn distance(&self, metric: Metric, query: &[f32], code: &[u8]) -> f32 {
        metric.distance(query, &self.decode(code))
    }

    /// Precompute the L2 asymmetric-distance table for `query`:
    /// `table[s * k + c]` is the squared-L2 distance from `query`'s subvector
    /// `s` to centroid `c`. Use with [`Pq::adc_l2`] for fast L2 scoring.
    /// Returns `None` when the query dimension does not match the codebook —
    /// a mismatched query has no meaningful table, and an all-zero one would
    /// score every node at distance 0.
    pub fn l2_table(&self, query: &[f32]) -> Option<Vec<f32>> {
        if query.len() != self.dim {
            return None;
        }
        let mut table = vec![0.0f32; self.m * self.k];
        for s in 0..self.m {
            let off = s * self.sub_dim;
            let qsub = &query[off..off + self.sub_dim];
            let cb = &self.codebooks[s];
            for c in 0..self.k {
                let cs = c * self.sub_dim;
                let mut d = 0.0f32;
                for j in 0..self.sub_dim {
                    let diff = qsub[j] - cb[cs + j];
                    d += diff * diff;
                }
                table[s * self.k + c] = d;
            }
        }
        Some(table)
    }

    /// Squared-L2 distance from a coded vector to the query whose `table`
    /// was built with [`Pq::l2_table`] — a sum of `m` table lookups. A code
    /// byte outside `[0, k)` (corrupt or foreign-format state) scores
    /// `INFINITY`: the node never ranks and nothing panics.
    pub fn adc_l2(&self, table: &[f32], code: &[u8]) -> f32 {
        let mut sum = 0.0f32;
        for (s, &c) in code.iter().enumerate().take(self.m) {
            let c = c as usize;
            if c >= self.k {
                return f32::INFINITY;
            }
            sum += table[s * self.k + c];
        }
        sum
    }

    /// Serialize the codebook for on-disk storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for v in [self.m, self.k, self.sub_dim, self.dim] {
            out.extend_from_slice(&(v as u32).to_le_bytes());
        }
        for cb in &self.codebooks {
            for &x in cb {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        out
    }

    /// Reconstruct a quantizer from [`Pq::to_bytes`] output.
    pub fn from_bytes(b: &[u8]) -> Option<Pq> {
        if b.len() < 16 {
            return None;
        }
        let rd = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        let (m, k, sub_dim, dim) = (rd(0), rd(4), rd(8), rd(12));
        if m == 0 || k == 0 || sub_dim == 0 || dim != m * sub_dim {
            return None;
        }
        let mut pos = 16;
        let mut codebooks = Vec::with_capacity(m);
        let per_cb = k * sub_dim;
        for _ in 0..m {
            let mut cb = Vec::with_capacity(per_cb);
            for _ in 0..per_cb {
                let bytes = b.get(pos..pos + 4)?;
                cb.push(f32::from_le_bytes(bytes.try_into().ok()?));
                pos += 4;
            }
            codebooks.push(cb);
        }
        Some(Pq {
            m,
            k,
            sub_dim,
            dim,
            codebooks,
        })
    }
}

/// Squared L2 between two equal-length slices.
fn dist_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Index of the nearest centroid (codebook is `k * sub_dim` flat).
fn nearest_centroid(sub: &[f32], codebook: &[f32], sub_dim: usize) -> usize {
    let k = codebook.len() / sub_dim;
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for c in 0..k {
        let cs = c * sub_dim;
        let d = dist_sq(sub, &codebook[cs..cs + sub_dim]);
        if d < best_d {
            best_d = d;
            best = c;
        }
    }
    best
}

/// Deterministic k-means over `sub_dim`-vectors → `k` centroids (flat
/// `k * sub_dim`). Centroids are seeded by spreading across the input and
/// refined with a fixed number of Lloyd iterations; empty clusters keep their
/// previous centroid.
///
/// With a team, each iteration's assignment step runs chunk-parallel: a
/// point's nearest centroid is a pure function of the point and the current
/// codebook, so `Team::map`'s item-indexed outputs are exactly the
/// sequential loop's assignments. The update step (and the change
/// detection) consumes them in input order, sequentially — same sums, same
/// f64-free f32 accumulation order, same codebook bits.
fn kmeans(points: &[&[f32]], k: usize, sub_dim: usize, mut team: Option<&mut Team>) -> Vec<f32> {
    let n = points.len();
    let mut centroids = vec![0.0f32; k * sub_dim];
    // Seed: spread k picks across the (input-ordered) points. Fewer points than
    // k → reuse by modulo, which is deterministic and harmless.
    for c in 0..k {
        let idx = if n == 0 {
            0
        } else {
            (c * n / k.max(1)) % n.max(1)
        };
        if n > 0 {
            centroids[c * sub_dim..(c + 1) * sub_dim].copy_from_slice(points[idx]);
        }
    }
    if n == 0 {
        return centroids;
    }

    // The parallel assignment path needs owned point data (the team's job
    // closures must be self-owned, so they carry copies — cloned once per
    // subspace and shared by reference across the iterations).
    let owned: Option<Arc<Vec<Vec<f32>>>> = match team.as_deref_mut() {
        Some(t) if t.workers() > 0 && n >= t.min_items() => {
            Some(Arc::new(points.iter().map(|p| p.to_vec()).collect()))
        }
        _ => None,
    };

    let mut assign = vec![0usize; n];
    for _ in 0..DEFAULT_ITERS {
        // Assignment step (pure per point — parallel with a team, else the
        // plain sequential map; identical values either way).
        let next: Vec<usize> = match (&owned, team.as_deref_mut()) {
            (Some(pts), Some(t)) => {
                let cbs = centroids.clone();
                let pts = Arc::clone(pts);
                crate::team::map_owned(t, n, move |i| nearest_centroid(&pts[i], &cbs, sub_dim))
            }
            _ => points
                .iter()
                .map(|p| nearest_centroid(p, &centroids, sub_dim))
                .collect(),
        };
        let mut changed = false;
        for (i, &a) in next.iter().enumerate() {
            if a != assign[i] {
                assign[i] = a;
                changed = true;
            }
        }
        // Update step: mean of each cluster (empty clusters keep their value).
        let mut sums = vec![0.0f32; k * sub_dim];
        let mut counts = vec![0usize; k];
        for (i, p) in points.iter().enumerate() {
            let a = assign[i];
            counts[a] += 1;
            let base = a * sub_dim;
            for j in 0..sub_dim {
                sums[base + j] += p[j];
            }
        }
        for (c, &count) in counts.iter().enumerate() {
            if count > 0 {
                let base = c * sub_dim;
                for j in 0..sub_dim {
                    centroids[base + j] = sums[base + j] / count as f32;
                }
            }
        }
        if !changed {
            break;
        }
    }
    centroids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random vectors in clusters, so PQ has structure to
    /// learn (uniform noise compresses poorly for any quantizer).
    fn clustered(n: usize, dim: usize, clusters: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
        // Cluster centers.
        let centers: Vec<Vec<f32>> = (0..clusters)
            .map(|_| (0..dim).map(|_| next() * 10.0).collect())
            .collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % clusters];
                c.iter().map(|&x| x + (next() - 0.5)).collect()
            })
            .collect()
    }

    #[test]
    fn train_rejects_bad_params() {
        let data = clustered(10, 8, 3);
        assert!(Pq::train(&data, 3, 16).is_none()); // 8 not divisible by 3
        assert!(Pq::train(&data, 2, 1).is_none()); // k too small
        assert!(Pq::train(&[], 2, 16).is_none()); // empty
    }

    #[test]
    fn encode_is_deterministic_and_compact() {
        let data = clustered(200, 8, 6);
        let pq = Pq::train(&data, 4, 16).unwrap();
        assert_eq!(pq.code_len(), 4);
        let a = pq.encode(&data[0]);
        let b = pq.encode(&data[0]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 4); // 8-dim vector -> 4 bytes
    }

    #[test]
    fn training_is_reproducible() {
        let data = clustered(150, 16, 5);
        let p1 = Pq::train(&data, 4, 16).unwrap();
        let p2 = Pq::train(&data, 4, 16).unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn decode_approximates_original_on_clustered_data() {
        let data = clustered(300, 8, 8);
        let pq = Pq::train(&data, 4, 32).unwrap();
        let mut err = 0.0f32;
        for v in &data {
            err += dist_sq(v, &pq.decode(&pq.encode(v))).sqrt();
        }
        let avg = err / data.len() as f32;
        // Reconstruction error is small relative to the cluster spread (~10).
        assert!(avg < 2.0, "avg reconstruction error {avg} too high");
    }

    #[test]
    fn adc_l2_matches_reconstruction_distance() {
        let data = clustered(100, 8, 5);
        let pq = Pq::train(&data, 4, 16).unwrap();
        let q = &data[10];
        let table = pq.l2_table(q).unwrap();
        for v in data.iter().take(20) {
            let code = pq.encode(v);
            let adc = pq.adc_l2(&table, &code);
            let recon = pq.distance(Metric::L2, q, &code);
            // Metric::L2 is squared Euclidean, and ADC sums squared sub-
            // distances over the same centroids, so they are the same quantity.
            assert!((adc - recon).abs() < 1e-2, "adc {adc} vs recon {recon}");
        }
    }

    #[test]
    fn pq_ranking_recall_vs_exact() {
        let data = clustered(500, 16, 12);
        let pq = Pq::train(&data, 8, 32).unwrap();
        let codes: Vec<Vec<u8>> = data.iter().map(|v| pq.encode(v)).collect();
        let k = 10;
        let mut total = 0.0;
        let queries = clustered(20, 16, 12);
        for q in &queries {
            // Exact top-k by L2.
            let mut exact: Vec<(usize, f32)> = data
                .iter()
                .enumerate()
                .map(|(i, v)| (i, dist_sq(q, v)))
                .collect();
            exact.sort_by(|a, b| a.1.total_cmp(&b.1));
            let want: std::collections::HashSet<usize> =
                exact.iter().take(k).map(|(i, _)| *i).collect();
            // PQ top-k via ADC.
            let table = pq.l2_table(q).unwrap();
            let mut approx: Vec<(usize, f32)> = codes
                .iter()
                .enumerate()
                .map(|(i, c)| (i, pq.adc_l2(&table, c)))
                .collect();
            approx.sort_by(|a, b| a.1.total_cmp(&b.1));
            let got: std::collections::HashSet<usize> =
                approx.iter().take(k).map(|(i, _)| *i).collect();
            total += got.intersection(&want).count() as f64 / k as f64;
        }
        let recall = total / queries.len() as f64;
        // 8 bytes for a 16-dim vector (8x smaller than f32) recovers most true
        // neighbours; exact thresholds depend on params, so assert a solid
        // majority rather than a brittle high bar.
        assert!(recall >= 0.6, "PQ recall {recall} too low");
    }

    /// Regression (audit A4): a code byte outside [0, k) used to index the ADC
    /// table out of bounds (panic from the query path). Such a node must score
    /// INFINITY — never rank, never panic.
    #[test]
    fn adc_l2_rejects_out_of_range_codes() {
        let data = clustered(100, 8, 5);
        let pq = Pq::train(&data, 4, 16).unwrap();
        let table = pq.l2_table(&data[0]).unwrap();
        assert_eq!(pq.adc_l2(&table, &[255, 255, 255, 255]), f32::INFINITY);
        // In-range codes still score finitely.
        let code = pq.encode(&data[1]);
        assert!(pq.adc_l2(&table, &code).is_finite());
    }

    /// Regression (audit A4): a dimension-mismatched query used to get an
    /// all-zero table (every node distance 0). It must get `None` so callers
    /// can decline to serve or fall back.
    #[test]
    fn l2_table_returns_none_on_dimension_mismatch() {
        let data = clustered(50, 8, 4);
        let pq = Pq::train(&data, 4, 16).unwrap();
        let wrong: Vec<f32> = (0..pq.dim() + 1).map(|i| i as f32).collect();
        assert!(pq.l2_table(&wrong).is_none());
        assert!(pq.l2_table(&data[0]).is_some());
    }

    #[test]
    fn codebook_round_trips_through_bytes() {
        let data = clustered(120, 8, 4);
        let pq = Pq::train(&data, 4, 16).unwrap();
        let bytes = pq.to_bytes();
        let back = Pq::from_bytes(&bytes).unwrap();
        assert_eq!(pq, back);
        // And it still encodes identically.
        assert_eq!(pq.encode(&data[3]), back.encode(&data[3]));
    }

    #[test]
    fn from_bytes_rejects_garbage() {
        assert!(Pq::from_bytes(&[1, 2, 3]).is_none());
    }

    // ---- deterministic parallel training (Task 13) ----

    /// Parallel training yields the identical codebook to the sequential
    /// path on the same corpus (large enough to engage the team), and is
    /// reproducible run over run.
    #[test]
    fn parallel_training_codebook_is_bit_identical_to_sequential() {
        let data = clustered(900, 32, 8);
        let seq = Pq::train_inner(&data, 8, 32, false).unwrap();
        let par = Pq::train(&data, 8, 32).unwrap();
        let par2 = Pq::train(&data, 8, 32).unwrap();
        assert_eq!(seq, par);
        assert_eq!(par, par2);
        // Identical codebook → identical encodings.
        for v in data.iter().step_by(97) {
            assert_eq!(seq.encode(v), par.encode(v));
        }
    }
}
