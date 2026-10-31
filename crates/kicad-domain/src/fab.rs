// SPDX-License-Identifier: GPL-3.0-or-later
//! What the manufacturing outputs need to know about a board beyond [`Board`](crate::board::Board)
//! (Phase 13), and the checks run before anything is exported: the outline is closed, silkscreen
//! stays on the board, every footprint has a BOM line, and DRC is clean.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::bom::Bom;
use crate::geometry::{Nm, Point, Rect};

/// One drawing on `Edge.Cuts`, on the board or inside a footprint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum EdgeShape {
    /// A line or arc, which must meet others end to end.
    Open { start: Point, end: Point },
    /// A rectangle, circle, or polygon: closed by itself.
    Closed { bounds: Rect },
}

/// A silkscreen item and its extent on the board.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SilkItem {
    /// The footprint it belongs to, if any.
    pub owner: Option<String>,
    /// For example `Reference field`, `fp_line`, `gr_text`.
    pub what: String,
    pub layer: String,
    pub bounds: Rect,
    /// Text whose size is estimated from its font size and length, not measured.
    pub estimated: bool,
}

/// A drilled hole: a through-hole pad, a mounting hole, or a via.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hole {
    /// The footprint of a pad's hole; none for a via.
    pub owner: Option<String>,
    pub at: Point,
    /// The drill; an oval hole has two sizes.
    pub drill: (Nm, Nm),
    pub plated: bool,
    pub via: bool,
}

/// A rule area (keep-out).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Keepout {
    pub name: String,
    pub layers: Vec<String>,
    pub outline: Vec<Point>,
    /// What may not be placed there: `tracks`, `vias`, `pads`, `copperpour`, `footprints`.
    pub forbids: Vec<String>,
}

/// What a footprint says about itself for the BOM, assembly, and the mechanical model.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct FootprintFacts {
    pub reference: String,
    pub value: String,
    pub lib_id: String,
    pub layer: String,
    pub position: Point,
    pub rotation_deg: f64,
    /// `smd`, `through_hole`, or empty.
    pub mount: String,
    pub exclude_from_bom: bool,
    pub exclude_from_pos: bool,
    pub board_only: bool,
    pub dnp: bool,
    /// 3D model paths as written in the board (`${KICAD…_3DMODEL_DIR}` left as is).
    pub models: Vec<String>,
    /// The footprint's own fields (`Reference`, `Value`, and any others, such as `Height`).
    pub properties: BTreeMap<String, String>,
    pub courtyard: Option<Rect>,
    /// Whether it has a pad on a net.
    pub connected: bool,
}

impl FootprintFacts {
    pub fn is_back(&self) -> bool {
        self.layer.starts_with("B.")
    }

    /// A mounting hole or fiducial: no part in the schematic, no BOM line.
    pub fn is_mechanical(&self) -> bool {
        self.lib_id.starts_with("MountingHole:")
            || self.lib_id.starts_with("Fiducial:")
            || ((self.reference.starts_with('H') || self.reference.starts_with("FID")) && !self.connected)
    }

