// SPDX-License-Identifier: GPL-3.0-or-later
//! Labels, drawings, and notes: adding, renaming, turning, moving, and deleting them on an existing
//! sheet (Phase 9).
//!
//! Each planner returns an ordinary plan (removals and additions), so the same review, confirm, and
//! apply flow serves them, and applying checks every net against KiCad. A label can rename a net or
//! join two, so a label that would do either by accident is refused.

use std::collections::{BTreeMap, BTreeSet};

use crate::connectivity::net_at;
use crate::declutter::free_label_direction;
use crate::geometry::{Dir, Nm, Point, Rect, GRID};
use crate::ids::LibraryId;
use crate::plan::{error, ExpectedNet, LibrarySymbol, Operation, OperationPlan, Removal, RemovedKind};
use crate::schematic::{NetInfo, SchematicDesign, ShapeKind};
use crate::text::{HAlign, VAlign};
use crate::validation::{has_errors, Finding, Severity};

/// The nets as the plan must leave them.
pub fn expected_from(nets: &[NetInfo]) -> Vec<ExpectedNet> {
    nets.iter()
        .filter(|n| !n.name.starts_with("unconnected-"))
        .map(|n| ExpectedNet {
            name: n.name.trim_start_matches('/').to_owned(),
            pins: n
                .pins
                .iter()
                .filter(|p| !p.reference.starts_with('#'))
                .map(|p| format!("{}.{}", p.reference, p.pin))
                .collect(),
            name_open: false,
        })
        .filter(|n| !n.pins.is_empty())
        .collect()
}

fn warn(code: &'static str, message: String) -> Finding {
    Finding {
        severity: Severity::Warning,
        code,
        message,
        objects: vec![],
    }
}

/// A net name KiCad makes up for a net nobody named.
fn is_auto_name(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("Net-(")
}

fn valid_net_text(text: &str) -> Option<&'static str> {
    if text.trim().is_empty() {
        Some("the label text is empty")
    } else if text.chars().count() > 128 {
        Some("the label text is longer than 128 characters")
    } else if text.chars().any(|c| c.is_control() || c == '"') {
        Some("the label text has a control character or a quote")
    } else {
        None
    }
}

// ---------------------------------------------------------------- labels

#[derive(Debug, Clone)]
pub enum LabelAt {
    /// At a part's pin, `REF` and pin number.
    Pin { reference: String, pin: String },
    /// On a wire or at a pin, at a point of the sheet.
    Point(Point),
}

#[derive(Debug, Clone)]
pub struct LabelAdd {
    pub text: String,
    pub at: LabelAt,
    /// The way the text runs; chosen to keep clear of other text when absent.
    pub direction: Option<Dir>,
    /// Allow the label to join the net it sits on with another net of the same name.
    pub allow_join: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LabelEdits {
    pub add: Vec<LabelAdd>,
    /// Existing labels (by id) given another text; the net takes the new name.
    pub rename: Vec<(String, String)>,
    /// Existing labels (by id) turned to run another way.
    pub turn: Vec<(String, Dir)>,
    /// Existing labels (by id) deleted.
    pub remove: Vec<String>,
}

fn pin_point(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    reference: &str,
    number: &str,
) -> Option<Point> {
    let symbol = design.symbols.iter().find(|s| s.reference == reference)?;
    let def = LibraryId::parse(&symbol.library_id)
        .ok()
        .and_then(|id| library.get(&id))?;
    let pin = def
        .pins
        .iter()
        .find(|p| p.number == number && (p.unit == 0 || p.unit == symbol.unit))?;
    Some(symbol.position + symbol.orientation.apply(pin.local)?)
}

pub fn plan_labels(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    edits: &LabelEdits,
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "labels".into(),
        ..Default::default()
    };
    let mut findings = Vec::new();
    let mut expected = expected_from(nets);
    let net_name = |n: &NetInfo| n.name.trim_start_matches('/').to_owned();
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    let mut scratch = design.clone();

