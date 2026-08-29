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
use std::sync::Arc;

use crate::distance::Metric;
use crate::pq::Pq;
pub use crate::quant::Quantization;
use crate::quant::{Probe, StoredVec};

/// Default maximum neighbours per node above layer 0.
pub const DEFAULT_M: usize = 16;
/// Default candidate-list size during construction.
pub const DEFAULT_EF_CONSTRUCTION: usize = 128;

/// A node: its (possibly quantized) vector and its per-layer neighbour lists.
struct Node {
    vector: StoredVec,
    layers: Vec<Vec<usize>>,
}

/// How a query is scored against stored vectors: a plain [`Quantization`]
/// probe, a full vector for the PQ reconstruction path, or a precomputed PQ
/// L2 asymmetric-distance table for the fast path — the in-memory twin of
/// `DiskParams`' probe logic in `disk_hnsw.rs`, so both indexes expose the
/// same metric×storage contract.
enum HProbe {
    Q(Probe),
    Pq(Vec<f32>),
    PqAdc(Vec<f32>),
}

/// Distance plumbing for one insert/search: the index's storage scheme
/// (plain quantization, or PQ codes under a trained codebook) and metric.
/// Owned (the `Arc` codebook is a refcount bump) so an insert can hold a
/// scorer across `&mut self` calls like `prune`.
#[derive(Clone)]
struct Scorer {
    metric: Metric,
    quant: Quantization,
    pq: Option<Arc<Pq>>,
}

impl Scorer {
    /// Build the PQ probe for a query: under L2, precompute the asymmetric-
    /// distance table once (so each node costs `m` table lookups instead of
    /// an O(dim) reconstruct + distance); other metrics keep the query
    /// vector for reconstruction distances. This mirrors `DiskParams::
    /// pq_probe` exactly — the same unreachable-fallback caveat applies
    /// (dimension-mismatched queries are declined by callers first).
    fn pq_probe(pq: &Pq, metric: Metric, query: Vec<f32>) -> HProbe {
        match metric {
            Metric::L2 => match pq.l2_table(&query) {
                Some(table) => HProbe::PqAdc(table),
                None => HProbe::Pq(query),
            },
            _ => HProbe::Pq(query),
        }
    }

    /// Encode a query vector into the form distances are computed against.
    fn probe(&self, query: &[f32]) -> HProbe {
        match &self.pq {
            Some(pq) => Self::pq_probe(pq, self.metric, query.to_vec()),
            None => HProbe::Q(self.quant.probe(query)),
        }
    }

    /// Build a probe from an already-stored vector (for neighbour pruning).
    /// Unlike [`Scorer::probe`], the PQ path always keeps the DECODED vector
    /// rather than an ADC table: pruning scores at most `m0 + 1` candidates,
    /// where a table build (`k * dim` work) costs more than scoring that
    /// handful by reconstruction — and for L2 the two score the same
    /// quantity (`adc_l2` is exactly the squared L2 to the reconstruction,
    /// so only fp-association can differ). The on-disk twin keeps its table
    /// shape (its IO dominates); the public contract is identical either way.
    fn probe_of(&self, stored: &StoredVec) -> HProbe {
        match (&self.pq, stored) {
            (Some(pq), StoredVec::Packed(code)) => HProbe::Pq(pq.decode(code)),
            (Some(_), StoredVec::Full(_)) => HProbe::Pq(Vec::new()),
            (None, _) => HProbe::Q(self.quant.probe_of(stored)),
        }
    }

