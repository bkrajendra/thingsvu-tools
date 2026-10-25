// SPDX-License-Identifier: GPL-3.0-or-later
//! Schematic layout: turns a validated circuit into placed symbols, wires, power symbols, labels,
//! and junctions that read like a hand-drawn schematic (plan §14, Phase 5).
//!
//! Strategy, all deterministic:
//! 1. Nets are *ground* (a ground symbol at every pin), *power* (wired, named by one power symbol)
//!    when KiCad has a power symbol with the net's name, or *signal* (wired, named by a label).
//! 2. Parts are *attached* (power-library parts such as `PWR_FLAG`), *shunts* (two pins, one on
//!    ground: decoupling capacitors, indicator LEDs), or on the *main path*.
//! 3. The main path runs left to right on a baseline, in breadth-first order from a source
//!    (a connector if there is one). Each part is rotated so pins toward the source face left,
//!    pins away from it face right, and ground pins face down; this also orients diodes.
//! 4. Shunts hang below the gap of the net they serve. Each wired net becomes a straight rail on
//!    the baseline with vertical drops to its shunts, broken at every connection point, with
//!    junctions where three or more connections meet.
//! 5. Any net that cannot be drawn that way without crossing a symbol falls back to a short stub
//!    and a label (or power symbol) at each pin, so the circuit is always complete.

use std::collections::{BTreeMap, BTreeSet};

use crate::circuit::CircuitSpec;
use crate::geometry::{Dir, Nm, Orientation, Point, Rect, GRID};
use crate::ids::{LibraryId, LogicalId};
use crate::plan::{LibraryPin, LibrarySymbol, Operation};
use crate::validation::{Finding, Severity};

/// Length of a pin stub: two grid steps.
const STUB: Nm = Nm(2 * GRID.0);
const START: Point = Point::new(Nm(50_800_000), Nm(63_500_000));
/// Minimum clear space between main-path parts.
const MIN_GAP: Nm = Nm(12_700_000);
/// Horizontal pitch of shunt parts hanging below a net (room for their value text).
const SHUNT_PITCH: Nm = Nm(10_160_000);
/// How far below the baseline a shunt's upper pin sits.
const SHUNT_DROP: Nm = Nm(7_620_000);
/// Vertical pitch when a column holds more than one main-path part.
const ROW_PITCH: Nm = Nm(25_400_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    Ground,
    Power,
    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Main,
    Shunt,
    Attached,
}

/// Is `name` conventionally a ground net?
pub fn is_ground_name(name: &str) -> bool {
    let n = name.trim_start_matches('/').to_ascii_uppercase();
    matches!(
        n.as_str(),
        "0V" | "VSS" | "VSSA" | "EARTH" | "AGND" | "DGND" | "PGND" | "SGND"
    ) || n.starts_with("GND")
}

/// The KiCad power symbol for net `name`, if the library has one (`power:<name>`).
pub fn power_symbol<'a>(library: &'a BTreeMap<LibraryId, LibrarySymbol>, name: &str) -> Option<&'a LibrarySymbol> {
    let id = LibraryId::parse(&format!("power:{name}")).ok()?;
    library.get(&id).filter(|s| s.is_power && s.pins.len() == 1)
}

/// Library ids a circuit's layout may need: its components and a power symbol per net name.
pub fn library_ids_for(spec: &CircuitSpec) -> Vec<LibraryId> {
    let mut ids: BTreeSet<LibraryId> = spec.components.iter().map(|c| c.library_id.clone()).collect();
    for net in &spec.nets {
        if let Ok(id) = LibraryId::parse(&format!("power:{}", net.name)) {
            ids.insert(id);
        }
    }
    ids.into_iter().collect()
}

struct Part<'a> {
    component: usize,
    id: LogicalId,
    symbol: &'a LibrarySymbol,
    pins: Vec<&'a LibraryPin>,
    role: Role,
    column: usize,
    orientation: Orientation,
    position: Point,
}

impl Part<'_> {
    fn pin_point(&self, pin: &LibraryPin) -> Point {
        self.position + self.orientation.apply(pin.local).unwrap_or(pin.local)
    }

    fn pin_dir(&self, pin: &LibraryPin) -> Dir {
        pin.outward.transformed(self.orientation)
    }

    /// The symbol body on the sheet (graphics only), for collision checks.
    fn body(&self) -> Option<Rect> {
        self.symbol
            .body
            .map(|b| b.transformed(self.orientation).translate(self.position))
    }

    /// Body plus pin connection points, for spacing.
    fn extent(&self) -> Rect {
        let local = self
            .symbol
            .body
            .into_iter()
            .chain(Rect::from_points(self.pins.iter().map(|p| p.local)))
            .reduce(Rect::union)
            .unwrap_or(Rect {
                min: Point::default(),
                max: Point::default(),
            });
        local.transformed(self.orientation)
    }
}

/// Everything the layout produces for a plan.
pub struct LayoutOutput {
    pub operations: Vec<Operation>,
    pub findings: Vec<Finding>,
    /// Nets drawn with stubs and labels instead of wires.
    pub fallback_nets: Vec<String>,
}

/// Collects drawing primitives and counts connections per point for junctions.
#[derive(Default)]
struct Drawing {
    wires: Vec<(String, Point, Point)>,
    labels: Vec<(String, Point, Dir)>,
    power: Vec<(String, Point, Dir)>,
    /// Connection ends per point: wire ends, pins, power-symbol pins.
    ends: BTreeMap<(i64, i64), u32>,
}

