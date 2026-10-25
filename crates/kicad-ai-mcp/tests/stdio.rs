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
        Self::start_with(&[]).await
    }

    async fn start_with(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kicad-ai-mcp"))
            .args(args)
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
    assert_eq!(
        names,
        [
            "sch_apply_plan",
            "sch_arrange_plan",
            "sch_create_demo_circuit",
            "sch_declutter_plan",
            "sch_describe_layout",
            "sch_draw_plan",
            "sch_edit_items_plan",
            "sch_export_netlist",
            "sch_get_netlist",
            "sch_get_symbol_pins",
            "sch_label_plan",
            "sch_list_symbols",
            "sch_plan_changes",
            "sch_reconnect_plan",
            "sch_render",
            "sch_review",
            "sch_run_erc",
            "sch_save",
            "sch_set_field",
            "sch_tidy_plan",
            "session_close",
            "session_list_documents",
            "session_open",
            "session_status"
        ]
    );

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

#[tokio::test]
async fn live_read_tools_on_the_fixture() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/two-resistor-series-chain");
    for file in ["kicad-demo.kicad_sch", "kicad-demo.kicad_pro"] {
        std::fs::copy(fixture.join(file), dir.join(file)).unwrap();
    }

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": dir.join("kicad-demo.kicad_sch") }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(symbols["ok"], true, "{symbols}");
    assert_eq!(symbols["result"]["count"], 2);
    let r1 = &symbols["result"]["symbols"][0];
    assert_eq!(
        (
            r1["reference"].as_str(),
            r1["value"].as_str(),
            r1["library_id"].as_str()
        ),
        (Some("R1"), Some("10k"), Some("Device:R"))
    );
    assert_eq!(r1["position_mm"], json!({ "x": 50.8, "y": 50.8 }));
    assert_eq!(symbols["provenance"][0]["origin"], "kicad_ipc");

    let pins = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": r1["id"] }))
        .await;
    assert_eq!(
        pins["result"]["pins"][0]["position_mm"],
        json!({ "x": 50.8, "y": 46.99 }),
        "{pins}"
    );
    let ambiguous = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": "R7" }))
        .await;
    assert_eq!(ambiguous["error"]["code"], "SYMBOL_NOT_FOUND");

    let netlist = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(netlist["result"]["count"], 3, "{netlist}");
    assert_eq!(netlist["result"]["unconnected_pins"], 0);
    let mid = netlist["result"]["nets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "/MID")
        .unwrap();
    let mid_pins: Vec<String> = mid["pins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| format!("{}.{}", p["reference"].as_str().unwrap(), p["pin"].as_str().unwrap()))
        .collect();
    assert_eq!(mid_pins, ["R1.2", "R2.1"]);

    let docs = mcp.call("session_list_documents", json!({ "session_id": id })).await;
    assert_eq!(docs["ok"], true, "{docs}");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
}

#[tokio::test]
async fn live_demo_circuit_end_to_end() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("demo.kicad_sch");

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("session_open", json!({ "schematic_path": schematic, "create": true }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // Plan only.
    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true);
    assert_eq!(plan["result"]["counts"]["symbols"], 2);
    assert!(
        mcp.call("sch_list_symbols", json!({ "session_id": id })).await["result"]["count"] == 0,
        "planning changes nothing"
    );

    // Apply without confirm is refused.
    let plan_id = plan["result"]["plan_id"].clone();
    let refused = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": false, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT");

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result_state"], "done", "{applied}");
    assert_eq!(applied["result"]["verified"], true);
    assert_eq!(applied["result"]["undo_steps"], 2);

    // Same key again: replayed, not applied twice.
    let replay = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0001" }),
        )
        .await;
    assert!(replay["warnings"].to_string().contains("replayed"), "{replay}");
    assert_eq!(
        mcp.call("sch_list_symbols", json!({ "session_id": id })).await["result"]["count"],
        2
    );

    // The same plan with a new key is stale.
    let again = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "demo-key-0002" }),
        )
        .await;
    assert_eq!(again["error"]["code"], "STALE_PLAN", "{again}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    assert_eq!(erc["ok"], true, "{erc}");
    assert_eq!(erc["provenance"][0]["origin"], "kicad_cli");
    println!(
        "ERC: {} errors, {} warnings: {}",
        erc["result"]["errors"], erc["result"]["warnings"], erc["result"]["violations"]
    );

    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let content = render["result"]["content"].as_array().unwrap();
    let image = content
        .iter()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("no image block: {render}"));
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().len() > 1000);

    let net_path = dir.join("demo.net");
    let exported = mcp
        .call(
            "sch_export_netlist",
            json!({ "session_id": id, "output_path": net_path }),
        )
        .await;
    assert_eq!(exported["result"]["nets"], 3, "{exported}");
    let outside = mcp
        .call(
            "sch_export_netlist",
            json!({ "session_id": id, "output_path": std::env::temp_dir().join("x.net") }),
        )
        .await;
    assert_eq!(outside["error"]["code"], "INVALID_ARGUMENT");

    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // The saved file, checked independently with kicad-cli.
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let netlist =
        kicad_cli_runner::netlist::export_netlist(cli, &schematic, &dir.join("check.net"), Duration::from_secs(120))
            .await
            .unwrap();
    let topology = netlist.topology();
    for (name, pins) in [
        ("/INPUT", vec!["R1.1"]),
        ("/MID", vec!["R1.2", "R2.1"]),
        ("/OUTPUT", vec!["R2.2"]),
    ] {
        let pins: std::collections::BTreeSet<String> = pins.into_iter().map(str::to_owned).collect();
        assert!(topology.contains(&(name.to_owned(), pins)), "{name} in {topology:?}");
    }
    assert!(
        netlist
            .components
            .iter()
            .all(|c| c.value == "10k" && c.lib_source == "Device:R"),
        "{:?}",
        netlist.components
    );
}

