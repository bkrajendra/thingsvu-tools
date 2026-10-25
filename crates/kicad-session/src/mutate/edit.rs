// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying a plan that changes an existing drawing: delete old wires, labels, junctions, and
//! net-naming power symbols, move the parts, then draw the new ones (Phase 8, tidy).
//!
//! Two commits, as for a new circuit: the first removes, moves, and places the new power symbols;
//! the second draws wires, labels, and junctions. A headless session can undo a failed tidy by
//! reloading the saved file, so it refuses to start with unsaved changes. An editor session has no
//! such reload: when something fails after a commit it says how many undo steps to take.

use kicad_domain::{ErrorCode, MutationState, Nm, OperationPlan, Orientation, Point, SymbolInfo, ToolError};
use kicad_ipc::proto::kiapi::common::commands::{DeleteItems, DeleteItemsResponse, ItemDeletionStatus, RevertDocument};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, Kiid as ProtoKiid};
use kicad_ipc::proto::kiapi::schematic::types::{SchematicField, SchematicSymbolInstance, SchematicSymbolTransform};
use kicad_ipc::Client;

use super::*;
use crate::session::Provider;

/// Moves one field so that it keeps its place relative to its symbol, turned the way the symbol
/// turns. KiCad's API moves the symbol and leaves its field text where it was. The text angle is
/// stored relative to the symbol (as in the file), so it stays as it is when the symbol turns.
fn carry_field(field: &mut SchematicField, old: (Point, Orientation), new: (Point, Orientation)) -> Option<()> {
    let text = field.text.as_mut()?;
    let at = text.position.as_ref()?;
    let offset = Point::new(Nm(at.x_nm), Nm(at.y_nm)) - old.0;
    let local = old.1.unapply(offset)?;
    let moved = new.0 + new.1.apply(local)?;
    text.position = vector(moved);
    Some(())
}

/// Removes the plan's items and moves its parts (inside an open commit).
async fn remove_and_move(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &OperationPlan,
    instances: &[SchematicSymbolInstance],
) -> Result<(), ToolError> {
    if !plan.removals.is_empty() {
        let r: DeleteItemsResponse = client
            .call(&DeleteItems {
                header: header(),
                item_ids: plan
                    .removals
                    .iter()
                    .map(|r| ProtoKiid { value: r.id.clone() })
                    .collect(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let refused: Vec<String> = r
            .deleted_items
            .iter()
            .filter(|d| d.status != ItemDeletionStatus::IdsOk as i32)
            .map(|d| d.id.as_ref().map(|k| k.value.clone()).unwrap_or_default())
            .collect();
        if !refused.is_empty() || r.deleted_items.len() != plan.removals.len() {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "KiCad deleted {} of {} items; it refused or did not find: {}",
                    r.deleted_items.len() - refused.len(),
                    plan.removals.len(),
                    refused.join(", ")
                ),
            ));
        }
    }
    for m in &plan.moves {
        let mut symbol = instances
            .iter()
            .find(|s| s.id.as_ref().is_some_and(|k| k.value.eq_ignore_ascii_case(&m.id)))
            .cloned()
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::StalePlan,
                    format!("{} is no longer in the schematic; make a new plan", m.reference),
                )
            })?;
        let original = symbol.clone();
        symbol.definition = None;
        symbol.position = vector(m.to);
        symbol.transform = Some(SchematicSymbolTransform {
            orientation: sso(m.orientation).unwrap_or(SchematicSymbolOrientation::Sso0 as i32),
            mirror_x: m.orientation.mirror_x,
            mirror_y: m.orientation.mirror_y,
        });
        let updated = send_symbol_update(client, header, symbol, vec!["position".into(), "transform".into()])
            .await
            .map_err(|mut e| {
                e.message = format!("moving {}: {}", m.reference, e.message);
                e
            })?;
        let info = symbol_info(&updated)?;
        let at = Point::new(
            Nm::from_mm(info.position_mm.x).unwrap_or_default(),
            Nm::from_mm(info.position_mm.y).unwrap_or_default(),
        );

        // Carry the fields along, from where they were before the move.
        let old = (m.from, m.from_orientation);
        let new = (m.to, m.orientation);
        let mut with_fields = updated.clone();
        with_fields.definition = None;
        let mut paths = Vec::new();
        for (name, field) in [
            ("reference_field", &mut with_fields.reference_field),
            ("value_field", &mut with_fields.value_field),
            ("footprint_field", &mut with_fields.footprint_field),
            ("datasheet_field", &mut with_fields.datasheet_field),
            ("description_field", &mut with_fields.description_field),
        ] {
            // The fields as they were before the symbol moved are on `original`.
            let source = match name {
                "reference_field" => &original.reference_field,
                "value_field" => &original.value_field,
                "footprint_field" => &original.footprint_field,
                "datasheet_field" => &original.datasheet_field,
                _ => &original.description_field,
            };
            if let (Some(f), Some(src)) = (field.as_mut(), source.as_ref()) {
                f.text = src.text.clone();
                if carry_field(f, old, new).is_some() {
                    paths.push(name.to_owned());
                }
            }
        }
        if !original.user_fields.is_empty() {
            with_fields.user_fields = original.user_fields.clone();
            for f in &mut with_fields.user_fields {
                carry_field(f, old, new);
            }
            paths.push("user_fields".to_owned());
        }
        if !paths.is_empty() {
            send_symbol_update(client, header, with_fields, paths)
                .await
                .map_err(|mut e| {
                    e.message = format!("moving the text of {}: {}", m.reference, e.message);
                    e
                })?;
        }

        if at != m.to || !info.orientation.same_effect(m.orientation) {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "{} was moved to ({}, {}) mm, {:?} instead of ({}, {}) mm, {:?}",
                    m.reference,
                    info.position_mm.x,
                    info.position_mm.y,
                    info.orientation,
                    m.to.x.to_mm(),
                    m.to.y.to_mm(),
                    m.orientation
                ),
            ));
        }
    }
    Ok(())
}

