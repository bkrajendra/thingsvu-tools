// SPDX-License-Identifier: GPL-3.0-or-later
//! Operation plans: the exact changes a circuit spec turns into, computed before anything is
//! applied (plan §7.2, §8.3). Geometry comes from the layout engine in [`crate::layout`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::circuit::CircuitSpec;
use crate::geometry::{Dir, Nm, Orientation, Point, Rect};
use crate::ids::{LibraryId, LogicalId};
use crate::layout;
use crate::validation::{has_errors, validate_circuit, Finding, Severity};

/// Upper bound on operations in one plan (plan §10 suggested 50; drawn schematics need more).
pub const MAX_OPERATIONS: usize = 400;

/// Geometry of a library symbol, from KiCad's library before anything is placed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibrarySymbol {
    pub library_id: LibraryId,
    /// Reference prefix from the library (`R`, `Q`, `U`, `#PWR`, …).
    pub reference_prefix: String,
    pub unit_count: u32,
    /// A power symbol (`power:+5V`, `power:GND`, `PWR_FLAG`): names or flags a net.
    pub is_power: bool,
    /// Bounding box of the symbol's graphics around its origin (y down), if it has any.
    pub body: Option<Rect>,
    pub pins: Vec<LibraryPin>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryPin {
    pub number: String,
    pub name: String,
    /// Connection point relative to the symbol origin (y down), unrotated.
    pub local: Point,
    /// 0 = shared by all units.
    pub unit: u32,
    /// Direction the pin points away from the symbol body, unrotated.
    pub outward: Dir,
    /// KiCad electrical type, e.g. `passive`, `power_in`.
    pub electrical_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    PlaceSymbol {
        component_id: LogicalId,
        library_id: LibraryId,
        reference: String,
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        footprint: Option<String>,
        position: Point,
        orientation: Orientation,
    },
    /// A power symbol (`+5V`, `GND`, …) that names a net; not part of the circuit spec.
    PlacePowerSymbol {
        net: String,
        library_id: LibraryId,
        reference: String,
        position: Point,
        orientation: Orientation,
    },
    AddWire {
        net: String,
        start: Point,
        end: Point,
    },
    AddLabel {
        net: String,
        position: Point,
        /// Direction the label text extends from its anchor.
        direction: Dir,
    },
    AddJunction {
        position: Point,
    },
}

