// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kicad_cli_runner::KicadInstall;
use kicad_domain::{Capability, CapabilitySet, ErrorCode, ToolError};
use kicad_ipc::capabilities;
use kicad_ipc::proto::kiapi::common::commands::{
    CreateDocument, DocumentModifiedState, GetDocumentModifiedState, GetDocumentModifiedStateResponse,
    GetOpenDocuments, GetOpenDocumentsResponse, LoadAllLibraries, OpenDocument, OpenDocumentResponse, Ping,
};
use kicad_ipc::proto::kiapi::common::types::{DocumentSpecifier, DocumentType, LibraryType};
use kicad_ipc::Client;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::capability_map::capabilities_from_probe;
use crate::errors::from_ipc_error;
use crate::headless::HeadlessServer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// A `kicad-cli api-server` started and owned by this process.
    Headless,
    /// A KiCad GUI that registered itself through the `kicad-ai-plugin` toolbar action.
    Gui,
    /// The schematic file itself, edited while no editor has it open (KiCad 10 fallback).
    File,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Directory for private session sockets.
    pub runtime_dir: PathBuf,
    pub start_timeout: Duration,
    pub request_timeout: Duration,
    /// Longest a `kicad-cli` job (netlist, ERC) may run in a file session.
    pub cli_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            runtime_dir: std::env::temp_dir().join("thingsvu"),
            start_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            cli_timeout: Duration::from_secs(180),
        }
    }
}

/// The document a session works on.
#[derive(Debug, Clone)]
pub enum OpenTarget {
    /// An existing `.kicad_sch` that no KiCad editor has open.
    Existing(PathBuf),
    /// A new schematic to be created at this path (kept in memory until saved).
    New(PathBuf),
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub provider: Provider,
    pub kicad_version: String,
    /// Short fingerprint of the KiCad instance token (the token itself is never reported).
    pub instance_fingerprint: Option<String>,
    pub document: Option<PathBuf>,
    pub server_pid: Option<u32>,
    pub connected: bool,
    pub unsaved_changes: Option<bool>,
    /// Has the file on disk changed since this session read or saved it? If so, the document this
    /// session holds is not the file's; `session_reload` takes the file's.
    pub file_changed_on_disk: Option<bool>,
    /// Opened for reading only (a file session on a file an editor may have open).
    pub read_only: bool,
    /// What to do about it, when something is stale.
    pub freshness_note: Option<String>,
    pub cli: Option<String>,
    pub capabilities: CapabilitySet,
}

pub struct Session {
    id: String,
    provider: Provider,
    kicad_version: String,
    fingerprint: Option<String>,
    document_path: PathBuf,
    pub(crate) document: DocumentSpecifier,
    capabilities: CapabilitySet,
    cli: Option<KicadInstall>,
    pub(crate) inner: Mutex<Inner>,
    /// When the file was last read or written by this session.
    pub(crate) freshness: crate::freshness::Freshness,
}

/// Everything that talks to KiCad sits behind one lock, so requests are strictly serialized.
pub(crate) struct Inner {
    pub(crate) client: Client,
    /// The owned headless server; `None` for a GUI session.
    server: Option<HeadlessServer>,
}

pub(crate) fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "s{:x}{:04x}{:x}",
        std::process::id(),
        (nanos & 0xffff) as u16,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn fingerprint(token: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// KiCad keeps `~<file>.lck` next to a document while an editor has it open.
pub(crate) fn lock_file(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    Some(path.with_file_name(format!("~{name}.lck")))
}

pub(crate) fn check_target(target: &OpenTarget) -> Result<PathBuf, ToolError> {
    check_target_with(target, false)
}

/// As `check_target`; with `allow_locked` a file an editor has open may be opened (for reading).
pub(crate) fn check_target_with(target: &OpenTarget, allow_locked: bool) -> Result<PathBuf, ToolError> {
    let (OpenTarget::Existing(path) | OpenTarget::New(path)) = target;
    let path = std::path::absolute(path).map_err(|e| ToolError::new(ErrorCode::InvalidArgument, e.to_string()))?;
    if path.extension().and_then(|e| e.to_str()) != Some("kicad_sch") {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a .kicad_sch file", path.display()),
        ));
    }
    match target {
        OpenTarget::Existing(_) => {
            if !path.is_file() {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("{} does not exist", path.display()),
                ));
            }
            if !allow_locked && lock_file(&path).is_some_and(|l| l.exists()) {
                return Err(ToolError::new(
                    ErrorCode::DocumentOpenElsewhere,
                    format!(
                        "{} is open in a KiCad editor (lock file present); close it there first, or open it with read_only: true",
                        path.display()
                    ),
                ));
            }
        }
        OpenTarget::New(_) => {
            if path.exists() {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("{} already exists", path.display()),
                ));
            }
            if !path.parent().is_some_and(Path::is_dir) {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("the folder for {} does not exist", path.display()),
                ));
            }
        }
    }
    Ok(path)
}

