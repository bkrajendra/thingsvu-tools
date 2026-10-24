// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying operation plans, saving, and export jobs.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kicad_domain::geometry::Orientation;
use kicad_domain::{
    Capability, ErrorCode, ExistingDocument, LibraryId, LibraryPin, LibrarySymbol, MutationState, Nm, Operation,
    OperationPlan, Point, ToolError,
};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, EndCommit,
    GetItemsFromLibrary, GetItemsResponse, ItemStatusCode, PlaceFromLibraryResponse, SaveCopyOfDocument, SaveDocument,
    SaveOptions, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    ItemHeader, Kiid as ProtoKiid, LibraryIdentifier, LibraryType, Text, Vector2,
};
use kicad_ipc::proto::kiapi::schematic::commands::PlaceSymbolFromLibrary;
use kicad_ipc::proto::kiapi::schematic::types::{
    LocalLabel, SchematicLine, SchematicLineType, SchematicPin, SchematicSymbol, SchematicSymbolInstance,
    SchematicSymbolOrientation,
};
use kicad_ipc::Client;
use prost::Name;
use prost_types::{Any, FieldMask};
use serde::Serialize;

use crate::errors::from_ipc_error;
use crate::schematic::symbol_info;
use crate::session::Session;

/// How long a headless server may still be loading libraries before "not found" is final, when
/// looking up symbols for a plan.
const LIBRARY_LOAD_GRACE: Duration = Duration::from_secs(90);
/// At apply time the plan's symbols were already found in the library, so "not found" is final
/// almost at once.
const PLACE_GRACE: Duration = Duration::from_secs(5);

fn vector(p: Point) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: p.x.0,
        y_nm: p.y.0,
    })
}

