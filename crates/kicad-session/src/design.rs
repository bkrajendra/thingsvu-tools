// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a whole sheet from a live KiCad: parts, wires, labels, junctions, and the items that
//! tidying cannot handle.

use kicad_domain::schematic::{DesignField, DesignShape, DesignText, ShapeKind};
use kicad_domain::text::{HAlign, VAlign, DEFAULT_SIZE};
use kicad_domain::{
    Capability, DesignJunction, DesignLabel, DesignNoConnect, DesignSymbol, DesignWire, Dir, ErrorCode, Nm, Point,
    SchematicDesign, ToolError,
};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse};
use kicad_ipc::proto::kiapi::common::types::graphic_shape::Geometry;
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Vector2};
use kicad_ipc::proto::kiapi::schematic::types::{
    DirectiveLabel, GlobalLabel, Group, HierarchicalLabel, Junction, LocalLabel, NoConnectMarker, SchematicField,
    SchematicGraphicShape, SchematicImage, SchematicLabelSpinStyle, SchematicLine, SchematicLineType,
    SchematicRuleArea, SchematicTable, SchematicText, SchematicTextBox, SheetSymbol,
};
use prost::Name;

use crate::errors::from_ipc_error;
use crate::session::Session;

fn point(v: &Option<Vector2>) -> Point {
    v.as_ref()
        .map(|v| Point::new(Nm(v.x_nm), Nm(v.y_nm)))
        .unwrap_or_default()
}

fn distance(d: &Option<kicad_ipc::proto::kiapi::common::types::Distance>) -> Nm {
    Nm(d.as_ref().map(|d| d.value_nm).unwrap_or(0))
}

/// The text of a field as drawn, with how it is anchored.
fn field_of(name: &str, field: &Option<SchematicField>) -> Option<DesignField> {
    let f = field.as_ref()?;
    let text = f.text.as_ref()?;
    let a = text.attributes.clone().unwrap_or_default();
    Some(DesignField {
        name: name.to_owned(),
        text: text.text.clone(),
        position: point(&text.position),
        angle_deg: a
            .angle
            .map(|d| d.value_degrees.rem_euclid(360.0).round() as u16 % 360)
            .unwrap_or(0),
        size: a
            .size
            .as_ref()
            .filter(|v| v.x_nm > 0)
            .map(|v| Nm(v.y_nm.max(v.x_nm)))
            .unwrap_or(DEFAULT_SIZE),
        hidden: !f.visible,
        h: match a.horizontal_alignment {
            1 => HAlign::Left,
            3 => HAlign::Right,
            _ => HAlign::Center,
        },
        v: match a.vertical_alignment {
            1 => VAlign::Top,
            3 => VAlign::Bottom,
            _ => VAlign::Center,
        },
    })
}

/// The way a label's text runs from its anchor.
fn spin_direction(spin: i32) -> Dir {
    match SchematicLabelSpinStyle::try_from(spin) {
        Ok(SchematicLabelSpinStyle::SlssLeft) => Dir::Left,
        Ok(SchematicLabelSpinStyle::SlssUp) => Dir::Up,
        Ok(SchematicLabelSpinStyle::SlssBottom) => Dir::Down,
        _ => Dir::Right,
    }
}

fn decode<T: prost::Message + Default + Name>(any: &prost_types::Any) -> Result<T, ToolError> {
    any.to_msg::<T>()
        .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("undecodable {}: {e}", T::NAME)))
}

