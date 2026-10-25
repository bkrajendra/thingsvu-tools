// SPDX-License-Identifier: GPL-3.0-or-later
//! Schematic geometry in integer nanometres (KiCad's API unit). Public tool inputs are in mm and
//! are converted here; everything after that is exact integer arithmetic, so grid checks never
//! suffer from floating-point rounding.
//!
//! Axes follow the KiCad schematic canvas: +x to the right, +y downwards.

use std::fmt;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

/// A length or coordinate in nanometres.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Nm(pub i64);

/// The schematic connection grid: 50 mil = 1.27 mm.
pub const GRID: Nm = Nm(1_270_000);

/// Largest accepted coordinate magnitude (10 m), far outside any real sheet.
pub const MAX_COORD_MM: f64 = 10_000.0;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GeometryError {
    #[error("coordinate {0} mm is not a finite number within ±{MAX_COORD_MM} mm")]
    OutOfRange(f64),
    #[error("point ({x_mm} mm, {y_mm} mm) is not on the {grid_mm} mm grid; nearest grid point is ({snap_x_mm} mm, {snap_y_mm} mm)")]
    OffGrid {
        x_mm: f64,
        y_mm: f64,
        grid_mm: f64,
        snap_x_mm: f64,
        snap_y_mm: f64,
    },
}

impl Nm {
    pub fn from_mm(mm: f64) -> Result<Self, GeometryError> {
        if !mm.is_finite() || mm.abs() > MAX_COORD_MM {
            return Err(GeometryError::OutOfRange(mm));
        }
        Ok(Nm((mm * 1_000_000.0).round() as i64))
    }

    pub fn to_mm(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    pub fn is_multiple_of(self, step: Nm) -> bool {
        self.0.rem_euclid(step.0) == 0
    }

    /// Nearest multiple of `step` (halves round away from zero).
    pub fn snap(self, step: Nm) -> Nm {
        let q = self.0 as f64 / step.0 as f64;
        Nm(q.round() as i64 * step.0)
    }
}

impl Add for Nm {
    type Output = Nm;
    fn add(self, rhs: Nm) -> Nm {
        Nm(self.0 + rhs.0)
    }
}

impl Sub for Nm {
    type Output = Nm;
    fn sub(self, rhs: Nm) -> Nm {
        Nm(self.0 - rhs.0)
    }
}

impl fmt::Display for Nm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} mm", self.to_mm())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: Nm,
    pub y: Nm,
}

impl Point {
    pub const fn new(x: Nm, y: Nm) -> Self {
        Self { x, y }
    }

    pub fn from_mm(x_mm: f64, y_mm: f64) -> Result<Self, GeometryError> {
        Ok(Self::new(Nm::from_mm(x_mm)?, Nm::from_mm(y_mm)?))
    }

    pub fn is_on_grid(self, grid: Nm) -> bool {
        self.x.is_multiple_of(grid) && self.y.is_multiple_of(grid)
    }

    pub fn snap(self, grid: Nm) -> Point {
        Point::new(self.x.snap(grid), self.y.snap(grid))
    }

    /// Accepts a point only if it lies on `grid`; the error names the nearest grid point.
    pub fn require_on_grid(self, grid: Nm) -> Result<Point, GeometryError> {
        if self.is_on_grid(grid) {
            return Ok(self);
        }
        let s = self.snap(grid);
        Err(GeometryError::OffGrid {
            x_mm: self.x.to_mm(),
            y_mm: self.y.to_mm(),
            grid_mm: grid.to_mm(),
            snap_x_mm: s.x.to_mm(),
            snap_y_mm: s.y.to_mm(),
        })
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, rhs: Point) -> Point {
        Point::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, rhs: Point) -> Point {
        Point::new(self.x - rhs.x, self.y - rhs.y)
    }
}

/// Placement transform of a symbol: a rotation in 90° steps, then optional mirroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Orientation {
    /// Counter-clockwise as seen on screen: 0, 90, 180 or 270.
    pub rotation_deg: u16,
    /// Mirror across the horizontal axis (flips y).
    pub mirror_x: bool,
    /// Mirror across the vertical axis (flips x).
    pub mirror_y: bool,
}

impl Orientation {
    /// True when both transforms place every point the same way. KiCad stores transforms in a
    /// normalized form (e.g. 180° + mirror_x reads back as mirror_y), so compare by effect.
    pub fn same_effect(self, other: Orientation) -> bool {
        let probes = [Point::new(Nm(1), Nm(0)), Point::new(Nm(0), Nm(1))];
        probes
            .iter()
            .all(|p| self.apply(*p).is_some() && self.apply(*p) == other.apply(*p))
    }

