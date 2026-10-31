// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a `.kicad_sch` file: what is placed, and which symbol definitions it carries.

use kicad_domain::schematic::{DesignField, DesignShape, DesignText, ShapeKind};
use kicad_domain::text::{HAlign, VAlign};
use kicad_domain::{
    DesignJunction, DesignLabel, DesignNoConnect, DesignSymbol, DesignWire, Dir, Nm, Orientation, Point,
    SchematicDesign,
};

use crate::node::{parse, Node, ParseError};

/// Oldest schematic format the writer targets: KiCad 10.0 (`generator_version "10.0"`).
pub const MIN_VERSION: u32 = 20260306;
/// Newest: the KiCad 11 nightly pinned in docs/kicad-api-compatibility.md.
pub const MAX_VERSION: u32 = 20261015;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("not a schematic: expected (kicad_sch …)")]
    NotASchematic,
    #[error(
        "the file's format version {found} is older than {MIN_VERSION} (KiCad 10.0); open and save it in \
         KiCad 10 or run `kicad-cli sch upgrade` on it first"
    )]
    TooOld { found: u32 },
    #[error(
        "the file's format version {found} is newer than {MAX_VERSION}, which this tool has been checked \
         against"
    )]
    TooNew { found: u32 },
    #[error("the file has no uuid or lib_symbols section")]
    Incomplete,
}

/// A symbol instance on the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedSymbol {
    pub uuid: String,
    /// Key of the definition in the file's `lib_symbols` (`lib_name` when set, else `lib_id`).
    pub definition: String,
    pub library_id: String,
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub position: Point,
    pub orientation: Orientation,
    pub unit: u32,
    /// Pin number → the pin's own uuid.
    pub pins: Vec<(String, String)>,
}

/// A parsed schematic with its original text.
pub struct Document {
    text: String,
    root: Node,
    version: u32,
}

