// SPDX-License-Identifier: GPL-3.0-or-later
//! Copper pours (Phase 12.6): a zone on one net over the board or a given area, on one or more
//! copper layers, with its clearance, minimum width, and how pads connect (thermal reliefs, solid,
//! or not at all). Planned here, created in one commit, refilled, and checked with DRC.
//!
//! Every setting is sent: a zone created without them gets a clearance and minimum width of zero
//! (the Phase 12 probe). The defaults are the net class's clearance and KiCad's own zone defaults
//! (0.25 mm minimum width, 0.5 mm thermal gap and spokes).

use serde::Serialize;

use crate::board::Board;
use crate::board_edit::{measure, BoardEditPlan, BoardOp};
use crate::geometry::{Nm, Point, Rect};
use crate::result::{ErrorCode, ToolError};
use crate::routing::BoardRules;

pub const DEFAULT_MIN_WIDTH_NM: i64 = 250_000;
pub const DEFAULT_THERMAL_GAP_NM: i64 = 500_000;
pub const DEFAULT_SPOKE_WIDTH_NM: i64 = 500_000;

#[derive(Debug, Clone, PartialEq)]
pub enum ZoneArea {
    /// The board outline's box (KiCad keeps the fill inside the edge itself).
    Board,
    Rect(Rect),
    Polygon(Vec<Point>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PadConnection {
    /// Spokes across a gap: easy to solder.
    Thermal,
    /// The pad is flooded.
    Solid,
    /// Pads are not connected by the pour.
    None,
}

impl PadConnection {
    pub fn parse(s: &str) -> Option<PadConnection> {
        match s {
            "thermal" => Some(PadConnection::Thermal),
            "solid" => Some(PadConnection::Solid),
            "none" => Some(PadConnection::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneRequest {
    pub net: String,
    pub layers: Vec<String>,
    pub area: ZoneArea,
    pub clearance: Option<Nm>,
    pub min_width: Option<Nm>,
    pub connection: PadConnection,
    pub thermal_gap: Option<Nm>,
    pub spoke_width: Option<Nm>,
    pub priority: Option<u32>,
    pub name: Option<String>,
    /// Delete the zones this net already has on these layers.
    pub replace: bool,
}

/// The settings a zone is created with.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneSpec {
    pub name: String,
    pub net: String,
    pub layers: Vec<String>,
    pub outline: Vec<Point>,
    pub clearance: Nm,
    pub min_width: Nm,
    pub connection: PadConnection,
    pub thermal_gap: Nm,
    pub spoke_width: Nm,
    pub priority: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneSummary {
    pub name: String,
    pub net: String,
    pub layers: Vec<String>,
    pub area_mm2: f64,
    /// Pads of the net on those layers inside the area: what the pour should reach.
    pub pads_inside: Vec<String>,
    pub replaces: Vec<String>,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn polygon_area_mm2(pts: &[Point]) -> f64 {
    let n = pts.len();
    let twice: f64 = (0..n)
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % n]);
            a.x.to_mm() * b.y.to_mm() - b.x.to_mm() * a.y.to_mm()
        })
        .sum();
    (twice / 2.0).abs()
}

/// Is `p` inside the polygon (even-odd rule)?
fn contains(poly: &[Point], p: Point) -> bool {
    let mut inside = false;
    let n = poly.len();
    let (px, py) = (p.x.0 as f64, p.y.0 as f64);
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + n - 1) % n]);
        let (ax, ay, bx, by) = (a.x.0 as f64, a.y.0 as f64, b.x.0 as f64, b.y.0 as f64);
        if (ay > py) != (by > py) && px < (bx - ax) * (py - ay) / (by - ay) + ax {
            inside = !inside;
        }
    }
    inside
}

