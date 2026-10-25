// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving, turning, and deleting parts of an existing schematic (Phase 8, manual editing).
//!
//! These are deliberately bare edits: only the named parts change. The wires, labels, and junctions
//! around them stay where they are, so a moved part is left with its wires behind. The assistant
//! places every part where it should be and then redraws the connections in one step
//! ([`crate::tidy::plan_reconnect`]), which suits small circuits and the start of a design.

use std::collections::{BTreeMap, BTreeSet};

use crate::geometry::{Nm, Orientation, Point, GRID};
use crate::ids::LibraryId;
use crate::plan::{error, LibrarySymbol, OperationPlan, Removal, RemovedKind, SymbolMove};
use crate::readability::check_design;
use crate::schematic::SchematicDesign;
use crate::validation::{has_errors, Finding, Severity};

/// How a part is mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    None,
    /// Flipped top to bottom.
    X,
    /// Flipped left to right.
    Y,
}

/// A change to one part. `at` is a place on the sheet, `by` an offset from where it is now; the
/// rotation is a number of degrees counter-clockwise from where it is now.
#[derive(Debug, Clone, Default)]
pub struct PartEdit {
    pub reference: String,
    pub at: Option<Point>,
    pub by: Option<Point>,
    pub rotate_deg: Option<u16>,
    pub mirror: Option<Mirror>,
}

const ROTATION_FRAMES: [u16; 4] = [0, 90, 180, 270];

/// `o` turned a further `deg` degrees counter-clockwise as seen on screen. Orientations are
/// compared by what they do to the axes, so a mirrored part turns the way it looks.
pub fn rotated(o: Orientation, deg: u16) -> Orientation {
    let turn = |p: Point, quarters: u16| {
        let (x, y) = (p.x.0, p.y.0);
        let (x, y) = match quarters % 4 {
            0 => (x, y),
            1 => (y, -x),
            2 => (-x, -y),
            _ => (-y, x),
        };
        Point::new(Nm(x), Nm(y))
    };
    let probes = [Point::new(Nm(1), Nm(0)), Point::new(Nm(0), Nm(1))];
    let wanted: Vec<Option<Point>> = probes.iter().map(|&p| o.apply(p).map(|q| turn(q, deg / 90))).collect();
    let mut candidates = Vec::new();
    for mirror_x in [o.mirror_x, !o.mirror_x] {
        for mirror_y in [o.mirror_y, !o.mirror_y] {
            for rotation_deg in ROTATION_FRAMES {
                candidates.push(Orientation {
                    rotation_deg,
                    mirror_x,
                    mirror_y,
                });
            }
        }
    }
    candidates
        .into_iter()
        .find(|c| probes.iter().zip(&wanted).all(|(&p, w)| c.apply(p) == *w))
        .unwrap_or(o)
}

/// `o` with the mirroring set to `mirror`, turning as it is.
pub fn mirrored(o: Orientation, mirror: Mirror) -> Orientation {
    Orientation {
        rotation_deg: o.rotation_deg,
        mirror_x: mirror == Mirror::X,
        mirror_y: mirror == Mirror::Y,
    }
}

fn warn(code: &'static str, message: String, objects: Vec<String>) -> Finding {
    Finding {
        severity: Severity::Warning,
        code,
        message,
        objects,
    }
}

