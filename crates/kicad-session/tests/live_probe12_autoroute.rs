// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 12.7 probe: the Specctra round trip through a headless KiCad and Freerouting. Skipped
//! unless `KICAD_LIVE=1` and `THINGSVU_FREEROUTING_JAR` (with `THINGSVU_JAVA`, or java on the
//! PATH) and `THINGSVU_PROBE_BOARD` (a placed `.kicad_pcb`) are set.
//!
//! Finding: the API's DSN export job reads the board **file**, so the probe board must be saved;
//! the tool exports a snapshot with `kicad-cli pcb export specctra` instead.

use std::path::PathBuf;
use std::time::Instant;

use kicad_ipc::proto::kiapi::board::commands::{ImportSpecctraSession, ImportSpecctraSessionResponse};
use kicad_ipc::proto::kiapi::board::jobs::RunBoardJobExportSpecctraDsn;
use kicad_ipc::proto::kiapi::common::types::{RunJobResponse, RunJobSettings};
use kicad_session::{BoardSession, SessionConfig};

#[tokio::test]
async fn the_specctra_round_trip() {
    let (Ok("1"), Ok(jar), Ok(board_path)) = (
        std::env::var("KICAD_LIVE").as_deref(),
        std::env::var("THINGSVU_FREEROUTING_JAR"),
        std::env::var("THINGSVU_PROBE_BOARD"),
    ) else {
        eprintln!("skipped: set KICAD_LIVE=1, THINGSVU_FREEROUTING_JAR, and THINGSVU_PROBE_BOARD");
        return;
    };
    let java = std::env::var("THINGSVU_JAVA").unwrap_or_else(|_| "java".into());
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe12r-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = PathBuf::from(&board_path);
    for f in std::fs::read_dir(src.parent().unwrap()).unwrap().flatten() {
        std::fs::copy(f.path(), dir.join(f.file_name())).unwrap();
    }
    let board = BoardSession::open_headless(
        &SessionConfig::default(),
        &installs,
        &dir.join(src.file_name().unwrap()),
    )
    .await
    .unwrap();
    let document = board.document_specifier().await.unwrap();
    let before = board.read().await.unwrap();
    println!("BEFORE tracks {} vias {}", before.tracks.len(), before.vias.len());

    // 1. Export.
    let dsn = dir.join("board.dsn");
    let t = Instant::now();
    let exported: Result<RunJobResponse, _> = board
        .raw_call(&RunBoardJobExportSpecctraDsn {
            job_settings: Some(RunJobSettings {
                document: Some(document.clone()),
                output_path: dsn.display().to_string(),
            }),
        })
        .await;
    println!("EXPORT {:?} in {:?}", exported, t.elapsed());
    println!(
        "DSN exists {} bytes {:?}",
        dsn.is_file(),
        std::fs::metadata(&dsn).map(|m| m.len())
    );
    if let Ok(text) = std::fs::read_to_string(&dsn) {
        for line in text
            .lines()
            .filter(|l| l.contains("(rule") || l.contains("(width") || l.contains("(clearance"))
            .take(6)
        {
            println!("DSN {}", line.trim());
        }
    }

    // 2. Route.
    let ses = dir.join("board.ses");
    let t = Instant::now();
    let out = tokio::process::Command::new(&java)
        .arg("-jar")
        .arg(&jar)
        .args(["-de"])
        .arg(&dsn)
        .arg("-do")
        .arg(&ses)
        .args(["-mp", "20", "--gui.enabled=false"])
        .output()
        .await
        .unwrap();
    println!("ROUTER exit {:?} in {:?}", out.status, t.elapsed());
    let log = String::from_utf8_lossy(&out.stdout);
    for line in log
        .lines()
        .filter(|l| {
            l.contains("unrouted")
                || l.contains("completed")
                || l.contains("ERROR")
                || l.contains("WARN")
                || l.contains("pass")
        })
        .take(20)
    {
        println!("ROUTER {line}");
    }
    println!(
        "ROUTER stderr {}",
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let ses_text = std::fs::read_to_string(&ses).unwrap_or_default();
    println!(
        "SES exists {} wires {} vias {}",
        ses.is_file(),
        ses_text.matches("(wire").count(),
        ses_text.matches("(via").count()
    );
    for line in ses_text.lines().take(30) {
        println!("SES {line}");
    }

    // 3. Import.
    let t = Instant::now();
    let imported: Result<ImportSpecctraSessionResponse, _> = board
        .raw_call(&ImportSpecctraSession {
            board: Some(document.clone()),
            session_path: ses.display().to_string(),
        })
        .await;
    println!("IMPORT {:?} in {:?}", imported, t.elapsed());
    let after = board.read().await.unwrap();
    println!(
        "AFTER tracks {} vias {} footprints moved {}",
        after.tracks.len(),
        after.vias.len(),
        after
            .footprints
            .iter()
            .zip(before.footprints.iter())
            .filter(|(a, b)| a.position != b.position)
            .count()
    );
    let mut widths: std::collections::BTreeMap<i64, usize> = Default::default();
    for t in &after.tracks {
        *widths.entry(t.width.0).or_default() += 1;
    }
    println!(
        "AFTER widths {widths:?} layers {:?}",
        after
            .tracks
            .iter()
            .map(|t| t.layer.clone())
            .collect::<std::collections::BTreeSet<_>>()
    );
    println!("CLOSE {:?}", board.close(true).await);
}
