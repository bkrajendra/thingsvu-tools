// SPDX-License-Identifier: GPL-3.0-or-later
//! Readability checks on a plan's geometry, independent of how the layout produced it
//! (plan §14, Phase 5). A short is an error; the other findings are warnings.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::geometry::{Point, Rect};
use crate::ids::LibraryId;
use crate::layout::power_symbol;
use crate::plan::{LibrarySymbol, Operation, OperationPlan};
use crate::schematic::SchematicDesign;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Readability {
    /// Pairs of nets whose wires or pins touch: the drawing would merge them.
    pub shorts: Vec<(String, String)>,
    /// Pairs of symbols (by reference) whose bodies overlap.
    pub overlapping_symbols: Vec<(String, String)>,
    /// Wires (by net) that pass through a symbol body (by reference).
    pub wires_through_symbols: Vec<(String, String)>,
    /// Power or ground nets named by a text label although a power symbol exists.
    pub labelled_power_nets: Vec<String>,
    /// Nets joined by labels because they could not be wired.
    pub fallback_nets: Vec<String>,
}

impl Readability {
    pub fn is_clean(&self) -> bool {
        self.shorts.is_empty()
            && self.overlapping_symbols.is_empty()
            && self.wires_through_symbols.is_empty()
            && self.labelled_power_nets.is_empty()
    }
}

fn on_segment(p: Point, a: Point, b: Point) -> bool {
    (a.x == b.x || a.y == b.y)
        && p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
}

/// Overlapping symbol bodies, and wires crossing a symbol body.
fn geometry_findings(
    bodies: &[(String, Rect)],
    pins_of: &BTreeMap<&str, Vec<Point>>,
    wires: &[(String, Point, Point)],
    report: &mut Readability,
) {
    for (i, (ra, a)) in bodies.iter().enumerate() {
        for (rb, b) in &bodies[i + 1..] {
            if a.overlaps(*b) {
                report.overlapping_symbols.push((ra.clone(), rb.clone()));
            }
        }
    }
    for (net, a, b) in wires {
        for (reference, body) in bodies {
            // A wire leaving the symbol's own pin may clip graphics that overhang the pin end
            // (an LED's arrows); KiCad's own drawings do the same.
            let pins = pins_of.get(reference.as_str()).map(Vec::as_slice).unwrap_or_default();
            let leaves_pin = pins.contains(a) || pins.contains(b);
            if !leaves_pin && body.crossed_by(*a, *b) {
                report.wires_through_symbols.push((net.clone(), reference.clone()));
            }
        }
    }
}

pub fn check(plan: &OperationPlan, library: &BTreeMap<LibraryId, LibrarySymbol>) -> Readability {
    let mut report = Readability {
        fallback_nets: plan.fallback_nets.clone(),
        ..Default::default()
    };
    // REF.PIN → net, from the plan's expected nets.
    let net_of_pin: BTreeMap<&str, &str> = plan
        .expected_nets
        .iter()
        .flat_map(|n| n.pins.iter().map(move |p| (p.as_str(), n.name.as_str())))
        .collect();

    let mut bodies: Vec<(String, Rect)> = Vec::new();
    let mut points: Vec<(String, Point)> = Vec::new(); // (net, connection point)
    let mut wires: Vec<(String, Point, Point)> = Vec::new();
    let mut pins_of: BTreeMap<&str, Vec<Point>> = BTreeMap::new(); // reference → pin points
    for op in &plan.operations {
        match op {
            Operation::PlaceSymbol {
                reference,
                library_id,
                position,
                orientation,
                ..
            } => {
                let Some(symbol) = library.get(library_id) else {
                    continue;
                };
                if let Some(body) = symbol.body {
                    bodies.push((reference.clone(), body.transformed(*orientation).translate(*position)));
                }
                for pin in &symbol.pins {
                    let at = *position + orientation.apply(pin.local).unwrap_or(pin.local);
                    let key = format!("{reference}.{}", pin.number);
                    let net = net_of_pin
                        .get(key.as_str())
                        .map(|n| n.to_string())
                        .unwrap_or(format!("unconnected {key}"));
                    points.push((net, at));
                    pins_of.entry(reference).or_default().push(at);
                }
            }
            Operation::PlacePowerSymbol {
                net,
                library_id,
                reference,
                position,
                orientation,
            } => {
                if let Some(symbol) = library.get(library_id) {
                    if let Some(body) = symbol.body {
                        bodies.push((reference.clone(), body.transformed(*orientation).translate(*position)));
                    }
                    for pin in &symbol.pins {
                        points.push((
                            net.clone(),
                            *position + orientation.apply(pin.local).unwrap_or(pin.local),
                        ));
                    }
                }
            }
            Operation::AddWire { net, start, end } => wires.push((net.clone(), *start, *end)),
            Operation::AddLabel { net, .. } => {
                if power_symbol(library, net).is_some() && !report.labelled_power_nets.contains(net) {
                    report.labelled_power_nets.push(net.clone());
                }
            }
            Operation::AddJunction { .. } => {}
        }
    }

    let mut short = |a: &str, b: &str| {
        let pair = if a < b {
            (a.to_owned(), b.to_owned())
        } else {
            (b.to_owned(), a.to_owned())
        };
        if !report.shorts.contains(&pair) {
            report.shorts.push(pair);
        }
    };
    for (i, (na, pa)) in points.iter().enumerate() {
        for (nb, pb) in &points[i + 1..] {
            if na != nb && pa == pb {
                short(na, nb);
            }
        }
    }
    for (net, a, b) in &wires {
        for (n, p) in &points {
            if n != net && on_segment(*p, *a, *b) {
                short(net, n);
            }
        }
        for (n, c, d) in &wires {
            if n != net && (on_segment(*c, *a, *b) || on_segment(*d, *a, *b)) {
                short(net, n);
            }
        }
    }

    geometry_findings(&bodies, &pins_of, &wires, &mut report);
    report
}

/// The same geometric checks on a drawing that exists already. Shorts are not looked for: the
/// wires carry no net names, and KiCad's netlist is the authority on connectivity.
pub fn check_design(design: &SchematicDesign, library: &BTreeMap<LibraryId, LibrarySymbol>) -> Readability {
    let mut report = Readability::default();
    let mut bodies: Vec<(String, Rect)> = Vec::new();
    let mut pins_of: BTreeMap<&str, Vec<Point>> = BTreeMap::new();
    for symbol in &design.symbols {
        let Some(def) = LibraryId::parse(&symbol.library_id)
            .ok()
            .and_then(|id| library.get(&id))
        else {
            continue;
        };
        if let Some(body) = def.body {
            bodies.push((
                symbol.reference.clone(),
                body.transformed(symbol.orientation).translate(symbol.position),
            ));
        }
        let pins = pins_of.entry(symbol.reference.as_str()).or_default();
        for pin in &def.pins {
            if let Some(offset) = symbol.orientation.apply(pin.local) {
                pins.push(symbol.position + offset);
            }
        }
    }
    let wires: Vec<(String, Point, Point)> = design
        .wires
        .iter()
        .map(|w| {
            let at = |p: Point| format!("({:.2}, {:.2})", p.x.to_mm(), p.y.to_mm());
            (format!("wire {}-{}", at(w.start), at(w.end)), w.start, w.end)
        })
        .collect();
    geometry_findings(&bodies, &pins_of, &wires, &mut report);
    report
}
