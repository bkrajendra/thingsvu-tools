// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving a pin to another net, or swapping the nets of two pins (Phase 14.7), without touching
//! anything else on the sheet.
//!
//! Where the drawing at a pin is simple (a stub wire that ends in a label or a power symbol, or a
//! wire that runs into the rest of its net), only that is changed: the pin's own wire and the name
//! at its end go, and a stub with the new net's name (a power symbol, or a label) is drawn. Where it
//! is not, the wiring of the nets involved is worked out again as `plan_reconnect` does, and only
//! what belongs to those nets is kept. Either way the result is checked for new shorts and wires
//! through parts before it is offered, and KiCad checks every net when it is applied.

use std::collections::{BTreeMap, BTreeSet};

use crate::connectivity::net_at;
use crate::geometry::{Dir, Nm, Point, GRID};
use crate::ids::LibraryId;
use crate::layout::{drawing_operations, is_ground_name, power_symbol, Drawing};
use crate::plan::{LibrarySymbol, Operation, OperationPlan, Removal, RemovedKind};
use crate::readability::check_design;
use crate::result::{ErrorCode, ToolError};
use crate::schematic::{DesignJunction, DesignLabel, DesignSymbol, DesignWire, NetInfo, NetPin, SchematicDesign};
use crate::tidy::plan_reconnect;
use crate::validation::{Finding, Severity};

const STUB: Nm = Nm(2 * GRID.0);

