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
}

#[cfg(test)]
mod tests {
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
