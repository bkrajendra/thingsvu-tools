// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kicad_cli_runner::KicadInstall;
use kicad_domain::{Capability, CapabilitySet, ErrorCode, ToolError};
use kicad_ipc::capabilities;
use kicad_ipc::proto::kiapi::common::commands::{
    CreateDocument, DocumentModifiedState, GetDocumentModifiedState, GetDocumentModifiedStateResponse,
    LoadAllLibraries, OpenDocument, OpenDocumentResponse, Ping,
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
    Headless,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Directory for private session sockets.
    pub runtime_dir: PathBuf,
    pub start_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            runtime_dir: std::env::temp_dir().join("thingsvu"),
            start_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
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
}

/// Everything that talks to KiCad sits behind one lock, so requests are strictly serialized.
pub(crate) struct Inner {
    pub(crate) client: Client,
    server: HeadlessServer,
}

fn new_session_id() -> String {
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
fn lock_file(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    Some(path.with_file_name(format!("~{name}.lck")))
}

fn check_target(target: &OpenTarget) -> Result<PathBuf, ToolError> {
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
            if lock_file(&path).is_some_and(|l| l.exists()) {
                return Err(ToolError::new(
                    ErrorCode::DocumentOpenElsewhere,
                    format!(
                        "{} is open in a KiCad editor (lock file present); close it there first",
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
            document_path,
            document,
            capabilities,
            cli: Some(cli),
            inner: Mutex::new(Inner { client, server }),
        })
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

    /// The document identifier KiCad assigned when the session opened.
    #[doc(hidden)]
    pub fn document_specifier(&self) -> &DocumentSpecifier {
        &self.document
    }

    pub fn kicad_version(&self) -> &str {
        &self.kicad_version
    }

    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    async fn unsaved_changes(inner: &mut Inner, document: &DocumentSpecifier) -> Option<bool> {
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
        let running = inner.server.is_running();
        let connected = running && inner.client.call_any(&Ping {}).await.is_ok();
        let unsaved = if connected && self.capabilities.is_available(Capability::DocumentLifecycle) {
            Self::unsaved_changes(&mut inner, &self.document).await
        } else {
            None
        };
        SessionStatus {
            session_id: self.id.clone(),
            provider: self.provider,
            kicad_version: self.kicad_version.clone(),
            instance_fingerprint: self.fingerprint.clone(),
            document: Some(self.document_path.clone()),
            server_pid: inner.server.pid(),
            connected,
            unsaved_changes: unsaved,
            cli: self.cli.as_ref().map(|c| c.cli.display().to_string()),
            capabilities: self.capabilities.clone(),
        }
    }

    /// Stops the session. Refuses while there are unsaved changes unless `discard_unsaved`.
    pub async fn close(&self, discard_unsaved: bool) -> Result<bool, ToolError> {
        let mut inner = self.inner.lock().await;
        let unsaved = if inner.server.is_running() {
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
        inner.server.stop().await;
        tracing::info!(session = %self.id, discarded = unsaved == Some(true), "session closed");
        Ok(unsaved == Some(true))
    }
}
