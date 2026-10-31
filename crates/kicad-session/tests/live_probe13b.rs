// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 13b probe: does `UpdateItems` on a footprint write its attributes (exclude from the
//! position file and the BOM, board only, do not populate)? Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::board::types::FootprintInstance;
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, EndCommit, GetItems, GetItemsResponse, UpdateItems,
    UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType};
use kicad_session::{BoardSession, SessionConfig};
use prost::Name;
use prost_types::Any;

fn refname(f: &FootprintInstance) -> String {
    f.reference_field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|t| t.text.as_ref())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

#[tokio::test]
async fn does_an_update_write_footprint_attributes() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe13b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    for f in ["regulator.kicad_pcb", "regulator.kicad_sch"] {
        std::fs::copy(root.join(f), dir.join(f)).unwrap();
    }
    let board = BoardSession::open_headless(&SessionConfig::default(), &installs, &dir.join("regulator.kicad_pcb"))
        .await
        .unwrap();
    let document = board.document_specifier().await.unwrap();
    let header = || {
        Some(ItemHeader {
            document: Some(document.clone()),
            ..Default::default()
        })
    };
    let read = |label: &'static str| {
        let board = &board;
        async move {
            let r: GetItemsResponse = board
                .raw_call(&GetItems {
                    header: header(),
                    types: vec![KiCadObjectType::KotPcbFootprint as i32],
                })
                .await
                .unwrap();
            let items: Vec<FootprintInstance> = r
                .items
                .iter()
                .filter(|a| a.type_url == FootprintInstance::type_url())
                .map(|a| a.to_msg().unwrap())
                .collect();
            for f in &items {
                let name = refname(f);
                println!("{label}: {name:?} attributes {:?}", f.attributes);
            }
            items
        }
    };
    let before = read("before").await;
    let mut j1 = before
        .iter()
        .find(|f| refname(f).starts_with('J'))
        .cloned()
        .expect("a connector on the fixture");
    let attrs = j1.attributes.get_or_insert_with(Default::default);
    attrs.exclude_from_position_files = true;
    attrs.exclude_from_bill_of_materials = true;
    attrs.not_in_schematic = true;
    attrs.do_not_populate = true;
    let commit: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
    let r: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: header(),
            items: vec![Any::from_msg(&j1).unwrap()],
        })
        .await;
    println!(
        "UPDATE {:?}",
        r.as_ref()
            .map(|r| r.updated_items.iter().map(|u| u.status.clone()).collect::<Vec<_>>())
    );
    let _ = board
        .raw_call::<_, kicad_ipc::proto::kiapi::common::commands::EndCommitResponse>(&EndCommit {
            id: commit.id,
            action: CommitAction::CmaCommit as i32,
            message: "probe".into(),
            header: header(),
        })
        .await;
    let after = read("after").await;
    let j1_after = after.iter().find(|f| f.id == j1.id).unwrap();
    let a = j1_after.attributes.clone().unwrap_or_default();
    println!(
        "RESULT pos {} bom {} board_only {} dnp {}",
        a.exclude_from_position_files, a.exclude_from_bill_of_materials, a.not_in_schematic, a.do_not_populate
    );
}
