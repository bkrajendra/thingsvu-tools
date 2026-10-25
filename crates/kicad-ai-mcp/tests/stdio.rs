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
            "bom_export",
            "bom_get",
            "bom_review",
            "fp_get",
            "fp_search",
            "pcb_apply_plan",
            "pcb_autoroute_plan",
            "pcb_close",
            "pcb_get_footprint",
            "pcb_info",
            "pcb_list_footprints",
            "pcb_list_nets",
            "pcb_list_tracks",
            "pcb_move_plan",
            "pcb_open",
            "pcb_outline_plan",
            "pcb_place_plan",
            "pcb_render",
            "pcb_review",
            "pcb_route_plan",
            "pcb_run_drc",
            "pcb_save",
            "pcb_update_from_schematic_plan",
            "pcb_zone_plan",
            "sch_apply_plan",
            "sch_arrange_plan",
            "sch_create_demo_circuit",
            "sch_declutter_plan",
            "sch_describe_layout",
            "sch_draw_plan",
            "sch_edit_items_plan",
            "sch_export_netlist",
            "sch_footprint_plan",
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
            "sch_set_fields_plan",
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

/// Parts and BOM: review, set fields, check and assign footprints, export. Every plan leaves the
/// nets as they were, and each field is read back.
#[tokio::test]
async fn live_bom_and_footprints_through_mcp() {
    bom_flow("file").await;
}

#[tokio::test]
async fn live_bom_and_footprints_through_the_api() {
    bom_flow("headless").await;
}

async fn bom_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-bom-{provider}-{}", std::process::id()));
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
                "idempotency_key": format!("bom-{provider}-{n}-{step}") })
    };

    // The BOM and its review, before any part data exists.
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    assert_eq!(bom["ok"], true, "{bom}");
    let lines_before = bom["result"]["rows"].as_array().unwrap().len();
    assert!(lines_before >= 5, "{bom}");
    assert_eq!(bom["result"]["custom_fields"], json!([]), "{bom}");
    let review = mcp.call("bom_review", json!({ "session_id": id })).await;
    let codes = review["result"]["issues"].to_string();
    assert!(
        codes.contains("MISSING_MPN") && codes.contains("MISSING_SUPPLIER_PART"),
        "{review}"
    );
    assert!(!review["result"]["lookups"].as_array().unwrap().is_empty(), "{review}");

    // Fields: part numbers on the capacitors, and one resistor out of the BOM run.
    let plan = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [
                { "reference": "C1", "fields": { "MPN": "GRM21BR61A106KE19", "Manufacturer": "Murata", "LCSC": "C440198" } },
                { "reference": "C2", "fields": { "MPN": "CL10B104KB8NNNC", "Manufacturer": "Samsung", "LCSC": "C1591" } },
                { "reference": "R3", "fields": { "DNP": "yes" } },
            ] }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert_eq!(plan["result"]["counts"]["property_changes"], 7, "{plan}");
    let applied = mcp.call("sch_apply_plan", apply(plan)).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert!(
        applied["result"]["placements_checked"].as_u64().unwrap() >= 7,
        "{applied}"
    );
    mcp.call("sch_save", json!({ "session_id": id })).await;
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    let fields = bom["result"]["custom_fields"].to_string();
    assert!(
        fields.contains("MPN") && fields.contains("LCSC") && fields.contains("Manufacturer"),
        "{bom}"
    );
    assert!(
        bom["result"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["dnp"] == true && r["references"] == json!(["R3"])),
        "{bom}"
    );
    let reviewed = mcp.call("bom_review", json!({ "session_id": id })).await;
    assert!(
        !reviewed["result"]["lookups"].to_string().contains("\"C1\""),
        "C1 is complete now: {reviewed}"
    );

    // A flag or field that makes no change, and bad requests, are refused or skipped.
    let same = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [{ "reference": "C1", "fields": { "MPN": "GRM21BR61A106KE19" } }] }),
        )
        .await;
    assert!(
        same["result"]["findings"].to_string().contains("NOTHING_TO_CHANGE"),
        "{same}"
    );
    let bad = mcp
        .call("sch_set_fields_plan", json!({ "session_id": id, "edits": [{ "reference": "C1", "fields": { "Reference": "C9", "DNP": "maybe" } }] }))
        .await;
    assert_eq!(bad["result"]["can_apply"], false, "{bad}");

    // Footprints: search, read, check, assign.
    let search = mcp
        .call(
            "fp_search",
            json!({ "query": "r 0805", "library": "Resistor_SMD", "session_id": id }),
        )
        .await;
    assert_eq!(search["ok"], true, "{search}");
    assert!(
        search["result"]["footprints"]
            .to_string()
            .contains("Resistor_SMD:R_0805_2012Metric"),
        "{search}"
    );
    let footprint = mcp
        .call("fp_get", json!({ "id": "Resistor_SMD:R_0805_2012Metric" }))
        .await;
    assert_eq!(footprint["result"]["pad_count"], 2, "{footprint}");
    assert!(
        footprint["result"]["courtyard"]["width"].as_f64().unwrap() > 2.0,
        "{footprint}"
    );
    let missing = mcp.call("fp_get", json!({ "id": "Nope:Nothing" })).await;
    assert_eq!(missing["ok"], false, "{missing}");

    let check = mcp.call("sch_footprint_plan", json!({ "session_id": id })).await;
    assert_eq!(check["ok"], true, "{check}");
    println!("FOOTPRINT CHECK ({provider}): {}", check["result"]["findings"]);
    let unknown = mcp
        .call(
            "sch_footprint_plan",
            json!({ "session_id": id, "assign": [{ "reference": "R3", "footprint": "Nope:Nothing" }] }),
        )
        .await;
    assert_eq!(unknown["result"]["can_apply"], false, "{unknown}");
    let assign = mcp
        .call(
            "sch_footprint_plan",
            json!({ "session_id": id, "assign": [{ "reference": "R3", "footprint": "Resistor_SMD:R_0805_2012Metric" }] }),
        )
        .await;
    assert_eq!(assign["result"]["can_apply"], true, "{assign}");
    assert_eq!(assign["result"]["counts"]["property_changes"], 1, "{assign}");
    let applied = mcp.call("sch_apply_plan", apply(assign)).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let symbols = mcp.call("sch_list_symbols", json!({ "session_id": id })).await;
    let r3 = symbols["result"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["reference"] == "R3")
        .unwrap();
    assert_eq!(r3["footprint"], "Resistor_SMD:R_0805_2012Metric", "{r3}");
    mcp.call("sch_save", json!({ "session_id": id })).await;

    // Export: the generic CSV has the custom fields; the JLCPCB one leaves the DNP part out.
    let generic = dir.join("bom.csv");
    let exported = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": generic, "preset": "generic" }),
        )
        .await;
    assert_eq!(exported["ok"], true, "{exported}");
    let text = std::fs::read_to_string(&generic).unwrap();
    assert!(text.contains("MPN") && text.contains("GRM21BR61A106KE19"), "{text}");
    let jlc = dir.join("jlc.csv");
    let exported = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": jlc, "preset": "jlcpcb" }),
        )
        .await;
    assert_eq!(exported["ok"], true, "{exported}");
    let text = std::fs::read_to_string(&jlc).unwrap();
    assert!(
        text.contains("Designator") && text.contains("LCSC Part #") && text.contains("C440198"),
        "{text}"
    );
    assert!(!text.contains("R3"), "the DNP part is left out: {text}");
    let outside = mcp
        .call(
            "bom_export",
            json!({ "session_id": id, "output_path": std::env::temp_dir().join("elsewhere.csv") }),
        )
        .await;
    assert_eq!(outside["error"]["code"], "INVALID_ARGUMENT", "{outside}");

    let erc = mcp.call("sch_run_erc", json!({ "session_id": id })).await;
    println!("ERC after BOM tools ({provider}): {} errors", erc["result"]["errors"]);
}