    /// Maps a pin position relative to the symbol origin (in placed-symbol axes, y down) to an
    /// offset in sheet coordinates. Returns `None` for a rotation that is not a multiple of 90°.
    pub fn apply(self, local: Point) -> Option<Point> {
        let (x, y) = (local.x.0, local.y.0);
        // With y pointing down, a visually counter-clockwise quarter turn maps (x, y) to (y, -x).
        let (mut x, mut y) = match self.rotation_deg {
            0 => (x, y),
            90 => (y, -x),
            180 => (-x, -y),
            270 => (-y, x),
            _ => return None,
        };
        if self.mirror_x {
            y = -y;
        }
        if self.mirror_y {
            x = -x;
        }
        Some(Point::new(Nm(x), Nm(y)))
    }

    /// The inverse of [`apply`](Self::apply): the point relative to the symbol origin that lands
    /// at `offset` on the sheet. Returns `None` for a rotation that is not a multiple of 90°.
    pub fn unapply(self, offset: Point) -> Option<Point> {
        // The images of the two axes are orthonormal columns, so the inverse is their transpose.
        let x_axis = self.apply(Point::new(Nm(1), Nm(0)))?;
        let y_axis = self.apply(Point::new(Nm(0), Nm(1)))?;
        Some(Point::new(
            Nm(offset.x.0 * x_axis.x.0 + offset.y.0 * x_axis.y.0),
            Nm(offset.x.0 * y_axis.x.0 + offset.y.0 * y_axis.y.0),
        ))
    }
}

/// One of the four drawing directions, in sheet axes (y down).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Left,
    #[default]
    Right,
    Up,
    Down,
}

impl Dir {
    pub const ALL: [Dir; 4] = [Dir::Left, Dir::Right, Dir::Up, Dir::Down];

    /// A step of `length` in this direction.
    pub fn step(self, length: Nm) -> Point {
        let l = length.0;
        let (x, y) = match self {
            Dir::Left => (-l, 0),
            Dir::Right => (l, 0),
            Dir::Up => (0, -l),
            Dir::Down => (0, l),
        };
        Point::new(Nm(x), Nm(y))
    }

    pub fn opposite(self) -> Dir {
        match self {
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
        }
    }

    pub fn is_horizontal(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }

    /// This direction after the symbol transform `o`.
    pub fn transformed(self, o: Orientation) -> Dir {
        let p = o.apply(self.step(Nm(1))).unwrap_or(self.step(Nm(1)));
        match (p.x.0.signum(), p.y.0.signum()) {
            (-1, _) => Dir::Left,
            (1, _) => Dir::Right,
            (_, -1) => Dir::Up,
            _ => Dir::Down,
        }
    }

    /// 0 when equal, 1 when perpendicular, 2 when opposite.
    pub fn distance(self, other: Dir) -> u32 {
        if self == other {
            0
        } else if self == other.opposite() {
            2
        } else {
            1
        }
    }
}

