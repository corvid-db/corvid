//! The fluent multi-modal query builder (engine layer L4).
//!
//! This is the engine's identity: one chained call composes filtering, vector
//! search, text search, rank fusion, and diversifying rerank into a single
//! result set. Example:
//!
//! ```
//! use corvid::{Db, Metric, Value, field};
//!
//! let db = Db::open_in_memory().unwrap();
//! // ... insert documents with "embedding" and "body" fields ...
//! let rows = db
//!     .collection("docs")
//!     .query()
//!     .filter(field("category").eq(Value::Text("blog".into())))
//!     .vector("embedding", vec![1.0, 0.0], 100, Metric::Cosine)
//!     .text("body", "rust embedded database", 100)
//!     .rerank_mmr(0.7)
//!     .limit(10)
//!     .run()
//!     .unwrap();
//! assert!(rows.is_empty()); // empty db
//! ```
//!
//! ## Execution model
//!
//! 1. Scan the collection once and apply every `filter` predicate. Filtering
//!    happens *before* ranking, so `filter` is a true predicate over the
//!    corpus — top-k is computed among matching documents, never post-hoc.
//! 2. Rank the filtered set independently for each retrieval source (each
//!    capped at its own `k`).
//! 3. Fuse the per-source rankings with Reciprocal Rank Fusion (a single
//!    source passes through unchanged; zero sources yields the filtered set in
//!    key order).
//! 4. Optionally reorder by Maximal Marginal Relevance, using the first vector
//!    source's query, field, and metric. Candidates lacking a usable embedding
//!    keep their fused order after the reranked ones.
//! 5. Truncate to `limit`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use crate::db::Collection;
use crate::distance::Metric;
use crate::error::Result;
use crate::filter::{CmpOp, Predicate};
use crate::fusion::{DEFAULT_RRF_K, mmr, reciprocal_rank_fusion};
use crate::query::{doc_map, ranked_bm25, ranked_vector};
use crate::value::Value;

/// A set of candidate `(key, document)` pairs.
type Candidates = Vec<(Vec<u8>, Value)>;

/// One row of a query result.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultRow {
    /// The document's key within the collection.
    pub key: Vec<u8>,
    /// The rank score (RRF fused score, or `0.0` for a pure filter query).
    pub score: f32,
    /// The full stored document.
    pub document: Value,
}

/// A retrieval source feeding the fusion step.
enum Source {
    Vector {
        field: String,
        query: Vec<f32>,
        k: usize,
        metric: Metric,
    },
    Text {
        field: String,
        query: String,
        k: usize,
    },
}

/// A composable multi-modal query over one collection. Built fluently and
/// executed with [`QueryBuilder::run`].
pub struct QueryBuilder<'c> {
    collection: Collection<'c>,
    filters: Vec<Predicate>,
    sources: Vec<Source>,
    rrf_k: f32,
    mmr_lambda: Option<f32>,
    limit: Option<usize>,
    offset: usize,
    order_by: Option<(String, bool)>,
    projection: Option<Vec<String>>,
    approx: bool,
}

impl<'c> Collection<'c> {
    /// Begin a fluent multi-modal query over this collection.
    pub fn query(&self) -> QueryBuilder<'c> {
        QueryBuilder {
            collection: *self,
            filters: Vec::new(),
            sources: Vec::new(),
            rrf_k: DEFAULT_RRF_K,
            mmr_lambda: None,
            limit: None,
            offset: 0,
            order_by: None,
            projection: None,
            approx: false,
        }
    }
}

