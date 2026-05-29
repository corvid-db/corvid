//! The corvid-mcp binary: an MCP server over stdio.
//!
//! Usage: `corvid-mcp [PATH]` — opens a file-backed store at `PATH`, or an
//! in-memory store when omitted. Speaks JSON-RPC over stdin/stdout; all
//! behavior lives in the library's `protocol` and `server` modules.

use std::io::{BufReader, Write};

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1);
    let server = corvid_mcp::protocol::open_server(path.as_deref())
        .map_err(|e| std::io::Error::other(format!("failed to open store: {e}")))?;

    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let writer = stdout.lock();
    corvid_mcp::protocol::run(&server, reader, writer)?;
    std::io::stdout().flush()
}
