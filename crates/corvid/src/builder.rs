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
//!    filter drives it, the scalar-index ORDER WALK for a filterless
//!    `order_by` over an indexed field (documents fetched only for the
//!    `offset + limit` window), a streaming bounded top-k for an unindexed
//!    single vector source, or a full scan as the fallback. Filtering
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
//!
//! ## Snapshot scope
//!
//! One [`run`](QueryBuilder::run) — and each aggregate — executes against ONE
//! consistent read snapshot (audit B3): every document read the query itself
//! performs (candidate verification, ANN/text fetch loops, streaming and
//! full scans) comes from a single read transaction, so a query's result
//! always matches some point in time even while writers commit concurrently.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};

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

/// The candidate-source arm the planner will drive for a query (audit C3):
/// the shape [`QueryBuilder::explain`] reports and
/// [`QueryBuilder::plan_shape`] predicts. An *advisory* prediction factored
/// from the same conditions the execution core's candidate ladder (`run_with`)
/// tests — see `plan_shape` for the exact correspondence and its limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanShape {
    /// Single vector source served by its ANN index (`ann_candidates`).
    AnnIndex {
        /// The indexed vector field.
        field: String,
    },
    /// Single text source served by its text index (`text_candidates`).
    TextIndex {
        /// The indexed text field.
        field: String,
    },
    /// Filters drive a scalar/compound/geo/OR index window
    /// (`indexed_candidates`), with no retrieval sources or after the
    /// single-source index paths declined.
    IndexedWindow {
        /// Which index family drives: `"scalar"`, `"compound"`, `"geo"`,
        /// or `"or"` — attributed by probe order (the first serviceable
        /// family in the ladder), not by smallest-window selectivity;
        /// advisory only.
        kind: &'static str,
    },
    /// Single vector source with no usable ANN index: bounded streaming
    /// top-k (`streaming_vector_candidates`).
    StreamingTopK,
    /// No retrieval sources and no filters: `order_by(field)` served by
    /// walking `field`'s complete scalar index in the total-order
    /// contract's comparable-class order (`order_index_rows`) instead of
    /// materializing and sorting every row.
    SortIndex {
        /// The indexed ordering field.
        field: String,
    },
    /// Full collection scan + filter (the fallback arm).
    Scan {
        /// The scanned collection.
        collection: String,
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
    /// Must be positive and finite: any other value (zero, negative, NaN) is
    /// rejected with [`crate::Error::InvalidArgument`] when the query
    /// executes (audit C6 — the builder stays fluent; execution validates).
    pub fn fuse_rrf(mut self, k: f32) -> Self {
        self.rrf_k = k;
        self
    }

    /// Diversify results with Maximal Marginal Relevance. `lambda` in `[0, 1]`
    /// trades relevance (1.0) against diversity (0.0); values outside the
    /// range (or NaN) are rejected with [`crate::Error::InvalidArgument`]
    /// when the query executes (audit C6). Requires a vector source to
    /// supply the query and metric; without one this is a no-op.
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

    /// Order results by a scalar field instead of by rank. Rows fall into
    /// three classes (audit C4): values present and comparable (int/float
    /// numerically, text lexically) come first, in value order; values
    /// present but pairwise incomparable (bools, containers, NaN) come after
    /// them — ordered by the same kind tag first, so NaN (a numeric kind)
    /// precedes the other incomparable kinds, which then fall to key order;
    /// rows missing the field come last. Ties within a class break by key.
    /// `descending` reverses the within-class order — kind tag and value
    /// together — in both the comparable and the incomparable class; the
    /// class order itself and the key tiebreak are fixed, so incomparable
    /// and missing values always sort last.
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
        self.validate_args()?;
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
    /// Groups are keyed by the canonical form: text is used
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
        self.validate_args()?;
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
    /// Group keys use the canonical form: text is used
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
    /// Group keys use the canonical form: text is used
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

    /// Validate caller-supplied ranking parameters (audit C6). The fluent
    /// builder stores arguments as given — returning `Result` from every
    /// chain link would break the API — so every execution entry point
    /// ([`Self::run`], [`Self::count`], and the aggregates via
    /// [`Self::for_each_match`]) rejects out-of-domain values with
    /// [`crate::Error::InvalidArgument`] before touching the store.
    /// Execution-time validation keeps the chain fluent and the errors typed.
    fn validate_args(&self) -> Result<()> {
        if !self.rrf_k.is_finite() || self.rrf_k <= 0.0 {
            return Err(crate::Error::InvalidArgument(format!(
                "fuse_rrf: k must be > 0, got {}",
                self.rrf_k
            )));
        }
        if let Some(lambda) = self.mmr_lambda
            && !(0.0..=1.0).contains(&lambda)
        {
            // The range test also catches NaN (not contained).
            return Err(crate::Error::InvalidArgument(format!(
                "rerank_mmr: lambda must be in [0, 1], got {lambda}"
            )));
        }
        Ok(())
    }

    /// Try the ANN fast path: a single vector source whose field/metric has a
    /// registered index. Returns the (already filtered) candidate set, or
    /// `None` to fall back to an exact scan. Filtered queries only take this
    /// path under [`Self::approx`]. The graph search and the document
    /// fetches both read `reader`, so the whole path shares the caller's
    /// snapshot (audit B3).
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
        let Some(ranked) = self.collection.db().ann_search_in(
            self.collection.name(),
            field,
            query,
            *k,
            *metric,
            reader,
        )?
        else {
            return Ok(None);
        };
        // Audit C10: the ANN index actually served this query's candidates.
        #[cfg(test)]
        test_probe::bump_ann();
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
    /// filter may leave fewer than `k`). The postings scan and the document
    /// fetches both read `reader`, so the whole path shares the caller's
    /// snapshot (audit B3).
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
                .fts_search_in(self.collection.name(), field, query, *k, reader)?
        else {
            return Ok(None);
        };
        // Audit C10: the text index actually served this query's candidates.
        #[cfg(test)]
        test_probe::bump_text();
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
    /// range predicate is used. The index window scans AND the document
    /// verification both read `reader` — the whole candidate+verify pass is
    /// one point in time (audit B3). Interrupted index builds are resumed by
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
                    db.scalar_candidates(coll, path, &constraints, CAP, reader)?,
                );
            }
        }
        // 2. Each Between / In / StartsWith predicate on an indexed field.
        for pred in &self.filters {
            if matches!(
                pred,
                Predicate::Between { .. } | Predicate::In { .. } | Predicate::StartsWith { .. }
            ) {
                keep_smaller(&mut best, self.predicate_candidates(pred, reader)?);
            }
        }
        // 3. Whole-query alternative drivers: compound prefix, geo, OR union.
        keep_smaller(&mut best, self.compound_candidate_keys(reader)?);
        keep_smaller(&mut best, self.geo_candidate_keys(reader)?);
        keep_smaller(&mut best, self.or_candidate_keys(reader)?);

        match best {
            // Audit C10: an index window actually drove this query's
            // candidates (some probe family returned serviceable keys).
            Some(keys) => {
                #[cfg(test)]
                test_probe::bump_indexed();
                self.verify_candidates(reader, keys)
            }
            None => Ok(None),
        }
    }

    /// Index-serviceable candidate keys for a *single* predicate (eq/range/in/
    /// between/starts_with on a scalar index, or geo within), read from
    /// `reader`'s snapshot, else `None`.
    fn predicate_candidates(
        &self,
        pred: &Predicate,
        reader: &dyn SnapshotReader,
    ) -> Result<Option<Vec<Vec<u8>>>> {
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
                db.scalar_candidates(coll, path, &cons, CAP, reader)
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
                db.scalar_candidates(coll, path, &cons, CAP, reader)
            }
            Predicate::In { path, values } if db.has_scalar_index(coll, path) => {
                // Audit B10: every value's window is individually capped,
                // but the UNION must honor the same aggregate cap as the
                // OR path — an unselective In list falls back to a scan
                // instead of materializing an unbounded key set.
                let mut union = KeyUnion::with_cap(CAP);
                for v in values {
                    let cons = [crate::scalar::Constraint {
                        op: CmpOp::Eq,
                        value: v,
                    }];
                    match db.scalar_candidates(coll, path, &cons, CAP, reader)? {
                        Some(ks) => {
                            if !union.push(ks) {
                                return Ok(None);
                            }
                        }
                        None => return Ok(None),
                    }
                }
                Ok(Some(union.finish()))
            }
            Predicate::StartsWith { path, prefix } if db.has_scalar_index(coll, path) => {
                db.scalar_prefix_candidates(coll, path, prefix, CAP, reader)
            }
            Predicate::GeoWithin {
                path,
                lat,
                lon,
                radius_km,
            } if db.has_geo_index(coll, path) => {
                match crate::geo_index::radius_bbox(*lat, *lon, *radius_km) {
                    Some((mn_lat, mn_lon, mx_lat, mx_lon)) => {
                        db.geo_candidates(coll, path, mn_lat, mn_lon, mx_lat, mx_lon, reader)
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
    fn or_candidate_keys(&self, reader: &dyn SnapshotReader) -> Result<Option<Vec<Vec<u8>>>> {
        const CAP: usize = 100_000;
        for pred in &self.filters {
            if matches!(pred, Predicate::Or(..)) {
                let mut disjuncts = Vec::new();
                flatten_or(pred, &mut disjuncts);
                let mut union = KeyUnion::with_cap(CAP);
                for d in disjuncts {
                    match self.predicate_candidates(d, reader)? {
                        Some(ks) => {
                            if !union.push(ks) {
                                return Ok(None);
                            }
                        }
                        None => return Ok(None),
                    }
                }
                return Ok(Some(union.finish()));
            }
        }
        Ok(None)
    }

    /// If a compound index's fields are fully covered by the query's
    /// constraints — equality on a leading prefix of the fields, with
    /// optionally a range/eq on the next field exhausting the list — the
    /// candidate keys for that prefix window (a verified superset), read
    /// from `reader`'s snapshot, else `None`. Picks the index that matches
    /// the longest equality prefix.
    ///
    /// Soundness gate: with trailing fields unconstrained (a prefix-only
    /// equality), the window is a verified superset ONLY when the def's
    /// `all_docs_indexed` flag is true. When the flag is false, the compound
    /// index skips documents missing any indexed field, so the query can
    /// match such documents while the window cannot contain them — those
    /// shapes decline to the scan path. When the flag is true, every
    /// document in the collection has ALL the index's fields present and
    /// encodable — in particular the leading field — so every document
    /// MATCHING a prefix-only filter (matching requires the leading field
    /// present, encodable, and equal to the prefix value) IS in the index,
    /// and the prefix window contains every match: the window is sound.
    /// Full-coverage shapes are sound regardless of the flag (a matching
    /// doc has every field constrained, hence present and encodable, hence
    /// indexed).
    fn compound_candidate_keys(&self, reader: &dyn SnapshotReader) -> Result<Option<Vec<Vec<u8>>>> {
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
            if prefix_len + usize::from(has_tail) < fields.len()
                && !db.compound_all_docs_indexed(coll, &fields)
            {
                // A trailing field is unconstrained AND the def's
                // `all_docs_indexed` flag is false: documents missing the
                // trailing field can match the filters while sitting outside
                // the index — the window would not be a superset, so decline
                // and let the scan serve the query.
                continue;
            }
            let score = prefix_len + usize::from(has_tail);
            if best
                .as_ref()
                .is_none_or(|(_, b, t)| score > *b + usize::from(*t))
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

        db.compound_candidates(coll, &fields, &eq_prefix, &tail, CANDIDATE_CAP, reader)
    }

    /// If a top-level `GeoWithin` filter targets a geo-indexed field, the
    /// candidate doc keys for its bounding box (a verified superset, read
    /// from `reader`'s snapshot), else `None`.
    fn geo_candidate_keys(&self, reader: &dyn SnapshotReader) -> Result<Option<Vec<Vec<u8>>>> {
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
                return db.geo_candidates(coll, path, min_lat, min_lon, max_lat, max_lon, reader);
            }
        }
        Ok(None)
    }

    /// Fetch each candidate key's document from `reader` (the caller's
    /// snapshot — one point in time for the whole set, audit B3) and keep
    /// those passing every filter.
    ///
    /// Internal optimization, invisible to results: a *dense* window
    /// (candidates vs the collection's maintained count, per
    /// [`ROWS_PER_POINT_GET`]) fetches in ONE ordered walk of the
    /// collection; a *sparse* window keeps one point-get per key. Both
    /// strategies produce identical rows in identical candidate order.
    fn verify_candidates(
        &self,
        reader: &dyn SnapshotReader,
        keys: Vec<Vec<u8>>,
    ) -> Result<Option<Candidates>> {
        if keys.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let coll = self.collection.name();
        let len = reader.count(coll)?;
        if walk_wins(keys.len(), len) {
            self.verify_candidates_walk(reader, keys, coll)
        } else {
            self.verify_candidates_point_gets(reader, keys)
        }
    }

    /// The sparse-window strategy: the historical fetch — one snapshot
    /// point-get per candidate key, in candidate order.
    fn verify_candidates_point_gets(
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

    /// The dense-window strategy: ONE ordered walk of the collection's
    /// records. `for_each` streams key-ordered rows on the caller's
    /// snapshot without materializing the collection; rows between
    /// candidates are skipped by key comparison (no copy, no decode) and
    /// the walk stops past the last candidate — only candidate bytes are
    /// ever held. Candidates are matched in sorted order, then
    /// re-sequenced into the caller's candidate order and
    /// decoded/filtered there, so rows, order (duplicated candidate keys
    /// included), filter verdicts, and decode-error order are identical
    /// to [`Self::verify_candidates_point_gets`].
    fn verify_candidates_walk(
        &self,
        reader: &dyn SnapshotReader,
        keys: Vec<Vec<u8>>,
        coll: &str,
    ) -> Result<Option<Candidates>> {
        // Candidate positions sorted by key (positions, not keys: the
        // original vector is the output's required order).
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_unstable_by_key(|&i| keys[i].as_slice());
        let mut next = 0usize; // first unmatched position in `order`
        let mut fetched: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
        reader.for_each(coll, &mut |k, v| {
            // Candidates the stream has passed are absent from the store.
            while next < order.len() && keys[order[next]].as_slice() < k {
                next += 1;
            }
            if next == order.len() {
                return Ok(false); // past the last candidate: stop the walk
            }
            if keys[order[next]].as_slice() == k {
                // One fetch for this key; every duplicate candidate
                // position shares the bytes (same snapshot, same filter
                // verdict later), matching the point-get loop's emission.
                let mut i = next;
                while i < order.len() && keys[order[i]].as_slice() == k {
                    fetched.push((order[i], k.to_vec(), v.to_vec()));
                    i += 1;
                }
                next = i;
                if next == order.len() {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        // Re-sequence to the caller's candidate order — the exact order
        // the point-get loop would decode and filter in.
        fetched.sort_unstable_by_key(|(i, _, _)| *i);
        let mut out = Vec::with_capacity(fetched.len());
        for (_, key, bytes) in fetched {
            let doc = Value::decode(&bytes)?;
            if self.filters.iter().all(|p| p.eval(&doc)) {
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

    /// Which index family would drive [`Self::indexed_candidates`] for these
    /// filters, or `None` when no registered index is serviceable. A
    /// reader-free twin of that probe ladder's *selection* half: the same
    /// conditions in the same order (scalar comparisons, then
    /// Between/In/StartsWith windows, then compound prefix, geo, then the
    /// FIRST top-level Or), checking index registries and the constraints'
    /// statically-knowable serviceability instead of reading candidate
    /// keys. The scalar steps mirror `scalar::window`'s decline conditions
    /// — a non-encodable constraint value (containers/bytes/null),
    /// constraints mixing lanes (e.g. Int + Text on one field), or any `Ne`
    /// among the field's comparisons — and the compound step mirrors
    /// `encode_tuple`'s prefix encodability plus the tail's `window` check,
    /// scoring and picking the same winning index the real probe does. Kept
    /// in lockstep with `indexed_candidates` by
    /// `plan_shape_matches_served_path`; the remaining divergence is
    /// execution-time only — a probe that runs but over-runs its 100k cap
    /// returns `None` there, a fact this advisory check cannot see.
    fn indexed_window_kind(&self) -> Option<&'static str> {
        if self.filters.is_empty() {
            return None;
        }
        let db = self.collection.db();
        let coll = self.collection.name();

        // Index this query's comparisons by field path (every Compare on a
        // field, `Ne` included — the real probes pass them all to
        // `scalar::window`).
        let by_field = |field: &str| -> Vec<(CmpOp, &Value)> {
            self.filters
                .iter()
                .filter_map(|p| match p {
                    Predicate::Compare { path, op, value } if path == field => Some((*op, value)),
                    _ => None,
                })
                .collect()
        };

        // The same disjunct serviceability `predicate_candidates` requires
        // (a geo disjunct additionally needs a real bbox: a radius that wraps
        // the antimeridian makes the real probe decline; a comparison
        // disjunct's single-constraint window additionally needs an
        // encodable constant).
        let disjunct_serviceable = |pred: &Predicate| match pred {
            Predicate::Compare {
                path,
                op: CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge,
                value,
            } => db.has_scalar_index(coll, path) && crate::scalar::encode_value(value).is_some(),
            Predicate::Between { path, low, high } => {
                db.has_scalar_index(coll, path)
                    && window_serviceable(&[(CmpOp::Ge, low), (CmpOp::Le, high)])
            }
            Predicate::In { path, values } => {
                db.has_scalar_index(coll, path)
                    && values
                        .iter()
                        .all(|v| crate::scalar::encode_value(v).is_some())
            }
            Predicate::StartsWith { path, .. } => db.has_scalar_index(coll, path),
            Predicate::GeoWithin {
                path,
                lat,
                lon,
                radius_km,
            } => {
                db.has_geo_index(coll, path)
                    && crate::geo_index::radius_bbox(*lat, *lon, *radius_km).is_some()
            }
            _ => false,
        };

        // 1. Comparisons on indexed scalar fields — the field's combined
        //    AND window must also pass `scalar::window`'s static decline
        //    checks, exactly like the probe's `scalar_candidates` call.
        for pred in &self.filters {
            if let Predicate::Compare { path, op, .. } = pred
                && matches!(
                    op,
                    CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
                )
                && db.has_scalar_index(coll, path)
                && window_serviceable(&by_field(path))
            {
                return Some("scalar");
            }
        }
        // 2. Between / In / StartsWith windows on indexed scalar fields.
        for pred in &self.filters {
            if matches!(
                pred,
                Predicate::Between { .. } | Predicate::In { .. } | Predicate::StartsWith { .. }
            ) && disjunct_serviceable(pred)
            {
                return Some("scalar");
            }
        }
        // 3. A compound index whose EVERY field is covered by the query's
        //    constraints — a full-equality prefix over the fields, or a
        //    prefix plus a tail constraint on the next (last) field; a
        //    prefix-only query declines to scan UNLESS the def's
        //    `all_docs_indexed` flag is true — the selection half of
        //    `compound_candidate_keys`, without reading keys. The real probe
        //    scores every registered index (longest Eq prefix, then tail)
        //    and drives ONLY the winner, so the twin picks the same winner
        //    before checking serviceability: a non-encodable prefix Eq
        //    value (`encode_tuple` declines) or an unserviceable tail
        //    window makes the winner decline, with no second-place retry.
        let mut best: Option<(Vec<String>, usize, bool)> = None; // (fields, prefix_len, has_tail)
        for fields in db.compound_indexes(coll) {
            let mut prefix_len = 0;
            while prefix_len < fields.len()
                && by_field(&fields[prefix_len])
                    .iter()
                    .any(|(op, _)| *op == CmpOp::Eq)
            {
                prefix_len += 1;
            }
            let has_tail = prefix_len < fields.len() && !by_field(&fields[prefix_len]).is_empty();
            if prefix_len + usize::from(has_tail) < fields.len()
                && !db.compound_all_docs_indexed(coll, &fields)
            {
                // Soundness gate mirrored from `compound_candidate_keys`: a
                // trailing unconstrained field admits unindexed matches
                // unless every document is indexed.
                continue;
            }
            let score = prefix_len + usize::from(has_tail);
            if best
                .as_ref()
                .is_none_or(|(_, b, t)| score > *b + usize::from(*t))
            {
                best = Some((fields, prefix_len, has_tail));
            }
        }
        if let Some((fields, prefix_len, _)) = best {
            // The prefix values are the first Eq per field, exactly what
            // `encode_tuple` must encode; the tail is the next field's
            // combined window.
            let prefix_ok = fields[..prefix_len].iter().all(|f| {
                by_field(f)
                    .into_iter()
                    .find(|(op, _)| *op == CmpOp::Eq)
                    .is_some_and(|(_, v)| crate::scalar::encode_value(v).is_some())
            });
            let tail_ok =
                prefix_len >= fields.len() || window_serviceable(&by_field(&fields[prefix_len]));
            if prefix_ok && tail_ok {
                return Some("compound");
            }
        }
        // 4. A GeoWithin filter on a geo-indexed field with a real bbox.
        for pred in &self.filters {
            if let Predicate::GeoWithin {
                path,
                lat,
                lon,
                radius_km,
            } = pred
                && db.has_geo_index(coll, path)
                && crate::geo_index::radius_bbox(*lat, *lon, *radius_km).is_some()
            {
                return Some("geo");
            }
        }
        // 5. The FIRST top-level OR only, exactly like `or_candidate_keys`:
        //    serviceable when every disjunct is — one unserviceable
        //    disjunct declines the whole probe and is never rescued by a
        //    later Or (review round 1).
        for pred in &self.filters {
            if matches!(pred, Predicate::Or(..)) {
                let mut disjuncts = Vec::new();
                flatten_or(pred, &mut disjuncts);
                let serviceable =
                    !disjuncts.is_empty() && disjuncts.iter().all(|d| disjunct_serviceable(d));
                return serviceable.then_some("or");
            }
        }
        None
    }

    /// Predict which candidate-source arm the execution core (`run_with`)
    /// will drive
    /// (audit C3): an advisory, execution-free probe mirroring its ladder
    /// arm for arm —
    ///
    /// * no sources → the order-index walk (`SortIndex`) when `order_by`
    ///   targets a completely indexed field with no filters, else
    ///   `indexed_candidates`' window if a filter index is serviceable,
    ///   else the streaming filter scan (`Scan`),
    /// * a single vector source with filters only under `approx`, and a
    ///   consultable ANN index (`vector_index_consultable`) → `AnnIndex`,
    /// * a single text source under the same filter rule with a consultable
    ///   text index (`text_index_consultable`) → `TextIndex`,
    /// * filters with a serviceable index → `IndexedWindow`,
    /// * a single vector source with no usable index → `StreamingTopK`,
    /// * anything else (multi-source scans) → `Scan`.
    ///
    /// The consultable checks read only the index registries (no lazy
    /// builds, no snapshot). Two execution-time facts stay beyond an
    /// execution-free prediction: an index whose graph cannot accept the
    /// query's dimension falls back to exact at run time, and a probe that
    /// over-runs its 100k candidate cap declines. `run_with` itself is
    /// untouched by this — explain never executes. The parity test
    /// `plan_shape_matches_served_path` pins prediction to reality.
    pub fn plan_shape(&self) -> PlanShape {
        let coll = self.collection.name().to_owned();
        if self.sources.is_empty() {
            // The order-index arm precedes the filter window in `run_with`'s
            // ladder; the two are mutually exclusive (it declines any
            // filtered query), so the check order cannot shadow anything.
            if let Some((field, _)) = &self.order_by
                && self.filters.is_empty()
                && self.collection.db().has_scalar_index(&coll, field)
            {
                return PlanShape::SortIndex {
                    field: field.clone(),
                };
            }
            return match self.indexed_window_kind() {
                Some(kind) => PlanShape::IndexedWindow { kind },
                None => PlanShape::Scan { collection: coll },
            };
        }
        // The single-source index arms decline filtered queries unless
        // `approx` is set — the same gate `ann_candidates`/`text_candidates`
        // test before consulting their index.
        let filtered_ok = self.filters.is_empty() || self.approx;
        if self.sources.len() == 1 {
            match &self.sources[0] {
                Source::Vector { field, metric, .. }
                    if filtered_ok
                        && self
                            .collection
                            .db()
                            .vector_index_consultable(&coll, field, *metric) =>
                {
                    return PlanShape::AnnIndex {
                        field: field.clone(),
                    };
                }
                Source::Text { field, .. }
                    if filtered_ok && self.collection.db().text_index_consultable(&coll, field) =>
                {
                    return PlanShape::TextIndex {
                        field: field.clone(),
                    };
                }
                _ => {}
            }
        }
        if let Some(kind) = self.indexed_window_kind() {
            return PlanShape::IndexedWindow { kind };
        }
        if self.sources.len() == 1 && matches!(self.sources[0], Source::Vector { .. }) {
            return PlanShape::StreamingTopK;
        }
        PlanShape::Scan { collection: coll }
    }

    /// Describe the query plan as a human-readable string (for debugging). Does
    /// not execute the query, so it may be called before [`Self::run`].
    ///
    /// The head names the candidate source the planner will actually drive
    /// (audit C3: it used to print an unconditional `scan(collection)`
    /// regardless of the arm taken); the remaining parts decorate the query
    /// itself.
    pub fn explain(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(match self.plan_shape() {
            PlanShape::AnnIndex { field } => format!("ann({field})"),
            PlanShape::TextIndex { field } => format!("text-index({field})"),
            PlanShape::IndexedWindow { kind } => format!("indexed-window({kind})"),
            PlanShape::StreamingTopK => "streaming-topk".to_owned(),
            PlanShape::SortIndex { field } => format!("sort-index({field})"),
            PlanShape::Scan { collection } => format!("scan({collection})"),
        });
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
    /// Ranking parameters are validated first (audit C6): an out-of-domain
    /// `fuse_rrf`/`rerank_mmr` value fails fast with
    /// [`crate::Error::InvalidArgument`] before any store access.
    ///
    /// ONE read snapshot covers the whole query (audit B3): every document
    /// read — candidate verification, ANN/text fetch loops, streaming and
    /// full scans — observes a single point in time, so the result always
    /// matches some committed state even while writers commit concurrently.
    /// Interrupted index builds are resumed first, before that snapshot
    /// opens, so nothing inside query execution writes.
    pub fn run(self) -> Result<Vec<ResultRow>> {
        self.validate_args()?;
        self.resume_index_builds()?;
        self.collection.db().store().read(|r| self.run_with(r))
    }

    /// Resume interrupted index builds for this collection BEFORE a query
    /// snapshot opens. Resumes take write transactions and registry locks,
    /// so they must not run inside execution (audit B3 discipline). Gated on
    /// "this query will consult an index", mirroring the probes it
    /// precedes: a filtered query may drive the scalar/geo fast path
    /// (`indexed_candidates`), a single-source query drives the ANN or
    /// text index paths (`ann_candidates`/`text_candidates`) even with no
    /// filters — that is the normal `.vector(...)`/`.text(...)` shape —
    /// and a filterless `order_by` over a scalar-indexed field drives the
    /// order-index walk (`order_index_rows`). Only a query that consults
    /// no index (pure streaming/scan) skips resuming. Try-lock
    /// inside: a concurrent resumer means we just probe stale and fall back
    /// to the (correct) scan.
    fn resume_index_builds(&self) -> Result<()> {
        if self.filters.is_empty() && self.sources.len() != 1 && !self.order_index_consultable() {
            return Ok(());
        }
        self.collection
            .db()
            .try_resume_index_builds(self.collection.name())
    }

    /// Whether a filterless, sourceless `order_by` targets a field with a
    /// scalar-index DEFINITION (complete or building) — the static
    /// "may consult the order index" fact `resume_index_builds` gates on
    /// (execution itself re-checks completeness on the registry).
    fn order_index_consultable(&self) -> bool {
        self.sources.is_empty()
            && self.filters.is_empty()
            && self.order_by.as_ref().is_some_and(|(field, _)| {
                self.collection
                    .db()
                    .has_scalar_index_def(self.collection.name(), field)
            })
    }

    /// The execution core of [`Self::run`]: the entire query — candidate
    /// generation (index window scans included), verification, ranking,
    /// fusion, rerank, ordering, pagination, projection — reads `reader` and
    /// nothing else, so the caller-supplied read transaction is the query's
    /// single point-in-time view. Nothing inside writes or resumes builds
    /// (those happen in [`Self::run`] before the snapshot opens).
    pub(crate) fn run_with(&self, reader: &dyn SnapshotReader) -> Result<Vec<ResultRow>> {
        // Pick the narrowest / most-bounded source for the candidate set:
        //   1. a filter-driven scalar/geo index; with no retrieval sources,
        //      a streaming filter/order/paginate pass with bounded memory,
        //   2. ANN index (single indexed vector source),
        //   3. text index (single indexed text source),
        //   4. streaming bounded top-k (single vector source, no index),
        //   5. full scan + filter (multi-source / unindexed text).
        //
        // Each arm emits its plan-shape choice (feature-gated via telemetry):
        // ONE event per query at the decision point — which source drove,
        // how many candidates it produced. The labels mirror the
        // [`PlanShape`] variants, so a subscriber counting these events is
        // counting index probes per family (the "counters" story in
        // DESIGN's Observability section).
        macro_rules! plan_shape {
            ($shape:literal, $coll:expr, $rows:expr) => {
                crate::telemetry::event!(
                    DEBUG,
                    message = "plan_shape",
                    collection = crate::telemetry::display($coll),
                    shape = $shape,
                    rows = $rows as u64,
                );
            };
        }
        let filtered: Vec<(Vec<u8>, Value)> = if self.sources.is_empty() {
            // No retrieval sources → a pure filter/order/paginate query.
            // Order-index fast path first: a FILTERLESS order_by over a
            // scalar-indexed field is served by walking the index in the
            // total-order contract's comparable-class order (bounded
            // memory under a limit; identical rows to the sort by
            // construction). It declines any filtered query, so it never
            // shadows the window path below.
            if let Some(rows) = self.order_index_rows(reader)? {
                plan_shape!("sort_index", self.collection.name(), rows.len());
                return Ok(rows);
            }
            // Scalar-index fast path: fetch only candidate documents instead
            // of scanning the whole collection, then order/paginate in memory
            // (the set is bounded by the number of matches).
            if let Some(mut matched) = self.indexed_candidates(reader)? {
                plan_shape!("indexed_window", self.collection.name(), matched.len());
                match &self.order_by {
                    Some((field, descending)) => sort_by_field(&mut matched, field, *descending),
                    None => matched.sort_by(|(ka, _), (kb, _)| ka.cmp(kb)),
                }
                return Ok(self.window_rows(matched));
            }
            let scanned = self.stream_scan_only(reader)?;
            plan_shape!("stream_scan", self.collection.name(), scanned.len());
            scanned
        } else if let Some(c) = self.ann_candidates(reader)? {
            plan_shape!("ann_index", self.collection.name(), c.len());
            c
        } else if let Some(c) = self.text_candidates(reader)? {
            plan_shape!("text_index", self.collection.name(), c.len());
            c
        } else if let Some(c) = self.indexed_candidates(reader)? {
            plan_shape!("indexed_window", self.collection.name(), c.len());
            c
        } else if let Some(c) = self.streaming_vector_candidates(reader)? {
            plan_shape!("streaming_top_k", self.collection.name(), c.len());
            c
        } else {
            let c: Vec<(Vec<u8>, Value)> = self
                .collection
                .scan_in(reader)?
                .into_iter()
                .filter(|(_, doc)| self.filters.iter().all(|p| p.eval(doc)))
                .collect();
            plan_shape!("scan", self.collection.name(), c.len());
            c
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
                compare_by_field_class(va, vb, *descending, ka, kb)
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

    /// Serve a filterless `order_by(field)` query (no retrieval sources,
    /// no filters) from the field's COMPLETE scalar index instead of
    /// materializing and sorting every row: the index walk
    /// (`scalar::comparable_entries`) enumerates the COMPARABLE class
    /// (ints/floats numerically, then texts lexically — the order
    /// contract's class-0 order) and documents are fetched only for the
    /// `offset + limit` window. Docs the index cannot hold — missing the
    /// field, or incomparable values (bools, NaN, containers) — sort after
    /// every comparable row in BOTH directions, so they only matter when
    /// the window reaches past the comparable set; the on-exhaustion
    /// fallback scans that tail and orders it with the same comparator.
    /// Walk-order ++ tail-order is exactly `sort_by_field`'s total order,
    /// so results are identical to the scan path by construction.
    ///
    /// Decline rules (identical results or decline — never approximate):
    /// * any retrieval source: there the rank/fusion order is the
    ///   contract, and this arm only covers the no-source shape;
    /// * any filter: the selectivity-driven window machinery serves
    ///   those, and an order walk would fetch-and-drop every comparable
    ///   document for a selective filter — over-fetch that is unbounded
    ///   without a window;
    /// * no complete scalar index on the ordering field (a building def
    ///   is resumed by [`Self::run`] before the snapshot opens; under a
    ///   contended resume the gate stays false and the scan path serves).
    ///
    /// Exactness notes: entries sharing one encoded value (true ties —
    /// `-0.0`/`+0.0`, equal texts — and distinct i64s beyond 2^53 that
    /// collapse onto one f64 in the numeric lane) are flushed as one
    /// bucket ordered by the EXACT comparator, so the index's doc-key
    /// tiebreak never leaks a precision collision into the result; NaN
    /// entries are indexed floats but incomparable, so the walk skips
    /// them and they land in the tail with the other class-1 rows.
    fn order_index_rows(&self, reader: &dyn SnapshotReader) -> Result<Option<Vec<ResultRow>>> {
        let Some((field, descending)) = &self.order_by else {
            return Ok(None);
        };
        if !self.filters.is_empty() || !self.sources.is_empty() {
            return Ok(None);
        }
        let collection = self.collection;
        let coll = collection.name();
        if !collection.db().has_scalar_index(coll, field) {
            return Ok(None);
        }
        // Audit C10: the order index actually served this query.
        #[cfg(test)]
        test_probe::bump_sort();

        let limit = self.limit;
        if limit == Some(0) {
            return Ok(Some(Vec::new())); // the empty window, no walk needed
        }
        let ns = crate::scalar::namespace(coll, field);
        let mut win = OrderWindow {
            collection: &collection,
            field,
            descending: *descending,
            limit,
            skip: self.offset,
            out: Vec::new(),
        };

        if !*descending {
            // Ascending: buckets flush as their encoded value changes; the
            // walk stops the moment the window fills.
            let mut bucket: Vec<Vec<u8>> = Vec::new();
            let mut enc: Option<Vec<u8>> = None;
            let mut stop = false;
            crate::scalar::comparable_entries(reader, &ns, |value, doc_key| {
                if win.full() {
                    stop = true;
                    return Ok(false);
                }
                if enc.as_deref() == Some(value) {
                    bucket.push(doc_key.to_vec());
                } else {
                    if enc.is_some() {
                        // Encoded value changed: the pending bucket is
                        // complete and emits through the window.
                        win.emit_bucket(reader, &bucket)?;
                        bucket.clear();
                    }
                    bucket.push(doc_key.to_vec());
                    enc = Some(value.to_vec());
                }
                Ok(true)
            })?;
            if !stop && !bucket.is_empty() {
                win.emit_bucket(reader, &bucket)?;
            }
        } else {
            // Descending: scan_from pages forward only, so the walk stays
            // ascending and the buffer keeps the NEWEST buckets — the
            // descending head — evicting a complete head bucket only while
            // the retained tail still covers the whole offset+limit window.
            // The buffer therefore holds the window extended back to a
            // bucket boundary (exact within-bucket order needs whole
            // buckets); memory stays bounded by window + largest bucket.
            // A VecDeque so head eviction is O(1) (a Vec's `remove(0)`
            // shifted the whole retained tail on every eviction).
            let window = limit.map_or(usize::MAX, |l| self.offset.saturating_add(l));
            let mut buckets: VecDeque<(Vec<u8>, Vec<Vec<u8>>)> = VecDeque::new();
            let mut buffered = 0usize;
            crate::scalar::comparable_entries(reader, &ns, |value, doc_key| {
                match buckets.back_mut() {
                    Some((e, b)) if e.as_slice() == value => b.push(doc_key.to_vec()),
                    _ => buckets.push_back((value.to_vec(), vec![doc_key.to_vec()])),
                }
                buffered += 1;
                while buckets.len() > 1 && buffered - buckets[0].1.len() >= window {
                    buffered -= buckets.pop_front().expect("len > 1").1.len();
                }
                Ok(true)
            })?;
            for (_, bucket) in buckets.into_iter().rev() {
                if win.full() {
                    break;
                }
                win.emit_bucket(reader, &bucket)?;
            }
        }

        // Exhaustion: the walk ended before the window filled, so the rows
        // still owed are the incomparable/missing docs the index does not
        // hold. Scan them, order by the same comparator, and continue the
        // window — the exact tail of the total order. (Only reachable when
        // offset+limit exceeds the comparable count — the common top-k
        // query never pays this scan.)
        if !win.full() {
            // The walk-vs-scan fallback: the window reached past everything
            // the index holds, so the tail scan runs. Rare by construction
            // (only when offset+limit exceeds the comparable count).
            crate::telemetry::event!(
                DEBUG,
                message = "order_index_tail_scan",
                collection = crate::telemetry::display(collection.name()),
                field = crate::telemetry::display(field),
            );
            let mut tail: Vec<(Vec<u8>, Value)> = Vec::new();
            collection.for_each_doc_in(reader, |key, doc| {
                let comparable = doc
                    .get_path(field)
                    .is_some_and(|v| crate::filter::value_order(v, v).is_some());
                if !comparable {
                    tail.push((key.to_vec(), doc));
                }
                Ok(true)
            })?;
            sort_by_field(&mut tail, field, *descending);
            for (key, doc) in tail {
                if win.full() {
                    break;
                }
                if win.skip > 0 {
                    win.skip -= 1;
                    continue;
                }
                win.out.push((key, doc));
            }
        }

        Ok(Some(
            win.out
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
                .collect(),
        ))
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
/// The canonical form: **text is used bare** — the
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

/// Cost ratio behind [`QueryBuilder::verify_candidates`]' fetch-strategy
/// pick: one candidate point-get (a catalog lookup, a table open, and a
/// root-to-leaf tree descent per key) costs roughly this many sequential
/// row visits of an ordered `for_each` walk, whose between-candidate steps
/// are key comparisons inside already-resident leaf pages. The walk
/// replaces `N` point-gets once
/// `candidates * ROWS_PER_POINT_GET >= collection_len` — a dense window;
/// a sparse window on a large collection keeps the point-gets, since the
/// walk would visit up to the whole collection. Measured on the
/// `selective_window_verify` bench (5k-doc corpus): point-gets win at 1%
/// and 5% window density (221 µs / 456 µs vs the walk's 413 µs / 487 µs),
/// the walk wins at 10% (577 µs vs 745 µs); the two cost lines cross at
/// ~289 candidates of 5k (density ≈ 5.8%, i.e. a ratio of ≈ 17) — see the
/// roadmap Task 4 report for the table.
const ROWS_PER_POINT_GET: u64 = 17;

/// The density crossover itself, factored for direct testing:
/// does a window of `candidates` keys over a collection of
/// `collection_len` records fetch faster as one ordered walk?
fn walk_wins(candidates: usize, collection_len: u64) -> bool {
    (candidates as u64).saturating_mul(ROWS_PER_POINT_GET) >= collection_len
}

/// Union-with-dedup of candidate key sets under the aggregate cap shared by
/// the `In` and `OR` index fast paths (audit B10): each *individual* index
/// window is already capped, but the union across an `In` list's values (or
/// an `OR`'s disjuncts) is not — without this accumulator it could grow with
/// the value count and materialize an unbounded set. [`KeyUnion::push`]
/// reports `false` once the union exceeds the cap, so the caller bails to
/// `Ok(None)` (a full scan) exactly like an unselective single window.
struct KeyUnion {
    seen: std::collections::HashSet<Vec<u8>>,
    out: Vec<Vec<u8>>,
    cap: usize,
}

impl KeyUnion {
    fn with_cap(cap: usize) -> Self {
        KeyUnion {
            seen: std::collections::HashSet::new(),
            out: Vec::new(),
            cap,
        }
    }

    /// Merge `keys` into the union; `false` once the deduped union exceeds
    /// the cap (the caller must discard the set and fall back to a scan).
    fn push(&mut self, keys: impl IntoIterator<Item = Vec<u8>>) -> bool {
        for k in keys {
            if self.seen.insert(k.clone()) {
                self.out.push(k);
                if self.out.len() > self.cap {
                    return false;
                }
            }
        }
        true
    }

    /// The deduped union (only meaningful when every `push` returned `true`).
    fn finish(self) -> Vec<Vec<u8>> {
        self.out
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

/// Statically mirror `scalar::window`'s decline conditions (scalar.rs) over
/// one field's AND-ed comparison constraints: the window is serviceable
/// iff every constraint value is index-encodable (`lane_of` is `Some`:
/// bool/int/float/text — the encodability `encode_value` reports), all the
/// constraint values share one lane (e.g. never Int + Text), and no
/// constraint is a `Ne`. Used by `indexed_window_kind` so the advisory
/// twin declines exactly where the real probe's `scalar_candidates` call
/// would — the over-cap decline stays execution-time only.
fn window_serviceable(constraints: &[(CmpOp, &Value)]) -> bool {
    let mut lane: Option<u8> = None;
    for &(op, value) in constraints {
        let Some(enc) = crate::scalar::encode_value(value) else {
            return false; // non-encodable: lane_of is None
        };
        match lane {
            Some(existing) if existing != enc[0] => return false, // mixed lanes
            _ => lane = Some(enc[0]),
        }
        if op == CmpOp::Ne {
            return false; // a Ne never forms a window
        }
    }
    true
}

/// A value as `f64` for numeric aggregation (int or float), else `None`.
fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// The ordering class of a row's `order_by` value (audit C4): `0` = present
/// and comparable (int/float/text — [`crate::filter::value_order`] is defined
/// for the kind, checked via `value_order(v, v)`), `1` = present but
/// pairwise incomparable (bools, containers, NaN), `2` = missing. Comparing
/// classes FIRST makes missing AND incomparable rows sort after the
/// comparable group instead of interleaving by key. The class order is fixed
/// under `descending` — only the value order within the comparable class
/// reverses.
fn order_class(v: Option<&Value>) -> u8 {
    match v {
        Some(v) => u8::from(crate::filter::value_order(v, v).is_none()),
        None => 2,
    }
}

/// The kind tag ordering the comparable class (audit C4): numbers
/// (Int/Float, compared numerically together) sort before texts — the
/// documented tie rule for cross-kind pairs within class 0. Class 0 never
/// holds bools, containers, or NaN (those are class 1), so the tag is
/// total on the class; ordering by it first — instead of the old
/// cross-kind key-order fallback — makes the comparator a total order.
fn comparable_kind_tag(v: &Value) -> u8 {
    match v {
        Value::Int(_) | Value::Float(_) => 0,
        _ => 1, // Text: the only other kind inside the comparable class
    }
}

/// The order_by comparator shared by [`QueryBuilder::run_with`] and
/// [`sort_by_field`] (audit C4): class first (`order_class` — fixed under
/// `descending`), then the value comparison within the present-value branch
/// (reversed by `descending`; pairs order by the kind tag — numbers before
/// texts — before their value comparison, so cross-kind pairs are total:
/// the old cross-kind key-order fallback admitted intransitive cycles, on
/// which a sort may panic; within the incomparable class the same tag puts
/// NaN ahead of the mutually-unordered kinds, which fall to key order),
/// then key.
fn compare_by_field_class(
    va: Option<&Value>,
    vb: Option<&Value>,
    descending: bool,
    ka: &[u8],
    kb: &[u8],
) -> Ordering {
    order_class(va)
        .cmp(&order_class(vb))
        .then_with(|| match (va, vb) {
            (Some(a), Some(b)) => {
                // Kind tag first (numbers before texts), then value — the
                // whole within-class order reverses under `descending`,
                // while the class order itself stays fixed.
                let base = comparable_kind_tag(a)
                    .cmp(&comparable_kind_tag(b))
                    .then_with(|| crate::filter::value_order(a, b).unwrap_or(Ordering::Equal));
                let base = if descending { base.reverse() } else { base };
                base.then_with(|| ka.cmp(kb))
            }
            // Equal classes mean both present or both missing; the missing
            // pair has no value comparison, the present pair is handled above.
            _ => ka.cmp(kb),
        })
}

/// Sort `(key, doc)` pairs by a scalar field using the order_by class rule
/// (audit C4): comparable values in value order (cross-kind pairs by the
/// kind tag — numbers before texts), then pairwise-incomparable values —
/// ordered by the same kind tag, so numerics (NaN) precede the other
/// incomparable kinds ascending, which are mutually unordered and fall to
/// key order — then rows missing the field; ties by key. `descending`
/// reverses the within-class kind-tag-and-value order in BOTH the
/// comparable and the incomparable class; the class order itself and the
/// key tiebreak are fixed.
fn sort_by_field(buf: &mut [(Vec<u8>, Value)], field: &str, descending: bool) {
    buf.sort_by(|(ka, da), (kb, db)| {
        compare_by_field_class(da.get_path(field), db.get_path(field), descending, ka, kb)
    });
}

/// The order-window state the sort-index walk (`order_index_rows`) flushes
/// its buckets through: the comparator parameters, the rows still to skip
/// (the offset), and the rows emitted so far against the limit.
struct OrderWindow<'c> {
    collection: &'c Collection<'c>,
    field: &'c str,
    descending: bool,
    limit: Option<usize>,
    skip: usize,
    out: Vec<(Vec<u8>, Value)>,
}

impl OrderWindow<'_> {
    /// Whether the offset+limit window is already full.
    fn full(&self) -> bool {
        self.limit.is_some_and(|n| self.out.len() >= n)
    }

    /// Fetch one ORDER-BUCKET's documents — every doc sharing one encoded
    /// index value — order them with the exact `order_by` comparator, and
    /// run them through the skip/limit window.
    ///
    /// Why the re-sort: the index orders same-encoded entries by doc key,
    /// which is the contract's tiebreak for TRUE ties (`-0.0`/`+0.0`, equal
    /// texts) but not for distinct large i64s that collapse onto one `f64`
    /// in the numeric lane — those compare exactly in the contract and must
    /// not inherit the index's key order. A bucket the window skips
    /// entirely is accounted without fetching anything (every entry is
    /// exactly one result row: a complete index on this snapshot mirrors
    /// the documents, and a key with no document would be invisible to the
    /// scan path too).
    fn emit_bucket(&mut self, reader: &dyn SnapshotReader, keys: &[Vec<u8>]) -> Result<()> {
        if self.skip >= keys.len() {
            self.skip -= keys.len();
            return Ok(());
        }
        let mut rows: Vec<(Vec<u8>, Value)> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(doc) = self.collection.get_in(reader, key)? {
                rows.push((key.clone(), doc));
            }
        }
        sort_by_field(&mut rows, self.field, self.descending);
        for (key, doc) in rows {
            if self.full() {
                break; // window already full: account nothing further
            }
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            self.out.push((key, doc));
        }
        Ok(())
    }
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
        // Resolution goes through the uniform field accessor (value.rs):
        // identical dotted traversal plus its documented empty-path rule —
        // `""` resolves no field. Task 11 review fix: this was a third
        // hand-rolled resolver (after filter::resolve) that DID match a
        // top-level `""` key, diverging from `field("")` predicates and from
        // index maintenance; only the OUTPUT-shape builder below is local.
        if let Some(value) = document.get_path(path) {
            insert_path(&mut out, path, value.clone());
        }
    }
    Value::Map(out)
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
/// Text sources score BM25 with corpus statistics (n, avg_len, df) computed
/// over this candidate set, not the whole corpus — unlike the direct
/// `text_search`/`phrase_search` entry points, whose stats are always
/// whole-corpus.
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

/// Test-only observation of which candidate source actually SERVED a query
/// (audit C10): an index path is only proven used when its non-fallback arm
/// returned `Some`, not when a query merely *could* take it. The planner
/// bumps the matching counter at each serve point; tests reset, run, and
/// read. `thread_local` (not a static): each test runs on its own thread,
/// so concurrently-running tests cannot pollute each other's counts.
#[cfg(test)]
pub(crate) mod test_probe {
    use std::cell::Cell;

    /// Serve counts since the last [`reset`]: how many times the ANN, text,
    /// filter-index, and order-index candidate sources each served a query.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Served {
        pub ann: usize,
        pub text: usize,
        pub indexed: usize,
        pub sort: usize,
    }

    thread_local! {
        static ANN: Cell<usize> = const { Cell::new(0) };
        static TEXT: Cell<usize> = const { Cell::new(0) };
        static INDEXED: Cell<usize> = const { Cell::new(0) };
        static SORT: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn bump_ann() {
        ANN.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn bump_text() {
        TEXT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn bump_indexed() {
        INDEXED.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn bump_sort() {
        SORT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn reset() {
        ANN.with(|c| c.set(0));
        TEXT.with(|c| c.set(0));
        INDEXED.with(|c| c.set(0));
        SORT.with(|c| c.set(0));
    }

    pub(crate) fn read() -> Served {
        Served {
            ann: ANN.with(Cell::get),
            text: TEXT.with(Cell::get),
            indexed: INDEXED.with(Cell::get),
            sort: SORT.with(Cell::get),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, field};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// The In/OR union accumulator caps the aggregate (audit B10): pushes
    /// dedupe, a union at exactly the cap stays usable, one past the cap
    /// reports `false` so the caller bails to a scan. Pinned here because
    /// driving the real 100_000-key cap needs a >100k-document fixture whose
    /// predicate verification is quadratic in the In list; the wiring into
    /// `predicate_candidates`/`or_candidate_keys` is the same `push` +
    /// bail-on-`false` pattern in both arms.
    #[test]
    fn key_union_dedupes_and_caps_the_aggregate() {
        let mut u = KeyUnion::with_cap(3);
        assert!(u.push(vec![b"a".to_vec(), b"b".to_vec()]));
        // Duplicates across pushes do not count twice.
        assert!(u.push(vec![b"a".to_vec(), b"c".to_vec()]));
        assert_eq!(
            u.finish(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );

        // Exactly at the cap: still usable.
        let mut u = KeyUnion::with_cap(2);
        assert!(u.push(vec![b"x".to_vec()]));
        assert!(u.push(vec![b"y".to_vec()]));
        assert_eq!(u.finish().len(), 2);

        // One past the cap: over.
        let mut u = KeyUnion::with_cap(2);
        assert!(u.push(vec![b"x".to_vec(), b"y".to_vec()]));
        assert!(!u.push(vec![b"z".to_vec()]), "cap+1 must bail");

        // A single oversized push bails too.
        let mut u = KeyUnion::with_cap(1);
        assert!(!u.push(vec![b"x".to_vec(), b"y".to_vec()]));
    }

    /// The two verify-candidates fetch strategies are observably
    /// identical: same rows, same candidate order (including a duplicated
    /// candidate key), same missing-key skips, same filter verdicts. The
    /// candidate list is deliberately NOT key-sorted and mixes a key
    /// absent from the records, a duplicated key, and a key whose document
    /// fails the filter — the walk must reproduce the point-get loop's
    /// output byte for byte.
    #[test]
    fn verify_candidates_walk_matches_point_gets_exactly() {
        let db = Db::open_in_memory().unwrap();
        let coll = db.collection("docs");
        let numbered = |key: &[u8], n: i64| {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(n));
            coll.insert(key, &Value::Map(m)).unwrap();
        };
        numbered(b"f", 15); // passes n >= 11
        numbered(b"b", 12); // passes
        numbered(b"d", 3); // fails the filter
        numbered(b"g", 20); // passes
        // (no "zzz" — a candidate the records no longer have)

        let candidates = vec![
            b"f".to_vec(),
            b"b".to_vec(),
            b"zzz".to_vec(),
            b"d".to_vec(),
            b"b".to_vec(), // duplicated candidate key
            b"g".to_vec(),
        ];
        let expected_order: Vec<Vec<u8>> =
            vec![b"f".to_vec(), b"b".to_vec(), b"b".to_vec(), b"g".to_vec()];

        db.store()
            .read(|r| {
                let qb = coll.query().filter(field("n").ge(Value::Int(11)));
                let points = qb
                    .verify_candidates_point_gets(r, candidates.clone())
                    .unwrap()
                    .unwrap();
                let walk = qb
                    .verify_candidates_walk(r, candidates.clone(), coll.name())
                    .unwrap()
                    .unwrap();
                assert_eq!(points, walk, "strategies must agree exactly");
                assert_eq!(
                    points.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                    expected_order,
                    "output preserves the caller's candidate order (duplicate emission included)"
                );
                // Every emitted pair carries its own decoded document.
                assert!(
                    points
                        .iter()
                        .all(|(k, doc)| doc.get_path("n").is_some() && !k.is_empty())
                );
                Ok(())
            })
            .unwrap();
    }

    /// The density crossover picks the measured-cheaper strategy: sparse
    /// windows keep point-gets, dense windows take the ordered walk, and
    /// the boundary is the `candidates * ROWS_PER_POINT_GET >= len` line
    /// (17 from the `selective_window_verify` measurements).
    #[test]
    fn verify_fetch_strategy_follows_density_crossover() {
        // The bench densities: 50 of 5k is sparse (points), 500 of 5k is
        // dense (walk).
        assert!(!walk_wins(50, 5_000));
        assert!(walk_wins(500, 5_000));
        // Exactly on the line walks (`>=`); one candidate below it does not.
        assert!(walk_wins(10, 170));
        assert!(!walk_wins(9, 170));
        // An empty collection with any candidates (an inconsistent index)
        // still picks a strategy and stays correct — degenerate arm.
        assert!(walk_wins(1, 0));
    }

    /// End-to-end twin (filters.rs convention) at walk-path density: a
    /// scalar eq window narrowed further by a second AND filter — so the
    /// window's candidate set is a strict superset of the matches —
    /// returns exactly what the same query returns with no index (full
    /// scan), rows and order included. 40 docs with 4 per `cat` value:
    /// `4 * 17 >= 40` puts verification on the walk path.
    #[test]
    fn verify_walk_dense_window_twin_matches_unindexed_scan() {
        let docs = |db: &Db, indexed: bool| {
            let coll = db.collection("docs");
            for i in 0..40u32 {
                let mut m = BTreeMap::new();
                m.insert("cat".to_owned(), Value::Int((i % 10) as i64));
                m.insert("n".to_owned(), Value::Int(i as i64));
                coll.insert(format!("k{i:02}").as_bytes(), &Value::Map(m))
                    .unwrap();
            }
            if indexed {
                coll.create_scalar_index("cat").unwrap();
            }
            coll.query()
                .filter(field("cat").eq(Value::Int(7)))
                .filter(field("n").ge(Value::Int(30)))
                .run()
                .unwrap()
        };
        let idx = docs(&Db::open_in_memory().unwrap(), true);
        let scan = docs(&Db::open_in_memory().unwrap(), false);
        let strip = |rows: Vec<super::ResultRow>| {
            rows.into_iter()
                .map(|r| (r.key, r.document))
                .collect::<Vec<_>>()
        };
        let expected = strip(scan);
        // Sanity: the twin is non-trivial — the window (cat == 7) holds 4
        // docs (i = 7, 17, 27, 37), the second filter keeps only 1 of them.
        assert_eq!(expected.len(), 1);
        assert_eq!(strip(idx), expected);
    }

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

    /// Audit C10: the custom RRF constant is *observed*, not just accepted —
    /// two sources whose fused order differs between k=1 and k=60 must
    /// produce different orders for the two constants.
    ///
    /// Seven 1-D docs, two L2 vector sources (query [0.0] for both, so each
    /// source's ranking is just its field's value ascending). Designed ranks:
    ///
    /// | doc | rank in e1 | rank in e2 |
    /// |-----|-----------:|-----------:|
    /// | a   | 1          | 4          |
    /// | b   | 2          | 2          |
    /// | y1  | 3          | 3          |
    /// | y2  | 4          | 5          |
    /// | y3  | 5          | 6          |
    /// | y4  | 6          | 7          |
    /// | x   | 7          | 1          |
    ///
    /// RRF score = sum of 1/(k + rank), so with f32-exact headroom:
    ///   k=1:  a = 1/2 + 1/5 = 0.7000   b = 1/3 + 1/3 = 0.6667   → a first
    ///         x = 1/8 + 1/2 = 0.6250   y1 = 1/4 + 1/4 = 0.5000 (all < a)
    ///   k=60: a = 1/61 + 1/64 = 0.03202  b = 2/62 = 0.03226     → b first
    ///         x = 1/67 + 1/61 = 0.03132  y1 = 2/63 = 0.03175    (all < b)
    ///
    /// (Large k orders by rank sum — a sums to 5, b to 4 — while small k
    /// amplifies a's single rank-1 enough to win; the a-vs-b crossover
    /// solves 1/(k+1) + 1/(k+4) = 2/(k+2), i.e. k = 2.)
    #[test]
    fn custom_rrf_constant_is_accepted() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        // (doc, e1 value, e2 value): e1 asc is source 1's ranking
        // (a,b,y1,y2,y3,y4,x), e2 asc is source 2's (x,b,y1,a,y2,y3,y4).
        let docs: &[(&[u8], f32, f32)] = &[
            (b"a", 0.1, 0.4),
            (b"b", 0.2, 0.2),
            (b"y1", 0.3, 0.3),
            (b"y2", 0.4, 0.5),
            (b"y3", 0.5, 0.6),
            (b"y4", 0.6, 0.7),
            (b"x", 0.7, 0.1),
        ];
        for &(key, v1, v2) in docs {
            let mut m = BTreeMap::new();
            m.insert("e1".to_owned(), Value::Vector(vec![v1]));
            m.insert("e2".to_owned(), Value::Vector(vec![v2]));
            c.insert(key, &Value::Map(m)).unwrap();
        }
        let run = |rrf_k: f32| {
            c.query()
                .vector("e1", vec![0.0], 10, Metric::L2)
                .vector("e2", vec![0.0], 10, Metric::L2)
                .fuse_rrf(rrf_k)
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key.clone())
                .collect::<Vec<_>>()
        };
        let small = run(1.0);
        let large = run(60.0);
        // The constant is observed: the two orders genuinely differ...
        assert_eq!(small[0], b"a".to_vec(), "k=1 amplifies a's rank-1");
        assert_eq!(large[0], b"b".to_vec(), "k=60 orders by rank sum");
        assert_ne!(small, large, "RRF k must change this fusion's order");
    }

    /// Audit C6: `fuse_rrf` is validated at execution entry (the builder
    /// stays fluent — invalid values are stored, then rejected by run/count/
    /// aggregates with `Error::InvalidArgument`): k must be positive and
    /// finite. Zero, negatives, and NaN divide nothing useful in RRF and
    /// used to flow into the scores silently.
    #[test]
    fn fuse_rrf_rejects_non_positive_and_nan() {
        let db = seed();
        for bad in [0.0f32, -1.0, f32::NAN] {
            let err = db
                .collection("docs")
                .query()
                .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
                .text("body", "rust", 10)
                .fuse_rrf(bad)
                .run()
                .unwrap_err();
            assert!(
                matches!(err, crate::Error::InvalidArgument(_)),
                "fuse_rrf({bad}) must be rejected"
            );
            // Aggregates validate too, not just run.
            let err = db
                .collection("docs")
                .query()
                .fuse_rrf(bad)
                .count()
                .unwrap_err();
            assert!(matches!(err, crate::Error::InvalidArgument(_)));
        }
        // Positive values are accepted, down to the smallest positive f32.
        for good in [f32::MIN_POSITIVE, 60.0] {
            assert!(
                db.collection("docs")
                    .query()
                    .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
                    .text("body", "rust", 10)
                    .fuse_rrf(good)
                    .run()
                    .is_ok(),
                "fuse_rrf({good}) must be accepted"
            );
        }
    }

    /// Audit C6: `rerank_mmr`'s lambda must lie in `[0, 1]` (NaN included in
    /// the rejection) — outside it, MMR's relevance/diversity trade-off is
    /// meaningless. Rejected at execution entry with
    /// `Error::InvalidArgument`; the boundaries themselves are accepted.
    #[test]
    fn rerank_mmr_rejects_out_of_range_and_nan() {
        let db = seed();
        for bad in [-0.1f32, 1.1, f32::NAN] {
            let err = db
                .collection("docs")
                .query()
                .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
                .rerank_mmr(bad)
                .run()
                .unwrap_err();
            assert!(
                matches!(err, crate::Error::InvalidArgument(_)),
                "rerank_mmr({bad}) must be rejected"
            );
        }
        // The closed-interval boundaries are accepted.
        for good in [0.0f32, 1.0] {
            assert!(
                db.collection("docs")
                    .query()
                    .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
                    .rerank_mmr(good)
                    .run()
                    .is_ok(),
                "rerank_mmr({good}) must be accepted"
            );
        }
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
        // Audit C3: the head reports the real shape — seed() registers no
        // indexes, so a filtered exact single-vector query streams.
        assert!(plan.contains("streaming-topk"));
        assert!(!plan.contains("scan(docs)"), "plan must not claim a scan");
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
        test_probe::reset();
        let rows = c
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        // Same top result as the exact path, now served via the index —
        // and provably so: the ANN arm served, nothing else did (C10).
        assert_eq!(rows[0].key, b"a".to_vec());
        assert_eq!(
            test_probe::read(),
            test_probe::Served {
                ann: 1,
                text: 0,
                indexed: 0,
                sort: 0
            }
        );
    }

    #[test]
    fn approx_filtered_vector_query_uses_index_and_respects_filter() {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        test_probe::reset();
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
        // approx is what admits the filtered query onto the index path (C10).
        assert_eq!(
            test_probe::read(),
            test_probe::Served {
                ann: 1,
                text: 0,
                indexed: 0,
                sort: 0
            }
        );
    }

    #[test]
    fn filtered_vector_query_without_approx_is_exact_but_correct() {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        test_probe::reset();
        // No .approx(): exact path, still correct, still filtered.
        let rows = c
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .run()
            .unwrap();
        assert_eq!(rows[0].key, b"a".to_vec());
        assert!(!rows.iter().any(|r| r.key == b"c".to_vec()));
        // "Exact" means the ANN index did NOT serve (C10): seed() has no
        // scalar index, so the streaming top-k arm drove instead.
        assert_eq!(
            test_probe::read(),
            test_probe::Served {
                ann: 0,
                text: 0,
                indexed: 0,
                sort: 0
            }
        );
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
        test_probe::reset();
        // "rust" appears in a (blog) and c (news); indexed text path ranks them.
        let rows = c.query().text("body", "rust", 10).run().unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert!(keys.contains(&b"a".to_vec()));
        assert!(keys.contains(&b"c".to_vec()));
        assert!(!keys.contains(&b"b".to_vec())); // "python web framework"
        // Provably the text index served, and only it (C10).
        assert_eq!(
            test_probe::read(),
            test_probe::Served {
                ann: 0,
                text: 1,
                indexed: 0,
                sort: 0
            }
        );
    }

    /// seed() plus every index registered: vector (embedding), text (body),
    /// scalar (category) — the fixture for plan-shape tests.
    fn seeded_and_indexed() -> Db {
        let db = seed();
        let c = db.collection("docs");
        c.create_vector_index("embedding", Metric::Cosine).unwrap();
        c.create_text_index("body").unwrap();
        c.create_scalar_index("category").unwrap();
        db
    }

    /// Three docs with a "home" `[lat, lon]` point (a = London, b = Paris,
    /// c = Tokyo) and a geo index on "home" — the fixture for geo kind rows.
    fn geo_seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for (key, (lat, lon)) in [
            (b"a".as_slice(), (51.5, -0.13)),
            (b"b".as_slice(), (48.85, 2.35)),
            (b"c".as_slice(), (35.68, 139.69)),
        ] {
            let mut m = BTreeMap::new();
            m.insert(
                "home".to_owned(),
                Value::Array(vec![Value::Float(lat), Value::Float(lon)]),
            );
            c.insert(key, &Value::Map(m)).unwrap();
        }
        c.create_geo_index("home").unwrap();
        db
    }

    /// Audit C3: every PlanShape renders as its own explain head, with the
    /// decorations (filters/sources/limit) still attached after it.
    #[test]
    fn explain_reports_each_plan_shape() {
        let db = seeded_and_indexed();
        let c = db.collection("docs");

        let ann = c
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .limit(2)
            .explain();
        assert!(ann.starts_with("ann(embedding)"), "{ann}");
        assert!(ann.contains("vector(embedding"), "{ann}");
        assert!(ann.contains("limit 2"), "{ann}");

        let text = c.query().text("body", "rust", 10).explain();
        assert!(text.starts_with("text-index(body)"), "{text}");
        assert!(text.contains("text(body, k=10)"), "{text}");

        let indexed = c
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .explain();
        assert!(indexed.starts_with("indexed-window(scalar)"), "{indexed}");
        assert!(indexed.contains("filter x1"), "{indexed}");

        // A filterless order_by over the scalar-indexed `category` field
        // plans as the sort-index walk.
        let sort = c.query().order_by("category", false).explain();
        assert!(sort.starts_with("sort-index(category)"), "{sort}");
        assert!(sort.contains("order_by(category)"), "{sort}");

        // No vector index on this one → the single vector source streams.
        let plain = Db::open_in_memory().unwrap();
        let pc = plain.collection("docs");
        pc.insert(b"a", &doc("blog", "rust embedded database", vec![1.0, 0.0]))
            .unwrap();
        let streaming = pc
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
            .explain();
        assert!(streaming.starts_with("streaming-topk"), "{streaming}");
        assert!(streaming.contains("vector(embedding"), "{streaming}");

        let scan = c.query().explain();
        assert!(scan.starts_with("scan(docs)"), "{scan}");
    }

    /// Audit C3 parity net: for a matrix of query shapes (filtered/unfiltered
    /// × single-vector/single-text/multi-source/none × approx on/off, indexes
    /// registered), `plan_shape()` must predict the arm `run_with` actually
    /// drove — observed through the C10 serve counters, not through
    /// `explain`'s own claim about itself.
    #[test]
    fn plan_shape_matches_served_path() {
        let db = seeded_and_indexed();
        let c = db.collection("docs");

        #[derive(Debug, Clone, Copy)]
        enum Src {
            Vector,
            Text,
            Both,
            None,
        }
        let ann = || PlanShape::AnnIndex {
            field: "embedding".to_owned(),
        };
        let text_shape = || PlanShape::TextIndex {
            field: "body".to_owned(),
        };
        let indexed = || PlanShape::IndexedWindow { kind: "scalar" };
        let scan = || PlanShape::Scan {
            collection: "docs".to_owned(),
        };

        for filtered in [false, true] {
            for approx in [false, true] {
                for src in [Src::Vector, Src::Text, Src::Both, Src::None] {
                    let label = format!("{:?} filtered={} approx={:?}", src, filtered, approx);
                    let mut q = c.query();
                    if filtered {
                        q = q.filter(field("category").eq(Value::Text("blog".into())));
                    }
                    match src {
                        Src::Vector => {
                            q = q.vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine);
                        }
                        Src::Text => {
                            q = q.text("body", "rust", 10);
                        }
                        Src::Both => {
                            q = q
                                .vector("embedding", vec![1.0, 0.0], 10, Metric::Cosine)
                                .text("body", "rust", 10);
                        }
                        Src::None => {}
                    }
                    if approx {
                        q = q.approx();
                    }

                    // The planner's ladder, written out: the single-source
                    // index arms take filtered queries only under approx;
                    // otherwise a serviceable filter index drives; multi or
                    // no-source unfiltered queries scan.
                    let expected = match src {
                        Src::None | Src::Both => {
                            if filtered {
                                indexed()
                            } else {
                                scan()
                            }
                        }
                        Src::Vector => {
                            if !filtered || approx {
                                ann()
                            } else {
                                indexed()
                            }
                        }
                        Src::Text => {
                            if !filtered || approx {
                                text_shape()
                            } else {
                                indexed()
                            }
                        }
                    };

                    assert_eq!(q.plan_shape(), expected, "plan_shape lied: {label}");
                    test_probe::reset();
                    let rows = q.run().unwrap();
                    let served = test_probe::read();
                    match &expected {
                        PlanShape::AnnIndex { .. } => assert_eq!(
                            (served.ann, served.text, served.indexed),
                            (1, 0, 0),
                            "ANN shape but served {served:?}: {label}"
                        ),
                        PlanShape::TextIndex { .. } => assert_eq!(
                            (served.ann, served.text, served.indexed),
                            (0, 1, 0),
                            "text shape but served {served:?}: {label}"
                        ),
                        PlanShape::IndexedWindow { .. } => assert_eq!(
                            (served.ann, served.text, served.indexed),
                            (0, 0, 1),
                            "indexed shape but served {served:?}: {label}"
                        ),
                        PlanShape::SortIndex { .. } => unreachable!(
                            "the matrix sets no order_by; sort rows are below: {label}"
                        ),
                        PlanShape::StreamingTopK | PlanShape::Scan { .. } => assert_eq!(
                            (served.ann, served.text, served.indexed),
                            (0, 0, 0),
                            "no-index shape but served {served:?}: {label}"
                        ),
                        // (Disclosed limit: the counters cannot distinguish
                        // StreamingTopK from Scan — both serve all-zero.)
                    }
                    assert!(!rows.is_empty(), "fixture row sanity: {label}");
                }
            }
        }

        // The streaming arm needs a collection with NO vector index.
        let plain = seed();
        let q = plain.collection("docs").query().vector(
            "embedding",
            vec![1.0, 0.0],
            10,
            Metric::Cosine,
        );
        assert_eq!(q.plan_shape(), PlanShape::StreamingTopK);
        test_probe::reset();
        assert!(!q.run().unwrap().is_empty());
        assert_eq!(test_probe::read(), test_probe::Served::default());

        // One prediction-vs-reality row: plan_shape first, then run, then the
        // serve counters must match the predicted arm.
        fn parity(q: QueryBuilder<'_>, expected: PlanShape, label: &str) {
            assert_eq!(q.plan_shape(), expected, "plan_shape lied: {label}");
            test_probe::reset();
            let rows = q.run().unwrap();
            let served = test_probe::read();
            match expected {
                PlanShape::IndexedWindow { .. } => assert_eq!(
                    (served.ann, served.text, served.indexed),
                    (0, 0, 1),
                    "indexed shape but served {served:?}: {label}"
                ),
                PlanShape::StreamingTopK | PlanShape::Scan { .. } => assert_eq!(
                    (served.ann, served.text, served.indexed),
                    (0, 0, 0),
                    "no-index shape but served {served:?}: {label}"
                ),
                _ => unreachable!("window/scan shapes only: {label}"),
            }
            assert!(!rows.is_empty(), "fixture row sanity: {label}");
        }

        // The same prediction-vs-reality check for rows whose filters are
        // ALSO unsatisfiable at evaluation time — an AND'd comparison with a
        // non-encodable or wrong-lane constant is false for every document,
        // so the scan they must take returns nothing. The serve counters,
        // not row presence, carry the parity signal.
        fn parity_empty(q: QueryBuilder<'_>, expected: PlanShape, label: &str) {
            assert_eq!(q.plan_shape(), expected, "plan_shape lied: {label}");
            test_probe::reset();
            let rows = q.run().unwrap();
            let served = test_probe::read();
            match expected {
                PlanShape::IndexedWindow { .. } => assert_eq!(
                    (served.ann, served.text, served.indexed),
                    (0, 0, 1),
                    "indexed shape but served {served:?}: {label}"
                ),
                PlanShape::StreamingTopK | PlanShape::Scan { .. } => assert_eq!(
                    (served.ann, served.text, served.indexed),
                    (0, 0, 0),
                    "no-index shape but served {served:?}: {label}"
                ),
                _ => unreachable!("window/scan shapes only: {label}"),
            }
            assert!(rows.is_empty(), "fixture row sanity: {label}");
        }

        // Review round 1, OR divergence 1: the real OR probe
        // (`or_candidate_keys`) declines on the FIRST top-level Or — an
        // unserviceable disjunct there must not be rescued by a later,
        // serviceable Or.
        let or_db = seed();
        or_db
            .collection("docs")
            .create_scalar_index("category")
            .unwrap();
        parity(
            or_db
                .collection("docs")
                .query()
                .filter(
                    field("body")
                        .starts_with("rust")
                        .or(field("body").starts_with("python")),
                )
                .filter(
                    field("category")
                        .eq(Value::Text("blog".into()))
                        .or(field("category").eq(Value::Text("news".into()))),
                ),
            PlanShape::Scan {
                collection: "docs".to_owned(),
            },
            "first Or unserviceable, second serviceable",
        );

        // Review round 1, OR divergence 2: a geo disjunct whose radius wraps
        // the antimeridian is NOT serviceable (`radius_bbox` -> None), so the
        // real OR declines even with a geo index registered.
        let geo = geo_seeded();
        parity(
            geo.collection("docs").query().filter(
                field("home")
                    .within_km(0.0, 179.0, 500.0)
                    .or(field("home").within_km(51.5, -0.13, 50.0)),
            ),
            PlanShape::Scan {
                collection: "docs".to_owned(),
            },
            "geo disjunct with wrapping radius",
        );

        // Compound kind: constraints covering every field of the compound
        // index (equality prefix + range tail; no scalar index on the
        // fields, so the ladder reaches step 3).
        let comp = seed();
        comp.collection("docs")
            .create_compound_index(&["category", "body"])
            .unwrap();
        parity(
            comp.collection("docs")
                .query()
                .filter(field("category").eq(Value::Text("blog".into())))
                .filter(field("body").le(Value::Text("s".into()))),
            PlanShape::IndexedWindow { kind: "compound" },
            "compound window",
        );
        // The seed corpus has EVERY doc carrying both indexed fields, so the
        // def completed with `all_docs_indexed` = true: a prefix-only query
        // (trailing field unconstrained) is now SOUND — every matching doc
        // has the leading field, hence is indexed — and takes the window.
        parity(
            comp.collection("docs")
                .query()
                .filter(field("category").eq(Value::Text("blog".into()))),
            PlanShape::IndexedWindow { kind: "compound" },
            "compound prefix-only served on an all-indexed corpus",
        );
        // The flag is live: one insert missing the trailing field
        // permanently clears it, and the same prefix-only query declines
        // again (the new doc matches the filter but sits outside the index).
        {
            let mut m = BTreeMap::new();
            m.insert("category".to_owned(), Value::Text("blog".to_owned()));
            comp.collection("docs")
                .insert(b"d", &Value::Map(m))
                .unwrap(); // no body at all
        }
        parity(
            comp.collection("docs")
                .query()
                .filter(field("category").eq(Value::Text("blog".into()))),
            PlanShape::Scan {
                collection: "docs".to_owned(),
            },
            "compound prefix-only declines once a doc misses the tail field",
        );
        // And the flag recompute: re-creating the index while the
        // missing-field doc remains keeps the decline (the backfill counts
        // it as a miss)...
        comp.collection("docs")
            .create_compound_index(&["category", "body"])
            .unwrap();
        assert_eq!(
            comp.collection("docs")
                .query()
                .filter(field("category").eq(Value::Text("blog".into())))
                .plan_shape(),
            PlanShape::Scan {
                collection: "docs".to_owned()
            },
            "re-registration over a corpus with missing docs recomputes false"
        );
        // ...while deleting it and re-creating re-earns the flag.
        comp.collection("docs").delete(b"d").unwrap();
        comp.collection("docs")
            .create_compound_index(&["category", "body"])
            .unwrap();
        assert_eq!(
            comp.collection("docs")
                .query()
                .filter(field("category").eq(Value::Text("blog".into())))
                .plan_shape(),
            PlanShape::IndexedWindow { kind: "compound" },
            "re-registration over an all-present corpus recomputes true"
        );

        // Geo kind: a top-level GeoWithin over a geo index with a real bbox.
        parity(
            geo.collection("docs")
                .query()
                .filter(field("home").within_km(51.5, -0.13, 50.0)),
            PlanShape::IndexedWindow { kind: "geo" },
            "geo window",
        );

        // OR kind: the single top-level Or has only serviceable disjuncts.
        parity(
            or_db.collection("docs").query().filter(
                field("category")
                    .eq(Value::Text("blog".into()))
                    .or(field("category").eq(Value::Text("news".into()))),
            ),
            PlanShape::IndexedWindow { kind: "or" },
            "or window",
        );

        // Review round 2, serviceability mirroring: the real probes decline
        // on conditions the twin CAN see statically — a Ne among the field's
        // comparisons, a non-encodable constraint value (containers/bytes/
        // null), constraints mixing lanes (Int + Text on one field), an In
        // list holding a non-encodable member, an OR disjunct comparing
        // against a non-encodable constant, and (compound) a non-encodable
        // prefix Eq value or an unserviceable tail. Prediction and execution
        // must both decline to Scan for those, and keep the window for clean
        // constraints.
        let sdb = Db::open_in_memory().unwrap();
        let sc = sdb.collection("docs");
        for i in 0..10i64 {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(i));
            sc.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        sc.create_scalar_index("n").unwrap();
        let scan_shape = || PlanShape::Scan {
            collection: "docs".to_owned(),
        };

        // ne + range on an indexed field: `scalar::window` declines the Ne.
        parity(
            sdb.collection("docs")
                .query()
                .filter(field("n").ne(Value::Int(5)))
                .filter(field("n").ge(Value::Int(1))),
            scan_shape(),
            "ne+range on an indexed field",
        );
        // Mixed lanes (Int + Text bounds on one field): `window` declines.
        parity_empty(
            sdb.collection("docs")
                .query()
                .filter(field("n").ge(Value::Int(1)))
                .filter(field("n").le(Value::Text("z".into()))),
            scan_shape(),
            "mixed-lane constraints on an indexed field",
        );
        // Non-encodable constraint value (an Array): `lane_of` is None.
        parity_empty(
            sdb.collection("docs")
                .query()
                .filter(field("n").ge(Value::Array(vec![Value::Int(1)]))),
            scan_shape(),
            "non-encodable constraint value on an indexed field",
        );
        // A Between whose bounds mix lanes: same decline via
        // `predicate_candidates`'s [Ge, Le] window.
        parity_empty(
            sdb.collection("docs")
                .query()
                .filter(field("n").between(Value::Int(1), Value::Text("z".into()))),
            scan_shape(),
            "between with mixed-lane bounds on an indexed field",
        );
        // An In list with a non-encodable member: that member's Eq window
        // declines, so the whole In probe declines.
        parity(
            sdb.collection("docs")
                .query()
                .filter(field("n").is_in([Value::Int(3), Value::Map(BTreeMap::new())])),
            scan_shape(),
            "in list with a non-encodable member",
        );
        // An OR disjunct comparing against a non-encodable constant: the
        // disjunct's probe declines, so the whole OR declines.
        parity(
            sdb.collection("docs").query().filter(
                field("n")
                    .eq(Value::Int(3))
                    .or(field("n").eq(Value::Array(vec![Value::Int(1)]))),
            ),
            scan_shape(),
            "or disjunct with a non-encodable constant",
        );
        // The clean counterpart still predicts and takes the window.
        parity(
            sdb.collection("docs")
                .query()
                .filter(field("n").ge(Value::Int(1)))
                .filter(field("n").le(Value::Int(5))),
            PlanShape::IndexedWindow { kind: "scalar" },
            "clean range on an indexed field",
        );

        // Sort-index kind: a FILTERLESS order_by over a completely indexed
        // field is served by the index order walk (both directions, with a
        // window); any filtered order_by declines it — the selectivity-
        // driven window keeps those, ordering on top of the candidates.
        fn parity_sort(q: QueryBuilder<'_>, expected: PlanShape, label: &str) {
            assert_eq!(q.plan_shape(), expected, "plan_shape lied: {label}");
            test_probe::reset();
            let rows = q.run().unwrap();
            let served = test_probe::read();
            assert_eq!(
                (served.ann, served.text, served.indexed, served.sort),
                (0, 0, 0, 1),
                "sort-index shape but served {served:?}: {label}"
            );
            assert!(!rows.is_empty(), "fixture row sanity: {label}");
        }
        parity_sort(
            sdb.collection("docs").query().order_by("n", false).limit(3),
            PlanShape::SortIndex {
                field: "n".to_owned(),
            },
            "sort-index walk asc",
        );
        parity_sort(
            sdb.collection("docs")
                .query()
                .order_by("n", true)
                .offset(2)
                .limit(3),
            PlanShape::SortIndex {
                field: "n".to_owned(),
            },
            "sort-index walk desc window",
        );
        parity(
            sdb.collection("docs")
                .query()
                .filter(field("n").ge(Value::Int(1)))
                .order_by("n", false),
            PlanShape::IndexedWindow { kind: "scalar" },
            "filtered order_by declines the sort-index walk",
        );
        parity(
            sdb.collection("docs").query().order_by("other", false),
            PlanShape::Scan {
                collection: "docs".to_owned(),
            },
            "order_by on an unindexed field scans",
        );

        // Compound twins: no scalar index anywhere, so the ladder reaches
        // step 3 only.
        let cdb = Db::open_in_memory().unwrap();
        let cc = cdb.collection("docs");
        for i in 0..10i64 {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("t".into()));
            m.insert("n".to_owned(), Value::Int(i));
            cc.insert(&[i as u8], &Value::Map(m)).unwrap();
        }
        cc.create_compound_index(&["tag", "n"]).unwrap();
        // Eq on a container value in the prefix: `encode_tuple` declines.
        parity_empty(
            cdb.collection("docs")
                .query()
                .filter(field("tag").eq(Value::Array(vec![Value::Int(1)]))),
            scan_shape(),
            "compound prefix Eq on a container value",
        );
        // Ne in the tail: the tail's `window` declines, and the real probe
        // does not retry any other shape for the winner index.
        parity(
            cdb.collection("docs")
                .query()
                .filter(field("tag").eq(Value::Text("t".into())))
                .filter(field("n").ne(Value::Int(5))),
            scan_shape(),
            "compound tail with a Ne",
        );

        // Building on-disk vector def (review round 1 coverage): consultable
        // is false — mid-build defs never serve — so prediction and reality
        // both decline to the streaming arm. Reality is driven through
        // `run_with` on a fresh snapshot because `run()` would first
        // resume-and-complete the build, which is a different (no longer
        // stuck) fixture.
        let building = seed();
        building
            .register_vector_index(
                "docs",
                "embedding",
                Metric::Cosine,
                crate::quant::Quantization::None,
                crate::index::IndexKind::OnDisk,
            )
            .unwrap();
        let q = building.collection("docs").query().vector(
            "embedding",
            vec![1.0, 0.0],
            10,
            Metric::Cosine,
        );
        assert_eq!(q.plan_shape(), PlanShape::StreamingTopK);
        test_probe::reset();
        let rows = building.store().read(|r| q.run_with(r)).unwrap();
        assert_eq!(test_probe::read(), test_probe::Served::default());
        assert!(!rows.is_empty());

        // And one explain-vs-path cross-check through the instrumentation:
        // the shape explain prints is the arm that actually served.
        let q = db
            .collection("docs")
            .query()
            .filter(field("category").eq(Value::Text("blog".into())))
            .text("body", "rust", 10)
            .approx();
        assert!(q.explain().starts_with("text-index(body)"));
        test_probe::reset();
        assert!(!q.run().unwrap().is_empty());
        assert_eq!(test_probe::read().text, 1);
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

    /// Seed for the order_by class tests: three comparable Ints, two
    /// pairwise-incomparable Bools with keys that sort BEFORE the Ints' keys
    /// (so interleave-by-key — the old behavior — is distinguishable from
    /// class grouping), and one row missing the field.
    fn order_class_seed(c: &crate::Collection) {
        let row = |v: Option<Value>| {
            let mut m = BTreeMap::new();
            if let Some(v) = v {
                m.insert("v".to_owned(), v);
            }
            Value::Map(m)
        };
        c.insert(b"b1", &row(Some(Value::Bool(true)))).unwrap();
        c.insert(b"b0", &row(Some(Value::Bool(false)))).unwrap();
        c.insert(b"i3", &row(Some(Value::Int(3)))).unwrap();
        c.insert(b"i1", &row(Some(Value::Int(1)))).unwrap();
        c.insert(b"i2", &row(Some(Value::Int(2)))).unwrap();
        c.insert(b"zz", &row(None)).unwrap();
    }

    /// Audit C4: order_by's class rule — comparable values (int/float/text)
    /// first in value order, pairwise-incomparable values (bools, containers,
    /// NaN) after them (key order within the class), missing last. The old
    /// comparators treated an incomparable pair as `Equal`, so bools
    /// interleaved by key ahead of the comparable group.
    #[test]
    fn order_by_sorts_incomparable_after_comparable_and_missing_last() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        order_class_seed(&c);
        let rows = c.query().order_by("v", false).run().unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                b"i1".to_vec(),
                b"i2".to_vec(),
                b"i3".to_vec(),
                b"b0".to_vec(),
                b"b1".to_vec(),
                b"zz".to_vec()
            ],
            "comparable group in value order, then incomparable, then missing"
        );
    }

    /// Audit C4: `descending` reverses the value order WITHIN the comparable
    /// class only — the class order itself is fixed, so incomparable and
    /// missing values still sort last.
    #[test]
    fn order_by_descending_reverses_values_but_not_classes() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        order_class_seed(&c);
        let rows = c.query().order_by("v", true).run().unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                b"i3".to_vec(),
                b"i2".to_vec(),
                b"i1".to_vec(),
                b"b0".to_vec(),
                b"b1".to_vec(),
                b"zz".to_vec()
            ],
            "descending reverses the comparable values, not the classes"
        );
    }

    /// Audit C4, mixed kinds: Int and Text are each comparable (class 0), so
    /// they form ONE group ahead of the bools; within that group each kind is
    /// value-ordered among its own (cross-kind pairs stay pairwise
    /// incomparable → key order). The same class rule must hold on the
    /// ranked-source arm of `run_with` (step 5), not just the filter-only
    /// sort paths.
    #[test]
    fn order_by_mixed_kinds_group_comparable_ahead_of_incomparable() {
        let seed_mixed = |c: &crate::Collection, embed: bool| {
            let row = |v: Value| {
                let mut m = BTreeMap::new();
                m.insert("v".to_owned(), v);
                if embed {
                    m.insert("e".to_owned(), Value::Vector(vec![1.0, 0.0]));
                }
                Value::Map(m)
            };
            c.insert(b"t1", &row(Value::Text("a".into()))).unwrap();
            c.insert(b"t2", &row(Value::Text("b".into()))).unwrap();
            c.insert(b"n2", &row(Value::Int(2))).unwrap();
            c.insert(b"n1", &row(Value::Int(1))).unwrap();
            c.insert(b"b1", &row(Value::Bool(true))).unwrap();
            let mut m = BTreeMap::new();
            if embed {
                m.insert("e".to_owned(), Value::Vector(vec![1.0, 0.0]));
            }
            c.insert(b"zz", &Value::Map(m)).unwrap();
        };
        let check = |keys: &[Vec<u8>]| {
            let pos = |k: &[u8]| keys.iter().position(|x| x == k).unwrap();
            // Every comparable row precedes every incomparable row, which
            // precedes the missing one.
            for comp in [
                b"t1".as_ref(),
                b"t2".as_ref(),
                b"n1".as_ref(),
                b"n2".as_ref(),
            ] {
                assert!(
                    pos(comp) < pos(b"b1"),
                    "comparable {comp:?} must precede the bool"
                );
                assert!(
                    pos(comp) < pos(b"zz"),
                    "comparable {comp:?} must precede missing"
                );
            }
            assert!(pos(b"b1") < pos(b"zz"));
            // Within the comparable group, each kind is value-ordered.
            assert!(pos(b"t1") < pos(b"t2"), "texts in lexical order");
            assert!(pos(b"n1") < pos(b"n2"), "ints in numeric order");
        };

        // Filter-only query (streaming sort path).
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        seed_mixed(&c, false);
        let keys: Vec<_> = c
            .query()
            .order_by("v", false)
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        check(&keys);

        // Ranked-source query: a vector source runs the fused path, and the
        // order_by arm of run_with (step 5) re-sorts by the same class rule.
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        seed_mixed(&c, true);
        let keys: Vec<_> = c
            .query()
            .vector("e", vec![1.0, 0.0], 100, Metric::Cosine)
            .order_by("v", false)
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        check(&keys);
    }

    /// Review round 3 (class-0 total order): within the comparable class,
    /// cross-kind pairs (Int vs Text) used to fall back to key order while
    /// same-kind pairs compared by value — not a total order. The cycle
    /// `Int(2)@kA < Text@kM < Int(1)@kZ < Int(2)@kA` (keys `kA < kM < kZ`)
    /// is constructible whenever kinds interleave by key, and Rust's sort
    /// may panic when it detects such a violation. The fix orders class 0
    /// by a kind tag first — numbers, then texts — so this fixture (kinds
    /// interleaving by key) must sort without panicking, deterministically.
    #[test]
    fn order_by_mixed_int_text_field_sorts_deterministically() {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        // Keys a < b < c < d < e, values alternating kinds so the old
        // cross-kind-by-key rule and the kind-tagged order disagree (the
        // old comparator says Text@b sorts before Int@c, the tag says the
        // number comes first).
        let rows = [
            (b"a".as_slice(), Value::Int(5)),
            (b"b".as_slice(), Value::Text("a".into())),
            (b"c".as_slice(), Value::Int(1)),
            (b"d".as_slice(), Value::Text("b".into())),
            (b"e".as_slice(), Value::Int(3)),
        ];
        for (k, v) in rows {
            let mut m = BTreeMap::new();
            m.insert("v".to_owned(), v);
            c.insert(k, &Value::Map(m)).unwrap();
        }
        let keys = |desc: bool| {
            c.query()
                .order_by("v", desc)
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect::<Vec<_>>()
        };
        // Ascending: numbers in value order, then texts in lexical order.
        assert_eq!(
            keys(false),
            vec![
                b"c".to_vec(),
                b"e".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"d".to_vec()
            ],
            "ascending: numbers (1, 3, 5) before texts (a, b)"
        );
        // Descending reverses the whole within-class order — texts first,
        // each kind reversed — while the class order itself stays fixed.
        assert_eq!(
            keys(true),
            vec![
                b"d".to_vec(),
                b"b".to_vec(),
                b"a".to_vec(),
                b"e".to_vec(),
                b"c".to_vec()
            ],
            "descending: texts reversed, then numbers reversed"
        );
    }

    /// Audit C4 parity: the scalar-index fast path (which sorts via
    /// `sort_by_field` over the indexed candidate set) and the plain scan
    /// must agree on the class rule.
    #[test]
    fn order_by_class_rule_matches_between_scan_and_indexed_paths() {
        let plain = Db::open_in_memory().unwrap();
        order_class_seed_with_tag(&plain.collection("docs"));
        let indexed = Db::open_in_memory().unwrap();
        let ic = indexed.collection("docs");
        order_class_seed_with_tag(&ic);
        ic.create_scalar_index("tag").unwrap();

        let run = |db: &Db| {
            db.collection("docs")
                .query()
                .filter(field("tag").eq(Value::Text("t".into())))
                .order_by("v", false)
                .run()
                .unwrap()
                .into_iter()
                .map(|r| r.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&plain), run(&indexed));
        assert_eq!(
            run(&indexed),
            vec![
                b"i1".to_vec(),
                b"i2".to_vec(),
                b"i3".to_vec(),
                b"b0".to_vec(),
                b"b1".to_vec()
            ]
        );
    }

    /// The class-test seed plus a `tag` field every row carries, so a scalar
    /// index on `tag` can drive the indexed candidate path.
    fn order_class_seed_with_tag(c: &crate::Collection) {
        let row = |v: Option<Value>| {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("t".into()));
            if let Some(v) = v {
                m.insert("v".to_owned(), v);
            }
            Value::Map(m)
        };
        c.insert(b"b1", &row(Some(Value::Bool(true)))).unwrap();
        c.insert(b"b0", &row(Some(Value::Bool(false)))).unwrap();
        c.insert(b"i3", &row(Some(Value::Int(3)))).unwrap();
        c.insert(b"i1", &row(Some(Value::Int(1)))).unwrap();
        c.insert(b"i2", &row(Some(Value::Int(2)))).unwrap();
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

    /// Wave-4 audit B3, scenario 3 (the indexed candidate window): the
    /// discriminator is the index scan itself, not the per-key fetch. Docs
    /// "a" and "z" SWAP the flipped field in one transaction (`insert_batch`):
    /// state A = (z.n=1, a.n=2), state B = (z.n=2, a.n=1), so at every point
    /// in time exactly one of them matches n==1 — valid answers are
    /// {fillers, z} or {fillers, a}, never {fillers} alone and never both.
    /// With scalar indexes on `tag` and `n`, the old shape ran each index
    /// window in its OWN read transaction after the verification snapshot
    /// had already opened: a swap landing between them yields candidates
    /// from one state and documents from the other, losing both docs — a
    /// set matching no point in time. One snapshot for window + verify
    /// cannot produce it. The 4100 fillers (keys between "a" and "z", all
    /// n=1) make the n-window span two `PAGE`s and stretch the
    /// probe-to-verify gap wide enough to observe the race.
    #[test]
    fn indexed_window_scan_and_verify_share_one_snapshot() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const FILLERS: usize = 4100; // > PAGE (4096): the n-window scans 2 pages
        let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
        let c = db.collection("docs");
        let doc = |n: i64| {
            let mut m = BTreeMap::new();
            m.insert("tag".to_owned(), Value::Text("t".into()));
            m.insert("n".to_owned(), Value::Int(n));
            Value::Map(m)
        };
        // Fillers in chunks (keys "f0000".."f4099" sort between "a" and "z").
        let filler_docs: Vec<(Vec<u8>, Value)> = (0..FILLERS)
            .map(|i| (format!("f{i:04}").into_bytes(), doc(1)))
            .collect();
        for chunk in filler_docs.chunks(500) {
            let items: Vec<(&[u8], &Value)> =
                chunk.iter().map(|(k, v)| (k.as_slice(), v)).collect();
            c.insert_batch(&items).unwrap();
        }
        c.insert(b"a", &doc(2)).unwrap(); // state A: a.n=2, z.n=1
        c.insert(b"z", &doc(1)).unwrap();
        c.create_scalar_index("tag").unwrap();
        c.create_scalar_index("n").unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let w = Arc::clone(&db);
        let fin = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            for i in 0..800 {
                // Swap: A -> B -> A -> ... atomically in one transaction.
                let (nz, na) = if i % 2 == 0 { (2, 1) } else { (1, 2) };
                let dz = doc(nz);
                let da = doc(na);
                w.collection("docs")
                    .insert_batch(&[(b"z", &dz), (b"a", &da)])
                    .unwrap();
            }
            fin.store(true, Ordering::Release);
        });

        let mut checks = 0usize;
        while !done.load(Ordering::Acquire) || checks < 20 {
            checks += 1;
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
            let has_a = keys.contains(&b"a".to_vec());
            let has_z = keys.contains(&b"z".to_vec());
            assert_eq!(
                keys.len(),
                FILLERS + 1,
                "query result holds {} of {} fillers (missing {} fillers alongside the flip)",
                keys.len(),
                FILLERS + 1,
                FILLERS + 1 - keys.len().min(FILLERS + 1)
            );
            assert!(
                has_a ^ has_z,
                "query result matched no single snapshot: \
                 fillers + {{a={has_a}, z={has_z}}} — the swap means exactly \
                 one of them matches at every point in time"
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

    /// Wave-4 final review: a FILTERLESS single-source query (the normal
    /// `.vector(...)`/`.text(...)` shape) consults an index via
    /// `ann_candidates`/`text_candidates`, so it must resume interrupted
    /// (Building) builds before its snapshot opens like any filtered query
    /// — the old `filters.is_empty()` early-return left such defs Building
    /// forever, served by the exact fallback.
    #[test]
    fn filterless_single_source_query_resumes_building_index() {
        use crate::index_build::{DefState, decode_def, encode_def};

        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        c.insert(b"a", &doc("blog", "rust embedded database", vec![1.0, 0.0]))
            .unwrap();
        c.create_vector_index_ondisk("embedding", Metric::L2)
            .unwrap();

        // Forge an interrupted creation exactly as a crash would leave it:
        // same def bytes, state flipped to Building{cursor: []}.
        const INDEX_DEFS: &str = "__indexes__"; // mirrors index.rs
        let def_row_key: Vec<u8> = b"docs\0embedding".to_vec(); // coll \0 field
        let row = db
            .store()
            .get(INDEX_DEFS, &def_row_key)
            .unwrap()
            .expect("the on-disk creation wrote a def row");
        let (kb, _) = decode_def(&row);
        db.store()
            .put(
                INDEX_DEFS,
                &def_row_key,
                &encode_def(&kb, &DefState::Building { cursor: vec![] }),
            )
            .unwrap();
        db.load_index_defs().unwrap();
        // Sanity: the forged job exists to be resumed.
        assert_eq!(db.collect_building_vector("docs").unwrap().len(), 1);

        // Filterless single-source query: no filters, one vector source.
        let rows = db
            .collection("docs")
            .query()
            .vector("embedding", vec![1.0, 0.0], 10, Metric::L2)
            .run()
            .unwrap();
        assert_eq!(rows[0].key, b"a".to_vec());

        // The query resumed the build: the durable def row is Complete.
        let row = db
            .store()
            .get(INDEX_DEFS, &def_row_key)
            .unwrap()
            .expect("def row survives the resume");
        let (_, st) = decode_def(&row);
        assert!(
            matches!(st, DefState::Complete),
            "filterless single-source query must resume a Building def"
        );
    }
}
