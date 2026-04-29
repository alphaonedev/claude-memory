// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Wave 7 / I7 — MCP stdio handshake regression guards.
//!
//! These tests spawn `ai-memory mcp --tier keyword` (the cheapest tier
//! that doesn't try to load MiniLM / nomic-embed) as a child process,
//! write JSON-RPC requests to its stdin, and assert the responses on
//! stdout. They guard the binary's stdio framing — newline-delimited
//! JSON-RPC 2.0 — which the in-process unit tests in `mcp.rs` can't
//! exercise (those drive the dispatcher directly with `RpcRequest`
//! struct values, not the line reader / writer wrapping it).
//!
//! All reads use a worker thread with a bounded timeout so a hung
//! response surfaces as a test failure rather than a CI hang.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tempfile::TempDir;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// RAII guard for the MCP child. Drops kill the child so a failed
/// assertion doesn't leak the process.
struct McpChild {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Drop for McpChild {
    fn drop(&mut self) {
        // Closing stdin ends the MCP server's read loop cleanly.
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn `ai-memory mcp --tier keyword` and return the child + a worker
/// thread reading stdout line-by-line into an mpsc channel. The caller
/// can pop responses with a bounded `recv_timeout`.
fn spawn_mcp(db: &std::path::Path) -> (McpChild, mpsc::Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-memory"))
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap(), "mcp", "--tier", "keyword"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ai-memory mcp");

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // Drain stderr so the child doesn't block writing to it.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut s = stderr;
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
    }

    let (tx, rx) = mpsc::channel();
    spawn_stdout_reader(stdout, tx);
    (
        McpChild {
            child: Some(child),
            stdin: Some(stdin),
        },
        rx,
    )
}

fn spawn_stdout_reader(stdout: ChildStdout, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(_) => {} // skip blanks
                Err(_) => break,
            }
        }
    });
}

/// Send a JSON-RPC request line to the MCP child's stdin, then wait up
/// to `READ_TIMEOUT` for the next response line.
fn send_and_recv(
    stdin: &mut ChildStdin,
    rx: &mpsc::Receiver<String>,
    payload: &serde_json::Value,
) -> serde_json::Value {
    let line = serde_json::to_string(payload).unwrap();
    writeln!(stdin, "{line}").expect("write to mcp stdin");
    stdin.flush().expect("flush mcp stdin");
    let resp = rx
        .recv_timeout(READ_TIMEOUT)
        .expect("mcp response did not arrive within READ_TIMEOUT");
    serde_json::from_str(&resp).unwrap_or_else(|e| panic!("parse mcp response: {e}: {resp}"))
}

#[test]
fn mcp_initialize_handshake_succeeds() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mcp.db");
    let (mut guard, rx) = spawn_mcp(&db);
    let stdin = guard.stdin.as_mut().unwrap();

    let resp = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        }),
    );
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object(), "result missing: {resp}");
    assert_eq!(resp["result"]["serverInfo"]["name"], "ai-memory");
}

#[test]
fn mcp_list_tools_returns_expected_count() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mcp.db");
    let (mut guard, rx) = spawn_mcp(&db);
    let stdin = guard.stdin.as_mut().unwrap();

    // initialize first
    let _ = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        }),
    );

    let resp = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools array missing");
    // The lib tests assert exactly 43 in v0.6.3 (`tool_definitions_returns_43_tools`).
    // We assert a lower bound so this test doesn't regress every time
    // a new tool ships, while still catching an empty/missing list.
    assert!(
        tools.len() >= 40,
        "expected >=40 tools, got {} ({})",
        tools.len(),
        resp
    );
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"memory_store"));
    assert!(names.contains(&"memory_recall"));
}

#[test]
fn mcp_call_memory_store_then_memory_recall_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mcp.db");
    let (mut guard, rx) = spawn_mcp(&db);
    let stdin = guard.stdin.as_mut().unwrap();

    // initialize
    let _ = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        }),
    );

    // tools/call memory_store
    let resp = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "memory_store",
                "arguments": {
                    "title": "mcp-roundtrip",
                    "content": "uniquemcptoken keyword content",
                    "tier": "mid",
                    "namespace": "mcp-test"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 2);
    let content = &resp["result"]["content"][0]["text"];
    assert!(content.is_string(), "expected text content, got: {resp}");
    let body = content.as_str().unwrap();
    assert!(
        body.contains("\"id\"") || body.contains("id"),
        "store response missing id: {body}"
    );

    // tools/call memory_recall — keyword tier so this is a pure FTS hit.
    let resp = send_and_recv(
        stdin,
        &rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": {
                    "context": "uniquemcptoken",
                    "namespace": "mcp-test"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 3);
    let recall_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("recall content text")
        .to_string();
    assert!(
        recall_text.contains("uniquemcptoken") || recall_text.contains("mcp-roundtrip"),
        "recall didn't return the stored memory: {recall_text}"
    );
}
