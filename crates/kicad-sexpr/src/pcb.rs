// SPDX-License-Identifier: GPL-3.0-or-later
//! A board from a `.kicad_pcb` file: footprints and their pads, tracks, vias, zones, and the
//! outline. Both file layouts are read: KiCad 10 and earlier place a footprint with `(at x y rot)`
//! and name nets by number and name, KiCad 10.99 and later use `(transform (translate …) (rotate …))`
//! and name nets by name only.

use std::collections::BTreeMap;

use kicad_domain::board::{
    place_point, place_rect, Board, BoardEdge, BoardFootprint, BoardPad, BoardTrack, BoardVia, BoardZone,
};
use kicad_domain::{Nm, Point, Rect};

use crate::node::{parse, Node, ParseError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoardError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("not a board: expected (kicad_pcb …)")]
    NotABoard,
}

pub(crate) fn nm(v: f64) -> Nm {
    Nm((v * 1e6).round() as i64)
}

pub(crate) fn point(n: &Node) -> Option<Point> {
    Some(Point::new(nm(n.arg_f64(0)?), nm(n.arg_f64(1)?)))
}

pub(crate) fn layer_of(n: &Node) -> String {
    n.child("layer")
        .and_then(|l| l.arg_text(0))
        .unwrap_or_default()
        .to_owned()
}

/// A net named on an item: `(net "GND")`, or `(net 3 "GND")` / `(net 3)` with a table.
pub(crate) fn net_of(n: &Node, table: &BTreeMap<i64, String>) -> String {
    let Some(net) = n.child("net") else {
        return n.child_text("net_name").unwrap_or_default().to_owned();
    };
    if let Some(name) = net.arg_text(1) {
        return name.to_owned();
    }
    match net.arg_text(0) {
        Some(text) => match text.parse::<i64>() {
            Ok(code) => table.get(&code).cloned().unwrap_or_default(),
            Err(_) => text.to_owned(),
        },
        None => String::new(),
    }
}

pub(crate) fn is_yes(n: &Node, name: &str) -> bool {
    n.child_text(name) == Some("yes") || n.items().iter().any(|i| i.text() == Some(name))
}

/// Position and rotation of a footprint, in either layout.
pub(crate) fn placement(fp: &Node) -> (Point, f64) {
    if let Some(at) = fp.child("at") {
        return (point(at).unwrap_or_default(), at.arg_f64(2).unwrap_or(0.0));
    }
    if let Some(t) = fp.child("transform") {
        let at = t.child("translate").and_then(point).unwrap_or_default();
        return (at, t.child_f64("rotate").unwrap_or(0.0));
    }
    (Point::default(), 0.0)
}

fn bounds(points: impl IntoIterator<Item = Point>) -> Option<Rect> {
    Rect::from_points(points)
}

/// Every point of a drawing item (lines, rectangles, arcs, circles, polygons).
pub(crate) fn shape_points(item: &Node) -> Vec<Point> {
    let mut out = Vec::new();
    for key in ["start", "mid", "end", "center"] {
        out.extend(item.child(key).and_then(point));
    }
    if let Some(pts) = item.child("pts") {
        out.extend(pts.children("xy").filter_map(point));
    }
    // A circle is its centre and a point on it.
    if item.head() == Some("gr_circle") || item.head() == Some("fp_circle") {
        if let (Some(c), Some(e)) = (item.child("center").and_then(point), item.child("end").and_then(point)) {
            let r = ((e.x.0 - c.x.0) as f64).hypot((e.y.0 - c.y.0) as f64).round() as i64;
            out = vec![
                Point::new(c.x - Nm(r), c.y - Nm(r)),
                Point::new(c.x + Nm(r), c.y + Nm(r)),
            ];
        }
    }
    out
}

