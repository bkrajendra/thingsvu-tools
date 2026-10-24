// SPDX-License-Identifier: GPL-3.0-or-later
//! Drives the real `kicad-ai-mcp` binary over stdio with raw JSON-RPC, as an MCP host would.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Mcp {
    async fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kicad-ai-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = mcp
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "thingsvu-stdio-test", "version": "0" }
                }),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "kicad-ai-mcp", "{init}");
        mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        mcp
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a request and returns its response. Every stdout line must be a JSON-RPC message.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(180), self.stdout.next_line())
                .await
                .expect("response timeout")
                .unwrap()
                .expect("server closed stdout");
            let message: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}");
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self
            .request("tools/call", json!({ "name": tool, "arguments": arguments }))
            .await;
        response["result"]["structuredContent"].clone()
    }
}

#[tokio::test]
async fn lists_tools_and_returns_envelopes() {
    let mut mcp = Mcp::start().await;

    let tools = mcp.request("tools/list", json!({})).await;
    let mut names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["session_close", "session_open", "session_status"]);

    let status = mcp.call("session_status", json!({})).await;
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result_state"], "done");
    assert_eq!(status["result"]["sessions"], json!([]));
    assert!(status["correlation_id"].as_str().unwrap().starts_with("req-"));

    let relative = mcp
        .call("session_open", json!({ "schematic_path": "relative.kicad_sch" }))
        .await;
    assert_eq!(relative["ok"], false);
    assert_eq!(relative["result_state"], "rejected");
    assert_eq!(relative["error"]["code"], "INVALID_ARGUMENT");

    let missing = mcp.call("session_close", json!({ "session_id": "nope" })).await;
    assert_eq!(missing["error"]["code"], "NO_SESSION");

    // Unknown fields are rejected by the input schema, before the tool runs.
    let response = mcp
        .request(
            "tools/call",
            json!({ "name": "session_status", "arguments": { "bogus": 1 } }),
        )
        .await;
    assert!(
        response.get("error").is_some() || response["result"]["isError"] == true,
        "{response}"
    );

    drop(mcp.stdin);
    let _ = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait()).await;
}

#[tokio::test]
async fn live_headless_session_round_trip() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("mcp-new.kicad_sch");

    let mut mcp = Mcp::start().await;
    let status = mcp.call("session_status", json!({})).await;
    let installs = status["result"]["kicad_installs"].as_array().unwrap();
    assert!(installs.iter().any(|i| i["series"] == 11), "{status}");

    let opened = mcp
        .call("session_open", json!({ "schematic_path": schematic, "create": true }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let result = &opened["result"];
    assert_eq!(result["connected"], true);
    assert!(result["kicad_version"].as_str().unwrap().starts_with("10.99"));
    assert_eq!(result["capabilities"]["sch.place_symbol"]["available"], true);
    assert_eq!(result["capabilities"]["sch.erc"]["backend"], "kicad_cli");
    assert_eq!(opened["provenance"][0]["origin"], "kicad_ipc");
    let id = result["session_id"].as_str().unwrap().to_owned();

    let listed = mcp.call("session_status", json!({})).await;
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 1);

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    let after = mcp.call("session_status", json!({ "session_id": id })).await;
    assert_eq!(after["error"]["code"], "NO_SESSION");
}
