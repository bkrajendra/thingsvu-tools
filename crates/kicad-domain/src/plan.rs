// SPDX-License-Identifier: GPL-3.0-or-later
//! Operation plans: the exact changes a circuit spec turns into, computed before anything is
//! applied (plan §7.2, §8.3). The layout here is deliberately simple and deterministic.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::circuit::CircuitSpec;
use crate::geometry::{Nm, Orientation, Point, GRID};
use crate::ids::{LibraryId, LogicalId};
use crate::validation::{has_errors, validate_circuit, Finding, Severity};

/// Upper bound on operations in one plan (plan §10 suggested 50; labels-per-pin layouts need more).
pub const MAX_OPERATIONS: usize = 200;

/// Pin geometry of a library symbol, from KiCad's library before anything is placed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibrarySymbol {
    pub library_id: LibraryId,
    /// Reference prefix from the library (`R`, `Q`, `U`, …).
    pub reference_prefix: String,
    pub unit_count: u32,
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
}

impl LibrarySymbol {
    fn pins_of_unit(&self, unit: u32) -> impl Iterator<Item = &LibraryPin> {
        self.pins.iter().filter(move |p| p.unit == 0 || p.unit == unit)
    }

    /// Half-extent of the pins around the origin, for spacing.
    fn extent(&self) -> Nm {
        self.pins
            .iter()
            .map(|p| p.local.x.0.abs().max(p.local.y.0.abs()))
            .max()
            .map(Nm)
            .unwrap_or(GRID)
    }
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
    AddWire {
        net: String,
        start: Point,
        end: Point,
    },
    AddLabel {
        net: String,
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
}

impl OperationPlan {
    pub fn count(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for op in &self.operations {
            match op {
                Operation::PlaceSymbol { .. } => counts.0 += 1,
                Operation::AddWire { .. } => counts.1 += 1,
                Operation::AddLabel { .. } => counts.2 += 1,
            }
        }
        counts
    }
}

/// What already exists in the target document, so the plan does not collide with it.
#[derive(Debug, Clone, Default)]
pub struct ExistingDocument {
    pub references: BTreeSet<String>,
    /// Lowest free row: new symbols are placed below this y.
    pub max_y: Option<Nm>,
}

const START_X: Nm = Nm(50_800_000);
const START_Y: Nm = Nm(50_800_000);
const COLUMNS: usize = 5;
/// Room around each symbol for labels and fields.
const MARGIN: Nm = Nm(12_700_000);

fn round_up_to_grid(v: Nm) -> Nm {
    Nm((v.0 + GRID.0 - 1).div_euclid(GRID.0) * GRID.0)
}

fn error(code: &'static str, message: String, objects: Vec<String>) -> Finding {
    Finding {
        severity: Severity::Error,
        code,
        message,
        objects,
    }
}

/// Turns `spec` into a plan. `library` must contain every library id the spec uses.
/// Returns the plan with findings; a plan whose findings include an error must not be applied.
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
                (1..)
                    .map(|n| format!("{prefix}{n}"))
                    .find(|r| !taken.contains(r))
                    .expect("unbounded range")
            }
        };
        taken.insert(reference.clone());
        plan.references.insert(c.id.clone(), reference);
    }

    // Placement: a grid of equal cells sized for the largest symbol, below anything existing.
    let pitch = round_up_to_grid(Nm(2 * spec
        .components
        .iter()
        .map(|c| library[&c.library_id].extent().0)
        .max()
        .unwrap_or(GRID.0)
        + MARGIN.0));
    let first_row = match existing.max_y {
        Some(y) => round_up_to_grid(y + pitch),
        None => START_Y,
    };
    let mut positions: BTreeMap<&LogicalId, Point> = BTreeMap::new();
    for (i, c) in spec.components.iter().enumerate() {
        let (row, col) = (i / COLUMNS, i % COLUMNS);
        let position = Point::new(
            Nm(START_X.0 + col as i64 * pitch.0),
            Nm(first_row.0 + row as i64 * pitch.0),
        );
        positions.insert(&c.id, position);
        plan.operations.push(Operation::PlaceSymbol {
            component_id: c.id.clone(),
            library_id: c.library_id.clone(),
            reference: plan.references[&c.id].clone(),
            value: c.value.clone(),
            footprint: c.footprint.clone(),
            position,
            orientation: Orientation::default(),
        });
    }

    // Pin points of every placed pin, to keep wires from crossing other pins.
    let mut pin_points: BTreeMap<(i64, i64), String> = BTreeMap::new();
    for c in &spec.components {
        for pin in library[&c.library_id].pins_of_unit(1) {
            let p = positions[&c.id] + pin.local;
            pin_points.insert((p.x.0, p.y.0), format!("{}.{}", plan.references[&c.id], pin.number));
        }
    }

    for net in &spec.nets {
        let mut points = Vec::new();
        let mut expected = BTreeSet::new();
        for pin_ref in &net.pins {
            let component = spec.component(&pin_ref.component_id).expect("validated");
            let symbol = &library[&component.library_id];
            let Some(pin) = symbol.pins_of_unit(1).find(|p| p.number == pin_ref.pin_number) else {
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
                continue;
            };
            let point = positions[&component.id] + pin.local;
            if !point.is_on_grid(GRID) {
                findings.push(error(
                    "OFF_GRID_PIN",
                    format!("pin {} of {} is off the 1.27 mm grid", pin.number, component.library_id),
                    vec![component.id.to_string()],
                ));
            }
            points.push(point);
            expected.insert(format!("{}.{}", plan.references[&component.id], pin.number));
        }
        if points.len() != net.pins.len() {
            continue;
        }

        if let [a, b] = points.as_slice() {
            if let Some(mid) = straight_wire_label(*a, *b, &pin_points) {
                plan.operations.push(Operation::AddWire {
                    net: net.name.clone(),
                    start: *a,
                    end: *b,
                });
                plan.operations.push(Operation::AddLabel {
                    net: net.name.clone(),
                    position: mid,
                });
                plan.expected_nets.push(ExpectedNet {
                    name: net.name.clone(),
                    pins: expected,
                });
                continue;
            }
        }
        for p in points {
            plan.operations.push(Operation::AddLabel {
                net: net.name.clone(),
                position: p,
            });
        }
        plan.expected_nets.push(ExpectedNet {
            name: net.name.clone(),
            pins: expected,
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

/// For two pins on one horizontal or vertical line with no other pin between them, returns a
/// grid point on the wire for its label.
fn straight_wire_label(a: Point, b: Point, pin_points: &BTreeMap<(i64, i64), String>) -> Option<Point> {
    if a == b || (a.x != b.x && a.y != b.y) {
        return None;
    }
    let (lo, hi) = if (a.x.0, a.y.0) <= (b.x.0, b.y.0) {
        (a, b)
    } else {
        (b, a)
    };
    let between = |p: &(i64, i64)| {
        if a.x == b.x {
            p.0 == a.x.0 && p.1 > lo.y.0 && p.1 < hi.y.0
        } else {
            p.1 == a.y.0 && p.0 > lo.x.0 && p.0 < hi.x.0
        }
    };
    if pin_points.keys().any(between) {
        return None;
    }
    let mid = Point::new(Nm((lo.x.0 + hi.x.0) / 2), Nm((lo.y.0 + hi.y.0) / 2)).snap(GRID);
    // A label on an endpoint would also be valid, but keep it visibly on the wire when possible.
    Some(if mid == lo || mid == hi { lo } else { mid })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

    fn resistor() -> LibrarySymbol {
        LibrarySymbol {
            library_id: LibraryId::parse("Device:R").unwrap(),
            reference_prefix: "R".into(),
            unit_count: 1,
            pins: vec![
                LibraryPin {
                    number: "1".into(),
                    name: "~".into(),
                    local: Point::from_mm(0.0, -3.81).unwrap(),
                    unit: 1,
                },
                LibraryPin {
                    number: "2".into(),
                    name: "~".into(),
                    local: Point::from_mm(0.0, 3.81).unwrap(),
                    unit: 1,
                },
            ],
        }
    }

    fn library() -> BTreeMap<LibraryId, LibrarySymbol> {
        [(LibraryId::parse("Device:R").unwrap(), resistor())]
            .into_iter()
            .collect()
    }

    #[test]
    fn plans_the_example_on_grid_with_expected_nets() {
        let spec = CircuitSpec::from_json(EXAMPLE).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:?}", plan.findings);
        assert_eq!(plan.count().0, 2);
        assert_eq!(plan.references.values().cloned().collect::<Vec<_>>(), ["R1", "R2"]);
        for op in &plan.operations {
            let points: Vec<Point> = match op {
                Operation::PlaceSymbol { position, .. } => vec![*position],
                Operation::AddWire { start, end, .. } => vec![*start, *end],
                Operation::AddLabel { position, .. } => vec![*position],
            };
            assert!(points.iter().all(|p| p.is_on_grid(GRID)), "{op:?}");
        }
        let mid = plan.expected_nets.iter().find(|n| n.name == "MID").unwrap();
        assert_eq!(mid.pins, ["R1.2", "R2.1"].iter().map(|s| s.to_string()).collect());
        // Side by side, R1.2 and R2.1 are not aligned: MID is joined by labels on both pins.
        let mid_labels = plan
            .operations
            .iter()
            .filter(|o| matches!(o, Operation::AddLabel { net, .. } if net == "MID"))
            .count();
        assert_eq!(mid_labels, 2);
        // Plans are deterministic.
        assert_eq!(plan, plan_circuit(&spec, &library(), &ExistingDocument::default()));
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

    #[test]
    fn straight_wire_for_aligned_pins_with_nothing_between() {
        let a = Point::from_mm(50.8, 54.61).unwrap();
        let b = Point::from_mm(50.8, 59.69).unwrap();
        let mid = straight_wire_label(a, b, &BTreeMap::new()).unwrap();
        assert_eq!(mid, Point::from_mm(50.8, 57.15).unwrap());
        let blocker: BTreeMap<_, _> = [((mm(50.8), mm(57.15)), "X.1".to_owned())].into();
        assert!(straight_wire_label(a, b, &blocker).is_none());
        assert!(straight_wire_label(a, Point::from_mm(52.07, 59.69).unwrap(), &BTreeMap::new()).is_none());
    }

    fn mm(v: f64) -> i64 {
        Nm::from_mm(v).unwrap().0
    }
}
