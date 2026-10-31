// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading and changing a board's design rules (Phase 14.3).
//!
//! In a live session the rules go through the API: net classes and their assignments (project
//! commands) and the board's minimums and mask/paste margins (`SetBoardDesignRules`); KiCad saves
//! them to the project file with the board (checked in the Phase 14.0 probe). With no editor, a
//! file session edits the same keys of the `.kicad_pro` file, after `kicad-cli` has checked a copy.
//! Fields this code does not know are left as KiCad has them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use kicad_domain::rules::{
    apply_op, ClassField, ClassState, MaskField, MinField, RuleChange, RuleOp, RulesPlan, RulesState,
};
use kicad_domain::{ErrorCode, Nm, ToolError};
use kicad_ipc::proto::kiapi::board::commands::{
    BoardDesignRulesResponse, BoardStackupResponse, GetBoardDesignRules, GetBoardStackup, SetBoardDesignRules,
};
use kicad_ipc::proto::kiapi::board::types::{DrillProperties, PadStack, PadStackLayer};
use kicad_ipc::proto::kiapi::common::commands::{
    GetNetClassAssignments, GetNetClasses, NetClassAssignmentsResponse, NetClassesResponse, SetNetClassAssignments,
    SetNetClasses,
};
use kicad_ipc::proto::kiapi::common::project::{
    NetClass, NetClassAssignment, NetClassBoardSettings, NetClassPatternAssignment, NetClassType,
};
use kicad_ipc::proto::kiapi::common::types::{Distance, MapMergeMode, Vector2};
use serde_json::{json, Value};

use super::BoardSession;
use crate::errors::from_ipc_error;
use crate::session::{lock_file, Provider};

fn d(v: Nm) -> Option<Distance> {
    Some(Distance { value_nm: v.0 })
}

fn nm_of(v: &Option<Distance>) -> Option<Nm> {
    v.as_ref().map(|d| Nm(d.value_nm))
}

/// The rules in a net class message.
fn class_state(c: &NetClass) -> ClassState {
    let mut values = BTreeMap::new();
    if let Some(b) = &c.board {
        let mut put = |f: ClassField, v: Option<Nm>| {
            if let Some(v) = v.filter(|v| v.0 > 0) {
                values.insert(f, v);
            }
        };
        put(ClassField::Clearance, nm_of(&b.clearance));
        put(ClassField::TrackWidth, nm_of(&b.track_width));
        put(ClassField::DiffPairWidth, nm_of(&b.diff_pair_track_width));
        put(ClassField::DiffPairGap, nm_of(&b.diff_pair_gap));
        if let Some(stack) = &b.via_stack {
            put(
                ClassField::ViaDiameter,
                stack
                    .copper_layers
                    .first()
                    .and_then(|l| l.size.as_ref())
                    .map(|s| Nm(s.x_nm)),
            );
            put(
                ClassField::ViaDrill,
                stack
                    .drill
                    .as_ref()
                    .and_then(|d| d.diameter.as_ref())
                    .map(|s| Nm(s.x_nm)),
            );
        }
    }
    ClassState {
        name: c.name.clone(),
        priority: c.priority,
        values,
    }
}

/// Sets one value on a net class message, leaving the rest of it as it is.
fn set_class_field(c: &mut NetClass, field: ClassField, value: Nm) {
    let board = c.board.get_or_insert_with(NetClassBoardSettings::default);
    match field {
        ClassField::Clearance => board.clearance = d(value),
        ClassField::TrackWidth => board.track_width = d(value),
        ClassField::DiffPairWidth => board.diff_pair_track_width = d(value),
        ClassField::DiffPairGap => board.diff_pair_gap = d(value),
        ClassField::ViaDiameter | ClassField::ViaDrill => {
            let stack = board.via_stack.get_or_insert_with(PadStack::default);
            if field == ClassField::ViaDiameter {
                let size = Vector2 {
                    x_nm: value.0,
                    y_nm: value.0,
                };
                match stack.copper_layers.first_mut() {
                    Some(l) => l.size = Some(size),
                    None => stack.copper_layers.push(PadStackLayer {
                        size: Some(size),
                        ..Default::default()
                    }),
                }
            } else {
                let drill = stack.drill.get_or_insert_with(DrillProperties::default);
                drill.diameter = Some(Vector2 {
                    x_nm: value.0,
                    y_nm: value.0,
                });
            }
        }
    }
}

