// SPDX-License-Identifier: GPL-3.0-or-later
//! Footprints and the checks that tie them to schematic parts (Phase 10).
//!
//! A footprint library entry is described by what a check needs: its pads, its courtyard, and its
//! text. The checks compare a part's pins with the footprint's pads and the package named in its
//! value or description with the footprint's name. The assistant chooses footprints; this checks
//! the choice and plans the change.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::geometry::{Nm, Point, Rect};
use crate::ids::LibraryId;
use crate::plan::{error, ExpectedNet, LibrarySymbol, OperationPlan, PropertyChange};
use crate::schematic::{NetInfo, SchematicDesign};
use crate::validation::{has_errors, Finding, Severity};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FootprintPad {
    pub number: String,
    /// `smd`, `thru_hole`, `np_thru_hole`, or `connect`.
    pub kind: String,
    /// Relative to the footprint origin (y down).
    pub at: Point,
    pub size: (Nm, Nm),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FootprintInfo {
    /// `Library:Name`.
    pub id: String,
    pub description: String,
    pub tags: String,
    pub pads: Vec<FootprintPad>,
    /// The courtyard's bounding box, when the footprint has one.
    pub courtyard: Option<Rect>,
}

impl FootprintInfo {
    /// The pad numbers that connect to something: numbered pads, not plain mounting holes.
    pub fn pad_numbers(&self) -> BTreeSet<String> {
        self.pads
            .iter()
            .filter(|p| p.kind != "np_thru_hole" && !p.number.trim().is_empty())
            .map(|p| p.number.clone())
            .collect()
    }
}

/// Package sizes and names worth comparing between a part's text and its footprint.
const PACKAGES: [&str; 22] = [
    "0201", "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2512", "SOT-23-5", "SOT-23-6", "SOT-23",
    "SOT-223", "SOT-89", "SOIC-8", "SOIC-16", "SSOP", "TSSOP", "MSOP", "QFN", "TQFP", "DIP-8",
];

fn squash(s: &str) -> String {
    s.to_ascii_uppercase().replace(['-', '_', ' '], "")
}

/// The package a piece of text names, if it names one.
fn package_in(text: &str) -> Option<&'static str> {
    let t = squash(text);
    // The longest name first, so SOT-23-5 is not read as SOT-23.
    let mut names: Vec<&'static str> = PACKAGES.to_vec();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    names.into_iter().find(|n| t.contains(&squash(n)))
}

/// A footprint to suggest for a plain passive, from its library symbol and the size asked for.
pub fn suggest_passive(library_id: &str, size: &str) -> Option<String> {
    let (lib, entry) = library_id.split_once(':')?;
    if lib != "Device" {
        return None;
    }
    let metric = match size {
        "0201" => "0603Metric",
        "0402" => "1005Metric",
        "0603" => "1608Metric",
        "0805" => "2012Metric",
        "1206" => "3216Metric",
        _ => return None,
    };
    Some(match entry {
        "R" | "R_Small" => format!("Resistor_SMD:R_{size}_{metric}"),
        "C" | "C_Small" => format!("Capacitor_SMD:C_{size}_{metric}"),
        "L" | "L_Small" => format!("Inductor_SMD:L_{size}_{metric}"),
        "LED" => format!("LED_SMD:LED_{size}_{metric}"),
        _ => return None,
    })
}

/// What the part's pins and the footprint's pads say about each other.
fn pin_numbers(def: &LibrarySymbol, unit_count_hint: u32) -> BTreeSet<String> {
    let _ = unit_count_hint;
    def.pins.iter().map(|p| p.number.clone()).collect()
}

