//! Result caps, bounded limits, pagination cursors, large payloads, and
//! frame-size boundaries. The pinned constants (from server.rs): search
//! defaults to 100 rows and rejects `limit`/`k` over 10000; page defaults to
//! 1000 and rejects over 10000; the LIST tools (neighbors, in_neighbors,
//! traverse, geo, join) clamp an oversized `limit` to 10000 instead of
//! erroring; the transport refuses frames over MAX_FRAME_SIZE (32 MiB).

use std::io::Cursor;

use corvid_mcp::Server;
use serde_json::{Value as Json, json};

use crate::wire::{self, Wire};

/// Store `count` documents into `collection` over ONE connection (a single
/// multi-request session), asserting every store answered ok.
fn bulk_store(server: &Server, collection: &str, count: usize) {
    let input = (0..count)
        .map(|i| {
            format!(
                "{}\n",
                wire::call_req(
                    i as i64 + 1,
                    "store",
                    json!({"collection": collection, "key": format!("k{i:05}"), "document": {"n": i}}),
                )
            )
        })
        .collect::<String>();
    let out = wire::exchange(server, input);
    assert_eq!(out.len(), count, "one response per store");
    assert!(
        out.iter().all(|r| r["result"]["isError"] == false),
        "every store succeeds"
    );
}

/// search caps results at 100 by default; an explicit limit is honored;
/// a limit over the hard maximum is refused.
#[test]
fn search_default_cap_and_over_max_limit() {
    let mut w = Wire::new();
    bulk_store(&w.server, "big", 150);
    let out = w.ok("search", json!({"collection": "big"}));
    assert_eq!(out["results"].as_array().unwrap().len(), 100, "default cap");
    let out = w.ok("search", json!({"collection": "big", "limit": 10}));
    assert_eq!(out["results"].as_array().unwrap().len(), 10);
    assert_eq!(
        w.err("search", json!({"collection": "big", "limit": 10001})),
        "bad params: 'limit' exceeds the maximum of 10000"
    );
}