fn kerr(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::KicadError, message)
}

impl BoardSession {
    /// The design rules as they are now.
    pub async fn read_rules(&self) -> Result<RulesState, ToolError> {
        match &self.ipc {
            Some(ipc) => {
                let mut ipc = ipc.lock().await;
                let project = ipc.document.project.clone();
                let document = ipc.document.clone();
                let classes: NetClassesResponse = ipc
                    .client
                    .call(&GetNetClasses {
                        project: project.clone(),
                    })
                    .await
                    .map_err(|e| from_ipc_error(&e, false))?;
                let assigned: NetClassAssignmentsResponse = ipc
                    .client
                    .call(&GetNetClassAssignments { project })
                    .await
                    .map_err(|e| from_ipc_error(&e, false))?;
                let rules: BoardDesignRulesResponse = ipc
                    .client
                    .call(&GetBoardDesignRules { board: Some(document) })
                    .await
                    .map_err(|e| from_ipc_error(&e, false))?;
                let mut state = RulesState {
                    classes: classes
                        .net_classes
                        .iter()
                        .filter(|c| c.r#type != NetClassType::NctImplicit as i32)
                        .map(class_state)
                        .collect(),
                    ..Default::default()
                };
                for a in assigned.assignments {
                    state.assignments.insert(a.net, a.netclasses);
                }
                for p in assigned.pattern_assignments {
                    state.patterns.push((p.pattern, p.netclass));
                }
                if let Some(r) = rules.rules {
                    if let Some(c) = r.constraints {
                        let mut put = |f: MinField, v: Option<Nm>| {
                            if let Some(v) = v {
                                state.minimums.insert(f, v);
                            }
                        };
                        put(MinField::Clearance, nm_of(&c.min_clearance));
                        put(MinField::TrackWidth, nm_of(&c.min_track_width));
                        put(MinField::ViaDiameter, nm_of(&c.min_via_size));
                        put(MinField::ViaAnnular, nm_of(&c.min_via_annular_width));
                        put(MinField::ThroughHole, nm_of(&c.min_through_drill));
                        put(MinField::HoleToHole, nm_of(&c.hole_to_hole_min));
                        put(MinField::CopperEdge, nm_of(&c.copper_edge_clearance));
                        put(MinField::HoleClearance, nm_of(&c.hole_clearance));
                        put(MinField::SilkClearance, nm_of(&c.silk_clearance));
                    }
                    if let Some(m) = r.solder_mask_paste {
                        let mut put = |f: MaskField, v: Option<Nm>| {
                            if let Some(v) = v {
                                state.mask.insert(f, v);
                            }
                        };
                        put(MaskField::Expansion, nm_of(&m.mask_expansion));
                        put(MaskField::MinWidth, nm_of(&m.mask_min_width));
                        put(MaskField::ToCopper, nm_of(&m.mask_to_copper_clearance));
                        put(MaskField::PasteMargin, nm_of(&m.paste_margin));
                        state.paste_ratio = Some(m.paste_margin_ratio);
                    }
                }
                Ok(state)
            }
            None => self.read_rules_file(),
        }
    }

    /// The stackup (live sessions): its layers, thickness, and material.
    pub async fn read_stackup(&self) -> Result<Value, ToolError> {
        let Some(ipc) = &self.ipc else {
            return Ok(Value::Null);
        };
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let r: BoardStackupResponse = ipc
            .client
            .call(&GetBoardStackup { board: Some(document) })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let Some(s) = r.stackup else {
            return Ok(Value::Null);
        };
        let mm = |d: &Option<Distance>| d.as_ref().map(|d| (d.value_nm as f64 / 1e6 * 1000.0).round() / 1000.0);
        Ok(json!({
            "finish": s.finish.map(|f| f.type_name),
            "layers": s.layers.iter().map(|l| json!({
                "layer": l.user_name, "type": format!("{:?}", l.r#type()), "enabled": l.enabled,
                "thickness_mm": mm(&l.thickness), "material": l.material_name,
            })).collect::<Vec<_>>(),
        }))
    }

    /// Makes the changes of `plan`. A live session changes the rules in the open board (unsaved
    /// until `save`); a file session rewrites the project file.
    pub async fn apply_rules(&self, plan: &RulesPlan) -> Result<(), ToolError> {
        match &self.ipc {
            Some(_) => self.apply_rules_api(&plan.changes).await,
            None => self.apply_rules_file(plan).await,
        }
    }

    async fn apply_rules_api(&self, changes: &[RuleChange]) -> Result<(), ToolError> {
        let ipc = self.ipc.as_ref().expect("a live session");
        let mut ipc = ipc.lock().await;
        let project = ipc.document.project.clone();
        let document = ipc.document.clone();
        let client = &mut ipc.client;

        // Net classes: the explicit ones as KiCad has them, changed field by field.
        let touches_classes = changes.iter().any(|c| {
            matches!(
                c.op,
                RuleOp::ClassAdd { .. }
                    | RuleOp::ClassRemove { .. }
                    | RuleOp::ClassSet { .. }
                    | RuleOp::ClassPriority { .. }
            )
        });
        if touches_classes {
            let current: NetClassesResponse = client
                .call(&GetNetClasses {
                    project: project.clone(),
                })
                .await
                .map_err(|e| from_ipc_error(&e, false))?;
            let mut classes: Vec<NetClass> = current
                .net_classes
                .into_iter()
                .filter(|c| c.r#type != NetClassType::NctImplicit as i32)
                .collect();
            let mut touched: Vec<String> = Vec::new();
            let mut removed = false;
            for change in changes {
                match &change.op {
                    RuleOp::ClassAdd { name, priority } => {
                        classes.push(NetClass {
                            name: name.clone(),
                            priority: Some(*priority),
                            r#type: NetClassType::NctExplicit as i32,
                            ..Default::default()
                        });
                        touched.push(name.clone());
                    }
                    RuleOp::ClassRemove { name } => {
                        classes.retain(|c| &c.name != name);
                        removed = true;
                    }
                    RuleOp::ClassSet { name, field, value } => {
                        if let Some(c) = classes.iter_mut().find(|c| &c.name == name) {
                            set_class_field(c, *field, *value);
                            touched.push(name.clone());
                        }
                    }
                    RuleOp::ClassPriority { name, priority } => {
                        if let Some(c) = classes.iter_mut().find(|c| &c.name == name) {
                            c.priority = Some(*priority);
                            touched.push(name.clone());
                        }
                    }
                    _ => {}
                }
            }
            // A removal needs the whole list (replace); otherwise only what changed (merge).
            let (list, mode) = if removed {
                (classes, MapMergeMode::MmmReplace)
            } else {
                (
                    classes.into_iter().filter(|c| touched.contains(&c.name)).collect(),
                    MapMergeMode::MmmMerge,
                )
            };
            client
                .call_any(&SetNetClasses {
                    net_classes: list,
                    merge_mode: mode as i32,
                    project: project.clone(),
                })
                .await
                .map_err(|e| from_ipc_error(&e, true))?;
        }

        let mut assignments = Vec::new();
        let mut patterns = Vec::new();
        for change in changes {
            match &change.op {
                RuleOp::Assign { net, class } => assignments.push(NetClassAssignment {
                    net: net.clone(),
                    netclasses: vec![class.clone()],
                }),
                RuleOp::Unassign { net } => assignments.push(NetClassAssignment {
                    net: net.clone(),
                    netclasses: vec![],
                }),
                RuleOp::PatternSet { pattern, class } => patterns.push(NetClassPatternAssignment {
                    pattern: pattern.clone(),
                    netclass: class.clone(),
                }),
                RuleOp::PatternRemove { pattern } => patterns.push(NetClassPatternAssignment {
                    pattern: pattern.clone(),
                    netclass: String::new(),
                }),
                _ => {}
            }
        }
        if !assignments.is_empty() || !patterns.is_empty() {
            client
                .call_any(&SetNetClassAssignments {
                    project,
                    merge_mode: MapMergeMode::MmmMerge as i32,
                    assignments,
                    pattern_assignments: patterns,
                })
                .await
                .map_err(|e| from_ipc_error(&e, true))?;
        }

        let board_rules = changes.iter().any(|c| {
            matches!(
                c.op,
                RuleOp::MinSet { .. } | RuleOp::MaskSet { .. } | RuleOp::PasteRatioSet { .. }
            )
        });
        if board_rules {
            let current: BoardDesignRulesResponse = client
                .call(&GetBoardDesignRules {
                    board: Some(document.clone()),
                })
                .await
                .map_err(|e| from_ipc_error(&e, false))?;
            let mut rules = current.rules.ok_or_else(|| kerr("KiCad returned no design rules"))?;
            let constraints = rules.constraints.get_or_insert_with(Default::default);
            let mask = rules.solder_mask_paste.get_or_insert_with(Default::default);
            for change in changes {
                match &change.op {
                    RuleOp::MinSet { field, value } => {
                        let v = d(*value);
                        match field {
                            MinField::Clearance => constraints.min_clearance = v,
                            MinField::TrackWidth => constraints.min_track_width = v,
                            MinField::ViaDiameter => constraints.min_via_size = v,
                            MinField::ViaAnnular => constraints.min_via_annular_width = v,
                            MinField::ThroughHole => constraints.min_through_drill = v,
                            MinField::HoleToHole => constraints.hole_to_hole_min = v,
                            MinField::CopperEdge => constraints.copper_edge_clearance = v,
                            MinField::HoleClearance => constraints.hole_clearance = v,
                            MinField::SilkClearance => constraints.silk_clearance = v,
                        }
                    }
                    RuleOp::MaskSet { field, value } => {
                        let v = d(*value);
                        match field {
                            MaskField::Expansion => mask.mask_expansion = v,
                            MaskField::MinWidth => mask.mask_min_width = v,
                            MaskField::ToCopper => mask.mask_to_copper_clearance = v,
                            MaskField::PasteMargin => mask.paste_margin = v,
                        }
                    }
                    RuleOp::PasteRatioSet { value } => mask.paste_margin_ratio = *value,
                    _ => {}
                }
            }
            client
                .call::<_, BoardDesignRulesResponse>(&SetBoardDesignRules {
                    board: Some(document),
                    rules: Some(rules),
                })
                .await
                .map_err(|e| from_ipc_error(&e, true))?;
        }
        Ok(())
    }

    // ---- file sessions: the `.kicad_pro` file -------------------------------------------------

    fn project_path(&self) -> PathBuf {
        self.document_path.with_extension("kicad_pro")
    }

    fn read_project(&self) -> Result<Value, ToolError> {
        let path = self.project_path();
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ToolError::new(
                ErrorCode::InvalidArgument,
                format!("cannot read the project file {}: {e}", path.display()),
            )
        })?;
        serde_json::from_str(&text).map_err(|e| kerr(format!("{} is not valid JSON: {e}", path.display())))
    }

    fn read_rules_file(&self) -> Result<RulesState, ToolError> {
        let pro = self.read_project()?;
        let mm = |v: &Value| v.as_f64().and_then(|x| Nm::from_mm(x).ok());
        let mut state = RulesState::default();
        for c in pro["net_settings"]["classes"].as_array().into_iter().flatten() {
            let mut values = BTreeMap::new();
            for (key, field) in CLASS_KEYS {
                if let Some(v) = mm(&c[*key]).filter(|v| v.0 > 0) {
                    values.insert(*field, v);
                }
            }
            state.classes.push(ClassState {
                name: c["name"].as_str().unwrap_or_default().to_owned(),
                priority: c["priority"].as_i64().map(|p| p as i32),
                values,
            });
        }
        if let Some(map) = pro["net_settings"]["netclass_assignments"].as_object() {
            for (net, v) in map {
                let classes: Vec<String> = match v {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
                    _ => vec![],
                };
                state.assignments.insert(net.clone(), classes);
            }
        }
        for p in pro["net_settings"]["netclass_patterns"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let (Some(pattern), Some(class)) = (p["pattern"].as_str(), p["netclass"].as_str()) {
                state.patterns.push((pattern.to_owned(), class.to_owned()));
            }
        }
        for (key, field) in MIN_KEYS {
            if let Some(v) = mm(&pro["board"]["design_settings"]["rules"][*key]) {
                state.minimums.insert(*field, v);
            }
        }
        Ok(state)
    }

    async fn apply_rules_file(&self, plan: &RulesPlan) -> Result<(), ToolError> {
        if plan
            .changes
            .iter()
            .any(|c| matches!(c.op, RuleOp::MaskSet { .. } | RuleOp::PasteRatioSet { .. }))
        {
            return Err(ToolError::new(
                ErrorCode::FeatureUnavailable,
                "solder mask and paste margins are stored in the board file, which a file session does not edit; \
                 open the board with a live KiCad 11 session for those",
            ));
        }
        for file in [self.document_path.clone(), self.project_path()] {
            if lock_file(&file).is_some_and(|l| l.exists()) {
                return Err(ToolError::new(
                    ErrorCode::DocumentOpenElsewhere,
                    format!(
                        "{} is open in an editor; close it, or use the live session",
                        file.display()
                    ),
                ));
            }
        }
        let mut pro = self.read_project()?;
        let current = self.read_rules_file()?;
        let version = pro["net_settings"]["meta"]["version"].as_i64().unwrap_or(0);
        let mm = |v: Nm| json!(v.to_mm());
        let mut state = current.clone();
        for change in &plan.changes {
            apply_op(&mut state, &change.op);
            match &change.op {
                RuleOp::ClassAdd { name, priority } => {
                    pro["net_settings"]["classes"]
                        .as_array_mut()
                        .ok_or_else(|| kerr("the project file has no net classes list"))?
                        .push(
                            json!({ "name": name, "priority": priority, "pcb_color": "rgba(0, 0, 0, 0.000)",
                                      "schematic_color": "rgba(0, 0, 0, 0.000)", "tuning_profile": "" }),
                        );
                }
                RuleOp::ClassRemove { name } => {
                    if let Some(list) = pro["net_settings"]["classes"].as_array_mut() {
                        list.retain(|c| c["name"].as_str() != Some(name));
                    }
                }
                RuleOp::ClassSet { name, field, value } => {
                    let key = CLASS_KEYS
                        .iter()
                        .find(|(_, f)| f == field)
                        .map(|(k, _)| *k)
                        .expect("every field has a key");
                    if let Some(c) = pro["net_settings"]["classes"]
                        .as_array_mut()
                        .and_then(|l| l.iter_mut().find(|c| c["name"].as_str() == Some(name)))
                    {
                        c[key] = mm(*value);
                    }
                }
                RuleOp::ClassPriority { name, priority } => {
                    if let Some(c) = pro["net_settings"]["classes"]
                        .as_array_mut()
                        .and_then(|l| l.iter_mut().find(|c| c["name"].as_str() == Some(name)))
                    {
                        c["priority"] = json!(priority);
                    }
                }
                RuleOp::Assign { net, class } => {
                    let v = if version >= 5 { json!([class]) } else { json!(class) };
                    if !pro["net_settings"]["netclass_assignments"].is_object() {
                        pro["net_settings"]["netclass_assignments"] = json!({});
                    }
                    pro["net_settings"]["netclass_assignments"][net.as_str()] = v;
                }
                RuleOp::Unassign { net } => {
                    if let Some(m) = pro["net_settings"]["netclass_assignments"].as_object_mut() {
                        m.remove(net);
                    }
                }
                RuleOp::PatternSet { pattern, class } => {
                    if !pro["net_settings"]["netclass_patterns"].is_array() {
                        pro["net_settings"]["netclass_patterns"] = json!([]);
                    }
                    let list = pro["net_settings"]["netclass_patterns"]
                        .as_array_mut()
                        .expect("an array");
                    match list.iter_mut().find(|p| p["pattern"].as_str() == Some(pattern)) {
                        Some(p) => p["netclass"] = json!(class),
                        None => list.push(json!({ "netclass": class, "pattern": pattern })),
                    }
                }
                RuleOp::PatternRemove { pattern } => {
                    if let Some(list) = pro["net_settings"]["netclass_patterns"].as_array_mut() {
                        list.retain(|p| p["pattern"].as_str() != Some(pattern));
                    }
                }
                RuleOp::MinSet { field, value } => {
                    let key = MIN_KEYS
                        .iter()
                        .find(|(_, f)| f == field)
                        .map(|(k, _)| *k)
                        .expect("every minimum has a key");
                    pro["board"]["design_settings"]["rules"][key] = mm(*value);
                }
                RuleOp::MaskSet { .. } | RuleOp::PasteRatioSet { .. } => unreachable!("refused above"),
            }
        }

        // The result is checked by KiCad before it replaces the file: the board with the new
        // project settings must still be read by `kicad-cli` and checked by DRC.
        let cli = self
            .cli
            .as_ref()
            .ok_or_else(|| ToolError::new(ErrorCode::KicadNotFound, "no kicad-cli to check the project file with"))?;
        let scratch = std::env::temp_dir().join(format!("thingsvu-rules-{}-{}", std::process::id(), self.id));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| kerr(format!("cannot create {}: {e}", scratch.display())))?;
        let body = serde_json::to_string_pretty(&pro).map_err(|e| kerr(e.to_string()))?;
        let result = async {
            let name = self.document_path.file_name().expect("a board file name");
            std::fs::copy(&self.document_path, scratch.join(name)).map_err(|e| kerr(e.to_string()))?;
            std::fs::write(scratch.join(name).with_extension("kicad_pro"), &body).map_err(|e| kerr(e.to_string()))?;
            kicad_cli_runner::pcb::run_drc(
                cli,
                &scratch.join(name),
                &scratch.join("drc.json"),
                false,
                false,
                Duration::from_secs(120),
            )
            .await
            .map_err(|e| {
                kerr(format!(
                    "kicad-cli did not accept the new project settings, nothing was changed: {e}"
                ))
            })
        }
        .await;
        let _ = std::fs::remove_dir_all(&scratch);
        result?;

        let path = self.project_path();
        let backup = path.with_extension("kicad_pro.thingsvu-bak");
        std::fs::copy(&path, &backup).map_err(|e| kerr(format!("cannot back up {}: {e}", path.display())))?;
        let tmp = path.with_extension("kicad_pro.thingsvu-tmp");
        std::fs::write(&tmp, format!("{body}\n")).map_err(|e| kerr(format!("cannot write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path).map_err(|e| kerr(format!("cannot replace {}: {e}", path.display())))?;
        Ok(())
    }

    pub fn is_file_session(&self) -> bool {
        self.provider == Provider::File
    }
}

const CLASS_KEYS: &[(&str, ClassField)] = &[
    ("clearance", ClassField::Clearance),
    ("track_width", ClassField::TrackWidth),
    ("via_diameter", ClassField::ViaDiameter),
    ("via_drill", ClassField::ViaDrill),
    ("diff_pair_width", ClassField::DiffPairWidth),
    ("diff_pair_gap", ClassField::DiffPairGap),
];

const MIN_KEYS: &[(&str, MinField)] = &[
    ("min_clearance", MinField::Clearance),
    ("min_track_width", MinField::TrackWidth),
    ("min_via_diameter", MinField::ViaDiameter),
    ("min_via_annular_width", MinField::ViaAnnular),
    ("min_through_hole_diameter", MinField::ThroughHole),
    ("min_hole_to_hole", MinField::HoleToHole),
    ("min_copper_edge_clearance", MinField::CopperEdge),
    ("min_hole_clearance", MinField::HoleClearance),
    ("min_silk_clearance", MinField::SilkClearance),
];
