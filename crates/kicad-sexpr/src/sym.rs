// SPDX-License-Identifier: GPL-3.0-or-later
//! Symbol definitions: from a `.kicad_sym` library or from the `lib_symbols` of a schematic.
//! Library files use y-up symbol coordinates; the domain model uses y-down (as KiCad's API
//! reports them), so every point is flipped on the way in.

use kicad_domain::plan::{LibraryPin, LibrarySymbol};
use kicad_domain::{Dir, LibraryId, Nm, Point, Rect};

use crate::node::{parse, Node, ParseError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SymError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("symbol '{0}' is not in this library")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

/// A parsed `.kicad_sym` file.
pub struct SymbolLibrary {
    root: Node,
}

impl SymbolLibrary {
    pub fn parse(text: &str) -> Result<Self, SymError> {
        let root = parse(text)?;
        if root.head() != Some("kicad_symbol_lib") {
            return Err(SymError::Invalid(
                "not a symbol library: expected (kicad_symbol_lib …)".into(),
            ));
        }
        Ok(Self { root })
    }

    pub fn entry(&self, name: &str) -> Option<&Node> {
        self.root.children("symbol").find(|s| s.arg_text(0) == Some(name))
    }

    /// The symbol as a schematic embeds it: named `nickname:name`, with a derived symbol
    /// (`extends`) merged into its parent.
    pub fn embeddable(&self, nickname: &str, name: &str) -> Result<Node, SymError> {
        let mut symbol = self.flatten(name, 0)?;
        if let crate::node::Kind::List(items) = &mut symbol.kind {
            items[1] = Node::string(format!("{nickname}:{name}"));
        }
        Ok(symbol)
    }

    fn flatten(&self, name: &str, depth: u8) -> Result<Node, SymError> {
        let sym = self.entry(name).ok_or_else(|| SymError::NotFound(name.to_owned()))?;
        let Some(parent_name) = sym.child_text("extends") else {
            return Ok(sym.clone());
        };
        if depth > 8 {
            return Err(SymError::Invalid(format!("'{name}' extends too many symbols")));
        }
        let parent = self.flatten(parent_name, depth + 1)?;
        let own_units: Vec<&Node> = sym.children("symbol").collect();
        let mut head = vec![Node::atom("symbol"), Node::string(name)];
        let mut units = Vec::new();
        for item in parent.items().iter().skip(2) {
            match item.head() {
                Some("property") => {
                    let own = sym.children("property").find(|p| p.arg_text(0) == item.arg_text(0));
                    head.push(own.unwrap_or(item).clone());
                }
                Some("symbol") => {
                    if own_units.is_empty() {
                        units.push(rename_unit(item, parent_name, name));
                    }
                }
                Some(other) => head.push(sym.child(other).unwrap_or(item).clone()),
                None => head.push(item.clone()),
            }
        }
        for p in sym.children("property") {
            if !parent.children("property").any(|q| q.arg_text(0) == p.arg_text(0)) {
                head.push(p.clone());
            }
        }
        for item in sym.items().iter().skip(2) {
            let is_new_field = !matches!(item.head(), Some("extends" | "property" | "symbol"))
                && parent.child(item.head().unwrap_or_default()).is_none();
            if is_new_field {
                head.push(item.clone());
            }
        }
        head.extend(units);
        head.extend(own_units.into_iter().cloned());
        Ok(Node::list(head))
    }
}

/// `(symbol "<parent>_1_1" …)` becomes `(symbol "<child>_1_1" …)`.
fn rename_unit(unit: &Node, parent: &str, child: &str) -> Node {
    let mut unit = unit.clone();
    if let crate::node::Kind::List(items) = &mut unit.kind {
        if let Some(name) = items.get(1).and_then(Node::text).and_then(|n| n.strip_prefix(parent)) {
            items[1] = Node::string(format!("{child}{name}"));
        }
    }
    unit
}

fn flipped(x: f64, y: f64) -> Point {
    Point::new(Nm((x * 1e6).round() as i64), Nm((-y * 1e6).round() as i64))
}

fn xy(node: &Node) -> Option<Point> {
    Some(flipped(node.arg_f64(0)?, node.arg_f64(1)?))
}

/// The unit and body style of a sub-symbol named `<name>_<unit>_<style>`.
fn unit_and_style(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.rsplitn(3, '_');
    let style = parts.next()?.parse().ok()?;
    let unit = parts.next()?.parse().ok()?;
    Some((unit, style))
}

/// The direction a pin points away from the body, in sheet axes. Library angles say where the pin
/// line runs from its connection point toward the body, in y-up axes.
fn outward(angle: f64) -> Dir {
    match (angle.rem_euclid(360.0) / 90.0).round() as i32 % 4 {
        0 => Dir::Left,
        1 => Dir::Down,
        2 => Dir::Right,
        _ => Dir::Up,
    }
}

/// Pin geometry, reference prefix and body box of a flattened symbol.
pub fn describe(id: &LibraryId, symbol: &Node) -> LibrarySymbol {
    let mut pins = Vec::new();
    let mut outline: Vec<Point> = Vec::new();
    let mut unit_count = 1;
    for sub in symbol.children("symbol") {
        let Some((unit, style)) = sub.arg_text(0).and_then(unit_and_style) else {
            continue;
        };
        unit_count = unit_count.max(unit);
        // Alternate (De Morgan) bodies repeat the pins of the normal one.
        if style > 1 {
            continue;
        }
        for item in sub.items() {
            match item.head() {
                Some("pin") => {
                    let Some(at) = item.child("at") else { continue };
                    let (Some(x), Some(y)) = (at.arg_f64(0), at.arg_f64(1)) else {
                        continue;
                    };
                    pins.push(LibraryPin {
                        number: item
                            .child("number")
                            .and_then(|n| n.arg_text(0))
                            .unwrap_or_default()
                            .to_owned(),
                        name: item
                            .child("name")
                            .and_then(|n| n.arg_text(0))
                            .unwrap_or_default()
                            .to_owned(),
                        local: flipped(x, y),
                        unit,
                        outward: outward(at.arg_f64(2).unwrap_or(0.0)),
                        electrical_type: item.arg_text(0).unwrap_or("unspecified").to_owned(),
                    });
                }
                Some("rectangle") => {
                    outline.extend(item.child("start").and_then(xy));
                    outline.extend(item.child("end").and_then(xy));
                }
                Some("polyline" | "bezier") => {
                    if let Some(pts) = item.child("pts") {
                        outline.extend(pts.children("xy").filter_map(xy));
                    }
                }
                Some("arc") => {
                    for key in ["start", "mid", "end"] {
                        outline.extend(item.child(key).and_then(xy));
                    }
                }
                Some("circle") => {
                    if let (Some(c), Some(r)) = (
                        item.child("center").and_then(xy),
                        item.child("radius").and_then(|r| r.arg_f64(0)),
                    ) {
                        let r = Nm((r * 1e6).round() as i64);
                        outline.push(Point::new(c.x - r, c.y - r));
                        outline.push(Point::new(c.x + r, c.y + r));
                    }
                }
                _ => {}
            }
        }
    }
    let prefix = property(symbol, "Reference")
        .map(|v| v.trim_end_matches(['?', '*']).to_owned())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "U".into());
    LibrarySymbol {
        library_id: id.clone(),
        reference_prefix: prefix,
        unit_count,
        is_power: symbol.child("power").is_some(),
        body: Rect::from_points(outline),
        pins,
    }
}

