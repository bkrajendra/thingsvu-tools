// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless sessions against the installed KiCad nightly. Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;
use std::time::Duration;

use kicad_domain::{Capability, ErrorCode};
use kicad_session::{OpenTarget, Session, SessionConfig};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-session-live-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_copy(dir: &std::path::Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/empty-schematic");
    for file in ["kicad-demo.kicad_sch", "kicad-demo.kicad_pro"] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    dir.join("kicad-demo.kicad_sch")
}

fn process_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
}

async fn wait_gone(pid: u32) -> bool {
    for _ in 0..50 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn open_existing_and_new_then_close() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let config = SessionConfig::default();

    let dir = scratch("existing");
    let session = Session::open_headless(&config, &installs, OpenTarget::Existing(fixture_copy(&dir)))
        .await
        .unwrap();
    let status = session.status().await;
    println!("{}", serde_json::to_string_pretty(&status).unwrap());
    assert!(status.connected);
    assert!(status.kicad_version.starts_with("10.99"));
    assert_eq!(status.unsaved_changes, Some(false));
    for c in [
        Capability::SchPlaceSymbol,
        Capability::SchEditItems,
        Capability::SchNetlist,
        Capability::SchErc,
        Capability::DocumentLifecycle,
    ] {
        assert!(
            session.capabilities().is_available(c),
            "{c:?}: {:?}",
            session.capabilities().get(c)
        );
    }
    let pid = status.server_pid.unwrap();
    assert!(!session.close(false).await.unwrap(), "nothing was discarded");
    assert!(wait_gone(pid).await, "server {pid} still running after close");

    let dir = scratch("new");
    let target = dir.join("fresh.kicad_sch");
    let session = Session::open_headless(&config, &installs, OpenTarget::New(target.clone()))
        .await
        .unwrap();
    assert!(session.status().await.connected);
    assert!(!target.exists(), "a new document is not written until saved");
    session.close(false).await.unwrap();
}

#[tokio::test]
async fn refuses_a_document_open_in_an_editor() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = scratch("locked");
    let sch = fixture_copy(&dir);
    std::fs::write(dir.join("~kicad-demo.kicad_sch.lck"), b"{}").unwrap();
    let installs = kicad_cli_runner::discover(&[]).await;
    let err = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::Existing(sch))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::DocumentOpenElsewhere);
}

/// Child half of `server_dies_with_a_crashed_owner`: opens a session, reports the server PID,
/// then aborts without any cleanup.
#[tokio::test]
async fn crash_child() {
    if std::env::var("THINGSVU_CRASH_CHILD").is_err() {
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = scratch("crash");
    let session = Session::open_headless(
        &SessionConfig::default(),
        &installs,
        OpenTarget::New(dir.join("c.kicad_sch")),
    )
    .await
    .unwrap();
    println!("SERVER_PID={}", session.status().await.server_pid.unwrap());
    std::process::abort();
}

#[tokio::test]
async fn server_dies_with_a_crashed_owner() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads", "1"])
        .env("THINGSVU_CRASH_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "child should have aborted");
    let pid: u32 = stdout
        .lines()
        .find_map(|l| l.split_once("SERVER_PID=").map(|(_, pid)| pid.trim()))
        .unwrap_or_else(|| panic!("no SERVER_PID in child output: {stdout}"))
        .parse()
        .unwrap();
    assert!(wait_gone(pid).await, "KiCad server {pid} outlived its crashed owner");
}