impl QueryBuilder<'_> {
    /// Restrict results to documents matching `predicate`. Multiple calls are
    /// combined with logical AND.
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.filters.push(predicate);
        self
    }

    /// Add a vector-search source over `field` for `query`, contributing up to
    /// `k` candidates.
    pub fn vector(
        mut self,
        field: impl Into<String>,
        query: Vec<f32>,
        k: usize,
        metric: Metric,
    ) -> Self {
        self.sources.push(Source::Vector {
            field: field.into(),
            query,
            k,
            metric,
        });
        self
    }

    /// Add a text-search source over `field` for `query`, contributing up to
    /// `k` candidates.
    pub fn text(mut self, field: impl Into<String>, query: impl Into<String>, k: usize) -> Self {
        self.sources.push(Source::Text {
            field: field.into(),
            query: query.into(),
            k,
        });
        self
    }

    /// Set the Reciprocal Rank Fusion constant (default [`DEFAULT_RRF_K`]).
    pub fn fuse_rrf(mut self, k: f32) -> Self {
        self.rrf_k = k;
        self
    }

    /// Diversify results with Maximal Marginal Relevance. `lambda` in `[0, 1]`
    /// trades relevance (1.0) against diversity (0.0). Requires a vector source
    /// to supply the query and metric; without one this is a no-op.
    pub fn rerank_mmr(mut self, lambda: f32) -> Self {
        self.mmr_lambda = Some(lambda);
        self
    }

    /// Limit the result to at most `n` rows.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows (pagination). Applied after ordering, before
    /// `limit`.
    pub fn offset(mut self, n: usize) -> Self {
        self.offset = n;
        self
    }

    /// Order results by a scalar field instead of by rank. `descending`
    /// reverses the order. Rows missing the field (or with an incomparable
    /// value) sort to the end. Comparable values: int/float (numeric), text
    /// (lexical).
    pub fn order_by(mut self, field: impl Into<String>, descending: bool) -> Self {
        self.order_by = Some((field.into(), descending));
        self
    }

    /// Allow approximate execution: when a single vector source has a matching
    /// ANN index, use it even if filters are present (over-fetch then filter).
    /// Faster, but a highly selective filter may return fewer than `limit`
    /// rows. Without this, filtered vector queries run exactly (full scan).
    pub fn approx(mut self) -> Self {
        self.approx = true;
        self
    }

    /// Project each result document down to the named top-level fields.
    ///
    /// Only applies to [`Value::Map`] documents; missing fields are simply
    /// absent from the projection, and non-map documents are returned
    /// unchanged. Ranking and filtering still see the full document — only the
    /// returned `document` is narrowed.
    pub fn select<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.projection = Some(fields.into_iter().map(Into::into).collect());
        self
    }

    /// Count the documents matching the filters. Retrieval sources, ranking,
    /// limit, and projection are ignored — this is an aggregate over the
    /// filtered set.
    pub fn count(self) -> Result<usize> {
        // No filters → use the maintained O(1) counter.
        if self.filters.is_empty() {
            return self.collection.len();
        }
        // Scalar-index fast path: count verified candidates without a full scan.
        if let Some(matched) = self.indexed_candidates()? {
            return Ok(matched.len());
        }
        let mut n = 0usize;
        self.collection.for_each_doc(|_, doc| {
            if self.filters.iter().all(|p| p.eval(&doc)) {
                n += 1;
            }
            Ok(true)
        })?;
        Ok(n)
    }

    /// Count matching documents grouped by the value at `field`.
    ///
    /// Groups are keyed by a canonical string of the field value (text as-is;
    /// int/float/bool stringified). Documents whose field is missing or is a
    /// container are not counted. Like [`Self::count`], this aggregates over
    /// the filtered set and ignores ranking.
    pub fn group_count(self, field: &str) -> Result<BTreeMap<String, usize>> {
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        self.collection.for_each_doc(|_, doc| {
            if self.filters.iter().all(|p| p.eval(&doc))
                && let Some(key) = doc.get(field).and_then(group_key)
            {
                *groups.entry(key).or_insert(0) += 1;
            }
            Ok(true)
        })?;
        Ok(groups)
    }

    /// Try the ANN fast path: a single vector source whose field/metric has a
    /// registered index. Returns the (already filtered) candidate set, or
    /// `None` to fall back to an exact scan. Filtered queries only take this
    /// path under [`Self::approx`].
    fn ann_candidates(&self) -> Result<Option<Candidates>> {
        if self.sources.len() != 1 {
            return Ok(None);
        }
        let Source::Vector {
            field,
            query,
            k,
            metric,
        } = &self.sources[0]
        else {
            return Ok(None);
        };
        if !self.filters.is_empty() && !self.approx {
            return Ok(None);
        }
        let Some(ranked) =
            self.collection
                .db()
                .ann_search(self.collection.name(), field, query, *k, *metric)?
        else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for (key, _dist) in ranked {
            if let Some(doc) = self.collection.get(&key)?
                && self.filters.iter().all(|p| p.eval(&doc))
            {
                out.push((key, doc));
            }
        }
        Ok(Some(out))
    }

    /// Try the scalar-index fast path: if a top-level AND filter is an
    /// equality or range comparison on a field with a scalar index, fetch only
    /// the candidate documents (a superset) and verify every filter against
    /// each. Returns the filtered set, or `None` to fall back to a full scan.
    ///
    /// An equality predicate is preferred (most selective); otherwise the first
    /// range predicate is used.
    fn indexed_candidates(&self) -> Result<Option<Candidates>> {
        if self.filters.is_empty() {
            return Ok(None);
        }
        let db = self.collection.db();
        let coll = self.collection.name();

        // Choose the indexed field to drive the scan: prefer one carrying an
        // equality (tightest), else one carrying a range. All top-level AND
        // comparisons on that field are combined into a single bounded window.
        let mut target: Option<&str> = None;
        let mut best_eq = false;
        for pred in &self.filters {
            let Predicate::Compare { path, op, .. } = pred else {
                continue;
            };
            if !matches!(
                op,
                CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
            ) || !db.has_scalar_index(coll, path)
            {
                continue;
            }
            let is_eq = *op == CmpOp::Eq;
            if target.is_none() || (is_eq && !best_eq) {
                target = Some(path);
                best_eq = is_eq;
            }
        }
        let Some(field) = target else {
            return Ok(None);
        };

        let constraints: Vec<crate::scalar::Constraint<'_>> = self
            .filters
            .iter()
            .filter_map(|p| match p {
                Predicate::Compare { path, op, value } if path == field => {
                    Some(crate::scalar::Constraint { op: *op, value })
                }
                _ => None,
            })
            .collect();

        // Cap candidates so a low-selectivity filter falls back to a bounded
        // scan instead of materialising a huge set in memory.
        const CANDIDATE_CAP: usize = 100_000;
        let Some(keys) = db.scalar_candidates(coll, field, &constraints, CANDIDATE_CAP)? else {
            return Ok(None);
        };

        let mut out = Vec::new();
        for key in keys {
            if let Some(doc) = self.collection.get(&key)?
                && self.filters.iter().all(|p| p.eval(&doc))
            {
                out.push((key, doc));
            }
        }
        Ok(Some(out))
    }

    /// Describe the query plan as a human-readable string (for debugging). Does
    /// not execute the query, so it may be called before [`Self::run`].
    pub fn explain(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("scan({})", self.collection.name()));
        if !self.filters.is_empty() {
            parts.push(format!("filter x{}", self.filters.len()));
        }
        for src in &self.sources {
            match src {
                Source::Vector {
                    field, k, metric, ..
                } => parts.push(format!("vector({field}, k={k}, {metric:?})")),
                Source::Text { field, k, .. } => parts.push(format!("text({field}, k={k})")),
            }
        }
        if self.sources.len() > 1 {
            parts.push(format!("rrf(k={})", self.rrf_k));
        }
        if let Some(l) = self.mmr_lambda {
            parts.push(format!("mmr(lambda={l})"));
        }
        if self.approx {
            parts.push("approx".to_owned());
        }
        if let Some((f, desc)) = &self.order_by {
            parts.push(format!("order_by({f}{})", if *desc { " desc" } else { "" }));
        }
        if self.offset > 0 {
            parts.push(format!("offset {}", self.offset));
        }
        if let Some(l) = self.limit {
            parts.push(format!("limit {l}"));
        }
        if let Some(p) = &self.projection {
            parts.push(format!("select [{}]", p.join(", ")));
        }
        parts.join(" | ")
    }

    /// Execute the query and return the ranked rows.
    pub fn run(self) -> Result<Vec<ResultRow>> {
        // No retrieval sources → a pure filter/order/paginate query. Stream it
        // with bounded memory instead of materializing the whole collection.
        if self.sources.is_empty() {
            return self.run_scan_only();
        }

        // Pick the narrowest available source for the filtered set: the ANN
        // index (vector queries), else a scalar index, else a full scan.
        let filtered: Vec<(Vec<u8>, Value)> = match self.ann_candidates()? {
            Some(candidates) => candidates,
            None => match self.indexed_candidates()? {
                Some(candidates) => candidates,
                None => self
                    .collection
                    .scan()?
                    .into_iter()
                    .filter(|(_, doc)| self.filters.iter().all(|p| p.eval(doc)))
                    .collect(),
            },
        };

        // 2. Rank the filtered set per source.
        let rankings: Vec<Vec<Vec<u8>>> = self
            .sources
            .iter()
            .map(|src| keys_for(src, &filtered))
            .collect();

        // 3. Fuse (or pass through the filtered set if there are no sources).
        let fused: Vec<(Vec<u8>, f32)> = if self.sources.is_empty() {
            filtered.iter().map(|(k, _)| (k.clone(), 0.0)).collect()
        } else {
            let refs: Vec<&[Vec<u8>]> = rankings.iter().map(Vec::as_slice).collect();
            reciprocal_rank_fusion(&refs, self.rrf_k)
        };

        let mut docs = doc_map(filtered);

        // 4. Optional MMR rerank, anchored on the first vector source.
        let ordered = match (self.mmr_lambda, self.first_vector_source()) {
            (Some(lambda), Some((field, query, metric))) => {
                rerank_mmr(&fused, &docs, field, query, lambda, metric)
            }
            _ => fused,
        };

        // 5. Optional ORDER BY a field (replaces rank order), then paginate.
        let mut ordered = ordered;
        if let Some((field, descending)) = &self.order_by {
            ordered.sort_by(|(ka, _), (kb, _)| {
                let va = docs.get(ka).and_then(|d| d.get(field));
                let vb = docs.get(kb).and_then(|d| d.get(field));
                match (va, vb) {
                    (Some(a), Some(b)) => {
                        let base = crate::filter::value_order(a, b).unwrap_or(Ordering::Equal);
                        let base = if *descending { base.reverse() } else { base };
                        base.then_with(|| ka.cmp(kb))
                    }
                    (Some(_), None) => Ordering::Less, // present sorts before missing
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => ka.cmp(kb),
                }
            });
        }

        // 6. Offset + limit, then assemble.
        let start = self.offset.min(ordered.len());
        let mut ordered = ordered.split_off(start);
        if let Some(limit) = self.limit {
            ordered.truncate(limit);
        }
        Ok(ordered
            .into_iter()
            .map(|(key, score)| {
                let document = docs.remove(&key).expect("key came from filtered set");
                let document = match &self.projection {
                    Some(fields) => project(document, fields),
                    None => document,
                };
                ResultRow {
                    key,
                    score,
                    document,
                }
            })
            .collect())
    }

    /// Streaming execution for the no-retrieval-source case (filter / order /
    /// paginate). Memory is bounded: with `limit`, at most ~`offset + limit`
    /// rows are held (early-stop in key order; a periodically-pruned buffer
    /// under `order_by`). Without `limit` and with `order_by`, all matching
    /// rows are held (an unbounded sort, as any DB without a sort index does).
    fn run_scan_only(self) -> Result<Vec<ResultRow>> {
        // Scalar-index fast path: fetch only candidate documents instead of
        // scanning the whole collection, then order/paginate in memory (the set
        // is bounded by the number of matches).
        if let Some(mut matched) = self.indexed_candidates()? {
            if let Some((field, descending)) = &self.order_by {
                sort_by_field(&mut matched, field, *descending);
            } else {
                matched.sort_by(|(ka, _), (kb, _)| ka.cmp(kb));
            }
            let start = self.offset.min(matched.len());
            let mut window = matched.split_off(start);
            if let Some(limit) = self.limit {
                window.truncate(limit);
            }
            return Ok(window
                .into_iter()
                .map(|(key, document)| {
                    let document = match &self.projection {
                        Some(fields) => project(document, fields),
                        None => document,
                    };
                    ResultRow {
                        key,
                        score: 0.0,
                        document,
                    }
                })
                .collect());
        }

        let cap = self.limit.map(|l| self.offset.saturating_add(l));
        let mut buf: Vec<(Vec<u8>, Value)> = Vec::new();

        match &self.order_by {
            // Key order: take only the `cap` window, stopping early.
            None => {
                self.collection.for_each_doc(|key, doc| {
                    if self.filters.iter().all(|p| p.eval(&doc)) {
                        buf.push((key.to_vec(), doc));
                    }
                    Ok(cap.is_none_or(|c| buf.len() < c))
                })?;
            }
            // Ordered: keep the best `cap` via a periodically pruned buffer.
            Some((field, descending)) => {
                let prune_at = cap.map(|c| c.saturating_mul(2).max(1024));
                self.collection.for_each_doc(|key, doc| {
                    if self.filters.iter().all(|p| p.eval(&doc)) {
                        buf.push((key.to_vec(), doc));
                        if let (Some(p), Some(c)) = (prune_at, cap)
                            && buf.len() >= p
                        {
                            sort_by_field(&mut buf, field, *descending);
                            buf.truncate(c);
                        }
                    }
                    Ok(true)
                })?;
                sort_by_field(&mut buf, field, *descending);
            }
        }

        let start = self.offset.min(buf.len());
        let mut window: Vec<(Vec<u8>, Value)> = buf.split_off(start);
        if let Some(limit) = self.limit {
            window.truncate(limit);
        }
        Ok(window
            .into_iter()
            .map(|(key, document)| {
                let document = match &self.projection {
                    Some(fields) => project(document, fields),
                    None => document,
                };
                ResultRow {
                    key,
                    score: 0.0,
                    document,
                }
            })
            .collect())
    }

    /// The first vector source's `(field, query, metric)`, if any.
    fn first_vector_source(&self) -> Option<(&str, &[f32], Metric)> {
        self.sources.iter().find_map(|s| match s {
            Source::Vector {
                field,
                query,
                metric,
                ..
            } => Some((field.as_str(), query.as_slice(), *metric)),
            Source::Text { .. } => None,
        })
    }
}