impl Drawing {
    fn end(&mut self, p: Point) {
        *self.ends.entry((p.x.0, p.y.0)).or_default() += 1;
    }

    fn wire(&mut self, net: &str, a: Point, b: Point) {
        if a != b {
            self.wires.push((net.to_owned(), a, b));
            self.end(a);
            self.end(b);
        }
    }

    /// A path through consecutive points.
    fn path(&mut self, net: &str, points: &[Point]) {
        for w in points.windows(2) {
            self.wire(net, w[0], w[1]);
        }
    }

    /// Would a wire of `net` from `a` to `b` connect to anything of another net? Touching a pin
    /// or wire end, or running along another wire, connects in KiCad; a plain crossing does not.
    fn touches_other_net(&self, net: &str, a: Point, b: Point, pins: &[(String, Point)]) -> bool {
        pins.iter().any(|(n, p)| n != net && on_segment(*p, a, b))
            || self.wires.iter().any(|(n, c, d)| {
                n != net
                    && (on_segment(*c, a, b) || on_segment(*d, a, b) || on_segment(a, *c, *d) || on_segment(b, *c, *d))
            })
            || self.power.iter().any(|(n, p, _)| n != net && on_segment(*p, a, b))
    }

    fn path_is_free(&self, net: &str, points: &[Point], pins: &[(String, Point)]) -> bool {
        points
            .windows(2)
            .all(|w| !self.touches_other_net(net, w[0], w[1], pins))
            && points.iter().all(|p| !self.touches_other_net(net, *p, *p, pins))
    }
}

/// Is `p` on the axis-aligned segment a–b (ends included)?
fn on_segment(p: Point, a: Point, b: Point) -> bool {
    let (lo_x, hi_x) = (a.x.min(b.x), a.x.max(b.x));
    let (lo_y, hi_y) = (a.y.min(b.y), a.y.max(b.y));
    (a.x == b.x || a.y == b.y) && p.x >= lo_x && p.x <= hi_x && p.y >= lo_y && p.y <= hi_y
}

/// Rotation that makes a symbol pointing `default` point `want` instead.
fn rotation_towards(default: Dir, want: Dir) -> Orientation {
    [0u16, 90, 180, 270]
        .into_iter()
        .map(|deg| Orientation {
            rotation_deg: deg,
            ..Default::default()
        })
        .find(|o| default.transformed(*o) == want)
        .unwrap_or_default()
}

