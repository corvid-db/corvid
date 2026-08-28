//! Conversion between `serde_json::Value` and the engine's [`corvid::Value`].
//!
//! JSON has no native embedding-vector or byte-string type, so two object
//! conventions carry them:
//!
//! - `{ "$vector": [f, ...] }` ⇒ [`corvid::Value::Vector`]
//! - `{ "$bytes": [0..=255, ...] }` ⇒ [`corvid::Value::Bytes`]
//!
//! Any other object becomes a [`corvid::Value::Map`]. A genuine map whose only
//! key is `$vector`/`$bytes` with a matching array payload collides with the
//! convention — an accepted limitation of representing typed values in JSON.
//!
//! Round-trip fidelity limits (accepted): a JSON integer beyond `i64::MAX`
//! (u64 territory) has no engine representation and converts to a lossy
//! `f64`; non-finite floats (`NaN`, `±inf`) have no JSON representation and
//! convert to `null` on the way out — such values do not survive a
//! JSON round trip.

use std::collections::BTreeMap;

use corvid::Value;
use serde_json::Value as Json;

/// Convert a JSON value into an engine value.
pub fn json_to_value(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().expect("json number is f64-representable")),
        },
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(json_to_value).collect()),
        Json::Object(map) => {
            if map.len() == 1 {
                if let Some(Json::Array(arr)) = map.get("$vector")
                    && let Some(v) = as_f32_vec(arr)
                {
                    return Value::Vector(v);
                }
                if let Some(Json::Array(arr)) = map.get("$bytes")
                    && let Some(b) = as_byte_vec(arr)
                {
                    return Value::Bytes(b);
                }
            }
            let mut out = BTreeMap::new();
            for (k, v) in map {
                out.insert(k.clone(), json_to_value(v));
            }
            Value::Map(out)
        }
    }
}

/// Convert an engine value into a JSON value.
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => {
            let arr: Vec<Json> = b.iter().map(|x| Json::Number((*x).into())).collect();
            let mut o = serde_json::Map::new();
            o.insert("$bytes".to_owned(), Json::Array(arr));
            Json::Object(o)
        }
        Value::Array(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let o = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            Json::Object(o)
        }
        Value::Vector(vec) => {
            let arr: Vec<Json> = vec
                .iter()
                .map(|x| {
                    serde_json::Number::from_f64(*x as f64)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                })
                .collect();
            let mut o = serde_json::Map::new();
            o.insert("$vector".to_owned(), Json::Array(arr));
            Json::Object(o)
        }
    }
}

/// Interpret a JSON array as an `f32` vector, or `None` if any element is not a
/// number.
fn as_f32_vec(arr: &[Json]) -> Option<Vec<f32>> {
    arr.iter().map(|e| e.as_f64().map(|f| f as f32)).collect()
}

/// Interpret a JSON array as bytes, or `None` if any element is not in `0..=255`.
fn as_byte_vec(arr: &[Json]) -> Option<Vec<u8>> {
    arr.iter()
        .map(|e| e.as_u64().and_then(|n| u8::try_from(n).ok()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip_json(j: Json) {
        let v = json_to_value(&j);
        assert_eq!(value_to_json(&v), j);
    }

    #[test]
    fn primitives_roundtrip() {
        roundtrip_json(json!(null));
        roundtrip_json(json!(true));
        roundtrip_json(json!(42));
        roundtrip_json(json!(-7));
        roundtrip_json(json!(1.5));
        roundtrip_json(json!("hello"));
    }

    #[test]
    fn array_and_object_roundtrip() {
        roundtrip_json(json!([1, "two", true, null]));
        roundtrip_json(json!({"a": 1, "b": "x", "c": [1, 2]}));
    }

    #[test]
    fn vector_convention_roundtrips() {
        let j = json!({"$vector": [1.0, 0.5, -2.0]});
        assert_eq!(json_to_value(&j), Value::Vector(vec![1.0, 0.5, -2.0]));
        assert_eq!(value_to_json(&Value::Vector(vec![1.0, 0.5, -2.0])), j);
    }

    #[test]
    fn bytes_convention_roundtrips() {
        let j = json!({"$bytes": [0, 1, 255]});
        assert_eq!(json_to_value(&j), Value::Bytes(vec![0, 1, 255]));
        assert_eq!(value_to_json(&Value::Bytes(vec![0, 1, 255])), j);
    }

    #[test]
    fn vector_wrapper_with_non_numbers_is_a_plain_map() {
        let j = json!({"$vector": [1.0, "nope"]});
        match json_to_value(&j) {
            Value::Map(m) => assert!(m.contains_key("$vector")),
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn bytes_wrapper_out_of_range_is_a_plain_map() {
        let j = json!({"$bytes": [0, 256]});
        match json_to_value(&j) {
            Value::Map(m) => assert!(m.contains_key("$bytes")),
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn float_int_distinction() {
        assert_eq!(json_to_value(&json!(5)), Value::Int(5));
        assert_eq!(json_to_value(&json!(5.0)), Value::Float(5.0));
    }
}