/// Against a real KiCad 11 editor: review the BOM, set a part-number field on the first part and
/// read it back, then check the footprints. Open a **copy**, press "KiCad AI: connect", then run
/// with THINGSVU_GUI_BOM=1. Ctrl+Z in the editor undoes the field change.
#[tokio::test]
async fn gui_bom_demo() {
    if std::env::var("THINGSVU_GUI_BOM").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_BOM=1 with a registered KiCad editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("session_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let review = mcp.call("bom_review", json!({ "session_id": id })).await;
    println!("GUI BOM REVIEW {}", review["result"]["issues"]);
    let bom = mcp.call("bom_get", json!({ "session_id": id })).await;
    let first = bom["result"]["rows"][0]["references"][0].as_str().unwrap().to_owned();
    println!("GUI BOM first part {first}");
    let plan = mcp
        .call(
            "sch_set_fields_plan",
            json!({ "session_id": id, "edits": [{ "reference": first, "fields": { "MPN": "TEST-PART-123" } }] }),
        )
        .await;
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    let applied = mcp
        .call(
            "sch_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true,
                    "idempotency_key": format!("gui-bom-{}", std::process::id()) }),
        )
        .await;
    println!("GUI BOM APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let check = mcp.call("sch_footprint_plan", json!({ "session_id": id })).await;
    println!("GUI FOOTPRINT CHECK {}", check["result"]["findings"]);
    let closed = mcp.call("session_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: look at part {first} (field MPN is hidden); Ctrl+Z undoes it");
}

