//! Minimal MCP transport: JSON-RPC 2.0 over a line-delimited stream.
//!
//! [`handle_request`] maps one JSON-RPC request to its response (or `None` for
//! notifications) and is where all protocol behavior lives — it is fully
//! testable without a real client. [`run`] is the thin read/dispatch/write
//! loop over any [`BufRead`]/[`Write`], so even the loop is exercised with
//! in-memory buffers. The `main` binary just wires it to stdin/stdout.
//!
//! Supported methods: `initialize`, `tools/list`, `tools/call`, `ping`, and
//! the `notifications/initialized` notification. Tool *failures* are returned
//! as a successful `tools/call` result with `isError: true` (per MCP), while
//! protocol-level problems use JSON-RPC error objects.

use std::io::{BufRead, Write};

use serde_json::{Value as Json, json};

use crate::error::ToolError;
use crate::server::Server;

/// The MCP protocol revision this server reports.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Build a server from an optional database path (in-memory when `None`).
pub fn open_server(path: Option<&str>) -> Result<Server, corvid::Error> {
    match path {
        Some(p) => Server::open(p),
        None => Server::open_in_memory(),
    }
}

/// Run the stdio JSON-RPC loop: read newline-delimited requests, dispatch, and
/// write newline-delimited responses. Blank lines are ignored; unparseable
/// lines get a JSON-RPC parse-error response.
pub fn run<R: BufRead, W: Write>(server: &Server, reader: R, mut writer: W) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Json>(&line) {
            Ok(req) => handle_request(server, &req),
            Err(_) => Some(error_response(Json::Null, -32700, "parse error")),
        };
        if let Some(resp) = response {
            writeln!(writer, "{resp}")?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Handle one parsed JSON-RPC request, returning its response, or `None` for
/// notifications (requests without an `id`).
pub fn handle_request(server: &Server, req: &Json) -> Option<Json> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Json::as_str);
    let params = req.get("params");

    // Notifications carry no id and never get a response.
    let is_notification = id.is_none();

    match method {
        Some("notifications/initialized") => None,
        _ if is_notification => None,
        Some("initialize") => Some(ok_response(id, initialize_result())),
        Some("ping") => Some(ok_response(id, json!({}))),
        Some("tools/list") => Some(ok_response(id, tools_list())),
        Some("tools/call") => match tools_call(server, params) {
            Ok(result) => Some(ok_response(id, result)),
            Err((code, msg)) => Some(error_response(id.unwrap_or(Json::Null), code, &msg)),
        },
        Some(other) => Some(error_response(
            id.unwrap_or(Json::Null),
            -32601,
            &format!("method not found: {other}"),
        )),
        None => Some(error_response(
            id.unwrap_or(Json::Null),
            -32600,
            "invalid request: missing method",
        )),
    }
}

fn initialize_result() -> Json {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": { "name": "corvid-mcp", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "tools": {} }
    })
}

