//! Vector quantization, shared by the in-memory and on-disk HNSW indexes.
//!
//! A vector is stored either at full `f32` precision or in a compressed form,
//! trading a little accuracy for a much smaller footprint (RAM for the
//! in-memory index, disk + page cache for the on-disk one). The same encode /
//! probe / distance logic backs both indexes so their recall behaviour matches.

use crate::distance::Metric;

/// How vectors are stored in an index — trading accuracy for memory.
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
#[derive(Clone)]
pub(crate) enum StoredVec {
    Full(Vec<f32>),
    Packed(Vec<u8>),
}

/// A query in the representation used for distance against stored vectors.
pub(crate) enum Probe {
    Full(Vec<f32>),
    Bits(Vec<u8>),
}

impl StoredVec {
    /// Serialize for on-disk storage. The variant is recoverable from the
    /// [`Quantization`] mode, so no discriminator is stored.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        match self {
            StoredVec::Full(v) => {
                let mut out = Vec::with_capacity(v.len() * 4);
                for &x in v {
                    out.extend_from_slice(&x.to_le_bytes());
                }
                out
            }
            StoredVec::Packed(b) => b.clone(),
        }
    }

    /// Reconstruct from on-disk bytes given the index's quantization mode.
    pub(crate) fn from_bytes(quant: Quantization, bytes: &[u8]) -> StoredVec {
        match quant {
            Quantization::None => {
                let mut v = Vec::with_capacity(bytes.len() / 4);
                for chunk in bytes.as_chunks::<4>().0 {
                    v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
                StoredVec::Full(v)
            }
            Quantization::Binary | Quantization::Scalar => StoredVec::Packed(bytes.to_vec()),
        }
    }
}

impl Quantization {
    pub(crate) fn encode(self, v: &[f32]) -> StoredVec {
        match self {
            Quantization::None => StoredVec::Full(v.to_vec()),
            Quantization::Binary => StoredVec::Packed(binary_encode(v)),
            Quantization::Scalar => StoredVec::Packed(scalar_encode(v)),
        }
    }

    pub(crate) fn probe(self, v: &[f32]) -> Probe {
        match self {
            Quantization::Binary => Probe::Bits(binary_encode(v)),
            // None and Scalar keep the query in full precision.
            _ => Probe::Full(v.to_vec()),
        }
    }

    /// Build a query probe from an already-stored vector (for neighbour pruning).
    pub(crate) fn probe_of(self, stored: &StoredVec) -> Probe {
        match (self, stored) {
            (Quantization::Binary, StoredVec::Packed(b)) => Probe::Bits(b.clone()),
            (Quantization::Scalar, StoredVec::Packed(b)) => Probe::Full(scalar_decode(b)),
            (_, StoredVec::Full(v)) => Probe::Full(v.clone()),
            (_, StoredVec::Packed(b)) => Probe::Full(scalar_decode(b)),
        }
    }

    pub(crate) fn dist(self, metric: Metric, probe: &Probe, stored: &StoredVec) -> f32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_encode_and_hamming() {
        assert_eq!(binary_encode(&[1.0, -1.0, 2.0, -3.0]), vec![0b0000_0101]);
        assert_eq!(hamming(&[0b1010], &[0b0011]), 2);
    }

    #[test]
    fn scalar_round_trips_closely() {
        let v = vec![0.0, 1.0, -2.0, 3.5, 0.25];
        let decoded = scalar_decode(&scalar_encode(&v));
        assert_eq!(decoded.len(), v.len());
        for (a, b) in v.iter().zip(&decoded) {
            assert!((a - b).abs() < 0.05, "{a} vs {b}");
        }
    }

    #[test]
    fn stored_vec_round_trips_through_bytes() {
        let full = StoredVec::Full(vec![1.0, -2.5, 3.0]);
        let bytes = full.to_bytes();
        match StoredVec::from_bytes(Quantization::None, &bytes) {
            StoredVec::Full(v) => assert_eq!(v, vec![1.0, -2.5, 3.0]),
            _ => panic!("expected full"),
        }
        let packed = StoredVec::Packed(vec![9, 8, 7]);
        match StoredVec::from_bytes(Quantization::Binary, &packed.to_bytes()) {
            StoredVec::Packed(b) => assert_eq!(b, vec![9, 8, 7]),
            _ => panic!("expected packed"),
        }
    }
}