fn board_fixture(name: &str, empty_board: bool) -> std::path::PathBuf {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    let dir = std::env::temp_dir().join(format!("thingsvu-mcp-board-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(source.join("regulator.kicad_sch"), dir.join("regulator.kicad_sch")).unwrap();
    if empty_board {
        std::fs::write(
            dir.join("regulator.kicad_pcb"),
            "(kicad_pcb (version 20261018) (generator \"pcbnew\") (generator_version \"10.99\")\n)\n",
        )
        .unwrap();
    } else {
        std::fs::copy(source.join("regulator.kicad_pcb"), dir.join("regulator.kicad_pcb")).unwrap();
    }
    dir.join("regulator.kicad_pcb")
}

/// Read a board, run DRC, render it, and review it.
#[tokio::test]
async fn live_board_read_drc_render_and_review_through_mcp() {
    board_read_flow("file").await;
}

#[tokio::test]
async fn live_board_read_drc_render_and_review_through_the_api() {
    board_read_flow("headless").await;
}

async fn board_read_flow(provider: &str) {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture(&format!("read-{provider}"), false);
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": provider }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(info["ok"], true, "{info}");
    assert_eq!(info["result"]["counts"]["footprints"], 11, "{info}");
    assert!(info["result"]["counts"]["pads"].as_u64().unwrap() >= 20, "{info}");
    assert!(info["result"]["counts"]["nets"].as_u64().unwrap() >= 6, "{info}");
    assert!(info["result"]["outline"].is_null(), "{info}");
    assert_eq!(info["result"]["copper_layers"], json!(["F.Cu", "B.Cu"]), "{info}");

    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    assert_eq!(list["result"]["count"], 11, "{list}");
    let j1 = list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["reference"] == "J1")
        .unwrap();
    assert_eq!(j1["side"], "front");
    assert!((j1["at"]["x"].as_f64().unwrap() - 15.66).abs() < 0.02, "{j1}");
    assert!(
        j1["courtyard"]["width"].as_f64().unwrap() > 2.0,
        "courtyards come from the libraries: {j1}"
    );

    let u1 = mcp
        .call("pcb_get_footprint", json!({ "session_id": id, "reference": "U1" }))
        .await;
    assert_eq!(u1["result"]["pad_count"], 4, "{u1}");
    assert!(u1["result"]["pads"].to_string().contains("+3V3"), "{u1}");
    let missing = mcp
        .call("pcb_get_footprint", json!({ "session_id": id, "reference": "U9" }))
        .await;
    assert_eq!(missing["ok"], false, "{missing}");

    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let gnd = nets["result"]["nets"]["GND"]["pads"].as_array().unwrap();
    assert!(gnd.len() >= 6, "{nets}");
    let tracks = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(tracks["result"]["track_count"], 0, "{tracks}");

    let drc = mcp.call("pcb_run_drc", json!({ "session_id": id })).await;
    assert_eq!(drc["ok"], true, "{drc}");
    assert_eq!(drc["result"]["errors"], 1, "the missing outline: {drc}");
    assert!(drc["result"]["by_type"]["invalid_outline"] == 1, "{drc}");
    assert!(
        drc["result"]["unrouted"].as_u64().unwrap() >= 10,
        "nothing is routed yet: {drc}"
    );

    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 500 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");

    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert_eq!(review["ok"], true, "{review}");
    let codes = review["result"]["issues"].to_string();
    assert!(codes.contains("NO_OUTLINE"), "{review}");
    assert!(
        !codes.contains("NO_FOOTPRINT_FOR_PART"),
        "every part has its footprint: {review}"
    );
    assert!(review["result"]["schematic_compared"].is_string(), "{review}");
    assert_eq!(review["result"]["drc"]["errors"], 1, "{review}");
    println!("BOARD REVIEW ({provider}): {}", review["result"]["issues"]);

    // A file session cannot update from the schematic.
    let update = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    if provider == "file" {
        assert_eq!(update["ok"], false, "{update}");
    } else {
        assert_eq!(
            update["result"]["new_footprints"], 0,
            "the board already has everything: {update}"
        );
    }
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
}

/// An empty board is updated from its schematic: planned (nothing changes), applied, verified
/// against the netlist, saved, and read back.
#[tokio::test]
async fn live_board_is_updated_from_the_schematic() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("update", true);
    let mut mcp = Mcp::start().await;
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let before = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(before["result"]["counts"]["footprints"], 0, "{before}");
    let early = mcp.call("pcb_review", json!({ "session_id": id })).await;
    assert!(
        early["result"]["issues"].to_string().contains("NO_FOOTPRINTS"),
        "{early}"
    );

    let plan = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["can_apply"], true, "{plan}");
    assert_eq!(plan["result"]["new_footprints"], 11, "{plan}");
    assert!(plan["result"]["report"].as_str().unwrap().contains("Add R3"), "{plan}");
    // Planning changed nothing.
    let still = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(still["result"]["counts"]["footprints"], 0, "{still}");

    let key = format!("pcb-update-{}", std::process::id());
    let refused = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": false, "idempotency_key": key }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "INVALID_ARGUMENT", "{refused}");
    let applied = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key }),
        )
        .await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["footprints_on_board"], 11, "{applied}");
    let again = mcp
        .call(
            "pcb_apply_plan",
            json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": format!("{key}-2") }),
        )
        .await;
    assert_eq!(again["ok"], false, "a plan is applied once: {again}");

    // The live board has them; the file does not until it is saved.
    let after = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert_eq!(after["result"]["counts"]["footprints"], 11, "{after}");
    assert_eq!(after["result"]["status"]["unsaved_changes"], true, "{after}");
    let close = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(close["ok"], false, "unsaved changes refuse a close: {close}");
    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let replanned = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    assert_eq!(
        replanned["result"]["new_footprints"], 0,
        "nothing is left to add: {replanned}"
    );
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");

    // Read back from the file by a second session.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let info = mcp.call("pcb_info", json!({ "session_id": fid })).await;
    assert_eq!(info["result"]["counts"]["footprints"], 11, "{info}");
    let review = mcp.call("pcb_review", json!({ "session_id": fid })).await;
    assert!(
        !review["result"]["issues"].to_string().contains("NO_FOOTPRINT_FOR_PART"),
        "{review}"
    );
    assert!(
        !review["result"]["issues"].to_string().contains("FOOTPRINT_DIFFERS"),
        "{review}"
    );
}

/// Against a real KiCad 11 PCB editor: read the board, run DRC, review it, and plan an update from
/// the schematic (a dry run). Open a **copy** of a project in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB=1.
#[tokio::test]
async fn gui_pcb_demo() {
    if std::env::var("THINGSVU_GUI_PCB").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    println!("GUI PCB {}", opened["result"]["document"]);
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    println!("GUI PCB INFO {}", info["result"]);
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    println!("GUI PCB REVIEW {}", review["result"]["issues"]);
    println!("GUI PCB DRC {}", review["result"]["drc"]);
    let plan = mcp
        .call("pcb_update_from_schematic_plan", json!({ "session_id": id }))
        .await;
    println!("GUI PCB UPDATE PLAN {plan}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: nothing was changed");
}

/// Applies a board plan and returns the result.
async fn apply_board(mcp: &mut Mcp, id: &Value, plan: &Value, key: &str) -> Value {
    mcp.call(
        "pcb_apply_plan",
        json!({ "session_id": id, "plan_id": plan["result"]["plan_id"], "confirm": true, "idempotency_key": key }),
    )
    .await
}

fn footprint<'a>(list: &'a Value, reference: &str) -> &'a Value {
    list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["reference"] == reference)
        .unwrap_or_else(|| panic!("no {reference} in {list}"))
}

