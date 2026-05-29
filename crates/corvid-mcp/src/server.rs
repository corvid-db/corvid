//! The transport-agnostic tool layer.
//!
//! [`Server`] wraps a [`corvid::Db`] and answers tool calls as JSON-in /
//! JSON-out. This is the logic of the sidecar; a thin MCP/stdio transport
//! (JSON-RPC framing, `initialize`/`tools/list`/`tools/call`) dispatches into
//! [`Server::handle`] and is intentionally kept separate so the behavior here
//! is fully testable without a protocol harness.
//!
//! ## Tools
//!
//! - `store`  — `{collection, key, document}` → `{ok: true}`
//! - `get`    — `{collection, key}` → `{document: <json|null>}`
//! - `delete` — `{collection, key}` → `{deleted: <bool>}`
//! - `search` — `{collection, filter?, vector?, text?, mmr?, rrf_k?, select?, limit?}`
//!   → `{results: [{key, score, document}, ...]}`
//!
//! A `filter` is a small predicate tree:
//! `{op: "eq"|"ne"|"lt"|"le"|"gt"|"ge", field, value}`,
//! `{op: "exists", field}`, `{op: "and"|"or", clauses: [...]}`,
//! `{op: "not", clause: {...}}`.

use corvid::{Db, Metric, Predicate, field};
use serde_json::{Value as Json, json};

use crate::convert::{json_to_value, value_to_json};
use crate::error::ToolError;

/// A corvid-backed MCP tool server.
pub struct Server {
    db: Db,
}

impl Server {
    /// Wrap an existing database.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Open a file-backed server.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, corvid::Error> {
        Ok(Self::new(Db::open(path)?))
    }

    /// Open an in-memory server.
    pub fn open_in_memory() -> Result<Self, corvid::Error> {
        Ok(Self::new(Db::open_in_memory()?))
    }

    /// Handle one tool call by name with its JSON parameters.
    pub fn handle(&self, tool: &str, params: &Json) -> Result<Json, ToolError> {
        match tool {
            "store" => self.store(params),
            "get" => self.get(params),
            "delete" => self.delete(params),
            "search" => self.search(params),
            other => Err(ToolError::UnknownTool(other.to_owned())),
        }
    }

    fn store(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let key = str_param(p, "key")?;
        let doc = p
            .get("document")
            .ok_or_else(|| ToolError::BadParams("missing 'document'".into()))?;
        self.db
            .collection(collection)
            .insert(key.as_bytes(), &json_to_value(doc))?;
        Ok(json!({ "ok": true }))
    }

    fn get(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let key = str_param(p, "key")?;
        let got = self.db.collection(collection).get(key.as_bytes())?;
        Ok(json!({ "document": got.as_ref().map(value_to_json).unwrap_or(Json::Null) }))
    }

    fn delete(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let key = str_param(p, "key")?;
        let deleted = self.db.collection(collection).delete(key.as_bytes())?;
        Ok(json!({ "deleted": deleted }))
    }

    fn search(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let handle = self.db.collection(collection);
        let mut q = handle.query();

        if let Some(f) = p.get("filter") {
            q = q.filter(parse_predicate(f)?);
        }
        if let Some(v) = p.get("vector") {
            let vfield = str_param(v, "field")?;
            let query = f32_array(v.get("query"))?;
            let k = uint_param(v, "k")?;
            let metric = parse_metric(v.get("metric"))?;
            q = q.vector(vfield, query, k, metric);
        }
        if let Some(t) = p.get("text") {
            let tfield = str_param(t, "field")?;
            let query = str_param(t, "query")?;
            let k = uint_param(t, "k")?;
            q = q.text(tfield, query, k);
        }
        if let Some(m) = p.get("mmr") {
            let lambda = m
                .get("lambda")
                .and_then(Json::as_f64)
                .ok_or_else(|| ToolError::BadParams("mmr needs numeric 'lambda'".into()))?;
            q = q.rerank_mmr(lambda as f32);
        }
        if let Some(rrf) = p.get("rrf_k") {
            let k = rrf
                .as_f64()
                .ok_or_else(|| ToolError::BadParams("'rrf_k' must be a number".into()))?;
            q = q.fuse_rrf(k as f32);
        }
        if let Some(sel) = p.get("select") {
            let arr = sel
                .as_array()
                .ok_or_else(|| ToolError::BadParams("'select' must be an array".into()))?;
            let mut fields = Vec::with_capacity(arr.len());
            for e in arr {
                fields.push(
                    e.as_str()
                        .ok_or_else(|| {
                            ToolError::BadParams("'select' entries must be strings".into())
                        })?
                        .to_owned(),
                );
            }
            q = q.select(fields);
        }
        if let Some(l) = p.get("limit") {
            let n = l.as_u64().ok_or_else(|| {
                ToolError::BadParams("'limit' must be a non-negative integer".into())
            })?;
            q = q.limit(n as usize);
        }

        let rows = q.run()?;
        let results: Vec<Json> = rows
            .iter()
            .map(|r| {
                json!({
                    "key": String::from_utf8_lossy(&r.key),
                    "score": r.score,
                    "document": value_to_json(&r.document),
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}

/// Parse a filter predicate tree from JSON.
fn parse_predicate(j: &Json) -> Result<Predicate, ToolError> {
    let obj = j
        .as_object()
        .ok_or_else(|| ToolError::BadParams("filter must be an object".into()))?;
    let op = obj
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| ToolError::BadParams("filter missing 'op'".into()))?;

    match op {
        "and" | "or" => {
            let clauses = obj
                .get("clauses")
                .and_then(Json::as_array)
                .ok_or_else(|| ToolError::BadParams("'and'/'or' need a 'clauses' array".into()))?;
            let mut iter = clauses.iter();
            let first = iter.next().ok_or_else(|| {
                ToolError::BadParams("'and'/'or' need at least one clause".into())
            })?;
            let mut acc = parse_predicate(first)?;
            for c in iter {
                let p = parse_predicate(c)?;
                acc = if op == "and" { acc.and(p) } else { acc.or(p) };
            }
            Ok(acc)
        }
        "not" => {
            let clause = obj
                .get("clause")
                .ok_or_else(|| ToolError::BadParams("'not' needs a 'clause'".into()))?;
            Ok(!parse_predicate(clause)?)
        }
        "exists" => Ok(field(field_param(obj)?).exists()),
        "eq" | "ne" | "lt" | "le" | "gt" | "ge" => {
            let f = field(field_param(obj)?);
            let value = obj
                .get("value")
                .ok_or_else(|| ToolError::BadParams("comparison needs 'value'".into()))?;
            let v = json_to_value(value);
            Ok(match op {
                "eq" => f.eq(v),
                "ne" => f.ne(v),
                "lt" => f.lt(v),
                "le" => f.le(v),
                "gt" => f.gt(v),
                "ge" => f.ge(v),
                _ => unreachable!("op matched above"),
            })
        }
        other => Err(ToolError::BadParams(format!("unknown filter op: {other}"))),
    }
}

fn field_param(obj: &serde_json::Map<String, Json>) -> Result<&str, ToolError> {
    obj.get("field")
        .and_then(Json::as_str)
        .ok_or_else(|| ToolError::BadParams("filter missing string 'field'".into()))
}

fn str_param<'a>(p: &'a Json, key: &str) -> Result<&'a str, ToolError> {
    p.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| ToolError::BadParams(format!("missing string '{key}'")))
}