/// Plans copper pours.
pub fn plan_zones(
    board: &Board,
    rules: &BoardRules,
    requests: &[ZoneRequest],
) -> Result<(BoardEditPlan, Vec<ZoneSummary>), ToolError> {
    if requests.is_empty() {
        return Err(bad("nothing to pour; give at least one zone"));
    }
    let mut operations = Vec::new();
    let mut summaries = Vec::new();
    let mut warnings = Vec::new();
    let mut expected = board.clone();
    let mut planned: Vec<ZoneSpec> = Vec::new();
    for (i, req) in requests.iter().enumerate() {
        let label = format!("zone {}", i + 1);
        if !board.nets.iter().any(|n| n == &req.net) || req.net.is_empty() {
            return Err(bad(format!(
                "{label}: the board has no net '{}' (use pcb_list_nets)",
                req.net
            )));
        }
        if req.layers.is_empty() {
            return Err(bad(format!("{label}: give at least one copper layer")));
        }
        let mut layers: Vec<String> = Vec::new();
        for l in &req.layers {
            if !board.copper_layers.iter().any(|c| c == l) {
                return Err(bad(format!(
                    "{label}: '{l}' is not a copper layer of this board ({})",
                    board.copper_layers.join(", ")
                )));
            }
            if !layers.contains(l) {
                layers.push(l.clone());
            }
        }
        let outline: Vec<Point> = match &req.area {
            ZoneArea::Board => {
                let o = board.outline.ok_or_else(|| {
                    bad(format!("{label}: the board has no outline to pour over; give it one (pcb_outline_plan) or give an area"))
                })?;
                vec![o.min, Point::new(o.max.x, o.min.y), o.max, Point::new(o.min.x, o.max.y)]
            }
            ZoneArea::Rect(r) => vec![r.min, Point::new(r.max.x, r.min.y), r.max, Point::new(r.min.x, r.max.y)],
            ZoneArea::Polygon(p) => p.clone(),
        };
        if outline.len() < 3 {
            return Err(bad(format!("{label}: an area needs at least three corners")));
        }
        let area = polygon_area_mm2(&outline);
        if area < 1.0 {
            return Err(bad(format!(
                "{label}: the area is {area:.2} mm²; a pour needs at least 1 mm²"
            )));
        }
        if let (Some(o), Some(b)) = (board.outline, Rect::from_points(outline.iter().copied())) {
            if !o.overlaps(b) {
                return Err(bad(format!("{label}: the area is outside the board")));
            }
        }
        // Zones this net already has on these layers.
        let same: Vec<&crate::board::BoardZone> = board
            .zones
            .iter()
            .filter(|z| z.net == req.net && z.layers.iter().any(|l| layers.contains(l)))
            .collect();
        let mut replaces = Vec::new();
        if !same.is_empty() {
            if !req.replace {
                return Err(bad(format!(
                    "{label}: net '{}' already has {} zone(s) on {} ({}); pass replace: true to replace them",
                    req.net,
                    same.len(),
                    layers.join(", "),
                    same.iter()
                        .map(|z| if z.name.is_empty() {
                            z.id.as_str()
                        } else {
                            z.name.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if let Some(z) = same.iter().find(|z| z.locked) {
                return Err(bad(format!(
                    "{label}: the zone {} is locked; unlock it in KiCad first",
                    z.id
                )));
            }
            for z in &same {
                if !operations
                    .iter()
                    .any(|o| matches!(o, BoardOp::RemoveZone { id, .. } if id == &z.id))
                {
                    operations.push(BoardOp::RemoveZone {
                        id: z.id.clone(),
                        name: z.name.clone(),
                    });
                }
                replaces.push(if z.name.is_empty() {
                    z.id.clone()
                } else {
                    z.name.clone()
                });
            }
            expected.zones.retain(|e| !same.iter().any(|z| z.id == e.id));
        }
        let class = rules.of(&req.net);
        let positive = |v: Option<Nm>, default: Nm, what: &str| -> Result<Nm, ToolError> {
            let v = v.unwrap_or(default);
            if v.0 < 0 || v.0 > 10_000_000 {
                return Err(bad(format!("{label}: {what} is 0 to 10 mm")));
            }
            Ok(v)
        };
        let clearance = positive(req.clearance, class.clearance, "clearance_mm")?;
        if clearance < class.clearance {
            warnings.push(format!(
                "{label}: a clearance of {:.2} mm is below the net class's {:.2} mm; DRC will use the larger",
                clearance.to_mm(),
                class.clearance.to_mm()
            ));
        }
        let min_width = positive(req.min_width, Nm(DEFAULT_MIN_WIDTH_NM), "min_width_mm")?;
        if min_width.0 == 0 {
            return Err(bad(format!("{label}: min_width_mm must be above zero")));
        }
        let spec = ZoneSpec {
            name: req
                .name
                .clone()
                .unwrap_or_else(|| format!("{} {}", req.net, layers.join("+"))),
            net: req.net.clone(),
            layers: layers.clone(),
            outline: outline.clone(),
            clearance,
            min_width,
            connection: req.connection,
            thermal_gap: positive(req.thermal_gap, Nm(DEFAULT_THERMAL_GAP_NM), "thermal_gap_mm")?,
            spoke_width: positive(req.spoke_width, Nm(DEFAULT_SPOKE_WIDTH_NM), "spoke_width_mm")?,
            priority: req.priority.unwrap_or(0),
        };
        // Other nets' zones that overlap on a shared layer with the same priority fill unpredictably.
        let bounds = Rect::from_points(outline.iter().copied()).expect("three corners");
        let clash_existing = expected.zones.iter().any(|z| {
            z.net != spec.net
                && z.priority == spec.priority
                && z.layers.iter().any(|l| layers.contains(l))
                && z.bounds.is_none_or(|b| b.overlaps(bounds))
        });
        let clash_planned = planned.iter().any(|p| {
            p.net != spec.net
                && p.priority == spec.priority
                && p.layers.iter().any(|l| layers.contains(l))
                && Rect::from_points(p.outline.iter().copied()).is_some_and(|b| b.overlaps(bounds))
        });
        if clash_existing || clash_planned {
            warnings.push(format!(
                "{label}: it overlaps a zone of another net on the same layer with the same priority; give one of them a higher priority so the fill is predictable"
            ));
        }
        let pads_inside: Vec<String> = board
            .footprints
            .iter()
            .flat_map(|f| {
                f.pads
                    .iter()
                    .filter(|p| p.net == spec.net)
                    .filter(|p| p.kind == "thru_hole" || p.layers.is_empty() || layers.iter().any(|l| p.on_layer(l)))
                    .filter(|p| contains(&outline, p.at))
                    .map(move |p| format!("{}.{}", f.reference, p.number))
            })
            .collect();
        if pads_inside.is_empty() {
            warnings.push(format!(
                "{label}: no pad of '{}' is inside the area on {}; the pour would connect nothing and may be removed as an island",
                spec.net,
                layers.join(", ")
            ));
        }
        if spec.connection == PadConnection::None {
            warnings.push(format!(
                "{label}: pad connection none: the pour will not connect the net's pads"
            ));
        }
        expected.zones.push(crate::board::BoardZone {
            id: String::new(),
            name: spec.name.clone(),
            layers: layers.clone(),
            net: spec.net.clone(),
            priority: spec.priority,
            bounds: Some(bounds),
            locked: false,
            filled: true,
        });
        summaries.push(ZoneSummary {
            name: spec.name.clone(),
            net: spec.net.clone(),
            layers,
            area_mm2: (area * 100.0).round() / 100.0,
            pads_inside,
            replaces,
        });
        operations.push(BoardOp::AddZone(Box::new(spec.clone())));
        planned.push(spec);
    }
    Ok((
        BoardEditPlan {
            kind: "zone",
            operations,
            before: measure(board),
            after: measure(&expected),
            warnings,
            expected,
        },
        summaries,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardFootprint, BoardPad, BoardZone};

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn board() -> Board {
        let pad = |n: &str, at: Point, net: &str| BoardPad {
            number: n.into(),
            kind: "smd".into(),
            at,
            net: net.into(),
            size: Some((Nm(1_000_000), Nm(1_000_000))),
            angle_deg: 0.0,
            layers: vec!["F.Cu".into()],
        };
        Board {
            footprints: vec![BoardFootprint {
                id: "c1".into(),
                reference: "C1".into(),
                value: String::new(),
                lib_id: "L:F".into(),
                layer: "F.Cu".into(),
                position: p(10.0, 10.0),
                rotation_deg: 0.0,
                locked: false,
                dnp: false,
                exclude_from_bom: false,
                exclude_from_pos: false,
                board_only: false,
                pads: vec![pad("1", p(9.0, 10.0), "+3V3"), pad("2", p(11.0, 10.0), "GND")],
                courtyard: None,
                courtyard_local: None,
            }],
            nets: vec!["GND".into(), "+3V3".into()],
            copper_layers: vec!["F.Cu".into(), "B.Cu".into()],
            outline: Rect::from_points([p(0.0, 0.0), p(30.0, 20.0)]),
            ..Default::default()
        }
    }

    fn gnd(layers: &[&str]) -> ZoneRequest {
        ZoneRequest {
            net: "GND".into(),
            layers: layers.iter().map(|l| (*l).to_owned()).collect(),
            area: ZoneArea::Board,
            clearance: None,
            min_width: None,
            connection: PadConnection::Thermal,
            thermal_gap: None,
            spoke_width: None,
            priority: None,
            name: None,
            replace: false,
        }
    }

    #[test]
    fn a_ground_pour_over_the_board_takes_the_class_and_kicad_defaults() {
        let (plan, s) = plan_zones(&board(), &BoardRules::default(), &[gnd(&["F.Cu", "B.Cu"])]).unwrap();
        let BoardOp::AddZone(z) = &plan.operations[0] else {
            panic!("a zone")
        };
        assert_eq!(z.name, "GND F.Cu+B.Cu");
        assert_eq!(
            (z.clearance, z.min_width, z.thermal_gap, z.spoke_width),
            (Nm(200_000), Nm(250_000), Nm(500_000), Nm(500_000))
        );
        assert_eq!(z.outline.len(), 4);
        assert_eq!(s[0].area_mm2, 600.0);
        assert_eq!(s[0].pads_inside, vec!["C1.2".to_owned()]);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn an_existing_zone_is_replaced_only_when_asked() {
        let mut b = board();
        b.zones.push(BoardZone {
            id: "z1".into(),
            name: "old".into(),
            layers: vec!["B.Cu".into()],
            net: "GND".into(),
            priority: 0,
            bounds: None,
            locked: false,
            filled: true,
        });
        assert!(plan_zones(&b, &BoardRules::default(), &[gnd(&["B.Cu"])]).is_err());
        let mut r = gnd(&["B.Cu"]);
        r.replace = true;
        let (plan, s) = plan_zones(&b, &BoardRules::default(), &[r.clone()]).unwrap();
        assert!(matches!(&plan.operations[0], BoardOp::RemoveZone { id, .. } if id == "z1"));
        assert_eq!(s[0].replaces, vec!["old".to_owned()]);
        b.zones[0].locked = true;
        assert!(plan_zones(&b, &BoardRules::default(), &[r]).is_err());
    }

    #[test]
    fn bad_nets_layers_and_areas_are_refused_and_empty_pours_warned() {
        let b = board();
        let mut r = gnd(&["F.Cu"]);
        r.net = "VCC".into();
        assert!(plan_zones(&b, &BoardRules::default(), &[r]).is_err());
        assert!(plan_zones(&b, &BoardRules::default(), &[gnd(&["In1.Cu"])]).is_err());
        let mut tiny = gnd(&["F.Cu"]);
        tiny.area = ZoneArea::Rect(Rect::from_points([p(1.0, 1.0), p(1.5, 1.5)]).unwrap());
        assert!(plan_zones(&b, &BoardRules::default(), &[tiny]).is_err());
        let mut away = gnd(&["F.Cu"]);
        away.area = ZoneArea::Rect(Rect::from_points([p(100.0, 100.0), p(110.0, 110.0)]).unwrap());
        assert!(plan_zones(&b, &BoardRules::default(), &[away]).is_err());
        let mut corner = gnd(&["F.Cu"]);
        corner.area = ZoneArea::Polygon(vec![p(20.0, 1.0), p(29.0, 1.0), p(29.0, 9.0)]);
        let (plan, _) = plan_zones(&b, &BoardRules::default(), &[corner]).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("connect nothing")),
            "{:?}",
            plan.warnings
        );
        let mut no_outline = board();
        no_outline.outline = None;
        assert!(plan_zones(&no_outline, &BoardRules::default(), &[gnd(&["F.Cu"])]).is_err());
    }

    #[test]
    fn overlapping_pours_of_two_nets_need_priorities() {
        let mut supply = gnd(&["F.Cu"]);
        supply.net = "+3V3".into();
        let (plan, _) = plan_zones(&board(), &BoardRules::default(), &[gnd(&["F.Cu"]), supply.clone()]).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("priority")),
            "{:?}",
            plan.warnings
        );
        supply.priority = Some(1);
        let (plan, _) = plan_zones(&board(), &BoardRules::default(), &[gnd(&["F.Cu"]), supply]).unwrap();
        assert!(
            !plan.warnings.iter().any(|w| w.contains("priority")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn polygons() {
        let tri = [p(0.0, 0.0), p(4.0, 0.0), p(0.0, 3.0)];
        assert_eq!(polygon_area_mm2(&tri), 6.0);
        assert!(contains(&tri, p(1.0, 1.0)));
        assert!(!contains(&tri, p(3.0, 3.0)));
    }
}
