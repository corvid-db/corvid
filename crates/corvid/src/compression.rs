//! Optional zstd compression of stored document values (feature `zstd`).
//!
//! The DESIGN's L1 row — "zstd for values above a threshold (applied by us,
//! above redb)" — lands here, at the [`crate::store`] seam: values are
//! compressed as they are written into redb and decompressed on every read
//! path, so nothing above L1 observes compression at all. The feature is
//! **OFF by default** and adds no public API; with it off this module
//! compiles to two identity functions (CI greps assert the default and WASM
//! dependency graphs never contain the `zstd` crate, which pulls C FFI via
//! `cc`).
//!
//! # Scope: user collections only
//!
//! Compression applies only to *user* collections — names that do not start
//! with `__`, the same reserved-prefix rule the dump/migrate path and name
//! validation use. Engine-internal namespaces (`__edges__*`, `__ttl__*`, the
//! index namespaces, adjacency) keep their exact byte form: their values are
//! not [`Value`] encodings, several are read on hot per-row paths, and their
//! bytes can legitimately begin with any byte (edge weights are raw f64s),
//! which the marker scheme below deliberately never has to disambiguate.
//!
//! # On-disk scheme: one reserved leading byte
//!
//! A compressed stored value is `MARKER (0xFF) ++ zstd frame`; everything
//! else is stored raw. `0xFF` is outside the [`crate::value`] codec's tag
//! space (tags `0..=8`), so a stored [`Value`] encoding can never begin with
//! it and a stored row is self-describing: the first byte says compressed or
//! not, per value, with no header or format-version bump. Rows written
//! before the feature (by an OFF binary) therefore read fine under an ON
//! binary — their encodings start with a tag. The reverse direction is not
//! supported and never needs to be: the feature is a per-deployment build
//! choice, and `dump` (which reads *through* the store, i.e. decompressed)
//! plus `load` carries data between deployments in its own versioned,
//! compression-free format.
//!
//! Write-side rule: values at/above [`THRESHOLD`] compress at [`LEVEL`] and
//! are stored marked only when the marked form is *strictly shorter* than
//! the raw value — incompressible data (random bytes) is stored raw and
//! never balloons. One residual case is forced: a user-collection value that
//! itself begins with `0xFF` is stored marked (compressed) even when the
//! frame is larger, so a raw stored row can never begin with the marker and
//! the read-side test stays exact. The engine itself never writes such a
//! value (all its user-collection writes are [`Value`] encodings); this only
//! binds direct [`crate::store::Store`] users, for whom the marker byte is
//! reserved in user namespaces while the feature is on.
//!
//! # Constants
//!
//! - [`THRESHOLD`] = 1 KiB: below this a zstd frame's fixed overhead (~13 B
//!   magic/header/checksum-free trailer plus block framing) eats the win on
//!   the data that does compress, and small values are the common case the
//!   write path must not slow down.
//! - [`LEVEL`] = 3: zstd's own default — the fast end of the
//!   general-purpose band; embedded writes favor latency over the last few
//!   percent of ratio (measured ratios and per-op overheads in
//!   `docs/BENCHES.md`).
//!
//! Determinism: the one-shot bulk compressor with a fixed level is
//! deterministic, so the same value stores to the same bytes every time —
//! the property the [`crate::value`] codec's determinism doc promises for
//! hashing/dedup continues to hold within any one binary.

use std::borrow::Cow;

#[cfg(feature = "zstd")]
use crate::error::Error;
use crate::error::Result;

/// Leading byte of a compressed stored value. Chosen outside the value
/// codec's tag space (`0..=8`, see [`crate::value`]) so an uncompressed
/// [`Value`] encoding can never begin with it and decode is self-describing.
#[cfg(feature = "zstd")]
pub(crate) const MARKER: u8 = 0xFF;

/// Values at/above this length (bytes) are compression candidates; shorter
/// values are stored raw — a zstd frame's fixed overhead would exceed the
/// win on the compressible part.
#[cfg(feature = "zstd")]
pub(crate) const THRESHOLD: usize = 1024;

/// zstd compression level for candidates: zstd's default, the fast end of
/// the general-purpose band (embedded writes favor latency over ratio).
#[cfg(feature = "zstd")]
pub(crate) const LEVEL: i32 = 3;

/// Whether values in `collection` are compression-eligible: user namespaces
/// only (the `__` prefix is engine-reserved, the same rule dump and name
/// validation use).
#[cfg(feature = "zstd")]
#[inline]
pub(crate) fn eligible(collection: &str) -> bool {
    !collection.starts_with("__")
}

