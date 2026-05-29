//! Optional declared schema and constraints per collection.
//!
//! Collections are schemaless by default. A collection *may* declare a schema —
//! a set of fields with a type, and optional `required` / `unique` flags —
//! which is then enforced on every write to that collection. Collections
//! without a schema are entirely unaffected. The schema persists across reopen.
//!
//! Enforcement is opt-in and strict: a write that violates the schema fails
//! with [`Error::SchemaViolation`](crate::Error::SchemaViolation) and nothing
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
        let mut fields = Vec::with_capacity(n);
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
    /// one is declared. Checks types, required presence, and uniqueness.
    pub(crate) fn validate_schema(&self, collection: &str, key: &[u8], doc: &Value) -> Result<()> {
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
                    if f.unique && self.unique_conflict(collection, &f.name, v, key)? {
                        return Err(Error::SchemaViolation(format!(
                            "field '{}' must be unique; value already exists",
                            f.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether another document (key != `exclude`) already has `value` at
    /// `field`. Uses a scalar index when present, else a streaming scan that
    /// stops at the first conflict.
    fn unique_conflict(
        &self,
        collection: &str,
        field: &str,
        value: &Value,
        exclude: &[u8],
    ) -> Result<bool> {
        use crate::filter::CmpOp;
        if self.has_scalar_index(collection, field) {
            let cons = [crate::scalar::Constraint {
                op: CmpOp::Eq,
                value,
            }];
            if let Some(keys) = self.scalar_candidates(collection, field, &cons, usize::MAX)? {
                for k in keys {
                    if k != exclude
                        && let Some(doc) = self.collection(collection).get(&k)?
                        && doc.get_path(field) == Some(value)
                    {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
        }
        // Fallback: streaming scan with early stop.
        let mut conflict = false;
        self.collection(collection).for_each_doc(|k, doc| {
            if k != exclude && doc.get_path(field) == Some(value) {
                conflict = true;
                return Ok(false); // stop
            }
            Ok(true)
        })?;
        Ok(conflict)
    }
}

impl Collection<'_> {
    /// Declare (or replace) this collection's schema. Subsequent writes are
    /// validated against it; existing documents are left untouched (validation
    /// applies on write, not retroactively). Persists across reopen.
    pub fn set_schema(&self, schema: &Schema) -> Result<()> {
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
}
