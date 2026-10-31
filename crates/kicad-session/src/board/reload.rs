// SPDX-License-Identifier: GPL-3.0-or-later
//! Reloading a board from its file (Phase 14.5): the open board is dropped and the file's is read
//! again. A session that holds changes does not do it without being told, and says what it would
//! lose.

use kicad_domain::changes::describe_board_changes;
use kicad_domain::{ErrorCode, ToolError};
use kicad_ipc::proto::kiapi::common::commands::{
    GetDocumentModifiedState, GetDocumentModifiedStateResponse, RevertDocument,
};

use super::BoardSession;
use crate::errors::from_ipc_error;
use crate::freshness::ReloadReport;

impl BoardSession {
    /// Reads the board from its file again. A session with unsaved changes refuses unless
    /// `discard_unsaved`; the refusal lists what would be lost.
    pub async fn reload(&self, discard_unsaved: bool) -> Result<ReloadReport, ToolError> {
        let changed = self.freshness.changed(&self.document_path);
        let Some(ipc) = &self.ipc else {
            // A file session reads the file every time; there is nothing held to drop.
            let counts = self.read().await?.counts();
            return Ok(ReloadReport {
                reloaded: true,
                provider: self.provider,
                had_unsaved_changes: Some(false),
                file_changed_on_disk: changed,
                dropped: vec![],
                counts_after: counts,
                note: "a file session reads the file each time; nothing was held".into(),
            });
        };
        // What the open board has that the file does not.
        let live = self.read().await?;
        let on_disk = std::fs::read_to_string(&self.document_path)
            .ok()
            .and_then(|t| kicad_sexpr::pcb::read_board(&t).ok());
        let dropped = on_disk
            .as_ref()
            .map(|disk| describe_board_changes(disk, &live))
            .unwrap_or_default();

        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let state: Result<GetDocumentModifiedStateResponse, _> = ipc
            .client
            .call(&GetDocumentModifiedState {
                document: Some(document.clone()),
            })
            .await;
        let unsaved = state.ok().map(|r| r.state == 2);
        if unsaved != Some(false) && !discard_unsaved {
            let what = if dropped.is_empty() {
                "the editor reports unsaved changes".to_owned()
            } else {
                dropped.join("; ")
            };
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "reloading would drop the board's unsaved changes ({what}); save first (pcb_save), or pass discard_unsaved: true"
                ),
            ));
        }
        ipc.client
            .call_any(&RevertDocument {
                document: Some(document.clone()),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let after: GetDocumentModifiedStateResponse = ipc
            .client
            .call(&GetDocumentModifiedState {
                document: Some(document),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        drop(ipc);
        if after.state == 2 {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                "KiCad still reports unsaved changes after the reload",
            ));
        }
        self.freshness.reset(&self.document_path);
        let counts = self.read().await?.counts();
        Ok(ReloadReport {
            reloaded: true,
            provider: self.provider,
            had_unsaved_changes: unsaved,
            file_changed_on_disk: changed,
            dropped: if unsaved == Some(true) { dropped } else { vec![] },
            counts_after: counts,
            note: "the board is the file's now; plans made before the reload are stale".into(),
        })
    }
}
