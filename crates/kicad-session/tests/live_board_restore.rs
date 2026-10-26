// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 12.8: a board put back after a change. Skipped unless `KICAD_LIVE=1`.
//!
//! On a copy of the regulator board: record it, apply an outline with a hole, moves (a turn, a
//! flip, a lock), a track with a via, and a ground pour, then restore the record and compare the
//! board with what it was, pad by pad.

use std::path::PathBuf;

use kicad_domain::board::Board;
use kicad_domain::board_edit::{self, HolePlacement, MoveRequest, OutlineRequest};
use kicad_domain::routing::{self, RouteEnd, RoutePoint, RouteRequest};
use kicad_domain::zones::{self, PadConnection, ZoneArea, ZoneRequest};
use kicad_domain::{Nm, Point};
use kicad_session::{BoardSession, SessionConfig};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn p(x: f64, y: f64) -> Point {
    Point::from_mm(x, y).unwrap()
}

/// Everything a rollback must put back: each footprint's place, turn, side, lock, and pads, and
/// the other items by kind.
fn state(b: &Board) -> (Vec<String>, usize, usize, usize, usize) {
    let mut fps: Vec<String> = b
        .footprints
        .iter()
        .map(|f| {
            format!(
                "{} {:?} {} {} {} {:?}",
                f.reference,
                f.position,
                f.rotation_deg,
                f.layer,
                f.locked,
                f.pads
                    .iter()
                    .map(|p| (p.number.clone(), p.at, p.net.clone()))
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    fps.sort();
    (fps, b.tracks.len(), b.vias.len(), b.zones.len(), b.edges.len())
}

#[tokio::test]
async fn a_board_is_put_back_after_changes() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/v11/regulator-board");
    for f in ["regulator.kicad_pcb", "regulator.kicad_sch"] {
        std::fs::copy(root.join(f), dir.join(f)).unwrap();
    }
    let board = BoardSession::open_headless(&SessionConfig::default(), &installs, &dir.join("regulator.kicad_pcb"))
        .await
        .unwrap();
    let before = board.read().await.unwrap();
    let snap = board.snapshot().await.unwrap();

    // An outline with a hole.
    let outline = board_edit::plan_outline(
        &before,
        &OutlineRequest {
            size: Some((Nm::from_mm(40.0).unwrap(), Nm::from_mm(30.0).unwrap())),
            origin: None,
            margin: Nm(0),
            corner_radius: Nm::from_mm(1.0).unwrap(),
            replace: false,
            holes: HolePlacement::At(vec![p(-2.0, -2.0)]),
            hole_footprint: board_edit::DEFAULT_HOLE_FOOTPRINT.into(),
            hole_courtyard: None,
            grid_mm: 0.5,
        },
    )
    .unwrap();
    board.apply_edit(&outline).await.unwrap();
    // Moves: R3 turned, C1 flipped, U1 locked.
    let now = board.read().await.unwrap();
    let moves = board_edit::plan_moves(
        &now,
        &[
            MoveRequest {
                reference: "R3".into(),
                at: Some(p(20.0, 5.0)),
                rotation_deg: Some(90.0),
                ..Default::default()
            },
            MoveRequest {
                reference: "C1".into(),
                back: Some(true),
                ..Default::default()
            },
            MoveRequest {
                reference: "U1".into(),
                lock: Some(true),
                ..Default::default()
            },
        ],
        0.5,
    )
    .unwrap();
    board.apply_edit(&moves).await.unwrap();
    // A track and a via on GND, and a ground pour.
    let now = board.read().await.unwrap();
    let rules = board.routing_rules(&now.nets).await.unwrap();
    let (route, _) = routing::plan_routes(
        &now,
        &rules,
        &[RouteRequest {
            net: Some("GND".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(-10.0, 18.0)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(-5.0, 18.0)),
                    layer: Some("B.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(-5.0, 14.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        }],
    )
    .unwrap();
    board.apply_edit(&route).await.unwrap();
    let now = board.read().await.unwrap();
    let (pour, _) = zones::plan_zones(
        &now,
        &rules,
        &[ZoneRequest {
            net: "GND".into(),
            layers: vec!["B.Cu".into()],
            area: ZoneArea::Board,
            clearance: None,
            min_width: None,
            connection: PadConnection::Thermal,
            thermal_gap: None,
            spoke_width: None,
            priority: None,
            name: None,
            replace: false,
        }],
    )
    .unwrap();
    board.apply_edit(&pour).await.unwrap();
    let changed = board.read().await.unwrap();
    assert_ne!(state(&changed), state(&before), "the changes happened");

    let report = board.restore(&snap).await.unwrap();
    println!("RESTORE {report:?}");
    let after = board.read().await.unwrap();
    assert_eq!(state(&after), state(&before), "the board is as it was");

    // Removed items come back: an outline replaced by another, then restored.
    let first = board_edit::plan_outline(
        &after,
        &OutlineRequest {
            size: Some((Nm::from_mm(40.0).unwrap(), Nm::from_mm(30.0).unwrap())),
            origin: Some(p(-10.0, -8.0)),
            margin: Nm(0),
            corner_radius: Nm(0),
            replace: false,
            holes: HolePlacement::None,
            hole_footprint: board_edit::DEFAULT_HOLE_FOOTPRINT.into(),
            hole_courtyard: None,
            grid_mm: 0.5,
        },
    )
    .unwrap();
    board.apply_edit(&first).await.unwrap();
    let with_outline = board.read().await.unwrap();
    let snap = board.snapshot().await.unwrap();
    let second = board_edit::plan_outline(
        &with_outline,
        &OutlineRequest {
            size: Some((Nm::from_mm(50.0).unwrap(), Nm::from_mm(35.0).unwrap())),
            origin: Some(p(-12.0, -10.0)),
            margin: Nm(0),
            corner_radius: Nm(0),
            replace: true,
            holes: HolePlacement::None,
            hole_footprint: board_edit::DEFAULT_HOLE_FOOTPRINT.into(),
            hole_courtyard: None,
            grid_mm: 0.5,
        },
    )
    .unwrap();
    board.apply_edit(&second).await.unwrap();
    assert_ne!(board.read().await.unwrap().outline, with_outline.outline);
    let report = board.restore(&snap).await.unwrap();
    assert_eq!((report.deleted, report.recreated), (1, 1), "{report:?}");
    let back = board.read().await.unwrap();
    assert_eq!(back.outline, with_outline.outline, "the first outline is back");
    assert_eq!(back.edges.len(), 1);
    board.close(true).await.unwrap();
}