/// Checks every part's footprint, and plans setting the ones in `assign` (`REF`, footprint id).
/// `footprints` holds every footprint the design and the assignments name; an id that is not in it
/// is reported as missing from the libraries.
pub fn plan_footprints(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    footprints: &BTreeMap<String, FootprintInfo>,
    assign: &[(String, String)],
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "footprints".into(),
        ..Default::default()
    };
    let mut findings: Vec<Finding> = Vec::new();
    let parts: BTreeMap<&str, &crate::schematic::DesignSymbol> = design
        .symbols
        .iter()
        .filter(|s| !s.reference.starts_with('#'))
        .map(|s| (s.reference.as_str(), s))
        .collect();

    let mut chosen: BTreeMap<&str, String> = parts.iter().map(|(r, s)| (*r, s.footprint.clone())).collect();
    let mut seen = BTreeSet::new();
    for (reference, footprint) in assign {
        if !seen.insert(reference.as_str()) {
            findings.push(error(
                "FOOTPRINT_TWICE",
                format!("{reference} is named twice"),
                vec![reference.clone()],
            ));
            continue;
        }
        let Some(part) = parts.get(reference.as_str()) else {
            findings.push(error(
                "FOOTPRINT_UNKNOWN_PART",
                format!("there is no part {reference}"),
                vec![reference.clone()],
            ));
            continue;
        };
        if !footprint.is_empty() && !footprints.contains_key(footprint) {
            findings.push(error(
                "FOOTPRINT_NOT_FOUND",
                format!("{reference}: the footprint '{footprint}' is not in the KiCad libraries (use fp_search)"),
                vec![reference.clone()],
            ));
            continue;
        }
        if part.footprint != *footprint {
            plan.property_changes.push(PropertyChange {
                id: part.id.clone(),
                reference: part.reference.clone(),
                name: "Footprint".into(),
                from: Some(part.footprint.clone()),
                to: footprint.clone(),
            });
        }
        chosen.insert(reference.as_str(), footprint.clone());
    }

    let mut missing = Vec::new();
    for (reference, part) in &parts {
        let id = chosen.get(reference).cloned().unwrap_or_default();
        if part.exclude_from_board {
            continue;
        }
        if id.is_empty() {
            let size = package_in(&format!(
                "{} {}",
                part.value,
                part.properties.get("Description").cloned().unwrap_or_default()
            ))
            .filter(|p| p.len() == 4)
            .unwrap_or("0603");
            let suggestion = suggest_passive(&part.library_id, size);
            findings.push(Finding {
                severity: Severity::Warning,
                code: "FOOTPRINT_MISSING",
                message: match suggestion {
                    Some(s) => format!("{reference} has no footprint; for a {size} part, {s} would fit"),
                    None => format!(
                        "{reference} ({}) has no footprint; search with fp_search",
                        part.library_id
                    ),
                },
                objects: vec![(*reference).to_owned()],
            });
            missing.push((*reference).to_owned());
            continue;
        }
        let Some(fp) = footprints.get(&id) else {
            findings.push(Finding {
                severity: Severity::Warning,
                code: "FOOTPRINT_NOT_FOUND",
                message: format!("{reference}: the footprint '{id}' was not found in the KiCad libraries (a project or user library is not read)"),
                objects: vec![(*reference).to_owned()],
            });
            continue;
        };
        // Pin numbers against pad numbers.
        if let Some(def) = LibraryId::parse(&part.library_id).ok().and_then(|l| library.get(&l)) {
            let pins = pin_numbers(def, def.unit_count);
            let pads = fp.pad_numbers();
            let no_pad: Vec<&String> = pins.difference(&pads).collect();
            if !no_pad.is_empty() {
                findings.push(Finding {
                    severity: Severity::Error,
                    code: "FOOTPRINT_PADS_MISSING",
                    message: format!(
                        "{reference}: {id} has no pad for pin(s) {}; the board would leave them unconnected",
                        no_pad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                    objects: vec![(*reference).to_owned()],
                });
            }
            let extra = pads.difference(&pins).count();
            if extra > 0 && no_pad.is_empty() {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: "FOOTPRINT_EXTRA_PADS",
                    message: format!(
                        "{reference}: {id} has {extra} pad(s) with no pin (an exposed pad or a shield is normal)"
                    ),
                    objects: vec![(*reference).to_owned()],
                });
            }
        }
        // The package the value or description names, against the footprint.
        let text = format!(
            "{} {}",
            part.value,
            part.properties.get("Description").cloned().unwrap_or_default()
        );
        if let Some(want) = package_in(&text) {
            let have = package_in(&id);
            if have != Some(want) && !squash(&id).contains(&squash(want)) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: "FOOTPRINT_PACKAGE_MISMATCH",
                    message: format!("{reference}: the text says {want} but the footprint is {id}"),
                    objects: vec![(*reference).to_owned()],
                });
            }
        }
    }
    if !missing.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "FOOTPRINTS_MISSING",
            message: format!(
                "{} part(s) still have no footprint: {}",
                missing.len(),
                missing.join(", ")
            ),
            objects: missing,
        });
    }
    if plan.property_changes.is_empty() && !has_errors(&findings) && assign.is_empty() {
        // A check only: nothing to apply.
        findings.push(Finding {
            severity: Severity::Warning,
            code: "NOTHING_TO_CHANGE",
            message: "no footprint was assigned; this is a check".into(),
            objects: vec![],
        });
    }
    plan.expected_nets = crate::edits::expected_from(nets);
    plan.findings = findings;
    let _: Option<ExpectedNet> = None;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::LibraryPin;
    use crate::schematic::DesignSymbol;

    fn pin(n: &str) -> LibraryPin {
        LibraryPin {
            number: n.into(),
            name: n.into(),
            local: Point::default(),
            unit: 0,
            outward: crate::geometry::Dir::Up,
            electrical_type: "passive".into(),
        }
    }

    fn library(pins: &[&str]) -> BTreeMap<LibraryId, LibrarySymbol> {
        let id = LibraryId::parse("Device:R").unwrap();
        BTreeMap::from([(
            id.clone(),
            LibrarySymbol {
                library_id: id,
                reference_prefix: "R".into(),
                unit_count: 1,
                is_power: false,
                body: None,
                pins: pins.iter().map(|p| pin(p)).collect(),
            },
        )])
    }

    fn footprint(id: &str, pads: &[&str]) -> FootprintInfo {
        FootprintInfo {
            id: id.into(),
            description: String::new(),
            tags: String::new(),
            pads: pads
                .iter()
                .map(|n| FootprintPad {
                    number: (*n).into(),
                    kind: "smd".into(),
                    at: Point::default(),
                    size: (Nm(1_000_000), Nm(1_000_000)),
                })
                .collect(),
            courtyard: None,
        }
    }

    fn design(value: &str, footprint: &str) -> SchematicDesign {
        SchematicDesign {
            symbols: vec![DesignSymbol {
                id: "u1".into(),
                reference: "R1".into(),
                value: value.into(),
                footprint: footprint.into(),
                library_id: "Device:R".into(),
                unit: 1,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn codes(plan: &OperationPlan) -> Vec<&'static str> {
        plan.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn a_missing_footprint_is_reported_with_a_suggestion() {
        let plan = plan_footprints(
            &design("10k 0805", ""),
            &[],
            &library(&["1", "2"]),
            &BTreeMap::new(),
            &[],
        );
        let f = plan.findings.iter().find(|f| f.code == "FOOTPRINT_MISSING").unwrap();
        assert!(f.message.contains("R_0805_2012Metric"), "{}", f.message);
    }

    #[test]
    fn assigning_a_footprint_plans_the_change_and_checks_pads() {
        let fps = BTreeMap::from([(
            "Resistor_SMD:R_0603_1608Metric".to_owned(),
            footprint("Resistor_SMD:R_0603_1608Metric", &["1", "2"]),
        )]);
        let plan = plan_footprints(
            &design("10k", ""),
            &[],
            &library(&["1", "2"]),
            &fps,
            &[("R1".into(), "Resistor_SMD:R_0603_1608Metric".into())],
        );
        assert!(!has_errors(&plan.findings), "{:#?}", plan.findings);
        assert_eq!(plan.property_changes.len(), 1);
        assert_eq!(plan.property_changes[0].to, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn a_footprint_with_too_few_pads_is_an_error() {
        let fps = BTreeMap::from([("X:One".to_owned(), footprint("X:One", &["1"]))]);
        let plan = plan_footprints(&design("10k", "X:One"), &[], &library(&["1", "2"]), &fps, &[]);
        assert!(codes(&plan).contains(&"FOOTPRINT_PADS_MISSING"), "{:#?}", plan.findings);
    }

    #[test]
    fn a_package_in_the_text_must_match_the_footprint() {
        let fps = BTreeMap::from([(
            "Resistor_SMD:R_0603_1608Metric".to_owned(),
            footprint("Resistor_SMD:R_0603_1608Metric", &["1", "2"]),
        )]);
        let plan = plan_footprints(
            &design("10k 0805", "Resistor_SMD:R_0603_1608Metric"),
            &[],
            &library(&["1", "2"]),
            &fps,
            &[],
        );
        assert!(
            codes(&plan).contains(&"FOOTPRINT_PACKAGE_MISMATCH"),
            "{:#?}",
            plan.findings
        );
        let fine = plan_footprints(
            &design("10k", "Resistor_SMD:R_0603_1608Metric"),
            &[],
            &library(&["1", "2"]),
            &fps,
            &[],
        );
        assert!(!codes(&fine).contains(&"FOOTPRINT_PACKAGE_MISMATCH"));
    }

    #[test]
    fn unknown_footprints_and_parts_are_refused() {
        let plan = plan_footprints(
            &design("10k", ""),
            &[],
            &library(&["1", "2"]),
            &BTreeMap::new(),
            &[("R1".into(), "Nope:Nothing".into()), ("R9".into(), "A:B".into())],
        );
        assert!(codes(&plan).contains(&"FOOTPRINT_NOT_FOUND"));
        assert!(codes(&plan).contains(&"FOOTPRINT_UNKNOWN_PART"));
        assert!(has_errors(&plan.findings));
    }

    #[test]
    fn long_package_names_win_over_their_prefixes() {
        assert_eq!(package_in("SOT-23-5 regulator"), Some("SOT-23-5"));
        assert_eq!(package_in("Package_TO_SOT_SMD:SOT-23"), Some("SOT-23"));
        assert_eq!(package_in("LM358"), None);
    }
}
