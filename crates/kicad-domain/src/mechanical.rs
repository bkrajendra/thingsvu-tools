// SPDX-License-Identifier: GPL-3.0-or-later
//! The board as a neutral mechanical model ([`mech_model`], Phase 13.4), from what the board file
//! says ([`FabFacts`]).

use mech_model::{
    nearest_edge, parse_height, typical_height, BoardBody, Box2, Component, Connector, HeightSource, KeepoutArea,
    MechanicalModel, MountingHole, OutlineShape, Side, FORMAT,
};

use crate::fab::{outline_check, CheckStatus, EdgeShape, FabFacts, FootprintFacts};
use crate::geometry::{Point, Rect};

/// A connector this close to an edge (courtyard to edge) faces it.
const AT_EDGE_MM: f64 = 3.0;

fn xy(p: Point) -> [f64; 2] {
    [round3(p.x.to_mm()), round3(p.y.to_mm())]
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn box2(r: Rect) -> Box2 {
    let ([min_x, min_y], [max_x, max_y]) = (xy(r.min), xy(r.max));
    Box2 {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

fn side(f: &FootprintFacts) -> Side {
    if f.is_back() {
        Side::Bottom
    } else {
        Side::Top
    }
}

/// A connector by its reference (`J`, `P`, `CN`, `USB` and a number) or its library.
fn is_connector(f: &FootprintFacts) -> bool {
    let prefix: String = f.reference.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    matches!(prefix.as_str(), "J" | "P" | "CN" | "USB") || f.lib_id.starts_with("Connector")
}

fn height(f: &FootprintFacts) -> (Option<f64>, HeightSource) {
    if let Some(h) = f.properties.get("Height").and_then(|h| parse_height(h)) {
        return (Some(h), HeightSource::Field);
    }
    match typical_height(&f.lib_id) {
        Some(h) => (Some(h), HeightSource::Estimated),
        None => (None, HeightSource::Unknown),
    }
}

pub fn build(facts: &FabFacts, outline: Option<Rect>, source: &str, step_path: Option<&str>) -> MechanicalModel {
    let board_box = outline.map(box2);
    let components: Vec<Component> = facts
        .footprints
        .iter()
        .filter(|f| !f.is_mechanical() && !f.board_only)
        .map(|f| {
            let (height_mm, height_source) = height(f);
            Component {
                reference: f.reference.clone(),
                value: f.value.clone(),
                footprint: f.lib_id.clone(),
                side: side(f),
                at: xy(f.position),
                rotation_deg: f.rotation_deg,
                bounds: f.courtyard.map(box2),
                height_mm,
                height_source,
                model: f.models.first().cloned(),
            }
        })
        .collect();
    let connectors = facts
        .footprints
        .iter()
        .filter(|f| is_connector(f))
        .map(|f| {
            let bounds = f.courtyard.map(box2);
            let near = board_box.zip(bounds).map(|(b, p)| nearest_edge(&b, &p));
            Connector {
                reference: f.reference.clone(),
                value: f.value.clone(),
                side: side(f),
                at: xy(f.position),
                rotation_deg: f.rotation_deg,
                bounds,
                edge: near.filter(|(_, d)| *d <= AT_EDGE_MM).map(|(e, _)| e),
                distance_to_edge_mm: near.map(|(_, d)| round3(d)),
                height_mm: height(f).0,
            }
        })
        .collect();
    let mechanical: Vec<&str> = facts
        .footprints
        .iter()
        .filter(|f| f.is_mechanical())
        .map(|f| f.reference.as_str())
        .collect();
    let mounting_holes = facts
        .holes
        .iter()
        .filter(|h| !h.via && (!h.plated || h.owner.as_deref().is_some_and(|o| mechanical.contains(&o))))
        .map(|h| MountingHole {
            reference: h.owner.clone(),
            at: xy(h.at),
            diameter_mm: round3(h.drill.0.to_mm().min(h.drill.1.to_mm())),
            plated: h.plated,
        })
        .collect();
    MechanicalModel {
        format: FORMAT.to_owned(),
        source: source.to_owned(),
        board: BoardBody {
            bounds: board_box,
            thickness_mm: facts.thickness.map(|t| round3(t.to_mm())),
            outline: facts
                .edges
                .iter()
                .map(|e| match e {
                    EdgeShape::Open { start, end } => OutlineShape::Segment {
                        start: xy(*start),
                        end: xy(*end),
                    },
                    EdgeShape::Closed { bounds } => OutlineShape::Closed { bounds: box2(*bounds) },
                })
                .collect(),
            outline_closed: outline_check(facts).status == CheckStatus::Pass,
        },
        mounting_holes,
        keepouts: facts
            .keepouts
            .iter()
            .map(|k| KeepoutArea {
                name: k.name.clone(),
                layers: k.layers.clone(),
                polygon: k.outline.iter().map(|p| xy(*p)).collect(),
                forbids: k.forbids.clone(),
            })
            .collect(),
        connectors,
        tallest_top: MechanicalModel::tallest(&components, Side::Top),
        tallest_bottom: MechanicalModel::tallest(&components, Side::Bottom),
        components,
        step_path: step_path.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fab::Hole;
    use crate::geometry::Nm;
    use std::collections::BTreeMap;

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    #[test]
    fn a_board_becomes_a_mechanical_model() {
        let fp = |r: &str, lib: &str, at: Point, court: (Point, Point)| FootprintFacts {
            reference: r.into(),
            lib_id: lib.into(),
            layer: "F.Cu".into(),
            position: at,
            courtyard: Rect::from_points([court.0, court.1]),
            connected: !r.starts_with('H'),
            ..Default::default()
        };
        let mut u1 = fp("U1", "Lib:Odd", p(20.0, 15.0), (p(17.0, 12.0), p(23.0, 18.0)));
        u1.properties = BTreeMap::from([("Height".to_owned(), "4.2mm".to_owned())]);
        u1.layer = "B.Cu".into();
        let facts = FabFacts {
            thickness: Some(Nm(1_600_000)),
            edges: vec![EdgeShape::Closed {
                bounds: Rect::from_points([p(0.0, 0.0), p(40.0, 30.0)]).unwrap(),
            }],
            holes: vec![
                Hole {
                    owner: Some("H1".into()),
                    at: p(4.0, 4.0),
                    drill: (Nm(3_200_000), Nm(3_200_000)),
                    plated: true,
                    via: false,
                },
                Hole {
                    owner: Some("J2".into()),
                    at: p(20.0, 25.0),
                    drill: (Nm(1_000_000), Nm(1_000_000)),
                    plated: true,
                    via: false,
                },
            ],
            footprints: vec![
                fp(
                    "J1",
                    "Connector_USB:USB_C_Receptacle",
                    p(1.0, 15.0),
                    (p(-1.0, 11.0), p(6.0, 19.0)),
                ),
                fp(
                    "J2",
                    "Connector_PinHeader_2.54mm:PinHeader_1x02",
                    p(20.0, 25.0),
                    (p(18.0, 23.0), p(22.0, 28.0)),
                ),
                fp(
                    "H1",
                    "MountingHole:MountingHole_3.2mm_M3",
                    p(4.0, 4.0),
                    (p(2.0, 2.0), p(6.0, 6.0)),
                ),
                fp(
                    "C1",
                    "Capacitor_SMD:C_0603_1608Metric",
                    p(10.0, 10.0),
                    (p(9.0, 9.5), p(11.0, 10.5)),
                ),
                u1,
            ],
            ..Default::default()
        };
        let outline = Rect::from_points([p(0.0, 0.0), p(40.0, 30.0)]);
        let m = build(&facts, outline, "b.kicad_pcb", Some("b.step"));
        assert_eq!(m.format, FORMAT);
        assert_eq!(m.board.thickness_mm, Some(1.6));
        assert!(m.board.outline_closed);
        // Only H1's hole is a mounting hole; J2's pins are not.
        assert_eq!(m.mounting_holes.len(), 1);
        assert_eq!(
            (m.mounting_holes[0].at, m.mounting_holes[0].diameter_mm),
            ([4.0, 4.0], 3.2)
        );
        let j1 = &m.connectors[0];
        assert_eq!(
            (j1.edge, j1.distance_to_edge_mm),
            (Some(mech_model::Edge::Left), Some(-1.0))
        );
        let j2 = &m.connectors[1];
        assert_eq!(j2.edge, Some(mech_model::Edge::Bottom), "2 mm from the bottom edge");
        assert_eq!(m.components.len(), 4, "the mounting hole is not a component");
        let u = m.components.iter().find(|c| c.reference == "U1").unwrap();
        assert_eq!(
            (u.side, u.height_mm, u.height_source),
            (Side::Bottom, Some(4.2), HeightSource::Field)
        );
        assert_eq!(m.tallest_top.as_ref().unwrap().reference, "J2");
        assert_eq!(m.tallest_bottom.as_ref().unwrap().height_mm, 4.2);
        assert_eq!(m.step_path.as_deref(), Some("b.step"));
    }
}
