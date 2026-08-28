//! MCP wire conformance (skeleton). Task 14 fills this file with the full
//! matrix: every tool x envelope x error, in-process over duplex in-memory
//! I/O. This smoke test anchors the radar's test-existence check with real,
//! asserted behavior.
//!
//! Child-process spawning is banned in the conformance suite (it evades
//! coverage and flakes); the public in-process route is
//! [`corvid_mcp::protocol::run`], which drives the full read/dispatch/write
//! loop over any `BufRead`/`Write` — here a `Cursor` input and a `Vec`
//! output, i.e. duplex in-memory I/O with the server in this process.

use std::io::Cursor;

use corvid_mcp::protocol;
use serde_json::{Value as Json, json};

/// Run one line-delimited JSON-RPC session against a fresh in-memory server:
/// every request is dispatched and its response collected, exactly as the
/// stdio transport would see it.
fn session(requests: &[Json]) -> Vec<Json> {
    let server = corvid_mcp::Server::open_in_memory().unwrap();
    let mut input = String::new();
    for r in requests {
        input.push_str(&format!("{r}\n"));
    }
    let mut out = Vec::new();
    protocol::run(&server, Cursor::new(input.into_bytes()), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn tools_smoke_in_process_wire_roundtrip() {
    let responses = session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "store", "arguments": {"collection": "docs", "key": "k", "document": {"n": 1}}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
               "params": {"name": "get", "arguments": {"collection": "docs", "key": "k"}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "frobnicate"}),
    ]);
    // One response per request, in order.
    assert_eq!(responses.len(), 6);

    // initialize envelope: server info and capabilities, asserted by value.
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(
        responses[0]["result"]["protocolVersion"],
        protocol::PROTOCOL_VERSION
    );
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "corvid-mcp");
    assert_eq!(responses[0]["result"]["capabilities"]["tools"], json!({}));

    // ping envelope: an empty result object.
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["result"], json!({}));

    // tools/list envelope: a non-empty tool table with named entries.
    let tools = responses[2]["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    assert!(tools.iter().all(|t| t["name"].is_string()));

    // tools/call envelope, happy path: store reports a non-error result.
    assert_eq!(responses[3]["result"]["isError"], false);

    // tools/call envelope, read back: the stored document round-trips
    // through the wire (tool results embed JSON text).
    assert_eq!(responses[4]["result"]["isError"], false);
    let text = responses[4]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let payload: Json = serde_json::from_str(text).unwrap();
    assert_eq!(payload, json!({"document": {"n": 1}}));

    // error envelope: an unknown method answers -32601, echoing the id.
    assert_eq!(responses[5]["id"], 6);
    assert_eq!(responses[5]["error"]["code"], -32601);
    assert!(
        responses[5]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("frobnicate")
    );
}