pub fn read_board(text: &str) -> Result<Board, BoardError> {
    let root = parse(text)?;
    if root.head() != Some("kicad_pcb") {
        return Err(BoardError::NotABoard);
    }
    let mut board = Board::default();

    if let Some(layers) = root.child("layers") {
        for layer in layers
            .items()
            .iter()
            .filter(|l| l.is_list() && l.head().is_some_and(|h| h.parse::<i64>().is_ok()))
        {
            let (Some(name), Some(kind)) = (layer.arg_text(0), layer.arg_text(1)) else {
                continue;
            };
            if matches!(kind, "signal" | "power" | "mixed" | "jumper") {
                board.copper_layers.push(name.to_owned());
            }
        }
    }
    let table: BTreeMap<i64, String> = root
        .children("net")
        .filter_map(|n| Some((n.arg_text(0)?.parse().ok()?, n.arg_text(1)?.to_owned())))
        .collect();
    let mut nets: std::collections::BTreeSet<String> = table.values().filter(|n| !n.is_empty()).cloned().collect();

    let mut outline: Vec<Point> = Vec::new();
    let copper = board.copper_layers.clone();
    for item in root.items() {
        match item.head() {
            Some("footprint") | Some("module") => {
                let (position, rotation_deg) = placement(item);
                let property = |name: &str| {
                    item.children("property")
                        .find(|p| p.arg_text(0) == Some(name))
                        .and_then(|p| p.arg_text(1))
                        .map(str::to_owned)
                };
                let reference = property("Reference").or_else(|| {
                    // Older files keep the reference in an fp_text item.
                    item.children("fp_text")
                        .find(|t| t.arg_text(0) == Some("reference"))
                        .and_then(|t| t.arg_text(1))
                        .map(str::to_owned)
                });
                let pads: Vec<BoardPad> = item
                    .children("pad")
                    .filter_map(|p| {
                        let at = p.child("at")?;
                        let local = point(at)?;
                        let net = net_of(p, &table);
                        if !net.is_empty() {
                            nets.insert(net.clone());
                        }
                        let kind = p.arg_text(1).unwrap_or("smd").to_owned();
                        let mut layers: Vec<String> = Vec::new();
                        for l in p.child("layers").map(|l| l.items()).unwrap_or_default().iter().skip(1) {
                            match l.text() {
                                Some("*.Cu") | Some("F&B.Cu") => layers.extend(copper.iter().cloned()),
                                Some(name) if name.ends_with(".Cu") => layers.push(name.to_owned()),
                                _ => {}
                            }
                        }
                        layers.dedup();
                        Some(BoardPad {
                            number: p.arg_text(0)?.to_owned(),
                            at: place_point(position, rotation_deg, local),
                            net,
                            size: p
                                .child("size")
                                .and_then(|s| Some((nm(s.arg_f64(0)?), nm(s.arg_f64(1)?)))),
                            // The angle in the file is the pad's own turn on the board.
                            angle_deg: at.arg_f64(2).unwrap_or(0.0),
                            layers,
                            kind,
                        })
                    })
                    .collect();
                // The courtyard, in footprint coordinates, then placed.
                let mut local_court: Vec<Point> = Vec::new();
                for shape in item.items() {
                    if matches!(
                        shape.head(),
                        Some("fp_rect" | "fp_line" | "fp_poly" | "fp_circle" | "fp_arc")
                    ) && layer_of(shape).ends_with(".CrtYd")
                    {
                        local_court.extend(shape_points(shape));
                    }
                }
                board.footprints.push(BoardFootprint {
                    id: item
                        .child_text("uuid")
                        .or_else(|| item.child_text("tstamp"))
                        .unwrap_or_default()
                        .to_owned(),
                    reference: reference.unwrap_or_default(),
                    value: property("Value").unwrap_or_default(),
                    lib_id: item.arg_text(0).unwrap_or_default().to_owned(),
                    layer: layer_of(item),
                    position,
                    rotation_deg,
                    locked: is_yes(item, "locked"),
                    dnp: item
                        .child("attr")
                        .is_some_and(|a| a.items().iter().any(|i| i.text() == Some("dnp"))),
                    pads,
                    courtyard: bounds(local_court.iter().copied()).map(|r| place_rect(position, rotation_deg, r)),
                    courtyard_local: bounds(local_court),
                });
            }
            Some("segment") | Some("arc") => {
                if let (Some(start), Some(end)) =
                    (item.child("start").and_then(point), item.child("end").and_then(point))
                {
                    let net = net_of(item, &table);
                    if !net.is_empty() {
                        nets.insert(net.clone());
                    }
                    board.tracks.push(BoardTrack {
                        id: item.child_text("uuid").unwrap_or_default().to_owned(),
                        start,
                        end,
                        width: nm(item.child_f64("width").unwrap_or(0.0)),
                        layer: layer_of(item),
                        net,
                        locked: is_yes(item, "locked"),
                    });
                }
            }
            Some("via") => {
                if let Some(at) = item.child("at").and_then(point) {
                    let net = net_of(item, &table);
                    if !net.is_empty() {
                        nets.insert(net.clone());
                    }
                    board.vias.push(BoardVia {
                        id: item.child_text("uuid").unwrap_or_default().to_owned(),
                        at,
                        diameter: nm(item.child_f64("size").unwrap_or(0.0)),
                        drill: nm(item.child_f64("drill").unwrap_or(0.0)),
                        net,
                    });
                }
            }
            Some("zone") => {
                let layers: Vec<String> = match item.child("layers") {
                    Some(l) => l
                        .items()
                        .iter()
                        .skip(1)
                        .filter_map(|n| n.text().map(str::to_owned))
                        .collect(),
                    None => vec![layer_of(item)].into_iter().filter(|l| !l.is_empty()).collect(),
                };
                let net = net_of(item, &table);
                if !net.is_empty() {
                    nets.insert(net.clone());
                }
                board.zones.push(BoardZone {
                    id: item.child_text("uuid").unwrap_or_default().to_owned(),
                    name: item.child_text("name").unwrap_or_default().to_owned(),
                    layers,
                    net,
                    priority: item.child_f64("priority").unwrap_or(0.0) as u32,
                    bounds: item
                        .child("polygon")
                        .and_then(|p| p.child("pts"))
                        .and_then(|p| bounds(p.children("xy").filter_map(point))),
                    locked: is_yes(item, "locked"),
                    filled: item.child("filled_polygon").is_some(),
                });
            }
            Some(head) if head.starts_with("gr_") => {
                board.drawings += 1;
                if layer_of(item) == "Edge.Cuts" {
                    let points = shape_points(item);
                    board.edges.push(BoardEdge {
                        id: item.child_text("uuid").unwrap_or_default().to_owned(),
                        kind: match head {
                            "gr_rect" => "rectangle",
                            "gr_line" => "segment",
                            "gr_arc" => "arc",
                            "gr_circle" => "circle",
                            "gr_poly" => "polygon",
                            _ => "other",
                        }
                        .to_owned(),
                        bounds: bounds(points.iter().copied()),
                        locked: is_yes(item, "locked"),
                    });
                    outline.extend(points);
                }
            }
            _ => {}
        }
    }
    board.outline = bounds(outline);
    board.nets = nets.into_iter().collect();
    Ok(board)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../tests/fixtures/v11/regulator-board/regulator.kicad_pcb");

    #[test]
    fn the_regulator_board_is_read() {
        let board = read_board(FIXTURE).unwrap();
        assert_eq!(board.footprints.len(), 11);
        assert_eq!(board.copper_layers, ["F.Cu", "B.Cu"]);
        assert!(board.tracks.is_empty() && board.vias.is_empty() && board.zones.is_empty());
        assert!(board.outline.is_none());
        for want in ["GND", "+5V", "+3V3", "/VBUS", "/VBUS_F", "Net-(D2-A)"] {
            assert!(board.nets.contains(&want.to_owned()), "{want} in {:?}", board.nets);
        }
        let j1 = board.footprints.iter().find(|f| f.reference == "J1").unwrap();
        assert_eq!(j1.lib_id, "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical");
        assert_eq!(j1.layer, "F.Cu");
        assert_eq!(j1.position, Point::from_mm(15.655, 1.795).unwrap());
        assert_eq!(j1.pads.len(), 2);
        // Pad 2 sits 2.54 mm below the origin of the unrotated footprint.
        assert_eq!(j1.pads[1].at, Point::from_mm(15.655, 1.795 + 2.54).unwrap());
        assert_eq!(j1.pads[0].net, "/VBUS");
        assert!(
            j1.courtyard.is_some(),
            "the courtyard comes from the footprint's own shapes"
        );
    }

    #[test]
    fn both_layouts_place_a_footprint() {
        let old = r#"(kicad_pcb (version 20240108)
            (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (44 "Edge.Cuts" user))
            (net 0 "") (net 1 "GND")
            (footprint "L:F" (layer "F.Cu") (at 10 20 90)
              (property "Reference" "R1" (at 0 0))
              (pad "1" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 1 "GND"))
              (fp_rect (start -2 -1) (end 2 1) (layer "F.CrtYd")))
            (segment (start 0 0) (end 5 0) (width 0.25) (layer "F.Cu") (net 1))
            (gr_rect (start 0 0) (end 50 40) (layer "Edge.Cuts")))"#;
        let board = read_board(old).unwrap();
        let f = &board.footprints[0];
        assert_eq!(
            (f.position, f.rotation_deg),
            (Point::from_mm(10.0, 20.0).unwrap(), 90.0)
        );
        // A quarter turn takes the pad at +1 mm to the right of the origin to 1 mm above it.
        assert_eq!(f.pads[0].at, Point::from_mm(10.0, 19.0).unwrap());
        assert_eq!(f.pads[0].net, "GND");
        assert_eq!(board.tracks[0].net, "GND", "a net number is looked up in the table");
        assert_eq!(
            board.outline,
            Rect::from_points([Point::from_mm(0.0, 0.0).unwrap(), Point::from_mm(50.0, 40.0).unwrap()])
        );
        let court = f.courtyard.unwrap();
        assert_eq!(
            (court.width(), court.height()),
            (nm(2.0), nm(4.0)),
            "the courtyard turns with the part"
        );
        let local = f.courtyard_local.unwrap();
        assert_eq!((local.width(), local.height()), (nm(4.0), nm(2.0)), "unturned");
        assert_eq!(board.edges.len(), 1);
        assert_eq!(board.edges[0].kind, "rectangle");
        assert!(!board.edges[0].locked);

        let new = r#"(kicad_pcb (version 20261018)
            (footprint "L:F" (layer "B.Cu") (transform (translate 3 4) (rotate 180) (scale 1 1))
              (property "Reference" "C1" (at 0 0 0))
              (pad "1" smd rect (at 2 0) (size 1 1) (layers "B.Cu") (net "VCC")))
            (via (at 1 1) (size 0.8) (drill 0.4) (net "VCC")))"#;
        let board = read_board(new).unwrap();
        let f = &board.footprints[0];
        assert_eq!((f.layer.as_str(), f.rotation_deg), ("B.Cu", 180.0));
        assert_eq!(f.pads[0].at, Point::from_mm(1.0, 4.0).unwrap());
        assert_eq!(board.vias[0].net, "VCC");
        assert_eq!(board.nets, ["VCC"]);
    }

    #[test]
    fn something_else_is_refused() {
        assert_eq!(read_board("(kicad_sch)"), Err(BoardError::NotABoard));
    }
}