/// The pins a net must contain after applying, as `REF.PIN`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectedNet {
    pub name: String,
    pub pins: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OperationPlan {
    pub circuit: String,
    pub operations: Vec<Operation>,
    pub expected_nets: Vec<ExpectedNet>,
    /// Component logical id → planned reference.
    pub references: BTreeMap<LogicalId, String>,
    pub findings: Vec<Finding>,
    /// Nets the layout could not wire and joined with labels instead.
    pub fallback_nets: Vec<String>,
    /// Geometric checks of the drawing (shorts, overlaps, wires through parts).
    pub readability: crate::readability::Readability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct OperationCounts {
    pub symbols: usize,
    pub power_symbols: usize,
    pub wires: usize,
    pub labels: usize,
    pub junctions: usize,
}

impl OperationPlan {
    pub fn counts(&self) -> OperationCounts {
        let mut c = OperationCounts::default();
        for op in &self.operations {
            match op {
                Operation::PlaceSymbol { .. } => c.symbols += 1,
                Operation::PlacePowerSymbol { .. } => c.power_symbols += 1,
                Operation::AddWire { .. } => c.wires += 1,
                Operation::AddLabel { .. } => c.labels += 1,
                Operation::AddJunction { .. } => c.junctions += 1,
            }
        }
        c
    }
}

/// What already exists in the target document, so the plan does not collide with it.
#[derive(Debug, Clone, Default)]
pub struct ExistingDocument {
    pub references: BTreeSet<String>,
    /// Lowest occupied y: new parts are placed below it.
    pub max_y: Option<Nm>,
}

fn error(code: &'static str, message: String, objects: Vec<String>) -> Finding {
    Finding {
        severity: Severity::Error,
        code,
        message,
        objects,
    }
}

/// Turns `spec` into a plan. `library` must contain every component's symbol, plus the power
/// symbols available for net names (see [`layout::library_ids_for`]). A plan whose findings
/// include an error must not be applied.
pub fn plan_circuit(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    existing: &ExistingDocument,
) -> OperationPlan {
    let mut findings = validate_circuit(spec);
    let mut plan = OperationPlan {
        circuit: spec.name.to_string(),
        operations: vec![],
        expected_nets: vec![],
        references: BTreeMap::new(),
        findings: vec![],
        fallback_nets: vec![],
        readability: Default::default(),
    };
    if has_errors(&findings) {
        plan.findings = findings;
        return plan;
    }

    // Library checks.
    for c in &spec.components {
        match library.get(&c.library_id) {
            None => findings.push(error(
                "UNKNOWN_LIBRARY_SYMBOL",
                format!("{} is not in the KiCad symbol libraries", c.library_id),
                vec![c.id.to_string()],
            )),
            Some(sym) if sym.unit_count > 1 => findings.push(error(
                "MULTI_UNIT_UNSUPPORTED",
                format!(
                    "{} has {} units; multi-unit symbols are not supported yet",
                    c.library_id, sym.unit_count
                ),
                vec![c.id.to_string()],
            )),
            Some(_) => {}
        }
    }
    for net in &spec.nets {
        for pin_ref in &net.pins {
            let component = spec.component(&pin_ref.component_id).expect("validated");
            if let Some(symbol) = library.get(&component.library_id) {
                if !symbol
                    .pins
                    .iter()
                    .any(|p| p.number == pin_ref.pin_number && (p.unit == 0 || p.unit == 1))
                {
                    findings.push(error(
                        "UNKNOWN_PIN",
                        format!(
                            "{} has no pin '{}' (pins: {})",
                            component.library_id,
                            pin_ref.pin_number,
                            symbol
                                .pins
                                .iter()
                                .map(|p| p.number.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        vec![component.id.to_string(), net.name.clone()],
                    ));
                }
            }
        }
    }
    if has_errors(&findings) {
        plan.findings = findings;
        return plan;
    }

    // References: keep a free preferred one, otherwise the library prefix plus the next free number.
    let mut taken: BTreeSet<String> = existing.references.clone();
    for c in &spec.components {
        if let Some(r) = &c.reference_preference {
            if taken.contains(r) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: "REFERENCE_TAKEN",
                    message: format!("{r} already exists in the schematic; '{}' gets another reference", c.id),
                    objects: vec![c.id.to_string()],
                });
            }
        }
    }
    for c in &spec.components {
        let reference = match &c.reference_preference {
            Some(r) if !taken.contains(r) => r.clone(),
            _ => {
                let prefix = &library[&c.library_id].reference_prefix;
                // Power-library parts conventionally use two-digit numbers (#PWR01, #FLG01).
                let width = if prefix.starts_with('#') { 2 } else { 1 };
                (1..)
                    .map(|n| format!("{prefix}{n:0width$}"))
                    .find(|r| !taken.contains(r))
                    .expect("unbounded range")
            }
        };
        taken.insert(reference.clone());
        plan.references.insert(c.id.clone(), reference);
    }

    for net in &spec.nets {
        let pins = net
            .pins
            .iter()
            .map(|p| format!("{}.{}", plan.references[&p.component_id], p.pin_number))
            .collect();
        plan.expected_nets.push(ExpectedNet {
            name: net.name.clone(),
            pins,
        });
    }

    let out = layout::layout(spec, library, &plan.references, &mut taken, existing.max_y);
    plan.operations = out.operations;
    plan.fallback_nets = out.fallback_nets;
    findings.extend(out.findings);

    plan.readability = crate::readability::check(&plan, library);
    for (a, b) in &plan.readability.shorts {
        findings.push(error(
            "LAYOUT_SHORT",
            format!("the drawing would join nets '{a}' and '{b}'"),
            vec![a.clone(), b.clone()],
        ));
    }
    for (a, b) in &plan.readability.overlapping_symbols {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "SYMBOLS_OVERLAP",
            message: format!("{a} and {b} overlap"),
            objects: vec![a.clone(), b.clone()],
        });
    }
    for (net, reference) in &plan.readability.wires_through_symbols {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "WIRE_THROUGH_SYMBOL",
            message: format!("a wire of '{net}' passes through {reference}"),
            objects: vec![net.clone(), reference.clone()],
        });
    }
    if plan.operations.len() > MAX_OPERATIONS {
        findings.push(error(
            "PLAN_TOO_LARGE",
            format!(
                "the plan has {} operations; the limit is {MAX_OPERATIONS}",
                plan.operations.len()
            ),
            vec![],
        ));
    }
    plan.findings = findings;
    plan
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::geometry::GRID;

    const EXAMPLE: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");
    const REGULATOR: &str = include_str!("../../../docs/examples/regulator-3v3.json");

    fn mm(x: f64, y: f64) -> Point {
        Point::from_mm(x, y).unwrap()
    }

    fn pin(number: &str, x: f64, y: f64, outward: Dir) -> LibraryPin {
        LibraryPin {
            number: number.into(),
            name: String::new(),
            local: mm(x, y),
            unit: 1,
            outward,
            electrical_type: "passive".into(),
        }
    }

    fn symbol(id: &str, prefix: &str, body: (f64, f64, f64, f64), pins: Vec<LibraryPin>) -> LibrarySymbol {
        LibrarySymbol {
            library_id: LibraryId::parse(id).unwrap(),
            reference_prefix: prefix.into(),
            unit_count: 1,
            is_power: id.starts_with("power:"),
            body: Rect::from_points([mm(body.0, body.1), mm(body.2, body.3)]),
            pins,
        }
    }

    /// Pin geometry as KiCad 10.99 reports it (y down).
    pub(crate) fn library() -> BTreeMap<LibraryId, LibrarySymbol> {
        let two_pin_vertical = |id: &str, prefix: &str| {
            symbol(
                id,
                prefix,
                (-1.0, -2.54, 1.0, 2.54),
                vec![pin("1", 0.0, -3.81, Dir::Up), pin("2", 0.0, 3.81, Dir::Down)],
            )
        };
        let two_pin_horizontal = |id: &str, prefix: &str| {
            symbol(
                id,
                prefix,
                (-1.27, -1.27, 1.27, 1.27),
                vec![pin("1", -3.81, 0.0, Dir::Left), pin("2", 3.81, 0.0, Dir::Right)],
            )
        };
        // Supplies draw above their pin, ground below it (y down); the pin is at the origin.
        let power = |id: &str, outward: Dir| {
            let body = if outward == Dir::Up {
                (-1.27, 0.25, 1.27, 2.54)
            } else {
                (-0.76, -2.54, 0.76, -0.25)
            };
            symbol(id, "#PWR", body, vec![pin("1", 0.0, 0.0, outward)])
        };
        [
            two_pin_vertical("Device:R", "R"),
            two_pin_vertical("Device:C", "C"),
            two_pin_vertical("Device:Fuse", "F"),
            two_pin_horizontal("Device:D_Schottky", "D"),
            two_pin_horizontal("Device:LED", "D"),
            symbol(
                "Connector_Generic:Conn_01x02",
                "J",
                (-1.27, -1.27, 1.27, 3.81),
                vec![pin("1", -5.08, 0.0, Dir::Left), pin("2", -5.08, 2.54, Dir::Left)],
            ),
            symbol(
                "Regulator_Linear:AMS1117-3.3",
                "U",
                (-5.08, -1.27, 5.08, 5.08),
                vec![
                    pin("1", 0.0, 7.62, Dir::Down),
                    pin("2", 7.62, 0.0, Dir::Right),
                    pin("3", -7.62, 0.0, Dir::Left),
                ],
            ),
            power("power:+5V", Dir::Down),
            power("power:+3V3", Dir::Down),
            power("power:GND", Dir::Up),
            power("power:PWR_FLAG", Dir::Down),
        ]
        .into_iter()
        .map(|s| (s.library_id.clone(), s))
        .collect()
    }

    fn all_points(plan: &OperationPlan) -> Vec<Point> {
        plan.operations
            .iter()
            .flat_map(|op| match op {
                Operation::PlaceSymbol { position, .. }
                | Operation::PlacePowerSymbol { position, .. }
                | Operation::AddLabel { position, .. }
                | Operation::AddJunction { position } => vec![*position],
                Operation::AddWire { start, end, .. } => vec![*start, *end],
            })
            .collect()
    }

    #[test]
    fn two_resistor_chain_is_wired_in_a_row() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:?}", plan.findings);
        assert_eq!(plan.references.values().cloned().collect::<Vec<_>>(), ["R1", "R2"]);
        assert!(all_points(&plan).iter().all(|p| p.is_on_grid(GRID)));
        // MID joins R1.2 and R2.1 with a wire, not labels on both pins.
        assert!(plan.counts().wires >= 1, "{:?}", plan.counts());
        assert!(plan.fallback_nets.is_empty(), "{:?}", plan.fallback_nets);
        // Both resistors lie on one baseline, rotated horizontal.
        let rotations: Vec<u16> = plan
            .operations
            .iter()
            .filter_map(|o| match o {
                Operation::PlaceSymbol { orientation, .. } => Some(orientation.rotation_deg),
                _ => None,
            })
            .collect();
        assert!(rotations.iter().all(|r| *r == 90 || *r == 270), "{rotations:?}");
        assert_eq!(
            plan,
            plan_circuit(&spec, &library(), &ExistingDocument::default()),
            "deterministic"
        );
    }

    #[test]
    fn regulator_reads_left_to_right_with_power_symbols() {
        let spec = CircuitSpec::from_json(REGULATOR).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:?}", plan.findings);
        let counts = plan.counts();
        // Ground is drawn as symbols, never labels; +5V and +3V3 are named by power symbols.
        assert!(counts.power_symbols >= 9, "{counts:?}");
        assert!(plan
            .operations
            .iter()
            .all(|o| !matches!(o, Operation::AddLabel { net, .. } if net == "GND" || net == "+5V" || net == "+3V3")));
        assert!(plan.fallback_nets.is_empty(), "{:?}", plan.fallback_nets);
        // Main path order: J1 < F1 < D1 < U1 < R1 along x.
        let x_of = |r: &str| {
            plan.operations
                .iter()
                .find_map(|o| match o {
                    Operation::PlaceSymbol {
                        reference, position, ..
                    } if reference == r => Some(position.x),
                    _ => None,
                })
                .unwrap()
        };
        let order = ["J1", "F1", "D1", "U1", "R1"];
        assert!(
            order.windows(2).all(|w| x_of(w[0]) < x_of(w[1])),
            "{:?}",
            order.map(x_of)
        );
        // Input caps sit between D1 and U1, output caps after U1.
        assert!(x_of("C1") > x_of("D1") && x_of("C1") < x_of("U1"));
        assert!(x_of("C3") > x_of("U1"));
        assert!(counts.junctions >= 2, "{counts:?}");
    }

    #[test]
    fn avoids_existing_references_and_rows() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let existing = ExistingDocument {
            references: ["R1".to_owned()].into(),
            max_y: Some(Nm::from_mm(60.96).unwrap()),
        };
        let plan = plan_circuit(&spec, &library(), &existing);
        assert_eq!(plan.references.values().cloned().collect::<Vec<_>>(), ["R2", "R3"]);
        assert!(plan.findings.iter().any(|f| f.code == "REFERENCE_TAKEN"));
        let Operation::PlaceSymbol { position, .. } = &plan.operations[0] else {
            panic!()
        };
        assert!(position.y > Nm::from_mm(60.96).unwrap());
    }

    #[test]
    fn reports_unknown_pins_and_symbols() {
        let mut spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        spec.nets[0].pins[0].pin_number = "7".into();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(plan.findings.iter().any(|f| f.code == "UNKNOWN_PIN"));
        assert!(plan_circuit(&spec, &BTreeMap::new(), &ExistingDocument::default())
            .findings
            .iter()
            .any(|f| f.code == "UNKNOWN_LIBRARY_SYMBOL"));
    }
}

