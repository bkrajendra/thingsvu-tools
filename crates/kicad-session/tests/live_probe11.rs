// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 11 probe: what the headless KiCad answers for a board. Skipped unless `KICAD_LIVE=1`.
//! With `THINGSVU_WRITE_FIXTURE=1` it also writes the imported board to the test fixtures.

use std::path::PathBuf;
use std::time::Duration;

use kicad_ipc::proto::kiapi::board::commands::{
    BoardBoundingBoxMode, BoardBoundingBoxResponse, BoardDesignRulesResponse, BoardEnabledLayersResponse,
    BoardStackupResponse, GetBoardBoundingBox, GetBoardDesignRules, GetBoardEnabledLayers, GetBoardStackup, GetNets,
    NetsResponse,
};
use kicad_ipc::proto::kiapi::board::types::{FootprintInstance, Pad, Track, Via, Zone};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType};
use kicad_session::{BoardSession, SessionConfig};
use prost::Name;

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

#[tokio::test]
async fn what_a_headless_board_answers() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe11-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-with-notes");
    std::fs::copy(root.join("regulator.kicad_sch"), dir.join("regulator.kicad_sch")).unwrap();
    std::fs::write(
        dir.join("regulator.kicad_pcb"),
        "(kicad_pcb (version 20261018) (generator \"pcbnew\") (generator_version \"10.99\")\n)\n",
    )
    .unwrap();

    // The netlist the board is updated from.
    let cli = installs.iter().rfind(|i| i.version.series() >= 11).unwrap();
    let netlist = dir.join("regulator.net");
    kicad_cli_runner::netlist::export_netlist(
        cli,
        &dir.join("regulator.kicad_sch"),
        &netlist,
        Duration::from_secs(120),
    )
    .await
    .unwrap();

    let board = BoardSession::open_headless(&SessionConfig::default(), &installs, &dir.join("regulator.kicad_pcb"))
        .await
        .unwrap();
    println!("STATUS {:?}", board.status().await);
    let dry = board.import_netlist(&netlist, true).await;
    println!("DRY RUN {dry:?}");
    let real = board.import_netlist(&netlist, false).await;
    println!("IMPORT {real:?}");
    println!("STATUS AFTER {:?}", board.status().await);

    let document = board.document_specifier().await.unwrap();
    let header = || {
        Some(ItemHeader {
            document: Some(document.clone()),
            ..Default::default()
        })
    };
    let items: Result<GetItemsResponse, _> = board
        .raw_call(&GetItems {
            header: header(),
            types: vec![
                KiCadObjectType::KotPcbFootprint as i32,
                KiCadObjectType::KotPcbTrace as i32,
                KiCadObjectType::KotPcbVia as i32,
                KiCadObjectType::KotPcbZone as i32,
                KiCadObjectType::KotPcbShape as i32,
            ],
        })
        .await;
    match items {
        Ok(r) => {
            let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
            for a in &r.items {
                *kinds.entry(a.type_url.clone()).or_default() += 1;
            }
            println!("ITEMS {kinds:#?}");
            for a in r
                .items
                .iter()
                .filter(|a| a.type_url == FootprintInstance::type_url())
                .take(3)
            {
                let f: FootprintInstance = a.to_msg().unwrap();
                let name = |x: &Option<kicad_ipc::proto::kiapi::board::types::Field>| {
                    x.as_ref()
                        .and_then(|f| f.text.as_ref())
                        .and_then(|t| t.text.as_ref())
                        .map(|t| t.text.clone())
                };
                println!(
                    "FOOTPRINT {:?} value {:?} lib {:?} at {:?} orientation {:?} layer {} locked {} definition items {} symbol_path {:?}",
                    name(&f.reference_field),
                    name(&f.value_field),
                    f.definition.as_ref().and_then(|d| d.id.clone()),
                    f.position,
                    f.orientation,
                    f.layer,
                    f.locked,
                    f.definition.as_ref().map(|d| d.items.len()).unwrap_or(0),
                    f.symbol_path
                );
                if let Some(d) = &f.definition {
                    let mut types: std::collections::BTreeMap<String, usize> = Default::default();
                    for it in &d.items {
                        *types.entry(it.type_url.clone()).or_default() += 1;
                    }
                    println!("   definition item types {types:?}");
                    for it in d.items.iter().filter(|i| i.type_url == Pad::type_url()).take(2) {
                        let p: Pad = it.to_msg().unwrap();
                        println!(
                            "   PAD {:?} net {:?} at {:?} type {}",
                            p.number, p.net, p.position, p.r#type
                        );
                    }
                }
            }
            for a in r.items.iter().filter(|a| a.type_url == Track::type_url()).take(1) {
                println!("TRACK {:?}", a.to_msg::<Track>().ok());
            }
            for a in r.items.iter().filter(|a| a.type_url == Via::type_url()).take(1) {
                println!("VIA {:?}", a.to_msg::<Via>().ok());
            }
            for a in r.items.iter().filter(|a| a.type_url == Zone::type_url()).take(1) {
                println!("ZONE {:?}", a.to_msg::<Zone>().ok().map(|z| (z.name, z.layers)));
            }
        }
        Err(e) => println!("ITEMS failed: {e}"),
    }
    let nets: Result<NetsResponse, _> = board
        .raw_call(&GetNets {
            board: Some(document.clone()),
            netclass_filter: vec![],
        })
        .await;
    println!(
        "NETS {:?}",
        nets.map(|n| n.nets.iter().map(|n| n.name.clone()).collect::<Vec<_>>())
    );
    let stackup: Result<BoardStackupResponse, _> = board
        .raw_call(&GetBoardStackup {
            board: Some(document.clone()),
        })
        .await;
    println!("STACKUP {:?}", stackup.map(|s| s.stackup.map(|s| s.layers.len())));
    let layers: Result<BoardEnabledLayersResponse, _> = board
        .raw_call(&GetBoardEnabledLayers {
            board: Some(document.clone()),
        })
        .await;
    println!("LAYERS {:?}", layers.map(|l| (l.copper_layer_count, l.layers)));
    let rules: Result<BoardDesignRulesResponse, _> = board
        .raw_call(&GetBoardDesignRules {
            board: Some(document.clone()),
        })
        .await;
    println!("RULES {:?}", rules.map(|r| r.rules.is_some()));
    let bbox: Result<BoardBoundingBoxResponse, _> = board
        .raw_call(&GetBoardBoundingBox {
            board: Some(document.clone()),
            mode: BoardBoundingBoxMode::BbbmAllItems as i32,
        })
        .await;
    println!("BBOX {bbox:?}");

    let copy = dir.join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    let saved = board.save_copy(&copy).await.unwrap();
    println!("SAVED {}", saved.display());
    if std::env::var("THINGSVU_WRITE_FIXTURE").as_deref() == Ok("1") {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::copy(&saved, target.join("regulator.kicad_pcb")).unwrap();
        std::fs::copy(dir.join("regulator.kicad_sch"), target.join("regulator.kicad_sch")).unwrap();
        println!("FIXTURE written to {}", target.display());
    }
    println!("CLOSE {:?}", board.close(true).await);
}
