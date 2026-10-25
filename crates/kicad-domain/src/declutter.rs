// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving text out of the way (Phase 9).
//!
//! The layout engine decides where parts and wires go; where a part's reference and value text and
//! a label's text end up is KiCad's doing, and it can land on a wire, a part, or other text. This
//! planner reads where the text really is, finds what it runs into, and moves it to the nearest
//! free spot beside its part (or turns a label to face another way). Text that cannot be placed
//! anywhere free is reported; the text of a `PWR_FLAG` is hidden instead, because it only repeats
//! what the symbol is. Nothing but text changes, so every net stays as it is.

use std::collections::{BTreeMap, BTreeSet};

use crate::geometry::{Dir, Nm, Point, Rect, GRID};
use crate::ids::LibraryId;
use crate::plan::{FieldChange, LibrarySymbol, Operation, OperationPlan, Removal, RemovedKind};
use crate::readability::{check_design, text_items};
use crate::schematic::{NetInfo, SchematicDesign};
use crate::text::{label_box, shrunk, text_box, HAlign, VAlign};
use crate::validation::{Finding, Severity};

#[derive(Debug, Clone)]
enum Owner {
    Field { symbol: usize, field: usize },
    Label(usize),
    Note,
}

#[derive(Debug, Clone)]
struct Item {
    name: String,
    owner: Owner,
    /// The symbol the text belongs to (a part's own body is not an obstacle to its own text only
    /// in the sense that text beside it is fine; text on it is not).
    part: Option<String>,
    bounds: Rect,
}

/// Does text in `bounds` run into anything else?
fn clashes(
    bounds: Rect,
    me: usize,
    part: &Option<String>,
    items: &[Item],
    wires: &[(Point, Point)],
    bodies: &[(String, Rect)],
) -> bool {
    let inside = shrunk(bounds);
    items
        .iter()
        .enumerate()
        .any(|(i, other)| i != me && inside.overlaps(shrunk(other.bounds)))
        || wires.iter().any(|(a, b)| inside.crossed_by(*a, *b))
        // A part's own text may sit on its own drawing (library symbols often put the value
        // inside the body); only other parts' bodies are obstacles.
        || bodies
            .iter()
            .any(|(reference, body)| part.as_deref() != Some(reference.as_str()) && inside.overlaps(*body))
}

fn distance(a: Point, b: Point) -> i64 {
    (a.x.0 - b.x.0).abs() + (a.y.0 - b.y.0).abs()
}

/// The way a new label at `at` should run so that its text stays clear of other text, wires, and
/// parts; to the right when nothing is better.
pub fn free_label_direction(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    text: &str,
    at: Point,
) -> Dir {
    let bodies: Vec<(String, Rect)> = design
        .symbols
        .iter()
        .filter_map(|s| {
            let def = LibraryId::parse(&s.library_id).ok().and_then(|id| library.get(&id))?;
            Some((
                s.reference.clone(),
                def.body?.transformed(s.orientation).translate(s.position),
            ))
        })
        .collect();
    let wires: Vec<(Point, Point)> = design.wires.iter().map(|w| (w.start, w.end)).collect();
    let items: Vec<Item> = text_items(design)
        .into_iter()
        .map(|(name, part, bounds)| Item {
            name,
            owner: Owner::Note,
            part,
            bounds,
        })
        .collect();
    [Dir::Right, Dir::Up, Dir::Left, Dir::Down]
        .into_iter()
        .find(|d| {
            let bounds = label_box(text, at, crate::text::DEFAULT_SIZE, *d);
            // A label sits at the end of its own wire: only wires that pass through the text count.
            let wires: Vec<(Point, Point)> = wires.iter().copied().filter(|(a, b)| *a != at && *b != at).collect();
            !clashes(bounds, usize::MAX, &None, &items, &wires, &bodies)
        })
        .unwrap_or(Dir::Right)
}

