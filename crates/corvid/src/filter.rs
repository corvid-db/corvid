//! Filter predicates over documents.
//!
//! Predicates form a small tree built fluently — `field("score").gt(Value::Int(5))`
//! combined with [`Predicate::and`] / [`Predicate::or`] / [`Predicate::not`].
//! They evaluate against a [`Value`] document and are the `filter` arm of the
//! query builder. Field paths are dotted and traverse nested maps
//! (`"meta.author"`).

use std::cmp::Ordering;

use crate::value::Value;

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

/// A filter predicate evaluated against a document.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// Compare the value at a dotted path against a constant.
    Compare {
        /// Dotted field path, e.g. `"meta.author"`.
        path: String,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand constant.
        value: Value,
    },
    /// True when the path resolves to a present value.
    Exists(String),
    /// Logical conjunction.
    And(Box<Predicate>, Box<Predicate>),
    /// Logical disjunction.
    Or(Box<Predicate>, Box<Predicate>),
    /// Logical negation.
    Not(Box<Predicate>),
}

impl Predicate {
    /// Combine with another predicate under logical AND.
    pub fn and(self, other: Predicate) -> Predicate {
        Predicate::And(Box::new(self), Box::new(other))
    }

    /// Combine with another predicate under logical OR.
    pub fn or(self, other: Predicate) -> Predicate {
        Predicate::Or(Box::new(self), Box::new(other))
    }

    /// Evaluate the predicate against a document.
    ///
    /// A comparison whose path is missing is `false`. Ordered comparisons
    /// between values that are not order-comparable (different or unordered
    /// types) are also `false`. Use [`Predicate::Exists`] for presence.
    pub fn eval(&self, doc: &Value) -> bool {
        match self {
            Predicate::Compare { path, op, value } => match resolve(doc, path) {
                None => false,
                Some(found) => compare(found, *op, value),
            },
            Predicate::Exists(path) => resolve(doc, path).is_some(),
            Predicate::And(a, b) => a.eval(doc) && b.eval(doc),
            Predicate::Or(a, b) => a.eval(doc) || b.eval(doc),
            Predicate::Not(p) => !p.eval(doc),
        }
    }
}

impl std::ops::Not for Predicate {
    type Output = Predicate;

    /// Negate the predicate: `!field("score").gt(Value::Int(5))`.
    fn not(self) -> Predicate {
        Predicate::Not(Box::new(self))
    }
}

/// Start building a predicate on the given dotted field path.
pub fn field(path: impl Into<String>) -> FieldRef {
    FieldRef { path: path.into() }
}

/// A reference to a field path, used to build [`Predicate`]s fluently.
pub struct FieldRef {
    path: String,
}

impl FieldRef {
    /// `path == value`.
    pub fn eq(self, value: Value) -> Predicate {
        self.compare(CmpOp::Eq, value)
    }
    /// `path != value`.
    pub fn ne(self, value: Value) -> Predicate {
        self.compare(CmpOp::Ne, value)
    }
    /// `path < value`.
    pub fn lt(self, value: Value) -> Predicate {
        self.compare(CmpOp::Lt, value)
    }
    /// `path <= value`.
    pub fn le(self, value: Value) -> Predicate {
        self.compare(CmpOp::Le, value)
    }
    /// `path > value`.
    pub fn gt(self, value: Value) -> Predicate {
        self.compare(CmpOp::Gt, value)
    }
    /// `path >= value`.
    pub fn ge(self, value: Value) -> Predicate {
        self.compare(CmpOp::Ge, value)
    }
    /// The path resolves to a present value.
    pub fn exists(self) -> Predicate {
        Predicate::Exists(self.path)
    }

    fn compare(self, op: CmpOp, value: Value) -> Predicate {
        Predicate::Compare {
            path: self.path,
            op,
            value,
        }
    }
}

