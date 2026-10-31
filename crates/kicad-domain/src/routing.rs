// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit routing (Phase 12.5): tracks and vias for named nets, checked against every other
//! net's copper before anything is sent to KiCad.
//!
//! A route is a net and a list of points (pads such as `U1.2`, or places). Each point may name the
//! layer of the segment that leaves it; where the layer changes a via is placed (a through-hole pad
//! needs none). A route of two points without a path is given one: straight if it is clear, else a
//! 45° dogleg or an L, on the asked layer and then on the other outer layer, the shortest that keeps
//! the clearance. Widths, clearances, and vias come from the net classes; nothing is invented.
//! Copper of other nets (pads, tracks, vias, holes) and the board edge are checked; zones are not,
//! since a pour is refilled around tracks.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::board::{Board, BoardTrack, BoardVia};
use crate::board_edit::{measure, BoardEditPlan, BoardOp};
use crate::geometry::{Nm, Point, Rect};
use crate::result::{ErrorCode, ToolError};

/// The sizes one net is routed with.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct NetRules {
    pub clearance: Nm,
    pub track_width: Nm,
    pub via_diameter: Nm,
    pub via_drill: Nm,
}

impl Default for NetRules {
    /// KiCad's default net class.
    fn default() -> Self {
        NetRules {
            clearance: Nm(200_000),
            track_width: Nm(200_000),
            via_diameter: Nm(600_000),
            via_drill: Nm(300_000),
        }
    }
}

/// The board's rules: each net's class, and the board-wide minimums.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardRules {
    pub nets: BTreeMap<String, NetRules>,
    pub default: NetRules,
    pub copper_edge_clearance: Nm,
    pub min_track_width: Nm,
    /// Copper to a hole that has none of its own (a mounting hole).
    pub hole_clearance: Nm,
}