    let expected_of = |expected: &mut Vec<ExpectedNet>, name: &str| expected.iter().position(|e| e.name == name);

    for (id, text) in &edits.rename {
        let Some(label) = design.labels.iter().find(|l| &l.id == id) else {
            findings.push(error(
                "LABEL_NOT_FOUND",
                format!("no label with id {id}"),
                vec![id.clone()],
            ));
            continue;
        };
        if !touched.insert(id) {
            findings.push(error(
                "LABEL_TWICE",
                format!("label {id} is named twice"),
                vec![id.clone()],
            ));
            continue;
        }
        if let Some(why) = valid_net_text(text) {
            findings.push(error("LABEL_BAD_TEXT", why.into(), vec![id.clone()]));
            continue;
        }
        let Some(net) = net_at(design, library, nets, label.position) else {
            findings.push(error(
                "LABEL_NOT_ON_NET",
                format!("label {} is not on a wire or pin", label.text),
                vec![id.clone()],
            ));
            continue;
        };
        let old = net_name(net);
        if nets.iter().any(|n| net_name(n) == *text && net_name(n) != old) {
            findings.push(error(
                "LABEL_WOULD_MERGE_NETS",
                format!(
                    "another net is already called '{text}'; renaming {} to it would join the two",
                    label.text
                ),
                vec![id.clone()],
            ));
            continue;
        }
        plan.removals.push(Removal {
            kind: RemovedKind::Label,
            id: id.clone(),
            description: format!("label {} (renamed to {text})", label.text),
        });
        plan.operations.push(Operation::AddLabel {
            net: text.clone(),
            position: label.position,
            direction: label.direction,
        });
        if let Some(i) = expected_of(&mut expected, &old) {
            expected[i].name = text.clone();
        }
        // Other labels of the old name on the same net keep it a net of two names; say so.
        if design
            .labels
            .iter()
            .filter(|l| l.text == label.text && l.id != *id)
            .count()
            > 0
        {
            findings.push(warn(
                "LABEL_OTHERS_KEEP_NAME",
                format!(
                    "other labels still say {}; rename them too, or the net keeps two names",
                    label.text
                ),
            ));
        }
    }

    for (id, direction) in &edits.turn {
        let Some(label) = design.labels.iter().find(|l| &l.id == id) else {
            findings.push(error(
                "LABEL_NOT_FOUND",
                format!("no label with id {id}"),
                vec![id.clone()],
            ));
            continue;
        };
        if !touched.insert(id) {
            findings.push(error(
                "LABEL_TWICE",
                format!("label {id} is named twice"),
                vec![id.clone()],
            ));
            continue;
        }
        plan.removals.push(Removal {
            kind: RemovedKind::Label,
            id: id.clone(),
            description: format!("label {} (turned)", label.text),
        });
        plan.operations.push(Operation::AddLabel {
            net: label.text.clone(),
            position: label.position,
            direction: *direction,
        });
    }

    for id in &edits.remove {
        let Some(label) = design.labels.iter().find(|l| &l.id == id) else {
            findings.push(error(
                "LABEL_NOT_FOUND",
                format!("no label with id {id}"),
                vec![id.clone()],
            ));
            continue;
        };
        if !touched.insert(id) {
            findings.push(error(
                "LABEL_TWICE",
                format!("label {id} is named twice"),
                vec![id.clone()],
            ));
            continue;
        }
        plan.removals.push(Removal {
            kind: RemovedKind::Label,
            id: id.clone(),
            description: format!("label {}", label.text),
        });
        // The net may fall back to another name; do not insist on one.
        if let Some(net) = net_at(design, library, nets, label.position) {
            if let Some(i) = expected_of(&mut expected, &net_name(net)) {
                expected[i].name_open = true;
            }
        }
        scratch.labels.retain(|l| &l.id != id);
    }

