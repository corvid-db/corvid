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

use corvid::{Db, Metric, Predicate, Quantization, field};
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
            "link" => self.link(params),
            "unlink" => self.unlink(params),
            "neighbors" => self.neighbors(params),
            "traverse" => self.traverse(params),
            "create_index" => self.create_index(params),
            "create_text_index" => self.create_text_index(params),
            "geo" => self.geo(params),
            "join" => self.join(params),
            "in_neighbors" => self.in_neighbors(params),
            "list_collections" => self.list_collections(),
            "count" => self.count(params),
            "insert_auto" => self.insert_auto(params),
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

    fn link(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let from = str_param(p, "from")?;
        let relation = str_param(p, "relation")?;
        let to = str_param(p, "to")?;
        self.db
            .collection(collection)
            .link(from.as_bytes(), relation, to.as_bytes())?;
        Ok(json!({ "ok": true }))
    }

    fn unlink(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let from = str_param(p, "from")?;
        let relation = str_param(p, "relation")?;
        let to = str_param(p, "to")?;
        let removed =
            self.db
                .collection(collection)
                .unlink(from.as_bytes(), relation, to.as_bytes())?;
        Ok(json!({ "removed": removed }))
    }

    fn neighbors(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let from = str_param(p, "from")?;
        let relation = str_param(p, "relation")?;
        let mut neighbors = self
            .db
            .collection(collection)
            .neighbors(from.as_bytes(), relation)?;
        neighbors.truncate(result_limit(p));
        Ok(json!({ "neighbors": keys_to_json(&neighbors) }))
    }

    fn in_neighbors(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let to = str_param(p, "to")?;
        let relation = str_param(p, "relation")?;
        let mut neighbors = self
            .db
            .collection(collection)
            .in_neighbors(to.as_bytes(), relation)?;
        neighbors.truncate(result_limit(p));
        Ok(json!({ "neighbors": keys_to_json(&neighbors) }))
    }

    fn traverse(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let start = str_param(p, "start")?;
        let relation = str_param(p, "relation")?;
        let hops = uint_param(p, "hops")?;
        let mut nodes =
            self.db
                .collection(collection)
                .traverse(start.as_bytes(), relation, hops)?;
        nodes.truncate(result_limit(p));
        Ok(json!({ "nodes": keys_to_json(&nodes) }))
    }

    fn create_index(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let field = str_param(p, "field")?;
        let metric = parse_metric(p.get("metric"))?;
        let on_disk = p.get("on_disk").and_then(Json::as_bool).unwrap_or(false);
        let c = self.db.collection(collection);
        if on_disk {
            c.create_vector_index_ondisk(field, metric)?;
        } else {
            c.create_vector_index_quantized(field, metric, parse_quant(p.get("quant"))?)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn create_text_index(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let field = str_param(p, "field")?;
        self.db.collection(collection).create_text_index(field)?;
        Ok(json!({ "ok": true }))
    }

    fn list_collections(&self) -> Result<Json, ToolError> {
        Ok(json!({ "collections": self.db.collections()? }))
    }

    fn count(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let mut q = self.db.collection(collection).query();
        if let Some(f) = p.get("filter") {
            q = q.filter(parse_predicate(f)?);
        }
        Ok(json!({ "count": q.count()? }))
    }

    fn insert_auto(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let doc = p
            .get("document")
            .ok_or_else(|| ToolError::BadParams("missing 'document'".into()))?;
        let key = self
            .db
            .collection(collection)
            .insert_auto(&json_to_value(doc))?;
        Ok(json!({ "key": String::from_utf8_lossy(&key) }))
    }

    fn geo(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let field = str_param(p, "field")?;
        let lat = f64_param(p, "lat")?;
        let lon = f64_param(p, "lon")?;
        let radius_km = f64_param(p, "radius_km")?;
        let mut hits = self
            .db
            .collection(collection)
            .geo_within_radius(field, lat, lon, radius_km)?;
        hits.truncate(result_limit(p));
        let results: Vec<Json> = hits
            .iter()
            .map(|h| {
                json!({
                    "key": String::from_utf8_lossy(&h.key),
                    "distance_km": h.distance_km,
                    "document": value_to_json(&h.document),
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }

    fn join(&self, p: &Json) -> Result<Json, ToolError> {
        let collection = str_param(p, "collection")?;
        let other = str_param(p, "other")?;
        let fk = str_param(p, "foreign_key_field")?;
        let mut rows = self.db.collection(collection).join(other, fk)?;
        rows.truncate(result_limit(p));
        let out: Vec<Json> = rows
            .iter()
            .map(|r| {
                json!({
                    "key": String::from_utf8_lossy(&r.key),
                    "left": value_to_json(&r.left),
                    "right": r.right.as_ref().map(value_to_json).unwrap_or(Json::Null),
                })
            })
            .collect();
        Ok(json!({ "rows": out }))
    }
}

/// Render a list of byte keys as JSON strings.
fn keys_to_json(keys: &[Vec<u8>]) -> Vec<Json> {
    keys.iter()
        .map(|k| Json::String(String::from_utf8_lossy(k).into_owned()))
        .collect()
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
        "geo_within" => {
            let f = field(field_param(obj)?);
            let lat = obj_f64(obj, "lat")?;
            let lon = obj_f64(obj, "lon")?;
            let radius_km = obj_f64(obj, "radius_km")?;
            Ok(f.within_km(lat, lon, radius_km))
        }
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

/// Default cap on list-returning tools so a single call can't dump an
/// unbounded payload. Overridable per call via a `limit` param.
const DEFAULT_LIST_LIMIT: usize = 1000;

/// The result cap for a list-returning tool: the `limit` param, or the default.
fn result_limit(p: &Json) -> usize {
    p.get("limit")
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_LIST_LIMIT)
}

fn uint_param(p: &Json, key: &str) -> Result<usize, ToolError> {
    p.get(key)
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| ToolError::BadParams(format!("missing non-negative integer '{key}'")))
}

fn f64_param(p: &Json, key: &str) -> Result<f64, ToolError> {
    p.get(key)
        .and_then(Json::as_f64)
        .ok_or_else(|| ToolError::BadParams(format!("missing number '{key}'")))
}

fn obj_f64(obj: &serde_json::Map<String, Json>, key: &str) -> Result<f64, ToolError> {
    obj.get(key)
        .and_then(Json::as_f64)
        .ok_or_else(|| ToolError::BadParams(format!("missing number '{key}'")))
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

fn parse_quant(value: Option<&Json>) -> Result<Quantization, ToolError> {
    match value.and_then(Json::as_str) {
        None | Some("none") => Ok(Quantization::None),
        Some("binary") => Ok(Quantization::Binary),
        Some("scalar") => Ok(Quantization::Scalar),
        _ => Err(ToolError::BadParams(
            "'quant' must be one of: none, binary, scalar".into(),
        )),
    }
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
    fn graph_link_neighbors_unlink() {
        let s = server();
        s.handle(
            "link",
            &json!({"collection": "g", "from": "a", "relation": "knows", "to": "b"}),
        )
        .unwrap();
        s.handle(
            "link",
            &json!({"collection": "g", "from": "a", "relation": "knows", "to": "c"}),
        )
        .unwrap();

        let n = s
            .handle(
                "neighbors",
                &json!({"collection": "g", "from": "a", "relation": "knows"}),
            )
            .unwrap();
        assert_eq!(n["neighbors"], json!(["b", "c"]));

        let r = s
            .handle(
                "unlink",
                &json!({"collection": "g", "from": "a", "relation": "knows", "to": "b"}),
            )
            .unwrap();
        assert_eq!(r["removed"], true);
    }

    #[test]
    fn graph_traverse_multi_hop() {
        let s = server();
        for (from, to) in [("a", "b"), ("b", "c"), ("c", "d")] {
            s.handle(
                "link",
                &json!({"collection": "g", "from": from, "relation": "r", "to": to}),
            )
            .unwrap();
        }
        let out = s
            .handle(
                "traverse",
                &json!({"collection": "g", "start": "a", "relation": "r", "hops": 2}),
            )
            .unwrap();
        assert_eq!(out["nodes"], json!(["b", "c"]));
    }

    #[test]
    fn create_index_then_search_uses_it() {
        let s = server();
        store(&s, "a", json!({"embedding": {"$vector": [1.0, 0.0]}}));
        store(&s, "b", json!({"embedding": {"$vector": [0.0, 1.0]}}));
        let created = s
            .handle(
                "create_index",
                &json!({"collection": "docs", "field": "embedding", "metric": "cosine"}),
            )
            .unwrap();
        assert_eq!(created, json!({"ok": true}));

        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "vector": {"field": "embedding", "query": [1.0, 0.0], "k": 1, "metric": "cosine"}
                }),
            )
            .unwrap();
        assert_eq!(out["results"][0]["key"], "a");
    }

    #[test]
    fn geo_tool_finds_nearby() {
        let s = server();
        store(&s, "london", json!({"loc": [51.5074, -0.1278]}));
        store(&s, "paris", json!({"loc": [48.8566, 2.3522]}));
        let out = s
            .handle(
                "geo",
                &json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13, "radius_km": 50.0}),
            )
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "london");
        assert!(results[0]["distance_km"].as_f64().unwrap() < 50.0);
    }

    #[test]
    fn search_with_geo_within_filter() {
        let s = server();
        store(
            &s,
            "near",
            json!({"loc": [51.5, -0.13], "body": "coffee shop"}),
        );
        store(
            &s,
            "far",
            json!({"loc": [48.86, 2.35], "body": "coffee shop"}),
        );
        // "coffee near London" — text + geo filter in one query.
        let out = s
            .handle(
                "search",
                &json!({
                    "collection": "docs",
                    "filter": {"op": "geo_within", "field": "loc", "lat": 51.5, "lon": -0.13, "radius_km": 50.0},
                    "text": {"field": "body", "query": "coffee", "k": 10}
                }),
            )
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "near");
    }

    #[test]
    fn join_tool_resolves_references() {
        let s = server();
        s.handle(
            "store",
            &json!({"collection": "authors", "key": "rocky", "document": {"name": "Rocky"}}),
        )
        .unwrap();
        s.handle(
            "store",
            &json!({"collection": "posts", "key": "p1", "document": {"title": "Hi", "author_id": "rocky"}}),
        )
        .unwrap();
        let out = s
            .handle(
                "join",
                &json!({"collection": "posts", "other": "authors", "foreign_key_field": "author_id"}),
            )
            .unwrap();
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["right"], json!({"name": "Rocky"}));
    }

    #[test]
    fn list_collections_count_and_insert_auto() {
        let s = server();
        let a = s
            .handle(
                "insert_auto",
                &json!({"collection": "events", "document": {"n": 1}}),
            )
            .unwrap();
        let key = a["key"].as_str().unwrap().to_owned();
        // The auto key round-trips through get.
        let got = s
            .handle("get", &json!({"collection": "events", "key": key}))
            .unwrap();
        assert_eq!(got, json!({"document": {"n": 1}}));

        store(&s, "k", json!({"x": 1})); // collection "docs"
        let cols = s.handle("list_collections", &json!({})).unwrap();
        let names: Vec<&str> = cols["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(names.contains(&"events"));
        assert!(names.contains(&"docs"));

        let c = s.handle("count", &json!({"collection": "events"})).unwrap();
        assert_eq!(c["count"], 1);
    }

    #[test]
    fn in_neighbors_tool() {
        let s = server();
        s.handle(
            "link",
            &json!({"collection": "g", "from": "a", "relation": "knows", "to": "x"}),
        )
        .unwrap();
        let out = s
            .handle(
                "in_neighbors",
                &json!({"collection": "g", "to": "x", "relation": "knows"}),
            )
            .unwrap();
        assert_eq!(out["neighbors"], json!(["a"]));
    }

    #[test]
    fn create_text_index_then_search() {
        let s = server();
        store(&s, "a", json!({"body": "rust embedded database"}));
        store(&s, "b", json!({"body": "python web"}));
        s.handle(
            "create_text_index",
            &json!({"collection": "docs", "field": "body"}),
        )
        .unwrap();
        let out = s
            .handle(
                "search",
                &json!({"collection": "docs", "text": {"field": "body", "query": "rust", "k": 5}}),
            )
            .unwrap();
        assert_eq!(out["results"][0]["key"], "a");
    }

    #[test]
    fn list_tools_respect_limit() {
        let s = server();
        for to in ["x", "y", "z"] {
            s.handle(
                "link",
                &json!({"collection": "g", "from": "a", "relation": "r", "to": to}),
            )
            .unwrap();
        }
        let out = s
            .handle(
                "neighbors",
                &json!({"collection": "g", "from": "a", "relation": "r", "limit": 2}),
            )
            .unwrap();
        assert_eq!(out["neighbors"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn graph_tools_validate_params() {
        let s = server();
        assert!(matches!(
            s.handle("link", &json!({"collection": "g", "from": "a"}))
                .unwrap_err(),
            ToolError::BadParams(_)
        ));
        assert!(matches!(
            s.handle(
                "traverse",
                &json!({"collection": "g", "start": "a", "relation": "r"})
            )
            .unwrap_err(),
            ToolError::BadParams(_)
        ));
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
