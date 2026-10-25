// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a whole sheet from a live KiCad: parts, wires, labels, junctions, and the items that
//! tidying cannot handle.

use kicad_domain::{
    Capability, DesignJunction, DesignLabel, DesignSymbol, DesignWire, ErrorCode, Nm, Point, SchematicDesign, ToolError,
};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Vector2};
use kicad_ipc::proto::kiapi::schematic::types::{
    DirectiveLabel, GlobalLabel, Group, HierarchicalLabel, Junction, LocalLabel, NoConnectMarker,
    SchematicGraphicShape, SchematicImage, SchematicLine, SchematicLineType, SchematicRuleArea, SchematicTable,
    SchematicText, SchematicTextBox, SheetSymbol,
};
use prost::Name;

use crate::errors::from_ipc_error;
use crate::session::Session;

fn point(v: &Option<Vector2>) -> Point {
    v.as_ref()
        .map(|v| Point::new(Nm(v.x_nm), Nm(v.y_nm)))
        .unwrap_or_default()
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
        for s in self.list_symbols().await? {
            let position = Point::from_mm(s.position_mm.x, s.position_mm.y)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("{}: {e}", s.reference)))?;
            design.symbols.push(DesignSymbol {
                id: s.id.as_str().to_owned(),
                reference: s.reference,
                value: s.value,
                library_id: s.library_id,
                footprint: s.footprint,
                position,
                orientation: s.orientation,
                unit: s.unit,
            });
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
        let mut annotations = 0;
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
                    _ => annotations += 1,
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
                });
            } else if url == GlobalLabel::type_url() {
                unsupported("global_label");
            } else if url == HierarchicalLabel::type_url() {
                unsupported("hierarchical_label");
            } else if url == DirectiveLabel::type_url() {
                unsupported("directive_label");
            } else if url == NoConnectMarker::type_url() {
                unsupported("no_connect");
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
                    design.notes.push(kicad_domain::schematic::note_area(
                        point(&text.position),
                        &text.text,
                        Nm(1_270_000),
                    ));
                }
            } else if url == SchematicTextBox::type_url()
                || url == SchematicGraphicShape::type_url()
                || url == SchematicImage::type_url()
            {
                annotations += 1;
            }
        }
        design.wires = wires;
        design.labels = labels;
        design.junctions = junctions;
        design.annotations = annotations;
        Ok(design)
    }
}
