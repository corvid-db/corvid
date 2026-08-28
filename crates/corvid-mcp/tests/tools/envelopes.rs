//! JSON-RPC envelope conformance: initialize / ping / tools/list / tools/call
//! shapes, the error envelope, notifications, malformed frames, and multi-line
//! sessions — all over the real in-process duplex loop.

use corvid_mcp::Server;
use serde_json::{Value as Json, json};

use crate::wire::{self, Wire};

/// One raw request against a fresh server; panics unless exactly one
/// well-formed response (ok or error) comes back, and returns it.
fn raw(request: Json) -> Json {
    let responses = wire::exchange(&Server::open_in_memory().unwrap(), format!("{request}\n"));
    assert_eq!(responses.len(), 1, "one request, one response");
    responses[0].clone()
}

/// initialize: the full result shape, by value — protocol revision, server
/// name and version (the crate's own version, as compiled in), tools
/// capability — with the id echoed verbatim, including a string id.
#[test]
fn envelope_initialize_result_shape() {
    let r = raw(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 1);
    assert_eq!(
        r["result"],
        json!({
            "protocolVersion": corvid_mcp::protocol::PROTOCOL_VERSION,
            "serverInfo": {
                "name": "corvid-mcp",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": { "tools": {} },
        })
    );
    // A string id is echoed back unchanged (JSON-RPC ids are any type).
    let r = raw(json!({"jsonrpc": "2.0", "id": "sess-42", "method": "initialize"}));
    assert_eq!(r["id"], "sess-42");
}

/// ping: an empty result object.
#[test]
fn envelope_ping_empty_result() {
    let r = raw(json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}));
    assert_eq!(r["id"], 7);
    assert_eq!(r["result"], json!({}));
    assert!(r.get("error").is_none());
}

/// tools/list: exactly the 27 advertised tools, each with an object
/// inputSchema and a non-empty description. The exact-count assertion pins
/// the manifest's tool inventory: adding a tool without a manifest row fails
/// here and in the radar.
#[test]
fn envelope_tools_list_all_27_with_schemas() {
    let r = raw(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = r["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 27, "the server advertises exactly 27 tools");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "store",
        "patch",
        "compare_and_set",
        "get",
        "delete",
        "delete_where",
        "search",
        "create_index",
        "link",
        "unlink",
        "neighbors",
        "traverse",
        "geo",
        "join",
        "in_neighbors",
        "page",
        "phrase_search",
        "create_text_index",
        "create_scalar_index",
        "create_geo_index",
        "create_compound_index",
        "backup",
        "dump",
        "load",
        "list_collections",
        "count",
        "insert_auto",
    ] {
        assert!(
            names.contains(&expected),
            "tools/list must advertise {expected}"
        );
    }
    for t in tools {
        let schema = t["inputSchema"].as_object().unwrap();
        assert_eq!(schema["type"], "object", "inputSchema is an object: {t}");
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "every tool carries a description: {t}"
        );
    }
}