/// page: default limit 1000 with a `next` cursor that walks the remainder;
/// limit 0 answers empty; an over-max limit is refused; a page exactly as
/// full as the limit still reports a cursor (pinned: a full page may have
/// more behind it — the next page then comes back empty).
#[test]
fn page_cursor_walk_default_and_boundaries() {
    let mut w = Wire::new();
    bulk_store(&w.server, "big", 1002);
    let p1 = w.ok("page", json!({"collection": "big"}));
    let rows = p1["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1000, "default page limit");
    assert_eq!(rows[0]["key"], "k00000");
    assert_eq!(rows[999]["key"], "k00999");
    let cursor = p1["next"].as_str().unwrap().to_owned();
    assert_eq!(cursor, "k00999", "next resumes after the last row");
    let p2 = w.ok("page", json!({"collection": "big", "after": cursor}));
    let rows = p2["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "the remainder");
    assert_eq!(rows[0]["key"], "k01000");
    assert_eq!(rows[1]["key"], "k01001");
    assert_eq!(p2["next"], Json::Null, "no more pages");

    // limit 0: an empty page with no cursor, not an error.
    assert_eq!(
        w.ok("page", json!({"collection": "big", "limit": 0})),
        json!({"rows": [], "next": null})
    );
    assert_eq!(
        w.err("page", json!({"collection": "big", "limit": 10001})),
        "bad params: 'limit' exceeds the maximum of 10000"
    );

    // Exactly-full page still yields a cursor; following it ends the walk.
    let p = w.ok("page", json!({"collection": "big", "limit": 1002}));
    assert_eq!(p["rows"].as_array().unwrap().len(), 1002);
    assert_eq!(p["next"], "k01001");
    let next = w.ok(
        "page",
        json!({"collection": "big", "limit": 1002, "after": "k01001"}),
    );
    assert_eq!(next, json!({"rows": [], "next": null}));

    // page_where through the wire: the filter is honored across cursor walks.
    let p = w.ok(
        "page",
        json!({"collection": "big", "limit": 1,
               "filter": {"op": "gt", "field": "n", "value": 999}}),
    );
    let rows = p["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "k01000");
    assert_eq!(rows[0]["document"], json!({"n": 1000}));
    let p2 = w.ok(
        "page",
        json!({"collection": "big", "limit": 10, "after": "k01000",
               "filter": {"op": "gt", "field": "n", "value": 999}}),
    );
    assert_eq!(p2["rows"].as_array().unwrap().len(), 1, "one match left");
    assert_eq!(p2["next"], Json::Null);
    wire::starts_with(
        &w.err("page", json!({})),
        "bad params: missing string 'collection'",
    );
}

/// The LIST tools CLAMP an oversized `limit` to the hard maximum instead of
/// erroring (unlike search/page, whose limits are validated): neighbors,
/// in_neighbors, traverse, geo and join all succeed with limit 999999. An
/// INVALID limit (negative or non-integer) is rejected everywhere alike.
#[test]
fn list_tools_clamp_oversized_limit_and_reject_invalid() {
    let mut w = Wire::new();
    for to in ["x", "y", "z"] {
        w.ok(
            "link",
            json!({"collection": "g", "from": "a", "relation": "r", "to": to}),
        );
    }
    let out = w.ok(
        "neighbors",
        json!({"collection": "g", "from": "a", "relation": "r", "limit": 999_999}),
    );
    assert_eq!(out["neighbors"], json!(["x", "y", "z"]));
    let out = w.ok(
        "in_neighbors",
        json!({"collection": "g", "to": "x", "relation": "r", "limit": 999_999}),
    );
    assert_eq!(out["neighbors"], json!(["a"]));
    let out = w.ok(
        "traverse",
        json!({"collection": "g", "start": "a", "relation": "r", "hops": 2, "limit": 999_999}),
    );
    assert_eq!(out["nodes"], json!(["x", "y", "z"]));
    w.store("docs", "p", json!({"loc": [51.5, -0.13]}));
    let out = w.ok(
        "geo",
        json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
               "radius_km": 10.0, "limit": 999_999}),
    );
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
    // join clamps identically, and a small limit truncates its rows.
    w.store("authors", "a", json!({"name": "A"}));
    for i in 0..5 {
        w.store("posts", &format!("p{i}"), json!({"author_id": "a"}));
    }
    let out = w.ok(
        "join",
        json!({"collection": "posts", "other": "authors",
               "foreign_key_field": "author_id", "limit": 999_999}),
    );
    assert_eq!(out["rows"].as_array().unwrap().len(), 5);
    let out = w.ok(
        "join",
        json!({"collection": "posts", "other": "authors",
               "foreign_key_field": "author_id", "limit": 2}),
    );
    assert_eq!(out["rows"].as_array().unwrap().len(), 2);

    // Invalid limits error on every list tool — no silent defaulting.
    for bad in [json!(-5), json!("two"), json!(1.5)] {
        assert_eq!(
            w.err(
                "neighbors",
                json!({"collection": "g", "from": "a", "relation": "r", "limit": bad}),
            ),
            "bad params: 'limit' must be a non-negative integer",
            "neighbors limit {bad}"
        );
    }
    assert_eq!(
        w.err(
            "in_neighbors",
            json!({"collection": "g", "to": "x", "relation": "r", "limit": -1}),
        ),
        "bad params: 'limit' must be a non-negative integer"
    );
    assert_eq!(
        w.err(
            "traverse",
            json!({"collection": "g", "start": "a", "relation": "r", "hops": 1, "limit": "x"}),
        ),
        "bad params: 'limit' must be a non-negative integer"
    );
    assert_eq!(
        w.err(
            "geo",
            json!({"collection": "docs", "field": "loc", "lat": 51.5, "lon": -0.13,
                   "radius_km": 10.0, "limit": -1}),
        ),
        "bad params: 'limit' must be a non-negative integer"
    );
    assert_eq!(
        w.err(
            "join",
            json!({"collection": "posts", "other": "authors",
                   "foreign_key_field": "author_id", "limit": 1.5}),
        ),
        "bad params: 'limit' must be a non-negative integer"
    );
}

/// A ~100 KB document round-trips through the wire byte-for-byte.
#[test]
fn large_document_100kb_roundtrips() {
    let mut w = Wire::new();
    let text = "x".repeat(100_000);
    w.store("docs", "k", json!({"text": text, "n": 1}));
    let got = w.get("docs", "k");
    assert_eq!(got["n"], 1);
    assert_eq!(
        got["text"].as_str().unwrap().len(),
        100_000,
        "the full payload survives"
    );
    assert_eq!(got["text"].as_str().unwrap().chars().next(), Some('x'));
}

/// Frame-size boundary (small explicit limit via run_with_limit): a frame
/// EXACTLY at the limit is served; one byte over answers -32700 "frame
/// exceeds maximum size" and the loop continues with the next request.
#[test]
fn frame_size_boundary_exact_and_one_over() {
    let server = Server::open_in_memory().unwrap();
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string();
    let limit = 128usize;
    let padding = limit - ping.len();
    let content = format!("{ping}{}", " ".repeat(padding));
    assert_eq!(content.len(), limit, "frame content is exactly the limit");
    let exact = format!("{content}\n");
    let mut out = Vec::new();
    corvid_mcp::protocol::run_with_limit(&server, Cursor::new(exact.into_bytes()), &mut out, limit)
        .unwrap();
    let responses: Vec<Json> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], 1, "an exact-size frame is served");

    let over = format!("{}{}\n", ping, " ".repeat(padding + 1));
    let input = format!("{over}{ping}\n");
    let mut out = Vec::new();
    corvid_mcp::protocol::run_with_limit(&server, Cursor::new(input.into_bytes()), &mut out, limit)
        .unwrap();
    let responses: Vec<Json> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(
        responses[0]["error"]["message"],
        "frame exceeds maximum size"
    );
    assert_eq!(responses[1]["id"], 1, "the connection survives");
}

