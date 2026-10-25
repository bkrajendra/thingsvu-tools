// SPDX-License-Identifier: GPL-3.0-or-later
//! Schematic read operations over IPC, converted to domain types.

use std::collections::HashMap;

use kicad_domain::geometry::Orientation;
use kicad_domain::schematic::{pin_world_position, point_nm};
use kicad_domain::{Capability, ErrorCode, Kiid, NetInfo, NetPin, PinInfo, SymbolInfo, SymbolPins, ToolError};
use kicad_ipc::proto::kiapi::common::commands::{
    GetItems, GetItemsResponse, GetOpenDocuments, GetOpenDocumentsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    document_specifier, DocumentType, ElectricalPinType, ItemHeader, KiCadObjectType, Kiid as ProtoKiid,
};
use kicad_ipc::proto::kiapi::schematic::commands::{GetSchematicNetlist, SchematicNetlistResponse};
use kicad_ipc::proto::kiapi::schematic::types::{
    SchematicField, SchematicPin, SchematicSymbolInstance, SchematicSymbolOrientation,
};
use prost::Name;
use serde::Serialize;

use crate::errors::from_ipc_error;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenDocument {
    pub kind: &'static str,
    /// Project name and folder when KiCad reports them.
    pub project: Option<String>,
    pub project_path: Option<String>,
    /// File name when KiCad identifies the document by file only.
    pub file: Option<String>,
}

fn text(field: &Option<SchematicField>) -> String {
    field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

fn kiid(id: &Option<ProtoKiid>, what: &str) -> Result<Kiid, ToolError> {
    let raw = id.as_ref().map(|k| k.value.clone()).unwrap_or_default();
    Kiid::new(raw).map_err(|e| {
        ToolError::new(
            ErrorCode::KicadError,
            format!("KiCad returned a {what} with a bad id: {e}"),
        )
    })
}

fn orientation(symbol: &SchematicSymbolInstance) -> Orientation {
    let t = symbol.transform.unwrap_or_default();
    let rotation_deg =
        match SchematicSymbolOrientation::try_from(t.orientation).unwrap_or(SchematicSymbolOrientation::Sso0) {
            SchematicSymbolOrientation::Sso90 => 90,
            SchematicSymbolOrientation::Sso180 => 180,
            SchematicSymbolOrientation::Sso270 => 270,
            _ => 0,
        };
    Orientation {
        rotation_deg,
        mirror_x: t.mirror_x,
        mirror_y: t.mirror_y,
    }
}

pub(crate) fn electrical_type(value: i32) -> &'static str {
    match ElectricalPinType::try_from(value).unwrap_or(ElectricalPinType::EptUnknown) {
        ElectricalPinType::EptInput => "input",
        ElectricalPinType::EptOutput => "output",
        ElectricalPinType::EptBidirectional => "bidirectional",
        ElectricalPinType::EptTristate => "tri_state",
        ElectricalPinType::EptPassive => "passive",
        ElectricalPinType::EptFree => "free",
        ElectricalPinType::EptUnspecified => "unspecified",
        ElectricalPinType::EptPowerInput => "power_in",
        ElectricalPinType::EptPowerOutput => "power_out",
        ElectricalPinType::EptOpenCollector => "open_collector",
        ElectricalPinType::EptOpenEmitter => "open_emitter",
        ElectricalPinType::EptNoConnect => "no_connect",
        ElectricalPinType::EptUnknown => "unknown",
    }
}

pub(crate) fn symbol_info(s: &SchematicSymbolInstance) -> Result<SymbolInfo, ToolError> {
    let lib = s
        .lib_id
        .clone()
        .or_else(|| s.definition.as_ref().and_then(|d| d.id.clone()))
        .unwrap_or_default();
    let position = s.position.unwrap_or_default();
    Ok(SymbolInfo {
        id: kiid(&s.id, "symbol")?,
        reference: text(&s.reference_field),
        value: text(&s.value_field),
        library_id: format!("{}:{}", lib.library_nickname, lib.entry_name),
        footprint: text(&s.footprint_field),
        position_mm: point_nm(position.x_nm, position.y_nm).into(),
        orientation: orientation(s),
        unit: s.unit.map(|u| u.unit.max(0) as u32).unwrap_or(1),
        sheet_path: s
            .path
            .as_ref()
            .map(|p| p.path_human_readable.clone())
            .unwrap_or_default(),
    })
}

