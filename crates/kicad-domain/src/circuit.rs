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
    /// Pins that are meant to stay unconnected; each gets a no-connect flag (a pin in a net cannot
    /// be one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_connects: Vec<PinRef>,
    /// Nets that carry a `PWR_FLAG` (a supply that comes from outside the circuit), each placed at
    /// its net and counted as no part.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub power_flags: Vec<String>,
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

    /// The spec with `power_flags` written out as parts: one `power:PWR_FLAG` on each named net
    /// that has none, with its pin in the net. Unknown nets are left for the validation to report.
    pub fn expanded(&self) -> CircuitSpec {
        let mut spec = self.clone();
        let is_flag = |spec: &CircuitSpec, pin: &PinRef| {
            spec.component(&pin.component_id)
                .is_some_and(|c| c.library_id.library() == "power" && c.library_id.entry() == "PWR_FLAG")
        };
        for name in &self.power_flags {
            let Some(at) = spec.nets.iter().position(|n| &n.name == name) else {
                continue;
            };
            if spec.nets[at].pins.iter().any(|p| is_flag(&spec, p)) {
                continue;
            }
            let base: String = format!(
                "flag_{}",
                name.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            );
            let mut id = base.clone();
            for n in 2.. {
                if spec.components.iter().all(|c| c.id.as_str() != id) {
                    break;
                }
                id = format!("{base}_{n}");
            }
            let Ok(id) = LogicalId::new(id.chars().take(64).collect::<String>()) else {
                continue;
            };
            spec.components.push(ComponentSpec {
                id: id.clone(),
                library_id: LibraryId::parse("power:PWR_FLAG").expect("a valid id"),
                reference_preference: None,
                value: "PWR_FLAG".into(),
                footprint: None,
                fields: BTreeMap::new(),
            });
            spec.nets[at].pins.push(PinRef {
                component_id: id,
                pin_number: "1".into(),
            });
        }
        spec.power_flags.clear();
        spec
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
