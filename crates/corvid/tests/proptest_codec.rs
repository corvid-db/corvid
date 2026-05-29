//! Property test: the value codec round-trips any value.

use std::collections::BTreeMap;

use corvid::Value;
use proptest::prelude::*;

/// A bounded recursive strategy generating arbitrary `Value`s.
fn arb_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        // Avoid NaN: it never equals itself, which is about IEEE semantics,
        // not codec fidelity (the bit pattern still round-trips).
        any::<f64>()
            .prop_filter("no NaN", |f| !f.is_nan())
            .prop_map(Value::Float),
        ".*".prop_map(Value::Text),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(Value::Bytes),
        proptest::collection::vec(any::<f32>().prop_filter("no NaN", |f| !f.is_nan()), 0..16)
            .prop_map(Value::Vector),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::btree_map(".*", inner, 0..6)
                .prop_map(|m| Value::Map(m.into_iter().collect::<BTreeMap<_, _>>())),
        ]
    })
}

proptest! {
    #[test]
    fn encode_decode_roundtrips(v in arb_value()) {
        let bytes = v.encode();
        let back = Value::decode(&bytes).expect("decode");
        prop_assert_eq!(v, back);
    }

    #[test]
    fn encoding_is_deterministic(v in arb_value()) {
        prop_assert_eq!(v.encode(), v.encode());
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        // Must return Ok or Err, never panic.
        let _ = Value::decode(&bytes);
    }
}
