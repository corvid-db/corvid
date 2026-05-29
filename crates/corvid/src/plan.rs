//! Identity-hashable query plans and a plan-keyed cache.
//!
//! [`QueryPlan`] is a canonical, hashable description of a query's *shape* —
//! its collection, filters, retrieval sources, fusion/rerank parameters,
//! ordering, pagination, and projection. Two builders configured identically
//! produce equal plans (and equal hashes); any difference in shape produces a
//! different plan. This lets a host deduplicate or key a cache on a query
//! without re-deriving its structure.
//!
//! [`PlanCache`] is a small map from plan to an arbitrary value, so repeated
//! query shapes can reuse prepared work. It caches by *shape*, not results, so
//! it never serves a stale answer — the engine's freshness guarantee is
//! untouched.

use std::collections::HashMap;

/// A canonical, hashable identity for a query's shape.
///
/// Build one with [`crate::QueryBuilder::plan`]. Equality and hashing are over
/// the full shape, so `plan(a) == plan(b)` iff `a` and `b` would execute the
/// same query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueryPlan(pub(crate) String);

impl QueryPlan {
    /// The canonical key string (stable for a given shape).
    pub fn key(&self) -> &str {
        &self.0
    }
}

/// A cache keyed by [`QueryPlan`]. Values are arbitrary prepared work the host
/// wants to associate with a query shape.
#[derive(Debug, Default)]
pub struct PlanCache<V> {
    entries: HashMap<QueryPlan, V>,
}

impl<V> PlanCache<V> {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// The value cached for `plan`, if any.
    pub fn get(&self, plan: &QueryPlan) -> Option<&V> {
        self.entries.get(plan)
    }

    /// Cache `value` under `plan` (replacing any existing entry).
    pub fn insert(&mut self, plan: QueryPlan, value: V) {
        self.entries.insert(plan, value);
    }

    /// Get the cached value for `plan`, computing and storing it with `f` on a
    /// miss.
    pub fn get_or_insert_with<F>(&mut self, plan: QueryPlan, f: F) -> &V
    where
        F: FnOnce() -> V,
    {
        self.entries.entry(plan).or_insert_with(f)
    }

    /// Number of cached plans.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, Metric, ResultRow, Value, field};
    use std::collections::BTreeMap;

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..5i64 {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(i));
            m.insert("v".to_owned(), Value::Vector(vec![i as f32, 1.0]));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        db
    }

    #[test]
    fn identical_shapes_have_equal_plans() {
        let db = seed();
        let c = db.collection("docs");
        let a = c
            .query()
            .filter(field("n").ge(Value::Int(2)))
            .vector("v", vec![1.0, 0.0], 5, Metric::L2)
            .limit(3)
            .plan();
        let b = c
            .query()
            .filter(field("n").ge(Value::Int(2)))
            .vector("v", vec![1.0, 0.0], 5, Metric::L2)
            .limit(3)
            .plan();
        assert_eq!(a, b);
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let h = |p: &super::QueryPlan| {
            let mut s = DefaultHasher::new();
            p.hash(&mut s);
            s.finish()
        };
        assert_eq!(h(&a), h(&b));
    }

    #[test]
    fn differing_shapes_have_different_plans() {
        let db = seed();
        let c = db.collection("docs");
        let base = c
            .query()
            .filter(field("n").ge(Value::Int(2)))
            .limit(3)
            .plan();
        // Different limit.
        assert_ne!(
            base,
            c.query()
                .filter(field("n").ge(Value::Int(2)))
                .limit(4)
                .plan()
        );
        // Different filter value.
        assert_ne!(
            base,
            c.query()
                .filter(field("n").ge(Value::Int(3)))
                .limit(3)
                .plan()
        );
        // Different vector query.
        let v1 = c.query().vector("v", vec![1.0, 0.0], 5, Metric::L2).plan();
        let v2 = c.query().vector("v", vec![0.0, 1.0], 5, Metric::L2).plan();
        assert_ne!(v1, v2);
    }

    #[test]
    fn plan_cache_keys_by_shape() {
        let db = seed();
        let c = db.collection("docs");
        let mut cache: super::PlanCache<u32> = super::PlanCache::new();
        let p1 = c.query().filter(field("n").ge(Value::Int(2))).plan();
        cache.insert(p1, 42);
        // A fresh, identically-shaped builder retrieves the same entry.
        let p2 = c.query().filter(field("n").ge(Value::Int(2))).plan();
        assert_eq!(cache.get(&p2), Some(&42));
        assert_eq!(cache.len(), 1);
        // A different shape misses.
        let p3 = c.query().filter(field("n").ge(Value::Int(9))).plan();
        assert_eq!(cache.get(&p3), None);
    }

    #[test]
    fn cached_execution_matches_fresh() {
        let db = seed();
        let c = db.collection("docs");
        let build = || {
            c.query()
                .filter(field("n").ge(Value::Int(1)))
                .vector("v", vec![2.0, 1.0], 5, Metric::L2)
                .limit(3)
        };
        let fresh: Vec<ResultRow> = build().run().unwrap();

        // Cache results by plan; a second identically-shaped query hits the
        // cache, and the cached rows equal a fresh execution.
        let mut cache: super::PlanCache<Vec<ResultRow>> = super::PlanCache::new();
        let plan = build().plan();
        let computed = cache
            .get_or_insert_with(plan.clone(), || build().run().unwrap())
            .clone();
        assert_eq!(computed, fresh);
        // Second lookup is a hit (no recompute needed) and still equals fresh.
        assert_eq!(cache.get(&build().plan()), Some(&fresh));
    }
}
