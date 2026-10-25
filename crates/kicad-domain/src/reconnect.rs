// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiring parts that already have their places (Phase 8).
//!
//! The layout engine decides where parts go and draws wires as it does. This router takes the
//! places as given: every pin gets a short stub, nets of power and ground end in power symbols,
//! and the other nets are joined by orthogonal wires that touch nothing of another net and cross no
//! part. A net that cannot be wired like that is named with a label at each pin instead, so the
//! circuit is always complete.

use std::collections::{BTreeMap, BTreeSet};

use crate::circuit::CircuitSpec;
use crate::geometry::{Dir, Nm, Orientation, Point, Rect, GRID};
use crate::ids::{LibraryId, LogicalId};
use crate::layout::{drawing_operations, is_ground_name, on_segment, power_symbol, Drawing, LayoutOutput, NetKind};
use crate::plan::{LibrarySymbol, Operation};
use crate::validation::{Finding, Severity};

const STUB: Nm = Nm(2 * GRID.0);

/// Where a part sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub position: Point,
    pub orientation: Orientation,
}

struct Pin {
    net: Option<usize>,
    at: Point,
    out: Dir,
    /// A flag (`PWR_FLAG`): it marks a net rather than belonging to the circuit proper.
    flag: bool,
    label: String,
}

fn stub_end(pin: &Pin) -> Point {
    pin.at + pin.out.step(STUB)
}

/// Candidate routes from `a` to `b`, shortest first.
fn candidates(a: Point, b: Point) -> Vec<Vec<Point>> {
    if a == b {
        return vec![vec![a]];
    }
    let mid_x = Point::new(Nm((a.x.0 + b.x.0) / 2), Nm(0)).snap(GRID).x;
    let mid_y = Point::new(Nm(0), Nm((a.y.0 + b.y.0) / 2)).snap(GRID).y;
    let mut routes = Vec::new();
    if a.x == b.x || a.y == b.y {
        routes.push(vec![a, b]);
    } else {
        routes.push(vec![a, Point::new(b.x, a.y), b]);
        routes.push(vec![a, Point::new(a.x, b.y), b]);
        routes.push(vec![a, Point::new(mid_x, a.y), Point::new(mid_x, b.y), b]);
        routes.push(vec![a, Point::new(a.x, mid_y), Point::new(b.x, mid_y), b]);
    }
    routes
}

struct Router<'a> {
    drawing: Drawing,
    bodies: &'a [Rect],
    pins: Vec<(String, Point)>,
}

impl Router<'_> {
    fn free(&self, net: &str, points: &[Point]) -> bool {
        self.drawing.path_is_free(net, points, &self.pins)
            && points
                .windows(2)
                .all(|w| !self.bodies.iter().any(|body| body.crossed_by(w[0], w[1])))
    }

    /// Draws the first free route from `a` to `b`; false when there is none.
    fn connect(&mut self, net: &str, a: Point, b: Point) -> bool {
        for route in candidates(a, b) {
            if self.free(net, &route) {
                self.drawing.path(net, &route);
                return true;
            }
        }
        false
    }
}