/// Pins of the instance's unit (pins with no unit, or unit 0, belong to every unit).
pub(crate) fn symbol_pins(s: &SchematicSymbolInstance) -> Result<Vec<PinInfo>, ToolError> {
    let unit = s.unit.map(|u| u.unit).unwrap_or(1);
    let position = s.position.unwrap_or_default();
    let origin = point_nm(position.x_nm, position.y_nm);
    let orient = orientation(s);
    let mut pins = Vec::new();
    for child in s.definition.iter().flat_map(|d| d.items.iter()) {
        let Some(any) = &child.item else { continue };
        if any.type_url != SchematicPin::type_url() {
            continue;
        }
        if child.unit.is_some_and(|u| u.unit != 0 && u.unit != unit) {
            continue;
        }
        let pin = any
            .to_msg::<SchematicPin>()
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("undecodable pin: {e}")))?;
        let p = pin.position.unwrap_or_default();
        let local = point_nm(p.x_nm, p.y_nm);
        let world = pin_world_position(origin, orient, local).ok_or_else(|| {
            ToolError::new(
                ErrorCode::KicadError,
                format!("unsupported rotation {}°", orient.rotation_deg),
            )
        })?;
        pins.push(PinInfo {
            id: kiid(&pin.id, "pin")?,
            number: pin.number,
            name: pin.name,
            electrical_type: electrical_type(pin.electrical_type).to_owned(),
            position_mm: world.into(),
            local_mm: local.into(),
        });
    }
    pins.sort_by_key(|p| natural_key(&p.number));
    Ok(pins)
}

/// Sorts "2" before "10" and keeps alphanumeric pin numbers stable.
pub(crate) fn natural_key(number: &str) -> (u64, String) {
    (number.parse::<u64>().unwrap_or(u64::MAX), number.to_owned())
}

impl Session {
    pub(crate) fn require(&self, capability: Capability) -> Result<(), ToolError> {
        if self.capabilities().is_available(capability) {
            return Ok(());
        }
        let note = self
            .capabilities()
            .get(capability)
            .and_then(|s| s.note.clone())
            .unwrap_or_default();
        Err(ToolError::new(
            ErrorCode::FeatureUnavailable,
            format!("this session cannot do {capability:?}: {note}"),
        ))
    }

    pub(crate) async fn symbol_instances(&self) -> Result<Vec<SchematicSymbolInstance>, ToolError> {
        self.require(Capability::SchReadSymbols)?;
        let mut inner = self.inner.lock().await;
        let r: GetItemsResponse = inner
            .client
            .call(&GetItems {
                header: Some(ItemHeader {
                    document: Some(self.document.clone()),
                    ..Default::default()
                }),
                types: vec![KiCadObjectType::KotSchSymbol as i32],
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        r.items
            .iter()
            .filter(|any| any.type_url == SchematicSymbolInstance::type_url())
            .map(|any| {
                any.to_msg::<SchematicSymbolInstance>()
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("undecodable symbol: {e}")))
            })
            .collect()
    }

    /// Every placed symbol, sorted by reference.
    pub async fn list_symbols(&self) -> Result<Vec<SymbolInfo>, ToolError> {
        let mut symbols = self
            .symbol_instances()
            .await?
            .iter()
            .map(symbol_info)
            .collect::<Result<Vec<_>, _>>()?;
        symbols.sort_by_key(|s| natural_reference(&s.reference));
        Ok(symbols)
    }

