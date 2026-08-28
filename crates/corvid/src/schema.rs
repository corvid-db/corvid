//! Optional declared schema and constraints per collection.
//!
//! Collections are schemaless by default. A collection *may* declare a schema —
//! a set of fields with a type, and optional `required` / `unique` flags —
//! which is then enforced on every write to that collection. Collections
//! without a schema are entirely unaffected. The schema persists across reopen.
//!
//! Enforcement is opt-in and strict: a write that violates the schema fails
//! with [`Error::SchemaViolation`] and nothing
//! is stored.

use std::collections::HashMap;

use crate::db::{Collection, Db};
use crate::error::{Error, Result};
use crate::value::Value;

/// Reserved collection holding persisted schemas.
const SCHEMA_DEFS: &str = "__schemas__";

/// The declared type of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// Any value is accepted (only `required`/`unique` apply).
    Any,
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    Vector,
    Array,
    Map,
}

impl FieldType {
    fn accepts(self, v: &Value) -> bool {
        matches!(
            (self, v),
            (FieldType::Any, _)
                | (FieldType::Bool, Value::Bool(_))
                | (FieldType::Int, Value::Int(_))
                | (FieldType::Float, Value::Float(_))
                | (FieldType::Text, Value::Text(_))
                | (FieldType::Bytes, Value::Bytes(_))
                | (FieldType::Vector, Value::Vector(_))
                | (FieldType::Array, Value::Array(_))
                | (FieldType::Map, Value::Map(_))
        )
    }

    pub(crate) fn to_byte(self) -> u8 {
        match self {
            FieldType::Any => 0,
            FieldType::Bool => 1,
            FieldType::Int => 2,
            FieldType::Float => 3,
            FieldType::Text => 4,
            FieldType::Bytes => 5,
            FieldType::Vector => 6,
            FieldType::Array => 7,
            FieldType::Map => 8,
        }
    }

    pub(crate) fn from_byte(b: u8) -> Option<FieldType> {
        Some(match b {
            0 => FieldType::Any,
            1 => FieldType::Bool,
            2 => FieldType::Int,
            3 => FieldType::Float,
            4 => FieldType::Text,
            5 => FieldType::Bytes,
            6 => FieldType::Vector,
            7 => FieldType::Array,
            8 => FieldType::Map,
            _ => return None,
        })
    }
}

/// One declared field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Field name (top-level key in the document map).
    pub name: String,
    /// The accepted value type.
    pub ty: FieldType,
    /// The field must be present and non-null on every document.
    pub required: bool,
    /// No two documents may share this field's value.
    pub unique: bool,
}

impl Field {
    /// A field of `ty`, neither required nor unique.
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Self {
            name: name.into(),
            ty,
            required: false,
            unique: false,
        }
    }

    /// Mark this field as required.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Mark this field as unique.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}

/// A declared schema: an ordered set of fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// An empty schema (accepts any map document).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a field.
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// The declared fields, in order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());
        for f in &self.fields {
            out.push(f.ty.to_byte());
            out.push(f.required as u8);
            out.push(f.unique as u8);
            out.extend_from_slice(&(f.name.len() as u32).to_le_bytes());
            out.extend_from_slice(f.name.as_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Option<Schema> {
        let n = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?) as usize;
        let mut pos = 4;
        // The count is untrusted input; allocate conservatively and grow
        // (migrate.rs precedent; audit C1 — a forged huge count must fail
        // the decode below, not reserve gigabytes first).
        let mut fields = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let ty = FieldType::from_byte(*b.get(pos)?)?;
            let required = *b.get(pos + 1)? != 0;
            let unique = *b.get(pos + 2)? != 0;
            let len = u32::from_le_bytes(b.get(pos + 3..pos + 7)?.try_into().ok()?) as usize;
            pos += 7;
            let name = std::str::from_utf8(b.get(pos..pos + len)?).ok()?.to_owned();
            pos += len;
            fields.push(Field {
                name,
                ty,
                required,
                unique,
            });
        }
        Some(Schema { fields })
    }
}

/// Per-database schema registry.
#[derive(Default)]
pub(crate) struct SchemaState {
    schemas: HashMap<String, Schema>,
}

