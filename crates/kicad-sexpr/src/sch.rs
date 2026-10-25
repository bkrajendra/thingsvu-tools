// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a `.kicad_sch` file: what is placed, and which symbol definitions it carries.

use kicad_domain::{Nm, Orientation, Point};

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
