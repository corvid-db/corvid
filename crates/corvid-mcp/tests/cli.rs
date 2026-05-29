//! End-to-end test of the compiled binary: drive the real MCP server over a
//! piped stdin/stdout the way an agent host would. Covers `main` itself.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn binary_serves_mcp_over_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_corvid-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn corvid-mcp");

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"store","arguments":{"collection":"c","key":"k","document":{"n":1}}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get","arguments":{"collection":"c","key":"k"}}}"#,
        "\n",
    );

    {
        let mut stdin = child.stdin.take().expect("child stdin");
        stdin.write_all(input.as_bytes()).expect("write stdin");
        // stdin drops here, signaling EOF so the server loop finishes.
    }

    let output = child.wait_with_output().expect("wait for child");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();

    // initialize, store result, get result — the notification produced nothing.
    assert_eq!(lines.len(), 3, "unexpected output: {stdout}");
    assert!(lines[0].contains("\"protocolVersion\""));
    assert!(lines[1].contains("\"isError\":false"));
    // The get result embeds the stored document as JSON text.
    assert!(lines[2].contains("\\\"n\\\":1"), "get result: {}", lines[2]);
}
