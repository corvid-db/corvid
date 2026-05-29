//! In-memory HNSW approximate nearest-neighbour index.
//!
//! Implements Malkov & Yashunin's Hierarchical Navigable Small World graph.
//! This is the approximate counterpart to the exact KNN baseline in
//! [`crate::query`]; correctness is pinned by recall tests against that
//! baseline. Level assignment uses a seeded xorshift PRNG so an index built
//! from the same inserts is byte-for-byte reproducible — important for stable
//! tests and, later, for persisting the graph as redb entries.
//!
//! This is the index data structure only; wiring it into the collection's
//! transactional write path (the `state-in-redb` invariant) is a later step.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::distance::Metric;

/// Default maximum neighbours per node above layer 0.
pub const DEFAULT_M: usize = 16;
/// Default candidate-list size during construction.
pub const DEFAULT_EF_CONSTRUCTION: usize = 128;

/// How vectors are stored in the index — trading accuracy for memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantization {
    /// Full `f32` precision (no quantization): `dim * 4` bytes/vector.
    None,
    /// One bit per dimension (sign), compared by Hamming distance: ~32x
    /// smaller. Best with cosine / normalized vectors.
    Binary,
    /// 8-bit scalar quantization (per-vector min + scale): ~4x smaller.
    /// Distance reconstructs to `f32` and uses the configured metric.
    Scalar,
}

/// A vector as stored in a node.
enum StoredVec {
    Full(Vec<f32>),
    Packed(Vec<u8>),
}

/// A query in the representation used for distance against stored vectors.
enum Probe {
    Full(Vec<f32>),
    Bits(Vec<u8>),
}

impl Quantization {
    fn encode(self, v: &[f32]) -> StoredVec {
        match self {
            Quantization::None => StoredVec::Full(v.to_vec()),
            Quantization::Binary => StoredVec::Packed(binary_encode(v)),
            Quantization::Scalar => StoredVec::Packed(scalar_encode(v)),
        }
    }

    fn probe(self, v: &[f32]) -> Probe {
        match self {
            Quantization::Binary => Probe::Bits(binary_encode(v)),
            // None and Scalar keep the query in full precision.
            _ => Probe::Full(v.to_vec()),
        }
    }

    /// Build a query probe from an already-stored vector (for neighbour pruning).
    fn probe_of(self, stored: &StoredVec) -> Probe {
        match (self, stored) {
            (Quantization::Binary, StoredVec::Packed(b)) => Probe::Bits(b.clone()),
            (Quantization::Scalar, StoredVec::Packed(b)) => Probe::Full(scalar_decode(b)),
            (_, StoredVec::Full(v)) => Probe::Full(v.clone()),
            (_, StoredVec::Packed(b)) => Probe::Full(scalar_decode(b)),
        }
    }

    fn dist(self, metric: Metric, probe: &Probe, stored: &StoredVec) -> f32 {
        match (self, probe, stored) {
            (Quantization::None, Probe::Full(q), StoredVec::Full(v)) => metric.distance(q, v),
            (Quantization::Binary, Probe::Bits(q), StoredVec::Packed(v)) => {
                if q.len() != v.len() {
                    return f32::INFINITY;
                }
                hamming(q, v) as f32
            }
            (Quantization::Scalar, Probe::Full(q), StoredVec::Packed(v)) => {
                let recon = scalar_decode(v);
                if recon.len() != q.len() {
                    return f32::INFINITY;
                }
                metric.distance(q, &recon)
            }
            _ => f32::INFINITY,
        }
    }
}

