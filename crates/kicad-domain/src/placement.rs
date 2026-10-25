// SPDX-License-Identifier: GPL-3.0-or-later
//! Placing footprints on a board (Phase 12.3): a constructive pass, then a deterministic
//! improvement loop, both driven by one cost made of measurable rules (decision D5).
//!
//! - Fixed: locked footprints, mounting holes, and parts the hints keep or place; they are obstacles.
//! - Constructive: connectors go to an edge (the one asked for, else the nearest), the IC with the
//!   most pads to the middle, other ICs and the remaining parts near the parts they connect to,
//!   decoupling capacitors beside the IC pin on their supply. Each part takes the cheapest free
//!   place found in rings around its target, in four turns.
//! - Improvement: every movable part tries shifts on the grid, quarter turns, and swaps with a
//!   part of similar size; a change is kept when it lowers the cost. Passes stop when nothing
//!   improves. No randomness: the same board and hints give the same plan.
//!
//! The cost: estimated wire length (half-perimeter of each net's pads; ground and power nets
//! weigh less, since pours and wide tracks carry them), courtyard overlaps and parts outside the
//! outline (heavy), decoupling capacitors far from their pin, connectors far from their edge, and
//! the `near` and `group` hints. The result is a move plan, applied and read back like any other.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::board::{is_ground_net, is_power_net, place_point, place_rect, Board};
use crate::board_edit::{
    grid_nm, measure, moved, normalize_deg, snap, unplace_point, BoardEditPlan, BoardOp, Placement,
};
use crate::geometry::{Nm, Point, Rect};
use crate::result::{ErrorCode, ToolError};
use crate::schematic::{SchematicDesign, ShapeKind};