#[tokio::test]
async fn live_file_provider_on_kicad_10() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("demo.kicad_sch");

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "create": true, "provider": "file" }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["result"]["provider"], "file");
    assert_eq!(opened["provenance"][0]["origin"], "project_file");
    assert!(
        opened["result"]["kicad_version"].as_str().unwrap().starts_with("10.0"),
        "{opened}"
    );
    assert_eq!(
        opened["result"]["capabilities"]["sch.place_symbol"]["backend"],
        "project_file"
    );
    assert_eq!(opened["result"]["capabilities"]["session.commits"]["available"], false);
    let id = opened["result"]["session_id"].clone();
    assert!(schematic.is_file(), "a file session creates the file");

    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let plan_id = plan["result"]["plan_id"].clone();

    // An edit made outside the tool after planning makes the plan stale and changes nothing more.
    let original = std::fs::read_to_string(&schematic).unwrap();
    std::fs::write(&schematic, format!("{original}\n")).unwrap();
    let stale = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0001" }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "STALE_PLAN", "{stale}");
    std::fs::write(&schematic, &original).unwrap();

    // An editor that opened the file in the meantime blocks the change.
    let lock = dir.join("~demo.kicad_sch.lck");
    std::fs::write(&lock, "{}").unwrap();
    let plan = mcp.call("sch_create_demo_circuit", json!({ "session_id": id })).await;
    let plan_id = plan["result"]["plan_id"].clone();
    let blocked = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0002" }),
        )
        .await;
    assert_eq!(blocked["error"]["code"], "DOCUMENT_OPEN_ELSEWHERE", "{blocked}");
    assert_eq!(std::fs::read_to_string(&schematic).unwrap(), original);
    std::fs::remove_file(&lock).unwrap();

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "file-key-0003" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result_state"], "done", "{applied}");
    assert_eq!(applied["result"]["verified"], true);
    assert_eq!(applied["result"]["undo_steps"], 0);
    let backup = applied["result"]["backup_path"]
        .as_str()
        .expect("the previous file is kept");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), original);

    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(symbols["result"]["count"], 2, "{symbols}");
    assert_eq!(symbols["provenance"][0]["origin"], "project_file");
    let pins = mcp
        .call("sch_get_symbol_pins", json!({ "session_id": id, "symbol": "R1" }))
        .await;
    assert_eq!(pins["result"]["pins"].as_array().unwrap().len(), 2, "{pins}");
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets["provenance"][0]["origin"], "kicad_cli");
    assert_eq!(nets["result"]["unconnected_pins"], 0, "{nets}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    assert_eq!(erc["ok"], true, "{erc}");
    assert!(
        erc["warnings"].to_string().contains("copy of the schematic file"),
        "{erc}"
    );

    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let image = render["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("no image block: {render}"));
    assert_eq!(image["mimeType"], "image/png");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // The file as KiCad 10 itself reads it.
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs
        .iter()
        .find(|i| i.version.series() == 10)
        .expect("a KiCad 10 install");
    let netlist =
        kicad_cli_runner::netlist::export_netlist(cli, &schematic, &dir.join("check.net"), Duration::from_secs(120))
            .await
            .unwrap();
    let topology = netlist.topology();
    for (name, pins) in [
        ("/INPUT", vec!["R1.1"]),
        ("/MID", vec!["R1.2", "R2.1"]),
        ("/OUTPUT", vec!["R2.2"]),
    ] {
        let pins: std::collections::BTreeSet<String> = pins.into_iter().map(str::to_owned).collect();
        assert!(topology.contains(&(name.to_owned(), pins)), "{name} in {topology:?}");
    }
    assert!(netlist
        .components
        .iter()
        .all(|c| c.value == "10k" && c.lib_source == "Device:R"));
}

#[tokio::test]
async fn live_review_tidy_and_set_field_through_mcp() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-tidy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": "file" }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // The review says what is wrong and what a tidy would do, and changes nothing.
    let before = std::fs::read(&schematic).unwrap();
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(review["ok"], true, "{review}");
    assert_eq!(review["result"]["parts"], 13, "{review}");
    assert_eq!(review["result"]["tidy"]["possible"], true, "{review}");
    assert!(
        review["result"]["problems"]
            .to_string()
            .contains("D2 and #PWR10 overlap"),
        "{review}"
    );
    assert_eq!(review["result"]["tidy"]["already_tidy"], false);
    assert_eq!(std::fs::read(&schematic).unwrap(), before, "a review changes nothing");

    let plan = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert!(plan["result"]["moves"].as_array().unwrap().len() >= 10);
    assert!(plan["result"]["removals"].as_array().unwrap().len() >= 30);
    assert!(plan["result"]["readability_before"]["overlapping_symbols"]
        .as_array()
        .is_some());
    assert_eq!(std::fs::read(&schematic).unwrap(), before, "planning changes nothing");
    let plan_id = plan["result"]["plan_id"].clone();

    let refused = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": false, "idempotency_key": "tidy-key-0001" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT");

    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan_id, "confirm": true, "idempotency_key": "tidy-key-0001" }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert!(applied["result"]["moved"].as_u64().unwrap() >= 10);
    assert!(applied["result"]["removed"].as_u64().unwrap() >= 30);

    // Reviewed again, it is tidy. The text KiCad put beside the parts may still run into things.
    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");
    println!("PROBLEMS after tidy: {}", after["result"]["problems"]);

    // Declutter moves that text; afterwards nothing runs into anything.
    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    assert_eq!(declutter["ok"], true, "{declutter}");
    println!("DECLUTTER {}", declutter["result"]["field_changes"]);
    if declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0)
        > 0
    {
        let applied = mcp
            .call(
                "sch_apply_plan",
                json!({ "session_id": id, "plan_id": declutter["result"]["plan_id"], "confirm": true, "idempotency_key": "declutter-key-0001" }),
            )
            .await;
        assert_eq!(applied["ok"], true, "{applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }
    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["problems"], json!([]), "{after}");
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");

    // Change one value. It needs confirm, a real part, and sensible text.
    let no_confirm = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "value", "text": "2k2", "confirm": false }),
        )
        .await;
    assert_eq!(no_confirm["error"]["code"], "INVALID_ARGUMENT");
    let unknown = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R99", "field": "value", "text": "2k2", "confirm": true }),
        )
        .await;
    assert_eq!(unknown["error"]["code"], "SYMBOL_NOT_FOUND", "{unknown}");
    let changed = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "value", "text": "2k2", "confirm": true }),
        )
        .await;
    assert_eq!(changed["ok"], true, "{changed}");
    assert_eq!(changed["result"]["value"], "2k2");
    let footprint = mcp
        .call(
            "sch_set_field",
            json!({ "session_id": id, "symbol": "R3", "field": "footprint", "text": "Resistor_SMD:R_0805_2012Metric", "confirm": true }),
        )
        .await;
    assert_eq!(
        footprint["result"]["footprint"], "Resistor_SMD:R_0805_2012Metric",
        "{footprint}"
    );
    assert_eq!(footprint["result"]["value"], "2k2", "the value is kept");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let r3 = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["reference"] == "R3")
        .unwrap();
    assert_eq!(r3["value"], "2k2");

    // The result is still the same circuit, as KiCad reads it.
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets["result"]["unconnected_pins"], 0, "{nets}");
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "ERC after tidy: {} errors, {} warnings",
        erc["result"]["errors"], erc["result"]["warnings"]
    );
}