/// Transform `value` for storage in `collection`. Identity (a borrowed
/// [`Cow`], zero copies) when the feature is off, the namespace is
/// engine-reserved, the value is below [`THRESHOLD`], or compression does
/// not pay for itself; otherwise a fresh `MARKER ++ frame` buffer. Never
/// returns a raw form beginning with [`MARKER`] (see the module doc's
/// forced-compression residual — the one case where a marker-prefixed raw
/// value MUST still compress, hence the [`Result`]).
#[inline]
pub(crate) fn compress<'a>(collection: &str, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    #[cfg(feature = "zstd")]
    {
        if !eligible(collection) {
            return Ok(Cow::Borrowed(value));
        }
        compress_user(value)
    }
    #[cfg(not(feature = "zstd"))]
    {
        // OFF is today's byte behavior exactly: nothing ever compresses,
        // nothing is ever marked. (The CI cargo-tree greps pin that the
        // zstd crate is not even in the graph here.)
        let _ = collection;
        Ok(Cow::Borrowed(value))
    }
}

/// Decompress a stored `value` from `collection`. Identity (borrowed, zero
/// copies) when the feature is off, the namespace is engine-reserved, or the
/// value is not marker-prefixed; otherwise the decoded raw bytes. A
/// marker-prefixed row that fails to decode (corruption) is
/// [`Error::Decode`] — same contract as the value codec on malformed input.
#[inline]
pub(crate) fn decompress<'a>(collection: &str, stored: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    #[cfg(feature = "zstd")]
    {
        if !eligible(collection) || stored.first() != Some(&MARKER) {
            return Ok(Cow::Borrowed(stored));
        }
        decompress_user(stored).map(Cow::Owned)
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = collection;
        Ok(Cow::Borrowed(stored))
    }
}

// ---- ON-build cores (the OFF build compiles to the identity arms above) ----

#[cfg(feature = "zstd")]
fn compress_user(value: &[u8]) -> Result<Cow<'_, [u8]>> {
    // Below the threshold a frame's fixed overhead eats the win — store
    // raw — unless the value itself begins with the marker, which can never
    // be stored raw (read-side disambiguation; see the module doc).
    if value.len() < THRESHOLD && value.first() != Some(&MARKER) {
        return Ok(Cow::Borrowed(value));
    }
    let frame = zstd::bulk::compress(value, LEVEL)
        .map_err(|e| Error::Decode(format!("zstd compress: {e}")))?;
    let mut marked = Vec::with_capacity(frame.len() + 1);
    marked.push(MARKER);
    marked.extend_from_slice(&frame);
    // Store the marked form only when strictly smaller than the raw value —
    // incompressible data never balloons — or when forced by the marker
    // prefix above (correctness outranks size there).
    if marked.len() < value.len() || value.first() == Some(&MARKER) {
        Ok(Cow::Owned(marked))
    } else {
        Ok(Cow::Borrowed(value))
    }
}