/// An outline with mounting holes, then footprints moved, turned, flipped, and locked; each plan
/// applied, read back, saved, and read again from the file.
#[tokio::test]
async fn live_board_outline_and_moves() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("edit", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-edit-{n}-{}", std::process::id());

    // A file session only reads.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let refused = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": file["result"]["session_id"] }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "FEATURE_UNAVAILABLE", "{refused}");
    mcp.call("pcb_close", json!({ "session_id": file["result"]["session_id"] }))
        .await;

    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    // 40 x 30 mm, rounded, centred on the parts, a hole in each corner.
    let plan = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 40, "height_mm": 30, "corner_radius_mm": 2,
                    "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("OUTLINE PLAN {}", plan["result"]);
    let ops = plan["result"]["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 5, "an outline and four holes: {plan}");
    assert_eq!(ops[0]["outline"]["width"], 40.0, "{plan}");
    assert_eq!(plan["result"]["after"]["outside_outline"], json!([]), "{plan}");
    let outline = ops[0]["outline"].clone();
    let applied = apply_board(&mut mcp, &id, &plan, &key("outline")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(
        applied["result"]["report"]["placed"],
        json!(["H1", "H2", "H3", "H4"]),
        "{applied}"
    );
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    assert!(
        (info["result"]["outline"]["width"].as_f64().unwrap() - 40.05).abs() < 0.02,
        "{info}"
    );
    assert_eq!(info["result"]["counts"]["footprints"], 15, "{info}");

    // A second outline is refused unless it replaces the first.
    let again = mcp.call("pcb_outline_plan", json!({ "session_id": id })).await;
    assert_eq!(again["ok"], false, "{again}");
    let replace = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "replace": true, "margin_mm": 3 }),
        )
        .await;
    assert_eq!(replace["ok"], true, "{replace}");
    assert_eq!(
        replace["result"]["operations"][0]["op"], "remove_outline_drawing",
        "{replace}"
    );

    // Moves: R3 turned and placed, C1 to the back, U1 shifted and locked.
    let before = mcp
        .call("pcb_list_footprints", json!({ "session_id": id, "include_pads": true }))
        .await;
    let u1_before = footprint(&before, "U1")["at"].clone();
    let plan = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [
                { "reference": "R3", "x_mm": 22, "y_mm": 4, "rotation_deg": 90 },
                { "reference": "C1", "side": "back" },
                { "reference": "U1", "dx_mm": 1, "dy_mm": 0, "lock": true }
            ] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("MOVE PLAN {}", plan["result"]);
    let applied = apply_board(&mut mcp, &id, &plan, &key("move")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["report"]["flipped"], json!(["C1"]), "{applied}");
    let after = mcp
        .call("pcb_list_footprints", json!({ "session_id": id, "include_pads": true }))
        .await;
    let r3 = footprint(&after, "R3");
    assert_eq!(r3["at"], json!({ "x": 22.0, "y": 4.0 }), "{r3}");
    assert_eq!(r3["rotation_deg"], 90.0, "{r3}");
    // Turned 90°: the pads are one above the other, about the new place.
    let pads = r3["pads"].as_array().unwrap();
    assert!((pads[0]["at"]["x"].as_f64().unwrap() - 22.0).abs() < 0.01, "{r3}");
    assert!((pads[1]["at"]["x"].as_f64().unwrap() - 22.0).abs() < 0.01, "{r3}");
    assert!(
        (pads[0]["at"]["y"].as_f64().unwrap() - pads[1]["at"]["y"].as_f64().unwrap()).abs() > 1.0,
        "{r3}"
    );
    assert_eq!(footprint(&after, "C1")["side"], "back");
    let u1 = footprint(&after, "U1");
    assert_eq!(u1["locked"], true, "{u1}");
    assert!(
        (u1["at"]["x"].as_f64().unwrap() - u1_before["x"].as_f64().unwrap() - 1.0).abs() < 0.011,
        "{u1}"
    );

    // A locked footprint stays unless the move allows it.
    let locked = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "U1", "dx_mm": 2, "dy_mm": 0 }] }),
        )
        .await;
    assert_eq!(locked["ok"], false, "{locked}");
    let unknown = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "U9", "x_mm": 2, "y_mm": 0 }] }),
        )
        .await;
    assert_eq!(unknown["ok"], false, "{unknown}");
    // A plan made before another change is stale.
    let stale = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "D1", "dx_mm": 1, "dy_mm": 0 }] }),
        )
        .await;
    let other = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": "D2", "dx_mm": 1, "dy_mm": 0 }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &other, &key("d2")).await["result"]["verified"],
        true
    );
    let late = apply_board(&mut mcp, &id, &stale, &key("d1")).await;
    assert_eq!(late["error"]["code"], "STALE_PLAN", "{late}");

    let drc = mcp.call("pcb_run_drc", json!({ "session_id": id })).await;
    assert_eq!(
        drc["result"]["by_type"]["invalid_outline"],
        Value::Null,
        "the outline is closed: {drc}"
    );
    println!("DRC AFTER EDITS {}", drc["result"]["by_type"]);
    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 500 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;

    // The file says the same.
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let info = mcp.call("pcb_info", json!({ "session_id": fid })).await;
    assert_eq!(info["result"]["outline"]["width"], outline["width"], "{info}");
    let list = mcp
        .call(
            "pcb_list_footprints",
            json!({ "session_id": fid, "include_pads": true }),
        )
        .await;
    assert_eq!(footprint(&list, "R3")["pads"], r3["pads"], "{list}");
    // The pads' copper is known on both sides and turned with the part: an 0603 pad is 0.8 mm wide
    // and 0.95 mm tall at 0°, so 0.95 x 0.8 at 90°.
    let copper = &r3["pads"][0]["copper"];
    assert_eq!(
        (copper["width"].as_f64(), copper["height"].as_f64()),
        (Some(0.95), Some(0.8)),
        "{r3}"
    );
    assert_eq!(r3["pads"][0]["layers"], json!(["F.Cu"]), "{r3}");
    assert_eq!(
        footprint(&list, "H4")["library_id"],
        "MountingHole:MountingHole_3.2mm_M3"
    );
    assert_eq!(footprint(&list, "U1")["locked"], true);
}

