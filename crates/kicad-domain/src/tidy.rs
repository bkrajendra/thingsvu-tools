// SPDX-License-Identifier: GPL-3.0-or-later
//! Tidying an existing schematic (Phase 8).
//!
//! The circuit is read back from the design (its parts, and the nets KiCad reports), laid out
//! again by the same engine that lays out new circuits, and the difference becomes a plan: move
//! each existing part to its new place, delete the old wires, labels, junctions, and net-naming
//! power symbols, and draw the new ones. Parts keep their identity, values, footprints, and fields.
//! Wires are never dragged, so a part is never moved without the drawing around it being replaced.
//!
//! Applying the plan must leave every net exactly as it was; the apply step checks that against
//! KiCad's own netlist, as for a new circuit.

use std::collections::{BTreeMap, BTreeSet};

use crate::arrange::{mirrored, Mirror};
use crate::circuit::{CircuitSpec, ComponentSpec, NetSpec, PinRef, SCHEMA_VERSION};
use crate::geometry::{Dir, Nm, Point, Rect, GRID};
use crate::ids::{LibraryId, LogicalId};
use crate::layout;
use crate::plan::LibrarySymbol;
use crate::plan::{
    error, library_findings, readability_findings, ExpectedNet, Operation, OperationPlan, Removal, RemovedKind,
    SymbolMove, MAX_OPERATIONS,
};
use crate::reconnect::{self, Placement};
use crate::schematic::{NetInfo, SchematicDesign};
use crate::validation::{has_errors, validate_circuit, Finding, Severity};