impl Document {
    pub fn parse(text: String) -> Result<Self, DocError> {
        let root = parse(&text)?;
        if root.head() != Some("kicad_sch") {
            return Err(DocError::NotASchematic);
        }
        let version = root
            .child_text("version")
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or(DocError::NotASchematic)?;
        if version < MIN_VERSION {
            return Err(DocError::TooOld { found: version });
        }
        if version > MAX_VERSION {
            return Err(DocError::TooNew { found: version });
        }
        if root.child_text("uuid").is_none() || root.child("lib_symbols").is_none() {
            return Err(DocError::Incomplete);
        }
        Ok(Self { text, root, version })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn root(&self) -> &Node {
        &self.root
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// The sheet's own uuid, which instance paths start from.
    pub fn uuid(&self) -> &str {
        self.root.child_text("uuid").unwrap_or_default()
    }

    /// Symbol definitions the file carries, by `library:name`.
    pub fn embedded(&self, id: &str) -> Option<&Node> {
        self.root
            .child("lib_symbols")?
            .children("symbol")
            .find(|s| s.arg_text(0) == Some(id))
    }

    /// The project name instances are recorded under, taken from a symbol already placed.
    pub fn project_name(&self) -> Option<&str> {
        self.root
            .children("symbol")
            .filter_map(|s| s.child("instances")?.child("project")?.arg_text(0))
            .next()
    }

    pub fn symbols(&self) -> Vec<PlacedSymbol> {
        self.root.children("symbol").filter_map(placed).collect()
    }

    /// The top-level item with this head (`wire`, `label`, `junction`, `symbol`) and uuid.
    pub fn item(&self, head: &str, uuid: &str) -> Option<&Node> {
        self.root
            .items()
            .iter()
            .filter(|n| n.head() == Some(head))
            .find(|n| n.child_text("uuid").is_some_and(|u| u.eq_ignore_ascii_case(uuid)))
    }

    /// Everything on the sheet that the review and tidy tools look at.
    pub fn design(&self) -> SchematicDesign {
        let mut design = SchematicDesign::default();
        for s in self.symbols() {
            let fields = self
                .item("symbol", &s.uuid)
                .map(|node| {
                    ["Reference", "Value"]
                        .into_iter()
                        .filter_map(|name| {
                            let property = node.children("property").find(|p| p.arg_text(0) == Some(name))?;
                            let at = property.child("at")?;
                            let look = look(property);
                            Some(DesignField {
                                name: name.to_owned(),
                                text: property.arg_text(1).unwrap_or_default().to_owned(),
                                position: Point::new(
                                    Nm((at.arg_f64(0)? * 1e6).round() as i64),
                                    Nm((at.arg_f64(1)? * 1e6).round() as i64),
                                ),
                                angle_deg: angle(at.arg_f64(2)),
                                size: look.size,
                                hidden: look.hidden,
                                h: look.h,
                                v: look.v,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let node = self.item("symbol", &s.uuid);
            let properties: std::collections::BTreeMap<String, String> = node
                .map(|n| {
                    n.children("property")
                        .filter_map(|p| Some((p.arg_text(0)?.to_owned(), p.arg_text(1).unwrap_or_default().to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            let flag = |name: &str, on: &str| node.is_some_and(|n| n.child_text(name) == Some(on));
            design.symbols.push(
                DesignSymbol {
                    id: s.uuid,
                    reference: s.reference,
                    value: s.value,
                    library_id: s.library_id,
                    footprint: s.footprint,
                    position: s.position,
                    orientation: s.orientation,
                    unit: s.unit,
                    fields,
                    properties,
                    dnp: flag("dnp", "yes"),
                    exclude_from_bom: flag("in_bom", "no"),
                    exclude_from_board: flag("on_board", "no"),
                }
                .with_drawn_angles(),
            );
        }
        let point = |n: &Node| {
            Some(Point::new(
                Nm((n.arg_f64(0)? * 1e6).round() as i64),
                Nm((n.arg_f64(1)? * 1e6).round() as i64),
            ))
        };
        for item in self.root.items() {
            let Some(head) = item.head() else { continue };
            let id = item.child_text("uuid").unwrap_or_default().to_owned();
            match head {
                "wire" => {
                    let ends: Vec<Point> = item
                        .child("pts")
                        .map(|p| p.children("xy").filter_map(point).collect())
                        .unwrap_or_default();
                    if let [start, end] = ends[..] {
                        design.wires.push(DesignWire { id, start, end });
                    }
                }
                "label" => {
                    if let (Some(text), Some(position)) = (item.arg_text(0), item.child("at").and_then(point)) {
                        design.labels.push(DesignLabel {
                            id,
                            text: text.to_owned(),
                            position,
                            direction: match angle(item.child("at").and_then(|a| a.arg_f64(2))) {
                                90 => Dir::Up,
                                180 => Dir::Left,
                                270 => Dir::Down,
                                _ => Dir::Right,
                            },
                        });
                    }
                }
                "junction" => {
                    if let Some(position) = item.child("at").and_then(point) {
                        design.junctions.push(DesignJunction { id, position });
                    }
                }
                "no_connect" => {
                    if let Some(position) = item.child("at").and_then(point) {
                        design.no_connects.push(DesignNoConnect { id, position });
                    }
                }
                // Things a person drew with and a tidy cannot redraw around.
                "bus" | "bus_entry" | "sheet" | "global_label" | "hierarchical_label" | "directive_label"
                | "netclass_flag" | "rule_area" | "table" | "group" => {
                    *design.unsupported.entry(head.to_owned()).or_default() += 1;
                }
                // Notes and graphics stay where they are.
                "text" | "text_box" | "polyline" | "rectangle" | "circle" | "arc" | "bezier" | "image" => {
                    design.annotations += 1;
                    match head {
                        "text" => {
                            if let (Some(text), Some(at)) = (item.arg_text(0), item.child("at").and_then(point)) {
                                let look = look(item);
                                let note = DesignText {
                                    id,
                                    text: text.to_owned(),
                                    position: at,
                                    size: look.size,
                                    angle_deg: angle(item.child("at").and_then(|a| a.arg_f64(2))),
                                    h: look.h,
                                    v: look.v,
                                };
                                design.notes.push(note.bounds());
                                design.texts.push(note);
                            }
                        }
                        "rectangle" | "circle" | "polyline" => {
                            let kind = match head {
                                "rectangle" => item
                                    .child("start")
                                    .and_then(point)
                                    .zip(item.child("end").and_then(point))
                                    .map(|(a, b)| ShapeKind::Rectangle {
                                        min: Point::new(a.x.min(b.x), a.y.min(b.y)),
                                        max: Point::new(a.x.max(b.x), a.y.max(b.y)),
                                    }),
                                "circle" => item.child("center").and_then(point).zip(item.child_f64("radius")).map(
                                    |(center, r)| ShapeKind::Circle {
                                        center,
                                        radius: Nm((r * 1e6).round() as i64),
                                    },
                                ),
                                _ => {
                                    let ends: Vec<Point> = item
                                        .child("pts")
                                        .map(|p| p.children("xy").filter_map(point).collect())
                                        .unwrap_or_default();
                                    match ends[..] {
                                        [start, end] => Some(ShapeKind::Line { start, end }),
                                        _ => None,
                                    }
                                }
                            };
                            if let Some(kind) = kind {
                                design.shapes.push(DesignShape {
                                    id,
                                    kind,
                                    stroke: Nm((item.child("stroke").and_then(|s| s.child_f64("width")).unwrap_or(0.0)
                                        * 1e6)
                                        .round() as i64),
                                    filled: item
                                        .child("fill")
                                        .and_then(|f| f.child_text("type"))
                                        .is_some_and(|t| t != "none"),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        design
    }
}

/// A text angle as a whole number of degrees in 0..360.
fn angle(a: Option<f64>) -> u16 {
    a.unwrap_or(0.0).rem_euclid(360.0).round() as u16 % 360
}

/// How a node's `effects` set its text: size, justification, and whether it is hidden.
struct Look {
    size: Nm,
    h: HAlign,
    v: VAlign,
    hidden: bool,
}

fn look(node: &Node) -> Look {
    let effects = node.child("effects");
    let size = effects
        .and_then(|e| e.child("font"))
        .and_then(|f| f.child("size"))
        .and_then(|s| s.arg_f64(0))
        .map(|mm| Nm((mm * 1e6).round() as i64))
        .unwrap_or(kicad_domain::text::DEFAULT_SIZE);
    let (mut h, mut v) = (HAlign::Center, VAlign::Center);
    if let Some(justify) = effects.and_then(|e| e.child("justify")) {
        for word in justify.items().iter().skip(1).filter_map(|n| n.text()) {
            match word {
                "left" => h = HAlign::Left,
                "right" => h = HAlign::Right,
                "top" => v = VAlign::Top,
                "bottom" => v = VAlign::Bottom,
                _ => {}
            }
        }
    }
    // `(hide yes)` sits on the property in current files, and inside `effects` in older ones.
    let hidden = [Some(node), effects]
        .into_iter()
        .flatten()
        .any(|n| n.child_text("hide") == Some("yes"));
    Look { size, h, v, hidden }
}

fn placed(node: &Node) -> Option<PlacedSymbol> {
    let library_id = node.child_text("lib_id")?.to_owned();
    let at = node.child("at")?;
    let angle = at.arg_f64(2).unwrap_or(0.0).rem_euclid(360.0).round() as u16 % 360;
    let mirror = node.child_text("mirror").unwrap_or_default();
    let property = |name: &str| {
        node.children("property")
            .find(|p| p.arg_text(0) == Some(name))
            .and_then(|p| p.arg_text(1))
            .unwrap_or_default()
            .to_owned()
    };
    Some(PlacedSymbol {
        uuid: node.child_text("uuid")?.to_owned(),
        definition: node.child_text("lib_name").unwrap_or(&library_id).to_owned(),
        library_id,
        reference: property("Reference"),
        value: property("Value"),
        footprint: property("Footprint"),
        position: Point::new(
            Nm((at.arg_f64(0)? * 1e6).round() as i64),
            Nm((at.arg_f64(1)? * 1e6).round() as i64),
        ),
        orientation: Orientation {
            rotation_deg: angle,
            mirror_x: mirror == "x",
            mirror_y: mirror == "y",
        },
        unit: node.child_text("unit").and_then(|u| u.parse().ok()).unwrap_or(1),
        pins: node
            .children("pin")
            .filter_map(|p| Some((p.arg_text(0)?.to_owned(), p.child_text("uuid")?.to_owned())))
            .collect(),
    })
}

/// A new, empty schematic in the format of `version` (for `generator_version`, e.g. `10.0`).
pub fn empty_schematic(version: u32, generator_version: &str) -> String {
    format!(
        "(kicad_sch\n\t(version {version})\n\t(generator \"eeschema\")\n\t(generator_version \"{generator_version}\")\n\
         \t(uuid \"{}\")\n\t(paper \"A4\")\n\t(lib_symbols)\n\t(sheet_instances\n\t\t(path \"/\"\n\t\t\t(page \"1\")\n\t\t)\n\t)\n\
         \t(embedded_fonts no)\n)\n",
        uuid::Uuid::new_v4()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCH: &str = r#"(kicad_sch
	(version 20260306)
	(generator "eeschema")
	(uuid "11111111-1111-1111-1111-111111111111")
	(lib_symbols
		(symbol "Device:R" (property "Reference" "R" (at 0 0 0)))
	)
	(symbol
		(lib_id "Device:R")
		(at 50.8 63.5 90)
		(mirror y)
		(unit 1)
		(uuid "22222222-2222-2222-2222-222222222222")
		(property "Reference" "R7" (at 0 0 0))
		(property "Value" "10k" (at 0 0 0))
		(pin "1" (uuid "33333333-3333-3333-3333-333333333333"))
		(pin "2" (uuid "44444444-4444-4444-4444-444444444444"))
		(instances (project "demo" (path "/11111111-1111-1111-1111-111111111111" (reference "R7") (unit 1))))
	)
)"#;

    #[test]
    fn reads_placed_symbols() {
        let doc = Document::parse(SCH.to_owned()).unwrap();
        assert_eq!(doc.uuid(), "11111111-1111-1111-1111-111111111111");
        assert_eq!(doc.project_name(), Some("demo"));
        assert!(doc.embedded("Device:R").is_some());
        let symbols = doc.symbols();
        assert_eq!(symbols.len(), 1);
        let s = &symbols[0];
        assert_eq!((s.reference.as_str(), s.value.as_str()), ("R7", "10k"));
        assert_eq!(s.position, Point::new(Nm(50_800_000), Nm(63_500_000)));
        assert_eq!(
            s.orientation,
            Orientation {
                rotation_deg: 90,
                mirror_x: false,
                mirror_y: true
            }
        );
        assert_eq!(s.pins.len(), 2);
        assert_eq!(s.definition, "Device:R");
    }

    #[test]
    fn field_text_is_read_as_drawn_on_a_turned_part() {
        // R7 is turned 90 degrees: text stored horizontal is drawn vertical, and the reverse.
        let stored_90 = SCH.replace(
            "(property \"Value\" \"10k\" (at 0 0 0))",
            "(property \"Value\" \"10k\" (at 0 0 90))",
        );
        let design = Document::parse(stored_90).unwrap().design();
        let angle = |name: &str| {
            design.symbols[0]
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .angle_deg
        };
        assert_eq!(angle("Reference"), 90);
        assert_eq!(angle("Value"), 0);
        // The same text on a part that is not turned keeps its angle.
        let upright = SCH.replace("(at 50.8 63.5 90)", "(at 50.8 63.5 0)");
        let design = Document::parse(upright).unwrap().design();
        assert!(design.symbols[0].fields.iter().all(|f| f.angle_deg == 0));
    }

    #[test]
    fn reads_the_whole_design() {
        let text = SCH.replace(
            "\n)",
            r#"
	(wire (pts (xy 10 20) (xy 10 30)) (stroke (width 0) (type default)) (uuid "a1"))
	(junction (at 10 25) (diameter 0) (uuid "a2"))
	(label "MID" (at 10 30 0) (uuid "a3"))
	(global_label "SDA" (at 1 1 0) (uuid "a4"))
	(no_connect (at 2 2) (uuid "a5"))
	(text "note" (at 5 5 0) (uuid "a6"))
)"#,
        );
        let design = Document::parse(text).unwrap().design();
        assert_eq!(design.symbols.len(), 1);
        assert_eq!(design.wires.len(), 1);
        assert_eq!(design.wires[0].end, Point::new(Nm(10_000_000), Nm(30_000_000)));
        assert_eq!(design.labels[0].text, "MID");
        assert_eq!(design.junctions[0].position, Point::new(Nm(10_000_000), Nm(25_000_000)));
        assert_eq!(design.annotations, 1);
        assert_eq!(design.unsupported.get("global_label"), Some(&1));
        assert_eq!(design.no_connects.len(), 1, "a no-connect flag is read, not refused");
        assert_eq!(design.no_connects[0].position, Point::new(Nm(2_000_000), Nm(2_000_000)));
        assert!(!design.unsupported.contains_key("no_connect"));
    }

    #[test]
    fn refuses_formats_it_was_not_checked_against() {
        let old = SCH.replace("20260306", "20250114");
        assert_eq!(Document::parse(old).err(), Some(DocError::TooOld { found: 20250114 }));
        let new = SCH.replace("20260306", "20270101");
        assert_eq!(Document::parse(new).err(), Some(DocError::TooNew { found: 20270101 }));
        assert_eq!(Document::parse("(other)".into()).err(), Some(DocError::NotASchematic));
    }

    #[test]
    fn an_empty_schematic_reads_back() {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        assert!(doc.symbols().is_empty());
        assert_eq!(doc.version(), MIN_VERSION);
    }
}