impl Session {
    /// Starts a headless KiCad server owned by this process and opens or creates the document.
    /// `installs` are candidate KiCad installs; the newest KiCad 11-series one is used.
    pub async fn open_headless(
        config: &SessionConfig,
        installs: &[KicadInstall],
        target: OpenTarget,
    ) -> Result<Self, ToolError> {
        let document_path = check_target(&target)?;
        let cli = installs
            .iter()
            .filter(|i| i.version.series() >= 11)
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned()
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::KicadNotFound,
                    "headless sessions need KiCad 11 (or a 10.99 nightly) with kicad-cli; none was found",
                )
            })?;

        std::fs::create_dir_all(&config.runtime_dir).map_err(|e| {
            ToolError::new(
                ErrorCode::KicadError,
                format!("cannot create {}: {e}", config.runtime_dir.display()),
            )
        })?;
        let id = new_session_id();
        let client_name = format!("thingsvu-kicad-ai-{id}");
        let socket = config.runtime_dir.join(format!("{id}.sock"));
        tracing::info!(session = %id, cli = %cli.cli.display(), "starting headless KiCad server");
        let (server, mut client) = HeadlessServer::start(
            &cli.cli,
            socket,
            &client_name,
            config.start_timeout,
            config.request_timeout,
        )
        .await?;

        let path_str = document_path.display().to_string();
        let opened: OpenDocumentResponse = match &target {
            OpenTarget::Existing(_) => {
                client
                    .call(&OpenDocument {
                        r#type: DocumentType::DoctypeSchematic as i32,
                        path: path_str,
                    })
                    .await
            }
            OpenTarget::New(_) => {
                client
                    .call(&CreateDocument {
                        r#type: DocumentType::DoctypeSchematic as i32,
                        path: path_str,
                    })
                    .await
            }
        }
        .map_err(|e| from_ipc_error(&e, false))?;
        let document = opened.document.ok_or_else(|| {
            ToolError::new(
                ErrorCode::KicadError,
                "KiCad opened the document but returned no identifier",
            )
        })?;

        // Headless servers load libraries on request only; the load continues in the background.
        if let Err(e) = client
            .call_any(&LoadAllLibraries {
                r#type: vec![LibraryType::LtSymbol as i32],
            })
            .await
        {
            if !e.is_unhandled() {
                return Err(from_ipc_error(&e, false));
            }
        }

        let report = capabilities::probe(&mut client)
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let capabilities = capabilities_from_probe(&report, true, Some(&cli));
        tracing::info!(session = %id, kicad = %report.kicad_version, "session ready");

        Ok(Self {
            id,
            provider: Provider::Headless,
            kicad_version: report.kicad_version,
            fingerprint: report.instance_token.as_deref().map(fingerprint),
            freshness: crate::freshness::Freshness::new(&document_path),
            document_path,
            document,
            capabilities,
            cli: Some(cli),
            inner: Mutex::new(Inner {
                client,
                server: Some(server),
            }),
        })
    }

    /// Connects to a KiCad GUI that registered itself through the plugin. `schematic` selects the
    /// open project whose root schematic it is; without it, the instance must have exactly one
    /// schematic open. Nothing is started or stopped.
    pub async fn open_gui(
        config: &SessionConfig,
        installs: &[KicadInstall],
        registration: &crate::registry::Registration,
        schematic: Option<&Path>,
    ) -> Result<Self, ToolError> {
        let id = new_session_id();
        let client_name = format!("thingsvu-kicad-ai-{id}");
        let mut client = Client::connect(&registration.socket_path, &client_name, config.request_timeout)
            .await
            .map_err(|e| {
                ToolError::new(
                    ErrorCode::NoSession,
                    format!("the registered KiCad is not reachable ({e}); press the KiCad AI button in KiCad again"),
                )
            })?;
        client.pin_instance_token(registration.token.clone());

        let docs: GetOpenDocumentsResponse = client
            .call(&GetOpenDocuments {
                r#type: DocumentType::DoctypeSchematic as i32,
            })
            .await
            .map_err(|e| {
                let mut err = from_ipc_error(&e, false);
                if err.code == ErrorCode::FeatureUnavailable {
                    err = ToolError::new(
                        ErrorCode::DocumentNotOpen,
                        "no schematic is open in that KiCad; open it in the Schematic Editor",
                    );
                }
                err
            })?;
        let root_of = |d: &DocumentSpecifier| {
            d.project
                .as_ref()
                .map(|p| PathBuf::from(&p.path).join(format!("{}.kicad_sch", p.name)))
        };
        let candidates: Vec<(DocumentSpecifier, PathBuf)> = docs
            .documents
            .iter()
            .filter_map(|d| root_of(d).map(|p| (d.clone(), p)))
            .collect();
        let (document, document_path) = match schematic {
            Some(wanted) => {
                let wanted = std::path::absolute(wanted)
                    .map_err(|e| ToolError::new(ErrorCode::InvalidArgument, e.to_string()))?;
                candidates
                    .into_iter()
                    .find(|(_, root)| same_path(root, &wanted))
                    .ok_or_else(|| {
                        ToolError::new(
                            ErrorCode::DocumentNotOpen,
                            format!(
                                "{} is not the root schematic of a project open in that KiCad",
                                wanted.display()
                            ),
                        )
                    })?
            }
            None => match candidates.len() {
                1 => candidates.into_iter().next().expect("one candidate"),
                0 => {
                    return Err(ToolError::new(
                        ErrorCode::DocumentNotOpen,
                        "no schematic is open in that KiCad",
                    ))
                }
                n => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{n} schematics are open in that KiCad; pass schematic_path to choose one"),
                    ))
                }
            },
        };

        let report = capabilities::probe(&mut client)
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let version = kicad_cli_runner::KicadVersion::parse(&report.kicad_version);
        let cli = installs
            .iter()
            .filter(|i| version.as_ref().is_some_and(|v| v.series() == i.version.series()))
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned();
        let capabilities = capabilities_from_probe(&report, false, cli.as_ref());
        tracing::info!(session = %id, kicad = %report.kicad_version, document = %document_path.display(), "GUI session ready");
        Ok(Self {
            id,
            provider: Provider::Gui,
            kicad_version: report.kicad_version,
            fingerprint: Some(fingerprint(&registration.token)),
            freshness: crate::freshness::Freshness::new(&document_path),
            document_path,
            document,
            capabilities,
            cli,
            inner: Mutex::new(Inner { client, server: None }),
        })
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Direct access to the session's KiCad client, for tests and probes only. Normal callers use
    /// the typed operations, which enforce capabilities and serialization.
    #[doc(hidden)]
    pub async fn raw_client(&self) -> tokio::sync::MappedMutexGuard<'_, Client> {
        tokio::sync::MutexGuard::map(self.inner.lock().await, |inner| &mut inner.client)
    }

    /// The socket of the session's own headless server, for tests that act as a KiCad plugin.
    #[doc(hidden)]
    pub async fn headless_socket(&self) -> Option<PathBuf> {
        self.inner.lock().await.server.as_ref().map(|s| s.socket().to_owned())
    }

    /// The document identifier KiCad assigned when the session opened.
    #[doc(hidden)]
    pub fn document_specifier(&self) -> &DocumentSpecifier {
        &self.document
    }

    /// The kicad-cli install matching this session's KiCad, if one was found.
    pub fn cli(&self) -> Option<&KicadInstall> {
        self.cli.as_ref()
    }

    pub fn kicad_version(&self) -> &str {
        &self.kicad_version
    }

    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    pub(crate) async fn unsaved_changes(inner: &mut Inner, document: &DocumentSpecifier) -> Option<bool> {
        let r: GetDocumentModifiedStateResponse = inner
            .client
            .call(&GetDocumentModifiedState {
                document: Some(document.clone()),
            })
            .await
            .ok()?;
        match DocumentModifiedState::try_from(r.state).ok()? {
            DocumentModifiedState::DmsModified => Some(true),
            DocumentModifiedState::DmsUnmodified => Some(false),
            DocumentModifiedState::DmsUnknown => None,
        }
    }

    /// Current state, re-checked against the server (liveness and unsaved changes).
    pub async fn status(&self) -> SessionStatus {
        let mut inner = self.inner.lock().await;
        let running = inner.server.as_mut().is_none_or(|s| s.is_running());
        let connected = running && inner.client.call_any(&Ping {}).await.is_ok();
        let unsaved = if connected && self.capabilities.is_available(Capability::DocumentLifecycle) {
            Self::unsaved_changes(&mut inner, &self.document).await
        } else {
            None
        };
        let changed = self.freshness.changed(&self.document_path);
        SessionStatus {
            session_id: self.id.clone(),
            provider: self.provider,
            kicad_version: self.kicad_version.clone(),
            instance_fingerprint: self.fingerprint.clone(),
            document: Some(self.document_path.clone()),
            server_pid: inner.server.as_ref().and_then(|s| s.pid()),
            connected,
            unsaved_changes: unsaved,
            file_changed_on_disk: changed,
            read_only: false,
            freshness_note: match (changed, unsaved) {
                (Some(true), Some(true)) => Some(
                    "the file on disk changed and the open document has unsaved changes too; session_reload with \
                     discard_unsaved: true takes the file's and drops the unsaved changes"
                        .to_owned(),
                ),
                (Some(true), _) if self.provider == Provider::Gui => Some(
                    "the file on disk changed (the editor saved it, or another program wrote it); the editor's document is what this session reads"
                        .to_owned(),
                ),
                (Some(true), _) => Some(
                    "the file on disk changed after this session loaded the document, so what it reads is not the file's; \
                     session_reload takes the file's"
                        .to_owned(),
                ),
                _ => None,
            },
            cli: self.cli.as_ref().map(|c| c.cli.display().to_string()),
            capabilities: self.capabilities.clone(),
        }
    }

    /// Reads the document from its file again. With unsaved changes it refuses unless
    /// `discard_unsaved`, and says what would be lost.
    pub async fn reload(&self, discard_unsaved: bool) -> Result<crate::freshness::ReloadReport, ToolError> {
        use kicad_ipc::proto::kiapi::common::commands::RevertDocument;
        if !self.capabilities.is_available(Capability::DocumentLifecycle) {
            return Err(ToolError::new(
                ErrorCode::FeatureUnavailable,
                "this session cannot reload the document",
            ));
        }
        let changed = self.freshness.changed(&self.document_path);
        let live = self.design().await?;
        let on_disk = std::fs::read_to_string(&self.document_path)
            .ok()
            .and_then(|t| kicad_sexpr::Document::parse(t).ok())
            .map(|d| d.design());
        let dropped = on_disk
            .as_ref()
            .map(|disk| kicad_domain::changes::describe_design_changes(disk, &live))
            .unwrap_or_default();
        let mut inner = self.inner.lock().await;
        let unsaved = Self::unsaved_changes(&mut inner, &self.document).await;
        if unsaved != Some(false) && !discard_unsaved {
            let what = if dropped.is_empty() {
                "the editor reports unsaved changes".to_owned()
            } else {
                dropped.join("; ")
            };
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "reloading would drop the document's unsaved changes ({what}); save first (sch_save), or pass discard_unsaved: true"
                ),
            ));
        }
        inner
            .client
            .call_any(&RevertDocument {
                document: Some(self.document.clone()),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let after = Self::unsaved_changes(&mut inner, &self.document).await;
        drop(inner);
        if after == Some(true) {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                "KiCad still reports unsaved changes after the reload",
            ));
        }
        self.freshness.reset(&self.document_path);
        let design = self.design().await?;
        Ok(crate::freshness::ReloadReport {
            reloaded: true,
            provider: self.provider,
            had_unsaved_changes: unsaved,
            file_changed_on_disk: changed,
            dropped: if unsaved == Some(true) { dropped } else { vec![] },
            counts_after: design_counts(&design),
            note: "the document is the file's now; plans made before the reload are stale".into(),
        })
    }

    /// Ends the session. A headless session stops its server and refuses while there are unsaved
    /// changes unless `discard_unsaved`. A GUI session only disconnects: the editor, its document,
    /// and any unsaved changes stay with the user.
    pub async fn close(&self, discard_unsaved: bool) -> Result<bool, ToolError> {
        let mut inner = self.inner.lock().await;
        let Some(server) = inner.server.as_mut() else {
            tracing::info!(session = %self.id, "GUI session disconnected");
            return Ok(false);
        };
        let unsaved = if server.is_running() {
            Self::unsaved_changes(&mut inner, &self.document).await
        } else {
            None
        };
        if unsaved == Some(true) && !discard_unsaved {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "the document has unsaved changes; save it first or pass discard_unsaved: true",
            ));
        }
        if let Some(server) = inner.server.as_mut() {
            server.stop().await;
        }
        tracing::info!(session = %self.id, discarded = unsaved == Some(true), "session closed");
        Ok(unsaved == Some(true))
    }
}

/// What a schematic holds, in a few counts.
pub(crate) fn design_counts(d: &kicad_domain::SchematicDesign) -> std::collections::BTreeMap<&'static str, usize> {
    std::collections::BTreeMap::from([
        ("symbols", d.symbols.len()),
        ("wires", d.wires.len()),
        ("labels", d.labels.len()),
        ("junctions", d.junctions.len()),
    ])
}

/// Compares paths the way the file system does (case-insensitively on Windows).
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.components().collect::<PathBuf>().display().to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

impl Session {
    /// One request to KiCad as it is, for probes and tests that explore the API.
    #[doc(hidden)]
    pub async fn raw_call<Req: prost::Name, Resp: prost::Name + Default>(
        &self,
        request: &Req,
    ) -> Result<Resp, kicad_ipc::Error> {
        self.inner.lock().await.client.call(request).await
    }
}