/// The library symbols the design's own parts use.
pub fn design_library_ids(design: &SchematicDesign) -> Vec<LibraryId> {
    let mut ids: Vec<LibraryId> = design
        .symbols
        .iter()
        .filter_map(|s| LibraryId::parse(&s.library_id).ok())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `R10` sorts after `R2`: references by prefix, then number.
fn natural(reference: &str) -> (String, u64, String) {
    let prefix: String = reference.chars().take_while(|c| !c.is_ascii_digit()).collect();
    let number = reference[prefix.len()..].parse::<u64>().unwrap_or(u64::MAX);
    (prefix, number, reference.to_owned())
}

/// A logical id for a reference; flags (`#FLG01`) cannot start with `#`.
fn logical_id(reference: &str) -> Option<LogicalId> {
    match reference.strip_prefix('#') {
        Some(rest) => LogicalId::new(format!("flag_{rest}")).ok(),
        None => LogicalId::new(reference).ok(),
    }
}

/// The circuit the design implements: its parts (not the power symbols that only name nets) and
/// the nets KiCad reports. Parts are listed in reference order so the layout does not depend on
/// the order KiCad returned them in.
pub fn tidy_spec(design: &SchematicDesign, nets: &[NetInfo]) -> Result<CircuitSpec, Vec<Finding>> {
    let mut findings = Vec::new();
    if !design.unsupported.is_empty() {
        let items = design
            .unsupported
            .iter()
            .map(|(kind, n)| format!("{n} {kind}"))
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(error(
            "TIDY_UNSUPPORTED_ITEMS",
            format!(
                "the sheet has items that tidying cannot redraw safely ({items}); nothing was planned. \
                 Tidying handles a single sheet of parts joined by wires, local labels, and power symbols"
            ),
            design.unsupported.keys().cloned().collect(),
        ));
    }

    let mut parts: Vec<_> = design
        .symbols
        .iter()
        .filter(|s| !s.reference.starts_with("#PWR"))
        .collect();
    parts.sort_by_key(|s| natural(&s.reference));
    let mut components = Vec::new();
    let mut ids: BTreeMap<&str, LogicalId> = BTreeMap::new();
    for s in parts {
        let Some(id) = logical_id(&s.reference) else {
            findings.push(error(
                "TIDY_BAD_REFERENCE",
                format!(
                    "'{}' is not a usable reference; annotate the schematic first",
                    s.reference
                ),
                vec![s.reference.clone()],
            ));
            continue;
        };
        let Ok(library_id) = LibraryId::parse(&s.library_id) else {
            findings.push(error(
                "TIDY_BAD_LIBRARY_ID",
                format!("{} has the library id '{}'", s.reference, s.library_id),
                vec![s.reference.clone()],
            ));
            continue;
        };
        if ids.insert(&s.reference, id.clone()).is_some() {
            findings.push(error(
                "TIDY_DUPLICATE_REFERENCE",
                format!("two parts are called {}", s.reference),
                vec![s.reference.clone()],
            ));
            continue;
        }
        components.push(ComponentSpec {
            id,
            library_id,
            reference_preference: Some(s.reference.clone()),
            value: s.value.clone(),
            footprint: Some(s.footprint.clone()).filter(|f| !f.is_empty()),
            fields: BTreeMap::new(),
        });
    }

    let mut net_specs = Vec::new();
    for net in nets {
        if net.name.starts_with("unconnected-") {
            continue;
        }
        let pins: Vec<PinRef> = net
            .pins
            .iter()
            .filter_map(|p| {
                ids.get(p.reference.as_str()).map(|id| PinRef {
                    component_id: id.clone(),
                    pin_number: p.pin.clone(),
                })
            })
            .collect();
        if !pins.is_empty() {
            net_specs.push(NetSpec {
                name: net.name.trim_start_matches('/').to_owned(),
                pins,
            });
        }
    }
    net_specs.sort_by(|a, b| a.name.cmp(&b.name));

    if has_errors(&findings) {
        return Err(findings);
    }
    Ok(CircuitSpec {
        schema_version: SCHEMA_VERSION,
        name: LogicalId::new("tidy").expect("valid id"),
        description: "the circuit as it is in the schematic".into(),
        components,
        nets: net_specs,
        no_connects: vec![],
        power_flags: vec![],
        notes: vec![],
    })
}

/// Segments compare the same in either direction.
fn wire_key(a: Point, b: Point) -> ((i64, i64), (i64, i64)) {
    let (a, b) = ((a.x.0, a.y.0), (b.x.0, b.y.0));
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

type WireKey = ((i64, i64), (i64, i64));

/// Wires by the straight line they lie on. KiCad joins wires that meet end to end into one, so
/// two drawings that look the same must compare the same however their wires are cut.
struct WireIndex {
    horizontal: BTreeMap<i64, Vec<(i64, i64)>>,
    vertical: BTreeMap<i64, Vec<(i64, i64)>>,
}

impl WireIndex {
    fn new(segments: impl Iterator<Item = (Point, Point)>) -> Self {
        let mut index = WireIndex {
            horizontal: BTreeMap::new(),
            vertical: BTreeMap::new(),
        };
        for (a, b) in segments {
            if a.y == b.y {
                index
                    .horizontal
                    .entry(a.y.0)
                    .or_default()
                    .push((a.x.0.min(b.x.0), a.x.0.max(b.x.0)));
            } else if a.x == b.x {
                index
                    .vertical
                    .entry(a.x.0)
                    .or_default()
                    .push((a.y.0.min(b.y.0), a.y.0.max(b.y.0)));
            }
        }
        for runs in index.horizontal.values_mut().chain(index.vertical.values_mut()) {
            runs.sort_unstable();
            let mut merged: Vec<(i64, i64)> = Vec::new();
            for &(from, to) in runs.iter() {
                match merged.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => merged.push((from, to)),
                }
            }
            *runs = merged;
        }
        index
    }

    /// The whole run of wire on one line that contains this segment (the segment itself when it
    /// is slanted).
    fn key(&self, a: Point, b: Point) -> WireKey {
        let run = |runs: Option<&Vec<(i64, i64)>>, lo: i64, hi: i64| {
            runs.and_then(|r| r.iter().find(|&&(from, to)| from <= lo && hi <= to).copied())
        };
        if a.y == b.y {
            if let Some((from, to)) = run(self.horizontal.get(&a.y.0), a.x.0.min(b.x.0), a.x.0.max(b.x.0)) {
                return ((from, a.y.0), (to, a.y.0));
            }
        } else if a.x == b.x {
            if let Some((from, to)) = run(self.vertical.get(&a.x.0), a.y.0.min(b.y.0), a.y.0.max(b.y.0)) {
                return ((a.x.0, from), (a.x.0, to));
            }
        }
        wire_key(a, b)
    }

    fn keys(&self) -> BTreeSet<WireKey> {
        let horizontal = self
            .horizontal
            .iter()
            .flat_map(|(&y, runs)| runs.iter().map(move |&(from, to)| ((from, y), (to, y))));
        let vertical = self
            .vertical
            .iter()
            .flat_map(|(&x, runs)| runs.iter().map(move |&(from, to)| ((x, from), (x, to))));
        horizontal.chain(vertical).collect()
    }
}

/// Where the person wants parts to stay when the rest is laid out again.
#[derive(Debug, Clone, Default)]
pub struct LayoutHints {
    /// Parts that stay exactly where they are.
    pub keep: Vec<String>,
    /// Parts that go to a given place.
    pub place: Vec<PlaceHint>,
    /// Parts that go next to another part.
    pub near: Vec<NearHint>,
}

impl LayoutHints {
    pub fn is_empty(&self) -> bool {
        self.keep.is_empty() && self.place.is_empty() && self.near.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PlaceHint {
    pub reference: String,
    pub position: Point,
    /// Counter-clockwise from where the part is now.
    pub rotate_deg: Option<u16>,
    pub mirror: Option<Mirror>,
}

#[derive(Debug, Clone)]
pub struct NearHint {
    pub reference: String,
    pub to: String,
    /// Which side of `to` the part goes on.
    pub side: Dir,
    /// Clear space between the two bodies.
    pub gap: Nm,
}

/// How the drawing is made again.
enum Redraw<'a> {
    /// The layout engine decides where parts go (except as the hints say).
    Layout(&'a LayoutHints),
    /// Parts stay; only their connections are drawn again.
    Reconnect,
}

/// Plans the new layout of an existing design. `library` needs the design's own symbols and the
/// power symbols the layout may use (see [`design_library_ids`] and [`layout::library_ids_for`]).
pub fn plan_tidy(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
) -> OperationPlan {
    plan_tidy_with_hints(design, nets, library, &LayoutHints::default())
}

/// [`plan_tidy`], with parts the person wants in particular places.
pub fn plan_tidy_with_hints(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    hints: &LayoutHints,
) -> OperationPlan {
    plan_redraw(design, nets, library, Redraw::Layout(hints))
}

/// Draws every connection again with the parts where they are. `nets` is the circuit that is
/// meant, which after moving parts is not what KiCad reads from the drawing.
pub fn plan_reconnect(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
) -> OperationPlan {
    plan_redraw(design, nets, library, Redraw::Reconnect)
}

fn plan_redraw(
    design: &SchematicDesign,
    nets: &[NetInfo],
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    mode: Redraw,
) -> OperationPlan {
    let mut plan = OperationPlan {
        circuit: "tidy".into(),
        ..Default::default()
    };
    let spec = match tidy_spec(design, nets) {
        Ok(spec) => spec,
        Err(findings) => {
            plan.findings = findings;
            return plan;
        }
    };
    let mut findings = validate_circuit(&spec);
    if !has_errors(&findings) {
        findings.extend(library_findings(&spec, library));
    }
    if has_errors(&findings) {
        plan.findings = findings;
        return plan;
    }

    plan.references = spec
        .components
        .iter()
        .filter_map(|c| Some((c.id.clone(), c.reference_preference.clone()?)))
        .collect();
    plan.expected_nets = spec
        .nets
        .iter()
        .map(|n| ExpectedNet {
            name: n.name.clone(),
            pins: n
                .pins
                .iter()
                .map(|p| format!("{}.{}", plan.references[&p.component_id], p.pin_number))
                .collect(),
            ..Default::default()
        })
        .collect();

    // A net the design names with a label stays named with a label: a power symbol there would be
    // a power input nothing drives, and ERC would start to complain. Only nets that already have a
    // power symbol get one again.
    let named_by_symbol: BTreeSet<&str> = design
        .symbols
        .iter()
        .filter(|s| s.reference.starts_with("#PWR"))
        .map(|s| s.value.as_str())
        .collect();
    let mut library = library.clone();
    library.retain(|id, symbol| {
        let net_naming = id.library() == "power"
            && symbol.is_power
            && symbol.pins.len() == 1
            && symbol.pins[0].electrical_type == "power_in";
        !net_naming || named_by_symbol.contains(id.entry())
    });
    let library = &library;

    // The layout starts from the top left of the sheet, as for a new circuit.
    let mut taken: BTreeSet<String> = plan.references.values().cloned().collect();
    let current: BTreeMap<&str, Placement> = design
        .symbols
        .iter()
        .map(|s| {
            (
                s.reference.as_str(),
                Placement {
                    position: s.position,
                    orientation: s.orientation,
                },
            )
        })
        .collect();
    let mut hint_errors = Vec::new();
    let mut out = match &mode {
        Redraw::Layout(hints) if hints.is_empty() => layout::layout(&spec, library, &plan.references, &mut taken, None),
        Redraw::Layout(hints) => {
            let base = layout::layout(&spec, library, &plan.references, &mut taken.clone(), None);
            let placements = hinted_placements(&spec, library, &current, &base.operations, hints, &mut hint_errors);
            let mut routed = reconnect::route(&spec, library, &plan.references, &placements, &mut taken);
            routed
                .findings
                .extend(base.findings.into_iter().filter(|f| f.code != "NET_DRAWN_WITH_LABELS"));
            routed
        }
        Redraw::Reconnect => {
            let placements: BTreeMap<LogicalId, Placement> = spec
                .components
                .iter()
                .filter_map(|c| Some((c.id.clone(), *current.get(c.reference_preference.as_deref()?)?)))
                .collect();
            reconnect::route(&spec, library, &plan.references, &placements, &mut taken)
        }
    };
    if !hint_errors.is_empty() {
        findings.extend(hint_errors);
        plan.findings = findings;
        return plan;
    }
    findings.extend(out.findings.clone());
    plan.fallback_nets = out.fallback_nets.clone();

    // KiCad names a net nobody named `Net-(R1-Pad2)`; a label saying so adds nothing to a net that
    // is drawn with wires.
    out.operations.retain(|op| match op {
        Operation::AddLabel { net, .. } => !net.starts_with("Net-(") || plan.fallback_nets.contains(net),
        _ => true,
    });

    // Text notes stay where they are, so the layout moves out of their way (below them). Parts the
    // person placed, or left in place, are not moved.
    let free_layout = matches!(&mode, Redraw::Layout(h) if h.is_empty());
    if let Some(drawn) = layout::operations_bounds(&out.operations, library).filter(|_| free_layout) {
        let mut dy = 0;
        for _ in 0..design.notes.len() + 1 {
            let area = drawn.translate(Point::new(Nm(0), Nm(dy)));
            let Some(lowest) = design
                .notes
                .iter()
                .filter(|n| n.overlaps(area))
                .map(|n| n.max.y.0)
                .max()
            else {
                break;
            };
            // Clear of the note by a little, on the grid.
            let wanted = lowest + 2_540_000 - drawn.min.y.0;
            dy = (wanted + GRID.0 - 1).div_euclid(GRID.0) * GRID.0;
        }
        if dy != 0 {
            let by = Point::new(Nm(0), Nm(dy));
            out.operations = out.operations.into_iter().map(|op| layout::translate(op, by)).collect();
            findings.push(Finding {
                severity: Severity::Warning,
                code: "TIDY_PLACED_BELOW_NOTES",
                message: format!(
                    "text notes sit where the layout would go, so the circuit is placed {:.0} mm lower, below them; move the notes if you want it higher",
                    Nm(dy).to_mm()
                ),
                objects: vec![],
            });
        }
    }

    // The checks run on the drawing as the layout made it, parts included.
    let drawn = OperationPlan {
        operations: out.operations.clone(),
        expected_nets: plan.expected_nets.clone(),
        references: plan.references.clone(),
        ..Default::default()
    };
    plan.readability = crate::readability::check(&drawn, library);
    findings.extend(readability_findings(&plan.readability));
    plan.readability_before = Some(crate::readability::check_design(design, library));

    // Existing parts: move the ones that are not already in place.
    let by_reference: BTreeMap<&str, _> = design.symbols.iter().map(|s| (s.reference.as_str(), s)).collect();
    let mut additions = Vec::new();
    for op in out.operations {
        match &op {
            Operation::PlaceSymbol {
                reference,
                position,
                orientation,
                ..
            } => {
                let Some(existing) = by_reference.get(reference.as_str()) else {
                    continue;
                };
                if existing.position != *position || !existing.orientation.same_effect(*orientation) {
                    plan.moves.push(SymbolMove {
                        id: existing.id.clone(),
                        reference: reference.clone(),
                        from: existing.position,
                        from_orientation: existing.orientation,
                        to: *position,
                        orientation: *orientation,
                    });
                }
            }
            _ => additions.push(op),
        }
    }

    // Existing drawing items go, except those the new drawing would draw identically.
    // Wires compare as runs along a line, so that wires KiCad has joined or cut differently still
    // match; a run is kept when the new drawing has exactly the same run.
    let existing_wires = WireIndex::new(design.wires.iter().map(|w| (w.start, w.end)));
    let new_wires = WireIndex::new(additions.iter().filter_map(|op| match op {
        Operation::AddWire { start, end, .. } => Some((*start, *end)),
        _ => None,
    }));
    let same_runs: BTreeSet<WireKey> = existing_wires.keys().intersection(&new_wires.keys()).copied().collect();
    let mut junctions: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (i, j) in design.junctions.iter().enumerate() {
        junctions.entry((j.position.x.0, j.position.y.0)).or_default().push(i);
    }
    let mut labels: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (i, l) in design.labels.iter().enumerate() {
        labels
            .entry((l.text.clone(), l.position.x.0, l.position.y.0))
            .or_default()
            .push(i);
    }
    let power: Vec<usize> = design
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.reference.starts_with("#PWR"))
        .map(|(i, _)| i)
        .collect();
    let (mut keep_wire, mut keep_junction, mut keep_label, mut keep_power) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    additions.retain(|op| {
        let matched = match op {
            Operation::AddWire { start, end, .. } => same_runs.contains(&new_wires.key(*start, *end)).then_some(true),
            Operation::AddJunction { position } => junctions
                .get_mut(&(position.x.0, position.y.0))
                .and_then(Vec::pop)
                .map(|i| keep_junction.insert(i)),
            Operation::AddLabel { net, position, .. } => labels
                .get_mut(&(net.clone(), position.x.0, position.y.0))
                .and_then(Vec::pop)
                .map(|i| keep_label.insert(i)),
            Operation::PlacePowerSymbol {
                net,
                position,
                orientation,
                ..
            } => power
                .iter()
                .find(|&&i| {
                    let s = &design.symbols[i];
                    !keep_power.contains(&i)
                        && s.value == *net
                        && s.position == *position
                        && s.orientation.same_effect(*orientation)
                })
                .map(|&i| keep_power.insert(i)),
            _ => None,
        };
        matched.is_none()
    });
    let at = |p: Point| format!("({:.2}, {:.2})", p.x.to_mm(), p.y.to_mm());
    keep_no_connects(design, library, &plan.moves, &mut plan.removals, &mut additions, &at);
    for (i, w) in design.wires.iter().enumerate() {
        if same_runs.contains(&existing_wires.key(w.start, w.end)) {
            keep_wire.insert(i);
        }
    }
    for (i, w) in design.wires.iter().enumerate().filter(|(i, _)| !keep_wire.contains(i)) {
        let _ = i;
        plan.removals.push(Removal {
            kind: RemovedKind::Wire,
            id: w.id.clone(),
            description: format!("wire {} to {}", at(w.start), at(w.end)),
        });
    }
    for (i, l) in design
        .labels
        .iter()
        .enumerate()
        .filter(|(i, _)| !keep_label.contains(i))
    {
        let _ = i;
        plan.removals.push(Removal {
            kind: RemovedKind::Label,
            id: l.id.clone(),
            description: format!("label {} at {}", l.text, at(l.position)),
        });
    }
    for (i, j) in design
        .junctions
        .iter()
        .enumerate()
        .filter(|(i, _)| !keep_junction.contains(i))
    {
        let _ = i;
        plan.removals.push(Removal {
            kind: RemovedKind::Junction,
            id: j.id.clone(),
            description: format!("junction at {}", at(j.position)),
        });
    }
    for &i in power.iter().filter(|i| !keep_power.contains(i)) {
        let s = &design.symbols[i];
        plan.removals.push(Removal {
            kind: RemovedKind::PowerSymbol,
            id: s.id.clone(),
            description: format!("power symbol {} ({}) at {}", s.reference, s.value, at(s.position)),
        });
    }

    // New power symbols must not reuse a reference that a kept one still has.
    let mut used: BTreeSet<String> = plan.references.values().cloned().collect();
    used.extend(keep_power.iter().map(|&i| design.symbols[i].reference.clone()));
    for op in &mut additions {
        if let Operation::PlacePowerSymbol { reference, .. } = op {
            let prefix: String = reference.chars().take_while(|c| !c.is_ascii_digit()).collect();
            *reference = (1..)
                .map(|n| format!("{prefix}{n:02}"))
                .find(|r| !used.contains(r))
                .expect("unbounded range");
            used.insert(reference.clone());
        }
    }
    plan.operations = additions;

    if design.annotations > 0 && free_layout {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "TIDY_ANNOTATIONS_STAY",
            message: format!(
                "{} text note(s) or graphics stay where they are and may overlap the new layout",
                design.annotations
            ),
            objects: vec![],
        });
    }
    if plan.moves.is_empty() && plan.removals.is_empty() && plan.operations.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "NOTHING_TO_CHANGE",
            message: "the schematic already matches the layout the tidy tool would draw".into(),
            objects: vec![],
        });
    } else {
        findings.push(Finding {
            severity: Severity::Warning,
            code: "TEXT_NOT_PLACED",
            message: "KiCad places each part's reference and value text itself, and it can run into wires or other text; after applying, run sch_declutter_plan".into(),
            objects: vec![],
        });
    }
    let total = plan.operations.len() + plan.moves.len() + plan.removals.len();
    if total > MAX_OPERATIONS {
        findings.push(error(
            "PLAN_TOO_LARGE",
            format!("the plan has {total} operations; the limit is {MAX_OPERATIONS}"),
            vec![],
        ));
    }
    plan.findings = findings;
    plan
}

