//! Product-quantization conformance (Task 15, folded Task 13.5): drives the
//! public quantizer type `corvid::pq::Pq` directly — the codebook machinery
//! behind `create_vector_index_ondisk_pq` — through its whole public surface:
//! training parameter math and determinism, encode/decode symmetry and
//! guards, the reconstruction distance (every metric) and its ADC fast path,
//! codebook persistence, and the `None`/`INFINITY` error paths. The engine
//! already exercises PQ end-to-end through the on-disk vector index; this
//! suite pins the quantizer's own contracts for a caller holding one.
//!
//! Determinism is the load-bearing property under test: `Pq::train` seeds
//! centroids deterministically and refines with a fixed iteration count, so
//! a fixed corpus yields a bit-identical codebook every run — which is what
//! makes recall assertions below a fixed value, not a sample from a
//! distribution. The corpus generator here is a seeded xorshift (no rand
//! dependency, no platform drift).

use corvid::distance::Metric;
use corvid::pq::Pq;

/// A deterministic uniform-in-[0,1) generator (xorshift64), so every corpus
/// below is a fixed constant vector set.
struct Xorshift(u64);

impl Xorshift {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f32 / (1u64 << 53) as f32
    }
}

/// `n` deterministic `dim`-vectors in `clusters` well-separated clusters:
/// cluster centers uniform in `[0, 10]^dim`, members jittered by ±0.5 per
/// component. PQ has structure to learn; uniform noise compresses poorly for
/// any quantizer.
fn clustered(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Xorshift(seed);
    let centers: Vec<Vec<f32>> = (0..clusters)
        .map(|_| (0..dim).map(|_| rng.next_f32() * 10.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            centers[i % clusters]
                .iter()
                .map(|&x| x + (rng.next_f32() - 0.5))
                .collect()
        })
        .collect()
}

/// Squared L2 between equal-length slices (the exactness reference).
fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// train's parameter/sample validation: the full reject matrix plus both
/// inclusive k bounds (`2 ≤ k ≤ 256`).
#[test]
fn pq_train_rejects_unusable_params_and_sample() {
    let data = clustered(40, 8, 4, 0x1234_5678_9abc_def0);
    // dim not divisible by m.
    assert!(Pq::train(&data, 3, 16).is_none(), "8 not divisible by 3");
    // m = 0 is unusable.
    assert!(Pq::train(&data, 0, 16).is_none());
    // k bounds are inclusive: 1 is too few, 257 too many…
    assert!(Pq::train(&data, 2, 1).is_none());
    assert!(Pq::train(&data, 2, 257).is_none());
    // …while 2 and 256 both train.
    assert!(Pq::train(&data, 4, 2).is_some(), "k = 2 is in range");
    assert!(Pq::train(&data, 1, 256).is_some(), "k = 256 is in range");
    // Empty sample, zero-dim sample, ragged sample.
    assert!(Pq::train(&[], 2, 16).is_none());
    assert!(Pq::train(&[vec![]], 1, 4).is_none());
    assert!(
        Pq::train(&[vec![0.0; 8], vec![0.0; 7]], 2, 16).is_none(),
        "ragged sample"
    );
}

/// Training is deterministic (same corpus → bit-identical quantizer), the
/// accessors report the training parameters, and the serialized codebook is
/// exactly the documented layout: a 16-byte header plus `m` codebooks of
/// `k × sub_dim` little-endian f32s.
#[test]
fn pq_train_is_deterministic_and_codebook_size_math_holds() {
    let data = clustered(60, 8, 5, 0x0fed_cba9_8765_4321);
    let a = Pq::train(&data, 4, 16).unwrap();
    let b = Pq::train(&data, 4, 16).unwrap();
    assert_eq!(a, b, "same corpus must train a bit-identical codebook");

    assert_eq!(a.code_len(), 4, "m code bytes per vector");
    assert_eq!(a.dim(), 8, "the trained-for dimensionality");
    assert_eq!(a.params(), (4, 16), "(m, k) round-trip");

    // 16 header bytes + m codebooks × k centroids × sub_dim components × 4B.
    let (m, k) = a.params();
    let sub_dim = a.dim() / m;
    assert_eq!(a.to_bytes().len(), 16 + m * k * sub_dim * 4);
}