pub(crate) fn new_state() -> std::sync::Mutex<SchemaState> {
    std::sync::Mutex::new(SchemaState::default())
}

/// Value equality for unique constraints: like `PartialEq`, except NaN
/// equals NaN (uniqueness is about identity of stored values, not IEEE
/// ordering) and containers compare element-wise under the same rule.
/// Also the equality [`crate::Collection::compare_and_set`] compares
/// expectations with — one engine-wide notion of "same stored value".
pub(crate) fn unique_value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| unique_value_eq(x, y))
        }
        (Value::Map(xs), Value::Map(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys.iter())
                    .all(|((kx, vx), (ky, vy))| kx == ky && unique_value_eq(vx, vy))
        }
        (Value::Vector(xs), Value::Vector(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys.iter())
                    .all(|(x, y)| x == y || (x.is_nan() && y.is_nan()))
        }
        _ => a == b,
    }
}

/// Scan-side unique check inside the caller's write transaction: reject any
/// *other* key whose value at `field` equals `value` under
/// [`unique_value_eq`]. Used when no scalar index serves the field, when the
/// value is not index-encodable (containers), and for NaN (whose encoded
/// bucket key is not guaranteed to collide).
fn unique_scan_in_txn(
    tx: &mut crate::store::WriteBatch<'_>,
    collection: &str,
    key: &[u8],
    field: &str,
    value: &Value,
) -> Result<()> {
    for (k, bytes) in tx.scan(collection)? {
        if k == key {
            continue;
        }
        let d = Value::decode(&bytes)?;
        if d.get_path(field).is_some_and(|v| unique_value_eq(v, value)) {
            return Err(Error::SchemaViolation(format!(
                "field '{field}' must be unique; value already exists"
            )));
        }
    }
    Ok(())
}

impl Db {
    /// Load persisted schemas. Called once on open.
    pub(crate) fn load_schemas(&self) -> Result<()> {
        let mut state = self.schemas().lock().expect("schema lock");
        for (key, value) in self.store().scan(SCHEMA_DEFS)? {
            if let Ok(name) = String::from_utf8(key)
                && let Some(schema) = Schema::decode(&value)
            {
                state.schemas.insert(name, schema);
            }
        }
        Ok(())
    }

    pub(crate) fn register_schema(&self, collection: &str, schema: &Schema) -> Result<()> {
        self.store()
            .put(SCHEMA_DEFS, collection.as_bytes(), &schema.encode())?;
        let mut state = self.schemas().lock().expect("schema lock");
        state.schemas.insert(collection.to_owned(), schema.clone());
        Ok(())
    }

    /// All declared schemas as `(collection, schema)` (for dump/migrate).
    pub(crate) fn schema_specs(&self) -> Vec<(String, Schema)> {
        let state = self.schemas().lock().expect("schema lock");
        state
            .schemas
            .iter()
            .map(|(c, s)| (c.clone(), s.clone()))
            .collect()
    }

    fn schema_of(&self, collection: &str) -> Option<Schema> {
        let state = self.schemas().lock().expect("schema lock");
        state.schemas.get(collection).cloned()
    }

