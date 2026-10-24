// SPDX-License-Identifier: GPL-3.0-or-later
//! Checks on a circuit spec before any KiCad call. These prove the spec is self-consistent, not
//! that the circuit is electrically correct.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::circuit::{CircuitSpec, PinRef, SCHEMA_VERSION};

/// MVP limits (plan §10); large specs are rejected rather than partially processed.
pub const MAX_COMPONENTS: usize = 50;
pub const MAX_NETS: usize = 200;
pub const MAX_TEXT_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    /// Logical ids or net names the finding is about.
    pub objects: Vec<String>,
}

impl Finding {
    fn error(code: &'static str, message: String, objects: Vec<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message,
            objects,
        }
    }

    fn warning(code: &'static str, message: String, objects: Vec<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message,
            objects,
        }
    }
}

/// Returns every finding; the spec is usable only if none has `Severity::Error`.
pub fn validate_circuit(spec: &CircuitSpec) -> Vec<Finding> {
    let mut out = Vec::new();

    if spec.schema_version != SCHEMA_VERSION {
        out.push(Finding::error(
            "UNSUPPORTED_SCHEMA_VERSION",
            format!(
                "schema_version {} is not supported (expected {SCHEMA_VERSION})",
                spec.schema_version
            ),
            vec![],
        ));
    }
    if spec.components.len() > MAX_COMPONENTS {
        out.push(Finding::error(
            "TOO_MANY_COMPONENTS",
            format!(
                "{} components exceed the limit of {MAX_COMPONENTS}",
                spec.components.len()
            ),
            vec![],
        ));
    }
    if spec.nets.len() > MAX_NETS {
        out.push(Finding::error(
            "TOO_MANY_NETS",
            format!("{} nets exceed the limit of {MAX_NETS}", spec.nets.len()),
            vec![],
        ));
    }

    let mut ids = BTreeSet::new();
    let mut references = BTreeMap::new();
    for c in &spec.components {
        if !ids.insert(&c.id) {
            out.push(Finding::error(
                "DUPLICATE_COMPONENT_ID",
                format!("component id '{}' is used twice", c.id),
                vec![c.id.to_string()],
            ));
        }
        if let Some(r) = &c.reference_preference {
            if !is_reference(r) {
                out.push(Finding::error(
                    "INVALID_REFERENCE",
                    format!("reference '{r}' must be letters followed by digits, like R1"),
                    vec![c.id.to_string()],
                ));
            } else if let Some(other) = references.insert(r.as_str(), &c.id) {
                out.push(Finding::error(
                    "DUPLICATE_REFERENCE",
                    format!("reference '{r}' is preferred by both '{other}' and '{}'", c.id),
                    vec![other.to_string(), c.id.to_string()],
                ));
            }
        }
        for text in [&c.value].into_iter().chain(c.fields.values()) {
            if text.len() > MAX_TEXT_LEN || text.chars().any(char::is_control) {
                out.push(Finding::error(
                    "INVALID_TEXT",
                    format!(
                        "a text value of '{}' is longer than {MAX_TEXT_LEN} characters or contains control characters",
                        c.id
                    ),
                    vec![c.id.to_string()],
                ));
            }
        }
    }

    let mut net_names = BTreeSet::new();
    let mut pin_owner: BTreeMap<&PinRef, &str> = BTreeMap::new();
    for net in &spec.nets {
        if !is_net_name(&net.name) {
            out.push(Finding::error(
                "INVALID_NET_NAME",
                format!(
                    "net name '{}' must be 1–{MAX_TEXT_LEN} printable characters without spaces",
                    net.name
                ),
                vec![net.name.clone()],
            ));
        }
        if !net_names.insert(net.name.as_str()) {
            out.push(Finding::error(
                "DUPLICATE_NET",
                format!("net '{}' is defined twice", net.name),
                vec![net.name.clone()],
            ));
        }
        if net.pins.is_empty() {
            out.push(Finding::error(
                "EMPTY_NET",
                format!("net '{}' has no pins", net.name),
                vec![net.name.clone()],
            ));
        } else if net.pins.len() == 1 {
            out.push(Finding::warning(
                "SINGLE_PIN_NET",
                format!("net '{}' connects only one pin; KiCad ERC will report it", net.name),
                vec![net.name.clone()],
            ));
        }
        for pin in &net.pins {
            if !ids.contains(&pin.component_id) {
                out.push(Finding::error(
                    "UNKNOWN_COMPONENT",
                    format!("net '{}' refers to unknown component '{}'", net.name, pin.component_id),
                    vec![net.name.clone(), pin.component_id.to_string()],
                ));
            }
            if pin.pin_number.is_empty() || pin.pin_number.len() > 32 || pin.pin_number.chars().any(char::is_whitespace)
            {
                out.push(Finding::error(
                    "INVALID_PIN_NUMBER",
                    format!(
                        "pin number '{}' of '{}' is empty, too long, or contains whitespace",
                        pin.pin_number, pin.component_id
                    ),
                    vec![pin.component_id.to_string()],
                ));
            }
            if let Some(other) = pin_owner.insert(pin, &net.name) {
                out.push(Finding::error(
                    "PIN_IN_TWO_NETS",
                    format!(
                        "pin {}.{} is in both '{other}' and '{}'",
                        pin.component_id, pin.pin_number, net.name
                    ),
                    vec![pin.component_id.to_string(), other.to_owned(), net.name.clone()],
                ));
            }
        }
    }
    out
}

pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

fn is_reference(r: &str) -> bool {
    let letters = r.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    letters > 0 && letters < r.len() && r[letters..].chars().all(|c| c.is_ascii_digit()) && r.len() <= 16
}

fn is_net_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_TEXT_LEN && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

    fn codes(spec: &CircuitSpec) -> Vec<&'static str> {
        validate_circuit(spec).into_iter().map(|f| f.code).collect()
    }

    #[test]
    fn example_has_only_single_pin_warnings() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let findings = validate_circuit(&spec);
        assert!(!has_errors(&findings));
        // INPUT and OUTPUT each have one pin; KiCad's ERC reported the same two (isolated_pin_label).
        assert_eq!(codes(&spec), ["SINGLE_PIN_NET", "SINGLE_PIN_NET"]);
    }

    #[test]
    fn detects_structural_errors() {
        let mut spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        spec.components[1].reference_preference = Some("R1".into());
        spec.nets[2].pins[0].pin_number = "1".into(); // R2.1 is already in MID
        spec.nets[0].name = "MID".into();
        spec.nets[0].pins[0].component_id = crate::ids::LogicalId::new("ghost").unwrap();
        let found = codes(&spec);
        for expected in [
            "DUPLICATE_REFERENCE",
            "PIN_IN_TWO_NETS",
            "DUPLICATE_NET",
            "UNKNOWN_COMPONENT",
        ] {
            assert!(found.contains(&expected), "missing {expected} in {found:?}");
        }
    }

    #[test]
    fn reference_format() {
        assert!(is_reference("R1") && is_reference("U12"));
        assert!(!is_reference("R") && !is_reference("1R") && !is_reference("R1a"));
    }
}