/// Resolve a dotted path through nested maps.
fn resolve<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = doc;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Apply a comparison operator between a found value and a constant.
fn compare(found: &Value, op: CmpOp, rhs: &Value) -> bool {
    match op {
        CmpOp::Eq => found == rhs,
        CmpOp::Ne => found != rhs,
        CmpOp::Lt => order(found, rhs) == Some(Ordering::Less),
        CmpOp::Le => matches!(order(found, rhs), Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => order(found, rhs) == Some(Ordering::Greater),
        CmpOp::Ge => matches!(order(found, rhs), Some(Ordering::Greater | Ordering::Equal)),
    }
}

/// Total-ish ordering between two values, when meaningfully comparable.
///
/// Numbers compare numerically (ints and floats interoperate), text compares
/// lexically. Everything else (bools, containers, mixed kinds) is unordered.
fn order(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc() -> Value {
        let mut meta = BTreeMap::new();
        meta.insert("author".to_owned(), Value::Text("rocky".to_owned()));
        let mut m = BTreeMap::new();
        m.insert("category".to_owned(), Value::Text("blog".to_owned()));
        m.insert("score".to_owned(), Value::Int(7));
        m.insert("ratio".to_owned(), Value::Float(0.5));
        m.insert("meta".to_owned(), Value::Map(meta));
        Value::Map(m)
    }

    #[test]
    fn eq_and_ne() {
        assert!(
            field("category")
                .eq(Value::Text("blog".into()))
                .eval(&doc())
        );
        assert!(
            !field("category")
                .eq(Value::Text("news".into()))
                .eval(&doc())
        );
        assert!(
            field("category")
                .ne(Value::Text("news".into()))
                .eval(&doc())
        );
    }

    #[test]
    fn ordered_int_comparisons() {
        assert!(field("score").gt(Value::Int(5)).eval(&doc()));
        assert!(field("score").ge(Value::Int(7)).eval(&doc()));
        assert!(field("score").lt(Value::Int(8)).eval(&doc()));
        assert!(field("score").le(Value::Int(7)).eval(&doc()));
        assert!(!field("score").gt(Value::Int(7)).eval(&doc()));
    }

    #[test]
    fn numeric_int_float_interop() {
        assert!(field("score").gt(Value::Float(6.5)).eval(&doc()));
        assert!(field("ratio").lt(Value::Int(1)).eval(&doc()));
    }

    #[test]
    fn text_ordering() {
        assert!(field("category").lt(Value::Text("c".into())).eval(&doc()));
        assert!(field("category").gt(Value::Text("a".into())).eval(&doc()));
    }

    #[test]
    fn missing_field_compares_false() {
        assert!(!field("absent").eq(Value::Int(1)).eval(&doc()));
        assert!(!field("absent").gt(Value::Int(1)).eval(&doc()));
        assert!(!field("absent").ne(Value::Int(1)).eval(&doc()));
    }

    #[test]
    fn unordered_types_compare_false() {
        // Comparing bool with an ordered op is not meaningful.
        let mut m = BTreeMap::new();
        m.insert("flag".to_owned(), Value::Bool(true));
        let d = Value::Map(m);
        assert!(!field("flag").gt(Value::Bool(false)).eval(&d));
        // eq still works on bools.
        assert!(field("flag").eq(Value::Bool(true)).eval(&d));
    }

    #[test]
    fn dotted_path_traverses_nested_maps() {
        assert!(
            field("meta.author")
                .eq(Value::Text("rocky".into()))
                .eval(&doc())
        );
        assert!(!field("meta.missing").exists().eval(&doc()));
        assert!(field("meta.author").exists().eval(&doc()));
    }

    #[test]
    fn exists_predicate() {
        assert!(field("score").exists().eval(&doc()));
        assert!(!field("nope").exists().eval(&doc()));
    }

    #[test]
    fn and_or_not_combinators() {
        let p = field("category")
            .eq(Value::Text("blog".into()))
            .and(field("score").gt(Value::Int(5)));
        assert!(p.eval(&doc()));

        let p = field("category")
            .eq(Value::Text("news".into()))
            .or(field("score").gt(Value::Int(5)));
        assert!(p.eval(&doc()));

        let p = !field("score").gt(Value::Int(100));
        assert!(p.eval(&doc()));
    }

    #[test]
    fn not_operator_negates() {
        assert!((!field("score").gt(Value::Int(100))).eval(&doc()));
        assert!(!(!field("score").gt(Value::Int(5))).eval(&doc()));
    }

    #[test]
    fn and_short_circuit_false() {
        let p = field("score")
            .gt(Value::Int(100))
            .and(field("category").eq(Value::Text("blog".into())));
        assert!(!p.eval(&doc()));
    }

    #[test]
    fn path_through_non_map_is_none() {
        // "score" is an int; descending further yields nothing.
        assert!(!field("score.deeper").exists().eval(&doc()));
    }
}