/// Against a real KiCad 11 PCB editor: an outline around the parts and one part turned, each one
/// undo step. Open a **copy** of a project in the PCB Editor, press "KiCad AI: connect" there,
/// then run with THINGSVU_GUI_PCB_EDIT=1.
#[tokio::test]
async fn gui_pcb_edit_demo() {
    if std::env::var("THINGSVU_GUI_PCB_EDIT").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_EDIT=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let key = |n: &str| format!("gui-pcb-edit-{n}-{}", std::process::id());
    let plan = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "margin_mm": 3, "corner_radius_mm": 1, "replace": true }),
        )
        .await;
    println!("GUI OUTLINE PLAN {plan}");
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("outline")).await;
    println!("GUI OUTLINE APPLY {applied}");
    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    let first = list["result"]["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["locked"] == false)
        .expect("an unlocked footprint")["reference"]
        .clone();
    let plan = mcp
        .call(
            "pcb_move_plan",
            json!({ "session_id": id, "moves": [{ "reference": first, "rotate_by_deg": 90 }] }),
        )
        .await;
    println!("GUI MOVE PLAN {plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("move")).await;
    println!("GUI MOVE APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: {first} is turned and the board has an outline; Ctrl+Z twice undoes both");
}

/// The regulator board as KiCad leaves it after an update from the schematic (footprints stacked
/// near the origin): an outline with holes, then every footprint placed, applied, read back,
/// reviewed, and saved.
#[tokio::test]
async fn live_board_is_placed() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("place", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-place-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();

    let early = mcp.call("pcb_place_plan", json!({ "session_id": id })).await;
    assert_eq!(early["ok"], false, "no outline yet: {early}");

    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "corner_radius_mm": 1.5,
                    "origin": { "x_mm": 100, "y_mm": 80 }, "mounting_holes": "corners", "hole_inset_mm": 3.5 }),
        )
        .await;
    assert_eq!(outline["ok"], true, "{outline}");
    let applied = apply_board(&mut mcp, &id, &outline, &key("outline")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");

    let plan = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    let measured = &plan["result"]["measured"];
    println!("PLACE MEASURED {measured}");
    println!("PLACE WARNINGS {}", plan["result"]["warnings"]);
    assert_eq!(measured["main_ic"], "U1", "{measured}");
    assert_eq!(measured["after"]["overlapping_pairs"], json!([]), "{measured}");
    assert_eq!(measured["after"]["outside_outline"], json!([]), "{measured}");
    assert_eq!(
        measured["before"]["outside_outline"].as_array().unwrap().len(),
        11,
        "everything starts clustered at the origin, away from the outline: {measured}"
    );
    for h in ["H1", "H2", "H3", "H4"] {
        assert!(measured["fixed"].to_string().contains(h), "holes stay: {measured}");
    }
    assert_eq!(
        plan["result"]["operations"].as_array().unwrap().len(),
        11,
        "every part moves: {plan}"
    );
    let applied = apply_board(&mut mcp, &id, &plan, &key("place")).await;
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(
        applied["result"]["measured_after"]["overlapping_pairs"],
        json!([]),
        "{applied}"
    );

    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    let codes = review["result"]["issues"].to_string();
    println!("REVIEW AFTER PLACING {}", review["result"]["issues"]);
    for code in [
        "FOOTPRINTS_OVERLAP",
        "OUTSIDE_OUTLINE",
        "DECOUPLING_FAR",
        "CONNECTOR_FAR_FROM_EDGE",
        "NO_OUTLINE",
    ] {
        assert!(!codes.contains(code), "{code}: {review}");
    }
    let drc = &review["result"]["drc"];
    println!("DRC AFTER PLACING {}", drc["by_type"]);
    assert_eq!(drc["by_type"]["courtyards_overlap"], Value::Null, "{drc}");
    assert_eq!(drc["by_type"]["invalid_outline"], Value::Null, "{drc}");
    let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    let j1 = footprint(&list, "J1");
    assert!(
        j1["courtyard"]["min"]["x"].as_f64().unwrap() < 101.0,
        "J1 on the left edge: {j1}"
    );

    let render = mcp
        .call("pcb_render", json!({ "session_id": id, "width": 800, "height": 600 }))
        .await;
    assert_eq!(render["ok"], true, "{render}");
    if let Ok(dir) = std::env::var("THINGSVU_SAVE_RENDER") {
        use base64::Engine;
        let response = mcp
            .request(
                "tools/call",
                json!({ "name": "pcb_render", "arguments": { "session_id": id, "width": 1200, "height": 900 } }),
            )
            .await;
        let data = response["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "image")
            .unwrap()["data"]
            .as_str()
            .unwrap()
            .to_owned();
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        std::fs::write(std::path::Path::new(&dir).join("placed-board.png"), bytes).unwrap();
    }

    // Placing again changes little: the board is already placed.
    let again = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    println!(
        "PLACE AGAIN {} operations",
        again["result"]["operations"].as_array().map(|a| a.len()).unwrap_or(0)
    );

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id, "discard_unsaved": true }))
        .await;
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let review = mcp.call("pcb_review", json!({ "session_id": fid })).await;
    assert!(
        !review["result"]["issues"].to_string().contains("FOOTPRINTS_OVERLAP"),
        "{review}"
    );
}