/// Place parts by hand, let the wires break, then wire everything again: the circuit must come
/// out the same, and no part moves in the second step.
#[tokio::test]
async fn live_arrange_then_reconnect_through_mcp() {
    arrange_flow("file").await;
}

#[tokio::test]
async fn live_arrange_then_reconnect_through_the_api() {
    arrange_flow("headless").await;
}

async fn arrange_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-arrange-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();

    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(layout["ok"], true, "{layout}");
    let parts = layout["result"]["parts"].as_array().unwrap();
    assert!(parts.len() >= 10, "{layout}");
    assert!(parts[0]["pins"][0]["x"].is_number(), "{layout}");
    let nets_before = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    let count_before = nets_before["result"]["nets"].as_array().unwrap().len();

    // Move a few parts far away, turn one, and delete nothing yet.
    let key = |n: &str| format!("{n}-{}", std::process::id());
    let arrange = mcp
        .call(
            "sch_arrange_plan",
            json!({ "session_id": id, "edits": [
                { "reference": "C1", "x_mm": 200.0, "y_mm": 40.0 },
                { "reference": "C2", "dx_mm": 30.0, "dy_mm": 20.0, "rotate_deg": 90 },
                { "reference": "R3", "x_mm": 200.0, "y_mm": 90.0, "mirror": "y" },
            ] }),
        )
        .await;
    assert_eq!(arrange["ok"], true, "{arrange}");
    assert_eq!(arrange["result"]["counts"]["moves"], 3, "{arrange}");
    assert!(arrange["result"]["findings"]
        .to_string()
        .contains("ARRANGE_WIRES_LEFT_BEHIND"));
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": arrange["result"]["plan_id"], "confirm": true, "idempotency_key": key("arrange-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["moved"], 3, "{applied}");

    // The circuit is broken now: KiCad no longer sees the nets it had.
    let broken = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    let moved_pins_net = |v: &serde_json::Value| {
        v["result"]["nets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["pins"].to_string().contains("\"C1\""))
            .count()
    };
    println!("nets with C1 before reconnect: {}", moved_pins_net(&broken));

    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    assert_eq!(reconnect["ok"], true, "{reconnect}");
    assert_eq!(reconnect["result"]["can_apply"], true, "{reconnect}");
    assert_eq!(reconnect["result"]["counts"]["moves"], 0, "{reconnect}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true, "idempotency_key": key("reconnect-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let nets_after = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert_eq!(nets_after["result"]["unconnected_pins"], 0, "{nets_after}");
    assert_eq!(nets_after["result"]["nets"].as_array().unwrap().len(), count_before);
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "ERC after arrange + reconnect: {} errors, {} warnings",
        erc["result"]["errors"], erc["result"]["warnings"]
    );

    // A tidy starts from a saved document (so that a failure can be reverted).
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");

    // Hints: keep C1 where it was put, and the rest is laid out around it.
    let tidy = mcp
        .call(
            "sch_tidy_plan",
            json!({ "session_id": id, "keep": ["C1"], "near": [{ "reference": "C2", "to": "C1", "side": "right", "gap_mm": 10.0 }] }),
        )
        .await;
    assert_eq!(tidy["ok"], true, "{tidy}");
    assert_eq!(tidy["result"]["can_apply"], true, "{tidy}");
    assert!(
        !tidy["result"]["moves"].to_string().contains("\"C1\""),
        "a kept part does not move: {tidy}"
    );
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": tidy["result"]["plan_id"], "confirm": true, "idempotency_key": key("hints-1") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    // Delete a part, then wire the rest again: the other parts keep their nets.
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let delete = mcp
        .call("sch_arrange_plan", json!({ "session_id": id, "delete": ["C2"] }))
        .await;
    assert_eq!(delete["result"]["counts"]["removals"], 1, "{delete}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": delete["result"]["plan_id"], "confirm": true, "idempotency_key": key("delete-1") }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true, "idempotency_key": key("delete-2") }),
        )
        .await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert!(
        !symbols["result"]["symbols"].to_string().contains("\"C2\""),
        "C2 is gone"
    );

    // A bad request is refused, and an unknown part named.
    let bad = mcp
        .call(
            "sch_arrange_plan",
            json!({ "session_id": id, "edits": [{ "reference": "R99", "dx_mm": 1.0 }] }),
        )
        .await;
    assert_eq!(bad["result"]["can_apply"], false, "{bad}");
}

