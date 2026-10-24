// SPDX-License-Identifier: GPL-3.0-or-later
//! The plugin binary and the GUI session provider, with a headless KiCad standing in for the
//! editor: KiCad hands a plugin a socket and an instance token, and a headless server has both.
//! Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;
use std::process::Command;

use kicad_domain::{Capability, ErrorCode};
use kicad_session::registry;
use kicad_session::{OpenTarget, Provider, Session, SessionConfig};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-plugin-live-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn plugin(command: &str, socket: &str, token: &str, registry_dir: &std::path::Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_kicad-ai-plugin"))
        .arg(command)
        .env("KICAD_API_SOCKET", socket)
        .env("KICAD_API_TOKEN", token)
        .env("THINGSVU_REGISTRY_DIR", registry_dir)
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[tokio::test]
async fn plugin_registers_and_gui_session_connects() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    // A headless KiCad with the two-resistor fixture plays the editor.
    let project = scratch("project");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/two-resistor-series-chain");
    for f in ["kicad-demo.kicad_sch", "kicad-demo.kicad_pro"] {
        std::fs::copy(fixture.join(f), project.join(f)).unwrap();
    }
    let schematic = project.join("kicad-demo.kicad_sch");
    let installs = kicad_cli_runner::discover(&[]).await;
    let editor = Session::open_headless(
        &SessionConfig::default(),
        &installs,
        OpenTarget::Existing(schematic.clone()),
    )
    .await
    .unwrap();
    let socket = editor.headless_socket().await.unwrap();
    let token = editor.raw_client().await.instance_token().unwrap().to_owned();
    let env_socket = format!("ipc://{}", socket.display());

    let registry_dir = scratch("registry");
    let (ok, stdout) = plugin("register", &env_socket, &token, &registry_dir);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("ready"), "{stdout}");
    let records = registry::list(&registry_dir);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].socket_path, socket);
    assert!(
        records[0]
            .documents
            .iter()
            .any(|d| d.project.as_deref() == Some("kicad-demo")),
        "{:?}",
        records[0]
    );
    assert!(registry::is_reachable(&records[0], std::time::Duration::from_secs(3)).await);

    // The GUI provider connects through the registration and reads the document.
    let gui = Session::open_gui(&SessionConfig::default(), &installs, &records[0], Some(&schematic))
        .await
        .unwrap();
    assert_eq!(gui.provider(), Provider::Gui);
    assert_eq!(gui.document_path(), schematic.as_path());
    let refs: Vec<_> = gui
        .list_symbols()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.reference)
        .collect();
    assert_eq!(refs, ["R1", "R2"]);
    assert!(gui.capabilities().is_available(Capability::SchPlaceSymbol));
    assert!(
        !gui.capabilities().is_available(Capability::DocumentLifecycle),
        "lifecycle commands are headless-only"
    );
    assert!(
        !gui.close(false).await.unwrap(),
        "closing a GUI session discards nothing"
    );
    assert!(
        editor.status().await.connected,
        "closing the GUI session leaves KiCad running"
    );

    // Without a path, the only open schematic is chosen.
    let gui = Session::open_gui(&SessionConfig::default(), &installs, &records[0], None)
        .await
        .unwrap();
    assert_eq!(gui.document_path(), schematic.as_path());
    gui.close(false).await.unwrap();

    // A path that is not open there is refused.
    let err = Session::open_gui(
        &SessionConfig::default(),
        &installs,
        &records[0],
        Some(&project.join("other.kicad_sch")),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::DocumentNotOpen);

    // A registration carrying another instance's token is refused.
    let mut wrong = records[0].clone();
    wrong.token = "00000000-0000-0000-0000-000000000000".into();
    assert!(!registry::is_reachable(&wrong, std::time::Duration::from_secs(3)).await);
    let err = Session::open_gui(&SessionConfig::default(), &installs, &wrong, None)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::KicadInstanceChanged, "{err:?}");

    let (ok, stdout) = plugin("unregister", &env_socket, &token, &registry_dir);
    assert!(ok && stdout.contains("no longer available"), "{stdout}");
    assert!(registry::list(&registry_dir).is_empty());

    // Outside KiCad the plugin explains itself instead of failing silently.
    let out = Command::new(env!("CARGO_BIN_EXE_kicad-ai-plugin"))
        .arg("register")
        .env_remove("KICAD_API_SOCKET")
        .env_remove("KICAD_API_TOKEN")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("toolbar button"));
    editor.close(true).await.unwrap();
}