fn tools_list() -> Json {
    let collection_key = json!({
        "collection": { "type": "string" },
        "key": { "type": "string" }
    });
    json!({
        "tools": [
            {
                "name": "store",
                "description": "Insert or overwrite a document at a key in a collection.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "key": { "type": "string" },
                        "document": {}
                    },
                    "required": ["collection", "key", "document"]
                }
            },
            {
                "name": "patch",
                "description": "Merge fields into the document at key (creating it if absent), keeping indexes consistent.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "key": { "type": "string" },
                        "patch": { "type": "object" }
                    },
                    "required": ["collection", "key", "patch"]
                }
            },
            {
                "name": "compare_and_set",
                "description": "Atomically write 'new' (omit to delete) only if the current document equals 'expected' (omit for must-be-absent). Returns {applied}.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "key": { "type": "string" },
                        "expected": {},
                        "new": {}
                    },
                    "required": ["collection", "key"]
                }
            },
            {
                "name": "get",
                "description": "Fetch the document stored at a key.",
                "inputSchema": {
                    "type": "object",
                    "properties": collection_key,
                    "required": ["collection", "key"]
                }
            },
            {
                "name": "delete",
                "description": "Delete the document at a key.",
                "inputSchema": {
                    "type": "object",
                    "properties": collection_key,
                    "required": ["collection", "key"]
                }
            },
            {
                "name": "delete_where",
                "description": "Delete every document matching a filter predicate. Returns {removed}.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "filter": { "type": "object" }
                    },
                    "required": ["collection", "filter"]
                }
            },
            {
                "name": "search",
                "description": "Hybrid search: filter + vector + text, fused, optionally MMR-reranked.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "filter": { "type": "object" },
                        "vector": { "type": "object" },
                        "text": { "type": "object" },
                        "mmr": { "type": "object" },
                        "rrf_k": { "type": "number" },
                        "select": { "type": "array" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["collection"]
                }
            },
            {
                "name": "create_index",
                "description": "Create an HNSW vector index on a field to accelerate search. Set on_disk for bounded memory; quant for binary/scalar compression; or pq:{m,k} for an on-disk product-quantized index (smallest footprint).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "field": { "type": "string" },
                        "metric": { "type": "string", "enum": ["cosine", "dot", "l2"] },
                        "quant": { "type": "string", "enum": ["none", "binary", "scalar"] },
                        "on_disk": { "type": "boolean" },
                        "pq": {
                            "type": "object",
                            "properties": {
                                "m": { "type": "integer" },
                                "k": { "type": "integer" }
                            },
                            "required": ["m", "k"]
                        }
                    },
                    "required": ["collection", "field"]
                }
            },
            {
                "name": "link",
                "description": "Add a directed edge from --relation--> to between document keys.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "from": { "type": "string" },
                        "relation": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["collection", "from", "relation", "to"]
                }
            },
            {
                "name": "unlink",
                "description": "Remove a directed edge from --relation--> to.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "from": { "type": "string" },
                        "relation": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["collection", "from", "relation", "to"]
                }
            },
            {
                "name": "neighbors",
                "description": "List the targets of from --relation--> ? edges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "from": { "type": "string" },
                        "relation": { "type": "string" }
                    },
                    "required": ["collection", "from", "relation"]
                }
            },
            {
                "name": "traverse",
                "description": "Breadth-first traversal following a relation up to N hops.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "start": { "type": "string" },
                        "relation": { "type": "string" },
                        "hops": { "type": "integer" }
                    },
                    "required": ["collection", "start", "relation", "hops"]
                }
            },
            {
                "name": "geo",
                "description": "Find documents whose location field is within radius_km of (lat, lon), nearest first.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "field": { "type": "string" },
                        "lat": { "type": "number" },
                        "lon": { "type": "number" },
                        "radius_km": { "type": "number" }
                    },
                    "required": ["collection", "field", "lat", "lon", "radius_km"]
                }
            },
            {
                "name": "join",
                "description": "Left-outer join a collection to another by a foreign-key field.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "other": { "type": "string" },
                        "foreign_key_field": { "type": "string" }
                    },
                    "required": ["collection", "other", "foreign_key_field"]
                }
            },
            {
                "name": "in_neighbors",
                "description": "List the sources of incoming ? --relation--> to edges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "to": { "type": "string" },
                        "relation": { "type": "string" }
                    },
                    "required": ["collection", "to", "relation"]
                }
            },
            {
                "name": "create_text_index",
                "description": "Create an inverted full-text index on a field to accelerate text search. Set on_disk for bounded memory on large corpora (persists, no rebuild).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "field": { "type": "string" },
                        "on_disk": { "type": "boolean" }
                    },
                    "required": ["collection", "field"]
                }
            },
            {
                "name": "create_scalar_index",
                "description": "Create a scalar index on a field so equality and range filters (and counts) on it are sub-linear instead of full scans.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "field": { "type": "string" }
                    },
                    "required": ["collection", "field"]
                }
            },
            {
                "name": "create_geo_index",
                "description": "Create a spatial index on a point field so radius/bbox geo queries scan only nearby grid cells instead of the whole collection.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "field": { "type": "string" }
                    },
                    "required": ["collection", "field"]
                }
            },
            {
                "name": "create_compound_index",
                "description": "Create a compound scalar index over an ordered list of fields, so queries with equality on a leading prefix (plus an optional range on the next field) are sub-linear.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "fields": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["collection", "fields"]
                }
            },
            {
                "name": "backup",
                "description": "Write a consistent point-in-time backup of the whole database to a new file at the given path. Safe to run while writers are active.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "list_collections",
                "description": "List user collection names.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "count",
                "description": "Count documents in a collection, optionally filtered.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "filter": { "type": "object" }
                    },
                    "required": ["collection"]
                }
            },
            {
                "name": "insert_auto",
                "description": "Insert a document under an auto-generated ordered key; returns the key.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "collection": { "type": "string" },
                        "document": {}
                    },
                    "required": ["collection", "document"]
                }
            }
        ]
    })
}