#[derive(Debug, Clone, PartialEq)]
pub enum PinNetRequest {
    /// Pin `pin` (`REF.PIN`) leaves its net and joins `to_net`.
    Move { pin: String, to_net: String },
    /// The two pins exchange their nets.
    Swap { a: String, b: String },
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn norm(name: &str) -> &str {
    name.trim_start_matches('/')
}

fn on_segment(p: Point, a: Point, b: Point) -> bool {
    (a.x == b.x || a.y == b.y)
        && p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
}

/// `REF.PIN` split at the last dot.
fn split_pin(text: &str) -> Result<(&str, &str), ToolError> {
    text.rsplit_once('.')
        .filter(|(r, p)| !r.is_empty() && !p.is_empty())
        .ok_or_else(|| bad(format!("a pin is REF.PIN, like U1.3, not '{text}'")))
}

struct Located {
    reference: String,
    pin: String,
    symbol_id: crate::ids::Kiid,
    /// The net it is on now, as an index into the nets.
    net: Option<usize>,
}

fn locate(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    text: &str,
) -> Result<Located, ToolError> {
    let (reference, pin) = split_pin(text)?;
    if reference.starts_with('#') {
        return Err(bad(format!(
            "{text}: power symbols and flags are not moved; they follow their net"
        )));
    }
    let symbol = design
        .symbols
        .iter()
        .find(|s| s.reference == reference)
        .ok_or_else(|| bad(format!("no part {reference} on the sheet (use sch_list_symbols)")))?;
    let def = LibraryId::parse(&symbol.library_id)
        .ok()
        .and_then(|id| library.get(&id))
        .ok_or_else(|| bad(format!("{reference}: its library symbol is not available")))?;
    if !def.pins.iter().any(|p| p.number == pin) {
        let mut numbers: Vec<&str> = def.pins.iter().map(|p| p.number.as_str()).collect();
        numbers.dedup();
        return Err(bad(format!(
            "{reference} has no pin {pin} (it has: {})",
            numbers.join(", ")
        )));
    }
    let net = nets
        .iter()
        .position(|n| n.pins.iter().any(|p| p.reference == reference && p.pin == pin));
    let symbol_id =
        crate::ids::Kiid::new(symbol.id.clone()).map_err(|e| bad(format!("{reference} has a bad id: {e}")))?;
    Ok(Located {
        reference: reference.to_owned(),
        pin: pin.to_owned(),
        symbol_id,
        net,
    })
}

fn remove_pin(nets: &mut [NetInfo], p: &Located) {
    for n in nets.iter_mut() {
        n.pins.retain(|q| !(q.reference == p.reference && q.pin == p.pin));
    }
}

fn add_pin(nets: &mut Vec<NetInfo>, p: &Located, net: &str) {
    let pin = NetPin {
        reference: p.reference.clone(),
        pin: p.pin.clone(),
        symbol_id: p.symbol_id.clone(),
    };
    match nets.iter_mut().find(|n| norm(&n.name) == norm(net)) {
        Some(n) => n.pins.push(pin),
        None => nets.push(NetInfo {
            name: net.to_owned(),
            pins: vec![pin],
            other_items: 0,
        }),
    }
}

/// The sheet as it would be after `removed` items go and `added` operations are drawn.
fn design_after(design: &SchematicDesign, removed: &BTreeSet<&str>, added: &[Operation]) -> SchematicDesign {
    let mut d = design.clone();
    d.wires.retain(|w| !removed.contains(w.id.as_str()));
    d.labels.retain(|l| !removed.contains(l.id.as_str()));
    d.junctions.retain(|j| !removed.contains(j.id.as_str()));
    d.symbols.retain(|s| !removed.contains(s.id.as_str()));
    for (n, op) in added.iter().enumerate() {
        let id = format!("new-{n}");
        match op {
            Operation::AddWire { start, end, .. } => d.wires.push(DesignWire {
                id,
                start: *start,
                end: *end,
            }),
            Operation::AddLabel {
                net,
                position,
                direction,
            } => d.labels.push(DesignLabel {
                id,
                text: net.clone(),
                position: *position,
                direction: *direction,
            }),
            Operation::AddJunction { position } => d.junctions.push(DesignJunction {
                id,
                position: *position,
            }),
            Operation::PlacePowerSymbol {
                net,
                library_id,
                reference,
                position,
                orientation,
            } => d.symbols.push(DesignSymbol {
                id,
                reference: reference.clone(),
                value: net.clone(),
                library_id: library_id.to_string(),
                position: *position,
                orientation: *orientation,
                unit: 1,
                ..Default::default()
            }),
            _ => {}
        }
    }
    d
}

// ---------------------------------------------------------------- the local change

/// A pin on the sheet: where it is and which way it points.
fn pin_geometry(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    reference: &str,
    pin: &str,
) -> Option<(Point, Dir)> {
    let symbol = design.symbols.iter().find(|s| s.reference == reference)?;
    let def = library.get(&LibraryId::parse(&symbol.library_id).ok()?)?;
    let p = def
        .pins
        .iter()
        .find(|p| p.number == pin && (p.unit == 0 || p.unit == symbol.unit))
        .or_else(|| def.pins.iter().find(|p| p.number == pin))?;
    Some((
        symbol.position + symbol.orientation.apply(p.local)?,
        p.outward.transformed(symbol.orientation),
    ))
}

/// Every pin on the sheet: (reference, place).
fn all_pins(design: &SchematicDesign, library: &BTreeMap<LibraryId, LibrarySymbol>) -> Vec<(String, Point)> {
    let mut out = Vec::new();
    for s in &design.symbols {
        let Some(def) = LibraryId::parse(&s.library_id).ok().and_then(|id| library.get(&id)) else {
            continue;
        };
        for p in def.pins.iter().filter(|p| p.unit == 0 || p.unit == s.unit) {
            if let Some(o) = s.orientation.apply(p.local) {
                out.push((s.reference.clone(), s.position + o));
            }
        }
    }
    out
}

#[derive(Default)]
struct LocalRemoval {
    /// Wires from the pin to the first place where something else joins.
    wires: Vec<String>,
    /// Labels and power symbols that named the net only for this pin.
    names: Vec<String>,
    /// The wire ends at a free end here (what names the net there is in `names`).
    leaf: Option<Point>,
}

/// What goes when a pin leaves its net, found from the drawing around the pin: the wire from the
/// pin to the first place where something else joins it, and the label or power symbol that ends
/// it. `None` when the drawing there is not that simple.
fn local_removal(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    pin_at: Point,
) -> Option<LocalRemoval> {
    let pins = all_pins(design, library);
    let touching = |at: Point, skip: &BTreeSet<&str>| -> Vec<&DesignWire> {
        design
            .wires
            .iter()
            .filter(|w| !skip.contains(w.id.as_str()) && on_segment(at, w.start, w.end))
            .collect()
    };
    let is_end = |w: &DesignWire, at: Point| w.start == at || w.end == at;
    let parts_at = |at: Point| pins.iter().filter(|(r, p)| *p == at && !r.starts_with('#')).count();
    let power_symbols_at = |at: Point| -> Vec<&DesignSymbol> {
        design
            .symbols
            .iter()
            .filter(|s| s.reference.starts_with('#') && pins.iter().any(|(r, p)| *r == s.reference && *p == at))
            .collect()
    };

    let mut out = LocalRemoval::default();
    // A power symbol or label sitting on the pin itself names the net there.
    for s in power_symbols_at(pin_at) {
        if !s.reference.starts_with("#PWR") {
            return None;
        }
        out.names.push(s.id.clone());
    }
    out.names.extend(
        design
            .labels
            .iter()
            .filter(|l| l.position == pin_at)
            .map(|l| l.id.clone()),
    );
    if design.junctions.iter().any(|j| j.position == pin_at) {
        return None;
    }

    let mut skip: BTreeSet<&str> = BTreeSet::new();
    let first = touching(pin_at, &skip);
    match first.len() {
        0 => return Some(out),
        1 => {}
        _ => return None,
    }
    let mut wire = first[0];
    if !is_end(wire, pin_at) {
        return None;
    }
    let mut from = pin_at;
    for _ in 0..50 {
        skip.insert(wire.id.as_str());
        out.wires.push(wire.id.clone());
        let to = if wire.start == from { wire.end } else { wire.start };
        // Nothing else may touch the middle of the wire.
        let inside_busy = pins
            .iter()
            .any(|(_, p)| *p != from && *p != to && on_segment(*p, wire.start, wire.end))
            || design
                .labels
                .iter()
                .any(|l| l.position != from && l.position != to && on_segment(l.position, wire.start, wire.end))
            || design
                .junctions
                .iter()
                .any(|j| j.position != from && j.position != to && on_segment(j.position, wire.start, wire.end))
            || design.wires.iter().any(|w| {
                w.id != wire.id
                    && [w.start, w.end]
                        .iter()
                        .any(|e| *e != from && *e != to && on_segment(*e, wire.start, wire.end))
            });
        if inside_busy {
            return None;
        }
        let others = touching(to, &skip);
        let labels_here: Vec<&DesignLabel> = design.labels.iter().filter(|l| l.position == to).collect();
        let symbols_here = power_symbols_at(to);
        let junction_here = design.junctions.iter().any(|j| j.position == to);
        let parts_here = parts_at(to);
        if others.is_empty() && parts_here == 0 && !junction_here {
            // The far end of the pin's own wire: what names the net there goes with it.
            for s in symbols_here {
                if !s.reference.starts_with("#PWR") {
                    return None;
                }
                out.names.push(s.id.clone());
            }
            out.names.extend(labels_here.iter().map(|l| l.id.clone()));
            out.leaf = Some(to);
            return Some(out);
        }
        if others.len() == 1
            && parts_here == 0
            && symbols_here.is_empty()
            && labels_here.is_empty()
            && !junction_here
            && is_end(others[0], to)
        {
            wire = others[0];
            from = to;
            continue;
        }
        // Something else joins here: the rest of the net stays connected through this point.
        return Some(out);
    }
    None
}

/// What the local change of one pin does.
struct LocalMove {
    removals: BTreeMap<String, String>,
    drawing: Drawing,
}

/// Tries the change for one pin: removes what ties it to its old net and names the new one at its
/// stub (`to_net` none: the pin is left on no net). `None` when the drawing around it is more than a
/// stub.
fn move_pin_locally(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    pin: (&str, &str),
    to_net: Option<&str>,
    work: &mut LocalMove,
) -> Option<()> {
    let (pin_at, out) = pin_geometry(design, library, pin.0, pin.1)?;
    let removal = local_removal(design, library, pin_at)?;
    let stub_end = pin_at + out.step(STUB);
    // A stub that is already there is kept.
    let keep_stub = to_net.is_some()
        && removal.wires.len() == 1
        && removal.leaf == Some(stub_end)
        && design
            .wires
            .iter()
            .any(|w| w.id == removal.wires[0] && (w.start == pin_at || w.end == pin_at));
    let label = |what: &str| format!("{what} of {}.{}", pin.0, pin.1);
    for id in &removal.names {
        work.removals.insert(id.clone(), label("name"));
    }
    if !keep_stub {
        for id in &removal.wires {
            work.removals.insert(id.clone(), label("wire"));
        }
    }
    let Some(to_net) = to_net.map(norm) else {
        return Some(());
    };
    if !keep_stub {
        work.drawing.wire(to_net, pin_at, stub_end);
    }
    // The new net is named at the end of the stub.
    name_at(&mut work.drawing, library, to_net, stub_end, out);
    Some(())
}

/// Draws the name of a net at `at`: a power symbol facing away from the pin, or a label.
fn name_at(drawing: &mut Drawing, library: &BTreeMap<LibraryId, LibrarySymbol>, net: &str, at: Point, out: Dir) {
    match power_symbol(library, net) {
        Some(_) => {
            let facing = match out {
                Dir::Up | Dir::Down => out,
                _ if is_ground_name(net) => Dir::Down,
                _ => Dir::Up,
            };
            drawing.power.push((net.to_owned(), at, facing));
            drawing.end(at);
        }
        None => drawing.labels.push((net.to_owned(), at, out)),
    }
}

/// The request as changes of single pins: (pin, the net it goes to, none for no net).
fn pin_moves<'a>(
    request: &'a PinNetRequest,
    a: &'a Located,
    b: Option<&'a Located>,
    nets: &'a [NetInfo],
) -> Vec<(&'a Located, Option<String>)> {
    match (request, b) {
        (PinNetRequest::Move { to_net, .. }, _) => vec![(a, Some(to_net.clone()))],
        (PinNetRequest::Swap { .. }, Some(b)) => vec![
            (a, b.net.map(|i| nets[i].name.clone())),
            (b, a.net.map(|i| nets[i].name.clone())),
        ],
        _ => vec![],
    }
}

