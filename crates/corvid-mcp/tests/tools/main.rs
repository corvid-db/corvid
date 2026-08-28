//! MCP wire conformance (Task 14): the full matrix — every JSON-RPC envelope,
//! every tool happy + error, the `$vector`/`$bytes` convert conventions, result
//! caps and bounded limits, large payloads and frame boundaries, and the three
//! error-taxonomy surfaces — driven IN-PROCESS over duplex in-memory I/O.
//!
//! Child-process spawning is banned in the conformance suite (it evades
//! coverage and flakes); the public in-process route is
//! [`corvid_mcp::protocol::run`], which drives the full read/dispatch/write
//! loop over any `BufRead`/`Write` — here a `Cursor` input and a `Vec`
//! output, i.e. duplex in-memory I/O with the server in this process. Every
//! assertion below is against real stored state read back through the wire.
//!
//! Layout: envelope conformance in [`envelopes`], document tools in [`docs`],
//! search/geo/index tools in [`search`] and [`schema_tools`], graph tools in
//! [`graph`], backup/dump/load in [`admin`], the convert conventions in
//! [`convert_conventions`], and the caps/limit/frame matrix in [`caps`]. The
//! Task 2 smoke test stays as the manifest anchor at the bottom of this file.

mod admin;
mod caps;
mod convert_conventions;
mod docs;
mod envelopes;
mod graph;
mod schema_tools;
mod search;

use serde_json::{Value as Json, json};

/// Shared wire harness: everything a module needs to speak real JSON-RPC to a
/// real [`corvid_mcp::Server`] through the real transport loop.
pub(crate) mod wire {
    use std::io::Cursor;

    use corvid_mcp::Server;
    use serde_json::{Value as Json, json};

    /// Run the real stdio loop over duplex in-memory I/O: `input` is the
    /// client's exact bytes, and the returned vec is the server's output
    /// lines parsed as JSON (one entry per response line, in order).
    pub(crate) fn exchange(server: &Server, input: String) -> Vec<Json> {
        let mut out = Vec::new();
        corvid_mcp::protocol::run(server, Cursor::new(input.into_bytes()), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// A `tools/call` request value for `tool` with `arguments`.
    pub(crate) fn call_req(id: i64, tool: &str, args: Json) -> Json {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        })
    }

    /// A session against one in-memory server: issues `tools/call`s over the
    /// wire with sequential ids, asserting the response envelope of each.
    pub(crate) struct Wire {
        pub server: Server,
        id: i64,
    }

    impl Wire {
        /// A fresh in-memory server.
        pub(crate) fn new() -> Wire {
            Wire {
                server: Server::open_in_memory().unwrap(),
                id: 0,
            }
        }

        /// A session over an existing server (e.g. one reopened from a
        /// backup file).
        pub(crate) fn over(server: Server) -> Wire {
            Wire { server, id: 0 }
        }

        /// One `tools/call` over the wire; asserts exactly one well-formed
        /// successful JSON-RPC response came back, and returns it whole.
        pub(crate) fn call(&mut self, tool: &str, args: Json) -> Json {
            self.id += 1;
            let responses = exchange(&self.server, format!("{}\n", call_req(self.id, tool, args)));
            assert_eq!(
                responses.len(),
                1,
                "one request must yield exactly one response"
            );
            let r = &responses[0];
            assert_eq!(r["jsonrpc"], "2.0", "responses are JSON-RPC 2.0");
            assert_eq!(r["id"], self.id, "the response must echo the request id");
            assert!(r.get("error").is_none(), "unexpected JSON-RPC error: {r}");
            r.clone()
        }

        /// The parsed tool payload of a successful call: asserts the MCP
        /// result shape (content array of one text block, `isError: false`)
        /// and returns the embedded JSON.
        pub(crate) fn ok(&mut self, tool: &str, args: Json) -> Json {
            let r = self.call(tool, args);
            assert_eq!(r["result"]["isError"], false, "call must not error: {r}");
            let content = r["result"]["content"].as_array().unwrap();
            assert_eq!(content.len(), 1, "one content block: {r}");
            assert_eq!(content[0]["type"], "text");
            let text = content[0]["text"].as_str().unwrap();
            serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text is JSON: {e}: {text}"))
        }

        /// The error text of a failed tool call: asserts `isError: true` and
        /// the content shape, returns the message the client would see.
        pub(crate) fn err(&mut self, tool: &str, args: Json) -> String {
            let r = self.call(tool, args);
            assert_eq!(r["result"]["isError"], true, "call must error: {r}");
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        /// Store a document through the wire, asserting the exact result.
        pub(crate) fn store(&mut self, collection: &str, key: &str, document: Json) {
            let out = self.ok(
                "store",
                json!({"collection": collection, "key": key, "document": document}),
            );
            assert_eq!(out, json!({"ok": true}), "store must report ok");
        }

        /// Get a document through the wire; returns the `document` value
        /// (`null` when absent).
        pub(crate) fn get(&mut self, collection: &str, key: &str) -> Json {
            self.ok("get", json!({"collection": collection, "key": key}))["document"].clone()
        }
    }

    /// Assert `got` starts with `prefix` (error-message surface checks).
    pub(crate) fn starts_with(got: &str, prefix: &str) {
        assert!(
            got.starts_with(prefix),
            "message {got:?} must start with {prefix:?}"
        );
    }
}

#[test]
fn tools_smoke_in_process_wire_roundtrip() {
    let responses = wire::exchange(
        &corvid_mcp::Server::open_in_memory().unwrap(),
        [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                   "params": {"name": "store", "arguments": {"collection": "docs", "key": "k", "document": {"n": 1}}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                   "params": {"name": "get", "arguments": {"collection": "docs", "key": "k"}}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "frobnicate"}),
        ]
        .iter()
        .map(|r| format!("{r}\n"))
        .collect::<String>(),
    );
    // One response per request, in order.
    assert_eq!(responses.len(), 6);

    // initialize envelope: server info and capabilities, asserted by value.
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(
        responses[0]["result"]["protocolVersion"],
        corvid_mcp::protocol::PROTOCOL_VERSION
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
