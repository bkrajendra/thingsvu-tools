// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 12 probe: how a headless KiCad edits a board. Skipped unless `KICAD_LIVE=1`.
//!
//! On a copy of the regulator board: a commit with an outline (`BoardGraphicShape` rectangle on
//! `Edge.Cuts`, with and without a corner radius), a mounting hole from the library, a footprint
//! moved, turned, locked, and flipped (`UpdateItems` and `FlipItems`), a track, a via, and a zone
//! with a refill; each read back from the API and from the saved file.

use std::path::PathBuf;
use std::time::Duration;

use kicad_ipc::proto::kiapi::board::commands::{
    BoardBoundingBoxMode, BoardBoundingBoxResponse, BoardFlipDirection, FlipItems, FlipItemsResponse,
    GetBoardBoundingBox, PlaceFootprintFromLibrary, RefillZones,
};
use kicad_ipc::proto::kiapi::board::types::{
    zone, BoardGraphicShape, BoardLayer, CopperZoneSettings, FootprintInstance, Net, Pad, Track, Via, Zone, ZoneType,
};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, EndCommit, GetItems,
    GetItemsResponse, LoadAllLibraries, PlaceFromLibraryResponse, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    graphic_shape, Angle, Distance, GraphicAttributes, GraphicRectangleAttributes, GraphicShape, ItemHeader,
    KiCadObjectType, LibraryIdentifier, LibraryType, LockedState, PolyLine, PolyLineNode, PolySet, PolygonWithHoles,
    StrokeAttributes, Vector2,
};
use kicad_session::{BoardSession, SessionConfig};
use prost::Name;
use prost_types::{Any, FieldMask};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn v(x_mm: f64, y_mm: f64) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: (x_mm * 1e6) as i64,
        y_nm: (y_mm * 1e6) as i64,
    })
}

fn mm(x: i64) -> f64 {
    x as f64 / 1e6
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64, radius_mm: f64) -> BoardGraphicShape {
    BoardGraphicShape {
        shape: Some(GraphicShape {
            attributes: Some(GraphicAttributes {
                stroke: Some(StrokeAttributes {
                    width: Some(Distance { value_nm: 50_000 }),
                    ..Default::default()
                }),
                fill: None,
            }),
            geometry: Some(graphic_shape::Geometry::Rectangle(GraphicRectangleAttributes {
                top_left: v(x0, y0),
                bottom_right: v(x1, y1),
                corner_radius: Some(Distance {
                    value_nm: (radius_mm * 1e6) as i64,
                }),
            })),
            ..Default::default()
        }),
        layer: BoardLayer::BlEdgeCuts as i32,
        ..Default::default()
    }
}