impl Session {
    /// Every part, wire, label, and junction on the sheet, with a count of what tidying cannot move.
    pub async fn design(&self) -> Result<SchematicDesign, ToolError> {
        self.require(Capability::SchReadSymbols)?;
        let mut design = SchematicDesign::default();
        let instances = self.symbol_instances().await?;
        let fields_of = |id: &str| -> Vec<DesignField> {
            instances
                .iter()
                .find(|i| i.id.as_ref().is_some_and(|k| k.value == id))
                .map(|i| {
                    [
                        field_of("Reference", &i.reference_field),
                        field_of("Value", &i.value_field),
                    ]
                    .into_iter()
                    .flatten()
                    .collect()
                })
                .unwrap_or_default()
        };
        let facts_of = |id: &str| -> (std::collections::BTreeMap<String, String>, [bool; 3]) {
            let Some(i) = instances.iter().find(|i| i.id.as_ref().is_some_and(|k| k.value == id)) else {
                return Default::default();
            };
            let text = |f: &Option<SchematicField>| {
                f.as_ref()
                    .and_then(|f| f.text.as_ref())
                    .map(|t| t.text.clone())
                    .unwrap_or_default()
            };
            let mut properties = std::collections::BTreeMap::from([
                ("Reference".to_owned(), text(&i.reference_field)),
                ("Value".to_owned(), text(&i.value_field)),
                ("Footprint".to_owned(), text(&i.footprint_field)),
                ("Datasheet".to_owned(), text(&i.datasheet_field)),
                ("Description".to_owned(), text(&i.description_field)),
            ]);
            for f in &i.user_fields {
                properties.insert(
                    f.name.clone(),
                    f.text.as_ref().map(|t| t.text.clone()).unwrap_or_default(),
                );
            }
            let a = i.attributes.unwrap_or_default();
            (
                properties,
                [
                    a.do_not_populate,
                    a.exclude_from_bill_of_materials,
                    a.exclude_from_board,
                ],
            )
        };
        for s in self.list_symbols().await? {
            let fields = fields_of(s.id.as_str());
            let (properties, [dnp, exclude_from_bom, exclude_from_board]) = facts_of(s.id.as_str());
            let position = Point::from_mm(s.position_mm.x, s.position_mm.y)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("{}: {e}", s.reference)))?;
            design.symbols.push(
                DesignSymbol {
                    id: s.id.as_str().to_owned(),
                    reference: s.reference,
                    value: s.value,
                    library_id: s.library_id,
                    footprint: s.footprint,
                    position,
                    orientation: s.orientation,
                    unit: s.unit,
                    fields,
                    properties,
                    dnp,
                    exclude_from_bom,
                    exclude_from_board,
                }
                .with_drawn_angles(),
            );
        }

        let response: GetItemsResponse = {
            let mut inner = self.inner.lock().await;
            inner
                .client
                .call(&GetItems {
                    header: Some(ItemHeader {
                        document: Some(self.document.clone()),
                        ..Default::default()
                    }),
                    types: [
                        KiCadObjectType::KotSchLine,
                        KiCadObjectType::KotSchJunction,
                        KiCadObjectType::KotSchLabel,
                        KiCadObjectType::KotSchGlobalLabel,
                        KiCadObjectType::KotSchHierLabel,
                        KiCadObjectType::KotSchDirectiveLabel,
                        KiCadObjectType::KotSchNoConnect,
                        KiCadObjectType::KotSchBusWireEntry,
                        KiCadObjectType::KotSchBusBusEntry,
                        KiCadObjectType::KotSchSheet,
                        KiCadObjectType::KotSchText,
                        KiCadObjectType::KotSchTextbox,
                        KiCadObjectType::KotSchShape,
                        KiCadObjectType::KotSchBitmap,
                        KiCadObjectType::KotSchTable,
                        KiCadObjectType::KotSchGroup,
                        KiCadObjectType::KotSchRuleArea,
                    ]
                    .map(|t| t as i32)
                    .to_vec(),
                })
                .await
                .map_err(|e| from_ipc_error(&e, false))?
        };
        let mut unsupported = |kind: &str| *design.unsupported.entry(kind.to_owned()).or_default() += 1;
        let mut wires = Vec::new();
        let mut labels = Vec::new();
        let mut junctions = Vec::new();
        let mut no_connects = Vec::new();
        let mut annotations = 0;
        let mut texts = Vec::new();
        let mut shapes = Vec::new();
        for any in &response.items {
            let url = any.type_url.as_str();
            if url == SchematicLine::type_url() {
                let line: SchematicLine = decode(any)?;
                match SchematicLineType::try_from(line.r#type) {
                    Ok(SchematicLineType::SltWire) => wires.push(DesignWire {
                        id: line.id.map(|k| k.value).unwrap_or_default(),
                        start: point(&line.start),
                        end: point(&line.end),
                    }),
                    Ok(SchematicLineType::SltBus) => unsupported("bus"),
                    _ => {
                        annotations += 1;
                        shapes.push(DesignShape {
                            id: line.id.map(|k| k.value).unwrap_or_default(),
                            kind: ShapeKind::Line {
                                start: point(&line.start),
                                end: point(&line.end),
                            },
                            stroke: distance(&line.stroke.and_then(|s| s.width)),
                            filled: false,
                        });
                    }
                }
            } else if url == Junction::type_url() {
                let j: Junction = decode(any)?;
                junctions.push(DesignJunction {
                    id: j.id.map(|k| k.value).unwrap_or_default(),
                    position: point(&j.position),
                });
            } else if url == LocalLabel::type_url() {
                let l: LocalLabel = decode(any)?;
                labels.push(DesignLabel {
                    id: l.id.map(|k| k.value).unwrap_or_default(),
                    text: l.text.map(|t| t.text).unwrap_or_default(),
                    position: point(&l.position),
                    direction: spin_direction(l.spin_style),
                });
            } else if url == GlobalLabel::type_url() {
                unsupported("global_label");
            } else if url == HierarchicalLabel::type_url() {
                unsupported("hierarchical_label");
            } else if url == DirectiveLabel::type_url() {
                unsupported("directive_label");
            } else if url == NoConnectMarker::type_url() {
                let n: NoConnectMarker = decode(any)?;
                no_connects.push(DesignNoConnect {
                    id: n.id.map(|k| k.value).unwrap_or_default(),
                    position: point(&n.position),
                });
            } else if url == SheetSymbol::type_url() {
                unsupported("sheet");
            } else if url == SchematicTable::type_url() {
                unsupported("table");
            } else if url == Group::type_url() {
                unsupported("group");
            } else if url == SchematicRuleArea::type_url() {
                unsupported("rule_area");
            } else if url.ends_with("BusEntry") {
                unsupported("bus_entry");
            } else if url == SchematicText::type_url() {
                annotations += 1;
                let note: SchematicText = decode(any)?;
                if let Some(text) = note.text {
                    let a = text.attributes.clone().unwrap_or_default();
                    let item = DesignText {
                        id: note.id.map(|k| k.value).unwrap_or_default(),
                        text: text.text.clone(),
                        position: point(&text.position),
                        size: a
                            .size
                            .as_ref()
                            .filter(|v| v.x_nm > 0)
                            .map(|v| Nm(v.y_nm.max(v.x_nm)))
                            .unwrap_or(DEFAULT_SIZE),
                        angle_deg: a
                            .angle
                            .map(|d| d.value_degrees.rem_euclid(360.0).round() as u16 % 360)
                            .unwrap_or(0),
                        h: match a.horizontal_alignment {
                            1 => HAlign::Left,
                            3 => HAlign::Right,
                            _ => HAlign::Center,
                        },
                        v: match a.vertical_alignment {
                            1 => VAlign::Top,
                            3 => VAlign::Bottom,
                            _ => VAlign::Center,
                        },
                    };
                    design.notes.push(item.bounds());
                    texts.push(item);
                }
            } else if url == SchematicGraphicShape::type_url() {
                annotations += 1;
                let shape: SchematicGraphicShape = decode(any)?;
                if let Some(g) = shape.shape {
                    let attrs = g.attributes.unwrap_or_default();
                    let kind = match g.geometry {
                        Some(Geometry::Rectangle(r)) => {
                            let (a, b) = (point(&r.top_left), point(&r.bottom_right));
                            Some(ShapeKind::Rectangle {
                                min: Point::new(a.x.min(b.x), a.y.min(b.y)),
                                max: Point::new(a.x.max(b.x), a.y.max(b.y)),
                            })
                        }
                        Some(Geometry::Circle(c)) => {
                            let (center, edge) = (point(&c.center), point(&c.radius_point));
                            let (dx, dy) = ((edge.x.0 - center.x.0) as f64, (edge.y.0 - center.y.0) as f64);
                            Some(ShapeKind::Circle {
                                center,
                                radius: Nm(dx.hypot(dy).round() as i64),
                            })
                        }
                        Some(Geometry::Segment(l)) => Some(ShapeKind::Line {
                            start: point(&l.start),
                            end: point(&l.end),
                        }),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        shapes.push(DesignShape {
                            id: shape.id.map(|k| k.value).unwrap_or_default(),
                            kind,
                            stroke: distance(&attrs.stroke.and_then(|s| s.width)),
                            filled: attrs.fill.is_some_and(|f| f.fill_type > 1),
                        });
                    }
                }
            } else if url == SchematicTextBox::type_url() || url == SchematicImage::type_url() {
                annotations += 1;
            }
        }
        design.wires = wires;
        design.labels = labels;
        design.junctions = junctions;
        design.no_connects = no_connects;
        design.annotations = annotations;
        design.texts = texts;
        design.shapes = shapes;
        Ok(design)
    }
}