const WIRE_SIGNAL: f64 = 1.0;
const WIRE_POWER: f64 = 0.5;
const WIRE_GROUND: f64 = 0.15;
/// Per mm² of courtyard overlap, plus a fixed amount per overlapping pair, so that any overlap
/// costs more than any wire.
const OVERLAP_AREA: f64 = 200.0;
const OVERLAP_PAIR: f64 = 500.0;
const OUTSIDE_AREA: f64 = 200.0;
const OUTSIDE_PART: f64 = 500.0;
/// Per mm a decoupling capacitor's supply pad is from its IC pin, beyond the first mm.
const DECOUPLING: f64 = 4.0;
/// Per mm a connector's courtyard is from its edge.
const CONNECTOR_EDGE: f64 = 6.0;
const NEAR: f64 = 5.0;
const GROUP: f64 = 1.5;
/// Per mm the main IC is from the middle of the board.
const MAIN_IC_CENTRE: f64 = 1.0;
/// Rings searched after the first ring that has a free place.
const EXTRA_RINGS: i64 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn parse(s: &str) -> Option<Edge> {
        match s {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            "top" => Some(Edge::Top),
            "bottom" => Some(Edge::Bottom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinnedPlace {
    pub reference: String,
    pub at: Point,
    /// Keeps its turn when none is given.
    pub rotation_deg: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NearHint {
    pub reference: String,
    pub target: String,
    /// Distance between the two anchors that costs nothing.
    pub max_mm: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaceHints {
    /// Not moved.
    pub keep: Vec<String>,
    /// Put exactly here, then not moved.
    pub place: Vec<PinnedPlace>,
    pub near: Vec<NearHint>,
    /// A connector (or any part) on this edge.
    pub edge: Vec<(String, Edge)>,
    /// Parts kept together.
    pub groups: Vec<Vec<String>>,
    /// `true` for the back side.
    pub side: Vec<(String, bool)>,
    /// Place only these; every other footprint stays where it is.
    pub only: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceOptions {
    pub grid_mm: f64,
    /// Space between a courtyard and the outline (connectors may reach the outline).
    pub edge_clearance_mm: f64,
    /// Space kept between courtyards, for reference text and routing.
    pub spacing_mm: f64,
    pub max_passes: usize,
}

impl Default for PlaceOptions {
    fn default() -> Self {
        PlaceOptions {
            grid_mm: crate::board_edit::DEFAULT_GRID_MM,
            edge_clearance_mm: 0.25,
            spacing_mm: 1.0,
            max_passes: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecouplingDistance {
    pub capacitor: String,
    /// `U1.2`: the nearest IC pin on the capacitor's supply.
    pub pin: String,
    pub distance_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConnectorEdge {
    pub reference: String,
    /// From its courtyard to the nearest board edge.
    pub edge_mm: f64,
}

/// The measured qualities of a placement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlacementQuality {
    /// The sum over nets of the half-perimeter of their pads' box: a lower bound on wire length.
    pub wire_length_mm: f64,
    pub overlapping_pairs: Vec<String>,
    pub outside_outline: Vec<String>,
    pub decoupling: Vec<DecouplingDistance>,
    pub connectors: Vec<ConnectorEdge>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlacementReport {
    pub before: PlacementQuality,
    pub after: PlacementQuality,
    /// The IC placed in the middle.
    pub main_ic: Option<String>,
    /// Footprints that stayed where they were (locked, holes, kept, placed by a hint, not in `only`).
    pub fixed: Vec<String>,
    pub moved: Vec<String>,
    pub groups: Vec<Vec<String>>,
    pub passes: usize,
}

fn mm(n: i64) -> f64 {
    n as f64 / 1e6
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn dist_mm(a: Point, b: Point) -> f64 {
    mm(((a.x.0 - b.x.0) as f64).hypot((a.y.0 - b.y.0) as f64).round() as i64)
}

fn is_connector(reference: &str) -> bool {
    reference.starts_with('J') || reference.starts_with("USB") || reference.starts_with("CN")
}

fn is_ic(reference: &str) -> bool {
    reference.starts_with('U') || reference.starts_with("IC")
}

fn net_weight(name: &str) -> f64 {
    if is_ground_net(name) {
        WIRE_GROUND
    } else if is_power_net(name) {
        WIRE_POWER
    } else {
        WIRE_SIGNAL
    }
}

fn overlap_area_mm2(a: Rect, b: Rect) -> f64 {
    let w = a.max.x.0.min(b.max.x.0) - a.min.x.0.max(b.min.x.0);
    let h = a.max.y.0.min(b.max.y.0) - a.min.y.0.max(b.min.y.0);
    if w <= 0 || h <= 0 {
        0.0
    } else {
        mm(w) * mm(h)
    }
}

fn area_mm2(r: Rect) -> f64 {
    mm(r.width().0) * mm(r.height().0)
}

fn centre(r: Rect) -> Point {
    Point::new(Nm((r.min.x.0 + r.max.x.0) / 2), Nm((r.min.y.0 + r.max.y.0) / 2))
}

/// How far a courtyard is from one edge of the outline (zero when it touches or crosses it).
fn edge_gap_mm(court: Rect, outline: Rect, edge: Edge) -> f64 {
    let gap = match edge {
        Edge::Left => court.min.x.0 - outline.min.x.0,
        Edge::Right => outline.max.x.0 - court.max.x.0,
        Edge::Top => court.min.y.0 - outline.min.y.0,
        Edge::Bottom => outline.max.y.0 - court.max.y.0,
    };
    mm(gap.max(0))
}

fn nearest_edge(court: Rect, outline: Rect) -> Edge {
    [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
        .into_iter()
        .min_by(|a, b| {
            let da = signed_gap(court, outline, *a);
            let db = signed_gap(court, outline, *b);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("four edges")
}

fn signed_gap(court: Rect, outline: Rect, edge: Edge) -> f64 {
    let c = centre(court);
    mm(match edge {
        Edge::Left => c.x.0 - outline.min.x.0,
        Edge::Right => outline.max.x.0 - c.x.0,
        Edge::Top => c.y.0 - outline.min.y.0,
        Edge::Bottom => outline.max.y.0 - c.y.0,
    })
}

/// Groups of parts from the schematic: the parts inside each rectangle drawn on the sheet (for
/// example a titled `box_around` from `sch_draw_plan`), when a rectangle holds two or more.
pub fn schematic_blocks(design: &SchematicDesign) -> Vec<Vec<String>> {
    let mut blocks: BTreeSet<Vec<String>> = BTreeSet::new();
    for shape in &design.shapes {
        let ShapeKind::Rectangle { min, max } = shape.kind else {
            continue;
        };
        let mut inside: Vec<String> = design
            .symbols
            .iter()
            .filter(|s| !s.reference.starts_with('#') && !s.exclude_from_board)
            .filter(|s| s.position.x > min.x && s.position.x < max.x && s.position.y > min.y && s.position.y < max.y)
            .map(|s| s.reference.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        inside.sort_by_key(|r| crate::bom::natural(r));
        if inside.len() >= 2 {
            blocks.insert(inside);
        }
    }
    blocks.into_iter().collect()
}

/// Measures a placement: wire length, overlaps, parts outside, decoupling distances, and how far
/// connectors are from the edge.
pub fn quality(board: &Board) -> PlacementQuality {
    let m = measure(board);
    let mut nets: BTreeMap<&str, Vec<Point>> = BTreeMap::new();
    for f in &board.footprints {
        for p in f.pads.iter().filter(|p| !p.net.is_empty()) {
            nets.entry(p.net.as_str()).or_default().push(p.at);
        }
    }
    let wire: f64 = nets
        .values()
        .filter(|pads| pads.len() >= 2)
        .filter_map(|pads| Rect::from_points(pads.iter().copied()))
        .map(|r| mm(r.width().0) + mm(r.height().0))
        .sum();
    let mut decoupling = Vec::new();
    for cap in board.footprints.iter().filter(|f| f.reference.starts_with('C')) {
        let power = cap.pads.iter().find(|p| is_power_net(&p.net));
        let grounded = cap.pads.iter().any(|p| is_ground_net(&p.net));
        let (Some(power), true) = (power, grounded) else {
            continue;
        };
        let nearest = board
            .footprints
            .iter()
            .filter(|f| is_ic(&f.reference))
            .flat_map(|u| u.pads.iter().map(move |p| (u, p)))
            .filter(|(_, p)| p.net == power.net)
            .map(|(u, p)| (format!("{}.{}", u.reference, p.number), dist_mm(p.at, power.at)))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((pin, d)) = nearest {
            decoupling.push(DecouplingDistance {
                capacitor: cap.reference.clone(),
                pin,
                distance_mm: round2(d),
            });
        }
    }
    let connectors = match board.outline {
        Some(o) => board
            .footprints
            .iter()
            .filter(|f| is_connector(&f.reference))
            .filter_map(|f| f.extent().map(|c| (f, c)))
            .map(|(f, c)| ConnectorEdge {
                reference: f.reference.clone(),
                edge_mm: round2(edge_gap_mm(c, o, nearest_edge(c, o))),
            })
            .collect(),
        None => vec![],
    };
    PlacementQuality {
        wire_length_mm: round2(wire),
        overlapping_pairs: m.overlapping_pairs,
        outside_outline: m.outside_outline,
        decoupling,
        connectors,
    }
}

// ---------------------------------------------------------------- the engine

#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// On the edge a hint names, else on the nearest one.
    Connector(Option<Edge>),
    Ic,
    Other,
}

#[derive(Debug, Clone)]
struct Part {
    index: usize,
    reference: String,
    movable: bool,
    back: bool,
    through: bool,
    role: Role,
    /// (net, place in the footprint's own coordinates on its planned side)
    pads: Vec<(usize, Point)>,
    court: Rect,
    at: Point,
    rot: f64,
    placed: bool,
}

impl Part {
    fn court_abs(&self) -> Rect {
        place_rect(self.at, self.rot, self.court)
    }

    fn pad_abs(&self, local: Point) -> Point {
        place_point(self.at, self.rot, local)
    }

    fn conflicts(&self, other: &Part) -> bool {
        self.back == other.back || self.through || other.through
    }
}

struct Decoupling {
    cap: usize,
    cap_pad: Point,
    ic: usize,
    ic_pad: Point,
}

struct Engine {
    parts: Vec<Part>,
    /// Net weight and its members (part, local pad).
    nets: Vec<(f64, Vec<(usize, Point)>)>,
    part_nets: Vec<Vec<usize>>,
    decoupling: Vec<Decoupling>,
    near: Vec<(usize, usize, f64)>,
    groups: Vec<Vec<usize>>,
    /// The IC with the most pads, which belongs in the middle.
    main_ic: Option<usize>,
    /// Half the spacing between courtyards: each courtyard grows by it for the overlap test.
    half_spacing: Nm,
    outline: Rect,
    inner: Rect,
}

impl Engine {
    fn net_cost(&self, net: usize) -> f64 {
        let (weight, members) = &self.nets[net];
        let pts = members
            .iter()
            .filter(|(p, _)| self.parts[*p].placed)
            .map(|(p, local)| self.parts[*p].pad_abs(*local));
        match Rect::from_points(pts) {
            Some(r) => weight * (mm(r.width().0) + mm(r.height().0)),
            None => 0.0,
        }
    }

    fn outside_cost(&self, i: usize) -> f64 {
        let p = &self.parts[i];
        let c = p.court_abs();
        let bound = if matches!(p.role, Role::Connector(_)) {
            self.outline
        } else {
            self.inner
        };
        let out = area_mm2(c) - overlap_area_mm2(c, bound);
        if out > 1e-9 {
            OUTSIDE_PART + OUTSIDE_AREA * out
        } else {
            0.0
        }
    }

    fn overlap_cost(&self, i: usize, j: usize) -> f64 {
        let (a, b) = (&self.parts[i], &self.parts[j]);
        if !b.placed || !a.conflicts(b) {
            return 0.0;
        }
        let area = overlap_area_mm2(
            a.court_abs().inflate(self.half_spacing),
            b.court_abs().inflate(self.half_spacing),
        );
        if area > 1e-9 {
            OVERLAP_PAIR + OVERLAP_AREA * area
        } else {
            0.0
        }
    }

    fn group_cost(&self, g: usize) -> f64 {
        let members: Vec<&Part> = self.groups[g]
            .iter()
            .map(|i| &self.parts[*i])
            .filter(|p| p.placed)
            .collect();
        if members.len() < 2 {
            return 0.0;
        }
        let n = members.len() as i64;
        let c = Point::new(
            Nm(members.iter().map(|p| p.at.x.0).sum::<i64>() / n),
            Nm(members.iter().map(|p| p.at.y.0).sum::<i64>() / n),
        );
        GROUP * members.iter().map(|p| dist_mm(p.at, c)).sum::<f64>()
    }

    /// Every term of the cost that involves part `i`.
    fn part_cost(&self, i: usize) -> f64 {
        let p = &self.parts[i];
        let mut cost: f64 = self.part_nets[i].iter().map(|n| self.net_cost(*n)).sum();
        for j in 0..self.parts.len() {
            if j != i {
                cost += self.overlap_cost(i, j);
            }
        }
        cost += self.outside_cost(i);
        if let Role::Connector(edge) = p.role {
            let c = p.court_abs();
            let edge = edge.unwrap_or_else(|| nearest_edge(c, self.outline));
            cost += CONNECTOR_EDGE * edge_gap_mm(c, self.outline, edge);
        }
        for d in self.decoupling.iter().filter(|d| d.cap == i || d.ic == i) {
            let (cap, ic) = (&self.parts[d.cap], &self.parts[d.ic]);
            if cap.placed && ic.placed {
                let gap = dist_mm(cap.pad_abs(d.cap_pad), ic.pad_abs(d.ic_pad));
                cost += DECOUPLING * (gap - 1.0).max(0.0);
            }
        }
        for (a, b, max) in self.near.iter().filter(|(a, b, _)| *a == i || *b == i) {
            let (pa, pb) = (&self.parts[*a], &self.parts[*b]);
            if pa.placed && pb.placed {
                cost += NEAR * (dist_mm(pa.at, pb.at) - max).max(0.0);
            }
        }
        for g in 0..self.groups.len() {
            if self.groups[g].contains(&i) {
                cost += self.group_cost(g);
            }
        }
        if self.main_ic == Some(i) {
            cost += MAIN_IC_CENTRE * dist_mm(p.at, centre(self.outline));
        }
        cost
    }

    fn feasible(&self, i: usize) -> bool {
        self.outside_cost(i) == 0.0 && (0..self.parts.len()).all(|j| j == i || self.overlap_cost(i, j) == 0.0)
    }

    /// Where the constructive pass looks first for part `i`.
    fn target(&self, i: usize, centre_of_board: Point) -> Point {
        let p = &self.parts[i];
        if let Some(d) = self.decoupling.iter().find(|d| d.cap == i && self.parts[d.ic].placed) {
            return self.parts[d.ic].pad_abs(d.ic_pad);
        }
        // The pads this part shares a net with, among placed parts; ground only if nothing else.
        let mut pts = Vec::new();
        let mut ground = Vec::new();
        for n in &self.part_nets[i] {
            let (w, members) = &self.nets[*n];
            for (q, local) in members.iter().filter(|(q, _)| *q != i && self.parts[*q].placed) {
                let at = self.parts[*q].pad_abs(*local);
                if *w == WIRE_GROUND {
                    ground.push(at);
                } else {
                    pts.push(at);
                }
            }
        }
        if pts.is_empty() {
            pts = ground;
        }
        let near = if pts.is_empty() {
            centre_of_board
        } else {
            let n = pts.len() as i64;
            Point::new(
                Nm(pts.iter().map(|p| p.x.0).sum::<i64>() / n),
                Nm(pts.iter().map(|p| p.y.0).sum::<i64>() / n),
            )
        };
        match p.role {
            Role::Connector(edge) => {
                // Unless a hint names one, the edge nearest what the connector connects to.
                let edge = edge.unwrap_or_else(|| {
                    let probe = Rect { min: near, max: near };
                    nearest_edge(probe, self.outline)
                });
                let c = p.court;
                let (half_w, half_h) = ((c.width().0) / 2, (c.height().0) / 2);
                let o = self.outline;
                match edge {
                    Edge::Left => Point::new(o.min.x + Nm(half_w), near.y),
                    Edge::Right => Point::new(o.max.x - Nm(half_w), near.y),
                    Edge::Top => Point::new(near.x, o.min.y + Nm(half_h)),
                    Edge::Bottom => Point::new(near.x, o.max.y - Nm(half_h)),
                }
            }
            Role::Ic if pts.is_empty() || self.main_ic == Some(i) => centre_of_board,
            _ => near,
        }
    }

    fn rotations(&self, i: usize) -> [f64; 4] {
        let r = self.parts[i].rot;
        [
            r,
            normalize_deg(r + 90.0),
            normalize_deg(r + 180.0),
            normalize_deg(r + 270.0),
        ]
    }

    /// Puts part `i` at the cheapest place found in rings around `target`.
    fn place_near(&mut self, i: usize, target: Point, step: Nm, grid: Nm) {
        let start = snap(target, grid);
        let max_ring = ((mm(self.outline.width().0).max(mm(self.outline.height().0)) / mm(step.0)).ceil() as i64) + 2;
        let rotations = self.rotations(i);
        self.parts[i].placed = true;
        let mut best: Option<(f64, Point, f64)> = None;
        let mut stop_at: Option<i64> = None;
        for ring in 0..=max_ring {
            if stop_at.is_some_and(|s| ring > s) {
                break;
            }
            for (dx, dy) in ring_offsets(ring) {
                let at = Point::new(start.x + Nm(dx * step.0), start.y + Nm(dy * step.0));
                for rot in rotations {
                    self.parts[i].at = at;
                    self.parts[i].rot = rot;
                    let cost = self.part_cost(i);
                    if best.is_none_or(|(b, _, _)| cost < b - 1e-9) {
                        best = Some((cost, at, rot));
                    }
                    if stop_at.is_none() && self.feasible(i) {
                        stop_at = Some(ring + EXTRA_RINGS);
                    }
                }
            }
        }
        let (_, at, rot) = best.expect("at least the target was tried");
        self.parts[i].at = at;
        self.parts[i].rot = rot;
    }

    /// One improvement pass; returns whether anything changed.
    fn improve(&mut self, order: &[usize], step: Nm) -> bool {
        let mut changed = false;
        for &i in order {
            let before = self.part_cost(i);
            let (at0, rot0) = (self.parts[i].at, self.parts[i].rot);
            let mut best: Option<(f64, Point, f64)> = None;
            let mut tries: Vec<(Point, f64)> = Vec::new();
            for k in [1, 2, 4, 8] {
                for (dx, dy) in ring_offsets(1) {
                    tries.push((
                        Point::new(at0.x + Nm(dx * k * step.0), at0.y + Nm(dy * k * step.0)),
                        rot0,
                    ));
                }
            }
            for rot in &self.rotations(i)[1..] {
                tries.push((at0, *rot));
            }
            for (at, rot) in tries {
                self.parts[i].at = at;
                self.parts[i].rot = rot;
                let delta = self.part_cost(i) - before;
                if delta < -1e-6 && best.is_none_or(|(b, _, _)| delta < b - 1e-9) {
                    best = Some((delta, at, rot));
                }
            }
            self.parts[i].at = at0;
            self.parts[i].rot = rot0;
            // Swaps with movable parts of a similar size on the same side.
            let mut best_swap: Option<(f64, usize)> = None;
            for &j in order {
                if j == i || self.parts[j].back != self.parts[i].back {
                    continue;
                }
                let (ai, aj) = (area_mm2(self.parts[i].court), area_mm2(self.parts[j].court));
                if (ai - aj).abs() > 0.3 * ai.max(aj) {
                    continue;
                }
                let before_pair = before + self.part_cost(j);
                let (atj, _) = (self.parts[j].at, self.parts[j].rot);
                self.parts[i].at = atj;
                self.parts[j].at = at0;
                let delta = self.part_cost(i) + self.part_cost(j) - before_pair;
                self.parts[i].at = at0;
                self.parts[j].at = atj;
                if delta < -1e-6 && best_swap.is_none_or(|(b, _)| delta < b - 1e-9) {
                    best_swap = Some((delta, j));
                }
            }
            match (best, best_swap) {
                (Some((d, at, rot)), Some((ds, _))) if d <= ds => {
                    self.parts[i].at = at;
                    self.parts[i].rot = rot;
                    changed = true;
                }
                (_, Some((_, j))) => {
                    let atj = self.parts[j].at;
                    self.parts[j].at = at0;
                    self.parts[i].at = atj;
                    changed = true;
                }
                (Some((_, at, rot)), None) => {
                    self.parts[i].at = at;
                    self.parts[i].rot = rot;
                    changed = true;
                }
                (None, None) => {}
            }
        }
        changed
    }
}

/// The grid offsets on the square ring `r` around the origin, in a fixed order.
fn ring_offsets(r: i64) -> Vec<(i64, i64)> {
    if r == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::with_capacity((8 * r) as usize);
    for x in -r..=r {
        out.push((x, -r));
    }
    for y in -r + 1..=r {
        out.push((r, y));
    }
    for x in (-r..r).rev() {
        out.push((x, r));
    }
    for y in (-r + 1..r).rev() {
        out.push((-r, y));
    }
    out
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

/// Plans placing the footprints of `board` inside its outline.
pub fn plan_placement(
    board: &Board,
    hints: &PlaceHints,
    options: &PlaceOptions,
) -> Result<(BoardEditPlan, PlacementReport), ToolError> {
    let outline = board.outline.ok_or_else(|| {
        bad("the board has no outline; give it one first (pcb_outline_plan), so the parts have somewhere to go")
    })?;
    let grid = grid_nm(options.grid_mm)?;
    let step = if grid.0 >= 500_000 { grid } else { Nm(500_000) };
    if !(0.0..=10.0).contains(&options.edge_clearance_mm) {
        return Err(bad("edge_clearance_mm is 0 to 10"));
    }
    if !(0.0..=10.0).contains(&options.spacing_mm) {
        return Err(bad("spacing_mm is 0 to 10"));
    }
    let clearance = Nm((options.edge_clearance_mm * 1e6).round() as i64);
    let inner = Rect {
        min: Point::new(outline.min.x + clearance, outline.min.y + clearance),
        max: Point::new(outline.max.x - clearance, outline.max.y - clearance),
    };

    // Hints name parts on the board.
    let index: BTreeMap<&str, usize> = board
        .footprints
        .iter()
        .enumerate()
        .map(|(i, f)| (f.reference.as_str(), i))
        .collect();
    let known = |r: &str, what: &str| -> Result<usize, ToolError> {
        index.get(r).copied().ok_or_else(|| {
            ToolError::new(
                ErrorCode::SymbolNotFound,
                format!("{what} names {r}, which is not on the board (use pcb_list_footprints)"),
            )
        })
    };
    let mut fixed_by_hint: BTreeSet<usize> = BTreeSet::new();
    for r in &hints.keep {
        fixed_by_hint.insert(known(r, "keep")?);
    }
    let mut pinned: BTreeMap<usize, &PinnedPlace> = BTreeMap::new();
    for p in &hints.place {
        let i = known(&p.reference, "place")?;
        if fixed_by_hint.contains(&i) || pinned.insert(i, p).is_some() {
            return Err(bad(format!("{} is named twice in keep and place", p.reference)));
        }
    }
    let only: Option<BTreeSet<usize>> = match &hints.only {
        Some(list) => Some(list.iter().map(|r| known(r, "only")).collect::<Result<_, _>>()?),
        None => None,
    };
    let mut sides: BTreeMap<usize, bool> = BTreeMap::new();
    for (r, back) in &hints.side {
        sides.insert(known(r, "side")?, *back);
    }
    let mut edges: BTreeMap<usize, Edge> = BTreeMap::new();
    for (r, e) in &hints.edge {
        edges.insert(known(r, "edge")?, *e);
    }

    // The parts.
    let mut net_ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut fixed_names = Vec::new();
    let mut parts: Vec<Part> = Vec::new();
    for (i, f) in board.footprints.iter().enumerate() {
        let movable = !f.locked
            && !f.is_mounting_hole()
            && !fixed_by_hint.contains(&i)
            && !pinned.contains_key(&i)
            && only.as_ref().is_none_or(|o| o.contains(&i));
        let from = Placement::of(f);
        let back = sides.get(&i).copied().unwrap_or(from.back);
        if back != from.back && !movable {
            return Err(bad(format!(
                "{} cannot change side: it is locked, a hole, kept, placed by a hint, or not in only",
                f.reference
            )));
        }
        // Planned side and turn: the part as it would be after a flip, in its own coordinates.
        let at_side = if back != from.back {
            moved(f, Placement { back, ..from })
        } else {
            f.clone()
        };
        let (mut at, mut rot) = (at_side.position, normalize_deg(at_side.rotation_deg));
        let court_local = at_side.courtyard_local.unwrap_or_else(|| {
            let local_pads = at_side.pads.iter().map(|p| unplace_point(at, rot, p.at));
            Rect::from_points(local_pads)
                .map(|r| r.inflate(Nm(500_000)))
                .unwrap_or(Rect {
                    min: Point::new(Nm(-500_000), Nm(-500_000)),
                    max: Point::new(Nm(500_000), Nm(500_000)),
                })
        });
        let pads: Vec<(usize, Point)> = at_side
            .pads
            .iter()
            .filter(|p| !p.net.is_empty())
            .map(|p| {
                let n = net_ids.len();
                let id = *net_ids.entry(p.net.clone()).or_insert(n);
                (id, unplace_point(at, rot, p.at))
            })
            .collect();
        if let Some(pin) = pinned.get(&i) {
            at = snap(pin.at, grid);
            if let Some(r) = pin.rotation_deg {
                rot = normalize_deg(r);
            }
        }
        let role = if is_connector(&f.reference) || edges.contains_key(&i) {
            Role::Connector(edges.get(&i).copied())
        } else if is_ic(&f.reference) {
            Role::Ic
        } else {
            Role::Other
        };
        if !movable {
            fixed_names.push(f.reference.clone());
        }
        parts.push(Part {
            index: i,
            reference: f.reference.clone(),
            movable,
            back,
            through: f.pads.iter().any(|p| p.kind == "thru_hole" || p.kind == "np_thru_hole"),
            role,
            pads,
            court: court_local,
            at,
            rot,
            placed: !movable,
        });
    }
    let names: Vec<&str> = {
        let mut v: Vec<(&str, usize)> = net_ids.iter().map(|(n, i)| (n.as_str(), *i)).collect();
        v.sort_by_key(|(_, i)| *i);
        v.into_iter().map(|(n, _)| n).collect()
    };
    let mut nets: Vec<(f64, Vec<(usize, Point)>)> = names.iter().map(|n| (net_weight(n), vec![])).collect();
    let mut part_nets: Vec<Vec<usize>> = vec![vec![]; parts.len()];
    for (pi, p) in parts.iter().enumerate() {
        for (n, local) in &p.pads {
            nets[*n].1.push((pi, *local));
            if !part_nets[pi].contains(n) {
                part_nets[pi].push(*n);
            }
        }
    }
    // Nets with one pad connect nothing.
    for (pi, list) in part_nets.iter_mut().enumerate() {
        list.retain(|n| nets[*n].1.iter().any(|(q, _)| *q != pi));
    }

    // Decoupling: a capacitor between a supply and ground, served by the IC with the most pads on
    // that supply (the first by reference on a tie).
    let mut decoupling = Vec::new();
    for (ci, cap) in parts.iter().enumerate().filter(|(_, p)| p.reference.starts_with('C')) {
        let f = &board.footprints[cap.index];
        let Some(power) = f.pads.iter().find(|p| is_power_net(&p.net)) else {
            continue;
        };
        if !f.pads.iter().any(|p| is_ground_net(&p.net)) {
            continue;
        }
        let net = net_ids[power.net.as_str()];
        let cap_pad = cap
            .pads
            .iter()
            .find(|(n, _)| *n == net)
            .map(|(_, l)| *l)
            .expect("the supply pad");
        let ic = parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.role == Role::Ic)
            .filter_map(|(ui, u)| {
                let on: Vec<Point> = u.pads.iter().filter(|(n, _)| *n == net).map(|(_, l)| *l).collect();
                on.first().map(|l| (ui, *l, board.footprints[u.index].pads.len()))
            })
            .max_by(|a, b| {
                a.2.cmp(&b.2).then_with(|| {
                    crate::bom::natural(&parts[b.0].reference).cmp(&crate::bom::natural(&parts[a.0].reference))
                })
            });
        if let Some((ui, ic_pad, _)) = ic {
            decoupling.push(Decoupling {
                cap: ci,
                cap_pad,
                ic: ui,
                ic_pad,
            });
        }
    }
    let pos = |i: usize| {
        parts
            .iter()
            .position(|p| p.index == i)
            .expect("every footprint is a part")
    };
    let mut near = Vec::new();
    for h in &hints.near {
        let (a, b) = (known(&h.reference, "near")?, known(&h.target, "near")?);
        if a == b {
            return Err(bad(format!("{} cannot be near itself", h.reference)));
        }
        if !(0.0..=500.0).contains(&h.max_mm) {
            return Err(bad("near max_mm is 0 to 500"));
        }
        near.push((pos(a), pos(b), h.max_mm));
    }
    let mut groups = Vec::new();
    let mut group_names = Vec::new();
    for g in &hints.groups {
        let members: Vec<usize> = g.iter().map(|r| known(r, "group").map(pos)).collect::<Result<_, _>>()?;
        if members.len() >= 2 {
            groups.push(members);
            group_names.push(g.clone());
        }
    }

    let mut engine = Engine {
        parts,
        nets,
        part_nets,
        decoupling,
        near,
        groups,
        main_ic: None,
        half_spacing: Nm((options.spacing_mm * 5e5).round() as i64),
        outline,
        inner,
    };

    // Constructive order: connectors, ICs by pad count, then by how much each connects to what
    // is placed.
    let natural = |p: &Part| crate::bom::natural(&p.reference);
    let mut todo: Vec<usize> = (0..engine.parts.len()).filter(|i| engine.parts[*i].movable).collect();
    todo.sort_by_key(|i| natural(&engine.parts[*i]));
    let mut order: Vec<usize> = Vec::new();
    let take = |todo: &mut Vec<usize>, pick: &dyn Fn(&Part) -> bool, engine: &Engine| -> Vec<usize> {
        let (yes, no): (Vec<usize>, Vec<usize>) = todo.iter().partition(|i| pick(&engine.parts[**i]));
        *todo = no;
        yes
    };
    let connectors = take(&mut todo, &|p| matches!(p.role, Role::Connector(_)), &engine);
    let mut ics = take(&mut todo, &|p| p.role == Role::Ic, &engine);
    ics.sort_by_key(|i| std::cmp::Reverse(board.footprints[engine.parts[*i].index].pads.len()));
    engine.main_ic = ics.first().copied();
    // The main IC first (the middle), then connectors (their edges), then the other ICs.
    order.extend(ics.first().copied());
    order.extend(connectors);
    order.extend(ics.into_iter().skip(1));
    let board_centre = centre(outline);
    for &i in &order.clone() {
        let t = engine.target(i, board_centre);
        engine.place_near(i, t, step, grid);
    }
    while !todo.is_empty() {
        // The part most connected to what is placed (weights of shared nets).
        let score = |i: usize| -> f64 {
            engine.part_nets[i]
                .iter()
                .filter(|n| {
                    engine.nets[**n]
                        .1
                        .iter()
                        .any(|(q, _)| *q != i && engine.parts[*q].placed)
                })
                .map(|n| engine.nets[*n].0)
                .sum()
        };
        let (k, _) = todo
            .iter()
            .enumerate()
            .map(|(k, i)| (k, score(*i)))
            .fold((0, f64::MIN), |acc, (k, s)| if s > acc.1 + 1e-9 { (k, s) } else { acc });
        let i = todo.remove(k);
        let t = engine.target(i, board_centre);
        engine.place_near(i, t, step, grid);
        order.push(i);
    }

    // Improvement.
    let mut passes = 0;
    while passes < options.max_passes {
        passes += 1;
        if !engine.improve(&order, step) {
            break;
        }
    }

    // The plan.
    let mut expected = board.clone();
    let mut operations = Vec::new();
    let mut moved_names = Vec::new();
    for p in engine
        .parts
        .iter()
        .filter(|p| p.movable || pinned.contains_key(&p.index))
    {
        let f = &board.footprints[p.index];
        let from = Placement::of(f);
        let to = Placement {
            at: p.at,
            rotation_deg: normalize_deg(p.rot),
            back: p.back,
        };
        let same = from.at == to.at && (from.rotation_deg - to.rotation_deg).abs() < 1e-6 && from.back == to.back;
        if same {
            continue;
        }
        expected.footprints[p.index] = moved(f, to);
        operations.push(BoardOp::MoveFootprint {
            reference: f.reference.clone(),
            id: f.id.clone(),
            from,
            to,
            locked: f.locked,
        });
        moved_names.push(f.reference.clone());
    }
    let after_q = quality(&expected);
    let mut warnings = Vec::new();
    if !after_q.overlapping_pairs.is_empty() {
        warnings.push(format!(
            "{} pair(s) still overlap: {}; the board may be too small, or fixed parts are in the way",
            after_q.overlapping_pairs.len(),
            after_q.overlapping_pairs.join("; ")
        ));
    }
    if !after_q.outside_outline.is_empty() {
        warnings.push(format!(
            "outside the outline: {}; make the board larger (pcb_outline_plan with replace)",
            after_q.outside_outline.join(", ")
        ));
    }
    let used: f64 = engine
        .parts
        .iter()
        .map(|p| area_mm2(p.court.inflate(engine.half_spacing)))
        .sum();
    let ratio = used / area_mm2(outline).max(1e-9);
    if ratio > 0.6 {
        warnings.push(format!(
            "the courtyards cover {:.0}% of the board; a dense board is hard to route",
            ratio * 100.0
        ));
    }
    for p in engine
        .parts
        .iter()
        .filter(|p| board.footprints[p.index].courtyard_local.is_none() && p.movable)
    {
        warnings.push(format!(
            "{} has no courtyard; its size is estimated from its pads",
            p.reference
        ));
    }
    if operations.is_empty() {
        return Err(bad(
            "every footprint is already where the placement puts it; nothing to change",
        ));
    }
    let report = PlacementReport {
        main_ic: engine.main_ic.map(|i| engine.parts[i].reference.clone()),
        before: quality(board),
        after: after_q,
        fixed: fixed_names,
        moved: moved_names,
        groups: group_names,
        passes,
    };
    let plan = BoardEditPlan {
        kind: "place",
        operations,
        before: measure(board),
        after: measure(&expected),
        warnings,
        expected,
    };
    Ok((plan, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardFootprint, BoardPad};

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    /// A part at `at` with pads at the given local places and nets, and a courtyard `w` x `h` mm.
    fn part(reference: &str, at: Point, w: f64, h: f64, pads: &[(&str, f64, f64, &str)]) -> BoardFootprint {
        BoardFootprint {
            id: format!("id-{reference}"),
            reference: reference.into(),
            value: String::new(),
            lib_id: "L:F".into(),
            layer: "F.Cu".into(),
            position: at,
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            pads: pads
                .iter()
                .map(|(n, x, y, net)| BoardPad {
                    number: (*n).into(),
                    kind: "smd".into(),
                    at: at + p(*x, *y),
                    net: (*net).into(),
                })
                .collect(),
            courtyard: Some(Rect::from_points([at + p(-w / 2.0, -h / 2.0), at + p(w / 2.0, h / 2.0)]).unwrap()),
            courtyard_local: Some(Rect::from_points([p(-w / 2.0, -h / 2.0), p(w / 2.0, h / 2.0)]).unwrap()),
        }
    }

    /// A small regulator: J1 (input), U1 (three pins), C1 on the input, C2 on the output, R1 and
    /// D1 an LED on the output; all stacked at the origin as after an update from the schematic.
    fn stacked() -> Board {
        let o = p(0.0, 0.0);
        Board {
            footprints: vec![
                part("J1", o, 3.0, 6.0, &[("1", 0.0, -1.27, "VIN"), ("2", 0.0, 1.27, "GND")]),
                part(
                    "U1",
                    o,
                    7.0,
                    4.0,
                    &[("1", -2.3, 1.5, "GND"), ("2", 0.0, 1.5, "+3V3"), ("3", 2.3, 1.5, "VIN")],
                ),
                part("C1", o, 3.0, 1.6, &[("1", -0.8, 0.0, "VIN"), ("2", 0.8, 0.0, "GND")]),
                part("C2", o, 3.0, 1.6, &[("1", -0.8, 0.0, "+3V3"), ("2", 0.8, 0.0, "GND")]),
                part("R1", o, 3.0, 1.6, &[("1", -0.8, 0.0, "+3V3"), ("2", 0.8, 0.0, "LED")]),
                part("D1", o, 3.0, 1.6, &[("1", -0.8, 0.0, "LED"), ("2", 0.8, 0.0, "GND")]),
            ],
            outline: Rect::from_points([p(10.0, 10.0), p(40.0, 30.0)]),
            ..Default::default()
        }
    }

    fn at(plan: &BoardEditPlan, r: &str) -> BoardFootprint {
        plan.expected
            .footprints
            .iter()
            .find(|f| f.reference == r)
            .unwrap()
            .clone()
    }

    #[test]
    fn a_stacked_board_is_spread_inside_the_outline_without_overlaps() {
        let (plan, report) = plan_placement(&stacked(), &PlaceHints::default(), &PlaceOptions::default()).unwrap();
        assert!(report.after.overlapping_pairs.is_empty(), "{report:?}");
        assert!(report.after.outside_outline.is_empty(), "{report:?}");
        assert_eq!(report.before.overlapping_pairs.len(), 15, "everything was stacked");
        assert_eq!(plan.operations.len(), 6);
        assert_eq!(report.main_ic.as_deref(), Some("U1"));
        let u1 = at(&plan, "U1");
        assert!(
            dist_mm(u1.position, p(25.0, 20.0)) < 8.0,
            "the main IC near the middle: {u1:?}"
        );
        // The connector sits on an edge, the regulator inside.
        assert!(report.after.connectors[0].edge_mm < 0.01, "{report:?}");
        // Decoupling capacitors close to their pins.
        for d in &report.after.decoupling {
            assert!(d.distance_mm < 4.0, "{d:?}");
        }
        // On the grid.
        for f in &plan.expected.footprints {
            assert_eq!(f.position, f.position.snap(Nm(500_000)), "{}", f.reference);
        }
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn placement_is_deterministic() {
        let a = plan_placement(&stacked(), &PlaceHints::default(), &PlaceOptions::default()).unwrap();
        let b = plan_placement(&stacked(), &PlaceHints::default(), &PlaceOptions::default()).unwrap();
        assert_eq!(a.0.operations, b.0.operations);
    }

    #[test]
    fn hints_keep_place_edge_and_side_are_followed() {
        let hints = PlaceHints {
            keep: vec!["R1".into()],
            place: vec![PinnedPlace {
                reference: "U1".into(),
                at: p(25.0, 20.0),
                rotation_deg: Some(180.0),
            }],
            edge: vec![("J1".into(), Edge::Right)],
            side: vec![("D1".into(), true)],
            ..Default::default()
        };
        let mut board = stacked();
        // R1 kept: move it inside first so it is not in the way.
        board.footprints[4] = part(
            "R1",
            p(15.0, 25.0),
            3.0,
            1.6,
            &[("1", -0.8, 0.0, "+3V3"), ("2", 0.8, 0.0, "LED")],
        );
        let (plan, report) = plan_placement(&board, &hints, &PlaceOptions::default()).unwrap();
        assert!(report.fixed.contains(&"R1".to_owned()));
        assert!(!plan
            .operations
            .iter()
            .any(|o| matches!(o, BoardOp::MoveFootprint { reference, .. } if reference == "R1")));
        let u1 = at(&plan, "U1");
        assert_eq!((u1.position, u1.rotation_deg), (p(25.0, 20.0), 180.0));
        let j1 = at(&plan, "J1");
        assert!(j1.courtyard.unwrap().max.x >= p(39.99, 0.0).x, "{j1:?}");
        assert!(at(&plan, "D1").is_back());
        // A back-side part may sit under a front one.
        assert!(report.after.outside_outline.is_empty());
    }

    #[test]
    fn near_and_group_hints_pull_parts_together() {
        let hints = PlaceHints {
            near: vec![NearHint {
                reference: "R1".into(),
                target: "J1".into(),
                max_mm: 4.0,
            }],
            ..Default::default()
        };
        let (plan, _) = plan_placement(&stacked(), &hints, &PlaceOptions::default()).unwrap();
        let d = dist_mm(at(&plan, "R1").position, at(&plan, "J1").position);
        assert!(d <= 6.0, "R1 is {d} mm from J1");
    }

    #[test]
    fn locked_and_holes_stay_and_bad_hints_are_refused() {
        let mut board = stacked();
        board.footprints[1].locked = true;
        board.footprints[1] = BoardFootprint {
            position: p(25.0, 20.0),
            ..board.footprints[1].clone()
        };
        let mut hole = part("H1", p(12.0, 12.0), 6.0, 6.0, &[("", 0.0, 0.0, "")]);
        hole.lib_id = "MountingHole:MountingHole_3.2mm_M3".into();
        board.footprints.push(hole);
        let (plan, report) = plan_placement(&board, &PlaceHints::default(), &PlaceOptions::default()).unwrap();
        assert!(report.fixed.contains(&"U1".to_owned()) && report.fixed.contains(&"H1".to_owned()));
        assert!(
            report.after.overlapping_pairs.is_empty(),
            "the hole is an obstacle: {report:?}"
        );
        assert!(!plan
            .operations
            .iter()
            .any(|o| matches!(o, BoardOp::MoveFootprint { reference, .. } if reference == "U1" || reference == "H1")));

        let unknown = PlaceHints {
            keep: vec!["X9".into()],
            ..Default::default()
        };
        assert_eq!(
            plan_placement(&board, &unknown, &PlaceOptions::default())
                .unwrap_err()
                .code,
            ErrorCode::SymbolNotFound
        );
        let flip_locked = PlaceHints {
            side: vec![("U1".into(), true)],
            ..Default::default()
        };
        assert!(plan_placement(&board, &flip_locked, &PlaceOptions::default()).is_err());
        let mut no_outline = stacked();
        no_outline.outline = None;
        assert!(plan_placement(&no_outline, &PlaceHints::default(), &PlaceOptions::default()).is_err());
    }

    #[test]
    fn only_places_the_named_parts() {
        let hints = PlaceHints {
            only: Some(vec!["C1".into(), "C2".into()]),
            ..Default::default()
        };
        let (plan, report) = plan_placement(&stacked(), &hints, &PlaceOptions::default()).unwrap();
        let refs: Vec<&str> = plan
            .operations
            .iter()
            .filter_map(|o| match o {
                BoardOp::MoveFootprint { reference, .. } => Some(reference.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(refs, vec!["C1", "C2"]);
        assert_eq!(report.fixed.len(), 4);
    }

    #[test]
    fn a_board_too_small_is_reported() {
        let mut board = stacked();
        board.outline = Rect::from_points([p(10.0, 10.0), p(18.0, 16.0)]);
        let (plan, _) = plan_placement(&board, &PlaceHints::default(), &PlaceOptions::default()).unwrap();
        assert!(!plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn blocks_come_from_rectangles_on_the_schematic() {
        use crate::schematic::{DesignShape, DesignSymbol};
        let sym = |r: &str, x: f64, y: f64| DesignSymbol {
            id: r.into(),
            reference: r.into(),
            value: String::new(),
            library_id: "Device:R".into(),
            footprint: String::new(),
            position: p(x, y),
            orientation: Default::default(),
            unit: 1,
            fields: vec![],
            properties: Default::default(),
            dnp: false,
            exclude_from_bom: false,
            exclude_from_board: false,
        };
        let design = SchematicDesign {
            symbols: vec![
                sym("R1", 10.0, 10.0),
                sym("C1", 20.0, 10.0),
                sym("U1", 80.0, 10.0),
                sym("#PWR01", 12.0, 12.0),
            ],
            shapes: vec![DesignShape {
                id: "s".into(),
                kind: ShapeKind::Rectangle {
                    min: p(0.0, 0.0),
                    max: p(30.0, 30.0),
                },
                stroke: Nm(0),
                filled: false,
            }],
            ..Default::default()
        };
        assert_eq!(schematic_blocks(&design), vec![vec!["C1".to_owned(), "R1".to_owned()]]);
    }

    #[test]
    fn rings_cover_each_square_once() {
        for r in 0..4 {
            let ring = ring_offsets(r);
            assert_eq!(ring.len(), if r == 0 { 1 } else { 8 * r as usize });
            let unique: BTreeSet<_> = ring.iter().collect();
            assert_eq!(unique.len(), ring.len());
        }
    }
}