    /// Distance from a probe to a stored vector.
    fn dist(&self, probe: &HProbe, stored: &StoredVec) -> f32 {
        match (probe, &self.pq, stored) {
            (HProbe::PqAdc(table), Some(pq), StoredVec::Packed(code)) => pq.adc_l2(table, code),
            (HProbe::Pq(q), Some(pq), StoredVec::Packed(code)) => pq.distance(self.metric, q, code),
            (HProbe::Q(p), None, _) => self.quant.dist(self.metric, p, stored),
            _ => f32::INFINITY,
        }
    }
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
    /// When set, vectors are stored as PQ codes and distances use this
    /// codebook (overrides `quant`, exactly like `DiskParams::pq`).
    pq: Option<Arc<Pq>>,
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
        Self::with_storage(metric, quant, None, m, ef_construction)
    }

    /// Create an index whose vectors are stored as product-quantized codes
    /// under the trained codebook `pq` (see [`Pq::train`]): `m` code bytes
    /// per vector instead of `dim * 4`. The metric×storage contract matches
    /// the on-disk PQ index exactly: every metric serves — L2 scores through
    /// the asymmetric-distance table ([`Pq::l2_table`]/[`Pq::adc_l2`], the
    /// fast path), cosine and dot through reconstruct-then-distance
    /// ([`Pq::distance`]). Callers insert vectors of the codebook's
    /// dimension; a mismatched vector encodes to the all-zero code
    /// ([`Pq::encode`]'s documented contract), which cannot be matched
    /// meaningfully.
    pub fn with_pq(metric: Metric, pq: Arc<Pq>, m: usize, ef_construction: usize) -> Self {
        Self::with_storage(metric, Quantization::None, Some(pq), m, ef_construction)
    }

    /// The shared constructor: plain quantization or PQ storage.
    fn with_storage(
        metric: Metric,
        quant: Quantization,
        pq: Option<Arc<Pq>>,
        m: usize,
        ef_construction: usize,
    ) -> Self {
        let m = m.max(2);
        Self {
            metric,
            quant,
            pq,
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

    /// An owned scorer for this index's storage scheme (Arc refcount bump).
    fn scorer(&self) -> Scorer {
        Scorer {
            metric: self.metric,
            quant: self.quant,
            pq: self.pq.clone(),
        }
    }

    /// Encode a vector for storage (PQ codes when a codebook is set).
    fn encode(&self, v: &[f32]) -> StoredVec {
        match &self.pq {
            Some(pq) => StoredVec::Packed(pq.encode(v)),
            None => self.quant.encode(v),
        }
    }

    /// Insert `vector`, returning its assigned node id (its insertion order).
    pub fn insert(&mut self, vector: Vec<f32>) -> usize {
        let id = self.nodes.len();
        let level = self.random_level();
        let scorer = self.scorer();
        let probe = scorer.probe(&vector);
        self.nodes.push(Node {
            vector: self.encode(&vector),
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
                &scorer,
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
                &scorer,
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

        let scorer = self.scorer();
        let mut cur = entry;
        let top = self.nodes[entry].layers.len() - 1;
        // Query-time search uses a local scratch (queries are far rarer than
        // build-time inserts, which reuse the index's scratch).
        let mut visited = Vec::new();
        let mut epoch = 0u32;
        let probe = scorer.probe(query);
        for layer in (1..=top).rev() {
            let w = Self::search_layer(
                &self.nodes,
                &scorer,
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
            &scorer,
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
        scorer: &Scorer,
        visited: &mut Vec<u32>,
        epoch: &mut u32,
        probe: &HProbe,
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
            let d = scorer.dist(probe, &nodes[ep].vector);
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
                    let d = scorer.dist(probe, &nodes[nb].vector);
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
        let scorer = self.scorer();
        let node_probe = scorer.probe_of(&self.nodes[node].vector);
        let mut scored: Vec<(f32, usize)> = ns
            .iter()
            .map(|&a| (scorer.dist(&node_probe, &self.nodes[a].vector), a))
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

    // ---- PQ storage (the in-memory twin of the on-disk PQ contract) ----

    /// Deterministic clustered vectors (the `pq.rs` shape): centers + noise,
    /// so PQ has structure to learn (uniform noise compresses poorly).
    fn clustered(n: usize, dim: usize, clusters: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0x0F1E_2D3C_4B5A_6978;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
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

    /// Every metric serves under PQ storage: L2 through the ADC table,
    /// cosine/dot through reconstruction. A query drawn from a cluster must
    /// rank that cluster's members nearest under all three.
    #[test]
    fn pq_storage_serves_every_metric() {
        for metric in [Metric::L2, Metric::Cosine, Metric::Dot] {
            let data = clustered(240, 16, 8);
            let pq = Arc::new(Pq::train(&data, 8, 32).unwrap());
            let mut h = Hnsw::with_pq(metric, pq, 16, 128);
            for v in &data {
                h.insert(v.clone());
            }
            let target = 100; // cluster 100 % 8 = 4
            let cluster_of = |i: usize| i % 8;
            let got = h.search(&data[target], 5, 64);
            assert!(!got.is_empty(), "{metric:?} served nothing");
            assert!(
                got.iter()
                    .all(|(id, _)| cluster_of(*id) == cluster_of(target)),
                "{metric:?}: PQ top-5 must share the query's cluster, got {got:?}"
            );
        }
    }

    /// Recall vs the exact baseline on a fixed clustered corpus. The bound is
    /// justified from where the loss lives, and the premise is pinned, not
    /// just claimed: recall on this corpus is measured identical at
    /// `ef_search` 100/200/400 (0.56 at each — the loop below asserts both
    /// the value and the insensitivity), because the graph recovers
    /// essentially the whole ADC ranking and the residual gap to exact is
    /// the codebook's resolution, not graph reach. `m=8, k=64`; the 0.55
    /// bound sits just under the measured 0.56 while staying far above
    /// chance (12 clusters → ~0.08).
    #[test]
    fn pq_recall_matches_exact_baseline() {
        let data = clustered(400, 16, 12);
        let pq = Arc::new(Pq::train(&data, 8, 64).unwrap());
        let mut h = Hnsw::with_pq(Metric::L2, pq, 16, 200);
        for v in &data {
            h.insert(v.clone());
        }
        let k = 10;
        let queries = clustered(20, 16, 12);
        let recall_at = |ef: usize| {
            let mut total = 0.0;
            for q in &queries {
                let approx: HashSet<usize> = h.search(q, k, ef).into_iter().map(|(i, _)| i).collect();
                let exact: HashSet<usize> = exact_knn(&data, q, k, Metric::L2).into_iter().collect();
                total += approx.intersection(&exact).count() as f64 / k as f64;
            }
            total / queries.len() as f64
        };
        // The ef-insensitivity premise is itself pinned, not just asserted in
        // prose: recall is identical at 100/200/400 (measured 0.56 at each),
        // because the graph already recovers essentially the whole ADC
        // ranking — the residual gap to exact is the codebook's resolution,
        // not graph reach. Raising ef cannot buy recall that ADC ranking
        // never contained.
        let recalls: Vec<f64> = [100usize, 200, 400].iter().map(|&ef| recall_at(ef)).collect();
        for (i, ef) in [100usize, 200, 400].iter().enumerate() {
            assert!(
                recalls[i] >= 0.55,
                "PQ recall {} at ef={ef} below 0.55",
                recalls[i]
            );
        }
        assert!(
            recalls.windows(2).all(|w| w[0] == w[1]),
            "PQ recall must be ef-insensitive on this corpus, got {recalls:?}"
        );
    }

    /// PQ build is reproducible end-to-end: training is seeded/fixed-iteration
    /// (pq.rs) and the graph's level RNG is seeded, so two identical builds
    /// return identical results — same contract as the unquantized build.
    #[test]
    fn pq_build_is_reproducible() {
        let data = clustered(80, 8, 4);
        let build = || {
            let pq = Arc::new(Pq::train(&data, 4, 16).unwrap());
            let mut h = Hnsw::with_pq(Metric::L2, pq, 8, 64);
            for v in &data {
                h.insert(v.clone());
            }
            h.search(&data[7], 5, 30)
        };
        assert_eq!(build(), build());
    }

    /// Heap-footprint proxy, as exact arithmetic on the stored
    /// representations: full precision stores `dim * 4` bytes per vector, PQ
    /// stores `m` code bytes per vector (no header), at the fixed one-off
    /// cost of the `m × k × (dim/m)`-float codebook. `dim=16, m=4` → 16×
    /// smaller vector payload.
    #[test]
    fn pq_storage_footprint_is_m_bytes_per_vector() {
        let data = corpus(100, 16);
        let pq = Arc::new(Pq::train(&data, 4, 16).unwrap());
        let mut plain = Hnsw::with_params(Metric::L2, 8, 64);
        let mut quant = Hnsw::with_pq(Metric::L2, pq, 8, 64);
        for v in &data {
            plain.insert(v.clone());
            quant.insert(v.clone());
        }
        let stored = |h: &Hnsw| {
            h.nodes
                .iter()
                .map(|n| match &n.vector {
                    StoredVec::Full(v) => v.len() * 4,
                    StoredVec::Packed(b) => b.len(),
                })
                .sum::<usize>()
        };
        assert_eq!(stored(&plain), 100 * 16 * 4);
        assert_eq!(stored(&quant), 100 * 4);
        assert_eq!(stored(&plain) / stored(&quant), 16);
    }
}