/// The local plan: removals and operations, or `None` when a pin's drawing is not simple enough.
fn local_plan(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    moves: &[(&Located, Option<String>)],
    new_nets: &[NetInfo],
    involved: &BTreeSet<String>,
) -> Option<(Vec<Removal>, Vec<Operation>)> {
    let mut work = LocalMove {
        removals: BTreeMap::new(),
        drawing: Drawing::default(),
    };
    for (p, to) in moves {
        move_pin_locally(design, library, (&p.reference, &p.pin), to.as_deref(), &mut work)?;
    }
    // A net must still be named by something once its old name's label or symbol went with a pin.
    for net in involved {
        let Some(info) = new_nets.iter().find(|n| norm(&n.name) == net.as_str()) else {
            continue;
        };
        if info.name.starts_with("Net-(") {
            continue;
        }
        let named_before = design
            .labels
            .iter()
            .any(|l| norm(&l.text) == net && !work.removals.contains_key(&l.id))
            || design.symbols.iter().any(|s| {
                s.reference.starts_with("#PWR") && norm(&s.value) == net && !work.removals.contains_key(&s.id)
            });
        let named_now = named_before
            || work.drawing.labels.iter().any(|(n, _, _)| norm(n) == net)
            || work.drawing.power.iter().any(|(n, _, _)| norm(n) == net);
        if !named_now {
            let p = info.pins.first()?;
            let (at, out) = pin_geometry(design, library, &p.reference, &p.pin)?;
            work.drawing.labels.push((norm(&info.name).to_owned(), at, out));
        }
    }
    let mut taken: BTreeSet<String> = design.symbols.iter().map(|s| s.reference.clone()).collect();
    let operations = drawing_operations(&work.drawing, library, &mut taken);
    let removals = work
        .removals
        .into_iter()
        .map(|(id, description)| {
            let kind = if design.wires.iter().any(|w| w.id == id) {
                RemovedKind::Wire
            } else if design.labels.iter().any(|l| l.id == id) {
                RemovedKind::Label
            } else {
                RemovedKind::PowerSymbol
            };
            Removal { kind, id, description }
        })
        .collect();
    Some((removals, operations))
}