/// encode is deterministic and compact (`m` bytes); a dimension-mismatched
/// vector yields the documented all-zero code instead of an error; every
/// code byte is a valid centroid index `< k`.
#[test]
fn pq_encode_is_deterministic_compact_and_dimension_guarded() {
    let data = clustered(50, 8, 5, 0xaaaa_bbbb_cccc_dddd);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let code = pq.encode(&data[7]);
    assert_eq!(code.len(), pq.code_len());
    assert_eq!(code, pq.encode(&data[7]), "same vector, same code");
    assert!(
        code.iter().all(|&c| (c as usize) < pq.params().1),
        "code bytes are centroid indices < k"
    );
    assert_eq!(
        pq.encode(&[1.0, 2.0, 3.0]),
        vec![0u8; pq.code_len()],
        "wrong-dim vector → all-zero code"
    );
}

/// decode reconstructs a full-dimension approximation; a truncated code
/// reconstructs only the subspaces it covers; an out-of-range code byte
/// reconstructs that subspace as zeros (the guard), never panics.
#[test]
fn pq_decode_reconstructs_and_guards_malformed_codes() {
    let data = clustered(50, 8, 5, 0x1111_2222_3333_4444);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let code = pq.encode(&data[3]);
    let recon = pq.decode(&code);
    assert_eq!(recon.len(), pq.dim());
    assert_eq!(recon, pq.decode(&pq.encode(&data[3])), "deterministic");

    // A short code covers only its subspaces.
    let short = pq.decode(&code[..pq.code_len() - 1]);
    assert_eq!(short.len(), pq.dim() - pq.dim() / pq.code_len());

    // A code byte beyond k decodes that subspace as zeros.
    let (_, k) = pq.params();
    let mut bad = code.clone();
    bad[0] = u8::MAX;
    assert!((bad[0] as usize) >= k, "precondition: 255 out of range");
    let guarded = pq.decode(&bad);
    assert_eq!(guarded.len(), pq.dim());
    let sub_dim = pq.dim() / pq.code_len();
    assert!(
        guarded[..sub_dim].iter().all(|&x| x == 0.0),
        "out-of-range centroid decodes as zeros"
    );
    assert_eq!(guarded[sub_dim..], recon[sub_dim..], "rest unaffected");
}

/// `Pq::distance` is exactly the metric applied to the reconstruction —
/// the "decode then measure" contract that keeps PQ correct for every
/// metric — and orders a near vector before a far one under L2.
#[test]
fn pq_distance_is_reconstruction_distance_for_every_metric() {
    let data = clustered(50, 8, 5, 0xdead_beef_cafe_f00d);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let q = &data[10];
    for metric in [Metric::Cosine, Metric::Dot, Metric::L2] {
        let code = pq.encode(&data[11]);
        assert_eq!(
            pq.distance(metric, q, &code),
            metric.distance(q, &pq.decode(&code)),
            "{metric:?}: distance is the metric over the reconstruction"
        );
    }
    // Same-cluster (near) vs different-cluster (far) under squared L2.
    let near = pq.encode(&data[15]); // same cluster as 10 when 5 | (15-10)
    let far = pq.encode(&data[12]);
    assert!(pq.distance(Metric::L2, q, &near) < pq.distance(Metric::L2, q, &far));
}

/// The L2 fast path: `l2_table` layout is `table[s*k + c]` = squared
/// distance from the query's subvector `s` to centroid `c` (cross-checked by
/// decoding the single-centroid code); `adc_l2` sums the lookups and matches
/// the reconstruction distance; a dimension-mismatched query gets `None` and
/// an out-of-range code scores `INFINITY`.
#[test]
fn pq_adc_l2_fast_path_matches_reconstruction_and_guards() {
    let data = clustered(60, 8, 5, 0x95a1_2b3c_4d5e_6f70);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let (m, k) = pq.params();
    let sub_dim = pq.dim() / m;
    let q = &data[20];
    let table = pq.l2_table(q).expect("matching dim builds a table");
    assert_eq!(table.len(), m * k);

    // Table layout: entry (s=0, c) equals ||q_sub0 − centroid_0,c||², where
    // the centroid is read back through decode of the single-centroid code.
    let mut one_centroid = vec![0u8; m];
    for (c, entry) in table.iter().enumerate().take(k) {
        one_centroid[0] = c as u8;
        let centroid = pq.decode(&one_centroid);
        let expected = l2_sq(&q[..sub_dim], &centroid[..sub_dim]);
        assert!(
            (entry - expected).abs() <= 1e-3 * (1.0 + expected),
            "table[0,{c}] = {entry} vs {expected}"
        );
    }

    // ADC sums the per-subspace lookups; it is the same squared-L2 quantity
    // as the reconstruction distance (float association differs, tolerance).
    for v in data.iter().take(15) {
        let code = pq.encode(v);
        let adc = pq.adc_l2(&table, &code);
        let recon = pq.distance(Metric::L2, q, &code);
        assert!(
            (adc - recon).abs() <= 1e-3 * (1.0 + recon),
            "adc {adc} vs reconstruction {recon}"
        );
    }

    // Guards: mismatched query dims → None; corrupt code bytes → INFINITY.
    let wrong_dim = vec![0.0; pq.dim() + 1];
    assert!(pq.l2_table(&wrong_dim).is_none());
    let mut corrupt = pq.encode(&data[1]);
    corrupt[0] = u8::MAX;
    assert_eq!(pq.adc_l2(&table, &corrupt), f32::INFINITY);
}

