// SPDX-License-Identifier: GPL-3.0-or-later
//! What differs between two states of a document, in words (Phase 14.5): what a reload would
//! lose, when the editor holds changes that are not in the file.

use std::collections::BTreeMap;

use crate::board::Board;
use crate::schematic::SchematicDesign;

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn counted(lines: &mut Vec<String>, name: (&str, &str), from: usize, to: usize) {
    if to > from {
        lines.push(format!("{} added", plural(to - from, name.0, name.1)));
    } else if from > to {
        lines.push(format!("{} removed", plural(from - to, name.0, name.1)));
    }
}

fn some_of(names: &[String], what: &str) -> String {
    const SHOWN: usize = 8;
    let mut shown: Vec<&str> = names.iter().take(SHOWN).map(String::as_str).collect();
    if names.len() > SHOWN {
        shown.push("…");
    }
    format!("{what}: {}", shown.join(", "))
}

/// What going from `from` (the file) to `to` (the open board) changes: the lines to show a person.
pub fn describe_board_changes(from: &Board, to: &Board) -> Vec<String> {
    let mut lines = Vec::new();
    let old: BTreeMap<&str, &crate::board::BoardFootprint> =
        from.footprints.iter().map(|f| (f.reference.as_str(), f)).collect();
    let new: BTreeMap<&str, &crate::board::BoardFootprint> =
        to.footprints.iter().map(|f| (f.reference.as_str(), f)).collect();
    let added: Vec<String> = new
        .keys()
        .filter(|r| !old.contains_key(*r))
        .map(|r| r.to_string())
        .collect();
    let removed: Vec<String> = old
        .keys()
        .filter(|r| !new.contains_key(*r))
        .map(|r| r.to_string())
        .collect();
    let moved: Vec<String> = new
        .iter()
        .filter_map(|(r, f)| {
            let was = old.get(r)?;
            let changed = was.position != f.position
                || was.layer != f.layer
                || was.locked != f.locked
                || (was.rotation_deg - f.rotation_deg).abs() > 0.01;
            changed.then(|| r.to_string())
        })
        .collect();
    if !added.is_empty() {
        lines.push(some_of(
            &added,
            &format!("{} added", plural(added.len(), "footprint", "footprints")),
        ));
    }
    if !removed.is_empty() {
        lines.push(some_of(
            &removed,
            &format!("{} removed", plural(removed.len(), "footprint", "footprints")),
        ));
    }
    if !moved.is_empty() {
        lines.push(some_of(
            &moved,
            &format!(
                "{} moved, turned, flipped, or locked",
                plural(moved.len(), "footprint", "footprints")
            ),
        ));
    }
    counted(&mut lines, ("track", "tracks"), from.tracks.len(), to.tracks.len());
    counted(&mut lines, ("via", "vias"), from.vias.len(), to.vias.len());
    counted(&mut lines, ("zone", "zones"), from.zones.len(), to.zones.len());
    counted(&mut lines, ("drawing", "drawings"), from.drawings, to.drawings);
    let track_state = |b: &Board| -> usize { b.tracks.iter().filter(|t| t.locked).count() };
    if from.tracks.len() == to.tracks.len() && track_state(from) != track_state(to) {
        lines.push("tracks locked or unlocked".to_owned());
    }
    if from.outline != to.outline {
        lines.push("the board outline differs".to_owned());
    }
    lines
}

/// The same for a schematic: parts by reference, and counts of wires, labels, and junctions.
pub fn describe_design_changes(from: &SchematicDesign, to: &SchematicDesign) -> Vec<String> {
    let mut lines = Vec::new();
    let key = |d: &SchematicDesign| -> BTreeMap<String, (String, String, crate::geometry::Point)> {
        d.symbols
            .iter()
            .map(|s| (s.reference.clone(), (s.value.clone(), s.footprint.clone(), s.position)))
            .collect()
    };
    let (old, new) = (key(from), key(to));
    let added: Vec<String> = new.keys().filter(|r| !old.contains_key(*r)).cloned().collect();
    let removed: Vec<String> = old.keys().filter(|r| !new.contains_key(*r)).cloned().collect();
    let changed: Vec<String> = new
        .iter()
        .filter(|(r, v)| old.get(*r).is_some_and(|o| o != *v))
        .map(|(r, _)| r.clone())
        .collect();
    if !added.is_empty() {
        lines.push(some_of(
            &added,
            &format!("{} added", plural(added.len(), "part", "parts")),
        ));
    }
    if !removed.is_empty() {
        lines.push(some_of(
            &removed,
            &format!("{} removed", plural(removed.len(), "part", "parts")),
        ));
    }
    if !changed.is_empty() {
        lines.push(some_of(
            &changed,
            &format!("{} changed or moved", plural(changed.len(), "part", "parts")),
        ));
    }
    counted(&mut lines, ("wire", "wires"), from.wires.len(), to.wires.len());
    counted(&mut lines, ("label", "labels"), from.labels.len(), to.labels.len());
    counted(
        &mut lines,
        ("junction", "junctions"),
        from.junctions.len(),
        to.junctions.len(),
    );
    counted(
        &mut lines,
        ("text note", "text notes"),
        from.texts.len(),
        to.texts.len(),
    );
    lines
}