    /// Pins of one symbol, selected by KIID or by a reference that matches exactly one symbol.
    pub async fn symbol_pins(&self, selector: &str) -> Result<SymbolPins, ToolError> {
        let instances = self.symbol_instances().await?;
        let by_id = Kiid::new(selector).ok();
        let matches: Vec<&SchematicSymbolInstance> = instances
            .iter()
            .filter(|s| match &by_id {
                Some(id) => s.id.as_ref().map(|k| k.value.to_ascii_lowercase()) == Some(id.as_str().to_owned()),
                None => text(&s.reference_field) == selector,
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(SymbolPins {
                symbol: symbol_info(one)?,
                pins: symbol_pins(one)?,
            }),
            [] => Err(ToolError::new(
                ErrorCode::SymbolNotFound,
                format!("no symbol matches '{selector}'"),
            )),
            many => Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "'{selector}' matches {} symbols; use one of their ids: {}",
                    many.len(),
                    many.iter()
                        .filter_map(|s| s.id.as_ref().map(|k| k.value.clone()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    /// Connectivity from KiCad's netlister, with pin KIIDs resolved to `REF.PIN`.
    pub async fn netlist(&self) -> Result<Vec<NetInfo>, ToolError> {
        self.require(Capability::SchNetlist)?;
        let instances = self.symbol_instances().await?;
        let mut pin_owner: HashMap<String, NetPin> = HashMap::new();
        for s in &instances {
            let info = symbol_info(s)?;
            for pin in symbol_pins(s)? {
                pin_owner.insert(
                    pin.id.as_str().to_owned(),
                    NetPin {
                        reference: info.reference.clone(),
                        pin: pin.number,
                        symbol_id: info.id.clone(),
                    },
                );
            }
        }
        let response: SchematicNetlistResponse = {
            let mut inner = self.inner.lock().await;
            inner
                .client
                .call(&GetSchematicNetlist {
                    document: Some(self.document.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|e| from_ipc_error(&e, false))?
        };
        let mut nets: Vec<NetInfo> = response
            .nets
            .iter()
            .map(|net| {
                let mut pins = Vec::new();
                let mut other_items = 0;
                for id in net.sheets.iter().flat_map(|s| s.items.iter()) {
                    match pin_owner.get(&id.value.to_ascii_lowercase()) {
                        Some(pin) => pins.push(pin.clone()),
                        None => other_items += 1,
                    }
                }
                pins.sort_by(|a, b| {
                    (natural_reference(&a.reference), natural_key(&a.pin))
                        .cmp(&(natural_reference(&b.reference), natural_key(&b.pin)))
                });
                NetInfo {
                    name: net.name.clone(),
                    pins,
                    other_items,
                }
            })
            .collect();
        nets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(nets)
    }

    /// Documents open in this session's KiCad instance.
    pub async fn open_documents(&self) -> Result<Vec<OpenDocument>, ToolError> {
        self.require(Capability::SessionOpenDocuments)?;
        let mut inner = self.inner.lock().await;
        let mut out = Vec::new();
        for (kind, doc_type) in [
            ("schematic", DocumentType::DoctypeSchematic),
            ("pcb", DocumentType::DoctypePcb),
        ] {
            match inner
                .client
                .call::<_, GetOpenDocumentsResponse>(&GetOpenDocuments {
                    r#type: doc_type as i32,
                })
                .await
            {
                Ok(r) => out.extend(r.documents.into_iter().map(|d| OpenDocument {
                    kind,
                    project: d.project.as_ref().map(|p| p.name.clone()),
                    project_path: d.project.as_ref().map(|p| p.path.clone()),
                    file: match d.identifier {
                        Some(document_specifier::Identifier::BoardFilename(f)) => Some(f),
                        _ => None,
                    },
                })),
                // An editor type with nothing open answers "unhandled".
                Err(e) if e.is_unhandled() => {}
                Err(e) => return Err(from_ipc_error(&e, false)),
            }
        }
        Ok(out)
    }
}

/// Sorts R2 before R10.
pub(crate) fn natural_reference(reference: &str) -> (String, u64, String) {
    let prefix: String = reference.chars().take_while(|c| !c.is_ascii_digit()).collect();
    let number = reference[prefix.len()..].parse::<u64>().unwrap_or(u64::MAX);
    (prefix, number, reference.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sorting() {
        let mut refs = vec!["R10", "R2", "C1", "R1"];
        refs.sort_by_key(|r| natural_reference(r));
        assert_eq!(refs, ["C1", "R1", "R2", "R10"]);
        let mut pins = vec!["10", "2", "A1", "1"];
        pins.sort_by_key(|p| natural_key(p));
        assert_eq!(pins, ["1", "2", "10", "A1"]);
    }
}