fn layout_chain(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    references: &BTreeMap<LogicalId, String>,
    taken: &mut BTreeSet<String>,
    below_y: Option<Nm>,
) -> LayoutOutput {
    let mut findings = Vec::new();

    // Nets and their kinds.
    let kinds: Vec<NetKind> = spec
        .nets
        .iter()
        .map(|n| match power_symbol(library, &n.name) {
            Some(_) if is_ground_name(&n.name) => NetKind::Ground,
            Some(_) => NetKind::Power,
            None => NetKind::Signal,
        })
        .collect();
    let mut net_of: BTreeMap<(LogicalId, String), usize> = BTreeMap::new();
    for (i, net) in spec.nets.iter().enumerate() {
        for p in &net.pins {
            net_of.insert((p.component_id.clone(), p.pin_number.clone()), i);
        }
    }
    let net_at = |id: &LogicalId, pin: &LibraryPin| net_of.get(&(id.clone(), pin.number.clone())).copied();
    let is_ground = |n: Option<usize>| n.is_some_and(|n| kinds[n] == NetKind::Ground);

    // Parts and roles.
    let mut parts: Vec<Part> = spec
        .components
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let symbol = &library[&c.library_id];
            let pins: Vec<&LibraryPin> = symbol.pins.iter().filter(|p| p.unit == 0 || p.unit == 1).collect();
            let grounded = pins.iter().filter(|p| is_ground(net_at(&c.id, p))).count();
            let role = if symbol.is_power {
                Role::Attached
            } else if pins.len() == 2
                && grounded == 1
                && !c.library_id.library().starts_with("Connector")
                && pins
                    .iter()
                    .any(|p| net_at(&c.id, p).is_some() && !is_ground(net_at(&c.id, p)))
            {
                Role::Shunt
            } else {
                Role::Main
            };
            Part {
                component: i,
                id: c.id.clone(),
                symbol,
                pins,
                role,
                column: 0,
                orientation: Orientation::default(),
                position: Point::default(),
            }
        })
        .collect();

    // Main path order: breadth-first over non-ground nets from a source.
    let mains: Vec<usize> = (0..parts.len()).filter(|&i| parts[i].role == Role::Main).collect();
    let nets_of = |i: usize, parts: &[Part]| -> BTreeSet<usize> {
        parts[i]
            .pins
            .iter()
            .filter_map(|p| net_at(&parts[i].id, p))
            .filter(|&n| kinds[n] != NetKind::Ground)
            .collect()
    };
    let mut column_of: BTreeMap<usize, usize> = BTreeMap::new();
    let mut next_column = 0;
    let source = mains
        .iter()
        .copied()
        .find(|&i| parts[i].symbol.library_id.library().starts_with("Connector"))
        .or_else(|| mains.first().copied());
    let mut seeds: Vec<usize> = source.into_iter().chain(mains.iter().copied()).collect();
    while let Some(seed) = seeds.first().copied() {
        seeds.remove(0);
        if column_of.contains_key(&seed) {
            continue;
        }
        let base = next_column;
        let mut queue = std::collections::VecDeque::from([(seed, base)]);
        column_of.insert(seed, base);
        while let Some((i, col)) = queue.pop_front() {
            next_column = next_column.max(col + 1);
            let my_nets = nets_of(i, &parts);
            for &j in &mains {
                if column_of.contains_key(&j) {
                    continue;
                }
                if !nets_of(j, &parts).is_disjoint(&my_nets) {
                    column_of.insert(j, col + 1);
                    queue.push_back((j, col + 1));
                }
            }
        }
    }
    for (&i, &c) in &column_of {
        parts[i].column = c;
    }
    let columns = next_column.max(1);
    let net_columns = |n: usize, parts: &[Part], except: usize| -> Vec<usize> {
        parts
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                *i != except && p.role == Role::Main && p.pins.iter().any(|pin| net_at(&p.id, pin) == Some(n))
            })
            .map(|(_, p)| p.column)
            .collect()
    };
    let has_hangers = |n: usize, parts: &[Part]| {
        parts
            .iter()
            .any(|p| p.role != Role::Main && p.pins.iter().any(|pin| net_at(&p.id, pin) == Some(n)))
    };

    // Orientation: score each rotation against the wanted pin directions.
    for i in 0..parts.len() {
        let part = &parts[i];
        let wants: Vec<(&LibraryPin, Dir)> = part
            .pins
            .iter()
            .filter_map(|&pin| {
                let n = net_at(&part.id, pin)?;
                let want = match (part.role, kinds[n]) {
                    (_, NetKind::Ground) => Dir::Down,
                    (Role::Shunt, _) => Dir::Up,
                    (Role::Attached, _) => return None,
                    (Role::Main, _) => {
                        let cols = net_columns(n, &parts, i);
                        if cols.iter().any(|&c| c < part.column) {
                            Dir::Left
                        } else if cols.iter().any(|&c| c > part.column) || has_hangers(n, &parts) {
                            Dir::Right
                        } else {
                            return None;
                        }
                    }
                };
                Some((pin, want))
            })
            .collect();
        let many_pins = part.pins.len() > 2;
        // Connectors and multi-pin parts may be mirrored: unlike a half turn, a mirror keeps the
        // order of pins along a side (pin 1 stays on top) and keeps text upright.
        let mirror_ok = many_pins || part.symbol.library_id.library().starts_with("Connector");
        let rot = |deg: u16| Orientation {
            rotation_deg: deg,
            ..Default::default()
        };
        let mut candidates: Vec<(Orientation, u32)> = vec![(rot(0), 0), (rot(90), 0), (rot(270), 0), (rot(180), 0)];
        if mirror_ok {
            candidates = vec![
                (rot(0), 0),
                (rot(90), 0),
                (rot(270), 0),
                (
                    Orientation {
                        mirror_y: true,
                        ..Default::default()
                    },
                    1,
                ),
                (
                    Orientation {
                        mirror_x: true,
                        ..Default::default()
                    },
                    1,
                ),
                (rot(180), 2),
            ];
        }
        let best = candidates
            .into_iter()
            .min_by_key(|(o, preference)| {
                // A misplaced ground pin costs less than a misplaced signal or power pin.
                let pin_cost: u32 = wants
                    .iter()
                    .map(|(pin, want)| {
                        let weight = if *want == Dir::Down && is_ground(net_at(&part.id, pin)) {
                            3
                        } else {
                            10
                        };
                        weight * want.distance(pin.outward.transformed(*o))
                    })
                    .sum();
                // Multi-pin symbols are drawn deliberately by the library author; change them only for a clear gain.
                let changed = o.rotation_deg != 0 || o.mirror_x || o.mirror_y;
                pin_cost + preference + if many_pins && changed { 15 } else { 0 }
            })
            .map(|(o, _)| o)
            .unwrap_or_default();
        parts[i].orientation = best;
    }

    // Shunt slots: the gap to the right of the leftmost main part on the shunt's net.
    let mut slots: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..parts.len() {
        if parts[i].role != Role::Shunt {
            continue;
        }
        let net = parts[i]
            .pins
            .iter()
            .filter_map(|p| net_at(&parts[i].id, p))
            .find(|&n| kinds[n] != NetKind::Ground);
        let slot = net
            .and_then(|n| net_columns(n, &parts, i).into_iter().min())
            .unwrap_or(columns - 1);
        slots.entry(slot).or_default().push(i);
    }

    // Placement.
    let baseline = match below_y {
        Some(y) => Nm((y.0 + 3 * ROW_PITCH.0).div_euclid(GRID.0) * GRID.0),
        None => START.y,
    };
    let snap_up = |v: Nm| Nm((v.0 + GRID.0 - 1).div_euclid(GRID.0) * GRID.0);
    let mut cursor = START.x;
    let mut taps: BTreeMap<usize, Nm> = BTreeMap::new();
    for col in 0..columns {
        let in_col: Vec<usize> = (0..parts.len())
            .filter(|&i| parts[i].role == Role::Main && parts[i].column == col)
            .collect();
        let mut right_edge = cursor;
        for (row, &i) in in_col.iter().enumerate() {
            let ext = parts[i].extent();
            // Anchor: the pin facing left (incoming), else facing right, onto the row's baseline.
            let anchor = parts[i]
                .pins
                .iter()
                .find(|p| {
                    parts[i].pin_dir(p) == Dir::Left
                        && net_at(&parts[i].id, p).is_some_and(|n| kinds[n] != NetKind::Ground)
                })
                .or_else(|| {
                    parts[i]
                        .pins
                        .iter()
                        .find(|p| parts[i].pin_dir(p) == Dir::Right && net_at(&parts[i].id, p).is_some())
                })
                .map(|p| parts[i].orientation.apply(p.local).unwrap_or(p.local))
                .unwrap_or_default();
            let y = baseline + Nm(row as i64 * ROW_PITCH.0) - anchor.y;
            let x = snap_up(cursor - ext.min.x);
            parts[i].position = Point::new(x, y).snap(GRID);
            right_edge = right_edge.max(parts[i].position.x + ext.max.x);
        }
        cursor = right_edge;
        let hanging = slots.get(&col).cloned().unwrap_or_default();
        if hanging.is_empty() {
            cursor = snap_up(cursor + MIN_GAP);
            continue;
        }
        let mut tap = snap_up(cursor + Nm(7_620_000));
        for &i in &hanging {
            let part = &parts[i];
            let top = part
                .pins
                .iter()
                .find(|p| !is_ground(net_at(&part.id, p)))
                .map(|p| part.orientation.apply(p.local).unwrap_or(p.local))
                .unwrap_or_default();
            parts[i].position = Point::new(tap - top.x, baseline + SHUNT_DROP - top.y).snap(GRID);
            taps.insert(i, tap);
            tap = tap + SHUNT_PITCH;
        }
        cursor = snap_up(tap - SHUNT_PITCH + MIN_GAP);
    }

    // Drawing.
    let bodies: Vec<(usize, Rect)> = parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.body().map(|b| (i, b)))
        .collect();
    let crosses_any = |a: Point, b: Point| bodies.iter().any(|(_, r)| r.crossed_by(a, b));
    // On the top and bottom of a multi-pin part, neighbouring pins are one grid apart: alternate
    // short and long stubs so their labels and power symbols do not collide.
    let mut stub_len: BTreeMap<(i64, i64), Nm> = BTreeMap::new();
    for part in parts.iter().filter(|p| p.role != Role::Attached && p.pins.len() > 2) {
        for side in [Dir::Up, Dir::Down] {
            let mut on_side: Vec<Point> = part
                .pins
                .iter()
                .filter(|pin| part.pin_dir(pin) == side)
                .map(|pin| part.pin_point(pin))
                .collect();
            on_side.sort_by_key(|p| p.x);
            for (k, p) in on_side.iter().enumerate() {
                if k % 2 == 1 {
                    stub_len.insert((p.x.0, p.y.0), Nm(3 * STUB.0));
                }
            }
        }
    }
    let has_flag = |n: usize| {
        parts
            .iter()
            .any(|p| p.role == Role::Attached && p.pins.iter().any(|pin| net_at(&p.id, pin) == Some(n)))
    };
    let mut drawing = Drawing::default();
    // Every placed pin with its net name; an unconnected pin gets a name of its own, since
    // touching it with any wire would connect it.
    let pin_points: Vec<(String, Point)> = parts
        .iter()
        .filter(|p| p.role != Role::Attached)
        .flat_map(|part| {
            part.pins.iter().map(move |pin| {
                let net = net_at(&part.id, pin)
                    .map(|n| spec.nets[n].name.clone())
                    .unwrap_or_else(|| format!("\u{0}unconnected {} {}", part.id, pin.number));
                (net, part.pin_point(pin))
            })
        })
        .collect();
    let mut fallback_nets = Vec::new();
    let mut flag_points: BTreeMap<usize, Vec<Point>> = BTreeMap::new();

    for (n, net) in spec.nets.iter().enumerate() {
        // Terminals on this net from main and shunt parts (attached parts are placed afterwards).
        let mut terminals: Vec<(Point, Dir, Role)> = Vec::new();
        for part in &parts {
            if part.role == Role::Attached {
                continue;
            }
            for pin in &part.pins {
                if net_at(&part.id, pin) == Some(n) {
                    terminals.push((part.pin_point(pin), part.pin_dir(pin), part.role));
                }
            }
        }
        for &(p, _, _) in &terminals {
            drawing.end(p);
        }

        if kinds[n] == NetKind::Ground {
            for &(p, dir, _) in &terminals {
                // Preferred shapes first; each must stay clear of other nets.
                let candidates: Vec<(Vec<Point>, Dir)> = match dir {
                    Dir::Down => vec![(vec![p], Dir::Down), (vec![p, p + Dir::Down.step(STUB)], Dir::Down)],
                    Dir::Left | Dir::Right => vec![
                        // A longer horizontal run keeps the ground symbol clear of the pin text.
                        (
                            vec![
                                p,
                                p + dir.step(Nm(2 * STUB.0)),
                                p + dir.step(Nm(2 * STUB.0)) + Dir::Down.step(STUB),
                            ],
                            Dir::Down,
                        ),
                        (
                            vec![p, p + dir.step(STUB), p + dir.step(STUB) + Dir::Down.step(STUB)],
                            Dir::Down,
                        ),
                        (vec![p, p + dir.step(STUB)], dir),
                        (vec![p], dir),
                    ],
                    Dir::Up => vec![(vec![p, p + Dir::Up.step(STUB)], Dir::Up), (vec![p], Dir::Up)],
                };
                let (path, facing) = candidates
                    .iter()
                    .find(|(path, facing)| {
                        // The symbol itself must also clear every part body.
                        let end = *path.last().expect("non-empty path");
                        drawing.path_is_free(&net.name, path, &pin_points)
                            && power_body(library, &net.name, end, *facing).is_none_or(|b| {
                                !parts
                                    .iter()
                                    .filter(|q| q.role != Role::Attached)
                                    .filter_map(|q| q.body())
                                    .any(|q| q.overlaps(b))
                            })
                    })
                    .cloned()
                    .unwrap_or_else(|| (vec![p], dir));
                drawing.path(&net.name, &path);
                let end = *path.last().expect("non-empty path");
                drawing.power.push((net.name.clone(), end, facing));
                drawing.end(end);
                flag_points.entry(n).or_default().push(end);
            }
            continue;
        }

        // Try a rail on the baseline.
        let rail = (|| {
            let mains: Vec<&(Point, Dir, Role)> = terminals.iter().filter(|t| t.2 == Role::Main).collect();
            if mains.is_empty() || mains.iter().any(|t| t.0.y != baseline || !t.1.is_horizontal()) {
                return None;
            }
            let shunts: Vec<&(Point, Dir, Role)> = terminals.iter().filter(|t| t.2 == Role::Shunt).collect();
            if shunts.iter().any(|t| t.0.y <= baseline || t.1 != Dir::Up) {
                return None;
            }
            let mut xs: BTreeSet<i64> = terminals.iter().map(|t| t.0.x.0).collect();
            if xs.len() < 2 {
                return None;
            }
            // Name the net in the middle of its longest gap, away from part text; a flag (if the
            // net has one) goes in the next-longest gap.
            let terminal_xs: Vec<i64> = xs.iter().copied().collect();
            let mut gaps: Vec<(i64, i64)> = terminal_xs.windows(2).map(|w| (w[0], w[1])).collect();
            gaps.sort_by_key(|(a, b)| (std::cmp::Reverse(b - a), *a));
            let mid = |(a, b): (i64, i64)| {
                let m = Nm((a + b) / 2).snap(GRID).0;
                if m == a || m == b {
                    None
                } else {
                    Some(m)
                }
            };
            // A label reads to the right of its anchor, so it starts near the left of the gap; a
            // power symbol stands up from the middle.
            let name_x = match kinds[n] {
                NetKind::Power => mid(gaps[0]).unwrap_or(gaps[0].0),
                _ => {
                    let (a, b) = gaps[0];
                    if b - a >= 4 * GRID.0 {
                        a + GRID.0
                    } else {
                        mid(gaps[0]).unwrap_or(a)
                    }
                }
            };
            xs.insert(name_x);
            let flag_x = if has_flag(n) {
                let x = gaps.get(1).and_then(|g| mid(*g)).unwrap_or_else(|| {
                    let (a, b) = gaps[0];
                    Nm(a + (b - a) * 3 / 4).snap(GRID).0
                });
                xs.insert(x);
                Some(x)
            } else {
                None
            };
            let mut segments = Vec::new();
            let pts: Vec<Point> = xs.iter().map(|&x| Point::new(Nm(x), baseline)).collect();
            for w in pts.windows(2) {
                segments.push((w[0], w[1]));
            }
            let drops: Vec<(Point, Point)> = shunts.iter().map(|t| (Point::new(t.0.x, baseline), t.0)).collect();
            if segments
                .iter()
                .chain(drops.iter())
                .any(|(a, b)| crosses_any(*a, *b) || drawing.touches_other_net(&net.name, *a, *b, &pin_points))
            {
                return None;
            }
            Some((
                segments,
                drops,
                Point::new(Nm(name_x), baseline),
                flag_x.map(|x| Point::new(Nm(x), baseline)),
            ))
        })();

        match rail {
            Some((segments, drops, name_at, flag_at)) => {
                for (a, b) in segments.iter().chain(drops.iter()) {
                    drawing.wire(&net.name, *a, *b);
                }
                match kinds[n] {
                    NetKind::Power => {
                        let top = name_at + Dir::Up.step(STUB);
                        drawing.wire(&net.name, name_at, top);
                        drawing.power.push((net.name.clone(), top, Dir::Up));
                        drawing.end(top);
                    }
                    _ => drawing.labels.push((net.name.clone(), name_at, Dir::Right)),
                }
                if let Some(flag_at) = flag_at {
                    let top = flag_at + Dir::Up.step(STUB);
                    drawing.wire(&net.name, flag_at, top);
                    flag_points.entry(n).or_default().push(top);
                }
            }
            None => {
                // A net with a single pin has nothing to wire to: a stub and a name is the drawing.
                if terminals.len() > 1 {
                    fallback_nets.push(net.name.clone());
                }
                for &(p, dir, _) in &terminals {
                    let mut len = stub_len.get(&(p.x.0, p.y.0)).copied().unwrap_or(STUB);
                    // A supply on a side pin stands upright at the end of a longer stub.
                    let upright = kinds[n] == NetKind::Power && dir.is_horizontal();
                    if upright {
                        len = Nm(2 * STUB.0);
                    }
                    let end = p + dir.step(len);
                    drawing.wire(&net.name, p, end);
                    match kinds[n] {
                        NetKind::Power => {
                            drawing
                                .power
                                .push((net.name.clone(), end, if upright { Dir::Up } else { dir }));
                            drawing.end(end);
                        }
                        _ => drawing.labels.push((net.name.clone(), end, dir)),
                    }
                    // A flag goes on the pin itself, clear of the name at the stub's end.
                    flag_points.entry(n).or_default().push(p);
                }
            }
        }
    }

    // Attached parts (e.g. PWR_FLAG): at a point of their net, facing the first direction whose
    // body stays clear of wires, parts, power symbols, and earlier flags.
    let mut occupied: Vec<Rect> = parts
        .iter()
        .filter(|p| p.role != Role::Attached)
        .map(|p| p.extent().translate(p.position))
        .collect();
    occupied.extend(
        drawing
            .power
            .iter()
            .filter_map(|(net, at, facing)| power_body(library, net, *at, *facing)),
    );
    for part in &mut parts {
        if part.role != Role::Attached {
            continue;
        }
        let Some(pin) = part.pins.first().copied() else {
            continue;
        };
        let Some(n) = net_at(&part.id, pin) else { continue };
        // Points kept for flags first, then every grid point along the net's wires.
        let mut candidates = flag_points.get(&n).cloned().unwrap_or_default();
        for (net, a, b) in &drawing.wires {
            if *net != spec.nets[n].name {
                continue;
            }
            let steps = (*b - *a).x.0.abs().max((*b - *a).y.0.abs()) / GRID.0;
            let unit = Point::new(
                Nm((b.x.0 - a.x.0).signum() * GRID.0),
                Nm((b.y.0 - a.y.0).signum() * GRID.0),
            );
            candidates.extend((1..steps).map(|k| Point::new(a.x + Nm(unit.x.0 * k), a.y + Nm(unit.y.0 * k))));
        }
        // Ideally the body keeps a grid of room (for its text) from parts, symbols, and other nets.
        // wires; its own net's wires only have to stay out of the body itself.
        let name = &spec.nets[n].name;
        let fits = |o: Orientation, at: Point, margin: Nm| {
            let origin = at - o.apply(pin.local).unwrap_or(pin.local);
            match part.symbol.body.map(|b| b.transformed(o).translate(origin)) {
                None => true,
                Some(body) => {
                    let room = body.inflate(margin);
                    !occupied.iter().any(|r| r.overlaps(room))
                        && !drawing
                            .wires
                            .iter()
                            .any(|(net, a, b)| on_segment_inside(if net == name { body } else { room }, *a, *b))
                }
            }
        };
        let choice = [GRID, Nm(0)].into_iter().find_map(|margin| {
            candidates.iter().find_map(|&at| {
                [Dir::Up, Dir::Right, Dir::Left, Dir::Down]
                    .into_iter()
                    .map(|facing| rotation_towards(pin.outward.opposite(), facing))
                    .find(|o| fits(*o, at, margin))
                    .map(|o| (at, o))
            })
        });
        let Some((at, o)) = choice.or_else(|| {
            candidates
                .first()
                .map(|&at| (at, rotation_towards(pin.outward.opposite(), Dir::Up)))
        }) else {
            continue;
        };
        part.orientation = o;
        part.position = at - o.apply(pin.local).unwrap_or(pin.local);
        if let Some(body) = part.body() {
            occupied.push(body);
        }
        drawing.end(at);
    }

    // Operations: parts, power symbols, wires, labels, junctions.
    let mut operations = Vec::new();
    for part in &parts {
        let c = &spec.components[part.component];
        operations.push(Operation::PlaceSymbol {
            component_id: c.id.clone(),
            library_id: c.library_id.clone(),
            reference: references[&c.id].clone(),
            value: c.value.clone(),
            footprint: c.footprint.clone(),
            position: part.position,
            orientation: part.orientation,
        });
    }
    for (net, at, facing) in &drawing.power {
        let Some(symbol) = power_symbol(library, net) else {
            continue;
        };
        let pin = &symbol.pins[0];
        // Supplies point away from their pin (up by default); ground symbols too (down by default).
        let orientation = rotation_towards(pin.outward.opposite(), *facing);
        let reference = (1..)
            .map(|k| format!("{}{k:02}", symbol.reference_prefix))
            .find(|r| !taken.contains(r))
            .expect("unbounded");
        taken.insert(reference.clone());
        operations.push(Operation::PlacePowerSymbol {
            net: net.clone(),
            library_id: symbol.library_id.clone(),
            reference,
            position: *at - orientation.apply(pin.local).unwrap_or(pin.local),
            orientation,
        });
    }
    for (net, a, b) in &drawing.wires {
        operations.push(Operation::AddWire {
            net: net.clone(),
            start: *a,
            end: *b,
        });
    }
    for (net, at, dir) in &drawing.labels {
        operations.push(Operation::AddLabel {
            net: net.clone(),
            position: *at,
            direction: *dir,
        });
    }
    for (&(x, y), &count) in &drawing.ends {
        if count >= 3 {
            operations.push(Operation::AddJunction {
                position: Point::new(Nm(x), Nm(y)),
            });
        }
    }

    for name in &fallback_nets {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "NET_DRAWN_WITH_LABELS",
            message: format!("net '{name}' could not be wired cleanly and is joined by labels"),
            objects: vec![name.clone()],
        });
    }
    LayoutOutput {
        operations,
        findings,
        fallback_nets,
    }
}

