// SPDX-License-Identifier: GPL-3.0-or-later
//! What the manufacturing outputs and the mechanical model need from a `.kicad_pcb` file beyond
//! [`read_board`](crate::pcb::read_board): the board thickness, the `Edge.Cuts` shapes, silkscreen
//! extents, drilled holes, rule areas, and each footprint's attributes, fields, and 3D models.
//! Footprint drawings are stored relative to the footprint and placed here; footprint rule areas
//! are stored in board coordinates already.

use std::collections::BTreeMap;

use kicad_domain::board::{place_point, place_rect};
use kicad_domain::fab::{EdgeShape, FabFacts, FootprintFacts, Hole, Keepout, SilkItem};
use kicad_domain::{Nm, Point, Rect};

use crate::node::{parse, Node};
use crate::pcb::{is_yes, layer_of, net_of, nm, placement, point, shape_points, BoardError};

fn is_silk(layer: &str) -> bool {
    matches!(layer, "F.SilkS" | "B.SilkS" | "F.Silkscreen" | "B.Silkscreen")
}

fn stroke_width(item: &Node) -> Nm {
    let w = item
        .child("stroke")
        .and_then(|s| s.child_f64("width"))
        .or_else(|| item.child_f64("width"))
        .unwrap_or(0.0);
    nm(w)
}

/// A text item is hidden by `(hide yes)` on it or in its effects, or a bare `hide` (older files).
fn is_hidden(text: &Node) -> bool {
    is_yes(text, "hide") || text.child("effects").is_some_and(|e| is_yes(e, "hide"))
}

/// An estimate of the box a text takes: the stroke font is about 0.8 of its size wide per letter,
/// centred on its position (KiCad's default). A quarter turn swaps width and height.
fn text_box(center: Point, text: &str, node: &Node) -> Option<Rect> {
    let font = node.child("effects").and_then(|e| e.child("font"));
    let size = font.and_then(|f| f.child("size"));
    let (h, w) = (
        size.and_then(|s| s.arg_f64(0)).unwrap_or(1.0),
        size.and_then(|s| s.arg_f64(1)).unwrap_or(1.0),
    );
    let thickness = font.and_then(|f| f.child_f64("thickness")).unwrap_or(0.15);
    let letters = text.chars().count().max(1) as f64;
    let (mut width, mut height) = (letters * w * 0.8 + thickness, h + thickness);
    let angle = node
        .child("at")
        .and_then(|a| a.arg_f64(2))
        .unwrap_or(0.0)
        .rem_euclid(180.0);
    if (angle - 90.0).abs() < 45.0 {
        std::mem::swap(&mut width, &mut height);
    }
    let (hw, hh) = (nm(width / 2.0), nm(height / 2.0));
    Rect::from_points([
        Point::new(center.x - hw, center.y - hh),
        Point::new(center.x + hw, center.y + hh),
    ])
}

fn edge_shape(item: &Node, place: impl Fn(Point) -> Point) -> Option<EdgeShape> {
    let head = item.head()?;
    if head.ends_with("_line") || head.ends_with("_arc") {
        let start = place(item.child("start").and_then(point)?);
        let end = place(item.child("end").and_then(point)?);
        return Some(EdgeShape::Open { start, end });
    }
    let points: Vec<Point> = shape_points(item).into_iter().map(place).collect();
    Some(EdgeShape::Closed {
        bounds: Rect::from_points(points)?,
    })
}

fn keepout(zone: &Node, owner: Option<&str>) -> Option<Keepout> {
    let rules = zone.child("keepout")?;
    let layers: Vec<String> = match zone.child("layers") {
        Some(l) => l
            .items()
            .iter()
            .skip(1)
            .filter_map(|n| n.text().map(str::to_owned))
            .collect(),
        None => vec![layer_of(zone)],
    };
    let name = zone.child_text("name").map(str::to_owned).unwrap_or_default();
    Some(Keepout {
        name: match (name.is_empty(), owner) {
            (true, Some(r)) => format!("{r} rule area"),
            _ => name,
        },
        layers,
        outline: zone
            .child("polygon")
            .and_then(|p| p.child("pts"))
            .map(|p| p.children("xy").filter_map(point).collect())
            .unwrap_or_default(),
        forbids: rules
            .items()
            .iter()
            .filter(|r| r.is_list() && r.arg_text(0) == Some("not_allowed"))
            .filter_map(|r| r.head().map(str::to_owned))
            .collect(),
    })
}

