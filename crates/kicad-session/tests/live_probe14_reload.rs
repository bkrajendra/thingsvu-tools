// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 14.5 probe: does `RevertDocument` on a headless board pick up a change made to the file
//! on disk after the session opened? Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::common::commands::{
    GetDocumentModifiedState, GetDocumentModifiedStateResponse, RevertDocument,
};
use kicad_session::{BoardSession, SessionConfig};

#[tokio::test]
async fn does_a_revert_read_a_file_that_changed_on_disk() {
    if std::env::var("KICAD_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe14r-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    for f in ["regulator.kicad_pcb", "regulator.kicad_sch"] {
        std::fs::copy(root.join(f), dir.join(f)).unwrap();
    }
    let pcb = dir.join("regulator.kicad_pcb");
    let board = BoardSession::open_headless(&SessionConfig::default(), &installs, &pcb)
        .await
        .unwrap();
    let before = board.read_detail().await.unwrap();
    println!("PROBE before: {} drawings", before.shapes.len());

    // Another program adds a line to the file.
    let text = std::fs::read_to_string(&pcb).unwrap();
    let cut = text.rfind(')').unwrap();
    let edited = format!(
        "{}\t(gr_line (start 1 1) (end 9 9) (stroke (width 0.1) (type solid)) (layer \"Edge.Cuts\") (uuid \"11111111-1111-4111-8111-111111111111\"))\n)\n",
        &text[..cut]
    );
    std::fs::write(&pcb, edited).unwrap();
    let still = board.read_detail().await.unwrap();
    println!(
        "PROBE after the file changed (session not reloaded): {} drawings",
        still.shapes.len()
    );

    let document = board.document_specifier().await.unwrap();
    let r: Result<Option<prost_types::Any>, _> = board
        .raw_call_any(&RevertDocument {
            document: Some(document.clone()),
        })
        .await;
    println!("PROBE RevertDocument {r:?}");
    let after = board.read_detail().await.unwrap();
    println!("PROBE after RevertDocument: {} drawings", after.shapes.len());
    let state: GetDocumentModifiedStateResponse = board
        .raw_call(&GetDocumentModifiedState {
            document: Some(document),
        })
        .await
        .unwrap();
    println!("PROBE state {:?}", state.state);
}