/// Against a real KiCad 11 PCB editor: every unlocked footprint placed inside the outline, one undo
/// step. Open a **copy** of a project that has an outline in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB_PLACE=1.
#[tokio::test]
async fn gui_pcb_place_demo() {
    if std::env::var("THINGSVU_GUI_PCB_PLACE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_PLACE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    // A board whose outline was deleted (select all, delete) gets one sized from the parts: about
    // three times their courtyard area, 4:3, centred on them.
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    if info["result"]["outline"].is_null() {
        let list = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
        let area: f64 = list["result"]["footprints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| Some(f["courtyard"]["width"].as_f64()? * f["courtyard"]["height"].as_f64()?))
            .sum();
        let width = ((area * 3.0 * 4.0 / 3.0).sqrt().max(20.0) / 5.0).ceil() * 5.0;
        let height = ((width * 3.0 / 4.0) / 5.0).ceil() * 5.0;
        let outline = mcp
            .call(
                "pcb_outline_plan",
                json!({ "session_id": id, "width_mm": width, "height_mm": height, "corner_radius_mm": 1 }),
            )
            .await;
        println!(
            "GUI NO OUTLINE: adding {width} x {height} mm: {}",
            outline["result"]["operations"]
        );
        assert_eq!(outline["ok"], true, "{outline}");
        let applied = apply_board(
            &mut mcp,
            &id,
            &outline,
            &format!("gui-pcb-outline-{}", std::process::id()),
        )
        .await;
        assert_eq!(applied["result"]["verified"], true, "{applied}");
    }
    let plan = mcp.call("pcb_place_plan", json!({ "session_id": id })).await;
    println!("GUI PLACE PLAN {}", plan["result"]["measured"]);
    println!("GUI PLACE WARNINGS {}", plan["result"]["warnings"]);
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-place-{}", std::process::id())).await;
    println!("GUI PLACE APPLY {applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let review = mcp.call("pcb_review", json!({ "session_id": id })).await;
    println!("GUI REVIEW AFTER PLACING {}", review["result"]["issues"]);
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the footprints are placed; Ctrl+Z undoes it (and once more the outline, if one was added)");
}

/// The placed regulator board gets tracks: every two-pad net routed pad to pad where a simple path
/// is clear, and one route through a via; DRC before and after, read back, saved.
#[tokio::test]
async fn live_board_is_routed() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("route", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-route-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    // Refusals: two nets, an unknown pad, no layer in common.
    let short = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "U1.1" }, { "pad": "U1.2" }] }] }),
        )
        .await;
    assert_eq!(short["ok"], false, "{short}");
    assert!(short["error"]["message"].as_str().unwrap().contains("short"), "{short}");
    let unknown = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "U1.1" }, { "pad": "U9.2" }] }] }),
        )
        .await;
    assert_eq!(unknown["ok"], false, "{unknown}");

    // Every net with two pads, pad to pad; keep the ones a simple path can do.
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routable = Vec::new();
    for (name, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() != 2 {
            continue;
        }
        let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
        let one = mcp
            .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
            .await;
        println!(
            "ROUTE {name}: {}",
            if one["ok"] == true {
                one["result"]["measured"]["routes"][0].to_string()
            } else {
                one["error"]["message"].to_string()
            }
        );
        if one["ok"] == true {
            routable.push(route);
        }
    }
    assert!(!routable.is_empty(), "at least one two-pad net has a simple path");

    // One route through a via: from a pad of the first routable net down on the back and back up.
    let all = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routable }))
        .await;
    assert_eq!(all["ok"], true, "{all}");
    let tracks: usize = all["result"]["measured"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["segments"].as_u64().unwrap() as usize)
        .sum();
    let applied = apply_board(&mut mcp, &id, &all, &key("routes")).await;
    println!("ROUTE APPLY {}", applied["result"]["drc"]);
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let drc = &applied["result"]["drc"];
    assert!(
        drc["unrouted_after"].as_u64().unwrap() < drc["unrouted_before"].as_u64().unwrap(),
        "{drc}"
    );
    assert!(
        drc["errors_after"].as_u64().unwrap() <= drc["errors_before"].as_u64().unwrap(),
        "{drc}"
    );

    // A via route on GND: a free spot on the front, down to the back, along, and back up.
    let info = mcp.call("pcb_info", json!({ "session_id": id })).await;
    let o = &info["result"]["outline"];
    let (x0, y0) = (o["min"]["x"].as_f64().unwrap(), o["min"]["y"].as_f64().unwrap());
    let (x1, y1) = (o["max"]["x"].as_f64().unwrap(), o["max"]["y"].as_f64().unwrap());
    let via_route = json!({ "net": "GND", "points": [
        { "x_mm": x1 - 4.0, "y_mm": y0 + 4.0, "layer": "F.Cu" },
        { "x_mm": x1 - 4.0, "y_mm": y0 + 7.0, "layer": "B.Cu" },
        { "x_mm": x1 - 8.0, "y_mm": y0 + 7.0, "layer": "F.Cu" },
        { "x_mm": x1 - 8.0, "y_mm": y0 + 4.0 }
    ] });
    let _ = (x0, y1);
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": [via_route] }))
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["measured"]["routes"][0]["vias"], 2, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("via")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let vias = listed["result"]["vias"].as_array().unwrap();
    assert_eq!(vias.len(), 2, "{listed}");
    assert!(
        vias.iter().all(|v| v["diameter"] == 0.6 && v["drill"] == 0.3),
        "{listed}"
    );

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;
    let file = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "file" }))
        .await;
    let fid = file["result"]["session_id"].clone();
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": fid })).await;
    assert_eq!(
        listed["result"]["track_count"].as_u64().unwrap() as usize,
        tracks + 3,
        "{listed}"
    );
}