/// The placement orientation, or `None` for 0° so KiCad uses its default.
fn sso(orientation: Orientation) -> Option<i32> {
    let o = match orientation.rotation_deg {
        90 => SchematicSymbolOrientation::Sso90,
        180 => SchematicSymbolOrientation::Sso180,
        270 => SchematicSymbolOrientation::Sso270,
        _ => return None,
    };
    Some(o as i32)
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacedComponent {
    pub component_id: String,
    pub reference: String,
    pub symbol_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetCheck {
    pub net: String,
    pub expected: BTreeSet<String>,
    /// Pins of the KiCad net that contains the first expected pin.
    pub actual: BTreeSet<String>,
    pub kicad_net_name: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub placed: Vec<PlacedComponent>,
    pub wires_created: usize,
    pub labels_created: usize,
    /// KiCad undo steps this apply produced (0 when the session has no commits).
    pub undo_steps: u32,
    pub verification: Vec<NetCheck>,
    pub verified: bool,
}

/// Items created so far, for cleanup when an apply fails part way.
#[derive(Default)]
struct Created {
    /// Created in the current step; dropped with its commit if the step fails.
    pending: Vec<String>,
    /// Created by steps that completed; must be deleted explicitly on a later failure.
    committed: Vec<String>,
}

impl Session {
    /// Pin geometry of library symbols, before placement. Missing entries are left out.
    pub async fn library_symbols(&self, ids: &[LibraryId]) -> Result<BTreeMap<LibraryId, LibrarySymbol>, ToolError> {
        let item_ids: Vec<LibraryIdentifier> = ids
            .iter()
            .map(|id| LibraryIdentifier {
                library_nickname: id.library().into(),
                entry_name: id.entry().into(),
            })
            .collect();
        let started = Instant::now();
        let response: GetItemsResponse = loop {
            let result = {
                let mut inner = self.inner.lock().await;
                inner
                    .client
                    .call::<_, GetItemsResponse>(&GetItemsFromLibrary {
                        r#type: LibraryType::LtSymbol as i32,
                        document: Some(self.document.clone()),
                        item_ids: item_ids.clone(),
                    })
                    .await
            };
            match result {
                // Libraries may still be loading in a headless server.
                Ok(r) if r.items.len() < ids.len() && started.elapsed() < LIBRARY_LOAD_GRACE && r.items.is_empty() => {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Ok(r) => break r,
                Err(kicad_ipc::Error::Status { ref message, .. })
                    if message.contains("not found") && started.elapsed() < LIBRARY_LOAD_GRACE =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Err(e) => return Err(from_ipc_error(&e, false)),
            }
        };
        let mut out = BTreeMap::new();
        for any in &response.items {
            let Ok(sym) = any.to_msg::<SchematicSymbol>() else {
                continue;
            };
            let Some(id) = &sym.id else { continue };
            let Ok(library_id) = LibraryId::parse(&format!("{}:{}", id.library_nickname, id.entry_name)) else {
                continue;
            };
            let mut pins = Vec::new();
            for child in &sym.items {
                let Some(item) = &child.item else { continue };
                if item.type_url != SchematicPin::type_url() {
                    continue;
                }
                let Ok(pin) = item.to_msg::<SchematicPin>() else {
                    continue;
                };
                let p = pin.position.unwrap_or_default();
                pins.push(LibraryPin {
                    number: pin.number,
                    name: pin.name,
                    local: Point::new(Nm(p.x_nm), Nm(p.y_nm)),
                    unit: child.unit.map(|u| u.unit.max(0) as u32).unwrap_or(0),
                });
            }
            let prefix = sym
                .reference_field
                .as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.trim_end_matches(['?', '*']).to_owned())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "U".into());
            out.insert(
                library_id.clone(),
                LibrarySymbol {
                    library_id,
                    reference_prefix: prefix,
                    unit_count: sym.unit_count.max(1),
                    pins,
                },
            );
        }
        Ok(out)
    }

    /// References and extent of what is already in the document.
    pub async fn existing_document(&self) -> Result<ExistingDocument, ToolError> {
        let symbols = self.list_symbols().await?;
        Ok(ExistingDocument {
            references: symbols.iter().map(|s| s.reference.clone()).collect(),
            max_y: symbols
                .iter()
                .map(|s| Nm::from_mm(s.position_mm.y).unwrap_or_default())
                .max(),
        })
    }

    /// A fingerprint of the document's symbols and connectivity. A plan made against one
    /// fingerprint is stale once it changes.
    pub async fn fingerprint(&self) -> Result<String, ToolError> {
        let symbols = self.list_symbols().await?;
        let nets = if self.capabilities().is_available(Capability::SchNetlist) {
            self.netlist().await?
        } else {
            vec![]
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for s in &symbols {
            (
                s.id.as_str(),
                &s.reference,
                &s.value,
                &s.library_id,
                s.position_mm.x.to_bits(),
                s.position_mm.y.to_bits(),
            )
                .hash(&mut h);
            (
                s.orientation.rotation_deg,
                s.orientation.mirror_x,
                s.orientation.mirror_y,
            )
                .hash(&mut h);
        }
        for n in &nets {
            n.name.hash(&mut h);
            for p in &n.pins {
                (&p.reference, &p.pin).hash(&mut h);
            }
            n.other_items.hash(&mut h);
        }
        Ok(format!("{:016x}", h.finish()))
    }

    /// Applies `plan` if the document still has `expected_fingerprint`. Grouped into one commit
    /// where KiCad supports commits; otherwise created items are deleted again if a step fails.
    /// Connectivity is read back and compared with the plan's expected nets.
    pub async fn apply_plan(&self, plan: &OperationPlan, expected_fingerprint: &str) -> Result<ApplyReport, ToolError> {
        for c in [Capability::SchPlaceSymbol, Capability::SchEditItems] {
            if !self.capabilities().is_available(c) {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    format!("this session cannot do {c:?}"),
                ));
            }
        }
        if kicad_domain::validation::has_errors(&plan.findings) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "the plan has errors and cannot be applied",
            ));
        }
        let current = self.fingerprint().await?;
        if current != expected_fingerprint {
            return Err(ToolError::new(
                ErrorCode::StalePlan,
                "the schematic changed since the plan was made; make a new plan",
            ));
        }

        let use_commit = self.capabilities().is_available(Capability::Commits);
        let header = || {
            Some(ItemHeader {
                document: Some(self.document.clone()),
                ..Default::default()
            })
        };
        let mut inner = self.inner.lock().await;
        let client = &mut inner.client;
        let mut created = Created::default();

        // Step 1 places the symbols. Inside an open commit a newly placed symbol is not yet part
        // of the document, so field updates must wait for the next commit (docs/tasks.md, 3.4).
        let commit = begin_commit(client, use_commit, &header).await?;
        let placed = match place_symbols(client, &header, plan, &mut created).await {
            Ok(placed) => placed,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(undo(client, &header, &created, use_commit, err).await);
            }
        };
        if let Err(err) = end_commit(client, commit, true, &format!("AI: place {}", plan.circuit), &header).await {
            return Err(undo(client, &header, &created, use_commit, err).await);
        }
        created.committed.append(&mut created.pending);

        // Step 2 sets field values and adds wires and labels.
        let commit = begin_commit(client, use_commit, &header)
            .await
            .map_err(|e| e.with_mutation_state(MutationState::Applied))?;
        let (wires, labels) = match finish(client, &header, plan, &placed, &mut created).await {
            Ok(counts) => counts,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(undo(client, &header, &created, use_commit, err).await);
            }
        };
        if let Err(err) = end_commit(client, commit, true, &format!("AI: connect {}", plan.circuit), &header).await {
            return Err(undo(client, &header, &created, use_commit, err).await);
        }
        drop(inner);

        let verification = self.verify(plan).await?;
        let verified = verification.iter().all(|c| c.ok);
        Ok(ApplyReport {
            placed: placed.into_iter().map(|(p, _)| p).collect(),
            wires_created: wires,
            labels_created: labels,
            undo_steps: if use_commit { 2 } else { 0 },
            verification,
            verified,
        })
    }

    async fn verify(&self, plan: &OperationPlan) -> Result<Vec<NetCheck>, ToolError> {
        let nets = self.netlist().await?;
        let mut checks = Vec::new();
        for expected in &plan.expected_nets {
            let first = expected.pins.iter().next().cloned().unwrap_or_default();
            let found = nets
                .iter()
                .find(|n| n.pins.iter().any(|p| format!("{}.{}", p.reference, p.pin) == first));
            let actual: BTreeSet<String> = found
                .map(|n| n.pins.iter().map(|p| format!("{}.{}", p.reference, p.pin)).collect())
                .unwrap_or_default();
            let name_ok = found.is_some_and(|n| n.name.trim_start_matches('/') == expected.name);
            checks.push(NetCheck {
                net: expected.name.clone(),
                ok: actual == expected.pins && name_ok,
                expected: expected.pins.clone(),
                actual,
                kicad_net_name: found.map(|n| n.name.clone()),
            });
        }
        Ok(checks)
    }

    /// Saves the document to its own path.
    pub async fn save(&self) -> Result<(), ToolError> {
        if !self.capabilities().is_available(Capability::SchSave) {
            return Err(ToolError::new(
                ErrorCode::FeatureUnavailable,
                "this session cannot save",
            ));
        }
        let mut inner = self.inner.lock().await;
        inner
            .client
            .call_any(&SaveDocument {
                document: Some(self.document.clone()),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        Ok(())
    }

    /// Writes the current in-memory document (with its project) into `dir`, which must exist,
    /// and returns the schematic path there. The session's own file is not touched.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        let name = self
            .document_path()
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "copy.kicad_sch".into());
        let target = dir.join(name);
        let mut inner = self.inner.lock().await;
        inner
            .client
            .call_any(&SaveCopyOfDocument {
                document: Some(self.document.clone()),
                path: target.display().to_string(),
                options: Some(SaveOptions {
                    overwrite: true,
                    include_project: true,
                }),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        Ok(target)
    }
}

async fn place(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    library_id: &LibraryId,
    reference: &str,
    position: Point,
    orientation: Orientation,
) -> Result<SchematicSymbolInstance, ToolError> {
    let request = PlaceSymbolFromLibrary {
        header: header(),
        lib_id: Some(LibraryIdentifier {
            library_nickname: library_id.library().into(),
            entry_name: library_id.entry().into(),
        }),
        position: vector(position),
        orientation: sso(orientation),
        reference: Some(reference.into()),
        ..Default::default()
    };
    let started = Instant::now();
    loop {
        match client.call::<_, PlaceFromLibraryResponse>(&request).await {
            Ok(r) => {
                return r
                    .item
                    .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "placement returned no item"))?
                    .to_msg::<SchematicSymbolInstance>()
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("undecodable placed symbol: {e}")))
            }
            Err(kicad_ipc::Error::Status { message, .. })
                if message.contains("not found") && started.elapsed() < PLACE_GRACE =>
            {
                tokio::time::sleep(Duration::from_millis(500)).await
            }
            Err(e) => {
                let mut err = from_ipc_error(&e, true);
                if err.code == ErrorCode::InvalidArgument && e.to_string().contains("not found") {
                    err.code = ErrorCode::SymbolNotFound;
                }
                return Err(err);
            }
        }
    }
}