    for add in &edits.add {
        if let Some(why) = valid_net_text(&add.text) {
            findings.push(error("LABEL_BAD_TEXT", why.into(), vec![add.text.clone()]));
            continue;
        }
        let point = match &add.at {
            LabelAt::Pin { reference, pin } => match pin_point(design, library, reference, pin) {
                Some(p) => p,
                None => {
                    findings.push(error(
                        "LABEL_UNKNOWN_PIN",
                        format!("there is no pin {reference}.{pin}"),
                        vec![format!("{reference}.{pin}")],
                    ));
                    continue;
                }
            },
            LabelAt::Point(p) => p.snap(GRID),
        };
        let Some(net) = net_at(design, library, nets, point) else {
            findings.push(error(
                "LABEL_NOT_ON_NET",
                format!(
                    "({:.2}, {:.2}) is not on a wire or pin that is part of a net; a label there would name nothing",
                    point.x.to_mm(),
                    point.y.to_mm()
                ),
                vec![],
            ));
            continue;
        };
        let here = net_name(net);
        let other = nets.iter().find(|n| net_name(n) == add.text && net_name(n) != here);
        if other.is_some() && !add.allow_join {
            findings.push(error(
                "LABEL_WOULD_MERGE_NETS",
                format!(
                    "a net called '{}' already exists; this label would join it to {here}. Pass allow_join to do that on purpose",
                    add.text
                ),
                vec![add.text.clone()],
            ));
            continue;
        }
        if !is_auto_name(&here) && here != add.text && other.is_none() {
            findings.push(error(
                "LABEL_NET_ALREADY_NAMED",
                format!("the net is already called '{here}'; rename its label instead of adding another name"),
                vec![here.clone()],
            ));
            continue;
        }
        let direction = add
            .direction
            .unwrap_or_else(|| free_label_direction(&scratch, library, &add.text, point));
        plan.operations.push(Operation::AddLabel {
            net: add.text.clone(),
            position: point,
            direction,
        });
        scratch.labels.push(crate::schematic::DesignLabel {
            id: String::new(),
            text: add.text.clone(),
            position: point,
            direction,
        });
        match other {
            Some(other) => {
                let (a, b) = (
                    expected_of(&mut expected, &here),
                    expected_of(&mut expected, &net_name(other)),
                );
                if let (Some(a), Some(b)) = (a, b) {
                    let merged: BTreeSet<String> = expected[a].pins.union(&expected[b].pins).cloned().collect();
                    expected[b].pins = merged;
                    expected.remove(a);
                }
                findings.push(warn(
                    "LABEL_JOINS_NETS",
                    format!("the label joins {here} and {} into one net", net_name(other)),
                ));
            }
            None => {
                if let Some(i) = expected_of(&mut expected, &here) {
                    expected[i].name = add.text.clone();
                }
            }
        }
    }

    if plan.operations.is_empty() && plan.removals.is_empty() && !has_errors(&findings) {
        findings.push(warn("NOTHING_TO_CHANGE", "no label changes were asked for".into()));
    }
    plan.expected_nets = expected;
    plan.findings = findings;
    plan
}

// ---------------------------------------------------------------- drawings

#[derive(Debug, Clone)]
pub enum DrawItem {
    Rectangle {
        min: Point,
        max: Point,
        stroke: Nm,
        filled: bool,
    },
    Circle {
        center: Point,
        radius: Nm,
        stroke: Nm,
        filled: bool,
    },
    Line {
        start: Point,
        end: Point,
        stroke: Nm,
    },
    Text {
        text: String,
        position: Point,
        size: Nm,
        angle_deg: u16,
        h: HAlign,
        v: VAlign,
    },
    /// A rectangle around the named parts (their bodies and text), with an optional title inside
    /// at the top left.
    BoxAround {
        parts: Vec<String>,
        margin: Nm,
        title: Option<String>,
        stroke: Nm,
    },
}

pub const DEFAULT_STROKE: Nm = Nm(254_000);
const TITLE_SIZE: Nm = Nm(1_905_000);