    /// Validate `doc` (to be stored at `key`) against `collection`'s schema, if
    /// one is declared. Checks types and required presence — the pure checks
    /// that need no storage access. Unique constraints are enforced by
    /// [`Db::validate_unique_in_txn`] inside the write transaction itself, so
    /// two writers contending for the same value serialize instead of racing.
    pub(crate) fn validate_schema(&self, collection: &str, _key: &[u8], doc: &Value) -> Result<()> {
        let Some(schema) = self.schema_of(collection) else {
            return Ok(());
        };
        for f in &schema.fields {
            let found = doc.get_path(&f.name);
            match found {
                None | Some(Value::Null) => {
                    if f.required {
                        return Err(Error::SchemaViolation(format!(
                            "field '{}' is required",
                            f.name
                        )));
                    }
                }
                Some(v) => {
                    if !f.ty.accepts(v) {
                        return Err(Error::SchemaViolation(format!(
                            "field '{}' has the wrong type",
                            f.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Enforce `unique` fields inside the caller's write transaction. The
    /// check observes the transaction's own earlier puts, so a batch cannot
    /// smuggle in duplicate unique values, and concurrent writers serialize
    /// on redb's single-writer commit.
    pub(crate) fn validate_unique_in_txn(
        &self,
        tx: &mut crate::store::WriteBatch<'_>,
        collection: &str,
        key: &[u8],
        doc: &Value,
    ) -> Result<()> {
        let Some(schema) = self.schema_of(collection) else {
            return Ok(());
        };
        for f in schema.fields.iter().filter(|f| f.unique) {
            let Some(value) = doc.get_path(&f.name) else {
                continue;
            };
            if matches!(value, Value::Null) {
                continue;
            }
            let conflict_msg = || {
                Error::SchemaViolation(format!(
                    "field '{}' must be unique; value already exists",
                    f.name
                ))
            };
            let nan = matches!(value, Value::Float(x) if x.is_nan());
            match if nan {
                None
            } else {
                crate::scalar::encode_value(value)
            } {
                Some(enc) if self.has_scalar_index(collection, &f.name) => {
                    // Walk exactly this value's bucket: index keys are
                    // `encoded_value ‖ doc_key`, so everything from `enc`
                    // until the first non-prefixed key shares the value.
                    let ns = crate::scalar::namespace(collection, &f.name);
                    let mut cursor = enc.clone();
                    'bucket: loop {
                        let page = tx.scan_from(&ns, &cursor, 256)?;
                        if page.is_empty() {
                            break 'bucket;
                        }
                        let mut next = None;
                        for (k, _) in &page {
                            if !k.starts_with(&enc) {
                                break 'bucket;
                            }
                            let doc_key = &k[enc.len()..];
                            if doc_key != key {
                                // The bucket key is the order-preserving f64
                                // encoding, which collapses numerically-equal
                                // but distinct stored values (Int(7) vs
                                // Float(7.0); f64-rounded huge ints) that the
                                // engine-wide storage equality keeps apart —
                                // re-check the actual stored value before
                                // rejecting, so this index path agrees with the
                                // scan path (and with compare_and_set).
                                let conflict = match tx.get(collection, doc_key)? {
                                    Some(bytes) => Value::decode(&bytes)?
                                        .get_path(&f.name)
                                        .is_some_and(|v| unique_value_eq(v, value)),
                                    // No document behind the entry (a row
                                    // deleted earlier in this same
                                    // transaction): not a conflict.
                                    None => false,
                                };
                                if conflict {
                                    return Err(conflict_msg());
                                }
                            }
                            next = Some(k.clone());
                        }
                        match next {
                            Some(mut c) => {
                                c.push(0);
                                cursor = c;
                            }
                            None => break 'bucket,
                        }
                    }
                }
                _ => unique_scan_in_txn(tx, collection, key, &f.name, value)?,
            }
        }
        Ok(())
    }
}

impl Collection<'_> {
    /// Declare (or replace) this collection's schema. Subsequent writes are
    /// validated against it; existing documents are left untouched (validation
    /// applies on write, not retroactively). Persists across reopen.
    pub fn set_schema(&self, schema: &Schema) -> Result<()> {
        self.ensure_writable()?;
        for f in schema.fields() {
            crate::db::validate_name(&f.name)?;
        }
        self.db().register_schema(self.name(), schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(pairs: &[(&str, Value)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_owned(), v.clone());
        }
        Value::Map(m)
    }

    fn schema() -> Schema {
        Schema::new()
            .field(Field::new("name", FieldType::Text).required())
            .field(Field::new("age", FieldType::Int))
            .field(Field::new("email", FieldType::Text).unique())
    }

    #[test]
    fn accepts_valid_document() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        c.insert(
            b"u1",
            &doc(&[
                ("name", Value::Text("rocky".into())),
                ("age", Value::Int(30)),
                ("email", Value::Text("a@x.com".into())),
            ]),
        )
        .unwrap();
        assert_eq!(c.len().unwrap(), 1);
    }

    #[test]
    fn rejects_missing_required_field() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        let err = c.insert(b"u1", &doc(&[("age", Value::Int(30))]));
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
        assert_eq!(c.len().unwrap(), 0); // nothing stored
    }

    #[test]
    fn rejects_wrong_type() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        let err = c.insert(
            b"u1",
            &doc(&[("name", Value::Int(5)), ("age", Value::Int(30))]),
        );
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
    }

    #[test]
    fn enforces_uniqueness() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        c.insert(
            b"u1",
            &doc(&[
                ("name", Value::Text("a".into())),
                ("email", Value::Text("dup@x.com".into())),
            ]),
        )
        .unwrap();
        // Same email, different key → rejected.
        let err = c.insert(
            b"u2",
            &doc(&[
                ("name", Value::Text("b".into())),
                ("email", Value::Text("dup@x.com".into())),
            ]),
        );
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
        // Overwriting the SAME key with its own value is fine (excluded).
        c.insert(
            b"u1",
            &doc(&[
                ("name", Value::Text("a2".into())),
                ("email", Value::Text("dup@x.com".into())),
            ]),
        )
        .unwrap();
    }

    /// Regression: a batch containing two items with the same unique value
    /// used to commit both (validation ran against pre-batch state). Now the
    /// check runs inside the transaction and sees earlier batch items — and
    /// the whole batch rolls back atomically.
    #[test]
    fn batch_unique_violation_is_atomic() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        let u1 = doc(&[
            ("name", Value::Text("a".into())),
            ("email", Value::Text("x@x.com".into())),
        ]);
        let u2 = doc(&[
            ("name", Value::Text("b".into())),
            ("email", Value::Text("y@x.com".into())),
        ]);
        let u3 = doc(&[
            ("name", Value::Text("c".into())),
            ("email", Value::Text("x@x.com".into())),
        ]);
        let err = c.insert_batch(&[(b"u1", &u1), (b"u2", &u2), (b"u3", &u3)]);
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
        // Nothing from the batch was committed.
        assert_eq!(c.len().unwrap(), 0);
    }

    /// Concurrent writers contending for one unique value: at most one may
    /// win. The check now lives inside the write transaction, so contenders
    /// serialize instead of both observing "no conflict" pre-commit.
    #[test]
    fn concurrent_unique_inserts_allow_at_most_one_winner() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Arc::new(Db::open(&path).unwrap());
        {
            let c = db.collection("users");
            c.set_schema(&schema()).unwrap();
        }
        const THREADS: usize = 8;
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let key = format!("k{t}");
                db.collection("users")
                    .insert(
                        key.as_bytes(),
                        &doc(&[
                            ("name", Value::Text(format!("n{t}"))),
                            ("email", Value::Text("contested@x.com".into())),
                        ]),
                    )
                    .is_ok()
            }));
        }
        let winners: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            winners.iter().filter(|w| **w).count(),
            1,
            "exactly one contender may win the unique value"
        );
    }

    #[test]
    fn uniqueness_uses_scalar_index_when_present() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&schema()).unwrap();
        c.create_scalar_index("email").unwrap();
        c.insert(
            b"u1",
            &doc(&[
                ("name", Value::Text("a".into())),
                ("email", Value::Text("x@x.com".into())),
            ]),
        )
        .unwrap();
        let err = c.insert(
            b"u2",
            &doc(&[
                ("name", Value::Text("b".into())),
                ("email", Value::Text("x@x.com".into())),
            ]),
        );
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
    }

    /// Regression (audit A3): with a scalar index on a unique field whose values
    /// are not index-encodable (Bytes/Array/Map/Vector), the constraint was
    /// silently skipped. It must fall back to the scan comparison.
    #[test]
    fn unique_bytes_field_is_enforced_even_with_scalar_index() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&Schema::new().field(Field::new("blob", FieldType::Bytes).unique()))
            .unwrap();
        c.create_scalar_index("blob").unwrap();
        c.insert(b"u1", &doc(&[("blob", Value::Bytes(vec![1, 2, 3]))]))
            .unwrap();
        let err = c.insert(b"u2", &doc(&[("blob", Value::Bytes(vec![1, 2, 3]))]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "duplicate unique Bytes value must be rejected"
        );
        // A different value is still fine.
        c.insert(b"u3", &doc(&[("blob", Value::Bytes(vec![4]))]))
            .unwrap();
        assert_eq!(c.len().unwrap(), 2);
    }

    /// Regression (audit A3): NaN never conflicted with NaN on a unique Float
    /// field (IEEE `!=`). For uniqueness, NaN is the same stored value as NaN.
    #[test]
    fn unique_float_nan_conflicts_with_nan() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("users");
        c.set_schema(&Schema::new().field(Field::new("x", FieldType::Float).unique()))
            .unwrap();
        c.insert(b"a", &doc(&[("x", Value::Float(f64::NAN))]))
            .unwrap();
        let err = c.insert(b"b", &doc(&[("x", Value::Float(f64::NAN))]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "duplicate NaN on a unique field must be rejected"
        );
        // Still true when a scalar index exists (NaN is not bucket-walkable).
        c.create_scalar_index("x").unwrap();
        c.delete(b"a").unwrap();
        c.insert(b"c", &doc(&[("x", Value::Float(f64::NAN))]))
            .unwrap();
        let err = c.insert(b"d", &doc(&[("x", Value::Float(f64::NAN))]));
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
    }

    /// Regression (review round 1): NaN equality must reach inside containers —
    /// Vector (f32 elements) and Array (recursive Values) — not just top-level
    /// Float fields.
    #[test]
    fn unique_nan_inside_containers_conflicts() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("vecs");
        c.set_schema(&Schema::new().field(Field::new("v", FieldType::Vector).unique()))
            .unwrap();
        c.insert(b"a", &doc(&[("v", Value::Vector(vec![f32::NAN, 1.0]))]))
            .unwrap();
        let err = c.insert(b"b", &doc(&[("v", Value::Vector(vec![f32::NAN, 1.0]))]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "byte-identical vectors containing NaN must conflict"
        );
        // Negative control: a different vector is still fine.
        c.insert(b"c", &doc(&[("v", Value::Vector(vec![1.0, f32::NAN]))]))
            .unwrap();
        assert_eq!(c.len().unwrap(), 2);

        let c = db.collection("arrs");
        c.set_schema(&Schema::new().field(Field::new("a", FieldType::Array).unique()))
            .unwrap();
        c.insert(
            b"a",
            &doc(&[("a", Value::Array(vec![Value::Float(f64::NAN)]))]),
        )
        .unwrap();
        let err = c.insert(
            b"b",
            &doc(&[("a", Value::Array(vec![Value::Float(f64::NAN)]))]),
        );
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "equal arrays containing NaN must conflict"
        );

        let c = db.collection("maps");
        c.set_schema(&Schema::new().field(Field::new("m", FieldType::Map).unique()))
            .unwrap();
        let nan_map = || {
            let mut m = BTreeMap::new();
            m.insert("x".to_owned(), Value::Float(f64::NAN));
            Value::Map(m)
        };
        c.insert(b"a", &doc(&[("m", nan_map())])).unwrap();
        let err = c.insert(b"b", &doc(&[("m", nan_map())]));
        assert!(
            matches!(err, Err(Error::SchemaViolation(_))),
            "equal maps containing NaN must conflict"
        );
    }

    #[test]
    fn schema_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let db = Db::open(&path).unwrap();
            db.collection("users").set_schema(&schema()).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let err = db
            .collection("users")
            .insert(b"u1", &doc(&[("age", Value::Int(1))]));
        assert!(matches!(err, Err(Error::SchemaViolation(_))));
    }

    #[test]
    fn unschemaed_collection_is_unaffected() {
        let db = Db::open_in_memory().unwrap();
        // No schema → anything goes, including a non-map document.
        db.collection("free").insert(b"k", &Value::Int(42)).unwrap();
        assert_eq!(
            db.collection("free").get(b"k").unwrap(),
            Some(Value::Int(42))
        );
    }

    #[test]
    fn schema_round_trips_through_bytes() {
        let s = schema();
        assert_eq!(Schema::decode(&s.encode()).unwrap(), s);
    }

    /// Audit C1: a forged huge field count must not drive a huge allocation
    /// — capacity is clamped (migrate.rs precedent) and the decode fails
    /// cleanly on the truncated input instead of reserving gigabytes first.
    #[test]
    fn decode_clamps_forged_huge_field_count() {
        let mut b = u32::MAX.to_le_bytes().to_vec();
        b.push(0); // one field type byte, then nothing
        assert!(Schema::decode(&b).is_none());
    }
}