/// Updates only the named fields: the embedded library definition is left out and a field mask
/// is sent, which avoids duplicated library symbols (docs/tasks.md, 2.6).
async fn update_fields(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    mut symbol: SchematicSymbolInstance,
    value: &str,
    footprint: Option<&str>,
) -> Result<SchematicSymbolInstance, ToolError> {
    let mut paths = vec!["value_field".to_owned()];
    symbol.definition = None;
    if let Some(t) = symbol.value_field.as_mut().and_then(|f| f.text.as_mut()) {
        t.text = value.into();
    }
    if let Some(fp) = footprint {
        if let Some(t) = symbol.footprint_field.as_mut().and_then(|f| f.text.as_mut()) {
            t.text = fp.into();
            paths.push("footprint_field".into());
        }
    }
    let mut h = header().unwrap_or_default();
    h.field_mask = Some(FieldMask { paths });
    let any = Any::from_msg(&symbol).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
    let r: UpdateItemsResponse = client
        .call(&UpdateItems {
            header: Some(h),
            items: vec![any],
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    let result = r
        .updated_items
        .into_iter()
        .next()
        .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "no update result"))?;
    let code = result.status.as_ref().map(|s| s.code).unwrap_or_default();
    if code != ItemStatusCode::IscOk as i32 {
        let message = result.status.map(|s| s.error_message).unwrap_or_default();
        return Err(ToolError::new(
            ErrorCode::KicadError,
            format!("KiCad rejected the field update: {message}"),
        ));
    }
    result
        .item
        .ok_or_else(|| ToolError::new(ErrorCode::KicadError, "update returned no item"))?
        .to_msg::<SchematicSymbolInstance>()
        .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
}