fn uint_param(p: &Json, key: &str) -> Result<usize, ToolError> {
    p.get(key)
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| ToolError::BadParams(format!("missing non-negative integer '{key}'")))
}

fn f32_array(value: Option<&Json>) -> Result<Vec<f32>, ToolError> {
    let arr = value
        .and_then(Json::as_array)
        .ok_or_else(|| ToolError::BadParams("'query' must be an array of numbers".into()))?;
    arr.iter()
        .map(|e| {
            e.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| ToolError::BadParams("'query' entries must be numbers".into()))
        })
        .collect()
}

fn parse_metric(value: Option<&Json>) -> Result<Metric, ToolError> {
    match value {
        None => Ok(Metric::Cosine),
        Some(j) => match j.as_str() {
            Some("cosine") => Ok(Metric::Cosine),
            Some("dot") => Ok(Metric::Dot),
            Some("l2") => Ok(Metric::L2),
            _ => Err(ToolError::BadParams(
                "'metric' must be one of: cosine, dot, l2".into(),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::open_in_memory().unwrap()
    }

    fn store(s: &Server, key: &str, doc: Json) {
        s.handle(
            "store",
            &json!({ "collection": "docs", "key": key, "document": doc }),
        )
        .unwrap();
    }

    #[test]
    fn file_backed_server_persists_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corvid.db");
        {
            let s = Server::open(&path).unwrap();
            store(&s, "k", json!({"v": 1}));
        }
        let s = Server::open(&path).unwrap();
        let got = s
            .handle("get", &json!({"collection": "docs", "key": "k"}))
            .unwrap();
        assert_eq!(got, json!({"document": {"v": 1}}));
    }

    #[test]
    fn store_get_roundtrip() {
        let s = server();
        store(&s, "k1", json!({"title": "hello", "n": 3}));
        let got = s
            .handle("get", &json!({"collection": "docs", "key": "k1"}))
            .unwrap();
        assert_eq!(got, json!({"document": {"title": "hello", "n": 3}}));
    }

    #[test]
    fn get_missing_is_null() {
        let s = server();
        let got = s
            .handle("get", &json!({"collection": "docs", "key": "nope"}))
            .unwrap();
        assert_eq!(got, json!({"document": null}));
    }

    #[test]
    fn delete_reports_outcome() {
        let s = server();
        store(&s, "k", json!({"a": 1}));
        assert_eq!(
            s.handle("delete", &json!({"collection": "docs", "key": "k"}))
                .unwrap(),
            json!({"deleted": true})
        );
        assert_eq!(
            s.handle("delete", &json!({"collection": "docs", "key": "k"}))
                .unwrap(),
            json!({"deleted": false})
        );
    }

    #[test]
    fn unknown_tool_errors() {
        let s = server();
        let err = s.handle("frobnicate", &json!({})).unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(t) if t == "frobnicate"));
    }

    #[test]
    fn missing_params_error() {
        let s = server();
        assert!(matches!(
            s.handle("get", &json!({"collection": "docs"})).unwrap_err(),
            ToolError::BadParams(_)
        ));
        assert!(matches!(
            s.handle("store", &json!({"collection": "docs", "key": "k"}))
                .unwrap_err(),
            ToolError::BadParams(_)
        ));
    }

    #[test]
    fn search_vector_and_filter() {
        let s = server();
        store(
            &s,
            "a",
            json!({"category": "blog", "embedding": {"$vector": [1.0, 0.0]}}),
        );
        store(
            &s,
            "b",
            json!({"category": "news", "embedding": {"$vector": [0.9, 0.1]}}),
        );
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "filter": {"op": "eq", "field": "category", "value": "blog"},
                    "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 10, "metric": "cosine"},
                    "limit": 5
                }),
            )
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "a");
    }

    #[test]
    fn search_text_modality() {
        let s = server();
        store(&s, "a", json!({"body": "rust embedded database"}));
        store(&s, "b", json!({"body": "python web framework"}));
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "text": {"field": "body", "query": "rust database", "k": 10}
                }),
            )
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results[0]["key"], "a");
    }

    #[test]
    fn search_with_select_projects() {
        let s = server();
        store(&s, "a", json!({"category": "blog", "body": "text here"}));
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "filter": {"op": "exists", "field": "category"},
                    "select": ["category"]
                }),
            )
            .unwrap();
        let doc = &out["results"][0]["document"];
        assert_eq!(doc, &json!({"category": "blog"}));
    }

    #[test]
    fn search_nested_boolean_filter() {
        let s = server();
        store(&s, "a", json!({"cat": "blog", "score": 9}));
        store(&s, "b", json!({"cat": "blog", "score": 2}));
        store(&s, "c", json!({"cat": "news", "score": 9}));
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "filter": {"op": "and", "clauses": [
                        {"op": "eq", "field": "cat", "value": "blog"},
                        {"op": "gt", "field": "score", "value": 5}
                    ]}
                }),
            )
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "a");
    }

    #[test]
    fn search_or_and_not_filters() {
        let s = server();
        store(&s, "a", json!({"cat": "blog"}));
        store(&s, "b", json!({"cat": "news"}));
        store(&s, "c", json!({"cat": "wiki"}));

        let or = s
            .handle(
                "search",
                &json!({"collection": "docs", "filter": {"op": "or", "clauses": [
                    {"op": "eq", "field": "cat", "value": "blog"},
                    {"op": "eq", "field": "cat", "value": "news"}
                ]}}),
            )
            .unwrap();
        assert_eq!(or["results"].as_array().unwrap().len(), 2);

        let not = s
            .handle(
                "search",
                &json!({"collection": "docs", "filter": {"op": "not", "clause":
                    {"op": "eq", "field": "cat", "value": "blog"}
                }}),
            )
            .unwrap();
        assert_eq!(not["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn search_mmr_and_rrf_params_accepted() {
        let s = server();
        store(
            &s,
            "a",
            json!({"body": "rust", "embedding": {"$vector": [1.0, 0.0]}}),
        );
        store(
            &s,
            "b",
            json!({"body": "rust", "embedding": {"$vector": [0.0, 1.0]}}),
        );
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 10},
                    "text": {"field": "body", "query": "rust", "k": 10},
                    "rrf_k": 30.0,
                    "mmr": {"lambda": 0.5}
                }),
            )
            .unwrap();
        assert!(!out["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn search_bad_metric_errors() {
        let s = server();
        let err = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "vector": {"field": "e", "query": [1.0], "k": 1, "metric": "manhattan"}
                }),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::BadParams(_)));
    }

    #[test]
    fn search_bad_filter_shapes_error() {
        let s = server();
        for bad in [
            json!({"collection": "docs", "filter": 42}),
            json!({"collection": "docs", "filter": {"field": "x", "value": 1}}),
            json!({"collection": "docs", "filter": {"op": "weird", "field": "x"}}),
            json!({"collection": "docs", "filter": {"op": "and", "clauses": []}}),
            json!({"collection": "docs", "filter": {"op": "eq", "field": "x"}}),
        ] {
            assert!(
                matches!(
                    s.handle("search", &bad).unwrap_err(),
                    ToolError::BadParams(_)
                ),
                "expected BadParams for {bad}"
            );
        }
    }

    #[test]
    fn search_bad_query_array_errors() {
        let s = server();
        let err = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "vector": {"field": "e", "query": [1.0, "x"], "k": 1}
                }),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::BadParams(_)));
    }
}