fn hole(pad: &Node, owner: &str, at: Point) -> Option<Hole> {
    let kind = pad.arg_text(1)?;
    if kind != "thru_hole" && kind != "np_thru_hole" {
        return None;
    }
    let drill = pad.child("drill")?;
    let (a, b) = if drill.arg_text(0) == Some("oval") {
        let a = drill.arg_f64(1)?;
        (a, drill.arg_f64(2).unwrap_or(a))
    } else {
        let a = drill.arg_f64(0)?;
        (a, a)
    };
    Some(Hole {
        owner: Some(owner.to_owned()),
        at,
        drill: (nm(a), nm(b)),
        plated: kind == "thru_hole",
        via: false,
    })
}

fn footprint(item: &Node, table: &BTreeMap<i64, String>, facts: &mut FabFacts) {
    let (position, rotation_deg) = placement(item);
    let place = |p: Point| place_point(position, rotation_deg, p);
    let properties: BTreeMap<String, String> = item
        .children("property")
        .filter_map(|p| Some((p.arg_text(0)?.to_owned(), p.arg_text(1).unwrap_or_default().to_owned())))
        .collect();
    let reference = properties.get("Reference").cloned().unwrap_or_else(|| {
        item.children("fp_text")
            .find(|t| t.arg_text(0) == Some("reference"))
            .and_then(|t| t.arg_text(1))
            .unwrap_or_default()
            .to_owned()
    });
    let attr: Vec<&str> = item
        .child("attr")
        .map(|a| a.items().iter().skip(1).filter_map(|i| i.text()).collect())
        .unwrap_or_default();
    let flag = |name: &str| attr.contains(&name) || is_yes(item, name);
    let mut connected = false;
    let mut courtyard: Vec<Point> = Vec::new();
    for child in item.items() {
        match child.head() {
            Some("pad") => {
                let Some(local) = child.child("at").and_then(point) else {
                    continue;
                };
                connected |= !net_of(child, table).is_empty();
                if let Some(h) = hole(child, &reference, place(local)) {
                    facts.holes.push(h);
                }
            }
            Some("zone") => facts.keepouts.extend(keepout(child, Some(&reference))),
            Some("property" | "fp_text") if is_silk(&layer_of(child)) && !is_hidden(child) => {
                let what = match child.head() {
                    Some("property") => format!("{} field", child.arg_text(0).unwrap_or("text")),
                    _ => "text".to_owned(),
                };
                let text = child
                    .arg_text(1)
                    .unwrap_or_default()
                    .replace("${REFERENCE}", &reference);
                let Some(local) = child.child("at").and_then(point) else {
                    continue;
                };
                if let Some(bounds) = text_box(place(local), &text, child) {
                    facts.silk.push(SilkItem {
                        owner: Some(reference.clone()),
                        what,
                        layer: layer_of(child),
                        bounds,
                        estimated: true,
                    });
                }
            }
            Some(head @ ("fp_line" | "fp_rect" | "fp_arc" | "fp_circle" | "fp_poly")) => {
                let layer = layer_of(child);
                if layer.ends_with(".CrtYd") {
                    courtyard.extend(shape_points(child));
                } else if layer == "Edge.Cuts" {
                    facts.edges.extend(edge_shape(child, place));
                } else if is_silk(&layer) {
                    let points: Vec<Point> = shape_points(child).into_iter().map(place).collect();
                    if let Some(r) = Rect::from_points(points) {
                        facts.silk.push(SilkItem {
                            owner: Some(reference.clone()),
                            what: head.to_owned(),
                            layer,
                            bounds: r.inflate(Nm(stroke_width(child).0 / 2)),
                            estimated: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    facts.footprints.push(FootprintFacts {
        value: properties.get("Value").cloned().unwrap_or_default(),
        lib_id: item.arg_text(0).unwrap_or_default().to_owned(),
        layer: layer_of(item),
        position,
        rotation_deg,
        mount: attr
            .iter()
            .find(|a| matches!(**a, "smd" | "through_hole"))
            .map(|a| (*a).to_owned())
            .unwrap_or_default(),
        exclude_from_bom: flag("exclude_from_bom"),
        exclude_from_pos: flag("exclude_from_pos_files"),
        board_only: flag("board_only"),
        dnp: flag("dnp"),
        models: item
            .children("model")
            .filter(|m| !is_hidden(m))
            .filter_map(|m| m.arg_text(0).map(str::to_owned))
            .collect(),
        courtyard: Rect::from_points(courtyard).map(|r| place_rect(position, rotation_deg, r)),
        connected,
        properties,
        reference,
    });
}

pub fn read_fab_facts(text: &str) -> Result<FabFacts, BoardError> {
    let root = parse(text)?;
    if root.head() != Some("kicad_pcb") {
        return Err(BoardError::NotABoard);
    }
    let table: BTreeMap<i64, String> = root
        .children("net")
        .filter_map(|n| Some((n.arg_text(0)?.parse().ok()?, n.arg_text(1)?.to_owned())))
        .collect();
    let mut facts = FabFacts {
        thickness: root.child("general").and_then(|g| g.child_f64("thickness")).map(nm),
        ..Default::default()
    };
    for item in root.items() {
        match item.head() {
            Some("footprint") | Some("module") => footprint(item, &table, &mut facts),
            Some("via") => {
                if let (Some(at), Some(drill)) = (item.child("at").and_then(point), item.child_f64("drill")) {
                    facts.holes.push(Hole {
                        owner: None,
                        at,
                        drill: (nm(drill), nm(drill)),
                        plated: true,
                        via: true,
                    });
                }
            }
            Some("zone") => facts.keepouts.extend(keepout(item, None)),
            Some("gr_text") if is_silk(&layer_of(item)) && !is_hidden(item) => {
                if let Some(bounds) = item
                    .child("at")
                    .and_then(point)
                    .and_then(|at| text_box(at, item.arg_text(0).unwrap_or_default(), item))
                {
                    facts.silk.push(SilkItem {
                        owner: None,
                        what: "gr_text".into(),
                        layer: layer_of(item),
                        bounds,
                        estimated: true,
                    });
                }
            }
            Some(head) if head.starts_with("gr_") => {
                let layer = layer_of(item);
                if layer == "Edge.Cuts" {
                    facts.edges.extend(edge_shape(item, |p| p));
                } else if is_silk(&layer) {
                    if let Some(r) = Rect::from_points(shape_points(item)) {
                        facts.silk.push(SilkItem {
                            owner: None,
                            what: head.to_owned(),
                            layer,
                            bounds: r.inflate(Nm(stroke_width(item).0 / 2)),
                            estimated: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_domain::fab::{outline_check, silk_check, CheckStatus};

    const FIXTURE: &str = include_str!("../../../tests/fixtures/v11/regulator-board/regulator.kicad_pcb");

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    #[test]
    fn the_regulator_board_is_read() {
        let facts = read_fab_facts(FIXTURE).unwrap();
        assert_eq!(facts.thickness, Some(nm(1.6)));
        assert_eq!(facts.footprints.len(), 11);
        assert!(facts.edges.is_empty(), "the fixture has no outline");
        assert_eq!(outline_check(&facts).status, CheckStatus::Fail);
        let j1 = facts.footprints.iter().find(|f| f.reference == "J1").unwrap();
        assert_eq!(j1.mount, "through_hole");
        assert_eq!(j1.value, "5V_IN");
        assert!(j1.connected && !j1.exclude_from_bom && j1.needs_bom_line());
        assert_eq!(j1.models.len(), 1);
        assert!(j1.models[0].ends_with("PinHeader_1x02_P2.54mm_Vertical.step"));
        assert!(j1.courtyard.is_some());
        // J1's two pins are the only holes.
        assert_eq!(facts.holes.len(), 2);
        assert!(facts.holes.iter().all(|h| h.plated && h.drill == (nm(1.0), nm(1.0))));
        assert_eq!(facts.holes[1].at, p(15.655, 1.795 + 2.54));
        // J1's reference sits 2.38 mm above it; the hidden generator field is not counted.
        let refs: Vec<&SilkItem> = facts
            .silk
            .iter()
            .filter(|s| s.owner.as_deref() == Some("J1") && s.estimated)
            .collect();
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].what, "Reference field");
        let c = refs[0].bounds;
        assert_eq!(
            Point::new(Nm((c.min.x.0 + c.max.x.0) / 2), Nm((c.min.y.0 + c.max.y.0) / 2)),
            p(15.655, 1.795 - 2.38)
        );
        // An outline at y = 0 leaves J1's reference above the board.
        let silk = silk_check(&facts, Rect::from_points([p(-1.0, 0.0), p(18.0, 16.0)]));
        assert_eq!(silk.status, CheckStatus::Warning);
        assert!(
            silk.items.iter().any(|i| i == "Reference field of J1 on F.SilkS"),
            "{:?}",
            silk.items
        );
    }

    #[test]
    fn edges_attributes_and_rule_areas_are_read() {
        let text = r#"(kicad_pcb (version 20261018) (general (thickness 1.2))
            (footprint "MountingHole:MountingHole_3.2mm_M3" (layer "F.Cu") (transform (translate 5 5) (rotate 0))
              (property "Reference" "H1" (at 0 -4 0) (layer "F.SilkS") (hide yes))
              (attr exclude_from_pos_files exclude_from_bom)
              (pad "" np_thru_hole circle (at 0 0) (size 3.2 3.2) (drill 3.2) (layers "*.Cu" "*.Mask")))
            (footprint "RF:Module" (layer "B.Cu") (transform (translate 20 10) (rotate 90))
              (property "Reference" "U1" (at 0 0 0) (layer "B.SilkS"))
              (property "Height" "3.1")
              (attr smd dnp)
              (pad "1" thru_hole oval (at 1 0) (size 1 2) (drill oval 0.6 1.2) (layers "*.Cu") (net "GND"))
              (fp_line (start 0 0) (end 2 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
              (zone (layers "F.Cu" "B.Cu") (keepout (tracks not_allowed) (vias not_allowed) (pads allowed)
                (copperpour not_allowed) (footprints allowed)) (polygon (pts (xy 18 0) (xy 22 0) (xy 22 4)))))
            (via (at 1 1) (size 0.6) (drill 0.3) (net "GND"))
            (gr_line (start 0 0) (end 30 0) (layer "Edge.Cuts"))
            (gr_arc (start 30 0) (mid 31 1) (end 30 2) (layer "Edge.Cuts"))
            (gr_text "REV A" (at 10 -1 0) (layer "F.SilkS") (effects (font (size 1 1) (thickness 0.15)))))"#;
        let facts = read_fab_facts(text).unwrap();
        assert_eq!(facts.thickness, Some(nm(1.2)));
        let h1 = &facts.footprints[0];
        assert!(h1.exclude_from_bom && h1.exclude_from_pos && h1.is_mechanical() && !h1.needs_bom_line());
        let u1 = &facts.footprints[1];
        assert!(u1.dnp && u1.is_back() && u1.connected);
        assert_eq!(
            (u1.mount.as_str(), u1.properties.get("Height").map(String::as_str)),
            ("smd", Some("3.1"))
        );
        assert_eq!(facts.holes.len(), 3);
        assert!(!facts.holes[0].plated);
        assert_eq!(facts.holes[1].drill, (nm(0.6), nm(1.2)));
        assert!(facts.holes[2].via);
        // The footprint's line is placed: a quarter turn takes (2, 0) to (0, -2) around (20, 10).
        assert_eq!(
            facts.edges[0],
            EdgeShape::Open {
                start: p(20.0, 10.0),
                end: p(20.0, 8.0)
            }
        );
        assert_eq!(facts.edges.len(), 3);
        let k = &facts.keepouts[0];
        assert_eq!(k.name, "U1 rule area");
        assert_eq!(k.forbids, ["tracks", "vias", "copperpour"]);
        assert_eq!(k.outline[0], p(18.0, 0.0), "rule areas are in board coordinates");
        // H1's reference is hidden; U1's and the board text count.
        let silk: Vec<_> = facts.silk.iter().map(|s| (s.owner.clone(), s.what.as_str())).collect();
        assert_eq!(silk, [(Some("U1".into()), "Reference field"), (None, "gr_text")]);
    }
}
