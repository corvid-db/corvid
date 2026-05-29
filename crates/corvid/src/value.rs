//! The typed value model (engine layer L2) and its binary encoding.
//!
//! A [`Value`] is the unit of data the engine stores and queries. The set is
//! deliberately small and closed: the primitives plus two containers and a
//! first-class embedding vector. Documents are simply a [`Value::Map`].
//!
//! Encoding is a compact tag/length/value format. It is **deterministic** —
//! the same value always produces the same bytes (map keys are sorted) — so
//! encoded values can be hashed, compared, and deduplicated byte-wise. The
//! format is internal and carries no version tag: per project policy it may
//! change freely before v1.0, and old files are not read back across a change.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// A typed value stored in the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Absence of a value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit signed integer. Timestamps are represented as integers.
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// A UTF-8 string.
    Text(String),
    /// Opaque bytes.
    Bytes(Vec<u8>),
    /// An ordered list of values.
    Array(Vec<Value>),
    /// A string-keyed map. Also serves as the document / struct type.
    Map(BTreeMap<String, Value>),
    /// A dense embedding vector.
    Vector(Vec<f32>),
}

// Tag bytes. Stable only within a single format generation.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_TEXT: u8 = 4;
const TAG_BYTES: u8 = 5;
const TAG_ARRAY: u8 = 6;
const TAG_MAP: u8 = 7;
const TAG_VECTOR: u8 = 8;