/// tools/call success: the MCP result shape — a content array with one text
/// block carrying the tool's JSON, and `isError: false`.
#[test]
fn envelope_tools_call_content_shape() {
    let mut w = Wire::new();
    let r = w.call("count", json!({"collection": "docs"}));
    assert_eq!(r["result"]["isError"], false);
    let content = r["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "{\"count\":0}");
    // `ok` re-asserts this shape and parses the payload.
    assert_eq!(
        w.ok("count", json!({"collection": "docs"})),
        json!({"count": 0})
    );
}

/// The three error-taxonomy surfaces are distinct: an unknown TOOL is a
/// JSON-RPC `-32601` error object; bad PARAMS and ENGINE failures are both
/// `isError` tool results, distinguished by their message prefixes
/// ("bad params:" vs the engine's own wording). The ToolError variants are
/// also pinned directly at the `Server::handle` boundary.
#[test]
fn envelope_error_taxonomy_three_surfaces() {
    let mut w = Wire::new();
    // UnknownTool -> JSON-RPC error, not an isError result.
    let responses = wire::exchange(
        &w.server,
        format!("{}\n", wire::call_req(9, "frobnicate", json!({}))),
    );
    assert_eq!(responses[0]["error"]["code"], -32601);
    assert_eq!(responses[0]["error"]["message"], "unknown tool: frobnicate");
    assert!(responses[0].get("result").is_none());

    // BadParams -> isError result with the "bad params:" prefix.
    wire::starts_with(
        &w.err("get", json!({"collection": "docs"})),
        "bad params: missing string 'key'",
    );

    // Engine -> isError result with the engine's own message (reserved name).
    wire::starts_with(
        &w.err(
            "store",
            json!({"collection": "__sys", "key": "k", "document": {}}),
        ),
        "reserved collection name: __sys",
    );

    // The same three variants at the handle() boundary, exactly.
    let e = w.server.handle("frobnicate", &json!({})).unwrap_err();
    assert!(matches!(e, corvid_mcp::ToolError::UnknownTool(t) if t == "frobnicate"));
    let e = w
        .server
        .handle("get", &json!({"collection": "docs"}))
        .unwrap_err();
    assert!(matches!(e, corvid_mcp::ToolError::BadParams(m) if m.contains("key")));
    let e = w
        .server
        .handle(
            "store",
            &json!({"collection": "__sys", "key": "k", "document": {}}),
        )
        .unwrap_err();
    assert!(matches!(e, corvid_mcp::ToolError::Engine(_)));
}

/// Unknown method: `-32601` "method not found: X" with the id echoed. A
/// request with NO method at all: `-32600` invalid request.
#[test]
fn envelope_unknown_and_missing_method_codes() {
    let r = raw(json!({"jsonrpc": "2.0", "id": 11, "method": "dance"}));
    assert_eq!(r["id"], 11);
    assert_eq!(r["error"]["code"], -32601);
    assert_eq!(r["error"]["message"], "method not found: dance");

    let r = raw(json!({"jsonrpc": "2.0", "id": 12}));
    assert_eq!(r["error"]["code"], -32600);
    assert_eq!(r["error"]["message"], "invalid request: missing method");
}

/// Malformed tools/call requests are protocol-level errors (not isError):
/// missing `params`, a non-string `name`, and `params` that is not an object
/// all answer `-32602` with a distinct message.
#[test]
fn envelope_tools_call_malformed_request_is_invalid_params() {
    let r = raw(json!({"jsonrpc": "2.0", "id": 21, "method": "tools/call"}));
    assert_eq!(r["error"]["code"], -32602);
    assert_eq!(r["error"]["message"], "missing params");

    let r = raw(
        json!({"jsonrpc": "2.0", "id": 22, "method": "tools/call", "params":
        {"name": 42, "arguments": {}}}),
    );
    assert_eq!(r["error"]["code"], -32602);
    assert_eq!(r["error"]["message"], "missing tool name");

    let r = raw(json!({"jsonrpc": "2.0", "id": 23, "method": "tools/call", "params": [1, 2]}));
    assert_eq!(r["error"]["code"], -32602);
    assert_eq!(r["error"]["message"], "missing tool name");
}

/// Notifications carry no id and produce NO response line: both the
/// `notifications/initialized` method and any other id-less request are
/// dropped silently by the loop.
#[test]
fn envelope_notifications_produce_no_response() {
    let out = wire::exchange(
        &Server::open_in_memory().unwrap(),
        format!(
            "{}\n{}\n{}\n",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "ping"}),
            json!({"jsonrpc": "2.0", "method": "tools/call",
                   "params": {"name": "count", "arguments": {"collection": "c"}}}),
        ),
    );
    assert!(
        out.is_empty(),
        "id-less requests must not be answered: {out:?}"
    );
}

/// A malformed JSON line answers `-32700` and does NOT kill the loop: the
/// next (valid) request on the same connection is still served.
#[test]
fn envelope_malformed_line_is_parse_error_and_loop_survives() {
    let server = Server::open_in_memory().unwrap();
    let out = wire::exchange(
        &server,
        "not json at all\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n".to_owned(),
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["error"]["code"], -32700);
    assert_eq!(out[0]["error"]["message"], "parse error");
    assert_eq!(out[0]["id"], Json::Null);
    assert_eq!(out[1]["id"], 1, "the loop survives a bad frame");
}

/// One connection, many requests in order: initialize, stores, reads, and
/// error responses interleave correctly, and every response echoes its id.
#[test]
fn envelope_session_multiple_requests_in_order() {
    let server = Server::open_in_memory().unwrap();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        wire::call_req(
            2,
            "store",
            json!({"collection": "c", "key": "a", "document": {"n": 1}}),
        ),
        wire::call_req(
            3,
            "store",
            json!({"collection": "c", "key": "b", "document": {"n": 2}}),
        ),
        wire::call_req(4, "get", json!({"collection": "c", "key": "b"})),
        wire::call_req(5, "count", json!({"collection": "c"})),
        json!({"jsonrpc": "2.0", "id": 6, "method": "no/such/method"}),
    ]
    .iter()
    .map(|r| format!("{r}\n"))
    .collect::<String>();
    let out = wire::exchange(&server, input);
    assert_eq!(out.len(), 6);
    for (i, resp) in out.iter().enumerate() {
        assert_eq!(resp["id"], i as i64 + 1, "responses arrive in order");
    }
    let text = out[3]["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Json = serde_json::from_str(text).unwrap();
    assert_eq!(payload, json!({"document": {"n": 2}}));
    let text = out[4]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "{\"count\":2}");
    assert_eq!(out[5]["error"]["code"], -32601);
}

/// Blank lines (and CRLF line endings) never produce responses: the loop
/// skips whitespace-only frames and tolerates `\r` before the newline.
#[test]
fn envelope_blank_and_crlf_frames_are_ignored() {
    let server = Server::open_in_memory().unwrap();
    let ping = json!({"jsonrpc": "2.0", "id": 31, "method": "ping"}).to_string();
    let input = format!("\n   \n{ping}\r\n\n{ping}\n");
    let out = wire::exchange(&server, input);
    assert_eq!(out.len(), 2, "only the two pings are answered: {out:?}");
    assert_eq!(out[0]["id"], 31);
    assert_eq!(out[1]["id"], 31);
    assert_eq!(out[0]["result"], json!({}));
}
