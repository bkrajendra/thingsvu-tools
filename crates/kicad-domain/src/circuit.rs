// SPDX-License-Identifier: GPL-3.0-or-later
//! Electrical intent: which components exist and which pins share a net. Coordinates are chosen
//! later by the layout engine, so a spec can be revised without rewriting geometry.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{LibraryId, LogicalId};

/// Highest spec format version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitSpec {
    pub schema_version: u32,
    pub name: LogicalId,
    #[serde(default)]
    pub description: String,
    pub components: Vec<ComponentSpec>,
    pub nets: Vec<NetSpec>,
    /// Free-form notes for humans; ignored by tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSpec {
    pub id: LogicalId,
    pub library_id: LibraryId,
    /// Preferred designator such as `R1`; the real one may differ after annotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_preference: Option<String>,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<String>,
    /// Additional symbol fields (name → value).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinRef {
    pub component_id: LogicalId,
    /// Pin number as printed in the symbol (may be alphanumeric, e.g. `A1`).
    pub pin_number: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetSpec {
    pub name: String,
    pub pins: Vec<PinRef>,
}

impl CircuitSpec {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn component(&self, id: &LogicalId) -> Option<&ComponentSpec> {
        self.components.iter().find(|c| &c.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

    #[test]
    fn loads_the_example_fixture() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        assert_eq!(spec.name.as_str(), "two_resistor_series_chain");
        assert_eq!(spec.components.len(), 2);
        assert_eq!(
            spec.nets.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["INPUT", "MID", "OUTPUT"]
        );
        let r1 = spec.component(&LogicalId::new("resistor_input").unwrap()).unwrap();
        assert_eq!(r1.library_id.to_string(), "Device:R");
    }

    #[test]
    fn rejects_unknown_fields() {
        let json = EXAMPLE.replacen("\"value\": \"10k\"", "\"value\": \"10k\", \"colour\": \"blue\"", 1);
        assert!(CircuitSpec::from_json(&json).is_err());
    }
}