/// A part with at least this many pins on non-ground nets is laid out as a hub (an IC with
/// parts around it) rather than as a link in a chain.
const HUB_MIN_PINS: usize = 4;
/// Space between blocks of a hub layout.
const BLOCK_GAP: Nm = Nm(7_620_000);
/// Blocks to the right of a hub wrap to a new row beyond this width.
const BLOCK_ROW_WIDTH: Nm = Nm(127_000_000);

/// Lays out `spec`: as one chain (power paths, filters, LED drivers) or, when a part has
/// [`HUB_MIN_PINS`] or more connected pins, as a hub. In a hub layout the IC keeps its library
/// drawing with a short stub and a name (label, power symbol, or ground) on every pin, and the
/// other parts form blocks: parts joined by nets that do not touch the IC. Each block is drawn as
/// a chain and connects to the IC through same-name labels and power symbols. Connector blocks go
/// to the left of the IC, the rest to its right.
pub fn layout(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    references: &BTreeMap<LogicalId, String>,
    taken: &mut BTreeSet<String>,
    below_y: Option<Nm>,
) -> LayoutOutput {
    let ground: Vec<bool> = spec
        .nets
        .iter()
        .map(|n| power_symbol(library, &n.name).is_some() && is_ground_name(&n.name))
        .collect();
    let mut nets_of: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); spec.components.len()];
    for (n, net) in spec.nets.iter().enumerate() {
        for p in &net.pins {
            if let Some(i) = spec.components.iter().position(|c| c.id == p.component_id) {
                nets_of[i].insert(n);
            }
        }
    }
    let is_power_part = |i: usize| library.get(&spec.components[i].library_id).is_some_and(|s| s.is_power);
    let connected_pins = |i: usize| {
        spec.nets
            .iter()
            .enumerate()
            .filter(|(n, _)| !ground[*n])
            .map(|(_, net)| {
                net.pins
                    .iter()
                    .filter(|p| p.component_id == spec.components[i].id)
                    .count()
            })
            .sum::<usize>()
    };
    let hub = (0..spec.components.len())
        .filter(|&i| !is_power_part(i) && connected_pins(i) >= HUB_MIN_PINS)
        .max_by_key(|&i| (connected_pins(i), std::cmp::Reverse(i)));
    let Some(hub) = hub else {
        return layout_chain(spec, library, references, taken, below_y);
    };

    // Blocks: parts joined by nets that neither touch the hub nor are ground.
    let hub_nets = nets_of[hub].clone();
    let others: Vec<usize> = (0..spec.components.len())
        .filter(|&i| i != hub && !is_power_part(i))
        .collect();
    let mut block_of: BTreeMap<usize, usize> = BTreeMap::new();
    let mut blocks: Vec<Vec<usize>> = Vec::new();
    for &start in &others {
        if block_of.contains_key(&start) {
            continue;
        }
        let id = blocks.len();
        let mut members = vec![start];
        block_of.insert(start, id);
        let mut k = 0;
        while k < members.len() {
            let i = members[k];
            for &j in &others {
                if block_of.contains_key(&j) {
                    continue;
                }
                let private = nets_of[i]
                    .intersection(&nets_of[j])
                    .any(|&n| !ground[n] && !hub_nets.contains(&n));
                if private {
                    block_of.insert(j, id);
                    members.push(j);
                }
            }
            k += 1;
        }
        members.sort_unstable();
        blocks.push(members);
    }
    // Power-library parts (flags) join a block that shares their net, preferring a connector.
    let is_connector = |i: usize| spec.components[i].library_id.library().starts_with("Connector");
    let mut hub_block = vec![hub];
    for i in (0..spec.components.len()).filter(|&i| is_power_part(i)) {
        let home = others
            .iter()
            .copied()
            .filter(|&j| !nets_of[i].is_disjoint(&nets_of[j]))
            .min_by_key(|&j| (!is_connector(j), j));
        match home.and_then(|j| block_of.get(&j)) {
            Some(&b) => blocks[b].push(i),
            None => hub_block.push(i),
        }
    }

    // Lay out each block on its own, then arrange the blocks around the hub.
    let sub_spec = |members: &[usize]| {
        let ids: BTreeSet<&LogicalId> = members.iter().map(|&i| &spec.components[i].id).collect();
        CircuitSpec {
            schema_version: spec.schema_version,
            name: spec.name.clone(),
            description: String::new(),
            components: members.iter().map(|&i| spec.components[i].clone()).collect(),
            nets: spec
                .nets
                .iter()
                .map(|net| crate::circuit::NetSpec {
                    name: net.name.clone(),
                    pins: net
                        .pins
                        .iter()
                        .filter(|p| ids.contains(&p.component_id))
                        .cloned()
                        .collect(),
                })
                .filter(|net| !net.pins.is_empty())
                .collect(),
            notes: vec![],
        }
    };
    let mut out = LayoutOutput {
        operations: vec![],
        findings: vec![],
        fallback_nets: vec![],
    };
    let mut place_block = |members: &[usize], at: Point, taken: &mut BTreeSet<String>, is_hub: bool| -> Rect {
        let block = layout_chain(&sub_spec(members), library, references, taken, None);
        let bounds = operations_bounds(&block.operations, library).unwrap_or(Rect { min: at, max: at });
        let shift = Point::new(at.x - bounds.min.x, at.y - bounds.min.y).snap(GRID);
        out.operations
            .extend(block.operations.into_iter().map(|op| translate(op, shift)));
        // The hub names its own pins by design; only other blocks report nets drawn with labels.
        if !is_hub {
            out.findings.extend(block.findings);
            out.fallback_nets.extend(block.fallback_nets);
        }
        bounds.translate(shift)
    };

    let origin_y = match below_y {
        Some(y) => Nm((y.0 + 3 * ROW_PITCH.0).div_euclid(GRID.0) * GRID.0),
        None => START.y,
    };
    let (left, right): (Vec<&Vec<usize>>, Vec<&Vec<usize>>) =
        blocks.iter().partition(|b| b.iter().any(|&i| is_connector(i)));
    let mut y = origin_y;
    let mut left_edge = START.x;
    for b in &left {
        let r = place_block(b, Point::new(START.x, y), taken, false);
        left_edge = left_edge.max(r.max.x);
        y = r.max.y + BLOCK_GAP;
    }
    let hub_x = if left.is_empty() {
        START.x
    } else {
        left_edge + BLOCK_GAP + BLOCK_GAP
    };
    let hub_rect = place_block(&hub_block, Point::new(hub_x, origin_y), taken, true);
    let row_start = hub_rect.max.x + BLOCK_GAP + BLOCK_GAP;
    let (mut x, mut y, mut row_height) = (row_start, origin_y, Nm(0));
    for b in &right {
        // Measure the block first to decide whether it still fits on this row.
        let probe = layout_chain(&sub_spec(b), library, references, &mut taken.clone(), None);
        let size = operations_bounds(&probe.operations, library)
            .map(|r| r.width())
            .unwrap_or(Nm(0));
        if x > row_start && x + size - row_start > BLOCK_ROW_WIDTH {
            x = row_start;
            y = y + row_height + BLOCK_GAP;
            row_height = Nm(0);
        }
        let r = place_block(b, Point::new(x, y), taken, false);
        x = r.max.x + BLOCK_GAP;
        row_height = row_height.max(r.height());
    }
    out
}