/// The default MAX_FRAME_SIZE (32 MiB) is real: a frame one byte larger is
/// refused and skipped even through the default `run` entry point.
#[test]
fn frame_over_default_max_frame_size_is_refused() {
    let server = Server::open_in_memory().unwrap();
    let over = "a".repeat(corvid_mcp::protocol::MAX_FRAME_SIZE + 1);
    let mut input = over.into_bytes();
    input.push(b'\n');
    input.extend_from_slice(
        format!("{}\n", json!({"jsonrpc": "2.0", "id": 5, "method": "ping"})).as_bytes(),
    );
    let mut out = Vec::new();
    corvid_mcp::protocol::run(&server, Cursor::new(input), &mut out).unwrap();
    let responses: Vec<Json> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(
        responses[0]["error"]["message"],
        "frame exceeds maximum size"
    );
    assert_eq!(responses[1]["id"], 5);
}

/// open_server builds both server flavors: in-memory (None) and file-backed
/// (Some(path)); the file-backed one serves a real call over the wire.
#[test]
fn open_server_memory_and_file_backed() {
    let memory = corvid_mcp::protocol::open_server(None).unwrap();
    let mut w = Wire::over(memory);
    w.store("docs", "k", json!({"n": 1}));
    assert_eq!(w.get("docs", "k"), json!({"n": 1}));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.db");
    let file = corvid_mcp::protocol::open_server(Some(path.to_str().unwrap())).unwrap();
    let mut w = Wire::over(file);
    w.store("docs", "k", json!({"n": 2}));
    assert_eq!(w.get("docs", "k"), json!({"n": 2}));
}

/// Server::new wraps a caller-supplied engine Db directly and serves it
/// over the wire (the fourth construction route alongside
/// open/open_in_memory/open_server).
#[test]
fn server_new_wraps_an_engine_db() {
    let db = corvid::Db::open_in_memory().unwrap();
    db.collection("docs")
        .insert(b"k", &corvid::Value::Text("engine-written".into()))
        .unwrap();
    let mut w = Wire::over(Server::new(db));
    // A document written through the ENGINE reads back through the WIRE.
    assert_eq!(w.get("docs", "k"), json!("engine-written"));
}