pub fn plan_declutter(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "declutter".into(),
        ..Default::default()
    };
    let bodies: Vec<(String, Rect)> = design
        .symbols
        .iter()
        .filter_map(|s| {
            let def = LibraryId::parse(&s.library_id).ok().and_then(|id| library.get(&id))?;
            Some((
                s.reference.clone(),
                def.body?.transformed(s.orientation).translate(s.position),
            ))
        })
        .collect();
    let wires: Vec<(Point, Point)> = design.wires.iter().map(|w| (w.start, w.end)).collect();

    // Every piece of text, in the order the design lists them.
    let mut items: Vec<Item> = Vec::new();
    for (si, symbol) in design.symbols.iter().enumerate() {
        for (fi, field) in symbol.fields.iter().enumerate() {
            if let Some(bounds) = field.bounds() {
                items.push(Item {
                    name: format!("{} {}", symbol.reference, field.name.to_lowercase()),
                    owner: Owner::Field { symbol: si, field: fi },
                    part: Some(symbol.reference.clone()),
                    bounds,
                });
            }
        }
    }
    for (li, label) in design.labels.iter().enumerate() {
        items.push(Item {
            name: format!("label {}", label.text),
            owner: Owner::Label(li),
            part: None,
            bounds: label.bounds(),
        });
    }
    for note in &design.texts {
        items.push(Item {
            name: format!("note {}", note.text.chars().take(24).collect::<String>()),
            owner: Owner::Note,
            part: None,
            bounds: note.bounds(),
        });
    }
    debug_assert_eq!(items.len(), text_items(design).len());

    let mut findings = Vec::new();
    let mut after = design.clone();
    let mut unresolved: Vec<String> = Vec::new();
    let mut touched_labels: BTreeSet<usize> = BTreeSet::new();

    for index in 0..items.len() {
        let item = items[index].clone();
        if !clashes(item.bounds, index, &item.part, &items, &wires, &bodies) {
            continue;
        }
        match item.owner {
            Owner::Field { symbol, field } => {
                let part = &design.symbols[symbol];
                let f = &part.fields[field];
                let mut candidates: Vec<(Point, HAlign, VAlign)> = Vec::new();
                if let Some((_, body)) = bodies.iter().find(|(r, _)| *r == part.reference) {
                    let (cx, cy) = ((body.min.x.0 + body.max.x.0) / 2, (body.min.y.0 + body.max.y.0) / 2);
                    let row = f.size.0 * 13 / 10;
                    for k in [0, -1, 1, -2, 2] {
                        let off = k * row;
                        candidates.push((
                            Point::new(Nm(body.max.x.0 + GRID.0), Nm(cy + off)),
                            HAlign::Left,
                            VAlign::Center,
                        ));
                        candidates.push((
                            Point::new(Nm(body.min.x.0 - GRID.0), Nm(cy + off)),
                            HAlign::Right,
                            VAlign::Center,
                        ));
                        candidates.push((
                            Point::new(Nm(cx + off), Nm(body.min.y.0 - GRID.0)),
                            HAlign::Center,
                            VAlign::Bottom,
                        ));
                        candidates.push((
                            Point::new(Nm(cx + off), Nm(body.max.y.0 + GRID.0)),
                            HAlign::Center,
                            VAlign::Top,
                        ));
                    }
                }
                candidates.sort_by_key(|(p, _, _)| distance(*p, f.position));
                let free = candidates.into_iter().find(|(p, h, v)| {
                    let bounds = text_box(&f.text, *p, f.size, f.angle_deg, *h, *v);
                    !clashes(bounds, index, &item.part, &items, &wires, &bodies)
                });
                let is_flag = part.reference.starts_with("#FLG")
                    || LibraryId::parse(&part.library_id).is_ok_and(|id| id.entry() == "PWR_FLAG");
                if let Some((position, h, v)) = free {
                    items[index].bounds = text_box(&f.text, position, f.size, f.angle_deg, h, v);
                    plan.field_changes.push(FieldChange {
                        id: part.id.clone(),
                        reference: part.reference.clone(),
                        field: f.name.clone(),
                        from: f.position,
                        position,
                        h,
                        v,
                        hide: false,
                    });
                    let target = &mut after.symbols[symbol].fields[field];
                    (target.position, target.h, target.v) = (position, h, v);
                } else if is_flag {
                    // Its text only repeats the symbol's name.
                    items[index].bounds = Rect {
                        min: f.position,
                        max: f.position,
                    };
                    plan.field_changes.push(FieldChange {
                        id: part.id.clone(),
                        reference: part.reference.clone(),
                        field: f.name.clone(),
                        from: f.position,
                        position: f.position,
                        h: f.h,
                        v: f.v,
                        hide: true,
                    });
                    after.symbols[symbol].fields[field].hidden = true;
                } else {
                    unresolved.push(item.name.clone());
                }
            }
            Owner::Label(li) => {
                let label = &design.labels[li];
                let size = crate::text::DEFAULT_SIZE;
                let turned = [Dir::Right, Dir::Up, Dir::Left, Dir::Down]
                    .into_iter()
                    .filter(|d| *d != label.direction)
                    .find(|d| {
                        let bounds = label_box(&label.text, label.position, size, *d);
                        !clashes(bounds, index, &item.part, &items, &wires, &bodies)
                    });
                match turned {
                    Some(direction) => {
                        items[index].bounds = label_box(&label.text, label.position, size, direction);
                        plan.removals.push(Removal {
                            kind: RemovedKind::Label,
                            id: label.id.clone(),
                            description: format!("label {} turned to face another way", label.text),
                        });
                        plan.operations.push(Operation::AddLabel {
                            net: label.text.clone(),
                            position: label.position,
                            direction,
                        });
                        after.labels[li].direction = direction;
                        touched_labels.insert(li);
                    }
                    None => unresolved.push(item.name.clone()),
                }
            }
            // Notes stay where they are; the others move around them.
            Owner::Note => {}
        }
    }

    plan.expected_nets = crate::edits::expected_from(nets);

    for name in &unresolved {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "TEXT_STILL_OVERLAPS",
            message: format!("the text of {name} still runs into something; there is no free place beside its part"),
            objects: vec![name.clone()],
        });
    }
    if plan.field_changes.is_empty() && plan.operations.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "NOTHING_TO_CHANGE",
            message: "no text could be moved to a better place".into(),
            objects: vec![],
        });
    }
    plan.readability_before = Some(check_design(design, library));
    plan.readability = check_design(&after, library);
    plan.findings = findings;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Orientation;
    use crate::schematic::{DesignField, DesignLabel, DesignSymbol, DesignWire};

    fn library() -> BTreeMap<LibraryId, LibrarySymbol> {
        crate::plan::tests::library()
    }

    fn field(name: &str, text: &str, at: Point) -> DesignField {
        DesignField {
            name: name.into(),
            text: text.into(),
            position: at,
            size: crate::text::DEFAULT_SIZE,
            h: HAlign::Left,
            v: VAlign::Center,
            ..Default::default()
        }
    }

    fn resistor(reference: &str, at: Point, fields: Vec<DesignField>) -> DesignSymbol {
        DesignSymbol {
            id: format!("00000000-0000-4000-8000-{:012}", reference.len() as i64 + at.x.0 % 1000),
            reference: reference.into(),
            value: "10k".into(),
            library_id: "Device:R".into(),
            position: at,
            orientation: Orientation::default(),
            unit: 1,
            fields,
            ..Default::default()
        }
    }

    #[test]
    fn text_on_a_wire_is_moved_off_it() {
        let at = Point::new(Nm(50_800_000), Nm(50_800_000));
        // The value text sits right on a wire that runs beside the part.
        let wire_x = Nm(at.x.0 + 2_540_000);
        let design = SchematicDesign {
            symbols: vec![resistor(
                "R1",
                at,
                vec![
                    field(
                        "Reference",
                        "R1",
                        Point::new(Nm(at.x.0 + 2_000_000), Nm(at.y.0 - 3_000_000)),
                    ),
                    field(
                        "Value",
                        "10k",
                        Point::new(Nm(wire_x.0 - 1_000_000), Nm(at.y.0 + 3_000_000)),
                    ),
                ],
            )],
            wires: vec![DesignWire {
                id: "w".into(),
                start: Point::new(wire_x, Nm(at.y.0 - 20_000_000)),
                end: Point::new(wire_x, Nm(at.y.0 + 20_000_000)),
            }],
            ..Default::default()
        };
        let before = check_design(&design, &library());
        assert!(!before.text_overlaps.is_empty(), "{:?}", before.text_overlaps);
        let plan = plan_declutter(&design, &[], &library());
        assert!(!plan.field_changes.is_empty(), "{:#?}", plan.findings);
        assert!(
            plan.readability.text_overlaps.is_empty(),
            "{:?}",
            plan.readability.text_overlaps
        );
    }

    #[test]
    fn a_label_is_turned_away_from_text() {
        let at = Point::new(Nm(50_800_000), Nm(50_800_000));
        let design = SchematicDesign {
            symbols: vec![resistor(
                "R1",
                at,
                vec![field(
                    "Value",
                    "10k",
                    Point::new(Nm(at.x.0 + 3_000_000), Nm(at.y.0 - 6_000_000)),
                )],
            )],
            labels: vec![DesignLabel {
                id: "l".into(),
                text: "SIGNAL".into(),
                position: Point::new(Nm(at.x.0 + 3_000_000), Nm(at.y.0 - 5_000_000)),
                direction: Dir::Right,
            }],
            ..Default::default()
        };
        let plan = plan_declutter(&design, &[], &library());
        assert!(
            plan.field_changes.iter().any(|c| c.field == "Value")
                || plan.operations.iter().any(|o| matches!(o, Operation::AddLabel { .. })),
            "{:#?}",
            plan
        );
        assert!(
            plan.readability.text_overlaps.is_empty(),
            "{:?}",
            plan.readability.text_overlaps
        );
    }

    #[test]
    fn nothing_is_changed_when_nothing_overlaps() {
        let at = Point::new(Nm(50_800_000), Nm(50_800_000));
        let design = SchematicDesign {
            symbols: vec![resistor(
                "R1",
                at,
                vec![field("Reference", "R1", Point::new(Nm(at.x.0 + 3_000_000), Nm(at.y.0)))],
            )],
            ..Default::default()
        };
        let plan = plan_declutter(&design, &[], &library());
        assert!(plan.field_changes.is_empty() && plan.operations.is_empty());
        assert!(plan.findings.iter().any(|f| f.code == "NOTHING_TO_CHANGE"));
    }
}