/// Moves an operation by `by`.
pub(crate) fn translate(op: Operation, by: Point) -> Operation {
    match op {
        Operation::PlaceSymbol {
            component_id,
            library_id,
            reference,
            value,
            footprint,
            position,
            orientation,
        } => Operation::PlaceSymbol {
            component_id,
            library_id,
            reference,
            value,
            footprint,
            position: position + by,
            orientation,
        },
        Operation::PlacePowerSymbol {
            net,
            library_id,
            reference,
            position,
            orientation,
        } => Operation::PlacePowerSymbol {
            net,
            library_id,
            reference,
            position: position + by,
            orientation,
        },
        Operation::AddWire { net, start, end } => Operation::AddWire {
            net,
            start: start + by,
            end: end + by,
        },
        Operation::AddLabel {
            net,
            position,
            direction,
        } => Operation::AddLabel {
            net,
            position: position + by,
            direction,
        },
        Operation::AddJunction { position } => Operation::AddJunction {
            position: position + by,
        },
    }
}

/// The area a set of operations draws on, including room for part text and label text.
pub fn operations_bounds(operations: &[Operation], library: &BTreeMap<LibraryId, LibrarySymbol>) -> Option<Rect> {
    let text_room = Nm(3_810_000);
    let char_width = 1_270_000;
    let symbol_rect = |id: &LibraryId, at: Point, o: Orientation| {
        let s = library.get(id)?;
        let local = s
            .body
            .into_iter()
            .chain(Rect::from_points(s.pins.iter().map(|p| p.local)))
            .reduce(Rect::union)?;
        Some(local.transformed(o).translate(at).inflate(text_room))
    };
    operations
        .iter()
        .filter_map(|op| match op {
            Operation::PlaceSymbol {
                library_id,
                position,
                orientation,
                ..
            }
            | Operation::PlacePowerSymbol {
                library_id,
                position,
                orientation,
                ..
            } => symbol_rect(library_id, *position, *orientation),
            Operation::AddWire { start, end, .. } => Rect::from_points([*start, *end]),
            Operation::AddJunction { position } => Rect::from_points([*position]),
            Operation::AddLabel {
                net,
                position,
                direction,
            } => {
                let length = Nm(net.chars().count() as i64 * char_width + 2 * GRID.0);
                let far = *position + direction.step(length);
                Rect::from_points([*position, far]).map(|r| r.inflate(GRID))
            }
        })
        .reduce(Rect::union)
}

/// Does the segment a–b run inside `r` (crossing or lying on an interior line)?
fn on_segment_inside(r: Rect, a: Point, b: Point) -> bool {
    let seg = Rect::from_points([a, b]).expect("two points");
    seg.min.x < r.max.x && seg.max.x > r.min.x && seg.min.y < r.max.y && seg.max.y > r.min.y
        || (a == b && a.x > r.min.x && a.x < r.max.x && a.y > r.min.y && a.y < r.max.y)
}

/// The body of the power symbol for `net` with its pin at `at`, facing `facing`.
fn power_body(library: &BTreeMap<LibraryId, LibrarySymbol>, net: &str, at: Point, facing: Dir) -> Option<Rect> {
    let symbol = power_symbol(library, net)?;
    let pin = symbol.pins.first()?;
    let o = rotation_towards(pin.outward.opposite(), facing);
    Some(
        symbol
            .body?
            .transformed(o)
            .translate(at - o.apply(pin.local).unwrap_or(pin.local)),
    )
}