/// Plans the edits and deletions. Nothing else changes; applying it leaves wires behind, so no nets
/// are expected to hold afterwards.
pub fn plan_arrange(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    edits: &[PartEdit],
    deletes: &[String],
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "arrange".into(),
        ..Default::default()
    };
    let mut findings = Vec::new();
    let parts: BTreeMap<&str, _> = design
        .symbols
        .iter()
        .filter(|s| !s.reference.starts_with("#PWR"))
        .map(|s| (s.reference.as_str(), s))
        .collect();
    let mut named = BTreeSet::new();
    for reference in edits.iter().map(|e| &e.reference).chain(deletes) {
        if !named.insert(reference.as_str()) {
            findings.push(error(
                "ARRANGE_DUPLICATE",
                format!("{reference} is named twice; one change per part"),
                vec![reference.clone()],
            ));
        }
        if !parts.contains_key(reference.as_str()) {
            findings.push(error(
                "ARRANGE_UNKNOWN_PART",
                format!("there is no part {reference} (power symbols cannot be edited here)"),
                vec![reference.clone()],
            ));
        }
    }

    let mut after = design.clone();
    let mut touched: Vec<&crate::schematic::DesignSymbol> = Vec::new();
    for edit in edits {
        let Some(&old) = parts.get(edit.reference.as_str()) else {
            continue;
        };
        if edit.at.is_some() && edit.by.is_some() {
            findings.push(error(
                "ARRANGE_AT_AND_BY",
                format!("{}: give either a place or an offset, not both", edit.reference),
                vec![edit.reference.clone()],
            ));
            continue;
        }
        if edit.rotate_deg.is_some_and(|d| d % 90 != 0 || d >= 360) {
            findings.push(error(
                "ARRANGE_BAD_ROTATION",
                format!("{}: a rotation is 90, 180, or 270 degrees", edit.reference),
                vec![edit.reference.clone()],
            ));
            continue;
        }
        let position = match (edit.at, edit.by) {
            (Some(at), _) => at.snap(GRID),
            (_, Some(by)) => (old.position + by).snap(GRID),
            _ => old.position,
        };
        let mut orientation = old.orientation;
        if let Some(deg) = edit.rotate_deg {
            orientation = rotated(orientation, deg);
        }
        if let Some(mirror) = edit.mirror {
            orientation = mirrored(orientation, mirror);
        }
        if position == old.position && orientation.same_effect(old.orientation) {
            continue;
        }
        plan.moves.push(SymbolMove {
            id: old.id.clone(),
            reference: old.reference.clone(),
            from: old.position,
            from_orientation: old.orientation,
            to: position,
            orientation,
        });
        touched.push(old);
        if let Some(s) = after.symbols.iter_mut().find(|s| s.id == old.id) {
            s.position = position;
            s.orientation = orientation;
        }
    }
    for reference in deletes {
        let Some(&old) = parts.get(reference.as_str()) else {
            continue;
        };
        plan.removals.push(Removal {
            kind: RemovedKind::Symbol,
            id: old.id.clone(),
            description: format!("part {} ({})", old.reference, old.value),
        });
        touched.push(old);
        after.symbols.retain(|s| s.id != old.id);
    }

    // Wires that end on a pin of a part that is moved or deleted stay behind.
    let pins_of = |s: &crate::schematic::DesignSymbol| -> Vec<Point> {
        LibraryId::parse(&s.library_id)
            .ok()
            .and_then(|id| library.get(&id))
            .map(|def| {
                def.pins
                    .iter()
                    .filter_map(|p| s.orientation.apply(p.local).map(|o| s.position + o))
                    .collect()
            })
            .unwrap_or_default()
    };
    let stranded: BTreeSet<&str> = design
        .wires
        .iter()
        .filter(|w| {
            touched
                .iter()
                .any(|s| pins_of(s).iter().any(|p| *p == w.start || *p == w.end))
        })
        .map(|w| w.id.as_str())
        .collect();
    if !stranded.is_empty() {
        findings.push(warn(
            "ARRANGE_WIRES_LEFT_BEHIND",
            format!(
                "{} wire(s) end on parts that are moved or deleted and will no longer reach them; \
                 when every part is in place, run sch_reconnect_plan to draw the connections again",
                stranded.len()
            ),
            vec![],
        ));
    }

    let before = check_design(design, library);
    let now = check_design(&after, library);
    for (a, b) in &now.overlapping_symbols {
        if !before.overlapping_symbols.contains(&(a.clone(), b.clone())) {
            findings.push(warn(
                "SYMBOLS_OVERLAP",
                format!("{a} and {b} would overlap"),
                vec![a.clone(), b.clone()],
            ));
        }
    }
    plan.readability = now;
    plan.readability_before = Some(before);

    if plan.moves.is_empty() && plan.removals.is_empty() && !has_errors(&findings) {
        findings.push(warn(
            "NOTHING_TO_CHANGE",
            "every part is already where the request puts it".into(),
            vec![],
        ));
    }
    plan.findings = findings;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_four_times_comes_back() {
        let o = Orientation {
            rotation_deg: 90,
            mirror_x: true,
            mirror_y: false,
        };
        let mut r = o;
        for _ in 0..4 {
            r = rotated(r, 90);
        }
        assert!(r.same_effect(o));
        assert!(!rotated(o, 90).same_effect(o));
    }

    #[test]
    fn a_quarter_turn_is_counter_clockwise_on_screen() {
        let r = rotated(Orientation::default(), 90);
        // Right becomes up; up is negative y.
        assert_eq!(r.apply(Point::new(Nm(1), Nm(0))), Some(Point::new(Nm(0), Nm(-1))));
    }
}