fn snap_out(r: Rect) -> Rect {
    let down = |v: Nm| Nm(v.0.div_euclid(GRID.0) * GRID.0);
    let up = |v: Nm| Nm((v.0 + GRID.0 - 1).div_euclid(GRID.0) * GRID.0);
    Rect {
        min: Point::new(down(r.min.x), down(r.min.y)),
        max: Point::new(up(r.max.x), up(r.max.y)),
    }
}

pub fn plan_draw(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    items: &[DrawItem],
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "draw".into(),
        expected_nets: expected_from(nets),
        ..Default::default()
    };
    let mut findings = Vec::new();
    for item in items {
        match item {
            DrawItem::Rectangle {
                min,
                max,
                stroke,
                filled,
            } => {
                if min == max {
                    findings.push(error("DRAW_EMPTY_SHAPE", "a rectangle needs a size".into(), vec![]));
                    continue;
                }
                plan.operations.push(Operation::AddShape {
                    kind: ShapeKind::Rectangle {
                        min: Point::new(min.x.min(max.x), min.y.min(max.y)).snap(GRID),
                        max: Point::new(min.x.max(max.x), min.y.max(max.y)).snap(GRID),
                    },
                    stroke: *stroke,
                    filled: *filled,
                });
            }
            DrawItem::Circle {
                center,
                radius,
                stroke,
                filled,
            } => {
                if radius.0 <= 0 {
                    findings.push(error("DRAW_EMPTY_SHAPE", "a circle needs a radius".into(), vec![]));
                    continue;
                }
                plan.operations.push(Operation::AddShape {
                    kind: ShapeKind::Circle {
                        center: center.snap(GRID),
                        radius: *radius,
                    },
                    stroke: *stroke,
                    filled: *filled,
                });
            }
            DrawItem::Line { start, end, stroke } => {
                if start == end {
                    findings.push(error(
                        "DRAW_EMPTY_SHAPE",
                        "a line needs two different ends".into(),
                        vec![],
                    ));
                    continue;
                }
                plan.operations.push(Operation::AddShape {
                    kind: ShapeKind::Line {
                        start: start.snap(GRID),
                        end: end.snap(GRID),
                    },
                    stroke: *stroke,
                    filled: false,
                });
            }
            DrawItem::Text {
                text,
                position,
                size,
                angle_deg,
                h,
                v,
            } => {
                if text.trim().is_empty()
                    || text.chars().count() > 2000
                    || text.chars().any(|c| c.is_control() && c != '\n')
                {
                    findings.push(error(
                        "DRAW_BAD_TEXT",
                        "the note is empty, too long, or has control characters".into(),
                        vec![],
                    ));
                    continue;
                }
                if angle_deg % 90 != 0 || *angle_deg >= 360 {
                    findings.push(error(
                        "DRAW_BAD_ANGLE",
                        "a note turns by 0, 90, 180, or 270 degrees".into(),
                        vec![],
                    ));
                    continue;
                }
                plan.operations.push(Operation::AddText {
                    text: text.clone(),
                    position: *position,
                    size: *size,
                    angle_deg: *angle_deg,
                    h: *h,
                    v: *v,
                });
            }
            DrawItem::BoxAround {
                parts,
                margin,
                title,
                stroke,
            } => {
                let mut area: Option<Rect> = None;
                let mut ok = true;
                for reference in parts {
                    let Some(symbol) = design.symbols.iter().find(|s| &s.reference == reference) else {
                        findings.push(error(
                            "DRAW_UNKNOWN_PART",
                            format!("there is no part {reference} to put a box around"),
                            vec![reference.clone()],
                        ));
                        ok = false;
                        continue;
                    };
                    let def = LibraryId::parse(&symbol.library_id)
                        .ok()
                        .and_then(|id| library.get(&id));
                    let mut rects: Vec<Rect> = def
                        .into_iter()
                        .flat_map(|d| {
                            d.body
                                .into_iter()
                                .chain(Rect::from_points(d.pins.iter().map(|p| p.local)))
                                .map(|r| r.transformed(symbol.orientation).translate(symbol.position))
                        })
                        .collect();
                    rects.extend(symbol.fields.iter().filter_map(|f| f.bounds()));
                    if rects.is_empty() {
                        rects.push(Rect::from_points([symbol.position]).expect("a point"));
                    }
                    for r in rects {
                        area = Some(area.map_or(r, |a| a.union(r)));
                    }
                }
                if !ok {
                    continue;
                }
                let Some(area) = area else {
                    findings.push(error(
                        "DRAW_EMPTY_SHAPE",
                        "name the parts to put a box around".into(),
                        vec![],
                    ));
                    continue;
                };
                let mut grown = area.inflate(*margin);
                if title.is_some() {
                    grown.min.y = grown.min.y - Nm(TITLE_SIZE.0 * 2);
                }
                let boxed = snap_out(grown);
                plan.operations.push(Operation::AddShape {
                    kind: ShapeKind::Rectangle {
                        min: boxed.min,
                        max: boxed.max,
                    },
                    stroke: *stroke,
                    filled: false,
                });
                if let Some(title) = title {
                    plan.operations.push(Operation::AddText {
                        text: title.clone(),
                        position: Point::new(boxed.min.x + GRID, boxed.min.y + GRID),
                        size: TITLE_SIZE,
                        angle_deg: 0,
                        h: HAlign::Left,
                        v: VAlign::Top,
                    });
                }
            }
        }
    }
    if plan.operations.is_empty() && !has_errors(&findings) {
        findings.push(warn("NOTHING_TO_CHANGE", "nothing to draw".into()));
    }
    plan.findings = findings;
    plan
}

