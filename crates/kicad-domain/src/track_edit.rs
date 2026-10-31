// SPDX-License-Identifier: GPL-3.0-or-later
//! Editing the copper that is already on a board (Phase 14.2): rip-up by net, area, layer, or id,
//! a new width, locking and unlocking, and cleaning up what a router leaves behind (dangling vias,
//! zero-length and duplicate tracks).
//!
//! Locked items are safe by default: a delete or a width change skips them unless they are named
//! by id (KiCad's own delete removes a locked track without complaint, Phase 14.0). Locking and
//! unlocking are the point of selecting them, so those do not skip.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::board::{Board, BoardTrack, BoardVia};
use crate::board_edit::{dangling_vias, measure, BoardEditPlan, BoardOp};
use crate::geometry::{Nm, Rect};
use crate::result::{ErrorCode, ToolError};

/// Which tracks and vias. Everything given must match (nets and layers are "any of").
#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub nets: Vec<String>,
    pub layers: Vec<String>,
    /// Items whose track ends or via lie inside this area (a track is in when both its ends are).
    pub area: Option<Rect>,
    /// Items named by id. Named items are the only locked ones a delete or width change touches.
    pub ids: Vec<String>,
    pub tracks: bool,
    pub vias: bool,
    /// Every item, as an explicit choice (`select` empty would otherwise be refused).
    pub all: bool,
}