/// Opens a commit when the session supports commits.
async fn begin_commit(
    client: &mut Client,
    use_commit: bool,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<Option<ProtoKiid>, ToolError> {
    if !use_commit {
        return Ok(None);
    }
    let r: BeginCommitResponse = client
        .call(&BeginCommit { header: header() })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    Ok(r.id)
}

/// Commits (`keep`) or drops an open commit; does nothing without one.
async fn end_commit(
    client: &mut Client,
    commit: Option<ProtoKiid>,
    keep: bool,
    message: &str,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<(), ToolError> {
    if commit.is_none() {
        return Ok(());
    }
    let action = if keep {
        CommitAction::CmaCommit
    } else {
        CommitAction::CmaDrop
    };
    client
        .call_any(&EndCommit {
            id: commit,
            action: action as i32,
            message: message.to_owned(),
            header: header(),
        })
        .await
        .map(|_| ())
        .map_err(|e| from_ipc_error(&e, true))
}

/// After a failed step: removes everything earlier steps created and reports the original error
/// with the resulting mutation state. A failed step's own commit has already been dropped.
async fn undo(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    created: &Created,
    use_commit: bool,
    err: ToolError,
) -> ToolError {
    // Without commits nothing was rolled back automatically, so the current step's items go too.
    let mut to_delete = created.committed.clone();
    if !use_commit {
        to_delete.extend(created.pending.iter().cloned());
    }
    let undone = if to_delete.is_empty() {
        true
    } else {
        let ids: Vec<ProtoKiid> = to_delete.iter().map(|id| ProtoKiid { value: id.clone() }).collect();
        // Delete inside its own commit where available, so the cleanup is one undo step too.
        match begin_commit(client, use_commit, header).await {
            Ok(commit) => {
                let deleted = client
                    .call_any(&DeleteItems {
                        header: header(),
                        item_ids: ids,
                    })
                    .await
                    .is_ok();
                let closed = end_commit(client, commit, deleted, "AI: undo failed apply", header)
                    .await
                    .is_ok();
                deleted && closed
            }
            Err(_) => false,
        }
    };
    let (state, note) = if undone {
        (MutationState::NotApplied, "all changes were undone")
    } else {
        (MutationState::Unknown, "cleanup failed; read the schematic back")
    };
    ToolError::new(err.code, format!("{} ({note})", err.message)).with_mutation_state(state)
}

/// Step 1: place every symbol. Returns each placement with the instance KiCad created.
async fn place_symbols(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &OperationPlan,
    created: &mut Created,
) -> Result<Vec<(PlacedComponent, SchematicSymbolInstance)>, ToolError> {
    let mut placed = Vec::new();
    for op in &plan.operations {
        if let Operation::PlaceSymbol {
            component_id,
            library_id,
            reference,
            position,
            orientation,
            ..
        } = op
        {
            let symbol = place(client, header, library_id, reference, *position, *orientation).await?;
            let id = symbol.id.as_ref().map(|k| k.value.clone()).unwrap_or_default();
            created.pending.push(id.clone());
            let info = symbol_info(&symbol)?;
            let at = Point::new(
                Nm::from_mm(info.position_mm.x).unwrap_or_default(),
                Nm::from_mm(info.position_mm.y).unwrap_or_default(),
            );
            if info.reference != *reference || at != *position {
                return Err(ToolError::new(
                    ErrorCode::KicadError,
                    format!(
                        "{reference} was placed as {} at ({}, {}) mm",
                        info.reference, info.position_mm.x, info.position_mm.y
                    ),
                ));
            }
            placed.push((
                PlacedComponent {
                    component_id: component_id.to_string(),
                    reference: reference.clone(),
                    symbol_id: id,
                },
                symbol,
            ));
        }
    }

    Ok(placed)
}

/// Step 2: set field values, then add wires and labels.
async fn finish(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &OperationPlan,
    placed: &[(PlacedComponent, SchematicSymbolInstance)],
    created: &mut Created,
) -> Result<(usize, usize), ToolError> {
    for op in &plan.operations {
        let Operation::PlaceSymbol {
            component_id,
            reference,
            value,
            footprint,
            ..
        } = op
        else {
            continue;
        };
        let (p, symbol) = placed
            .iter()
            .find(|(p, _)| p.component_id == component_id.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::KicadError, format!("{reference} was not placed")))?;
        let updated = update_fields(client, header, symbol.clone(), value, footprint.as_deref())
            .await
            .map_err(|mut e| {
                e.message = format!("{reference} ({}): {}", p.symbol_id, e.message);
                e
            })?;
        let info = symbol_info(&updated)?;
        if info.value != *value {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!("{reference} value read back as '{}' instead of '{value}'", info.value),
            ));
        }
    }

    let mut items = Vec::new();
    let (mut wires, mut labels) = (0, 0);
    for op in &plan.operations {
        match op {
            Operation::AddWire { start, end, .. } => {
                wires += 1;
                items.push(
                    Any::from_msg(&SchematicLine {
                        start: vector(*start),
                        end: vector(*end),
                        r#type: SchematicLineType::SltWire as i32,
                        ..Default::default()
                    })
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?,
                );
            }
            Operation::AddLabel { net, position } => {
                labels += 1;
                items.push(
                    Any::from_msg(&LocalLabel {
                        position: vector(*position),
                        text: Some(Text {
                            position: vector(*position),
                            text: net.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?,
                );
            }
            Operation::PlaceSymbol { .. } => {}
        }
    }
    if items.is_empty() {
        return Ok((0, 0));
    }
    let expected = items.len();
    let r: CreateItemsResponse = client
        .call(&CreateItems {
            header: header(),
            items,
            ..Default::default()
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    let ok = r
        .created_items
        .iter()
        .filter(|c| c.status.as_ref().map(|s| s.code) == Some(ItemStatusCode::IscOk as i32))
        .count();
    if ok != expected {
        // Without a commit, the items that were created must be removed by the undo step.
        created.committed.extend(
            r.created_items
                .iter()
                .filter_map(|c| c.item.as_ref().and_then(item_kiid)),
        );
        return Err(ToolError::new(
            ErrorCode::KicadError,
            format!("KiCad created {ok} of {expected} wires and labels"),
        ));
    }
    Ok((wires, labels))
}

/// The KIID of a created wire or label.
fn item_kiid(any: &Any) -> Option<String> {
    if any.type_url == SchematicLine::type_url() {
        any.to_msg::<SchematicLine>().ok()?.id.map(|k| k.value)
    } else if any.type_url == LocalLabel::type_url() {
        any.to_msg::<LocalLabel>().ok()?.id.map(|k| k.value)
    } else {
        None
    }
}
