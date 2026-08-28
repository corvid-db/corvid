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
//! 1. Build the candidate set with the most bounded source available: an ANN
//!    or text index for a single indexed source, a scalar/geo index when a
//!    filter drives it, a streaming bounded top-k for an unindexed single
//!    vector source, or a full scan as the fallback. Filtering happens *before*
//!    ranking, so `filter` is a true predicate over the corpus — top-k is
//!    computed among matching documents, never post-hoc.
//! 2. Rank the filtered set independently for each retrieval source (each
//!    capped at its own `k`).
//! 3. Fuse the per-source rankings with Reciprocal Rank Fusion (a single
//!    source passes through unchanged; zero sources yields the filtered set in
//!    key order).
//! 4. Optionally reorder by Maximal Marginal Relevance, using the first vector
//!    source's query, field, and metric. Candidates lacking a usable embedding
//!    keep their fused order after the reranked ones.
//! 5. Truncate to `limit`.
//!
//! ## Snapshot scope
//!
//! One [`run`](QueryBuilder::run) — and each aggregate — executes against ONE
//! consistent read snapshot (audit B3): every document read the query itself
//! performs (candidate verification, ANN/text fetch loops, streaming and
//! full scans) comes from a single read transaction, so a query's result
//! always matches some point in time even while writers commit concurrently.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use crate::db::Collection;
use crate::distance::Metric;
use crate::error::Result;
use crate::filter::{CmpOp, Predicate};
use crate::fusion::{DEFAULT_RRF_K, mmr, reciprocal_rank_fusion};
use crate::query::{doc_map, ranked_bm25, ranked_vector};
use crate::store::SnapshotReader;
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
#[derive(Debug)]
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
    ///
    /// Like [`Self::run`], the whole aggregate executes on one read snapshot
    /// (audit B3).
    pub fn count(self) -> Result<usize> {
        // No filters → use the maintained O(1) counter.
        if self.filters.is_empty() {
            return self.collection.len();
        }
        self.resume_index_builds()?;
        self.collection.db().store().read(|r| {
            // Scalar-index fast path: count verified candidates without a
            // full scan.
            if let Some(matched) = self.indexed_candidates(r)? {
                return Ok(matched.len());
            }
            let mut n = 0usize;
            self.collection.for_each_doc_in(r, |_, doc| {
                if self.filters.iter().all(|p| p.eval(&doc)) {
                    n += 1;
                }
                Ok(true)
            })?;
            Ok(n)
        })
    }

    /// Count matching documents grouped by the value at `field`.
    ///
    /// Groups are keyed by the canonical form (spec decision 3): text is used
    /// bare; int/float/bool are type-tagged (`i:1`, `f:1.5`, `b:true`) so
    /// distinct types never collapse into one group; a text that would be
    /// ambiguous with a tagged form (it starts with `i:`, `f:`, `b:`, or
    /// `t:`) is escaped with a `t:` prefix. Documents whose field is missing
    /// or is a container are not counted. Like [`Self::count`], this
    /// aggregates over the filtered set and ignores ranking.
    pub fn group_count(self, field: &str) -> Result<BTreeMap<String, usize>> {
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        self.for_each_match(|doc| {
            if let Some(key) = doc.get_path(field).and_then(group_key) {
                *groups.entry(key).or_insert(0) += 1;
            }
        })?;
        Ok(groups)
    }

    /// Stream each document matching the filters, reusing the scalar/geo index
    /// fast path when a filter drives one, else a bounded scan. The whole
    /// stream observes one read snapshot (audit B3), so an aggregate built on
    /// it never mixes states from two points in time.
    fn for_each_match(&self, mut f: impl FnMut(&Value)) -> Result<()> {
        self.resume_index_builds()?;
        self.collection.db().store().read(|r| {
            if let Some(cands) = self.indexed_candidates(r)? {
                for (_, doc) in &cands {
                    f(doc);
                }
                return Ok(());
            }
            self.collection.for_each_doc_in(r, |_, doc| {
                if self.filters.iter().all(|p| p.eval(&doc)) {
                    f(&doc);
                }
                Ok(true)
            })
        })
    }

    /// Sum the numeric (`int`/`float`) values at `field` over the filtered set.
    /// Missing or non-numeric values are skipped.
    pub fn sum(self, field: &str) -> Result<f64> {
        let mut total = 0.0;
        self.for_each_match(|doc| {
            if let Some(n) = doc.get_path(field).and_then(as_number) {
                total += n;
            }
        })?;
        Ok(total)
    }

    /// Mean of the numeric values at `field`, or `None` if there are none.
    pub fn avg(self, field: &str) -> Result<Option<f64>> {
        let (mut total, mut n) = (0.0, 0usize);
        self.for_each_match(|doc| {
            if let Some(x) = doc.get_path(field).and_then(as_number) {
                total += x;
                n += 1;
            }
        })?;
        Ok((n > 0).then(|| total / n as f64))
    }

    /// The minimum comparable value at `field` (numeric or text), or `None`.
    pub fn min(self, field: &str) -> Result<Option<Value>> {
        self.extremum(field, Ordering::Less)
    }

    /// The maximum comparable value at `field` (numeric or text), or `None`.
    pub fn max(self, field: &str) -> Result<Option<Value>> {
        self.extremum(field, Ordering::Greater)
    }

    fn extremum(self, field: &str, want: Ordering) -> Result<Option<Value>> {
        let mut best: Option<Value> = None;
        self.for_each_match(|doc| {
            if let Some(v) = doc.get_path(field) {
                let replace = match &best {
                    None => crate::filter::value_order(v, v).is_some(), // comparable at all
                    Some(b) => crate::filter::value_order(v, b) == Some(want),
                };
                if replace {
                    best = Some(v.clone());
                }
            }
        })?;
        Ok(best)
    }

    /// The number of distinct values at `field` over the filtered set, by the
    /// canonical group key (text bare; int/float/bool type-tagged `i:`/`f:`/
    /// `b:` so distinct types stay distinct; a text that would be ambiguous
    /// with a tagged form is `t:`-escaped; missing/container values are
    /// ignored).
    pub fn count_distinct(self, field: &str) -> Result<usize> {
        let mut seen = std::collections::HashSet::new();
        self.for_each_match(|doc| {
            if let Some(k) = doc.get_path(field).and_then(group_key) {
                seen.insert(k);
            }
        })?;
        Ok(seen.len())
    }

    /// Sum `value_field` grouped by `group_field` (numeric values only).
    /// Group keys use the canonical form (spec decision 3): text is used
    /// bare; int/float/bool are type-tagged (`i:1`, `f:1.5`, `b:true`) so
    /// distinct types never collapse into one group; a text that would be
    /// ambiguous with a tagged form (it starts with `i:`, `f:`, `b:`, or
    /// `t:`) is escaped with a `t:` prefix.
    pub fn group_sum(self, group_field: &str, value_field: &str) -> Result<BTreeMap<String, f64>> {
        let mut groups: BTreeMap<String, f64> = BTreeMap::new();
        self.for_each_match(|doc| {
            if let (Some(g), Some(x)) = (
                doc.get_path(group_field).and_then(group_key),
                doc.get_path(value_field).and_then(as_number),
            ) {
                *groups.entry(g).or_insert(0.0) += x;
            }
        })?;
        Ok(groups)
    }

    /// Mean of `value_field` grouped by `group_field` (numeric values only).
    /// Group keys use the canonical form (spec decision 3): text is used
    /// bare; int/float/bool are type-tagged (`i:1`, `f:1.5`, `b:true`) so
    /// distinct types never collapse into one group; a text that would be
    /// ambiguous with a tagged form (it starts with `i:`, `f:`, `b:`, or
    /// `t:`) is escaped with a `t:` prefix.
    pub fn group_avg(self, group_field: &str, value_field: &str) -> Result<BTreeMap<String, f64>> {
        let mut sums: BTreeMap<String, (f64, usize)> = BTreeMap::new();
        self.for_each_match(|doc| {
            if let (Some(g), Some(x)) = (
                doc.get_path(group_field).and_then(group_key),
                doc.get_path(value_field).and_then(as_number),
            ) {
                let e = sums.entry(g).or_insert((0.0, 0));
                e.0 += x;
                e.1 += 1;
            }
        })?;
        Ok(sums
            .into_iter()
            .map(|(g, (s, n))| (g, s / n as f64))
            .collect())
    }

    /// Try the ANN fast path: a single vector source whose field/metric has a
    /// registered index. Returns the (already filtered) candidate set, or
    /// `None` to fall back to an exact scan. Filtered queries only take this
    /// path under [`Self::approx`]. Document fetches read `reader`, so the
    /// verification shares the caller's snapshot (audit B3).
    fn ann_candidates(&self, reader: &dyn SnapshotReader) -> Result<Option<Candidates>> {
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
            if let Some(doc) = self.collection.get_in(reader, &key)?
                && self.filters.iter().all(|p| p.eval(&doc))
            {
                out.push((key, doc));
            }
        }
        Ok(Some(out))
    }

    /// Single text source backed by a text index: fetch the top `k` by BM25
    /// straight from the index (bounded memory, no corpus rescan), then verify
    /// filters. `None` to fall back. Filtered queries take this only under
    /// [`Self::approx`] (the index ranks before filtering, so a selective
    /// filter may leave fewer than `k`). Document fetches read `reader`, so
    /// the verification shares the caller's snapshot (audit B3).
    fn text_candidates(&self, reader: &dyn SnapshotReader) -> Result<Option<Candidates>> {
        if self.sources.len() != 1 {
            return Ok(None);
        }
        let Source::Text { field, query, k } = &self.sources[0] else {
            return Ok(None);
        };
        if !self.filters.is_empty() && !self.approx {
            return Ok(None);
        }
        let Some(ranked) =
            self.collection
                .db()
                .fts_search(self.collection.name(), field, query, *k)?
        else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for (key, _score) in ranked {
            if let Some(doc) = self.collection.get_in(reader, &key)?
                && self.filters.iter().all(|p| p.eval(&doc))
            {
                out.push((key, doc));
            }
        }
        Ok(Some(out))
    }

    /// Single vector source with no usable ANN index (or an exact filtered
    /// query): compute the top `k` by distance while *streaming* the collection
    /// from `reader`, holding only a bounded working set (~`4k`) instead of
    /// materializing every matching document. Distance needs no corpus
    /// statistics, so this is exact.
    fn streaming_vector_candidates(
        &self,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Candidates>> {
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
        let k = *k;
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        let prune_at = k.saturating_mul(4).max(1024);
        let sort_trunc = |buf: &mut Vec<(f32, Vec<u8>, Value)>| {
            buf.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            buf.truncate(k);
        };
        let mut buf: Vec<(f32, Vec<u8>, Value)> = Vec::new();
        self.collection.for_each_doc_in(reader, |key, doc| {
            if self.filters.iter().all(|p| p.eval(&doc))
                && let Some(v) = doc.get_path(field).and_then(Value::as_vector)
                && v.len() == query.len()
            {
                let dist = metric.distance(query, v);
                buf.push((dist, key.to_vec(), doc));
                if buf.len() >= prune_at {
                    sort_trunc(&mut buf);
                }
            }
            Ok(true)
        })?;
        sort_trunc(&mut buf);
        Ok(Some(
            buf.into_iter().map(|(_, key, doc)| (key, doc)).collect(),
        ))
    }

    /// Try the scalar-index fast path: if a top-level AND filter is an
    /// equality or range comparison on a field with a scalar index, fetch only
    /// the candidate documents (a superset) and verify every filter against
    /// each. Returns the filtered set, or `None` to fall back to a full scan.
    ///
    /// An equality predicate is preferred (most selective); otherwise the first
    /// range predicate is used. Documents are verified against `reader` (the
    /// caller's snapshot, audit B3). Interrupted index builds are resumed by
    /// the caller BEFORE that snapshot opens ([`Self::resume_index_builds`]).
    fn indexed_candidates(&self, reader: &dyn SnapshotReader) -> Result<Option<Candidates>> {
        if self.filters.is_empty() {
            return Ok(None);
        }
        let db = self.collection.db();
        let coll = self.collection.name();
        // Cap candidates so an unselective predicate blows the cap (returns
        // None) and is skipped, instead of materialising a huge set.
        const CAP: usize = 100_000;

        // Probe every index-serviceable source (each bounded by the cap) and
        // keep the *smallest* candidate set — i.e. let the most selective index
        // drive, minimising the documents fetched and verified. This is
        // selectivity-driven planning without persisted statistics: an
        // unselective index over-runs the cap and drops out on its own.
        let mut best: Option<Vec<Vec<u8>>> = None;

        // 1. Each indexed scalar field carrying comparisons: combine all of its
        //    AND comparisons into one window (e.g. n>=5 AND n<=10).
        let mut seen_fields: Vec<&str> = Vec::new();
        for pred in &self.filters {
            if let Predicate::Compare { path, op, .. } = pred
                && matches!(
                    op,
                    CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
                )
                && db.has_scalar_index(coll, path)
                && !seen_fields.contains(&path.as_str())
            {
                seen_fields.push(path);
                let constraints: Vec<crate::scalar::Constraint<'_>> = self
                    .filters
                    .iter()
                    .filter_map(|p| match p {
                        Predicate::Compare {
                            path: p2,
                            op,
                            value,
                        } if p2 == path => Some(crate::scalar::Constraint { op: *op, value }),
                        _ => None,
                    })
                    .collect();
                keep_smaller(
                    &mut best,
                    db.scalar_candidates(coll, path, &constraints, CAP)?,
                );
            }
        }
        // 2. Each Between / In / StartsWith predicate on an indexed field.
        for pred in &self.filters {
            if matches!(
                pred,
                Predicate::Between { .. } | Predicate::In { .. } | Predicate::StartsWith { .. }
            ) {
                keep_smaller(&mut best, self.predicate_candidates(pred)?);
            }
        }
        // 3. Whole-query alternative drivers: compound prefix, geo, OR union.
        keep_smaller(&mut best, self.compound_candidate_keys()?);
        keep_smaller(&mut best, self.geo_candidate_keys()?);
        keep_smaller(&mut best, self.or_candidate_keys()?);

        match best {
            Some(keys) => self.verify_candidates(reader, keys),
            None => Ok(None),
        }
    }

    /// Index-serviceable candidate keys for a *single* predicate (eq/range/in/
    /// between/starts_with on a scalar index, or geo within), else `None`.
    fn predicate_candidates(&self, pred: &Predicate) -> Result<Option<Vec<Vec<u8>>>> {
        const CAP: usize = 100_000;
        let db = self.collection.db();
        let coll = self.collection.name();
        match pred {
            Predicate::Compare { path, op, value }
                if matches!(
                    op,
                    CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
                ) && db.has_scalar_index(coll, path) =>
            {
                let cons = [crate::scalar::Constraint { op: *op, value }];
                db.scalar_candidates(coll, path, &cons, CAP)
            }
            Predicate::Between { path, low, high } if db.has_scalar_index(coll, path) => {
                let cons = [
                    crate::scalar::Constraint {
                        op: CmpOp::Ge,
                        value: low,
                    },
                    crate::scalar::Constraint {
                        op: CmpOp::Le,
                        value: high,
                    },
                ];
                db.scalar_candidates(coll, path, &cons, CAP)
            }
            Predicate::In { path, values } if db.has_scalar_index(coll, path) => {
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for v in values {
                    let cons = [crate::scalar::Constraint {
                        op: CmpOp::Eq,
                        value: v,
                    }];
                    match db.scalar_candidates(coll, path, &cons, CAP)? {
                        Some(ks) => {
                            for k in ks {
                                if seen.insert(k.clone()) {
                                    out.push(k);
                                }
                            }
                        }
                        None => return Ok(None),
                    }
                }
                Ok(Some(out))
            }
            Predicate::StartsWith { path, prefix } if db.has_scalar_index(coll, path) => {
                db.scalar_prefix_candidates(coll, path, prefix, CAP)
            }
            Predicate::GeoWithin {
                path,
                lat,
                lon,
                radius_km,
            } if db.has_geo_index(coll, path) => {
                match crate::geo_index::radius_bbox(*lat, *lon, *radius_km) {
                    Some((mn_lat, mn_lon, mx_lat, mx_lon)) => {
                        db.geo_candidates(coll, path, mn_lat, mn_lon, mx_lat, mx_lon)
                    }
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// If a top-level filter is an `OR` whose every disjunct is index-
    /// serviceable, the union of their candidate key sets (deduped, capped) — so
    /// a disjunction stays sub-linear. `None` if any disjunct can't use an index
    /// (then the whole thing must scan, to avoid missing matches).
    fn or_candidate_keys(&self) -> Result<Option<Vec<Vec<u8>>>> {
        const CAP: usize = 100_000;
        for pred in &self.filters {
            if matches!(pred, Predicate::Or(..)) {
                let mut disjuncts = Vec::new();
                flatten_or(pred, &mut disjuncts);
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for d in disjuncts {
                    match self.predicate_candidates(d)? {
                        Some(ks) => {
                            for k in ks {
                                if seen.insert(k.clone()) {
                                    out.push(k);
                                    if out.len() > CAP {
                                        return Ok(None);
                                    }
                                }
                            }
                        }
                        None => return Ok(None),
                    }
                }
                return Ok(Some(out));
            }
        }
        Ok(None)
    }

    /// If a compound index's leading fields are pinned by equality filters
    /// (optionally with a range on the next field), the candidate keys for that
    /// prefix window (a verified superset), else `None`. Picks the index that
    /// matches the longest equality prefix.
    fn compound_candidate_keys(&self) -> Result<Option<Vec<Vec<u8>>>> {
        const CANDIDATE_CAP: usize = 100_000;
        let db = self.collection.db();
        let coll = self.collection.name();

        // Index this query's comparisons by field path.
        let by_field = |field: &str| -> Vec<(CmpOp, &Value)> {
            self.filters
                .iter()
                .filter_map(|p| match p {
                    Predicate::Compare { path, op, value } if path == field => Some((*op, value)),
                    _ => None,
                })
                .collect()
        };

        let mut best: Option<(Vec<String>, usize, bool)> = None; // (fields, prefix_len, has_tail)
        for fields in db.compound_indexes(coll) {
            // Longest leading run of fields each constrained by an equality.
            let mut prefix_len = 0;
            while prefix_len < fields.len()
                && by_field(&fields[prefix_len])
                    .iter()
                    .any(|(op, _)| *op == CmpOp::Eq)
            {
                prefix_len += 1;
            }
            // A range/eq on the field right after the prefix extends the window.
            let has_tail = prefix_len < fields.len() && !by_field(&fields[prefix_len]).is_empty();
            if prefix_len == 0 && !has_tail {
                continue; // leading field unconstrained → index unusable
            }
            let score = prefix_len + has_tail as usize;
            if best
                .as_ref()
                .is_none_or(|(_, b, t)| score > *b + *t as usize)
            {
                best = Some((fields, prefix_len, has_tail));
            }
        }

        let Some((fields, prefix_len, has_tail)) = best else {
            return Ok(None);
        };

        // Build the equality prefix values (first matching Eq per field).
        let mut eq_prefix: Vec<&Value> = Vec::with_capacity(prefix_len);
        for f in &fields[..prefix_len] {
            let v = by_field(f).into_iter().find(|(op, _)| *op == CmpOp::Eq);
            match v {
                Some((_, value)) => eq_prefix.push(value),
                None => return Ok(None),
            }
        }
        // Tail constraints on the next field, if any.
        let tail: Vec<crate::scalar::Constraint<'_>> = if has_tail {
            by_field(&fields[prefix_len])
                .into_iter()
                .map(|(op, value)| crate::scalar::Constraint { op, value })
                .collect()
        } else {
            Vec::new()
        };

        db.compound_candidates(coll, &fields, &eq_prefix, &tail, CANDIDATE_CAP)
    }

    /// If a top-level `GeoWithin` filter targets a geo-indexed field, the
    /// candidate doc keys for its bounding box (a verified superset), else
    /// `None`.
    fn geo_candidate_keys(&self) -> Result<Option<Vec<Vec<u8>>>> {
        let db = self.collection.db();
        let coll = self.collection.name();
        for pred in &self.filters {
            if let Predicate::GeoWithin {
                path,
                lat,
                lon,
                radius_km,
            } = pred
                && db.has_geo_index(coll, path)
                && let Some((min_lat, min_lon, max_lat, max_lon)) =
                    crate::geo_index::radius_bbox(*lat, *lon, *radius_km)
            {
                return db.geo_candidates(coll, path, min_lat, min_lon, max_lat, max_lon);
            }
        }
        Ok(None)
    }

    /// Fetch each candidate key's document from `reader` (the caller's
    /// snapshot — one point in time for the whole set, audit B3) and keep
    /// those passing every filter.
    fn verify_candidates(
        &self,
        reader: &dyn SnapshotReader,
        keys: Vec<Vec<u8>>,
    ) -> Result<Option<Candidates>> {
        let mut out = Vec::new();
        for key in keys {
            if let Some(doc) = self.collection.get_in(reader, &key)?
                && self.filters.iter().all(|p| p.eval(&doc))
            {
                out.push((key, doc));
            }
        }
        Ok(Some(out))
    }

    /// A canonical, hashable [`QueryPlan`](crate::plan::QueryPlan) capturing this query's full shape
    /// (collection, filters, sources, fusion/rerank params, ordering,
    /// pagination, projection). Identically-configured builders produce equal
    /// plans; any difference produces a different plan. Use it to deduplicate
    /// or key a [`crate::plan::PlanCache`] on a query shape.
    pub fn plan(&self) -> crate::plan::QueryPlan {
        use std::fmt::Write;
        let mut s = String::new();
        // `Debug` of these shape components is deterministic for a given shape
        // (floats render by value), so it is a sound canonical key.
        let _ = writeln!(s, "collection={}", self.collection.name());
        for f in &self.filters {
            let _ = writeln!(s, "filter={f:?}");
        }
        for src in &self.sources {
            let _ = writeln!(s, "source={src:?}");
        }
        let _ = writeln!(s, "rrf_k={}", self.rrf_k.to_bits());
        let _ = writeln!(s, "mmr={:?}", self.mmr_lambda.map(f32::to_bits));
        let _ = writeln!(s, "order_by={:?}", self.order_by);
        let _ = writeln!(s, "offset={}", self.offset);
        let _ = writeln!(s, "limit={:?}", self.limit);
        let _ = writeln!(s, "select={:?}", self.projection);
        let _ = writeln!(s, "approx={}", self.approx);
        crate::plan::QueryPlan(s)
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
    ///
    /// ONE read snapshot covers the whole query (audit B3): every document
    /// read — candidate verification, ANN/text fetch loops, streaming and
    /// full scans — observes a single point in time, so the result always
    /// matches some committed state even while writers commit concurrently.
    /// Interrupted index builds are resumed first, before that snapshot
    /// opens, so nothing inside query execution writes.
    pub fn run(self) -> Result<Vec<ResultRow>> {
        self.resume_index_builds()?;
        self.collection.db().store().read(|r| self.run_with(r))
    }

    /// Resume interrupted index builds for this collection BEFORE a query
    /// snapshot opens. Resumes take write transactions and registry locks,
    /// so they must not run inside execution (audit B3 discipline). Gated on
    /// filters exactly like the probe it precedes: a filterless query never
    /// consults an index, so it never needed to resume one. Try-lock inside:
    /// a concurrent resumer means we just probe stale and fall back to the
    /// (correct) scan.
    fn resume_index_builds(&self) -> Result<()> {
        if self.filters.is_empty() {
            return Ok(());
        }
        self.collection
            .db()
            .try_resume_index_builds(self.collection.name())
    }

    /// The execution core of [`Self::run`]: the entire query — candidate
    /// generation, verification, ranking, fusion, rerank, ordering,
    /// pagination, projection — reads `reader` and nothing else, so the
    /// caller-supplied read transaction is the query's single point-in-time
    /// view. Nothing inside writes or resumes builds. (The `Db` index
    /// helpers consulted here still open their own transactions until the
    /// candidate paths are threaded onto the reader; every document fetch is
    /// already on `reader`.)
    pub(crate) fn run_with(&self, reader: &dyn SnapshotReader) -> Result<Vec<ResultRow>> {
        // Pick the narrowest / most-bounded source for the candidate set:
        //   1. a filter-driven scalar/geo index; with no retrieval sources,
        //      a streaming filter/order/paginate pass with bounded memory,
        //   2. ANN index (single indexed vector source),
        //   3. text index (single indexed text source),
        //   4. streaming bounded top-k (single vector source, no index),
        //   5. full scan + filter (multi-source / unindexed text).
        let filtered: Vec<(Vec<u8>, Value)> = if self.sources.is_empty() {
            // No retrieval sources → a pure filter/order/paginate query.
            // Scalar-index fast path: fetch only candidate documents instead
            // of scanning the whole collection, then order/paginate in memory
            // (the set is bounded by the number of matches).
            if let Some(mut matched) = self.indexed_candidates(reader)? {
                match &self.order_by {
                    Some((field, descending)) => sort_by_field(&mut matched, field, *descending),
                    None => matched.sort_by(|(ka, _), (kb, _)| ka.cmp(kb)),
                }
                return Ok(self.window_rows(matched));
            }
            self.stream_scan_only(reader)?
        } else if let Some(c) = self.ann_candidates(reader)? {
            c
        } else if let Some(c) = self.text_candidates(reader)? {
            c
        } else if let Some(c) = self.indexed_candidates(reader)? {
            c
        } else if let Some(c) = self.streaming_vector_candidates(reader)? {
            c
        } else {
            self.collection
                .scan_in(reader)?
                .into_iter()
                .filter(|(_, doc)| self.filters.iter().all(|p| p.eval(doc)))
                .collect()
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
                let va = docs.get(ka).and_then(|d| d.get_path(field));
                let vb = docs.get(kb).and_then(|d| d.get_path(field));
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

    /// Streaming candidate pass for the no-retrieval-source case (filter /
    /// order / paginate), reading `reader`. Memory is bounded: with `limit`,
    /// at most ~`offset + limit` rows are held (early-stop in key order; a
    /// periodically-pruned buffer under `order_by`). Without `limit` and with
    /// `order_by`, all matching rows are held (an unbounded sort, as any DB
    /// without a sort index does).
    fn stream_scan_only(&self, reader: &dyn SnapshotReader) -> Result<Vec<(Vec<u8>, Value)>> {
        let cap = self.limit.map(|l| self.offset.saturating_add(l));
        let mut buf: Vec<(Vec<u8>, Value)> = Vec::new();

        match &self.order_by {
            // Key order: take only the `cap` window, stopping early.
            None => {
                self.collection.for_each_doc_in(reader, |key, doc| {
                    if self.filters.iter().all(|p| p.eval(&doc)) {
                        buf.push((key.to_vec(), doc));
                    }
                    Ok(cap.is_none_or(|c| buf.len() < c))
                })?;
            }
            // Ordered: keep the best `cap` via a periodically pruned buffer.
            Some((field, descending)) => {
                let prune_at = cap.map(|c| c.saturating_mul(2).max(1024));
                self.collection.for_each_doc_in(reader, |key, doc| {
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
        Ok(buf)
    }

    /// Offset + limit + projection over already-ordered `(key, document)`
    /// rows. These rows were not ranked, so their score is `0.0`.
    fn window_rows(&self, mut buf: Vec<(Vec<u8>, Value)>) -> Vec<ResultRow> {
        let start = self.offset.min(buf.len());
        let mut window = buf.split_off(start);
        if let Some(limit) = self.limit {
            window.truncate(limit);
        }
        window
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
            .collect()
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
///
/// The canonical form (spec decision 3): **text is used bare** — the
/// natural, dominant case; int/float/bool are type-tagged (`i:1`, `f:1.5`,
/// `b:true`) so distinct types never collapse into one group; and a text
/// that would be ambiguous with a tagged form (it starts with `i:`, `f:`,
/// `b:`, or `t:`) is escaped with a `t:` prefix. The mapping is injective:
/// bare texts never start with any tag, tagged non-text keys start with
/// `i:`/`f:`/`b:`, and escaped texts start with `t:` — three disjoint
/// prefixes plus disjoint bare text. `-0.0` and `+0.0` share a group
/// (numerically equal); NaN groups as `f:NaN`.
fn group_key(v: &Value) -> Option<String> {
    const TAGS: [&str; 4] = ["i:", "f:", "b:", "t:"];
    match v {
        Value::Text(s) => Some(if TAGS.iter().any(|t| s.starts_with(t)) {
            format!("t:{s}")
        } else {
            s.clone()
        }),
        Value::Int(i) => Some(format!("i:{i}")),
        Value::Float(f) => Some(format!("f:{}", if *f == 0.0 { 0.0 } else { *f })),
        Value::Bool(b) => Some(format!("b:{b}")),
        _ => None,
    }
}

/// Keep whichever candidate set is smaller (the more selective). `None`
/// candidates (no index / over the cap) are ignored.
fn keep_smaller(best: &mut Option<Vec<Vec<u8>>>, candidate: Option<Vec<Vec<u8>>>) {
    if let Some(k) = candidate
        && best.as_ref().is_none_or(|b| k.len() < b.len())
    {
        *best = Some(k);
    }
}

/// Flatten a (possibly nested) `OR` predicate tree into its disjuncts.
fn flatten_or<'a>(pred: &'a Predicate, out: &mut Vec<&'a Predicate>) {
    match pred {
        Predicate::Or(a, b) => {
            flatten_or(a, out);
            flatten_or(b, out);
        }
        other => out.push(other),
    }
}

/// A value as `f64` for numeric aggregation (int or float), else `None`.
fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Sort `(key, doc)` pairs by a scalar field, missing/incomparable last, ties
/// by key. `descending` reverses the value comparison.
fn sort_by_field(buf: &mut [(Vec<u8>, Value)], field: &str, descending: bool) {
    buf.sort_by(
        |(ka, da), (kb, db)| match (da.get_path(field), db.get_path(field)) {
            (Some(a), Some(b)) => {
                let base = crate::filter::value_order(a, b).unwrap_or(Ordering::Equal);
                let base = if descending { base.reverse() } else { base };
                base.then_with(|| ka.cmp(kb))
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => ka.cmp(kb),
        },
    );
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
            .and_then(|d| d.get_path(field))
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
    fn streaming_vector_topk_matches_exact_over_prune_threshold() {
        // More than the 1024 prune threshold, no vector index → the builder
        // uses the streaming bounded top-k path; it must equal exact KNN.
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..1500u32 {
            let mut m = BTreeMap::new();
            // A spread of 2-D vectors.
            let a = (i % 50) as f32;
            let b = (i / 50) as f32;
            m.insert("embedding".to_owned(), Value::Vector(vec![a, b]));
            c.insert(&i.to_le_bytes(), &Value::Map(m)).unwrap();
        }
        let q = vec![3.0, 7.0];
        let exact = c.vector_search("embedding", &q, 10, Metric::L2).unwrap();
        let rows = c
            .query()
            .vector("embedding", q.clone(), 10, Metric::L2)
            .limit(10)
            .run()
            .unwrap();
        let got: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        let want: Vec<_> = exact.iter().map(|h| h.key.clone()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn single_text_source_uses_index_and_ranks() {
        let db = seed();
        let c = db.collection("docs");
        c.create_text_index("body").unwrap();
        // "rust" appears in a (blog) and c (news); indexed text path ranks them.
        let rows = c.query().text("body", "rust", 10).run().unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert!(keys.contains(&b"a".to_vec()));
        assert!(keys.contains(&b"c".to_vec()));
        assert!(!keys.contains(&b"b".to_vec())); // "python web framework"
    }

    #[test]
    fn selectivity_picks_smallest_index_and_is_correct() {
        // Two indexed fields: `tag` (very common) and `uid` (unique). A query
        // pinning both must still return the exact rows; the planner drives on
        // the selective field. Parity with an unindexed collection proves it.
        fn fill(c: &crate::Collection) {
            for i in 0..200i64 {
                let mut m = BTreeMap::new();
                m.insert("tag".to_owned(), Value::Text("common".into())); // all share it
                m.insert("uid".to_owned(), Value::Int(i)); // unique
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        fill(&ic);
        ic.create_scalar_index("tag").unwrap();
        ic.create_scalar_index("uid").unwrap();

        let run = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("tag").eq(Value::Text("common".into())))
                .filter(field("uid").eq(Value::Int(42)))
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&plain), run(&indexed));
        assert_eq!(run(&indexed), vec![vec![42u8]]);
    }

    #[test]
    fn or_predicate_uses_index_union_matching_scan() {
        fn fill(c: &crate::Collection) {
            for i in 0..60i64 {
                let mut m = BTreeMap::new();
                m.insert("n".to_owned(), Value::Int(i));
                m.insert("cat".to_owned(), Value::Text(format!("c{}", i % 5)));
                c.insert(&[i as u8], &Value::Map(m)).unwrap();
            }
        }
        let plain = Db::open_in_memory().unwrap();
        fill(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        fill(&ic);
        ic.create_scalar_index("n").unwrap();
        ic.create_scalar_index("cat").unwrap();

        let run = |db: &Db| {
            let mut k: Vec<_> = db
                .collection("docs")
                .query()
                // n == 3  OR  n >= 58  OR  cat == "c2"
                .filter(
                    field("n")
                        .eq(Value::Int(3))
                        .or(field("n").ge(Value::Int(58)))
                        .or(field("cat").eq(Value::Text("c2".into()))),
                )
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect::<Vec<_>>();
            k.sort();
            k
        };
        assert_eq!(run(&plain), run(&indexed));
        assert!(!run(&indexed).is_empty());
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
    fn aggregations_global_and_grouped() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        let rows = [("a", 10i64), ("a", 20), ("b", 30), ("b", 40), ("c", 5)];
        for (i, (cat, n)) in rows.iter().enumerate() {
            let mut m = BTreeMap::new();
            m.insert("cat".to_owned(), Value::Text((*cat).to_owned()));
            m.insert("n".to_owned(), Value::Int(*n));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // Global aggregates.
        assert_eq!(c.query().sum("n").unwrap(), 105.0);
        assert_eq!(c.query().avg("n").unwrap(), Some(21.0));
        assert_eq!(c.query().min("n").unwrap(), Some(Value::Int(5)));
        assert_eq!(c.query().max("n").unwrap(), Some(Value::Int(40)));
        assert_eq!(c.query().count_distinct("cat").unwrap(), 3);
        // Empty aggregates.
        assert_eq!(c.query().avg("missing").unwrap(), None);
        assert_eq!(c.query().min("missing").unwrap(), None);

        // Grouped.
        let gs = c.query().group_sum("cat", "n").unwrap();
        assert_eq!(gs.get("a"), Some(&30.0));
        assert_eq!(gs.get("b"), Some(&70.0));
        assert_eq!(gs.get("c"), Some(&5.0));
        let ga = c.query().group_avg("cat", "n").unwrap();
        assert_eq!(ga.get("a"), Some(&15.0));
        assert_eq!(ga.get("b"), Some(&35.0));
    }

    #[test]
    fn aggregations_respect_filters() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..10i64 {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(i));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        // sum of n for n >= 7 → 7+8+9 = 24.
        let s = c
            .query()
            .filter(field("n").ge(Value::Int(7)))
            .sum("n")
            .unwrap();
        assert_eq!(s, 24.0);
    }

    #[test]
    fn min_max_work_on_text() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for (i, s) in ["banana", "apple", "cherry"].iter().enumerate() {
            let mut m = BTreeMap::new();
            m.insert("name".to_owned(), Value::Text((*s).to_owned()));
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        assert_eq!(
            c.query().min("name").unwrap(),
            Some(Value::Text("apple".into()))
        );
        assert_eq!(
            c.query().max("name").unwrap(),
            Some(Value::Text("cherry".into()))
        );
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

    /// Regression: distinct types must not collapse into one group
    /// (`Text("1")` / `Int(1)` / `Float(1.0)` used to all serialize to "1"),
    /// and `-0.0` shares a group with `+0.0`.
    #[test]
    fn group_keys_are_typed_so_distinct_types_stay_distinct() {
        use crate::Value;
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for (i, v) in [
            Value::Text("1".into()),
            Value::Text("i:1".into()), // ambiguous with Int tag → escaped
            Value::Text("t:x".into()), // ambiguous with the escape tag itself
            Value::Int(1),
            Value::Float(1.0),
            Value::Float(-0.0),
            Value::Float(0.0),
            Value::Bool(true),
        ]
        .into_iter()
        .enumerate()
        {
            let mut m = BTreeMap::new();
            m.insert("v".to_owned(), v);
            c.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        let groups = c.query().group_count("v").unwrap();
        assert_eq!(groups.get("1"), Some(&1)); // bare text
        assert_eq!(groups.get("t:i:1"), Some(&1)); // escaped text
        assert_eq!(groups.get("t:t:x"), Some(&1)); // t:-prefixed text self-escapes
        assert_eq!(groups.get("i:1"), Some(&1));
        assert_eq!(groups.get("f:1"), Some(&1));
        assert_eq!(groups.get("f:0"), Some(&2)); // -0.0 == 0.0 for grouping
        assert_eq!(groups.get("b:true"), Some(&1));
        assert_eq!(groups.len(), 7);
        assert_eq!(c.query().count_distinct("v").unwrap(), 7);
    }

    /// Wave-4 audit B3, scenario 1 (the spec's own example): a writer flips
    /// doc "k" between variant A (n=1) and variant B (n=2) in a tight loop
    /// while the main thread runs a filtered query 200x. Every result set
    /// must be one of the valid single-snapshot answers — {k} (post-A) or {}
    /// (post-B) — never anything else. The scalar index on `tag` routes the
    /// query through per-key document verification, the read shape this wave
    /// makes snapshot-scoped.
    #[test]
    fn interleaved_flip_query_results_match_a_single_snapshot() {
        let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
        let c = db.collection("docs");
        c.create_scalar_index("tag").unwrap();
        let doc = |n: i64| {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("t".into()));
            m.insert("n".to_owned(), Value::Int(n));
            Value::Map(m)
        };
        // 10 docs: "k" starts at variant A (n=1); the other nine never match
        // n==1 or n==2.
        c.insert(b"k", &doc(1)).unwrap();
        for i in 0..9u8 {
            c.insert(format!("p{i}").as_bytes(), &doc(0)).unwrap();
        }

        let w = std::sync::Arc::clone(&db);
        let writer = std::thread::spawn(move || {
            for i in 0..1000 {
                let n = if i % 2 == 0 { 2 } else { 1 };
                w.collection("docs").insert(b"k", &doc(n)).unwrap();
            }
        });

        for _ in 0..200 {
            let keys: Vec<Vec<u8>> = db
                .collection("docs")
                .query()
                .filter(field("tag").eq(Value::Text("t".into())))
                .filter(field("n").eq(Value::Int(1)))
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect();
            assert!(
                keys.is_empty() || keys == vec![b"k".to_vec()],
                "query result {keys:?} matches no single snapshot"
            );
        }
        writer.join().unwrap();
    }

    /// Wave-4 audit B3, scenario 2 (the discriminator): docs k1 ("a") and
    /// k2 ("z") flip TOGETHER in one transaction (`insert_batch`) between
    /// A=(n=1,n=1) and B=(n=2,n=2); eight filler docs sit between them in key
    /// order, widening the per-key fetch window. A query for n==1 is
    /// {k1,k2} or {} at every point in time — a result holding exactly one
    /// of them observed k1 pre-flip and k2 post-flip, a set matching NO
    /// point in time. The pre-wave-4 shape (one read transaction per
    /// per-key document fetch) can produce that; single-snapshot execution
    /// cannot.
    #[test]
    fn interleaved_paired_flip_never_mixes_states_from_two_snapshots() {
        let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
        let c = db.collection("docs");
        c.create_scalar_index("tag").unwrap();
        let doc = |n: i64| {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("t".into()));
            m.insert("n".to_owned(), Value::Int(n));
            Value::Map(m)
        };
        c.insert(b"a", &doc(1)).unwrap(); // k1
        for i in 1..=8u8 {
            c.insert(format!("b{i}").as_bytes(), &doc(0)).unwrap();
        }
        c.insert(b"z", &doc(1)).unwrap(); // k2

        let w = std::sync::Arc::clone(&db);
        let writer = std::thread::spawn(move || {
            for i in 0..1000 {
                let n = if i % 2 == 0 { 2 } else { 1 };
                let d = doc(n);
                w.collection("docs")
                    .insert_batch(&[(b"a", &d), (b"z", &d)])
                    .unwrap();
            }
        });

        for _ in 0..200 {
            let mut keys: Vec<Vec<u8>> = db
                .collection("docs")
                .query()
                .filter(field("tag").eq(Value::Text("t".into())))
                .filter(field("n").eq(Value::Int(1)))
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect();
            keys.sort();
            assert!(
                keys.is_empty() || keys == vec![b"a".to_vec(), b"z".to_vec()],
                "query result {keys:?} mixes two snapshots \
                 (k1 observed in A-state, k2 in B-state)"
            );
        }
        writer.join().unwrap();
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