impl BoardRules {
    pub fn of(&self, net: &str) -> NetRules {
        self.nets.get(net).copied().unwrap_or(self.default)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteEnd {
    Pad { reference: String, number: String },
    At(Point),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutePoint {
    pub at: RouteEnd,
    /// The layer of the segment that leaves this point.
    pub layer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteRequest {
    /// The net; taken from the pads when not given.
    pub net: Option<String>,
    pub points: Vec<RoutePoint>,
    /// The layer to start on (default: the first point's, else F.Cu).
    pub layer: Option<String>,
    /// Track width; the net class's when not given.
    pub width: Option<Nm>,
    /// Lock the tracks and vias this route makes, so a later rip-up or autoroute keeps them.
    pub locked: bool,
}

/// What one route became.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteSummary {
    pub net: String,
    /// `straight`, `dogleg`, `l_shape`, or `as_given`.
    pub shape: &'static str,
    pub layers: Vec<String>,
    pub segments: usize,
    pub vias: usize,
    pub length_mm: f64,
    pub width_mm: f64,
    /// The pads it touches, `REF.PAD`.
    pub pads: Vec<String>,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn mm(n: i64) -> f64 {
    n as f64 / 1e6
}

fn fmt_pt(p: Point) -> String {
    format!("({:.2}, {:.2})", p.x.to_mm(), p.y.to_mm())
}

// ---------------------------------------------------------------- geometry (nm, as f64)

fn sub(a: Point, b: Point) -> (f64, f64) {
    ((a.x.0 - b.x.0) as f64, (a.y.0 - b.y.0) as f64)
}

fn point_seg(p: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = sub(b, a);
    let (px, py) = sub(p, a);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        ((px * dx + py * dy) / len2).clamp(0.0, 1.0)
    };
    (px - t * dx).hypot(py - t * dy)
}

fn cross(o: Point, a: Point, b: Point) -> f64 {
    let (ax, ay) = sub(a, o);
    let (bx, by) = sub(b, o);
    ax * by - ay * bx
}

fn segments_cross(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = cross(b1, b2, a1);
    let d2 = cross(b1, b2, a2);
    let d3 = cross(a1, a2, b1);
    let d4 = cross(a1, a2, b2);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

fn seg_seg(a1: Point, a2: Point, b1: Point, b2: Point) -> f64 {
    if segments_cross(a1, a2, b1, b2) {
        return 0.0;
    }
    point_seg(a1, b1, b2)
        .min(point_seg(a2, b1, b2))
        .min(point_seg(b1, a1, a2))
        .min(point_seg(b2, a1, a2))
}

fn inside(p: Point, r: Rect) -> bool {
    p.x >= r.min.x && p.x <= r.max.x && p.y >= r.min.y && p.y <= r.max.y
}

fn seg_rect(a: Point, b: Point, r: Rect) -> f64 {
    if inside(a, r) || inside(b, r) {
        return 0.0;
    }
    let c = [r.min, Point::new(r.max.x, r.min.y), r.max, Point::new(r.min.x, r.max.y)];
    (0..4)
        .map(|i| seg_seg(a, b, c[i], c[(i + 1) % 4]))
        .fold(f64::INFINITY, f64::min)
}

fn point_rect(p: Point, r: Rect) -> f64 {
    let dx = (r.min.x.0 - p.x.0).max(0).max(p.x.0 - r.max.x.0) as f64;
    let dy = (r.min.y.0 - p.y.0).max(0).max(p.y.0 - r.max.y.0) as f64;
    dx.hypot(dy)
}

// ---------------------------------------------------------------- obstacles

/// Copper that a new track or via must keep clear of.
enum Shape {
    /// A pad's copper box (or a point with a radius when its size is unknown).
    Box(Rect),
    Disc(Point, f64),
    Line(Point, Point, f64),
}

struct Obstacle {
    net: String,
    /// The copper layers it is on; empty for every layer.
    layers: Vec<String>,
    shape: Shape,
    clearance: Nm,
    what: String,
}

impl Obstacle {
    fn on(&self, layer: &str) -> bool {
        self.layers.is_empty() || self.layers.iter().any(|l| l == layer)
    }

    /// Distance from the copper's edge to the segment's centre line (nm).
    fn to_segment(&self, a: Point, b: Point) -> f64 {
        match self.shape {
            Shape::Box(r) => seg_rect(a, b, r),
            Shape::Disc(c, radius) => point_seg(c, a, b) - radius,
            Shape::Line(p, q, half) => seg_seg(a, b, p, q) - half,
        }
    }

    fn to_point(&self, c: Point) -> f64 {
        match self.shape {
            Shape::Box(r) => point_rect(c, r),
            Shape::Disc(o, radius) => {
                let (dx, dy) = sub(c, o);
                dx.hypot(dy) - radius
            }
            Shape::Line(p, q, half) => point_seg(c, p, q) - half,
        }
    }
}

fn obstacles(board: &Board, rules: &BoardRules) -> Vec<Obstacle> {
    let mut out = Vec::new();
    for f in &board.footprints {
        for p in &f.pads {
            let shape = match p.copper() {
                Some(r) => Shape::Box(r),
                None => Shape::Disc(p.at, 500_000.0),
            };
            let through = p.kind == "thru_hole" || p.kind == "np_thru_hole";
            let clearance = if p.kind == "np_thru_hole" {
                Nm(rules.of(&p.net).clearance.0.max(rules.hole_clearance.0))
            } else {
                rules.of(&p.net).clearance
            };
            out.push(Obstacle {
                net: p.net.clone(),
                layers: if through { vec![] } else { p.layers.clone() },
                shape,
                clearance,
                what: if p.number.is_empty() {
                    format!("{} (hole)", f.reference)
                } else {
                    format!("{}.{}", f.reference, p.number)
                },
            });
        }
    }
    for t in &board.tracks {
        out.push(Obstacle {
            net: t.net.clone(),
            layers: vec![t.layer.clone()],
            shape: Shape::Line(t.start, t.end, t.width.0 as f64 / 2.0),
            clearance: rules.of(&t.net).clearance,
            what: format!("the {} track at {}", t.net, fmt_pt(t.start)),
        });
    }
    for v in &board.vias {
        out.push(Obstacle {
            net: v.net.clone(),
            layers: vec![],
            shape: Shape::Disc(v.at, v.diameter.0 as f64 / 2.0),
            clearance: rules.of(&v.net).clearance,
            what: format!("the {} via at {}", v.net, fmt_pt(v.at)),
        });
    }
    out
}

/// A segment or via of the plan, before it becomes an operation.
#[derive(Debug, Clone)]
enum Piece {
    Track { a: Point, b: Point, layer: String },
    Via { at: Point },
}

struct Checker<'a> {
    obstacles: Vec<Obstacle>,
    outline: Option<Rect>,
    rules: &'a BoardRules,
}

impl Checker<'_> {
    /// The first conflict of a candidate route, in words.
    fn conflict(&self, net: &str, width: Nm, via: (Nm, Nm), pieces: &[Piece]) -> Option<String> {
        let own = self.rules.of(net).clearance;
        let half = width.0 as f64 / 2.0;
        let vr = via.0 .0 as f64 / 2.0;
        for piece in pieces {
            // The board edge.
            if let Some(o) = self.outline {
                let keep = self.rules.copper_edge_clearance.0 as f64;
                let pts: Vec<(Point, f64)> = match piece {
                    Piece::Track { a, b, .. } => vec![(*a, half), (*b, half)],
                    Piece::Via { at } => vec![(*at, vr)],
                };
                for (p, r) in pts {
                    let gap = [
                        p.x.0 - o.min.x.0,
                        o.max.x.0 - p.x.0,
                        p.y.0 - o.min.y.0,
                        o.max.y.0 - p.y.0,
                    ]
                    .into_iter()
                    .min()
                    .unwrap_or(0) as f64
                        - r;
                    if gap < keep - 1.0 {
                        return Some(format!(
                            "copper at {} is {:.2} mm from the board edge; it needs {:.2} mm",
                            fmt_pt(p),
                            gap.max(0.0) / 1e6,
                            keep / 1e6
                        ));
                    }
                }
            }
            for o in self.obstacles.iter().filter(|o| o.net != net) {
                let need = own.0.max(o.clearance.0) as f64;
                let (gap, layer_ok, desc) = match piece {
                    Piece::Track { a, b, layer } => (
                        o.to_segment(*a, *b) - half,
                        o.on(layer),
                        format!("the track {} to {} on {layer}", fmt_pt(*a), fmt_pt(*b)),
                    ),
                    Piece::Via { at } => (o.to_point(*at) - vr, true, format!("the via at {}", fmt_pt(*at))),
                };
                if layer_ok && gap < need - 1.0 {
                    return Some(format!(
                        "{desc} comes {:.2} mm from {} (net '{}'); the clearance is {:.2} mm",
                        gap.max(0.0) / 1e6,
                        o.what,
                        o.net,
                        need / 1e6
                    ));
                }
            }
        }
        None
    }

    /// The route's pieces become obstacles for the routes after it.
    fn add(&mut self, net: &str, width: Nm, via: Nm, pieces: &[Piece]) {
        for piece in pieces {
            self.obstacles.push(match piece {
                Piece::Track { a, b, layer } => Obstacle {
                    net: net.to_owned(),
                    layers: vec![layer.clone()],
                    shape: Shape::Line(*a, *b, width.0 as f64 / 2.0),
                    clearance: self.rules.of(net).clearance,
                    what: format!("the planned {net} track at {}", fmt_pt(*a)),
                },
                Piece::Via { at } => Obstacle {
                    net: net.to_owned(),
                    layers: vec![],
                    shape: Shape::Disc(*at, via.0 as f64 / 2.0),
                    clearance: self.rules.of(net).clearance,
                    what: format!("the planned {net} via at {}", fmt_pt(*at)),
                },
            });
        }
    }
}

/// The path a route takes: the shortest clear candidate.
struct Chosen {
    length: f64,
    shape: &'static str,
    path: Vec<Point>,
    layers: Vec<String>,
    pieces: Vec<Piece>,
}

/// The paths tried between two points, shortest kinds first.
fn simple_paths(a: Point, b: Point) -> Vec<(&'static str, Vec<Point>)> {
    let (dx, dy) = (b.x.0 - a.x.0, b.y.0 - a.y.0);
    let mut out = vec![("straight", vec![a, b])];
    if dx != 0 && dy != 0 {
        let d = dx.abs().min(dy.abs());
        let diag = Point::new(Nm(dx.signum() * d), Nm(dy.signum() * d));
        out.push(("dogleg", vec![a, a + diag, b]));
        out.push(("dogleg", vec![a, b - diag, b]));
        out.push(("l_shape", vec![a, Point::new(b.x, a.y), b]));
        out.push(("l_shape", vec![a, Point::new(a.x, b.y), b]));
    }
    // Drop zero-length legs (a dogleg on a 45° line is straight).
    for (_, pts) in &mut out {
        pts.dedup();
    }
    out
}

fn length(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|w| {
            let (dx, dy) = sub(w[1], w[0]);
            dx.hypot(dy)
        })
        .sum()
}

/// Plans the routes; refuses the whole plan if any route cannot keep its clearance.
pub fn plan_routes(
    board: &Board,
    rules: &BoardRules,
    requests: &[RouteRequest],
) -> Result<(BoardEditPlan, Vec<RouteSummary>), ToolError> {
    if requests.is_empty() {
        return Err(bad("nothing to route; give at least one route"));
    }
    let outer = |l: &str| board.copper_layers.iter().any(|c| c == l);
    let front = board.copper_layers.first().cloned().unwrap_or_else(|| "F.Cu".into());
    let back = board.copper_layers.last().cloned().unwrap_or_else(|| "B.Cu".into());
    let mut checker = Checker {
        obstacles: obstacles(board, rules),
        outline: board.outline,
        rules,
    };
    let mut expected = board.clone();
    let mut operations = Vec::new();
    let mut summaries = Vec::new();
    let mut failures = Vec::new();

    for (n, req) in requests.iter().enumerate() {
        let label = format!("route {}", n + 1);
        if req.points.len() < 2 {
            return Err(bad(format!("{label}: give at least two points")));
        }
        // Resolve the points.
        let mut pts: Vec<(Point, Option<&crate::board::BoardPad>, String)> = Vec::new();
        for p in &req.points {
            match &p.at {
                RouteEnd::At(at) => pts.push((*at, None, fmt_pt(*at))),
                RouteEnd::Pad { reference, number } => {
                    let f = board
                        .footprints
                        .iter()
                        .find(|f| &f.reference == reference)
                        .ok_or_else(|| {
                            ToolError::new(
                                ErrorCode::SymbolNotFound,
                                format!("{label}: no footprint {reference} on the board"),
                            )
                        })?;
                    let pad = f.pads.iter().find(|q| &q.number == number).ok_or_else(|| {
                        ToolError::new(
                            ErrorCode::SymbolNotFound,
                            format!("{label}: {reference} has no pad {number}"),
                        )
                    })?;
                    pts.push((pad.at, Some(pad), format!("{reference}.{number}")));
                }
            }
        }
        // The net.
        let pad_nets: Vec<(&String, &str)> = pts
            .iter()
            .filter_map(|(_, pad, name)| pad.map(|p| (name, p.net.as_str())))
            .collect();
        let net = match (&req.net, pad_nets.first()) {
            (Some(n), _) => n.clone(),
            (None, Some((_, n))) => (*n).to_owned(),
            (None, None) => return Err(bad(format!("{label}: give the net, or start or end at a pad"))),
        };
        if net.is_empty() {
            return Err(bad(format!(
                "{label}: the pad is on no net; a track there would connect nothing"
            )));
        }
        if !board.nets.iter().any(|x| x == &net) {
            return Err(bad(format!(
                "{label}: the board has no net '{net}' (use pcb_list_nets)"
            )));
        }
        for (name, pn) in &pad_nets {
            if *pn != net {
                return Err(bad(format!(
                    "{label}: {name} is on net '{pn}', the route is for '{net}'; a track would short them"
                )));
            }
        }
        let class = rules.of(&net);
        let width = req.width.unwrap_or(class.track_width);
        if width < rules.min_track_width || width.0 <= 0 {
            return Err(bad(format!(
                "{label}: a {:.2} mm track is below the board's minimum of {:.2} mm",
                mm(width.0),
                mm(rules.min_track_width.0)
            )));
        }
        let via = (class.via_diameter, class.via_drill);
        // Layers: the asked one, else the first point's, else a layer every pad of the route is
        // on, front first.
        let reaches = |pad: Option<&crate::board::BoardPad>, layer: &str| {
            pad.is_none_or(|p| p.kind == "thru_hole" || p.layers.is_empty() || p.on_layer(layer))
        };
        for l in req
            .points
            .iter()
            .filter_map(|p| p.layer.as_deref())
            .chain(req.layer.as_deref())
        {
            if !outer(l) {
                return Err(bad(format!(
                    "{label}: '{l}' is not a copper layer of this board ({})",
                    board.copper_layers.join(", ")
                )));
            }
        }
        let start = req
            .points
            .first()
            .and_then(|p| p.layer.clone())
            .or_else(|| req.layer.clone());

        let auto = req.points.len() == 2 && req.points.iter().all(|p| p.layer.is_none());
        let candidates: Vec<(&'static str, Vec<Point>, Vec<String>)> = if auto {
            let (a, b) = (pts[0].0, pts[1].0);
            let mut layers: Vec<String> = match &start {
                Some(l) => vec![l.clone()],
                None => vec![front.clone(), back.clone()],
            };
            layers.dedup();
            layers.retain(|l| reaches(pts[0].1, l) && reaches(pts[1].1, l));
            if layers.is_empty() {
                return Err(bad(format!(
                    "{label}: {} and {} share no copper layer{}; give a point with a layer to change layer through a via",
                    pts[0].2,
                    pts[1].2,
                    start.as_ref().map(|l| format!(" with {l}")).unwrap_or_default()
                )));
            }
            let mut c = Vec::new();
            for l in &layers {
                for (shape, path) in simple_paths(a, b) {
                    let n = path.len() - 1;
                    c.push((shape, path, vec![l.clone(); n]));
                }
            }
            c
        } else {
            // As given: each point's layer holds until the next point names another.
            let mut layer = start.clone().unwrap_or_else(|| {
                if reaches(pts[0].1, &front) {
                    front.clone()
                } else {
                    back.clone()
                }
            });
            let mut layers = Vec::new();
            for p in &req.points[..req.points.len() - 1] {
                if let Some(l) = &p.layer {
                    layer = l.clone();
                }
                layers.push(layer.clone());
            }
            vec![("as_given", pts.iter().map(|(p, _, _)| *p).collect(), layers)]
        };

        // Build each candidate's pieces and keep the shortest clear one.
        let mut best: Option<Chosen> = None;
        let mut first_problem: Option<String> = None;
        for (shape, path, layers) in candidates {
            let mut pieces = Vec::new();
            let mut problem = None;
            // The asked point at each corner of the path: every one when given, else the two ends.
            let last = path.len() - 1;
            let point_at = |k: usize| -> Option<usize> {
                match (auto, k) {
                    (false, k) => Some(k),
                    (true, 0) => Some(0),
                    (true, k) if k == last => Some(1),
                    _ => None,
                }
            };
            for (i, w) in path.windows(2).enumerate() {
                let layer = &layers[i];
                // A leg that starts or ends on a pad must be on one of its layers.
                for k in [i, i + 1] {
                    if let Some(idx) = point_at(k) {
                        if !reaches(pts[idx].1, layer) {
                            problem = Some(format!(
                                "{} is not on {layer}; give that point the pad's layer, or route to it on its own layer",
                                pts[idx].2
                            ));
                        }
                    }
                }
                if i > 0 && layers[i - 1] != *layer {
                    let through_pad = point_at(i)
                        .and_then(|idx| pts[idx].1)
                        .is_some_and(|p| p.kind == "thru_hole");
                    if !through_pad {
                        pieces.push(Piece::Via { at: w[0] });
                    }
                }
                pieces.push(Piece::Track {
                    a: w[0],
                    b: w[1],
                    layer: layer.clone(),
                });
            }
            if problem.is_none() {
                problem = checker.conflict(&net, width, via, &pieces);
            }
            match problem {
                Some(p) => {
                    first_problem.get_or_insert(p);
                }
                None => {
                    let len = length(&path);
                    if best.as_ref().is_none_or(|b| len < b.length - 1.0) {
                        best = Some(Chosen {
                            length: len,
                            shape,
                            path,
                            layers,
                            pieces,
                        });
                    }
                }
            }
        }
        let Some(Chosen {
            length: len,
            shape,
            path,
            layers,
            pieces,
        }) = best
        else {
            failures.push(format!(
                "{label} ({net}, {} to {}): {}{}",
                pts[0].2,
                pts[pts.len() - 1].2,
                first_problem.unwrap_or_else(|| "no path".into()),
                if auto {
                    "; no straight, 45° or L path is clear: give waypoints"
                } else {
                    ""
                }
            ));
            continue;
        };
        checker.add(&net, width, via.0, &pieces);
        let mut vias = 0;
        for piece in &pieces {
            match piece {
                Piece::Track { a, b, layer } => {
                    operations.push(BoardOp::AddTrack {
                        net: net.clone(),
                        start: *a,
                        end: *b,
                        width,
                        layer: layer.clone(),
                        locked: req.locked,
                    });
                    expected.tracks.push(BoardTrack {
                        id: String::new(),
                        start: *a,
                        end: *b,
                        width,
                        layer: layer.clone(),
                        net: net.clone(),
                        locked: req.locked,
                    });
                }
                Piece::Via { at } => {
                    vias += 1;
                    operations.push(BoardOp::AddVia {
                        net: net.clone(),
                        at: *at,
                        diameter: via.0,
                        drill: via.1,
                        locked: req.locked,
                    });
                    expected.vias.push(BoardVia {
                        id: String::new(),
                        at: *at,
                        diameter: via.0,
                        drill: via.1,
                        net: net.clone(),
                        locked: req.locked,
                    });
                }
            }
        }
        let mut used = layers.clone();
        used.dedup();
        summaries.push(RouteSummary {
            net: net.clone(),
            shape,
            layers: used,
            segments: path.len() - 1,
            vias,
            length_mm: (len / 1e4).round() / 100.0,
            width_mm: mm(width.0),
            pads: pts.iter().filter(|p| p.1.is_some()).map(|p| p.2.clone()).collect(),
        });
    }
    if !failures.is_empty() {
        return Err(bad(format!(
            "{} of {} route(s) cannot keep their clearance; nothing was planned: {}",
            failures.len(),
            requests.len(),
            failures.join(" | ")
        )));
    }
    Ok((
        BoardEditPlan {
            kind: "route",
            operations,
            before: measure(board),
            after: measure(&expected),
            warnings: vec![],
            expected,
        },
        summaries,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardFootprint, BoardPad};

    fn p(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn pad(number: &str, at: Point, net: &str, layer: &str) -> BoardPad {
        BoardPad {
            number: number.into(),
            kind: "smd".into(),
            at,
            net: net.into(),
            size: Some((Nm(1_000_000), Nm(1_000_000))),
            angle_deg: 0.0,
            layers: vec![layer.into()],
        }
    }

    fn fp(reference: &str, pads: Vec<BoardPad>) -> BoardFootprint {
        BoardFootprint {
            id: reference.into(),
            reference: reference.into(),
            value: String::new(),
            lib_id: "L:F".into(),
            layer: "F.Cu".into(),
            position: pads[0].at,
            rotation_deg: 0.0,
            locked: false,
            dnp: false,
            exclude_from_bom: false,
            exclude_from_pos: false,
            board_only: false,
            pads,
            courtyard: None,
            courtyard_local: None,
        }
    }

    /// R1.1 (A) at (10, 10) and R2.1 (A) at (20, 10); R1.2 (B) at (10, 14), R2.2 (B) at (20, 14).
    fn board() -> Board {
        Board {
            footprints: vec![
                fp(
                    "R1",
                    vec![
                        pad("1", p(10.0, 10.0), "A", "F.Cu"),
                        pad("2", p(10.0, 14.0), "B", "F.Cu"),
                    ],
                ),
                fp(
                    "R2",
                    vec![
                        pad("1", p(20.0, 10.0), "A", "F.Cu"),
                        pad("2", p(20.0, 14.0), "B", "F.Cu"),
                    ],
                ),
            ],
            nets: vec!["A".into(), "B".into(), "C".into()],
            copper_layers: vec!["F.Cu".into(), "B.Cu".into()],
            outline: Rect::from_points([p(0.0, 0.0), p(30.0, 30.0)]),
            ..Default::default()
        }
    }

    fn rules() -> BoardRules {
        BoardRules {
            copper_edge_clearance: Nm(500_000),
            min_track_width: Nm(150_000),
            ..Default::default()
        }
    }

    fn pads(a: &str, b: &str) -> RouteRequest {
        let end = |s: &str| {
            let (r, n) = s.split_once('.').unwrap();
            RoutePoint {
                at: RouteEnd::Pad {
                    reference: r.into(),
                    number: n.into(),
                },
                layer: None,
            }
        };
        RouteRequest {
            points: vec![end(a), end(b)],
            ..Default::default()
        }
    }

    #[test]
    fn a_clear_pair_of_pads_is_joined_straight_at_the_class_width() {
        let (plan, s) = plan_routes(&board(), &rules(), &[pads("R1.1", "R2.1")]).unwrap();
        assert_eq!(s[0].shape, "straight");
        assert_eq!(s[0].net, "A");
        assert_eq!(s[0].length_mm, 10.0);
        assert!(
            matches!(&plan.operations[0], BoardOp::AddTrack { width, layer, .. } if *width == Nm(200_000) && layer == "F.Cu")
        );
        assert_eq!(plan.expected.tracks.len(), 1);
    }

    #[test]
    fn a_track_that_would_cross_another_net_goes_around_or_is_refused() {
        // R1.1 to R2.2 diagonally: the straight line passes nothing, but put a C pad in the way.
        let mut b = board();
        b.footprints.push(fp("X1", vec![pad("1", p(15.0, 12.0), "C", "F.Cu")]));
        let (_, s) = plan_routes(&b, &rules(), &[pads("R1.2", "R2.2")]).unwrap();
        assert_eq!(s[0].shape, "straight", "y = 14 passes 1.5 mm from X1.1");
        let r = RouteRequest {
            net: Some("A".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(5.0, 12.0)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(25.0, 12.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let err = plan_routes(&b, &rules(), &[r]).unwrap_err();
        assert!(err.message.contains("X1.1"), "{}", err.message);
        // The same on the back is clear: X1 is a front pad.
        let mut on_back = pads("R1.1", "R2.1");
        on_back.points[0].layer = None;
        on_back.layer = Some("B.Cu".into());
        let err = plan_routes(&b, &rules(), &[on_back]).unwrap_err();
        assert!(err.message.contains("share no copper layer"), "{}", err.message);
    }

    #[test]
    fn a_route_around_an_obstacle_is_found_when_one_is_clear() {
        let mut b = board();
        // On the straight line from R1.1 (10, 10) to (24, 20), clear of the 45° dogleg.
        b.footprints.push(fp("X1", vec![pad("1", p(17.0, 15.0), "C", "F.Cu")]));
        let mut r = pads("R1.1", "R2.2");
        r.net = Some("A".into());
        // R2.2 is on B: that would short; refused.
        assert!(plan_routes(&b, &rules(), &[r]).is_err());
        // From R1.1 to a place below-right, around X1.
        let req = RouteRequest {
            net: None,
            points: vec![
                RoutePoint {
                    at: RouteEnd::Pad {
                        reference: "R1".into(),
                        number: "1".into(),
                    },
                    layer: None,
                },
                RoutePoint {
                    at: RouteEnd::At(p(24.0, 20.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let (_, s) = plan_routes(&b, &rules(), &[req]).unwrap();
        assert_eq!(s[0].shape, "dogleg", "the straight line passes X1");
    }

    #[test]
    fn a_layer_change_places_a_via_from_the_class() {
        let req = RouteRequest {
            net: None,
            points: vec![
                RoutePoint {
                    at: RouteEnd::Pad {
                        reference: "R1".into(),
                        number: "1".into(),
                    },
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(10.0, 5.0)),
                    layer: Some("B.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(20.0, 5.0)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::Pad {
                        reference: "R2".into(),
                        number: "1".into(),
                    },
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let (plan, s) = plan_routes(&board(), &rules(), &[req]).unwrap();
        assert_eq!((s[0].segments, s[0].vias), (3, 2));
        assert!(plan.operations.iter().any(
            |o| matches!(o, BoardOp::AddVia { at, diameter, .. } if *at == p(10.0, 5.0) && *diameter == Nm(600_000))
        ));
    }

    #[test]
    fn wrong_nets_edges_widths_and_unknown_pads_are_refused() {
        let b = board();
        assert!(plan_routes(&b, &rules(), &[pads("R1.1", "R2.2")])
            .unwrap_err()
            .message
            .contains("short"));
        assert_eq!(
            plan_routes(&b, &rules(), &[pads("R1.1", "R9.1")]).unwrap_err().code,
            ErrorCode::SymbolNotFound
        );
        let mut thin = pads("R1.1", "R2.1");
        thin.width = Some(Nm(100_000));
        assert!(plan_routes(&b, &rules(), &[thin])
            .unwrap_err()
            .message
            .contains("minimum"));
        let edge = RouteRequest {
            net: Some("A".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(0.2, 5.0)),
                    layer: None,
                },
                RoutePoint {
                    at: RouteEnd::At(p(0.2, 9.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        assert!(plan_routes(&b, &rules(), &[edge]).unwrap_err().message.contains("edge"));
    }

    #[test]
    fn routes_in_one_plan_keep_clear_of_each_other() {
        // A from R1.1 straight down to (10, 20) on F.Cu, then B from (5, 15) to (15, 15) crosses it.
        let a = RouteRequest {
            net: Some("A".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(8.0, 16.0)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(8.0, 24.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let b_route = RouteRequest {
            net: Some("B".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(4.0, 20.0)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(12.0, 20.0)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let err = plan_routes(&board(), &rules(), &[a.clone(), b_route.clone()]).unwrap_err();
        assert!(err.message.contains("planned A track"), "{}", err.message);
        // On different layers they pass.
        let mut b2 = b_route;
        b2.points[0].layer = Some("B.Cu".into());
        assert!(plan_routes(&board(), &rules(), &[a, b2]).is_ok());
    }

    #[test]
    fn a_mounting_hole_keeps_the_board_hole_clearance() {
        let mut b = board();
        let mut hole = pad("", p(20.0, 20.0), "", "F.Cu");
        hole.kind = "np_thru_hole".into();
        hole.size = Some((Nm(3_200_000), Nm(3_200_000)));
        b.footprints.push(fp("H1", vec![hole]));
        let mut r = rules();
        r.hole_clearance = Nm(250_000);
        // Track edge 0.21 mm below the hole: clear of 0.2, inside 0.25.
        let y = 20.0 + 1.6 + 0.21 + 0.1;
        let req = RouteRequest {
            net: Some("A".into()),
            points: vec![
                RoutePoint {
                    at: RouteEnd::At(p(15.0, y)),
                    layer: Some("F.Cu".into()),
                },
                RoutePoint {
                    at: RouteEnd::At(p(25.0, y)),
                    layer: None,
                },
            ],
            ..Default::default()
        };
        let err = plan_routes(&b, &r, std::slice::from_ref(&req)).unwrap_err();
        assert!(
            err.message.contains("H1 (hole)") && err.message.contains("0.25"),
            "{}",
            err.message
        );
        r.hole_clearance = Nm(0);
        assert!(plan_routes(&b, &r, &[req]).is_ok());
    }

    #[test]
    fn geometry() {
        assert_eq!(point_seg(p(0.0, 1.0), p(-1.0, 0.0), p(1.0, 0.0)), 1e6);
        assert_eq!(seg_seg(p(0.0, -1.0), p(0.0, 1.0), p(-1.0, 0.0), p(1.0, 0.0)), 0.0);
        let r = Rect::from_points([p(0.0, 0.0), p(1.0, 1.0)]).unwrap();
        assert_eq!(seg_rect(p(2.0, -1.0), p(2.0, 2.0), r), 1e6);
        assert_eq!(seg_rect(p(-1.0, 0.5), p(2.0, 0.5), r), 0.0);
        assert_eq!(point_rect(p(4.0, 5.0), r), 5e6);
    }
}
