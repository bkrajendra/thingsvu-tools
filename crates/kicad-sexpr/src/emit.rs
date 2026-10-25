// SPDX-License-Identifier: GPL-3.0-or-later
//! Adding a plan's items to a schematic file. New text is inserted at two places (the end of
//! `lib_symbols`, and before `sheet_instances`); every other byte of the file stays as it was.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kicad_domain::plan::{Operation, OperationPlan, RemovedKind, SymbolMove};
use kicad_domain::{Dir, LibraryId, Nm, Orientation, Point};

use crate::node::{mm, quote, Node};
use crate::sch::{Document, PlacedSymbol};
use crate::sym::describe;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmitError {
    #[error("no definition for {0}")]
    MissingDefinition(String),
    #[error("a symbol is rotated by something other than a multiple of 90 degrees")]
    BadRotation,
    #[error("the file has no place to add items: {0}")]
    NoInsertPoint(&'static str),
    #[error("the file has no {0} with id {1}")]
    ItemNotFound(&'static str, String),
}

fn atom(s: impl Into<String>) -> Node {
    Node::atom(s)
}

fn item(head: &str, args: Vec<Node>) -> Node {
    let mut items = vec![Node::atom(head)];
    items.extend(args);
    Node::list(items)
}

fn word(head: &str, value: &str) -> Node {
    item(head, vec![atom(value)])
}

fn num(nm: Nm) -> Node {
    atom(mm(nm.0))
}

fn uuid() -> Node {
    item("uuid", vec![Node::string(uuid::Uuid::new_v4().to_string())])
}

fn at(p: Point, angle: u16) -> Node {
    item("at", vec![num(p.x), num(p.y), atom(angle.to_string())])
}

fn library_id_text(id: &LibraryId) -> String {
    format!("{}:{}", id.library(), id.entry())
}

fn effects_default() -> Node {
    item(
        "effects",
        vec![item("font", vec![item("size", vec![atom("1.27"), atom("1.27")])])],
    )
}

/// The library property's text style. KiCad stores a field's angle and justification relative to
/// its symbol and applies the symbol's rotation and mirroring when drawing, so they are copied as
/// they are.
fn effects(def: &Node) -> Node {
    def.child("effects").cloned().unwrap_or_else(effects_default)
}

/// One field of a placed symbol, positioned from the library's offset the way KiCad does.
fn property(def: &Node, text: &str, origin: Point, o: Orientation) -> Result<Node, EmitError> {
    let name = def.arg_text(0).unwrap_or_default();
    let lib_at = def.child("at");
    let coord = |i: usize| lib_at.and_then(|a| a.arg_f64(i)).unwrap_or(0.0);
    // Library offsets are y-up; sheet axes are y-down.
    let local = Point::new(
        Nm((coord(0) * 1e6).round() as i64),
        Nm((-coord(1) * 1e6).round() as i64),
    );
    let world = origin + o.apply(local).ok_or(EmitError::BadRotation)?;
    let angle = coord(2).rem_euclid(360.0).round() as u16;

    let mut items = vec![
        atom("property"),
        Node::string(name),
        Node::string(text),
        at(world, angle),
    ];
    if def.child_text("hide") == Some("yes") {
        items.push(word("hide", "yes"));
    }
    items.push(word("show_name", def.child_text("show_name").unwrap_or("no")));
    items.push(word(
        "do_not_autoplace",
        def.child_text("do_not_autoplace").unwrap_or("no"),
    ));
    items.push(effects(def));
    Ok(Node::list(items))
}

struct Placement<'a> {
    id: &'a LibraryId,
    reference: &'a str,
    value: &'a str,
    footprint: Option<&'a str>,
    position: Point,
    orientation: Orientation,
}

fn symbol_node(def: &Node, p: &Placement, sheet: &str, project: &str) -> Result<Node, EmitError> {
    // Mirroring both ways is a half turn.
    let mut o = p.orientation;
    if o.mirror_x && o.mirror_y {
        o = Orientation {
            rotation_deg: (o.rotation_deg + 180) % 360,
            mirror_x: false,
            mirror_y: false,
        };
    }
    let mut items = vec![
        atom("symbol"),
        item("lib_id", vec![Node::string(library_id_text(p.id))]),
        at(p.position, o.rotation_deg),
    ];
    if o.mirror_x {
        items.push(word("mirror", "x"));
    } else if o.mirror_y {
        items.push(word("mirror", "y"));
    }
    let flag = |name: &str, default: &str| word(name, def.child_text(name).unwrap_or(default));
    items.extend([
        word("unit", "1"),
        word("body_style", "1"),
        flag("exclude_from_sim", "no"),
        flag("in_bom", "yes"),
        flag("on_board", "yes"),
        flag("in_pos_files", "yes"),
        word("dnp", "no"),
        word("fields_autoplaced", "yes"),
        uuid(),
    ]);
    for name in ["Reference", "Value", "Footprint", "Datasheet", "Description"] {
        let Some(field) = def.children("property").find(|f| f.arg_text(0) == Some(name)) else {
            continue;
        };
        let text = match name {
            "Reference" => p.reference,
            "Value" => p.value,
            "Footprint" => p.footprint.unwrap_or_else(|| field.arg_text(1).unwrap_or_default()),
            _ => field.arg_text(1).unwrap_or_default(),
        };
        items.push(property(field, text, p.position, o)?);
    }
    let mut seen = BTreeSet::new();
    for pin in describe(p.id, def).pins.iter().filter(|pin| pin.unit <= 1) {
        if seen.insert(pin.number.clone()) {
            items.push(item("pin", vec![Node::string(&pin.number), uuid()]));
        }
    }
    items.push(item(
        "instances",
        vec![item(
            "project",
            vec![
                Node::string(project),
                item(
                    "path",
                    vec![
                        Node::string(format!("/{sheet}")),
                        item("reference", vec![Node::string(p.reference)]),
                        word("unit", "1"),
                    ],
                ),
            ],
        )],
    ));
    Ok(Node::list(items))
}

fn wire_node(a: Point, b: Point) -> Node {
    item(
        "wire",
        vec![
            item(
                "pts",
                vec![
                    item("xy", vec![num(a.x), num(a.y)]),
                    item("xy", vec![num(b.x), num(b.y)]),
                ],
            ),
            item("stroke", vec![word("width", "0"), word("type", "default")]),
            uuid(),
        ],
    )
}

fn label_node(net: &str, position: Point, direction: Dir) -> Node {
    // The text extends away from the anchor in `direction`.
    let (angle, side) = match direction {
        Dir::Right => (0, "left"),
        Dir::Up => (90, "left"),
        Dir::Left => (180, "right"),
        Dir::Down => (270, "right"),
    };
    item(
        "label",
        vec![
            Node::string(net),
            at(position, angle),
            item(
                "effects",
                vec![
                    item("font", vec![item("size", vec![atom("1.27"), atom("1.27")])]),
                    item("justify", vec![atom(side), atom("bottom")]),
                ],
            ),
            uuid(),
        ],
    )
}

fn junction_node(position: Point) -> Node {
    item(
        "junction",
        vec![
            item("at", vec![num(position.x), num(position.y)]),
            word("diameter", "0"),
            item("color", vec![atom("0"), atom("0"), atom("0"), atom("0")]),
            uuid(),
        ],
    )
}

/// Where to insert lines so they end up just before the closing parenthesis of `list`.
fn before_close(text: &str, list: &Node, lines: &str) -> (usize, String) {
    let close = list.span.end - 1;
    match text[..close].rfind('\n') {
        Some(nl) if text[nl + 1..close].chars().all(|c| c == ' ' || c == '\t') => (nl + 1, lines.to_owned()),
        _ => (close, format!("\n{lines}\t")),
    }
}

/// A change to the original text: replace `range` with `text` (an insertion has an empty range).
struct Edit {
    range: Range<usize>,
    text: String,
}

/// The range to delete for `node`: its own line when it is alone on it.
fn removal_range(text: &str, node: &Node) -> Range<usize> {
    let mut start = node.span.start;
    if let Some(nl) = text[..start].rfind('\n') {
        if text[nl + 1..start].chars().all(|c| c == ' ' || c == '\t') {
            start = nl + 1;
        }
    }
    let mut end = node.span.end;
    if text[end..].starts_with("\r\n") {
        end += 2;
    } else if text[end..].starts_with('\n') {
        end += 1;
    }
    start..end
}

/// The transform a symbol is written with: mirroring both ways is a half turn.
fn normalized(o: Orientation) -> Orientation {
    if o.mirror_x && o.mirror_y {
        Orientation {
            rotation_deg: (o.rotation_deg + 180) % 360,
            mirror_x: false,
            mirror_y: false,
        }
    } else {
        o
    }
}

fn at_text(p: Point, angle: &str) -> String {
    format!("(at {} {} {angle})", mm(p.x.0), mm(p.y.0))
}

fn sheet_point(at: &Node) -> Option<Point> {
    Some(Point::new(
        Nm((at.arg_f64(0)? * 1e6).round() as i64),
        Nm((at.arg_f64(1)? * 1e6).round() as i64),
    ))
}

/// The edits that move one symbol and carry its fields along: each field keeps its place relative
/// to the symbol, turned the way the symbol turns.
fn move_edits(text: &str, node: &Node, from: &PlacedSymbol, to: &SymbolMove) -> Result<Vec<Edit>, EmitError> {
    let (old_o, new_o) = (normalized(from.orientation), normalized(to.orientation));
    let mut edits = Vec::new();
    let at = node
        .child("at")
        .ok_or(EmitError::NoInsertPoint("a symbol without a position"))?;
    edits.push(Edit {
        range: at.span.clone(),
        text: at_text(to.to, &new_o.rotation_deg.to_string()),
    });
    let wanted = if new_o.mirror_x {
        Some("x")
    } else if new_o.mirror_y {
        Some("y")
    } else {
        None
    };
    match (node.child("mirror"), wanted) {
        (Some(m), Some(axis)) => edits.push(Edit {
            range: m.span.clone(),
            text: format!("(mirror {axis})"),
        }),
        (Some(m), None) => edits.push(Edit {
            range: removal_range(text, m),
            text: String::new(),
        }),
        (None, Some(axis)) => edits.push(Edit {
            range: at.span.end..at.span.end,
            text: format!("\n\t\t(mirror {axis})"),
        }),
        (None, None) => {}
    }
    for property in node.children("property") {
        let Some(field_at) = property.child("at") else { continue };
        let Some(old_world) = sheet_point(field_at) else {
            continue;
        };
        let local = old_o.unapply(old_world - from.position).ok_or(EmitError::BadRotation)?;
        let new_world = to.to + new_o.apply(local).ok_or(EmitError::BadRotation)?;
        edits.push(Edit {
            range: field_at.span.clone(),
            text: at_text(new_world, field_at.arg_text(2).unwrap_or("0")),
        });
    }
    Ok(edits)
}

/// `doc`'s text with the text of one property (`Value`, `Footprint`, …) of the symbol with this
/// uuid replaced; everything else is untouched.
pub fn set_property(doc: &Document, uuid: &str, name: &str, text: &str) -> Result<String, EmitError> {
    let symbol = doc
        .item("symbol", uuid)
        .ok_or_else(|| EmitError::ItemNotFound("symbol", uuid.to_owned()))?;
    let value = symbol
        .children("property")
        .find(|p| p.arg_text(0) == Some(name))
        .and_then(|p| p.arg(1))
        .ok_or_else(|| EmitError::ItemNotFound("property", name.to_owned()))?;
    let mut out = doc.text().to_owned();
    out.replace_range(value.span.clone(), &quote(text));
    Ok(out)
}

/// `doc`'s text with the plan's items added. `defs` holds a flattened definition (named
/// `library:name`) for every library symbol the plan places; those the file does not carry yet
/// are added to its `lib_symbols`. `project` names the project in the new symbols' instance data.
pub fn add_plan(
    doc: &Document,
    plan: &OperationPlan,
    defs: &BTreeMap<LibraryId, Node>,
    project: &str,
) -> Result<String, EmitError> {
    let text = doc.text();
    let sheet = doc.uuid();
    let def_of = |id: &LibraryId| {
        defs.get(id)
            .or_else(|| doc.embedded(&library_id_text(id)))
            .ok_or_else(|| EmitError::MissingDefinition(library_id_text(id)))
    };

    let mut new_defs = String::new();
    let mut added: BTreeSet<&LibraryId> = BTreeSet::new();
    let mut items = String::new();
    for op in &plan.operations {
        let node = match op {
            Operation::PlaceSymbol {
                library_id,
                reference,
                value,
                footprint,
                position,
                orientation,
                ..
            } => {
                let def = def_of(library_id)?;
                symbol_node(
                    def,
                    &Placement {
                        id: library_id,
                        reference,
                        value,
                        footprint: footprint.as_deref(),
                        position: *position,
                        orientation: *orientation,
                    },
                    sheet,
                    project,
                )?
            }
            Operation::PlacePowerSymbol {
                net,
                library_id,
                reference,
                position,
                orientation,
            } => {
                let def = def_of(library_id)?;
                symbol_node(
                    def,
                    &Placement {
                        id: library_id,
                        reference,
                        value: net,
                        footprint: None,
                        position: *position,
                        orientation: *orientation,
                    },
                    sheet,
                    project,
                )?
            }
            Operation::AddWire { start, end, .. } => wire_node(*start, *end),
            Operation::AddLabel {
                net,
                position,
                direction,
            } => label_node(net, *position, *direction),
            Operation::AddJunction { position } => junction_node(*position),
        };
        if let Operation::PlaceSymbol { library_id, .. } | Operation::PlacePowerSymbol { library_id, .. } = op {
            if doc.embedded(&library_id_text(library_id)).is_none() && added.insert(library_id) {
                new_defs.push_str(&format!("\t\t{}\n", def_of(library_id)?.render(2)));
            }
        }
        items.push_str(&format!("\t{}\n", node.render(1)));
    }

    let root = doc.root();
    let lib_symbols = root
        .child("lib_symbols")
        .ok_or(EmitError::NoInsertPoint("lib_symbols"))?;
    let mut edits: Vec<Edit> = Vec::new();
    let insertion = |(pos, text): (usize, String)| Edit { range: pos..pos, text };
    if !new_defs.is_empty() {
        edits.push(insertion(before_close(text, lib_symbols, &new_defs)));
    }

    // Existing items the plan deletes or moves.
    for removal in &plan.removals {
        let (head, what) = match removal.kind {
            RemovedKind::Wire => ("wire", "wire"),
            RemovedKind::Label => ("label", "label"),
            RemovedKind::Junction => ("junction", "junction"),
            RemovedKind::PowerSymbol => ("symbol", "power symbol"),
            RemovedKind::Symbol => ("symbol", "part"),
        };
        let node = doc
            .item(head, &removal.id)
            .ok_or_else(|| EmitError::ItemNotFound(what, removal.id.clone()))?;
        edits.push(Edit {
            range: removal_range(text, node),
            text: String::new(),
        });
    }
    if !plan.moves.is_empty() {
        let placed = doc.symbols();
        for m in &plan.moves {
            let node = doc
                .item("symbol", &m.id)
                .ok_or_else(|| EmitError::ItemNotFound("symbol", m.id.clone()))?;
            let from = placed
                .iter()
                .find(|s| s.uuid.eq_ignore_ascii_case(&m.id))
                .ok_or_else(|| EmitError::ItemNotFound("symbol", m.id.clone()))?;
            edits.extend(move_edits(text, node, from, m)?);
        }
    }
    if !items.is_empty() {
        edits.push(insertion(match root.child("sheet_instances") {
            Some(sheet_instances) => {
                let start = sheet_instances.span.start;
                match text[..start].rfind('\n') {
                    Some(nl) if text[nl + 1..start].chars().all(|c| c == ' ' || c == '\t') => (nl + 1, items),
                    _ => (start, format!("{}\n\t", items.trim_start())),
                }
            }
            None => before_close(text, root, &items),
        }));
    }
    // Apply from the back so earlier offsets stay valid.
    edits.sort_by_key(|e| std::cmp::Reverse((e.range.start, e.range.end)));
    let mut out = text.to_owned();
    for edit in edits {
        out.replace_range(edit.range, &edit.text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use kicad_domain::plan::OperationPlan;

    use super::*;
    use crate::node::parse;
    use crate::sch::{empty_schematic, MIN_VERSION};
    use crate::sym::SymbolLibrary;

    const LIB: &str = r#"(kicad_symbol_lib (version 20251024)
	(symbol "R" (property "Reference" "R" (at 2.032 0 90) (effects (font (size 1.27 1.27)) (justify left)))
		(property "Value" "R" (at 0 0 90) (effects (font (size 1.27 1.27))))
		(property "Footprint" "" (at -1.778 0 90) (hide yes) (effects (font (size 1.27 1.27))))
		(symbol "R_0_1" (rectangle (start -1.016 -2.54) (end 1.016 2.54)))
		(symbol "R_1_1"
			(pin passive line (at 0 3.81 270) (length 1.27) (name "~") (number "1"))
			(pin passive line (at 0 -3.81 90) (length 1.27) (name "~") (number "2")))))"#;

    fn plan(operations: Vec<Operation>) -> OperationPlan {
        OperationPlan {
            circuit: "t".into(),
            operations,
            ..Default::default()
        }
    }

    fn defs() -> BTreeMap<LibraryId, Node> {
        let lib = SymbolLibrary::parse(LIB).unwrap();
        BTreeMap::from([(
            LibraryId::parse("Device:R").unwrap(),
            lib.embeddable("Device", "R").unwrap(),
        )])
    }

    fn place(reference: &str, position: Point, orientation: Orientation) -> Operation {
        Operation::PlaceSymbol {
            component_id: kicad_domain::LogicalId::new(reference.to_lowercase()).unwrap(),
            library_id: LibraryId::parse("Device:R").unwrap(),
            reference: reference.into(),
            value: "10k".into(),
            footprint: None,
            position,
            orientation,
        }
    }

    fn mm_point(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    #[test]
    fn adds_a_symbol_wires_and_definitions_and_keeps_the_rest() {
        let original = empty_schematic(MIN_VERSION, "10.0");
        let doc = Document::parse(original.clone()).unwrap();
        let operations = vec![
            place("R1", mm_point(50.8, 63.5), Orientation::default()),
            Operation::AddWire {
                net: "A".into(),
                start: mm_point(50.8, 59.69),
                end: mm_point(50.8, 53.34),
            },
            Operation::AddLabel {
                net: "A".into(),
                position: mm_point(50.8, 53.34),
                direction: Dir::Left,
            },
            Operation::AddJunction {
                position: mm_point(50.8, 55.88),
            },
        ];
        let out = add_plan(&doc, &plan(operations), &defs(), "demo").unwrap();

        let reread = Document::parse(out.clone()).unwrap();
        assert!(reread.embedded("Device:R").is_some());
        let symbols = reread.symbols();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].reference, "R1");
        assert_eq!(symbols[0].value, "10k");
        assert_eq!(symbols[0].pins.len(), 2);
        let root = parse(&out).unwrap();
        assert!(root.child("wire").is_some() && root.child("label").is_some() && root.child("junction").is_some());
        assert_eq!(reread.project_name(), Some("demo"));
        // Everything that was there is still there, in order.
        let head = original.find("(lib_symbols").unwrap();
        assert!(out.starts_with(&original[..head]));
        assert!(out.contains("(sheet_instances"));
        assert!(out.trim_end().ends_with("(embedded_fonts no)\n)"));
    }

    #[test]
    fn does_not_add_a_definition_the_file_already_has() {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        let first = add_plan(
            &doc,
            &plan(vec![place("R1", mm_point(0.0, 0.0), Orientation::default())]),
            &defs(),
            "p",
        )
        .unwrap();
        let doc = Document::parse(first).unwrap();
        let second = add_plan(
            &doc,
            &plan(vec![place("R2", mm_point(20.32, 0.0), Orientation::default())]),
            &defs(),
            "p",
        )
        .unwrap();
        let root = parse(&second).unwrap();
        assert_eq!(root.child("lib_symbols").unwrap().children("symbol").count(), 1);
        assert_eq!(root.children("symbol").count(), 2);
    }

    #[test]
    fn fields_follow_the_symbol_transform() {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        let rotated = Orientation {
            rotation_deg: 90,
            ..Orientation::default()
        };
        let out = add_plan(
            &doc,
            &plan(vec![place("R1", mm_point(100.0, 100.0), rotated)]),
            &defs(),
            "p",
        )
        .unwrap();
        let root = parse(&out).unwrap();
        let symbol = root.child("symbol").unwrap();
        let reference = symbol
            .children("property")
            .find(|p| p.arg_text(0) == Some("Reference"))
            .unwrap();
        // The library puts the reference 2.032 mm to the right of the origin; turned a quarter,
        // it sits 2.032 mm above it. The angle stays the library's (KiCad adds the rotation).
        let at = reference.child("at").unwrap();
        assert_eq!(
            (at.arg_f64(0), at.arg_f64(1), at.arg_f64(2)),
            (Some(100.0), Some(97.968), Some(90.0))
        );
        let footprint = symbol
            .children("property")
            .find(|p| p.arg_text(0) == Some("Footprint"))
            .unwrap();
        assert_eq!(footprint.child_text("hide"), Some("yes"));
    }

    #[test]
    fn mirroring_both_ways_is_a_half_turn() {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        let both = Orientation {
            rotation_deg: 0,
            mirror_x: true,
            mirror_y: true,
        };
        let out = add_plan(&doc, &plan(vec![place("R1", mm_point(0.0, 0.0), both)]), &defs(), "p").unwrap();
        let symbol = Document::parse(out).unwrap().symbols().remove(0);
        assert_eq!(symbol.orientation.rotation_deg, 180);
        assert!(!symbol.orientation.mirror_x && !symbol.orientation.mirror_y);
    }

    fn built_with_a_resistor_and_a_wire() -> String {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        let operations = vec![
            place("R1", mm_point(50.8, 63.5), Orientation::default()),
            Operation::AddWire {
                net: "A".into(),
                start: mm_point(50.8, 59.69),
                end: mm_point(50.8, 53.34),
            },
        ];
        add_plan(&doc, &plan(operations), &defs(), "p").unwrap()
    }

    fn move_plan(doc: &Document, to: Point, orientation: Orientation, remove_wires: bool) -> OperationPlan {
        let symbol = doc.symbols().remove(0);
        let design = doc.design();
        OperationPlan {
            moves: vec![SymbolMove {
                id: symbol.uuid,
                reference: symbol.reference,
                from: symbol.position,
                from_orientation: symbol.orientation,
                to,
                orientation,
            }],
            removals: if remove_wires {
                design
                    .wires
                    .iter()
                    .map(|w| kicad_domain::plan::Removal {
                        kind: RemovedKind::Wire,
                        id: w.id.clone(),
                        description: String::new(),
                    })
                    .collect()
            } else {
                vec![]
            },
            ..plan(vec![])
        }
    }

    fn field(root: &Node, name: &str) -> (f64, f64, f64) {
        let at = root
            .child("symbol")
            .unwrap()
            .children("property")
            .find(|p| p.arg_text(0) == Some(name))
            .unwrap()
            .child("at")
            .unwrap();
        (at.arg_f64(0).unwrap(), at.arg_f64(1).unwrap(), at.arg_f64(2).unwrap())
    }

    #[test]
    fn moving_a_symbol_carries_its_fields_and_leaves_the_rest_alone() {
        let before = built_with_a_resistor_and_a_wire();
        let doc = Document::parse(before.clone()).unwrap();
        let turned = Orientation {
            rotation_deg: 90,
            ..Orientation::default()
        };
        let after = add_plan(
            &doc,
            &move_plan(&doc, mm_point(100.0, 100.0), turned, true),
            &BTreeMap::new(),
            "p",
        )
        .unwrap();
        let moved = Document::parse(after.clone()).unwrap();
        let symbol = moved.symbols().remove(0);
        assert_eq!((symbol.position, symbol.orientation), (mm_point(100.0, 100.0), turned));
        assert_eq!(symbol.pins.len(), 2, "pins and their ids stay");
        assert_eq!(moved.design().wires.len(), 0, "the wire is gone");
        // The reference sat 2.032 mm to the right of the symbol; turned a quarter it is above it.
        let root = parse(&after).unwrap();
        assert_eq!(field(&root, "Reference"), (100.0, 97.968, 90.0));
        // Everything else is as it was: the library, the uuid, the instance data.
        let strip = |t: &str| {
            t.lines()
                .filter(|l| l.contains("uuid") || l.contains("lib_symbols"))
                .count()
        };
        assert_eq!(strip(&before) - 1, strip(&after), "only the wire's uuid line is gone");
        assert!(after.contains("(lib_id \"Device:R\")"));
    }

    #[test]
    fn mirroring_is_added_and_removed_again() {
        let doc = Document::parse(built_with_a_resistor_and_a_wire()).unwrap();
        let mirrored = Orientation {
            mirror_y: true,
            ..Orientation::default()
        };
        let once = add_plan(
            &doc,
            &move_plan(&doc, mm_point(20.0, 20.0), mirrored, false),
            &BTreeMap::new(),
            "p",
        )
        .unwrap();
        let doc2 = Document::parse(once.clone()).unwrap();
        assert!(once.contains("(mirror y)"));
        assert_eq!(doc2.symbols()[0].orientation, mirrored);
        // Mirrored left to right, the reference moves from the right of the symbol to its left.
        assert_eq!(field(&parse(&once).unwrap(), "Reference").0, 17.968);

        let back = add_plan(
            &doc2,
            &move_plan(&doc2, mm_point(20.0, 20.0), Orientation::default(), false),
            &BTreeMap::new(),
            "p",
        )
        .unwrap();
        assert!(!back.contains("(mirror"));
        assert_eq!(field(&parse(&back).unwrap(), "Reference"), (22.032, 20.0, 90.0));
    }

    #[test]
    fn items_that_are_not_in_the_file_are_reported() {
        let doc = Document::parse(built_with_a_resistor_and_a_wire()).unwrap();
        let mut p = move_plan(&doc, mm_point(0.0, 0.0), Orientation::default(), false);
        p.removals.push(kicad_domain::plan::Removal {
            kind: RemovedKind::Junction,
            id: "nope".into(),
            description: String::new(),
        });
        assert_eq!(
            add_plan(&doc, &p, &BTreeMap::new(), "p"),
            Err(EmitError::ItemNotFound("junction", "nope".into()))
        );
    }

    #[test]
    fn a_property_can_be_changed_alone() {
        let before = built_with_a_resistor_and_a_wire();
        let doc = Document::parse(before.clone()).unwrap();
        let id = doc.symbols()[0].uuid.clone();
        let after = set_property(&doc, &id, "Value", "4k7 \"1%\"").unwrap();
        assert_eq!(Document::parse(after.clone()).unwrap().symbols()[0].value, "4k7 \"1%\"");
        // Only that text differs.
        let (a, b): (Vec<_>, Vec<_>) = (before.lines().collect(), after.lines().collect());
        assert_eq!(a.len(), b.len());
        assert_eq!(a.iter().zip(&b).filter(|(x, y)| x != y).count(), 1);
        assert_eq!(
            set_property(&doc, &id, "Nope", "x"),
            Err(EmitError::ItemNotFound("property", "Nope".into()))
        );
    }

    #[test]
    fn an_empty_plan_changes_nothing() {
        let original = empty_schematic(MIN_VERSION, "10.0");
        let doc = Document::parse(original.clone()).unwrap();
        assert_eq!(add_plan(&doc, &plan(vec![]), &BTreeMap::new(), "p").unwrap(), original);
    }

    #[test]
    fn a_missing_definition_is_an_error() {
        let doc = Document::parse(empty_schematic(MIN_VERSION, "10.0")).unwrap();
        let result = add_plan(
            &doc,
            &plan(vec![place("R1", mm_point(0.0, 0.0), Orientation::default())]),
            &BTreeMap::new(),
            "p",
        );
        assert_eq!(result, Err(EmitError::MissingDefinition("Device:R".into())));
    }
}
