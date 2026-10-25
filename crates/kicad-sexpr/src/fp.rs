// SPDX-License-Identifier: GPL-3.0-or-later
//! Footprints from a `.kicad_mod` file: the pads, the courtyard, and the text a check needs.

use kicad_domain::footprint::{FootprintInfo, FootprintPad};
use kicad_domain::{Nm, Point, Rect};

use crate::node::{parse, Node, ParseError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FootprintError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("not a footprint: expected (footprint …)")]
    NotAFootprint,
}

fn nm(v: f64) -> Nm {
    Nm((v * 1e6).round() as i64)
}

fn point(n: &Node) -> Option<Point> {
    Some(Point::new(nm(n.arg_f64(0)?), nm(n.arg_f64(1)?)))
}

/// Parses a footprint; `id` is its `Library:Name`.
pub fn parse_footprint(id: &str, text: &str) -> Result<FootprintInfo, FootprintError> {
    let root = parse(text)?;
    if !matches!(root.head(), Some("footprint") | Some("module")) {
        return Err(FootprintError::NotAFootprint);
    }
    let pads = root
        .children("pad")
        .filter_map(|p| {
            Some(FootprintPad {
                number: p.arg_text(0)?.to_owned(),
                kind: p.arg_text(1).unwrap_or("smd").to_owned(),
                at: point(p.child("at")?)?,
                size: {
                    let s = p.child("size")?;
                    (
                        nm(s.arg_f64(0)?),
                        nm(s.arg_f64(1).unwrap_or_else(|| s.arg_f64(0).unwrap_or(0.0))),
                    )
                },
            })
        })
        .collect();

    // The courtyard: every point of every shape on a courtyard layer.
    let mut points: Vec<Point> = Vec::new();
    for item in root.items() {
        let on_courtyard = item
            .child("layer")
            .and_then(|l| l.arg_text(0))
            .is_some_and(|l| l.ends_with(".CrtYd"));
        if !on_courtyard {
            continue;
        }
        match item.head() {
            Some("fp_rect") | Some("fp_line") => {
                points.extend(item.child("start").and_then(point));
                points.extend(item.child("end").and_then(point));
            }
            Some("fp_poly") => {
                if let Some(pts) = item.child("pts") {
                    points.extend(pts.children("xy").filter_map(point));
                }
            }
            Some("fp_circle") => {
                if let (Some(c), Some(e)) = (item.child("center").and_then(point), item.child("end").and_then(point)) {
                    let r = ((e.x.0 - c.x.0) as f64).hypot((e.y.0 - c.y.0) as f64).round() as i64;
                    points.push(Point::new(c.x - Nm(r), c.y - Nm(r)));
                    points.push(Point::new(c.x + Nm(r), c.y + Nm(r)));
                }
            }
            _ => {}
        }
    }
    Ok(FootprintInfo {
        id: id.to_owned(),
        description: root.child_text("descr").unwrap_or_default().to_owned(),
        tags: root.child_text("tags").unwrap_or_default().to_owned(),
        pads,
        courtyard: Rect::from_points(points),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const R0603: &str = r#"(footprint "R_0603_1608Metric"
	(version 20240108)
	(generator "pcbnew")
	(layer "F.Cu")
	(descr "Resistor SMD 0603")
	(tags "resistor")
	(fp_rect (start -1.48 -0.73) (end 1.48 0.73) (stroke (width 0.05) (type solid)) (fill none) (layer "F.CrtYd"))
	(fp_line (start -0.2 -0.5) (end 0.2 -0.5) (layer "F.SilkS"))
	(pad "1" smd roundrect (at -0.825 0) (size 0.8 0.95) (layers "F.Cu" "F.Mask" "F.Paste"))
	(pad "2" smd roundrect (at 0.825 0) (size 0.8 0.95) (layers "F.Cu" "F.Mask" "F.Paste"))
	(pad "" np_thru_hole circle (at 0 3) (size 1 1) (drill 1) (layers "*.Cu"))
)"#;

    #[test]
    fn pads_courtyard_and_text_are_read() {
        let fp = parse_footprint("Resistor_SMD:R_0603_1608Metric", R0603).unwrap();
        assert_eq!(fp.pads.len(), 3);
        assert_eq!(
            fp.pad_numbers().into_iter().collect::<Vec<_>>(),
            ["1", "2"],
            "a plain mounting hole is not a pad to connect"
        );
        let court = fp.courtyard.unwrap();
        assert_eq!((court.width(), court.height()), (nm(2.96), nm(1.46)));
        assert_eq!(fp.description, "Resistor SMD 0603");
        assert_eq!(fp.tags, "resistor");
        assert_eq!(fp.pads[0].at, Point::new(nm(-0.825), nm(0.0)));
    }

    #[test]
    fn something_else_is_refused() {
        assert_eq!(
            parse_footprint("X:Y", "(kicad_sch)"),
            Err(FootprintError::NotAFootprint)
        );
    }
}