/// Against a real KiCad 11 PCB editor: every two-pad net that a simple path can reach is routed,
/// one undo step, with DRC before and after. Open a **copy** of a placed board in the PCB Editor,
/// press "KiCad AI: connect" there, then run with THINGSVU_GUI_PCB_ROUTE=1.
#[tokio::test]
async fn gui_pcb_route_demo() {
    if std::env::var("THINGSVU_GUI_PCB_ROUTE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_ROUTE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let nets = mcp.call("pcb_list_nets", json!({ "session_id": id })).await;
    let mut routes = Vec::new();
    for (name, net) in nets["result"]["nets"].as_object().unwrap() {
        let pads = net["pads"].as_array().unwrap();
        if pads.len() != 2 || net["tracks"] != 0 {
            continue;
        }
        let route = json!({ "points": [{ "pad": pads[0] }, { "pad": pads[1] }] });
        let one = mcp
            .call("pcb_route_plan", json!({ "session_id": id, "routes": [route.clone()] }))
            .await;
        println!(
            "GUI ROUTE {name}: {}",
            if one["ok"] == true {
                "clear"
            } else {
                one["error"]["message"].as_str().unwrap_or("")
            }
        );
        if one["ok"] == true {
            routes.push(route);
        }
    }
    assert!(
        !routes.is_empty(),
        "no two-pad net has a clear simple path; place the parts first"
    );
    let plan = mcp
        .call("pcb_route_plan", json!({ "session_id": id, "routes": routes }))
        .await;
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-route-{}", std::process::id())).await;
    println!(
        "GUI ROUTE APPLY verified {} unrouted {} -> {}, errors {} -> {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"]
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the tracks are on the board; Ctrl+Z undoes them");
}

/// The placed and partly routed regulator board gets a ground pour on both layers: planned,
/// created, refilled, read back, and checked with DRC; a second pour is refused unless it replaces.
#[tokio::test]
async fn live_board_gets_a_ground_pour() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let pcb = board_fixture("zone", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-zone-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "corner_radius_mm": 1,
                    "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );

    let plan = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND" }] }),
        )
        .await;
    assert_eq!(plan["ok"], true, "{plan}");
    println!("ZONE PLAN {}", plan["result"]["measured"]);
    let zone = &plan["result"]["measured"]["zones"][0];
    assert_eq!(zone["layers"], json!(["F.Cu", "B.Cu"]), "{plan}");
    assert!(
        zone["pads_inside"].as_array().unwrap().len() >= 6,
        "every GND pad is inside: {plan}"
    );
    assert_eq!(plan["result"]["operations"][0]["clearance"], 0.2, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &key("gnd")).await;
    println!(
        "ZONE APPLY {} {}",
        applied["result"]["report"], applied["result"]["drc"]["after"]["by_type"]
    );
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    assert_eq!(applied["result"]["report"]["refilled"], true, "{applied}");
    let drc = &applied["result"]["drc"];
    assert!(
        drc["unrouted_after"].as_u64().unwrap() < drc["unrouted_before"].as_u64().unwrap(),
        "the pour connects GND: {drc}"
    );
    let unrouted_gnd = drc["after"]["unrouted_items"].to_string().matches("[GND]").count();
    assert_eq!(unrouted_gnd, 0, "no GND pad is left unconnected: {drc}");

    // A second GND pour is refused unless it replaces the first.
    let again = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "layers": ["B.Cu"] }] }),
        )
        .await;
    assert_eq!(again["ok"], false, "{again}");
    let replace = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "layers": ["B.Cu"], "replace": true, "name": "GND back" }] }),
        )
        .await;
    assert_eq!(replace["ok"], true, "{replace}");
    assert_eq!(replace["result"]["operations"][0]["op"], "remove_zone", "{replace}");
    let applied = apply_board(&mut mcp, &id, &replace, &key("replace")).await;
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let listed = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    let zones = listed["result"]["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 1, "{listed}");
    assert_eq!(zones[0]["name"], "GND back", "{listed}");
    let bad = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "NOPE" }] }),
        )
        .await;
    assert_eq!(bad["ok"], false, "{bad}");

    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    mcp.call("pcb_close", json!({ "session_id": id })).await;
    let text = std::fs::read_to_string(&pcb).unwrap();
    assert!(
        text.contains("\"GND back\"") && text.contains("filled_polygon"),
        "the pour is saved filled"
    );
    assert!(text.contains("(thermal_gap 0.5)") && text.contains("(min_thickness 0.25)"));
}

