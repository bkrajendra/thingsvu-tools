// SPDX-License-Identifier: GPL-3.0-or-later
//! A circuit board as the tools see it (Phase 11): footprints with their pads, tracks, vias,
//! zones, and the board outline, read the same way from a live KiCad and from a `.kicad_pcb` file,
//! and a review of how it is laid out. Coordinates are in nanometres, y down, as on the schematic.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::geometry::{Nm, Point, Rect};
use crate::schematic::SchematicDesign;
use crate::validation::Severity;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardPad {
    pub number: String,
    /// `smd`, `thru_hole`, `np_thru_hole`, or `connect`.
    pub kind: String,
    /// Where the pad's centre is on the board.
    pub at: Point,
    pub net: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardFootprint {
    pub id: String,
    pub reference: String,
    pub value: String,
    /// `Library:Name`.
    pub lib_id: String,
    /// `F.Cu` or `B.Cu`.
    pub layer: String,
    pub position: Point,
    /// Degrees, counter-clockwise on screen.
    pub rotation_deg: f64,
    pub locked: bool,
    pub dnp: bool,
    pub pads: Vec<BoardPad>,
    /// The courtyard's bounding box on the board, when known.
    pub courtyard: Option<Rect>,
}

impl BoardFootprint {
    /// The area to treat as the part's extent: its courtyard, else a box around its pads.
    pub fn extent(&self) -> Option<Rect> {
        self.courtyard
            .or_else(|| Rect::from_points(self.pads.iter().map(|p| p.at)).map(|r| r.inflate(Nm(500_000))))
    }

    pub fn is_back(&self) -> bool {
        self.layer.starts_with("B.")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardTrack {
    pub id: String,
    pub start: Point,
    pub end: Point,
    pub width: Nm,
    pub layer: String,
    pub net: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardVia {
    pub id: String,
    pub at: Point,
    pub diameter: Nm,
    pub drill: Nm,
    pub net: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardZone {
    pub id: String,
    pub name: String,
    pub layers: Vec<String>,
    pub net: String,
    pub priority: u32,
    /// The area of the zone's outline.
    pub bounds: Option<Rect>,
    pub locked: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Board {
    pub footprints: Vec<BoardFootprint>,
    pub tracks: Vec<BoardTrack>,
    pub vias: Vec<BoardVia>,
    pub zones: Vec<BoardZone>,
    /// The bounding box of everything on `Edge.Cuts`; none when the board has no outline.
    pub outline: Option<Rect>,
    /// Copper layers, front to back.
    pub copper_layers: Vec<String>,
    pub nets: Vec<String>,
    /// Graphic items and text on the board itself (not in footprints).
    pub drawings: usize,
}

/// Rotates `local` by `deg` degrees counter-clockwise on screen (y down) and adds `origin`.
pub fn place_point(origin: Point, deg: f64, local: Point) -> Point {
    let (s, c) = deg.to_radians().sin_cos();
    let (x, y) = (local.x.0 as f64, local.y.0 as f64);
    Point::new(
        Nm(origin.x.0 + (x * c + y * s).round() as i64),
        Nm(origin.y.0 + (-x * s + y * c).round() as i64),
    )
}

/// The bounding box of a footprint-local rectangle placed on the board.
pub fn place_rect(origin: Point, deg: f64, local: Rect) -> Rect {
    let corners = [
        local.min,
        local.max,
        Point::new(local.min.x, local.max.y),
        Point::new(local.max.x, local.min.y),
    ];
    Rect::from_points(corners.into_iter().map(|c| place_point(origin, deg, c))).expect("four corners")
}

impl Board {
    /// A short summary for clients.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        BTreeMap::from([
            ("footprints", self.footprints.len()),
            ("pads", self.footprints.iter().map(|f| f.pads.len()).sum()),
            ("tracks", self.tracks.len()),
            ("vias", self.vias.len()),
            ("zones", self.zones.len()),
            ("nets", self.nets.len()),
            ("drawings", self.drawings),
        ])
    }
}

// ---------------------------------------------------------------- review

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoardIssue {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub references: Vec<String>,
}

fn issue(code: &'static str, severity: Severity, message: String, references: Vec<String>) -> BoardIssue {
    BoardIssue {
        code,
        severity,
        message,
        references,
    }
}

fn is_power_net(name: &str) -> bool {
    let n = name.trim_start_matches('/').to_ascii_uppercase();
    n.starts_with('+') || n.starts_with("VCC") || n.starts_with("VDD") || n.starts_with("VBUS") || n.starts_with("VIN")
}

fn is_ground_net(name: &str) -> bool {
    crate::layout::is_ground_name(name)
}

fn centre(r: Rect) -> Point {
    Point::new(Nm((r.min.x.0 + r.max.x.0) / 2), Nm((r.min.y.0 + r.max.y.0) / 2))
}

fn mm(n: i64) -> f64 {
    n as f64 / 1e6
}

fn distance(a: Point, b: Point) -> f64 {
    mm(((a.x.0 - b.x.0) as f64).hypot((a.y.0 - b.y.0) as f64).round() as i64)
}

/// How far a decoupling capacitor may be from the part it serves before the review says so.
pub const DECOUPLING_MAX_MM: f64 = 10.0;
/// How far from the outline a connector may sit before the review says so.
pub const CONNECTOR_EDGE_MAX_MM: f64 = 15.0;

/// Reviews the layout of `board`; with the board's schematic, also compares the two.
pub fn review(board: &Board, schematic: Option<&SchematicDesign>) -> Vec<BoardIssue> {
    let mut issues = Vec::new();

    if board.footprints.is_empty() {
        issues.push(issue(
            "NO_FOOTPRINTS",
            Severity::Warning,
            "the board has no footprints; update it from the schematic".into(),
            vec![],
        ));
    }
    if board.outline.is_none() && !board.footprints.is_empty() {
        issues.push(issue(
            "NO_OUTLINE",
            Severity::Warning,
            "the board has no outline on Edge.Cuts, so nothing can be placed inside it yet".into(),
            vec![],
        ));
    }

    // Against the schematic.
    if let Some(sch) = schematic {
        let parts: BTreeMap<&str, &crate::schematic::DesignSymbol> = sch
            .symbols
            .iter()
            .filter(|s| !s.reference.starts_with('#') && !s.exclude_from_board)
            .map(|s| (s.reference.as_str(), s))
            .collect();
        let on_board: BTreeSet<&str> = board.footprints.iter().map(|f| f.reference.as_str()).collect();
        let missing: Vec<String> = parts
            .keys()
            .filter(|r| !on_board.contains(**r))
            .map(|r| (*r).to_owned())
            .collect();
        if !missing.is_empty() {
            issues.push(issue(
                "NO_FOOTPRINT_FOR_PART",
                Severity::Warning,
                format!(
                    "{} schematic part(s) have no footprint on the board; update the board from the schematic",
                    missing.len()
                ),
                missing,
            ));
        }
        let extra: Vec<String> = board
            .footprints
            .iter()
            .filter(|f| !parts.contains_key(f.reference.as_str()))
            .map(|f| f.reference.clone())
            .collect();
        if !extra.is_empty() {
            issues.push(issue(
                "FOOTPRINT_WITHOUT_PART",
                Severity::Warning,
                format!("{} footprint(s) have no part in the schematic", extra.len()),
                extra,
            ));
        }
        let differ: Vec<String> = board
            .footprints
            .iter()
            .filter(|f| {
                parts
                    .get(f.reference.as_str())
                    .is_some_and(|p| !p.footprint.is_empty() && p.footprint != f.lib_id)
            })
            .map(|f| f.reference.clone())
            .collect();
        if !differ.is_empty() {
            issues.push(issue(
                "FOOTPRINT_DIFFERS",
                Severity::Warning,
                "these footprints are not the ones the schematic names; update the board from the schematic".into(),
                differ,
            ));
        }
    }

    // Overlaps and the outline.
    let extents: Vec<(&BoardFootprint, Rect)> = board
        .footprints
        .iter()
        .filter_map(|f| f.extent().map(|r| (f, r)))
        .collect();
    let mut overlaps = Vec::new();
    for (i, (a, ra)) in extents.iter().enumerate() {
        for (b, rb) in &extents[i + 1..] {
            // Parts on opposite sides do not collide, unless one goes through the board.
            let through = |f: &BoardFootprint| f.pads.iter().any(|p| p.kind == "thru_hole");
            if (a.is_back() == b.is_back() || through(a) || through(b)) && ra.overlaps(*rb) {
                overlaps.push(format!("{} and {}", a.reference, b.reference));
            }
        }
    }
    if !overlaps.is_empty() {
        let shown: Vec<String> = overlaps.iter().take(12).cloned().collect();
        issues.push(issue(
            "FOOTPRINTS_OVERLAP",
            Severity::Warning,
            format!(
                "{} pair(s) of footprints overlap: {}{}",
                overlaps.len(),
                shown.join("; "),
                if overlaps.len() > shown.len() { "; …" } else { "" }
            ),
            vec![],
        ));
    }
    if let Some(outline) = board.outline {
        let outside: Vec<String> = extents
            .iter()
            .filter(|(_, r)| {
                r.min.x < outline.min.x || r.min.y < outline.min.y || r.max.x > outline.max.x || r.max.y > outline.max.y
            })
            .map(|(f, _)| f.reference.clone())
            .collect();
        if !outside.is_empty() {
            issues.push(issue(
                "OUTSIDE_OUTLINE",
                Severity::Warning,
                format!("{} footprint(s) reach outside the board outline", outside.len()),
                outside,
            ));
        }
        // Connectors belong at the edge.
        let far: Vec<String> = extents
            .iter()
            .filter(|(f, _)| f.reference.starts_with('J') || f.reference.starts_with("USB"))
            .filter(|(_, r)| {
                let c = centre(*r);
                let edge = [
                    c.x.0 - outline.min.x.0,
                    outline.max.x.0 - c.x.0,
                    c.y.0 - outline.min.y.0,
                    outline.max.y.0 - c.y.0,
                ]
                .into_iter()
                .min()
                .unwrap_or(0);
                mm(edge) > CONNECTOR_EDGE_MAX_MM
            })
            .map(|(f, _)| f.reference.clone())
            .collect();
        if !far.is_empty() {
            issues.push(issue(
                "CONNECTOR_FAR_FROM_EDGE",
                Severity::Warning,
                format!("connector(s) sit more than {CONNECTOR_EDGE_MAX_MM} mm from the board edge"),
                far,
            ));
        }
    }

    // Decoupling capacitors close to the part they serve.
    let mut far_caps = Vec::new();
    for cap in board.footprints.iter().filter(|f| f.reference.starts_with('C')) {
        let nets: Vec<&BoardPad> = cap.pads.iter().filter(|p| !p.net.is_empty()).collect();
        let (Some(power), true) = (
            nets.iter().find(|p| is_power_net(&p.net)),
            nets.iter().any(|p| is_ground_net(&p.net)),
        ) else {
            continue;
        };
        let nearest = board
            .footprints
            .iter()
            .filter(|f| f.reference.starts_with('U'))
            .flat_map(|u| u.pads.iter())
            .filter(|p| p.net == power.net)
            .map(|p| distance(p.at, power.at))
            .fold(f64::INFINITY, f64::min);
        if nearest.is_finite() && nearest > DECOUPLING_MAX_MM {
            far_caps.push(format!("{} ({:.0} mm)", cap.reference, nearest));
        }
    }
    if !far_caps.is_empty() {
        issues.push(issue(
            "DECOUPLING_FAR",
            Severity::Warning,
            format!(
                "decoupling capacitor(s) more than {DECOUPLING_MAX_MM} mm from the nearest IC pin on their supply: {}",
                far_caps.join(", ")
            ),
            far_caps
                .iter()
                .map(|s| s.split(' ').next().unwrap_or_default().to_owned())
                .collect(),
        ));
    }

    // Pads that connect to nothing.
    let loose: Vec<String> = board
        .footprints
        .iter()
        .filter(|f| f.pads.iter().any(|p| !p.net.is_empty()))
        .flat_map(|f| {
            f.pads
                .iter()
                .filter(|p| p.net.is_empty() && p.kind != "np_thru_hole" && !p.number.is_empty())
                .map(|p| format!("{}.{}", f.reference, p.number))
                .collect::<Vec<_>>()
        })
        .collect();
    if !loose.is_empty() {
        issues.push(issue(
            "PADS_WITHOUT_NET",
            Severity::Warning,
            format!(
                "{} pad(s) are on no net (a pin the schematic leaves unconnected, or a missing connection)",
                loose.len()
            ),
            loose,
        ));
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x_mm: f64, y_mm: f64) -> Point {
        Point::from_mm(x_mm, y_mm).unwrap()
    }

    fn footprint(reference: &str, at: Point, size_mm: f64, pads: &[(&str, &str, f64, f64)]) -> BoardFootprint {
        BoardFootprint {
            id: reference.into(),
            reference: reference.into(),
            value: String::new(),
            lib_id: "L:F".into(),
            layer: "F.Cu".into(),
            position: at,
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            pads: pads
                .iter()
                .map(|(n, net, x, y)| BoardPad {
                    number: (*n).into(),
                    kind: "smd".into(),
                    at: p(at.x.to_mm() + x, at.y.to_mm() + y),
                    net: (*net).into(),
                })
                .collect(),
            courtyard: Some(
                Rect::from_points([
                    p(at.x.to_mm() - size_mm, at.y.to_mm() - size_mm),
                    p(at.x.to_mm() + size_mm, at.y.to_mm() + size_mm),
                ])
                .unwrap(),
            ),
        }
    }

    fn codes(issues: &[BoardIssue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn rotation_is_counter_clockwise_on_screen() {
        let q = place_point(p(10.0, 10.0), 90.0, p(1.0, 0.0));
        assert_eq!(q, p(10.0, 9.0), "a quarter turn takes right to up");
        let r = place_rect(
            p(0.0, 0.0),
            90.0,
            Rect::from_points([p(0.0, 0.0), p(4.0, 2.0)]).unwrap(),
        );
        assert_eq!((r.width(), r.height()), (Nm(2_000_000), Nm(4_000_000)));
    }

    #[test]
    fn an_empty_board_and_one_without_an_outline_are_noticed() {
        assert!(codes(&review(&Board::default(), None)).contains(&"NO_FOOTPRINTS"));
        let board = Board {
            footprints: vec![footprint("R1", p(5.0, 5.0), 1.0, &[])],
            ..Default::default()
        };
        assert!(codes(&review(&board, None)).contains(&"NO_OUTLINE"));
    }

    #[test]
    fn overlapping_and_outside_parts_are_found() {
        let board = Board {
            footprints: vec![
                footprint("R1", p(10.0, 10.0), 2.0, &[]),
                footprint("R2", p(11.0, 10.0), 2.0, &[]),
                footprint("R3", p(60.0, 10.0), 2.0, &[]),
            ],
            outline: Rect::from_points([p(0.0, 0.0), p(30.0, 30.0)]),
            ..Default::default()
        };
        let issues = review(&board, None);
        let overlap = issues.iter().find(|i| i.code == "FOOTPRINTS_OVERLAP").unwrap();
        assert!(overlap.message.contains("R1 and R2"));
        let outside = issues.iter().find(|i| i.code == "OUTSIDE_OUTLINE").unwrap();
        assert_eq!(outside.references, ["R3"]);
    }

    #[test]
    fn opposite_sides_do_not_collide() {
        let mut back = footprint("R2", p(10.0, 10.0), 2.0, &[]);
        back.layer = "B.Cu".into();
        let board = Board {
            footprints: vec![footprint("R1", p(10.0, 10.0), 2.0, &[]), back],
            outline: Rect::from_points([p(0.0, 0.0), p(30.0, 30.0)]),
            ..Default::default()
        };
        assert!(!codes(&review(&board, None)).contains(&"FOOTPRINTS_OVERLAP"));
    }

    #[test]
    fn connectors_belong_at_the_edge_and_decoupling_near_the_chip() {
        let board = Board {
            footprints: vec![
                footprint("J1", p(50.0, 50.0), 2.0, &[]),
                footprint(
                    "U1",
                    p(20.0, 20.0),
                    3.0,
                    &[("1", "+5V", -2.0, 0.0), ("2", "GND", 2.0, 0.0)],
                ),
                footprint(
                    "C1",
                    p(21.0, 24.0),
                    1.0,
                    &[("1", "+5V", 0.0, 0.0), ("2", "GND", 1.0, 0.0)],
                ),
                footprint(
                    "C2",
                    p(40.0, 24.0),
                    1.0,
                    &[("1", "+5V", 0.0, 0.0), ("2", "GND", 1.0, 0.0)],
                ),
            ],
            outline: Rect::from_points([p(0.0, 0.0), p(100.0, 100.0)]),
            ..Default::default()
        };
        let issues = review(&board, None);
        assert_eq!(
            issues
                .iter()
                .find(|i| i.code == "CONNECTOR_FAR_FROM_EDGE")
                .unwrap()
                .references,
            ["J1"]
        );
        assert_eq!(
            issues.iter().find(|i| i.code == "DECOUPLING_FAR").unwrap().references,
            ["C2"]
        );
    }

    #[test]
    fn the_board_is_compared_with_the_schematic() {
        use crate::schematic::DesignSymbol;
        let sch = SchematicDesign {
            symbols: ["R1", "R2"]
                .iter()
                .map(|r| DesignSymbol {
                    id: (*r).into(),
                    reference: (*r).into(),
                    footprint: "L:Other".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let board = Board {
            footprints: vec![
                footprint("R1", p(1.0, 1.0), 1.0, &[]),
                footprint("R9", p(5.0, 5.0), 1.0, &[]),
            ],
            outline: Rect::from_points([p(0.0, 0.0), p(30.0, 30.0)]),
            ..Default::default()
        };
        let issues = review(&board, Some(&sch));
        assert_eq!(
            issues
                .iter()
                .find(|i| i.code == "NO_FOOTPRINT_FOR_PART")
                .unwrap()
                .references,
            ["R2"]
        );
        assert_eq!(
            issues
                .iter()
                .find(|i| i.code == "FOOTPRINT_WITHOUT_PART")
                .unwrap()
                .references,
            ["R9"]
        );
        assert_eq!(
            issues
                .iter()
                .find(|i| i.code == "FOOTPRINT_DIFFERS")
                .unwrap()
                .references,
            ["R1"]
        );
    }
}