/// Dispatch a `tools/call`. Tool errors become `isError` results; only
/// malformed calls return a JSON-RPC error `(code, message)`.
fn tools_call(server: &Server, params: Option<&Json>) -> Result<Json, (i64, String)> {
    let params = params.ok_or((-32602, "missing params".to_owned()))?;
    let name = params
        .get("name")
        .and_then(Json::as_str)
        .ok_or((-32602, "missing tool name".to_owned()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match server.handle(name, &arguments) {
        Ok(value) => Ok(tool_result(&value.to_string(), false)),
        Err(ToolError::UnknownTool(t)) => Err((-32601, format!("unknown tool: {t}"))),
        Err(e) => Ok(tool_result(&e.to_string(), true)),
    }
}

fn tool_result(text: &str, is_error: bool) -> Json {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

fn ok_response(id: Option<Json>, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Json::Null), "result": result })
}

fn error_response(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn server() -> Server {
        Server::open_in_memory().unwrap()
    }

    fn req(id: i64, method: &str, params: Json) -> Json {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn initialize_reports_server_info() {
        let r = handle_request(&server(), &req(1, "initialize", json!({}))).unwrap();
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["result"]["serverInfo"]["name"], "corvid-mcp");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_advertises_all_tools() {
        let r = handle_request(&server(), &req(2, "tools/list", json!({}))).unwrap();
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "store",
            "get",
            "delete",
            "patch",
            "compare_and_set",
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
            "create_text_index",
            "create_scalar_index",
            "create_geo_index",
            "create_compound_index",
            "backup",
            "list_collections",
            "count",
            "insert_auto",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn ping_responds_empty() {
        let r = handle_request(&server(), &req(9, "ping", json!({}))).unwrap();
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn tools_call_store_then_get_roundtrips() {
        let s = server();
        let store = req(
            3,
            "tools/call",
            json!({"name": "store", "arguments": {"collection": "c", "key": "k", "document": {"n": 1}}}),
        );
        let r = handle_request(&s, &store).unwrap();
        assert_eq!(r["result"]["isError"], false);

        let get = req(
            4,
            "tools/call",
            json!({"name": "get", "arguments": {"collection": "c", "key": "k"}}),
        );
        let r = handle_request(&s, &get).unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Json = serde_json::from_str(text).unwrap();
        assert_eq!(parsed, json!({"document": {"n": 1}}));
    }

    #[test]
    fn tool_failure_is_an_iserror_result() {
        // 'get' with missing key → engine-level BadParams → isError result.
        let call = req(
            5,
            "tools/call",
            json!({"name": "get", "arguments": {"collection": "c"}}),
        );
        let r = handle_request(&server(), &call).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("bad params")
        );
    }

    #[test]
    fn unknown_tool_is_jsonrpc_error() {
        let call = req(6, "tools/call", json!({"name": "frob", "arguments": {}}));
        let r = handle_request(&server(), &call).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_missing_params_is_error() {
        let bad = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call"});
        let r = handle_request(&server(), &bad).unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let r = handle_request(&server(), &req(8, "dance", json!({}))).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn missing_method_is_invalid_request() {
        let bad = json!({"jsonrpc": "2.0", "id": 10});
        let r = handle_request(&server(), &bad).unwrap();
        assert_eq!(r["error"]["code"], -32600);
    }

    #[test]
    fn initialized_notification_has_no_response() {
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_request(&server(), &note).is_none());
    }

    #[test]
    fn notification_without_id_has_no_response() {
        let note = json!({"jsonrpc": "2.0", "method": "ping"});
        assert!(handle_request(&server(), &note).is_none());
    }

    #[test]
    fn run_loop_processes_requests_and_skips_blanks() {
        let s = server();
        let input = format!(
            "{}\n\n{}\n",
            req(1, "initialize", json!({})),
            req(2, "tools/list", json!({}))
        );
        let mut out = Vec::new();
        run(&s, Cursor::new(input.into_bytes()), &mut out).unwrap();
        let lines: Vec<Json> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["id"], 2);
    }

    #[test]
    fn run_loop_reports_parse_errors() {
        let s = server();
        let mut out = Vec::new();
        run(&s, Cursor::new(b"not json\n".to_vec()), &mut out).unwrap();
        let resp: Json = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
    }

    #[test]
    fn run_loop_drops_notifications() {
        let s = server();
        let input = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        );
        let mut out = Vec::new();
        run(&s, Cursor::new(input.into_bytes()), &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn open_server_in_memory_and_file() {
        assert!(open_server(None).is_ok());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        assert!(open_server(Some(path.to_str().unwrap())).is_ok());
    }
}