#[tokio::test]
async fn how_a_headless_board_is_edited() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe12-{}", std::process::id()));
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
    let footprints = || async {
        let r: GetItemsResponse = board
            .raw_call(&GetItems {
                header: header(),
                types: vec![KiCadObjectType::KotPcbFootprint as i32],
            })
            .await
            .unwrap();
        r.items
            .iter()
            .map(|a| a.to_msg::<FootprintInstance>().unwrap())
            .collect::<Vec<_>>()
    };
    let reference = |f: &FootprintInstance| {
        f.reference_field
            .as_ref()
            .and_then(|f| f.text.as_ref())
            .and_then(|t| t.text.as_ref())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    };
    let first_pad = |f: &FootprintInstance| {
        f.definition
            .iter()
            .flat_map(|d| d.items.iter())
            .find(|i| i.type_url == Pad::type_url())
            .and_then(|i| i.to_msg::<Pad>().ok())
            .map(|p| (p.number, p.position.map(|v| (mm(v.x_nm), mm(v.y_nm)))))
    };

    // 1. An outline in one commit.
    let commit: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
    println!("BEGIN COMMIT {:?}", commit.id);
    let created: Result<CreateItemsResponse, _> = board
        .raw_call(&CreateItems {
            header: header(),
            items: vec![
                Any::from_msg(&rectangle(100.0, 80.0, 150.0, 120.0, 2.0)).unwrap(),
                Any::from_msg(&rectangle(160.0, 80.0, 170.0, 90.0, 0.0)).unwrap(),
            ],
            ..Default::default()
        })
        .await;
    let shape_ids: Vec<_> = match &created {
        Ok(r) => {
            for c in &r.created_items {
                println!(
                    "CREATED shape status {:?} item {:?}",
                    c.status,
                    c.item.as_ref().and_then(|a| a.to_msg::<BoardGraphicShape>().ok())
                );
            }
            r.created_items
                .iter()
                .filter_map(|c| c.item.as_ref())
                .filter_map(|a| a.to_msg::<BoardGraphicShape>().ok())
                .filter_map(|s| s.id)
                .collect()
        }
        Err(e) => {
            println!("CREATE shape failed: {e}");
            vec![]
        }
    };
    let end = board
        .raw_call_any(&EndCommit {
            id: commit.id.clone(),
            action: CommitAction::CmaCommit as i32,
            message: "AI: outline".into(),
            header: header(),
        })
        .await;
    println!("END COMMIT {end:?}");
    let bbox: Result<BoardBoundingBoxResponse, _> = board
        .raw_call(&GetBoardBoundingBox {
            board: Some(document.clone()),
            mode: BoardBoundingBoxMode::BbbmBoardEdgesOnly as i32,
        })
        .await;
    println!("EDGES BOX {bbox:?}");
    let shapes: GetItemsResponse = board
        .raw_call(&GetItems {
            header: header(),
            types: vec![KiCadObjectType::KotPcbShape as i32],
        })
        .await
        .unwrap();
    for a in &shapes.items {
        println!("SHAPE {:?}", a.to_msg::<BoardGraphicShape>().ok());
    }
    // Delete the small one again.
    if let Some(id) = shape_ids.get(1) {
        let d = board
            .raw_call::<_, kicad_ipc::proto::kiapi::common::commands::DeleteItemsResponse>(&DeleteItems {
                header: header(),
                item_ids: vec![id.clone()],
            })
            .await;
        println!("DELETE small shape {:?}", d.map(|r| r.deleted_items.len()));
    }

    // 2. A mounting hole from the library.
    let _ = board
        .raw_call_any(&LoadAllLibraries {
            r#type: vec![LibraryType::LtFootprint as i32],
        })
        .await;
    let mut placed = None;
    for attempt in 0..30 {
        let r: Result<PlaceFromLibraryResponse, _> = board
            .raw_call(&PlaceFootprintFromLibrary {
                header: header(),
                lib_id: Some(LibraryIdentifier {
                    library_nickname: "MountingHole".into(),
                    entry_name: "MountingHole_3.2mm_M3".into(),
                }),
                position: v(104.0, 84.0),
                orientation: None,
                layer: None,
            })
            .await;
        match r {
            Ok(r) => {
                placed = Some(r);
                println!("PLACE hole after {attempt} retries");
                break;
            }
            Err(e) => {
                if attempt == 0 || attempt == 29 {
                    println!("PLACE hole failed: {e}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if let Some(r) = &placed {
        let f = r.item.as_ref().and_then(|a| a.to_msg::<FootprintInstance>().ok());
        println!(
            "HOLE type {:?} ref {:?} at {:?} pad {:?}",
            r.item.as_ref().map(|a| a.type_url.clone()),
            f.as_ref().map(reference),
            f.as_ref().and_then(|f| f.position),
            f.as_ref().and_then(first_pad)
        );
        // Rename it to H1 through the reference field.
        if let Some(mut f) = f {
            if let Some(t) = f
                .reference_field
                .as_mut()
                .and_then(|x| x.text.as_mut())
                .and_then(|t| t.text.as_mut())
            {
                t.text = "H1".into();
            }
            let mut h = header().unwrap();
            h.field_mask = Some(FieldMask {
                paths: vec!["reference_field".into()],
            });
            let u: Result<UpdateItemsResponse, _> = board
                .raw_call(&UpdateItems {
                    header: Some(h),
                    items: vec![Any::from_msg(&f).unwrap()],
                })
                .await;
            println!(
                "RENAME hole {:?}",
                u.map(|u| u
                    .updated_items
                    .first()
                    .and_then(|i| i.item.as_ref())
                    .and_then(|a| a.to_msg::<FootprintInstance>().ok())
                    .map(|f| reference(&f)))
            );
        }
    }

    // 2b. Inside a commit, a placed footprint cannot be updated until the commit ends. A footprint
    // made from the library definition, named and placed by us, created in the commit?
    let lib: Result<GetItemsResponse, _> = board
        .raw_call(&kicad_ipc::proto::kiapi::common::commands::GetItemsFromLibrary {
            r#type: LibraryType::LtFootprint as i32,
            document: Some(document.clone()),
            item_ids: vec![LibraryIdentifier {
                library_nickname: "MountingHole".into(),
                entry_name: "MountingHole_3.2mm_M3".into(),
            }],
        })
        .await;
    match lib {
        Ok(r) => {
            println!(
                "LIBRARY items {:?}",
                r.items.iter().map(|a| a.type_url.clone()).collect::<Vec<_>>()
            );
            if let Some(def) = r
                .items
                .first()
                .and_then(|a| a.to_msg::<kicad_ipc::proto::kiapi::board::types::Footprint>().ok())
            {
                let pad = def
                    .items
                    .iter()
                    .find(|i| i.type_url == Pad::type_url())
                    .and_then(|i| i.to_msg::<Pad>().ok());
                println!(
                    "LIBRARY def items {} ref field {:?} pad at {:?}",
                    def.items.len(),
                    def.reference_field
                        .as_ref()
                        .and_then(|f| f.text.as_ref())
                        .and_then(|t| t.text.as_ref())
                        .map(|t| (t.text.clone(), t.position)),
                    pad.and_then(|p| p.position)
                );
                let mut shifted = def.clone();
                for item in shifted.items.iter_mut() {
                    if item.type_url == Pad::type_url() {
                        let mut p: Pad = item.to_msg().unwrap();
                        if let Some(v) = p.position.as_mut() {
                            v.x_nm += 110_000_000;
                            v.y_nm += 84_000_000;
                        }
                        *item = Any::from_msg(&p).unwrap();
                    }
                }
                let mut reference_field = def.reference_field.clone();
                if let Some(t) = reference_field
                    .as_mut()
                    .and_then(|x| x.text.as_mut())
                    .and_then(|t| t.text.as_mut())
                {
                    t.text = "H9".into();
                }
                let instance = FootprintInstance {
                    position: v(110.0, 84.0),
                    orientation: Some(Angle { value_degrees: 0.0 }),
                    layer: BoardLayer::BlFCu as i32,
                    locked: LockedState::LsUnlocked as i32,
                    reference_field,
                    value_field: def.value_field.clone(),
                    definition: Some(shifted),
                    ..Default::default()
                };
                let commit: BeginCommitResponse = board.raw_call(&BeginCommit { header: header() }).await.unwrap();
                let created: Result<CreateItemsResponse, _> = board
                    .raw_call(&CreateItems {
                        header: header(),
                        items: vec![Any::from_msg(&instance).unwrap()],
                        ..Default::default()
                    })
                    .await;
                let end = board
                    .raw_call_any(&EndCommit {
                        id: commit.id,
                        action: CommitAction::CmaCommit as i32,
                        message: "AI: hole".into(),
                        header: header(),
                    })
                    .await;
                match created {
                    Ok(r) => {
                        let f = r
                            .created_items
                            .first()
                            .and_then(|c| c.item.as_ref())
                            .and_then(|a| a.to_msg::<FootprintInstance>().ok());
                        println!(
                            "CREATE footprint status {:?} ref {:?} at {:?} pad {:?} lib {:?} end {:?}",
                            r.created_items.first().map(|c| c.status.clone()),
                            f.as_ref().map(reference),
                            f.as_ref().and_then(|f| f.position),
                            f.as_ref().and_then(first_pad),
                            f.as_ref()
                                .and_then(|f| f.definition.as_ref())
                                .and_then(|d| d.id.clone()),
                            end.is_ok()
                        );
                    }
                    Err(e) => println!("CREATE footprint failed: {e}"),
                }
            }
        }
        Err(e) => println!("LIBRARY failed: {e}"),
    }

    // 3. Move, turn, and lock one footprint with a field mask.
    let all = footprints().await;
    println!(
        "FOOTPRINTS {:?}",
        all.iter()
            .map(|f| (reference(f), f.layer, f.locked))
            .collect::<Vec<_>>()
    );
    let mut r1 = all.iter().find(|f| reference(f) == "R3").cloned().expect("R3");
    println!(
        "R3 before at {:?} rot {:?} pad {:?}",
        r1.position,
        r1.orientation,
        first_pad(&r1)
    );
    r1.position = v(120.0, 100.0);
    r1.orientation = Some(Angle { value_degrees: 90.0 });
    for paths in [vec!["position".to_owned(), "orientation".to_owned()], vec![]] {
        let mut h = header().unwrap();
        h.field_mask = if paths.is_empty() {
            None
        } else {
            Some(FieldMask { paths: paths.clone() })
        };
        let u: Result<UpdateItemsResponse, _> = board
            .raw_call(&UpdateItems {
                header: Some(h),
                items: vec![Any::from_msg(&r1).unwrap()],
            })
            .await;
        match u {
            Ok(u) => {
                let back = u
                    .updated_items
                    .first()
                    .and_then(|i| i.item.as_ref())
                    .and_then(|a| a.to_msg::<FootprintInstance>().ok());
                println!(
                    "UPDATE mask {paths:?} status {:?} back at {:?} rot {:?} pad {:?}",
                    u.updated_items.first().and_then(|i| i.status.clone()),
                    back.as_ref().and_then(|f| f.position),
                    back.as_ref().and_then(|f| f.orientation),
                    back.as_ref().and_then(first_pad)
                );
            }
            Err(e) => println!("UPDATE mask {paths:?} failed: {e}"),
        }
    }
    let mut r1 = footprints().await.into_iter().find(|f| reference(f) == "R3").unwrap();
    println!(
        "R3 re-read at {:?} rot {:?} pad {:?}",
        r1.position,
        r1.orientation,
        first_pad(&r1)
    );
    r1.locked = LockedState::LsLocked as i32;
    let mut h = header().unwrap();
    h.field_mask = Some(FieldMask {
        paths: vec!["locked".into()],
    });
    let u: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: Some(h),
            items: vec![Any::from_msg(&r1).unwrap()],
        })
        .await;
    println!(
        "LOCK status {:?}",
        u.map(|u| u.updated_items.first().and_then(|i| i.status.clone()))
    );
    // A locked footprint moved through the API?
    let mut r1 = footprints().await.into_iter().find(|f| reference(f) == "R3").unwrap();
    println!("R3 locked state {}", r1.locked);
    r1.position = v(125.0, 100.0);
    let mut h = header().unwrap();
    h.field_mask = Some(FieldMask {
        paths: vec!["position".into()],
    });
    let u: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: Some(h),
            items: vec![Any::from_msg(&r1).unwrap()],
        })
        .await;
    println!(
        "MOVE LOCKED status {:?}",
        u.map(|u| u.updated_items.first().and_then(|i| i.status.clone()))
    );
    let r1 = footprints().await.into_iter().find(|f| reference(f) == "R3").unwrap();
    println!("R3 after moving locked: at {:?}", r1.position);

    // 3b. The full instance leaves pads and fields where they were. A bare instance (id, position,
    // orientation, layer; no definition, no fields)?
    let c3 = footprints().await.into_iter().find(|f| reference(f) == "C3").unwrap();
    println!("C3 before at {:?} pad {:?}", c3.position, first_pad(&c3));
    let bare = FootprintInstance {
        id: c3.id.clone(),
        position: v(130.0, 95.0),
        orientation: Some(Angle { value_degrees: 90.0 }),
        layer: c3.layer,
        locked: c3.locked,
        ..Default::default()
    };
    let u: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: header(),
            items: vec![Any::from_msg(&bare).unwrap()],
        })
        .await;
    println!(
        "BARE UPDATE status {:?}",
        u.map(|u| u.updated_items.first().and_then(|i| i.status.clone()))
    );
    let c3 = footprints().await.into_iter().find(|f| reference(f) == "C3");
    println!(
        "C3 after bare update: {:?}",
        c3.as_ref().map(|c| (
            reference(c),
            c.position,
            c.orientation,
            first_pad(c),
            c.definition.as_ref().map(|d| d.items.len()),
            c.definition.as_ref().and_then(|d| d.id.clone())
        ))
    );
    // A full instance with every pad and field moved by the same offset first (no turn).
    let mut c4 = footprints().await.into_iter().find(|f| reference(f) == "C4").unwrap();
    let old = c4.position.unwrap();
    let (dx, dy) = (135_000_000 - old.x_nm, 95_000_000 - old.y_nm);
    let shift = |p: &mut Option<Vector2>| {
        if let Some(p) = p.as_mut() {
            p.x_nm += dx;
            p.y_nm += dy;
        }
    };
    c4.position = v(135.0, 95.0);
    if let Some(d) = c4.definition.as_mut() {
        for item in d.items.iter_mut() {
            if item.type_url == Pad::type_url() {
                let mut p: Pad = item.to_msg().unwrap();
                shift(&mut p.position);
                *item = Any::from_msg(&p).unwrap();
            }
        }
    }
    for f in [
        &mut c4.reference_field,
        &mut c4.value_field,
        &mut c4.datasheet_field,
        &mut c4.description_field,
    ] {
        if let Some(t) = f.as_mut().and_then(|f| f.text.as_mut()).and_then(|t| t.text.as_mut()) {
            shift(&mut t.position);
        }
    }
    let u: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: header(),
            items: vec![Any::from_msg(&c4).unwrap()],
        })
        .await;
    println!(
        "SHIFTED UPDATE status {:?}",
        u.map(|u| u.updated_items.first().and_then(|i| i.status.clone()))
    );
    let c4 = footprints().await.into_iter().find(|f| reference(f) == "C4").unwrap();
    println!("C4 after shifted update at {:?} pad {:?}", c4.position, first_pad(&c4));

    // 4. Flip one footprint to the back, and try flipping through the layer field.
    let c1 = footprints().await.into_iter().find(|f| reference(f) == "C1").unwrap();
    println!(
        "C1 before at {:?} layer {} pad {:?}",
        c1.position,
        c1.layer,
        first_pad(&c1)
    );
    let flip: Result<FlipItemsResponse, _> = board
        .raw_call(&FlipItems {
            header: header(),
            items: vec![c1.id.clone().unwrap()],
            direction: BoardFlipDirection::BfdLeftRight as i32,
        })
        .await;
    println!(
        "FLIP {:?}",
        flip.map(|f| (
            f.status,
            f.flipped_items.iter().map(|i| i.status.clone()).collect::<Vec<_>>()
        ))
    );
    let c1 = footprints().await.into_iter().find(|f| reference(f) == "C1").unwrap();
    println!(
        "C1 after flip at {:?} layer {} rot {:?} pad {:?}",
        c1.position,
        c1.layer,
        c1.orientation,
        first_pad(&c1)
    );
    let mut c2 = footprints().await.into_iter().find(|f| reference(f) == "C2").unwrap();
    c2.layer = BoardLayer::BlBCu as i32;
    let mut h = header().unwrap();
    h.field_mask = Some(FieldMask {
        paths: vec!["layer".into()],
    });
    let u: Result<UpdateItemsResponse, _> = board
        .raw_call(&UpdateItems {
            header: Some(h),
            items: vec![Any::from_msg(&c2).unwrap()],
        })
        .await;
    println!(
        "LAYER UPDATE status {:?}",
        u.map(|u| u.updated_items.first().and_then(|i| i.status.clone()))
    );
    let c2 = footprints().await.into_iter().find(|f| reference(f) == "C2").unwrap();
    println!("C2 after layer update: layer {} pad {:?}", c2.layer, first_pad(&c2));

    // 5. A track, a via, and a zone (Phase 12.5 and 12.6), then a refill.
    let net = |name: &str| {
        Some(Net {
            name: name.into(),
            ..Default::default()
        })
    };
    let r2 = footprints().await.into_iter().find(|f| reference(f) == "U1").unwrap();
    let pad_net = r2
        .definition
        .iter()
        .flat_map(|d| d.items.iter())
        .filter(|i| i.type_url == Pad::type_url())
        .filter_map(|i| i.to_msg::<Pad>().ok())
        .map(|p| p.net.map(|n| n.name).unwrap_or_default())
        .find(|n| !n.is_empty())
        .unwrap_or_default();
    println!("NET used for the track: {pad_net:?}");
    let track = Track {
        start: v(110.0, 110.0),
        end: v(130.0, 110.0),
        width: Some(Distance { value_nm: 250_000 }),
        layer: BoardLayer::BlFCu as i32,
        net: net(&pad_net),
        ..Default::default()
    };
    let via = Via {
        position: v(130.0, 110.0),
        net: net(&pad_net),
        ..Default::default()
    };
    let outline = PolySet {
        polygons: vec![PolygonWithHoles {
            outline: Some(PolyLine {
                nodes: [(101.0, 81.0), (149.0, 81.0), (149.0, 119.0), (101.0, 119.0)]
                    .iter()
                    .map(|(x, y)| PolyLineNode {
                        geometry: Some(kicad_ipc::proto::kiapi::common::types::poly_line_node::Geometry::Point(
                            Vector2 {
                                x_nm: (x * 1e6) as i64,
                                y_nm: (y * 1e6) as i64,
                            },
                        )),
                    })
                    .collect(),
                closed: true,
            }),
            holes: vec![],
        }],
    };
    let zone = Zone {
        r#type: ZoneType::ZtCopper as i32,
        layers: vec![BoardLayer::BlBCu as i32],
        outline: Some(outline),
        name: "GND pour".into(),
        settings: Some(zone::Settings::CopperSettings(CopperZoneSettings {
            net: net("GND"),
            ..Default::default()
        })),
        ..Default::default()
    };
    let created: Result<CreateItemsResponse, _> = board
        .raw_call(&CreateItems {
            header: header(),
            items: vec![
                Any::from_msg(&track).unwrap(),
                Any::from_msg(&via).unwrap(),
                Any::from_msg(&zone).unwrap(),
            ],
            ..Default::default()
        })
        .await;
    match &created {
        Ok(r) => {
            for c in &r.created_items {
                println!(
                    "CREATED {:?} status {:?}",
                    c.item.as_ref().map(|a| a.type_url.clone()),
                    c.status
                );
            }
            if let Some(v) = r
                .created_items
                .iter()
                .filter_map(|c| c.item.as_ref())
                .find(|a| a.type_url == Via::type_url())
                .and_then(|a| a.to_msg::<Via>().ok())
            {
                println!(
                    "VIA defaults {:?}",
                    v.pad_stack.map(|p| (p.drill, p.copper_layers.first().cloned()))
                );
            }
        }
        Err(e) => println!("CREATE track/via/zone failed: {e}"),
    }
    let refill = board
        .raw_call_any(&RefillZones {
            board: Some(document.clone()),
            zones: vec![],
        })
        .await;
    println!("REFILL {refill:?}");
    let zones: GetItemsResponse = board
        .raw_call(&GetItems {
            header: header(),
            types: vec![KiCadObjectType::KotPcbZone as i32],
        })
        .await
        .unwrap();
    for a in &zones.items {
        let z: Zone = a.to_msg().unwrap();
        println!(
            "ZONE {} filled {} polygons {}",
            z.name,
            z.filled,
            z.filled_polygons
                .iter()
                .map(|l| l.shapes.as_ref().map(|s| s.polygons.len()).unwrap_or(0))
                .sum::<usize>()
        );
    }

    // 5b. Routing facts (12.5): net classes, the board's minimums, a pad's copper, and a via made
    // from the net class's pad stack.
    let classes: Result<kicad_ipc::proto::kiapi::board::commands::NetClassForNetsResponse, _> = board
        .raw_call(&kicad_ipc::proto::kiapi::board::commands::GetNetClassForNets {
            net: vec![
                Net {
                    name: "GND".into(),
                    ..Default::default()
                },
                Net {
                    name: "+3V3".into(),
                    ..Default::default()
                },
            ],
        })
        .await;
    let mut via_stack = None;
    match &classes {
        Ok(r) => {
            for (net, class) in &r.classes {
                let b = class.board.as_ref();
                println!(
                    "NETCLASS {net}: {} clearance {:?} track {:?} via {:?}",
                    class.name,
                    b.and_then(|b| b.clearance),
                    b.and_then(|b| b.track_width),
                    b.and_then(|b| b.via_stack.as_ref()).map(|v| (
                        v.drill.as_ref().and_then(|d| d.diameter),
                        v.copper_layers.first().and_then(|l| l.size)
                    ))
                );
                via_stack = via_stack.or(b.and_then(|b| b.via_stack.clone()));
            }
        }
        Err(e) => println!("NETCLASS failed: {e}"),
    }
    let rules: Result<kicad_ipc::proto::kiapi::board::commands::BoardDesignRulesResponse, _> = board
        .raw_call(&kicad_ipc::proto::kiapi::board::commands::GetBoardDesignRules {
            board: Some(document.clone()),
        })
        .await;
    println!(
        "RULES {:?}",
        rules.map(|r| r.rules.and_then(|r| r.constraints).map(|c| (
            c.min_clearance,
            c.min_track_width,
            c.copper_edge_clearance,
            c.min_via_size,
            c.min_through_drill
        )))
    );
    if let Some(u1) = footprints().await.into_iter().find(|f| reference(f) == "U1") {
        for p in u1
            .definition
            .iter()
            .flat_map(|d| d.items.iter())
            .filter(|i| i.type_url == Pad::type_url())
            .filter_map(|i| i.to_msg::<Pad>().ok())
            .take(4)
        {
            let s = p.pad_stack.as_ref();
            println!(
                "U1 PAD {} type {} at {:?} angle {:?} layers {:?} copper {:?}",
                p.number,
                p.r#type,
                p.position,
                s.and_then(|s| s.angle),
                s.map(|s| s.layers.clone()),
                s.map(|s| s
                    .copper_layers
                    .iter()
                    .map(|l| (l.layer, l.shape, l.size))
                    .collect::<Vec<_>>())
            );
        }
    }
    if let Some(stack) = via_stack {
        let via = Via {
            position: v(140.0, 112.0),
            net: net("GND"),
            pad_stack: Some(stack),
            r#type: kicad_ipc::proto::kiapi::board::types::ViaType::VtThrough as i32,
            ..Default::default()
        };
        let created: Result<CreateItemsResponse, _> = board
            .raw_call(&CreateItems {
                header: header(),
                items: vec![Any::from_msg(&via).unwrap()],
                ..Default::default()
            })
            .await;
        match created {
            Ok(r) => {
                let back = r
                    .created_items
                    .first()
                    .and_then(|c| c.item.as_ref())
                    .and_then(|a| a.to_msg::<Via>().ok());
                println!(
                    "VIA FROM NETCLASS status {:?} drill {:?} size {:?}",
                    r.created_items.first().map(|c| c.status.clone()),
                    back.as_ref()
                        .and_then(|v| v.pad_stack.as_ref())
                        .and_then(|p| p.drill.as_ref())
                        .and_then(|d| d.diameter),
                    back.as_ref()
                        .and_then(|v| v.pad_stack.as_ref())
                        .and_then(|p| p.copper_layers.first())
                        .and_then(|l| l.size)
                );
            }
            Err(e) => println!("VIA FROM NETCLASS failed: {e}"),
        }
    }

    // 6. The saved file, read with our own reader.
    let copy = dir.join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    let saved = board.save_copy(&copy).await.unwrap();
    let text = std::fs::read_to_string(&saved).unwrap();
    for line in text.lines().filter(|l| l.contains("gr_rect") || l.contains("radius")) {
        println!("FILE {}", line.trim());
    }
    match kicad_sexpr::pcb::read_board(&text) {
        Ok(b) => {
            println!("FILE outline {:?}", b.outline);
            for f in &b.footprints {
                println!(
                    "FILE fp {} at {:?} rot {} layer {} locked {}",
                    f.reference, f.position, f.rotation_deg, f.layer, f.locked
                );
            }
            println!(
                "FILE tracks {} vias {} zones {}",
                b.tracks.len(),
                b.vias.len(),
                b.zones.len()
            );
        }
        Err(e) => println!("FILE read failed: {e}"),
    }
    let api = board.read().await.unwrap();
    println!("API outline {:?}", api.outline);
    println!("CLOSE {:?}", board.close(true).await);
}