/// Draws the connections of `spec` between parts at `placements`. Every placed part is part of
/// the output (as a `PlaceSymbol` at its place), so the result can be compared with a design.
pub fn route(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    references: &BTreeMap<LogicalId, String>,
    placements: &BTreeMap<LogicalId, Placement>,
    taken: &mut BTreeSet<String>,
) -> LayoutOutput {
    let kinds: Vec<NetKind> = spec
        .nets
        .iter()
        .map(|n| match power_symbol(library, &n.name) {
            Some(_) if is_ground_name(&n.name) => NetKind::Ground,
            Some(_) => NetKind::Power,
            None => NetKind::Signal,
        })
        .collect();
    let mut net_of: BTreeMap<(&LogicalId, &str), usize> = BTreeMap::new();
    for (i, net) in spec.nets.iter().enumerate() {
        for p in &net.pins {
            net_of.insert((&p.component_id, p.pin_number.as_str()), i);
        }
    }

    let mut operations = Vec::new();
    let mut bodies = Vec::new();
    let mut pins: Vec<Pin> = Vec::new();
    for c in &spec.components {
        let (Some(at), Some(symbol)) = (placements.get(&c.id), library.get(&c.library_id)) else {
            continue;
        };
        operations.push(Operation::PlaceSymbol {
            component_id: c.id.clone(),
            library_id: c.library_id.clone(),
            reference: references[&c.id].clone(),
            value: c.value.clone(),
            footprint: c.footprint.clone(),
            position: at.position,
            orientation: at.orientation,
        });
        if let Some(body) = symbol.body {
            bodies.push(body.transformed(at.orientation).translate(at.position));
        }
        for pin in symbol.pins.iter().filter(|p| p.unit == 0 || p.unit == 1) {
            pins.push(Pin {
                net: net_of.get(&(&c.id, pin.number.as_str())).copied(),
                at: at.position + at.orientation.apply(pin.local).unwrap_or(pin.local),
                out: pin.outward.transformed(at.orientation),
                flag: symbol.is_power,
                label: format!("{}.{}", references[&c.id], pin.number),
            });
        }
    }

    let mut router = Router {
        drawing: Drawing::default(),
        bodies: &bodies,
        pins: pins
            .iter()
            .map(|p| {
                (
                    p.net
                        .map(|n| spec.nets[n].name.clone())
                        .unwrap_or_else(|| format!("unconnected {}", p.label)),
                    p.at,
                )
            })
            .collect(),
    };
    for pin in &pins {
        router.drawing.end(pin.at);
    }

    let mut findings = Vec::new();
    let mut fallback_nets = Vec::new();
    for (n, net) in spec.nets.iter().enumerate() {
        let members: Vec<&Pin> = pins.iter().filter(|p| p.net == Some(n)).collect();
        if members.is_empty() {
            continue;
        }
        let backup = router.drawing.clone();
        let named = |router: &mut Router, pin: &Pin, facing: Option<Dir>| {
            // A stub, then the name of the net at its end: a power symbol or a label.
            let free_stub = router.free(&net.name, &[pin.at, stub_end(pin)]);
            let end = if free_stub { stub_end(pin) } else { pin.at };
            if free_stub {
                router.drawing.wire(&net.name, pin.at, end);
            }
            match (kinds[n], facing) {
                (NetKind::Signal, _) | (_, None) => router.drawing.labels.push((net.name.clone(), end, pin.out)),
                (_, Some(dir)) => {
                    router.drawing.power.push((net.name.clone(), end, dir));
                    router.drawing.end(end);
                }
            }
        };
        let facing_of = |pin: &Pin| {
            Some(match (kinds[n], pin.out) {
                (_, dir @ (Dir::Up | Dir::Down)) => dir,
                (NetKind::Ground, _) => Dir::Down,
                _ => Dir::Up,
            })
        };

        match kinds[n] {
            NetKind::Ground | NetKind::Power => {
                let (flags, parts): (Vec<&Pin>, Vec<&Pin>) = members.iter().copied().partition(|p| p.flag);
                for pin in &parts {
                    named(&mut router, pin, facing_of(pin));
                }
                // A flag joins the net by a wire to the nearest stub end, or gets a symbol itself.
                for flag in flags {
                    let nearest = parts
                        .iter()
                        .map(|p| stub_end(p))
                        .min_by_key(|e| (e.x.0 - flag.at.x.0).abs() + (e.y.0 - flag.at.y.0).abs());
                    let wired = nearest.is_some_and(|to| router.connect(&net.name, flag.at, to));
                    if !wired {
                        named(&mut router, flag, facing_of(flag));
                    }
                }
            }
            NetKind::Signal => {
                if members.len() == 1 {
                    if !net.name.starts_with("Net-(") {
                        named(&mut router, members[0], None);
                    }
                    continue;
                }
                let stubs_free = members.iter().all(|p| router.free(&net.name, &[p.at, stub_end(p)]));
                let mut routed = stubs_free;
                if routed {
                    for pin in &members {
                        router.drawing.wire(&net.name, pin.at, stub_end(pin));
                    }
                    // A tree over the stub ends, nearest first.
                    let ends: Vec<Point> = members.iter().map(|p| stub_end(p)).collect();
                    let mut joined = vec![0usize];
                    while joined.len() < ends.len() && routed {
                        let best = (0..ends.len())
                            .filter(|i| !joined.contains(i))
                            .flat_map(|i| joined.iter().map(move |&j| (i, j)))
                            .min_by_key(|&(i, j)| {
                                (ends[i].x.0 - ends[j].x.0).abs() + (ends[i].y.0 - ends[j].y.0).abs()
                            });
                        let Some((i, j)) = best else { break };
                        if router.connect(&net.name, ends[j], ends[i]) {
                            joined.push(i);
                        } else {
                            routed = false;
                        }
                    }
                }
                if routed && !net.name.starts_with("Net-(") {
                    // KiCad names a net nobody named after a pin; the name must be written down.
                    let first = members[0];
                    router
                        .drawing
                        .labels
                        .push((net.name.clone(), stub_end(first), first.out));
                }
                if !routed {
                    router.drawing = backup;
                    for pin in &members {
                        named(&mut router, pin, None);
                    }
                    fallback_nets.push(net.name.clone());
                }
            }
        }
    }

    // A wire end that lands on the inside of another wire of its net needs a junction.
    let wires = router.drawing.wires.clone();
    for (net, a, b) in &wires {
        for end in [*a, *b] {
            let on_other = wires
                .iter()
                .any(|(n, c, d)| n == net && end != *c && end != *d && on_segment(end, *c, *d));
            if on_other {
                let count = router.drawing.ends.entry((end.x.0, end.y.0)).or_default();
                *count = (*count).max(3);
            }
        }
    }

    operations.extend(drawing_operations(&router.drawing, library, taken));
    for name in &fallback_nets {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "NET_DRAWN_WITH_LABELS",
            message: format!(
                "net '{name}' could not be wired without crossing parts or other nets; it is joined by labels"
            ),
            objects: vec![name.clone()],
        });
    }
    LayoutOutput {
        operations,
        findings,
        fallback_nets,
    }
}