impl Value {
    /// Encode the value into its deterministic byte representation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// If this is a [`Value::Map`], get the field named `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.get(key),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Bool`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Int`].
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Float`].
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Text`].
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Bytes`].
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Borrow the contents if this is a [`Value::Vector`].
    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }

    /// Decode a value previously produced by [`Value::encode`].
    ///
    /// Fails if the bytes are malformed or have trailing content.
    pub fn decode(bytes: &[u8]) -> Result<Value> {
        let mut dec = Decoder { buf: bytes, pos: 0 };
        let value = dec.value()?;
        if dec.pos != dec.buf.len() {
            return Err(Error::Decode(format!(
                "{} trailing byte(s) after value",
                dec.buf.len() - dec.pos
            )));
        }
        Ok(value)
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(TAG_NULL),
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(*b as u8);
            }
            Value::Int(n) => {
                out.push(TAG_INT);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Text(s) => {
                out.push(TAG_TEXT);
                put_len(out, s.len());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Bytes(b) => {
                out.push(TAG_BYTES);
                put_len(out, b.len());
                out.extend_from_slice(b);
            }
            Value::Array(items) => {
                out.push(TAG_ARRAY);
                put_len(out, items.len());
                for item in items {
                    item.encode_into(out);
                }
            }
            Value::Map(map) => {
                out.push(TAG_MAP);
                put_len(out, map.len());
                // BTreeMap iterates in sorted key order → deterministic.
                for (k, v) in map {
                    put_len(out, k.len());
                    out.extend_from_slice(k.as_bytes());
                    v.encode_into(out);
                }
            }
            Value::Vector(v) => {
                out.push(TAG_VECTOR);
                put_len(out, v.len());
                for f in v {
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
        }
    }
}

/// Append a length as a little-endian `u32`.
fn put_len(out: &mut Vec<u8>, n: usize) {
    debug_assert!(n <= u32::MAX as usize, "length exceeds u32");
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

/// A cursor over an encoded byte buffer.
struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn value(&mut self) -> Result<Value> {
        let tag = self.byte()?;
        match tag {
            TAG_NULL => Ok(Value::Null),
            TAG_BOOL => Ok(Value::Bool(self.byte()? != 0)),
            TAG_INT => Ok(Value::Int(i64::from_le_bytes(self.array8()?))),
            TAG_FLOAT => Ok(Value::Float(f64::from_bits(u64::from_le_bytes(
                self.array8()?,
            )))),
            TAG_TEXT => {
                let n = self.len()?;
                let bytes = self.take(n)?;
                let s = std::str::from_utf8(bytes)
                    .map_err(|e| Error::Decode(format!("invalid utf-8: {e}")))?;
                Ok(Value::Text(s.to_owned()))
            }
            TAG_BYTES => {
                let n = self.len()?;
                Ok(Value::Bytes(self.take(n)?.to_vec()))
            }
            TAG_ARRAY => {
                let n = self.len()?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Ok(Value::Array(items))
            }
            TAG_MAP => {
                let n = self.len()?;
                let mut map = BTreeMap::new();
                for _ in 0..n {
                    let klen = self.len()?;
                    let kbytes = self.take(klen)?;
                    let key = std::str::from_utf8(kbytes)
                        .map_err(|e| Error::Decode(format!("invalid utf-8 map key: {e}")))?
                        .to_owned();
                    let val = self.value()?;
                    map.insert(key, val);
                }
                Ok(Value::Map(map))
            }
            TAG_VECTOR => {
                let n = self.len()?;
                let mut v = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    v.push(f32::from_le_bytes(self.array4()?));
                }
                Ok(Value::Vector(v))
            }
            other => Err(Error::Decode(format!("unknown tag {other}"))),
        }
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| Error::Decode("unexpected end of input".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| Error::Decode("unexpected end of input".into()))?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn len(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.array4()?) as usize)
    }

    fn array4(&mut self) -> Result<[u8; 4]> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        Ok(a)
    }

    fn array8(&mut self) -> Result<[u8; 8]> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) {
        let bytes = v.encode();
        let back = Value::decode(&bytes).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn roundtrip_primitives() {
        roundtrip(Value::Null);
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        roundtrip(Value::Int(0));
        roundtrip(Value::Int(-1));
        roundtrip(Value::Int(i64::MIN));
        roundtrip(Value::Int(i64::MAX));
        roundtrip(Value::Float(0.0));
        roundtrip(Value::Float(-3.5));
        roundtrip(Value::Float(f64::INFINITY));
    }

    #[test]
    fn roundtrip_text_and_bytes() {
        roundtrip(Value::Text(String::new()));
        roundtrip(Value::Text("héllo 🐦".to_owned()));
        roundtrip(Value::Bytes(Vec::new()));
        roundtrip(Value::Bytes(vec![0, 1, 2, 255]));
    }

    #[test]
    fn roundtrip_containers_and_vector() {
        roundtrip(Value::Array(vec![
            Value::Int(1),
            Value::Text("two".into()),
            Value::Null,
        ]));
        let mut m = BTreeMap::new();
        m.insert("name".to_owned(), Value::Text("corvid".into()));
        m.insert("dims".to_owned(), Value::Int(8));
        roundtrip(Value::Map(m));
        roundtrip(Value::Vector(vec![0.0, -1.5, 3.25]));
        roundtrip(Value::Vector(Vec::new()));
    }

    #[test]
    fn roundtrip_deeply_nested() {
        let mut inner = BTreeMap::new();
        inner.insert("vec".to_owned(), Value::Vector(vec![1.0, 2.0]));
        inner.insert(
            "tags".to_owned(),
            Value::Array(vec![Value::Text("a".into()), Value::Text("b".into())]),
        );
        let mut outer = BTreeMap::new();
        outer.insert("meta".to_owned(), Value::Map(inner));
        outer.insert("ok".to_owned(), Value::Bool(true));
        roundtrip(Value::Map(outer));
    }

    #[test]
    fn encoding_is_deterministic_regardless_of_insert_order() {
        let mut a = BTreeMap::new();
        a.insert("z".to_owned(), Value::Int(1));
        a.insert("a".to_owned(), Value::Int(2));
        let mut b = BTreeMap::new();
        b.insert("a".to_owned(), Value::Int(2));
        b.insert("z".to_owned(), Value::Int(1));
        assert_eq!(Value::Map(a).encode(), Value::Map(b).encode());
    }

    #[test]
    fn decode_empty_input_errors() {
        let err = Value::decode(&[]).unwrap_err();
        assert!(matches!(err, Error::Decode(_)));
    }

    #[test]
    fn decode_unknown_tag_errors() {
        let err = Value::decode(&[200]).unwrap_err();
        assert!(format!("{err}").contains("unknown tag 200"));
    }

    #[test]
    fn decode_truncated_int_errors() {
        // INT tag but only 3 of 8 payload bytes.
        let err = Value::decode(&[TAG_INT, 1, 2, 3]).unwrap_err();
        assert!(format!("{err}").contains("unexpected end"));
    }

    #[test]
    fn decode_truncated_length_prefixed_errors() {
        // TEXT tag, length 10, but no content bytes.
        let mut bytes = vec![TAG_TEXT];
        bytes.extend_from_slice(&10u32.to_le_bytes());
        let err = Value::decode(&bytes).unwrap_err();
        assert!(matches!(err, Error::Decode(_)));
    }

    #[test]
    fn decode_invalid_utf8_text_errors() {
        let mut bytes = vec![TAG_TEXT];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let err = Value::decode(&bytes).unwrap_err();
        assert!(format!("{err}").contains("invalid utf-8"));
    }

    #[test]
    fn decode_invalid_utf8_map_key_errors() {
        let mut bytes = vec![TAG_MAP];
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one entry
        bytes.extend_from_slice(&1u32.to_le_bytes()); // key len 1
        bytes.push(0xff); // invalid utf-8 key byte
        let err = Value::decode(&bytes).unwrap_err();
        assert!(format!("{err}").contains("invalid utf-8 map key"));
    }

    #[test]
    fn decode_trailing_bytes_errors() {
        let mut bytes = Value::Bool(true).encode();
        bytes.push(0);
        let err = Value::decode(&bytes).unwrap_err();
        assert!(format!("{err}").contains("trailing"));
    }

    #[test]
    fn accessors_return_inner_value_on_match() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::Int(7).as_int(), Some(7));
        assert_eq!(Value::Float(1.5).as_float(), Some(1.5));
        assert_eq!(Value::Text("hi".into()).as_text(), Some("hi"));
        assert_eq!(Value::Bytes(vec![1, 2]).as_bytes(), Some(&[1u8, 2][..]));
        assert_eq!(
            Value::Vector(vec![1.0, 2.0]).as_vector(),
            Some(&[1.0f32, 2.0][..])
        );
    }

    #[test]
    fn accessors_return_none_on_mismatch() {
        assert_eq!(Value::Null.as_bool(), None);
        assert_eq!(Value::Null.as_int(), None);
        assert_eq!(Value::Null.as_float(), None);
        assert_eq!(Value::Null.as_text(), None);
        assert_eq!(Value::Null.as_bytes(), None);
        assert_eq!(Value::Null.as_vector(), None);
    }

    #[test]
    fn get_reads_map_fields_only() {
        let mut m = BTreeMap::new();
        m.insert("a".to_owned(), Value::Int(1));
        let v = Value::Map(m);
        assert_eq!(v.get("a"), Some(&Value::Int(1)));
        assert_eq!(v.get("missing"), None);
        // Non-map values have no fields.
        assert_eq!(Value::Int(1).get("a"), None);
    }
}
