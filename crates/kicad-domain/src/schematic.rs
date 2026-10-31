// SPDX-License-Identifier: GPL-3.0-or-later
//! What the read tools report about a schematic. Coordinates are in millimetres for clients.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::geometry::{Dir, Nm, Orientation, Point, Rect};
use crate::ids::Kiid;
use crate::text::{label_box, text_box, HAlign, VAlign};

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

/// One text field of a placed symbol (its reference or its value) as drawn.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DesignField {
    /// `Reference` or `Value`.
    pub name: String,
    pub text: String,
    /// Where the text is anchored on the sheet.
    pub position: Point,
    /// How the text is turned on the sheet: 0 reads left to right, 90 upward.
    pub angle_deg: u16,
    pub size: Nm,
    pub hidden: bool,
    pub h: HAlign,
    pub v: VAlign,
}

impl DesignField {
    /// The area the text covers, estimated; none for hidden or empty text.
    pub fn bounds(&self) -> Option<Rect> {
        (!self.hidden && !self.text.is_empty())
            .then(|| text_box(&self.text, self.position, self.size, self.angle_deg, self.h, self.v))
    }
}

/// The angle a field is drawn at on the sheet, from the angle stored with it. KiCad stores the
/// angle relative to the symbol: when the symbol is turned a quarter turn (90° or 270°), horizontal
/// text is drawn vertical and the other way round (`SCH_FIELD::GetDrawRotation`). Half turns and
/// mirrors leave it as it is.
pub fn drawn_field_angle(stored_deg: u16, orientation: Orientation) -> u16 {
    if orientation.rotation_deg % 180 != 90 {
        return stored_deg % 360;
    }
    match stored_deg % 180 {
        0 => 90,
        90 => 0,
        other => other,
    }
}

/// A placed symbol, as much of it as planning needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
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
    /// The reference and value text as drawn (`angle_deg` is the angle on the sheet, see
    /// [`DesignSymbol::with_drawn_angles`]).
    pub fields: Vec<DesignField>,
    /// Every field with its text: Reference, Value, Footprint, Datasheet, Description, and any
    /// custom field (manufacturer, part numbers, ...).
    pub properties: BTreeMap<String, String>,
    /// Marked do-not-populate.
    pub dnp: bool,
    /// Left out of the BOM.
    pub exclude_from_bom: bool,
    /// Left off the board.
    pub exclude_from_board: bool,
}

impl DesignSymbol {
    /// Turns each field's stored angle (relative to the symbol) into the angle it is drawn at, using
    /// the symbol's orientation. Readers call this once, so every box and check sees the text as it
    /// appears.
    pub fn with_drawn_angles(mut self) -> Self {
        let orientation = self.orientation;
        for field in &mut self.fields {
            field.angle_deg = drawn_field_angle(field.angle_deg, orientation);
        }
        self
    }
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
    /// The way the text runs from its anchor.
    pub direction: Dir,
}

impl DesignLabel {
    pub fn bounds(&self) -> Rect {
        label_box(&self.text, self.position, crate::text::DEFAULT_SIZE, self.direction)
    }
}

/// A free text note.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DesignText {
    pub id: String,
    pub text: String,
    pub position: Point,
    pub size: Nm,
    pub angle_deg: u16,
    pub h: HAlign,
    pub v: VAlign,
}

impl DesignText {
    pub fn bounds(&self) -> Rect {
        text_box(&self.text, self.position, self.size, self.angle_deg, self.h, self.v)
    }
}

/// A line, rectangle, or circle drawn on the sheet (not a wire).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesignShape {
    pub id: String,
    pub kind: ShapeKind,
    pub stroke: Nm,
    pub filled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    Rectangle { min: Point, max: Point },
    Circle { center: Point, radius: Nm },
    Line { start: Point, end: Point },
}

impl ShapeKind {
    pub fn bounds(&self) -> Rect {
        match *self {
            ShapeKind::Rectangle { min, max } => Rect::from_points([min, max]).expect("two points"),
            ShapeKind::Circle { center, radius } => Rect::from_points([center]).expect("a point").inflate(radius),
            ShapeKind::Line { start, end } => Rect::from_points([start, end]).expect("two points"),
        }
    }
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
    /// Free text notes.
    pub texts: Vec<DesignText>,
    /// Lines, rectangles, and circles drawn on the sheet.
    pub shapes: Vec<DesignShape>,
    /// Text notes, graphics, and images: they are left where they are.
    pub annotations: usize,
    /// The area each text note covers, roughly: a layout keeps clear of these.
    pub notes: Vec<Rect>,
    /// Items that tidying cannot handle, by kind: `no_connect`, `bus`, `sheet`, `global_label`, …
    pub unsupported: BTreeMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turned(rotation_deg: u16, mirror_x: bool) -> Orientation {
        Orientation {
            rotation_deg,
            mirror_x,
            mirror_y: false,
        }
    }

    #[test]
    fn quarter_turns_swap_horizontal_and_vertical_text() {
        assert_eq!(drawn_field_angle(0, turned(90, false)), 90);
        assert_eq!(drawn_field_angle(90, turned(90, false)), 0);
        assert_eq!(drawn_field_angle(0, turned(270, true)), 90);
        assert_eq!(drawn_field_angle(90, turned(270, false)), 0);
    }

    #[test]
    fn half_turns_and_mirrors_leave_the_angle_alone() {
        for (stored, o) in [
            (0, turned(0, false)),
            (90, turned(180, false)),
            (0, turned(0, true)),
            (90, turned(180, true)),
        ] {
            assert_eq!(drawn_field_angle(stored, o), stored);
        }
    }

    #[test]
    fn a_symbol_turns_all_its_fields() {
        let field = |name: &str, angle_deg| DesignField {
            name: name.into(),
            text: "R1".into(),
            angle_deg,
            ..Default::default()
        };
        let symbol = DesignSymbol {
            orientation: turned(90, false),
            fields: vec![field("Reference", 0), field("Value", 90)],
            ..Default::default()
        }
        .with_drawn_angles();
        assert_eq!(symbol.fields.iter().map(|f| f.angle_deg).collect::<Vec<_>>(), [90, 0]);
    }
}
