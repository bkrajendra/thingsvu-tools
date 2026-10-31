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
