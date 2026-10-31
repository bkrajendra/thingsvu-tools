// SPDX-License-Identifier: GPL-3.0-or-later
//! The checkpoint tools (Phase 14.6): `checkpoint_create`, `checkpoint_list`, `checkpoint_diff`, and
//! `checkpoint_restore`. A checkpoint keeps the project's `.kicad_pcb`, `.kicad_sch`, and
//! `.kicad_pro`; a live session's unsaved changes are written into it, so it holds what the open
//! board and schematic look like now.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kicad_domain::{ErrorCode, ToolError};
use kicad_session::checkpoint::{self, Manifest, Project, Snapshot};
use kicad_session::{AnySession, BoardSession, Provider};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{envelope, KicadAiServer};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointTarget {
    /// A board or schematic session of the project. Or give project_path.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The absolute path of the project's .kicad_pcb, .kicad_sch, or .kicad_pro file (no session
    /// needed; the files on disk are used).
    #[serde(default)]
    pub project_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCreateArgs {
    #[serde(flatten)]
    pub target: CheckpointTarget,
    /// What the checkpoint is for, e.g. "before the rip-up".
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointListArgs {
    #[serde(flatten)]
    pub target: CheckpointTarget,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDiffArgs {
    #[serde(flatten)]
    pub target: CheckpointTarget,
    /// The checkpoint id (from checkpoint_list), or `latest`.
    pub checkpoint: String,
    /// What to compare it with: another checkpoint id, or `current` (the default): the project as it
    /// is now, a live session's unsaved changes included.
    #[serde(default)]
    pub against: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRestoreArgs {
    #[serde(flatten)]
    pub target: CheckpointTarget,
    /// The checkpoint id (from checkpoint_list), or `latest`.
    pub checkpoint: String,
    /// Must be true: the user has seen checkpoint_diff and wants the project put back.
    pub confirm: bool,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.display().to_string().to_lowercase();
    norm(a) == norm(b)
}

/// The sessions of this server that work on `project`.
struct ProjectSessions {
    boards: Vec<Arc<BoardSession>>,
    schematics: Vec<Arc<AnySession>>,
}

fn belongs(project: &Project, document: &Path) -> bool {
    Project::of(document).is_ok_and(|p| same_file(&p.dir, &project.dir) && p.stem.eq_ignore_ascii_case(&project.stem))
}

impl KicadAiServer {
    async fn project_sessions(&self, project: &Project) -> ProjectSessions {
        let boards = self
            .state
            .boards
            .lock()
            .await
            .values()
            .filter(|b| belongs(project, b.document_path()))
            .cloned()
            .collect();
        let schematics = self
            .state
            .sessions
            .lock()
            .await
            .values()
            .filter(|s| belongs(project, s.document_path()))
            .cloned()
            .collect();
        ProjectSessions { boards, schematics }
    }

    /// The project a request is about: a session's, or the one a file path names.
    async fn checkpoint_project(&self, t: &CheckpointTarget) -> Result<Project, ToolError> {
        match (&t.session_id, &t.project_path) {
            (Some(id), None) => {
                if let Some(b) = self.state.boards.lock().await.get(id) {
                    return Project::of(b.document_path());
                }
                if let Some(s) = self.state.sessions.lock().await.get(id) {
                    return Project::of(s.document_path());
                }
                Err(ToolError::new(
                    ErrorCode::NoSession,
                    format!("no session '{id}'; session_status lists them"),
                ))
            }
            (None, Some(path)) => {
                let p = PathBuf::from(path);
                if !p.is_absolute() {
                    return Err(bad("project_path must be an absolute path"));
                }
                Project::of(&p)
            }
            _ => Err(bad("give session_id or project_path, one of them")),
        }
    }

    /// Makes a checkpoint, with the open documents of live sessions written into it.
    async fn make_checkpoint(&self, project: &Project, label: &str) -> Result<Manifest, ToolError> {
        let sessions = self.project_sessions(project).await;
        let scratch = std::env::temp_dir().join(format!("thingsvu-checkpoint-{}-{}", std::process::id(), project.stem));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| {
            ToolError::new(
                ErrorCode::KicadError,
                format!("cannot create {}: {e}", scratch.display()),
            )
        })?;
        let result = async {
            let mut sources: Vec<(&str, PathBuf)> = Vec::new();
            let mut unsaved = false;
            let mut version = None;
            for b in sessions.boards.iter().filter(|b| b.is_live()) {
                let status = b.status().await;
                unsaved |= status.unsaved_changes == Some(true);
                version = Some(b.kicad_version().to_owned());
                let dir = scratch.join("board");
                std::fs::create_dir_all(&dir).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
                let copy = b.save_copy(&dir).await?;
                sources.push(("kicad_pcb", copy.clone()));
                if copy.with_extension("kicad_pro").is_file() {
                    sources.push(("kicad_pro", copy.with_extension("kicad_pro")));
                }
            }
            for s in sessions.schematics.iter().filter(|s| s.provider() != Provider::File) {
                let status = s.status().await;
                unsaved |= status.unsaved_changes == Some(true);
                let dir = scratch.join("schematic");
                std::fs::create_dir_all(&dir).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
                let copy = s.save_copy(&dir).await?;
                sources.push(("kicad_sch", copy));
            }
            checkpoint::create(project, label, &sources, unsaved, version)
        }
        .await;
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }

    /// Makes a checkpoint for `apply` with a label, and returns its record for the result.
    pub(super) async fn checkpoint_before_apply(
        &self,
        board: &BoardSession,
        label: &str,
    ) -> Result<serde_json::Value, ToolError> {
        let project = Project::of(board.document_path())?;
        let m = self.make_checkpoint(&project, label).await?;
        Ok(
            serde_json::json!({ "id": m.id, "label": m.label, "created": m.created, "unsaved_included": m.unsaved_included }),
        )
    }
}

fn manifest_json(m: &Manifest) -> serde_json::Value {
    serde_json::json!({
        "id": m.id, "label": m.label, "created": m.created, "unsaved_included": m.unsaved_included,
        "kicad_version": m.kicad_version,
        "files": m.files.iter().map(|f| serde_json::json!({ "name": f.name, "bytes": f.size })).collect::<Vec<_>>(),
    })
}

#[tool_router(router = checkpoint_router, vis = "pub(super)")]
impl KicadAiServer {
    #[tool(
        name = "checkpoint_create",
        description = "Keep a copy of the project's board, schematic, and project file (.kicad_pcb, .kicad_sch, .kicad_pro) in \
.thingsvu/checkpoints/ next to them, to come back to. A live session's unsaved changes are written into it, so it \
holds what the open board looks like now. Give a session_id or a project_path, and a label (\"before the \
rip-up\"). Only the project's root files are kept (no sub-sheets or libraries). Take one before anything that \
deletes: pcb_track_edit_plan, an autoroute, a restore. pcb_apply_plan can take one itself (checkpoint: label)."
    )]
    pub async fn checkpoint_create(&self, Parameters(args): Parameters<CheckpointCreateArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let result = async {
            let project = self.checkpoint_project(&args.target).await?;
            let label = args.label.clone().unwrap_or_else(|| "checkpoint".to_owned());
            let m = self.make_checkpoint(&project, &label).await?;
            Ok::<_, ToolError>((project, m))
        }
        .await;
        match result {
            Ok((project, m)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "checkpoint": manifest_json(&m),
                    "folder": project.store().join(&m.id).display().to_string(),
                    "note": if m.unsaved_included { "the open board or schematic had unsaved changes; they are in the checkpoint, not in the files on disk" } else { "the files on disk were kept" },
                }),
                vec![],
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "checkpoint_list",
        description = "The project's checkpoints, newest first: id, label, when, which files and how big, and whether unsaved changes were in it. Read-only."
    )]
    pub async fn checkpoint_list(&self, Parameters(args): Parameters<CheckpointListArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.checkpoint_project(&args.target).await {
            Ok(project) => {
                let all = checkpoint::list(&project);
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "project": project.stem, "folder": project.dir.display().to_string(),
                        "count": all.len(), "checkpoints": all.iter().map(manifest_json).collect::<Vec<_>>(),
                    }),
                    vec![],
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "checkpoint_diff",
        description = "What differs between a checkpoint and another checkpoint, or the project as it is now (against: current, the \
default; a live session's unsaved changes are included): parts added, removed, or moved; nets and the tracks \
on them; tracks, vias, zones, and drawings; schematic parts, wires, and labels; and the design rules (net \
classes, assignments, minimums). Read-only."
    )]
    pub async fn checkpoint_diff(&self, Parameters(args): Parameters<CheckpointDiffArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let result = async {
            let project = self.checkpoint_project(&args.target).await?;
            let (from, from_dir) = checkpoint::get(&project, &args.checkpoint)?;
            let first = Snapshot::of(&project, &from_dir);
            let (second, against) = match args.against.as_deref() {
                None | Some("current") => {
                    // Unsaved changes of live sessions count as the current state: a checkpoint's
                    // files are made the same way, so this one is, and is removed afterwards.
                    let live = !self
                        .project_sessions(&project)
                        .await
                        .boards
                        .iter()
                        .all(|b| !b.is_live())
                        || self
                            .project_sessions(&project)
                            .await
                            .schematics
                            .iter()
                            .any(|s| s.provider() != Provider::File);
                    if live {
                        let m = self.make_checkpoint(&project, "diff (temporary)").await?;
                        let (_, dir) = checkpoint::get(&project, &m.id)?;
                        let snap = Snapshot::of(&project, &dir);
                        let _ = std::fs::remove_dir_all(&dir);
                        (snap, "the project now (open documents included)".to_owned())
                    } else {
                        (Snapshot::current(&project), "the files on disk now".to_owned())
                    }
                }
                Some(other) => {
                    let (m, dir) = checkpoint::get(&project, other)?;
                    (Snapshot::of(&project, &dir), format!("checkpoint {}", m.id))
                }
            };
            Ok::<_, ToolError>((from, against, checkpoint::diff(&first, &second)))
        }
        .await;
        match result {
            Ok((from, against, diff)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "from": manifest_json(&from), "to": against,
                    "identical": diff.is_empty(), "differences": diff,
                }),
                vec![],
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "checkpoint_restore",
        description = "Put the project's files back as a checkpoint kept them (confirm: true after checkpoint_diff has been shown). \
It first takes a checkpoint of the project as it is now (\"before restoring ...\"), so a restore can be undone with another. \
Refused while a session has unsaved changes (save or reload with discard_unsaved first), while a KiCad editor has \
the files open (a gui session, or a lock file), or while the server is read-only. Headless and file sessions on the \
project are reloaded afterwards. Plans made before are dropped."
    )]
    pub async fn checkpoint_restore(&self, Parameters(args): Parameters<CheckpointRestoreArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        if !args.confirm {
            return envelope::error(
                &cid,
                &bad("confirm must be true; show checkpoint_diff to the user first"),
            );
        }
        let result = async {
            let project = self.checkpoint_project(&args.target).await?;
            let (wanted, _) = checkpoint::get(&project, &args.checkpoint)?;
            let sessions = self.project_sessions(&project).await;
            // Nothing is lost silently, and nobody has the files open.
            for b in &sessions.boards {
                if b.provider() == Provider::Gui {
                    return Err(ToolError::new(
                        ErrorCode::DocumentOpenElsewhere,
                        "the board is open in the PCB editor (a gui session); close it there, or restore with the editor closed",
                    ));
                }
                if b.status().await.unsaved_changes == Some(true) {
                    return Err(bad(
                        "the open board has unsaved changes; save them (pcb_save) or drop them (pcb_reload with discard_unsaved) first",
                    ));
                }
            }
            for s in &sessions.schematics {
                if s.provider() == Provider::Gui {
                    return Err(ToolError::new(
                        ErrorCode::DocumentOpenElsewhere,
                        "the schematic is open in the schematic editor (a gui session); close it there, or restore with the editor closed",
                    ));
                }
                if s.status().await.unsaved_changes == Some(true) {
                    return Err(bad(
                        "the open schematic has unsaved changes; save them (sch_save) or drop them (session_reload with discard_unsaved) first",
                    ));
                }
            }
            // A headless server of ours keeps lock files for the board and its project; a lock
            // with none of our sessions on the project is an editor's.
            let ours = sessions.boards.iter().any(|b| b.provider() == Provider::Headless)
                || sessions.schematics.iter().any(|s| s.provider() == Provider::Headless);
            for ext in checkpoint::EXTENSIONS {
                let file = project.file(ext);
                let lock = file.with_file_name(format!(
                    "~{}.lck",
                    file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                ));
                if lock.exists() && !ours {
                    return Err(ToolError::new(
                        ErrorCode::DocumentOpenElsewhere,
                        format!("{} is open in a KiCad editor (lock file present); close it there first", file.display()),
                    ));
                }
            }
            // The way back.
            let before = self
                .make_checkpoint(&project, &format!("before restoring {}", wanted.id))
                .await?;
            let written = checkpoint::restore_files(&project, &wanted.id)?;
            // Sessions that hold the old documents take the restored ones.
            let mut reloaded = Vec::new();
            for b in &sessions.boards {
                b.reload(true).await?;
                reloaded.push(b.id().to_owned());
                self.state.pending_updates.lock().await.retain(|_, p| p.session_id != b.id());
            }
            for s in &sessions.schematics {
                s.reload(true).await?;
                reloaded.push(s.id().to_owned());
                self.state.plans.lock().await.forget_session(s.id());
            }
            Ok::<_, ToolError>((wanted, before, written, reloaded))
        }
        .await;
        match result {
            Ok((wanted, before, written, reloaded)) => envelope::ok(
                &cid,
                serde_json::json!({
                    "restored": manifest_json(&wanted),
                    "files_written": written,
                    "checkpoint_of_the_state_before": manifest_json(&before),
                    "sessions_reloaded": reloaded,
                    "undo": format!("checkpoint_restore with checkpoint {} puts the project back as it was", before.id),
                }),
                vec![],
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }
}