/// Drawings, labels, and notes: add, rename, turn, move, delete. Every plan leaves the nets as
/// they should be, which applying checks against KiCad.
#[tokio::test]
async fn live_labels_drawings_and_items_through_mcp() {
    text_tools_flow("file").await;
}

#[tokio::test]
async fn live_labels_drawings_and_items_through_the_api() {
    text_tools_flow("headless").await;
}

async fn text_tools_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-text-{provider}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call(
            "session_open",
            json!({ "schematic_path": schematic, "provider": provider }),
        )
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let n = std::process::id();
    let mut step = 0;
    let mut apply = |plan: serde_json::Value| {
        step += 1;
        json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                "idempotency_key": format!("text-{provider}-{n}-{step}") })
    };

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(layout["ok"], true, "{layout}");
    let drawings_before = layout["result"]["drawings"].as_array().unwrap().len();
    let notes_before = layout["result"]["notes"].as_array().unwrap().len();
    assert!(layout["result"]["labels"][0]["id"].is_string(), "{layout}");
    let label = layout["result"]["labels"][0].clone();

    // Draw: a box around two parts with a title, a note, and a line.
    let draw = mcp
        .call(
            "sch_draw_plan",
            json!({ "session_id": id, "items": [
                { "kind": "box_around", "parts": ["U1", "C1"], "title": "Regulator", "margin_mm": 5.0 },
                { "kind": "text", "x_mm": 30.0, "y_mm": 150.0, "text": "Checked by assistant" },
                { "kind": "line", "x1_mm": 30.0, "y1_mm": 152.0, "x2_mm": 60.0, "y2_mm": 152.0 },
                { "kind": "circle", "x_mm": 80.0, "y_mm": 150.0, "radius_mm": 3.0 },
            ] }),
        )
        .await;
    assert_eq!(draw["ok"], true, "{draw}");
    assert_eq!(draw["result"]["can_apply"], true, "{draw}");
    let applied = mcp.call("sch_apply_plan", apply(draw)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(
        layout["result"]["drawings"].as_array().unwrap().len(),
        drawings_before + 3,
        "{layout}"
    );
    assert_eq!(
        layout["result"]["notes"].as_array().unwrap().len(),
        notes_before + 2,
        "{layout}"
    );
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Labels: a second name for a named net is refused; renaming works and keeps every net.
    let second = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "add": [{ "text": "SECOND", "x_mm": label["at"]["x"], "y_mm": label["at"]["y"] }] }),
        )
        .await;
    assert_eq!(second["result"]["can_apply"], false, "{second}");
    assert!(
        second["result"]["findings"]
            .to_string()
            .contains("LABEL_NET_ALREADY_NAMED"),
        "{second}"
    );
    let rename = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "rename": [{ "id": label["id"], "text": "RENAMED" }] }),
        )
        .await;
    assert_eq!(rename["result"]["can_apply"], true, "{rename}");
    let applied = mcp.call("sch_apply_plan", apply(rename)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let nets = mcp.call("sch_get_netlist", json!({ "session_id": id })).await;
    assert!(nets["result"].to_string().contains("RENAMED"), "{nets}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Turn a label.
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let renamed = layout["result"]["labels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["text"] == "RENAMED")
        .unwrap()
        .clone();
    let turn = mcp
        .call(
            "sch_label_plan",
            json!({ "session_id": id, "turn": [{ "id": renamed["id"], "direction": "up" }] }),
        )
        .await;
    let applied = mcp.call("sch_apply_plan", apply(turn)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Items: move the new note, delete the line and the circle, refuse to move a wire.
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let note = layout["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["text"] == "Checked by assistant")
        .unwrap()
        .clone();
    let wire = layout["result"]["wires"][0]["id"].clone();
    let shapes: Vec<serde_json::Value> = layout["result"]["drawings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["shape"].to_string().contains("circle") || d["shape"].to_string().contains("line"))
        .map(|d| d["id"].clone())
        .collect();
    let refused = mcp
        .call(
            "sch_edit_items_plan",
            json!({ "session_id": id, "moves": [{ "id": wire, "dx_mm": 1.0, "dy_mm": 0.0 }] }),
        )
        .await;
    assert_eq!(refused["result"]["can_apply"], false, "{refused}");
    let edit = mcp
        .call(
            "sch_edit_items_plan",
            json!({ "session_id": id, "remove": shapes, "moves": [{ "id": note["id"], "dx_mm": 10.0, "dy_mm": 5.0 }] }),
        )
        .await;
    assert_eq!(edit["result"]["can_apply"], true, "{edit}");
    let applied = mcp.call("sch_apply_plan", apply(edit)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    assert_eq!(
        layout["result"]["drawings"].as_array().unwrap().len(),
        drawings_before + 1,
        "{layout}"
    );
    let moved = layout["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["text"] == "Checked by assistant")
        .unwrap();
    assert_eq!(
        moved["at"]["x"].as_f64().unwrap(),
        note["at"]["x"].as_f64().unwrap() + 10.16,
        "snapped to the grid: {moved}"
    );
    // Text: after a tidy, move whatever text runs into something. The placements read back.
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let tidy = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    assert_eq!(tidy["result"]["can_apply"], true, "{tidy}");
    let applied = mcp.call("sch_apply_plan", apply(tidy)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    assert_eq!(declutter["result"]["can_apply"], true, "{declutter}");
    let changes = declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0);
    println!("DECLUTTER ({provider}): {changes} changes");
    if changes > 0 {
        let applied = mcp.call("sch_apply_plan", apply(declutter)).await;
        assert_eq!(applied["ok"], true, "{applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
        assert!(applied["result"]["placements_checked"].as_u64().unwrap_or(0) > 0 || changes > 0);
    }
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    let problems = review["result"]["problems"].to_string();
    assert!(!problems.contains("runs into"), "text still overlaps: {problems}");
    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!("ERC after text tools ({provider}): {} errors", erc["result"]["errors"]);
}

#[tokio::test]
async fn read_only_mode_refuses_changes() {
    let mut mcp = Mcp::start_with(&["--read-only"]).await;
    for (tool, args) in [
        (
            "sch_apply_plan",
            json!({ "session_id": "s", "plan_id": "p", "confirm": true, "idempotency_key": "key-12345" }),
        ),
        ("sch_save", json!({ "session_id": "s" })),
        (
            "sch_export_netlist",
            json!({ "session_id": "s", "output_path": "C:/x/y.net" }),
        ),
    ] {
        let r = mcp.call(tool, args).await;
        assert_eq!(r["error"]["code"], "READ_ONLY_MODE", "{tool}: {r}");
    }
}

/// Against a real KiCad 11 editor: open a disposable, empty project in the nightly Schematic
/// Editor, press "KiCad AI: connect", then run with THINGSVU_GUI=1.
#[tokio::test]
async fn gui_editor_demo() {
    if std::env::var("THINGSVU_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let status = mcp.call("session_status", json!({})).await;
    println!("GUI EDITORS {}", status["result"]["gui_editors"]);
    assert!(
        !status["result"]["gui_editors"].as_array().unwrap().is_empty(),
        "press the KiCad AI button first"
    );

    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!(
        "GUI SESSION {} document {}",
        opened["result"]["kicad_version"], opened["result"]["document"]
    );
    println!("GUI CAPABILITIES {}", opened["result"]["capabilities"]);
    assert_eq!(opened["provenance"][0]["provider"], "gui");

    let before = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    assert_eq!(before["result"]["count"], 0, "use an empty project: {before}");

    let applied = mcp
        .call("sch_create_demo_circuit", json!({ "session_id": id, "apply": true, "confirm": true, "idempotency_key": format!("gui-{}", std::process::id()) }))
        .await;
    println!("GUI APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["undo_steps"], 2);

    // Values must persist in the editor (the full-object update did not in Phase 0).
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let values: Vec<_> = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["reference"].as_str().unwrap().to_owned(),
                s["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        values,
        [("R1".to_owned(), "10k".to_owned()), ("R2".to_owned(), "10k".to_owned())]
    );

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC ok={} errors={} warnings={}",
        erc["ok"], erc["result"]["errors"], erc["result"]["warnings"]
    );
    let render = mcp
        .request(
            "tools/call",
            json!({ "name": "sch_render", "arguments": { "session_id": id } }),
        )
        .await;
    let has_image = render["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["type"] == "image");
    println!("GUI RENDER image={has_image}");

    // Saving in the GUI: recorded, not asserted (SaveDocument support in the editor is unverified).
    let saved = mcp.call("sch_save", json!({ "session_id": id })).await;
    println!("GUI SAVE {saved}");

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: check the editor (two Ctrl+Z steps should remove the demo)");
}

/// Against a real KiCad 11 editor with a schematic open: review it, plan a tidy, apply it, and
/// check ERC. Open a **copy** of a design in the nightly Schematic Editor, press "KiCad AI:
/// connect", then run with THINGSVU_GUI_TIDY=1. Undo in the editor (Ctrl+Z twice) restores it.
#[tokio::test]
async fn gui_tidy_demo() {
    if std::env::var("THINGSVU_GUI_TIDY").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_TIDY=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!("GUI SESSION {}", opened["result"]["document"]);

    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW {}", review["result"]);
    assert_eq!(review["ok"], true, "{review}");
    let erc_before = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC BEFORE errors={} warnings={}",
        erc_before["result"]["errors"], erc_before["result"]["warnings"]
    );

    let plan = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    println!(
        "GUI PLAN counts={} findings={}",
        plan["result"]["counts"], plan["result"]["findings"]
    );
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({
                "session_id": id,
                "plan_id": plan["result"]["plan_id"],
                "confirm": true,
                "idempotency_key": format!("gui-tidy-{}", std::process::id())
            }),
        )
        .await;
    println!("GUI APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["undo_steps"], 2);

    let after = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW AFTER {}", after["result"]);
    let again = mcp.call("sch_tidy_plan", json!({ "session_id": id })).await;
    println!("GUI PLAN AGAIN removals={}", again["result"]["removals"]);
    assert_eq!(after["result"]["tidy"]["already_tidy"], true, "{after}");
    let erc_after = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC AFTER errors={} warnings={}",
        erc_after["result"]["errors"], erc_after["result"]["warnings"]
    );
    assert!(erc_after["result"]["errors"].as_u64() <= erc_before["result"]["errors"].as_u64());

    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z twice undoes the tidy");
}

/// Against a real KiCad 11 editor: move two parts, let their wires break, wire everything again.
/// Open a **copy** of a design, press "KiCad AI: connect", then run with THINGSVU_GUI_ARRANGE=1.
/// Ctrl+Z in the editor undoes each step.
#[tokio::test]
async fn gui_arrange_demo() {
    if std::env::var("THINGSVU_GUI_ARRANGE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_ARRANGE=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let parts = layout["result"]["parts"].as_array().unwrap();
    let movable: Vec<&str> = parts
        .iter()
        .filter_map(|p| p["reference"].as_str())
        .filter(|r| !r.starts_with('#'))
        .take(2)
        .collect();
    println!("GUI ARRANGE moving {movable:?}");
    let erc_before = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    let edits: Vec<_> = movable
        .iter()
        .map(|r| json!({ "reference": r, "dx_mm": 25.4, "dy_mm": 12.7 }))
        .collect();
    let plan = mcp
        .call("sch_arrange_plan", json!({ "session_id": id, "edits": edits }))
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-arrange-{}", std::process::id()) }),
        )
        .await;
    println!("GUI ARRANGE APPLY {applied}");
    assert_eq!(applied["ok"], true, "{applied}");

    let reconnect = mcp.call("sch_reconnect_plan", json!({ "session_id": id })).await;
    println!(
        "GUI RECONNECT PLAN counts={} findings={}",
        reconnect["result"]["counts"], reconnect["result"]["findings"]
    );
    assert_eq!(reconnect["result"]["can_apply"], true, "{reconnect}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": reconnect["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-reconnect-{}", std::process::id()) }),
        )
        .await;
    println!("GUI RECONNECT APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let erc_after = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!(
        "GUI ERC before={} after={}",
        erc_before["result"]["errors"], erc_after["result"]["errors"]
    );
    assert!(erc_after["result"]["errors"].as_u64() <= erc_before["result"]["errors"].as_u64());
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z undoes the steps");
}

/// Against a real KiCad 11 editor: report text that runs into things, move it, draw a titled box
/// around the first two parts, and report again. Open a **copy**, press "KiCad AI: connect", then
/// run with THINGSVU_GUI_TEXT=1. Ctrl+Z in the editor undoes each step.
#[tokio::test]
async fn gui_text_demo() {
    if std::env::var("THINGSVU_GUI_TEXT").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_TEXT=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI TEXT PROBLEMS BEFORE {}", review["result"]["problems"]);

    let declutter = mcp.call("sch_declutter_plan", json!({ "session_id": id })).await;
    println!(
        "GUI DECLUTTER counts={} changes={} findings={}",
        declutter["result"]["counts"], declutter["result"]["field_changes"], declutter["result"]["findings"]
    );
    assert_eq!(declutter["result"]["can_apply"], true, "{declutter}");
    let changes = declutter["result"]["counts"]["field_changes"].as_u64().unwrap_or(0)
        + declutter["result"]["counts"]["labels"].as_u64().unwrap_or(0);
    if changes > 0 {
        let applied = mcp
            .call(
                "sch_apply_plan",
                json!({ "session_id": id, "plan_id": declutter["result"]["plan_id"], "confirm": true,
                        "idempotency_key": format!("gui-declutter-{}", std::process::id()) }),
            )
            .await;
        println!("GUI DECLUTTER APPLY {applied}");
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }

    let layout = mcp.call("sch_describe_layout", json!({ "session_id": id })).await;
    let parts: Vec<&str> = layout["result"]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["reference"].as_str())
        .filter(|r| !r.starts_with('#'))
        .take(2)
        .collect();
    let draw = mcp
        .call(
            "sch_draw_plan",
            json!({ "session_id": id, "items": [{ "kind": "box_around", "parts": parts, "title": "Assistant box" }] }),
        )
        .await;
    assert_eq!(draw["result"]["can_apply"], true, "{draw}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": draw["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-draw-{}", std::process::id()) }),
        )
        .await;
    println!("GUI DRAW APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let review = mcp.call("sch_review", json!({ "session_id": id })).await;
    println!("GUI TEXT PROBLEMS AFTER {}", review["result"]["problems"]);
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at the editor; Ctrl+Z undoes the steps");
}
