// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 14.8 probe: what KiCad writes into the Specctra DSN for tracks that are already on the
//! board and for net classes. Needs a routed board in `THINGSVU_PROBE_BOARD`. Skipped unless
//! `KICAD_LIVE=1`.

use std::path::PathBuf;
use std::time::Duration;

#[tokio::test]
async fn what_the_dsn_holds_for_existing_tracks() {
    let (Ok("1"), Ok(board)) = (
        std::env::var("KICAD_LIVE").as_deref(),
        std::env::var("THINGSVU_PROBE_BOARD"),
    ) else {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_PROBE_BOARD (a routed .kicad_pcb)");
        return;
    };
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs
        .iter()
        .filter(|i| i.version.series() >= 11)
        .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
        .expect("a KiCad 11 series install");
    let dir = std::env::temp_dir().join(format!("thingsvu-probe14dsn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dsn = dir.join("board.dsn");
    kicad_cli_runner::pcb::export_specctra(cli, &PathBuf::from(board), &dsn, Duration::from_secs(120))
        .await
        .unwrap();
    let text = std::fs::read_to_string(&dsn).unwrap();
    println!("DSN bytes {}", text.len());
    for key in ["(resolution", "(wiring", "(network", "(class"] {
        if let Some(i) = text.find(key) {
            let end = (i + 1400).min(text.len());
            println!("---- {key}\n{}", &text[i..end]);
        } else {
            println!("---- {key}: none");
        }
    }
    if let Some(i) = text.find("(wiring") {
        println!("WIRING SECTION\n{}", &text[i..(i + 2500).min(text.len())]);
    }
    println!("DSN written to {}", dsn.display());
}

/// Does the session Freerouting writes contain the wires the DSN marks `protect`? Needs
/// `THINGSVU_PROBE_BOARD` and a configured router (`THINGSVU_CONFIG` or the installer's file).
#[tokio::test]
async fn does_the_session_keep_protected_wires() {
    let (Ok("1"), Ok(board)) = (
        std::env::var("KICAD_LIVE").as_deref(),
        std::env::var("THINGSVU_PROBE_BOARD"),
    ) else {
        eprintln!("skipped");
        return;
    };
    let config = kicad_session::registry::config_file();
    let Some(router) = kicad_cli_runner::freerouting::Freerouting::configured(None, None, Some(&config)) else {
        eprintln!("skipped: no router configured");
        return;
    };
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs
        .iter()
        .filter(|i| i.version.series() >= 11)
        .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
        .unwrap();
    let dir = std::env::temp_dir().join(format!("thingsvu-probe14ses-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (dsn, ses) = (dir.join("board.dsn"), dir.join("board.ses"));
    kicad_cli_runner::pcb::export_specctra(cli, &PathBuf::from(board), &dsn, Duration::from_secs(120))
        .await
        .unwrap();
    let run = router.route(&dsn, &ses, 10, Duration::from_secs(120)).await.unwrap();
    println!("ROUTER {run:?}");
    let text = std::fs::read_to_string(&ses).unwrap();
    let summary = kicad_sexpr::ses::summarize(&text).unwrap();
    for n in &summary.nets {
        println!(
            "SES net {} segments {} vias {} layers {:?}",
            n.net, n.segments, n.vias, n.layers
        );
    }
    println!("SES wires total {}", text.matches("(wire").count());
}

/// What the router does with a DSN edited to leave a net alone, or to start from nothing.
#[tokio::test]
async fn how_the_router_reads_edited_dsn() {
    let (Ok("1"), Ok(board)) = (
        std::env::var("KICAD_LIVE").as_deref(),
        std::env::var("THINGSVU_PROBE_BOARD"),
    ) else {
        eprintln!("skipped");
        return;
    };
    let config = kicad_session::registry::config_file();
    let Some(router) = kicad_cli_runner::freerouting::Freerouting::configured(None, None, Some(&config)) else {
        eprintln!("skipped: no router configured");
        return;
    };
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs
        .iter()
        .filter(|i| i.version.series() >= 11)
        .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
        .unwrap();
    let dir = std::env::temp_dir().join(format!("thingsvu-probe14edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dsn = dir.join("board.dsn");
    kicad_cli_runner::pcb::export_specctra(cli, &PathBuf::from(board), &dsn, Duration::from_secs(120))
        .await
        .unwrap();
    let original = std::fs::read_to_string(&dsn).unwrap();
    let show = |label: &str, text: &str| {
        let s = kicad_sexpr::ses::summarize(text).unwrap();
        println!(
            "{label}: {}",
            s.nets
                .iter()
                .map(|n| format!("{} {}s/{}v", n.net, n.segments, n.vias))
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    for (label, edited) in [
        (
            "GND left alone",
            kicad_sexpr::dsn::without_pins_of(&original, &["GND".to_owned()]).0,
        ),
        (
            "everything ripped up",
            kicad_sexpr::dsn::without_wiring(&original, |_| false).0,
        ),
        (
            "only +3V3 and +5V routed",
            kicad_sexpr::dsn::without_pins_of(
                &original,
                &[
                    "GND".to_owned(),
                    "/VBUS".to_owned(),
                    "/VBUS_F".to_owned(),
                    "Net-(D2-A)".to_owned(),
                ],
            )
            .0,
        ),
    ] {
        let (d, s) = (dir.join("e.dsn"), dir.join("e.ses"));
        std::fs::write(&d, &edited).unwrap();
        let _ = std::fs::remove_file(&s);
        match router.route(&d, &s, 10, Duration::from_secs(120)).await {
            Ok(run) => {
                println!(
                    "ROUTER {label}: unrouted {:?} violations {:?}",
                    run.unrouted, run.violations
                );
                show(label, &std::fs::read_to_string(&s).unwrap());
            }
            Err(e) => println!("ROUTER {label}: ERROR {e}"),
        }
    }
}