/// The same for design rules: classes, assignments, patterns, and minimums.
pub fn describe_rules_changes(from: &crate::rules::RulesState, to: &crate::rules::RulesState) -> Vec<String> {
    use crate::rules::{field_name, min_name};
    let mm = |v: crate::geometry::Nm| format!("{} mm", (v.to_mm() * 1000.0).round() / 1000.0);
    let mut lines = Vec::new();
    for c in &to.classes {
        match from.class(&c.name) {
            None => lines.push(format!("net class '{}' added", c.name)),
            Some(old) => {
                for (f, v) in &c.values {
                    match old.values.get(f) {
                        Some(o) if o == v => {}
                        Some(o) => lines.push(format!(
                            "class '{}' {} {} -> {}",
                            c.name,
                            field_name(*f),
                            mm(*o),
                            mm(*v)
                        )),
                        None => lines.push(format!("class '{}' {} set to {}", c.name, field_name(*f), mm(*v))),
                    }
                }
                for f in old.values.keys().filter(|f| !c.values.contains_key(*f)) {
                    lines.push(format!("class '{}' {} cleared", c.name, field_name(*f)));
                }
            }
        }
    }
    for c in from.classes.iter().filter(|c| to.class(&c.name).is_none()) {
        lines.push(format!("net class '{}' removed", c.name));
    }
    for (net, class) in &to.assignments {
        match from.assignments.get(net) {
            Some(o) if o == class => {}
            Some(o) => lines.push(format!("net {net} moved from {} to {}", o.join(", "), class.join(", "))),
            None => lines.push(format!("net {net} assigned to {}", class.join(", "))),
        }
    }
    for net in from.assignments.keys().filter(|n| !to.assignments.contains_key(*n)) {
        lines.push(format!("net {net} no longer assigned"));
    }
    for (p, k) in &to.patterns {
        match from.patterns.iter().find(|(q, _)| q == p) {
            Some((_, o)) if o == k => {}
            Some((_, o)) => lines.push(format!("pattern {p} moved from {o} to {k}")),
            None => lines.push(format!("pattern {p} added for {k}")),
        }
    }
    for (p, _) in from
        .patterns
        .iter()
        .filter(|(p, _)| !to.patterns.iter().any(|(q, _)| q == p))
    {
        lines.push(format!("pattern {p} removed"));
    }
    for (f, v) in &to.minimums {
        match from.minimums.get(f) {
            Some(o) if o == v => {}
            Some(o) => lines.push(format!("minimum {} {} -> {}", min_name(*f), mm(*o), mm(*v))),
            None => lines.push(format!("minimum {} set to {}", min_name(*f), mm(*v))),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardFootprint, BoardTrack};
    use crate::geometry::{Nm, Point};

    fn fp(reference: &str, x: i64) -> BoardFootprint {
        BoardFootprint {
            id: reference.into(),
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            layer: "F.Cu".into(),
            position: Point::new(Nm(x), Nm(0)),
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            exclude_from_bom: false,
            exclude_from_pos: false,
            board_only: false,
            pads: vec![],
            courtyard: None,
            courtyard_local: None,
        }
    }

    #[test]
    fn rules_that_differ_are_described() {
        use crate::rules::{apply_op, ClassField, ClassState, MinField, RuleOp, RulesState};
        let mut from = RulesState::default();
        from.classes.push(ClassState {
            name: "Default".into(),
            priority: Some(i32::MAX),
            values: Default::default(),
        });
        let mut to = from.clone();
        let nm = |v: f64| Nm::from_mm(v).unwrap();
        for op in [
            RuleOp::ClassSet {
                name: "Default".into(),
                field: ClassField::TrackWidth,
                value: nm(0.25),
            },
            RuleOp::ClassAdd {
                name: "Power".into(),
                priority: 0,
            },
            RuleOp::Assign {
                net: "GND".into(),
                class: "Power".into(),
            },
            RuleOp::MinSet {
                field: MinField::TrackWidth,
                value: nm(0.15),
            },
        ] {
            apply_op(&mut to, &op);
        }
        let lines = describe_rules_changes(&from, &to);
        assert_eq!(
            lines,
            [
                "class 'Default' track width set to 0.25 mm",
                "net class 'Power' added",
                "net GND assigned to Power",
                "minimum track width set to 0.15 mm"
            ]
        );
        assert!(describe_rules_changes(&to, &to).is_empty());
        let back = describe_rules_changes(&to, &from);
        assert!(back.iter().any(|l| l.contains("'Power' removed")), "{back:?}");
    }

    #[test]
    fn a_board_that_differs_is_described() {
        let disk = Board {
            footprints: vec![fp("R1", 0), fp("R2", 0)],
            ..Default::default()
        };
        let mut live = Board {
            footprints: vec![fp("R1", 5_000_000), fp("R3", 0)],
            ..Default::default()
        };
        live.tracks.push(BoardTrack {
            id: "t".into(),
            start: Point::default(),
            end: Point::default(),
            width: Nm(1),
            layer: "F.Cu".into(),
            net: "A".into(),
            locked: false,
        });
        let lines = describe_board_changes(&disk, &live);
        assert!(lines.iter().any(|l| l.contains("1 footprint added: R3")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("1 footprint removed: R2")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("moved") && l.contains("R1")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l == "1 track added"), "{lines:?}");
        assert!(describe_board_changes(&disk, &disk).is_empty());
    }
}