#[cfg(test)]
mod debug_dump {
    #[test]
    #[ignore]
    fn dump_regulator() {
        let spec = crate::CircuitSpec::from_json(include_str!("../../../docs/examples/regulator-3v3.json")).unwrap();
        let plan = super::plan_circuit(&spec, &super::tests::library(), &Default::default());
        for op in &plan.operations {
            println!("{op:?}");
        }
    }
}

#[cfg(test)]
mod readability_tests {
    use super::tests::library;
    use super::*;

    fn plan(json: &str) -> OperationPlan {
        plan_circuit(
            &CircuitSpec::from_json(json).unwrap(),
            &library(),
            &ExistingDocument::default(),
        )
    }

    #[test]
    fn golden_circuits_are_clean() {
        for json in [
            include_str!("../../../docs/examples/two-resistor-series-chain.json"),
            include_str!("../../../docs/examples/regulator-3v3.json"),
        ] {
            let p = plan(json);
            assert!(p.readability.is_clean(), "{}: {:#?}", p.circuit, p.readability);
            assert!(
                p.readability.fallback_nets.is_empty(),
                "{}: {:?}",
                p.circuit,
                p.readability.fallback_nets
            );
        }
    }

    #[test]
    fn a_short_is_an_error() {
        let mut p = plan(include_str!("../../../docs/examples/two-resistor-series-chain.json"));
        // Join a MID wire to INPUT's label point by hand.
        let input_at = p
            .operations
            .iter()
            .find_map(|o| match o {
                Operation::AddLabel { net, position, .. } if net == "INPUT" => Some(*position),
                _ => None,
            })
            .unwrap();
        p.operations.push(Operation::AddWire {
            net: "MID".into(),
            start: input_at,
            end: input_at + Dir::Up.step(Nm(2_540_000)),
        });
        let report = crate::readability::check(&p, &library());
        assert!(!report.shorts.is_empty());
    }
}