/// A no-connect flag sits on a pin, and a part that moves leaves it behind. The flag of a part
/// that moved is deleted and drawn again on the pin's new place; a flag of a part that stays, or
/// one that is on no pin, is left alone.
fn keep_no_connects(
    design: &SchematicDesign,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    moves: &[SymbolMove],
    removals: &mut Vec<Removal>,
    additions: &mut Vec<Operation>,
    at: &impl Fn(Point) -> String,
) {
    for flag in &design.no_connects {
        let found = design.symbols.iter().find_map(|s| {
            let pins = &library.get(&LibraryId::parse(&s.library_id).ok()?)?.pins;
            let pin = pins.iter().find(|pin| {
                s.orientation
                    .apply(pin.local)
                    .is_some_and(|local| s.position + local == flag.position)
            })?;
            Some((s, pin))
        });
        let Some((symbol, pin)) = found else {
            continue;
        };
        let Some(moved) = moves.iter().find(|m| m.id == symbol.id) else {
            continue;
        };
        let Some(local) = moved.orientation.apply(pin.local) else {
            continue;
        };
        if moved.to + local == flag.position {
            continue;
        }
        removals.push(Removal {
            kind: RemovedKind::NoConnect,
            id: flag.id.clone(),
            description: format!(
                "no-connect flag on {} pin {} at {}",
                symbol.reference,
                pin.number,
                at(flag.position)
            ),
        });
        additions.push(Operation::AddNoConnect {
            position: moved.to + local,
        });
    }
}

fn body_at(library: &BTreeMap<LibraryId, LibrarySymbol>, id: &LibraryId, at: Placement) -> Option<Rect> {
    Some(
        library
            .get(id)?
            .body?
            .transformed(at.orientation)
            .translate(at.position),
    )
}