/// Against a real KiCad 11 PCB editor: a ground pour over the whole board on every copper layer.
/// Open a **copy** of a placed board in the PCB Editor, press "KiCad AI: connect" there, then run
/// with THINGSVU_GUI_PCB_ZONE=1.
#[tokio::test]
async fn gui_pcb_zone_demo() {
    if std::env::var("THINGSVU_GUI_PCB_ZONE").as_deref() != Ok("1") {
        eprintln!("skipped: set THINGSVU_GUI_PCB_ZONE=1 with a registered KiCad PCB editor");
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let plan = mcp
        .call(
            "pcb_zone_plan",
            json!({ "session_id": id, "zones": [{ "net": "GND", "replace": true }] }),
        )
        .await;
    println!("GUI ZONE PLAN {plan}");
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(&mut mcp, &id, &plan, &format!("gui-pcb-zone-{}", std::process::id())).await;
    println!(
        "GUI ZONE APPLY verified {} unrouted {} -> {}, errors {} -> {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"]
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: GND is poured; Ctrl+Z undoes the fill, a second Ctrl+Z the zone");
}

/// The placed regulator board routed by Freerouting: planned (export, route, summary; nothing
/// changes), imported, read back, and checked with DRC. Needs KICAD_LIVE=1 and
/// THINGSVU_FREEROUTING_JAR (and THINGSVU_JAVA unless java is on the PATH).
#[tokio::test]
async fn live_board_is_autorouted() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") || std::env::var("THINGSVU_FREEROUTING_JAR").is_err() {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_FREEROUTING_JAR");
        return;
    }
    let pcb = board_fixture("autoroute", false);
    let mut mcp = Mcp::start().await;
    let key = |n: &str| format!("pcb-autoroute-{n}-{}", std::process::id());
    let opened = mcp
        .call("pcb_open", json!({ "pcb_path": pcb, "provider": "headless" }))
        .await;
    let id = opened["result"]["session_id"].clone();
    let outline = mcp
        .call(
            "pcb_outline_plan",
            json!({ "session_id": id, "width_mm": 36, "height_mm": 28, "origin": { "x_mm": 100, "y_mm": 80 } }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &outline, &key("outline")).await["result"]["verified"],
        true
    );
    let place = mcp
        .call(
            "pcb_place_plan",
            json!({ "session_id": id, "edge": [{ "reference": "J1", "edge": "left" }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &place, &key("place")).await["result"]["verified"],
        true
    );
    // One track by hand, which the import will replace.
    let route = mcp
        .call(
            "pcb_route_plan",
            json!({ "session_id": id, "routes": [{ "points": [{ "pad": "J1.1" }, { "pad": "F1.1" }] }] }),
        )
        .await;
    assert_eq!(
        apply_board(&mut mcp, &id, &route, &key("route")).await["result"]["verified"],
        true
    );
    let before = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;

    let bad = mcp
        .call("pcb_autoroute_plan", json!({ "session_id": id, "passes": 0 }))
        .await;
    assert_eq!(bad["ok"], false, "{bad}");
    let plan = mcp
        .call(
            "pcb_autoroute_plan",
            json!({ "session_id": id, "passes": 20, "time_limit_s": 180 }),
        )
        .await;
    println!(
        "AUTOROUTE PLAN router {} session {} replaces {} warnings {}",
        plan["result"]["router"],
        plan["result"]["session"]["segments"],
        plan["result"]["replaces"],
        plan["result"]["warnings"]
    );
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(plan["result"]["router"]["unrouted"], 0, "{plan}");
    assert!(plan["result"]["session"]["segments"].as_u64().unwrap() > 10, "{plan}");
    assert_eq!(plan["result"]["replaces"]["tracks"], 1, "the hand-made track: {plan}");
    // Planning changed nothing.
    let tracks = mcp.call("pcb_list_tracks", json!({ "session_id": id })).await;
    assert_eq!(tracks["result"]["track_count"], 1, "{tracks}");

    let applied = apply_board(&mut mcp, &id, &plan, &key("import")).await;
    println!(
        "AUTOROUTE APPLY verified {} drc {} problems {}",
        applied["result"]["verified"],
        json!({ "unrouted": [applied["result"]["drc"]["unrouted_before"], applied["result"]["drc"]["unrouted_after"]], "errors": [applied["result"]["drc"]["errors_before"], applied["result"]["drc"]["errors_after"]], "by_type": applied["result"]["drc"]["after"]["by_type"] }),
        applied["result"]["problems"]
    );
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["result"]["drc"]["unrouted_after"], 0, "{applied}");
    let after = mcp.call("pcb_list_footprints", json!({ "session_id": id })).await;
    assert_eq!(
        before["result"]["footprints"], after["result"]["footprints"],
        "no footprint moved"
    );
    assert_eq!(applied["result"]["verified"], true, "{applied}");
    let again = apply_board(&mut mcp, &id, &plan, &key("again")).await;
    assert_eq!(again["ok"], false, "a plan is applied once: {again}");
    let saved = mcp.call("pcb_save", json!({ "session_id": id })).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let text = std::fs::read_to_string(&pcb).unwrap();
    assert!(text.matches("(segment").count() > 10, "the routes are saved");
}

/// Against a real KiCad 11 PCB editor: the board routed by Freerouting, after a plan that shows
/// the router's result. Open a **copy** of a placed board in the PCB Editor, press "KiCad AI:
/// connect" there, then run with THINGSVU_GUI_PCB_AUTOROUTE=1 and THINGSVU_FREEROUTING_JAR.
#[tokio::test]
async fn gui_pcb_autoroute_demo() {
    if std::env::var("THINGSVU_GUI_PCB_AUTOROUTE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set THINGSVU_GUI_PCB_AUTOROUTE=1 and THINGSVU_FREEROUTING_JAR with a registered KiCad PCB editor"
        );
        return;
    }
    let mut mcp = Mcp::start().await;
    let opened = mcp.call("pcb_open", json!({ "provider": "gui" })).await;
    assert_eq!(opened["ok"], true, "{opened}");
    let id = opened["result"]["session_id"].clone();
    let plan = mcp.call("pcb_autoroute_plan", json!({ "session_id": id })).await;
    println!(
        "GUI AUTOROUTE PLAN router {} session {} replaces {} warnings {}",
        plan["result"]["router"]["unrouted"],
        plan["result"]["session"]["segments"],
        plan["result"]["replaces"],
        plan["result"]["warnings"]
    );
    assert_eq!(plan["ok"], true, "{plan}");
    let applied = apply_board(
        &mut mcp,
        &id,
        &plan,
        &format!("gui-pcb-autoroute-{}", std::process::id()),
    )
    .await;
    println!(
        "GUI AUTOROUTE APPLY verified {} unrouted {} -> {}, errors {} -> {}, problems {}",
        applied["result"]["verified"],
        applied["result"]["drc"]["unrouted_before"],
        applied["result"]["drc"]["unrouted_after"],
        applied["result"]["drc"]["errors_before"],
        applied["result"]["drc"]["errors_after"],
        applied["result"]["problems"]
    );
    let closed = mcp.call("pcb_close", json!({ "session_id": id })).await;
    assert_eq!(closed["ok"], true, "{closed}");
    println!("GUI DONE: the board is routed; Ctrl+Z undoes KiCad's import");
}
