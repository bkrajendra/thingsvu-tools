// SPDX-License-Identifier: GPL-3.0-or-later
//! Which net a power symbol or flag belongs to, worked out from the drawing.
//!
//! `kicad-cli`'s netlister leaves out power symbols and `PWR_FLAG`s, but a tidy has to put a flag
//! back on the net it was on. A pin belongs to the net of whatever it touches: a part's pin at the
//! same point, or a wire that has a part's pin on it (wires meeting end to end, or end to side,
//! are one wire network).

use std::collections::BTreeMap;

use crate::geometry::Point;
use crate::ids::{Kiid, LibraryId};
use crate::plan::LibrarySymbol;
use crate::schematic::{NetInfo, NetPin, SchematicDesign};

/// True when `p` lies on the segment `a`–`b`, ends included.
fn on_segment(p: Point, a: Point, b: Point) -> bool {
    let (px, py) = (p.x.0 as i128, p.y.0 as i128);
    let (ax, ay, bx, by) = (a.x.0 as i128, a.y.0 as i128, b.x.0 as i128, b.y.0 as i128);
    let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
    cross == 0 && px >= ax.min(bx) && px <= ax.max(bx) && py >= ay.min(by) && py <= ay.max(by)
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Adds the pins of power symbols and flags (references starting with `#`) that the netlist left
/// out to the nets they touch. Pins that touch nothing, or are already listed, are left alone.
pub fn attach_power_pins(design: &SchematicDesign, library: &BTreeMap<LibraryId, LibrarySymbol>, nets: &mut [NetInfo]) {
    // Wire networks.
    let wires: Vec<(Point, Point)> = design.wires.iter().map(|w| (w.start, w.end)).collect();
    let mut parent: Vec<usize> = (0..wires.len()).collect();
    for i in 0..wires.len() {
        for j in i + 1..wires.len() {
            let (a, b) = (wires[i], wires[j]);
            if [a.0, a.1].iter().any(|&p| on_segment(p, b.0, b.1))
                || [b.0, b.1].iter().any(|&p| on_segment(p, a.0, a.1))
            {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }

    let roots: Vec<usize> = (0..wires.len()).map(|i| find(&mut parent, i)).collect();

    // The pins of real parts: where they are, and which net they are on.
    let net_of_pin: BTreeMap<(&str, &str), usize> = nets
        .iter()
        .enumerate()
        .flat_map(|(n, net)| {
            net.pins
                .iter()
                .map(move |p| ((p.reference.as_str(), p.pin.as_str()), n))
        })
        .collect();
    let mut pin_points: Vec<(Point, usize)> = Vec::new();
    let mut hidden: Vec<(&str, &str, Point, Kiid)> = Vec::new();
    // Power symbols name their net: where each one is, and what it says.
    let mut named: Vec<(Point, &str)> = Vec::new();
    for symbol in &design.symbols {
        let Some(def) = LibraryId::parse(&symbol.library_id)
            .ok()
            .and_then(|id| library.get(&id))
        else {
            continue;
        };
        for pin in def.pins.iter().filter(|p| p.unit == 0 || p.unit == symbol.unit) {
            let Some(offset) = symbol.orientation.apply(pin.local) else {
                continue;
            };
            let at = symbol.position + offset;
            if symbol.reference.starts_with('#') {
                if let Ok(id) = Kiid::new(symbol.id.clone()) {
                    hidden.push((symbol.reference.as_str(), pin.number.as_str(), at, id));
                }
                if symbol.reference.starts_with("#PWR") {
                    named.push((at, symbol.value.as_str()));
                }
            } else if let Some(&n) = net_of_pin.get(&(symbol.reference.as_str(), pin.number.as_str())) {
                pin_points.push((at, n));
            }
        }
    }

    // Net of each wire network: the net of any real pin that touches one of its wires.
    let mut network_net: BTreeMap<usize, usize> = BTreeMap::new();
    for &(at, n) in &pin_points {
        for (i, w) in wires.iter().enumerate() {
            if on_segment(at, w.0, w.1) {
                network_net.entry(roots[i]).or_insert(n);
            }
        }
    }

    // A wire network that touches no part can still be on a net by name, through a power symbol.
    let mut network_name: BTreeMap<usize, &str> = BTreeMap::new();
    for &(at, name) in &named {
        for (i, w) in wires.iter().enumerate() {
            if on_segment(at, w.0, w.1) {
                network_name.entry(roots[i]).or_insert(name);
            }
        }
    }
    let net_named = |name: &str| nets.iter().position(|n| n.name.trim_start_matches('/') == name);

    let mut additions: Vec<(usize, NetPin)> = Vec::new();
    for (reference, pin, at, id) in hidden {
        if net_of_pin.contains_key(&(reference, pin)) {
            continue;
        }
        let direct = pin_points.iter().find(|(p, _)| *p == at).map(|&(_, n)| n);
        let through_wire = || {
            (0..wires.len())
                .find(|&i| on_segment(at, wires[i].0, wires[i].1))
                .and_then(|i| network_net.get(&roots[i]).copied())
        };
        let by_name = || {
            let by_symbol = named.iter().find(|(p, _)| *p == at).map(|&(_, name)| name);
            let by_network = (0..wires.len())
                .find(|&i| on_segment(at, wires[i].0, wires[i].1))
                .and_then(|i| network_name.get(&roots[i]).copied());
            by_symbol.or(by_network).and_then(net_named)
        };
        if let Some(n) = direct.or_else(through_wire).or_else(by_name) {
            additions.push((
                n,
                NetPin {
                    reference: reference.to_owned(),
                    pin: pin.to_owned(),
                    symbol_id: id,
                },
            ));
        }
    }
    for (n, pin) in additions {
        nets[n].pins.push(pin);
    }
}