impl Selector {
    fn is_empty(&self) -> bool {
        self.nets.is_empty() && self.layers.is_empty() && self.area.is_none() && self.ids.is_empty() && !self.all
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Delete,
    SetWidth(Nm),
    Lock,
    Unlock,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Cleanup {
    pub dangling_vias: bool,
    pub zero_length: bool,
    pub duplicates: bool,
}

impl Cleanup {
    fn any(self) -> bool {
        self.dangling_vias || self.zero_length || self.duplicates
    }
}

/// What the plan leaves alone because it is locked.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrackEditNotes {
    /// Locked items the selection matched but a delete or width change skipped (name them by id to
    /// include them).
    pub skipped_locked: Vec<String>,
    /// Items that already were as asked (already that width, already locked).
    pub unchanged: usize,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn inside(area: Rect, a: crate::geometry::Point, b: crate::geometry::Point) -> bool {
    area.contains(a) && area.contains(b)
}

fn describe_track(t: &BoardTrack) -> String {
    format!(
        "{} track {} ({:.2}, {:.2}) to ({:.2}, {:.2}) on {}",
        t.net,
        t.id,
        t.start.x.to_mm(),
        t.start.y.to_mm(),
        t.end.x.to_mm(),
        t.end.y.to_mm(),
        t.layer
    )
}

fn describe_via(v: &BoardVia) -> String {
    format!(
        "{} via {} at ({:.2}, {:.2})",
        v.net,
        v.id,
        v.at.x.to_mm(),
        v.at.y.to_mm()
    )
}

fn remove_track(t: &BoardTrack) -> BoardOp {
    BoardOp::RemoveTrack {
        id: t.id.clone(),
        net: t.net.clone(),
        layer: t.layer.clone(),
        start: t.start,
        end: t.end,
    }
}

fn remove_via(v: &BoardVia) -> BoardOp {
    BoardOp::RemoveVia {
        id: v.id.clone(),
        net: v.net.clone(),
        at: v.at,
    }
}

/// Plans an edit of the tracks and vias `select` picks with `action`, plus the cleanups asked for.
pub fn plan_track_edit(
    board: &Board,
    select: &Selector,
    action: Option<Action>,
    cleanup: Cleanup,
) -> Result<(BoardEditPlan, TrackEditNotes), ToolError> {
    if action.is_none() && !cleanup.any() {
        return Err(bad(
            "name an action (delete, set_width, lock, unlock) or a cleanup (dangling_vias, zero_length, duplicates)",
        ));
    }
    if action.is_some() && select.is_empty() {
        return Err(bad(
            "select what to change: nets, layers, an area, or ids; or all: true for every track and via",
        ));
    }
    if !select.tracks && !select.vias && action.is_some() {
        return Err(bad("select tracks, vias, or both"));
    }
    let known: BTreeSet<&str> = board.nets.iter().map(String::as_str).collect();
    for n in &select.nets {
        if !known.contains(n.as_str()) {
            return Err(bad(format!("net '{n}' is not on the board (use pcb_list_nets)")));
        }
    }
    for l in &select.layers {
        if !board.copper_layers.iter().any(|c| c == l) {
            return Err(bad(format!(
                "layer '{l}' is not a copper layer of this board ({})",
                board.copper_layers.join(", ")
            )));
        }
    }
    for id in &select.ids {
        if !board.tracks.iter().any(|t| &t.id == id) && !board.vias.iter().any(|v| &v.id == id) {
            return Err(bad(format!("no track or via with id {id} (use pcb_list_tracks)")));
        }
    }
    if let Some(Action::SetWidth(w)) = action {
        if w.0 <= 0 {
            return Err(bad("width_mm must be above zero"));
        }
    }

    let mut ops: Vec<BoardOp> = Vec::new();
    let mut notes = TrackEditNotes::default();
    let mut expected = board.clone();
    let mut warnings = Vec::new();
    let mut touched: BTreeSet<String> = BTreeSet::new();

    let named = |id: &str| select.ids.is_empty() || select.ids.iter().any(|i| i == id);
    let picked_track = |t: &BoardTrack| {
        select.tracks
            && (select.all
                || named(&t.id) && matches_filters(select, &t.net, Some(&t.layer), |a| inside(a, t.start, t.end)))
    };
    let picked_via = |v: &BoardVia| {
        select.vias && (select.all || named(&v.id) && matches_filters(select, &v.net, None, |a| a.contains(v.at)))
    };

    if let Some(action) = action {
        for t in board.tracks.iter().filter(|t| picked_track(t)) {
            let named = select.ids.iter().any(|i| i == &t.id);
            match action {
                Action::Delete | Action::SetWidth(_) if t.locked && !named => {
                    notes.skipped_locked.push(describe_track(t));
                }
                Action::Delete => {
                    ops.push(remove_track(t));
                    touched.insert(t.id.clone());
                    expected.tracks.retain(|e| e.id != t.id);
                }
                Action::SetWidth(w) if t.width == w => notes.unchanged += 1,
                Action::SetWidth(w) => {
                    ops.push(BoardOp::SetTrackWidth {
                        id: t.id.clone(),
                        net: t.net.clone(),
                        layer: t.layer.clone(),
                        from: t.width,
                        to: w,
                    });
                    touched.insert(t.id.clone());
                    if let Some(e) = expected.tracks.iter_mut().find(|e| e.id == t.id) {
                        e.width = w;
                    }
                }
                Action::Lock | Action::Unlock => {
                    let lock = action == Action::Lock;
                    if t.locked == lock {
                        notes.unchanged += 1;
                    } else {
                        ops.push(BoardOp::SetLocked {
                            id: t.id.clone(),
                            item: "track",
                            net: t.net.clone(),
                            locked: lock,
                        });
                        touched.insert(t.id.clone());
                        if let Some(e) = expected.tracks.iter_mut().find(|e| e.id == t.id) {
                            e.locked = lock;
                        }
                    }
                }
            }
        }
        for v in board.vias.iter().filter(|v| picked_via(v)) {
            let named = select.ids.iter().any(|i| i == &v.id);
            match action {
                Action::Delete if v.locked && !named => notes.skipped_locked.push(describe_via(v)),
                Action::Delete => {
                    ops.push(remove_via(v));
                    touched.insert(v.id.clone());
                    expected.vias.retain(|e| e.id != v.id);
                }
                Action::SetWidth(_) => {}
                Action::Lock | Action::Unlock => {
                    let lock = action == Action::Lock;
                    if v.locked == lock {
                        notes.unchanged += 1;
                    } else {
                        ops.push(BoardOp::SetLocked {
                            id: v.id.clone(),
                            item: "via",
                            net: v.net.clone(),
                            locked: lock,
                        });
                        touched.insert(v.id.clone());
                        if let Some(e) = expected.vias.iter_mut().find(|e| e.id == v.id) {
                            e.locked = lock;
                        }
                    }
                }
            }
        }
        if matches!(action, Action::SetWidth(_)) && select.vias && !select.tracks {
            warnings.push("a via has no track width; nothing was changed".into());
        } else if matches!(action, Action::SetWidth(_)) && select.vias {
            warnings.push("vias have no track width and were left as they are".into());
        }
    }

    // Cleanups never touch a locked item, and skip what the action already removes.
    if cleanup.zero_length {
        let zero: Vec<&BoardTrack> = board
            .tracks
            .iter()
            .filter(|t| t.start == t.end && !t.locked && !touched.contains(&t.id))
            .collect();
        for t in zero {
            ops.push(remove_track(t));
            touched.insert(t.id.clone());
            expected.tracks.retain(|e| e.id != t.id);
        }
    }
    if cleanup.duplicates {
        // The same net, layer, width, and ends (either way round): the first stays, a locked one
        // is preferred to stay.
        let key = |t: &BoardTrack| {
            let (a, b) = ((t.start.x.0, t.start.y.0), (t.end.x.0, t.end.y.0));
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            (t.net.clone(), t.layer.clone(), t.width.0, a, b)
        };
        let mut kept: std::collections::BTreeMap<_, &BoardTrack> = Default::default();
        let mut ordered: Vec<&BoardTrack> = board.tracks.iter().collect();
        ordered.sort_by_key(|t| !t.locked);
        for t in ordered {
            match kept.get(&key(t)) {
                None => {
                    kept.insert(key(t), t);
                }
                Some(_) if t.locked || touched.contains(&t.id) => {}
                Some(_) => {
                    ops.push(remove_track(t));
                    touched.insert(t.id.clone());
                    expected.tracks.retain(|e| e.id != t.id);
                }
            }
        }
    }
    if cleanup.dangling_vias {
        for v in dangling_vias(board) {
            if !v.locked && !touched.contains(&v.id) {
                ops.push(remove_via(v));
                touched.insert(v.id.clone());
                expected.vias.retain(|e| e.id != v.id);
            }
        }
    }

    if ops.is_empty() {
        let why = if !notes.skipped_locked.is_empty() {
            format!(
                "everything the selection matched is locked ({} item(s)); name the ids to include them",
                notes.skipped_locked.len()
            )
        } else if notes.unchanged > 0 {
            "everything selected is already as asked".to_owned()
        } else {
            "nothing matches the selection".to_owned()
        };
        return Err(bad(format!("nothing to change: {why}")));
    }
    if !notes.skipped_locked.is_empty() {
        warnings.push(format!(
            "{} locked item(s) were skipped; name their ids to include them",
            notes.skipped_locked.len()
        ));
    }
    let measure = measure(board);
    Ok((
        BoardEditPlan {
            kind: "tracks",
            operations: ops,
            before: measure.clone(),
            after: measure,
            warnings,
            expected,
        },
        notes,
    ))
}

/// Does an item with this net and layer pass the net, layer, and area filters?
fn matches_filters(select: &Selector, net: &str, layer: Option<&str>, in_area: impl Fn(Rect) -> bool) -> bool {
    (select.nets.is_empty() || select.nets.iter().any(|n| n == net))
        && (select.layers.is_empty() || layer.is_none_or(|l| select.layers.iter().any(|x| x == l)))
        && select.area.is_none_or(in_area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board_edit::verify_edit;
    use crate::geometry::Point;

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn track(id: &str, net: &str, layer: &str, a: Point, b: Point, locked: bool) -> BoardTrack {
        BoardTrack {
            id: id.into(),
            start: a,
            end: b,
            width: Nm::from_mm(0.25).unwrap(),
            layer: layer.into(),
            net: net.into(),
            locked,
        }
    }

    fn via(id: &str, net: &str, at: Point, locked: bool) -> BoardVia {
        BoardVia {
            id: id.into(),
            at,
            diameter: Nm::from_mm(0.6).unwrap(),
            drill: Nm::from_mm(0.3).unwrap(),
            net: net.into(),
            locked,
        }
    }

    fn board() -> Board {
        Board {
            nets: vec!["GND".into(), "VCC".into()],
            copper_layers: vec!["F.Cu".into(), "B.Cu".into()],
            tracks: vec![
                track("g1", "GND", "F.Cu", p(0.0, 0.0), p(10.0, 0.0), false),
                track("g2", "GND", "B.Cu", p(10.0, 0.0), p(10.0, 10.0), false),
                track("g3", "GND", "F.Cu", p(20.0, 0.0), p(30.0, 0.0), true),
                track("v1", "VCC", "F.Cu", p(0.0, 5.0), p(10.0, 5.0), false),
                track("v2", "VCC", "F.Cu", p(10.0, 5.0), p(0.0, 5.0), false),
                track("z1", "VCC", "F.Cu", p(3.0, 3.0), p(3.0, 3.0), false),
            ],
            vias: vec![
                via("a", "GND", p(10.0, 0.0), false),
                via("b", "VCC", p(50.0, 50.0), false),
                via("c", "GND", p(10.0, 10.0), true),
            ],
            ..Default::default()
        }
    }

    fn sel() -> Selector {
        Selector {
            tracks: true,
            vias: true,
            ..Default::default()
        }
    }

    fn ids(plan: &BoardEditPlan) -> Vec<String> {
        plan.operations
            .iter()
            .map(|o| match o {
                BoardOp::RemoveTrack { id, .. }
                | BoardOp::RemoveVia { id, .. }
                | BoardOp::SetTrackWidth { id, .. }
                | BoardOp::SetLocked { id, .. } => id.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_rip_up_by_net_skips_locked_items_unless_they_are_named() {
        let b = board();
        let s = Selector {
            nets: vec!["GND".into()],
            ..sel()
        };
        let (plan, notes) = plan_track_edit(&b, &s, Some(Action::Delete), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["g1", "g2", "a"]);
        assert_eq!(notes.skipped_locked.len(), 2, "{notes:?}");
        assert!(plan.warnings.iter().any(|w| w.contains("locked")));

        // Naming the locked track deletes it.
        let s = Selector {
            ids: vec!["g3".into()],
            ..sel()
        };
        let (plan, _) = plan_track_edit(&b, &s, Some(Action::Delete), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["g3"]);
        assert!(plan.expected.tracks.iter().all(|t| t.id != "g3"));
    }

    #[test]
    fn selection_by_layer_and_area() {
        let b = board();
        let s = Selector {
            layers: vec!["B.Cu".into()],
            ..sel()
        };
        let (plan, _) = plan_track_edit(&b, &s, Some(Action::Delete), Cleanup::default()).unwrap();
        // Vias are on every layer, so a layer filter leaves them in the selection.
        assert_eq!(ids(&plan), ["g2", "a", "b"]);
        let s = Selector {
            area: Rect::from_points([p(-1.0, -1.0), p(11.0, 6.0)]),
            tracks: true,
            ..Default::default()
        };
        let (plan, _) = plan_track_edit(&b, &s, Some(Action::Delete), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["g1", "v1", "v2", "z1"]);
    }

    #[test]
    fn a_new_width_and_locking() {
        let b = board();
        let s = Selector {
            nets: vec!["VCC".into()],
            tracks: true,
            ..Default::default()
        };
        let w = Nm::from_mm(0.5).unwrap();
        let (plan, _) = plan_track_edit(&b, &s, Some(Action::SetWidth(w)), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["v1", "v2", "z1"]);
        let mut after = b.clone();
        for t in &mut after.tracks {
            if t.net == "VCC" {
                t.width = w;
            }
        }
        assert!(verify_edit(&plan, &after).is_empty());
        assert!(
            !verify_edit(&plan, &b).is_empty(),
            "the old board does not satisfy the plan"
        );

        // Locking skips nothing, and an already-locked item is counted, not planned.
        let s = Selector {
            nets: vec!["GND".into()],
            ..sel()
        };
        let (plan, notes) = plan_track_edit(&b, &s, Some(Action::Lock), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["g1", "g2", "a"]);
        assert_eq!(notes.unchanged, 2);
        let (plan, _) = plan_track_edit(&b, &s, Some(Action::Unlock), Cleanup::default()).unwrap();
        assert_eq!(ids(&plan), ["g3", "c"]);
    }

    #[test]
    fn cleanup_removes_what_a_router_leaves() {
        let b = board();
        let all = Cleanup {
            dangling_vias: true,
            zero_length: true,
            duplicates: true,
        };
        let (plan, _) = plan_track_edit(&b, &Selector::default(), None, all).unwrap();
        // The zero-length track, the second of the two equal VCC tracks, the loose VCC via.
        // Via a joins g1 and g2 on two layers; c is locked and never cleaned.
        assert_eq!(ids(&plan), ["z1", "v2", "b"]);
    }

    #[test]
    fn unclear_requests_are_refused() {
        let b = board();
        let none = Cleanup::default();
        assert!(plan_track_edit(&b, &sel(), None, none).is_err(), "no action");
        assert!(
            plan_track_edit(&b, &sel(), Some(Action::Delete), none).is_err(),
            "no selection: all must be explicit"
        );
        let all = Selector { all: true, ..sel() };
        assert!(plan_track_edit(&b, &all, Some(Action::Delete), none).is_ok());
        let ghost = Selector {
            nets: vec!["NOPE".into()],
            ..sel()
        };
        assert!(plan_track_edit(&b, &ghost, Some(Action::Delete), none).is_err());
        let layer = Selector {
            layers: vec!["In1.Cu".into()],
            ..sel()
        };
        assert!(plan_track_edit(&b, &layer, Some(Action::Delete), none).is_err());
        let all_locked = Selector {
            nets: vec!["GND".into()],
            layers: vec!["F.Cu".into()],
            area: Rect::from_points([p(19.0, -1.0), p(31.0, 1.0)]),
            ..sel()
        };
        let e = plan_track_edit(&b, &all_locked, Some(Action::Delete), none).unwrap_err();
        assert!(e.message.contains("locked"), "{}", e.message);
    }
}