/// A canonical group key for a scalar value, or `None` for containers/null.
fn group_key(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Sort `(key, doc)` pairs by a scalar field, missing/incomparable last, ties
/// by key. `descending` reverses the value comparison.
fn sort_by_field(buf: &mut [(Vec<u8>, Value)], field: &str, descending: bool) {
    buf.sort_by(|(ka, da), (kb, db)| match (da.get(field), db.get(field)) {
        (Some(a), Some(b)) => {
            let base = crate::filter::value_order(a, b).unwrap_or(Ordering::Equal);
            let base = if descending { base.reverse() } else { base };
            base.then_with(|| ka.cmp(kb))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => ka.cmp(kb),
    });
}

/// Narrow a document to the named field paths, which may be dotted
/// (`"meta.author"`); the projected structure is rebuilt nested. Missing paths
/// are omitted. Non-map documents pass through unchanged.
fn project(document: Value, fields: &[String]) -> Value {
    if !matches!(document, Value::Map(_)) {
        return document;
    }
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for path in fields {
        if let Some(value) = resolve_path(&document, path) {
            insert_path(&mut out, path, value.clone());
        }
    }
    Value::Map(out)
}

/// Resolve a dotted path through nested maps.
fn resolve_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = doc;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Insert `value` at a dotted `path`, creating intermediate maps.
fn insert_path(out: &mut BTreeMap<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cursor = out;
    for seg in &parts[..parts.len() - 1] {
        let entry = cursor
            .entry((*seg).to_owned())
            .or_insert_with(|| Value::Map(BTreeMap::new()));
        if !matches!(entry, Value::Map(_)) {
            *entry = Value::Map(BTreeMap::new());
        }
        cursor = match entry {
            Value::Map(m) => m,
            _ => unreachable!("just set to map"),
        };
    }
    cursor.insert(parts[parts.len() - 1].to_owned(), value);
}

/// Rank the filtered set for one source and return its capped key list.
fn keys_for(src: &Source, filtered: &[(Vec<u8>, Value)]) -> Vec<Vec<u8>> {
    let mut ranked = match src {
        Source::Vector {
            field,
            query,
            metric,
            ..
        } => ranked_vector(filtered, field, query, *metric),
        Source::Text { field, query, .. } => ranked_bm25(filtered, field, query),
    };
    let k = match src {
        Source::Vector { k, .. } | Source::Text { k, .. } => *k,
    };
    ranked.truncate(k);
    ranked.into_iter().map(|(key, _)| key).collect()
}

/// Reorder a fused result by MMR, keeping fused scores. Candidates whose
/// `field` lacks an embedding of `query`'s dimension keep their fused order
/// after the reranked ones.
fn rerank_mmr(
    fused: &[(Vec<u8>, f32)],
    docs: &HashMap<Vec<u8>, Value>,
    field: &str,
    query: &[f32],
    lambda: f32,
    metric: Metric,
) -> Vec<(Vec<u8>, f32)> {
    let scores: HashMap<&[u8], f32> = fused.iter().map(|(k, s)| (k.as_slice(), *s)).collect();

    let mut with_vec: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
    let mut tail: Vec<Vec<u8>> = Vec::new();
    for (key, _) in fused {
        match docs
            .get(key)
            .and_then(|d| d.get(field))
            .and_then(Value::as_vector)
        {
            Some(v) if v.len() == query.len() => with_vec.push((key.clone(), v.to_vec())),
            _ => tail.push(key.clone()),
        }
    }

    let order = mmr(query, &with_vec, lambda, with_vec.len(), metric);
    let mut out: Vec<(Vec<u8>, f32)> = Vec::with_capacity(fused.len());
    for key in order.into_iter().chain(tail) {
        let score = scores[key.as_slice()];
        out.push((key, score));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, field};
    use std::collections::BTreeMap;

    fn doc(category: &str, body: &str, embedding: Vec<f32>) -> Value {
        let mut m = BTreeMap::new();
        m.insert("category".to_owned(), Value::Text(category.to_owned()));
        m.insert("body".to_owned(), Value::Text(body.to_owned()));
        m.insert("embedding".to_owned(), Value::Vector(embedding));
        Value::Map(m)
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("blog", "rust embedded database", vec![1.0, 0.0]))
            .unwrap();
        c.insert(b"b", &doc("blog", "python web framework", vec![0.0, 1.0]))
            .unwrap();
        c.insert(
            b"c",
            &doc("news", "rust systems programming", vec![0.9, 0.1]),
        )
        .unwrap();
        db
    }

    #[test]
    fn filter_only_returns_matching_docs_in_key_order() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .run()
            .unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(rows.iter().all(|r| r.score == 0.0));
    }

    #[test]
    fn vector_only_ranks_by_distance() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        // a is exact match, c is close, b is orthogonal.
        assert_eq!(rows[0].key, b"a".to_vec());
        assert_eq!(rows[1].key, b"c".to_vec());
    }

    #[test]
    fn filter_constrains_vector_search() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        // c is nearest after a, but it's "news" so it's filtered out.
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn hybrid_fuses_vector_and_text() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .text("body", "rust", 10)
            .run()
            .unwrap();
        // "a" is both nearest by vector and contains "rust" → ranked first.
        assert_eq!(rows[0].key, b"a".to_vec());
        assert!(rows.len() >= 2);
    }

    #[test]
    fn limit_truncates_results() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .limit(1)
            .run()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, b"a".to_vec());
    }

    #[test]
    fn mmr_rerank_diversifies() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"dup1", &doc("x", "alpha", vec![1.0, 0.0]))
            .unwrap();
        c.insert(b"dup2", &doc("x", "beta", vec![0.99, 0.01]))
            .unwrap();
        c.insert(b"div", &doc("x", "gamma", vec![0.0, 1.0]))
            .unwrap();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .rerank_mmr(0.5)
            .run()
            .unwrap();
        // After the top match, MMR prefers the diverse doc over the near-dup.
        assert_eq!(rows[0].key, b"dup1".to_vec());
        assert_eq!(rows[1].key, b"div".to_vec());
    }

    #[test]
    fn mmr_without_vector_source_is_noop() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .text("body", "rust", 10)
            .rerank_mmr(0.5)
            .run()
            .unwrap();
        // Still returns the text matches; rerank had no anchor so order stands.
        assert!(rows.iter().any(|r| r.key == b"a".to_vec()));
        assert!(rows.iter().any(|r| r.key == b"c".to_vec()));
    }

    #[test]
    fn empty_query_returns_whole_collection() {
        let db = seed();
        let rows = db.collection("docs").query().run().unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn combined_filters_are_anded() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .filter(field("body").exists())
            .run()
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn custom_rrf_constant_is_accepted() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .text("body", "rust", 10)
            .fuse_rrf(10.0)
            .run()
            .unwrap();
        assert_eq!(rows[0].key, b"a".to_vec());
    }

    #[test]
    fn select_projects_documents_to_named_fields() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .select(["category"])
            .run()
            .unwrap();
        for row in &rows {
            let map = match &row.document {
                Value::Map(m) => m,
                _ => panic!("expected map"),
            };
            assert_eq!(map.len(), 1);
            assert!(map.contains_key("category"));
            assert!(!map.contains_key("body"));
        }
    }

    #[test]
    fn select_supports_nested_paths() {
        let db = Db::open_in_memory().unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("author".to_owned(), Value::Text("rocky".into()));
        meta.insert("year".to_owned(), Value::Int(2026));
        let mut m = BTreeMap::new();
        m.insert("title".to_owned(), Value::Text("hi".into()));
        m.insert("meta".to_owned(), Value::Map(meta));
        db.collection("docs").insert(b"k", &Value::Map(m)).unwrap();

        let rows = db
            .collection("docs")
            .query()
            .select(["title", "meta.author"])
            .run()
            .unwrap();
        let mut expected_meta = BTreeMap::new();
        expected_meta.insert("author".to_owned(), Value::Text("rocky".into()));
        let mut expected = BTreeMap::new();
        expected.insert("title".to_owned(), Value::Text("hi".into()));
        expected.insert("meta".to_owned(), Value::Map(expected_meta));
        assert_eq!(rows[0].document, Value::Map(expected));
    }

    #[test]
    fn explain_describes_the_plan() {
        let db = seed();
        let q = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .order_by("category", true)
            .limit(5);
        let plan = q.explain();
        assert!(plan.contains("scan(docs)"));
        assert!(plan.contains("filter x1"));
        assert!(plan.contains("vector(embedding"));
        assert!(plan.contains("order_by(category desc)"));
        assert!(plan.contains("limit 5"));
    }

    #[test]
    fn select_missing_field_yields_empty_map() {
        let db = seed();
        let rows = db
            .collection("docs")
            .query()
            .select(["does_not_exist"])
            .limit(1)
            .run()
            .unwrap();
        assert_eq!(rows[0].document, Value::Map(BTreeMap::new()));
    }

    #[test]
    fn select_leaves_non_map_documents_unchanged() {
        let db = Db::open_in_memory().unwrap();
        db.collection("docs").insert(b"v", &Value::Int(42)).unwrap();
        let rows = db
            .collection("docs")
            .query()
            .select(["anything"])
            .run()
            .unwrap();
        assert_eq!(rows[0].document, Value::Int(42));
    }

    #[test]
    fn builder_uses_ann_index_for_vector_only_query() {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        let rows = c
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        // Same top result as the exact path, now served via the index.
        assert_eq!(rows[0].key, b"a".to_vec());
    }

    #[test]
    fn approx_filtered_vector_query_uses_index_and_respects_filter() {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        let rows = c
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .approx()
            .run()
            .unwrap();
        // c (news) is nearest after a, but filtered out; only blog docs remain.
        assert!(
            rows.iter()
                .all(|r| r.document.get("category") == Some(&Value::Text("blog".into())))
        );
        assert_eq!(rows[0].key, b"a".to_vec());
    }

    #[test]
    fn filtered_vector_query_without_approx_is_exact_but_correct() {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        // No .approx(): exact path, still correct, still filtered.
        let rows = c
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        assert_eq!(rows[0].key, b"a".to_vec());
        assert!(!rows.iter().any(|r| r.key == b"c".to_vec()));
    }

    #[test]
    fn scalar_index_path_matches_unindexed_for_range_order_paginate() {
        // Two identical collections; index one field on only one of them. A
        // range + order + paginate query must return byte-identical rows.
        fn fill(c: &crate::Collection) {
            for i in 0..50i64 {
                let mut m = BTreeMap::new();
                m.insert("n".to_owned(), Value::Int(i % 7));
                m.insert("tag".to_owned(), Value::Text(format!("t{}", i % 3)));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        fill(&ic);
        ic.create_scalar_index("n").unwrap();

        let run = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("n").ge(Value::Int(2)))
                .filter(field("tag").eq(Value::Text("t1".into())))
                .order_by("n", true)
                .offset(1)
                .limit(5)
                .run()
                .unwrap()
        };
        assert_eq!(run(&plain), run(&indexed));
        // And counts match.
        let count = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("n").ge(Value::Int(2)))
                .count()
                .unwrap()
        };
        assert_eq!(count(&plain), count(&indexed));
    }

    #[test]
    fn order_by_field_ascending_and_descending() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for (k, n) in [(b"a", 3), (b"b", 1), (b"c", 2)] {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(n));
            c.insert(k, &Value::Map(m)).unwrap();
        }
        let asc = c.query().order_by("n", false).run().unwrap();
        assert_eq!(
            asc.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            vec![b"b".to_vec(), b"c".to_vec(), b"a".to_vec()]
        );
        let desc = c.query().order_by("n", true).run().unwrap();
        assert_eq!(
            desc.iter().map(|r| r.key.clone()).collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"c".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn order_by_puts_missing_field_last() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(5));
        c.insert(b"has", &Value::Map(m)).unwrap();
        c.insert(b"missing", &Value::Map(BTreeMap::new())).unwrap();
        let rows = c.query().order_by("n", false).run().unwrap();
        assert_eq!(rows[0].key, b"has".to_vec());
        assert_eq!(rows[1].key, b"missing".to_vec());
    }

    #[test]
    fn offset_and_limit_paginate() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..10u8 {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(i as i64));
            c.insert(&[i], &Value::Map(m)).unwrap();
        }
        let page = c
            .query()
            .order_by("n", false)
            .offset(3)
            .limit(2)
            .run()
            .unwrap();
        let ns: Vec<i64> = page
            .iter()
            .map(|r| r.document.get("n").unwrap().as_int().unwrap())
            .collect();
        assert_eq!(ns, vec![3, 4]);
    }

    #[test]
    fn offset_past_end_is_empty() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &Value::Int(1)).unwrap();
        assert!(c.query().offset(100).run().unwrap().is_empty());
    }

    #[test]
    fn count_counts_filtered_documents() {
        let db = seed();
        assert_eq!(db.collection("docs").query().count().unwrap(), 3);
        let blogs = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .count()
            .unwrap();
        assert_eq!(blogs, 2);
    }

    #[test]
    fn group_count_buckets_by_field() {
        let db = seed();
        let groups = db
            .collection("docs")
            .query()
            .group_count("category")
            .unwrap();
        assert_eq!(groups.get("blog"), Some(&2));
        assert_eq!(groups.get("news"), Some(&1));
    }

    #[test]
    fn group_count_skips_missing_and_container_fields() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("blog", "x", vec![1.0])).unwrap();
        c.insert(b"b", &Value::Int(5)).unwrap(); // no "category" field
        let groups = c.query().group_count("category").unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.get("blog"), Some(&1));
    }

    #[test]
    fn group_count_respects_filters() {
        let db = seed();
        let groups = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .group_count("category")
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.get("blog"), Some(&2));
    }

    #[test]
    fn mmr_keeps_docs_without_embeddings_after_reranked() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"vec", &doc("x", "has embedding", vec![1.0, 0.0]))
            .unwrap();
        // A doc matched by text but with no embedding field of the right dim.
        let mut m = BTreeMap::new();
        m.insert("body".to_owned(), Value::Text("rust text only".into()));
        c.insert(b"txt", &Value::Map(m)).unwrap();
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .text("body", "rust", 10)
            .rerank_mmr(0.5)
            .run()
            .unwrap();
        // Both appear; the embedded one is reranked, the text-only one tails.
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert!(keys.contains(&b"vec".to_vec()));
        assert!(keys.contains(&b"txt".to_vec()));
        assert_eq!(keys[0], b"vec".to_vec());
    }
}