impl Session {
    /// Changes the value or footprint of one part, as one undo step in an editor.
    pub async fn set_field(
        &self,
        selector: &str,
        field: crate::SymbolField,
        text: &str,
    ) -> Result<SymbolInfo, ToolError> {
        for c in [Capability::SchEditItems, Capability::SchReadSymbols] {
            self.require(c)?;
        }
        let instances = self.symbol_instances().await?;
        let target = crate::select_symbol(&instances, selector, |s| {
            (
                s.id.as_ref().map(|k| k.value.as_str()).unwrap_or_default(),
                s.reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .map(|t| t.text.as_str())
                    .unwrap_or_default(),
            )
        })?
        .clone();
        let current = symbol_info(&target)?;
        let (value, footprint) = match field {
            crate::SymbolField::Value => (text.to_owned(), None),
            crate::SymbolField::Footprint => (current.value.clone(), Some(text)),
        };
        let use_commit = self.capabilities().is_available(Capability::Commits);
        let header = || {
            Some(ItemHeader {
                document: Some(self.document.clone()),
                ..Default::default()
            })
        };
        let mut inner = self.inner.lock().await;
        let client = &mut inner.client;
        let commit = begin_commit(client, use_commit, &header).await?;
        let updated = match update_fields(client, &header, target, &value, footprint, None).await {
            Ok(updated) => updated,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(err.with_mutation_state(MutationState::NotApplied));
            }
        };
        let info = symbol_info(&updated)?;
        let stored = match field {
            crate::SymbolField::Value => &info.value,
            crate::SymbolField::Footprint => &info.footprint,
        };
        if stored != text {
            let _ = end_commit(client, commit, false, "", &header).await;
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!("KiCad stored '{stored}' instead of '{text}'; the change was dropped"),
            )
            .with_mutation_state(MutationState::NotApplied));
        }
        end_commit(client, commit, true, "AI: set field", &header).await?;
        Ok(info)
    }

    /// Applies a tidy plan: see the module documentation.
    pub(super) async fn apply_edit(&self, plan: &OperationPlan) -> Result<ApplyReport, ToolError> {
        let use_commit = self.capabilities().is_available(Capability::Commits);
        let headless = self.provider() == Provider::Headless;
        if headless {
            let mut inner = self.inner.lock().await;
            if Self::unsaved_changes(&mut inner, &self.document).await != Some(false) {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    "the document has unsaved changes; save it first (sch_save) so that a failed tidy can be reverted",
                ));
            }
        }
        let instances = self.symbol_instances().await?;
        let header = || {
            Some(ItemHeader {
                document: Some(self.document.clone()),
                ..Default::default()
            })
        };

        let mut inner = self.inner.lock().await;
        let client = &mut inner.client;
        let mut created = Created::default();
        let mut commits = 0u32;

        // Step 1: remove, move, and place the new power symbols.
        let commit = begin_commit(client, use_commit, &header).await?;
        let step1 = async {
            remove_and_move(client, &header, plan, &instances).await?;
            place_symbols(client, &header, plan, &mut created).await
        }
        .await;
        let placed = match step1 {
            Ok(placed) => placed,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(self
                    .after_failed_edit(client, &header, headless, use_commit, commits, err)
                    .await);
            }
        };
        if let Err(err) = end_commit(
            client,
            commit,
            true,
            &format!("AI: tidy (move) {}", plan.circuit),
            &header,
        )
        .await
        {
            return Err(self
                .after_failed_edit(client, &header, headless, use_commit, commits, err)
                .await);
        }
        commits += 1;

        // Step 2: draw.
        let commit = match begin_commit(client, use_commit, &header).await {
            Ok(c) => c,
            Err(err) => {
                return Err(self
                    .after_failed_edit(client, &header, headless, use_commit, commits, err)
                    .await)
            }
        };
        let counts = match finish(client, &header, plan, &placed, &mut created).await {
            Ok(counts) => counts,
            Err(err) => {
                let _ = end_commit(client, commit, false, "", &header).await;
                return Err(self
                    .after_failed_edit(client, &header, headless, use_commit, commits, err)
                    .await);
            }
        };
        if let Err(err) = end_commit(
            client,
            commit,
            true,
            &format!("AI: tidy (draw) {}", plan.circuit),
            &header,
        )
        .await
        {
            return Err(self
                .after_failed_edit(client, &header, headless, use_commit, commits, err)
                .await);
        }
        commits += 1;
        drop(inner);

        let verification = self.verify(plan).await?;
        let verified = verification.iter().all(|c| c.ok);
        if !verified && headless {
            let mut inner = self.inner.lock().await;
            let err = ToolError::new(
                ErrorCode::KicadError,
                "the tidied schematic's connectivity differs from the plan".to_owned(),
            );
            return Err(self
                .after_failed_edit(&mut inner.client, &header, headless, use_commit, commits, err)
                .await);
        }
        Ok(ApplyReport {
            placed: vec![],
            wires_created: counts.0,
            labels_created: counts.1,
            undo_steps: if use_commit { commits } else { 0 },
            verification,
            verified,
            backup_path: None,
            moved: plan.moves.len(),
            removed: plan.removals.len(),
        })
    }

    /// What to tell the caller when a tidy fails after changes were made. A headless session
    /// reloads the saved file; an editor session cannot, so the user must undo.
    async fn after_failed_edit(
        &self,
        client: &mut Client,
        _header: &impl Fn() -> Option<ItemHeader>,
        headless: bool,
        use_commit: bool,
        commits: u32,
        err: ToolError,
    ) -> ToolError {
        if commits == 0 {
            // The open commit was dropped, which undoes everything.
            return ToolError::new(err.code, format!("{} (nothing was changed)", err.message))
                .with_mutation_state(MutationState::NotApplied);
        }
        if headless {
            let reverted = client
                .call_any(&RevertDocument {
                    document: Some(self.document.clone()),
                })
                .await
                .is_ok();
            return if reverted {
                ToolError::new(
                    err.code,
                    format!("{} (the document was reverted to its saved state)", err.message),
                )
                .with_mutation_state(MutationState::NotApplied)
            } else {
                ToolError::new(
                    err.code,
                    format!("{} (reverting failed; read the schematic back)", err.message),
                )
                .with_mutation_state(MutationState::Unknown)
            };
        }
        let how = if use_commit {
            format!("press Ctrl+Z {commits} time(s) in the editor to undo what was done")
        } else {
            "read the schematic back before doing anything else".to_owned()
        };
        ToolError::new(err.code, format!("{} (partly applied: {how})", err.message))
            .with_mutation_state(MutationState::Applied)
    }
}
