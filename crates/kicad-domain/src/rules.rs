// SPDX-License-Identifier: GPL-3.0-or-later
//! The board's design rules as a person sets them (Phase 14.3): net classes (track width,
//! clearance, via size, differential pair), which nets are in which class, the board's minimums, and
//! the solder mask and paste margins. A request becomes a list of single changes that is shown,
//! applied, and read back.
//!
//! The state here holds only the values this module knows about. What it does not know (colours,
//! microvia sizes, schematic widths) is left alone by the appliers, which change single fields of
//! what KiCad has.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::geometry::Nm;
use crate::result::{ErrorCode, ToolError};

/// A value a net class carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassField {
    Clearance,
    TrackWidth,
    ViaDiameter,
    ViaDrill,
    DiffPairWidth,
    DiffPairGap,
}

/// A minimum the whole board has to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MinField {
    Clearance,
    TrackWidth,
    ViaDiameter,
    ViaAnnular,
    ThroughHole,
    HoleToHole,
    CopperEdge,
    HoleClearance,
    SilkClearance,
}

/// A solder mask or paste margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskField {
    Expansion,
    MinWidth,
    ToCopper,
    PasteMargin,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassState {
    pub name: String,
    pub priority: Option<i32>,
    pub values: BTreeMap<ClassField, Nm>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RulesState {
    pub classes: Vec<ClassState>,
    /// Net name → classes assigned to it directly.
    pub assignments: BTreeMap<String, Vec<String>>,
    /// (wildcard pattern, class).
    pub patterns: Vec<(String, String)>,
    pub minimums: BTreeMap<MinField, Nm>,
    pub mask: BTreeMap<MaskField, Nm>,
    /// Paste margin as a fraction of the pad (-0.1 is 10 % smaller).
    pub paste_ratio: Option<f64>,
}

impl RulesState {
    pub fn class(&self, name: &str) -> Option<&ClassState> {
        self.classes.iter().find(|c| c.name == name)
    }
}

/// One change to the rules.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RuleOp {
    ClassAdd {
        name: String,
        priority: i32,
    },
    ClassRemove {
        name: String,
    },
    ClassSet {
        name: String,
        field: ClassField,
        value: Nm,
    },
    ClassPriority {
        name: String,
        priority: i32,
    },
    /// The net gets exactly this class (replacing what it had).
    Assign {
        net: String,
        class: String,
    },
    Unassign {
        net: String,
    },
    PatternSet {
        pattern: String,
        class: String,
    },
    PatternRemove {
        pattern: String,
    },
    MinSet {
        field: MinField,
        value: Nm,
    },
    MaskSet {
        field: MaskField,
        value: Nm,
    },
    PasteRatioSet {
        value: f64,
    },
}

