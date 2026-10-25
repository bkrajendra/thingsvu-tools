// SPDX-License-Identifier: GPL-3.0-or-later
//! What the read tools report about a schematic. Coordinates are in millimetres for clients.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::geometry::{Nm, Orientation, Point, Rect};
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

/// The area a text note covers, roughly: from its anchor, `chars` wide at text size `size`,
/// `lines` high. Generous on purpose, because where the text starts depends on its justification.
pub fn note_area(anchor: Point, text: &str, size: Nm) -> Rect {
    let lines = text.lines().count().max(1) as i64;
    let chars = text.lines().map(|l| l.chars().count()).max().unwrap_or(1).max(1) as i64;
    let width = Nm(chars * size.0 * 6 / 5);
    let height = Nm(lines * size.0 * 3 / 2);
    Rect {
        min: Point::new(anchor.x - Nm(width.0 / 2), anchor.y - height),
        max: Point::new(anchor.x + width, anchor.y + height),
    }
}

/// A placed symbol, as much of it as planning needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesignSymbol {
    /// The symbol's id in the document (its UUID).
    pub id: String,
    pub reference: String,
    pub value: String,
    pub library_id: String,
    pub footprint: String,
    pub position: Point,
    pub orientation: Orientation,
    pub unit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesignWire {
    pub id: String,
    pub start: Point,
    pub end: Point,
}

/// A label that names a net on this sheet only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesignLabel {
    pub id: String,
    pub text: String,
    pub position: Point,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesignJunction {
    pub id: String,
    pub position: Point,
}

/// Everything on a sheet that the review and tidy tools look at, read the same way from a live
/// KiCad or from a file.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SchematicDesign {
    pub symbols: Vec<DesignSymbol>,
    pub wires: Vec<DesignWire>,
    pub labels: Vec<DesignLabel>,
    pub junctions: Vec<DesignJunction>,
    /// Text notes, graphics, and images: they are left where they are.
    pub annotations: usize,
    /// The area each text note covers, roughly: a layout keeps clear of these.
    pub notes: Vec<Rect>,
    /// Items that tidying cannot handle, by kind: `no_connect`, `bus`, `sheet`, `global_label`, …
    pub unsupported: BTreeMap<String, usize>,
}