/// An axis-aligned rectangle (inclusive), y down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub fn from_points(points: impl IntoIterator<Item = Point>) -> Option<Rect> {
        let mut it = points.into_iter();
        let first = it.next()?;
        let mut r = Rect { min: first, max: first };
        for p in it {
            r.min = Point::new(r.min.x.min(p.x), r.min.y.min(p.y));
            r.max = Point::new(r.max.x.max(p.x), r.max.y.max(p.y));
        }
        Some(r)
    }

    pub fn union(self, other: Rect) -> Rect {
        Rect::from_points([self.min, self.max, other.min, other.max]).expect("non-empty")
    }

    pub fn inflate(self, by: Nm) -> Rect {
        Rect {
            min: Point::new(self.min.x - by, self.min.y - by),
            max: Point::new(self.max.x + by, self.max.y + by),
        }
    }

    pub fn translate(self, by: Point) -> Rect {
        Rect {
            min: self.min + by,
            max: self.max + by,
        }
    }

    /// The rectangle after the symbol transform `o` (around the symbol origin).
    pub fn transformed(self, o: Orientation) -> Rect {
        let corners = [
            self.min,
            self.max,
            Point::new(self.min.x, self.max.y),
            Point::new(self.max.x, self.min.y),
        ];
        Rect::from_points(corners.into_iter().map(|c| o.apply(c).unwrap_or(c))).expect("four corners")
    }

    pub fn width(self) -> Nm {
        self.max.x - self.min.x
    }

    pub fn height(self) -> Nm {
        self.max.y - self.min.y
    }

    /// True when the rectangles share interior area (touching edges do not count).
    pub fn overlaps(self, other: Rect) -> bool {
        self.min.x < other.max.x && other.min.x < self.max.x && self.min.y < other.max.y && other.min.y < self.max.y
    }

    /// True when the axis-aligned segment a–b passes through the rectangle's interior.
    pub fn crossed_by(self, a: Point, b: Point) -> bool {
        let seg = Rect::from_points([a, b]).expect("two points");
        if a.x == b.x {
            a.x > self.min.x && a.x < self.max.x && seg.min.y < self.max.y && seg.max.y > self.min.y
        } else {
            a.y > self.min.y && a.y < self.max.y && seg.min.x < self.max.x && seg.max.x > self.min.x
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unapply_undoes_apply_for_every_orientation() {
        let local = Point::new(Nm(3_810_000), Nm(-1_270_000));
        for rotation_deg in [0, 90, 180, 270] {
            for (mirror_x, mirror_y) in [(false, false), (true, false), (false, true), (true, true)] {
                let o = Orientation {
                    rotation_deg,
                    mirror_x,
                    mirror_y,
                };
                assert_eq!(o.unapply(o.apply(local).unwrap()), Some(local), "{o:?}");
            }
        }
        assert_eq!(
            Orientation {
                rotation_deg: 45,
                ..Default::default()
            }
            .unapply(local),
            None
        );
    }

    use super::*;

    #[test]
    fn mm_conversion_is_exact_on_grid() {
        assert_eq!(Nm::from_mm(1.27).unwrap(), GRID);
        assert_eq!(Nm::from_mm(54.61).unwrap(), Nm(54_610_000));
        assert!(Nm::from_mm(54.61).unwrap().is_multiple_of(GRID));
        assert!(Nm::from_mm(f64::NAN).is_err());
        assert!(Nm::from_mm(10_000.1).is_err());
    }

    #[test]
    fn grid_checks_and_snapping() {
        let on = Point::from_mm(50.8, 46.99).unwrap();
        assert!(on.is_on_grid(GRID));
        let off = Point::from_mm(50.0, 47.0).unwrap();
        assert!(!off.is_on_grid(GRID));
        assert_eq!(off.snap(GRID), Point::from_mm(49.53, 46.99).unwrap());
        assert!(matches!(off.require_on_grid(GRID), Err(GeometryError::OffGrid { .. })));
        assert_eq!(Nm(-1_900_000).snap(GRID), Nm(-1_270_000));
    }

    #[test]
    fn orientation_identity_matches_observed_resistor_pins() {
        // Device:R placed at (50.8, 50.8) with no rotation: pins at local y = ∓3.81 mm,
        // observed connected at (50.8, 46.99) and (50.8, 54.61) on the nightly.
        let origin = Point::from_mm(50.8, 50.8).unwrap();
        let pin1 = Orientation::default()
            .apply(Point::from_mm(0.0, -3.81).unwrap())
            .unwrap();
        assert_eq!(origin + pin1, Point::from_mm(50.8, 46.99).unwrap());
    }

    #[test]
    fn quarter_turns_compose_to_identity() {
        let p = Point::from_mm(1.27, -3.81).unwrap();
        let mut q = p;
        for _ in 0..4 {
            q = Orientation {
                rotation_deg: 90,
                ..Default::default()
            }
            .apply(q)
            .unwrap();
        }
        assert_eq!(q, p);
        assert!(Orientation {
            rotation_deg: 45,
            ..Default::default()
        }
        .apply(p)
        .is_none());
    }
}

#[cfg(test)]
mod dir_rect_tests {
    use super::*;

    #[test]
    fn directions_follow_the_transform() {
        let r90 = Orientation {
            rotation_deg: 90,
            ..Default::default()
        };
        // A quarter turn counter-clockwise on screen: up becomes left, right becomes up.
        assert_eq!(Dir::Up.transformed(r90), Dir::Left);
        assert_eq!(Dir::Right.transformed(r90), Dir::Up);
        let mirror_y = Orientation {
            mirror_y: true,
            ..Default::default()
        };
        assert_eq!(Dir::Left.transformed(mirror_y), Dir::Right);
        assert_eq!(Dir::Up.distance(Dir::Down), 2);
    }

    #[test]
    fn rect_checks() {
        let body =
            Rect::from_points([Point::from_mm(-1.0, -2.54).unwrap(), Point::from_mm(1.0, 2.54).unwrap()]).unwrap();
        let wire_through = (Point::from_mm(-5.08, 0.0).unwrap(), Point::from_mm(5.08, 0.0).unwrap());
        assert!(body.crossed_by(wire_through.0, wire_through.1));
        let wire_beside = (
            Point::from_mm(-5.08, 3.81).unwrap(),
            Point::from_mm(5.08, 3.81).unwrap(),
        );
        assert!(!body.crossed_by(wire_beside.0, wire_beside.1));
        let rotated = body.transformed(Orientation {
            rotation_deg: 90,
            ..Default::default()
        });
        assert_eq!((rotated.width(), rotated.height()), (body.height(), body.width()));
        assert!(body.overlaps(rotated));
    }
}