    /// A part the BOM must list.
    pub fn needs_bom_line(&self) -> bool {
        !self.exclude_from_bom && !self.board_only && !self.is_mechanical() && !self.reference.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct FabFacts {
    pub thickness: Option<Nm>,
    pub edges: Vec<EdgeShape>,
    pub silk: Vec<SilkItem>,
    pub holes: Vec<Hole>,
    pub keepouts: Vec<Keepout>,
    pub footprints: Vec<FootprintFacts>,
}

// ---------------------------------------------------------------- checks

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    /// Worth a look; does not stop an export.
    Warning,
    /// Stops an export until the user accepts it.
    Fail,
    /// Could not be checked (for example no schematic); never counted as passing.
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FabCheck {
    pub id: &'static str,
    pub status: CheckStatus,
    pub message: String,
    /// What failed: references, places, DRC types.
    pub items: Vec<String>,
}

impl FabCheck {
    fn new(id: &'static str, status: CheckStatus, message: impl Into<String>, items: Vec<String>) -> FabCheck {
        FabCheck {
            id,
            status,
            message: message.into(),
            items,
        }
    }
}

/// What DRC found, for [`pre_export`].
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct DrcSummary {
    pub errors: usize,
    pub warnings: usize,
    pub unrouted: usize,
    /// Each violation type with its count, e.g. `clearance (2)`.
    pub error_types: Vec<String>,
    pub warning_types: Vec<String>,
}

/// End points closer than this meet.
const JOIN: i64 = 10_000;

fn mm(p: Point) -> String {
    format!("({:.2}, {:.2}) mm", p.x.to_mm(), p.y.to_mm())
}

/// The outline is closed: every line and arc end meets exactly one other end, and something is
/// drawn on `Edge.Cuts`.
pub fn outline_check(facts: &FabFacts) -> FabCheck {
    if facts.edges.is_empty() {
        return FabCheck::new(
            "outline_closed",
            CheckStatus::Fail,
            "the board has no outline on Edge.Cuts (pcb_outline_plan draws one)",
            vec![],
        );
    }
    // End points grouped where they meet.
    let mut ends: Vec<(Point, usize)> = Vec::new();
    for shape in &facts.edges {
        if let EdgeShape::Open { start, end } = shape {
            for p in [*start, *end] {
                match ends
                    .iter_mut()
                    .find(|(q, _)| (q.x.0 - p.x.0).abs() <= JOIN && (q.y.0 - p.y.0).abs() <= JOIN)
                {
                    Some((_, n)) => *n += 1,
                    None => ends.push((p, 1)),
                }
            }
        }
    }
    let loose: Vec<String> = ends
        .iter()
        .filter(|(_, n)| *n != 2)
        .map(|(p, n)| match n {
            1 => format!("open end at {}", mm(*p)),
            _ => format!("{n} lines meet at {}", mm(*p)),
        })
        .collect();
    if loose.is_empty() {
        FabCheck::new(
            "outline_closed",
            CheckStatus::Pass,
            format!("the outline is closed ({} shapes on Edge.Cuts)", facts.edges.len()),
            vec![],
        )
    } else {
        FabCheck::new(
            "outline_closed",
            CheckStatus::Fail,
            "the outline on Edge.Cuts is not closed; a board house cannot cut it",
            loose,
        )
    }
}

/// Silkscreen inside the board outline (its bounding box). Text sizes are estimated, so text may
/// reach up to 0.25 mm past the edge before it counts.
pub fn silk_check(facts: &FabFacts, outline: Option<Rect>) -> FabCheck {
    let Some(outline) = outline else {
        return FabCheck::new(
            "silkscreen_on_board",
            CheckStatus::NotRun,
            "no outline to compare the silkscreen with",
            vec![],
        );
    };
    let outside: Vec<String> = facts
        .silk
        .iter()
        .filter(|s| {
            let slack = if s.estimated { Nm(250_000) } else { Nm(0) };
            let within = outline.inflate(slack);
            s.bounds.min.x < within.min.x
                || s.bounds.min.y < within.min.y
                || s.bounds.max.x > within.max.x
                || s.bounds.max.y > within.max.y
        })
        .map(|s| match &s.owner {
            Some(r) => format!("{} of {r} on {}", s.what, s.layer),
            None => format!("{} on {} at {}", s.what, s.layer, mm(s.bounds.min)),
        })
        .collect();
    if outside.is_empty() {
        FabCheck::new(
            "silkscreen_on_board",
            CheckStatus::Pass,
            format!("{} silkscreen items, all on the board", facts.silk.len()),
            vec![],
        )
    } else {
        FabCheck::new(
            "silkscreen_on_board",
            CheckStatus::Warning,
            "silkscreen reaches past the board edge; the board house cuts it off there",
            outside,
        )
    }
}

/// Every footprint the BOM must list has a line in the schematic's BOM.
pub fn bom_check(facts: &FabFacts, bom: Option<&Bom>) -> FabCheck {
    let Some(bom) = bom else {
        return FabCheck::new(
            "bom_lines",
            CheckStatus::NotRun,
            "no schematic next to the board (or given) to take the BOM from",
            vec![],
        );
    };
    let listed: BTreeSet<&str> = bom
        .rows
        .iter()
        .flat_map(|r| r.references.iter().map(String::as_str))
        .collect();
    let excluded: BTreeSet<&str> = bom.excluded.iter().map(String::as_str).collect();
    let missing: Vec<String> = facts
        .footprints
        .iter()
        .filter(|f| f.needs_bom_line() && !listed.contains(f.reference.as_str()))
        .map(|f| {
            if excluded.contains(f.reference.as_str()) {
                format!(
                    "{}: excluded from the BOM in the schematic but not on the board",
                    f.reference
                )
            } else {
                format!("{}: on the board, not in the schematic", f.reference)
            }
        })
        .collect();
    let counted = facts.footprints.iter().filter(|f| f.needs_bom_line()).count();
    if missing.is_empty() {
        FabCheck::new(
            "bom_lines",
            CheckStatus::Pass,
            format!("all {counted} footprints that need one have a BOM line"),
            vec![],
        )
    } else {
        FabCheck::new(
            "bom_lines",
            CheckStatus::Fail,
            "footprints without a BOM line (update the board from the schematic, or exclude them from the BOM)",
            missing,
        )
    }
}

pub fn drc_check(drc: Option<&DrcSummary>) -> FabCheck {
    let Some(d) = drc else {
        return FabCheck::new("drc", CheckStatus::NotRun, "DRC could not be run", vec![]);
    };
    if d.errors > 0 || d.unrouted > 0 {
        let mut items = d.error_types.clone();
        if d.unrouted > 0 {
            items.push(format!("unrouted ({})", d.unrouted));
        }
        FabCheck::new(
            "drc",
            CheckStatus::Fail,
            format!("DRC: {} errors, {} unrouted connections", d.errors, d.unrouted),
            items,
        )
    } else if d.warnings > 0 {
        FabCheck::new(
            "drc",
            CheckStatus::Warning,
            format!("DRC: no errors, all routed, {} warnings", d.warnings),
            d.warning_types.clone(),
        )
    } else {
        FabCheck::new("drc", CheckStatus::Pass, "DRC clean and all routed", vec![])
    }
}

/// Mounting holes still listed as parts: boards made with v0.7.0 had no attributes on their holes,
/// so a placement file would list each as a part to place.
pub fn holes_check(facts: &FabFacts) -> FabCheck {
    let mut listed: Vec<String> = facts
        .footprints
        .iter()
        .filter(|f| {
            (f.lib_id.starts_with("MountingHole:") || (f.reference.starts_with('H') && !f.connected))
                && !(f.exclude_from_pos && f.exclude_from_bom)
        })
        .map(|f| f.reference.clone())
        .collect();
    listed.sort();
    if listed.is_empty() {
        return FabCheck::new(
            "holes_marked",
            CheckStatus::Pass,
            "no mounting hole is listed as a part",
            vec![],
        );
    }
    FabCheck::new(
        "holes_marked",
        CheckStatus::Warning,
        format!(
            "{} mounting hole(s) would appear in the position file and the BOM as parts to place;              pcb_footprint_attrs_plan with mounting_holes: true leaves them out",
            listed.len()
        ),
        listed,
    )
}

/// Every check before an export, in order.
pub fn pre_export(
    facts: &FabFacts,
    outline: Option<Rect>,
    bom: Option<&Bom>,
    drc: Option<&DrcSummary>,
) -> Vec<FabCheck> {
    vec![
        outline_check(facts),
        drc_check(drc),
        bom_check(facts, bom),
        silk_check(facts, outline),
        holes_check(facts),
    ]
}

/// The checks that stop an export until the user accepts them.
pub fn blocking(checks: &[FabCheck]) -> Vec<&FabCheck> {
    checks.iter().filter(|c| c.status == CheckStatus::Fail).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bom::BomRow;

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn line(a: Point, b: Point) -> EdgeShape {
        EdgeShape::Open { start: a, end: b }
    }

    #[test]
    fn four_lines_that_meet_close_the_outline() {
        let mut facts = FabFacts {
            edges: vec![
                line(p(0.0, 0.0), p(10.0, 0.0)),
                line(p(10.0, 0.0), p(10.0, 8.0)),
                line(p(10.0, 8.0), p(0.0, 8.0)),
                line(p(0.0, 8.0), p(0.0, 0.0)),
            ],
            ..Default::default()
        };
        assert_eq!(outline_check(&facts).status, CheckStatus::Pass);
        facts.edges.pop();
        let open = outline_check(&facts);
        assert_eq!(open.status, CheckStatus::Fail);
        assert_eq!(open.items.len(), 2, "{:?}", open.items);
        assert!(open.items[0].contains("open end"));
        facts.edges.clear();
        assert_eq!(outline_check(&facts).status, CheckStatus::Fail);
        facts.edges.push(EdgeShape::Closed {
            bounds: Rect::from_points([p(0.0, 0.0), p(5.0, 5.0)]).unwrap(),
        });
        assert_eq!(outline_check(&facts).status, CheckStatus::Pass);
    }

    #[test]
    fn silkscreen_past_the_edge_is_a_warning() {
        let outline = Rect::from_points([p(0.0, 0.0), p(20.0, 10.0)]);
        let item = |owner: &str, a: Point, b: Point, estimated| SilkItem {
            owner: Some(owner.into()),
            what: "Reference field".into(),
            layer: "F.SilkS".into(),
            bounds: Rect::from_points([a, b]).unwrap(),
            estimated,
        };
        let facts = FabFacts {
            silk: vec![
                item("R1", p(1.0, 1.0), p(3.0, 2.0), true),
                // An estimate a little past the edge is let through.
                item("R2", p(-0.2, 1.0), p(2.0, 2.0), true),
                item("J1", p(15.0, -1.5), p(17.0, -0.5), true),
            ],
            ..Default::default()
        };
        let check = silk_check(&facts, outline);
        assert_eq!(check.status, CheckStatus::Warning);
        assert_eq!(check.items, ["Reference field of J1 on F.SilkS"]);
        assert_eq!(silk_check(&facts, None).status, CheckStatus::NotRun);
    }

    #[test]
    fn a_footprint_missing_from_the_bom_fails() {
        let fp = |r: &str, lib: &str, connected| FootprintFacts {
            reference: r.into(),
            lib_id: lib.into(),
            connected,
            ..Default::default()
        };
        let facts = FabFacts {
            footprints: vec![
                fp("R1", "Resistor_SMD:R_0603", true),
                fp("R2", "Resistor_SMD:R_0603", true),
                fp("H1", "MountingHole:MountingHole_3.2mm_M3", false),
                FootprintFacts {
                    board_only: true,
                    ..fp("LOGO1", "Symbol:Logo", false)
                },
            ],
            ..Default::default()
        };
        let bom = Bom {
            rows: vec![BomRow {
                item: 1,
                references: vec!["R1".into()],
                quantity: 1,
                value: "10k".into(),
                footprint: String::new(),
                datasheet: String::new(),
                description: String::new(),
                fields: BTreeMap::new(),
                dnp: false,
            }],
            total_parts: 1,
            custom_fields: vec![],
            excluded: vec![],
        };
        let check = bom_check(&facts, Some(&bom));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.items, ["R2: on the board, not in the schematic"]);
        assert_eq!(bom_check(&facts, None).status, CheckStatus::NotRun);
    }

    #[test]
    fn drc_errors_and_unrouted_block_and_warnings_do_not() {
        let mut d = DrcSummary {
            errors: 1,
            unrouted: 3,
            error_types: vec!["clearance (1)".into()],
            ..Default::default()
        };
        let c = drc_check(Some(&d));
        assert_eq!((c.status, c.items.len()), (CheckStatus::Fail, 2));
        d = DrcSummary {
            warnings: 4,
            ..Default::default()
        };
        assert_eq!(drc_check(Some(&d)).status, CheckStatus::Warning);
        assert_eq!(drc_check(None).status, CheckStatus::NotRun);
        let checks = pre_export(&FabFacts::default(), None, None, Some(&d));
        assert_eq!(
            blocking(&checks).iter().map(|c| c.id).collect::<Vec<_>>(),
            ["outline_closed"]
        );
    }

    #[test]
    fn holes_that_would_be_listed_as_parts_are_a_warning() {
        let hole = |reference: &str, excluded: bool| FootprintFacts {
            reference: reference.into(),
            lib_id: "MountingHole:MountingHole_3.2mm_M3".into(),
            exclude_from_bom: excluded,
            exclude_from_pos: excluded,
            board_only: excluded,
            ..Default::default()
        };
        let facts = |holes: Vec<FootprintFacts>| FabFacts {
            footprints: holes,
            ..Default::default()
        };
        let ok = holes_check(&facts(vec![hole("H1", true), hole("H2", true)]));
        assert_eq!(ok.status, CheckStatus::Pass);
        let bad = holes_check(&facts(vec![hole("H1", true), hole("H2", false)]));
        assert_eq!(bad.status, CheckStatus::Warning);
        assert_eq!(bad.items, ["H2"]);
        assert!(bad.message.contains("pcb_footprint_attrs_plan"));
        assert!(blocking(&[bad]).is_empty(), "a warning never stops an export");
    }
}