/// Pack the sign of each dimension into one bit (1 if `>= 0`).
fn binary_encode(v: &[f32]) -> Vec<u8> {
    let mut bits = vec![0u8; v.len().div_ceil(8)];
    for (i, &x) in v.iter().enumerate() {
        if x >= 0.0 {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    bits
}

/// Hamming distance between two equal-length bit-packed vectors.
fn hamming(a: &[u8], b: &[u8]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// Scalar-quantize to `min(4) ‖ scale(4) ‖ u8/dim`.
fn scalar_encode(v: &[f32]) -> Vec<u8> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &x in v {
        min = min.min(x);
        max = max.max(x);
    }
    if !min.is_finite() {
        min = 0.0;
    }
    if !max.is_finite() {
        max = 0.0;
    }
    let scale = if max > min { (max - min) / 255.0 } else { 1.0 };
    let mut out = Vec::with_capacity(8 + v.len());
    out.extend_from_slice(&min.to_le_bytes());
    out.extend_from_slice(&scale.to_le_bytes());
    for &x in v {
        out.push(((x - min) / scale).round().clamp(0.0, 255.0) as u8);
    }
    out
}

/// Reconstruct an approximate `f32` vector from scalar-quantized bytes.
fn scalar_decode(b: &[u8]) -> Vec<f32> {
    if b.len() < 8 {
        return Vec::new();
    }
    let min = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let scale = f32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    b[8..].iter().map(|&q| min + q as f32 * scale).collect()
}

/// A node: its (possibly quantized) vector and its per-layer neighbour lists.
struct Node {
    vector: StoredVec,
    layers: Vec<Vec<usize>>,
}

/// A distance/id pair, ordered by distance then id.
#[derive(Clone, Copy)]
struct Cand {
    dist: f32,
    id: usize,
}

impl PartialEq for Cand {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.id == other.id
    }
}
impl Eq for Cand {}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist
            .total_cmp(&other.dist)
            .then(self.id.cmp(&other.id))
    }
}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An HNSW index over fixed-dimension vectors under one [`Metric`].
pub struct Hnsw {
    metric: Metric,
    quant: Quantization,
    m: usize,
    m0: usize,
    ef_construction: usize,
    ml: f64,
    rng: u64,
    entry: Option<usize>,
    nodes: Vec<Node>,
    /// Build-time scratch: per-node "visited" epoch stamps, reused across
    /// inserts to avoid allocating a visited set on every layer search.
    visited: Vec<u32>,
    epoch: u32,
}

impl Hnsw {
    /// Create an index with the default parameters (full `f32` precision).
    pub fn new(metric: Metric) -> Self {
        Self::with_params(metric, DEFAULT_M, DEFAULT_EF_CONSTRUCTION)
    }

    /// Create an index with explicit `m` (neighbours per layer) and
    /// `ef_construction` (build-time candidate breadth).
    pub fn with_params(metric: Metric, m: usize, ef_construction: usize) -> Self {
        Self::with_quant(metric, Quantization::None, m, ef_construction)
    }

    /// Create an index with a storage quantization mode.
    pub fn with_quant(
        metric: Metric,
        quant: Quantization,
        m: usize,
        ef_construction: usize,
    ) -> Self {
        let m = m.max(2);
        Self {
            metric,
            quant,
            m,
            m0: m * 2,
            ef_construction: ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
            rng: 0x9E3779B97F4A7C15,
            entry: None,
            nodes: Vec::new(),
            visited: Vec::new(),
            epoch: 0,
        }
    }

    /// Number of indexed vectors.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Insert `vector`, returning its assigned node id (its insertion order).
    pub fn insert(&mut self, vector: Vec<f32>) -> usize {
        let id = self.nodes.len();
        let level = self.random_level();
        let probe = self.quant.probe(&vector);
        self.nodes.push(Node {
            vector: self.quant.encode(&vector),
            layers: vec![Vec::new(); level + 1],
        });

        let Some(entry) = self.entry else {
            self.entry = Some(id);
            return id;
        };

        let top = self.nodes[entry].layers.len() - 1;

        // Descend greedily from the top down to just above the new node's level.
        let mut cur = entry;
        for layer in ((level + 1)..=top).rev() {
            let w = Self::search_layer(
                &self.nodes,
                self.metric,
                self.quant,
                &mut self.visited,
                &mut self.epoch,
                &probe,
                &[cur],
                1,
                layer,
            );
            cur = w[0].id;
        }

        // Connect on every layer from min(level, top) down to 0.
        let start = level.min(top);
        for layer in (0..=start).rev() {
            let w = Self::search_layer(
                &self.nodes,
                self.metric,
                self.quant,
                &mut self.visited,
                &mut self.epoch,
                &probe,
                &[cur],
                self.ef_construction,
                layer,
            );
            let m_layer = if layer == 0 { self.m0 } else { self.m };
            let neighbors: Vec<usize> = w.iter().take(m_layer).map(|c| c.id).collect();

            self.nodes[id].layers[layer] = neighbors.clone();
            for &nb in &neighbors {
                self.nodes[nb].layers[layer].push(id);
                if self.nodes[nb].layers[layer].len() > m_layer {
                    self.prune(nb, layer, m_layer);
                }
            }
            cur = w[0].id;
        }

        if level > top {
            self.entry = Some(id);
        }
        id
    }

    /// Return the `k` nearest indexed vectors to `query` as `(id, distance)`,
    /// nearest first. `ef_search` widens the search beam (larger = more
    /// accurate, slower); it is raised to at least `k`.
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Vec<(usize, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }

        let mut cur = entry;
        let top = self.nodes[entry].layers.len() - 1;
        // Query-time search uses a local scratch (queries are far rarer than
        // build-time inserts, which reuse the index's scratch).
        let mut visited = Vec::new();
        let mut epoch = 0u32;
        let probe = self.quant.probe(query);
        for layer in (1..=top).rev() {
            let w = Self::search_layer(
                &self.nodes,
                self.metric,
                self.quant,
                &mut visited,
                &mut epoch,
                &probe,
                &[cur],
                1,
                layer,
            );
            cur = w[0].id;
        }
        let w = Self::search_layer(
            &self.nodes,
            self.metric,
            self.quant,
            &mut visited,
            &mut epoch,
            &probe,
            &[cur],
            ef_search.max(k),
            0,
        );
        w.into_iter().take(k).map(|c| (c.id, c.dist)).collect()
    }

    /// Greedy best-first search on one layer, returning up to `ef` closest
    /// nodes sorted nearest-first. Uses an epoch-stamped `visited` buffer
    /// (reused across calls) instead of allocating a set each time.
    #[allow(clippy::too_many_arguments)]
    fn search_layer(
        nodes: &[Node],
        metric: Metric,
        quant: Quantization,
        visited: &mut Vec<u32>,
        epoch: &mut u32,
        probe: &Probe,
        entries: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<Cand> {
        if visited.len() < nodes.len() {
            visited.resize(nodes.len(), 0);
        }
        *epoch = epoch.wrapping_add(1);
        if *epoch == 0 {
            visited.iter_mut().for_each(|v| *v = 0);
            *epoch = 1;
        }
        let mark = *epoch;
        // Returns true if `id` was not yet visited this search (and marks it).
        let mut newly_visited = |id: usize| -> bool {
            let unseen = visited[id] != mark;
            visited[id] = mark;
            unseen
        };

        let mut candidates: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut results: BinaryHeap<Cand> = BinaryHeap::new();

        for &ep in entries {
            let d = quant.dist(metric, probe, &nodes[ep].vector);
            let c = Cand { dist: d, id: ep };
            candidates.push(Reverse(c));
            results.push(c);
            newly_visited(ep);
        }

        while let Some(Reverse(c)) = candidates.pop() {
            if results.len() >= ef && c.dist > results.peek().expect("non-empty").dist {
                break;
            }
            for &nb in &nodes[c.id].layers[layer] {
                if newly_visited(nb) {
                    let d = quant.dist(metric, probe, &nodes[nb].vector);
                    let worst = results.peek().map(|w| w.dist).unwrap_or(f32::INFINITY);
                    if results.len() < ef || d < worst {
                        let cand = Cand { dist: d, id: nb };
                        candidates.push(Reverse(cand));
                        results.push(cand);
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<Cand> = results.into_vec();
        out.sort_unstable();
        out
    }

    /// Keep only the `m` nearest neighbours of `node` on `layer`.
    fn prune(&mut self, node: usize, layer: usize, m: usize) {
        // Take the neighbour list out so we can borrow other nodes immutably.
        let ns = std::mem::take(&mut self.nodes[node].layers[layer]);
        // Compute each distance exactly once (not per comparison), with the
        // pruned node itself as the query.
        let node_probe = self.quant.probe_of(&self.nodes[node].vector);
        let mut scored: Vec<(f32, usize)> = ns
            .iter()
            .map(|&a| {
                (
                    self.quant
                        .dist(self.metric, &node_probe, &self.nodes[a].vector),
                    a,
                )
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.truncate(m);
        self.nodes[node].layers[layer] = scored.into_iter().map(|(_, id)| id).collect();
    }

    /// Draw a level from a geometric distribution, `floor(-ln(U) * ml)`.
    fn random_level(&mut self) -> usize {
        let u = self.unit();
        (-u.ln() * self.ml).floor() as usize
    }

    /// A uniform `(0, 1]` double from the xorshift stream.
    fn unit(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        // Map to (0, 1]; +1 avoids ln(0).
        (((x >> 11) as f64) + 1.0) / ((1u64 << 53) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Deterministic pseudo-random vectors for reproducible recall tests.
    fn corpus(n: usize, dim: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64) as f32
        };
        (0..n).map(|_| (0..dim).map(|_| next()).collect()).collect()
    }

    fn exact_knn(corpus: &[Vec<f32>], query: &[f32], k: usize, metric: Metric) -> Vec<usize> {
        let mut scored: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, metric.distance(query, v)))
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        scored.into_iter().take(k).map(|(i, _)| i).collect()
    }

    #[test]
    fn empty_index_searches_to_empty() {
        let h = Hnsw::new(Metric::L2);
        assert!(h.is_empty());
        assert!(h.search(&[1.0, 2.0], 5, 10).is_empty());
    }

    #[test]
    fn single_vector_is_found() {
        let mut h = Hnsw::new(Metric::L2);
        let id = h.insert(vec![1.0, 2.0, 3.0]);
        assert_eq!(h.len(), 1);
        let got = h.search(&[1.0, 2.0, 3.0], 1, 10);
        assert_eq!(got[0].0, id);
        assert_eq!(got[0].1, 0.0);
    }

    #[test]
    fn k_zero_returns_empty() {
        let mut h = Hnsw::new(Metric::L2);
        h.insert(vec![1.0]);
        assert!(h.search(&[1.0], 0, 10).is_empty());
    }

    #[test]
    fn exact_nearest_neighbour_is_returned() {
        let data = corpus(300, 16);
        let mut h = Hnsw::with_params(Metric::L2, 16, 200);
        for v in &data {
            h.insert(v.clone());
        }
        // Query equal to a stored vector → that vector must be rank 1.
        let target = 123;
        let got = h.search(&data[target], 1, 64);
        assert_eq!(got[0].0, target);
    }

    #[test]
    fn recall_matches_exact_baseline() {
        let dim = 16;
        let data = corpus(500, dim);
        let mut h = Hnsw::with_params(Metric::L2, 16, 200);
        for v in &data {
            h.insert(v.clone());
        }

        let k = 10;
        let mut total_recall = 0.0;
        let queries = corpus(20, dim);
        for q in &queries {
            let approx: HashSet<usize> = h.search(q, k, 100).into_iter().map(|(i, _)| i).collect();
            let exact: HashSet<usize> = exact_knn(&data, q, k, Metric::L2).into_iter().collect();
            let hits = approx.intersection(&exact).count();
            total_recall += hits as f64 / k as f64;
        }
        let recall = total_recall / queries.len() as f64;
        // Exact KNN is the ground truth; HNSW with these params should recover
        // the large majority of true neighbours.
        assert!(recall >= 0.9, "recall {recall} below 0.9");
    }

    #[test]
    fn results_are_sorted_nearest_first() {
        let data = corpus(100, 8);
        let mut h = Hnsw::with_params(Metric::Cosine, 8, 100);
        for v in &data {
            h.insert(v.clone());
        }
        let got = h.search(&data[0], 10, 50);
        for pair in got.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distances not ascending");
        }
    }

    #[test]
    fn binary_quantized_index_finds_near_neighbours() {
        // Two well-separated sign-pattern clusters; binary (Hamming) must keep
        // a query's own cluster nearest.
        let mut h = Hnsw::with_quant(Metric::Cosine, Quantization::Binary, 16, 100);
        let mut data = Vec::new();
        for i in 0..200 {
            // Cluster A: mostly positive; Cluster B: mostly negative.
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            let v: Vec<f32> = (0..32)
                .map(|d| sign * ((d as f32 * 0.1 + i as f32 * 0.01).sin().abs() + 0.1))
                .collect();
            h.insert(v.clone());
            data.push((i, v));
        }
        // Query from cluster A (even) → nearest should be an even index.
        let (_, q) = &data[0];
        let got = h.search(q, 5, 50);
        assert!(!got.is_empty());
        assert_eq!(
            got[0].0 % 2,
            0,
            "nearest should share the query's sign cluster"
        );
    }

    #[test]
    fn scalar_quantized_index_matches_exact_ranking_closely() {
        let data = corpus(300, 16);
        let mut h = Hnsw::with_quant(Metric::L2, Quantization::Scalar, 16, 128);
        for v in &data {
            h.insert(v.clone());
        }
        // Query equal to a stored vector → it should still rank at the top
        // (scalar quantization is near-lossless for the exact match).
        let target = 42;
        let got = h.search(&data[target], 3, 64);
        assert!(got.iter().take(3).any(|(id, _)| *id == target));
    }

    #[test]
    fn binary_encode_and_hamming() {
        assert_eq!(binary_encode(&[1.0, -1.0, 2.0, -3.0]), vec![0b0000_0101]);
        assert_eq!(hamming(&[0b1010], &[0b0011]), 2);
    }

    #[test]
    fn scalar_roundtrip_is_approximate() {
        let v = vec![0.0, 0.25, 0.5, 1.0];
        let decoded = scalar_decode(&scalar_encode(&v));
        for (a, b) in v.iter().zip(&decoded) {
            assert!((a - b).abs() < 0.01, "{a} vs {b}");
        }
    }

    #[test]
    fn build_is_reproducible() {
        let data = corpus(50, 4);
        let build = || {
            let mut h = Hnsw::with_params(Metric::L2, 8, 50);
            for v in &data {
                h.insert(v.clone());
            }
            h.search(&data[7], 5, 30)
        };
        assert_eq!(build(), build());
    }
}
