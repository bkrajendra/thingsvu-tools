// SPDX-License-Identifier: GPL-3.0-or-later
//! What the read tools report about a schematic. Coordinates are in millimetres for clients.

use serde::Serialize;

use crate::geometry::{Nm, Orientation, Point};
use crate::ids::Kiid;

/// A point in millimetres, as reported to clients.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PointMm {
    pub x: f64,
    pub y: f64,
}

impl From<Point> for PointMm {
    fn from(p: Point) -> Self {
        Self {
            x: p.x.to_mm(),
            y: p.y.to_mm(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolInfo {
    pub id: Kiid,
    pub reference: String,
    pub value: String,
    pub library_id: String,
    pub footprint: String,
    pub position_mm: PointMm,
    pub orientation: Orientation,
    pub unit: u32,
    /// Human-readable sheet path, e.g. `/`.
    pub sheet_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PinInfo {
    pub id: Kiid,
    pub number: String,
    pub name: String,
    /// KiCad electrical type, e.g. `passive`, `input`, `power_in`.
    pub electrical_type: String,
    /// Connection point on the sheet.
    pub position_mm: PointMm,
    /// Connection point relative to the symbol origin, before the symbol's transform.
    pub local_mm: PointMm,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolPins {
    pub symbol: SymbolInfo,
    pub pins: Vec<PinInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct NetPin {
    pub reference: String,
    pub pin: String,
    pub symbol_id: Kiid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetInfo {
    pub name: String,
    pub pins: Vec<NetPin>,
    /// Wires, labels, and other connected items that are not symbol pins.
    pub other_items: usize,
}

/// Where a pin of a placed symbol lands on the sheet.
pub fn pin_world_position(symbol_position: Point, orientation: Orientation, local: Point) -> Option<Point> {
    Some(symbol_position + orientation.apply(local)?)
}

/// Converts a KiCad nanometre pair to a point.
pub fn point_nm(x_nm: i64, y_nm: i64) -> Point {
    Point::new(Nm(x_nm), Nm(y_nm))
}