/// Codebook persistence: `to_bytes`/`from_bytes` round-trip bit-identically
/// (encoding included), and `from_bytes` rejects every malformed input —
/// short prefix, zeroed header fields, inconsistent dims, truncated payload.
#[test]
fn pq_codebook_roundtrips_bytes_and_rejects_malformed() {
    let data = clustered(80, 12, 6, 0x7f3a_91c4_55e0_28d9);
    let pq = Pq::train(&data, 4, 16).unwrap();
    let bytes = pq.to_bytes();
    let back = Pq::from_bytes(&bytes).unwrap();
    assert_eq!(pq, back);
    assert_eq!(pq.encode(&data[9]), back.encode(&data[9]));

    // Short prefix.
    assert!(Pq::from_bytes(&[]).is_none());
    assert!(Pq::from_bytes(&bytes[..15]).is_none());
    // Zeroed m / k / sub_dim / dim ≠ m·sub_dim.
    let zero_field = |idx: usize| {
        let mut b = bytes.clone();
        b[idx..idx + 4].copy_from_slice(&0u32.to_le_bytes());
        b
    };
    assert!(Pq::from_bytes(&zero_field(0)).is_none(), "m = 0");
    assert!(Pq::from_bytes(&zero_field(4)).is_none(), "k = 0");
    assert!(Pq::from_bytes(&zero_field(8)).is_none(), "sub_dim = 0");
    let mut inconsistent = bytes.clone();
    inconsistent[12..16].copy_from_slice(&((pq.dim() + 4) as u32).to_le_bytes());
    assert!(
        Pq::from_bytes(&inconsistent).is_none(),
        "dim != m * sub_dim"
    );
    // Truncated payload.
    assert!(Pq::from_bytes(&bytes[..bytes.len() - 4]).is_none());
}

/// Recall sanity on a fixed corpus: 8 code bytes per 16-dim vector (8×
/// smaller than f32) recovering the exact L2 top-10. The corpus, the
/// training, and the ADC scan are all deterministic, so the recall is a
/// fixed number per toolchain; the asserted bound is justified by the
/// geometry: cluster members sit within L2 ≈ 2 of their center while
/// distinct centers are ≈ 10–20 apart, and 32 centroids per subspace fit
/// the 8-cluster structure with room for the jitter, so a query's true
/// neighbours reconstruct near it and dominate the ranking. The bound
/// (0.75) sits well below the observed value to absorb f32 noise while
/// still failing if ADC ever stops matching reconstruction quality.
#[test]
fn pq_adc_recall_bound_on_fixed_corpus() {
    let corpus = clustered(200, 16, 8, 0x0c0f_fe0e_ba0b_1e55);
    let queries = clustered(20, 16, 8, 0x5eed_5eed_5eed_5eed);
    let pq = Pq::train(&corpus, 8, 32).unwrap();
    let codes: Vec<Vec<u8>> = corpus.iter().map(|v| pq.encode(v)).collect();
    let k = 10;
    let mut total = 0.0f64;
    for q in &queries {
        // Exact squared-L2 top-k.
        let mut exact: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, l2_sq(q, v)))
            .collect();
        exact.sort_by(|a, b| a.1.total_cmp(&b.1));
        let want: std::collections::HashSet<usize> =
            exact.iter().take(k).map(|&(i, _)| i).collect();
        // ADC top-k over the codes.
        let table = pq.l2_table(q).unwrap();
        let mut approx: Vec<(usize, f32)> = codes
            .iter()
            .enumerate()
            .map(|(i, c)| (i, pq.adc_l2(&table, c)))
            .collect();
        approx.sort_by(|a, b| a.1.total_cmp(&b.1));
        let got: std::collections::HashSet<usize> =
            approx.iter().take(k).map(|&(i, _)| i).collect();
        total += want.intersection(&got).count() as f64 / k as f64;
    }
    let recall = total / queries.len() as f64;
    assert!(recall >= 0.75, "PQ top-10 recall {recall} below bound");
}
