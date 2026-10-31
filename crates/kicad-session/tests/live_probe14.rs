// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 14.0 probe: the API commands the Phase 14 tools would use, on the pinned build. Each
//! step prints one `PROBE <name> ...` line (what the command returned, and what a read-back
//! showed), so the answer can be recorded in the API notes. Skipped unless `KICAD_LIVE=1`.

use std::fmt::Debug;
use std::path::PathBuf;

use kicad_ipc::proto::kiapi::board::commands::{
    BoardDesignRulesResponse, BoardStackupResponse, CustomRulesResponse, GetBoardDesignRules, GetBoardStackup,
    GetConnectedItems, GetCustomDesignRules, GetPadShapeAsPolygon, NetClassForNetsResponse, PadShapeAsPolygonResponse,
    SetBoardDesignRules,
};
use kicad_ipc::proto::kiapi::board::types::{BoardLayer, Net, Pad, PadStack, PadStackLayer, PadStackShape, Track, Via};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, DeleteItemsResponse,
    EndCommit, GetDocumentModifiedState, GetDocumentModifiedStateResponse, GetItems, GetItemsResponse, GetNetClasses,
    GetPageSettings, GetTextExtents, GetTitleBlockInfo, NetClassesResponse, RefreshEditor, RevertDocument,
    SetNetClassAssignments, SetNetClasses, SetPageSettings, SetTitleBlockInfo, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::project::{NetClass, NetClassBoardSettings};
use kicad_ipc::proto::kiapi::common::types::{
    Distance, FrameType, ItemHeader, KiCadObjectType, LockedState, MapMergeMode, PageSettings, Text, TitleBlockInfo,
    Vector2,
};
use kicad_session::{BoardSession, SessionConfig};
use prost::Name;
use prost_types::Any;

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn show<T: Debug, E: Debug>(name: &str, r: &Result<T, E>) {
    match r {
        Ok(v) => {
            let text = format!("{v:?}");
            let cut: String = text.chars().take(600).collect();
            println!("PROBE {name} OK {cut}");
        }
        Err(e) => println!("PROBE {name} ERR {e:?}"),
    }
}

fn mm(v: f64) -> i64 {
    (v * 1_000_000.0) as i64
}