/// The places of all parts: the layout's, except where the hints say otherwise, and with the
/// layout's parts moved off the ones the person placed.
fn hinted_placements(
    spec: &CircuitSpec,
    library: &BTreeMap<LibraryId, LibrarySymbol>,
    current: &BTreeMap<&str, Placement>,
    laid_out: &[Operation],
    hints: &LayoutHints,
    errors: &mut Vec<Finding>,
) -> BTreeMap<LogicalId, Placement> {
    let mut placements: BTreeMap<LogicalId, Placement> = laid_out
        .iter()
        .filter_map(|op| match op {
            Operation::PlaceSymbol {
                component_id,
                position,
                orientation,
                ..
            } => Some((
                component_id.clone(),
                Placement {
                    position: *position,
                    orientation: *orientation,
                },
            )),
            _ => None,
        })
        .collect();
    let find = |reference: &str| {
        spec.components
            .iter()
            .find(|c| c.reference_preference.as_deref() == Some(reference))
    };
    let unknown = |reference: &str, errors: &mut Vec<Finding>| {
        errors.push(error(
            "TIDY_HINT_UNKNOWN_PART",
            format!("the layout hints name {reference}, which is not a part of this sheet"),
            vec![reference.to_owned()],
        ));
    };
    let mut fixed: BTreeSet<LogicalId> = BTreeSet::new();
    for reference in &hints.keep {
        match (find(reference), current.get(reference.as_str())) {
            (Some(c), Some(&at)) => {
                placements.insert(c.id.clone(), at);
                fixed.insert(c.id.clone());
            }
            _ => unknown(reference, errors),
        }
    }
    for hint in &hints.place {
        match (find(&hint.reference), current.get(hint.reference.as_str())) {
            (Some(c), Some(&now)) => {
                let mut orientation = now.orientation;
                if let Some(deg) = hint.rotate_deg {
                    orientation = crate::arrange::rotated(orientation, deg);
                }
                if let Some(mirror) = hint.mirror {
                    orientation = mirrored(orientation, mirror);
                }
                placements.insert(
                    c.id.clone(),
                    Placement {
                        position: hint.position.snap(GRID),
                        orientation,
                    },
                );
                fixed.insert(c.id.clone());
            }
            _ => unknown(&hint.reference, errors),
        }
    }
    for hint in &hints.near {
        let (Some(c), Some(target)) = (find(&hint.reference), find(&hint.to)) else {
            let missing = if find(&hint.reference).is_none() {
                &hint.reference
            } else {
                &hint.to
            };
            unknown(missing, errors);
            continue;
        };
        let (Some(&own), Some(&other)) = (placements.get(&c.id), placements.get(&target.id)) else {
            continue;
        };
        let at_origin = Placement {
            position: Point::default(),
            ..own
        };
        let (Some(own_body), Some(target_body)) = (
            body_at(library, &c.library_id, at_origin),
            body_at(library, &target.library_id, other),
        ) else {
            continue;
        };
        let centre = |r: Rect| Point::new(Nm((r.min.x.0 + r.max.x.0) / 2), Nm((r.min.y.0 + r.max.y.0) / 2));
        let (own_mid, target_mid) = (centre(own_body), centre(target_body));
        let position = match hint.side {
            Dir::Right => Point::new(target_body.max.x + hint.gap - own_body.min.x, target_mid.y - own_mid.y),
            Dir::Left => Point::new(target_body.min.x - hint.gap - own_body.max.x, target_mid.y - own_mid.y),
            Dir::Down => Point::new(target_mid.x - own_mid.x, target_body.max.y + hint.gap - own_body.min.y),
            Dir::Up => Point::new(target_mid.x - own_mid.x, target_body.min.y - hint.gap - own_body.max.y),
        };
        placements.insert(
            c.id.clone(),
            Placement {
                position: position.snap(GRID),
                orientation: own.orientation,
            },
        );
        fixed.insert(c.id.clone());
    }

    // Parts the layout placed must not sit on the ones that were placed by hand.
    let step = Nm(5 * GRID.0);
    for c in &spec.components {
        if fixed.contains(&c.id) {
            continue;
        }
        for _ in 0..200 {
            let Some(&at) = placements.get(&c.id) else { break };
            let Some(body) = body_at(library, &c.library_id, at).map(|b| b.inflate(Nm(2 * GRID.0))) else {
                break;
            };
            let blocked = spec.components.iter().filter(|o| o.id != c.id).any(|o| {
                placements
                    .get(&o.id)
                    .and_then(|&p| body_at(library, &o.library_id, p))
                    .is_some_and(|r| r.overlaps(body))
            });
            if !blocked {
                break;
            }
            placements.insert(
                c.id.clone(),
                Placement {
                    position: at.position + Point::new(Nm(0), step),
                    ..at
                },
            );
        }
    }
    placements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit::CircuitSpec;
    use crate::geometry::{Nm, Orientation};
    use crate::plan::{plan_circuit, ExistingDocument};
    use crate::schematic::{DesignJunction, DesignLabel, DesignSymbol, DesignWire, NetPin};

    const REGULATOR: &str = include_str!("../../../docs/examples/regulator-3v3.json");

    fn library() -> BTreeMap<LibraryId, LibrarySymbol> {
        crate::plan::tests::library()
    }

    /// The design a plan would produce, as it would be read back (ids made up).
    fn design_of(plan: &OperationPlan) -> SchematicDesign {
        let mut design = SchematicDesign::default();
        for (n, op) in plan.operations.iter().enumerate() {
            let id = format!("00000000-0000-4000-8000-{n:012}");
            match op {
                Operation::PlaceSymbol {
                    library_id,
                    reference,
                    value,
                    position,
                    orientation,
                    ..
                } => design.symbols.push(DesignSymbol {
                    id,
                    reference: reference.clone(),
                    value: value.clone(),
                    library_id: library_id.to_string(),
                    footprint: String::new(),
                    position: *position,
                    orientation: *orientation,
                    unit: 1,
                    fields: vec![],
                    ..Default::default()
                }),
                Operation::PlacePowerSymbol {
                    net,
                    library_id,
                    reference,
                    position,
                    orientation,
                } => design.symbols.push(DesignSymbol {
                    id,
                    reference: reference.clone(),
                    value: net.clone(),
                    library_id: library_id.to_string(),
                    footprint: String::new(),
                    position: *position,
                    orientation: *orientation,
                    unit: 1,
                    fields: vec![],
                    ..Default::default()
                }),
                Operation::AddWire { start, end, .. } => design.wires.push(DesignWire {
                    id,
                    start: *start,
                    end: *end,
                }),
                Operation::AddLabel {
                    net,
                    position,
                    direction,
                } => design.labels.push(DesignLabel {
                    id,
                    text: net.clone(),
                    position: *position,
                    direction: *direction,
                }),
                Operation::AddJunction { position } => design.junctions.push(DesignJunction {
                    id,
                    position: *position,
                }),
                Operation::AddNoConnect { position } => design.no_connects.push(crate::schematic::DesignNoConnect {
                    id,
                    position: *position,
                }),
                Operation::AddText {
                    text,
                    position,
                    size,
                    angle_deg,
                    h,
                    v,
                } => design.texts.push(crate::schematic::DesignText {
                    id,
                    text: text.clone(),
                    position: *position,
                    size: *size,
                    angle_deg: *angle_deg,
                    h: *h,
                    v: *v,
                }),
                Operation::AddShape { kind, stroke, filled } => design.shapes.push(crate::schematic::DesignShape {
                    id,
                    kind: *kind,
                    stroke: *stroke,
                    filled: *filled,
                }),
            }
        }
        design
    }

    /// The nets KiCad would report for the plan's circuit.
    fn nets_of(plan: &OperationPlan) -> Vec<NetInfo> {
        plan.expected_nets
            .iter()
            .map(|n| NetInfo {
                name: format!("/{}", n.name),
                pins: n
                    .pins
                    .iter()
                    .map(|p| {
                        let (reference, pin) = p.split_once('.').unwrap();
                        NetPin {
                            reference: reference.into(),
                            pin: pin.into(),
                            symbol_id: crate::ids::Kiid::new("00000000-0000-4000-8000-000000000000").unwrap(),
                        }
                    })
                    .collect(),
                other_items: 0,
            })
            .collect()
    }

    /// What applying the plan does to the design, for the next round.
    fn apply(design: &SchematicDesign, plan: &OperationPlan) -> SchematicDesign {
        let mut next = design.clone();
        let gone: BTreeSet<&str> = plan.removals.iter().map(|r| r.id.as_str()).collect();
        next.symbols.retain(|s| !gone.contains(s.id.as_str()));
        next.wires.retain(|w| !gone.contains(w.id.as_str()));
        next.labels.retain(|l| !gone.contains(l.id.as_str()));
        next.junctions.retain(|j| !gone.contains(j.id.as_str()));
        for m in &plan.moves {
            let s = next.symbols.iter_mut().find(|s| s.id == m.id).unwrap();
            s.position = m.to;
            s.orientation = m.orientation;
        }
        let added = design_of(&OperationPlan {
            operations: plan.operations.clone(),
            ..Default::default()
        });
        let offset = format!("new{}", next.symbols.len() + next.wires.len());
        for mut s in added.symbols {
            s.id = format!("{offset}{}", s.id);
            next.symbols.push(s);
        }
        for mut w in added.wires {
            w.id = format!("{offset}{}", w.id);
            next.wires.push(w);
        }
        for mut l in added.labels {
            l.id = format!("{offset}{}", l.id);
            next.labels.push(l);
        }
        for mut j in added.junctions {
            j.id = format!("{offset}{}", j.id);
            next.junctions.push(j);
        }
        next
    }

    fn built_regulator() -> (OperationPlan, SchematicDesign) {
        let spec = CircuitSpec::from_json(REGULATOR).unwrap();
        let plan = plan_circuit(&spec, &library(), &ExistingDocument::default());
        assert!(!has_errors(&plan.findings), "{:#?}", plan.findings);
        let design = design_of(&plan);
        (plan, design)
    }

    fn errors(plan: &OperationPlan) -> Vec<&Finding> {
        plan.findings.iter().filter(|f| f.severity == Severity::Error).collect()
    }

    #[test]
    fn power_pins_are_attached_to_the_nets_they_touch() {
        let (built, design) = built_regulator();
        // What `kicad-cli` reports: no power symbols or flags.
        let mut nets = nets_of(&built);
        for net in &mut nets {
            net.pins.retain(|p| !p.reference.starts_with('#'));
        }
        crate::connectivity::attach_power_pins(&design, &library(), &mut nets);
        let pins_of = |net: &NetInfo| -> BTreeSet<String> {
            net.pins.iter().map(|p| format!("{}.{}", p.reference, p.pin)).collect()
        };
        let expected = nets_of(&built);
        for want in &expected {
            let got = nets.iter().find(|n| n.name == want.name).unwrap();
            // Every pin the plan expected is there (the flags among them)...
            assert!(
                pins_of(want).is_subset(&pins_of(got)),
                "{}: {:?} vs {:?}",
                want.name,
                pins_of(want),
                pins_of(got)
            );
            // ...and each power symbol sits on the net its value names.
            for extra in pins_of(got).difference(&pins_of(want)) {
                let reference = extra.split_once('.').unwrap().0;
                let symbol = design.symbols.iter().find(|s| s.reference == reference).unwrap();
                assert_eq!(format!("/{}", symbol.value), got.name, "{extra}");
            }
        }
    }

    #[test]
    fn the_layout_keeps_out_of_the_way_of_text_notes() {
        let (built, mut design) = built_regulator();
        // A note across the middle of where the circuit is drawn.
        design.annotations = 1;
        design.notes.push(crate::schematic::note_area(
            Point::new(Nm(50_800_000), Nm(76_200_000)),
            &"x".repeat(80),
            Nm(1_270_000),
        ));
        let nets = nets_of(&built);
        let plan = plan_tidy(&design, &nets, &library());
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert!(plan.findings.iter().any(|f| f.code == "TIDY_PLACED_BELOW_NOTES"));
        // Nothing is drawn on the note.
        let drawn = layout::operations_bounds(
            &design_of(&OperationPlan {
                operations: plan.operations.clone(),
                ..Default::default()
            })
            .symbols
            .iter()
            .map(|s| Operation::PlacePowerSymbol {
                net: s.value.clone(),
                library_id: LibraryId::parse(&s.library_id).unwrap(),
                reference: s.reference.clone(),
                position: s.position,
                orientation: s.orientation,
            })
            .collect::<Vec<_>>(),
            &library(),
        );
        let note = design.notes[0];
        assert!(
            drawn.is_none_or(|d| !d.overlaps(note)) || plan.moves.iter().all(|m| m.to.y.0 > note.max.y.0 - 20_000_000)
        );
        // Tidying again changes nothing: the same notes give the same place.
        let again = plan_tidy(&apply(&design, &plan), &nets, &library());
        assert!(
            again.moves.is_empty() && again.removals.is_empty() && again.operations.is_empty(),
            "{again:#?}"
        );
    }

    #[test]
    fn a_tidied_design_needs_no_further_change() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let first = plan_tidy(&design, &nets, &library());
        assert_eq!(errors(&first), Vec::<&Finding>::new(), "{:#?}", first.findings);
        let tidy = apply(&design, &first);
        let second = plan_tidy(&tidy, &nets, &library());
        assert_eq!(errors(&second), Vec::<&Finding>::new(), "{:#?}", second.findings);
        assert!(second.moves.is_empty(), "moves: {:#?}", second.moves);
        assert!(second.removals.is_empty(), "removals: {:#?}", second.removals);
        assert!(second.operations.is_empty(), "operations: {:#?}", second.operations);
        assert!(second.findings.iter().any(|f| f.code == "NOTHING_TO_CHANGE"));
    }

    #[test]
    fn a_scrambled_design_is_laid_out_again_with_the_same_nets() {
        let (built, mut design) = built_regulator();
        // Pile the parts up, turn some, and lose the labels: a mess with the same circuit.
        for (i, s) in design
            .symbols
            .iter_mut()
            .enumerate()
            .filter(|(_, s)| !s.reference.starts_with('#'))
        {
            // Parts piled up on one spot.
            s.position = Point::new(Nm(76_200_000 + 1_270_000 * (i as i64 % 3)), Nm(127_000_000));
            if i % 2 == 0 {
                s.orientation = Orientation {
                    rotation_deg: 90,
                    ..s.orientation
                };
            }
        }
        design.labels.clear();
        let plan = plan_tidy(&design, &nets_of(&built), &library());
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert!(!plan.moves.is_empty());
        assert!(plan.readability.is_clean(), "{:#?}", plan.readability);
        // The circuit asked for is the circuit that was there.
        let mut expected: Vec<_> = plan
            .expected_nets
            .iter()
            .map(|n| (n.name.clone(), n.pins.clone()))
            .collect();
        // (The test library gives flags a `#PWR` reference; those are not circuit parts.)
        let mut original: Vec<_> = built
            .expected_nets
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.pins
                        .iter()
                        .filter(|p| !p.starts_with('#'))
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                )
            })
            .collect();
        expected.sort();
        original.sort();
        assert_eq!(expected, original);
        // Every part is kept (moved, never removed), and old power symbols are replaced.
        assert!(plan
            .removals
            .iter()
            .all(|r| r.kind != RemovedKind::PowerSymbol || r.description.contains("#PWR")));
        let moved: BTreeSet<_> = plan.moves.iter().map(|m| m.reference.as_str()).collect();
        assert!(moved.contains("U1") && moved.contains("J1"));
        // The review notices what was wrong before.
        let before = plan
            .readability_before
            .as_ref()
            .expect("a tidy plan reports the old state");
        assert!(
            !before.is_clean(),
            "the scrambled design should have problems: {before:#?}"
        );
    }

    #[test]
    fn a_no_connect_flag_follows_its_pin_when_the_part_moves() {
        let (built, mut design) = built_regulator();
        let lib = library();
        for (i, s) in design
            .symbols
            .iter_mut()
            .filter(|s| !s.reference.starts_with('#'))
            .enumerate()
        {
            s.position = Point::new(Nm(76_200_000 + 1_270_000 * (i as i64 % 3)), Nm(127_000_000));
        }
        design.labels.clear();
        // A flag on pin 1 of U1, where U1 is now.
        let u1 = design.symbols.iter().find(|s| s.reference == "U1").unwrap().clone();
        let pin = lib[&LibraryId::parse(&u1.library_id).unwrap()].pins[0].clone();
        let flag_at = u1.position + u1.orientation.apply(pin.local).unwrap();
        design.no_connects.push(crate::schematic::DesignNoConnect {
            id: "flag-1".into(),
            position: flag_at,
        });
        let plan = plan_tidy(&design, &nets_of(&built), &lib);
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        let moved = plan.moves.iter().find(|m| m.reference == "U1").expect("U1 moves");
        let now = moved.to + moved.orientation.apply(pin.local).unwrap();
        assert!(plan
            .removals
            .iter()
            .any(|r| r.kind == RemovedKind::NoConnect && r.id == "flag-1"));
        assert!(
            plan.operations
                .iter()
                .any(|o| matches!(o, Operation::AddNoConnect { position } if *position == now)),
            "the flag is drawn again on the pin's new place"
        );
        // A flag that is on no pin is left where it is.
        design.no_connects[0].position = Point::new(Nm(1_270_000), Nm(1_270_000));
        let plan = plan_tidy(&design, &nets_of(&built), &lib);
        assert!(plan.removals.iter().all(|r| r.kind != RemovedKind::NoConnect));
    }

    #[test]
    fn designs_with_items_that_cannot_be_redrawn_are_refused() {
        let (built, mut design) = built_regulator();
        design.unsupported.insert("no_connect".into(), 2);
        design.unsupported.insert("sheet".into(), 1);
        let plan = plan_tidy(&design, &nets_of(&built), &library());
        assert!(plan.moves.is_empty() && plan.operations.is_empty() && plan.removals.is_empty());
        let finding = plan
            .findings
            .iter()
            .find(|f| f.code == "TIDY_UNSUPPORTED_ITEMS")
            .unwrap();
        assert!(
            finding.message.contains("2 no_connect") && finding.message.contains("1 sheet"),
            "{finding:?}"
        );
    }

    #[test]
    fn unannotated_parts_are_refused() {
        let (built, mut design) = built_regulator();
        design
            .symbols
            .iter_mut()
            .find(|s| s.reference == "C1")
            .unwrap()
            .reference = "C?".into();
        let plan = plan_tidy(&design, &nets_of(&built), &library());
        assert!(plan.findings.iter().any(|f| f.code == "TIDY_BAD_REFERENCE"));
    }

    #[test]
    fn new_power_symbols_do_not_reuse_references_that_stay() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let first = plan_tidy(&design, &nets, &library());
        let mut tidy = apply(&design, &first);
        // One ground symbol is no longer where the layout wants it; the others stay.
        let moved = tidy
            .symbols
            .iter_mut()
            .find(|s| s.value == "GND" && s.reference.starts_with("#PWR"))
            .unwrap();
        moved.position = Point::new(Nm(moved.position.x.0 + 5_080_000), moved.position.y);
        let plan = plan_tidy(&tidy, &nets, &library());
        let kept: BTreeSet<_> = tidy
            .symbols
            .iter()
            .filter(|s| s.reference.starts_with("#PWR") && !plan.removals.iter().any(|r| r.id == s.id))
            .map(|s| s.reference.clone())
            .collect();
        for op in &plan.operations {
            if let Operation::PlacePowerSymbol { reference, .. } = op {
                assert!(
                    !kept.contains(reference),
                    "{reference} is already used by a symbol that stays"
                );
            }
        }
        assert_eq!(plan.operations.len(), 1, "{:#?}", plan.operations);
    }

    /// Which pins end up joined, read from the geometry of a design the way KiCad would.
    fn joined(design: &SchematicDesign) -> BTreeMap<String, usize> {
        use std::collections::HashMap;
        #[derive(Hash, PartialEq, Eq, Clone)]
        enum Key {
            At(i64, i64),
            Name(String),
        }
        fn find(parent: &mut HashMap<Key, Key>, k: &Key) -> Key {
            let p = parent.entry(k.clone()).or_insert_with(|| k.clone()).clone();
            if &p == k {
                return p;
            }
            let root = find(parent, &p);
            parent.insert(k.clone(), root.clone());
            root
        }
        fn union(parent: &mut HashMap<Key, Key>, a: Key, b: Key) {
            let (ra, rb) = (find(parent, &a), find(parent, &b));
            parent.insert(ra, rb);
        }
        let mut parent: HashMap<Key, Key> = HashMap::new();
        let at = |p: Point| Key::At(p.x.0, p.y.0);
        let lib = library();
        let mut pins: Vec<(String, Point)> = Vec::new();
        let mut points: Vec<Point> = Vec::new();
        for s in &design.symbols {
            let def = &lib[&LibraryId::parse(&s.library_id).unwrap()];
            for pin in &def.pins {
                let p = s.position + s.orientation.apply(pin.local).unwrap();
                points.push(p);
                if s.reference.starts_with("#PWR") {
                    union(&mut parent, at(p), Key::Name(s.value.clone()));
                } else {
                    pins.push((format!("{}.{}", s.reference, pin.number), p));
                }
            }
        }
        for l in &design.labels {
            union(&mut parent, at(l.position), Key::Name(l.text.clone()));
            points.push(l.position);
        }
        for w in &design.wires {
            points.push(w.start);
            points.push(w.end);
        }
        for w in &design.wires {
            union(&mut parent, at(w.start), at(w.end));
            for p in &points {
                if crate::layout::on_segment(*p, w.start, w.end) {
                    union(&mut parent, at(*p), at(w.start));
                }
            }
        }
        let mut ids: HashMap<Key, usize> = HashMap::new();
        let mut out = BTreeMap::new();
        for (name, p) in pins {
            let root = find(&mut parent, &at(p));
            let next = ids.len();
            out.insert(name, *ids.entry(root).or_insert(next));
        }
        out
    }

    fn assert_nets_hold(design: &SchematicDesign, built: &OperationPlan) {
        let groups = joined(design);
        let mut seen = BTreeMap::new();
        for net in &built.expected_nets {
            let pins: Vec<&String> = net.pins.iter().filter(|p| !p.starts_with('#')).collect();
            let Some(first) = pins.first() else { continue };
            let group = groups[*first];
            for p in &pins {
                assert_eq!(
                    groups[*p], group,
                    "{} is not joined to {} on net {}",
                    p, first, net.name
                );
            }
            if let Some(other) = seen.insert(group, net.name.clone()) {
                panic!("nets {other} and {} are joined", net.name);
            }
        }
    }

    /// A pin moves to another net, or two pins swap nets: only the wiring of the nets involved is
    /// redrawn, and the rest of the sheet holds still and stays correct.
    #[test]
    fn a_pin_moves_to_another_net_and_only_those_nets_are_redrawn() {
        use crate::pin_net::{plan_pin_net, PinNetRequest};
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let lib = library();
        // C5.1 is on +3V3; it goes to +5V.
        let plan = plan_pin_net(
            &design,
            &nets,
            &lib,
            &PinNetRequest::Move {
                pin: "C5.1".into(),
                to_net: "+5V".into(),
            },
        )
        .unwrap();
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        println!(
            "MOVE removals {} additions {}",
            plan.removals.len(),
            plan.operations.len()
        );
        assert!(plan.moves.is_empty());
        assert!(
            plan.removals.len() + plan.operations.len() < design.wires.len(),
            "far less than a redraw of the sheet: {} + {} of {} wires",
            plan.removals.len(),
            plan.operations.len(),
            design.wires.len()
        );
        // (Flags carry one name, PWR_FLAG, whatever net they are on; they are left out of the check.)
        let without_flags = |d: SchematicDesign| {
            let mut d = d;
            d.symbols.retain(|s| s.value != "PWR_FLAG");
            d
        };
        let after = without_flags(apply(&design, &plan));
        let groups = joined(&after);
        // C5.1 is now with the +5V parts, and the +3V3 parts are not with them.
        assert_eq!(groups["C5.1"], groups["C1.1"], "joined to +5V");
        assert_ne!(groups["C5.1"], groups["C3.1"], "no longer with +3V3");
        assert_ne!(groups["C3.1"], groups["C1.1"], "+3V3 and +5V stay apart");
        // The nets the plan promises hold for the whole sheet.
        assert_nets_hold(
            &after,
            &OperationPlan {
                expected_nets: plan.expected_nets.clone(),
                ..Default::default()
            },
        );
        // Wiring of nets that were not involved is untouched: no wire of the LED branch goes.
        let led_wire_ids: BTreeSet<&str> = design
            .wires
            .iter()
            .filter(|w| {
                crate::connectivity::net_at(&design, &lib, &nets, w.start).is_some_and(|n| n.name.contains("LED_PWR"))
            })
            .map(|w| w.id.as_str())
            .collect();
        assert!(!led_wire_ids.is_empty());
        assert!(plan.removals.iter().all(|r| !led_wire_ids.contains(r.id.as_str())));

        // A swap of a +5V pin and a +3V3 pin.
        let swap = plan_pin_net(
            &design,
            &nets,
            &lib,
            &PinNetRequest::Swap {
                a: "C2.1".into(),
                b: "C5.1".into(),
            },
        )
        .unwrap();
        assert_eq!(errors(&swap), Vec::<&Finding>::new(), "{:#?}", swap.findings);
        let after = without_flags(apply(&design, &swap));
        let groups = joined(&after);
        assert_eq!(groups["C2.1"], groups["C3.1"], "C2.1 is on +3V3");
        assert_eq!(groups["C5.1"], groups["C1.1"], "C5.1 is on +5V");
        assert_nets_hold(
            &after,
            &OperationPlan {
                expected_nets: swap.expected_nets.clone(),
                ..Default::default()
            },
        );

        // Refusals.
        let try_it = |r: PinNetRequest| plan_pin_net(&design, &nets, &lib, &r);
        assert!(
            try_it(PinNetRequest::Move {
                pin: "C5.1".into(),
                to_net: "+3V3".into()
            })
            .is_err(),
            "already there"
        );
        assert!(
            try_it(PinNetRequest::Move {
                pin: "C9.1".into(),
                to_net: "GND".into()
            })
            .is_err(),
            "no such part"
        );
        assert!(
            try_it(PinNetRequest::Move {
                pin: "C5.9".into(),
                to_net: "GND".into()
            })
            .is_err(),
            "no such pin"
        );
        assert!(
            try_it(PinNetRequest::Move {
                pin: "C5".into(),
                to_net: "GND".into()
            })
            .is_err(),
            "not REF.PIN"
        );
        assert!(
            try_it(PinNetRequest::Swap {
                a: "C3.1".into(),
                b: "C5.1".into()
            })
            .is_err(),
            "same net"
        );
        assert!(try_it(PinNetRequest::Swap {
            a: "C3.1".into(),
            b: "C3.1".into()
        })
        .is_err());
    }

    #[test]
    fn parts_moved_by_hand_are_wired_again_without_changing_a_net() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        // Spread the parts out: each part to a place of its own, the way a person might.
        let edits: Vec<crate::arrange::PartEdit> = design
            .symbols
            .iter()
            .filter(|s| !s.reference.starts_with('#'))
            .enumerate()
            .map(|(i, s)| crate::arrange::PartEdit {
                reference: s.reference.clone(),
                at: Some(Point::new(
                    Nm(60_960_000 + (i as i64 % 4) * 38_100_000),
                    Nm(63_500_000 + (i as i64 / 4) * 38_100_000),
                )),
                ..Default::default()
            })
            .collect();
        let arranged = crate::arrange::plan_arrange(&design, &library(), &edits, &[]);
        assert_eq!(errors(&arranged), Vec::<&Finding>::new(), "{:#?}", arranged.findings);
        assert!(!arranged.moves.is_empty());
        assert!(arranged.findings.iter().any(|f| f.code == "ARRANGE_WIRES_LEFT_BEHIND"));
        let moved = apply(&design, &arranged);

        let plan = plan_reconnect(&moved, &nets, &library());
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert!(
            plan.moves.is_empty(),
            "reconnecting must not move parts: {:#?}",
            plan.moves
        );
        assert!(plan.readability.shorts.is_empty(), "{:?}", plan.readability.shorts);
        assert!(
            plan.readability.wires_through_symbols.is_empty(),
            "{:?}",
            plan.readability
        );
        assert!(plan.fallback_nets.is_empty(), "{:?}", plan.fallback_nets);
        for net in built.expected_nets.iter().filter(|n| !n.name.starts_with("Net-(")) {
            let named = plan.operations.iter().any(|op| match op {
                Operation::AddLabel { net: n, .. } => *n == net.name,
                Operation::PlacePowerSymbol { net: n, .. } => *n == net.name,
                _ => false,
            });
            assert!(named, "net {} is not named in the drawing", net.name);
        }
        let wired = apply(&moved, &plan);
        assert_nets_hold(&wired, &built);
    }

    #[test]
    fn reconnecting_a_tidy_drawing_changes_nothing_that_matters() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let tidy = apply(&design, &plan_tidy(&design, &nets, &library()));
        let plan = plan_reconnect(&tidy, &nets, &library());
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert!(plan.moves.is_empty());
        assert_nets_hold(&apply(&tidy, &plan), &built);
    }

    #[test]
    fn a_kept_part_stays_and_the_others_make_room() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let first = apply(&design, &plan_tidy(&design, &nets, &library()));
        let keep = first
            .symbols
            .iter()
            .find(|s| s.reference.starts_with('C'))
            .unwrap()
            .reference
            .clone();
        // Put it somewhere the layout would not, then ask for it to stay there.
        let mut dragged = first.clone();
        let part = dragged.symbols.iter_mut().find(|s| s.reference == keep).unwrap();
        part.position = Point::new(Nm(part.position.x.0 + 63_500_000), Nm(part.position.y.0 + 50_800_000));
        let there = part.position;
        let hints = LayoutHints {
            keep: vec![keep.clone()],
            ..Default::default()
        };
        let plan = plan_tidy_with_hints(&dragged, &nets, &library(), &hints);
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert!(!plan.moves.iter().any(|m| m.reference == keep), "{keep} must not move");
        let done = apply(&dragged, &plan);
        assert_eq!(
            done.symbols.iter().find(|s| s.reference == keep).unwrap().position,
            there
        );
        assert_nets_hold(&done, &built);
    }

    #[test]
    fn a_part_can_be_placed_next_to_another() {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let first = apply(&design, &plan_tidy(&design, &nets, &library()));
        let mut parts = first.symbols.iter().filter(|s| !s.reference.starts_with('#'));
        let a = parts.next().unwrap().reference.clone();
        let b = parts.next_back().unwrap().reference.clone();
        let hints = LayoutHints {
            near: vec![NearHint {
                reference: b.clone(),
                to: a.clone(),
                side: Dir::Right,
                gap: Nm(10_160_000),
            }],
            ..Default::default()
        };
        let plan = plan_tidy_with_hints(&first, &nets, &library(), &hints);
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        let done = apply(&first, &plan);
        let body = |r: &str| {
            let s = done.symbols.iter().find(|s| s.reference == r).unwrap();
            let def = &library()[&LibraryId::parse(&s.library_id).unwrap()];
            def.body.unwrap().transformed(s.orientation).translate(s.position)
        };
        let gap = body(&b).min.x.0 - body(&a).max.x.0;
        assert!((gap - 10_160_000).abs() <= GRID.0, "gap was {gap}");
        assert_nets_hold(&done, &built);
    }

    #[test]
    fn hints_naming_a_missing_part_are_refused() {
        let (built, design) = built_regulator();
        let hints = LayoutHints {
            keep: vec!["R99".into()],
            ..Default::default()
        };
        let plan = plan_tidy_with_hints(&design, &nets_of(&built), &library(), &hints);
        assert!(errors(&plan).iter().any(|f| f.code == "TIDY_HINT_UNKNOWN_PART"));
    }

    #[test]
    fn arranging_refuses_what_it_cannot_do() {
        let (_, design) = built_regulator();
        let reference = design
            .symbols
            .iter()
            .find(|s| !s.reference.starts_with('#'))
            .unwrap()
            .reference
            .clone();
        let both = crate::arrange::plan_arrange(
            &design,
            &library(),
            &[crate::arrange::PartEdit {
                reference: reference.clone(),
                at: Some(Point::default()),
                by: Some(Point::default()),
                ..Default::default()
            }],
            &[],
        );
        assert!(errors(&both).iter().any(|f| f.code == "ARRANGE_AT_AND_BY"));
        let missing = crate::arrange::plan_arrange(&design, &library(), &[], &["#PWR01".into(), "Z9".into()]);
        assert_eq!(errors(&missing).len(), 2);
        let twisted = crate::arrange::plan_arrange(
            &design,
            &library(),
            &[crate::arrange::PartEdit {
                reference,
                rotate_deg: Some(45),
                ..Default::default()
            }],
            &[],
        );
        assert!(errors(&twisted).iter().any(|f| f.code == "ARRANGE_BAD_ROTATION"));
    }

    /// A tidy design and the nets KiCad would report for it.
    fn tidy_design() -> (OperationPlan, SchematicDesign, Vec<NetInfo>) {
        let (built, design) = built_regulator();
        let nets = nets_of(&built);
        let tidy = apply(&design, &plan_tidy(&design, &nets, &library()));
        (built, tidy, nets)
    }

    #[test]
    fn a_label_can_be_renamed_but_not_onto_another_net() {
        let (_, design, nets) = tidy_design();
        let label = design.labels.first().expect("the regulator has labels").clone();
        let other = design
            .labels
            .iter()
            .find(|l| l.text != label.text)
            .expect("two differently named labels")
            .text
            .clone();
        let edits = crate::edits::LabelEdits {
            rename: vec![(label.id.clone(), "RENAMED".into())],
            ..Default::default()
        };
        let plan = crate::edits::plan_labels(&design, &nets, &library(), &edits);
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert_eq!(plan.removals.len(), 1);
        assert!(plan
            .operations
            .iter()
            .any(|op| matches!(op, Operation::AddLabel { net, .. } if net == "RENAMED")));
        assert!(plan.expected_nets.iter().any(|n| n.name == "RENAMED"));

        let onto = crate::edits::LabelEdits {
            rename: vec![(label.id.clone(), other)],
            ..Default::default()
        };
        let refused = crate::edits::plan_labels(&design, &nets, &library(), &onto);
        assert!(
            errors(&refused).iter().any(|f| f.code == "LABEL_WOULD_MERGE_NETS"),
            "{:#?}",
            refused.findings
        );
    }

    #[test]
    fn a_label_cannot_give_a_named_net_a_second_name_or_sit_on_nothing() {
        let (_, design, nets) = tidy_design();
        let label = design.labels.first().unwrap().clone();
        let add = |text: &str, at: crate::edits::LabelAt| crate::edits::LabelEdits {
            add: vec![crate::edits::LabelAdd {
                text: text.into(),
                at,
                direction: None,
                allow_join: false,
            }],
            ..Default::default()
        };
        let second = crate::edits::plan_labels(
            &design,
            &nets,
            &library(),
            &add("ANOTHER", crate::edits::LabelAt::Point(label.position)),
        );
        assert!(
            errors(&second).iter().any(|f| f.code == "LABEL_NET_ALREADY_NAMED"),
            "{:#?}",
            second.findings
        );
        let nowhere = crate::edits::plan_labels(
            &design,
            &nets,
            &library(),
            &add(
                "X",
                crate::edits::LabelAt::Point(Point::new(Nm(500_000_000), Nm(500_000_000))),
            ),
        );
        assert!(errors(&nowhere).iter().any(|f| f.code == "LABEL_NOT_ON_NET"));
        let bad = crate::edits::plan_labels(
            &design,
            &nets,
            &library(),
            &add("  ", crate::edits::LabelAt::Point(label.position)),
        );
        assert!(errors(&bad).iter().any(|f| f.code == "LABEL_BAD_TEXT"));
        let pin = crate::edits::plan_labels(
            &design,
            &nets,
            &library(),
            &add(
                "X",
                crate::edits::LabelAt::Pin {
                    reference: "R99".into(),
                    pin: "1".into(),
                },
            ),
        );
        assert!(errors(&pin).iter().any(|f| f.code == "LABEL_UNKNOWN_PIN"));
    }

    #[test]
    fn removing_a_label_leaves_its_net_name_open() {
        let (_, design, nets) = tidy_design();
        let label = design.labels.first().unwrap().clone();
        let edits = crate::edits::LabelEdits {
            remove: vec![label.id.clone()],
            ..Default::default()
        };
        let plan = crate::edits::plan_labels(&design, &nets, &library(), &edits);
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        assert_eq!(plan.removals.len(), 1);
        assert!(
            plan.expected_nets.iter().any(|n| n.name_open),
            "{:#?}",
            plan.expected_nets
        );
    }

    #[test]
    fn a_box_goes_around_parts_and_carries_a_title() {
        let (_, design, nets) = tidy_design();
        let parts: Vec<String> = design
            .symbols
            .iter()
            .filter(|s| !s.reference.starts_with('#'))
            .take(2)
            .map(|s| s.reference.clone())
            .collect();
        let plan = crate::edits::plan_draw(
            &design,
            &nets,
            &library(),
            &[crate::edits::DrawItem::BoxAround {
                parts: parts.clone(),
                margin: Nm(5_080_000),
                title: Some("Power supply".into()),
                stroke: crate::edits::DEFAULT_STROKE,
            }],
        );
        assert_eq!(errors(&plan), Vec::<&Finding>::new(), "{:#?}", plan.findings);
        let rect = plan
            .operations
            .iter()
            .find_map(|op| match op {
                Operation::AddShape {
                    kind: crate::schematic::ShapeKind::Rectangle { min, max },
                    ..
                } => Some(Rect { min: *min, max: *max }),
                _ => None,
            })
            .expect("a rectangle");
        for reference in &parts {
            let s = design.symbols.iter().find(|s| &s.reference == reference).unwrap();
            let body = library()[&LibraryId::parse(&s.library_id).unwrap()]
                .body
                .unwrap()
                .transformed(s.orientation)
                .translate(s.position);
            assert!(
                rect.min.x <= body.min.x
                    && rect.min.y <= body.min.y
                    && rect.max.x >= body.max.x
                    && rect.max.y >= body.max.y
            );
        }
        assert!(plan
            .operations
            .iter()
            .any(|op| matches!(op, Operation::AddText { text, .. } if text == "Power supply")));
        assert!(
            !plan.expected_nets.is_empty(),
            "a drawing must leave every net as it is"
        );
        let missing = crate::edits::plan_draw(
            &design,
            &nets,
            &library(),
            &[crate::edits::DrawItem::BoxAround {
                parts: vec!["R99".into()],
                margin: Nm(0),
                title: None,
                stroke: crate::edits::DEFAULT_STROKE,
            }],
        );
        assert!(errors(&missing).iter().any(|f| f.code == "DRAW_UNKNOWN_PART"));
    }

    #[test]
    fn items_are_removed_or_moved_by_id() {
        let (_, mut design, nets) = tidy_design();
        design.texts.push(crate::schematic::DesignText {
            id: "note-1".into(),
            text: "hello".into(),
            position: Point::new(Nm(10_160_000), Nm(10_160_000)),
            size: crate::text::DEFAULT_SIZE,
            ..Default::default()
        });
        let wire = design.wires.first().unwrap().id.clone();
        let remove_note = crate::edits::plan_edit_items(&design, &nets, &library(), &["note-1".into()], &[]);
        assert_eq!(errors(&remove_note), Vec::<&Finding>::new());
        assert_eq!(remove_note.removals.len(), 1);
        assert!(!remove_note.expected_nets.is_empty(), "a note changes no net");

        let moved = crate::edits::plan_edit_items(
            &design,
            &nets,
            &library(),
            &[],
            &[("note-1".into(), Point::new(Nm(5_080_000), Nm(0)))],
        );
        assert_eq!(moved.removals.len(), 1);
        assert!(moved
            .operations
            .iter()
            .any(|op| matches!(op, Operation::AddText { position, .. } if position.x == Nm(15_240_000))));

        let drop_wire = crate::edits::plan_edit_items(&design, &nets, &library(), std::slice::from_ref(&wire), &[]);
        assert!(drop_wire.findings.iter().any(|f| f.code == "ITEMS_MAY_CHANGE_NETS"));
        assert!(
            drop_wire.expected_nets.is_empty(),
            "nets are not promised after a wire goes"
        );
        let move_wire = crate::edits::plan_edit_items(&design, &nets, &library(), &[], &[(wire, Point::default())]);
        assert!(errors(&move_wire).iter().any(|f| f.code == "ITEM_CANNOT_MOVE"));
        let part = design.symbols.first().unwrap().id.clone();
        let unknown = crate::edits::plan_edit_items(&design, &nets, &library(), &[part], &[]);
        assert!(errors(&unknown).iter().any(|f| f.code == "ITEM_NOT_FOUND"));
    }
}