/// A change and the value it replaces, for the person reading the plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleChange {
    #[serde(flatten)]
    pub op: RuleOp,
    /// What it was, as text, when there was something.
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassRequest {
    pub name: String,
    pub priority: Option<i32>,
    pub values: BTreeMap<ClassField, Nm>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignRequest {
    /// A net name, or a wildcard pattern (`*` and `?`), not both.
    pub net: Option<String>,
    pub pattern: Option<String>,
    pub class: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RulesRequest {
    pub classes: Vec<ClassRequest>,
    pub remove_classes: Vec<String>,
    pub assign: Vec<AssignRequest>,
    /// Net names or patterns whose assignment is removed.
    pub unassign: Vec<String>,
    pub minimums: BTreeMap<MinField, Nm>,
    pub mask: BTreeMap<MaskField, Nm>,
    pub paste_ratio: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RulesPlan {
    pub changes: Vec<RuleChange>,
    pub warnings: Vec<String>,
}

pub const DEFAULT_CLASS: &str = "Default";
const MAX_MM: f64 = 100.0;

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

/// Does `pattern` (with `*` for any run and `?` for one character) match all of `text`?
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn mm_text(v: Nm) -> String {
    format!("{} mm", (v.to_mm() * 1000.0).round() / 1000.0)
}

fn check_size(what: &str, v: Nm, allow_zero: bool) -> Result<(), ToolError> {
    if v.0 < 0 || (v.0 == 0 && !allow_zero) || v.to_mm() > MAX_MM {
        return Err(bad(format!(
            "{what} must be {} up to {MAX_MM} mm",
            if allow_zero { "zero or more, " } else { "above zero, " }
        )));
    }
    Ok(())
}

/// The change that makes `state` look as `op` says.
pub fn apply_op(state: &mut RulesState, op: &RuleOp) {
    match op {
        RuleOp::ClassAdd { name, priority } => state.classes.push(ClassState {
            name: name.clone(),
            priority: Some(*priority),
            values: BTreeMap::new(),
        }),
        RuleOp::ClassRemove { name } => state.classes.retain(|c| &c.name != name),
        RuleOp::ClassSet { name, field, value } => {
            if let Some(c) = state.classes.iter_mut().find(|c| &c.name == name) {
                c.values.insert(*field, *value);
            }
        }
        RuleOp::ClassPriority { name, priority } => {
            if let Some(c) = state.classes.iter_mut().find(|c| &c.name == name) {
                c.priority = Some(*priority);
            }
        }
        RuleOp::Assign { net, class } => {
            state.assignments.insert(net.clone(), vec![class.clone()]);
        }
        RuleOp::Unassign { net } => {
            state.assignments.remove(net);
        }
        RuleOp::PatternSet { pattern, class } => match state.patterns.iter_mut().find(|(p, _)| p == pattern) {
            Some(e) => e.1 = class.clone(),
            None => state.patterns.push((pattern.clone(), class.clone())),
        },
        RuleOp::PatternRemove { pattern } => state.patterns.retain(|(p, _)| p != pattern),
        RuleOp::MinSet { field, value } => {
            state.minimums.insert(*field, *value);
        }
        RuleOp::MaskSet { field, value } => {
            state.mask.insert(*field, *value);
        }
        RuleOp::PasteRatioSet { value } => state.paste_ratio = Some(*value),
    }
}

/// Plans `request` against the rules as they are. `nets` are the board's net names.
pub fn plan_rules(current: &RulesState, nets: &[String], request: &RulesRequest) -> Result<RulesPlan, ToolError> {
    let nothing = request.classes.is_empty()
        && request.remove_classes.is_empty()
        && request.assign.is_empty()
        && request.unassign.is_empty()
        && request.minimums.is_empty()
        && request.mask.is_empty()
        && request.paste_ratio.is_none();
    if nothing {
        return Err(bad(
            "name a change: net_classes, remove_net_classes, assign, unassign, minimums, or mask_paste (pcb_rules shows the current rules)",
        ));
    }
    let mut state = current.clone();
    let mut changes: Vec<RuleChange> = Vec::new();
    let mut warnings = Vec::new();
    let mut push = |state: &mut RulesState, op: RuleOp, from: Option<String>| {
        apply_op(state, &op);
        changes.push(RuleChange { op, from });
    };

    for c in &request.classes {
        if c.name.is_empty() || c.name.len() > 64 || c.name.chars().any(char::is_control) {
            return Err(bad("a net class name is 1 to 64 characters without control characters"));
        }
        let exists = state.class(&c.name).is_some();
        if !exists {
            if c.values.is_empty() {
                return Err(bad(format!(
                    "net class '{}' does not exist; give at least one value (track_width_mm, clearance_mm, ...) to create it",
                    c.name
                )));
            }
            let next = state.classes.iter().filter(|k| k.name != DEFAULT_CLASS).count() as i32;
            push(
                &mut state,
                RuleOp::ClassAdd {
                    name: c.name.clone(),
                    priority: c.priority.unwrap_or(next),
                },
                None,
            );
        } else if let Some(p) = c.priority {
            let was = state.class(&c.name).and_then(|k| k.priority);
            if c.name == DEFAULT_CLASS {
                return Err(bad("the Default class always has the lowest priority"));
            }
            if was != Some(p) {
                push(
                    &mut state,
                    RuleOp::ClassPriority {
                        name: c.name.clone(),
                        priority: p,
                    },
                    was.map(|v| v.to_string()),
                );
            }
        }
        for (field, value) in &c.values {
            check_size(&format!("{} of class '{}'", field_name(*field), c.name), *value, false)?;
            let was = state.class(&c.name).and_then(|k| k.values.get(field)).copied();
            if was == Some(*value) {
                continue;
            }
            push(
                &mut state,
                RuleOp::ClassSet {
                    name: c.name.clone(),
                    field: *field,
                    value: *value,
                },
                was.map(mm_text),
            );
        }
    }

    for name in &request.remove_classes {
        if name == DEFAULT_CLASS {
            return Err(bad("the Default class cannot be removed"));
        }
        if state.class(name).is_none() {
            return Err(bad(format!("net class '{name}' does not exist")));
        }
        // What pointed at it goes too, or it would point at nothing.
        let nets_in: Vec<String> = state
            .assignments
            .iter()
            .filter(|(_, v)| v.iter().any(|k| k == name))
            .map(|(n, _)| n.clone())
            .collect();
        for n in nets_in {
            push(&mut state, RuleOp::Unassign { net: n.clone() }, Some(name.clone()));
            warnings.push(format!(
                "net {n} was assigned to '{name}' and goes back to its own class"
            ));
        }
        let pats: Vec<String> = state
            .patterns
            .iter()
            .filter(|(_, k)| k == name)
            .map(|(p, _)| p.clone())
            .collect();
        for p in pats {
            push(
                &mut state,
                RuleOp::PatternRemove { pattern: p.clone() },
                Some(name.clone()),
            );
            warnings.push(format!("the pattern {p} pointed at '{name}' and is removed"));
        }
        push(&mut state, RuleOp::ClassRemove { name: name.clone() }, None);
    }

    for a in &request.assign {
        if state.class(&a.class).is_none() {
            return Err(bad(format!(
                "net class '{}' does not exist (it has: {})",
                a.class,
                state
                    .classes
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        match (&a.net, &a.pattern) {
            (Some(net), None) => {
                if !nets.iter().any(|n| n == net) {
                    return Err(bad(format!(
                        "net '{net}' is not on the board (use pcb_list_nets), or give a pattern"
                    )));
                }
                let was = state.assignments.get(net).map(|v| v.join(", "));
                if was.as_deref() == Some(a.class.as_str()) {
                    continue;
                }
                push(
                    &mut state,
                    RuleOp::Assign {
                        net: net.clone(),
                        class: a.class.clone(),
                    },
                    was,
                );
            }
            (None, Some(pattern)) => {
                if pattern.is_empty() {
                    return Err(bad("a pattern is not empty"));
                }
                let matched = nets.iter().filter(|n| glob_matches(pattern, n)).count();
                if matched == 0 {
                    warnings.push(format!("the pattern {pattern} matches no net on the board now"));
                }
                let was = state
                    .patterns
                    .iter()
                    .find(|(p, _)| p == pattern)
                    .map(|(_, k)| k.clone());
                if was.as_deref() == Some(a.class.as_str()) {
                    continue;
                }
                push(
                    &mut state,
                    RuleOp::PatternSet {
                        pattern: pattern.clone(),
                        class: a.class.clone(),
                    },
                    was,
                );
            }
            _ => {
                return Err(bad(
                    "each assignment gives a net or a pattern, not both and not neither",
                ))
            }
        }
    }
    for name in &request.unassign {
        if state.assignments.contains_key(name) {
            let was = state.assignments.get(name).map(|v| v.join(", "));
            push(&mut state, RuleOp::Unassign { net: name.clone() }, was);
        } else if let Some((_, k)) = state.patterns.iter().find(|(p, _)| p == name).cloned() {
            push(&mut state, RuleOp::PatternRemove { pattern: name.clone() }, Some(k));
        } else {
            return Err(bad(format!("'{name}' has no assignment or pattern to remove")));
        }
    }

    for (field, value) in &request.minimums {
        check_size(&format!("minimum {}", min_name(*field)), *value, true)?;
        let was = state.minimums.get(field).copied();
        if was != Some(*value) {
            push(
                &mut state,
                RuleOp::MinSet {
                    field: *field,
                    value: *value,
                },
                was.map(mm_text),
            );
        }
    }
    for (field, value) in &request.mask {
        check_size(&format!("mask/paste {}", mask_name(*field)), *value, true).or_else(|e| {
            if matches!(field, MaskField::PasteMargin | MaskField::Expansion)
                && value.0 < 0
                && value.to_mm().abs() <= MAX_MM
            {
                Ok(())
            } else {
                Err(e)
            }
        })?;
        let was = state.mask.get(field).copied();
        if was != Some(*value) {
            push(
                &mut state,
                RuleOp::MaskSet {
                    field: *field,
                    value: *value,
                },
                was.map(mm_text),
            );
        }
    }
    if let Some(r) = request.paste_ratio {
        if !(-1.0..=1.0).contains(&r) {
            return Err(bad(
                "paste_margin_ratio is between -1 and 1 (-0.1 is 10 % smaller than the pad)",
            ));
        }
        if state.paste_ratio.is_none_or(|w| (w - r).abs() > 1e-9) {
            let was = state.paste_ratio.map(|w| w.to_string());
            push(&mut state, RuleOp::PasteRatioSet { value: r }, was);
        }
    }

    if changes.is_empty() {
        return Err(bad("the rules are already as asked; nothing to change"));
    }

    // Checks on the rules as they would be.
    for c in &state.classes {
        let min = |f: MinField| state.minimums.get(&f).copied();
        let v = |f: ClassField| c.values.get(&f).copied();
        if let (Some(w), Some(m)) = (v(ClassField::TrackWidth), min(MinField::TrackWidth)) {
            if w < m {
                warnings.push(format!(
                    "class '{}' has a track width of {}, below the board minimum of {}; DRC flags every track routed with it",
                    c.name,
                    mm_text(w),
                    mm_text(m)
                ));
            }
        }
        if let (Some(w), Some(m)) = (v(ClassField::Clearance), min(MinField::Clearance)) {
            if w < m {
                warnings.push(format!(
                    "class '{}' has a clearance of {}, below the board minimum of {}; the minimum wins",
                    c.name,
                    mm_text(w),
                    mm_text(m)
                ));
            }
        }
        if let (Some(d), Some(m)) = (v(ClassField::ViaDiameter), min(MinField::ViaDiameter)) {
            if d < m {
                warnings.push(format!(
                    "class '{}' has a via of {}, below the board minimum via size of {}",
                    c.name,
                    mm_text(d),
                    mm_text(m)
                ));
            }
        }
        if let (Some(d), Some(drill)) = (v(ClassField::ViaDiameter), v(ClassField::ViaDrill)) {
            if drill >= d {
                return Err(bad(format!(
                    "class '{}': the via drill ({}) must be smaller than the via diameter ({})",
                    c.name,
                    mm_text(drill),
                    mm_text(d)
                )));
            }
            if let Some(a) = min(MinField::ViaAnnular) {
                if (d.0 - drill.0) / 2 < a.0 {
                    warnings.push(format!(
                        "class '{}': the via ring is {}, below the minimum annular width of {}",
                        c.name,
                        mm_text(Nm((d.0 - drill.0) / 2)),
                        mm_text(a)
                    ));
                }
            }
        }
        if let (Some(drill), Some(m)) = (v(ClassField::ViaDrill), min(MinField::ThroughHole)) {
            if drill < m {
                warnings.push(format!(
                    "class '{}' has a via drill of {}, below the board minimum hole of {}",
                    c.name,
                    mm_text(drill),
                    mm_text(m)
                ));
            }
        }
    }
    Ok(RulesPlan { changes, warnings })
}

/// Where the rules read back differ from what the plan set.
pub fn verify_rules(plan: &RulesPlan, after: &RulesState) -> Vec<String> {
    let near = |a: Nm, b: Nm| (a.0 - b.0).abs() <= 1000;
    let mut problems = Vec::new();
    for change in &plan.changes {
        match &change.op {
            RuleOp::ClassAdd { name, .. } => {
                if after.class(name).is_none() {
                    problems.push(format!("net class '{name}' is not there after applying"));
                }
            }
            RuleOp::ClassRemove { name } => {
                if after.class(name).is_some() {
                    problems.push(format!("net class '{name}' is still there"));
                }
            }
            RuleOp::ClassSet { name, field, value } => match after.class(name).and_then(|c| c.values.get(field)) {
                Some(v) if near(*v, *value) => {}
                other => problems.push(format!(
                    "class '{name}' {} reads back as {}, not {}",
                    field_name(*field),
                    other.map_or("unset".to_owned(), |v| mm_text(*v)),
                    mm_text(*value)
                )),
            },
            RuleOp::ClassPriority { name, priority } => {
                if after.class(name).and_then(|c| c.priority) != Some(*priority) {
                    problems.push(format!("class '{name}' priority is not {priority} after applying"));
                }
            }
            RuleOp::Assign { net, class } => {
                if after.assignments.get(net).map(|v| v.as_slice()) != Some(std::slice::from_ref(class)) {
                    problems.push(format!("net {net} is not assigned to '{class}' after applying"));
                }
            }
            RuleOp::Unassign { net } => {
                if after.assignments.contains_key(net) {
                    problems.push(format!("net {net} is still assigned after applying"));
                }
            }
            RuleOp::PatternSet { pattern, class } => {
                if !after.patterns.iter().any(|(p, k)| p == pattern && k == class) {
                    problems.push(format!(
                        "the pattern {pattern} does not point at '{class}' after applying"
                    ));
                }
            }
            RuleOp::PatternRemove { pattern } => {
                if after.patterns.iter().any(|(p, _)| p == pattern) {
                    problems.push(format!("the pattern {pattern} is still there"));
                }
            }
            RuleOp::MinSet { field, value } => match after.minimums.get(field) {
                Some(v) if near(*v, *value) => {}
                other => problems.push(format!(
                    "minimum {} reads back as {}, not {}",
                    min_name(*field),
                    other.map_or("unset".to_owned(), |v| mm_text(*v)),
                    mm_text(*value)
                )),
            },
            RuleOp::MaskSet { field, value } => match after.mask.get(field) {
                Some(v) if near(*v, *value) => {}
                other => problems.push(format!(
                    "{} reads back as {}, not {}",
                    mask_name(*field),
                    other.map_or("unset".to_owned(), |v| mm_text(*v)),
                    mm_text(*value)
                )),
            },
            RuleOp::PasteRatioSet { value } => {
                if after.paste_ratio.is_none_or(|v| (v - value).abs() > 1e-6) {
                    problems.push(format!(
                        "the paste margin ratio reads back as {:?}, not {value}",
                        after.paste_ratio
                    ));
                }
            }
        }
    }
    problems
}

pub fn field_name(f: ClassField) -> &'static str {
    match f {
        ClassField::Clearance => "clearance",
        ClassField::TrackWidth => "track width",
        ClassField::ViaDiameter => "via diameter",
        ClassField::ViaDrill => "via drill",
        ClassField::DiffPairWidth => "differential pair width",
        ClassField::DiffPairGap => "differential pair gap",
    }
}

pub fn min_name(f: MinField) -> &'static str {
    match f {
        MinField::Clearance => "clearance",
        MinField::TrackWidth => "track width",
        MinField::ViaDiameter => "via diameter",
        MinField::ViaAnnular => "via annular width",
        MinField::ThroughHole => "through hole",
        MinField::HoleToHole => "hole to hole",
        MinField::CopperEdge => "copper to edge",
        MinField::HoleClearance => "copper to hole",
        MinField::SilkClearance => "silkscreen clearance",
    }
}

pub fn mask_name(f: MaskField) -> &'static str {
    match f {
        MaskField::Expansion => "solder mask expansion",
        MaskField::MinWidth => "solder mask minimum web",
        MaskField::ToCopper => "solder mask to copper",
        MaskField::PasteMargin => "paste margin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nm(mm: f64) -> Nm {
        Nm::from_mm(mm).unwrap()
    }

    fn state() -> RulesState {
        let mut s = RulesState::default();
        s.classes.push(ClassState {
            name: "Default".into(),
            priority: Some(i32::MAX),
            values: BTreeMap::from([
                (ClassField::Clearance, nm(0.2)),
                (ClassField::TrackWidth, nm(0.2)),
                (ClassField::ViaDiameter, nm(0.6)),
                (ClassField::ViaDrill, nm(0.3)),
            ]),
        });
        s.minimums.insert(MinField::TrackWidth, nm(0.2));
        s.minimums.insert(MinField::ViaAnnular, nm(0.1));
        s
    }

    fn nets() -> Vec<String> {
        ["GND", "+3V3", "/VBUS", "/VBUS_F", "D+"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn class(name: &str, values: &[(ClassField, f64)]) -> ClassRequest {
        ClassRequest {
            name: name.into(),
            priority: None,
            values: values.iter().map(|(f, v)| (*f, nm(*v))).collect(),
        }
    }

    #[test]
    fn patterns_match_like_a_shell() {
        assert!(glob_matches("/VBUS*", "/VBUS_F"));
        assert!(glob_matches("*", "GND"));
        assert!(glob_matches("D?", "D+"));
        assert!(!glob_matches("D?", "D"));
        assert!(!glob_matches("GND", "GND2"));
        assert!(glob_matches("*3V3", "+3V3"));
    }

    #[test]
    fn a_new_class_is_added_set_and_assigned() {
        let req = RulesRequest {
            classes: vec![class(
                "Power",
                &[(ClassField::TrackWidth, 0.5), (ClassField::Clearance, 0.25)],
            )],
            assign: vec![
                AssignRequest {
                    net: Some("GND".into()),
                    pattern: None,
                    class: "Power".into(),
                },
                AssignRequest {
                    net: None,
                    pattern: Some("/VBUS*".into()),
                    class: "Power".into(),
                },
            ],
            ..Default::default()
        };
        let plan = plan_rules(&state(), &nets(), &req).unwrap();
        assert!(matches!(&plan.changes[0].op, RuleOp::ClassAdd { name, priority: 0 } if name == "Power"));
        assert_eq!(plan.changes.len(), 5, "{:#?}", plan.changes);
        let mut after = state();
        for c in &plan.changes {
            apply_op(&mut after, &c.op);
        }
        assert!(verify_rules(&plan, &after).is_empty());
        assert!(!verify_rules(&plan, &state()).is_empty());
        assert_eq!(after.assignments["GND"], ["Power"]);
        assert_eq!(after.patterns, [("/VBUS*".to_owned(), "Power".to_owned())]);
    }

    #[test]
    fn changes_show_what_they_replace_and_skip_what_is_already_so() {
        let req = RulesRequest {
            classes: vec![class(
                "Default",
                &[(ClassField::TrackWidth, 0.25), (ClassField::Clearance, 0.2)],
            )],
            ..Default::default()
        };
        let plan = plan_rules(&state(), &nets(), &req).unwrap();
        assert_eq!(plan.changes.len(), 1, "the clearance is already 0.2");
        assert_eq!(plan.changes[0].from.as_deref(), Some("0.2 mm"));
        let again = RulesRequest {
            classes: vec![class("Default", &[(ClassField::Clearance, 0.2)])],
            ..Default::default()
        };
        assert!(plan_rules(&state(), &nets(), &again).is_err());
    }

    #[test]
    fn removing_a_class_takes_its_assignments_with_it() {
        let mut s = state();
        s.classes.push(ClassState {
            name: "HV".into(),
            priority: Some(0),
            values: BTreeMap::new(),
        });
        s.assignments.insert("D+".into(), vec!["HV".into()]);
        s.patterns.push(("/VBUS*".into(), "HV".into()));
        let req = RulesRequest {
            remove_classes: vec!["HV".into()],
            ..Default::default()
        };
        let plan = plan_rules(&s, &nets(), &req).unwrap();
        assert_eq!(plan.changes.len(), 3);
        assert!(plan.warnings.iter().any(|w| w.contains("D+")));
        let mut after = s.clone();
        for c in &plan.changes {
            apply_op(&mut after, &c.op);
        }
        assert!(after.assignments.is_empty() && after.patterns.is_empty() && after.class("HV").is_none());
        assert!(verify_rules(&plan, &after).is_empty());
        let default = RulesRequest {
            remove_classes: vec!["Default".into()],
            ..Default::default()
        };
        assert!(plan_rules(&s, &nets(), &default).is_err());
    }

    #[test]
    fn minimums_and_masks() {
        let req = RulesRequest {
            minimums: BTreeMap::from([(MinField::TrackWidth, nm(0.15)), (MinField::HoleToHole, nm(0.25))]),
            mask: BTreeMap::from([(MaskField::Expansion, nm(0.05))]),
            paste_ratio: Some(-0.1),
            ..Default::default()
        };
        let plan = plan_rules(&state(), &nets(), &req).unwrap();
        assert_eq!(plan.changes.len(), 4);
        let mut after = state();
        for c in &plan.changes {
            apply_op(&mut after, &c.op);
        }
        assert!(verify_rules(&plan, &after).is_empty());
        let too_big = RulesRequest {
            paste_ratio: Some(2.0),
            ..Default::default()
        };
        assert!(plan_rules(&state(), &nets(), &too_big).is_err());
    }

    #[test]
    fn mistakes_are_refused_and_conflicts_warned() {
        let s = state();
        let n = nets();
        let r = |req: RulesRequest| plan_rules(&s, &n, &req);
        assert!(r(RulesRequest::default()).is_err(), "nothing asked");
        assert!(
            r(RulesRequest {
                classes: vec![class("New", &[])],
                ..Default::default()
            })
            .is_err(),
            "a new class needs a value"
        );
        assert!(r(RulesRequest {
            classes: vec![class("X", &[(ClassField::TrackWidth, 0.0)])],
            ..Default::default()
        })
        .is_err());
        assert!(r(RulesRequest {
            assign: vec![AssignRequest {
                net: Some("NOPE".into()),
                pattern: None,
                class: "Default".into()
            }],
            ..Default::default()
        })
        .is_err());
        assert!(r(RulesRequest {
            assign: vec![AssignRequest {
                net: Some("GND".into()),
                pattern: None,
                class: "Ghost".into()
            }],
            ..Default::default()
        })
        .is_err());
        assert!(r(RulesRequest {
            assign: vec![AssignRequest {
                net: Some("GND".into()),
                pattern: Some("*".into()),
                class: "Default".into()
            }],
            ..Default::default()
        })
        .is_err());
        assert!(r(RulesRequest {
            unassign: vec!["GND".into()],
            ..Default::default()
        })
        .is_err());
        let drill = RulesRequest {
            classes: vec![class(
                "X",
                &[(ClassField::ViaDiameter, 0.4), (ClassField::ViaDrill, 0.5)],
            )],
            ..Default::default()
        };
        assert!(r(drill).is_err(), "a drill wider than the via");
        // Below the board minimum: allowed, with a warning.
        let thin = RulesRequest {
            classes: vec![class("Fine", &[(ClassField::TrackWidth, 0.1)])],
            ..Default::default()
        };
        let plan = r(thin).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("below the board minimum")),
            "{:?}",
            plan.warnings
        );
        // A pattern that matches nothing: allowed, with a warning.
        let none = RulesRequest {
            assign: vec![AssignRequest {
                net: None,
                pattern: Some("ZZ*".into()),
                class: "Default".into(),
            }],
            ..Default::default()
        };
        assert!(r(none).unwrap().warnings.iter().any(|w| w.contains("matches no net")));
    }
}