/// The value of a symbol property, e.g. `Reference`.
pub fn property<'a>(symbol: &'a Node, name: &str) -> Option<&'a str> {
    symbol
        .children("property")
        .find(|p| p.arg_text(0) == Some(name))
        .and_then(|p| p.arg_text(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = r##"(kicad_symbol_lib (version 20251024)
	(symbol "R" (property "Reference" "R" (at 2 0 90)) (property "Value" "R" (at 0 0 0))
		(symbol "R_0_1" (rectangle (start -1.016 -2.54) (end 1.016 2.54)))
		(symbol "R_1_1"
			(pin passive line (at 0 3.81 270) (length 1.27) (name "~") (number "1"))
			(pin passive line (at 0 -3.81 90) (length 1.27) (name "~") (number "2"))))
	(symbol "R_Small" (extends "R") (property "Value" "R_Small" (at 1 1 0)) (property "Tolerance" "1%" (at 0 0 0)))
	(symbol "GND" (power) (property "Reference" "#PWR" (at 0 0 0))
		(symbol "GND_1_1" (pin power_in line (at 0 0 270) (length 0) (name "GND") (number "1")))))"##;

    #[test]
    fn describes_pins_and_body_in_sheet_axes() {
        let lib = SymbolLibrary::parse(LIB).unwrap();
        let id = LibraryId::parse("Device:R").unwrap();
        let sym = describe(&id, &lib.embeddable("Device", "R").unwrap());
        assert_eq!(sym.reference_prefix, "R");
        assert_eq!(sym.pins.len(), 2);
        // Pin 1 is at the top of the drawing: y is negative in sheet axes.
        assert_eq!(sym.pins[0].local, Point::new(Nm(0), Nm(-3_810_000)));
        assert_eq!(sym.pins[0].outward, Dir::Up);
        assert_eq!(sym.pins[1].outward, Dir::Down);
        let body = sym.body.unwrap();
        assert_eq!((body.min.y, body.max.y), (Nm(-2_540_000), Nm(2_540_000)));
        assert!(!sym.is_power);
    }

    #[test]
    fn power_symbols_are_flagged() {
        let lib = SymbolLibrary::parse(LIB).unwrap();
        let sym = describe(
            &LibraryId::parse("power:GND").unwrap(),
            &lib.embeddable("power", "GND").unwrap(),
        );
        assert!(sym.is_power);
        assert_eq!(sym.reference_prefix, "#PWR");
        assert_eq!(sym.pins[0].electrical_type, "power_in");
    }

    #[test]
    fn derived_symbols_are_merged_into_their_parent() {
        let lib = SymbolLibrary::parse(LIB).unwrap();
        let flat = lib.embeddable("Device", "R_Small").unwrap();
        assert_eq!(flat.arg_text(0), Some("Device:R_Small"));
        assert!(flat.child("extends").is_none());
        assert_eq!(property(&flat, "Value"), Some("R_Small"));
        assert_eq!(property(&flat, "Reference"), Some("R"));
        assert_eq!(property(&flat, "Tolerance"), Some("1%"));
        let units: Vec<_> = flat.children("symbol").filter_map(|s| s.arg_text(0)).collect();
        assert_eq!(units, ["R_Small_0_1", "R_Small_1_1"]);
        let sym = describe(&LibraryId::parse("Device:R_Small").unwrap(), &flat);
        assert_eq!(sym.pins.len(), 2);
    }

    #[test]
    fn unknown_symbols_are_reported() {
        let lib = SymbolLibrary::parse(LIB).unwrap();
        assert_eq!(lib.embeddable("Device", "Nope"), Err(SymError::NotFound("Nope".into())));
    }
}