#[tokio::test]
async fn the_phase_14_commands() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe14-{}", std::process::id()));
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
    let project = document.project.clone();
    println!("PROBE document {document:?}");
    let header = || {
        Some(ItemHeader {
            document: Some(document.clone()),
            ..Default::default()
        })
    };

    // --- project commands: net classes and assignments -------------------------------------
    let r: Result<NetClassesResponse, _> = board
        .raw_call(&GetNetClasses {
            project: project.clone(),
        })
        .await;
    show("GetNetClasses", &r);

    let power = NetClass {
        name: "Power".into(),
        priority: Some(1),
        board: Some(NetClassBoardSettings {
            track_width: Some(Distance { value_nm: mm(0.5) }),
            clearance: Some(Distance { value_nm: mm(0.25) }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let r: Result<Option<Any>, _> = board
        .raw_call_any(&SetNetClasses {
            net_classes: vec![power],
            merge_mode: MapMergeMode::MmmMerge as i32,
            project: project.clone(),
        })
        .await;
    show("SetNetClasses", &r);
    let r: Result<NetClassesResponse, _> = board
        .raw_call(&GetNetClasses {
            project: project.clone(),
        })
        .await;
    println!(
        "PROBE GetNetClasses-after names {:?}",
        r.as_ref()
            .map(|r| r.net_classes.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
    );

    let nets = board.read().await.unwrap().nets;
    println!("PROBE nets {:?}", nets);
    let net_name = nets
        .iter()
        .find(|n| n.contains("GND"))
        .or(nets.first())
        .cloned()
        .unwrap_or_default();
    let r: Result<Option<Any>, _> = board
        .raw_call_any(&SetNetClassAssignments {
            project: project.clone(),
            merge_mode: MapMergeMode::MmmMerge as i32,
            assignments: vec![kicad_ipc::proto::kiapi::common::project::NetClassAssignment {
                net: net_name.clone(),
                netclasses: vec!["Power".into()],
            }],
            pattern_assignments: vec![],
        })
        .await;
    show("SetNetClassAssignments", &r);
    let r: Result<NetClassForNetsResponse, _> = board
        .raw_call(&kicad_ipc::proto::kiapi::board::commands::GetNetClassForNets {
            net: vec![Net {
                name: net_name.clone(),
                ..Default::default()
            }],
        })
        .await;
    show("GetNetClassForNets", &r);

    // --- board design rules ---------------------------------------------------------------
    let r: Result<BoardDesignRulesResponse, _> = board
        .raw_call(&GetBoardDesignRules {
            board: Some(document.clone()),
        })
        .await;
    show("GetBoardDesignRules", &r);
    if let Ok(mut rules) = r {
        if let Some(rules) = rules.rules.as_mut() {
            if let Some(c) = rules.constraints.as_mut() {
                c.min_track_width = Some(Distance { value_nm: mm(0.15) });
            }
        }
        let set: Result<BoardDesignRulesResponse, _> = board
            .raw_call(&SetBoardDesignRules {
                board: Some(document.clone()),
                rules: rules.rules.clone(),
            })
            .await;
        show("SetBoardDesignRules", &set);
        let again: Result<BoardDesignRulesResponse, _> = board
            .raw_call(&GetBoardDesignRules {
                board: Some(document.clone()),
            })
            .await;
        println!(
            "PROBE SetBoardDesignRules-readback min_track_width {:?}",
            again
                .ok()
                .and_then(|r| r.rules)
                .and_then(|r| r.constraints)
                .and_then(|c| c.min_track_width)
        );
    }
    let r: Result<CustomRulesResponse, _> = board
        .raw_call(&GetCustomDesignRules {
            board: Some(document.clone()),
        })
        .await;
    show("GetCustomDesignRules", &r);
    let r: Result<CustomRulesResponse, _> = board
        .raw_call(&kicad_ipc::proto::kiapi::board::commands::SetCustomDesignRules {
            board: Some(document.clone()),
            rules: vec![],
        })
        .await;
    show("SetCustomDesignRules(empty)", &r);

    let r: Result<BoardStackupResponse, _> = board
        .raw_call(&GetBoardStackup {
            board: Some(document.clone()),
        })
        .await;
    show("GetBoardStackup", &r);

    // --- editor state ----------------------------------------------------------------------
    let r: Result<GetDocumentModifiedStateResponse, _> = board
        .raw_call(&GetDocumentModifiedState {
            document: Some(document.clone()),
        })
        .await;
    show("GetDocumentModifiedState(after edits)", &r);
    let r: Result<Option<Any>, _> = board
        .raw_call_any(&RefreshEditor {
            frame: FrameType::FtPcbEditor as i32,
        })
        .await;
    show("RefreshEditor", &r);

    // --- page and title block --------------------------------------------------------------
    let r: Result<TitleBlockInfo, _> = board
        .raw_call(&GetTitleBlockInfo {
            document: Some(document.clone()),
        })
        .await;
    show("GetTitleBlockInfo", &r);
    let r: Result<Option<Any>, _> = board
        .raw_call_any(&SetTitleBlockInfo {
            document: Some(document.clone()),
            title_block: Some(TitleBlockInfo {
                title: "Probe".into(),
                revision: "P1".into(),
                ..Default::default()
            }),
        })
        .await;
    show("SetTitleBlockInfo", &r);
    let r: Result<TitleBlockInfo, _> = board
        .raw_call(&GetTitleBlockInfo {
            document: Some(document.clone()),
        })
        .await;
    show("GetTitleBlockInfo-after", &r);
    let r: Result<PageSettings, _> = board
        .raw_call(&GetPageSettings {
            document: Some(document.clone()),
        })
        .await;
    show("GetPageSettings", &r);
    if let Ok(page) = r {
        let r: Result<Option<Any>, _> = board
            .raw_call_any(&SetPageSettings {
                document: Some(document.clone()),
                page_settings: Some(page),
            })
            .await;
        show("SetPageSettings", &r);
    }

    // --- text extents and pad shapes -------------------------------------------------------
    let r: Result<kicad_ipc::proto::kiapi::common::types::Box2, _> = board
        .raw_call(&GetTextExtents {
            text: Some(Text {
                position: Some(Vector2 { x_nm: 0, y_nm: 0 }),
                text: "R1 probe".into(),
                attributes: Some(kicad_ipc::proto::kiapi::common::types::TextAttributes {
                    size: Some(Vector2 {
                        x_nm: mm(1.0),
                        y_nm: mm(1.0),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })
        .await;
    show("GetTextExtents", &r);

    let pads: Vec<Pad> = {
        let r: GetItemsResponse = board
            .raw_call(&GetItems {
                header: header(),
                types: vec![KiCadObjectType::KotPcbPad as i32],
            })
            .await
            .unwrap();
        r.items
            .iter()
            .filter(|a| a.type_url == Pad::type_url())
            .filter_map(|a| a.to_msg().ok())
            .collect()
    };
    println!("PROBE pads {}", pads.len());
    if let Some(p) = pads.first() {
        let r: Result<PadShapeAsPolygonResponse, _> = board
            .raw_call(&GetPadShapeAsPolygon {
                board: Some(document.clone()),
                pads: p.id.iter().cloned().collect(),
                layer: BoardLayer::BlFCu as i32,
            })
            .await;
        show("GetPadShapeAsPolygon", &r);
    }

    // Do the rules and classes reach the files when the board is saved?
    let saved = board.save().await;
    println!("PROBE save {saved:?}");
    let pro = std::fs::read_to_string(dir.join("regulator.kicad_pro")).unwrap_or_default();
    println!(
        "PROBE saved .kicad_pro: has Power {} has min_track_width 0.15 {} ({} bytes)",
        pro.contains("\"Power\""),
        pro.contains("\"min_track_width\": 0.15"),
        pro.len()
    );
    println!(
        "PROBE saved .kicad_pro netclass_assignments {}",
        pro.contains("netclass_assignments") && pro.contains("GND")
    );

    // --- tracks and vias: create, lock, widen, delete --------------------------------------
    let begin: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
    let track = |x0: f64, x1: f64| Track {
        start: Some(Vector2 {
            x_nm: mm(x0),
            y_nm: mm(40.0),
        }),
        end: Some(Vector2 {
            x_nm: mm(x1),
            y_nm: mm(40.0),
        }),
        width: Some(Distance { value_nm: mm(0.25) }),
        layer: BoardLayer::BlFCu as i32,
        net: Some(Net {
            name: net_name.clone(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let via = Via {
        position: Some(Vector2 {
            x_nm: mm(60.0),
            y_nm: mm(40.0),
        }),
        net: Some(Net {
            name: net_name.clone(),
            ..Default::default()
        }),
        r#type: kicad_ipc::proto::kiapi::board::types::ViaType::VtThrough as i32,
        pad_stack: Some(PadStack {
            r#type: kicad_ipc::proto::kiapi::board::types::PadStackType::PstNormal as i32,
            layers: vec![BoardLayer::BlFCu as i32, BoardLayer::BlBCu as i32],
            drill: Some(kicad_ipc::proto::kiapi::board::types::DrillProperties {
                start_layer: BoardLayer::BlFCu as i32,
                end_layer: BoardLayer::BlBCu as i32,
                diameter: Some(Vector2 {
                    x_nm: mm(0.3),
                    y_nm: mm(0.3),
                }),
                ..Default::default()
            }),
            copper_layers: vec![PadStackLayer {
                layer: BoardLayer::BlFCu as i32,
                shape: PadStackShape::PssCircle as i32,
                size: Some(Vector2 {
                    x_nm: mm(0.6),
                    y_nm: mm(0.6),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let created: Result<CreateItemsResponse, _> = board
        .raw_call(&CreateItems {
            header: header(),
            items: vec![
                Any::from_msg(&track(50.0, 55.0)).unwrap(),
                Any::from_msg(&track(55.0, 60.0)).unwrap(),
                Any::from_msg(&via).unwrap(),
            ],
            container: None,
        })
        .await;
    show("CreateItems(2 tracks, 1 via)", &created);
    let _ = board
        .raw_call::<_, kicad_ipc::proto::kiapi::common::commands::EndCommitResponse>(&EndCommit {
            id: begin.id,
            action: CommitAction::CmaCommit as i32,
            message: "probe create".into(),
            header: header(),
        })
        .await;

    let read_copper = || {
        let board = &board;
        let header = header();
        async move {
            let r: GetItemsResponse = board
                .raw_call(&GetItems {
                    header,
                    types: vec![KiCadObjectType::KotPcbTrace as i32, KiCadObjectType::KotPcbVia as i32],
                })
                .await
                .unwrap();
            let tracks: Vec<Track> = r
                .items
                .iter()
                .filter(|a| a.type_url == Track::type_url())
                .filter_map(|a| a.to_msg().ok())
                .collect();
            let vias: Vec<Via> = r
                .items
                .iter()
                .filter(|a| a.type_url == Via::type_url())
                .filter_map(|a| a.to_msg().ok())
                .collect();
            (tracks, vias)
        }
    };
    let (mut tracks, vias) = read_copper().await;
    println!("PROBE created tracks {} vias {}", tracks.len(), vias.len());

    // Connected items for the first track.
    if let Some(t) = tracks.first() {
        let r: Result<GetItemsResponse, _> = board
            .raw_call(&GetConnectedItems {
                header: header(),
                items: t.id.iter().cloned().collect(),
                types: vec![],
            })
            .await;
        show(
            "GetConnectedItems",
            &r.map(|r| r.items.iter().map(|a| a.type_url.clone()).collect::<Vec<_>>()),
        );
    }

    // Re-widen and lock through UpdateItems.
    if tracks.len() >= 2 {
        tracks[0].width = Some(Distance { value_nm: mm(0.4) });
        tracks[1].locked = LockedState::LsLocked as i32;
        let begin: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
        let r: Result<UpdateItemsResponse, _> = board
            .raw_call(&UpdateItems {
                header: header(),
                items: tracks.iter().take(2).map(|t| Any::from_msg(t).unwrap()).collect(),
            })
            .await;
        show(
            "UpdateItems(width, lock)",
            &r.map(|r| r.updated_items.iter().map(|u| u.status.clone()).collect::<Vec<_>>()),
        );
        let _ = board
            .raw_call::<_, kicad_ipc::proto::kiapi::common::commands::EndCommitResponse>(&EndCommit {
                id: begin.id,
                action: CommitAction::CmaCommit as i32,
                message: "probe update".into(),
                header: header(),
            })
            .await;
        let (after, _) = read_copper().await;
        for t in &after {
            println!(
                "PROBE readback track width {:?} locked {:?}",
                t.width.as_ref().map(|w| w.value_nm),
                t.locked
            );
        }
    }

    // Delete: a locked track, a plain track, and the via.
    let (tracks, vias) = read_copper().await;
    let ids: Vec<_> = tracks
        .iter()
        .filter_map(|t| t.id.clone())
        .chain(vias.iter().filter_map(|v| v.id.clone()))
        .collect();
    let begin: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
    let r: Result<DeleteItemsResponse, _> = board
        .raw_call(&DeleteItems {
            header: header(),
            item_ids: ids,
        })
        .await;
    show(
        "DeleteItems(locked track, track, via)",
        &r.map(|r| r.deleted_items.iter().map(|d| d.status).collect::<Vec<_>>()),
    );
    let _ = board
        .raw_call::<_, kicad_ipc::proto::kiapi::common::commands::EndCommitResponse>(&EndCommit {
            id: begin.id,
            action: CommitAction::CmaCommit as i32,
            message: "probe delete".into(),
            header: header(),
        })
        .await;
    let (tracks, vias) = read_copper().await;
    println!("PROBE after delete tracks {} vias {}", tracks.len(), vias.len());

    // --- revert ----------------------------------------------------------------------------
    let r: Result<GetDocumentModifiedStateResponse, _> = board
        .raw_call(&GetDocumentModifiedState {
            document: Some(document.clone()),
        })
        .await;
    show("GetDocumentModifiedState(before revert)", &r);
    let r: Result<Option<Any>, _> = board
        .raw_call_any(&RevertDocument {
            document: Some(document.clone()),
        })
        .await;
    show("RevertDocument", &r);
    let r: Result<GetDocumentModifiedStateResponse, _> = board
        .raw_call(&GetDocumentModifiedState {
            document: Some(document.clone()),
        })
        .await;
    show("GetDocumentModifiedState(after revert)", &r);
    let nets_after = board.read().await.map(|b| (b.nets.len(), b.tracks.len(), b.vias.len()));
    println!("PROBE after revert (nets, tracks, vias) {nets_after:?}");
}