#[cfg(feature = "zstd")]
fn decompress_user(stored: &[u8]) -> Result<Vec<u8>> {
    let frame = stored
        .get(1..)
        .ok_or_else(|| Error::Decode("zstd: marker with no frame".into()))?;
    zstd::stream::decode_all(frame).map_err(|e| Error::Decode(format!("zstd decompress: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift byte stream (the repo's no-`rand` convention).
    #[cfg(feature = "zstd")]
    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.push((x >> 33) as u8);
        }
        out
    }

    fn compressible(n: usize) -> Vec<u8> {
        // Repetitive but not uniform: long runs with periodic structure.
        let base = b"the quick brown fox jumps over the lazy dog; ";
        (0..n).map(|i| base[i % base.len()]).collect()
    }

    #[test]
    fn ineligible_namespaces_are_identity_both_directions() {
        let big = compressible(8192);
        assert!(matches!(
            compress("__edges__docs", &big).unwrap(),
            Cow::Borrowed(b) if b == &big[..]
        ));
        let got = decompress("__edges__docs", &big).unwrap();
        assert!(matches!(got, Cow::Borrowed(b) if b == &big[..]));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn below_threshold_stored_raw() {
        // 32 bytes short of the threshold: never a candidate, even though
        // it would compress well.
        let small = compressible(THRESHOLD - 32);
        assert_eq!(small.len(), THRESHOLD - 32);
        assert!(matches!(
            compress("docs", &small).unwrap(),
            Cow::Borrowed(_)
        ));
        // And the read side returns it untouched.
        let got = decompress("docs", &small).unwrap();
        assert!(matches!(got, Cow::Borrowed(b) if b == &small[..]));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn at_threshold_compressible_stored_marked() {
        // The boundary is inclusive: len == THRESHOLD is a candidate (the
        // check is `< THRESHOLD`), so compressible input at exactly the
        // threshold must store the strictly-smaller marked form — the
        // len == THRESHOLD - 32 pin above and this one bracket the edge.
        let v = compressible(THRESHOLD);
        assert_eq!(v.len(), THRESHOLD);
        let stored = compress("docs", &v).unwrap();
        let Cow::Owned(buf) = &stored else {
            panic!("compressible input at exactly THRESHOLD must compress");
        };
        assert_eq!(buf[0], MARKER, "stored form must be marker-prefixed");
        assert!(
            buf.len() < v.len(),
            "marked form must be strictly smaller: {} vs {}",
            buf.len(),
            v.len()
        );
        // Round trip.
        let back = decompress("docs", buf).unwrap();
        assert_eq!(back.as_ref(), &v[..]);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compressible_above_threshold_stored_marked_and_smaller() {
        let big = compressible(8192);
        let stored = compress("docs", &big).unwrap();
        let Cow::Owned(buf) = &stored else {
            panic!("8 KiB of repetitive text must compress, got raw");
        };
        assert_eq!(buf[0], MARKER, "stored form must be marker-prefixed");
        assert!(
            buf.len() < big.len(),
            "marked form must be strictly smaller: {} vs {}",
            buf.len(),
            big.len()
        );
        // Round trip.
        let back = decompress("docs", buf).unwrap();
        assert_eq!(back.as_ref(), &big[..]);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn incompressible_above_threshold_never_balloons() {
        // Random bytes at and well above the threshold: stored raw, never
        // larger than the input.
        for n in [THRESHOLD, THRESHOLD * 4, 64 * 1024] {
            let rnd = pseudo_random(n, 0xC0FFEE);
            match compress("docs", &rnd).unwrap() {
                Cow::Borrowed(raw) => assert_eq!(raw, &rnd[..]),
                Cow::Owned(buf) => {
                    panic!(
                        "incompressible {n} B must stay raw, got a {} B marked form",
                        buf.len()
                    );
                }
            }
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn marker_prefixed_raw_value_is_forced_compressed() {
        // A user-namespace value that itself begins with the marker must
        // still read back exactly — it is stored marked (compressed) even
        // when the frame is larger, so the raw form never starts with the
        // marker and the read-side test stays exact.
        let mut v = pseudo_random(2048, 7);
        v[0] = MARKER;
        let stored = compress("docs", &v).unwrap();
        assert_eq!(stored.as_ref()[0], MARKER, "must be stored marked");
        let back = decompress("docs", stored.as_ref()).unwrap();
        assert_eq!(back.as_ref(), &v[..]);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compression_is_deterministic() {
        let v = compressible(4096);
        assert_eq!(compress("docs", &v).unwrap(), compress("docs", &v).unwrap());
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn non_marker_input_decodes_as_identity() {
        // A raw (unmarked) row from an OFF-written database: returned
        // verbatim even when at/above threshold.
        let legacy = compressible(8192);
        let got = decompress("docs", &legacy).unwrap();
        assert!(matches!(got, Cow::Borrowed(b) if b == &legacy[..]));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn corrupt_frame_is_a_decode_error() {
        let v = compressible(4096);
        let stored = compress("docs", &v).unwrap().into_owned();
        let mut corrupt = stored.clone();
        // Flip payload bytes inside the frame (keep the marker).
        for b in corrupt.iter_mut().skip(4) {
            *b = !*b;
        }
        let err = decompress("docs", &corrupt).unwrap_err();
        assert!(
            matches!(&err, Error::Decode(m) if m.contains("zstd")),
            "{err}"
        );
        // Truncated frame likewise.
        let err = decompress("docs", &stored[..stored.len() / 2]).unwrap_err();
        assert!(
            matches!(&err, Error::Decode(m) if m.contains("zstd")),
            "{err}"
        );
    }

    /// Ratio arithmetic for the BENCHES.md table (exact byte counts on
    /// deterministic corpora). The three shapes bracket real workloads:
    /// structured text (varying numbers inside repeated sentence frames —
    /// log/JSON-like, not one repeated pattern), smooth f32 arrays
    /// (embedding-like), and random bytes (the stay-raw floor).
    #[cfg(feature = "zstd")]
    #[test]
    fn ratios_for_representative_documents() {
        use crate::value::Value;
        use std::collections::BTreeMap;

        // Text: sentence frames with varying numbers — structure to find,
        // entropy to keep (a JSON-ish/map document body).
        let mut m = BTreeMap::new();
        let body: String = (0..1024)
            .map(|i| format!("item {i}: the quick brown fox judges jug {i}x{i}; "))
            .collect();
        m.insert("id".to_owned(), Value::Int(1));
        m.insert("body".to_owned(), Value::Text(body));
        let text = Value::Map(m).encode();
        // Vectors: smooth deterministic floats (embedding-shaped payload).
        let floats: Vec<f32> = (0..16_384)
            .map(|i| ((i as f32) * 0.031).sin() * 10.0)
            .collect();
        let floats = Value::Vector(floats).encode();
        // Random: stays raw by rule.
        let rnd = pseudo_random(64 * 1024, 0xBADC0DE);

        let stored_text = compress("docs", &text).unwrap().into_owned();
        let stored_floats = compress("docs", &floats).unwrap().into_owned();
        let stored_rnd = compress("docs", &rnd).unwrap();
        assert!(matches!(stored_rnd, Cow::Borrowed(_)), "random stays raw");

        println!(
            "zstd ratio: structured-text map {} -> {} B ({:.1}%), \
             f32 vector array {} -> {} B ({:.1}%), random {} B -> raw",
            text.len(),
            stored_text.len(),
            100.0 * stored_text.len() as f64 / text.len() as f64,
            floats.len(),
            stored_floats.len(),
            100.0 * stored_floats.len() as f64 / floats.len() as f64,
            rnd.len(),
        );
    }
}