// ---------------------------------------------------------------- the general change

/// What a redraw removes and adds, and what it found.
type Redraw = (Vec<Removal>, Vec<Operation>, Vec<Finding>);

/// The wiring of the new circuit worked out again, keeping only the nets involved.
fn redraw_involved(
    design: &SchematicDesign,
    nets: &[NetInfo],
    new_nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    involved: &BTreeSet<String>,
) -> Result<Redraw, ToolError> {
    let mut full = plan_reconnect(design, new_nets, library);
    if full.findings.iter().any(|f| f.severity == Severity::Error) {
        return Err(bad(format!(
            "the wiring cannot be worked out: {}",
            full.findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .map(|f| f.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    let net_name_at = |p: Point| net_at(design, library, nets, p).map(|n| norm(&n.name).to_owned());
    let mut mine: BTreeSet<String> = BTreeSet::new();
    for w in &design.wires {
        if net_name_at(w.start)
            .or_else(|| net_name_at(w.end))
            .is_some_and(|n| involved.contains(&n))
        {
            mine.insert(w.id.clone());
        }
    }
    for l in &design.labels {
        if involved.contains(norm(&l.text)) || net_name_at(l.position).is_some_and(|n| involved.contains(&n)) {
            mine.insert(l.id.clone());
        }
    }
    for j in &design.junctions {
        if net_name_at(j.position).is_some_and(|n| involved.contains(&n)) {
            mine.insert(j.id.clone());
        }
    }
    for s in design.symbols.iter().filter(|s| s.reference.starts_with("#PWR")) {
        if involved.contains(norm(&s.value)) {
            mine.insert(s.id.clone());
        }
    }
    full.removals.retain(|r| mine.contains(&r.id));
    let wires_of_mine: Vec<(Point, Point)> = full
        .operations
        .iter()
        .filter_map(|op| match op {
            Operation::AddWire { net, start, end } if involved.contains(norm(net)) => Some((*start, *end)),
            _ => None,
        })
        .collect();
    full.operations.retain(|op| match op {
        Operation::AddWire { net, .. } | Operation::AddLabel { net, .. } | Operation::PlacePowerSymbol { net, .. } => {
            involved.contains(norm(net))
        }
        Operation::AddJunction { position } => wires_of_mine.iter().any(|(a, b)| on_segment(*position, *a, *b)),
        _ => false,
    });
    Ok((full.removals, full.operations, full.findings))
}

/// Plans the change. `nets` is the circuit as it is (power symbols and flags attached).
pub fn plan_pin_net(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    request: &PinNetRequest,
) -> Result<OperationPlan, ToolError> {
    let mut new_nets: Vec<NetInfo> = nets.to_vec();
    let mut findings: Vec<Finding> = Vec::new();
    let (first, second, involved, what) = match request {
        PinNetRequest::Move { pin, to_net } => {
            let p = locate(design, nets, library, pin)?;
            let from = p.net.map(|i| nets[i].name.clone());
            if to_net.is_empty() || to_net.chars().any(|c| c.is_whitespace() || c.is_control()) || to_net.len() > 256 {
                return Err(bad("to_net is a net name without spaces"));
            }
            if from.as_deref().is_some_and(|f| norm(f) == norm(to_net)) {
                return Err(bad(format!("{pin} is on {to_net} already")));
            }
            if !nets.iter().any(|n| norm(&n.name) == norm(to_net)) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: "NEW_NET",
                    message: format!("no net is called {to_net}; the pin starts a new one"),
                    objects: vec![to_net.clone()],
                });
            }
            remove_pin(&mut new_nets, &p);
            add_pin(&mut new_nets, &p, to_net);
            let mut set: BTreeSet<String> = [norm(to_net).to_owned()].into();
            set.extend(from.iter().map(|f| norm(f).to_owned()));
            let what = format!("{pin} moves from {} to {to_net}", from.as_deref().unwrap_or("no net"));
            (p, None, set, what)
        }
        PinNetRequest::Swap { a, b } => {
            if a == b {
                return Err(bad("a swap needs two different pins"));
            }
            let (pa, pb) = (locate(design, nets, library, a)?, locate(design, nets, library, b)?);
            if pa.net == pb.net {
                return Err(bad(format!(
                    "{a} and {b} are on the same net; swapping them changes nothing"
                )));
            }
            let name = |p: &Located| p.net.map(|i| nets[i].name.clone());
            let (na, nb) = (name(&pa), name(&pb));
            remove_pin(&mut new_nets, &pa);
            remove_pin(&mut new_nets, &pb);
            if let Some(n) = &nb {
                add_pin(&mut new_nets, &pa, n);
            }
            if let Some(n) = &na {
                add_pin(&mut new_nets, &pb, n);
            }
            let set = [&na, &nb].into_iter().flatten().map(|n| norm(n).to_owned()).collect();
            let what = format!(
                "{a} ({}) and {b} ({}) swap nets",
                na.as_deref().unwrap_or("no net"),
                nb.as_deref().unwrap_or("no net")
            );
            (pa, Some(pb), set, what)
        }
    };
    new_nets.retain(|n| !n.pins.is_empty());
    let moves = pin_moves(request, &first, second.as_ref(), nets);

    let before = check_design(design, library);
    let acceptable = |removals: &[Removal], operations: &[Operation]| {
        let removed: BTreeSet<&str> = removals.iter().map(|r| r.id.as_str()).collect();
        let now = check_design(&design_after(design, &removed, operations), library);
        let worse = now.shorts.iter().any(|s| !before.shorts.contains(s))
            || now
                .wires_through_symbols
                .iter()
                .any(|s| !before.wires_through_symbols.contains(s));
        (!worse).then_some(now)
    };

    let mut how = "only the pin's own wire and the name at its end were changed";
    let mut chosen = local_plan(design, library, &moves, &new_nets, &involved)
        .and_then(|(r, o)| acceptable(&r, &o).map(|now| (r, o, now)));
    if chosen.is_none() {
        how = "the wiring of the nets involved was drawn again";
        let (r, o, f) = redraw_involved(design, nets, &new_nets, library, &involved)?;
        findings.extend(f.into_iter().filter(|f| f.code == "NET_DRAWN_WITH_LABELS"));
        match acceptable(&r, &o) {
            Some(now) => chosen = Some((r, o, now)),
            None => {
                return Err(bad(format!(
                    "the new wiring of {} cannot be drawn without crossing something that is already drawn; \
                     sch_reconnect_plan redraws the whole sheet instead",
                    involved.iter().cloned().collect::<Vec<_>>().join(" and ")
                )))
            }
        }
    }
    let (removals, operations, now) = chosen.expect("a plan was chosen");
    if removals.is_empty() && operations.is_empty() {
        return Err(bad("the drawing already shows these connections; nothing to change"));
    }
    findings.push(Finding {
        severity: Severity::Warning,
        code: "PIN_NET_CHANGE",
        message: format!("{what}; {how}"),
        objects: vec![],
    });
    Ok(OperationPlan {
        circuit: "pin_net".into(),
        operations,
        removals,
        expected_nets: crate::edits::expected_from(&new_nets),
        findings,
        readability: now,
        readability_before: Some(before),
        ..Default::default()
    })
}