// ---------------------------------------------------------------- other items

/// Removes or moves wires, labels, junctions, notes, and drawings by id. Parts are not handled
/// here (see `plan_arrange`).
pub fn plan_edit_items(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    remove: &[String],
    moves: &[(String, Point)],
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "edit-items".into(),
        ..Default::default()
    };
    let mut findings = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut nets_may_change = false;
    let at = |p: Point| format!("({:.2}, {:.2})", p.x.to_mm(), p.y.to_mm());

    for id in remove.iter().chain(moves.iter().map(|(id, _)| id)) {
        if !seen.insert(id) {
            findings.push(error("ITEM_TWICE", format!("{id} is named twice"), vec![id.clone()]));
        }
    }
    let known = |id: &str| {
        design.wires.iter().any(|w| w.id == id)
            || design.labels.iter().any(|l| l.id == id)
            || design.junctions.iter().any(|j| j.id == id)
            || design.texts.iter().any(|t| t.id == id)
            || design.shapes.iter().any(|s| s.id == id)
    };
    for id in remove.iter().chain(moves.iter().map(|(id, _)| id)) {
        if !known(id) {
            let hint = if design.symbols.iter().any(|s| s.id == *id) {
                " (that is a part; use sch_arrange_plan to delete or move parts)"
            } else {
                ""
            };
            findings.push(error(
                "ITEM_NOT_FOUND",
                format!("no wire, label, junction, note, or drawing with id {id}{hint}"),
                vec![id.clone()],
            ));
        }
    }

    for id in remove {
        if let Some(w) = design.wires.iter().find(|w| &w.id == id) {
            nets_may_change = true;
            plan.removals.push(Removal {
                kind: RemovedKind::Wire,
                id: id.clone(),
                description: format!("wire {} to {}", at(w.start), at(w.end)),
            });
        } else if let Some(l) = design.labels.iter().find(|l| &l.id == id) {
            nets_may_change = true;
            plan.removals.push(Removal {
                kind: RemovedKind::Label,
                id: id.clone(),
                description: format!("label {} at {}", l.text, at(l.position)),
            });
        } else if let Some(j) = design.junctions.iter().find(|j| &j.id == id) {
            nets_may_change = true;
            plan.removals.push(Removal {
                kind: RemovedKind::Junction,
                id: id.clone(),
                description: format!("junction at {}", at(j.position)),
            });
        } else if let Some(t) = design.texts.iter().find(|t| &t.id == id) {
            plan.removals.push(Removal {
                kind: RemovedKind::Text,
                id: id.clone(),
                description: format!("note \"{}\"", t.text.chars().take(40).collect::<String>()),
            });
        } else if let Some(s) = design.shapes.iter().find(|s| &s.id == id) {
            let b = s.kind.bounds();
            plan.removals.push(Removal {
                kind: RemovedKind::Shape,
                id: id.clone(),
                description: format!("drawing from {} to {}", at(b.min), at(b.max)),
            });
        }
    }

    for (id, delta) in moves {
        let delta = delta.snap(GRID);
        if design.wires.iter().any(|w| &w.id == id) || design.junctions.iter().any(|j| &j.id == id) {
            findings.push(error(
                "ITEM_CANNOT_MOVE",
                format!("{id} is a wire or junction; moving it would change the circuit. Move parts and use sch_reconnect_plan"),
                vec![id.clone()],
            ));
        } else if let Some(l) = design.labels.iter().find(|l| &l.id == id) {
            let to = l.position + delta;
            let before = net_at(design, library, nets, l.position).map(|n| n.name.clone());
            let after = net_at(design, library, nets, to).map(|n| n.name.clone());
            if before.is_none() || before != after {
                findings.push(error(
                    "ITEM_LABEL_LEAVES_NET",
                    format!("label {} would leave its net; move it along its own wire", l.text),
                    vec![id.clone()],
                ));
                continue;
            }
            plan.removals.push(Removal {
                kind: RemovedKind::Label,
                id: id.clone(),
                description: format!("label {} (moved to {})", l.text, at(to)),
            });
            plan.operations.push(Operation::AddLabel {
                net: l.text.clone(),
                position: to,
                direction: l.direction,
            });
        } else if let Some(t) = design.texts.iter().find(|t| &t.id == id) {
            plan.removals.push(Removal {
                kind: RemovedKind::Text,
                id: id.clone(),
                description: format!("note \"{}\" (moved)", t.text.chars().take(40).collect::<String>()),
            });
            plan.operations.push(Operation::AddText {
                text: t.text.clone(),
                position: t.position + delta,
                size: t.size,
                angle_deg: t.angle_deg,
                h: t.h,
                v: t.v,
            });
        } else if let Some(s) = design.shapes.iter().find(|s| &s.id == id) {
            let kind = match s.kind {
                ShapeKind::Rectangle { min, max } => ShapeKind::Rectangle {
                    min: min + delta,
                    max: max + delta,
                },
                ShapeKind::Circle { center, radius } => ShapeKind::Circle {
                    center: center + delta,
                    radius,
                },
                ShapeKind::Line { start, end } => ShapeKind::Line {
                    start: start + delta,
                    end: end + delta,
                },
            };
            plan.removals.push(Removal {
                kind: RemovedKind::Shape,
                id: id.clone(),
                description: "drawing (moved)".into(),
            });
            plan.operations.push(Operation::AddShape {
                kind,
                stroke: s.stroke,
                filled: s.filled,
            });
        }
    }

    if nets_may_change {
        findings.push(warn(
            "ITEMS_MAY_CHANGE_NETS",
            "removing wires, labels, or junctions can split or rename nets; the nets are not checked after this plan. Run sch_get_netlist, or sch_reconnect_plan to draw the connections again".into(),
        ));
    } else {
        plan.expected_nets = expected_from(nets);
    }
    if plan.operations.is_empty() && plan.removals.is_empty() && !has_errors(&findings) {
        findings.push(warn("NOTHING_TO_CHANGE", "nothing to remove or move".into()));
    }
    plan.findings = findings;
    plan
}
