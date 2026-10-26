// SPDX-License-Identifier: GPL-3.0-or-later
//! A neutral description of a circuit board for mechanical design (enclosures, mounting, panels):
//! the board's outline and thickness, mounting holes, keep-out areas, connectors and which edge
//! they face, and how tall each part is. Millimetres, with x to the right and y down from the
//! origin of the board file, as in the PCB editor. No EDA types: a tool that designs an enclosure
//! reads this and never needs KiCad.
//!
//! The format is versioned by [`FORMAT`]; fields are only added within a version.

use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "thingsvu.mechanical/1";

/// An axis-aligned box.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Box2 {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Box2 {
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn center(&self) -> [f64; 2] {
        [(self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutlineShape {
    /// A line, or an arc given by its ends.
    Segment { start: [f64; 2], end: [f64; 2] },
    /// A rectangle, circle, or polygon, by its box.
    Closed { bounds: Box2 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardBody {
    /// The box around the outline.
    pub bounds: Option<Box2>,
    pub thickness_mm: Option<f64>,
    /// The outline's shapes, as drawn.
    pub outline: Vec<OutlineShape>,
    pub outline_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountingHole {
    /// The footprint the hole belongs to.
    pub reference: Option<String>,
    pub at: [f64; 2],
    pub diameter_mm: f64,
    pub plated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepoutArea {
    pub name: String,
    pub layers: Vec<String>,
    pub polygon: Vec<[f64; 2]>,
    /// What may not be placed there: `tracks`, `vias`, `pads`, `copperpour`, `footprints`.
    pub forbids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where a part's height comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightSource {
    /// A `Height` field on the footprint or part.
    Field,
    /// A typical height for the package named in the footprint; check the datasheet.
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub side: Side,
    pub at: [f64; 2],
    pub rotation_deg: f64,
    /// The courtyard on the board.
    pub bounds: Option<Box2>,
    /// Above the board surface on its side.
    pub height_mm: Option<f64>,
    pub height_source: HeightSource,
    /// The 3D model the footprint names.
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connector {
    pub reference: String,
    pub value: String,
    pub side: Side,
    pub at: [f64; 2],
    pub rotation_deg: f64,
    pub bounds: Option<Box2>,
    /// The board edge it sits at (within 3 mm), which its opening most likely faces; none for a
    /// connector inside the board, such as a header.
    pub edge: Option<Edge>,
    /// From its courtyard to that edge; negative when it hangs over.
    pub distance_to_edge_mm: Option<f64>,
    pub height_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tallest {
    pub reference: String,
    pub height_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MechanicalModel {
    pub format: String,
    /// The board file this was read from.
    pub source: String,
    pub board: BoardBody,
    pub mounting_holes: Vec<MountingHole>,
    pub keepouts: Vec<KeepoutArea>,
    pub connectors: Vec<Connector>,
    pub components: Vec<Component>,
    /// The tallest part on each side, of those with a known or estimated height.
    pub tallest_top: Option<Tallest>,
    pub tallest_bottom: Option<Tallest>,
    /// A STEP export of the board, when one was written.
    pub step_path: Option<String>,
}

impl MechanicalModel {
    /// The tallest part on `side`.
    pub fn tallest(components: &[Component], side: Side) -> Option<Tallest> {
        components
            .iter()
            .filter(|c| c.side == side)
            .filter_map(|c| Some((c, c.height_mm?)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(c, h)| Tallest {
                reference: c.reference.clone(),
                height_mm: h,
            })
    }
}

/// The edge of `board` nearest to `part` and the gap between them (negative when `part` reaches
/// past it).
pub fn nearest_edge(board: &Box2, part: &Box2) -> (Edge, f64) {
    [
        (Edge::Left, part.min_x - board.min_x),
        (Edge::Right, board.max_x - part.max_x),
        (Edge::Top, part.min_y - board.min_y),
        (Edge::Bottom, board.max_y - part.max_y),
    ]
    .into_iter()
    .min_by(|a, b| a.1.total_cmp(&b.1))
    .expect("four edges")
}

/// A typical height above the board for a package named in a footprint, in millimetres, taken
/// high so an enclosure has room. None when the name says too little.
pub fn typical_height(footprint: &str) -> Option<f64> {
    let name = footprint.rsplit(':').next().unwrap_or(footprint);
    let has = |s: &str| name.contains(s);
    // Most specific first.
    let table: &[(&[&str], f64)] = &[
        (&["PinHeader", "PinSocket"], 8.5),
        (&["USB_C"], 3.3),
        (&["USB_Micro"], 3.0),
        (&["USB_A"], 7.0),
        (&["BarrelJack"], 11.0),
        (&["TerminalBlock"], 10.0),
        (&["JST_XH"], 7.0),
        (&["JST_PH"], 6.0),
        (&["JST_SH"], 3.0),
        (&["SW_SPST", "SW_Push"], 5.0),
        (&["TO-220"], 16.0),
        (&["TO-252", "DPAK"], 2.4),
        (&["SOT-223"], 1.8),
        (&["SOT-23", "SOT-323", "SOT-363", "SC-70"], 1.2),
        (&["SOIC", "SOP-"], 1.75),
        (&["TSSOP", "MSOP", "SSOP"], 1.2),
        (&["QFN", "DFN"], 1.0),
        (&["LQFP", "TQFP", "QFP"], 1.6),
        (&["D_SMA", "D_SMB", "D_SMC"], 2.5),
        (&["SOD-123", "SOD-323"], 1.35),
        (&["Crystal_SMD"], 1.2),
        (&["ESP32", "RF_Module"], 3.3),
        (&["0201"], 0.3),
        (&["0402"], 0.5),
        (&["0603"], 0.9),
        (&["0805"], 1.3),
        (&["1206"], 1.6),
        (&["1210"], 2.5),
    ];
    table
        .iter()
        .find(|(keys, _)| keys.iter().any(|k| has(k)))
        .map(|(_, h)| *h)
}

/// A height written in a field: `3.1`, `3.1mm`, `3.1 mm`.
pub fn parse_height(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches("mm").trim();
    t.parse::<f64>().ok().filter(|h| *h > 0.0 && *h < 500.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_edge_and_overhang() {
        let board = Box2 {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 50.0,
            max_y: 30.0,
        };
        let usb = Box2 {
            min_x: 20.0,
            min_y: -1.0,
            max_x: 29.0,
            max_y: 6.0,
        };
        assert_eq!(nearest_edge(&board, &usb), (Edge::Top, -1.0));
        let jack = Box2 {
            min_x: 45.0,
            min_y: 10.0,
            max_x: 49.0,
            max_y: 18.0,
        };
        assert_eq!(nearest_edge(&board, &jack), (Edge::Right, 1.0));
    }

    #[test]
    fn heights_from_names_and_fields() {
        assert_eq!(typical_height("Capacitor_SMD:C_0603_1608Metric"), Some(0.9));
        assert_eq!(
            typical_height("Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical"),
            Some(8.5)
        );
        assert_eq!(typical_height("Package_TO_SOT_SMD:SOT-223-3_TabPin2"), Some(1.8));
        assert_eq!(typical_height("MyLib:Thing"), None);
        assert_eq!(parse_height("3.1 mm"), Some(3.1));
        assert_eq!(parse_height("tall"), None);
    }

    #[test]
    fn the_tallest_part_per_side() {
        let part = |r: &str, side, h| Component {
            reference: r.into(),
            value: String::new(),
            footprint: String::new(),
            side,
            at: [0.0, 0.0],
            rotation_deg: 0.0,
            bounds: None,
            height_mm: h,
            height_source: HeightSource::Estimated,
            model: None,
        };
        let parts = [
            part("C1", Side::Top, Some(0.9)),
            part("J1", Side::Top, Some(8.5)),
            part("U2", Side::Bottom, Some(1.2)),
            part("X1", Side::Top, None),
        ];
        assert_eq!(MechanicalModel::tallest(&parts, Side::Top).unwrap().reference, "J1");
        assert_eq!(MechanicalModel::tallest(&parts, Side::Bottom).unwrap().height_mm, 1.2);
    }
}
